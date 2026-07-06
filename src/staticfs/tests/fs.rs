use std::path::PathBuf;

use fuser::Errno;

use super::super::Ctx;
use super::super::format::FsMode;
use super::super::tree::Node;
use super::{HandleState, InodeTable, StaticFs, pending_empty_write_command};

fn test_fs() -> StaticFs {
    StaticFs::new(Ctx::new(PathBuf::from("/nonexistent/static-root")))
}

fn handle_for(node: Node) -> HandleState {
    HandleState {
        node,
        buf: Vec::new(),
        triggered: false,
        failed: None,
        wrote: false,
        saw_content: false,
    }
}

#[test]
fn inode_table_preallocates_static_tree_with_root_at_one() {
    let table = InodeTable::new();
    assert_eq!(table.node(1), Some(Node::Root));
    // 37 fixed nodes (see tree tests) preallocated at mount.
    assert_eq!(table.by_ino.len(), 37);
    assert_eq!(table.by_node.len(), 37);
    for (ino, node) in &table.by_ino {
        assert_eq!(table.by_node.get(node), Some(ino));
    }
}

#[test]
fn inode_table_caches_dynamic_nodes() {
    let mut table = InodeTable::new();
    let node = Node::SettingsKey("volume_step".to_string());
    let first = table.get_or_assign(&node);
    let second = table.get_or_assign(&node);
    assert_eq!(first, second);
    assert_eq!(table.node(first), Some(node));
    // Existing assignments never move.
    assert_eq!(table.get_or_assign(&Node::Root), 1);
}

#[test]
fn complete_lines_are_processed_as_they_arrive() {
    let fs = test_fs();
    let mut handle = handle_for(Node::SettingsFsmod);

    // Partial line: buffered, nothing processed yet.
    handle.buf.extend_from_slice(b"j");
    fs.drain_complete_lines(&mut handle).expect("no full line");
    assert_eq!(fs.ctx.fsmod(), FsMode::Awk);
    assert_eq!(handle.buf, b"j");

    // Completing the line processes it as one command.
    handle.buf.extend_from_slice(b"q\n");
    fs.drain_complete_lines(&mut handle).expect("valid fsmod");
    assert_eq!(fs.ctx.fsmod(), FsMode::Jq);
    assert!(handle.buf.is_empty());
}

#[test]
fn first_failing_line_aborts_the_rest() {
    let fs = test_fs();
    let mut handle = handle_for(Node::SettingsFsmod);
    handle.buf.extend_from_slice(b"bogus\nawk\n");
    let errno = fs
        .drain_complete_lines(&mut handle)
        .expect_err("bogus token must fail");
    assert_eq!(i32::from(errno), i32::from(Errno::EINVAL));
    assert_eq!(handle.failed.map(i32::from), Some(i32::from(Errno::EINVAL)));
    // The valid "awk" line after the failure was discarded, not applied.
    assert!(handle.buf.is_empty());
    assert_eq!(fs.ctx.fsmod(), FsMode::Awk);
}

#[test]
fn remainder_is_processed_at_flush_and_empty_lines_are_ignored() {
    let fs = test_fs();
    let mut handle = handle_for(Node::SettingsFsmod);

    // Unterminated remainder processed at flush/release time.
    handle.buf.extend_from_slice(b"jq");
    fs.drain_remainder(&mut handle).expect("valid remainder");
    assert_eq!(fs.ctx.fsmod(), FsMode::Jq);
    assert!(handle.buf.is_empty());

    // Whitespace-only lines are ignored (no EINVAL).
    let mut handle = handle_for(Node::SettingsFsmod);
    handle.buf.extend_from_slice(b"\n  \n");
    fs.drain_complete_lines(&mut handle)
        .expect("empty lines ignored");
    fs.drain_remainder(&mut handle)
        .expect("empty remainder ignored");
    assert_eq!(fs.ctx.fsmod(), FsMode::Jq);
}

