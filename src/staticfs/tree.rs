//! The complete staticfs node model (contract B5): one variant per file and
//! directory, per-node permissions, static routing for the fixed tree,
//! dynamic-children hooks (settings keys, stats track dirs) and read/write
//! dispatch into the owning slice functions.

use super::{
    Ctx, FsError, FsResult, cartridge_files, playback_files, queue_files, settings_files,
    stats_files,
};

/// Per-track read-only files under `stats/tracks/{video_id}/` (contract B5).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TrackField {
    Title,
    Artist,
    PlayCount,
    TotalListenSecs,
    LastPlayed,
    LastRecommended,
    DecayScore,
    Cached,
}

impl TrackField {
    pub(crate) const ALL: [TrackField; 8] = [
        TrackField::Title,
        TrackField::Artist,
        TrackField::PlayCount,
        TrackField::TotalListenSecs,
        TrackField::LastPlayed,
        TrackField::LastRecommended,
        TrackField::DecayScore,
        TrackField::Cached,
    ];

    pub(crate) fn file_name(self) -> &'static str {
        match self {
            TrackField::Title => "title",
            TrackField::Artist => "artist",
            TrackField::PlayCount => "play_count",
            TrackField::TotalListenSecs => "total_listen_secs",
            TrackField::LastPlayed => "last_played",
            TrackField::LastRecommended => "last_recommended",
            TrackField::DecayScore => "decay_score",
            TrackField::Cached => "cached",
        }
    }

    fn from_file_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|field| field.file_name() == name)
    }
}

/// Every file and directory staticfs serves. Dynamic nodes (settings keys,
/// stats track dirs and their children) carry their identifying string.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Node {
    Root,
    CartridgesDir,
    Cartridge(String),

    /// Root-level `shutdown` file: read returns a liveness one-liner, writing
    /// the line "yes" asks the daemon to shut down (which takes this
    /// filesystem with it, since the daemon hosts the mount).
    Shutdown,

    // playback/ (D-Bus; every read starts from one GetState snapshot)
    PlaybackDir,
    PlaybackState,
    PlaybackTrack,
    PlaybackVideoId,
    PlaybackVideoName,
    PlaybackPosition,
    PlaybackDuration,
    PlaybackVolume,
    PlaybackRepeat,
    PlaybackNext,
    PlaybackPrev,
    PlaybackDucked,
    PlaybackDuck,
    PlaybackDesktopAwareVolume,
    PlaybackActivePeers,
    PlaybackRating,
    PlaybackLyrics,
    PlaybackSleep,

    // playback/queue/
    QueueDir,
    QueueItems,
    QueueTouchedLen,
    QueueIndex,
    QueueClear,

    // playback/radio/
    RadioDir,
    RadioSeed,
    RadioJump,
    RadioChips,

    // settings/ (D-Bus; keys are dynamic, from GetSettings)
    SettingsDir,
    SettingsFsmod,
    SettingsKey(String),

    // stats/ (read-only SQLite)
    StatsDir,
    StatsOverview,
    StatsTopDir,
    StatsTopToday,
    StatsTopWeek,
    StatsTopMonth,
    StatsTopAlltime,
    StatsTracksDir,
    StatsTrackDir(String),
    StatsTrackField(String, TrackField),
}

impl Node {
    pub(crate) fn is_dir(&self) -> bool {
        matches!(
            self,
            Node::Root
                | Node::CartridgesDir
                | Node::PlaybackDir
                | Node::QueueDir
                | Node::RadioDir
                | Node::SettingsDir
                | Node::StatsDir
                | Node::StatsTopDir
                | Node::StatsTracksDir
                | Node::StatsTrackDir(_)
        )
    }

