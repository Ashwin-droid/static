use std::path::PathBuf;

use super::super::Ctx;
use super::{Node, TrackField, settings_dir_entries};

fn test_ctx() -> Ctx {
    Ctx::new(PathBuf::from("/nonexistent/static-root"))
}

fn walk_static(node: &Node, all: &mut Vec<Node>) {
    all.push(node.clone());
    for (_, child) in node.static_children() {
        walk_static(&child, all);
    }
}

#[test]
fn static_tree_contains_every_fixed_node() {
    let mut all = Vec::new();
    walk_static(&Node::Root, &mut all);
    // 9 fixed dirs (root, playback, queue, radio, settings, stats, top,
    // tracks, cartridges) + shutdown + 17 playback files + 4 queue files + 3 radio files
    // + fsmod + overview + 4 top files = 40 fixed nodes.
    assert_eq!(all.len(), 40);
    for node in [
        Node::Shutdown,
        Node::CartridgesDir,
        Node::PlaybackVideoName,
        Node::PlaybackLyrics,
        Node::PlaybackSleep,
        Node::PlaybackDesktopAwareVolume,
        Node::QueueClear,
        Node::RadioChips,
        Node::SettingsFsmod,
        Node::StatsTopAlltime,
        Node::StatsTracksDir,
    ] {
        assert!(all.contains(&node), "missing {node:?}");
    }
}

#[test]
fn display_paths_walk_up_to_the_mount_root() {
    assert_eq!(Node::Shutdown.display_path(), "shutdown");
    assert_eq!(
        Node::PlaybackVideoName.display_path(),
        "playback/video_name"
    );
    assert_eq!(Node::QueueItems.display_path(), "playback/queue/items");
    assert_eq!(
        Node::SettingsKey("prefetch_ahead".to_string()).display_path(),
        "settings/prefetch_ahead"
    );
    assert_eq!(
        Node::StatsTrackField("abc123".to_string(), TrackField::PlayCount).display_path(),
        "stats/tracks/abc123/play_count"
    );
}

#[test]
fn permissions_match_contract() {
    assert_eq!(Node::Root.perm(), 0o755);
    assert_eq!(Node::PlaybackDir.perm(), 0o755);
    // read-write
    for node in [
        Node::Shutdown,
        Node::PlaybackState,
        Node::PlaybackTrack,
        Node::PlaybackVideoId,
        Node::PlaybackVideoName,
        Node::PlaybackPosition,
        Node::PlaybackVolume,
        Node::PlaybackRepeat,
        Node::PlaybackNext,
        Node::PlaybackPrev,
        Node::PlaybackRating,
        Node::PlaybackSleep,
        Node::QueueItems,
        Node::RadioChips,
        Node::SettingsFsmod,
        Node::SettingsKey("volume_step".to_string()),
    ] {
        assert_eq!(node.perm(), 0o644, "{node:?}");
        assert!(node.readable() && node.writable(), "{node:?}");
    }
    // write-only
    for node in [Node::QueueClear, Node::RadioJump] {
        assert_eq!(node.perm(), 0o200, "{node:?}");
        assert!(!node.readable() && node.writable(), "{node:?}");
    }
    // read-only
    for node in [
        Node::PlaybackDuration,
        Node::PlaybackDucked,
        Node::PlaybackActivePeers,
        Node::PlaybackLyrics,
        Node::QueueTouchedLen,
        Node::QueueIndex,
        Node::RadioSeed,
        Node::StatsOverview,
        Node::StatsTopToday,
        Node::StatsTrackField("abc".to_string(), TrackField::PlayCount),
    ] {
        assert_eq!(node.perm(), 0o444, "{node:?}");
        assert!(node.readable() && !node.writable(), "{node:?}");
    }
}

#[test]
fn triggers_are_exactly_prev_clear() {
    let mut all = Vec::new();
    walk_static(&Node::Root, &mut all);
    let triggers: Vec<&Node> = all.iter().filter(|node| node.is_trigger()).collect();
    assert_eq!(triggers, vec![&Node::PlaybackPrev, &Node::QueueClear]);
    assert!(!Node::RadioJump.is_trigger());
    // next left the trigger set when it grew line semantics; its empty
    // write skips via the empty-write command instead.
    assert!(!Node::PlaybackNext.is_trigger());
}

