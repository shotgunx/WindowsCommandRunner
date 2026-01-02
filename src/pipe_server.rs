use crate::error::{Error, Result};
use crate::job_manager::{JobHandle, JobManager, JobState};
use crate::logger::Logger;
use crate::process_launcher::{ProcessLauncher, ChildPipeHandles};
use crate::protocol::{
    Frame, FrameType, HelloPayload, RunAckPayload, RunPayload, StreamId, 
    WindowUpdatePayload, PROTOCOL_VERSION, DEFAULT_WINDOW_SIZE,
};
use crate::cancellation::CancellationManager;
use crate::io_pump::{pump_stderr, pump_stdout, StreamPump};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, ERROR_BROKEN_PIPE, BOOL};
use windows::Win32::Security::{
    ImpersonateNamedPipeClient, RevertToSelf, SECURITY_ATTRIBUTES,
    GetTokenInformation, TokenUser, TOKEN_USER, TOKEN_QUERY,
    PSID,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, 
    PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE, PIPE_WAIT, PIPE_READMODE_BYTE,
};
use windows::Win32::System::Threading::{CreatePipe, OpenThreadToken, GetCurrentThread};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};

/// Default buffer sizes
const PIPE_BUFFER_SIZE: u32 = 65536;
const FRAME_CHANNEL_SIZE: usize = 1000;

pub struct PipeServer {
    pipe_name: String,
    job_manager: Arc<JobManager>,
    logger: Arc<Logger>,
    max_concurrent_clients: usize,
    active_clients: Arc<tokio::sync::Semaphore>,
}

impl PipeServer {
    pub fn new(
        pipe_name: String,
        job_manager: Arc<JobManager>,
        logger: Arc<Logger>,
        max_concurrent_clients: usize,
    ) -> Self {
        Self {
            pipe_name,
            job_manager,
            logger,
            max_concurrent_clients,
            active_clients: Arc::new(tokio::sync::Semaphore::new(max_concurrent_clients)),
        }
    }

    pub async fn start_listening(&self) -> Result<()> {
        tracing::info!(pipe_name = %self.pipe_name, "Starting pipe server");

        loop {
            // Wait for available slot
            let permit = self.active_clients.clone().acquire_owned().await.map_err(|e| {
                Error::Service(format!("Failed to acquire client permit: {}", e))
            })?;

            // Create pipe instance with proper security
            let pipe_handle = self.create_pipe_instance()?;

            // Spawn handler for this connection
            let job_manager = self.job_manager.clone();
            let logger = self.logger.clone();

            tokio::spawn(async move {
                // Wait for client to connect (blocking)
                let connect_result = tokio::task::spawn_blocking({
                    let handle = pipe_handle;
                    move || unsafe { ConnectNamedPipe(handle, None) }
                }).await;

                match connect_result {
                    Ok(Ok(())) => {
                        tracing::debug!("Client connected");
                        if let Err(e) = Self::handle_client(pipe_handle, job_manager, logger).await {
                            tracing::error!(error = %e, "Error handling client");
                        }
                    }
                    Ok(Err(e)) => {
                        // ERROR_PIPE_CONNECTED means client already connected (OK)
                        let code = e.code();
                        if code == windows::Win32::Foundation::ERROR_PIPE_CONNECTED.into() {
                            tracing::debug!("Client already connected");
                            if let Err(e) = Self::handle_client(pipe_handle, job_manager, logger).await {
                                tracing::error!(error = %e, "Error handling client");
                            }
                        } else {
                            tracing::error!(error = %e, "Failed to connect client");
                            unsafe { CloseHandle(pipe_handle).ok(); }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Spawn blocking failed");
                        unsafe { CloseHandle(pipe_handle).ok(); }
                    }
                }
                // Permit dropped automatically when task ends
                drop(permit);
            });
        }
    }

    fn create_pipe_instance(&self) -> Result<HANDLE> {
        // Create security descriptor: SYSTEM + Administrators only
        // SDDL format:
        // D: = DACL
        // (A;;GA;;;SY) = Allow SYSTEM Generic All
        // (A;;GA;;;BA) = Allow Built-in Administrators Generic All
        let sddl = "D:(A;;GA;;;SY)(A;;GA;;;BA)";
        
        let security_descriptor = unsafe {
            let mut sd_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR::from_raw(to_wide_null(sddl).as_ptr()),
                SDDL_REVISION_1,
                &mut sd_ptr,
                None,
            ).map_err(|e| Error::Service(format!("Failed to create security descriptor: {}", e)))?;
            sd_ptr
        };

        let security_attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security_descriptor,
            bInheritHandle: BOOL::from(false),
        };

