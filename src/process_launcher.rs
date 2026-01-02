#![cfg(windows)]

use crate::error::{Error, Result};
use std::ffi::OsStr;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

pub struct ChildPipeHandles {
    pub stdin_read: HANDLE,
    pub stdin_write: HANDLE,
    pub stdout_read: HANDLE,
    pub stdout_write: HANDLE,
    pub stderr_read: HANDLE,
    pub stderr_write: HANDLE,
}

impl ChildPipeHandles {
    pub fn close_all(&mut self) {
        unsafe {
            let handles = [
                &mut self.stdin_read,
                &mut self.stdin_write,
                &mut self.stdout_read,
                &mut self.stdout_write,
                &mut self.stderr_read,
                &mut self.stderr_write,
            ];
            for h in handles {
                if !h.is_invalid() && h.0 != null_mut() {
                    let _ = CloseHandle(*h);
                    *h = HANDLE::default();
                }
            }
        }
    }
}

impl Drop for ChildPipeHandles {
    fn drop(&mut self) {
        self.close_all();
    }
}

pub struct LaunchedProcess {
    pub process_handle: HANDLE,
    pub thread_handle: HANDLE,
    pub process_id: u32,
    pub job_object: HANDLE,
    pub stdout_read: HANDLE,
    pub stderr_read: HANDLE,
    pub stdin_write: HANDLE,
}

impl Drop for LaunchedProcess {
    fn drop(&mut self) {
        unsafe {
            if !self.thread_handle.is_invalid() && self.thread_handle.0 != null_mut() {
                let _ = CloseHandle(self.thread_handle);
            }
            // Don't close process_handle or job_object here - they're managed by JobHandle
        }
    }
}

pub struct ProcessLauncher;

impl ProcessLauncher {
    pub fn launch(
        command_line: &str,
        working_directory: Option<&str>,
        environment: Option<&std::collections::HashMap<String, String>>,
    ) -> Result<LaunchedProcess> {
        let mut pipes = Self::create_pipes()?;
        let job_object = Self::create_job_object()?;

        let result = Self::do_launch(command_line, working_directory, environment, &pipes);

        match result {
            Ok(mut proc) => {
                // Assign to job object
                unsafe {
                    AssignProcessToJobObject(job_object, proc.process_handle)?;
                }
                proc.job_object = job_object;

                // Close write ends, we only read from child
                unsafe {
                    if !pipes.stdout_write.is_invalid() {
                        let _ = CloseHandle(pipes.stdout_write);
                        pipes.stdout_write = HANDLE::default();
                    }
                    if !pipes.stderr_write.is_invalid() {
                        let _ = CloseHandle(pipes.stderr_write);
                        pipes.stderr_write = HANDLE::default();
                    }
                    if !pipes.stdin_read.is_invalid() {
                        let _ = CloseHandle(pipes.stdin_read);
                        pipes.stdin_read = HANDLE::default();
                    }
                }

                proc.stdout_read = pipes.stdout_read;
                proc.stderr_read = pipes.stderr_read;
                proc.stdin_write = pipes.stdin_write;

                // Clear to prevent Drop from closing them
                pipes.stdout_read = HANDLE::default();
                pipes.stderr_read = HANDLE::default();
                pipes.stdin_write = HANDLE::default();

                Ok(proc)
            }
            Err(e) => {
                unsafe {
                    let _ = CloseHandle(job_object);
                }
                Err(e)
            }
        }
    }

    fn do_launch(
        command_line: &str,
        working_directory: Option<&str>,
        environment: Option<&std::collections::HashMap<String, String>>,
        pipes: &ChildPipeHandles,
    ) -> Result<LaunchedProcess> {
        let cmd_wide: Vec<u16> = OsStr::new(command_line)
            .encode_wide()
            .chain(once(0))
            .collect();

        let wd_wide: Option<Vec<u16>> =
            working_directory.map(|wd| OsStr::new(wd).encode_wide().chain(once(0)).collect());

        let env_block: Option<Vec<u16>> = environment.map(|env| Self::build_env_block(env));

        let mut startup_info = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: pipes.stdin_read,
            hStdOutput: pipes.stdout_write,
            hStdError: pipes.stderr_write,
            ..Default::default()
        };

        let mut process_info = PROCESS_INFORMATION::default();

        let creation_flags = CREATE_NEW_PROCESS_GROUP | CREATE_UNICODE_ENVIRONMENT;

