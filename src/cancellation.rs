#![cfg(windows)]

use crate::error::{Error, Result};
use crate::job_manager::{JobHandle, JobManager, JobState};
use crate::process_launcher::ProcessLauncher;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};

const WAIT_TIMEOUT_MS: u32 = 0;
// WAIT_OBJECT_0 = 0 - The state of the object is signaled
const WAIT_OBJECT_0: u32 = 0;

pub struct CancellationManager;

impl CancellationManager {
    pub async fn monitor_timeout(job: Arc<JobHandle>, job_manager: Arc<JobManager>) {
        let timeout = match job.timeout {
            Some(t) => t,
            None => return,
        };

        let start = Instant::now();
        let check_interval = Duration::from_secs(1);

        loop {
            {
                let state = job.state.lock().unwrap().clone();
                if state.is_terminal() {
                    return;
                }
            }

            if job.cancellation_token.is_cancelled() {
                return;
            }

            if start.elapsed() >= timeout {
                tracing::warn!(job_id = job.job_id, "Job timed out");

                let _ = job_manager.transition_state(job.job_id, JobState::TimedOut);

                if let Err(e) = Self::terminate_job(job.clone()).await {
                    tracing::error!(job_id = job.job_id, error = %e, "Failed to terminate");
                }
                return;
            }

            tokio::select! {
                _ = sleep(check_interval) => {}
                _ = job.cancellation_token.cancelled() => { return; }
            }
        }
    }

    pub async fn terminate_job(job: Arc<JobHandle>) -> Result<()> {
        job.cancellation_token.cancel();

        // Try job object termination first
        let job_obj_ptr = job.job_object.get();
        if job_obj_ptr != 0 {
            let job_obj = HANDLE(job_obj_ptr as *mut std::ffi::c_void);
            if let Err(e) = ProcessLauncher::terminate_job_object(job_obj, 1) {
                tracing::warn!(error = %e, "Job object termination failed");
            } else {
                let proc_ptr = job.process_handle.get();
                if proc_ptr != 0 {
                    if Self::wait_for_exit_timeout(proc_ptr, Duration::from_secs(5))
                        .await
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
            }
        }

        // Fallback: direct process termination
        let proc_ptr = job.process_handle.get();
        if proc_ptr != 0 {
            let proc_handle = HANDLE(proc_ptr as *mut std::ffi::c_void);
            unsafe {
                TerminateProcess(proc_handle, 1)
                    .map_err(|e| Error::ProcessLaunchFailed(format!("TerminateProcess: {}", e)))?;
            }
        }

        Ok(())
    }

    async fn wait_for_exit_timeout(process_handle_ptr: isize, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;

        while Instant::now() < deadline {
            let signaled = unsafe {
                let handle = HANDLE(process_handle_ptr as *mut std::ffi::c_void);
                let result = WaitForSingleObject(handle, WAIT_TIMEOUT_MS);
                result.0 == WAIT_OBJECT_0
            };

            if signaled {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }

        Err(Error::ProcessLaunchFailed(
            "Process did not exit within timeout".to_string(),
        ))
    }

    pub async fn wait_for_process_exit(
        handle_ptr: isize,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<i32> {
        let wait_handle = tokio::task::spawn_blocking(move || {
            let handle = HANDLE(handle_ptr as *mut std::ffi::c_void);
            unsafe {
                loop {
                    let result = WaitForSingleObject(handle, 1000);
                    if result.0 == WAIT_OBJECT_0 {
                        let mut exit_code: u32 = 0;
                        if GetExitCodeProcess(handle, &mut exit_code).is_ok() {
                            return Ok(exit_code as i32);
                        } else {
                            return Ok(-1);
                        }
                    }
                    // Still running, continue loop
                }
            }
        });

        tokio::select! {
            result = wait_handle => {
                match result {
                    Ok(inner) => inner,
                    Err(e) => Err(Error::ProcessLaunchFailed(format!("Wait failed: {}", e))),
                }
            }
            _ = cancellation_token.cancelled() => {
                Err(Error::ProcessLaunchFailed("Wait cancelled".to_string()))
            }
        }
    }
}
