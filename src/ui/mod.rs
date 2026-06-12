//! iced 0.14 GUI for Static (replaced the GTK4 UI).
//!
//! NOTE: modules inside this directory import siblings via `super::...`.

mod app;
mod content_loading;
mod icons;
mod player_bar;
mod queue_panel;
mod sidebar;
mod theme;
mod thumbnails;
mod views;
mod widgets;

#[allow(unused_imports)]
pub use app::{App, Message};

/// Launch the iced application. Mirrors the GTK shell: title "Static",
/// 1532x760 default window, dark theme, daemon-backed playback.
pub fn run() -> iced::Result {
    // GTK parity: gtk::Application("com.static.music") made the app
    // single-instance (a second launch presented the existing window). iced
    // has no instance broker, so claim a unix-socket lock instead; a second
    // launch exits cleanly rather than racing a second UI-client lease and a
    // second optimistic queue mirror against daemon snapshots.
    match acquire_single_instance_lock() {
        SingleInstance::AlreadyRunning => {
            crate::app_log!("[STATIC] another GUI instance is already running; exiting");
            eprintln!("static: the GUI is already running.");
            return Ok(());
        }
        SingleInstance::Acquired(listener) => {
            // Keep the socket bound for the whole process lifetime.
            std::mem::forget(listener);
        }
        SingleInstance::Unavailable => {
            // Lock could not be established (e.g. exotic runtime dir); run
            // unguarded rather than refusing to start.
        }
    }

    let icon =
        iced::window::icon::from_file_data(include_bytes!("../../assets/static.png"), None).ok();

    iced::application(App::boot, App::update, App::view)
        .title("Static")
        .theme(|_app: &App| theme::theme())
        .subscription(App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1532.0, 760.0),
            min_size: Some(iced::Size::new(1024.0, 600.0)),
            icon,
            // Close is intercepted so we can persist the resume snapshot and
            // release the daemon UI lease first (shell spec §9).
            exit_on_close_request: false,
            ..iced::window::Settings::default()
        })
        .run()
}

enum SingleInstance {
    Acquired(std::os::unix::net::UnixListener),
    AlreadyRunning,
    Unavailable,
}

/// Bind the instance-lock socket under `$XDG_RUNTIME_DIR` (fallback: temp
/// dir). `AddrInUse` + a successful connect means a live instance owns it; a
/// failed connect means a stale socket from an unclean shutdown — remove and
/// rebind.
fn acquire_single_instance_lock() -> SingleInstance {
    use std::os::unix::net::{UnixListener, UnixStream};

    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join("static-ui.sock");

    match UnixListener::bind(&path) {
        Ok(listener) => SingleInstance::Acquired(listener),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(&path).is_ok() {
                return SingleInstance::AlreadyRunning;
            }
            // Stale socket: previous instance died without unlinking it.
            let _ = std::fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(listener) => SingleInstance::Acquired(listener),
                Err(err) => {
                    crate::app_log!("[STATIC] failed to rebind instance lock: {err}");
                    SingleInstance::Unavailable
                }
            }
        }
        Err(err) => {
            crate::app_log!("[STATIC] failed to bind instance lock: {err}");
            SingleInstance::Unavailable
        }
    }
}
