use crate::error::{Error, Result};
use crate::job_manager::{JobHandle, JobManager, JobState};
use crate::process_launcher::ProcessLauncher;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE, WAIT_OBJECT_0, WAIT_TIMEOUT};

pub struct CancellationManager;

impl CancellationManager {
    /// Monitor job for timeout, terminate if exceeded
    pub async fn monitor_timeout(
        job: Arc<JobHandle>,
        job_manager: Arc<JobManager>,
    ) {
        let timeout = match job.timeout {
            Some(t) => t,
            None => return, // No timeout configured
        };

        let start = Instant::now();
        let check_interval = Duration::from_secs(1);

        loop {
            // Check if already terminal (job completed/failed/canceled)
            {
                let state = job.state.lock().unwrap().clone();
                if state.is_terminal() {
                    tracing::trace!(job_id = job.job_id, "Timeout monitor: job in terminal state");
                    return;
                }
            }

            // Check cancellation token
            if job.cancellation_token.is_cancelled() {
                tracing::trace!(job_id = job.job_id, "Timeout monitor: cancellation requested");
                return;
            }

            // Check timeout
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                tracing::warn!(
                    job_id = job.job_id, 
                    timeout_sec = timeout.as_secs(),
                    elapsed_sec = elapsed.as_secs(),
                    "Job timed out"
                );

                // Transition to TimedOut
                if let Err(e) = job_manager.transition_state(job.job_id, JobState::TimedOut) {
                    tracing::warn!(job_id = job.job_id, error = %e, "Failed to transition to TimedOut");
                }

                // Terminate process tree
                if let Err(e) = Self::terminate_job(job.clone()).await {
                    tracing::error!(job_id = job.job_id, error = %e, "Failed to terminate timed-out job");
                }

                return;
            }

            // Wait before next check
            tokio::select! {
                _ = sleep(check_interval) => {}
                _ = job.cancellation_token.cancelled() => {
                    return;
                }
            }
        }
    }

    /// Terminate a job's process tree
    pub async fn terminate_job(job: Arc<JobHandle>) -> Result<()> {
        // Signal cancellation
        job.cancellation_token.cancel();

        // Try termination via Job Object first (cleanest, kills entire process tree)
        let job_obj = {
            let guard = job.job_object.lock().unwrap();
            *guard
        };

        if let Some(job_obj_handle) = job_obj {
            tracing::debug!(job_id = job.job_id, "Terminating via job object");
            
            if let Err(e) = ProcessLauncher::terminate_job_object(job_obj_handle, 1) {
                tracing::warn!(
                    job_id = job.job_id, 
                    error = %e, 
                    "Job object termination failed, trying direct process termination"
                );
            } else {
                // Wait for process to exit (up to 5 seconds)
                let proc_handle = {
                    let guard = job.process_handle.lock().unwrap();
                    *guard
                };
                
                if let Some(handle) = proc_handle {
                    if Self::wait_for_exit_timeout(handle, Duration::from_secs(5)).await.is_ok() {
                        tracing::debug!(job_id = job.job_id, "Process terminated via job object");
                        return Ok(());
                    }
                }
            }
        }

        // Fallback: terminate process directly
        let proc_handle = {
            let guard = job.process_handle.lock().unwrap();
            *guard
        };

        if let Some(handle) = proc_handle {
            tracing::debug!(job_id = job.job_id, "Terminating process directly");
            
            use windows::Win32::System::Threading::TerminateProcess;
            unsafe {
                TerminateProcess(handle, 1)
                    .map_err(|e| Error::ProcessLaunchFailed(format!("TerminateProcess failed: {}", e)))?;
            }
        }

        Ok(())
    }

    /// Wait for process exit with timeout
    async fn wait_for_exit_timeout(process_handle: HANDLE, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let poll_interval = Duration::from_millis(100);

        while Instant::now() < deadline {
            let signaled = unsafe {
                let result = WaitForSingleObject(process_handle, 0);
                result == WAIT_OBJECT_0
            };

            if signaled {
                return Ok(());
            }

            sleep(poll_interval).await;
        }

        Err(Error::ProcessLaunchFailed("Process did not exit within timeout".to_string()))
    }

    /// Wait for process to exit (blocking, use spawn_blocking)
    pub async fn wait_for_process_exit(
        process_handle: HANDLE,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<i32> {
        use tokio::task::spawn_blocking;

        let wait_handle = spawn_blocking(move || {
            unsafe {
                // Wait with periodic checks (1 second intervals) to allow cancellation
                loop {
                    let result = WaitForSingleObject(process_handle, 1000);
                    
                    if result == WAIT_OBJECT_0 {
                        // Process exited
                        use windows::Win32::System::Threading::GetExitCodeProcess;
                        let mut exit_code: u32 = 0;
                        if GetExitCodeProcess(process_handle, &mut exit_code).is_ok() {
                            return Ok(exit_code as i32);
                        } else {
                            return Ok(-1); // Unknown exit code
                        }
                    } else if result == WAIT_TIMEOUT {
                        // Still running, check cancellation on next iteration
                        // Note: we can't check cancellation_token from blocking task,
                        // but tokio::select below handles that
                        continue;
                    } else {
                        // Error
                        return Err(Error::ProcessLaunchFailed(format!(
                            "WaitForSingleObject failed with code: {:?}",
                            result
                        )));
                    }
                }
            }
        });

        tokio::select! {
            result = wait_handle => {
                match result {
                    Ok(inner) => inner,
                    Err(e) => Err(Error::ProcessLaunchFailed(format!("Wait task failed: {}", e))),
                }
            }
            _ = cancellation_token.cancelled() => {
                tracing::debug!("Wait cancelled by cancellation token");
                Err(Error::ProcessLaunchFailed("Wait cancelled".to_string()))
            }
        }
    }
}
