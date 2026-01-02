use crate::error::{Error, Result};
use crate::protocol::{Frame, FrameType, StreamId, FLAG_EOS, MAX_PAYLOAD_SIZE};
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_MORE_DATA, HANDLE};
use windows::Win32::Storage::FileSystem::ReadFile;

pub struct StreamPump {
    send_window: Arc<AtomicU64>,
    bytes_in_flight: Arc<AtomicU64>,
    window_condvar: Arc<tokio::sync::Notify>,
    drop_policy: bool,
}

impl StreamPump {
    pub fn new(initial_window: u64, drop_policy: bool) -> Self {
        Self {
            send_window: Arc::new(AtomicU64::new(initial_window)),
            bytes_in_flight: Arc::new(AtomicU64::new(0)),
            window_condvar: Arc::new(tokio::sync::Notify::new()),
            drop_policy,
        }
    }

    pub fn update_window(&self, bytes_consumed: u64) {
        self.send_window
            .fetch_add(bytes_consumed, Ordering::Release);
        let old_in_flight = self.bytes_in_flight.fetch_sub(
            bytes_consumed.min(self.bytes_in_flight.load(Ordering::Acquire)),
            Ordering::Release,
        );
        if old_in_flight >= self.send_window.load(Ordering::Acquire) {
            // Window was exhausted, notify waiting pumps
            self.window_condvar.notify_waiters();
        }
    }

    pub async fn try_send_frame(&self, sender: &mpsc::Sender<Frame>, frame: Frame) -> Result<()> {
        let data_len = frame.payload.len() as u64;

        // Check window with retry loop
        loop {
            let in_flight = self.bytes_in_flight.load(Ordering::Acquire);
            let window = self.send_window.load(Ordering::Acquire);

            if in_flight + data_len <= window {
                // Window available, send
                self.bytes_in_flight.fetch_add(data_len, Ordering::Release);
                sender.send(frame).await.map_err(|e| {
                    Error::Protocol(format!("Failed to send frame to channel: {}", e))
                })?;
                break;
            } else {
                // Window exhausted
                if self.drop_policy {
                    tracing::warn!(
                        "Dropping output frame, window exhausted (in_flight: {}, window: {})",
                        in_flight,
                        window
                    );
                    return Ok(()); // Swallow error
                } else {
                    // Block until window available
                    let notified = self.window_condvar.notified();
                    // Re-check window (may have been updated)
                    let in_flight = self.bytes_in_flight.load(Ordering::Acquire);
                    let window = self.send_window.load(Ordering::Acquire);
                    if in_flight + data_len <= window {
                        continue; // Retry
                    }
                    notified.await;
                }
            }
        }
        Ok(())
    }
}

pub async fn pump_stdout(
    pipe_handle: HANDLE,
    job_id: u32,
    frame_sender: mpsc::Sender<Frame>,
    pump: Arc<StreamPump>,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
    pump_stream(
        pipe_handle,
        job_id,
        StreamId::Stdout,
        frame_sender,
        pump,
        cancellation_token,
    )
    .await
}

pub async fn pump_stderr(
    pipe_handle: HANDLE,
    job_id: u32,
    frame_sender: mpsc::Sender<Frame>,
    pump: Arc<StreamPump>,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
    pump_stream(
        pipe_handle,
        job_id,
        StreamId::Stderr,
        frame_sender,
        pump,
        cancellation_token,
    )
    .await
}

/// Read data result from pipe
struct PipeReadResult {
    data: Vec<u8>,
    bytes_read: usize,
    is_eof: bool,
}

/// Read from pipe handle (blocking)
fn read_pipe_blocking(handle: HANDLE, buffer_size: usize) -> Result<PipeReadResult> {
    let mut buffer = vec![0u8; buffer_size];
    let mut bytes_read: u32 = 0;

    unsafe {
        let result = ReadFile(
            handle,
            Some(&mut buffer),
            Some(&mut bytes_read),
            None, // No overlapped (synchronous)
        );

        match result {
            Ok(()) => {
                if bytes_read == 0 {
                    // EOF
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
                    // Pipe closed (EOF)
                    Ok(PipeReadResult {
                        data: Vec::new(),
                        bytes_read: 0,
                        is_eof: true,
                    })
                } else if code == ERROR_MORE_DATA.into() {
                    // More data available, return what we have
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

async fn pump_stream(
    pipe_handle: HANDLE,
    job_id: u32,
    stream_id: StreamId,
    frame_sender: mpsc::Sender<Frame>,
    pump: Arc<StreamPump>,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
    use tokio::task::spawn_blocking;

    let mut sequence = 0u32;

    loop {
        // Check cancellation
        if cancellation_token.is_cancelled() {
            tracing::debug!(job_id, ?stream_id, "Pump cancelled");
            break;
        }

        // Read from pipe using blocking I/O in spawn_blocking
        let handle = pipe_handle;
        let read_result = spawn_blocking(move || read_pipe_blocking(handle, MAX_PAYLOAD_SIZE))
            .await
            .map_err(|e| Error::ProcessLaunchFailed(format!("Spawn blocking failed: {}", e)))?;

        match read_result {
            Ok(result) if result.is_eof => {
                // EOF - pipe closed
                tracing::debug!(job_id, ?stream_id, "Pipe EOF, end of stream");
                break;
            }
            Ok(result) if result.bytes_read == 0 => {
                // No data but not EOF (shouldn't happen in blocking mode)
                continue;
            }
            Ok(result) => {
                // Create OUTPUT frame with actual bytes read
                let payload = Bytes::from(result.data);
                let frame = Frame::new(FrameType::Output, stream_id)
                    .with_job_id(job_id)
                    .with_sequence(sequence)
                    .with_payload(payload);

                sequence = sequence.wrapping_add(1);

                // Send frame with backpressure handling
                pump.try_send_frame(&frame_sender, frame).await?;
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
