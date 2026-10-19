//! Dynamic PipeWire ducking: the duck follows how loud the foreign audio is.
//!
//! Each coordinated foreign stream is tapped with a `pw-record` child
//! (`--target <node> stream.capture.sink=true`, which captures exactly that
//! playback stream's output). A reader thread per child keeps a mean-square
//! level over the latest 50 ms block. The daemon combines the streams'
//! energies every tick, smooths the level with a fast attack and a slow
//! release, and maps it through a fixed CSS-style ease curve to a duck amount
//! between the user's minimum and maximum duck. There is no speech model:
//! the mapping is a fixed level window chosen so that meeting voices reach
//! the maximum duck and gaps between sentences fall back to the minimum.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Foreign audio at or below this short-term level is "quiet": minimum duck.
pub const QUIET_DBFS: f64 = -50.0;
/// Foreign audio at or above this short-term level is "loud": maximum duck.
pub const LOUD_DBFS: f64 = -20.0;
/// Level smoothing: react to a voice starting within a syllable...
const ATTACK: Duration = Duration::from_millis(80);
/// ...but let a pause between sentences pass before the music comes back up.
const RELEASE: Duration = Duration::from_millis(700);
/// A tap that has not delivered a block for this long is treated as silent.
const STALE_AFTER: Duration = Duration::from_millis(400);
const TAP_RATE: u32 = 8000;
const BLOCK_SAMPLES: usize = 400; // 50 ms at 8 kHz mono
const SILENCE_DBFS: f64 = -100.0;

/// CSS `ease-in-out` (cubic): slow out of the quiet end, slow into the loud
/// end, so faint background noise barely moves the music and a voice that is
/// already clearly there does not keep pumping it.
pub fn ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Duck amount (0 = untouched, 1 = silent) for a foreign level in dBFS.
pub fn duck_amount_for_level(level_dbfs: f64, min_duck: f64, max_duck: f64) -> f64 {
    let (min_duck, max_duck) = ordered_ducks(min_duck, max_duck);
    let t = (level_dbfs - QUIET_DBFS) / (LOUD_DBFS - QUIET_DBFS);
    min_duck + (max_duck - min_duck) * ease_in_out(t)
}

pub fn ordered_ducks(min_duck: f64, max_duck: f64) -> (f64, f64) {
    let min_duck = min_duck.clamp(0.0, 1.0);
    let max_duck = max_duck.clamp(0.0, 1.0);
    if min_duck <= max_duck {
        (min_duck, max_duck)
    } else {
        (max_duck, min_duck)
    }
}

/// Smoothed foreign level with asymmetric attack/release, in dBFS.
#[derive(Debug, Clone, Copy)]
pub struct LevelFollower {
    level_dbfs: f64,
    last_update: Option<Instant>,
}

impl Default for LevelFollower {
    fn default() -> Self {
        Self {
            level_dbfs: SILENCE_DBFS,
            last_update: None,
        }
    }
}

impl LevelFollower {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn level_dbfs(&self) -> f64 {
        self.level_dbfs
    }

    /// Feed the instantaneous combined level measured at `now`.
    pub fn update(&mut self, instant_dbfs: f64, now: Instant) -> f64 {
        let instant_dbfs = instant_dbfs.max(SILENCE_DBFS);
        let Some(last) = self.last_update else {
            self.level_dbfs = instant_dbfs;
            self.last_update = Some(now);
            return self.level_dbfs;
        };
        let dt = now.saturating_duration_since(last).as_secs_f64();
        self.last_update = Some(now);
        let tau = if instant_dbfs > self.level_dbfs {
            ATTACK.as_secs_f64()
        } else {
            RELEASE.as_secs_f64()
        };
        let coefficient = if tau <= 0.0 {
            1.0
        } else {
            1.0 - (-dt / tau).exp()
        };
        self.level_dbfs += (instant_dbfs - self.level_dbfs) * coefficient;
        self.level_dbfs
    }
}

#[derive(Debug, Default)]
struct BlockLevel {
    mean_square: f64,
    updated_at: Option<Instant>,
    /// The child exited or never produced audio: tap is dead.
    failed: bool,
}

struct Tap {
    child: Child,
    level: Arc<Mutex<BlockLevel>>,
}

