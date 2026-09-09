//! Local visualizer client. The daemon remains the sole playback owner.
mod cartridge;
mod cartridge_codec;
mod server;
mod theme;
mod window;

use anyhow::{Context, Result};

pub fn launch() -> Result<()> {
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .arg("visualizer")
        .spawn()
        .context("Could not open the visualizer window")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn run() -> Result<()> {
    let client = crate::ipc::DaemonClient::new()?;
    client.visualization_frame(30).context(
        "The running daemon does not support this visualizer. Install the current build first",
    )?;
    let id = format!("visualizer-{}", uuid::Uuid::new_v4());
    client.register_ui_client(&id)?;
    let result = (|| {
        let server = server::start()?;
        window::run(&server.url, &server.certificate_pem)
    })();
    let _ = client.unregister_ui_client(&id);
    result
}

/// Decode without starting the daemon, accessing audio, or changing playback.
pub fn inspect_cartridge(path: &str) -> Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(cartridge_codec::MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&cartridge_codec::decode_bytes(&bytes)?)?
    );
    Ok(())
}

/// Dedicated compact window; observes and tunes the daemon's existing processor.
pub fn calibrate() -> Result<()> {
    let client = crate::ipc::DaemonClient::new()?;
    client.gain_diagnostics().context("The running daemon does not support dGain calibration. Install the current build and restart Static first")?;
    let id = format!("gain-calibration-{}", uuid::Uuid::new_v4());
    client.register_ui_client(&id)?;
    let result = (|| {
        let mut server = server::start()?;
        server.url = server.url.replacen("/#", "/calibrate#", 1);
        window::run(&server.url, &server.certificate_pem)
    })();
    let _ = client.unregister_ui_client(&id);
    result
}
