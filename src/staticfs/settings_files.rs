//! settings/* files (contract B5): key listing via GetSettings, read/write
//! by key, and the local `fsmod` file.
//!
//! Contract B5 settings/ (verbatim):
//! "readdir = keys from GetSettings (daemon order) + \"fsmod\". A setting
//! added to the daemon appears here with no staticfs change. read <key>:
//! value string from GetSettings + \"\\n\". Unknown key → ENOENT.
//! write <key>: SetSetting(key, trimmed line). Daemon Error → EINVAL.
//! fsmod rw: see B4. (Local file; also listed in readdir.)
//! When daemon is down: readdir/settings reads → EIO."
//!
//! `read_fsmod`/`write_fsmod` are purely local (contract B4); everything
//! else round-trips through the daemon per operation.

use super::format::{self, FsMode};
use super::{Ctx, FsError, FsResult};

/// Dynamic readdir/lookup hook for `settings/`: keys from GetSettings in
/// daemon order ("fsmod" is appended by tree.rs, not returned here).
/// Daemon down -> `FsError::Io` (EIO).
pub(crate) fn settings_keys(ctx: &Ctx) -> FsResult<Vec<String>> {
    let pairs = ctx.with_daemon(|client| client.try_settings())?;
    Ok(daemon_keys(&pairs))
}

/// Extracts the key column from GetSettings pairs, preserving daemon order.
fn daemon_keys(pairs: &[(String, String)]) -> Vec<String> {
    pairs.iter().map(|(key, _)| key.clone()).collect()
}

/// Looks up `key` in GetSettings pairs (exact match, daemon order).
fn value_for<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// settings/<key> (rw), read side. B5: "read <key>: value string from
/// GetSettings + \"\\n\". Unknown key → ENOENT." (Scalar rendering; `mode`
/// does not change scalars.)
pub(crate) fn read_setting(ctx: &Ctx, mode: FsMode, key: &str) -> FsResult<Vec<u8>> {
    let _ = mode; // scalar files render identically in awk and jq mode.
    let pairs = ctx.with_daemon(|client| client.try_settings())?;
    match value_for(&pairs, key) {
        Some(value) => Ok(render_setting_value(value)),
        None => Err(FsError::NotFound),
    }
}

/// B5 settings/ reads are value + "\n" UNCONDITIONALLY: unlike
/// `format::scalar`, whose empty-content special case B5 reserves for the
/// playback files (video_id/track/sleep), an empty-valued setting (stock
/// installs: other_mpris_filter_list, pipewire_filter_list, ...) still
/// renders one empty line so `read -r v < settings/<key>` succeeds and
/// `wc -l` reports 1.
fn render_setting_value(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
}

/// settings/<key> (rw), write side. B5: "write <key>: SetSetting(key,
/// trimmed line). Daemon Error → EINVAL." (`line` arrives already trimmed.)
pub(crate) fn write_setting(ctx: &Ctx, key: &str, line: &str) -> FsResult<()> {
    ctx.with_daemon(|client| client.try_set_setting(key, line))?;
    Ok(())
}

/// settings/fsmod (rw), read side. B4: per-mount session mode, `awk`
/// (default) | `jq`. Scalar file: raw value + trailing "\n".
pub(crate) fn read_fsmod(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    let _ = mode;
    Ok(format::scalar(ctx.fsmod().label()))
}

/// settings/fsmod (rw), write side. B4: "Writing anything else → EINVAL."
/// (`line` arrives already trimmed.)
pub(crate) fn write_fsmod(ctx: &Ctx, line: &str) -> FsResult<()> {
    match FsMode::parse(line) {
        Some(mode) => {
            ctx.set_fsmod(mode);
            Ok(())
        }
        None => Err(FsError::invalid(format!(
            "invalid fsmod value `{line}` (expected `awk` or `jq`)"
        ))),
    }
}

#[cfg(test)]
#[path = "tests/settings_files.rs"]
mod tests;