        let pipe_name_wide = to_wide_null(&self.pipe_name);

        unsafe {
            let pipe_handle = CreateNamedPipeW(
                PCWSTR::from_raw(pipe_name_wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                self.max_concurrent_clients as u32,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                0, // Default timeout
                Some(&security_attrs),
            ).map_err(|e| Error::Service(format!("Failed to create named pipe: {}", e)))?;

            // Free security descriptor
            windows::Win32::System::Memory::LocalFree(
                windows::Win32::Foundation::HLOCAL(security_descriptor)
            );

            tracing::debug!("Created pipe instance");
            Ok(pipe_handle)
        }
    }

    async fn handle_client(
        pipe_handle: HANDLE,
        job_manager: Arc<JobManager>,
        logger: Arc<Logger>,
    ) -> Result<()> {
        // Get client identity
        let client_user = Self::get_client_identity(pipe_handle)?;
        tracing::info!(client_user = %client_user, "Client connected");

        // Track jobs for this client (for cleanup on disconnect)
        let client_jobs: Arc<std::sync::Mutex<Vec<u32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        // Create frame channel for output
        let (frame_tx, mut frame_rx) = mpsc::channel::<Frame>(FRAME_CHANNEL_SIZE);

        // Track stream pumps for flow control
        let stream_pumps: Arc<std::sync::Mutex<HashMap<u32, Arc<StreamPump>>>> = 
            Arc::new(std::sync::Mutex::new(HashMap::new()));

        // Spawn frame writer task
        let writer_handle = {
            let handle = pipe_handle;
            tokio::spawn(async move {
                while let Some(frame) = frame_rx.recv().await {
                    let encoded = match frame.encode() {
                        Ok(data) => data,
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to encode frame");
                            break;
                        }
                    };

                    // Write to pipe (blocking I/O)
                    let write_result = tokio::task::spawn_blocking({
                        let data = encoded.to_vec();
                        move || write_pipe_blocking(handle, &data)
                    }).await;

                    match write_result {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => {
                            tracing::debug!(error = %e, "Pipe write failed (client disconnect?)");
                            break;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Spawn blocking failed");
                            break;
                        }
                    }
                }
                tracing::debug!("Frame writer task ended");
            })
        };

        // Protocol handler loop
        loop {
            // Read frame from pipe
            let frame = match Self::read_frame_from_pipe(pipe_handle).await {
                Ok(f) => f,
                Err(Error::Protocol(msg)) if msg.contains("EOF") || msg.contains("broken") || msg.contains("disconnect") => {
                    tracing::debug!("Client disconnected");
                    break;
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to read frame");
                    break;
                }
            };

            // Handle frame
            match Self::handle_frame(
                frame,
                &job_manager,
                &logger,
                &frame_tx,
                &client_user,
                &client_jobs,
                &stream_pumps,
            ).await {
                Ok(should_continue) => {
                    if !should_continue {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Error handling frame");
                    // Send error frame
                    let error_frame = Frame::new(FrameType::Error, StreamId::Control)
                        .with_payload(Bytes::from(
                            serde_json::to_string(&crate::protocol::ErrorPayload {
                                job_id: None,
                                error_code: 1,
                                error_message: e.to_string(),
                            }).unwrap_or_default(),
                        ));
                    let _ = frame_tx.send(error_frame).await;
                    break;
                }
            }
        }

        // Cancel all jobs for this client
        let jobs_to_cancel: Vec<u32> = {
            let guard = client_jobs.lock().unwrap();
            guard.clone()
        };
        for job_id in jobs_to_cancel {
            let _ = job_manager.cancel_job(job_id, "Client disconnected");
        }

        // Close pipe
        unsafe {
            DisconnectNamedPipe(pipe_handle).ok();
            CloseHandle(pipe_handle).ok();
        }

        // Wait for writer to finish
        let _ = writer_handle.await;

        tracing::debug!(client_user = %client_user, "Client session ended");
        Ok(())
    }

