use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Once;

use anyhow::{Context, Result, bail};

use crate::ipc::{
    CoordinationTargetSnapshot, CoordinationTargetsSnapshot, DaemonClient, PersistedPlaybackState,
    PlaybackSnapshot, PlaylistContext, PlaylistContextSource, RepeatMode, SleepStatusSnapshot,
    daemon_is_running, load_persisted_playback_state,
};
use crate::models::{
    Artist, CrossDeviceQueue, HomeItem, ListenAgainTarget, PlaylistEditor, SearchResult, Track,
};
use crate::network::{
    SPONSORBLOCK_CATEGORY_OPTIONS, SongRating, delete_cookie, download_media_bytes,
    extract_video_id_from_input, fetch_thumbnail_bytes, get_cookie, get_storage_root,
    load_cookies_from_chromium_profiles, sanitize_categories, store_cookie,
};
use crate::network_state::{NetworkState, NetworkValidationOutcome};
use crate::offline::{
    AppSettings, CoordinationFilterMode, CoordinationMode, DESKTOP_AWARE_VOLUME_HOLD_UI_MAX_SECS,
    DESKTOP_AWARE_VOLUME_HOLD_UI_MIN_SECS, DESKTOP_AWARE_VOLUME_RESTORE_UI_MAX_SECS,
    DESKTOP_AWARE_VOLUME_RESTORE_UI_MIN_SECS, MusicCache, OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS,
    OTHER_MPRIS_HALT_AUTO_RESUME_UI_MIN_SECS, PIPEWIRE_DUCKING_THRESHOLD_UI_MAX_SECS,
    PIPEWIRE_DUCKING_THRESHOLD_UI_MIN_SECS, PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
    PIPEWIRE_DUCKING_VOLUME_UI_MAX, PIPEWIRE_DUCKING_VOLUME_UI_MIN,
    PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MAX_SECS, PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MIN_SECS,
    coordination_filter_contains, sanitize_coordination_filter_entries,
};

static HEADLESS_SESSION_BOOTSTRAP: Once = Once::new();
const HELP_USAGE: &str = "\
Static CLI

Usage:
  static help
  static like
  static dislike
  static neutral
  static pin [ytm/yt url | video id | search term]
  static unpin [ytm/yt url | video id | search term]
  static resume
  static sleep [minutes|after|song|off]
  static share
  static exec <ytm/yt url | video id | search term> [probe=y|n]
  static queue get
  static queue addNext <ytm/yt url | video id | search term>
  static queue addLast <ytm/yt url | video id | search term>
  static playlist list
  static playlist show <playlist id>
  static playlist create <title>
  static playlist delete <playlist id> [--yes]
  static playlist pin <playlist id>
  static playlist unpin <playlist id>
  static playlist from-radio <title>
  static playlist add <playlist id> <ytm/yt url | video id | search term>
  static playlist remove <playlist id> <position> [position ...]
  static playlist move <playlist id> <from position> <to position>
  static offline list
  static offline add <ytm/yt url | video id | search term>
  static offline remove <ytm/yt url | video id>
  static offline pin <ytm/yt url | video id>
  static offline unpin <ytm/yt url | video id>
  static offline play [ytm/yt url | video id]
  static info
  static visualizer
  static visualizer decode <cartridge.png|jpg|webp>
  static coordination list
  static settings dGain calibrate
  static config list
  static config desc [config_name]
  static config get [config_name]
  static config set <config_name> <config_value>
  static i";

const HELP_NOTES: &str = "\
Notes:
  static without arguments still opens the GTK app.
  static i opens the terminal-native TUI on the alternate screen.
  Playback commands talk to staticd over D-Bus and auto-start it when needed.
  Some config keys intentionally accept a wider CLI range than the GTK settings UI.
  That is a feature, not a bug: when a saved value sits outside the UI-safe range,
  the GTK settings page disables that control and treats it as CLI-only until it is
  brought back into range.
  Playlist positions are 1-based, including duplicate song occurrences.
  Pin/unpin update YouTube Music Listen again; no song input uses the current song.
  Playlist deletion is permanent and requires --yes after the playlist ID.
  New playlists are private. `from-radio` saves the live radio queue in order.
  Run `static config desc` for detailed behavior, accepted values, and range notes.";

fn help_text() -> String {
    format!("{HELP_USAGE}\n\n{HELP_NOTES}")
}

pub fn wants_cli(args: &[String]) -> bool {
    !args.is_empty() && args.first().is_some_and(|arg| arg != "--daemon")
}

pub fn run(args: &[String]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(());
    };

    match command {
        "help" | "-h" | "--help" => {
            println!("{}", help_text());
            Ok(())
        }
        "like" => run_rate_current_track(SongRating::Like),
        "dislike" => run_rate_current_track(SongRating::Dislike),
        "neutral" => run_rate_current_track(SongRating::Indifferent),
        "pin" => run_listen_again_pin(&args[1..], true),
        "unpin" => run_listen_again_pin(&args[1..], false),
        "resume" => run_resume(),
        "sleep" => run_sleep(&args[1..]),
        "share" => run_share(),
        "exec" => run_exec(&args[1..]),
        "queue" => run_queue(&args[1..]),
        "playlist" => run_playlist(&args[1..]),
        "offline" => run_offline(&args[1..]),
        "info" => run_info(),
        "coordination" => run_coordination(&args[1..]),
        "config" => run_config(&args[1..]),
        "settings"
            if args.len() == 3
                && args[1].eq_ignore_ascii_case("dgain")
                && args[2].eq_ignore_ascii_case("calibrate") =>
        {
            crate::visualizer::calibrate()
        }
        "settings" => bail!("Usage: static settings dGain calibrate"),
        "i" => run_interactive_tui(),
        other => bail!("Unknown CLI command `{other}`.\n\n{}", help_text()),
    }
}

fn run_rate_current_track(rating: SongRating) -> Result<()> {
    let client = DaemonClient::new()?;
    let snapshot = client.rate_current_track(rating)?;
    let track = snapshot
        .current_track
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No song is currently playing"))?;
    println!(
        "{}: {} - {}",
        rating_label(rating),
        track.title,
        track.artist
    );
    Ok(())
}

fn run_listen_again_pin(args: &[String], pinned: bool) -> Result<()> {
    let network = saved_network_state()?;
    let (video_id, title) = if args.is_empty() {
        let track = DaemonClient::connect_existing()?
            .state()?
            .current_track
            .ok_or_else(|| anyhow::anyhow!("No song is currently playing"))?;
        (track.video_id, track.title)
    } else {
        let input = join_required_input(args, "Listen again song")?;
        let track = resolve_track_input(&network, &input, true)?;
        (track.video_id, track.title)
    };
    network.set_listen_again_pin(&ListenAgainTarget::Track(video_id), pinned)?;
    println!(
        "{} {title} {} YouTube Music Listen again",
        if pinned { "Pinned" } else { "Unpinned" },
        if pinned { "in" } else { "from" }
    );
    Ok(())
}

fn run_resume() -> Result<()> {
    let snapshot = load_local_resume_snapshot()?
        .ok_or_else(|| anyhow::anyhow!("No saved playback state is available"))?;
    let restored = restore_playback_snapshot_via_daemon(snapshot, true)?;
    println!("Resumed {}", current_track_label(&restored));
    Ok(())
}

fn run_sleep(args: &[String]) -> Result<()> {
    let client = DaemonClient::new()?;
    match args.first().map(String::as_str) {
        None => {
            println!("sleep: {}", format_sleep_status(&client.sleep_status()?));
        }
        Some("off") | Some("clear") => {
            client.clear_sleep_timer()?;
            println!("sleep: off");
        }
        Some("after") | Some("song") => {
            client.set_sleep_after_current()?;
            println!("sleep: after current song");
        }
        Some(raw_minutes) => {
            let minutes = raw_minutes
                .parse::<u32>()
                .with_context(|| format!("Invalid sleep timer value `{raw_minutes}`"))?;
            if minutes == 0 {
                bail!("Sleep timer minutes must be greater than zero");
            }
            client.set_sleep_timer(minutes)?;
            let unit = if minutes == 1 { "minute" } else { "minutes" };
            println!("sleep: {minutes} {unit}");
        }
    }

    Ok(())
}

fn run_share() -> Result<()> {
    let client = DaemonClient::new()?;
    let url = client.copy_share_url()?;
    println!("{url}");
    if let Err(err) = copy_text_to_clipboard(&url) {
        eprintln!("copy failed: {err}");
    }
    Ok(())
}

fn format_sleep_status(status: &SleepStatusSnapshot) -> String {
    if status.after_current {
        "after current song".to_string()
    } else if let Some(remaining_secs) = status.remaining_secs {
        format!("{} remaining", format_duration_secs(remaining_secs))
    } else {
        "off".to_string()
    }
}

fn format_sleep_status_from_snapshot(snapshot: &PlaybackSnapshot) -> String {
    format_sleep_status(&SleepStatusSnapshot {
        remaining_secs: snapshot.sleep_remaining_secs,
        after_current: snapshot.sleep_after_current,
    })
}

pub(crate) fn copy_text_to_clipboard(text: &str) -> Result<()> {
    match copy_text_with_command("wl-copy", &[], text) {
        Ok(()) => Ok(()),
        Err(wl_err) => match copy_text_with_command("xclip", &["-selection", "clipboard"], text) {
            Ok(()) => Ok(()),
            Err(xclip_err) => bail!("wl-copy failed ({wl_err}); xclip failed ({xclip_err})"),
        },
    }
}

fn copy_text_with_command(program: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .with_context(|| format!("failed to write to {program}"))?;
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait for {program}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} exited with {}{}",
            program,
            output.status,
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        )
    }
}

fn run_exec(args: &[String]) -> Result<()> {
    let (input, probe) = parse_exec_input(args)?;
    let network = saved_network_state()?;
    let track = resolve_track_input(&network, &input, true)?;
    let client = DaemonClient::new()?;

    if probe {
        client.probe_track(track.clone())?;
        println!("Probing {}", format_track_summary(&track));
        return Ok(());
    }

    let snapshot = play_track_with_radio(&client, track.clone())?;

    if snapshot.queue.len() > 1 {
        println!("Playing {} with radio", format_track_summary(&track));
    } else {
        println!("Playing {}", format_track_summary(&track));
    }

    Ok(())
}

pub(crate) fn play_track_with_radio(
    client: &DaemonClient,
    track: Track,
) -> Result<PlaybackSnapshot> {
    client.start_radio_from_seed(vec![track], 0, 0, 1, None, RepeatMode::Off, true)
}

