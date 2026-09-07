use std::path::PathBuf;

use super::super::{Ctx, FsError};
use super::{
    SleepCommand, StateCommand, parse_position_millis, parse_rating_token, parse_repeat_token,
    parse_sleep_token, parse_state_token, parse_volume_value, queue_neighbour_record, rating_label,
    read_shutdown, reveal_lyrics, write_shutdown,
};
use crate::ipc::{PlaybackSnapshot, RepeatMode};
use crate::models::{Artist, SyncedLyricLine, SyncedLyrics, SyncedLyricsSource, Track};
use crate::network::SongRating;

#[test]
fn shutdown_read_is_the_liveness_line_and_write_demands_yes() {
    let ctx = Ctx::new(PathBuf::from("/nonexistent/static-root"));
    assert_eq!(
        read_shutdown(&ctx, crate::staticfs::format::FsMode::default()).unwrap(),
        b"if you are reading this then its up\n".to_vec()
    );
    // Anything but "yes" is rejected locally (EINVAL) without touching the
    // daemon; the "yes" path itself needs a live daemon, so it is exercised
    // at runtime rather than here.
    for junk in ["no", "y", "shutdown", "please"] {
        assert!(matches!(
            write_shutdown(&ctx, junk),
            Err(FsError::Invalid(_))
        ));
    }
}

fn queue_track(video_id: &str, title: &str) -> Track {
    Track {
        video_id: video_id.to_string(),
        title: title.to_string(),
        artists: vec![Artist {
            name: "Artist".to_string(),
            id: None,
        }],
        album: None,
        duration: None,
        thumbnail_url: None,
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    }
}

#[test]
fn queue_neighbours_render_and_clamp() {
    let mut snap = PlaybackSnapshot {
        queue: vec![queue_track("a", "A"), queue_track("b", "B")],
        queue_index: 0,
        ..PlaybackSnapshot::default()
    };
    // At the head: next exists, prev does not.
    let next = queue_neighbour_record(&snap, 1);
    assert_eq!(next[0].1, "B");
    assert_eq!(next[2].1, "b");
    assert!(queue_neighbour_record(&snap, -1).is_empty());
    // At the tail: prev exists, next does not.
    snap.queue_index = 1;
    assert!(queue_neighbour_record(&snap, 1).is_empty());
    assert_eq!(queue_neighbour_record(&snap, -1)[2].1, "a");
    // Empty queue: neither.
    snap.queue.clear();
    snap.queue_index = 0;
    assert!(queue_neighbour_record(&snap, 1).is_empty());
    assert!(queue_neighbour_record(&snap, -1).is_empty());
}

#[test]
fn state_tokens_map_to_commands() {
    for token in ["play", "playing", "resume"] {
        assert_eq!(parse_state_token(token), Some(StateCommand::Resume));
    }
    for token in ["pause", "paused"] {
        assert_eq!(parse_state_token(token), Some(StateCommand::Pause));
    }
    assert_eq!(parse_state_token("toggle"), Some(StateCommand::Toggle));
    for junk in ["stop", "PLAY", "play ", "", "1"] {
        assert_eq!(parse_state_token(junk), None, "{junk:?}");
    }
}

#[test]
fn position_accepts_non_negative_ints_and_floats() {
    assert_eq!(parse_position_millis("0"), Some(0));
    assert_eq!(parse_position_millis("42"), Some(42_000));
    assert_eq!(parse_position_millis("1.5"), Some(1_500));
    assert_eq!(parse_position_millis("0.001"), Some(1));
    assert_eq!(parse_position_millis("2e2"), Some(200_000));
}

#[test]
fn position_rejects_nan_negative_and_junk() {
    for junk in ["-1", "-0.5", "NaN", "nan", "inf", "-inf", "abc", "1s", ""] {
        assert_eq!(parse_position_millis(junk), None, "{junk:?}");
    }
}

#[test]
fn volume_accepts_boost_range_for_daemon_validation() {
    assert_eq!(parse_volume_value("0"), Some(0.0));
    assert_eq!(parse_volume_value("0.75"), Some(0.75));
    assert_eq!(parse_volume_value("1"), Some(1.0));
    assert_eq!(parse_volume_value("1.0"), Some(1.0));
    assert_eq!(parse_volume_value("1.01"), Some(1.01));
    assert_eq!(parse_volume_value("5"), Some(5.0));
    for junk in ["5.01", "-0.1", "NaN", "inf", "loud", "", "75%"] {
        assert_eq!(parse_volume_value(junk), None, "{junk:?}");
    }
}