    fn get_client_identity(pipe_handle: HANDLE) -> Result<String> {
        unsafe {
            // Impersonate client
            ImpersonateNamedPipeClient(pipe_handle)
                .map_err(|e| Error::AuthorizationFailed(format!("Failed to impersonate client: {}", e)))?;

            // Get thread token
            let mut token_handle = HANDLE::default();
            let result = OpenThreadToken(
                GetCurrentThread(),
                TOKEN_QUERY,
                BOOL::from(false),
                &mut token_handle,
            );

            // Revert impersonation regardless of success
            RevertToSelf().ok();

            result.map_err(|e| Error::AuthorizationFailed(format!("Failed to open thread token: {}", e)))?;

            // Get token user info
            let mut token_info_size: u32 = 0;
            let _ = GetTokenInformation(token_handle, TokenUser, None, 0, &mut token_info_size);

            let mut buffer = vec![0u8; token_info_size as usize];
            GetTokenInformation(
                token_handle,
                TokenUser,
                Some(buffer.as_mut_ptr() as *mut std::ffi::c_void),
                token_info_size,
                &mut token_info_size,
            ).map_err(|e| {
                CloseHandle(token_handle).ok();
                Error::AuthorizationFailed(format!("Failed to get token info: {}", e))
            })?;

            CloseHandle(token_handle).ok();

            // Extract SID
            let token_user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let sid = token_user.User.Sid;

            // Convert SID to string
            let sid_string = sid_to_string(sid)?;
            
            // Try to get account name from SID
            let account_name = lookup_account_sid(sid).unwrap_or_else(|_| sid_string.clone());

            Ok(account_name)
        }
    }

    async fn read_frame_from_pipe(pipe_handle: HANDLE) -> Result<Frame> {
        // Read frame length (4 bytes)
        let len_buf = tokio::task::spawn_blocking({
            let handle = pipe_handle;
            move || read_pipe_exact(handle, 4)
        }).await
        .map_err(|e| Error::Protocol(format!("Spawn blocking failed: {}", e)))??;

        if len_buf.len() != 4 {
            return Err(Error::Protocol("Failed to read frame length (EOF or disconnect)".to_string()));
        }

        let frame_len = u32::from_le_bytes([len_buf[0], len_buf[1], len_buf[2], len_buf[3]]) as usize;
        
        if frame_len == 0 {
            return Err(Error::Protocol("Invalid frame length: 0".to_string()));
        }
        if frame_len > crate::protocol::MAX_FRAME_SIZE {
            return Err(Error::Protocol(format!("Frame too large: {} bytes", frame_len)));
        }

        // Read frame body
        let frame_buf = tokio::task::spawn_blocking({
            let handle = pipe_handle;
            move || read_pipe_exact(handle, frame_len)
        }).await
        .map_err(|e| Error::Protocol(format!("Spawn blocking failed: {}", e)))??;

        if frame_buf.len() != frame_len {
            return Err(Error::Protocol(format!(
                "Incomplete frame read: got {} bytes, expected {}",
                frame_buf.len(), frame_len
            )));
        }

        // Parse frame header (12 bytes: type, stream_id, flags, job_id, sequence)
        if frame_len < 12 {
            return Err(Error::Protocol(format!("Frame too small: {} bytes (min: 12)", frame_len)));
        }

        let frame_type = crate::protocol::FrameType::try_from(frame_buf[0])?;
        let stream_id = crate::protocol::StreamId::try_from(frame_buf[1])?;
        let flags = u16::from_le_bytes([frame_buf[2], frame_buf[3]]);
        let job_id_raw = u32::from_le_bytes([frame_buf[4], frame_buf[5], frame_buf[6], frame_buf[7]]);
        let sequence_raw = u32::from_le_bytes([frame_buf[8], frame_buf[9], frame_buf[10], frame_buf[11]]);

        let job_id = if job_id_raw != 0 { Some(job_id_raw) } else { None };
        let sequence_number = if sequence_raw != 0 { Some(sequence_raw) } else { None };

        let payload = if frame_len > 12 {
            Bytes::from(frame_buf[12..].to_vec())
        } else {
            Bytes::new()
        };

        Ok(Frame {
            frame_type,
            stream_id,
            flags,
            job_id,
            sequence_number,
            payload,
        })
    }