#[test]
fn non_utf8_writes_are_invalid() {
    let fs = test_fs();
    let mut handle = handle_for(Node::SettingsFsmod);
    handle.buf.extend_from_slice(&[0xff, 0xfe, b'\n']);
    let errno = fs
        .drain_complete_lines(&mut handle)
        .expect_err("non-UTF-8 write");
    assert_eq!(i32::from(errno), i32::from(Errno::EINVAL));
}

#[test]
fn after_a_failure_the_remainder_is_dropped_silently() {
    let fs = test_fs();
    let mut handle = handle_for(Node::SettingsFsmod);
    handle.failed = Some(Errno::EINVAL);
    handle.buf.extend_from_slice(b"jq");
    fs.drain_remainder(&mut handle)
        .expect("error already reported on write()");
    assert_eq!(fs.ctx.fsmod(), FsMode::Awk);
    assert!(handle.buf.is_empty());
}

#[test]
fn blank_lines_on_playback_state_are_no_ops_not_toggles() {
    // Contract B3: empty (after trim) lines inside a write are ignored;
    // per-line toggling would leave `printf 'pause\n\n'` resumed. No
    // command may be dispatched here (a dispatch would try the daemon).
    let fs = test_fs();
    let mut handle = handle_for(Node::PlaybackState);
    handle.wrote = true;
    handle.buf.extend_from_slice(b"\n  \n");
    fs.drain_complete_lines(&mut handle)
        .expect("blank lines are plain no-ops");
    assert!(!handle.saw_content);
    // The whole write was whitespace-only, so ONE toggle is owed at close.
    assert_eq!(pending_empty_write_command(&handle), Some("toggle"));
}

#[test]
fn empty_write_toggle_fires_only_for_fully_empty_written_handles() {
    // `printf 'pause\n\n'`-style: a non-blank line was seen -> no toggle.
    let mut handle = handle_for(Node::PlaybackState);
    handle.wrote = true;
    handle.saw_content = true;
    assert_eq!(pending_empty_write_command(&handle), None);

    // `echo > playback/state`: an entirely empty write toggles exactly once
    // (the same `triggered` guard the trigger files use).
    let mut handle = handle_for(Node::PlaybackState);
    handle.wrote = true;
    assert_eq!(pending_empty_write_command(&handle), Some("toggle"));
    handle.triggered = true; // set when drain_remainder fires the command
    assert_eq!(pending_empty_write_command(&handle), None);

    // Pure truncate / open-close without any write() never toggles.
    let handle = handle_for(Node::PlaybackState);
    assert_eq!(pending_empty_write_command(&handle), None);

    // Failed handles never fire.
    let mut handle = handle_for(Node::PlaybackState);
    handle.wrote = true;
    handle.failed = Some(Errno::EINVAL);
    assert_eq!(pending_empty_write_command(&handle), None);

    // Only playback/state gives empty writes a meaning.
    let mut handle = handle_for(Node::SettingsFsmod);
    handle.wrote = true;
    assert_eq!(pending_empty_write_command(&handle), None);
}

#[test]
fn saw_content_tracks_non_blank_lines() {
    let fs = test_fs();
    let mut handle = handle_for(Node::SettingsFsmod);
    handle.wrote = true;
    handle.buf.extend_from_slice(b"jq\n");
    fs.drain_complete_lines(&mut handle).expect("valid fsmod");
    assert!(handle.saw_content);

    // The remainder path records content too.
    let mut handle = handle_for(Node::SettingsFsmod);
    handle.wrote = true;
    handle.buf.extend_from_slice(b"awk");
    fs.drain_remainder(&mut handle).expect("valid remainder");
    assert!(handle.saw_content);
}

#[test]
fn content_len_matches_render_and_write_only_files_are_zero() {
    let fs = test_fs();
    // fsmod is locally readable even in the scaffold: "awk\n" = 4 bytes.
    assert_eq!(fs.content_len(&Node::SettingsFsmod).expect("render"), 4);
    assert_eq!(fs.content_len(&Node::QueueClear).expect("write-only"), 0);
    assert_eq!(fs.content_len(&Node::PlaybackDir).expect("dir"), 0);
}
