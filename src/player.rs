use crate::ipc::DaemonClient;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

pub fn notify_daemon_settings_changed() {
    match DaemonClient::new().and_then(|client| client.reload_settings()) {
        Ok(_) => {}
        Err(err) => eprintln!("[PLAYER] failed to notify staticd about settings changes: {err}"),
    }
}
