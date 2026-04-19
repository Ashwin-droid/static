mod cli;
mod cli_tui;
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
    let args = std::env::args().collect::<Vec<_>>();

    if args.get(1).is_some_and(|arg| arg == "--daemon") {
        tracing_subscriber::fmt::init();
        gstreamer::init().expect("Failed to initialise GStreamer");
        daemon::run().expect("Failed to start playback daemon");
        return;
    }

    if cli::wants_cli(&args[1..]) {
        if let Err(err) = cli::run(&args[1..]) {
            eprintln!("{err}");
            std::process::exit(1);
        }
        return;
    } else {
        tracing_subscriber::fmt::init();
        gstreamer::init().expect("Failed to initialise GStreamer");
        let app = ui::build_app();
        app.run();
    }
}