fn run_queue(args: &[String]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        bail!("`static queue` expects `get`, `addNext`, or `addLast`");
    };

    match command {
        "get" => {
            let snapshot = DaemonClient::new()?.state()?;
            print_queue(&snapshot);
            Ok(())
        }
        "addNext" => run_queue_insert(QueueInsertMode::Next, &args[1..]),
        "addLast" => run_queue_insert(QueueInsertMode::Last, &args[1..]),
        other => bail!("Unknown queue command `{other}`"),
    }
}

fn run_queue_insert(mode: QueueInsertMode, args: &[String]) -> Result<()> {
    let input = join_required_input(args, "queue addition")?;
    let network = saved_network_state()?;
    let track = resolve_track_input(&network, &input, true)?;
    let client = DaemonClient::new()?;
    let snapshot = client.state()?;
    let updated = apply_queue_insert(&client, snapshot, track.clone(), mode)?;
    println!(
        "{}: {}",
        match mode {
            QueueInsertMode::Next => "Queued next",
            QueueInsertMode::Last => "Queued last",
        },
        format_track_summary(&track)
    );
    if updated.queue.is_empty() {
        println!("Queue is still empty.");
    }
    Ok(())
}

fn playlist_position(raw: &str, len: usize) -> Result<usize> {
    let position = raw
        .parse::<usize>()
        .with_context(|| format!("Invalid playlist position `{raw}`"))?;
    if position == 0 || position > len {
        bail!("Playlist position must be between 1 and {len}; got {position}");
    }
    Ok(position - 1)
}

// The remote operation inserts before a successor. Resolve that successor after
// removing the moving occurrence so downward moves land at the requested index.
fn playlist_move_successor(len: usize, from: usize, to: usize) -> Option<usize> {
    (0..len).filter(|index| *index != from).nth(to)
}

fn require_editable_playlist(editor: &PlaylistEditor) -> Result<()> {
    if !editor.editable {
        bail!("Playlist `{}` is read-only", editor.title);
    }
    Ok(())
}

fn playlist_delete_confirmed(args: &[String]) -> Result<bool> {
    match args {
        [id] if !id.trim().is_empty() && !id.starts_with('-') => Ok(false),
        [id, flag] if !id.trim().is_empty() && !id.starts_with('-') && flag == "--yes" => Ok(true),
        _ => bail!("Usage: static playlist delete <playlist id> [--yes]"),
    }
}

fn run_playlist(args: &[String]) -> Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("list");
    match command {
        "list" if args.len() <= 1 => {
            for playlist in saved_network_state()?.get_editable_playlists()? {
                println!("{}  {}", playlist.playlist_id, playlist.title);
            }
        }
        "delete" => {
            let confirmed = playlist_delete_confirmed(&args[1..])?;
            let network = saved_network_state()?;
            let editor = network.get_playlist_editor(&args[1])?;
            require_editable_playlist(&editor)?;
            if !confirmed {
                println!(
                    "Permanently delete playlist \"{}\" ({}) and all its playlist entries?",
                    editor.title, editor.playlist_id
                );
                println!(
                    "This cannot be undone. To confirm, run: static playlist delete {} --yes",
                    editor.playlist_id
                );
                return Ok(());
            }
            network.delete_playlist(&editor.playlist_id)?;
            println!(
                "Deleted playlist: {} ({})",
                editor.title, editor.playlist_id
            );
        }
        "pin" | "unpin" if args.len() == 2 => {
            let pinned = command == "pin";
            saved_network_state()?
                .set_listen_again_pin(&ListenAgainTarget::Playlist(args[1].clone()), pinned)?;
            println!(
                "{} playlist {} {} YouTube Music Listen again",
                if pinned { "Pinned" } else { "Unpinned" },
                args[1],
                if pinned { "in" } else { "from" }
            );
        }
        "show" if args.len() == 2 => {
            let editor = saved_network_state()?.get_playlist_editor(&args[1])?;
            println!(
                "{} ({}){}",
                editor.title,
                editor.playlist_id,
                if editor.editable { "" } else { " [read-only]" }
            );
            for (index, entry) in editor.entries.iter().enumerate() {
                println!("{}. {}", index + 1, format_track_summary(&entry.track));
            }
        }
        "create" | "from-radio" => {
            let title = join_required_input(&args[1..], "playlist title")?;
            let video_ids = if command == "from-radio" {
                let snapshot = DaemonClient::connect_existing()?.state()?;
                if snapshot.radio_virtual_playlist.is_none() || snapshot.queue.is_empty() {
                    bail!("Start a radio before creating a playlist from it");
                }
                snapshot
                    .queue
                    .into_iter()
                    .map(|track| track.video_id)
                    .collect()
            } else {
                Vec::new()
            };
            let id = saved_network_state()?.create_playlist(&title, &video_ids)?;
            println!("Created private playlist: {title} ({id})");
        }
        "add" if args.len() >= 3 => {
            let input = join_required_input(&args[2..], "playlist song")?;
            let network = saved_network_state()?;
            let editor = network.get_playlist_editor(&args[1])?;
            require_editable_playlist(&editor)?;
            let track = resolve_track_input(&network, &input, true)?;
            network.add_playlist_tracks(&editor.playlist_id, &[track.video_id.clone()])?;
            println!("Added {} to {}", format_track_summary(&track), editor.title);
        }
        "remove" if args.len() >= 3 => {
            let network = saved_network_state()?;
            let editor = network.get_playlist_editor(&args[1])?;
            require_editable_playlist(&editor)?;
            let mut seen = HashSet::new();
            let mut entries = Vec::new();
            for raw in &args[2..] {
                let index = playlist_position(raw, editor.entries.len())?;
                let entry = &editor.entries[index];
                if entry.set_video_id.as_deref().is_none_or(str::is_empty) {
                    bail!(
                        "Playlist position {} has no editable occurrence ID",
                        index + 1
                    );
                }
                if seen.insert(index) {
                    entries.push(entry.clone());
                }
            }
            network.remove_playlist_entries(&editor.playlist_id, &entries)?;
            println!("Removed {} song(s) from {}", entries.len(), editor.title);
        }
        "move" if args.len() == 4 => {
            let network = saved_network_state()?;
            let editor = network.get_playlist_editor(&args[1])?;
            require_editable_playlist(&editor)?;
            let from = playlist_position(&args[2], editor.entries.len())?;
            let to = playlist_position(&args[3], editor.entries.len())?;
            if from == to {
                println!("Song is already at position {}", to + 1);
                return Ok(());
            }
            let occurrence_id = |index: usize| -> Result<&str> {
                editor.entries[index]
                    .set_video_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Playlist position {} has no editable occurrence ID",
                            index + 1
                        )
                    })
            };
            let moving = occurrence_id(from)?;
            let before = playlist_move_successor(editor.entries.len(), from, to)
                .map(occurrence_id)
                .transpose()?;
            network.move_playlist_entry(&editor.playlist_id, moving, before)?;
            println!(
                "Moved position {} to {} in {}",
                from + 1,
                to + 1,
                editor.title
            );
        }
        _ => bail!("Invalid playlist command.\n\n{}", help_text()),
    }
    Ok(())
}

fn run_offline(args: &[String]) -> Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("list");

    match command {
        "list" => {
            let cache = open_music_cache()?;
            print_offline_library(&cache)?;
            Ok(())
        }
        "add" | "download" => run_offline_add(&args[1..]),
        "remove" | "delete" | "rm" => run_offline_remove(&args[1..]),
        "pin" => run_offline_pin(&args[1..], true),
        "unpin" => run_offline_pin(&args[1..], false),
        "play" => run_offline_play(&args[1..]),
        other => bail!("Unknown offline command `{other}`"),
    }
}

fn run_offline_add(args: &[String]) -> Result<()> {
    let input = join_required_input(args, "offline track")?;
    let network = saved_network_state()?;
    let track = resolve_track_input(&network, &input, true)?;
    let cache = open_music_cache()?;
    let result = ensure_cli_track_available_offline(&cache, &network, &track, true)
        .with_context(|| format!("Failed to cache {}", format_track_summary(&track)))?;
    notify_daemon_settings_if_running();

    if result.already_cached {
        println!("Already offline: {}", format_track_summary(&track));
    } else {
        println!(
            "Downloaded offline: {} ({} KB)",
            format_track_summary(&track),
            result.bytes / 1024
        );
    }
    if result.video_id != track.video_id {
        println!("canonical_video_id: {}", result.video_id);
    }
    Ok(())
}

fn run_offline_remove(args: &[String]) -> Result<()> {
    let video_id = parse_video_id_argument(args, "offline removal")?;
    let cache = open_music_cache()?;
    if !cache.is_cached(&video_id) {
        bail!("No offline download exists for `{video_id}`");
    }
    cache.remove_cached(&video_id)?;
    notify_daemon_settings_if_running();
    println!("Removed offline: {video_id}");
    Ok(())
}

fn run_offline_pin(args: &[String], pinned: bool) -> Result<()> {
    let video_id = parse_video_id_argument(args, "offline pin")?;
    let cache = open_music_cache()?;
    if !cache.is_cached(&video_id) {
        bail!(
            "No offline download exists for `{video_id}`. Use `static offline add {video_id}` first."
        );
    }

    if pinned {
        cache.pin_offline(&video_id)?;
        println!("Pinned offline: {video_id}");
    } else {
        cache.unpin_offline(&video_id)?;
        println!("Unpinned offline: {video_id}");
    }
    notify_daemon_settings_if_running();
    Ok(())
}

fn run_offline_play(args: &[String]) -> Result<()> {
    let cache = open_music_cache()?;
    let settings = cache.load_settings()?;
    let queue = if args.is_empty() {
        offline_tracks_as_queue(&cache)?
    } else {
        let video_id = parse_video_id_argument(args, "offline playback")?;
        vec![offline_track_by_id(&cache, &video_id)?]
    };

    if queue.is_empty() {
        bail!("No offline songs are cached yet");
    }
    if !settings.prefer_local_cache_on_playback {
        eprintln!(
            "warning: prefer_local_cache_on_playback=no, so staticd may stream instead of using cached audio"
        );
    }

    let context = PlaylistContext {
        playlist_id: "cli-offline".to_string(),
        title: "Offline".to_string(),
        thumbnail_url: queue.first().and_then(|track| track.thumbnail_url.clone()),
        subtitle: Some(format!("{} cached songs", queue.len())),
        source: PlaylistContextSource::Offline,
    };
    let snapshot = DaemonClient::new()?.play_queue_at(
        queue.clone(),
        0,
        Some(context),
        None,
        RepeatMode::Off,
    )?;
    println!("Playing offline {}", current_track_label(&snapshot));
    Ok(())
}

