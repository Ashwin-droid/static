//! playback/* files except queue/ and radio/ (contract B5 playback table).
//! Every read starts from ONE `GetState` snapshot per operation
//! (`ctx.with_daemon(|client| client.try_state_request(&DaemonRequest::GetState))`).

use anyhow::anyhow;

use super::format::{self, FsMode};
use super::{Ctx, FsError, FsResult};
use crate::ipc::{DaemonCallError, DaemonRequest, DaemonResponse, PlaybackSnapshot, RepeatMode};
use crate::models::SyncedLyrics;
use crate::network::SongRating;

/// One `GetState` snapshot per operation (contract B5 preamble).
fn snapshot(ctx: &Ctx) -> FsResult<PlaybackSnapshot> {
    ctx.with_daemon(|client| client.try_state_request(&DaemonRequest::GetState))
}

/// Fire-and-forget command: any non-`Error` daemon response is success.
fn send(ctx: &Ctx, request: &DaemonRequest) -> FsResult<()> {
    ctx.with_daemon(|client| client.try_request(request))?;
    Ok(())
}

/// playback/state write grammar (pure; `line` is trimmed and non-empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateCommand {
    Resume,
    Pause,
    Toggle,
}

fn parse_state_token(line: &str) -> Option<StateCommand> {
    match line {
        "play" | "playing" | "resume" => Some(StateCommand::Resume),
        "pause" | "paused" => Some(StateCommand::Pause),
        "toggle" => Some(StateCommand::Toggle),
        _ => None,
    }
}

/// playback/position write grammar: non-negative number (int or float,
/// seconds) -> millis. `None` on NaN/negative/junk.
fn parse_position_millis(line: &str) -> Option<u64> {
    let secs: f64 = line.parse().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some((secs * 1000.0).round() as u64)
}

/// playback/volume write grammar: f64 in 0.0..=1.0.
fn parse_volume_value(line: &str) -> Option<f64> {
    let volume: f64 = line.parse().ok()?;
    if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
        return None;
    }
    Some(volume)
}

/// playback/repeat grammar: "off" | "all" | "one" (approximation 4: no
/// loop-budget syntax).
fn parse_repeat_token(line: &str) -> Option<RepeatMode> {
    match line {
        "off" => Some(RepeatMode::Off),
        "all" => Some(RepeatMode::All),
        "one" => Some(RepeatMode::One),
        _ => None,
    }
}

fn repeat_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    }
}

/// playback/rating grammar: liked/disliked/neutral <-> Like/Dislike/Indifferent.
fn parse_rating_token(line: &str) -> Option<SongRating> {
    match line {
        "liked" => Some(SongRating::Like),
        "disliked" => Some(SongRating::Dislike),
        "neutral" => Some(SongRating::Indifferent),
        _ => None,
    }
}

fn rating_label(rating: SongRating) -> &'static str {
    match rating {
        SongRating::Like => "liked",
        SongRating::Dislike => "disliked",
        SongRating::Indifferent => "neutral",
    }
}

/// playback/sleep write grammar (pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SleepCommand {
    /// positive integer N -> SetSleepTimer(N minutes)
    Minutes(u32),
    /// "off" / "0" -> ClearSleepTimer
    Clear,
    /// "current" -> SetSleepAfterCurrent
    AfterCurrent,
}

fn parse_sleep_token(line: &str) -> Option<SleepCommand> {
    match line {
        "off" => Some(SleepCommand::Clear),
        "current" => Some(SleepCommand::AfterCurrent),
        _ => match line.parse::<u32>().ok()? {
            0 => Some(SleepCommand::Clear),
            minutes => Some(SleepCommand::Minutes(minutes)),
        },
    }
}

/// Lyrics revealed up to the current position: every line with
/// `start_millis <= position_millis`, text one per line, in source order.
/// No revealed lines -> empty content.
fn reveal_lyrics(lyrics: &SyncedLyrics, position_millis: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for line in &lyrics.lines {
        if line.start_millis <= position_millis {
            out.extend_from_slice(line.text.as_bytes());
            out.push(b'\n');
        }
    }
    out
}

/// playback/state (rw), read side. B5: "r: \"playing\"|\"paused\" (paused
/// flag)." Scalar file: raw value + trailing "\n" regardless of fsmod.
pub(crate) fn read_state(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    Ok(format::scalar(if snap.paused {
        "paused"
    } else {
        "playing"
    }))
}

