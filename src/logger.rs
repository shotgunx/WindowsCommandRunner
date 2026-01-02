use crate::error::Result;
use std::collections::HashMap;

pub struct Logger {
    #[allow(dead_code)]
    event_log_source: String,
    #[allow(dead_code)]
    file_log_path: Option<String>,
}

impl Logger {
    pub fn new(event_log_source: String, file_log_path: Option<String>) -> Self {
        Self {
            event_log_source,
            file_log_path,
        }
    }

    pub fn log_event(
        &self,
        level: LogLevel,
        message: &str,
        fields: Option<&HashMap<String, String>>,
    ) -> Result<()> {
        // Format log entry
        let mut log_entry = format!("[{}] {}", level, message);
        if let Some(fields) = fields {
            for (key, value) in fields {
                log_entry.push_str(&format!(" {}={}", key, value));
            }
        }

        // Log to tracing (which can be configured to write to Event Log or file)
        match level {
            LogLevel::Error => tracing::error!("{}", log_entry),
            LogLevel::Warn => tracing::warn!("{}", log_entry),
            LogLevel::Info => tracing::info!("{}", log_entry),
            LogLevel::Debug => tracing::debug!("{}", log_entry),
            LogLevel::Trace => tracing::trace!("{}", log_entry),
        }

        Ok(())
    }

    pub fn audit_command_started(
        &self,
        job_id: u32,
        client_user: &str,
        command_line: &str,
        working_dir: Option<&str>,
    ) -> Result<()> {
        let mut fields = HashMap::new();
        fields.insert("job_id".to_string(), job_id.to_string());
        fields.insert("client_user".to_string(), client_user.to_string());
        fields.insert("command_line".to_string(), command_line.to_string());
        if let Some(cwd) = working_dir {
            fields.insert("working_directory".to_string(), cwd.to_string());
        }

        self.log_event(LogLevel::Info, "COMMAND_STARTED", Some(&fields))
    }

    pub fn audit_command_completed(
        &self,
        job_id: u32,
        exit_code: i32,
        duration_ms: u64,
    ) -> Result<()> {
        let mut fields = HashMap::new();
        fields.insert("job_id".to_string(), job_id.to_string());
        fields.insert("exit_code".to_string(), exit_code.to_string());
        fields.insert("duration_ms".to_string(), duration_ms.to_string());

        self.log_event(LogLevel::Info, "COMMAND_COMPLETED", Some(&fields))
    }

    pub fn audit_command_failed(
        &self,
        job_id: u32,
        error_code: u32,
        error_message: &str,
    ) -> Result<()> {
        let mut fields = HashMap::new();
        fields.insert("job_id".to_string(), job_id.to_string());
        fields.insert("error_code".to_string(), error_code.to_string());
        fields.insert("error_message".to_string(), error_message.to_string());

        self.log_event(LogLevel::Error, "COMMAND_FAILED", Some(&fields))
    }

    pub fn audit_command_canceled(&self, job_id: u32, reason: &str) -> Result<()> {
        let mut fields = HashMap::new();
        fields.insert("job_id".to_string(), job_id.to_string());
        fields.insert("reason".to_string(), reason.to_string());

        self.log_event(LogLevel::Warn, "COMMAND_CANCELED", Some(&fields))
    }

    pub fn audit_access_denied(&self, client_user: &str, attempted_command: &str) -> Result<()> {
        let mut fields = HashMap::new();
        fields.insert("client_user".to_string(), client_user.to_string());
        fields.insert(
            "attempted_command".to_string(),
            attempted_command.to_string(),
        );

        self.log_event(LogLevel::Warn, "ACCESS_DENIED", Some(&fields))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Error => write!(f, "ERROR"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Trace => write!(f, "TRACE"),
        }
    }
}
