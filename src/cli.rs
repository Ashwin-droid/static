use std::collections::HashSet;
use std::sync::Once;

use anyhow::{bail, Context, Result};

use crate::ipc::{
    daemon_is_running, load_persisted_playback_state, DaemonClient, PlaybackSnapshot, RepeatMode,
};
use crate::models::{HomeItem, RadioVirtualPlaylist, SearchResult, Track};
use crate::network::{
    delete_cookie, get_cookie, get_storage_root, load_cookie_from_chromium_profile,
    sanitize_categories, store_cookie, SongRating,
};
use crate::network_state::{NetworkState, NetworkValidationOutcome};
use crate::offline::{AppSettings, MusicCache};

static HEADLESS_SESSION_BOOTSTRAP: Once = Once::new();
const HELP_TEXT: &str = "\
Static CLI

Usage:
  static help
  static like
  static dislike
  static neutral
  static resume
  static exec <ytm/yt url | video id | search term> [probe=y|n]
  static queue get
  static queue addNext <ytm/yt url | video id | search term>
  static queue addLast <ytm/yt url | video id | search term>
  static info
  static config list
  static config desc [config_name]
  static config get [config_name]
  static config set <config_name> <config_value>
  static i

Notes:
  static without arguments still opens the GTK app.
  static i opens the terminal-native TUI on the alternate screen.
  Playback commands talk to staticd over D-Bus and auto-start it when needed.";

pub fn wants_cli(args: &[String]) -> bool {
    !args.is_empty() && args.first().is_some_and(|arg| arg != "--daemon")
}

