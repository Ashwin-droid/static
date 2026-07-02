use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::super::format::{self, FsMode};
use super::super::tree::TrackField;
use super::super::{Ctx, FsError};
use super::{TOP_COLUMNS, decay_score, overview_pairs, top_rows, track_field_value};

/// Fixed "now" so windowed/decay expectations are deterministic.
const NOW: i64 = 1_750_000_000;

/// Real DDL copied from store_offline.rs:110 (initialize_database), so the
/// fixtures exercise the exact production schema.
const DDL: &str = "CREATE TABLE IF NOT EXISTS plays (
        video_id     TEXT PRIMARY KEY,
        title        TEXT NOT NULL,
        artist       TEXT NOT NULL,
        thumbnail_url TEXT,
        play_count   INTEGER NOT NULL DEFAULT 1,
        listen_secs  INTEGER NOT NULL DEFAULT 0,
        last_played  INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS offline (
        video_id   TEXT PRIMARY KEY,
        audio_path TEXT NOT NULL,
        art_path   TEXT NOT NULL DEFAULT '',
        cached_at  INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS offline_sponsorblock (
        video_id      TEXT PRIMARY KEY,
        segments_json TEXT NOT NULL,
        cached_at     INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS listen_again_candidates (
        video_id            TEXT PRIMARY KEY,
        last_recommended_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS app_settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    ALTER TABLE offline ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;";

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "staticfs-stats-tests-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp static root");
    root
}

fn create_db(root: &Path) -> Connection {
    let conn = Connection::open(root.join("data.db")).expect("open test db");
    conn.execute_batch(DDL).expect("apply schema");
    conn
}

fn insert_play(
    conn: &Connection,
    video_id: &str,
    title: &str,
    artist: &str,
    play_count: i64,
    listen_secs: i64,
    last_played: i64,
) {
    conn.execute(
        "INSERT INTO plays (video_id, title, artist, play_count, listen_secs, last_played)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            video_id,
            title,
            artist,
            play_count,
            listen_secs,
            last_played
        ],
    )
    .expect("insert play row");
}

/// Standard fixture: five plays rows spanning every top window, one
/// listen_again candidate, one cached track (file on disk) and one offline
/// row whose audio file is missing.
fn fixture(name: &str) -> (Ctx, PathBuf) {
    let root = temp_root(name);
    let conn = create_db(&root);
    insert_play(&conn, "vid_a", "Alpha", "Artist A", 10, 3600, NOW - 100);
    insert_play(
        &conn,
        "vid_b",
        "Beta",
        "Artist B",
        10,
        7200,
        NOW - 2 * 86_400,
    );
    insert_play(
        &conn,
        "vid_c",
        "Gamma",
        "Artist C",
        3,
        50,
        NOW - 10 * 86_400,
    );
    insert_play(
        &conn,
        "vid_d",
        "Delta",
        "Artist D",
        1,
        10,
        NOW - 40 * 86_400,
    );
    insert_play(&conn, "vid_e", "Echo", "Artist E", 2, 5, NOW - 86_400);
    // Metadata-only row: play_count 0, never played.
    insert_play(&conn, "vid_meta", "Meta", "Artist M", 0, 0, 0);
    conn.execute(
        "INSERT INTO listen_again_candidates (video_id, last_recommended_at) VALUES (?1, ?2)",
        rusqlite::params!["vid_a", NOW - 500],
    )
    .expect("insert candidate");
    let audio_path = root.join("audio.opus");
    std::fs::write(&audio_path, b"opus").expect("write cached audio file");
    conn.execute(
        "INSERT INTO offline (video_id, audio_path, cached_at) VALUES (?1, ?2, ?3)",
        rusqlite::params!["vid_a", audio_path.to_string_lossy(), NOW],
    )
    .expect("insert offline row");
    conn.execute(
        "INSERT INTO offline (video_id, audio_path, cached_at) VALUES (?1, ?2, ?3)",
        rusqlite::params!["vid_c", "/nonexistent/staticfs-test/audio.opus", NOW],
    )
    .expect("insert offline row with missing file");
    (Ctx::new(root.clone()), root)
}

fn read_field(ctx: &Ctx, video_id: &str, field: TrackField) -> Vec<u8> {
    super::read_track_field(ctx, FsMode::Awk, video_id, field).expect("track field read")
}

/// Test-local reimplementation of history_offline.rs:132-135 for parity
/// checks against the production formula.
fn expected_decay(play_count: i64, last_played: i64, now: i64, window_days: u32) -> f64 {
    let days_since = ((now - last_played).max(0) as f64) / 86_400.0;
    play_count as f64 / (1.0 + (days_since / window_days.max(1) as f64))
}

