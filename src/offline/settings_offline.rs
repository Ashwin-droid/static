use anyhow::Result;

use crate::network::{default_skip_categories, sanitize_categories};

use super::store_offline::MusicCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinationMode {
    Off,
    Respect,
    Halt,
    HaltAuto,
}

impl CoordinationMode {
    pub const ALL: [Self; 4] = [Self::Off, Self::Respect, Self::Halt, Self::HaltAuto];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Respect => "respect",
            Self::Halt => "halt",
            Self::HaltAuto => "halt_auto",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Respect => "Respect",
            Self::Halt => "Halt",
            Self::HaltAuto => "Halt Auto",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "ignore" | "disabled" => Some(Self::Off),
            "respect" | "true" | "1" | "yes" | "on" => Some(Self::Respect),
            "halt" | "manual" => Some(Self::Halt),
            "halt_auto" | "halt-auto" | "haltauto" => Some(Self::HaltAuto),
            _ => None,
        }
    }

    pub fn coordinates_peers(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn resumes_after_delay(self) -> bool {
        matches!(self, Self::HaltAuto)
    }
}

pub const OTHER_MPRIS_HALT_AUTO_RESUME_UI_MIN_SECS: u32 = 1;
pub const OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS: u32 = 60;
pub const PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MIN_SECS: u32 = 1;
pub const PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MAX_SECS: u32 = 60;
pub const PIPEWIRE_DUCKING_THRESHOLD_UI_MIN_SECS: u32 = 1;
pub const PIPEWIRE_DUCKING_THRESHOLD_UI_MAX_SECS: u32 = 30;
pub const PIPEWIRE_DUCKING_VOLUME_UI_MIN: f64 = 0.0;
pub const PIPEWIRE_DUCKING_VOLUME_UI_MAX: f64 = 1.0;
pub const PIPEWIRE_DUCKING_VOLUME_CLI_MAX: f64 = 2.0;

#[derive(Debug, Clone)]
pub struct AppSettings {
    pub auto_sync_chromium_cookies: bool,
    pub chromium_cookie_path: String,
    pub auto_release_track_loops: bool,
    pub auto_release_track_loops_after: u32,
    pub auto_manage_offline_cache: bool,
    pub auto_cache_listen_again: bool,
    pub listen_again_cache_days: u32,
    pub offline_cache_target: u32,
    pub offline_rewatch_threshold: u32,
    pub score_decay_window_days: u32,
    pub minimum_offline_score: f64,
    pub prefetch_enabled: bool,
    pub prefetch_ahead: u32,
    pub prefer_local_cache_on_playback: bool,
    pub other_mpris_mode: CoordinationMode,
    pub other_mpris_halt_auto_resume_secs: u32,
    pub pipewire_ducking_enabled: bool,
    pub pipewire_pause_mode: CoordinationMode,
    pub pipewire_pause_halt_auto_resume_secs: u32,
    pub pipewire_ducking_threshold_secs: u32,
    pub pipewire_ducking_volume: f64,
    pub sponsorblock_enabled: bool,
    pub sponsorblock_categories: Vec<String>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            auto_sync_chromium_cookies: true,
            chromium_cookie_path: String::new(),
            auto_release_track_loops: false,
            auto_release_track_loops_after: 1,
            auto_manage_offline_cache: true,
            auto_cache_listen_again: true,
            listen_again_cache_days: 30,
            offline_cache_target: 100,
            offline_rewatch_threshold: 1,
            score_decay_window_days: 30,
            minimum_offline_score: 0.0,
            prefetch_enabled: true,
            prefetch_ahead: 3,
            prefer_local_cache_on_playback: true,
            other_mpris_mode: CoordinationMode::Respect,
            other_mpris_halt_auto_resume_secs: 5,
            pipewire_ducking_enabled: false,
            pipewire_pause_mode: CoordinationMode::Off,
            pipewire_pause_halt_auto_resume_secs: 5,
            pipewire_ducking_threshold_secs: 5,
            pipewire_ducking_volume: 0.3,
            sponsorblock_enabled: true,
            sponsorblock_categories: default_skip_categories(),
        }
    }
}

