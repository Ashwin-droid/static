mod daemon;
mod ipc;
mod logging;
mod models;
mod network;
mod network_parser;
mod network_state;
mod offline;
mod pipewire_monitor;
mod playback_engine;
mod player;
mod ui;

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
