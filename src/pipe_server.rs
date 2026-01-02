#![cfg(windows)]

use crate::cancellation::CancellationManager;
use crate::error::{Error, Result};
use crate::io_pump::{start_output_pump, StreamPump};
use crate::job_manager::{JobManager, JobState};
use crate::process_launcher::ProcessLauncher;
use crate::protocol::{
    ExitPayload, Frame, FrameType, HelloPayload, RunPayload, StreamId, PROTOCOL_VERSION,
};
use bytes::Bytes;
use std::ptr::null_mut;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_BROKEN_PIPE, HANDLE};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES};

// PIPE_ACCESS_DUPLEX (0x3) - Pipe can be used for both reading and writing
const PIPE_ACCESS_DUPLEX: FILE_FLAGS_AND_ATTRIBUTES = FILE_FLAGS_AND_ATTRIBUTES(0x00000003);
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_MODE, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

const PIPE_NAME: &str = r"\\.\pipe\VirimaRemoteAgent";
const PIPE_BUFFER_SIZE: u32 = 65536;
const MAX_CONCURRENT_CONNECTIONS: usize = 100;

pub struct PipeServer {
    job_manager: Arc<JobManager>,
    connection_semaphore: Arc<Semaphore>,
}

impl PipeServer {
    pub fn new(job_manager: Arc<JobManager>, max_connections: usize) -> Self {
        Self {
            job_manager,
            connection_semaphore: Arc::new(Semaphore::new(max_connections)),
        }
    }

