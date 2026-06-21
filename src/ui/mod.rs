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

use iced::Subscription;
use iced::futures::Stream;
use std::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Bus name the GTK build owned via `gtk::Application("com.static.music")`.
/// Owning it again lets the daemon's `org.freedesktop.Application.Activate`
/// call (and the GNOME dock's activation) reach this window instead of always
/// failing and spawning a duplicate process.
const UI_APP_BUS_NAME: &str = "com.static.music";
/// Object path the daemon's `reopen_ui_window()` activates (must match
/// `UI_APP_OBJECT_PATH` in `daemon.rs`).
const UI_APP_OBJECT_PATH: &str = "/com/static/music";

/// Hands the raise-channel receiver from `run()` (which builds it) to the iced
/// subscription (which drains it once). The subscription builder is a bare
/// `fn`, so it cannot capture the receiver directly.
static RAISE_RX: Mutex<Option<UnboundedReceiver<()>>> = Mutex::new(None);

/// `org.freedesktop.Application` (freedesktop application spec). `gtk::Application`
/// served this for free pre-migration; the iced port serves it by hand so an
/// external "surface yourself" request can reach the running window.
struct UiApplication {
    raise_tx: UnboundedSender<()>,
}

#[zbus::interface(name = "org.freedesktop.Application")]
impl UiApplication {
    fn activate(
        &self,
        _platform_data: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) {
        let _ = self.raise_tx.send(());
    }

    fn open(
        &self,
        _uris: Vec<String>,
        _platform_data: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) {
        // No file/URI handling — treat like a plain activation.
        let _ = self.raise_tx.send(());
    }

    fn activate_action(
        &self,
        _action_name: String,
        _parameter: Vec<zbus::zvariant::OwnedValue>,
        _platform_data: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) {
        let _ = self.raise_tx.send(());
    }
}

/// Own `com.static.music` and serve `org.freedesktop.Application` for the life
/// of the process. Returns the receiver the subscription drains, or `None` if
/// the bus/name is unavailable (the UI still runs, just without remote raise).
fn start_ui_dbus_service() -> Option<UnboundedReceiver<()>> {
    let (raise_tx, raise_rx) = tokio::sync::mpsc::unbounded_channel();
    let built = zbus::blocking::connection::Builder::session()
        .and_then(|builder| builder.name(UI_APP_BUS_NAME))
        .and_then(|builder| builder.serve_at(UI_APP_OBJECT_PATH, UiApplication { raise_tx }))
        .and_then(|builder| builder.build());
    match built {
        Ok(connection) => {
            // Hold the name + object server open for the whole process; its
            // internal worker dispatches incoming Activate calls.
            std::mem::forget(connection);
            Some(raise_rx)
        }
        Err(err) => {
            crate::app_log!("[STATIC] UI activation D-Bus service unavailable: {err}");
            None
        }
    }
}

/// Subscription that turns remote `Activate` calls into [`Message::RaiseRequested`].
pub fn raise_subscription() -> Subscription<Message> {
    Subscription::run(raise_stream)
}

fn raise_stream() -> impl Stream<Item = Message> {
    let receiver = RAISE_RX.lock().expect("raise channel poisoned").take();
    iced::futures::stream::unfold(receiver, |mut receiver| async move {
        let signal = match receiver.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        };
        match signal {
            Some(()) => Some((Message::RaiseRequested, receiver)),
            // No channel (service failed) or it closed: park so the subscription
            // stays alive without spinning.
            None => {
                std::future::pending::<()>().await;
                None
            }
        }
    })
}

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

    // Restore the gtk::Application activation entry point: own com.static.music
    // and serve org.freedesktop.Application so the daemon's reopen path and the
    // GNOME dock raise this window instead of spawning a duplicate. Only the
    // instance that won the single-instance lock reaches here, so the name is
    // never contended between two of our own processes.
    *RAISE_RX.lock().expect("raise channel poisoned") = start_ui_dbus_service();

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
            // WM_CLASS (X11) / app_id (Wayland). On Linux, iced derives both
            // solely from this field; without it the compositor falls back to
            // the binary name (`static`), can't match StartupWMClass in the
            // installed com.static.music.desktop, and shows a generic icon.
            // Must equal the .desktop basename so the taskbar finds our icon.
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: "com.static.music".to_string(),
                ..Default::default()
            },
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