#[test]
fn track_ids_list_all_plays_rows_including_metadata_only() {
    let (ctx, _root) = fixture("track_ids");
    let ids = super::stats_track_ids(&ctx).expect("track ids");
    assert_eq!(
        ids,
        vec!["vid_a", "vid_b", "vid_c", "vid_d", "vid_e", "vid_meta"]
    );
}

#[test]
fn missing_db_is_io_until_it_appears() {
    let root = temp_root("missing_db");
    let ctx = Ctx::new(root.clone());
    match super::stats_track_ids(&ctx) {
        Err(FsError::Io(_)) => {}
        other => panic!("expected Io for missing data.db, got {other:?}"),
    }
    match super::read_overview(&ctx, FsMode::Awk) {
        Err(FsError::Io(_)) => {}
        other => panic!("expected Io for missing data.db, got {other:?}"),
    }
    // The db appearing after mount starts working without a remount.
    let conn = create_db(&root);
    insert_play(&conn, "vid_late", "Late", "Artist L", 1, 5, NOW);
    let ids = super::stats_track_ids(&ctx).expect("track ids after db appears");
    assert_eq!(ids, vec!["vid_late"]);
}

#[test]
fn unknown_video_id_is_not_found() {
    let (ctx, _root) = fixture("unknown_id");
    for field in TrackField::ALL {
        match super::read_track_field(&ctx, FsMode::Awk, "vid_nope", field) {
            Err(FsError::NotFound) => {}
            other => panic!("expected NotFound for {field:?}, got {other:?}"),
        }
    }
}

#[test]
fn track_field_scalars_render_raw_value_plus_newline() {
    let (ctx, _root) = fixture("track_fields");
    assert_eq!(read_field(&ctx, "vid_a", TrackField::Title), b"Alpha\n");
    assert_eq!(read_field(&ctx, "vid_a", TrackField::Artist), b"Artist A\n");
    assert_eq!(read_field(&ctx, "vid_a", TrackField::PlayCount), b"10\n");
    assert_eq!(
        read_field(&ctx, "vid_a", TrackField::TotalListenSecs),
        b"3600\n"
    );
    assert_eq!(
        read_field(&ctx, "vid_a", TrackField::LastPlayed),
        format!("{}\n", NOW - 100).into_bytes()
    );
    assert_eq!(
        read_field(&ctx, "vid_a", TrackField::LastRecommended),
        format!("{}\n", NOW - 500).into_bytes()
    );
    // Scalars ignore fsmod: jq mode renders identically.
    assert_eq!(
        super::read_track_field(&ctx, FsMode::Jq, "vid_a", TrackField::PlayCount).unwrap(),
        b"10\n"
    );
}

#[test]
fn empty_content_cases() {
    let (ctx, _root) = fixture("empty_cases");
    // last_played == 0 -> empty content (no newline).
    assert_eq!(read_field(&ctx, "vid_meta", TrackField::LastPlayed), b"");
    // No listen_again_candidates row -> empty content.
    assert_eq!(read_field(&ctx, "vid_b", TrackField::LastRecommended), b"");
}

#[test]
fn cached_requires_offline_row_and_file_on_disk() {
    let (ctx, _root) = fixture("cached");
    // Offline row + file exists.
    assert_eq!(read_field(&ctx, "vid_a", TrackField::Cached), b"true\n");
    // No offline row.
    assert_eq!(read_field(&ctx, "vid_b", TrackField::Cached), b"false\n");
    // Offline row but audio_path missing on disk.
    assert_eq!(read_field(&ctx, "vid_c", TrackField::Cached), b"false\n");
}

#[test]
fn decay_score_matches_history_offline_formula() {
    let (ctx, _root) = fixture("decay_default");
    let snapshot = ctx.stats.snapshot().expect("snapshot");
    // No app_settings row -> default window 30.
    assert_eq!(snapshot.decay_window_days, 30);
    for (video_id, play_count, last_played) in [
        ("vid_a", 10, NOW - 100),
        ("vid_b", 10, NOW - 2 * 86_400),
        ("vid_c", 3, NOW - 10 * 86_400),
        ("vid_d", 1, NOW - 40 * 86_400),
        ("vid_meta", 0, 0),
    ] {
        let value = track_field_value(&snapshot, video_id, TrackField::DecayScore, NOW)
            .expect("decay value");
        assert_eq!(
            value,
            format!("{:.4}", expected_decay(play_count, last_played, NOW, 30)),
            "decay parity for {video_id}"
        );
    }
    // Future last_played clamps days_since to 0: score == play_count.
    let value =
        track_field_value(&snapshot, "vid_a", TrackField::DecayScore, NOW - 10_000).unwrap();
    assert_eq!(value, "10.0000");
    // The public read path (epoch now) agrees with the formula within the
    // skew a test run can introduce.
    let raw = read_field(&ctx, "vid_a", TrackField::DecayScore);
    let actual: f64 = String::from_utf8(raw).unwrap().trim().parse().unwrap();
    let expected = expected_decay(10, NOW - 100, super::epoch_secs(), 30);
    assert!(
        (actual - expected).abs() < 1e-3,
        "decay via read_track_field: {actual} vs {expected}"
    );
}

