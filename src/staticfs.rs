//! staticfs: a FUSE filesystem exposing the Static playback daemon and the
//! local stats database as plaintext files (contract Part B).
//!
//! Module layout mirrors the repo's `foo.rs` + `foo/` convention:
//! - `staticfs.rs` (this file): `run_mount`, mount options, signal handling,
//!   tracing init, plus the shared seams every slice codes against (`Ctx`,
//!   `FsError`/`FsResult`).
//! - `staticfs/fs.rs`: the `fuser::Filesystem` implementation.
//! - `staticfs/tree.rs`: the complete node model and read/write dispatch.
//! - `staticfs/format.rs`, `staticfs/playback_files.rs`,
//!   `staticfs/queue_files.rs`, `staticfs/settings_files.rs`,
//!   `staticfs/stats_files.rs`: per-slice content generators.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use fuser::{Config, MountOption, SessionACL, spawn_mount2};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::ipc::{DaemonCallError, DaemonClient};

pub(crate) mod format;
mod fs;
pub(crate) mod playback_files;
pub(crate) mod queue_files;
pub(crate) mod settings_files;
pub(crate) mod stats_files;
pub(crate) mod tree;

use format::FsMode;

/// Error model shared by every slice (contract B3/B6). `fs.rs` maps the
/// variants onto errnos:
/// - `Invalid` -> EINVAL (daemon `DaemonResponse::Error` / local parse failure)
/// - `Io` -> EIO (daemon unreachable, transport error, missing stats DB)
/// - `NotFound` -> ENOENT (unknown path, unknown settings key/video id)
///
/// Reads on write-only files and writes on read-only files are refused with
/// EACCES by `fs.rs` via the per-node permissions before slice code runs.
#[derive(Debug)]
pub(crate) enum FsError {
    /// Rejected input: daemon-side validation failure or local parse failure.
    Invalid(String),
    /// Backend failure: daemon unreachable / transport error / stats DB error.
    Io(String),
    /// Unknown path component (settings key, stats video id, ...).
    NotFound,
}

pub(crate) type FsResult<T> = std::result::Result<T, FsError>;

impl FsError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub(crate) fn io(message: impl Into<String>) -> Self {
        Self::Io(message.into())
    }
}

impl From<DaemonCallError> for FsError {
    fn from(err: DaemonCallError) -> Self {
        match err {
            DaemonCallError::Rejected(message) => FsError::Invalid(message),
            DaemonCallError::Transport(err) => FsError::Io(format!("{err:#}")),
        }
    }
}

/// Lazily connected, cached daemon client. `connect_existing` never
/// auto-spawns the daemon; the connection is rebuilt at most once per
/// operation when a transport failure suggests the daemon restarted.
struct DaemonHandle {
    client: Mutex<Option<DaemonClient>>,
}

impl DaemonHandle {
    fn new() -> Self {
        Self {
            client: Mutex::new(None),
        }
    }

    /// Returns the cached client, connecting first if necessary.
    fn get(&self) -> Result<DaemonClient> {
        let mut guard = self.client.lock().expect("daemon client mutex poisoned");
        if guard.is_none() {
            *guard = Some(DaemonClient::connect_existing()?);
        }
        Ok(guard.as_ref().expect("client populated above").clone())
    }

    /// Drops the cached client so the next `get` reconnects.
    fn invalidate(&self) {
        *self.client.lock().expect("daemon client mutex poisoned") = None;
    }
}

/// Shared context handed to every slice function (contract B6).
pub(crate) struct Ctx {
    daemon: DaemonHandle,
    /// Read-only stats DB seam; internals owned by the stats slice
    /// (`stats_files.rs`). Constructed at mount with `static_root` resolved
    /// exactly once.
    pub(crate) stats: stats_files::StatsDb,
    /// Per-mount `settings/fsmod` session mode (contract B4). Storage lives
    /// here; read/write semantics live in the settings + format slices.
    fsmod: Mutex<FsMode>,
}