pub fn run(args: &[String]) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(());
    };

    match command {
        "help" | "-h" | "--help" => {
            println!("{HELP_TEXT}");
            Ok(())
        }
        "like" => run_rate_current_track(SongRating::Like),
        "dislike" => run_rate_current_track(SongRating::Dislike),
        "neutral" => run_rate_current_track(SongRating::Indifferent),
        "resume" => run_resume(),
        "exec" => run_exec(&args[1..]),
        "queue" => run_queue(&args[1..]),
        "info" => run_info(),
        "config" => run_config(&args[1..]),
        "i" => run_interactive_tui(),
        other => bail!("Unknown CLI command `{other}`.\n\n{HELP_TEXT}"),
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

fn run_resume() -> Result<()> {
    let persisted = load_persisted_playback_state()?
        .filter(|persisted| !persisted.snapshot.queue.is_empty())
        .ok_or_else(|| anyhow::anyhow!("No saved playback state is available"))?;
    let restored = restore_playback_snapshot_via_daemon(persisted.snapshot, true)?;
    println!("Resumed {}", current_track_label(&restored));
    Ok(())
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

    play_track_with_radio(&client, track.clone())?;

    if saved_network_state()
        .and_then(|network| network.get_radio(&track.video_id, None, None))
        .is_ok()
    {
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
    client.play_queue_at(
        vec![track.clone()],
        0,
        None,
        Some(RadioVirtualPlaylist {
            source_video_id: track.video_id.clone(),
            touched_len: 1,
            chips: Vec::new(),
        }),
        RepeatMode::Off,
    )?;

    match saved_network_state().and_then(|network| network.get_radio(&track.video_id, None, None)) {
        Ok(mut radio) => {
            let queue_seed = radio
                .tracks
                .iter()
                .position(|queued| queued.video_id == track.video_id)
                .map(|index| radio.tracks.remove(index))
                .unwrap_or_else(|| track.clone());
            let mut queue = vec![queue_seed];
            queue.extend(dedupe_radio_recommendations(&queue, radio.tracks));
            let radio_virtual_playlist = RadioVirtualPlaylist {
                source_video_id: track.video_id.clone(),
                touched_len: 1,
                chips: radio.chips,
            };
            client.replace_queue(
                queue,
                0,
                None,
                Some(radio_virtual_playlist),
                RepeatMode::Off,
            )
        }
        Err(_) => client.state(),
    }
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
        (snapshot.volume * 100.0).clamp(0.0, 100.0)
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
                for spec in CONFIG_SPECS {
                    println!(
                        "{} [{}]\n{}",
                        spec.name,
                        config_kind_label(spec.kind),
                        spec.description
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
            let mut settings = cache.load_settings()?;
            apply_config_value(&mut settings, name, &value)?;
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

fn run_interactive_tui() -> Result<()> {
    bootstrap_headless_saved_session();
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

    let chromium_cookie = match load_cookie_from_chromium_profile(&settings) {
        Ok(Some(cookie)) if !cookie.trim().is_empty() => cookie,
        Ok(_) | Err(_) => return,
    };

    match NetworkState::validate_cookie(chromium_cookie.clone(), "Chromium cookie auto-sync") {
        NetworkValidationOutcome::Authenticated(_) | NetworkValidationOutcome::Deferred(_) => {
            let _ = store_cookie(&chromium_cookie);
        }
        NetworkValidationOutcome::Rejected => {}
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

pub(crate) fn restore_playback_snapshot_via_daemon(
    snapshot: PlaybackSnapshot,
    resume_playback: bool,
) -> Result<PlaybackSnapshot> {
    let client = DaemonClient::new()?;
    let queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    let mut restored = client.set_volume(snapshot.volume)?;
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

fn resolve_track_input(
    network: &NetworkState,
    input: &str,
    allow_search_fallback: bool,
) -> Result<Track> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("Track input cannot be empty");
    }

    if let Some(video_id) = extract_video_id_from_input(trimmed) {
        return fetch_track_for_video_id(network, &video_id, Some(trimmed)).or_else(|err| {
            if allow_search_fallback {
                resolve_first_search_track(network, trimmed)
                    .with_context(|| format!("Could not resolve `{trimmed}` directly: {err}"))
            } else {
                Err(err)
            }
        });
    }

    if !trimmed.chars().any(char::is_whitespace) {
        match fetch_track_for_video_id(network, trimmed, Some(trimmed)) {
            Ok(track) => return Ok(track),
            Err(err) if !allow_search_fallback => return Err(err),
            Err(_) => {}
        }
    }

    if allow_search_fallback {
        return resolve_first_search_track(network, trimmed);
    }

    bail!("Could not resolve `{trimmed}` as a YouTube URL or video id")
}

fn extract_video_id_from_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let normalized = if trimmed.starts_with("music.youtube.com/")
        || trimmed.starts_with("youtube.com/")
        || trimmed.starts_with("www.youtube.com/")
        || trimmed.starts_with("youtu.be/")
    {
        format!("https://{trimmed}")
    } else {
        trimmed.to_string()
    };

    let parsed = url::Url::parse(&normalized).ok()?;
    let host = parsed.host_str()?.trim_start_matches("www.");

    match host {
        "youtu.be" => parsed
            .path_segments()
            .and_then(|mut segments| segments.next())
            .filter(|segment| !segment.is_empty())
            .map(str::to_string),
        "youtube.com" | "music.youtube.com" => {
            if parsed.path().trim_end_matches('/') == "/watch" {
                return parsed
                    .query_pairs()
                    .find_map(|(key, value)| (key == "v").then_some(value.into_owned()));
            }
            None
        }
        _ => None,
    }
}

fn fetch_track_for_video_id(
    network: &NetworkState,
    video_id: &str,
    fallback_title: Option<&str>,
) -> Result<Track> {
    let mut radio = network
        .get_radio(video_id, None, None)
        .with_context(|| format!("Failed to resolve video `{video_id}`"))?;

    Ok(radio
        .tracks
        .iter()
        .position(|track| track.video_id == video_id)
        .map(|index| radio.tracks.remove(index))
        .or_else(|| radio.tracks.into_iter().next())
        .unwrap_or_else(|| Track {
            video_id: video_id.to_string(),
            title: fallback_title.unwrap_or(video_id).to_string(),
            artists: Vec::new(),
            album: None,
            duration: None,
            thumbnail_url: None,
            playback_playlist_id: None,
            playback_params: None,
            not_interested_feedback_token: None,
            undo_feedback_token: None,
        }))
}

fn resolve_first_search_track(network: &NetworkState, query: &str) -> Result<Track> {
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

fn dedupe_radio_recommendations(prefix: &[Track], tracks: Vec<Track>) -> Vec<Track> {
    let mut seen_ids = prefix
        .iter()
        .map(|track| track.video_id.clone())
        .collect::<HashSet<_>>();
    let mut deduped = Vec::with_capacity(tracks.len());

    for track in tracks {
        if seen_ids.insert(track.video_id.clone()) {
            deduped.push(track);
        }
    }

    deduped
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
    let mut queue = snapshot.queue.clone();
    let mut radio_virtual_playlist = snapshot.radio_virtual_playlist.clone();

    let insert_at = match mode {
        QueueInsertMode::Next => {
            if queue.is_empty() {
                0
            } else {
                snapshot.queue_index.saturating_add(1).min(queue.len())
            }
        }
        QueueInsertMode::Last => radio_virtual_playlist
            .as_ref()
            .map(|radio| radio.touched_len.min(queue.len()))
            .unwrap_or(queue.len()),
    };

    if insert_at >= queue.len() {
        queue.push(track);
    } else {
        queue.insert(insert_at, track);
        extend_radio_touched_for_insert(&mut radio_virtual_playlist, insert_at, 1);
    }

    let queue_index = if queue.is_empty() {
        0
    } else {
        snapshot.queue_index.min(queue.len().saturating_sub(1))
    };

    client.replace_queue(
        queue,
        queue_index,
        snapshot.playlist_context.clone(),
        radio_virtual_playlist,
        snapshot.repeat_mode,
    )
}

fn extend_radio_touched_for_insert(
    radio_virtual_playlist: &mut Option<RadioVirtualPlaylist>,
    insert_at: usize,
    added: usize,
) {
    if added == 0 {
        return;
    }

    if let Some(radio) = radio_virtual_playlist.as_mut() {
        let touched_len = radio.touched_len;
        let inserted_end = insert_at + added;
        if insert_at < touched_len {
            radio.touched_len = touched_len + added;
        } else {
            radio.touched_len = touched_len.max(inserted_end);
        }
    }
}

fn print_queue(snapshot: &PlaybackSnapshot) {
    println!("status: {}", playback_status(snapshot));
    if snapshot.queue.is_empty() {
        println!("Queue is empty.");
        return;
    }

    for (index, track) in snapshot.queue.iter().enumerate() {
        let marker = if index == snapshot.queue_index {
            if snapshot.paused {
                "||"
            } else {
                ">>"
            }
        } else if snapshot
            .radio_virtual_playlist
            .as_ref()
            .is_some_and(|radio| index >= radio.touched_len)
        {
            "~ "
        } else {
            "  "
        };

        println!(
            "{marker} {:>3}. {}",
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
enum ConfigKind {
    Bool,
    U32,
    F64,
    String,
    StringList,
}

#[derive(Clone, Copy)]
struct ConfigSpec {
    name: &'static str,
    kind: ConfigKind,
    description: &'static str,
}

const CONFIG_SPECS: &[ConfigSpec] = &[
    ConfigSpec {
        name: "auto_sync_chromium_cookies",
        kind: ConfigKind::Bool,
        description: "Automatically import cookies from a Chromium-family browser when the saved session is missing or stale.",
    },
    ConfigSpec {
        name: "chromium_cookie_path",
        kind: ConfigKind::String,
        description: "Path to the Chromium cookie database. Leave empty to let Static auto-detect it.",
    },
    ConfigSpec {
        name: "auto_release_track_loops",
        kind: ConfigKind::Bool,
        description: "Convert repeat-one into a finite loop budget for saved resume state so tracks do not loop forever after restore.",
    },
    ConfigSpec {
        name: "auto_release_track_loops_after",
        kind: ConfigKind::U32,
        description: "How many repeat-one loops remain after a resume snapshot is restored when auto release is enabled.",
    },
    ConfigSpec {
        name: "auto_manage_offline_cache",
        kind: ConfigKind::Bool,
        description: "Allow Static to automatically keep or evict offline tracks to stay near the configured cache target.",
    },
    ConfigSpec {
        name: "auto_cache_listen_again",
        kind: ConfigKind::Bool,
        description: "Automatically keep strong Listen again candidates available offline.",
    },
    ConfigSpec {
        name: "listen_again_cache_days",
        kind: ConfigKind::U32,
        description: "How far back Listen again activity is considered when picking tracks for auto-caching.",
    },
    ConfigSpec {
        name: "offline_cache_target",
        kind: ConfigKind::U32,
        description: "Approximate maximum number of automatically managed offline tracks.",
    },
    ConfigSpec {
        name: "offline_rewatch_threshold",
        kind: ConfigKind::U32,
        description: "Minimum historical replay count before a track is considered for heuristic auto-caching.",
    },
    ConfigSpec {
        name: "score_decay_window_days",
        kind: ConfigKind::U32,
        description: "Decay window for the heuristic offline score so newer plays weigh more heavily.",
    },
    ConfigSpec {
        name: "minimum_offline_score",
        kind: ConfigKind::F64,
        description: "Minimum heuristic score a track needs before Static keeps it in the managed offline cache.",
    },
    ConfigSpec {
        name: "prefetch_enabled",
        kind: ConfigKind::Bool,
        description: "Enable ahead-of-time audio prefetching for upcoming queue items.",
    },
    ConfigSpec {
        name: "prefetch_ahead",
        kind: ConfigKind::U32,
        description: "How many upcoming tracks Static should prefetch.",
    },
    ConfigSpec {
        name: "prefer_local_cache_on_playback",
        kind: ConfigKind::Bool,
        description: "Prefer local cached audio over network playback when a track is already available offline.",
    },
    ConfigSpec {
        name: "respect_other_mpris_players",
        kind: ConfigKind::Bool,
        description: "Pause or coordinate with other active MPRIS players when Static starts playback.",
    },
    ConfigSpec {
        name: "pipewire_ducking_enabled",
        kind: ConfigKind::Bool,
        description: "Watch PipeWire activity and duck or pause Static when other audio streams are active for long enough.",
    },
    ConfigSpec {
        name: "pipewire_duck_pause",
        kind: ConfigKind::Bool,
        description: "Pause instead of lowering volume when the PipeWire ducking rule is triggered.",
    },
    ConfigSpec {
        name: "pipewire_ducking_threshold_secs",
        kind: ConfigKind::U32,
        description: "How long another PipeWire stream must stay active before ducking or pausing Static.",
    },
    ConfigSpec {
        name: "pipewire_ducking_volume",
        kind: ConfigKind::F64,
        description: "Duck target volume as a 0.0-1.0 fraction when PipeWire ducking is enabled.",
    },
    ConfigSpec {
        name: "sponsorblock_enabled",
        kind: ConfigKind::Bool,
        description: "Enable SponsorBlock segment loading and skip behavior.",
    },
    ConfigSpec {
        name: "sponsorblock_categories",
        kind: ConfigKind::StringList,
        description: "Comma-separated SponsorBlock category list to skip, such as `sponsor,selfpromo,interaction`.",
    },
];

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
    println!("{}", spec.description);
    Ok(())
}

fn format_config_value(settings: &AppSettings, name: &str) -> Result<String> {
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
        "respect_other_mpris_players" => yes_no_bool(settings.respect_other_mpris_players),
        "pipewire_ducking_enabled" => yes_no_bool(settings.pipewire_ducking_enabled),
        "pipewire_duck_pause" => yes_no_bool(settings.pipewire_duck_pause),
        "pipewire_ducking_threshold_secs" => settings.pipewire_ducking_threshold_secs.to_string(),
        "pipewire_ducking_volume" => settings.pipewire_ducking_volume.to_string(),
        "sponsorblock_enabled" => yes_no_bool(settings.sponsorblock_enabled),
        "sponsorblock_categories" => settings.sponsorblock_categories.join(","),
        other => return Err(anyhow::anyhow!("Unknown config key `{other}`")),
    })
}

fn apply_config_value(settings: &mut AppSettings, name: &str, value: &str) -> Result<()> {
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
        "respect_other_mpris_players" => {
            settings.respect_other_mpris_players = parse_bool(trimmed)?
        }
        "pipewire_ducking_enabled" => settings.pipewire_ducking_enabled = parse_bool(trimmed)?,
        "pipewire_duck_pause" => settings.pipewire_duck_pause = parse_bool(trimmed)?,
        "pipewire_ducking_threshold_secs" => {
            settings.pipewire_ducking_threshold_secs = parse_u32(trimmed)?.clamp(1, 30)
        }
        "pipewire_ducking_volume" => {
            settings.pipewire_ducking_volume = parse_f64(trimmed)?.clamp(0.0, 1.0)
        }
        "sponsorblock_enabled" => settings.sponsorblock_enabled = parse_bool(trimmed)?,
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

fn parse_u32(value: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .with_context(|| format!("Expected an unsigned integer, got `{value}`"))
}

fn parse_f64(value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("Expected a number, got `{value}`"))
}

fn playback_status(snapshot: &PlaybackSnapshot) -> &'static str {
    if snapshot.probe_active {
        "probe"
    } else if snapshot.current_track.is_some() {
        if snapshot.paused {
            "paused"
        } else {
            "playing"
        }
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
    if value {
        "yes"
    } else {
        "no"
    }
}

fn yes_no_bool(value: bool) -> String {
    yes_no(value).to_string()
}

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