#[test]
fn parents_invert_static_children() {
    let mut all = Vec::new();
    walk_static(&Node::Root, &mut all);
    for node in &all {
        for (_, child) in node.static_children() {
            assert_eq!(child.parent(), *node, "child {child:?}");
        }
    }
    assert_eq!(Node::Root.parent(), Node::Root);
    let track_file = Node::StatsTrackField("abc".to_string(), TrackField::DecayScore);
    assert_eq!(track_file.parent(), Node::StatsTrackDir("abc".to_string()));
    assert_eq!(
        Node::StatsTrackDir("abc".to_string()).parent(),
        Node::StatsTracksDir
    );
}

#[test]
fn track_dir_children_are_the_eight_fields() {
    let ctx = test_ctx();
    let children = Node::StatsTrackDir("abc".to_string())
        .children(&ctx)
        .expect("fixed listing");
    let names: Vec<&str> = children.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "title",
            "artist",
            "play_count",
            "total_listen_secs",
            "last_played",
            "last_recommended",
            "decay_score",
            "cached",
        ]
    );
    for (name, node) in &children {
        assert_eq!(
            Node::StatsTrackDir("abc".to_string())
                .lookup_child(&ctx, name)
                .expect("lookup"),
            *node
        );
    }
}

#[test]
fn lookup_of_unknown_names_is_not_found() {
    let ctx = test_ctx();
    assert!(Node::Root.lookup_child(&ctx, "nope").is_err());
    assert!(Node::PlaybackDir.lookup_child(&ctx, "nope").is_err());
    assert!(
        Node::StatsTrackDir("abc".to_string())
            .lookup_child(&ctx, "rating")
            .is_err(),
        "per-track rating was dropped from the design"
    );
}

#[test]
fn settings_dir_always_lists_fsmod() {
    let ctx = test_ctx();
    // No daemon keys: fsmod must still be there (and daemon key order is
    // preserved ahead of it). Pure entry construction — the daemon round
    // trip lives in settings_files::settings_keys, not here.
    assert_eq!(
        settings_dir_entries(Vec::new()),
        vec![("fsmod".to_string(), Node::SettingsFsmod)]
    );
    assert_eq!(
        settings_dir_entries(vec!["volume_step".to_string(), "audio_quality".to_string()]),
        vec![
            (
                "volume_step".to_string(),
                Node::SettingsKey("volume_step".to_string())
            ),
            (
                "audio_quality".to_string(),
                Node::SettingsKey("audio_quality".to_string())
            ),
            ("fsmod".to_string(), Node::SettingsFsmod),
        ]
    );
    assert_eq!(
        Node::SettingsDir
            .lookup_child(&ctx, "fsmod")
            .expect("lookup"),
        Node::SettingsFsmod
    );
}

#[test]
fn daemon_setting_named_fsmod_is_shadowed_not_listed_twice() {
    // If the daemon ever exposes a setting literally named "fsmod", the
    // local node shadows it (matching lookup_child order) instead of the
    // name appearing twice in readdir.
    let entries = settings_dir_entries(vec!["fsmod".to_string(), "volume_step".to_string()]);
    let fsmod_entries: Vec<&(String, Node)> =
        entries.iter().filter(|(name, _)| name == "fsmod").collect();
    assert_eq!(
        fsmod_entries,
        vec![&("fsmod".to_string(), Node::SettingsFsmod)]
    );
}

#[test]
fn only_state_and_next_give_empty_writes_a_meaning() {
    let mut all = Vec::new();
    walk_static(&Node::Root, &mut all);
    for node in &all {
        let expected = match node {
            Node::PlaybackState => Some("toggle"),
            Node::PlaybackNext => Some("skip"),
            _ => None,
        };
        assert_eq!(node.empty_write_command(), expected, "{node:?}");
    }
}
