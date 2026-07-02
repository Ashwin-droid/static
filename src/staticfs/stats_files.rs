//! stats/* files (contract B5): read-only SQLite backend over data.db with
//! a 500ms TTL cache per query result.
//!
//! Contract B2/B3 backend rules (verbatim excerpts):
//! "SQLite: resolve `crate::logging::static_root()` ONCE at mount ..., open
//! data.db read-only (READ_ONLY plus default threading), busy_timeout(1s).
//! If the file is missing at mount: still mount; stats reads return EIO
//! until it appears (retry open per read burst). Keep every query a short
//! autocommit statement — NO long transactions."
//! "TTL cache (stats only): cache each stats query result for 500ms
//! (std::time::Instant) so `cat stats/tracks/*/play_count` globs collapse to
//! ~1 query. NEVER cache live (playback/, settings/) reads."
//!
//! Implementation: every read goes through one cached [`Snapshot`] of the
//! four small tables involved (plays, listen_again_candidates, offline,
//! app_settings score window). A glob burst therefore collapses to at most
//! four short autocommit SELECTs per 500ms. A fresh read-only connection is
//! opened for each snapshot refresh (at most one open per 500ms burst, then
//! dropped) so a data.db that appears, is atomically replaced, or is
//! deleted after mount is picked up without a remount — a cached connection
//! would keep serving the old unlinked inode forever.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use super::format::{self, FsMode};
use super::tree::TrackField;
use super::{Ctx, FsError, FsResult};

/// TTL for the table snapshot (contract B3: 500ms).
const CACHE_TTL: Duration = Duration::from_millis(500);
/// Default for `score_decay_window_days` when the app_settings row is absent
/// (matches AppSettings::default in settings_offline.rs).
const DEFAULT_SCORE_DECAY_WINDOW_DAYS: u32 = 30;
/// Row cap for the stats/top/* lists (contract B5: LIMIT 100).
const TOP_LIMIT: usize = 100;

/// One `plays` row (cumulative counters, one row per track).
struct PlayRow {
    video_id: String,
    title: String,
    artist: String,
    play_count: i64,
    listen_secs: i64,
    last_played: i64,
}

/// A point-in-time copy of every table the stats files read.
struct Snapshot {
    /// plays rows in table order; also the readdir order of stats/tracks/.
    tracks: Vec<PlayRow>,
    /// video_id -> index into `tracks`.
    by_id: HashMap<String, usize>,
    /// listen_again_candidates: video_id -> last_recommended_at.
    last_recommended: HashMap<String, i64>,
    /// offline: video_id -> audio_path (existence on disk checked per read).
    offline_audio: HashMap<String, String>,
    /// app_settings score_decay_window_days (default 30 when absent).
    decay_window_days: u32,
}

struct Inner {
    cached: Option<(Instant, Arc<Snapshot>)>,
}

/// Read-only handle onto the stats database. Constructed at mount
/// (`Ctx::new`) with `static_root` resolved exactly once; a fresh read-only
/// connection is opened per snapshot refresh so a data.db that appears, is
/// replaced or is deleted after mount is picked up without a remount.
pub(crate) struct StatsDb {
    /// crate::logging::static_root().join("data.db") — never hardcode
    /// ~/Music/Static.
    db_path: PathBuf,
    inner: Mutex<Inner>,
}

impl StatsDb {
    pub(crate) fn new(static_root: PathBuf) -> Self {
        Self {
            db_path: static_root.join("data.db"),
            inner: Mutex::new(Inner { cached: None }),
        }
    }

    /// Returns the current table snapshot, refreshing it when the cached one
    /// is older than [`CACHE_TTL`]. Each refresh opens (and drops) its own
    /// read-only connection — never cached across bursts, so a data.db that
    /// was deleted or atomically replaced (rename-over restore, cache reset)
    /// is detected instead of serving the old unlinked inode forever.
    /// Missing/unopenable data.db and query failures surface as
    /// `FsError::Io`.
    fn snapshot(&self) -> FsResult<Arc<Snapshot>> {
        let mut inner = self.inner.lock().expect("stats db mutex poisoned");
        if let Some((taken, snapshot)) = &inner.cached {
            if taken.elapsed() < CACHE_TTL {
                return Ok(snapshot.clone());
            }
        }
        let conn = open_read_only(&self.db_path).map_err(|err| {
            inner.cached = None;
            FsError::io(format!(
                "failed to open stats database {}: {err}",
                self.db_path.display()
            ))
        })?;
        match load_snapshot(&conn) {
            Ok(snapshot) => {
                let snapshot = Arc::new(snapshot);
                inner.cached = Some((Instant::now(), snapshot.clone()));
                Ok(snapshot)
            }
            Err(err) => {
                inner.cached = None;
                Err(FsError::io(format!("stats database query failed: {err}")))
            }
        }
    }
}

