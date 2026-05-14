use super::{
    AppSettings, OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS, PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
    apply_config_value, config_description,
};

#[test]
fn config_set_preserves_cli_only_delay_values() {
    let mut settings = AppSettings::default();

    apply_config_value(&mut settings, "other_mpris_halt_auto_resume_secs", "300").unwrap();
    apply_config_value(&mut settings, "pipewire_pause_halt_auto_resume_secs", "300").unwrap();
    apply_config_value(&mut settings, "pipewire_ducking_threshold_secs", "300").unwrap();

    assert!(settings.other_mpris_halt_auto_resume_secs > OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS);
    assert_eq!(settings.pipewire_pause_halt_auto_resume_secs, 300);
    assert_eq!(settings.pipewire_ducking_threshold_secs, 300);
}

#[test]
fn config_set_supports_boosted_pipewire_ducking_volume() {
    let mut settings = AppSettings::default();

    apply_config_value(&mut settings, "pipewire_ducking_volume", "1.75").unwrap();
    assert!((settings.pipewire_ducking_volume - 1.75).abs() < 0.0001);

    apply_config_value(&mut settings, "pipewire_ducking_volume", "9.0").unwrap();
    assert_eq!(
        settings.pipewire_ducking_volume,
        PIPEWIRE_DUCKING_VOLUME_CLI_MAX
    );
}

#[test]
fn config_descriptions_explain_cli_only_ranges() {
    let description = config_description("pipewire_ducking_volume");

    assert!(description.contains("GTK settings UI range: 0.0-1.0."));
    assert!(description.contains("CLI range: 0.0-2.0."));
    assert!(description.contains("CLI-only"));
}