#[test]
fn decay_window_reads_app_settings_with_floor_of_one() {
    let root = temp_root("decay_window");
    let conn = create_db(&root);
    insert_play(
        &conn,
        "vid_a",
        "Alpha",
        "Artist A",
        8,
        900,
        NOW - 5 * 86_400,
    );
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES ('score_decay_window_days', '14')",
        [],
    )
    .expect("insert setting");
    let ctx = Ctx::new(root.clone());
    let snapshot = ctx.stats.snapshot().expect("snapshot");
    assert_eq!(snapshot.decay_window_days, 14);
    let value = track_field_value(&snapshot, "vid_a", TrackField::DecayScore, NOW).unwrap();
    assert_eq!(
        value,
        format!("{:.4}", expected_decay(8, NOW - 5 * 86_400, NOW, 14))
    );
    // A degenerate stored value of 0 is floored to 1 (max(window, 1)),
    // and an unparseable value falls back to the default 30.
    assert_eq!(
        format!("{:.4}", decay_score(8, NOW - 5 * 86_400, NOW, 0)),
        format!("{:.4}", expected_decay(8, NOW - 5 * 86_400, NOW, 1))
    );
}

#[test]
fn unparseable_decay_window_falls_back_to_default() {
    let root = temp_root("decay_window_junk");
    let conn = create_db(&root);
    insert_play(&conn, "vid_a", "Alpha", "Artist A", 8, 900, NOW);
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES ('score_decay_window_days', 'junk')",
        [],
    )
    .expect("insert setting");
    let ctx = Ctx::new(root);
    let snapshot = ctx.stats.snapshot().expect("snapshot");
    assert_eq!(snapshot.decay_window_days, 30);
}

#[test]
fn overview_reports_totals_in_contract_order() {
    let (ctx, _root) = fixture("overview");
    let snapshot = ctx.stats.snapshot().expect("snapshot");
    // listen_secs 3600+7200+50+10+5 = 10865 -> 3.0179 hours -> "3.0".
    let expected = vec![
        ("listen_hours", "3.0".to_string()),
        ("listen_secs", "10865".to_string()),
        ("total_plays", "26".to_string()),
        ("unique_tracks", "5".to_string()),
        ("cached_tracks", "2".to_string()),
    ];
    assert_eq!(overview_pairs(&snapshot), expected);
    // Byte parity with the shared serializer in both modes.
    for mode in [FsMode::Awk, FsMode::Jq] {
        assert_eq!(
            super::read_overview(&ctx, mode).expect("overview"),
            format::key_values(mode, &expected)
        );
    }
}

#[test]
fn overview_of_empty_db_is_all_zeroes() {
    let root = temp_root("overview_empty");
    create_db(&root);
    let ctx = Ctx::new(root);
    let snapshot = ctx.stats.snapshot().expect("snapshot");
    assert_eq!(
        overview_pairs(&snapshot),
        vec![
            ("listen_hours", "0.0".to_string()),
            ("listen_secs", "0".to_string()),
            ("total_plays", "0".to_string()),
            ("unique_tracks", "0".to_string()),
            ("cached_tracks", "0".to_string()),
        ]
    );
}

