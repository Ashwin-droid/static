use super::{FsMode, key_values, list, record, scalar};

fn s(value: &str) -> String {
    value.to_string()
}

// -- FsMode -----------------------------------------------------------------

#[test]
fn parse_accepts_only_awk_and_jq() {
    assert_eq!(FsMode::parse("awk"), Some(FsMode::Awk));
    assert_eq!(FsMode::parse("jq"), Some(FsMode::Jq));
    assert_eq!(FsMode::parse(""), None);
    assert_eq!(FsMode::parse("json"), None);
    assert_eq!(FsMode::parse("AWK"), None);
}

#[test]
fn label_round_trips() {
    assert_eq!(FsMode::parse(FsMode::Awk.label()), Some(FsMode::Awk));
    assert_eq!(FsMode::parse(FsMode::Jq.label()), Some(FsMode::Jq));
}

// -- scalar -----------------------------------------------------------------

#[test]
fn scalar_is_raw_value_plus_newline() {
    assert_eq!(scalar("0.75"), b"0.75\n");
    assert_eq!(scalar("playing"), b"playing\n");
}

#[test]
fn scalar_empty_value_is_empty_content() {
    assert_eq!(scalar(""), b"");
}

// -- record -----------------------------------------------------------------

fn track_fields() -> Vec<(&'static str, String)> {
    vec![
        ("title", s("Karma Police")),
        ("artist", s("Radiohead")),
        ("video_id", s("1uYWYWPc9HU")),
    ]
}

#[test]
fn record_awk_is_one_tab_separated_line_without_header() {
    assert_eq!(
        record(FsMode::Awk, &track_fields()),
        b"Karma Police\tRadiohead\t1uYWYWPc9HU\n"
    );
}

#[test]
fn record_jq_is_compact_object_in_field_order() {
    assert_eq!(
        record(FsMode::Jq, &track_fields()),
        b"{\"title\":\"Karma Police\",\"artist\":\"Radiohead\",\"video_id\":\"1uYWYWPc9HU\"}\n"
    );
}

#[test]
fn record_empty_fields_is_empty_content_in_both_modes() {
    assert_eq!(record(FsMode::Awk, &[]), b"");
    assert_eq!(record(FsMode::Jq, &[]), b"");
}

#[test]
fn record_awk_replaces_embedded_tabs_and_newlines_with_spaces() {
    let fields = vec![("title", s("A\tB\nC\rD")), ("artist", s("X"))];
    assert_eq!(record(FsMode::Awk, &fields), b"A B C D\tX\n");
}

#[test]
fn record_jq_preserves_embedded_tabs_and_newlines_escaped() {
    let fields = vec![("title", s("A\tB\nC"))];
    assert_eq!(record(FsMode::Jq, &fields), b"{\"title\":\"A\\tB\\nC\"}\n");
}

// -- list -------------------------------------------------------------------

const PEER_COLUMNS: [&str; 3] = ["kind", "label", "detail"];

fn peer_rows() -> Vec<Vec<String>> {
    vec![
        vec![
            s("mpris"),
            s("Firefox"),
            s("org.mpris.MediaPlayer2.firefox"),
        ],
        vec![s("pipewire"), s("Discord"), s("node 87")],
    ]
}

#[test]
fn list_awk_is_one_record_per_line_without_header() {
    assert_eq!(
        list(FsMode::Awk, &PEER_COLUMNS, &peer_rows()),
        b"mpris\tFirefox\torg.mpris.MediaPlayer2.firefox\npipewire\tDiscord\tnode 87\n"
    );
}

#[test]
fn list_jq_is_array_of_objects_in_column_order() {
    let expected = concat!(
        "[{\"kind\":\"mpris\",\"label\":\"Firefox\",\"detail\":\"org.mpris.MediaPlayer2.firefox\"},",
        "{\"kind\":\"pipewire\",\"label\":\"Discord\",\"detail\":\"node 87\"}]\n"
    );
    assert_eq!(
        list(FsMode::Jq, &PEER_COLUMNS, &peer_rows()),
        expected.as_bytes()
    );
}

#[test]
fn list_empty_rows_is_empty_awk_and_empty_array_jq() {
    assert_eq!(list(FsMode::Awk, &PEER_COLUMNS, &[]), b"");
    assert_eq!(list(FsMode::Jq, &PEER_COLUMNS, &[]), b"[]\n");
}

#[test]
fn list_awk_replaces_embedded_tabs_and_newlines_with_spaces() {
    let rows = vec![vec![s("a\tb"), s("c\nd"), s("e\r\nf")]];
    assert_eq!(list(FsMode::Awk, &PEER_COLUMNS, &rows), b"a b\tc d\te  f\n");
}

#[test]
fn list_jq_escapes_embedded_control_characters() {
    let rows = vec![vec![s("a\tb"), s("c\"d"), s("e\\f")]];
    assert_eq!(
        list(FsMode::Jq, &PEER_COLUMNS, &rows),
        b"[{\"kind\":\"a\\tb\",\"label\":\"c\\\"d\",\"detail\":\"e\\\\f\"}]\n"
    );
}

#[test]
fn list_pads_short_rows_and_ignores_extra_values() {
    let rows = vec![
        vec![s("only-kind")],
        vec![s("k"), s("l"), s("d"), s("extra")],
    ];
    assert_eq!(
        list(FsMode::Awk, &PEER_COLUMNS, &rows),
        b"only-kind\t\t\nk\tl\td\n"
    );
    assert_eq!(
        list(FsMode::Jq, &PEER_COLUMNS, &rows),
        b"[{\"kind\":\"only-kind\",\"label\":\"\",\"detail\":\"\"},{\"kind\":\"k\",\"label\":\"l\",\"detail\":\"d\"}]\n"
    );
}

// -- key_values ---------------------------------------------------------------

fn overview_pairs() -> Vec<(&'static str, String)> {
    vec![
        ("listen_hours", s("12.5")),
        ("listen_secs", s("45000")),
        ("total_plays", s("321")),
        ("unique_tracks", s("87")),
        ("cached_tracks", s("14")),
    ]
}

#[test]
fn key_values_awk_is_key_tab_value_lines() {
    assert_eq!(
        key_values(FsMode::Awk, &overview_pairs()),
        b"listen_hours\t12.5\nlisten_secs\t45000\ntotal_plays\t321\nunique_tracks\t87\ncached_tracks\t14\n"
    );
}

#[test]
fn key_values_jq_is_single_object_in_specced_order() {
    let expected = concat!(
        "{\"listen_hours\":\"12.5\",\"listen_secs\":\"45000\",\"total_plays\":\"321\",",
        "\"unique_tracks\":\"87\",\"cached_tracks\":\"14\"}\n"
    );
    assert_eq!(
        key_values(FsMode::Jq, &overview_pairs()),
        expected.as_bytes()
    );
}

#[test]
fn key_values_empty_pairs_is_empty_awk_and_empty_object_jq() {
    assert_eq!(key_values(FsMode::Awk, &[]), b"");
    assert_eq!(key_values(FsMode::Jq, &[]), b"{}\n");
}

#[test]
fn key_values_awk_replaces_embedded_tabs_and_newlines_with_spaces() {
    let pairs = vec![("k\te", s("v\nal"))];
    assert_eq!(key_values(FsMode::Awk, &pairs), b"k e\tv al\n");
}