    /// Contract B3 permissions: read-write 0644, read-only 0444,
    /// write-only 0200, directories 0755. Cartridge files support unlink only.
    pub(crate) fn perm(&self) -> u16 {
        match self {
            node if node.is_dir() => 0o755,
            // read-write
            Node::Shutdown
            | Node::PlaybackState
            | Node::PlaybackTrack
            | Node::PlaybackVideoId
            | Node::PlaybackVideoName
            | Node::PlaybackPosition
            | Node::PlaybackVolume
            | Node::PlaybackRepeat
            | Node::PlaybackNext
            | Node::PlaybackPrev
            | Node::PlaybackRating
            | Node::PlaybackSleep
            | Node::QueueItems
            | Node::RadioChips
            | Node::SettingsFsmod
            | Node::SettingsKey(_) => 0o644,
            // write-only
            Node::QueueClear | Node::RadioJump => 0o200,
            // read-only
            _ => 0o444,
        }
    }

    pub(crate) fn readable(&self) -> bool {
        self.perm() & 0o400 != 0
    }

    pub(crate) fn writable(&self) -> bool {
        self.perm() & 0o200 != 0
    }

    /// Trigger files: ANY write (even empty/newline-only) fires the action
    /// once per open handle (contract B3). `radio/jump` is write-only too,
    /// but line-based, so it is NOT a trigger. `playback/next` left this set
    /// when it grew line semantics (queries insert addNext-style); its empty
    /// write skips via `empty_write_command` instead.
    pub(crate) fn is_trigger(&self) -> bool {
        matches!(self, Node::PlaybackPrev | Node::QueueClear)
    }