#[test]
fn top_windows_filter_on_last_played_and_order_by_plays_then_listen() {
    let (ctx, _root) = fixture("top_windows");
    let snapshot = ctx.stats.snapshot().expect("snapshot");
    let row = |video_id: &str| -> Vec<String> {
        let (title, artist, plays, listen) = match video_id {
            "vid_a" => ("Alpha", "Artist A", 10, 3600),
            "vid_b" => ("Beta", "Artist B", 10, 7200),
            "vid_c" => ("Gamma", "Artist C", 3, 50),
            "vid_d" => ("Delta", "Artist D", 1, 10),
            "vid_e" => ("Echo", "Artist E", 2, 5),
            other => panic!("unknown fixture id {other}"),
        };
        vec![
            title.to_string(),
            artist.to_string(),
            video_id.to_string(),
            plays.to_string(),
            listen.to_string(),
        ]
    };
    // today: window boundary (last_played == now - 86400) is inclusive.
    assert_eq!(
        top_rows(&snapshot, NOW, Some(86_400)),
        vec![row("vid_a"), row("vid_e")]
    );
    // week: the play_count tie (vid_a/vid_b, 10) breaks on listen_secs DESC.
    assert_eq!(
        top_rows(&snapshot, NOW, Some(7 * 86_400)),
        vec![row("vid_b"), row("vid_a"), row("vid_e")]
    );
    assert_eq!(
        top_rows(&snapshot, NOW, Some(30 * 86_400)),
        vec![row("vid_b"), row("vid_a"), row("vid_c"), row("vid_e")]
    );
    // alltime: no window; play_count 0 rows stay excluded.
    let alltime = top_rows(&snapshot, NOW, None);
    assert_eq!(
        alltime,
        vec![
            row("vid_b"),
            row("vid_a"),
            row("vid_c"),
            row("vid_e"),
            row("vid_d"),
        ]
    );
    // Byte parity with the shared serializer (alltime is window-free, so the
    // public read matches the fixed-now expectation exactly).
    for mode in [FsMode::Awk, FsMode::Jq] {
        assert_eq!(
            super::read_top_alltime(&ctx, mode).expect("top alltime"),
            format::list(mode, &TOP_COLUMNS, &alltime)
        );
    }
}

#[test]
fn top_lists_cap_at_100_rows() {
    let root = temp_root("top_limit");
    let conn = create_db(&root);
    for index in 0..120i64 {
        insert_play(
            &conn,
            &format!("vid_{index:03}"),
            &format!("Track {index:03}"),
            "Artist",
            index + 1,
            index,
            NOW,
        );
    }
    let ctx = Ctx::new(root);
    let snapshot = ctx.stats.snapshot().expect("snapshot");
    let rows = top_rows(&snapshot, NOW, None);
    assert_eq!(rows.len(), 100);
    assert_eq!(rows[0][2], "vid_119"); // highest play_count first
    assert_eq!(rows[99][2], "vid_020");
}

#[test]
fn replaced_or_deleted_db_is_detected_after_the_ttl() {
    let root = temp_root("replace_db");
    let conn = create_db(&root);
    insert_play(&conn, "vid_old", "Old", "Artist O", 1, 5, NOW);
    drop(conn);
    let ctx = Ctx::new(root.clone());
    assert_eq!(super::stats_track_ids(&ctx).unwrap(), vec!["vid_old"]);

    // Atomically replace data.db (rename-over, like a backup restore).
    let replacement_path = root.join("data.db.replacement");
    let replacement = Connection::open(&replacement_path).expect("open replacement db");
    replacement.execute_batch(DDL).expect("apply schema");
    insert_play(&replacement, "vid_new", "New", "Artist N", 2, 9, NOW);
    drop(replacement);
    std::fs::rename(&replacement_path, root.join("data.db")).expect("rename over data.db");

    // Once the TTL cache expires the refresh must serve the NEW file, not
    // the old unlinked inode.
    std::thread::sleep(std::time::Duration::from_millis(550));
    assert_eq!(super::stats_track_ids(&ctx).unwrap(), vec!["vid_new"]);

    // Deleting data.db surfaces as Io (EIO) on the next refresh.
    std::fs::remove_file(root.join("data.db")).expect("delete data.db");
    std::thread::sleep(std::time::Duration::from_millis(550));
    match super::stats_track_ids(&ctx) {
        Err(FsError::Io(_)) => {}
        other => panic!("expected Io after data.db was deleted, got {other:?}"),
    }
}

#[test]
fn ttl_cache_collapses_bursts_and_expires() {
    let root = temp_root("ttl_cache");
    let conn = create_db(&root);
    insert_play(&conn, "vid_a", "Alpha", "Artist A", 1, 5, NOW);
    let ctx = Ctx::new(root);
    assert_eq!(super::stats_track_ids(&ctx).unwrap().len(), 1);
    insert_play(&conn, "vid_b", "Beta", "Artist B", 1, 5, NOW);
    // Within the 500ms TTL the snapshot is served from cache.
    assert_eq!(super::stats_track_ids(&ctx).unwrap().len(), 1);
    std::thread::sleep(std::time::Duration::from_millis(550));
    assert_eq!(super::stats_track_ids(&ctx).unwrap().len(), 2);
}
