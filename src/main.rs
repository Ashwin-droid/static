mod models;
mod player;
mod ui;
mod ytmusic;

use gtk::prelude::*;

fn main() {
    tracing_subscriber::fmt::init();

    // GTK owns the main loop. Async network I/O runs via glib::spawn_future_local
    // on the glib main context. reqwest handles its own tokio runtime internally.
    let app = ui::build_app();
    app.run();
}