fn print_offline_library(cache: &MusicCache) -> Result<()> {
    let playlists = cache.offline_playlists()?;
    let tracks = cache.offline_tracks()?;
    let pinned = cache.pinned_ids()?.into_iter().collect::<HashSet<_>>();

    println!("offline_root: {}", cache.offline_dir().display());
    println!("offline_playlists: {}", playlists.len());
    for (playlist_id, title, _, subtitle) in playlists {
        println!(
            "  - {} ({playlist_id}){}",
            title,
            subtitle
                .as_deref()
                .map(|value| format!(" - {value}"))
                .unwrap_or_default()
        );
    }

    println!("offline_tracks: {}", tracks.len());
    for (video_id, title, artist, _) in tracks {
        let pinned_label = if pinned.contains(&video_id) {
            " [pinned]"
        } else {
            ""
        };
        if artist.trim().is_empty() {
            println!("  - {title} ({video_id}){pinned_label}");
        } else {
            println!("  - {title} - {artist} ({video_id}){pinned_label}");
        }
    }

    Ok(())
}

struct CliOfflineDownload {
    video_id: String,
    bytes: usize,
    already_cached: bool,
}

struct CliAudioDownload {
    video_id: String,
    audio: Vec<u8>,
}

fn ensure_cli_track_available_offline(
    cache: &MusicCache,
    network: &NetworkState,
    track: &Track,
    pin: bool,
) -> Result<CliOfflineDownload> {
    cache_track_metadata_cli(cache, track)?;
    if cache.is_cached(&track.video_id) {
        if pin {
            cache.pin_offline(&track.video_id)?;
        }
        return Ok(CliOfflineDownload {
            video_id: track.video_id.clone(),
            bytes: 0,
            already_cached: true,
        });
    }

    let download = download_audio_for_offline_cli(cache, network, &track.video_id)?;
    if download.video_id != track.video_id {
        cache.upsert_track_metadata(
            &download.video_id,
            &track.title,
            &artist_string(track),
            history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        )?;
    }

    let art = track
        .thumbnail_url
        .as_ref()
        .and_then(|url| fetch_thumbnail_bytes(url));
    cache.store_audio(&download.video_id, &download.audio, art.as_deref())?;
    if pin {
        cache.pin_offline(&download.video_id)?;
    }

    Ok(CliOfflineDownload {
        video_id: download.video_id,
        bytes: download.audio.len(),
        already_cached: false,
    })
}

fn download_audio_for_offline_cli(
    cache: &MusicCache,
    network: &NetworkState,
    video_id: &str,
) -> Result<CliAudioDownload> {
    let resolved = network
        .resolved_audio_stream_url(video_id)
        .with_context(|| format!("failed to get audio URL for {video_id}"))?;
    if resolved.video_id != video_id {
        cache
            .replace_video_id(video_id, &resolved.video_id)
            .with_context(|| {
                format!(
                    "failed to migrate stale video id {video_id} -> {}",
                    resolved.video_id
                )
            })?;
    }

    let resolved_video_id = resolved.video_id;
    let audio = download_media_bytes(&resolved.url, Some("bytes=0-")).or_else(|primary_err| {
        let fallback_url = network
            .android_vr_audio_stream_url(&resolved_video_id)
            .with_context(|| {
                format!("failed to get Android VR audio URL for {resolved_video_id}")
            })?;
        download_media_bytes(&fallback_url, Some("bytes=0-"))
            .with_context(|| format!("failed to download Android VR media for {resolved_video_id}"))
            .map_err(|fallback_err| {
                primary_err.context(format!(
                    "Android VR fallback also failed for {resolved_video_id}: {fallback_err:#}"
                ))
            })
    })?;

    Ok(CliAudioDownload {
        video_id: resolved_video_id,
        audio,
    })
}

fn cache_track_metadata_cli(cache: &MusicCache, track: &Track) -> Result<()> {
    cache.record_play(
        &track.video_id,
        &track.title,
        &artist_string(track),
        history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        0,
    )
}

fn parse_video_id_argument(args: &[String], label: &str) -> Result<String> {
    let input = join_required_input(args, label)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("Missing {label}");
    }
    Ok(extract_video_id_from_input(trimmed).unwrap_or_else(|| trimmed.to_string()))
}

fn offline_tracks_as_queue(cache: &MusicCache) -> Result<Vec<Track>> {
    cache
        .offline_tracks()?
        .into_iter()
        .map(|(video_id, title, artist, _)| {
            offline_track_from_parts(cache, video_id, title, artist)
        })
        .collect()
}

fn offline_track_by_id(cache: &MusicCache, video_id: &str) -> Result<Track> {
    if !cache.is_cached(video_id) {
        bail!("No offline download exists for `{video_id}`");
    }
    let (title, artist) = cache
        .get_track_meta(video_id)
        .unwrap_or_else(|| (video_id.to_string(), String::new()));
    offline_track_from_parts(cache, video_id.to_string(), title, artist)
}

fn offline_track_from_parts(
    cache: &MusicCache,
    video_id: String,
    title: String,
    artist: String,
) -> Result<Track> {
    Ok(Track {
        video_id: video_id.clone(),
        title,
        artists: if artist.trim().is_empty() {
            Vec::new()
        } else {
            vec![Artist {
                name: artist,
                id: None,
                page_type: None,
            }]
        },
        album: None,
        duration: None,
        thumbnail_url: cache
            .get_cached_art(&video_id)
            .map(|path| format!("file://{}", path.display())),
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
        offline_fill: false,
    })
}

fn history_thumbnail_candidate(url: Option<&str>) -> Option<&str> {
    url.filter(|value| !value.starts_with("file://"))
}

fn notify_daemon_settings_if_running() {
    if daemon_is_running() {
        let _ = DaemonClient::new().and_then(|client| client.reload_settings());
    }
}

fn run_info() -> Result<()> {
    let snapshot = DaemonClient::new()?.state()?;
    let remote_rating = snapshot
        .current_track
        .as_ref()
        .and_then(|track| fetch_remote_rating(&track.video_id).ok());

    println!("status: {}", playback_status(&snapshot));
    println!("probe_active: {}", yes_no(snapshot.probe_active));
    println!("repeat_mode: {}", repeat_mode_label(snapshot.repeat_mode));
    println!("loops_left: {}", format_loops_left(&snapshot));
    println!("sleep: {}", format_sleep_status_from_snapshot(&snapshot));
    println!("queue_len: {}", snapshot.queue.len());
    println!(
        "queue_position: {}",
        if snapshot.queue.is_empty() {
            "0/0".to_string()
        } else {
            format!(
                "{}/{}",
                snapshot.queue_index.saturating_add(1),
                snapshot.queue.len()
            )
        }
    );
    println!(
        "progress: {}",
        format_progress(snapshot.position_millis, snapshot.duration_millis)
    );
    println!(
        "buffered_percent: {:.1}",
        (snapshot.buffered_fraction * 100.0).clamp(0.0, 100.0)
    );
    println!(
        "volume_percent: {:.0}",
        (snapshot.volume * 100.0).clamp(0.0, 500.0)
    );

    if let Some(track) = snapshot.current_track.as_ref() {
        println!("video_id: {}", track.video_id);
        println!("title: {}", track.title);
        println!("artist: {}", track.artist);
        println!(
            "rating: {}",
            rating_label(remote_rating.unwrap_or(track.rating))
        );
        println!(
            "thumbnail_url: {}",
            track.thumb_url.as_deref().unwrap_or("-")
        );
    } else {
        println!("video_id: -");
        println!("title: -");
        println!("artist: -");
        println!("rating: -");
        println!("thumbnail_url: -");
    }

    if let Some(next) = snapshot.queue.get(snapshot.queue_index.saturating_add(1)) {
        println!("next_track: {}", format_track_summary(next));
    } else {
        println!("next_track: -");
    }

    if let Some(radio) = snapshot.radio_virtual_playlist.as_ref() {
        println!("radio_seed: {}", radio.source_video_id);
        println!("radio_touched_len: {}", radio.touched_len);
    } else {
        println!("radio_seed: -");
        println!("radio_touched_len: -");
    }

    Ok(())
}

fn run_coordination(args: &[String]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        bail!("`static coordination` expects `list`");
    };

    match command {
        "list" => {
            let cache = open_music_cache()?;
            let settings = cache.load_settings()?;
            let targets = DaemonClient::new()?.coordination_targets()?;
            print_coordination_targets(&settings, &targets);
            Ok(())
        }
        other => bail!("Unknown coordination command `{other}`"),
    }
}

fn print_coordination_targets(settings: &AppSettings, targets: &CoordinationTargetsSnapshot) {
    println!(
        "MPRIS mode: {} ({})",
        settings.other_mpris_filter_mode.label(),
        if settings.other_mpris_filter_list.is_empty() {
            "empty list"
        } else {
            "configured list"
        }
    );
    print_coordination_target_group(
        &targets.mpris,
        &settings.other_mpris_filter_list,
        settings.other_mpris_filter_mode,
    );
    println!();
    println!(
        "PipeWire mode: {} ({})",
        settings.pipewire_filter_mode.label(),
        if settings.pipewire_filter_list.is_empty() {
            "empty list"
        } else {
            "configured list"
        }
    );
    print_coordination_target_group(
        &targets.pipewire,
        &settings.pipewire_filter_list,
        settings.pipewire_filter_mode,
    );
}

fn print_coordination_target_group(
    targets: &[CoordinationTargetSnapshot],
    configured: &[String],
    mode: CoordinationFilterMode,
) {
    if targets.is_empty() {
        println!("  No current targets.");
        return;
    }

    for target in targets {
        let identifiers = vec![target.key.clone(), target.label.clone()];
        let listed = target.filterable && coordination_filter_contains(configured, &identifiers);
        let coordinated = target.filterable
            && match mode {
                CoordinationFilterMode::Blacklist => !listed,
                CoordinationFilterMode::Whitelist => listed,
            };
        let listed_label = if listed { "listed" } else { "unlisted" };
        let coordinated_label = if coordinated {
            "coordinated"
        } else if target.filterable {
            "ignored"
        } else {
            "no stable key"
        };
        let key = if target.filterable {
            target.key.as_str()
        } else {
            "-"
        };
        println!(
            "  - {} [{}; {}; key={}]",
            target.label, listed_label, coordinated_label, key
        );
        if !target.detail.is_empty() {
            println!("    {}", target.detail);
        }
    }
}

