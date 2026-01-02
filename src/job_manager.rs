use crate::error::{Error, Result};
use bytes::Bytes;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use windows::Win32::Foundation::{CloseHandle, HANDLE};

pub struct JobHandle {
    pub job_id: u32,
    pub state: Arc<Mutex<JobState>>,
    pub process_handle: Arc<Mutex<Option<HANDLE>>>,
    pub job_object: Arc<Mutex<Option<HANDLE>>>,
    pub stdout_pump: Arc<Mutex<Option<tokio::task::JoinHandle<Result<()>>>>>,
    pub stderr_pump: Arc<Mutex<Option<tokio::task::JoinHandle<Result<()>>>>>,
    pub stdin_writer: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Bytes>>>>,
    pub cancellation_token: tokio_util::sync::CancellationToken,
    pub start_time: SystemTime,
    pub timeout: Option<Duration>,
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        // Ensure handles are closed when JobHandle is dropped
        if let Ok(mut guard) = self.process_handle.lock() {
            if let Some(h) = guard.take() {
                unsafe {
                    CloseHandle(h).ok();
                }
            }
        }
        if let Ok(mut guard) = self.job_object.lock() {
            if let Some(h) = guard.take() {
                unsafe {
                    CloseHandle(h).ok();
                }
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
            // Allow cancellation from any non-terminal state
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
        // Check concurrent job limit
        if self.jobs.len() >= self.max_concurrent_jobs {
            return Err(Error::Service(format!(
                "Maximum concurrent jobs ({}) reached",
                self.max_concurrent_jobs
            )));
        }

        // FIX: Properly handle job_id wrap-around to skip 0
        let mut job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        if job_id == 0 {
            // Wrapped around, skip 0 (reserved for "no job")
            job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        }

        // Check for collision (extremely rare but possible)
        if self.jobs.contains_key(&job_id) {
            return Err(Error::JobExists(job_id));
        }

        let handle = Arc::new(JobHandle {
            job_id,
            state: Arc::new(Mutex::new(JobState::Created)),
            process_handle: Arc::new(Mutex::new(None)),
            job_object: Arc::new(Mutex::new(None)),
            stdout_pump: Arc::new(Mutex::new(None)),
            stderr_pump: Arc::new(Mutex::new(None)),
            stdin_writer: Arc::new(Mutex::new(None)),
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

        // Check if already terminal
        {
            let state = job.state.lock().unwrap();
            if state.is_terminal() {
                tracing::debug!(
                    job_id,
                    reason,
                    "Job already in terminal state, skipping cancel"
                );
                return Ok(());
            }
        }

        // Cancel token first (signals all workers)
        job.cancellation_token.cancel();

        // Transition to Canceled state (may fail if already terminal, that's OK)
        let _ = self.transition_state(job_id, JobState::Canceled);

        tracing::info!(job_id, reason, "Job canceled");

        Ok(())
    }

    pub async fn cleanup_job(&self, job_id: u32) -> Result<()> {
        let job = self.get_job(job_id).ok_or(Error::JobNotFound(job_id))?;

        // Cancel token (ensures all workers stop)
        job.cancellation_token.cancel();

        // Wait for pumps to finish (with timeout)
        let pump_timeout = Duration::from_secs(5);

        let stdout_handle = {
            let mut guard = job.stdout_pump.lock().unwrap();
            guard.take()
        };
        if let Some(handle) = stdout_handle {
            let _ = tokio::time::timeout(pump_timeout, handle).await;
        }

        let stderr_handle = {
            let mut guard = job.stderr_pump.lock().unwrap();
            guard.take()
        };
        if let Some(handle) = stderr_handle {
            let _ = tokio::time::timeout(pump_timeout, handle).await;
        }

        // Close process handle (Drop impl will handle this, but explicit is clearer)
        {
            let mut handle_guard = job.process_handle.lock().unwrap();
            if let Some(h) = handle_guard.take() {
                unsafe {
                    CloseHandle(h).ok();
                }
            }
        }

        // Close job object
        {
            let mut job_obj_guard = job.job_object.lock().unwrap();
            if let Some(h) = job_obj_guard.take() {
                unsafe {
                    CloseHandle(h).ok();
                }
            }
        }

        // Remove from registry
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

    pub async fn force_cleanup_all(&self) -> Result<()> {
        let job_ids: Vec<u32> = self.jobs.iter().map(|entry| *entry.key()).collect();
        for job_id in job_ids {
            let _ = self.cleanup_job(job_id).await;
        }
        Ok(())
    }

    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }
}
