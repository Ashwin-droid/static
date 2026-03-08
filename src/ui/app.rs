use gtk::prelude::*;
use gtk::{gdk, Application, CssProvider};

use super::window;

const APP_ID: &str = "com.static.music";

pub fn build_app() -> Application {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        let provider = CssProvider::new();
        provider.load_from_data(include_str!("style.css"));
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("Could not get default display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );
    });

    app.connect_activate(|app| {
        window::build_window(app);
    });

    app
}
