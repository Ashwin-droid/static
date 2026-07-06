//! In-daemon staticfs mount host: the daemon owns the FUSE driver.
//!
//! staticfs stays architecturally a *client* — the mounted filesystem talks
//! to the daemon over the same D-Bus request model as any other frontend.
//! What the daemon owns is the mount's lifecycle:
//!
//! - mounts when `staticfs_enabled` turns on (startup or hot reload),
//! - unmounts (drains) when it turns off or the mountpoint setting changes,
//! - respawns a crashed or externally-unmounted session with backoff,
//! - and takes the mount down with it on daemon shutdown.
//!
//! Busy unmounts follow the pending halt-auto pattern: when the kernel
//! refuses a clean unmount (open handles), the host publishes a pending
//! state with the busy file paths. `state_snapshot` copies it into
//! `PlaybackSnapshot::staticfs_unmount_pending`, the GUI/TUI render
//! "an app is using {file}" with close-anyway/wait controls, and the CLI
//! and the fs itself deliberately show no prompt. Close-anyway forces a
//! lazy unmount ("crash" the session; straggling handles get ENOTCONN);
//! wait dismisses the prompt and unmounts as soon as the last handle
//! closes. Toggling the setting back on cancels a pending unmount.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fuser::BackgroundSession;

use super::fs::StaticFs;
use super::{Ctx, is_live_staticfs_mount, mount_config, recover_stale_mount};

/// Host loop tick: drain progress checks, mount health checks, backoff.
const TICK: Duration = Duration::from_millis(500);
/// How long a drain may stay busy before the pending prompt is published.
/// Covers the self-toggle race: turning the setting off THROUGH the fs
/// leaves the settings file handle open for a moment — no prompt for that.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Open file paths by fh, updated by `fs.rs` open/release; the host uses it
/// for the "an app is using {file}" message (the busy *oracle* is
/// fusermount3 itself — a cwd inside the mount blocks unmount without any
/// open handle).
pub(crate) type OpenPathTracker = Arc<Mutex<HashMap<u64, String>>>;

enum Command {
    SetDesired {
        enabled: bool,
        mountpoint: Option<String>,
    },
    UnmountNow,
    UnmountWait,
    Shutdown,
}

struct Shared {
    /// Busy paths of a pending (prompted) unmount; `None` when no prompt.
    pending: Mutex<Option<Vec<String>>>,
}

/// Daemon-side handle: cheap to clone into snapshot/request paths.
pub struct HostHandle {
    tx: mpsc::Sender<Command>,
    shared: Arc<Shared>,
}

impl HostHandle {
    /// Spawns the host thread (idle until the first `set_desired(true)`).
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            pending: Mutex::new(None),
        });
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("staticfs-host".to_string())
            .spawn(move || Host::new(thread_shared).run(rx))
            .expect("failed to spawn staticfs host thread");
        Self { tx, shared }
    }

    /// Hot-reload entry: called at daemon startup and from every settings
    /// reload. `mountpoint` is the raw setting value (empty/None = default).
    pub fn set_desired(&self, enabled: bool, mountpoint: Option<String>) {
        let _ = self.tx.send(Command::SetDesired {
            enabled,
            mountpoint,
        });
    }

    /// "Close anyway": force the pending unmount now (lazy detach).
    pub fn unmount_now(&self) {
        let _ = self.tx.send(Command::UnmountNow);
    }

    /// "Wait": dismiss the prompt; the unmount happens once handles close.
    pub fn unmount_wait(&self) {
        let _ = self.tx.send(Command::UnmountWait);
    }

    /// Daemon exiting: unmount (clean, else lazy) and stop the host thread.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown);
    }

    /// Busy paths of a prompted pending unmount, for `state_snapshot`.
    pub fn pending_unmount(&self) -> Option<Vec<String>> {
        self.shared
            .pending
            .lock()
            .expect("staticfs pending mutex poisoned")
            .clone()
    }
}

struct Mounted {
    session: BackgroundSession,
    mountpoint: PathBuf,
    tracker: OpenPathTracker,
}

struct Drain {
    since: Instant,
    /// "Wait" was chosen: keep draining but never (re-)publish the prompt.
    wait: bool,
}

