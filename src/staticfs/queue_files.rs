//! playback/queue/* and playback/radio/* files (contract B5).

use crate::ipc::DaemonRequest;
use crate::models::{RadioChip, Track};

use super::format::{self, FsMode};
use super::{Ctx, FsError, FsResult};

/// Origin marker for a queue entry: entries at or past the radio
/// `touched_len` boundary are radio-generated; with no radio session every
/// entry is "touched" (same boundary logic as `print_queue` in cli.rs).
fn origin_label(index: usize, touched_len: Option<usize>) -> &'static str {
    match touched_len {
        Some(len) if index >= len => "radio_generated",
        _ => "touched",
    }
}

/// Renders the queue snapshot into `{title, artist, video_id, origin}` rows.
/// Artist formatting matches the CLI (`artist_string`, used by print_queue).
fn queue_rows(queue: &[Track], touched_len: Option<usize>) -> Vec<Vec<String>> {
    queue
        .iter()
        .enumerate()
        .map(|(index, track)| {
            vec![
                track.title.clone(),
                crate::cli::artist_string(track),
                track.video_id.clone(),
                origin_label(index, touched_len).to_string(),
            ]
        })
        .collect()
}

/// Renders radio chips into `{label, selected}` rows.
fn chip_rows(chips: &[RadioChip], selected_chip_index: Option<usize>) -> Vec<Vec<String>> {
    chips
        .iter()
        .enumerate()
        .map(|(index, chip)| {
            vec![
                chip.label.clone(),
                (selected_chip_index == Some(index)).to_string(),
            ]
        })
        .collect()
}

/// Trimmed, case-insensitive chip label match; first match wins.
fn match_chip_label(chips: &[RadioChip], label: &str) -> Option<usize> {
    let wanted = label.trim().to_lowercase();
    chips
        .iter()
        .position(|chip| chip.label.trim().to_lowercase() == wanted)
}

/// playback/queue/items (rw, append), read side. B5: "r: list over
/// snapshot.queue: fields {title, artist, video_id, origin} where
/// origin=\"touched\" if index < touched boundary else \"radio_generated\"
/// (no radio → all \"touched\"). artist = first artist name or joined
/// \"A, B\" — match how the CLI formats (implementer: use same artist join
/// as print_queue in cli.rs)."
pub(crate) fn read_queue_items(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    let snapshot = ctx.with_daemon(|client| client.try_state_request(&DaemonRequest::GetState))?;
    let touched_len = snapshot
        .radio_virtual_playlist
        .as_ref()
        .map(|radio| radio.touched_len);
    Ok(format::list(
        mode,
        &["title", "artist", "video_id", "origin"],
        &queue_rows(&snapshot.queue, touched_len),
    ))
}

/// playback/queue/items (rw, append), write side. B5: "w: each line =
/// free-text query → ResolveAndQueue. Line failing resolution → EINVAL,
/// nothing queued for that line, remaining lines aborted." (fs.rs handles
/// the per-line buffering and the abort-on-first-failure.)
///
/// Non-idempotent (a retry could queue the track twice), so no transport
/// retry: `with_daemon_once`.
pub(crate) fn write_queue_items(ctx: &Ctx, line: &str) -> FsResult<()> {
    ctx.with_daemon_once(|client| client.try_resolve_and_queue(line, false))?;
    Ok(())
}

/// playback/queue/touched_len (r). B5: "scalar: radio touched_len if radio
/// active else queue.len()."
pub(crate) fn read_touched_len(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    let _ = mode; // scalar files ignore fsmod (contract B4)
    let snapshot = ctx.with_daemon(|client| client.try_state_request(&DaemonRequest::GetState))?;
    let touched_len = snapshot
        .radio_virtual_playlist
        .as_ref()
        .map(|radio| radio.touched_len)
        .unwrap_or(snapshot.queue.len());
    Ok(format::scalar(&touched_len.to_string()))
}

/// playback/queue/index (r). B5: "scalar queue_index."
pub(crate) fn read_queue_index(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    let _ = mode; // scalar files ignore fsmod (contract B4)
    let snapshot = ctx.with_daemon(|client| client.try_state_request(&DaemonRequest::GetState))?;
    Ok(format::scalar(&snapshot.queue_index.to_string()))
}

/// playback/queue/clear (w). B5: "any write → ClearQueue." Fired by fs.rs
/// once per open handle, whatever the written bytes.
pub(crate) fn trigger_queue_clear(ctx: &Ctx) -> FsResult<()> {
    ctx.with_daemon(|client| client.try_clear_queue())?;
    Ok(())
}

/// playback/radio/seed (r). B5: "record {video_id, title} of radio source
/// (title looked up from the queue entry matching source_video_id; empty
/// title if absent). Empty content when no radio session."
pub(crate) fn read_radio_seed(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    let snapshot = ctx.with_daemon(|client| client.try_state_request(&DaemonRequest::GetState))?;
    let Some(radio) = snapshot.radio_virtual_playlist.as_ref() else {
        return Ok(Vec::new());
    };
    let title = snapshot
        .queue
        .iter()
        .find(|track| track.video_id == radio.source_video_id)
        .map(|track| track.title.clone())
        .unwrap_or_default();
    Ok(format::record(
        mode,
        &[
            ("video_id", radio.source_video_id.clone()),
            ("title", title),
        ],
    ))
}

/// playback/radio/jump (w). B5: "line = free-text query →
/// ResolveAndPlayRadio. EINVAL on resolve fail." (Write-only but
/// line-based — NOT a trigger file.)
///
/// Non-idempotent (a retry could restart the radio session), so no
/// transport retry: `with_daemon_once`.
pub(crate) fn write_radio_jump(ctx: &Ctx, line: &str) -> FsResult<()> {
    ctx.with_daemon_once(|client| client.try_resolve_and_play_radio(line, false))?;
    Ok(())
}

/// playback/radio/chips (rw), read side. B5: "r: list {label, selected}
/// over radio_virtual_playlist.chips (selected: \"true\"/\"false\" from
/// selected_chip_index). Empty when no radio."
pub(crate) fn read_radio_chips(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    let snapshot = ctx.with_daemon(|client| client.try_state_request(&DaemonRequest::GetState))?;
    let Some(radio) = snapshot.radio_virtual_playlist.as_ref() else {
        return Ok(Vec::new());
    };
    Ok(format::list(
        mode,
        &["label", "selected"],
        &chip_rows(&radio.chips, radio.selected_chip_index),
    ))
}

/// playback/radio/chips (rw), write side. B5: "w: chip label (trim,
/// case-insensitive match) → ActivateRadioChip(index). EINVAL when no radio
/// or label unknown."
pub(crate) fn write_radio_chips(ctx: &Ctx, line: &str) -> FsResult<()> {
    let snapshot = ctx.with_daemon(|client| client.try_state_request(&DaemonRequest::GetState))?;
    let Some(radio) = snapshot.radio_virtual_playlist.as_ref() else {
        return Err(FsError::invalid("no radio session is active"));
    };
    let chip_index = match_chip_label(&radio.chips, line)
        .ok_or_else(|| FsError::invalid(format!("unknown radio chip: {line}")))?;
    ctx.with_daemon(|client| {
        client.try_state_request(&DaemonRequest::ActivateRadioChip { chip_index })
    })?;
    Ok(())
}

#[cfg(test)]
#[path = "tests/queue_files.rs"]
mod tests;