    async fn handle_frame(
        frame: Frame,
        job_manager: &Arc<JobManager>,
        logger: &Arc<Logger>,
        frame_tx: &mpsc::Sender<Frame>,
        client_user: &str,
        client_jobs: &Arc<std::sync::Mutex<Vec<u32>>>,
        stream_pumps: &Arc<std::sync::Mutex<HashMap<u32, Arc<StreamPump>>>>,
    ) -> Result<bool> {
        match frame.frame_type {
            FrameType::Hello => {
                // Send HELLO_ACK
                let ack = Frame::new(FrameType::HelloAck, StreamId::Control)
                    .with_payload(Bytes::from(
                        serde_json::to_string(&HelloPayload {
                            version: PROTOCOL_VERSION,
                            capabilities: vec![
                                "multiplexed".to_string(),
                                "flow_control".to_string(),
                            ],
                        }).unwrap(),
                    ));
                frame_tx.send(ack).await.map_err(|e| {
                    Error::Protocol(format!("Failed to send HELLO_ACK: {}", e))
                })?;
                Ok(true)
            }

            FrameType::Run => {
                let payload: RunPayload = serde_json::from_slice(&frame.payload)
                    .map_err(|e| Error::Protocol(format!("Invalid RUN payload: {}", e)))?;

                // Create job
                let timeout = payload.timeout_sec.map(Duration::from_secs);
                let job = job_manager.create_job(timeout)?;
                let job_id = job.job_id;

                // Track job for this client
                {
                    let mut guard = client_jobs.lock().unwrap();
                    guard.push(job_id);
                }

                // Audit log
                logger.audit_command_started(
                    job_id,
                    client_user,
                    &payload.command_line,
                    payload.working_directory.as_deref(),
                )?;

                // Transition to Starting
                job_manager.transition_state(job_id, JobState::Starting)?;

                // Send RUN_ACK
                let ack = Frame::new(FrameType::RunAck, StreamId::Control)
                    .with_job_id(job_id)
                    .with_payload(Bytes::from(
                        serde_json::to_string(&RunAckPayload { job_id }).unwrap(),
                    ));
                frame_tx.send(ack).await.map_err(|e| {
                    Error::Protocol(format!("Failed to send RUN_ACK: {}", e))
                })?;

                // Create stream pump for this job (shared between stdout/stderr)
                let pump = Arc::new(StreamPump::new(DEFAULT_WINDOW_SIZE, false));
                {
                    let mut guard = stream_pumps.lock().unwrap();
                    guard.insert(job_id, pump.clone());
                }

                // Launch process (spawn async task)
                let job_clone = job.clone();
                let job_manager_clone = job_manager.clone();
                let logger_clone = logger.clone();
                let frame_tx_clone = frame_tx.clone();
                let stream_pumps_clone = stream_pumps.clone();

                tokio::spawn(async move {
                    let result = Self::launch_and_monitor_process(
                        job_clone,
                        &payload,
                        job_manager_clone.clone(),
                        logger_clone,
                        frame_tx_clone,
                        pump,
                    ).await;

                    // Remove pump from tracking
                    {
                        let mut guard = stream_pumps_clone.lock().unwrap();
                        guard.remove(&job_id);
                    }

                    if let Err(e) = result {
                        tracing::error!(job_id, error = %e, "Process execution failed");
                        // Ensure job is cleaned up
                        let _ = job_manager_clone.cleanup_job(job_id).await;
                    }
                });

                Ok(true)
            }

            FrameType::Cancel => {
                let job_id = frame.job_id.ok_or(Error::Protocol(
                    "CANCEL frame missing job_id".to_string(),
                ))?;
                
                job_manager.cancel_job(job_id, "Client requested cancellation")?;

                let ack = Frame::new(FrameType::CancelAck, StreamId::Control)
                    .with_job_id(job_id);
                frame_tx.send(ack).await.map_err(|e| {
                    Error::Protocol(format!("Failed to send CANCEL_ACK: {}", e))
                })?;
                Ok(true)
            }

            FrameType::WindowUpdate => {
                // Client acknowledges bytes consumed, update flow control window
                let payload: WindowUpdatePayload = serde_json::from_slice(&frame.payload)
                    .map_err(|e| Error::Protocol(format!("Invalid WINDOW_UPDATE payload: {}", e)))?;

                let job_id = frame.job_id.ok_or(Error::Protocol(
                    "WINDOW_UPDATE frame missing job_id".to_string(),
                ))?;

                // Update stream pump window
                let pump = {
                    let guard = stream_pumps.lock().unwrap();
                    guard.get(&job_id).cloned()
                };

                if let Some(pump) = pump {
                    pump.update_window(payload.bytes_consumed);
                    tracing::trace!(job_id, bytes = payload.bytes_consumed, "Window updated");
                }

                Ok(true)
            }

            FrameType::Ping => {
                let pong = Frame::new(FrameType::Pong, StreamId::Control);
                frame_tx.send(pong).await.map_err(|e| {
                    Error::Protocol(format!("Failed to send PONG: {}", e))
                })?;
                Ok(true)
            }

            _ => {
                tracing::warn!(frame_type = ?frame.frame_type, "Unhandled frame type");
                Ok(true)
            }
        }
    }

