use std::process::{Child, Command};
use std::sync::Mutex;

use anyhow::{Context, Result};

pub struct Player {
    process: Mutex<Option<Child>>,
}

impl Player {
    pub fn new() -> Self {
        Self {
            process: Mutex::new(None),
        }
    }

    pub fn play(&self, url: &str) -> Result<()> {
        self.stop();

        let child = Command::new("ffplay")
            .args([
                "-nodisp",     // no video window
                "-autoexit",   // exit when done
                "-loglevel", "error",
                url,
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("Failed to launch ffplay — is ffmpeg installed?")?;

        *self.process.lock().unwrap() = Some(child);
        Ok(())
    }

    pub fn stop(&self) {
        if let Some(mut child) = self.process.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    #[allow(dead_code)]
    pub fn is_playing(&self) -> bool {
        let mut guard = self.process.lock().unwrap();
        if let Some(child) = guard.as_mut() {
            // Check if still running
            match child.try_wait() {
                Ok(None) => true,       // still running
                _ => {
                    *guard = None;
                    false
                }
            }
        } else {
            false
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}
