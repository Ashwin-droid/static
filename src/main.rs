mod cache;
mod daemon;
mod ipc;
mod logging;
mod models;
mod playback_engine;
mod player;
mod sponsorblock;
mod ui;
mod ytmusic;

use gtk::prelude::*;

fn main() {
    tracing_subscriber::fmt::init();
    gstreamer::init().expect("Failed to initialise GStreamer");

    if std::env::args().any(|arg| arg == "--daemon") {
        daemon::run().expect("Failed to start playback daemon");
    } else {
        let app = ui::build_app();
        app.run();
    }
}