struct Host {
    shared: Arc<Shared>,
    desired: bool,
    mountpoint_setting: Option<String>,
    mounted: Option<Mounted>,
    drain: Option<Drain>,
    backoff: Duration,
    next_attempt: Option<Instant>,
    static_root: Option<PathBuf>,
    /// Lazily-unmounted sessions whose fuse thread may still serve straggling
    /// handles; never joined (that could block the host on a stuck reader).
    graveyard: Vec<BackgroundSession>,
}

impl Host {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            desired: false,
            mountpoint_setting: None,
            mounted: None,
            drain: None,
            backoff: BACKOFF_MIN,
            next_attempt: None,
            static_root: None,
            graveyard: Vec::new(),
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Command>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Command::SetDesired {
                    enabled,
                    mountpoint,
                }) => {
                    self.desired = enabled;
                    self.mountpoint_setting = mountpoint;
                    if enabled {
                        // Toggling back on cancels a pending unmount (unless
                        // the mountpoint moved, which tick() re-detects).
                        self.drain = None;
                        self.clear_pending();
                        self.next_attempt = None;
                        self.backoff = BACKOFF_MIN;
                    }
                }
                Ok(Command::UnmountNow) => {
                    if self.drain.is_some() || (self.mounted.is_some() && !self.desired) {
                        self.force_unmount();
                    }
                }
                Ok(Command::UnmountWait) => {
                    if let Some(drain) = self.drain.as_mut() {
                        drain.wait = true;
                    }
                    self.clear_pending();
                }
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    if let Some(mounted) = self.mounted.take() {
                        if !try_clean_unmount(&mounted.mountpoint) {
                            lazy_unmount(&mounted.mountpoint);
                            // A lazily-detached session may block drop/join on
                            // straggling handles; the process is exiting, so
                            // deliberately leak instead of hanging shutdown.
                            std::mem::forget(mounted.session);
                        }
                    }
                    for session in self.graveyard.drain(..) {
                        std::mem::forget(session);
                    }
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
    }

    fn tick(&mut self) {
        let want_mountpoint = self.resolved_mountpoint();
        let needs_drain = match (&self.mounted, self.desired) {
            (Some(_), false) => true,
            (Some(mounted), true) => mounted.mountpoint != want_mountpoint,
            (None, _) => false,
        };

        if needs_drain {
            self.drain_tick();
            return;
        }
        self.drain = None;
        self.clear_pending();

        if !self.desired {
            return;
        }

        // Health check: respawn (with backoff) a session that died or was
        // unmounted externally. Setting on == mounted.
        if let Some(mounted) = &self.mounted {
            if !is_live_staticfs_mount(&mounted.mountpoint) {
                tracing::warn!(
                    "staticfs mount at {} disappeared; respawning",
                    mounted.mountpoint.display()
                );
                let dead = self.mounted.take().expect("checked above");
                self.graveyard.push(dead.session);
                self.schedule_retry();
            }
            return;
        }

        if self
            .next_attempt
            .map(|at| Instant::now() >= at)
            .unwrap_or(true)
        {
            self.mount(&want_mountpoint);
        }
    }

    /// One drain step: try a clean unmount; while the kernel refuses (open
    /// handles / cwd inside the mount), publish the prompt after the grace
    /// window unless "wait" dismissed it.
    fn drain_tick(&mut self) {
        let Some(mounted) = &self.mounted else { return };
        if try_clean_unmount(&mounted.mountpoint) {
            let mounted = self.mounted.take().expect("checked above");
            tracing::info!("staticfs unmounted from {}", mounted.mountpoint.display());
            // Clean unmount ends the fuse loop; joining via drop is safe.
            drop(mounted.session);
            self.drain = None;
            self.clear_pending();
            return;
        }

        let drain = self.drain.get_or_insert(Drain {
            since: Instant::now(),
            wait: false,
        });
        if drain.wait || drain.since.elapsed() < DRAIN_GRACE {
            return;
        }
        let mut busy = {
            let tracker = mounted
                .tracker
                .lock()
                .expect("staticfs tracker mutex poisoned");
            let mut paths: Vec<String> = tracker.values().cloned().collect();
            paths.sort();
            paths.dedup();
            paths
        };
        if busy.is_empty() {
            // No open fs handle, yet the kernel says busy: something like a
            // shell cwd pins the mount. Name the mountpoint itself.
            busy.push(mounted.mountpoint.display().to_string());
        }
        *self
            .shared
            .pending
            .lock()
            .expect("staticfs pending mutex poisoned") = Some(busy);
    }

    fn mount(&mut self, mountpoint: &PathBuf) {
        recover_stale_mount(mountpoint);
        if let Err(err) = std::fs::create_dir_all(mountpoint) {
            tracing::warn!(
                "staticfs: cannot create mountpoint {}: {err}",
                mountpoint.display()
            );
            self.schedule_retry();
            return;
        }
        if is_live_staticfs_mount(mountpoint) {
            tracing::warn!(
                "staticfs: {} is already a live staticfs mount (foreign); retrying later",
                mountpoint.display()
            );
            self.schedule_retry();
            return;
        }

        // Resolve the storage root exactly once per daemon (the keyring
        // lookup behind configured storage roots may block).
        let static_root = self
            .static_root
            .get_or_insert_with(crate::logging::static_root)
            .clone();
        let tracker: OpenPathTracker = Arc::new(Mutex::new(HashMap::new()));
        let make_fs = || StaticFs::with_tracker(Ctx::new(static_root.clone()), tracker.clone());

        // Prefer AutoUnmount (kernel drops the mount if the daemon dies), but
        // it maps to allow_other, which most systems reject for regular users
        // unless /etc/fuse.conf enables user_allow_other — fall back to a
        // plain owner-only mount in that case.
        let session = match fuser::spawn_mount2(make_fs(), mountpoint, &mount_config(true)) {
            Ok(session) => Ok(session),
            Err(err) => {
                tracing::warn!(
                    "staticfs: mounting with auto_unmount failed ({err}); retrying without it"
                );
                fuser::spawn_mount2(make_fs(), mountpoint, &mount_config(false))
            }
        };
        match session {
            Ok(session) => {
                tracing::info!("staticfs mounted at {}", mountpoint.display());
                self.mounted = Some(Mounted {
                    session,
                    mountpoint: mountpoint.clone(),
                    tracker,
                });
                self.backoff = BACKOFF_MIN;
                self.next_attempt = None;
            }
            Err(err) => {
                tracing::warn!(
                    "staticfs: failed to mount at {}: {err}",
                    mountpoint.display()
                );
                self.schedule_retry();
            }
        }
    }

    /// "Close anyway" / shutdown path: lazy-detach the mount even with open
    /// handles (they get ENOTCONN — the deliberate "crash" semantics).
    fn force_unmount(&mut self) {
        if let Some(mounted) = self.mounted.take() {
            if !try_clean_unmount(&mounted.mountpoint) {
                lazy_unmount(&mounted.mountpoint);
                self.graveyard.push(mounted.session);
            } else {
                drop(mounted.session);
            }
            tracing::info!(
                "staticfs force-unmounted from {}",
                mounted.mountpoint.display()
            );
        }
        self.drain = None;
        self.clear_pending();
    }

    fn schedule_retry(&mut self) {
        self.next_attempt = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }

    fn clear_pending(&self) {
        *self
            .shared
            .pending
            .lock()
            .expect("staticfs pending mutex poisoned") = None;
    }

    /// The raw mountpoint setting resolved to a path: empty = the default
    /// (~/Music/staticfs), leading `~/` expands to $HOME.
    fn resolved_mountpoint(&self) -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        match self
            .mountpoint_setting
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            None => PathBuf::from(home).join("Music").join("staticfs"),
            Some(value) => match value.strip_prefix("~/") {
                Some(rest) => PathBuf::from(home).join(rest),
                None => PathBuf::from(value),
            },
        }
    }
}

fn try_clean_unmount(mountpoint: &PathBuf) -> bool {
    ProcessCommand::new("fusermount3")
        .arg("-u")
        .arg(mountpoint)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn lazy_unmount(mountpoint: &PathBuf) {
    let _ = ProcessCommand::new("fusermount3")
        .args(["-u", "-z"])
        .arg(mountpoint)
        .output();
}