    async fn launch_and_monitor_process(
        job: Arc<JobHandle>,
        payload: &RunPayload,
        job_manager: Arc<JobManager>,
        logger: Arc<Logger>,
        frame_tx: mpsc::Sender<Frame>,
        pump: Arc<StreamPump>,
    ) -> Result<()> {
        let job_id = job.job_id;

        // Create pipes for stdio
        let (stdout_read, stdout_write) = create_pipe_pair()?;
        let (stderr_read, stderr_write) = create_pipe_pair()?;
        let (stdin_read, stdin_write) = create_pipe_pair()?;

        // Track child pipe handles for cleanup
        let mut child_handles = ChildPipeHandles {
            stdout_write,
            stderr_write,
            stdin_read,
        };

        // Launch process
        let process_info = match ProcessLauncher::launch_process(
            payload,
            stdout_write,
            stderr_write,
            stdin_read,
        ) {
            Ok(info) => {
                // Close child-side handles (child owns them now)
                child_handles.close_all();
                info
            }
            Err(e) => {
                // Close all handles on failure
                child_handles.close_all();
                unsafe {
                    CloseHandle(stdout_read).ok();
                    CloseHandle(stderr_read).ok();
                    CloseHandle(stdin_write).ok();
                }

                // Transition to Failed and send error
                let _ = job_manager.transition_state(job_id, JobState::Failed);
                
                let error_frame = Frame::new(FrameType::Error, StreamId::Control)
                    .with_job_id(job_id)
                    .with_payload(Bytes::from(
                        serde_json::to_string(&crate::protocol::ErrorPayload {
                            job_id: Some(job_id),
                            error_code: 1,
                            error_message: e.to_string(),
                        }).unwrap_or_default(),
                    ));
                let _ = frame_tx.send(error_frame).await;

                logger.audit_command_failed(job_id, 1, &e.to_string())?;
                return Err(e);
            }
        };

        // Store process handle
        {
            let mut handle_guard = job.process_handle.lock().unwrap();
            *handle_guard = Some(process_info.process_handle);
        }

        // Create job object and assign process
        let job_object = ProcessLauncher::create_job_object()?;
        ProcessLauncher::assign_process_to_job(job_object, process_info.process_handle)?;
        {
            let mut job_obj_guard = job.job_object.lock().unwrap();
            *job_obj_guard = Some(job_object);
        }

        // Transition to Running
        job_manager.transition_state(job_id, JobState::Running)?;

        // Send STATUS frame
        let status_frame = Frame::new(FrameType::Status, StreamId::Control)
            .with_job_id(job_id)
            .with_payload(Bytes::from(
                serde_json::to_string(&crate::protocol::StatusPayload {
                    job_id,
                    status: "Running".to_string(),
                }).unwrap(),
            ));
        frame_tx.send(status_frame).await.map_err(|e| {
            Error::Protocol(format!("Failed to send STATUS: {}", e))
        })?;

        // Start IO pumps
        let pump_stdout = pump.clone();
        let pump_stderr = pump.clone();
        let frame_tx_stdout = frame_tx.clone();
        let frame_tx_stderr = frame_tx.clone();
        let token_stdout = job.cancellation_token.clone();
        let token_stderr = job.cancellation_token.clone();

        let stdout_handle = tokio::spawn(async move {
            pump_stdout(
                stdout_read,
                job_id,
                frame_tx_stdout,
                pump_stdout,
                token_stdout,
            ).await
        });

        let stderr_handle = tokio::spawn(async move {
            pump_stderr(
                stderr_read,
                job_id,
                frame_tx_stderr,
                pump_stderr,
                token_stderr,
            ).await
        });

        // Store pump handles
        {
            let mut guard = job.stdout_pump.lock().unwrap();
            *guard = Some(stdout_handle);
        }
        {
            let mut guard = job.stderr_pump.lock().unwrap();
            *guard = Some(stderr_handle);
        }

        // Start timeout monitor
        let job_timeout = job.clone();
        let job_manager_timeout = job_manager.clone();
        tokio::spawn(async move {
            CancellationManager::monitor_timeout(job_timeout, job_manager_timeout).await;
        });

        // Wait for process exit
        let exit_code = CancellationManager::wait_for_process_exit(
            process_info.process_handle,
            job.cancellation_token.clone(),
        ).await.unwrap_or(-1);

        // Close stdin write handle (signals EOF to process)
        unsafe { CloseHandle(stdin_write).ok(); }

        // Wait for pumps to finish (they'll exit when pipes close)
        {
            let stdout_handle = job.stdout_pump.lock().unwrap().take();
            if let Some(h) = stdout_handle {
                let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
            }
        }
        {
            let stderr_handle = job.stderr_pump.lock().unwrap().take();
            if let Some(h) = stderr_handle {
                let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
            }
        }

        // Close parent's read handles
        unsafe {
            CloseHandle(stdout_read).ok();
            CloseHandle(stderr_read).ok();
        }

        // Transition to Exiting
        let _ = job_manager.transition_state(job_id, JobState::Exiting);

        // Send EXIT frame
        let exit_frame = Frame::new(FrameType::Exit, StreamId::Control)
            .with_job_id(job_id)
            .with_payload(Bytes::from(
                serde_json::to_string(&crate::protocol::ExitPayload {
                    job_id,
                    exit_code,
                }).unwrap(),
            ));
        let _ = frame_tx.send(exit_frame).await;

        // Transition to Completed
        let _ = job_manager.transition_state(job_id, JobState::Completed);

        // Audit log
        let duration = job.start_time.elapsed().unwrap_or_default();
        logger.audit_command_completed(job_id, exit_code, duration.as_millis() as u64)?;

        // Cleanup
        let _ = job_manager.cleanup_job(job_id).await;

        Ok(())
    }
}

