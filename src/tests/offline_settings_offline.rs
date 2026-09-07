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

#[test]
fn gain_settings_default_to_unmodified_playback() {
    let settings = super::AppSettings::default();
    assert!(!settings.volume_boost_enabled);
    assert!(!settings.dgain_enabled);
    assert_eq!(settings.max_volume(), 1.0);
    assert_eq!(settings.dgain_flatten_percent, 50.0);
    assert_eq!(settings.dgain_ra_seconds, 15.0);
}

#[test]
fn gain_settings_survive_reopening_the_store_and_sanitize_corrupt_numbers() {
    let root = std::env::temp_dir().join(format!("static-gain-settings-{}", uuid::Uuid::new_v4()));
    // An existing destination DB prevents the constructor's production storage migration.
    std::fs::create_dir_all(&root).unwrap();
    drop(rusqlite::Connection::open(root.join("data.db")).unwrap());
    let raw_root = root.to_string_lossy();
    let cache = super::MusicCache::new_with_storage_root(Some(&raw_root)).unwrap();
    // Existing databases without these keys retain the new defaults.
    let initial = cache.load_settings().unwrap();
    assert_eq!(initial.max_volume(), 1.0);
    assert!(!initial.dgain_enabled);
    let settings = super::AppSettings {
        volume_boost_enabled: true,
        dgain_enabled: true,
        dgain_flatten_percent: 37.5,
        dgain_ra_seconds: 0.5,
        ..initial
    };
    cache.save_settings(&settings).unwrap();
    drop(cache);

    let cache = super::MusicCache::new_with_storage_root(Some(&raw_root)).unwrap();
    let loaded = cache.load_settings().unwrap();
    assert_eq!(loaded.max_volume(), 5.0);
    assert!(loaded.dgain_enabled);
    assert_eq!(loaded.dgain_flatten_percent, 37.5);
    assert_eq!(loaded.dgain_ra_seconds, 0.5);

    for (flatten, seconds, expected_flatten, expected_seconds) in [
        ("NaN", "inf", 50.0, 15.0),
        ("-1", "0", 0.0, 0.1),
        ("101", "121", 100.0, 120.0),
    ] {
        for (key, value) in [
            ("dgain_flatten_percent", flatten),
            ("dgain_ra_seconds", seconds),
        ] {
            cache
                .db
                .execute(
                    "UPDATE app_settings SET value = ?1 WHERE key = ?2",
                    rusqlite::params![value, key],
                )
                .unwrap();
        }
        let loaded = cache.load_settings().unwrap();
        assert_eq!(loaded.dgain_flatten_percent, expected_flatten);
        assert_eq!(loaded.dgain_ra_seconds, expected_seconds);
    }
    drop(cache);
    std::fs::remove_dir_all(root).unwrap();
}
