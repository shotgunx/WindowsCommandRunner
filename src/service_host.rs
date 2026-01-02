use crate::error::{Error, Result};
use crate::job_manager::JobManager;
use crate::logger::Logger;
use crate::pipe_server::PipeServer;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

/// Configuration for the service
pub struct ServiceConfig {
    pub pipe_name: String,
    pub max_concurrent_clients: usize,
    pub max_concurrent_jobs: usize,
    pub graceful_shutdown_timeout: Duration,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            pipe_name: r"\\.\pipe\VirimaRemoteAgent".to_string(),
            max_concurrent_clients: 64,
            max_concurrent_jobs: 32,
            graceful_shutdown_timeout: Duration::from_secs(30),
        }
    }
}

/// Main service host
pub struct ServiceHost {
    config: ServiceConfig,
}

impl ServiceHost {
    pub fn new(config: ServiceConfig) -> Self {
        Self { config }
    }

    pub fn with_defaults() -> Self {
        Self::new(ServiceConfig::default())
    }

    /// Run the service (main entry point)
    pub async fn run(&self) -> Result<()> {
        tracing::info!(
            pipe_name = %self.config.pipe_name,
            max_clients = self.config.max_concurrent_clients,
            max_jobs = self.config.max_concurrent_jobs,
            "Starting Virima Remote Agent Service"
        );

        // Initialize components
        let job_manager = Arc::new(JobManager::new(self.config.max_concurrent_jobs));
        let logger = Arc::new(Logger::new(
            "VirimaRemoteAgent".to_string(),
            Some("C:\\ProgramData\\Virima\\RemoteAgent\\service.log".to_string()),
        ));

        // Create pipe server
        let pipe_server = Arc::new(PipeServer::new(
            self.config.pipe_name.clone(),
            job_manager.clone(),
            logger.clone(),
            self.config.max_concurrent_clients,
        ));

        // Shutdown signal channel
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        // Start pipe server in background task
        let server_handle = {
            let pipe_server = pipe_server.clone();
            tokio::spawn(async move {
                if let Err(e) = pipe_server.start_listening().await {
                    tracing::error!(error = %e, "Pipe server error");
                }
            })
        };

        // Set up signal handlers
        Self::setup_shutdown_signal(shutdown_tx);

        // Wait for shutdown signal
        tracing::info!("Service running, waiting for shutdown signal...");
        let _ = shutdown_rx.await;

        // Graceful shutdown
        tracing::info!("Shutdown signal received, initiating graceful shutdown");
        self.graceful_shutdown(job_manager, self.config.graceful_shutdown_timeout).await?;

        // Cancel server task
        server_handle.abort();

        tracing::info!("Service stopped");
        Ok(())
    }

    fn setup_shutdown_signal(shutdown_tx: oneshot::Sender<()>) {
        // Handle Ctrl+C (for console mode)
        tokio::spawn(async move {
            let ctrl_c = async {
                tokio::signal::ctrl_c()
                    .await
                    .expect("Failed to install Ctrl+C handler");
            };

            #[cfg(windows)]
            let terminate = async {
                // Windows doesn't have SIGTERM, so we just wait on ctrl_c
                std::future::pending::<()>().await
            };

            #[cfg(not(windows))]
            let terminate = async {
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("Failed to install signal handler")
                    .recv()
                    .await;
            };

            tokio::select! {
                _ = ctrl_c => {
                    tracing::info!("Received Ctrl+C");
                }
                _ = terminate => {
                    tracing::info!("Received terminate signal");
                }
            }

            let _ = shutdown_tx.send(());
        });
    }