/// Create a pipe pair (read handle, write handle)
fn create_pipe_pair() -> Result<(HANDLE, HANDLE)> {
    let mut read_handle = HANDLE::default();
    let mut write_handle = HANDLE::default();

    // Create inheritable security attributes for child handles
    let security_attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: BOOL::from(true), // Handles are inheritable
    };

    unsafe {
        CreatePipe(
            &mut read_handle,
            &mut write_handle,
            Some(&security_attrs),
            PIPE_BUFFER_SIZE,
        ).map_err(|e| Error::Service(format!("Failed to create pipe: {}", e)))?;
    }

    Ok((read_handle, write_handle))
}

/// Read exactly `len` bytes from pipe (blocking)
fn read_pipe_exact(handle: HANDLE, len: usize) -> Result<Vec<u8>> {
    let mut buffer = vec![0u8; len];
    let mut total_read = 0usize;

    while total_read < len {
        let mut bytes_read: u32 = 0;
        unsafe {
            let result = ReadFile(
                handle,
                Some(&mut buffer[total_read..]),
                Some(&mut bytes_read),
                None,
            );

            match result {
                Ok(()) => {
                    if bytes_read == 0 {
                        // EOF
                        break;
                    }
                    total_read += bytes_read as usize;
                }
                Err(e) => {
                    let code = e.code();
                    if code == ERROR_BROKEN_PIPE.into() {
                        // Pipe closed
                        break;
                    }
                    return Err(Error::Protocol(format!("Pipe read error: {}", e)));
                }
            }
        }
    }

    buffer.truncate(total_read);
    Ok(buffer)
}

