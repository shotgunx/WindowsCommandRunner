#![cfg(windows)]

use crate::error::{Error, Result};
use crate::protocol::{Frame, FrameType, StreamId, FLAG_EOS, MAX_PAYLOAD_SIZE};
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_MORE_DATA, HANDLE};
use windows::Win32::Storage::FileSystem::ReadFile;

pub struct StreamPump {
    send_window: AtomicU64,
    bytes_in_flight: AtomicU64,
    window_condvar: tokio::sync::Notify,
    drop_policy: bool,
}

impl StreamPump {
    pub fn new(initial_window: u64, drop_policy: bool) -> Self {
        Self {
            send_window: AtomicU64::new(initial_window),
            bytes_in_flight: AtomicU64::new(0),
            window_condvar: tokio::sync::Notify::new(),
            drop_policy,
        }
    }

    pub fn update_window(&self, bytes_consumed: u64) {
        self.send_window
            .fetch_add(bytes_consumed, Ordering::Release);
        let current = self.bytes_in_flight.load(Ordering::Acquire);
        self.bytes_in_flight
            .fetch_sub(bytes_consumed.min(current), Ordering::Release);
        self.window_condvar.notify_waiters();
    }

    pub async fn try_send_frame(&self, sender: &mpsc::Sender<Frame>, frame: Frame) -> Result<()> {
        let data_len = frame.payload.len() as u64;

        loop {
            let in_flight = self.bytes_in_flight.load(Ordering::Acquire);
            let window = self.send_window.load(Ordering::Acquire);

            if in_flight + data_len <= window {
                self.bytes_in_flight.fetch_add(data_len, Ordering::Release);
                sender
                    .send(frame)
                    .await
                    .map_err(|e| Error::Protocol(format!("Failed to send frame: {}", e)))?;
                break;
            } else {
                if self.drop_policy {
                    tracing::warn!("Dropping output frame, window exhausted");
                    return Ok(());
                } else {
                    self.window_condvar.notified().await;
                }
            }
        }
        Ok(())
    }
}

struct PipeReadResult {
    data: Vec<u8>,
    bytes_read: usize,
    is_eof: bool,
}

fn read_pipe_blocking(handle_ptr: isize, buffer_size: usize) -> Result<PipeReadResult> {
    let handle = HANDLE(handle_ptr as *mut std::ffi::c_void);
    let mut buffer = vec![0u8; buffer_size];
    let mut bytes_read: u32 = 0;

    unsafe {
        let result = ReadFile(handle, Some(&mut buffer), Some(&mut bytes_read), None);

        match result {
            Ok(()) => {
                if bytes_read == 0 {
                    Ok(PipeReadResult {
                        data: Vec::new(),
                        bytes_read: 0,
                        is_eof: true,
                    })
                } else {
                    buffer.truncate(bytes_read as usize);
                    Ok(PipeReadResult {
                        data: buffer,
                        bytes_read: bytes_read as usize,
                        is_eof: false,
                    })
                }
            }
            Err(e) => {
                let code = e.code();
                if code == ERROR_BROKEN_PIPE.into() {
                    Ok(PipeReadResult {
                        data: Vec::new(),
                        bytes_read: 0,
                        is_eof: true,
                    })
                } else if code == ERROR_MORE_DATA.into() {
                    buffer.truncate(bytes_read as usize);
                    Ok(PipeReadResult {
                        data: buffer,
                        bytes_read: bytes_read as usize,
                        is_eof: false,
                    })
                } else {
                    Err(Error::Io(std::io::Error::from_raw_os_error(code.0 as i32)))
                }
            }
        }
    }
}

pub async fn start_output_pump(
    handle_ptr: isize,
    job_id: u32,
    stream_id: StreamId,
    frame_sender: mpsc::Sender<Frame>,
    pump: Arc<StreamPump>,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let mut sequence = 0u32;

    loop {
        if cancellation_token.is_cancelled() {
            tracing::debug!(job_id, ?stream_id, "Pump cancelled");
            break;
        }

        // Copy handle_ptr for the closure since it's used in a loop
        let h = handle_ptr;
        let read_result =
            tokio::task::spawn_blocking(move || read_pipe_blocking(h, MAX_PAYLOAD_SIZE))
                .await
                .map_err(|e| Error::ProcessLaunchFailed(format!("Spawn blocking failed: {}", e)))?;

        match read_result {
            Ok(result) if result.is_eof => {
                tracing::debug!(job_id, ?stream_id, "Pipe EOF");
                break;
            }
            Ok(result) if result.bytes_read == 0 => {
                continue;
            }
            Ok(result) => {
                let payload = Bytes::from(result.data);
                let frame = Frame::new(FrameType::Output, stream_id)
                    .with_job_id(job_id)
                    .with_sequence(sequence)
                    .with_payload(payload);

                sequence = sequence.wrapping_add(1);
                pump.try_send_frame(&frame_sender, frame).await?;
                // Note: Activity is updated when frames are processed in pipe_server
            }
            Err(e) => {
                tracing::error!(job_id, ?stream_id, error = %e, "Pipe read error");
                return Err(e);
            }
        }
    }

    // Send EOS frame
    let eos_frame = Frame::new(FrameType::Output, stream_id)
        .with_job_id(job_id)
        .with_sequence(sequence)
        .with_flag(FLAG_EOS);

    pump.try_send_frame(&frame_sender, eos_frame).await?;
    tracing::debug!(job_id, ?stream_id, "Pump completed");
    Ok(())
}