fn run_config(args: &[String]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        bail!("`static config` expects `list`, `desc`, `get`, or `set`");
    };

    match command {
        "list" => {
            let cache = open_music_cache()?;
            let settings = cache.load_settings()?;
            print_config_list(&settings);
            Ok(())
        }
        "desc" => {
            if let Some(name) = args.get(1) {
                print_config_description(name)?;
            } else {
                print_config_help_overview();
                for spec in CONFIG_SPECS {
                    println!(
                        "{} [{}]\n{}",
                        spec.name,
                        config_kind_label(spec.kind),
                        config_description(spec.name)
                    );
                    println!();
                }
            }
            Ok(())
        }
        "get" => {
            let cache = open_music_cache()?;
            let settings = cache.load_settings()?;
            if let Some(name) = args.get(1) {
                println!("{}", format_config_value(&settings, name)?);
            } else {
                print_config_list(&settings);
            }
            Ok(())
        }
        "set" => {
            let name = args
                .get(1)
                .map(String::as_str)
                .ok_or_else(|| anyhow::anyhow!("`static config set` requires a config name"))?;
            let value = join_required_input(&args[2..], "config value")?;
            let cache = open_music_cache()?;
            let current_settings = cache.load_settings()?;
            let mut settings = current_settings.clone();
            apply_config_value(&mut settings, name, &value)?;
            if let Some(warning) =
                settings_eviction_warning(&cache, &current_settings, &settings, name)?
            {
                eprintln!("warning: {}", warning.message);
            }
            cache.save_settings(&settings)?;
            if daemon_is_running() {
                let _ = DaemonClient::new().and_then(|client| client.reload_settings());
            }
            println!("{name}={}", format_config_value(&settings, name)?);
            Ok(())
        }
        other => bail!("Unknown config command `{other}`"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfflineEvictionWarning {
    pub(crate) impact: usize,
    pub(crate) message: String,
}

pub(crate) fn settings_eviction_warning(
    cache: &MusicCache,
    current: &AppSettings,
    next: &AppSettings,
    key: &str,
) -> Result<Option<OfflineEvictionWarning>> {
    if !settings_change_can_evict_auto_downloads(current, next, key) {
        return Ok(None);
    }

    let impact = cache.auto_download_eviction_count(next)?;
    if impact == 0 {
        return Ok(None);
    }

    Ok(Some(OfflineEvictionWarning {
        impact,
        message: offline_eviction_warning_message(impact, key),
    }))
}

fn settings_change_can_evict_auto_downloads(
    current: &AppSettings,
    next: &AppSettings,
    key: &str,
) -> bool {
    match key {
        "auto_cache_listen_again" => {
            current.auto_cache_listen_again && !next.auto_cache_listen_again
        }
        "listen_again_cache_days" => next.listen_again_cache_days < current.listen_again_cache_days,
        "auto_manage_offline_cache" => {
            current.auto_manage_offline_cache && !next.auto_manage_offline_cache
        }
        "offline_cache_target" => next.offline_cache_target < current.offline_cache_target,
        "offline_rewatch_threshold" => {
            next.offline_rewatch_threshold > current.offline_rewatch_threshold
        }
        "score_decay_window_days" => next.score_decay_window_days < current.score_decay_window_days,
        "minimum_offline_score" => next.minimum_offline_score > current.minimum_offline_score,
        _ => false,
    }
}

fn offline_eviction_warning_message(impact: usize, key: &str) -> String {
    let song_label = if impact == 1 { "song" } else { "songs" };
    let pronoun = if impact == 1 { "it" } else { "they" };
    let object = if impact == 1 { "it" } else { "them" };
    let qualify = if impact == 1 { "qualifies" } else { "qualify" };
    let fall = if impact == 1 { "falls" } else { "fall" };
    let reason = match key {
        "auto_cache_listen_again" => {
            format!("{pronoun} only {qualify} through Listen again cache right now")
        }
        "listen_again_cache_days" => {
            format!("{pronoun} would {fall} outside the new Listen again cache window")
        }
        "auto_manage_offline_cache" => {
            format!("{pronoun} only {qualify} through the heuristic offline cache right now")
        }
        "offline_cache_target" => {
            format!("the smaller heuristic offline library size would no longer include {object}")
        }
        "offline_rewatch_threshold" => {
            format!("{pronoun} no longer {qualify} for the heuristic offline cache")
        }
        "score_decay_window_days" => format!("{pronoun} would {fall} below the offline cutoff"),
        "minimum_offline_score" => {
            format!("{pronoun} would {fall} below the new score floor")
        }
        _ => format!("the new offline cache settings would no longer include {object}"),
    };

    format!("{impact} auto-downloaded {song_label} would be evicted immediately because {reason}.")
}

fn run_interactive_tui() -> Result<()> {
    // Cookie validation is a network round-trip; on a barely-alive connection
    // it must not gate opening the TUI. Warming it here still helps: the
    // bootstrap runs behind a Once, so background content loaders calling
    // saved_network_state() wait for this validation instead of repeating it.
    std::thread::spawn(bootstrap_headless_saved_session);
    crate::cli_tui::run()
}

fn open_music_cache() -> Result<MusicCache> {
    let storage_root = get_storage_root();
    MusicCache::new_with_storage_root(storage_root.as_deref())
        .context("Failed to open Static settings store")
}

pub(crate) fn saved_network_state() -> Result<NetworkState> {
    bootstrap_headless_saved_session();
    NetworkState::from_saved_session().context(
        "A saved YouTube Music session is required. Open the GTK app once to connect your cookie.",
    )
}

fn bootstrap_headless_saved_session() {
    HEADLESS_SESSION_BOOTSTRAP.call_once(bootstrap_headless_saved_session_inner);
}

fn bootstrap_headless_saved_session_inner() {
    let saved_cookie = get_cookie().filter(|cookie| !cookie.trim().is_empty());
    if let Some(cookie) = saved_cookie {
        match NetworkState::validate_cookie(cookie, "keyring cookie") {
            NetworkValidationOutcome::Authenticated(_) | NetworkValidationOutcome::Deferred(_) => {
                return;
            }
            NetworkValidationOutcome::Rejected => {
                delete_cookie();
            }
        }
    }

    let settings = match open_music_cache().and_then(|cache| cache.load_settings()) {
        Ok(settings) => settings,
        Err(_) => return,
    };
    if !settings.auto_sync_chromium_cookies {
        return;
    }

    let chromium_cookies = match load_cookies_from_chromium_profiles(&settings) {
        Ok(cookies) => cookies,
        Err(_) => return,
    };

    for cookie in chromium_cookies {
        match NetworkState::validate_cookie(cookie.clone(), "Chromium cookie auto-sync") {
            NetworkValidationOutcome::Authenticated(_) | NetworkValidationOutcome::Deferred(_) => {
                let _ = store_cookie(&cookie);
                return;
            }
            NetworkValidationOutcome::Rejected => {}
        }
    }
}

fn fetch_remote_rating(video_id: &str) -> Result<SongRating> {
    saved_network_state()?.get_song_rating(video_id)
}

fn parse_exec_input(args: &[String]) -> Result<(String, bool)> {
    let mut probe = false;
    let mut input_parts = Vec::new();

    for arg in args {
        if let Some(flag) = parse_probe_flag(arg) {
            probe = flag;
        } else {
            input_parts.push(arg.as_str());
        }
    }

    if input_parts.is_empty() {
        bail!("`static exec` requires a URL, video id, or search term");
    }

    Ok((input_parts.join(" "), probe))
}

fn parse_probe_flag(arg: &str) -> Option<bool> {
    let normalized = arg.trim().trim_start_matches('?').to_ascii_lowercase();
    match normalized.as_str() {
        "--probe" => Some(true),
        "--no-probe" => Some(false),
        "probe=y" | "probe=yes" | "probe=true" | "probe=1" => Some(true),
        "probe=n" | "probe=no" | "probe=false" | "probe=0" => Some(false),
        _ => None,
    }
}

fn join_required_input(args: &[String], label: &str) -> Result<String> {
    if args.is_empty() {
        bail!("Missing {label}");
    }
    Ok(args.join(" "))
}

fn restored_volume(saved_volume: f64, current_max_volume: f64) -> f64 {
    if saved_volume.is_finite() {
        saved_volume.clamp(0.0, current_max_volume)
    } else {
        1.0_f64.min(current_max_volume)
    }
}

pub(crate) fn restore_playback_snapshot_via_daemon(
    snapshot: PlaybackSnapshot,
    resume_playback: bool,
) -> Result<PlaybackSnapshot> {
    let client = DaemonClient::new()?;
    let queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    let current = client.state()?;
    let mut restored = client.set_volume(restored_volume(snapshot.volume, current.max_volume))?;
    if snapshot.queue.is_empty() {
        return Ok(restored);
    }

    restored = if resume_playback {
        client.play_queue_at(
            snapshot.queue.clone(),
            queue_index,
            snapshot.playlist_context.clone(),
            snapshot.radio_virtual_playlist.clone(),
            snapshot.repeat_mode,
        )?
    } else {
        client.replace_queue(
            snapshot.queue.clone(),
            queue_index,
            snapshot.playlist_context.clone(),
            snapshot.radio_virtual_playlist.clone(),
            snapshot.repeat_mode,
        )?
    };

    if resume_playback {
        if let Some(position_millis) = snapshot.position_millis.filter(|position| *position > 0) {
            restored = client.seek_to_millis(position_millis)?;
        }
        if snapshot.paused {
            restored = client.pause()?;
        } else if restored.paused {
            restored = client.resume()?;
        }
    }

    Ok(restored)
}

pub(crate) fn restore_paused_playback_snapshot_via_daemon(
    snapshot: PlaybackSnapshot,
) -> Result<PlaybackSnapshot> {
    let client = DaemonClient::new()?;
    let queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    let current = client.state()?;
    let mut restored = client.set_volume(restored_volume(snapshot.volume, current.max_volume))?;
    if snapshot.queue.is_empty() {
        return Ok(restored);
    }

    restored = client.restore_queue_at(
        snapshot.queue.clone(),
        queue_index,
        snapshot.playlist_context.clone(),
        snapshot.radio_virtual_playlist.clone(),
        snapshot.repeat_mode,
        false,
        true,
        snapshot.position_millis,
    )?;
    Ok(restored)
}

pub(crate) fn restore_cross_device_queue_via_daemon(
    queue: CrossDeviceQueue,
) -> Result<PlaybackSnapshot> {
    if queue.tracks.is_empty() {
        bail!("No cross-device queue is available");
    }

    let playlist_context = queue
        .playlist_id
        .as_ref()
        .map(|playlist_id| PlaylistContext {
            playlist_id: playlist_id.clone(),
            title: queue
                .source_label
                .clone()
                .unwrap_or_else(|| "Cross-device queue".to_string()),
            thumbnail_url: queue
                .tracks
                .get(queue.selected_index)
                .and_then(|track| track.thumbnail_url.clone()),
            subtitle: Some("YouTube Music cross-device resume".to_string()),
            source: PlaylistContextSource::Remote,
        });

    DaemonClient::new()?.play_queue_at(
        queue.tracks.clone(),
        queue.selected_index,
        playlist_context,
        None,
        RepeatMode::Off,
    )
}

pub(crate) fn load_local_resume_snapshot() -> Result<Option<PlaybackSnapshot>> {
    Ok(load_local_resume_state()?.map(|persisted| persisted.snapshot))
}

pub(crate) fn load_local_resume_state() -> Result<Option<PersistedPlaybackState>> {
    Ok(load_persisted_playback_state()?.filter(|persisted| !persisted.snapshot.queue.is_empty()))
}

pub(crate) fn resume_snapshot_track(snapshot: &PlaybackSnapshot) -> Option<Track> {
    snapshot
        .queue
        .get(
            snapshot
                .queue_index
                .min(snapshot.queue.len().saturating_sub(1)),
        )
        .cloned()
        .or_else(|| {
            snapshot.current_track.as_ref().map(|track| Track {
                video_id: track.video_id.clone(),
                title: track.title.clone(),
                artists: vec![Artist {
                    name: track.artist.clone(),
                    id: None,
                    page_type: None,
                }],
                album: None,
                duration: None,
                thumbnail_url: track.thumb_url.clone(),
                playback_playlist_id: None,
                playback_params: None,
                not_interested_feedback_token: None,
                undo_feedback_token: None,
                offline_fill: false,
            })
        })
}

pub(crate) fn resume_snapshot_meta(snapshot: &PlaybackSnapshot) -> String {
    let queue_label = if snapshot.radio_virtual_playlist.is_some() {
        "Radio queue"
    } else if snapshot.playlist_context.is_some() {
        "Playlist"
    } else if snapshot.queue.len() > 1 {
        "Queue"
    } else {
        "Track"
    };
    match snapshot.position_millis {
        Some(position_millis) if position_millis > 0 => {
            let action = if snapshot.paused {
                "Paused at"
            } else {
                "Stopped at"
            };
            format!(
                "{queue_label} - {action} {}",
                format_millis(position_millis)
            )
        }
        _ => queue_label.to_string(),
    }
}

pub(crate) fn cross_device_selected_track(queue: &CrossDeviceQueue) -> Option<&Track> {
    queue.tracks.get(queue.selected_index)
}

pub(crate) fn cross_device_queue_is_singleton_seed(queue: &CrossDeviceQueue) -> bool {
    let Some(track) = cross_device_selected_track(queue) else {
        return false;
    };
    queue.selected_index == 0
        && queue
            .playlist_id
            .as_deref()
            .and_then(|playlist_id| playlist_id.strip_prefix("RDAMVM"))
            .is_some_and(|seed_video_id| seed_video_id == track.video_id)
}

pub(crate) fn cross_device_singleton_matches_local_resume(
    queue: &CrossDeviceQueue,
    snapshot: &PlaybackSnapshot,
) -> bool {
    cross_device_queue_is_singleton_seed(queue)
        && cross_device_selected_track(queue)
            .zip(resume_snapshot_track(snapshot))
            .is_some_and(|(remote_track, local_track)| {
                remote_track.video_id == local_track.video_id
            })
}

/// The remote queue only outranks the local snapshot when its lastWatchTimestamp is STRICTLY
/// newer than the local save. Queue registration rides best-effort network calls, so a session
/// played offline leaves the account queue holding this machine's own previous session: it
/// differs from the local snapshot but predates it, and must not be offered back as if another
/// device had played it. A tied or missing remote timestamp cannot prove otherwise, so local wins.
pub(crate) fn local_resume_is_newer_than_cross_device(
    queue: &CrossDeviceQueue,
    local_resume: &PersistedPlaybackState,
) -> bool {
    queue
        .last_watch_timestamp_millis
        .is_none_or(|remote_timestamp| local_resume.saved_at_unix_millis >= remote_timestamp)
}

pub(crate) fn should_use_local_resume_for_cross_device(
    queue: Option<&CrossDeviceQueue>,
    local_resume: Option<&PersistedPlaybackState>,
) -> bool {
    match (queue, local_resume) {
        (None, Some(_)) => true,
        (Some(queue), Some(local_resume)) => {
            local_resume_is_newer_than_cross_device(queue, local_resume)
                || cross_device_singleton_matches_local_resume(queue, &local_resume.snapshot)
        }
        _ => false,
    }
}

pub(crate) fn resolve_track_input(
    network: &NetworkState,
    input: &str,
    allow_search_fallback: bool,
) -> Result<Track> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("Track input cannot be empty");
    }

    if let Some(video_id) = extract_video_id_from_input(trimmed) {
        // Explicit URL/id: the id is trusted. Use the radio's track when it
        // has one, otherwise fabricate a placeholder around the trusted id so
        // it still plays even when the radio endpoint comes back empty.
        return match fetch_track_for_video_id(network, &video_id) {
            Ok(Some(track)) => Ok(track),
            Ok(None) => Ok(fabricate_track(&video_id, trimmed)),
            Err(err) => {
                if allow_search_fallback {
                    resolve_first_search_track(network, trimmed)
                        .with_context(|| format!("Could not resolve `{trimmed}` directly: {err}"))
                } else {
                    Err(err)
                }
            }
        };
    }

    if !trimmed.chars().any(char::is_whitespace) {
        // Speculative: a whitespace-free token *might* be a bare video id.
        // Accept it only if the radio actually resolves to a track. An empty
        // radio means it was not a real id, so fall through to the search
        // fallback instead of fabricating a placeholder row for junk input.
        match fetch_track_for_video_id(network, trimmed) {
            Ok(Some(track)) => return Ok(track),
            Ok(None) => {}
            Err(err) if !allow_search_fallback => return Err(err),
            Err(_) => {}
        }
    }

    if allow_search_fallback {
        return resolve_first_search_track(network, trimmed);
    }

    bail!("Could not resolve `{trimmed}` as a YouTube URL or video id")
}

