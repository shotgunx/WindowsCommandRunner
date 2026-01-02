#![cfg(windows)]

use crate::error::{Error, Result};
use bytes::Bytes;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// A Send-safe wrapper for Windows HANDLE
#[derive(Debug)]
pub struct SafeHandle(AtomicUsize);

impl SafeHandle {
    pub fn new(handle: isize) -> Self {
        Self(AtomicUsize::new(handle as usize))
    }

    pub fn get(&self) -> isize {
        self.0.load(Ordering::SeqCst) as isize
    }

    pub fn take(&self) -> Option<isize> {
        let val = self.0.swap(0, Ordering::SeqCst) as isize;
        if val == 0 {
            None
        } else {
            Some(val)
        }
    }

    pub fn set(&self, handle: isize) {
        self.0.store(handle as usize, Ordering::SeqCst);
    }

    pub fn is_valid(&self) -> bool {
        self.get() != 0
    }
}

unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

impl Default for SafeHandle {
    fn default() -> Self {
        Self::new(0)
    }
}

pub struct JobHandle {
    pub job_id: u32,
    pub state: Mutex<JobState>,
    pub process_handle: SafeHandle,
    pub job_object: SafeHandle,
    pub cancellation_token: tokio_util::sync::CancellationToken,
    pub start_time: SystemTime,
    pub timeout: Option<Duration>,
    pub completed_at: Mutex<Option<SystemTime>>,
    pub last_activity: Mutex<SystemTime>,
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Foundation::HANDLE;

        if let Some(h) = self.process_handle.take() {
            unsafe {
                let _ = CloseHandle(HANDLE(h as *mut std::ffi::c_void));
            }
        }
        if let Some(h) = self.job_object.take() {
            unsafe {
                let _ = CloseHandle(HANDLE(h as *mut std::ffi::c_void));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Created,
    Starting,
    Running,
    Exiting,
    Completed,
    Failed,
    Canceled,
    TimedOut,
}

impl JobState {
    pub fn can_transition_to(&self, new_state: &JobState) -> bool {
        match (self, new_state) {
            (JobState::Created, JobState::Starting) => true,
            (JobState::Starting, JobState::Running) => true,
            (JobState::Starting, JobState::Failed) => true,
            (JobState::Running, JobState::Exiting) => true,
            (JobState::Running, JobState::Canceled) => true,
            (JobState::Running, JobState::TimedOut) => true,
            (JobState::Running, JobState::Failed) => true,
            (JobState::Exiting, JobState::Completed) => true,
            (JobState::Exiting, JobState::Failed) => true,
            (JobState::Created, JobState::Canceled) => true,
            (JobState::Starting, JobState::Canceled) => true,
            (JobState::Exiting, JobState::Canceled) => true,
            _ => false,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Completed | JobState::Failed | JobState::Canceled | JobState::TimedOut
        )
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobState::Created => write!(f, "Created"),
            JobState::Starting => write!(f, "Starting"),
            JobState::Running => write!(f, "Running"),
            JobState::Exiting => write!(f, "Exiting"),
            JobState::Completed => write!(f, "Completed"),
            JobState::Failed => write!(f, "Failed"),
            JobState::Canceled => write!(f, "Canceled"),
            JobState::TimedOut => write!(f, "TimedOut"),
        }
    }
}

pub struct JobManager {
    jobs: DashMap<u32, Arc<JobHandle>>,
    next_job_id: AtomicU32,
    max_concurrent_jobs: usize,
    last_job_created: Mutex<SystemTime>,
}

impl JobManager {
    pub fn new(max_concurrent_jobs: usize) -> Self {
        Self {
            jobs: DashMap::new(),
            next_job_id: AtomicU32::new(1),
            max_concurrent_jobs,
            last_job_created: Mutex::new(SystemTime::now()),
        }
    }

    pub fn create_job(&self, timeout: Option<Duration>) -> Result<Arc<JobHandle>> {
        if self.jobs.len() >= self.max_concurrent_jobs {
            return Err(Error::Service(format!(
                "Maximum concurrent jobs ({}) reached",
                self.max_concurrent_jobs
            )));
        }

        let mut job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        if job_id == 0 {
            job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        }

        if self.jobs.contains_key(&job_id) {
            return Err(Error::JobExists(job_id));
        }

        let now = SystemTime::now();
        let handle = Arc::new(JobHandle {
            job_id,
            state: Mutex::new(JobState::Created),
            process_handle: SafeHandle::default(),
            job_object: SafeHandle::default(),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            start_time: now,
            timeout,
            completed_at: Mutex::new(None),
            last_activity: Mutex::new(now),
        });

        self.jobs.insert(job_id, handle.clone());
        *self.last_job_created.lock().unwrap() = now;
        tracing::debug!(job_id, "Job created");
        Ok(handle)
    }

    pub fn get_job(&self, job_id: u32) -> Option<Arc<JobHandle>> {
        self.jobs.get(&job_id).map(|entry| entry.value().clone())
    }