    /// The command an ENTIRELY empty (after trim) write stands for, if any.
    /// Blank lines inside a write are plain no-ops (contract B3); only when
    /// a written handle never carries any non-whitespace content does
    /// playback/state fire one TogglePause (contract B5's "\"toggle\" or
    /// empty write"), so `echo > playback/state` toggles exactly once while
    /// `printf 'pause\n\n' > playback/state` sends just the Pause.
    /// Fired once per handle at flush/release by `fs.rs`.
    pub(crate) fn empty_write_command(&self) -> Option<&'static str> {
        match self {
            Node::PlaybackState => Some("toggle"),
            // `echo > next` (or a bare "skip" line) skips forward; any other
            // line is a query queued at the addNext slot.
            Node::PlaybackNext => Some("skip"),
            _ => None,
        }
    }

    /// Mount-relative path for human-facing messages (the host's pending
    /// unmount prompt). Static nodes find their name in the parent's child
    /// table; dynamic nodes carry it.
    pub(crate) fn display_path(&self) -> String {
        if matches!(self, Node::Root) {
            return String::new();
        }
        let name = match self {
            Node::Cartridge(name) => name.clone(),
            Node::SettingsKey(key) => key.clone(),
            Node::StatsTrackDir(video_id) => video_id.clone(),
            Node::StatsTrackField(_, field) => field.file_name().to_string(),
            other => other
                .parent()
                .static_children()
                .into_iter()
                .find(|(_, node)| node == other)
                .map(|(name, _)| name.to_string())
                .unwrap_or_default(),
        };
        let parent = self.parent().display_path();
        if parent.is_empty() {
            name
        } else {
            format!("{parent}/{name}")
        }
    }

    /// Parent directory (for `..` readdir entries). `Root`'s parent is itself.
    pub(crate) fn parent(&self) -> Node {
        match self {
            Node::Root
            | Node::CartridgesDir
            | Node::Shutdown
            | Node::PlaybackDir
            | Node::SettingsDir
            | Node::StatsDir => Node::Root,
            Node::Cartridge(_) => Node::CartridgesDir,
            Node::PlaybackState
            | Node::PlaybackTrack
            | Node::PlaybackVideoId
            | Node::PlaybackVideoName
            | Node::PlaybackPosition
            | Node::PlaybackDuration
            | Node::PlaybackVolume
            | Node::PlaybackRepeat
            | Node::PlaybackNext
            | Node::PlaybackPrev
            | Node::PlaybackDucked
            | Node::PlaybackDuck
            | Node::PlaybackDesktopAwareVolume
            | Node::PlaybackActivePeers
            | Node::PlaybackRating
            | Node::PlaybackLyrics
            | Node::PlaybackSleep
            | Node::QueueDir
            | Node::RadioDir => Node::PlaybackDir,
            Node::QueueItems | Node::QueueTouchedLen | Node::QueueIndex | Node::QueueClear => {
                Node::QueueDir
            }
            Node::RadioSeed | Node::RadioJump | Node::RadioChips => Node::RadioDir,
            Node::SettingsFsmod | Node::SettingsKey(_) => Node::SettingsDir,
            Node::StatsOverview | Node::StatsTopDir | Node::StatsTracksDir => Node::StatsDir,
            Node::StatsTopToday
            | Node::StatsTopWeek
            | Node::StatsTopMonth
            | Node::StatsTopAlltime => Node::StatsTopDir,
            Node::StatsTrackDir(_) => Node::StatsTracksDir,
            Node::StatsTrackField(video_id, _) => Node::StatsTrackDir(video_id.clone()),
        }
    }

    /// The fixed (backend-independent) children of this directory; used both
    /// to preallocate the static inode table at mount and as the static part
    /// of `children`/`lookup_child`. Dynamic directories contribute their
    /// fixed entries only (e.g. `settings/fsmod`).
    pub(crate) fn static_children(&self) -> Vec<(&'static str, Node)> {
        match self {
            Node::Root => vec![
                ("cartridges", Node::CartridgesDir),
                ("playback", Node::PlaybackDir),
                ("settings", Node::SettingsDir),
                ("stats", Node::StatsDir),
                ("shutdown", Node::Shutdown),
            ],
            Node::PlaybackDir => vec![
                ("state", Node::PlaybackState),
                ("track", Node::PlaybackTrack),
                ("video_id", Node::PlaybackVideoId),
                ("video_name", Node::PlaybackVideoName),
                ("position", Node::PlaybackPosition),
                ("duration", Node::PlaybackDuration),
                ("volume", Node::PlaybackVolume),
                ("repeat", Node::PlaybackRepeat),
                ("next", Node::PlaybackNext),
                ("prev", Node::PlaybackPrev),
                ("ducked", Node::PlaybackDucked),
                ("duck", Node::PlaybackDuck),
                ("desktop_aware_volume", Node::PlaybackDesktopAwareVolume),
                ("active_peers", Node::PlaybackActivePeers),
                ("rating", Node::PlaybackRating),
                ("lyrics", Node::PlaybackLyrics),
                ("sleep", Node::PlaybackSleep),
                ("queue", Node::QueueDir),
                ("radio", Node::RadioDir),
            ],
            Node::QueueDir => vec![
                ("items", Node::QueueItems),
                ("touched_len", Node::QueueTouchedLen),
                ("index", Node::QueueIndex),
                ("clear", Node::QueueClear),
            ],
            Node::RadioDir => vec![
                ("seed", Node::RadioSeed),
                ("jump", Node::RadioJump),
                ("chips", Node::RadioChips),
            ],
            Node::SettingsDir => vec![("fsmod", Node::SettingsFsmod)],
            Node::StatsDir => vec![
                ("overview", Node::StatsOverview),
                ("top", Node::StatsTopDir),
                ("tracks", Node::StatsTracksDir),
            ],
            Node::StatsTopDir => vec![
                ("today", Node::StatsTopToday),
                ("week", Node::StatsTopWeek),
                ("month", Node::StatsTopMonth),
                ("alltime", Node::StatsTopAlltime),
            ],
            _ => Vec::new(),
        }
    }

    /// Full directory listing for readdir: static children plus dynamic ones.
    /// - `settings/` readdir = keys from GetSettings (daemon order) + "fsmod"
    ///   (daemon down -> EIO).
    /// - `stats/tracks/` readdir = video_id dirs from the plays table
    ///   (all rows, incl. metadata-only).
    pub(crate) fn children(&self, ctx: &Ctx) -> FsResult<Vec<(String, Node)>> {
        match self {
            Node::CartridgesDir => Ok(cartridge_files::names(ctx)?
                .into_iter()
                .map(|name| (name.clone(), Node::Cartridge(name)))
                .collect()),
            Node::SettingsDir => Ok(settings_dir_entries(settings_files::settings_keys(ctx)?)),
            Node::StatsTracksDir => Ok(stats_files::stats_track_ids(ctx)?
                .into_iter()
                .map(|video_id| (video_id.clone(), Node::StatsTrackDir(video_id)))
                .collect()),
            Node::StatsTrackDir(video_id) => Ok(TrackField::ALL
                .into_iter()
                .map(|field| {
                    (
                        field.file_name().to_string(),
                        Node::StatsTrackField(video_id.clone(), field),
                    )
                })
                .collect()),
            _ => Ok(self
                .static_children()
                .into_iter()
                .map(|(name, node)| (name.to_string(), node))
                .collect()),
        }
    }

    /// Resolve one child by name (lookup). Unknown names -> `FsError::NotFound`
    /// (ENOENT); dynamic parents consult their listing hooks so an unknown
    /// settings key or video_id dir is ENOENT (contract B5).
    pub(crate) fn lookup_child(&self, ctx: &Ctx, name: &str) -> FsResult<Node> {
        if let Some((_, node)) = self
            .static_children()
            .into_iter()
            .find(|(child_name, _)| *child_name == name)
        {
            return Ok(node);
        }
        match self {
            Node::CartridgesDir => {
                cartridge_files::metadata(ctx, name)?;
                Ok(Node::Cartridge(name.to_owned()))
            }
            Node::SettingsDir => {
                if settings_files::settings_keys(ctx)?
                    .iter()
                    .any(|key| key == name)
                {
                    Ok(Node::SettingsKey(name.to_string()))
                } else {
                    Err(FsError::NotFound)
                }
            }
            Node::StatsTracksDir => {
                if stats_files::stats_track_ids(ctx)?
                    .iter()
                    .any(|video_id| video_id == name)
                {
                    Ok(Node::StatsTrackDir(name.to_string()))
                } else {
                    Err(FsError::NotFound)
                }
            }
            Node::StatsTrackDir(video_id) => match TrackField::from_file_name(name) {
                Some(field) => Ok(Node::StatsTrackField(video_id.clone(), field)),
                None => Err(FsError::NotFound),
            },
            _ => Err(FsError::NotFound),
        }
    }

    /// Read a requested byte range. Large binary cartridges never materialize
    /// their full audio payload; text files retain their live snapshot semantics.
    pub(crate) fn read_range(&self, ctx: &Ctx, offset: u64, size: u32) -> FsResult<Vec<u8>> {
        if let Node::Cartridge(name) = self {
            return cartridge_files::read_range(ctx, name, offset, size);
        }
        let bytes = self.read(ctx)?;
        if offset >= bytes.len() as u64 {
            return Ok(Vec::new());
        }
        let start = offset as usize;
        let end = bytes.len().min(start.saturating_add(size as usize));
        Ok(bytes[start..end].to_vec())
    }

    /// Read dispatch: regenerate this file's full current content. `fs.rs`
    /// slices the returned bytes for the requested offset/size and uses the
    /// length for getattr.
    pub(crate) fn read(&self, ctx: &Ctx) -> FsResult<Vec<u8>> {
        let mode = ctx.fsmod();
        match self {
            Node::Shutdown => playback_files::read_shutdown(ctx, mode),
            Node::PlaybackState => playback_files::read_state(ctx, mode),
            Node::PlaybackTrack => playback_files::read_track(ctx, mode),
            Node::PlaybackVideoId => playback_files::read_video_id(ctx, mode),
            Node::PlaybackVideoName => playback_files::read_video_name(ctx, mode),
            Node::PlaybackPosition => playback_files::read_position(ctx, mode),
            Node::PlaybackDuration => playback_files::read_duration(ctx, mode),
            Node::PlaybackVolume => playback_files::read_volume(ctx, mode),
            Node::PlaybackRepeat => playback_files::read_repeat(ctx, mode),
            Node::PlaybackNext => playback_files::read_next(ctx, mode),
            Node::PlaybackPrev => playback_files::read_prev(ctx, mode),
            Node::PlaybackDucked => playback_files::read_ducked(ctx, mode),
            Node::PlaybackDuck => playback_files::read_duck(ctx, mode),
            Node::PlaybackDesktopAwareVolume => {
                playback_files::read_desktop_aware_volume(ctx, mode)
            }
            Node::PlaybackActivePeers => playback_files::read_active_peers(ctx, mode),
            Node::PlaybackRating => playback_files::read_rating(ctx, mode),
            Node::PlaybackLyrics => playback_files::read_lyrics(ctx, mode),
            Node::PlaybackSleep => playback_files::read_sleep(ctx, mode),
            Node::QueueItems => queue_files::read_queue_items(ctx, mode),
            Node::QueueTouchedLen => queue_files::read_touched_len(ctx, mode),
            Node::QueueIndex => queue_files::read_queue_index(ctx, mode),
            Node::RadioSeed => queue_files::read_radio_seed(ctx, mode),
            Node::RadioChips => queue_files::read_radio_chips(ctx, mode),
            Node::SettingsFsmod => settings_files::read_fsmod(ctx, mode),
            Node::SettingsKey(key) => settings_files::read_setting(ctx, mode, key),
            Node::StatsOverview => stats_files::read_overview(ctx, mode),
            Node::StatsTopToday => stats_files::read_top_today(ctx, mode),
            Node::StatsTopWeek => stats_files::read_top_week(ctx, mode),
            Node::StatsTopMonth => stats_files::read_top_month(ctx, mode),
            Node::StatsTopAlltime => stats_files::read_top_alltime(ctx, mode),
            Node::StatsTrackField(video_id, field) => {
                stats_files::read_track_field(ctx, mode, video_id, *field)
            }
            _ => Err(FsError::invalid("this file is not readable")),
        }
    }

    /// Write dispatch: process ONE complete, non-empty, already-trimmed line
    /// as a command (contract B3 line buffering lives in `fs.rs`).
    pub(crate) fn write_line(&self, ctx: &Ctx, line: &str) -> FsResult<()> {
        match self {
            Node::Shutdown => playback_files::write_shutdown(ctx, line),
            Node::PlaybackState => playback_files::write_state(ctx, line),
            Node::PlaybackTrack | Node::PlaybackVideoId => {
                playback_files::write_play_query(ctx, line)
            }
            Node::PlaybackVideoName => playback_files::write_video_name(ctx, line),
            Node::PlaybackNext => playback_files::write_next(ctx, line),
            Node::PlaybackPosition => playback_files::write_position(ctx, line),
            Node::PlaybackVolume => playback_files::write_volume(ctx, line),
            Node::PlaybackRepeat => playback_files::write_repeat(ctx, line),
            Node::PlaybackRating => playback_files::write_rating(ctx, line),
            Node::PlaybackSleep => playback_files::write_sleep(ctx, line),
            Node::QueueItems => queue_files::write_queue_items(ctx, line),
            Node::RadioJump => queue_files::write_radio_jump(ctx, line),
            Node::RadioChips => queue_files::write_radio_chips(ctx, line),
            Node::SettingsFsmod => settings_files::write_fsmod(ctx, line),
            Node::SettingsKey(key) => settings_files::write_setting(ctx, key, line),
            _ => Err(FsError::invalid("this file is not writable")),
        }
    }

    /// Trigger dispatch for write-only trigger files (fired once per open
    /// handle by `fs.rs`, regardless of the written bytes).
    pub(crate) fn trigger(&self, ctx: &Ctx) -> FsResult<()> {
        match self {
            Node::PlaybackPrev => playback_files::trigger_prev(ctx),
            Node::QueueClear => queue_files::trigger_queue_clear(ctx),
            _ => Err(FsError::invalid("this file is not a trigger")),
        }
    }
}

/// `settings/` readdir entries: daemon keys in daemon order, then the local
/// "fsmod" node. A daemon setting literally named "fsmod" is shadowed by the
/// local node (matching `lookup_child`, where the static child wins) instead
/// of listing twice.
fn settings_dir_entries(keys: Vec<String>) -> Vec<(String, Node)> {
    let mut entries: Vec<(String, Node)> = keys
        .into_iter()
        .filter(|key| key != "fsmod")
        .map(|key| (key.clone(), Node::SettingsKey(key)))
        .collect();
    entries.push(("fsmod".to_string(), Node::SettingsFsmod));
    entries
}

#[cfg(test)]
#[path = "tests/tree.rs"]
mod tests;