/// Fetch the seed track for a video id via its radio. `Ok(None)` means the
/// radio endpoint succeeded but returned no tracks (e.g. the id was bogus);
/// callers decide whether to fabricate a placeholder or fall back to search.
fn fetch_track_for_video_id(network: &NetworkState, video_id: &str) -> Result<Option<Track>> {
    let mut radio = network
        .get_radio(video_id, None, None)
        .with_context(|| format!("Failed to resolve video `{video_id}`"))?;

    Ok(radio
        .tracks
        .iter()
        .position(|track| track.video_id == video_id)
        .map(|index| radio.tracks.remove(index))
        .or_else(|| radio.tracks.into_iter().next()))
}

/// Build a minimal placeholder `Track` around a trusted video id when no real
/// metadata is available. Only used for explicit URL/id input, never for
/// speculative free-text tokens.
fn fabricate_track(video_id: &str, fallback_title: &str) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: fallback_title.to_string(),
        artists: Vec::new(),
        album: None,
        duration: None,
        thumbnail_url: None,
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
        offline_fill: false,
    }
}

pub(crate) fn resolve_first_search_track(network: &NetworkState, query: &str) -> Result<Track> {
    let results = network
        .search(query)
        .with_context(|| format!("Failed to search for `{query}`"))?;
    first_search_track(&results)
        .ok_or_else(|| anyhow::anyhow!("No playable track was found for `{query}`"))
}

fn first_search_track(results: &SearchResult) -> Option<Track> {
    results.sections.iter().find_map(|section| {
        section.contents.iter().find_map(|item| match item {
            HomeItem::Track(track) => Some(track.clone()),
            _ => None,
        })
    })
}

#[derive(Clone, Copy)]
enum QueueInsertMode {
    Next,
    Last,
}

fn apply_queue_insert(
    client: &DaemonClient,
    snapshot: PlaybackSnapshot,
    track: Track,
    mode: QueueInsertMode,
) -> Result<PlaybackSnapshot> {
    let insert_at = match mode {
        QueueInsertMode::Next => {
            if snapshot.queue.is_empty() {
                0
            } else {
                snapshot
                    .queue_index
                    .saturating_add(1)
                    .min(snapshot.queue.len())
            }
        }
        QueueInsertMode::Last => snapshot
            .radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len.min(snapshot.queue.len()))
            .unwrap_or(snapshot.queue.len()),
    };

    client.insert_into_queue(vec![track], insert_at)
}

fn print_queue(snapshot: &PlaybackSnapshot) {
    println!("status: {}", playback_status(snapshot));
    if snapshot.queue.is_empty() {
        println!("Queue is empty.");
        return;
    }

    for (index, track) in snapshot.queue.iter().enumerate() {
        let marker = if index == snapshot.queue_index {
            if snapshot.paused { "||" } else { ">>" }
        } else if snapshot
            .radio_virtual_playlist
            .as_ref()
            .is_some_and(|radio| index >= radio.touched_len)
        {
            "~ "
        } else {
            "  "
        };

        let offline_fill = if track.offline_fill {
            "  [offline fill]"
        } else {
            ""
        };

        println!(
            "{marker} {:>3}. {}{offline_fill}",
            index.saturating_add(1),
            format_track_summary(track)
        );
    }
}

