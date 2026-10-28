//! Session lock / screen blank watcher.
//!
//! Static pauses itself when the desktop session locks or the screen blanks.
//! This is unconditional product behaviour (no setting): music playing into
//! a dark, locked room is never what the user meant.
//!
//! The pause is edge-triggered, not level-held. Each transition into a
//! locked/blanked state asks the daemon to pause exactly once; if the user
//! then presses Play from the lock screen (MPRIS), that is their call and
//! nothing re-pauses them while the lock stays up. Unlocking does not resume.
//!
//! Signals watched, in a background thread per bus:
//! - system bus, `org.freedesktop.login1.Session` for our own session:
//!   the `Lock` signal and `LockedHint`/`IdleHint` flipping to true. GNOME
//!   raises `IdleHint` together with the screen blank, so this covers blank
//!   on compositors that never show a screensaver object;
//! - session bus, `ActiveChanged(true)` from `org.gnome.ScreenSaver` or
//!   `org.freedesktop.ScreenSaver` (the shield / screensaver coming up).
//!
//! Events are queued into a shared list; the daemon drains them from its
//! maintenance tick with `take_trigger`, which collapses a burst (GNOME
//! raises shield, idle hint and locked hint within a few hundred ms) into a
//! single pause.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use zbus::MatchRule;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::OwnedValue;

/// How long the watcher waits before reconnecting after a bus failure.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

pub struct SessionLockMonitor {
    pending: Arc<Mutex<Vec<String>>>,
}

impl SessionLockMonitor {
    pub fn new() -> Self {
        let pending = Arc::new(Mutex::new(Vec::new()));
        for watcher in [
            watch_login1 as fn(&Arc<Mutex<Vec<String>>>) -> zbus::Result<()>,
            watch_screensaver,
        ] {
            let pending = pending.clone();
            thread::spawn(move || {
                loop {
                    if let Err(err) = watcher(&pending) {
                        eprintln!(
                            "[SESSION] lock watcher stopped: {err}; retrying in {RECONNECT_DELAY:?}"
                        );
                    }
                    thread::sleep(RECONNECT_DELAY);
                }
            });
        }
        Self { pending }
    }

    /// Returns the reason for a pending lock/blank pause, draining every
    /// queued event so a burst of signals pauses once.
    pub fn take_trigger(&self) -> Option<String> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.is_empty() {
            return None;
        }
        let reasons = std::mem::take(&mut *pending);
        Some(collapse_reasons(&reasons))
    }
}

fn collapse_reasons(reasons: &[String]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for reason in reasons {
        if !seen.contains(&reason.as_str()) {
            seen.push(reason);
        }
    }
    seen.join(" + ")
}

fn push(pending: &Arc<Mutex<Vec<String>>>, reason: &str) {
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(reason.to_string());
}