    async fn graceful_shutdown(
        &self,
        job_manager: Arc<JobManager>,
        timeout: Duration,
    ) -> Result<()> {
        let active_jobs = job_manager.list_active_jobs();
        
        if active_jobs.is_empty() {
            tracing::info!("No active jobs to cancel");
            return Ok(());
        }

        tracing::info!(count = active_jobs.len(), "Canceling active jobs");

        // Cancel all active jobs
        for job_id in &active_jobs {
            if let Err(e) = job_manager.cancel_job(*job_id, "Service stopping") {
                tracing::warn!(job_id, error = %e, "Failed to cancel job");
            }
        }

        // Wait for jobs to complete (with timeout)
        let deadline = tokio::time::Instant::now() + timeout;
        let poll_interval = Duration::from_millis(500);

        while job_manager.has_active_jobs() && tokio::time::Instant::now() < deadline {
            let remaining = job_manager.job_count();
            tracing::debug!(remaining, "Waiting for jobs to finish...");
            tokio::time::sleep(poll_interval).await;
        }

        // Force cleanup remaining jobs
        if job_manager.has_active_jobs() {
            let remaining = job_manager.list_active_jobs();
            tracing::warn!(count = remaining.len(), "Force cleaning up remaining jobs");
            job_manager.force_cleanup_all().await?;
        }

        Ok(())
    }
}

// Windows Service support (for running as actual Windows Service)
#[cfg(windows)]
pub mod windows_service {
    use super::*;
    use std::ffi::OsString;
    use std::sync::mpsc;
    use std::time::Duration;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    const SERVICE_NAME: &str = "VirimaRemoteAgent";
    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

    // Generate Windows service boilerplate
    define_windows_service!(ffi_service_main, my_service_main);

    pub fn run_as_service() -> windows_service::Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
        Ok(())
    }

    fn my_service_main(arguments: Vec<OsString>) {
        if let Err(e) = run_service(arguments) {
            tracing::error!(error = %e, "Service failed");
        }
    }

    fn run_service(_arguments: Vec<OsString>) -> Result<()> {
        // Create channel for service control events
        let (shutdown_tx, shutdown_rx) = mpsc::channel();

        // Define service control handler
        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    tracing::info!("Service stop requested");
                    let _ = shutdown_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        // Register service control handler
        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .map_err(|e| Error::Service(format!("Failed to register service handler: {}", e)))?;

        // Report service starting
        status_handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: ServiceState::StartPending,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::from_secs(10),
                process_id: None,
            })
            .map_err(|e| Error::Service(format!("Failed to set service status: {}", e)))?;

        // Create tokio runtime
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Service(format!("Failed to create runtime: {}", e)))?;

        // Initialize service
        let service_host = ServiceHost::with_defaults();
        let job_manager = Arc::new(JobManager::new(service_host.config.max_concurrent_jobs));
        let logger = Arc::new(Logger::new(
            "VirimaRemoteAgent".to_string(),
            Some("C:\\ProgramData\\Virima\\RemoteAgent\\service.log".to_string()),
        ));
        let pipe_server = Arc::new(PipeServer::new(
            service_host.config.pipe_name.clone(),
            job_manager.clone(),
            logger.clone(),
            service_host.config.max_concurrent_clients,
        ));

        // Report service running
        status_handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: ServiceState::Running,
                controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::from_secs(0),
                process_id: None,
            })
            .map_err(|e| Error::Service(format!("Failed to set service status: {}", e)))?;

        // Start pipe server
        let pipe_server_clone = pipe_server.clone();
        let server_handle = runtime.spawn(async move {
            if let Err(e) = pipe_server_clone.start_listening().await {
                tracing::error!(error = %e, "Pipe server error");
            }
        });

        // Wait for stop signal
        let _ = shutdown_rx.recv();

        // Report service stopping
        status_handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: ServiceState::StopPending,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::from_secs(30),
                process_id: None,
            })
            .ok();

        // Graceful shutdown
        runtime.block_on(async {
            service_host
                .graceful_shutdown(job_manager, service_host.config.graceful_shutdown_timeout)
                .await
                .ok();
        });

        // Stop server
        server_handle.abort();

        // Report service stopped
        status_handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: ServiceState::Stopped,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(0),
                checkpoint: 0,
                wait_hint: Duration::from_secs(0),
                process_id: None,
            })
            .ok();

        Ok(())
    }
}