fn print_config_list(settings: &AppSettings) {
    for spec in CONFIG_SPECS {
        if let Ok(value) = format_config_value(settings, spec.name) {
            println!("{}={value}", spec.name);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ConfigKind {
    Bool,
    U32,
    F64,
    String,
    StringList,
}

#[derive(Clone, Copy)]
pub(crate) struct ConfigSpec {
    pub(crate) name: &'static str,
    pub(crate) kind: ConfigKind,
}

pub(crate) const CONFIG_SPECS: &[ConfigSpec] = &[
    ConfigSpec {
        name: "auto_sync_chromium_cookies",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "chromium_cookie_path",
        kind: ConfigKind::String,
    },
    ConfigSpec {
        name: "auto_release_track_loops",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "auto_release_track_loops_after",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "auto_manage_offline_cache",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "auto_cache_listen_again",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "listen_again_cache_days",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "offline_cache_target",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "offline_rewatch_threshold",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "score_decay_window_days",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "minimum_offline_score",
        kind: ConfigKind::F64,
    },
    ConfigSpec {
        name: "prefetch_enabled",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "prefetch_ahead",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "prefer_local_cache_on_playback",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "seamless_listening",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "volume_boost_enabled",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "dgain_enabled",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "dgain_flatten_percent",
        kind: ConfigKind::F64,
    },
    ConfigSpec {
        name: "dgain_ra_seconds",
        kind: ConfigKind::F64,
    },
    ConfigSpec {
        name: "dgain_change_lazyness",
        kind: ConfigKind::F64,
    },
    ConfigSpec {
        name: "xdgain_enabled",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "other_mpris_mode",
        kind: ConfigKind::String,
    },
    ConfigSpec {
        name: "other_mpris_halt_auto_resume_secs",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "other_mpris_filter_mode",
        kind: ConfigKind::String,
    },
    ConfigSpec {
        name: "other_mpris_filter_list",
        kind: ConfigKind::StringList,
    },
    ConfigSpec {
        name: "pipewire_ducking_enabled",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "pipewire_pause_mode",
        kind: ConfigKind::String,
    },
    ConfigSpec {
        name: "pipewire_pause_halt_auto_resume_secs",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "pipewire_ducking_threshold_secs",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "pipewire_ducking_volume",
        kind: ConfigKind::F64,
    },
    ConfigSpec {
        name: "desktop_aware_volume",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "desktop_aware_volume_hold_secs",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "desktop_aware_volume_restore_secs",
        kind: ConfigKind::U32,
    },
    ConfigSpec {
        name: "pipewire_filter_mode",
        kind: ConfigKind::String,
    },
    ConfigSpec {
        name: "pipewire_filter_list",
        kind: ConfigKind::StringList,
    },
    ConfigSpec {
        name: "sponsorblock_enabled",
        kind: ConfigKind::Bool,
    },
    ConfigSpec {
        name: "sponsorblock_categories",
        kind: ConfigKind::StringList,
    },
    // staticfs settings only exist in staticfs-enabled builds; the daemon
    // hot-reloads them (toggling mounts/unmounts the live filesystem).
    #[cfg(feature = "staticfs")]
    ConfigSpec {
        name: "staticfs_enabled",
        kind: ConfigKind::Bool,
    },
    #[cfg(feature = "staticfs")]
    ConfigSpec {
        name: "staticfs_mountpoint",
        kind: ConfigKind::String,
    },
];

fn print_config_help_overview() {
    println!("CLI config note:");
    println!("  Some keys intentionally accept a wider range than the GTK settings page.");
    println!("  That is a feature, not a bug.");
    println!("  When a saved value sits outside the UI-safe range, the GTK settings page disables");
    println!(
        "  that control and treats it as CLI-only until the value is brought back into range."
    );
    println!();
}

fn bool_values_note(enabled: &str, disabled: &str) -> String {
    format!(
        "Accepted values: `yes` to {enabled}; `no` to {disabled}. The CLI also accepts \
`true`/`false`, `1`/`0`, and `on`/`off`."
    )
}

fn advanced_cli_range_note(ui_range: &str, cli_range: &str) -> String {
    format!(
        "GTK settings UI range: {ui_range}.\nCLI range: {cli_range}. Wider CLI values are \
supported on purpose; if the saved value sits outside the GTK range, the GTK settings page \
disables that control and treats it as CLI-only until the value is brought back into range."
    )
}

fn sponsorblock_category_list() -> String {
    SPONSORBLOCK_CATEGORY_OPTIONS
        .iter()
        .map(|option| format!("`{}`", option.key))
        .collect::<Vec<_>>()
        .join(", ")
}

fn config_description(name: &str) -> String {
    match name {
        "volume_boost_enabled" => format!(
            "Extends the normal volume control from 100% to 500% for quiet tracks.\nDefault: off.\n{}",
            bool_values_note("allow volume up to 500%", "limit volume to 100%")
        ),
        "dgain_enabled" => format!(
            "Enables dGain, which smooths quiet and loud passages against a rolling loudness average.\nDefault: off.\n{}",
            bool_values_note("apply dynamic gain during playback", "leave track dynamics unchanged")
        ),
        "dgain_flatten_percent" => "Allowed loudness deviation around the rolling average before dGain corrects it.\n0% follows the average closely; 100% allows loudness from half to twice the average.\nAccepted range: 0-100. Default: 50."
            .to_string(),
        "dgain_ra_seconds" => "Weighted rolling-average response time for dGain, in seconds. Weights apply in dBFS over this much audible audio. Laziness moves the input/history split smoothly from 10/90 to 90/10. With an xdGain reference, its fixed 20% share leaves 80% for that split. Updates between intervals are smooth. Earlier history fades continuously rather than expiring at a window boundary.\nAccepted range: 0.1-120. Default: 15."
            .to_string(),
        "dgain_change_lazyness" => "Resists gain changes by linearly reducing correction in dB, and lets WRA follow input faster by smoothly shifting input/history weights from 10/90 at 0% to 90/10 at 100%. Low laziness therefore retains more WRA history while applying stronger correction. xdGain keeps 20% when present; input/history share the other 80%. Attack/release smoothing remains fixed. 0% gives the full, most responsive correction; 50% applies half the correction in dB; 100% applies 0 dB without changing either enable switch. Rolling analysis and the xdGain reference remain active. Master volume and clipping protection stay active.\nAccepted range: 0-100%. Default: 50%.".to_string(),
        "xdgain_enabled" => format!(
            "Chains each song from the previous song's final emitted WRA in dBFS. The first played song has no cross-song reference. Later songs seed WRA from the prior song and reserve a fixed 20% for that reference; laziness splits the remaining 80% between input and WRA history over the configured RA time.\nThe reference stays fixed during each song and advances at its end or skip. Seeks do not advance it. There is no separate xdGain volume offset.\nRequires dGain. Toggling xdGain off clears the chain. Default: off.\n{}",
            bool_values_note("enable cross-song gain", "disable cross-song gain")
        ),
        "auto_sync_chromium_cookies" => format!(
            "Controls whether Static tries to refresh a saved YouTube Music session from a \
Chromium-family browser profile when the current saved cookie is missing or rejected.\n{}",
            bool_values_note(
                "allow automatic cookie imports and session self-healing",
                "leave cookie management entirely manual"
            )
        ),
        "chromium_cookie_path" => "Overrides which Chromium cookie database Static reads when \
cookie auto-sync is enabled.\nSet a full path to pin Static to one browser profile, or leave it \
empty to let Static auto-detect the most likely Chromium-family profile on this machine."
            .to_string(),
        "auto_release_track_loops" => format!(
            "Controls how repeat-one is represented inside saved resume state.\nWith this on, \
Static converts an effectively infinite repeat-one session into a finite remaining loop budget so \
restored playback does not get trapped looping forever.\n{}",
            bool_values_note(
                "cap repeat-one in saved resume state",
                "preserve the normal repeat-one behavior exactly as-is"
            )
        ),
        "auto_release_track_loops_after" => "Sets the loop budget that is re-armed after a \
resume snapshot is restored when `auto_release_track_loops=yes`.\n`1` means the current track may \
repeat one more time after restore; larger values keep more repeat-one cycles available.\nAccepted \
range: 1-100."
            .to_string(),
        "auto_manage_offline_cache" => format!(
            "Master switch for Static's automatic offline cache manager.\nWhen enabled, Static \
may add or evict automatically managed downloads to stay near the configured cache target using \
the listen-again and heuristic scoring rules.\n{}",
            bool_values_note(
                "let Static manage automatically selected offline tracks",
                "disable automatic offline adds and evictions"
            )
        ),
        "auto_cache_listen_again" => format!(
            "Controls whether strong Listen again candidates should count toward automatic \
offline caching.\nThis is one of the signals used by the auto-cache manager, so it matters most \
when `auto_manage_offline_cache=yes`.\n{}",
            bool_values_note(
                "include Listen again activity as an auto-cache source",
                "ignore Listen again activity when picking automatic downloads"
            )
        ),
        "listen_again_cache_days" => "Lookback window, in days, used when evaluating Listen \
again activity for automatic offline caching.\nSmaller values bias toward recent habits; larger \
values let older repeat listening keep influencing the auto-cache decision.\nAccepted range: \
1-365."
            .to_string(),
        "offline_cache_target" => "Approximate size target for the automatically managed \
offline cache.\nStatic tries to stay near this many auto-managed tracks over time; higher values \
favor keeping more candidates downloaded, while lower values make eviction more aggressive.\n\
Accepted range: 0-500."
            .to_string(),
        "offline_rewatch_threshold" => "Minimum replay count a track should have before the \
heuristic offline scorer considers it strong enough to keep around.\n`0` makes the scorer more \
inclusive; higher values require stronger evidence that you revisit the track.\nAccepted range: \
0-50."
            .to_string(),
        "score_decay_window_days" => "Controls how quickly older plays lose weight in the \
heuristic offline score.\nShort windows make the score react faster to current tastes; longer \
windows keep older listening history relevant for longer.\nAccepted range: 1-365."
            .to_string(),
        "minimum_offline_score" => "Minimum heuristic score required before Static keeps a \
track in the automatically managed offline cache.\nLower values keep more marginal candidates; \
higher values make the cache more selective.\nAccepted range: 0.0-999.0."
            .to_string(),
        "prefetch_enabled" => format!(
            "Controls whether Static fetches upcoming queue items before they are needed for \
playback.\nThis mainly trades a little more network and temporary disk activity for smoother \
track transitions.\n{}",
            bool_values_note(
                "prefetch upcoming queue items ahead of time",
                "only fetch tracks when playback reaches them"
            )
        ),
        "prefetch_ahead" => "How many upcoming queue entries Static should try to prefetch when \
prefetching is enabled.\n`0` effectively disables the ahead-of-time queue depth without changing \
the switch; larger values are more aggressive.\nAccepted range: 0-12."
            .to_string(),
        "prefer_local_cache_on_playback" => format!(
            "Controls whether playback should favor already cached audio over the network when \
the same track exists locally.\n{}",
            bool_values_note(
                "prefer local cached media when it is available",
                "prefer normal network playback even if a cached copy exists"
            )
        ),
        "seamless_listening" => format!(
            "Controls whether Static keeps music going from the offline library when the next \
song cannot be fetched.\nApplies to forward advances only (track end, Next, skipping a failed \
track): if the next song is not cached or prefetched and the network is down, or a radio queue \
runs dry because it could not be extended, Static inserts a pick from your combined offline store \
(automatic, manual, and playlist downloads) and plays that instead. Songs you pick explicitly are \
never substituted.\n{}",
            bool_values_note(
                "fill from the offline library when the next song cannot be fetched",
                "stop or fail as usual when the next song cannot be fetched"
            )
        ),
        "other_mpris_mode" => "Controls how Static reacts when another MPRIS player is active.\n\
Values:\n  - `off`: ignore peer MPRIS players entirely.\n  - `halt`: pause for the other \
player and stay halted until you resume manually.\n  - `halt_auto`: pause for the other player, \
then auto-resume after `other_mpris_halt_auto_resume_secs` once peers stay quiet. A delay of `0` \
resumes as soon as the peer goes quiet."
            .to_string(),
        "other_mpris_halt_auto_resume_secs" => format!(
            "Quiet window used only when `other_mpris_mode=halt_auto`.\n`5` means resume 5 \
seconds after peer players stop; `0` means resume immediately after they stop.\n{}",
            advanced_cli_range_note(
                &format!(
                    "{}-{} seconds",
                    OTHER_MPRIS_HALT_AUTO_RESUME_UI_MIN_SECS,
                    OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS
                ),
                &format!(
                    "any integer >= {}",
                    OTHER_MPRIS_HALT_AUTO_RESUME_UI_MIN_SECS
                )
            )
        ),
        "other_mpris_filter_mode" => "Switches how `other_mpris_filter_list` is interpreted.\n\
Values:\n  - `blacklist`: coordinate with every current MPRIS player except listed identifiers.\n  - \
`whitelist`: coordinate only with listed MPRIS identifiers.\nAn empty blacklist preserves the \
default behavior; an empty whitelist ignores every peer player."
            .to_string(),
        "other_mpris_filter_list" => "Comma-separated MPRIS identifiers for the current filter \
mode.\nStatic saves app-level identifiers, matching entries case-insensitively against the peer \
desktop entry, normalized MPRIS app key, and identity. Runtime instance bus names are shown for \
diagnostics but are not saved as filter keys. Use the settings UI or `static coordination list` to \
discover current identifiers."
            .to_string(),
        "pipewire_ducking_enabled" => format!(
            "Master switch for PipeWire-based coordination with non-MPRIS audio such as games, \
calls, or browsers.\nWhen enabled, Static watches for active foreign streams and then either \
ducks volume or pauses depending on `pipewire_pause_mode`.\n{}",
            bool_values_note(
                "enable PipeWire stream monitoring and coordination",
                "disable all PipeWire-triggered ducking and pausing"
            )
        ),
        "pipewire_pause_mode" => "Chooses what Static does after PipeWire detects another audio \
stream staying active long enough to matter.\nValues:\n  - `off`: keep playback running and use \
ducking volume instead.\n  - `halt`: pause and stay halted until you resume manually.\n  - \
`halt_auto`: pause, then auto-resume after `pipewire_pause_halt_auto_resume_secs` once the other \
audio stays quiet. A delay of `0` resumes as soon as it goes quiet."
            .to_string(),
        "pipewire_pause_halt_auto_resume_secs" => format!(
            "Quiet window used only when `pipewire_pause_mode=halt_auto`.\n`5` means Static \
comes back 5 seconds after other audio stops; `0` means resume immediately after it stops.\n\
{}",
            advanced_cli_range_note(
                &format!(
                    "{}-{} seconds",
                    PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MIN_SECS,
                    PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MAX_SECS
                ),
                &format!(
                    "any integer >= {}",
                    PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MIN_SECS
                )
            )
        ),
        "pipewire_ducking_threshold_secs" => format!(
            "How long another PipeWire stream must remain active before Static reacts.\nLow \
values make ducking or pausing kick in quickly; high values ignore short notification sounds or \
brief audio blips.\n{}",
            advanced_cli_range_note(
                &format!(
                    "{}-{} seconds",
                    PIPEWIRE_DUCKING_THRESHOLD_UI_MIN_SECS, PIPEWIRE_DUCKING_THRESHOLD_UI_MAX_SECS
                ),
                &format!("any integer >= {}", PIPEWIRE_DUCKING_THRESHOLD_UI_MIN_SECS)
            )
        ),
        "desktop_aware_volume" => "Desktop aware volume. While PipeWire coordination has Static \
ducked or paused for other audio, Static watches the system sink volume. If you cranked the sink \
for that quieter content, Static comes back with its own output compensated so it is as loud as \
before the interruption; after `desktop_aware_volume_hold_secs` of uninterrupted playback it \
slides the sink back to its old level while fading the compensation out, so nothing audible \
changes.\nGround rules: the job is tied to the sink node it started on (switching devices \
abandons it); any sink change Static did not make itself abandons it; the hold clock only runs \
while Static is actually playing alone. Abandoning only fades the compensation out and never \
touches the sink. Requires `pipewire_ducking_enabled=true` and wpctl."
            .to_string(),
        "desktop_aware_volume_hold_secs" => format!(
            "Uninterrupted playback Static waits for before it starts moving the sink back.\n{}",
            advanced_cli_range_note(
                &format!(
                    "{}-{} seconds",
                    DESKTOP_AWARE_VOLUME_HOLD_UI_MIN_SECS, DESKTOP_AWARE_VOLUME_HOLD_UI_MAX_SECS
                ),
                &format!("any integer >= {}", DESKTOP_AWARE_VOLUME_HOLD_UI_MIN_SECS)
            )
        ),
        "desktop_aware_volume_restore_secs" => format!(
            "How long the sink restore and the compensation fade take.\n{}",
            advanced_cli_range_note(
                &format!(
                    "{}-{} seconds",
                    DESKTOP_AWARE_VOLUME_RESTORE_UI_MIN_SECS,
                    DESKTOP_AWARE_VOLUME_RESTORE_UI_MAX_SECS
                ),
                &format!("any integer >= {}", DESKTOP_AWARE_VOLUME_RESTORE_UI_MIN_SECS)
            )
        ),
        "pipewire_ducking_volume" => format!(
            "Target multiplier applied to the saved pre-duck volume while PipeWire ducking is \
active and `pipewire_pause_mode=off`.\n`0.3` means play at 30% of the original level, `1.0` \
means do not lower the level at all, and values above `1.0` intentionally overdrive playback so \
Static can punch through other audio when that is useful.\n{}",
            advanced_cli_range_note(
                &format!(
                    "{:.1}-{:.1}",
                    PIPEWIRE_DUCKING_VOLUME_UI_MIN, PIPEWIRE_DUCKING_VOLUME_UI_MAX
                ),
                &format!(
                    "{:.1}-{:.1}",
                    PIPEWIRE_DUCKING_VOLUME_UI_MIN, PIPEWIRE_DUCKING_VOLUME_CLI_MAX
                )
            )
        ),
        "pipewire_filter_mode" => "Switches how `pipewire_filter_list` is interpreted.\nValues:\n  - \
`blacklist`: coordinate with every current foreign PipeWire stream except listed identifiers.\n  - \
`whitelist`: coordinate only with listed PipeWire identifiers.\nAn empty blacklist preserves the \
default behavior; an empty whitelist ignores every foreign stream."
            .to_string(),
        "pipewire_filter_list" => "Comma-separated PipeWire identifiers for the current filter \
mode.\nStatic matches entries case-insensitively against the stream application name, binary, \
media name, and node labels. Runtime-only node ids and process ids are shown for diagnostics but \
are not saved as stable filter keys. Use the settings UI or `static coordination list` to discover \
current identifiers."
            .to_string(),
        "sponsorblock_enabled" => format!(
            "Controls whether Static loads SponsorBlock metadata and uses it to skip configured \
segment categories during playback.\n{}",
            bool_values_note(
                "enable SponsorBlock loading and category-based skipping",
                "ignore SponsorBlock data entirely"
            )
        ),
        "sponsorblock_categories" => format!(
            "Comma-separated SponsorBlock category keys that Static should skip when \
SponsorBlock is enabled.\nLeave the list empty to skip nothing, or provide any subset of the \
supported keys.\nSupported keys: {}.",
            sponsorblock_category_list()
        ),
        _ => unreachable!("unknown config description requested for `{name}`"),
    }
}

fn config_kind_label(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Bool => "bool",
        ConfigKind::U32 => "u32",
        ConfigKind::F64 => "f64",
        ConfigKind::String => "string",
        ConfigKind::StringList => "csv",
    }
}

fn config_spec(name: &str) -> Result<&'static ConfigSpec> {
    CONFIG_SPECS
        .iter()
        .find(|spec| spec.name == name)
        .ok_or_else(|| anyhow::anyhow!("Unknown config key `{name}`"))
}

fn print_config_description(name: &str) -> Result<()> {
    let spec = config_spec(name)?;
    println!("{} [{}]", spec.name, config_kind_label(spec.kind));
    println!("{}", config_description(spec.name));
    Ok(())
}

pub(crate) fn format_config_value(settings: &AppSettings, name: &str) -> Result<String> {
    Ok(match name {
        "auto_sync_chromium_cookies" => yes_no_bool(settings.auto_sync_chromium_cookies),
        "chromium_cookie_path" => settings.chromium_cookie_path.clone(),
        "auto_release_track_loops" => yes_no_bool(settings.auto_release_track_loops),
        "auto_release_track_loops_after" => settings.auto_release_track_loops_after.to_string(),
        "auto_manage_offline_cache" => yes_no_bool(settings.auto_manage_offline_cache),
        "auto_cache_listen_again" => yes_no_bool(settings.auto_cache_listen_again),
        "listen_again_cache_days" => settings.listen_again_cache_days.to_string(),
        "offline_cache_target" => settings.offline_cache_target.to_string(),
        "offline_rewatch_threshold" => settings.offline_rewatch_threshold.to_string(),
        "score_decay_window_days" => settings.score_decay_window_days.to_string(),
        "minimum_offline_score" => settings.minimum_offline_score.to_string(),
        "prefetch_enabled" => yes_no_bool(settings.prefetch_enabled),
        "prefetch_ahead" => settings.prefetch_ahead.to_string(),
        "prefer_local_cache_on_playback" => yes_no_bool(settings.prefer_local_cache_on_playback),
        "seamless_listening" => yes_no_bool(settings.seamless_listening),
        "volume_boost_enabled" => yes_no_bool(settings.volume_boost_enabled),
        "dgain_enabled" => yes_no_bool(settings.dgain_enabled),
        "dgain_flatten_percent" => settings.dgain_flatten_percent.to_string(),
        "dgain_ra_seconds" => settings.dgain_ra_seconds.to_string(),
        "dgain_change_lazyness" => settings.dgain_change_lazyness.to_string(),
        "xdgain_enabled" => yes_no_bool(settings.xdgain_enabled),
        "other_mpris_mode" => settings.other_mpris_mode.as_str().to_string(),
        "other_mpris_halt_auto_resume_secs" => {
            settings.other_mpris_halt_auto_resume_secs.to_string()
        }
        "other_mpris_filter_mode" => settings.other_mpris_filter_mode.as_str().to_string(),
        "other_mpris_filter_list" => settings.other_mpris_filter_list.join(","),
        "pipewire_ducking_enabled" => yes_no_bool(settings.pipewire_ducking_enabled),
        "pipewire_pause_mode" => settings.pipewire_pause_mode.as_str().to_string(),
        "pipewire_pause_halt_auto_resume_secs" => {
            settings.pipewire_pause_halt_auto_resume_secs.to_string()
        }
        "pipewire_ducking_threshold_secs" => settings.pipewire_ducking_threshold_secs.to_string(),
        "pipewire_ducking_volume" => settings.pipewire_ducking_volume.to_string(),
        "desktop_aware_volume" => yes_no_bool(settings.desktop_aware_volume),
        "desktop_aware_volume_hold_secs" => settings.desktop_aware_volume_hold_secs.to_string(),
        "desktop_aware_volume_restore_secs" => {
            settings.desktop_aware_volume_restore_secs.to_string()
        }
        "pipewire_filter_mode" => settings.pipewire_filter_mode.as_str().to_string(),
        "pipewire_filter_list" => settings.pipewire_filter_list.join(","),
        "sponsorblock_enabled" => yes_no_bool(settings.sponsorblock_enabled),
        "sponsorblock_categories" => settings.sponsorblock_categories.join(","),
        "staticfs_enabled" => yes_no_bool(settings.staticfs_enabled),
        "staticfs_mountpoint" => settings.staticfs_mountpoint.clone(),
        other => return Err(anyhow::anyhow!("Unknown config key `{other}`")),
    })
}

pub(crate) fn apply_config_value(
    settings: &mut AppSettings,
    name: &str,
    value: &str,
) -> Result<()> {
    let trimmed = value.trim();
    match name {
        "auto_sync_chromium_cookies" => settings.auto_sync_chromium_cookies = parse_bool(trimmed)?,
        "chromium_cookie_path" => settings.chromium_cookie_path = trimmed.to_string(),
        "auto_release_track_loops" => settings.auto_release_track_loops = parse_bool(trimmed)?,
        "auto_release_track_loops_after" => {
            settings.auto_release_track_loops_after = parse_u32(trimmed)?.clamp(1, 100)
        }
        "auto_manage_offline_cache" => settings.auto_manage_offline_cache = parse_bool(trimmed)?,
        "auto_cache_listen_again" => settings.auto_cache_listen_again = parse_bool(trimmed)?,
        "listen_again_cache_days" => {
            settings.listen_again_cache_days = parse_u32(trimmed)?.clamp(1, 365)
        }
        "offline_cache_target" => settings.offline_cache_target = parse_u32(trimmed)?.clamp(0, 500),
        "offline_rewatch_threshold" => {
            settings.offline_rewatch_threshold = parse_u32(trimmed)?.clamp(0, 50)
        }
        "score_decay_window_days" => {
            settings.score_decay_window_days = parse_u32(trimmed)?.clamp(1, 365)
        }
        "minimum_offline_score" => {
            settings.minimum_offline_score = parse_f64(trimmed)?.clamp(0.0, 999.0)
        }
        "prefetch_enabled" => settings.prefetch_enabled = parse_bool(trimmed)?,
        "prefetch_ahead" => settings.prefetch_ahead = parse_u32(trimmed)?.clamp(0, 12),
        "prefer_local_cache_on_playback" => {
            settings.prefer_local_cache_on_playback = parse_bool(trimmed)?
        }
        "seamless_listening" => settings.seamless_listening = parse_bool(trimmed)?,
        "volume_boost_enabled" => settings.volume_boost_enabled = parse_bool(trimmed)?,
        "dgain_enabled" => settings.dgain_enabled = parse_bool(trimmed)?,
        "dgain_flatten_percent" => {
            settings.dgain_flatten_percent = parse_f64_in_range(trimmed, 0.0, 100.0)?;
        }
        "dgain_ra_seconds" => {
            settings.dgain_ra_seconds = parse_f64_in_range(trimmed, 0.1, 120.0)?;
        }
        "dgain_change_lazyness" => {
            settings.dgain_change_lazyness = parse_f64_in_range(trimmed, 0.0, 100.0)?;
        }
        "xdgain_enabled" => {
            let enabled = parse_bool(trimmed)?;
            if enabled && !settings.dgain_enabled {
                bail!("Enable dgain_enabled before enabling xdGain");
            }
            settings.xdgain_enabled = enabled;
        }
        "other_mpris_mode" => settings.other_mpris_mode = parse_coordination_mode(trimmed)?,
        "other_mpris_halt_auto_resume_secs" => {
            settings.other_mpris_halt_auto_resume_secs =
                parse_u32(trimmed)?.max(OTHER_MPRIS_HALT_AUTO_RESUME_UI_MIN_SECS)
        }
        "other_mpris_filter_mode" => {
            settings.other_mpris_filter_mode = parse_coordination_filter_mode(trimmed)?
        }
        "other_mpris_filter_list" => {
            settings.other_mpris_filter_list = parse_coordination_filter_list(trimmed)
        }
        "pipewire_ducking_enabled" => settings.pipewire_ducking_enabled = parse_bool(trimmed)?,
        "pipewire_pause_mode" => settings.pipewire_pause_mode = parse_coordination_mode(trimmed)?,
        "pipewire_pause_halt_auto_resume_secs" => {
            settings.pipewire_pause_halt_auto_resume_secs =
                parse_u32(trimmed)?.max(PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MIN_SECS)
        }
        "pipewire_ducking_threshold_secs" => {
            settings.pipewire_ducking_threshold_secs =
                parse_u32(trimmed)?.max(PIPEWIRE_DUCKING_THRESHOLD_UI_MIN_SECS)
        }
        "pipewire_ducking_volume" => {
            settings.pipewire_ducking_volume = parse_f64(trimmed)?.clamp(
                PIPEWIRE_DUCKING_VOLUME_UI_MIN,
                PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
            )
        }
        "desktop_aware_volume" => settings.desktop_aware_volume = parse_bool(trimmed)?,
        "desktop_aware_volume_hold_secs" => {
            settings.desktop_aware_volume_hold_secs =
                parse_u32(trimmed)?.max(DESKTOP_AWARE_VOLUME_HOLD_UI_MIN_SECS)
        }
        "desktop_aware_volume_restore_secs" => {
            settings.desktop_aware_volume_restore_secs =
                parse_u32(trimmed)?.max(DESKTOP_AWARE_VOLUME_RESTORE_UI_MIN_SECS)
        }
        "pipewire_filter_mode" => {
            settings.pipewire_filter_mode = parse_coordination_filter_mode(trimmed)?
        }
        "pipewire_filter_list" => {
            settings.pipewire_filter_list = parse_coordination_filter_list(trimmed)
        }
        "sponsorblock_enabled" => settings.sponsorblock_enabled = parse_bool(trimmed)?,
        "staticfs_enabled" => settings.staticfs_enabled = parse_bool(trimmed)?,
        "staticfs_mountpoint" => settings.staticfs_mountpoint = trimmed.to_string(),
        "sponsorblock_categories" => {
            let categories = trimmed
                .split(',')
                .map(str::trim)
                .filter(|category| !category.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            settings.sponsorblock_categories = sanitize_categories(&categories);
        }
        other => return Err(anyhow::anyhow!("Unknown config key `{other}`")),
    }

    Ok(())
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "no" | "n" | "off" => Ok(false),
        _ => bail!("Expected a boolean value, got `{value}`"),
    }
}

fn parse_coordination_mode(value: &str) -> Result<CoordinationMode> {
    CoordinationMode::from_str(value).ok_or_else(|| {
        anyhow::anyhow!("Expected one of `off`, `halt`, or `halt_auto`, got `{value}`")
    })
}

fn parse_coordination_filter_mode(value: &str) -> Result<CoordinationFilterMode> {
    CoordinationFilterMode::from_str(value)
        .ok_or_else(|| anyhow::anyhow!("Expected one of `blacklist` or `whitelist`, got `{value}`"))
}

fn parse_coordination_filter_list(value: &str) -> Vec<String> {
    let values = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    sanitize_coordination_filter_entries(&values)
}

fn parse_u32(value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .with_context(|| format!("Expected an unsigned integer, got `{value}`"))
}

fn parse_f64(value: &str) -> Result<f64> {
    let parsed = value
        .parse::<f64>()
        .with_context(|| format!("Expected a number, got `{value}`"))?;
    if !parsed.is_finite() {
        bail!("Expected a finite number, got `{value}`");
    }
    Ok(parsed)
}

fn parse_f64_in_range(value: &str, minimum: f64, maximum: f64) -> Result<f64> {
    let parsed = parse_f64(value)?;
    if !(minimum..=maximum).contains(&parsed) {
        bail!("Expected a number from {minimum} to {maximum}, got `{value}`");
    }
    Ok(parsed)
}

fn playback_status(snapshot: &PlaybackSnapshot) -> &'static str {
    if snapshot.probe_active {
        "probe"
    } else if snapshot.current_track.is_some() {
        if snapshot.paused { "paused" } else { "playing" }
    } else {
        "stopped"
    }
}