impl Ctx {
    pub(crate) fn new(static_root: PathBuf) -> Self {
        Self {
            daemon: DaemonHandle::new(),
            stats: stats_files::StatsDb::new(static_root),
            fsmod: Mutex::new(FsMode::default()),
        }
    }

    fn daemon_client(&self) -> FsResult<DaemonClient> {
        self.daemon
            .get()
            .map_err(|err| FsError::io(format!("{err:#}")))
    }

    /// Runs `op` against the daemon, connecting lazily. On a transport
    /// failure the cached connection is dropped and rebuilt exactly once
    /// (the daemon may have restarted while mounted), then the operation is
    /// retried; a second transport failure surfaces as `FsError::Io`.
    /// `DaemonCallError::Rejected` surfaces as `FsError::Invalid`.
    ///
    /// The retry can re-invoke `op` after a request that reached the daemon
    /// but lost its reply, so only route idempotent requests through here;
    /// non-idempotent ones (ResolveAndQueue, ResolveAndPlayRadio) use
    /// [`Ctx::with_daemon_once`] instead.
    pub(crate) fn with_daemon<T>(
        &self,
        op: impl Fn(&DaemonClient) -> std::result::Result<T, DaemonCallError>,
    ) -> FsResult<T> {
        let client = self.daemon_client()?;
        match op(&client) {
            Err(DaemonCallError::Transport(_)) => {
                self.daemon.invalidate();
                let client = self.daemon_client()?;
                op(&client).map_err(FsError::from)
            }
            other => other.map_err(FsError::from),
        }
    }

    /// Like [`Ctx::with_daemon`], but NEVER retries `op` after a transport
    /// failure: a non-idempotent request that reached the daemon but lost
    /// its reply (e.g. a client-side timeout while the daemon resolves a
    /// query over the network) must not be executed twice. The cached
    /// connection is still dropped so the NEXT operation reconnects.
    pub(crate) fn with_daemon_once<T>(
        &self,
        op: impl FnOnce(&DaemonClient) -> std::result::Result<T, DaemonCallError>,
    ) -> FsResult<T> {
        let client = self.daemon_client()?;
        let result = op(&client);
        if matches!(result, Err(DaemonCallError::Transport(_))) {
            self.daemon.invalidate();
        }
        result.map_err(FsError::from)
    }

    pub(crate) fn fsmod(&self) -> FsMode {
        *self.fsmod.lock().expect("fsmod mutex poisoned")
    }

    pub(crate) fn set_fsmod(&self, mode: FsMode) {
        *self.fsmod.lock().expect("fsmod mutex poisoned") = mode;
    }
}

/// The CLI path never calls `main::init_tracing`, so the mount installs its
/// own subscriber (same `EnvFilter` default pattern as src/main.rs).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn default_mountpoint() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Music").join("staticfs"))
}

/// Stale-mount recovery: a previous staticfs process that died uncleanly
/// leaves the mountpoint disconnected (opening it errors with ENOTCONN);
/// `fusermount3 -u` clears it before we remount.
fn recover_stale_mount(mountpoint: &Path) {
    if let Err(err) = std::fs::read_dir(mountpoint) {
        if err.raw_os_error() == Some(libc::ENOTCONN) {
            tracing::info!(
                "stale staticfs mount detected at {}; running fusermount3 -u",
                mountpoint.display()
            );
            let _ = Command::new("fusermount3")
                .arg("-u")
                .arg(mountpoint)
                .status();
        }
    }
}

/// Refuse to mount over a directory ONLY if it is already a live staticfs
/// mount (any other non-empty directory is accepted).
fn is_live_staticfs_mount(mountpoint: &Path) -> bool {
    let canonical = match mountpoint.canonicalize() {
        Ok(path) => path,
        Err(_) => return false,
    };
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    mounts_list_staticfs_at(&mounts, canonical.to_string_lossy().as_ref())
}

/// Pure matcher behind [`is_live_staticfs_mount`]: does any
/// /proc/self/mounts line list a `staticfs` source mounted at `canonical`?
/// The kernel octal-escapes space/tab/newline/backslash in the target field
/// (`static\040fs`), so the field is decoded before comparing.
fn mounts_list_staticfs_at(mounts: &str, canonical: &str) -> bool {
    mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let source = fields.next();
        let target = fields.next().map(decode_mounts_field);
        source == Some("staticfs") && target.as_deref() == Some(canonical)
    })
}

