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