#[test]
fn repeat_tokens_map_to_modes() {
    assert_eq!(parse_repeat_token("off"), Some(RepeatMode::Off));
    assert_eq!(parse_repeat_token("all"), Some(RepeatMode::All));
    assert_eq!(parse_repeat_token("one"), Some(RepeatMode::One));
    for junk in ["Off", "none", "2", ""] {
        assert_eq!(parse_repeat_token(junk), None, "{junk:?}");
    }
}

#[test]
fn rating_tokens_round_trip() {
    assert_eq!(parse_rating_token("liked"), Some(SongRating::Like));
    assert_eq!(parse_rating_token("disliked"), Some(SongRating::Dislike));
    assert_eq!(parse_rating_token("neutral"), Some(SongRating::Indifferent));
    for junk in ["like", "Liked", "meh", ""] {
        assert_eq!(parse_rating_token(junk), None, "{junk:?}");
    }
    assert_eq!(rating_label(SongRating::Like), "liked");
    assert_eq!(rating_label(SongRating::Dislike), "disliked");
    assert_eq!(rating_label(SongRating::Indifferent), "neutral");
}

#[test]
fn sleep_tokens_map_to_commands() {
    assert_eq!(parse_sleep_token("30"), Some(SleepCommand::Minutes(30)));
    assert_eq!(parse_sleep_token("1"), Some(SleepCommand::Minutes(1)));
    assert_eq!(parse_sleep_token("0"), Some(SleepCommand::Clear));
    assert_eq!(parse_sleep_token("off"), Some(SleepCommand::Clear));
    assert_eq!(
        parse_sleep_token("current"),
        Some(SleepCommand::AfterCurrent)
    );
    for junk in ["-5", "1.5", "on", "Off", "Current", "", "10m"] {
        assert_eq!(parse_sleep_token(junk), None, "{junk:?}");
    }
}

fn synthetic_lyrics() -> SyncedLyrics {
    let lines = [
        (0u64, "line zero"),
        (5_000, "line one"),
        (12_500, "line two"),
    ]
    .into_iter()
    .map(|(start_millis, text)| SyncedLyricLine {
        start_millis,
        end_millis: None,
        text: text.to_string(),
    })
    .collect();
    SyncedLyrics {
        source: SyncedLyricsSource::TimedLyrics,
        language_code: None,
        track_name: None,
        lines,
    }
}

#[test]
fn lyrics_reveal_up_to_position() {
    let lyrics = synthetic_lyrics();
    assert_eq!(reveal_lyrics(&lyrics, 0), b"line zero\n");
    assert_eq!(reveal_lyrics(&lyrics, 4_999), b"line zero\n");
    assert_eq!(reveal_lyrics(&lyrics, 5_000), b"line zero\nline one\n");
    assert_eq!(
        reveal_lyrics(&lyrics, 60_000),
        b"line zero\nline one\nline two\n"
    );
}

#[test]
fn lyrics_reveal_is_monotone_in_position() {
    // tail -f relies on the content only growing within a track.
    let lyrics = synthetic_lyrics();
    let mut previous: Vec<u8> = Vec::new();
    for position in [0, 1_000, 5_000, 12_499, 12_500, 100_000] {
        let revealed = reveal_lyrics(&lyrics, position);
        assert!(revealed.starts_with(&previous), "shrank at {position}");
        previous = revealed;
    }
}

#[test]
fn lyrics_reveal_empty_cases() {
    let lyrics = synthetic_lyrics();
    let empty = SyncedLyrics {
        source: SyncedLyricsSource::TimedLyrics,
        language_code: None,
        track_name: None,
        lines: Vec::new(),
    };
    assert!(reveal_lyrics(&empty, 99_999).is_empty());
    // Position before the first line (nothing sung yet) with a nonzero start.
    let late_start = SyncedLyrics {
        lines: lyrics.lines[1..].to_vec(),
        ..lyrics
    };
    assert!(reveal_lyrics(&late_start, 4_000).is_empty());
}