fn repeat_mode_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    }
}

fn format_loops_left(snapshot: &PlaybackSnapshot) -> String {
    match snapshot.repeat_mode {
        RepeatMode::One => snapshot
            .track_repeat_loops_left
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unbounded".to_string()),
        _ => "-".to_string(),
    }
}

fn current_track_label(snapshot: &PlaybackSnapshot) -> String {
    snapshot
        .current_track
        .as_ref()
        .map(|track| format!("{} - {}", track.title, track.artist))
        .unwrap_or_else(|| "nothing".to_string())
}

pub(crate) fn rating_label(rating: SongRating) -> &'static str {
    match rating {
        SongRating::Like => "like",
        SongRating::Dislike => "dislike",
        SongRating::Indifferent => "neutral",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn yes_no_bool(value: bool) -> String {
    yes_no(value).to_string()
}

#[cfg(test)]
#[path = "tests/cli.rs"]
mod tests;

fn format_progress(position_millis: Option<u64>, duration_millis: Option<u64>) -> String {
    match (position_millis, duration_millis) {
        (Some(position), Some(duration)) => {
            format!(
                "{} / {} ({:.1}%)",
                format_millis(position),
                format_millis(duration),
                if duration == 0 {
                    0.0
                } else {
                    (position as f64 / duration as f64 * 100.0).clamp(0.0, 100.0)
                }
            )
        }
        (Some(position), None) => format!("{} / -", format_millis(position)),
        _ => "-".to_string(),
    }
}

pub(crate) fn format_millis(millis: u64) -> String {
    let total_seconds = millis / 1000;
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3600;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub(crate) fn format_duration_secs(total_seconds: u64) -> String {
    let seconds = total_seconds % 60;
    let minutes = (total_seconds / 60) % 60;
    let hours = total_seconds / 3600;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub(crate) fn format_track_summary(track: &Track) -> String {
    format!("{} - {}", track.title, artist_string(track))
}

pub(crate) fn artist_string(track: &Track) -> String {
    if track.artists.is_empty() {
        "Unknown artist".to_string()
    } else {
        track
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}