impl Drop for Tap {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Owns one pw-record tap per coordinated foreign stream.
#[derive(Default)]
pub struct ForeignLevelTaps {
    taps: HashMap<u32, Tap>,
}

impl ForeignLevelTaps {
    /// Make the tap set match `streams` (node id, object serial), spawning
    /// and reaping as needed. pw-record targets by serial, not node id.
    pub fn sync(&mut self, streams: &[(u32, u32)]) {
        self.taps
            .retain(|node_id, _| streams.iter().any(|(node, _)| node == node_id));
        for &(node_id, serial) in streams {
            if let std::collections::hash_map::Entry::Vacant(slot) = self.taps.entry(node_id) {
                if let Some(tap) = spawn_tap(node_id, serial) {
                    slot.insert(tap);
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.taps.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    /// Combined instantaneous level of every live tap, in dBFS. `None` when
    /// no tap is delivering audio (all dead or none spawned): the caller
    /// should fall back to the maximum duck rather than guess.
    pub fn combined_level_dbfs(&self, now: Instant) -> Option<f64> {
        let mut energy = 0.0;
        let mut live = false;
        for tap in self.taps.values() {
            let level = tap.level.lock().unwrap();
            if level.failed {
                continue;
            }
            match level.updated_at {
                Some(at) if now.saturating_duration_since(at) <= STALE_AFTER => {
                    live = true;
                    energy += level.mean_square;
                }
                Some(_) => live = true, // silent for now, but the tap is alive
                None => {}
            }
        }
        live.then(|| 10.0 * energy.max(1e-10).log10())
    }
}

fn spawn_tap(node_id: u32, serial: u32) -> Option<Tap> {
    let mut child = Command::new("pw-record")
        .args([
            "--target",
            &serial.to_string(),
            "-P",
            "{ stream.capture.sink = true, node.name = static-duck-tap, media.role = Production }",
            "--rate",
            &TAP_RATE.to_string(),
            "--channels",
            "1",
            "--format",
            "f32",
            "--raw",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| eprintln!("[PIPEWIRE] failed to spawn duck tap for node {node_id}: {err}"))
        .ok()?;
    let stdout = child.stdout.take()?;
    let level = Arc::new(Mutex::new(BlockLevel::default()));
    let reader_level = level.clone();
    std::thread::Builder::new()
        .name(format!("duck-tap-{node_id}"))
        .spawn(move || read_blocks(stdout, reader_level))
        .ok()?;
    Some(Tap { child, level })
}

fn read_blocks(mut stdout: impl Read, level: Arc<Mutex<BlockLevel>>) {
    let mut buf = vec![0u8; BLOCK_SAMPLES * 4];
    loop {
        let mut filled = 0;
        while filled < buf.len() {
            match stdout.read(&mut buf[filled..]) {
                Ok(0) => {
                    level.lock().unwrap().failed = true;
                    return;
                }
                Ok(n) => filled += n,
                Err(_) => {
                    level.lock().unwrap().failed = true;
                    return;
                }
            }
        }
        let mean_square = buf
            .chunks_exact(4)
            .map(|b| {
                let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64;
                if v.is_finite() { v * v } else { 0.0 }
            })
            .sum::<f64>()
            / BLOCK_SAMPLES as f64;
        let mut guard = level.lock().unwrap();
        guard.mean_square = mean_square;
        guard.updated_at = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_curve_hits_both_ends_and_is_monotonic() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-12);
        let mut last = 0.0;
        for i in 1..=100 {
            let v = ease_in_out(i as f64 / 100.0);
            assert!(v >= last);
            last = v;
        }
        // Flat ends: faint noise barely moves it.
        assert!(ease_in_out(0.1) < 0.01);
        assert!(ease_in_out(0.9) > 0.99);
    }

    #[test]
    fn duck_amount_spans_min_to_max_across_the_level_window() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(duck_amount_for_level(-80.0, 0.3, 0.85), 0.3));
        assert!(close(duck_amount_for_level(QUIET_DBFS, 0.3, 0.85), 0.3));
        assert!(close(duck_amount_for_level(LOUD_DBFS, 0.3, 0.85), 0.85));
        assert!(close(duck_amount_for_level(-5.0, 0.3, 0.85), 0.85));
        let mid = duck_amount_for_level((QUIET_DBFS + LOUD_DBFS) / 2.0, 0.3, 0.85);
        assert!(close(mid, 0.575));
        // Swapped settings are tolerated.
        assert!(close(duck_amount_for_level(-80.0, 0.85, 0.3), 0.3));
    }

    #[test]
    fn follower_attacks_fast_and_releases_slowly() {
        let t0 = Instant::now();
        let mut follower = LevelFollower::default();
        follower.update(-60.0, t0);
        // A voice starts: within 200 ms we are most of the way there.
        let after_attack = follower.update(-20.0, t0 + Duration::from_millis(200));
        assert!(after_attack > -25.0, "{after_attack}");
        // The voice stops: 200 ms later we have barely moved back.
        let after_pause = follower.update(-60.0, t0 + Duration::from_millis(400));
        assert!(after_pause > -35.0, "{after_pause}");
        // ...but after a couple of seconds we are back down.
        let settled = follower.update(-60.0, t0 + Duration::from_millis(2600));
        assert!(settled < -55.0, "{settled}");
    }

    #[test]
    fn taps_report_none_without_streams() {
        let taps = ForeignLevelTaps::default();
        assert!(taps.combined_level_dbfs(Instant::now()).is_none());
    }
}
