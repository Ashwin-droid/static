//! Local visualizer client. The daemon remains the sole playback owner.
mod cartridge;
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