/// Write data to pipe (blocking)
fn write_pipe_blocking(handle: HANDLE, data: &[u8]) -> Result<usize> {
    let mut total_written = 0usize;

    while total_written < data.len() {
        let mut bytes_written: u32 = 0;
        unsafe {
            let result = WriteFile(
                handle,
                Some(&data[total_written..]),
                Some(&mut bytes_written),
                None,
            );

            match result {
                Ok(()) => {
                    total_written += bytes_written as usize;
                }
                Err(e) => {
                    let code = e.code();
                    if code == ERROR_BROKEN_PIPE.into() {
                        return Err(Error::Protocol("Pipe broken (client disconnect)".to_string()));
                    }
                    return Err(Error::Protocol(format!("Pipe write error: {}", e)));
                }
            }
        }
    }

    Ok(total_written)
}

/// Convert string to null-terminated wide string (UTF-16)
fn to_wide_null(s: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Convert SID to string representation
fn sid_to_string(sid: PSID) -> Result<String> {
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;

    unsafe {
        let mut string_sid: windows::core::PWSTR = windows::core::PWSTR::null();
        ConvertSidToStringSidW(sid, &mut string_sid)
            .map_err(|e| Error::AuthorizationFailed(format!("Failed to convert SID: {}", e)))?;

        let len = (0..).take_while(|&i| *string_sid.0.add(i) != 0).count();
        let slice = std::slice::from_raw_parts(string_sid.0, len);
        let result = String::from_utf16_lossy(slice);

        // Free the string
        windows::Win32::System::Memory::LocalFree(
            windows::Win32::Foundation::HLOCAL(string_sid.0 as *mut std::ffi::c_void)
        );

        Ok(result)
    }
}

/// Look up account name from SID
fn lookup_account_sid(sid: PSID) -> Result<String> {
    use windows::Win32::Security::LookupAccountSidW;
    use windows::Win32::System::SystemServices::SID_NAME_USE;

    unsafe {
        let mut name_len: u32 = 0;
        let mut domain_len: u32 = 0;
        let mut sid_type = SID_NAME_USE::default();

        // First call to get buffer sizes
        let _ = LookupAccountSidW(
            None,
            sid,
            windows::core::PWSTR::null(),
            &mut name_len,
            windows::core::PWSTR::null(),
            &mut domain_len,
            &mut sid_type,
        );

        if name_len == 0 {
            return Err(Error::AuthorizationFailed("Failed to get account name size".to_string()));
        }

        let mut name_buf: Vec<u16> = vec![0; name_len as usize];
        let mut domain_buf: Vec<u16> = vec![0; domain_len as usize];

        LookupAccountSidW(
            None,
            sid,
            windows::core::PWSTR::from_raw(name_buf.as_mut_ptr()),
            &mut name_len,
            windows::core::PWSTR::from_raw(domain_buf.as_mut_ptr()),
            &mut domain_len,
            &mut sid_type,
        ).map_err(|e| Error::AuthorizationFailed(format!("Failed to lookup account: {}", e)))?;

        // Remove null terminators
        while name_buf.last() == Some(&0) { name_buf.pop(); }
        while domain_buf.last() == Some(&0) { domain_buf.pop(); }

        let name = String::from_utf16_lossy(&name_buf);
        let domain = String::from_utf16_lossy(&domain_buf);

        if domain.is_empty() {
            Ok(name)
        } else {
            Ok(format!("{}\\{}", domain, name))
        }
    }
}