/// Decodes the kernel's `\NNN` three-octal-digit escapes in a
/// /proc/self/mounts field (`\040` space, `\011` tab, `\012` newline,
/// `\134` backslash). Sequences that are not a backslash followed by
/// exactly three octal digits pass through untouched.
fn decode_mounts_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = bytes.get(index..index + 4).filter(|window| {
            window[0] == b'\\'
                && window[1..]
                    .iter()
                    .all(|digit| (b'0'..=b'7').contains(digit))
        });
        match escape {
            Some(window) => {
                let value = u32::from(window[1] - b'0') * 64
                    + u32::from(window[2] - b'0') * 8
                    + u32::from(window[3] - b'0');
                decoded.push(value as u8);
                index += 4;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn mount_config(auto_unmount: bool) -> Config {
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("staticfs".to_string())];
    if auto_unmount {
        config.mount_options.push(MountOption::AutoUnmount);
        // fuser 0.17 requires an ACL other than `Owner` for AutoUnmount
        // (fusermount needs allow_root/allow_other to implement it).
        config.acl = SessionACL::RootAndOwner;
    }
    config
}

/// `static fs [mountpoint]` — mounts staticfs in the foreground and blocks
/// until the filesystem is unmounted (SIGINT/SIGTERM trigger a clean
/// unmount; external `fusermount3 -u` also ends the session).
pub fn run_mount(args: &[String]) -> Result<()> {
    init_tracing();

    let mountpoint = match args.first() {
        Some(raw) => PathBuf::from(raw),
        None => default_mountpoint()?,
    };

    recover_stale_mount(&mountpoint);
    std::fs::create_dir_all(&mountpoint).with_context(|| {
        format!(
            "Failed to create the mountpoint directory {}",
            mountpoint.display()
        )
    })?;
    if is_live_staticfs_mount(&mountpoint) {
        bail!(
            "staticfs is already mounted at {}; unmount it first with `fusermount3 -u {}`",
            mountpoint.display(),
            mountpoint.display()
        );
    }

    // Resolve the storage root exactly once, before mounting anything (the
    // keyring lookup behind configured storage roots may block).
    let static_root = crate::logging::static_root();

    let make_fs = || fs::StaticFs::new(Ctx::new(static_root.clone()));

    // Prefer AutoUnmount (kernel drops the mount if this process dies), but
    // it maps to allow_other, which most systems reject for regular users
    // unless /etc/fuse.conf enables user_allow_other — fall back to a plain
    // owner-only mount in that case. Signal handling below still guarantees
    // a clean unmount on SIGINT/SIGTERM.
    let session = match spawn_mount2(make_fs(), &mountpoint, &mount_config(true)) {
        Ok(session) => session,
        Err(err) => {
            tracing::warn!("mounting with auto_unmount failed ({err}); retrying without it");
            spawn_mount2(make_fs(), &mountpoint, &mount_config(false))
                .with_context(|| format!("Failed to mount staticfs at {}", mountpoint.display()))?
        }
    };

    println!("{}", mountpoint.display());

    let mut signals =
        Signals::new([SIGINT, SIGTERM]).context("Failed to install signal handlers")?;
    let signal_mountpoint = mountpoint.clone();
    std::thread::spawn(move || {
        for _ in signals.forever() {
            tracing::info!("signal received; unmounting staticfs");
            let unmounted = Command::new("fusermount3")
                .arg("-u")
                .arg(&signal_mountpoint)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !unmounted {
                let _ = Command::new("fusermount3")
                    .args(["-u", "-z"])
                    .arg(&signal_mountpoint)
                    .status();
            }
        }
    });

    session
        .join()
        .context("staticfs session ended with an error")?;
    Ok(())
}

#[cfg(test)]
#[path = "staticfs/tests/staticfs.rs"]
mod tests;