impl MusicCache {
    pub fn load_settings(&self) -> Result<AppSettings> {
        let mut settings = AppSettings::default();
        let mut loaded_other_mpris_mode = None;
        let mut legacy_respect_other_mpris_players = None;
        let mut loaded_pipewire_pause_mode = None;
        let mut legacy_pipewire_duck_pause = None;
        let mut stmt = self.db.prepare("SELECT key, value FROM app_settings")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        for row in rows.flatten() {
            match row.0.as_str() {
                "auto_sync_chromium_cookies" => {
                    settings.auto_sync_chromium_cookies =
                        parse_bool_setting(&row.1, settings.auto_sync_chromium_cookies);
                }
                "chromium_cookie_path" => {
                    settings.chromium_cookie_path = row.1;
                }
                "auto_release_track_loops" => {
                    settings.auto_release_track_loops =
                        parse_bool_setting(&row.1, settings.auto_release_track_loops);
                }
                "auto_release_track_loops_after" => {
                    settings.auto_release_track_loops_after = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.auto_release_track_loops_after)
                        .clamp(1, 100);
                }
                "auto_manage_offline_cache" => {
                    settings.auto_manage_offline_cache =
                        parse_bool_setting(&row.1, settings.auto_manage_offline_cache);
                }
                "auto_cache_listen_again" => {
                    settings.auto_cache_listen_again =
                        parse_bool_setting(&row.1, settings.auto_cache_listen_again);
                }
                "listen_again_cache_days" => {
                    settings.listen_again_cache_days = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.listen_again_cache_days)
                        .clamp(1, 365);
                }
                "offline_cache_target" => {
                    settings.offline_cache_target = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.offline_cache_target)
                        .clamp(0, 500);
                }
                "offline_rewatch_threshold" => {
                    settings.offline_rewatch_threshold = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.offline_rewatch_threshold)
                        .clamp(0, 50);
                }
                "score_decay_window_days" => {
                    settings.score_decay_window_days = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.score_decay_window_days)
                        .clamp(1, 365);
                }
                "minimum_offline_score" => {
                    settings.minimum_offline_score = row
                        .1
                        .parse::<f64>()
                        .ok()
                        .unwrap_or(settings.minimum_offline_score)
                        .clamp(0.0, 999.0);
                }
                "prefetch_enabled" => {
                    settings.prefetch_enabled =
                        parse_bool_setting(&row.1, settings.prefetch_enabled);
                }
                "prefetch_ahead" => {
                    settings.prefetch_ahead = row
                        .1
                        .parse::<u32>()
                        .ok()
                        .unwrap_or(settings.prefetch_ahead)
                        .clamp(0, 12);
                }
                "prefer_local_cache_on_playback" => {
                    settings.prefer_local_cache_on_playback =
                        parse_bool_setting(&row.1, settings.prefer_local_cache_on_playback);
                }
                "other_mpris_mode" => {
                    loaded_other_mpris_mode = CoordinationMode::from_str(&row.1);
                }
                "other_mpris_halt_auto_resume_secs" => {
                    settings.other_mpris_halt_auto_resume_secs = parse_u32_setting_at_least(
                        &row.1,
                        settings.other_mpris_halt_auto_resume_secs,
                        OTHER_MPRIS_HALT_AUTO_RESUME_UI_MIN_SECS,
                    );
                }
                "respect_other_mpris_players" => {
                    legacy_respect_other_mpris_players = Some(parse_bool_setting(
                        &row.1,
                        settings.other_mpris_mode.coordinates_peers(),
                    ));
                }
                "pipewire_ducking_enabled" => {
                    settings.pipewire_ducking_enabled =
                        parse_bool_setting(&row.1, settings.pipewire_ducking_enabled);
                }
                "pipewire_pause_mode" => {
                    loaded_pipewire_pause_mode = CoordinationMode::from_str(&row.1);
                }
                "pipewire_pause_halt_auto_resume_secs" => {
                    settings.pipewire_pause_halt_auto_resume_secs = parse_u32_setting_at_least(
                        &row.1,
                        settings.pipewire_pause_halt_auto_resume_secs,
                        PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MIN_SECS,
                    );
                }
                "pipewire_duck_pause" => {
                    legacy_pipewire_duck_pause = Some(parse_bool_setting(
                        &row.1,
                        settings.pipewire_pause_mode.is_enabled(),
                    ));
                }
                "pipewire_ducking_threshold_secs" => {
                    settings.pipewire_ducking_threshold_secs = parse_u32_setting_at_least(
                        &row.1,
                        settings.pipewire_ducking_threshold_secs,
                        PIPEWIRE_DUCKING_THRESHOLD_UI_MIN_SECS,
                    );
                }
                "pipewire_ducking_volume" => {
                    settings.pipewire_ducking_volume = parse_f64_setting_in_range(
                        &row.1,
                        settings.pipewire_ducking_volume,
                        PIPEWIRE_DUCKING_VOLUME_UI_MIN,
                        PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
                    );
                }
                "sponsorblock_enabled" => {
                    settings.sponsorblock_enabled =
                        parse_bool_setting(&row.1, settings.sponsorblock_enabled);
                }
                "sponsorblock_categories" => {
                    if let Ok(categories) = serde_json::from_str::<Vec<String>>(&row.1) {
                        settings.sponsorblock_categories = sanitize_categories(&categories);
                    }
                }
                _ => {}
            }
        }

        settings.other_mpris_mode = loaded_other_mpris_mode.unwrap_or_else(|| {
            if legacy_respect_other_mpris_players
                .unwrap_or(settings.other_mpris_mode.coordinates_peers())
            {
                CoordinationMode::Respect
            } else {
                CoordinationMode::Off
            }
        });

        settings.pipewire_pause_mode = loaded_pipewire_pause_mode.unwrap_or_else(|| {
            if legacy_pipewire_duck_pause.unwrap_or(settings.pipewire_pause_mode.is_enabled()) {
                CoordinationMode::Respect
            } else {
                CoordinationMode::Off
            }
        });

        Ok(settings)
    }

    pub fn save_settings(&self, settings: &AppSettings) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        let sponsorblock_categories =
            serde_json::to_string(&sanitize_categories(&settings.sponsorblock_categories))?;
        for (key, value) in [
            (
                "auto_sync_chromium_cookies",
                bool_setting(settings.auto_sync_chromium_cookies),
            ),
            (
                "chromium_cookie_path",
                settings.chromium_cookie_path.clone(),
            ),
            (
                "auto_release_track_loops",
                bool_setting(settings.auto_release_track_loops),
            ),
            (
                "auto_release_track_loops_after",
                settings.auto_release_track_loops_after.to_string(),
            ),
            (
                "auto_manage_offline_cache",
                bool_setting(settings.auto_manage_offline_cache),
            ),
            (
                "auto_cache_listen_again",
                bool_setting(settings.auto_cache_listen_again),
            ),
            (
                "listen_again_cache_days",
                settings.listen_again_cache_days.to_string(),
            ),
            (
                "offline_cache_target",
                settings.offline_cache_target.to_string(),
            ),
            (
                "offline_rewatch_threshold",
                settings.offline_rewatch_threshold.to_string(),
            ),
            (
                "score_decay_window_days",
                settings.score_decay_window_days.to_string(),
            ),
            (
                "minimum_offline_score",
                settings.minimum_offline_score.to_string(),
            ),
            ("prefetch_enabled", bool_setting(settings.prefetch_enabled)),
            ("prefetch_ahead", settings.prefetch_ahead.to_string()),
            (
                "prefer_local_cache_on_playback",
                bool_setting(settings.prefer_local_cache_on_playback),
            ),
            (
                "other_mpris_mode",
                settings.other_mpris_mode.as_str().to_string(),
            ),
            (
                "other_mpris_halt_auto_resume_secs",
                settings.other_mpris_halt_auto_resume_secs.to_string(),
            ),
            (
                "respect_other_mpris_players",
                bool_setting(settings.other_mpris_mode.coordinates_peers()),
            ),
            (
                "pipewire_ducking_enabled",
                bool_setting(settings.pipewire_ducking_enabled),
            ),
            (
                "pipewire_pause_mode",
                settings.pipewire_pause_mode.as_str().to_string(),
            ),
            (
                "pipewire_pause_halt_auto_resume_secs",
                settings.pipewire_pause_halt_auto_resume_secs.to_string(),
            ),
            (
                "pipewire_duck_pause",
                bool_setting(settings.pipewire_pause_mode.is_enabled()),
            ),
            (
                "pipewire_ducking_threshold_secs",
                settings.pipewire_ducking_threshold_secs.to_string(),
            ),
            (
                "pipewire_ducking_volume",
                settings.pipewire_ducking_volume.to_string(),
            ),
            (
                "sponsorblock_enabled",
                bool_setting(settings.sponsorblock_enabled),
            ),
            ("sponsorblock_categories", sponsorblock_categories.clone()),
        ] {
            tx.execute(
                "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn parse_bool_setting(value: &str, default: bool) -> bool {
    match value {
        "1" | "true" | "TRUE" | "yes" | "on" => true,
        "0" | "false" | "FALSE" | "no" | "off" => false,
        _ => default,
    }
}

fn parse_u32_setting_at_least(value: &str, default: u32, minimum: u32) -> u32 {
    value.parse::<u32>().ok().unwrap_or(default).max(minimum)
}

fn parse_f64_setting_in_range(value: &str, default: f64, minimum: f64, maximum: f64) -> f64 {
    match value.parse::<f64>() {
        Ok(parsed) if parsed.is_finite() => parsed.clamp(minimum, maximum),
        _ => default.clamp(minimum, maximum),
    }
}

fn bool_setting(value: bool) -> String {
    if value { "1" } else { "0" }.to_string()
}

#[cfg(test)]
mod tests {
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
            CoordinationMode::from_str("true"),
            Some(CoordinationMode::Respect)
        );
        assert_eq!(
            CoordinationMode::from_str("manual"),
            Some(CoordinationMode::Halt)
        );
        assert_eq!(
            CoordinationMode::from_str("halt-auto"),
            Some(CoordinationMode::HaltAuto)
        );
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
}