fn open_read_only(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(1))?;
    Ok(conn)
}

/// Four short autocommit SELECTs; no transaction is ever held open
/// (rollback-journal DB: long readers would block the daemon's writes).
fn load_snapshot(conn: &Connection) -> rusqlite::Result<Snapshot> {
    let mut stmt = conn.prepare(
        "SELECT video_id, title, artist, play_count, listen_secs, last_played FROM plays",
    )?;
    let tracks = stmt
        .query_map([], |row| {
            Ok(PlayRow {
                video_id: row.get(0)?,
                title: row.get(1)?,
                artist: row.get(2)?,
                play_count: row.get(3)?,
                listen_secs: row.get(4)?,
                last_played: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let by_id = tracks
        .iter()
        .enumerate()
        .map(|(index, row)| (row.video_id.clone(), index))
        .collect();

    let mut stmt =
        conn.prepare("SELECT video_id, last_recommended_at FROM listen_again_candidates")?;
    let last_recommended = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<HashMap<String, i64>>>()?;

    let mut stmt = conn.prepare("SELECT video_id, audio_path FROM offline")?;
    let offline_audio = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<HashMap<String, String>>>()?;

    let decay_window_days = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'score_decay_window_days'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(DEFAULT_SCORE_DECAY_WINDOW_DAYS);

    Ok(Snapshot {
        tracks,
        by_id,
        last_recommended,
        offline_audio,
        decay_window_days,
    })
}

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Dynamic readdir/lookup hook for `stats/tracks/`. B5: "tracks/ readdir =
/// video_id dirs from plays (all rows, incl. metadata-only)." Missing DB ->
/// `FsError::Io` (EIO). Unknown video_id lookups are rejected by tree.rs via
/// this listing (ENOENT).
pub(crate) fn stats_track_ids(ctx: &Ctx) -> FsResult<Vec<String>> {
    let snapshot = ctx.stats.snapshot()?;
    Ok(snapshot
        .tracks
        .iter()
        .map(|row| row.video_id.clone())
        .collect())
}

/// stats/overview (r). B5: "key/value record with keys, in this order:
/// listen_hours (SUM(listen_secs)/3600.0, 1 decimal); listen_secs
/// (SUM(listen_secs)); total_plays (SUM(play_count)); unique_tracks
/// (COUNT(*) WHERE play_count > 0); cached_tracks (COUNT(*) FROM offline).
/// awk: \"key\\tvalue\" lines; jq: one object."
pub(crate) fn read_overview(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    let snapshot = ctx.stats.snapshot()?;
    Ok(format::key_values(mode, &overview_pairs(&snapshot)))
}

fn overview_pairs(snapshot: &Snapshot) -> Vec<(&'static str, String)> {
    let listen_secs: i64 = snapshot.tracks.iter().map(|row| row.listen_secs).sum();
    let total_plays: i64 = snapshot.tracks.iter().map(|row| row.play_count).sum();
    let unique_tracks = snapshot
        .tracks
        .iter()
        .filter(|row| row.play_count > 0)
        .count();
    vec![
        (
            "listen_hours",
            format!("{:.1}", listen_secs as f64 / 3600.0),
        ),
        ("listen_secs", listen_secs.to_string()),
        ("total_plays", total_plays.to_string()),
        ("unique_tracks", unique_tracks.to_string()),
        ("cached_tracks", snapshot.offline_audio.len().to_string()),
    ]
}

const TOP_COLUMNS: [&str; 5] = ["title", "artist", "video_id", "play_count", "listen_secs"];

/// stats/top/today (r). B5: "list {title, artist, video_id, play_count,
/// listen_secs} of plays rows with play_count>0 AND last_played >=
/// now-86400, ORDER BY play_count DESC, listen_secs DESC, LIMIT 100."
pub(crate) fn read_top_today(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    read_top(ctx, mode, Some(86_400))
}

/// stats/top/week (r). B5: "same, window 7*86400."
pub(crate) fn read_top_week(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    read_top(ctx, mode, Some(7 * 86_400))
}

/// stats/top/month (r). B5: "same, window 30*86400."
pub(crate) fn read_top_month(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    read_top(ctx, mode, Some(30 * 86_400))
}

/// stats/top/alltime (r). B5: "same, no window." (Exact, unlike the
/// windowed approximations.)
pub(crate) fn read_top_alltime(ctx: &Ctx, mode: FsMode) -> FsResult<Vec<u8>> {
    read_top(ctx, mode, None)
}

fn read_top(ctx: &Ctx, mode: FsMode, window_secs: Option<i64>) -> FsResult<Vec<u8>> {
    let snapshot = ctx.stats.snapshot()?;
    let rows = top_rows(&snapshot, epoch_secs(), window_secs);
    Ok(format::list(mode, &TOP_COLUMNS, &rows))
}

/// Rows for a top list: plays with play_count > 0, `last_played` within the
/// window (when one is given), ordered play_count DESC then listen_secs
/// DESC, capped at [`TOP_LIMIT`].
fn top_rows(snapshot: &Snapshot, now: i64, window_secs: Option<i64>) -> Vec<Vec<String>> {
    let mut rows: Vec<&PlayRow> = snapshot
        .tracks
        .iter()
        .filter(|row| row.play_count > 0)
        .filter(|row| window_secs.is_none_or(|window| row.last_played >= now - window))
        .collect();
    rows.sort_by(|left, right| {
        right
            .play_count
            .cmp(&left.play_count)
            .then(right.listen_secs.cmp(&left.listen_secs))
    });
    rows.truncate(TOP_LIMIT);
    rows.into_iter()
        .map(|row| {
            vec![
                row.title.clone(),
                row.artist.clone(),
                row.video_id.clone(),
                row.play_count.to_string(),
                row.listen_secs.to_string(),
            ]
        })
        .collect()
}

/// stats/tracks/{video_id}/<field> (r), all scalars. B5:
/// - title, artist: scalar
/// - play_count: scalar
/// - total_listen_secs: scalar (listen_secs column)
/// - last_played: scalar unix seconds (0 → empty content)
/// - last_recommended: scalar unix seconds from listen_again_candidates
///   (no row → empty content)
/// - decay_score: scalar, 4 decimals. EXACT formula (parity with
///   history_offline.rs:132-135 / cache_files_offline.rs:86-88):
///   score = play_count / (1.0 + (max(now - last_played, 0)/86400.0) /
///   max(score_decay_window_days, 1)) where score_decay_window_days comes
///   from the app_settings table (same read-only db; default 30 when absent).
/// - cached: scalar "true"|"false": offline row exists AND audio_path exists
///   on disk (cache_files_offline.rs:19 semantics).
///
/// Scalars are always raw value + "\n" regardless of fsmod (contract B4),
/// so `mode` is intentionally unused. Unknown video_id -> ENOENT.
pub(crate) fn read_track_field(
    ctx: &Ctx,
    _mode: FsMode,
    video_id: &str,
    field: TrackField,
) -> FsResult<Vec<u8>> {
    let snapshot = ctx.stats.snapshot()?;
    let value = track_field_value(&snapshot, video_id, field, epoch_secs())?;
    Ok(format::scalar(&value))
}

fn track_field_value(
    snapshot: &Snapshot,
    video_id: &str,
    field: TrackField,
    now: i64,
) -> FsResult<String> {
    let Some(&index) = snapshot.by_id.get(video_id) else {
        return Err(FsError::NotFound);
    };
    let row = &snapshot.tracks[index];
    Ok(match field {
        TrackField::Title => row.title.clone(),
        TrackField::Artist => row.artist.clone(),
        TrackField::PlayCount => row.play_count.to_string(),
        TrackField::TotalListenSecs => row.listen_secs.to_string(),
        TrackField::LastPlayed => {
            if row.last_played == 0 {
                String::new()
            } else {
                row.last_played.to_string()
            }
        }
        TrackField::LastRecommended => snapshot
            .last_recommended
            .get(video_id)
            .map(|at| at.to_string())
            .unwrap_or_default(),
        TrackField::DecayScore => format!(
            "{:.4}",
            decay_score(
                row.play_count,
                row.last_played,
                now,
                snapshot.decay_window_days
            )
        ),
        TrackField::Cached => {
            let cached = snapshot
                .offline_audio
                .get(video_id)
                .is_some_and(|path| Path::new(path).exists());
            if cached { "true" } else { "false" }.to_string()
        }
    })
}

/// EXACT decay formula from history_offline.rs:132-135:
/// `days_since = max(now - last_played, 0) / 86400.0;
///  score = play_count / (1.0 + days_since / max(decay_window_days, 1))`.
fn decay_score(play_count: i64, last_played: i64, now: i64, decay_window_days: u32) -> f64 {
    let decay_window_days = decay_window_days.max(1) as f64;
    let days_since = ((now - last_played).max(0) as f64) / 86_400.0;
    play_count as f64 / (1.0 + (days_since / decay_window_days))
}

#[cfg(test)]
#[path = "tests/stats_files.rs"]
mod tests;