/// playback/state (rw), write side. B5: "w: \"play\"/\"playing\"/\"resume\"→
/// Resume; \"pause\"/\"paused\"→Pause; \"toggle\" or empty write→TogglePause;
/// else EINVAL." (An empty write arrives as the literal "toggle" via
/// `Node::empty_line_command`, so `line` here is always non-empty and
/// trimmed.)
pub(crate) fn write_state(ctx: &Ctx, line: &str) -> FsResult<()> {
    let request = match parse_state_token(line) {
        Some(StateCommand::Resume) => DaemonRequest::Resume,
        Some(StateCommand::Pause) => DaemonRequest::Pause,
        Some(StateCommand::Toggle) => DaemonRequest::TogglePause,
        None => {
            return Err(FsError::invalid(format!(
                "invalid playback state command: {line:?}"
            )));
        }
    };
    send(ctx, &request)
}

/// playback/track (r). B5: "record {title, artist, video_id} of current
/// track (awk: title\tartist\tvideo_id). Empty content when nothing playing."
pub(crate) fn read_track(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    let Some(track) = snap.current_track else {
        return Ok(format::record(mode, &[]));
    };
    Ok(format::record(
        mode,
        &[
            ("title", track.title),
            ("artist", track.artist),
            ("video_id", track.video_id),
        ],
    ))
}

/// playback/video_id (r). B5: "scalar. Empty when nothing playing."
pub(crate) fn read_video_id(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    let video_id = snap
        .current_track
        .map(|track| track.video_id)
        .unwrap_or_default();
    Ok(format::scalar(&video_id))
}

/// playback/position (rw), read side. B5: "scalar integer seconds
/// (position_millis/1000)." Empty when no position (nothing playing).
pub(crate) fn read_position(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    let value = snap
        .position_millis
        .map(|millis| (millis / 1000).to_string())
        .unwrap_or_default();
    Ok(format::scalar(&value))
}

/// playback/position (rw), write side. B5: "w: non-negative number (int or
/// float, seconds) → SeekToMillis(secs*1000). EINVAL on NaN/neg/junk."
pub(crate) fn write_position(ctx: &Ctx, line: &str) -> FsResult<()> {
    let position_millis = parse_position_millis(line)
        .ok_or_else(|| FsError::invalid(format!("invalid position (seconds): {line:?}")))?;
    send(ctx, &DaemonRequest::SeekToMillis { position_millis })
}

/// playback/duration (r). B5: "scalar integer seconds." Empty when unknown.
pub(crate) fn read_duration(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    let value = snap
        .duration_millis
        .map(|millis| (millis / 1000).to_string())
        .unwrap_or_default();
    Ok(format::scalar(&value))
}

/// playback/volume (rw), read side. B5: "scalar float 0.0-1.0 (2 decimals
/// ok, print like 0.75)."
pub(crate) fn read_volume(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    Ok(format::scalar(&format!("{:.2}", snap.volume)))
}

/// playback/volume (rw), write side. B5: "w: f64 in 0.0..=1.0 → SetVolume,
/// else EINVAL."
pub(crate) fn write_volume(ctx: &Ctx, line: &str) -> FsResult<()> {
    let volume = parse_volume_value(line)
        .ok_or_else(|| FsError::invalid(format!("invalid volume (0.0..=1.0): {line:?}")))?;
    send(ctx, &DaemonRequest::SetVolume { volume })
}

/// playback/repeat (rw), read side. B5: "scalar \"off\"|\"all\"|\"one\"."
pub(crate) fn read_repeat(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    Ok(format::scalar(repeat_label(snap.repeat_mode)))
}

/// playback/repeat (rw), write side. B5: "w: those tokens → SetRepeatMode,
/// else EINVAL." (Approximation 4: off|all|one only, no loop-budget syntax.)
pub(crate) fn write_repeat(ctx: &Ctx, line: &str) -> FsResult<()> {
    let repeat_mode = parse_repeat_token(line)
        .ok_or_else(|| FsError::invalid(format!("invalid repeat mode (off|all|one): {line:?}")))?;
    send(ctx, &DaemonRequest::SetRepeatMode { repeat_mode })
}

/// playback/next (w). B5: "any write → Next (once per open handle)."
/// Fired by fs.rs on the first write() of the handle, whatever the bytes.
pub(crate) fn trigger_next(ctx: &Ctx) -> FsResult<()> {
    send(ctx, &DaemonRequest::Next)
}

/// playback/prev (w). B5: "any write → Previous (once per open handle)."
pub(crate) fn trigger_prev(ctx: &Ctx) -> FsResult<()> {
    send(ctx, &DaemonRequest::Previous)
}

/// playback/ducked (r). B5: "scalar \"true\"|\"false\" (new snapshot field)."
pub(crate) fn read_ducked(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    Ok(format::scalar(if snap.ducked { "true" } else { "false" }))
}

