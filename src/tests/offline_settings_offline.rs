use super::{
    CoordinationMode, PIPEWIRE_DUCKING_VOLUME_CLI_MAX, parse_f64_setting_in_range,
    parse_u32_setting_at_least,
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