        unsafe {
            // CreateProcessW expects PWSTR (mutable) for command line
            let mut cmd_wide_mut = cmd_wide.clone();
            let cmd_ptr = PWSTR::from_raw(cmd_wide_mut.as_mut_ptr());

            // For working directory, use match to handle None vs Some properly
            // windows-rs requires explicit handling of Option parameters
            match wd_wide {
                Some(ref wd) => {
                    let wd_ptr = PCWSTR::from_raw(wd.as_ptr());
                    CreateProcessW(
                        None,
                        cmd_ptr,
                        None,
                        None,
                        true,
                        creation_flags,
                        env_block
                            .as_ref()
                            .map(|b| b.as_ptr() as *const std::ffi::c_void),
                        wd_ptr, // Pass PCWSTR directly, not wrapped in Option
                        &mut startup_info,
                        &mut process_info,
                    )
                    .map_err(|e| Error::ProcessLaunchFailed(format!("CreateProcessW: {}", e)))?;
                }
                None => {
                    CreateProcessW(
                        None,
                        cmd_ptr,
                        None,
                        None,
                        true,
                        creation_flags,
                        env_block
                            .as_ref()
                            .map(|b| b.as_ptr() as *const std::ffi::c_void),
                        PCWSTR::null(), // Use null PCWSTR instead of None
                        &mut startup_info,
                        &mut process_info,
                    )
                    .map_err(|e| Error::ProcessLaunchFailed(format!("CreateProcessW: {}", e)))?;
                }
            }
        }

        Ok(LaunchedProcess {
            process_handle: process_info.hProcess,
            thread_handle: process_info.hThread,
            process_id: process_info.dwProcessId,
            job_object: HANDLE::default(),
            stdout_read: HANDLE::default(),
            stderr_read: HANDLE::default(),
            stdin_write: HANDLE::default(),
        })
    }

    fn create_pipes() -> Result<ChildPipeHandles> {
        unsafe {
            let mut sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                bInheritHandle: true.into(),
                lpSecurityDescriptor: null_mut(),
            };

            let mut stdin_read = HANDLE::default();
            let mut stdin_write = HANDLE::default();
            let mut stdout_read = HANDLE::default();
            let mut stdout_write = HANDLE::default();
            let mut stderr_read = HANDLE::default();
            let mut stderr_write = HANDLE::default();

            CreatePipe(&mut stdin_read, &mut stdin_write, Some(&sa), 0)
                .map_err(|e| Error::ProcessLaunchFailed(format!("CreatePipe stdin: {}", e)))?;

            CreatePipe(&mut stdout_read, &mut stdout_write, Some(&sa), 0)
                .map_err(|e| Error::ProcessLaunchFailed(format!("CreatePipe stdout: {}", e)))?;

            CreatePipe(&mut stderr_read, &mut stderr_write, Some(&sa), 0)
                .map_err(|e| Error::ProcessLaunchFailed(format!("CreatePipe stderr: {}", e)))?;

            // Note: SetHandleInformation is not available in windows-rs 0.58
            // Pipe handles inherit correctly via SECURITY_ATTRIBUTES.bInheritHandle = true
            // The child process will inherit the handles as intended

            Ok(ChildPipeHandles {
                stdin_read,
                stdin_write,
                stdout_read,
                stdout_write,
                stderr_read,
                stderr_write,
            })
        }
    }

    fn create_job_object() -> Result<HANDLE> {
        unsafe {
            let job = CreateJobObjectW(None, None)
                .map_err(|e| Error::ProcessLaunchFailed(format!("CreateJobObjectW: {}", e)))?;

            let mut limit_info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limit_info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limit_info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|e| Error::ProcessLaunchFailed(format!("SetInformationJobObject: {}", e)))?;

            Ok(job)
        }
    }

    pub fn terminate_job_object(job: HANDLE, exit_code: u32) -> Result<()> {
        unsafe {
            TerminateJobObject(job, exit_code)
                .map_err(|e| Error::ProcessLaunchFailed(format!("TerminateJobObject: {}", e)))
        }
    }

    fn build_env_block(env: &std::collections::HashMap<String, String>) -> Vec<u16> {
        let mut block: Vec<u16> = Vec::new();
        for (k, v) in env {
            let entry = format!("{}={}", k, v);
            block.extend(OsStr::new(&entry).encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }
}