/// playback/active_peers (r). B5: "list of CURRENTLY ACTIVE coordination
/// targets from GetCoordinationTargets (filter .active). Fields {kind,
/// label, detail} where kind=\"mpris\"|\"pipewire\"."
pub(crate) fn read_active_peers(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    let targets = ctx.with_daemon(|client| {
        match client.try_request(&DaemonRequest::GetCoordinationTargets)? {
            DaemonResponse::CoordinationTargets(targets) => Ok(targets),
            other => Err(DaemonCallError::Transport(anyhow!(
                "unexpected daemon response to GetCoordinationTargets: {other:?}"
            ))),
        }
    })?;
    let mut rows = Vec::new();
    for (kind, group) in [("mpris", &targets.mpris), ("pipewire", &targets.pipewire)] {
        for target in group.iter().filter(|target| target.active) {
            rows.push(vec![
                kind.to_string(),
                target.label.clone(),
                target.detail.clone(),
            ]);
        }
    }
    Ok(format::list(mode, &["kind", "label", "detail"], &rows))
}

/// playback/rating (rw), read side. B5: "scalar \"liked\"|\"disliked\"|
/// \"neutral\" (from ActiveTrackSnapshot.rating: Like/Dislike/Indifferent)."
/// Empty content when nothing playing.
pub(crate) fn read_rating(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    let value = snap
        .current_track
        .map(|track| rating_label(track.rating))
        .unwrap_or_default();
    Ok(format::scalar(value))
}

/// playback/rating (rw), write side. B5: "w: those tokens →
/// RateCurrentTrack. EINVAL else; EINVAL (from daemon Error) when nothing
/// playing."
pub(crate) fn write_rating(ctx: &Ctx, line: &str) -> FsResult<()> {
    let rating = parse_rating_token(line).ok_or_else(|| {
        FsError::invalid(format!("invalid rating (liked|disliked|neutral): {line:?}"))
    })?;
    send(ctx, &DaemonRequest::RateCurrentTrack { rating })
}

/// playback/lyrics (r). B5: "synced lyrics REVEALED UP TO CURRENT POSITION:
/// all SyncedLyricLine.text with start_millis <= current position_millis,
/// one per line, in order. This makes `cat` show sung-so-far and `tail -f`
/// stream lines as they're sung (tail polls getattr; size only grows within
/// a track). Empty when no track / no lyrics (daemon returns Lyrics(None) —
/// treat as empty content, NOT error)." The nothing-playing case arrives as
/// Rejected("Nothing is playing") and is also empty content.
pub(crate) fn read_lyrics(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    if snap.current_track.is_none() {
        return Ok(Vec::new());
    }
    let lyrics = match ctx.with_daemon(|client| client.try_lyrics()) {
        Ok(lyrics) => lyrics,
        // The track may have ended between the snapshot and the fetch.
        Err(FsError::Invalid(message)) if message == "Nothing is playing" => None,
        Err(err) => return Err(err),
    };
    let Some(lyrics) = lyrics else {
        return Ok(Vec::new());
    };
    Ok(reveal_lyrics(&lyrics, snap.position_millis.unwrap_or(0)))
}

/// playback/sleep (rw), read side. B5: "r: remaining seconds (scalar) or
/// empty when no timer; \"current\" form shows \"after_current\" when
/// sleep_after_current."
pub(crate) fn read_sleep(ctx: &Ctx, _mode: FsMode) -> FsResult<Vec<u8>> {
    let snap = snapshot(ctx)?;
    if snap.sleep_after_current {
        return Ok(format::scalar("after_current"));
    }
    let value = snap
        .sleep_remaining_secs
        .map(|secs| secs.to_string())
        .unwrap_or_default();
    Ok(format::scalar(&value))
}

/// playback/sleep (rw), write side. B5: "w: positive integer N →
/// SetSleepTimer(N minutes); \"off\"/\"0\" → ClearSleepTimer; \"current\" →
/// SetSleepAfterCurrent; else EINVAL."
pub(crate) fn write_sleep(ctx: &Ctx, line: &str) -> FsResult<()> {
    let request = match parse_sleep_token(line) {
        Some(SleepCommand::Minutes(minutes)) => DaemonRequest::SetSleepTimer { minutes },
        Some(SleepCommand::Clear) => DaemonRequest::ClearSleepTimer,
        Some(SleepCommand::AfterCurrent) => DaemonRequest::SetSleepAfterCurrent,
        None => {
            return Err(FsError::invalid(format!(
                "invalid sleep command (minutes|off|0|current): {line:?}"
            )));
        }
    };
    send(ctx, &request)
}

#[cfg(test)]
#[path = "tests/playback_files.rs"]
mod tests;
