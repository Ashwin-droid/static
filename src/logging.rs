use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local};
use keyring::Entry;
use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "com.static.music";
const KEYRING_STORAGE_ROOT: &str = "storage_root";
const LOG_FILE_NAME: &str = "static.log";
const LOG_DAY_STAMP_FILE_NAME: &str = ".static.log.day";

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

fn log_day_stamp_path() -> PathBuf {
    static_root().join(LOG_DAY_STAMP_FILE_NAME)
}

fn day_key_for_time(time: SystemTime) -> String {
    DateTime::<Local>::from(time).format("%Y-%m-%d").to_string()
}

fn current_day_key() -> String {
    day_key_for_time(SystemTime::now())
}

fn read_day_key(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let trimmed = contents.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn fallback_log_day_key(path: &Path) -> io::Result<Option<String>> {
    match fs::metadata(path) {
        Ok(metadata) => match metadata.modified() {
            Ok(modified) => Ok(Some(day_key_for_time(modified))),
            Err(_) => Ok(None),
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn ensure_daily_log_freshness_for_paths(
    log_path: &Path,
    day_stamp_path: &Path,
    today_day_key: &str,
) -> io::Result<()> {
    let last_day_key = match read_day_key(day_stamp_path)? {
        Some(day_key) => Some(day_key),
        None => fallback_log_day_key(log_path)?,
    };

    if last_day_key.as_deref() != Some(today_day_key) {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(log_path)?;
    }

    fs::write(day_stamp_path, format!("{today_day_key}\n"))?;
    Ok(())
}

fn ensure_daily_log_freshness(log_path: &Path) -> io::Result<()> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    ensure_daily_log_freshness_for_paths(log_path, &log_day_stamp_path(), &current_day_key())
}

pub fn reset_session_log() -> io::Result<PathBuf> {
    let path = session_log_path();
    ensure_daily_log_freshness(&path)?;
    let existing_len = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    if existing_len > 0 {
        file.write_all(b"\n")?;
    }
    file.write_all(b"[UI]: [LOGGER] starting new UI session\n")?;
    Ok(path)
}

pub fn append_log(source: LogSource, message: &str) -> io::Result<()> {
    let path = session_log_path();
    ensure_daily_log_freshness(&path)?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let trimmed = message.trim_end_matches('\n');
    let mut payload = String::new();

    if trimmed.is_empty() {
        payload.push_str(&format!("[{}]:\n", source.label()));
        file.write_all(payload.as_bytes())?;
        return Ok(());
    }

    for line in trimmed.lines() {
        payload.push_str(&format!("[{}]: {line}\n", source.label()));
    }

    file.write_all(payload.as_bytes())?;
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
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[LOGGER] failed to write daemon log: {err}"
                );
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

#[cfg(test)]
mod tests {
    use super::ensure_daily_log_freshness_for_paths;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("static-logging-test-{unique}"))
    }

    #[test]
    fn lazy_daily_wipe_truncates_log_when_day_stamp_is_stale() {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir).expect("failed to create temp dir");
        let log_path = dir.join("static.log");
        let day_stamp_path = dir.join(".static.log.day");

        fs::write(&log_path, b"old log\n").expect("failed to seed log");
        fs::write(&day_stamp_path, b"2026-03-23\n").expect("failed to seed day stamp");

        ensure_daily_log_freshness_for_paths(&log_path, &day_stamp_path, "2026-03-24")
            .expect("failed to refresh log");

        assert_eq!(fs::read(&log_path).expect("failed to read log"), b"");
        assert_eq!(
            fs::read_to_string(&day_stamp_path).expect("failed to read day stamp"),
            "2026-03-24\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lazy_daily_wipe_preserves_log_when_day_stamp_matches_today() {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir).expect("failed to create temp dir");
        let log_path = dir.join("static.log");
        let day_stamp_path = dir.join(".static.log.day");

        fs::write(&log_path, b"keep me\n").expect("failed to seed log");
        fs::write(&day_stamp_path, b"2026-03-24\n").expect("failed to seed day stamp");

        ensure_daily_log_freshness_for_paths(&log_path, &day_stamp_path, "2026-03-24")
            .expect("failed to refresh log");

        assert_eq!(
            fs::read_to_string(&log_path).expect("failed to read log"),
            "keep me\n"
        );
        assert_eq!(
            fs::read_to_string(&day_stamp_path).expect("failed to read day stamp"),
            "2026-03-24\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
