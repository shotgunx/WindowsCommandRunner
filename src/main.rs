#![cfg(windows)]

use virima_remote_agent::error::Result;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--service") {
        tracing::info!("Starting as Windows Service");
        virima_remote_agent::service_host::start_service()?;
    } else {
        tracing::info!("Starting in console mode (Ctrl+C to stop)");
        virima_remote_agent::service_host::run_console().await?;
    }

    Ok(())
}

fn init_logging() {
    use tracing_subscriber::fmt;

    fmt::init();
}
