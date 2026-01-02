use std::env;
use virima_remote_agent::error::Result;
use virima_remote_agent::service_host::{ServiceConfig, ServiceHost};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    init_logging();

    // Check if running as Windows Service
    let args: Vec<String> = env::args().collect();
    
    if args.iter().any(|a| a == "--service") {
        // Run as Windows Service
        #[cfg(windows)]
        {
            tracing::info!("Starting as Windows Service");
            virima_remote_agent::service_host::windows_service::run_as_service()
                .map_err(|e| virima_remote_agent::error::Error::Service(format!("{}", e)))?;
        }
        #[cfg(not(windows))]
        {
            eprintln!("Windows Service mode only available on Windows");
            std::process::exit(1);
        }
    } else {
        // Run in console mode
        tracing::info!("Starting in console mode (Ctrl+C to stop)");
        
        let config = ServiceConfig {
            pipe_name: env::var("VIRIMA_PIPE_NAME")
                .unwrap_or_else(|_| r"\\.\pipe\VirimaRemoteAgent".to_string()),
            max_concurrent_clients: env::var("VIRIMA_MAX_CLIENTS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(64),
            max_concurrent_jobs: env::var("VIRIMA_MAX_JOBS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(32),
            graceful_shutdown_timeout: std::time::Duration::from_secs(
                env::var("VIRIMA_SHUTDOWN_TIMEOUT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(30),
            ),
        };

        let service = ServiceHost::new(config);
        service.run().await?;
    }

    Ok(())
}

fn init_logging() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(fmt::layer().with_target(true).with_thread_ids(true))
        .with(filter)
        .init();
}