    pub async fn start_listening(self: Arc<Self>) -> Result<()> {
        tracing::info!(pipe = PIPE_NAME, "Starting pipe server");

        loop {
            // Create pipe instance
            let pipe_handle = Self::create_pipe_instance()?;

            // Acquire permit
            let permit = self
                .connection_semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::Service("Semaphore closed".to_string()))?;

            let server = self.clone();
            let handle_ptr = pipe_handle.0 as isize;

            tokio::task::spawn_blocking(move || {
                let runtime = tokio::runtime::Handle::current();
                runtime.block_on(async move {
                    // Wait for client connection
                    let handle = HANDLE(handle_ptr as *mut std::ffi::c_void);
                    let connect_result = unsafe { ConnectNamedPipe(handle, None) };

                    match connect_result {
                        Ok(()) | Err(_) => {
                            // ConnectNamedPipe returns false if client already connected
                            if let Err(e) = server.handle_client(handle_ptr).await {
                                tracing::error!(error = %e, "Client session error");
                            }
                        }
                    }

                    // Cleanup
                    unsafe {
                        let _ = DisconnectNamedPipe(handle);
                        let _ = CloseHandle(handle);
                    }
                    drop(permit);
                });
            });
        }
    }

    fn create_pipe_instance() -> Result<HANDLE> {
        let pipe_name_wide: Vec<u16> = PIPE_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: false.into(),
            lpSecurityDescriptor: null_mut(),
        };

        unsafe {
            let handle = CreateNamedPipeW(
                PCWSTR::from_raw(pipe_name_wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                0,
                Some(&sa),
            )
            .map_err(|e| Error::Service(format!("CreateNamedPipeW: {}", e)))?;

            if handle.is_invalid() {
                return Err(Error::Service("CreateNamedPipeW returned invalid handle".to_string()));
            }

            Ok(handle)
        }
    }

    async fn handle_client(&self, handle_ptr: isize) -> Result<()> {
        tracing::info!("Client connected");

        // Protocol negotiation
        let hello_frame = self.read_frame(handle_ptr).await?;
        if hello_frame.frame_type != FrameType::Hello {
            return Err(Error::Protocol("Expected HELLO frame".to_string()));
        }

        // Parse hello payload
        let hello: HelloPayload = serde_json::from_slice(&hello_frame.payload)
            .map_err(|e| Error::Protocol(format!("Invalid HELLO: {}", e)))?;

        if hello.version != PROTOCOL_VERSION {
            let error_msg = format!("Version mismatch: {} vs {}", hello.version, PROTOCOL_VERSION);
            self.send_error_frame(handle_ptr, 0, &error_msg).await?;
            return Err(Error::Protocol(error_msg));
        }

        // Send HELLO_ACK
        let ack_payload = HelloPayload {
            version: PROTOCOL_VERSION,
            client_id: hello.client_id.clone(),
        };
        let ack = Frame::new(FrameType::HelloAck, StreamId::Control)
            .with_payload(Bytes::from(serde_json::to_vec(&ack_payload).unwrap()));
        self.write_frame(handle_ptr, &ack).await?;

        // Create frame channel for output
        let (frame_tx, mut frame_rx) = mpsc::channel::<Frame>(256);

        // Spawn writer task
        let writer_handle = {
            let handle = handle_ptr;
            tokio::spawn(async move {
                while let Some(frame) = frame_rx.recv().await {
                    if let Err(e) = Self::write_frame_static(handle, &frame).await {
                        tracing::error!(error = %e, "Frame write error");
                        break;
                    }
                }
            })
        };

        // Main request loop
        let result = self.process_requests(handle_ptr, frame_tx.clone()).await;

        drop(frame_tx);
        let _ = writer_handle.await;

        result
    }

    async fn process_requests(
        &self,
        handle_ptr: isize,
        frame_tx: mpsc::Sender<Frame>,
    ) -> Result<()> {
        loop {
            let frame = match self.read_frame(handle_ptr).await {
                Ok(f) => f,
                Err(Error::Io(e)) if e.raw_os_error() == Some(ERROR_BROKEN_PIPE.0 as i32) => {
                    tracing::info!("Client disconnected");
                    break;
                }
                Err(e) => return Err(e),
            };

            match frame.frame_type {
                FrameType::Run => {
                    self.handle_run(frame, frame_tx.clone()).await?;
                }
                FrameType::Cancel => {
                    let job_id = frame.job_id;
                    if let Err(e) = self.job_manager.cancel_job(job_id, "Client requested") {
                        tracing::warn!(job_id, error = %e, "Cancel failed");
                    }
                }
                FrameType::WindowUpdate => {
                    // Handle flow control
                    tracing::debug!(job_id = frame.job_id, "Window update received");
                }
                FrameType::Goodbye => {
                    tracing::info!("Client sent GOODBYE");
                    break;
                }
                _ => {
                    tracing::warn!(frame_type = ?frame.frame_type, "Unexpected frame");
                }
            }
        }
        Ok(())
    }

    async fn handle_run(&self, frame: Frame, frame_tx: mpsc::Sender<Frame>) -> Result<()> {
        let payload: RunPayload = serde_json::from_slice(&frame.payload)
            .map_err(|e| Error::Protocol(format!("Invalid RUN payload: {}", e)))?;

        let timeout = payload.timeout_ms.map(std::time::Duration::from_millis);
        let job = self.job_manager.create_job(timeout)?;
        let job_id = job.job_id;

        tracing::info!(job_id, command = %payload.command, "Starting job");

        // Send ACK
        let ack = Frame::new(FrameType::RunAck, StreamId::Control).with_job_id(job_id);
        frame_tx
            .send(ack)
            .await
            .map_err(|e| Error::Protocol(format!("Send ACK: {}", e)))?;

        // Launch process
        self.job_manager.transition_state(job_id, JobState::Starting)?;

        let wd = payload.working_directory.as_deref();
        let env = payload.environment.as_ref();

        match ProcessLauncher::launch(&payload.command, wd, env) {
            Ok(proc) => {
                job.process_handle.set(proc.process_handle.0 as isize);
                job.job_object.set(proc.job_object.0 as isize);

                self.job_manager.transition_state(job_id, JobState::Running)?;

                let pump = Arc::new(StreamPump::new(65536, false));

                // Spawn stdout pump
                let stdout_handle = proc.stdout_read.0 as isize;
                let pump_stdout = pump.clone();
                let token_stdout = job.cancellation_token.clone();
                let tx_stdout = frame_tx.clone();

                tokio::spawn(async move {
                    let _ = start_output_pump(
                        stdout_handle,
                        job_id,
                        StreamId::Stdout,
                        tx_stdout,
                        pump_stdout,
                        token_stdout,
                    )
                    .await;
                });

                // Spawn stderr pump
                let stderr_handle = proc.stderr_read.0 as isize;
                let pump_stderr = pump.clone();
                let token_stderr = job.cancellation_token.clone();
                let tx_stderr = frame_tx.clone();

                tokio::spawn(async move {
                    let _ = start_output_pump(
                        stderr_handle,
                        job_id,
                        StreamId::Stderr,
                        tx_stderr,
                        pump_stderr,
                        token_stderr,
                    )
                    .await;
                });

                // Spawn timeout monitor
                let job_clone = job.clone();
                let mgr_clone = self.job_manager.clone();
                tokio::spawn(async move {
                    CancellationManager::monitor_timeout(job_clone, mgr_clone).await;
                });

                // Wait for process exit
                let job_clone = job.clone();
                let mgr_clone = self.job_manager.clone();
                let tx_exit = frame_tx.clone();

                tokio::spawn(async move {
                    let exit_code = CancellationManager::wait_for_process_exit(
                        job_clone.process_handle.get(),
                        job_clone.cancellation_token.clone(),
                    )
                    .await
                    .unwrap_or(-1);

                    let _ = mgr_clone.transition_state(job_id, JobState::Exiting);

                    let exit_payload = ExitPayload { exit_code };
                    let exit_frame = Frame::new(FrameType::Exit, StreamId::Control)
                        .with_job_id(job_id)
                        .with_payload(Bytes::from(serde_json::to_vec(&exit_payload).unwrap()));

                    let _ = tx_exit.send(exit_frame).await;
                    let _ = mgr_clone.transition_state(job_id, JobState::Completed);
                    let _ = mgr_clone.cleanup_job(job_id);
                });
            }
            Err(e) => {
                tracing::error!(job_id, error = %e, "Launch failed");
                let _ = self.job_manager.transition_state(job_id, JobState::Failed);

                let error_frame = Frame::new(FrameType::Error, StreamId::Control)
                    .with_job_id(job_id)
                    .with_payload(Bytes::from(format!("Launch failed: {}", e)));
                frame_tx.send(error_frame).await.ok();

                let _ = self.job_manager.cleanup_job(job_id);
            }
        }

        Ok(())
    }

    async fn read_frame(&self, handle_ptr: isize) -> Result<Frame> {
        Self::read_frame_static(handle_ptr).await
    }

    async fn read_frame_static(handle_ptr: isize) -> Result<Frame> {
        // Read length prefix (4 bytes)
        let len_buf = Self::read_exact(handle_ptr, 4).await?;
        let len = u32::from_be_bytes([len_buf[0], len_buf[1], len_buf[2], len_buf[3]]) as usize;

        if len > 1024 * 1024 {
            return Err(Error::Protocol("Frame too large".to_string()));
        }

        // Read frame data
        let data = Self::read_exact(handle_ptr, len).await?;
        Frame::decode(&data)
    }

    async fn read_exact(handle_ptr: isize, size: usize) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; size];
        let mut offset = 0;

        while offset < size {
            let handle = HANDLE(handle_ptr as *mut std::ffi::c_void);
            let mut bytes_read: u32 = 0;
            let remaining = size - offset;

            unsafe {
                ReadFile(
                    handle,
                    Some(&mut buffer[offset..]),
                    Some(&mut bytes_read),
                    None,
                )
                .map_err(|e| Error::Io(std::io::Error::from_raw_os_error(e.code().0 as i32)))?;
            }

            if bytes_read == 0 {
                return Err(Error::Io(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
            }
            offset += bytes_read as usize;
        }

        Ok(buffer)
    }

    async fn write_frame(&self, handle_ptr: isize, frame: &Frame) -> Result<()> {
        Self::write_frame_static(handle_ptr, frame).await
    }

    async fn write_frame_static(handle_ptr: isize, frame: &Frame) -> Result<()> {
        let data = frame.encode()?;

        // Write length prefix
        let len = data.len() as u32;
        Self::write_all(handle_ptr, &len.to_be_bytes()).await?;

        // Write frame data
        Self::write_all(handle_ptr, &data).await
    }

    async fn write_all(handle_ptr: isize, data: &[u8]) -> Result<()> {
        let handle = HANDLE(handle_ptr as *mut std::ffi::c_void);
        let mut offset = 0;

        while offset < data.len() {
            let mut bytes_written: u32 = 0;
            unsafe {
                WriteFile(
                    handle,
                    Some(&data[offset..]),
                    Some(&mut bytes_written),
                    None,
                )
                .map_err(|e| Error::Io(std::io::Error::from_raw_os_error(e.code().0 as i32)))?;
            }
            offset += bytes_written as usize;
        }

        Ok(())
    }

    async fn send_error_frame(&self, handle_ptr: isize, job_id: u32, message: &str) -> Result<()> {
        let frame = Frame::new(FrameType::Error, StreamId::Control)
            .with_job_id(job_id)
            .with_payload(Bytes::from(message.to_string()));
        self.write_frame(handle_ptr, &frame).await
    }
}
