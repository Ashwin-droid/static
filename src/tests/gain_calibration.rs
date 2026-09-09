use super::*;

#[test]
fn calibration_settings_validate_all_fields_before_applying() {
    let mut settings = crate::offline::AppSettings::default();
    let baseline = settings.clone();
    let valid = GainCalibrationSettings {
        enabled: true,
        flatten_percent: 40.0,
        ra_seconds: 7.5,
        change_lazyness_percent: 80.0,
        xdgain_enabled: true,
    };
    for invalid in [
        GainCalibrationSettings {
            ra_seconds: 0.0,
            ..valid
        },
        GainCalibrationSettings {
            change_lazyness_percent: f64::NAN,
            ..valid
        },
        GainCalibrationSettings {
            enabled: false,
            ..valid
        },
        GainCalibrationSettings {
            flatten_percent: 101.0,
            ..valid
        },
    ] {
        assert!(invalid.apply_to(&mut settings).is_err());
        assert_eq!(settings.dgain_enabled, baseline.dgain_enabled);
        assert_eq!(settings.dgain_ra_seconds, baseline.dgain_ra_seconds);
        assert_eq!(
            settings.dgain_flatten_percent,
            baseline.dgain_flatten_percent
        );
        assert_eq!(settings.xdgain_enabled, baseline.xdgain_enabled);
    }
    valid.apply_to(&mut settings).unwrap();
    assert!(settings.dgain_enabled && settings.xdgain_enabled);
    assert_eq!(settings.dgain_ra_seconds, 7.5);
    assert_eq!(settings.dgain_flatten_percent, 40.0);
    assert_eq!(settings.dgain_change_lazyness, 80.0);
}
