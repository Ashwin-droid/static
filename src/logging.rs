use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use keyring::Entry;
use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "com.static.music";
const KEYRING_STORAGE_ROOT: &str = "storage_root";
const LOG_FILE_NAME: &str = "static.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogSource {
    Daemon,
    Ui,
}

impl LogSource {
    fn label(self) -> &'static str {
        match self {
            Self::Daemon => "DAEMON",
            Self::Ui => "UI",
        }
    }
}

#[macro_export]
macro_rules! app_log {
    ($($arg:tt)*) => {{
        $crate::logging::log_current_process_message(format!($($arg)*));
    }};
}

fn default_static_root() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("Music/Static")
}

fn configured_storage_root() -> Option<String> {
    let entry = Entry::new(KEYRING_SERVICE, KEYRING_STORAGE_ROOT).ok()?;
    entry.get_password().ok()
}

fn resolve_storage_root(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return default_static_root();
    }

    if trimmed == "~" {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
    }

    if let Some(rest) = trimmed.strip_prefix("~/") {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join(rest);
    }

    PathBuf::from(trimmed)
}

pub fn static_root() -> PathBuf {
    configured_storage_root()
        .map(|raw| resolve_storage_root(&raw))
        .unwrap_or_else(default_static_root)
}

pub fn session_log_path() -> PathBuf {
    static_root().join(LOG_FILE_NAME)
}

pub fn reset_session_log() -> io::Result<PathBuf> {
    let root = static_root();
    std::fs::create_dir_all(&root)?;
    let path = root.join(LOG_FILE_NAME);
    File::create(&path)?;
    Ok(path)
}

pub fn append_log(source: LogSource, message: &str) -> io::Result<()> {
    let path = session_log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let trimmed = message.trim_end_matches('\n');

    if trimmed.is_empty() {
        writeln!(file, "[{}]:", source.label())?;
        return Ok(());
    }

    for line in trimmed.lines() {
        writeln!(file, "[{}]: {}", source.label(), line)?;
    }

    Ok(())
}

pub fn current_process_source() -> LogSource {
    if std::env::args_os().any(|arg| arg == "--daemon") {
        LogSource::Daemon
    } else {
        LogSource::Ui
    }
}

pub fn log_current_process_message(message: String) {
    match current_process_source() {
        LogSource::Daemon => {
            if let Err(err) = append_log(LogSource::Daemon, &message) {
                let _ = writeln!(std::io::stderr().lock(), "[LOGGER] failed to write daemon log: {err}");
            }
        }
        LogSource::Ui => {
            if let Err(err) = crate::ipc::forward_log_message(LogSource::Ui, message.clone()) {
                let _ = append_log(LogSource::Ui, &message);
                let _ = append_log(
                    LogSource::Ui,
                    &format!("[LOGGER] failed to forward UI log to daemon: {err}"),
                );
            }
        }
    }
}
