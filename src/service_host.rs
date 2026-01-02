use crate::error::{Error, Result};
use crate::file_ipc::FileIpcServer;
use crate::job_manager::JobManager;
use crate::pipe_server::PipeServer;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const SERVICE_NAME: &str = "VirimaRemoteAgent";
const MAX_CONCURRENT_JOBS: usize = 50;
const MAX_CONNECTIONS: usize = 100;
const IDLE_CLEANUP_INTERVAL: Duration = Duration::from_secs(30); // Check every 30 seconds
const IDLE_CLEANUP_THRESHOLD: Duration = Duration::from_secs(30 * 60); // Clean up after 30 minutes idle
const QUIET_CLEANUP_THRESHOLD: Duration = Duration::from_secs(2 * 60); // Clean up all if quiet for 2 minutes

pub struct ServiceHost {
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl ServiceHost {
    pub fn new() -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            shutdown_tx,
            shutdown_rx,
        }
    }

    pub async fn run(&self) -> Result<()> {
        tracing::info!("Starting {} service", SERVICE_NAME);

        let job_manager = Arc::new(JobManager::new(MAX_CONCURRENT_JOBS));
        let pipe_server = Arc::new(PipeServer::new(job_manager.clone(), MAX_CONNECTIONS));

        // Spawn pipe server
        let server_handle = {
            let server = pipe_server.clone();
            tokio::spawn(async move {
                if let Err(e) = server.start_listening().await {
                    tracing::error!(error = %e, "Pipe server error");
                }
            })
        };

        // Spawn file-based IPC server
        let file_ipc_handle = {
            let shutdown_rx = self.shutdown_rx.clone();
            let file_ipc = FileIpcServer::new(shutdown_rx);
            tokio::spawn(async move {
                if let Err(e) = file_ipc.run().await {
                    tracing::error!(error = %e, "File IPC server error");
                }
            })
        };

        // Spawn idle job cleanup task
        let cleanup_handle = {
            let job_manager = job_manager.clone();
            let mut shutdown_rx = self.shutdown_rx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(IDLE_CLEANUP_INTERVAL);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            job_manager.cleanup_idle_jobs(IDLE_CLEANUP_THRESHOLD, QUIET_CLEANUP_THRESHOLD);
                        }
                        _ = shutdown_rx.changed() => {
                            tracing::debug!("Cleanup task shutting down");
                            break;
                        }
                    }
                }
            })
        };

        // Wait for shutdown signal
        let mut shutdown_rx = self.shutdown_rx.clone();
        tokio::select! {
            _ = shutdown_rx.changed() => {
                tracing::info!("Shutdown signal received");
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Ctrl+C received");
            }
        }

        // Graceful shutdown
        tracing::info!("Initiating graceful shutdown");
        job_manager.force_cleanup_all()?;

        // Wait briefly for pending operations
        tokio::time::sleep(Duration::from_secs(2)).await;

        cleanup_handle.abort();
        server_handle.abort();
        file_ipc_handle.abort();
        tracing::info!("Service stopped");
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl Default for ServiceHost {
    fn default() -> Self {
        Self::new()
    }
}

// Windows service entry points
#[cfg(windows)]
mod service_impl {
    use super::*;
    use ::windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };
    use std::ffi::OsString;
    use std::sync::OnceLock;

    static SERVICE_HOST: OnceLock<ServiceHost> = OnceLock::new();

    define_windows_service!(ffi_service_main, my_service_main);

    fn my_service_main(_arguments: Vec<OsString>) {
        if let Err(e) = run_service() {
            tracing::error!(error = %e, "Service main error");
        }
    }

    fn run_service() -> Result<()> {
        let host = SERVICE_HOST.get_or_init(ServiceHost::new);

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    if let Some(h) = SERVICE_HOST.get() {
                        h.stop();
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .map_err(|e| Error::Service(format!("Register handler: {}", e)))?;

        // Report running
        status_handle
            .set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Running,
                controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })
            .map_err(|e| Error::Service(format!("Set status: {}", e)))?;

        // Run the service
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Service(format!("Runtime: {}", e)))?;
        let result = runtime.block_on(host.run());

        // Report stopped
        let exit_code = if result.is_ok() {
            ServiceExitCode::Win32(0)
        } else {
            ServiceExitCode::Win32(1)
        };

        status_handle
            .set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Stopped,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code,
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })
            .ok();

        result
    }

    pub fn start_service() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .map_err(|e| Error::Service(format!("Start dispatcher: {}", e)))
    }
}

#[cfg(windows)]
pub use service_impl::start_service;

pub async fn run_console() -> Result<()> {
    let host = ServiceHost::new();
    host.run().await
}
