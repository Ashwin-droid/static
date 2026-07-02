use std::path::PathBuf;

use super::super::format::FsMode;
use super::super::{Ctx, FsError};
use super::{daemon_keys, read_fsmod, render_setting_value, value_for, write_fsmod};

fn test_ctx() -> Ctx {
    Ctx::new(PathBuf::from("/nonexistent/static-root"))
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn fsmod_defaults_to_awk_and_round_trips_tokens() {
    let ctx = test_ctx();
    assert_eq!(ctx.fsmod(), FsMode::Awk);
    assert_eq!(read_fsmod(&ctx, ctx.fsmod()).unwrap(), b"awk\n");

    write_fsmod(&ctx, "jq").unwrap();
    assert_eq!(ctx.fsmod(), FsMode::Jq);
    assert_eq!(read_fsmod(&ctx, ctx.fsmod()).unwrap(), b"jq\n");

    write_fsmod(&ctx, "awk").unwrap();
    assert_eq!(ctx.fsmod(), FsMode::Awk);
    assert_eq!(read_fsmod(&ctx, ctx.fsmod()).unwrap(), b"awk\n");
}

#[test]
fn fsmod_rejects_unknown_tokens_and_keeps_current_mode() {
    let ctx = test_ctx();
    write_fsmod(&ctx, "jq").unwrap();
    for junk in ["json", "AWK", "Jq", "awk jq", "awk\tjq", ""] {
        match write_fsmod(&ctx, junk) {
            Err(FsError::Invalid(_)) => {}
            other => panic!("expected Invalid for {junk:?}, got {other:?}"),
        }
        assert_eq!(ctx.fsmod(), FsMode::Jq, "mode changed by junk {junk:?}");
    }
}

#[test]
fn fsmod_read_is_scalar_regardless_of_mode() {
    let ctx = test_ctx();
    ctx.set_fsmod(FsMode::Jq);
    // Scalar files never change with fsmod: raw value + "\n" in both modes.
    assert_eq!(read_fsmod(&ctx, FsMode::Awk).unwrap(), b"jq\n");
    assert_eq!(read_fsmod(&ctx, FsMode::Jq).unwrap(), b"jq\n");
}

#[test]
fn daemon_keys_preserve_daemon_order_without_fsmod() {
    let pairs = pairs(&[
        ("volume_step", "0.05"),
        ("audio_quality", "high"),
        ("score_decay_window_days", "30"),
    ]);
    let keys = daemon_keys(&pairs);
    assert_eq!(
        keys,
        ["volume_step", "audio_quality", "score_decay_window_days"]
    );
    assert!(!keys.iter().any(|key| key == "fsmod"));
}

#[test]
fn key_listing_merge_appends_fsmod_last() {
    // tree.rs builds the settings/ readdir as settings_keys(ctx) + "fsmod";
    // replicate the merge over the pure key extraction.
    let pairs = pairs(&[("audio_quality", "high"), ("volume_step", "0.05")]);
    let mut listing = daemon_keys(&pairs);
    listing.push("fsmod".to_string());
    assert_eq!(listing, ["audio_quality", "volume_step", "fsmod"]);
}

#[test]
fn setting_reads_always_render_value_plus_newline() {
    assert_eq!(render_setting_value("high"), b"high\n");
    // B5: value + "\n" even for empty values (no empty-content case is
    // specced for settings/), so `read -r v < settings/<key>` exits 0.
    assert_eq!(render_setting_value(""), b"\n");
}

#[test]
fn value_for_matches_exact_key_only() {
    let pairs = pairs(&[("audio_quality", "high"), ("volume_step", "0.05")]);
    assert_eq!(value_for(&pairs, "audio_quality"), Some("high"));
    assert_eq!(value_for(&pairs, "volume_step"), Some("0.05"));
    assert_eq!(value_for(&pairs, "audio"), None);
    assert_eq!(value_for(&pairs, "fsmod"), None);
}
