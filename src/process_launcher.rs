use crate::error::{Error, Result};
use crate::protocol::RunPayload;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::System::Threading::{
    CreateProcessW, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// Process information returned after successful launch
pub struct ProcessInfo {
    pub process_handle: HANDLE,
    pub process_id: u32,
}

/// Handles that need to be closed after process launch
pub struct ChildPipeHandles {
    pub stdout_write: HANDLE,
    pub stderr_write: HANDLE,
    pub stdin_read: HANDLE,
}

impl ChildPipeHandles {
    /// Close all handles (call this after CreateProcess)
    pub fn close_all(&mut self) {
        unsafe {
            if !self.stdout_write.is_invalid() {
                CloseHandle(self.stdout_write).ok();
                self.stdout_write = HANDLE::default();
            }
            if !self.stderr_write.is_invalid() {
                CloseHandle(self.stderr_write).ok();
                self.stderr_write = HANDLE::default();
            }
            if !self.stdin_read.is_invalid() {
                CloseHandle(self.stdin_read).ok();
                self.stdin_read = HANDLE::default();
            }
        }
    }
}

pub struct ProcessLauncher;

impl ProcessLauncher {
    /// Launch a process with the given configuration
    ///
    /// # Arguments
    /// * `payload` - Run configuration (command line, working dir, env vars)
    /// * `stdout_write` - Write end of stdout pipe (will be inherited by child)
    /// * `stderr_write` - Write end of stderr pipe (will be inherited by child)
    /// * `stdin_read` - Read end of stdin pipe (will be inherited by child)
    ///
    /// # Returns
    /// * `ProcessInfo` - Process handle and ID
    ///
    /// # Note
    /// Caller must close stdout_write, stderr_write, stdin_read handles AFTER
    /// this function returns successfully (child now owns them).
    pub fn launch_process(
        payload: &RunPayload,
        stdout_write: HANDLE,
        stderr_write: HANDLE,
        stdin_read: HANDLE,
    ) -> Result<ProcessInfo> {
        // Validate command line length (Windows limit is 32767)
        if payload.command_line.len() > 32_767 {
            return Err(Error::ProcessLaunchFailed(format!(
                "Command line too long: {} chars (max: 32767)",
                payload.command_line.len()
            )));
        }

        // Validate command line is not empty
        if payload.command_line.trim().is_empty() {
            return Err(Error::ProcessLaunchFailed(
                "Command line cannot be empty".to_string(),
            ));
        }

        // Validate working directory
        let working_dir_wide: Option<Vec<u16>> = if let Some(ref cwd) = payload.working_directory {
            let path = Path::new(cwd);
            if !path.exists() {
                return Err(Error::ProcessLaunchFailed(format!(
                    "Working directory does not exist: {}",
                    cwd
                )));
            }
            if !path.is_dir() {
                return Err(Error::ProcessLaunchFailed(format!(
                    "Working directory is not a directory: {}",
                    cwd
                )));
            }
            Some(to_wide_null(cwd))
        } else {
            None
        };

        // Build environment block
        let env_block = Self::build_environment_block(payload.environment.as_ref())?;

        // Prepare STARTUPINFO with stdio handles
        let mut startup_info = STARTUPINFOW::default();
        startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        startup_info.dwFlags = STARTF_USESTDHANDLES;
        startup_info.hStdInput = stdin_read;
        startup_info.hStdOutput = stdout_write;
        startup_info.hStdError = stderr_write;

        // Prepare PROCESS_INFORMATION
        let mut process_info = PROCESS_INFORMATION::default();

        // Convert command line to wide string (mutable for CreateProcessW)
        let mut cmd_line_wide = to_wide_null(&payload.command_line);

        // Working directory pointer
        let cwd_ptr = working_dir_wide
            .as_ref()
            .map(|v| v.as_ptr())
            .unwrap_or(std::ptr::null());

        // Environment block pointer
        let env_ptr = env_block
            .as_ref()
            .map(|b| b.as_ptr() as *const std::ffi::c_void)
            .unwrap_or(std::ptr::null());

        // Launch process
        let creation_flags = CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT;

        unsafe {
            let success = CreateProcessW(
                None, // Application name (use command line instead)
                windows::core::PWSTR::from_raw(cmd_line_wide.as_mut_ptr()),
                None,             // Process security attributes
                None,             // Thread security attributes
                BOOL::from(true), // Inherit handles
                creation_flags,
                Some(env_ptr),
                windows::core::PCWSTR::from_raw(cwd_ptr),
                &startup_info,
                &mut process_info,
            );

            if success.as_bool() {
                // Close thread handle immediately (not needed)
                CloseHandle(process_info.hThread).ok();

                tracing::debug!(
                    pid = process_info.dwProcessId,
                    "Process launched successfully"
                );

                Ok(ProcessInfo {
                    process_handle: process_info.hProcess,
                    process_id: process_info.dwProcessId,
                })
            } else {
                let err = windows::core::Error::from_win32();
                Err(Error::ProcessLaunchFailed(format!(
                    "CreateProcess failed: {} (command: {})",
                    err, payload.command_line
                )))
            }
        }
    }

    fn build_environment_block(
        env_vars: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<Option<Vec<u16>>> {
        let env_vars = match env_vars {
            Some(vars) if !vars.is_empty() => vars,
            _ => return Ok(None),
        };

        // Validate total size (Windows limit ~32KB typically)
        let total_size: usize = env_vars
            .iter()
            .map(|(k, v)| k.len() + v.len() + 2) // +2 for '=' and null terminator
            .sum();
        if total_size > 32_767 {
            return Err(Error::ProcessLaunchFailed(format!(
                "Environment block too large: {} chars (max: 32767)",
                total_size
            )));
        }

        // Build environment block: KEY=VALUE\0KEY=VALUE\0\0
        let mut block = Vec::new();
        for (key, value) in env_vars {
            // Validate key (no = or null characters)
            if key.contains('=') || key.contains('\0') {
                return Err(Error::ProcessLaunchFailed(format!(
                    "Invalid environment variable name: {} (contains = or null)",
                    key
                )));
            }
            if key.is_empty() {
                return Err(Error::ProcessLaunchFailed(
                    "Environment variable name cannot be empty".to_string(),
                ));
            }
            // Validate value (no null characters)
            if value.contains('\0') {
                return Err(Error::ProcessLaunchFailed(format!(
                    "Invalid environment variable value for {}: contains null character",
                    key
                )));
            }

            // Convert to wide string
            let entry = format!("{}={}", key, value);
            let wide: Vec<u16> = OsStr::new(&entry).encode_wide().chain(Some(0)).collect();
            block.extend_from_slice(&wide);
        }
        block.push(0); // Double null terminator

        Ok(Some(block))
    }

    /// Create a Windows Job Object for process tree management
    pub fn create_job_object() -> Result<HANDLE> {
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        unsafe {
            let job_handle = CreateJobObjectW(None, None)?;

            // Configure job object to kill all processes on close
            let mut limit_info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limit_info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            SetInformationJobObject(
                job_handle,
                JobObjectExtendedLimitInformation,
                &limit_info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;

            tracing::debug!("Job object created");
            Ok(job_handle)
        }
    }

    /// Assign a process to a job object
    pub fn assign_process_to_job(job_handle: HANDLE, process_handle: HANDLE) -> Result<()> {
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;

        unsafe {
            AssignProcessToJobObject(job_handle, process_handle).map_err(|e| {
                Error::ProcessLaunchFailed(format!("AssignProcessToJobObject failed: {}", e))
            })
        }
    }

    /// Terminate all processes in a job object
    pub fn terminate_job_object(job_handle: HANDLE, exit_code: u32) -> Result<()> {
        use windows::Win32::System::JobObjects::TerminateJobObject;

        unsafe {
            TerminateJobObject(job_handle, exit_code).map_err(|e| {
                Error::ProcessLaunchFailed(format!("TerminateJobObject failed: {}", e))
            })
        }
    }
}

/// Convert a string to null-terminated wide string (UTF-16)
fn to_wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}
