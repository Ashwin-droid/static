mod models;
mod player;
mod ui;
mod ytmusic;

use gtk::prelude::*;

fn main() {
    tracing_subscriber::fmt::init();
    gstreamer::init().expect("Failed to initialise GStreamer");

    let app = ui::build_app();
    app.run();
}
