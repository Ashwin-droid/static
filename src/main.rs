mod cartridges;
mod cli;
mod cli_tui;
mod daemon;
mod desktop_volume;
mod ipc;
mod logging;
mod models;
mod network;
mod network_parser;
mod network_state;
mod offline;
mod pipewire_monitor;
mod pipewire_tap;
mod playback_engine;
mod player;
mod session_lock;
#[cfg(feature = "staticfs")]
mod staticfs;
mod ui;
mod visualizer;

/// Default to INFO but silence `iced_winit`, which logs the full window-icon
/// RGBA buffer at INFO on every launch (tens of MB of stderr). `RUST_LOG`, when
/// set, overrides this entirely.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,iced_winit=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();

    if args.get(1).is_some_and(|arg| arg == "visualizer") {
        init_tracing();
        let result = match args.get(2).map(String::as_str) {
            None => visualizer::run(),
            Some("decode") if args.len() == 4 => visualizer::inspect_cartridge(&args[3]),
            _ => Err(anyhow::anyhow!(
                "Usage: static visualizer [decode <cartridge.png|jpg|webp>]"
            )),
        };
        if let Err(err) = result {
            eprintln!("Visualizer: {err:#}");
            std::process::exit(1);
        }
        return;
    }

    if args.get(1).is_some_and(|arg| arg == "--daemon") {
        init_tracing();
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
        init_tracing();
        gstreamer::init().expect("Failed to initialise GStreamer");
        ui::run().expect("Failed to run UI");
    }
}
