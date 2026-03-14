use gtk::prelude::*;
use gtk::{Application, CssProvider, gdk};

use super::window;
use crate::ipc;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

const APP_ID: &str = "com.static.music";

pub fn build_app() -> Application {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        let _ = crate::logging::reset_session_log();
        let provider = CssProvider::new();
        provider.load_from_data(include_str!("style.css"));
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("Could not get default display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );

        if let Err(err) = ipc::ensure_daemon_running() {
            eprintln!("[STATIC] failed to ensure playback daemon: {err}");
        }
    });

    app.connect_activate(|app| {
        if let Some(existing) = app
            .active_window()
            .or_else(|| app.windows().into_iter().next())
        {
            existing.present();
        } else {
            window::build_window(app);
        }
    });

    app
}