    pub fn transition_state(&self, job_id: u32, new_state: JobState) -> Result<JobState> {
        let job = self.get_job(job_id).ok_or(Error::JobNotFound(job_id))?;

        let mut state_guard = job.state.lock().unwrap();
        let old_state = state_guard.clone();

        if !old_state.can_transition_to(&new_state) {
            return Err(Error::InvalidStateTransition(format!(
                "Job {}: cannot transition from {} to {}",
                job_id, old_state, new_state
            )));
        }

        tracing::debug!(job_id, from = %old_state, to = %new_state, "Job state transition");
        *state_guard = new_state;

        // Update last_activity and completed_at timestamps
        let now = SystemTime::now();
        *job.last_activity.lock().unwrap() = now;
        if new_state.is_terminal() {
            *job.completed_at.lock().unwrap() = Some(now);
        }

        drop(state_guard);

        Ok(old_state)
    }

    pub fn cancel_job(&self, job_id: u32, reason: &str) -> Result<()> {
        let job = self.get_job(job_id).ok_or(Error::JobNotFound(job_id))?;

        {
            let state = job.state.lock().unwrap();
            if state.is_terminal() {
                tracing::debug!(job_id, reason, "Job already in terminal state");
                return Ok(());
            }
        }

        job.cancellation_token.cancel();
        let _ = self.transition_state(job_id, JobState::Canceled);

        tracing::info!(job_id, reason, "Job canceled");
        Ok(())
    }

    pub fn cleanup_job(&self, job_id: u32) -> Result<()> {
        let job = self.get_job(job_id).ok_or(Error::JobNotFound(job_id))?;
        job.cancellation_token.cancel();

        // Handles are closed by Drop
        self.jobs.remove(&job_id);
        tracing::debug!(job_id, "Job cleaned up");
        Ok(())
    }

    pub fn list_active_jobs(&self) -> Vec<u32> {
        self.jobs.iter().map(|entry| *entry.key()).collect()
    }

    pub fn has_active_jobs(&self) -> bool {
        !self.jobs.is_empty()
    }

    pub fn force_cleanup_all(&self) -> Result<()> {
        let job_ids: Vec<u32> = self.jobs.iter().map(|entry| *entry.key()).collect();
        for job_id in job_ids {
            let _ = self.cleanup_job(job_id);
        }
        Ok(())
    }

    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }

    /// Clean up jobs that have been idle for longer than the specified duration
    /// Also cleans up all completed jobs if all jobs are terminal and no new job for quiet_duration
    pub fn cleanup_idle_jobs(&self, idle_duration: Duration, quiet_duration: Duration) -> usize {
        let now = SystemTime::now();
        let mut cleaned = 0;

        // Check if all jobs are terminal and no new job for quiet_duration
        {
            let last_job_time = *self.last_job_created.lock().unwrap();
            let mut all_terminal = true;
            let mut has_jobs = false;

            for entry in self.jobs.iter() {
                has_jobs = true;
                let state = entry.value().state.lock().unwrap();
                if !state.is_terminal() {
                    all_terminal = false;
                    break;
                }
            }

            if has_jobs && all_terminal {
                if let Ok(elapsed) = now.duration_since(last_job_time) {
                    if elapsed >= quiet_duration {
                        tracing::info!(
                            elapsed_secs = elapsed.as_secs(),
                            "All jobs are terminal and quiet for {} seconds, cleaning up all completed jobs",
                            quiet_duration.as_secs()
                        );
                        // Clean up all completed jobs
                        let job_ids: Vec<u32> =
                            self.jobs.iter().map(|entry| *entry.key()).collect();
                        for job_id in job_ids {
                            if self.cleanup_job(job_id).is_ok() {
                                cleaned += 1;
                            }
                        }
                        return cleaned;
                    }
                }
            }
        }

        // Normal idle cleanup for individual jobs
        let mut to_remove = Vec::new();

        for entry in self.jobs.iter() {
            let job_id = *entry.key();
            let job = entry.value();

            let should_cleanup = {
                let state = job.state.lock().unwrap();
                let completed_at = job.completed_at.lock().unwrap();
                let last_activity = job.last_activity.lock().unwrap();

                if state.is_terminal() {
                    // Job is in terminal state - check if it's been idle long enough
                    if let Some(completed_time) = *completed_at {
                        if let Ok(elapsed) = now.duration_since(completed_time) {
                            if elapsed >= idle_duration {
                                tracing::debug!(
                                    job_id,
                                    elapsed_secs = elapsed.as_secs(),
                                    "Cleaning up idle completed job"
                                );
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    // Job is still active - check last activity time
                    if let Ok(elapsed) = now.duration_since(*last_activity) {
                        if elapsed >= idle_duration {
                            tracing::debug!(
                                job_id,
                                elapsed_secs = elapsed.as_secs(),
                                state = %state,
                                "Cleaning up idle active job"
                            );
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
            };

            if should_cleanup {
                to_remove.push(job_id);
            }
        }

        for job_id in to_remove {
            if self.cleanup_job(job_id).is_ok() {
                cleaned += 1;
            }
        }

        if cleaned > 0 {
            tracing::info!(cleaned, "Cleaned up idle jobs");
        }

        cleaned
    }

    /// Update the last activity timestamp for a job
    pub fn update_activity(&self, job_id: u32) {
        if let Some(job) = self.get_job(job_id) {
            *job.last_activity.lock().unwrap() = SystemTime::now();
        }
    }
}