fn watch_login1(pending: &Arc<Mutex<Vec<String>>>) -> zbus::Result<()> {
    let conn = Connection::system()?;
    let auto = Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1/session/auto",
        "org.freedesktop.login1.Session",
    )?;
    let session_id: String = auto.get_property("Id")?;
    let path = format!("/org/freedesktop/login1/session/{session_id}");
    // Signals come from the real object path, never the `auto` alias.
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .path(path.as_str())?
        .build();
    let iter = MessageIterator::for_match_rule(rule, &conn, None)?;
    eprintln!("[SESSION] watching login1 session {session_id} for lock/blank");
    for msg in iter {
        let msg = msg?;
        let header = msg.header();
        let interface = header.interface().map(|i| i.as_str().to_string());
        let member = header.member().map(|m| m.as_str().to_string());
        match (interface.as_deref(), member.as_deref()) {
            (Some("org.freedesktop.login1.Session"), Some("Lock")) => {
                push(pending, "session locked");
            }
            (Some("org.freedesktop.DBus.Properties"), Some("PropertiesChanged")) => {
                // A malformed signal from a stray sender must not kill the
                // watcher: skip it and keep listening.
                let Ok((iface, changed, _invalidated)) =
                    msg.body()
                        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                else {
                    continue;
                };
                if iface != "org.freedesktop.login1.Session" {
                    continue;
                }
                for reason in login1_property_reasons(&changed) {
                    push(pending, reason);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn login1_property_reasons(changed: &HashMap<String, OwnedValue>) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    for (name, reason) in [
        ("LockedHint", "session locked"),
        ("IdleHint", "screen blanked"),
    ] {
        if changed
            .get(name)
            .and_then(|value| bool::try_from(value).ok())
            == Some(true)
        {
            reasons.push(reason);
        }
    }
    reasons
}

fn watch_screensaver(pending: &Arc<Mutex<Vec<String>>>) -> zbus::Result<()> {
    let conn = Connection::session()?;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .member("ActiveChanged")?
        .build();
    let iter = MessageIterator::for_match_rule(rule, &conn, None)?;
    eprintln!("[SESSION] watching the session bus for screensaver activation");
    for msg in iter {
        let msg = msg?;
        let header = msg.header();
        let is_screensaver = header.interface().is_some_and(|iface| {
            matches!(
                iface.as_str(),
                "org.gnome.ScreenSaver" | "org.freedesktop.ScreenSaver"
            )
        });
        if !is_screensaver {
            continue;
        }
        if msg.body().deserialize::<bool>().is_ok_and(|active| active) {
            push(pending, "screen blanked");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, bool)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), OwnedValue::from(*value)))
            .collect()
    }

    #[test]
    fn only_true_transitions_of_lock_or_idle_hints_count() {
        assert_eq!(
            login1_property_reasons(&props(&[("LockedHint", true)])),
            vec!["session locked"]
        );
        assert_eq!(
            login1_property_reasons(&props(&[("IdleHint", true)])),
            vec!["screen blanked"]
        );
        assert!(
            login1_property_reasons(&props(&[("LockedHint", false), ("IdleHint", false)]))
                .is_empty()
        );
        assert!(login1_property_reasons(&props(&[("Active", true)])).is_empty());
    }

    #[test]
    fn a_burst_of_signals_collapses_into_one_reason_string() {
        let reasons = [
            "screen blanked".to_string(),
            "screen blanked".to_string(),
            "session locked".to_string(),
        ];
        assert_eq!(
            collapse_reasons(&reasons),
            "screen blanked + session locked"
        );
    }

    #[test]
    fn take_trigger_drains_the_queue() {
        let monitor = SessionLockMonitor {
            pending: Arc::new(Mutex::new(vec!["session locked".to_string()])),
        };
        assert_eq!(monitor.take_trigger().as_deref(), Some("session locked"));
        assert_eq!(monitor.take_trigger(), None);
    }
}

#[cfg(test)]
mod live_bus_tests {
    //! Plumbing check against the real buses: fake the signals the watcher
    //! listens for instead of locking the desktop. Run with
    //! `cargo test -- --ignored session_lock::live_bus_tests`.
    use super::*;
    use std::process::Command;
    use std::time::Instant;

    fn wait_for_trigger(monitor: &SessionLockMonitor) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(reason) = monitor.take_trigger() {
                return Some(reason);
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }

    #[test]
    #[ignore]
    fn fake_screensaver_and_login1_signals_reach_the_queue() {
        let monitor = SessionLockMonitor::new();
        thread::sleep(Duration::from_millis(500));

        let status = Command::new("gdbus")
            .args([
                "emit",
                "--session",
                "--object-path",
                "/org/gnome/ScreenSaver",
                "--signal",
                "org.gnome.ScreenSaver.ActiveChanged",
                "true",
            ])
            .status()
            .expect("gdbus");
        assert!(status.success());
        assert_eq!(
            wait_for_trigger(&monitor).as_deref(),
            Some("screen blanked")
        );

        let session_id = String::from_utf8(
            Command::new("busctl")
                .args([
                    "get-property",
                    "org.freedesktop.login1",
                    "/org/freedesktop/login1/session/auto",
                    "org.freedesktop.login1.Session",
                    "Id",
                ])
                .output()
                .expect("busctl")
                .stdout,
        )
        .unwrap();
        let session_id = session_id
            .trim()
            .trim_start_matches("s ")
            .trim_matches('"')
            .to_string();
        let status = Command::new("gdbus")
            .args([
                "emit",
                "--system",
                "--object-path",
                &format!("/org/freedesktop/login1/session/{session_id}"),
                "--signal",
                "org.freedesktop.DBus.Properties.PropertiesChanged",
                "org.freedesktop.login1.Session",
                "{'LockedHint': <true>}",
                "@as []",
            ])
            .status()
            .expect("gdbus");
        assert!(status.success());
        assert_eq!(
            wait_for_trigger(&monitor).as_deref(),
            Some("session locked")
        );

        // A false edge must not queue anything.
        let status = Command::new("gdbus")
            .args([
                "emit",
                "--session",
                "--object-path",
                "/org/gnome/ScreenSaver",
                "--signal",
                "org.gnome.ScreenSaver.ActiveChanged",
                "false",
            ])
            .status()
            .expect("gdbus");
        assert!(status.success());
        thread::sleep(Duration::from_millis(500));
        assert_eq!(monitor.take_trigger(), None);
    }
}
