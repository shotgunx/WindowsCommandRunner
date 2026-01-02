//! File-based IPC for remote command execution.
//! 
//! This module monitors a request directory for JSON request files and
//! writes responses to a response directory. This provides a simpler
//! IPC mechanism that works reliably over SMB.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::sleep;

const BASE_DIR: &str = r"C:\Windows\Temp\VirimaAgent";
const REQUEST_DIR: &str = r"C:\Windows\Temp\VirimaAgent\requests";
const RESPONSE_DIR: &str = r"C:\Windows\Temp\VirimaAgent\responses";
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_CONCURRENT_JOBS: usize = 10;

#[derive(Debug, Deserialize)]
struct Request {
    id: String,
    command: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    environment: Option<HashMap<String, String>>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    300000 // 5 minutes
}

#[derive(Debug, Serialize)]
struct Response {
    id: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub struct FileIpcServer {
    shutdown_rx: watch::Receiver<bool>,
}

impl FileIpcServer {
    pub fn new(shutdown_rx: watch::Receiver<bool>) -> Self {
        Self { shutdown_rx }
    }

    pub async fn run(&self) -> Result<()> {
        tracing::info!("Starting file-based IPC server");
        tracing::info!("Request directory: {}", REQUEST_DIR);
        tracing::info!("Response directory: {}", RESPONSE_DIR);

        // Ensure directories exist
        ensure_directories().await?;

        // Process loop
        let mut shutdown_rx = self.shutdown_rx.clone();
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_JOBS));

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    tracing::info!("File IPC server shutting down");
                    break;
                }
                _ = sleep(POLL_INTERVAL) => {
                    if let Err(e) = self.process_requests(&semaphore).await {
                        tracing::error!(error = %e, "Error processing requests");
                    }
                }
            }
        }

        Ok(())
    }

    async fn process_requests(&self, semaphore: &std::sync::Arc<tokio::sync::Semaphore>) -> Result<()> {
        let request_dir = Path::new(REQUEST_DIR);
        
        if !request_dir.exists() {
            return Ok(());
        }

        let mut entries = match fs::read_dir(request_dir).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::debug!("Cannot read request directory: {}", e);
                return Ok(());
            }
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            
            // Only process .json files
            if path.extension().map_or(false, |ext| ext == "json") {
                // Acquire semaphore permit
                let permit = match semaphore.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!("Max concurrent jobs reached, waiting...");
                        continue;
                    }
                };

                // Process in background
                let path_clone = path.clone();
                tokio::spawn(async move {
                    if let Err(e) = process_single_request(&path_clone).await {
                        tracing::error!(error = %e, path = %path_clone.display(), "Error processing request");
                    }
                    drop(permit);
                });
            }
        }

        Ok(())
    }
}

async fn ensure_directories() -> Result<()> {
    let base = Path::new(BASE_DIR);
    let request_dir = Path::new(REQUEST_DIR);
    let response_dir = Path::new(RESPONSE_DIR);

    if !base.exists() {
        fs::create_dir_all(base).await
            .map_err(Error::Io)?;
    }
    if !request_dir.exists() {
        fs::create_dir_all(request_dir).await
            .map_err(Error::Io)?;
    }
    if !response_dir.exists() {
        fs::create_dir_all(response_dir).await
            .map_err(Error::Io)?;
    }

    tracing::debug!("IPC directories ready");
    Ok(())
}

async fn process_single_request(request_path: &Path) -> Result<()> {
    let request_id = request_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    tracing::info!(request_id = %request_id, "Processing request");

    // Read request file
    let mut file = fs::File::open(request_path).await
        .map_err(Error::Io)?;
    let mut content = String::new();
    file.read_to_string(&mut content).await
        .map_err(Error::Io)?;
    drop(file);

    // Delete request file immediately to prevent re-processing
    if let Err(e) = fs::remove_file(request_path).await {
        tracing::warn!(error = %e, "Failed to delete request file");
    }

    // Parse request
    let request: Request = match serde_json::from_str(&content) {
        Ok(req) => req,
        Err(e) => {
            tracing::error!(error = %e, "Invalid request JSON");
            write_error_response(&request_id, &format!("Invalid JSON: {}", e)).await?;
            return Ok(());
        }
    };

    // Execute command
    let response = execute_command(&request).await;

    // Write response
    write_response(&response).await?;

    tracing::info!(
        request_id = %request_id,
        exit_code = response.exit_code,
        stdout_len = response.stdout.len(),
        stderr_len = response.stderr.len(),
        "Request completed"
    );

    Ok(())
}

async fn execute_command(request: &Request) -> Response {
    let start = std::time::Instant::now();
    let timeout = Duration::from_millis(request.timeout_ms);

    // Build command - use cmd.exe for shell command execution
    let mut cmd = Command::new("cmd.exe");
    cmd.args(["/C", &request.command]);
    
    // Set working directory
    if let Some(ref wd) = request.working_dir {
        cmd.current_dir(wd);
    }
    
    // Set environment variables
    if let Some(ref env) = request.environment {
        for (key, value) in env {
            cmd.env(key, value);
        }
    }
    
    // Capture output
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());
    
    // Spawn child
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Response {
                id: request.id.clone(),
                exit_code: -1,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("Failed to spawn process: {}", e)),
            };
        }
    };
    
    // Wait with timeout
    let output_result = tokio::time::timeout(timeout, child.wait_with_output()).await;
    
    let (exit_code, stdout, stderr, error) = match output_result {
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            (exit_code, stdout, stderr, None)
        }
        Ok(Err(e)) => {
            (-1, String::new(), String::new(), Some(format!("Process error: {}", e)))
        }
        Err(_) => {
            // Timeout
            tracing::warn!(request_id = %request.id, "Process timed out after {:?}", timeout);
            (-1, String::new(), String::new(), Some(format!("Command timed out after {:?}", timeout)))
        }
    };

    let elapsed = start.elapsed();
    tracing::debug!(
        request_id = %request.id,
        elapsed_ms = elapsed.as_millis(),
        "Command execution completed"
    );

    Response {
        id: request.id.clone(),
        exit_code,
        stdout,
        stderr,
        error,
    }
}

async fn write_response(response: &Response) -> Result<()> {
    let response_path = PathBuf::from(RESPONSE_DIR).join(format!("{}.json", response.id));
    
    let json = serde_json::to_string_pretty(response)
        .map_err(|e| Error::Protocol(format!("JSON serialize: {}", e)))?;
    
    let mut file = fs::File::create(&response_path).await
        .map_err(Error::Io)?;
    file.write_all(json.as_bytes()).await
        .map_err(Error::Io)?;
    file.flush().await
        .map_err(Error::Io)?;
    
    tracing::debug!(path = %response_path.display(), "Response written");
    Ok(())
}

async fn write_error_response(request_id: &str, error: &str) -> Result<()> {
    let response = Response {
        id: request_id.to_string(),
        exit_code: -1,
        stdout: String::new(),
        stderr: String::new(),
        error: Some(error.to_string()),
    };
    write_response(&response).await
}
