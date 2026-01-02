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
}

impl JobManager {
    pub fn new(max_concurrent_jobs: usize) -> Self {
        Self {
            jobs: DashMap::new(),
            next_job_id: AtomicU32::new(1),
            max_concurrent_jobs,
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

        let handle = Arc::new(JobHandle {
            job_id,
            state: Mutex::new(JobState::Created),
            process_handle: SafeHandle::default(),
            job_object: SafeHandle::default(),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            start_time: SystemTime::now(),
            timeout,
        });

        self.jobs.insert(job_id, handle.clone());
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
}
