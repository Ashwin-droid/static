use super::{
    CoordinationFilterMode, CoordinationMode, PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
    coordination_filter_allows, parse_f64_setting_in_range, parse_u32_setting_at_least,
    sanitize_coordination_filter_entries,
};

#[test]
fn parses_coordination_mode_aliases() {
    assert_eq!(
        CoordinationMode::from_str("off"),
        Some(CoordinationMode::Off)
    );
    assert_eq!(
        CoordinationMode::from_str("manual"),
        Some(CoordinationMode::Halt)
    );
    assert_eq!(
        CoordinationMode::from_str("halt-auto"),
        Some(CoordinationMode::HaltAuto)
    );
    assert_eq!(CoordinationMode::from_str("respect"), None);
}

#[test]
fn advanced_u32_settings_preserve_cli_only_values() {
    assert_eq!(parse_u32_setting_at_least("300", 5, 1), 300);
    assert_eq!(parse_u32_setting_at_least("0", 5, 1), 1);
}

#[test]
fn advanced_f64_settings_allow_boosted_values_but_stay_bounded() {
    assert_eq!(
        parse_f64_setting_in_range("1.75", 0.3, 0.0, PIPEWIRE_DUCKING_VOLUME_CLI_MAX),
        1.75
    );
    assert_eq!(
        parse_f64_setting_in_range("9.0", 0.3, 0.0, PIPEWIRE_DUCKING_VOLUME_CLI_MAX),
        PIPEWIRE_DUCKING_VOLUME_CLI_MAX
    );
}

#[test]
fn parses_coordination_filter_modes() {
    assert_eq!(
        CoordinationFilterMode::from_str("blacklist"),
        Some(CoordinationFilterMode::Blacklist)
    );
    assert_eq!(
        CoordinationFilterMode::from_str("allowlist"),
        Some(CoordinationFilterMode::Whitelist)
    );
    assert_eq!(CoordinationFilterMode::from_str("maybe"), None);
}

#[test]
fn coordination_filter_entries_are_trimmed_and_deduped() {
    let sanitized = sanitize_coordination_filter_entries(&[
        " Firefox ".to_string(),
        "firefox".to_string(),
        "".to_string(),
        "mpv".to_string(),
    ]);

    assert_eq!(sanitized, vec!["Firefox".to_string(), "mpv".to_string()]);
}

#[test]
fn coordination_filters_support_blacklist_and_whitelist_semantics() {
    let entries = vec!["Firefox".to_string()];
    let identifiers = vec![
        "org.mpris.MediaPlayer2.firefox.instance1".to_string(),
        "firefox".to_string(),
    ];
    let unrelated = vec!["mpv".to_string()];

    assert!(!coordination_filter_allows(
        CoordinationFilterMode::Blacklist,
        &entries,
        &identifiers
    ));
    assert!(coordination_filter_allows(
        CoordinationFilterMode::Blacklist,
        &entries,
        &unrelated
    ));
    assert!(coordination_filter_allows(
        CoordinationFilterMode::Whitelist,
        &entries,
        &identifiers
    ));
    assert!(!coordination_filter_allows(
        CoordinationFilterMode::Whitelist,
        &entries,
        &unrelated
    ));
}
