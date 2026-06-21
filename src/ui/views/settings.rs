//! Settings page (all cards) + auth screens (onboarding cookie paste, VR
//! Premium device flow, startup cookie restore, Chromium auto-sync, logout).
//!
//! Specs: panels.md §1.3–1.4 + §4 (settings cards,
//! CLI-only out-of-range rule, coordination targets), page_builders.md §2–3 +
//! §5 (onboarding / vr_auth / settings layout), playback.md §7 (auth flows),
//! shell.md §4 + §8.3 (page switching, startup restore).
//! GTK sources: src/ui/window/page_builders.rs, src/ui/window/
//! settings_support.rs, src/ui/window/auth.rs.
//!
//! Invariants:
//! - AGENTS rule: when a saved value is outside the UI-safe range the UI says
//!   "CLI-only" instead of clamping it (panels spec §4.5). NEVER clamp on save.
//! - Settings that affect the daemon must call
//!   `crate::player::notify_daemon_settings_changed()` after persisting.
//! - Onboarding Connect always advances to vr_auth, even when the auth check
//!   fails (offline-first).
//! - Startup restore: Authenticated → activate client + switch to Main;
//!   Rejected → delete keyring cookie; Deferred → stage client, keep page.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use iced::widget::{
    button, column, container, image, pick_list, row, slider, text, text_input, toggler,
};
use iced::{Element, Length, Task};

use crate::ipc::{CoordinationTargetSnapshot, CoordinationTargetsSnapshot};
use crate::network::{SPONSORBLOCK_CATEGORY_OPTIONS, sanitize_categories};
use crate::network_state::{NetworkState, NetworkValidationOutcome};
use crate::offline::{
    AppSettings, CoordinationFilterMode, CoordinationMode,
    OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS, OTHER_MPRIS_HALT_AUTO_RESUME_UI_MIN_SECS,
    PIPEWIRE_DUCKING_THRESHOLD_UI_MAX_SECS, PIPEWIRE_DUCKING_THRESHOLD_UI_MIN_SECS,
    PIPEWIRE_DUCKING_VOLUME_CLI_MAX, PIPEWIRE_DUCKING_VOLUME_UI_MAX,
    PIPEWIRE_DUCKING_VOLUME_UI_MIN, PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MAX_SECS,
    PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MIN_SECS, coordination_filter_contains,
    sanitize_coordination_filter_entries,
};

use super::super::app::{self, App, ContentView, Message, NavItem, Screen};
use super::super::{content_loading, player_bar, sidebar, theme, widgets};

/// Live-refresh cadence of the coordination target popup (GTK 1 s timer).
const COORDINATION_POPUP_REFRESH: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Settings page state
// ---------------------------------------------------------------------------

/// Identifies a boolean setting row (one per switch in the cards).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingToggle {
    PreferLocalCacheOnPlayback,
    PrefetchEnabled,
    AutoManageOfflineCache,
    AutoCacheListenAgain,
    AutoReleaseTrackLoops,
    SponsorblockEnabled,
    PipewireDuckingEnabled,
    AutoSyncChromiumCookies,
}

/// Identifies a numeric setting row (spin/scale controls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingNumber {
    PrefetchAhead,
    OfflineCacheTarget,
    OfflineRewatchThreshold,
    ListenAgainCacheDays,
    ScoreDecayWindowDays,
    MinimumOfflineScore,
    AutoReleaseTrackLoopsAfter,
    OtherMprisHaltAutoResumeSecs,
    PipewirePauseHaltAutoResumeSecs,
    PipewireDuckingThresholdSecs,
    PipewireDuckingVolume,
}

/// Which coordination stack a control belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinationStack {
    Mpris,
    Pipewire,
}

pub struct State {
    /// Working copy persisted via `MusicCache::save_settings` on change.
    pub draft: AppSettings,
    pub storage_root: Option<String>,
    pub storage_root_input: String,
    pub storage_root_status: Option<String>,
    pub default_storage_root: String,
    pub cache_root: Option<String>,
    pub offline_root: Option<String>,
    pub temp_root: Option<String>,
    pub chromium_path_input: String,
    /// Live MPRIS/PipeWire targets for the filter popup (refetched on open).
    pub coordination_targets: Option<CoordinationTargetsSnapshot>,
    pub coordination_targets_unavailable: bool,
    pub coordination_popup_open: Option<CoordinationStack>,
    /// Invalidates the popup's 1 s refresh chain when it closes/reopens.
    pub coordination_popup_serial: u64,
    /// Pending destructive confirmation (eviction warnings, logout).
    pub pending_warning: Option<PendingWarning>,
    pub status: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            draft: AppSettings::default(),
            storage_root: None,
            storage_root_input: String::new(),
            storage_root_status: None,
            default_storage_root: std::env::var("HOME")
                .map(|home| format!("{home}/Music/Static"))
                .unwrap_or_else(|_| "/tmp/Music/Static".to_string()),
            cache_root: None,
            offline_root: None,
            temp_root: None,
            chromium_path_input: String::new(),
            coordination_targets: None,
            coordination_targets_unavailable: false,
            coordination_popup_open: None,
            coordination_popup_serial: 0,
            pending_warning: None,
            status: None,
        }
    }
}

/// A draft change to an eviction-guarded setting awaiting its impact preview
/// (offline-cache settings warn before evicting auto-downloads).
#[derive(Debug, Clone, Copy)]
pub enum EvictionChange {
    Toggle {
        toggle: SettingToggle,
        next: bool,
        previous: bool,
    },
    Number {
        number: SettingNumber,
        next: f64,
        previous: f64,
    },
}

#[derive(Clone)]
pub enum PendingWarning {
    /// Offline-cache eviction warning for a guarded change (revert on cancel).
    EvictionRisk {
        change: EvictionChange,
        impact: usize,
    },
    Logout,
}

/// Refreshed storage paths after a storage-root migration / page open.
#[derive(Clone)]
pub struct LoadedRoots {
    pub storage_root: Option<String>,
    pub cache_root: String,
    pub offline_root: String,
    pub temp_root: String,
}

#[derive(Clone)]
pub enum Msg {
    Opened,
    PathsLoaded(LoadedRoots),
    Toggled(SettingToggle, bool),
    NumberChanged(SettingNumber, f64),
    /// Slider released (PipeWire duck volume) — persist the draft value.
    NumberCommitted(SettingNumber),
    EvictionPreviewed {
        change: EvictionChange,
        impact: usize,
    },
    MprisModeChanged(CoordinationMode),
    PipewirePauseModeChanged(CoordinationMode),
    FilterModeChanged(CoordinationStack, CoordinationFilterMode),
    FilterEntryAdded(CoordinationStack, String),
    FilterEntryRemoved(CoordinationStack, Vec<String>),
    CoordinationPopupToggled(CoordinationStack),
    CoordinationPopupTick {
        serial: u64,
    },
    CoordinationTargetsLoaded {
        serial: u64,
        result: Result<CoordinationTargetsSnapshot, String>,
    },
    SponsorCategoryToggled(String, bool),
    ChromiumCookiePathChanged(String),
    SaveChromiumCookiePath,
    ClearChromiumCookiePath,
    SyncChromiumCookieNow,
    StorageRootChanged(String),
    ApplyStorageRoot,
    ResetStorageRootToDefault,
    StorageRootApplied {
        to_default: bool,
        result: Result<LoadedRoots, String>,
    },
    WarningConfirmed,
    WarningDismissed,
    LogoutPressed,
    LogoutFinished,
    SettingsSaved {
        result: Result<(), String>,
        previous: AppSettings,
    },
}

// ---------------------------------------------------------------------------
// Auth state (onboarding + VR device flow + startup restore)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VrPhase {
    Idle,
    Starting,
    /// Code shown; button reads "Done" and polls on press.
    CodeShown,
    Polling,
}

pub struct AuthState {
    pub cookie_input: String,
    pub connecting: bool,
    pub status: Option<String>,
    pub vr_phase: VrPhase,
    pub vr_device_code: Option<String>,
    pub vr_user_code: Option<String>,
    pub vr_verification_url: Option<String>,
    pub vr_status: Option<String>,
}

impl Default for AuthState {
    fn default() -> Self {
        Self {
            cookie_input: String::new(),
            connecting: false,
            status: None,
            vr_phase: VrPhase::Idle,
            vr_device_code: None,
            vr_user_code: None,
            vr_verification_url: None,
            vr_status: None,
        }
    }
}

/// Startup cookie validation outcome (auth.rs `CookieValidationOutcome`).
#[derive(Clone)]
pub enum RestoreOutcome {
    Authenticated(Arc<NetworkState>),
    Rejected,
    /// Cookie could not be confirmed (e.g. offline) — stage provisionally.
    Deferred(Arc<NetworkState>),
}

#[derive(Clone)]
pub enum AuthMsg {
    CookieInput(String),
    ConnectPressed,
    /// `NetworkState::from_cookie` + `check_auth` + `store_cookie` finished.
    ConnectFinished {
        result: Result<Arc<NetworkState>, String>,
    },
    VrSignInPressed,
    VrFlowStarted {
        result: Result<(String, String, String), String>,
    },
    VrDonePressed,
    VrPollFinished {
        result: Result<String, String>,
    },
    VrSkipPressed,
    /// Startup keyring-cookie validation finished (phase 1).
    KeyringRestoreFinished {
        outcome: RestoreOutcome,
    },
    /// No keyring cookie / rejected — Chromium auto-sync result (phase 2).
    /// `manual: true` when triggered from the settings card's "Sync now".
    ChromiumRestoreFinished {
        outcome: Option<RestoreOutcome>,
        cookie: Option<String>,
        manual: bool,
    },
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

fn onboarding_logo() -> image::Handle {
    static HANDLE: LazyLock<image::Handle> = LazyLock::new(|| {
        image::Handle::from_bytes(include_bytes!("../../../assets/static.png").as_slice())
    });
    HANDLE.clone()
}

/// Onboarding screen (root stack page "onboarding"): centered cookie card.
pub fn onboarding_view(app: &App) -> Element<'_, Message> {
    let auth = &app.auth;

    let logo = container(
        image(onboarding_logo())
            .width(Length::Fixed(96.0))
            .height(Length::Fixed(96.0)),
    )
    .width(Length::Fill)
    .align_x(iced::alignment::Horizontal::Center);

    let cookie_entry = text_input("Paste cookie header here...", &auth.cookie_input)
        .secure(true)
        .on_input(|value| Message::Auth(AuthMsg::CookieInput(value)))
        .on_submit(Message::Auth(AuthMsg::ConnectPressed))
        .style(theme::entry)
        .padding(theme::SPACE_LG)
        .size(theme::SIZE_BODY);

    let connect_btn = button(
        container(
            text(if auth.connecting {
                "Connecting..."
            } else {
                "Connect"
            })
            .size(theme::SIZE_EMPHASIS),
        )
        .width(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center),
    )
    .style(theme::primary_button)
    .padding([theme::SPACE_LG, theme::SPACE_PAGE])
    .width(Length::Fill)
    .on_press_maybe((!auth.connecting).then_some(Message::Auth(AuthMsg::ConnectPressed)));

    let mut card = column![
        logo,
        container(
            text("Static")
                .size(theme::SIZE_DISPLAY)
                .font(theme::FONT_HEAVY)
                .color(theme::TEXT_HEADING)
        )
        .width(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center),
        container(
            text("YouTube Music, natively.")
                .size(theme::SIZE_SUBTITLE)
                .color(theme::TEXT_DIM)
        )
        .width(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center),
        widgets::separator(),
        text("To get started, paste your YouTube Music cookie:")
            .size(theme::SIZE_BODY)
            .color(theme::TEXT),
        text(
            "1. Open music.youtube.com in your browser\n\
             2. Open DevTools (F12) \u{2192} Network tab\n\
             3. Reload the page\n\
             4. Click any request to music.youtube.com\n\
             5. Copy the \u{2018}Cookie\u{2019} header value",
        )
        .size(theme::SIZE_SUB)
        .color(theme::TEXT_DIM),
        cookie_entry,
        connect_btn,
    ]
    .spacing(theme::SPACE_XL);

    if let Some(status) = &auth.status {
        card = card.push(
            text(status.clone())
                .size(theme::SIZE_SUB)
                .color(theme::TEXT_DANGER),
        );
    }

    iced::widget::center(
        container(card)
            .style(theme::onboarding_card)
            .padding(theme::SPACE_PAGE)
            .width(Length::Fixed(480.0)),
    )
    .style(theme::content_area)
    .into()
}

/// VR auth screen (root stack page "vr_auth"): optional Premium device flow.
pub fn vr_auth_view(app: &App) -> Element<'_, Message> {
    let auth = &app.auth;

    let mut card = column![
        container(
            text("Premium Playback (Optional)")
                .size(theme::SIZE_DISPLAY)
                .font(theme::FONT_HEAVY)
                .color(theme::TEXT_HEADING)
        )
        .width(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center),
        container(
            text(
                "If you have YouTube Music Premium, sign in to play\n\
                 premium-only tracks. You can skip this step.",
            )
            .size(theme::SIZE_SUBTITLE)
            .color(theme::TEXT_DIM)
            .align_x(iced::alignment::Horizontal::Center)
        )
        .width(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center),
        widgets::separator(),
    ]
    .spacing(theme::SPACE_XL);

    if let Some(user_code) = &auth.vr_user_code {
        card = card.push(
            container(
                text(user_code.clone())
                    .size(theme::SIZE_HEADER)
                    .color(theme::ACCENT_FG),
            )
            .width(Length::Fill)
            .align_x(iced::alignment::Horizontal::Center),
        );
        if let Some(url) = &auth.vr_verification_url {
            card = card.push(
                text(format!(
                    "1. Go to {url}\n2. Enter the code above\n3. Sign in with your Google \
                     account\n4. Click \"Done\" below when finished"
                ))
                .size(theme::SIZE_SUB)
                .color(theme::TEXT_DIM),
            );
        }
    }

    if let Some(status) = &auth.vr_status {
        card = card.push(
            text(status.clone())
                .size(theme::SIZE_SUB)
                .color(theme::TEXT_DANGER),
        );
    }

    let (sign_in_label, sign_in_msg) = match auth.vr_phase {
        VrPhase::Idle => ("Sign In", Some(Message::Auth(AuthMsg::VrSignInPressed))),
        VrPhase::Starting => ("Starting...", None),
        VrPhase::CodeShown => ("Done", Some(Message::Auth(AuthMsg::VrDonePressed))),
        VrPhase::Polling => ("Checking...", None),
    };

    let buttons = row![
        button(text(sign_in_label).size(theme::SIZE_EMPHASIS))
            .style(theme::primary_button)
            .padding([theme::SPACE_LG, theme::SPACE_PAGE])
            .on_press_maybe(sign_in_msg),
        button(text("Skip").size(theme::SIZE_EMPHASIS))
            .style(theme::secondary_button)
            .padding([theme::SPACE_LG, theme::SPACE_PAGE])
            .on_press(Message::Auth(AuthMsg::VrSkipPressed)),
    ]
    .spacing(theme::SPACE_LG);

    card = card.push(
        container(buttons)
            .width(Length::Fill)
            .align_x(iced::alignment::Horizontal::Center),
    );

    iced::widget::center(
        container(card)
            .style(theme::onboarding_card)
            .padding(theme::SPACE_PAGE)
            .width(Length::Fixed(480.0)),
    )
    .style(theme::content_area)
    .into()
}

/// Settings content view (all cards: Playback & Cache, Coordination,
/// SponsorBlock, Offline Library, Chromium Cookie Import, Session).
pub fn view(app: &App) -> Element<'_, Message> {
    let state = &app.settings_page;
    let draft = &state.draft;

    let hero = column![
        text("Settings")
            .size(theme::SIZE_HERO)
            .font(theme::FONT_HEAVY)
            .color(theme::TEXT_HEADING),
        text(
            "Tune how Static imports cookies, warms up playback, and manages your offline \
             library.",
        )
        .size(theme::SIZE_SUBTITLE)
        .color(theme::TEXT_DIM),
    ]
    .spacing(theme::SPACE_MD);

    let mut page = column![hero].spacing(theme::SPACE_XL).width(Length::Fill);

    if let Some(warning) = &state.pending_warning {
        page = page.push(warning_card(warning));
    }

    page = page
        .push(playback_card(draft))
        .push(coordination_card(state, draft))
        .push(sponsorblock_card(draft))
        .push(offline_card(state, draft))
        .push(import_card(state, draft))
        .push(session_card());

    page.into()
}

// ---------------------------------------------------------------------------
// Settings cards (private view builders)
// ---------------------------------------------------------------------------

fn settings_card<'a>(
    title: &'a str,
    copy: &'a str,
    rows: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut body = column![
        text(title)
            .size(theme::SIZE_SECTION)
            .font(theme::FONT_BOLD)
            .color(theme::TEXT_BRIGHT),
        text(copy).size(theme::SIZE_SUB).color(theme::TEXT_DIM),
    ]
    .spacing(theme::SPACE_MD);
    for element in rows {
        body = body.push(element);
    }
    container(body.spacing(theme::SPACE_LG))
        .style(theme::settings_card)
        .padding(theme::SPACE_XXL)
        .width(Length::Fill)
        .into()
}

fn settings_row<'a>(
    title: &'a str,
    hint: &'a str,
    control: Element<'a, Message>,
) -> Element<'a, Message> {
    settings_row_with_note(title, hint, None, control)
}

/// Row with title + dim hint + trailing control, plus an optional CLI-only
/// note rendered under the hint (GTK `settings-row-cli-note`).
fn settings_row_with_note<'a>(
    title: &'a str,
    hint: &'a str,
    cli_note: Option<String>,
    control: Element<'a, Message>,
) -> Element<'a, Message> {
    let mut copy = column![
        text(title)
            .size(theme::SIZE_SUBTITLE)
            .color(theme::TEXT_BRIGHT),
        text(hint).size(theme::SIZE_SUB).color(theme::TEXT_DIM),
    ]
    .spacing(theme::SPACE_XXS)
    .width(Length::Fill);
    if let Some(note) = cli_note {
        copy = copy.push(
            text(note)
                .size(theme::SIZE_SUB)
                .color(theme::ACCENT_TEXT_DIM),
        );
    }
    row![copy, control]
        .spacing(theme::SPACE_LG)
        .align_y(iced::Alignment::Center)
        .padding([theme::SPACE_XS, 0.0])
        .into()
}

fn playback_card(draft: &AppSettings) -> Element<'_, Message> {
    settings_card(
        "Playback & Cache",
        "Decide when local cache wins and how aggressively Static stages upcoming songs.",
        vec![
            settings_row(
                "Prefer local cache on playback",
                "Play offline or prefetched audio first when it exists, then fall back to \
                 streaming.",
                setting_toggle(
                    draft.prefer_local_cache_on_playback,
                    true,
                    SettingToggle::PreferLocalCacheOnPlayback,
                ),
            ),
            settings_row(
                "Auto-release Track loops",
                "When Track repeat is active, release it back to Off after the current song \
                 loops itself a set number of times.",
                setting_toggle(
                    draft.auto_release_track_loops,
                    true,
                    SettingToggle::AutoReleaseTrackLoops,
                ),
            ),
            settings_row(
                "Auto-release Track loops after",
                "Counts only natural self-repeats of the current track, not seeks, replay \
                 clicks, or manual skips.",
                stepper(
                    SettingNumber::AutoReleaseTrackLoopsAfter,
                    draft.auto_release_track_loops_after as f64,
                    draft.auto_release_track_loops,
                ),
            ),
            settings_row(
                "Prefetch upcoming tracks",
                "Downloads the next few queue items into temp cache for faster skips and \
                 rewinds.",
                setting_toggle(draft.prefetch_enabled, true, SettingToggle::PrefetchEnabled),
            ),
            settings_row(
                "Prefetch lookahead",
                "How many upcoming songs should be staged in temp cache.",
                stepper(
                    SettingNumber::PrefetchAhead,
                    draft.prefetch_ahead as f64,
                    true,
                ),
            ),
        ],
    )
}

fn coordination_card<'a>(state: &'a State, draft: &'a AppSettings) -> Element<'a, Message> {
    let mpris_cli_note = cli_only_spin_note(
        draft.other_mpris_halt_auto_resume_secs,
        OTHER_MPRIS_HALT_AUTO_RESUME_UI_MIN_SECS,
        OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS,
        "other_mpris_halt_auto_resume_secs",
    );
    let mpris_halt_auto_enabled =
        draft.other_mpris_mode.resumes_after_delay() && mpris_cli_note.is_none();

    let pipewire_pause_cli_note = cli_only_spin_note(
        draft.pipewire_pause_halt_auto_resume_secs,
        PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MIN_SECS,
        PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MAX_SECS,
        "pipewire_pause_halt_auto_resume_secs",
    );
    let pipewire_pause_halt_auto_enabled = draft.pipewire_ducking_enabled
        && draft.pipewire_pause_mode.resumes_after_delay()
        && pipewire_pause_cli_note.is_none();

    let threshold_cli_note = cli_only_spin_note(
        draft.pipewire_ducking_threshold_secs,
        PIPEWIRE_DUCKING_THRESHOLD_UI_MIN_SECS,
        PIPEWIRE_DUCKING_THRESHOLD_UI_MAX_SECS,
        "pipewire_ducking_threshold_secs",
    );
    let threshold_enabled = draft.pipewire_ducking_enabled && threshold_cli_note.is_none();

    let volume_cli_note = cli_only_volume_note(draft.pipewire_ducking_volume);
    let volume_enabled = draft.pipewire_ducking_enabled
        && !draft.pipewire_pause_mode.is_enabled()
        && volume_cli_note.is_none();

    settings_card(
        "Coordination settings",
        "Control how Static reacts when other audio apps or streams take over, including \
         whether playback resumes on its own.",
        vec![
            settings_row(
                "MPRIS coordination",
                "Choose whether Static should ignore peer media apps, stay halted, or \
                 auto-resume after they stop.",
                coordination_mode_pick(draft.other_mpris_mode, true, Msg::MprisModeChanged),
            ),
            settings_row_with_note(
                "Halt auto delay",
                "When Halt Auto is selected, wait this long after peer players go quiet before \
                 Static starts again. Use 0 for immediate resume.",
                mpris_cli_note,
                stepper(
                    SettingNumber::OtherMprisHaltAutoResumeSecs,
                    draft.other_mpris_halt_auto_resume_secs as f64,
                    mpris_halt_auto_enabled,
                ),
            ),
            settings_row(
                "MPRIS filter mode",
                "Blacklist ignores listed peer media apps; whitelist coordinates only with \
                 listed apps.",
                filter_mode_pick(draft.other_mpris_filter_mode, CoordinationStack::Mpris),
            ),
            coordination_popup_section(
                state,
                draft,
                CoordinationStack::Mpris,
                "MPRIS player list",
                "Choose apps from the live MPRIS target list.",
                "MPRIS players",
                "Start staticd to discover current MPRIS players.",
            ),
            settings_row(
                "PipeWire audio ducking",
                "Lower Static when non-MPRIS audio streams like games or calls stay active for \
                 a few seconds.",
                setting_toggle(
                    draft.pipewire_ducking_enabled,
                    true,
                    SettingToggle::PipewireDuckingEnabled,
                ),
            ),
            settings_row(
                "PipeWire coordination",
                "Choose whether PipeWire activity should duck Static, halt until you restart \
                 playback, or auto-resume after it stops.",
                coordination_mode_pick(
                    draft.pipewire_pause_mode,
                    draft.pipewire_ducking_enabled,
                    Msg::PipewirePauseModeChanged,
                ),
            ),
            settings_row_with_note(
                "PipeWire halt auto delay",
                "When PipeWire coordination is set to Halt Auto, wait this long after other \
                 audio stops before Static comes back. Use 0 for immediate resume.",
                pipewire_pause_cli_note,
                stepper(
                    SettingNumber::PipewirePauseHaltAutoResumeSecs,
                    draft.pipewire_pause_halt_auto_resume_secs as f64,
                    pipewire_pause_halt_auto_enabled,
                ),
            ),
            settings_row_with_note(
                "PipeWire trigger delay",
                "How long a foreign audio stream must stay active before ducking or pausing \
                 starts.",
                threshold_cli_note,
                stepper(
                    SettingNumber::PipewireDuckingThresholdSecs,
                    draft.pipewire_ducking_threshold_secs as f64,
                    threshold_enabled,
                ),
            ),
            settings_row_with_note(
                "PipeWire duck volume",
                "Temporary playback level while foreign audio is active.",
                volume_cli_note,
                duck_volume_slider(draft.pipewire_ducking_volume, volume_enabled),
            ),
            settings_row(
                "PipeWire filter mode",
                "Blacklist ignores listed audio streams; whitelist coordinates only with \
                 listed streams.",
                filter_mode_pick(draft.pipewire_filter_mode, CoordinationStack::Pipewire),
            ),
            coordination_popup_section(
                state,
                draft,
                CoordinationStack::Pipewire,
                "PipeWire stream list",
                "Choose streams from the live PipeWire target list.",
                "PipeWire streams",
                "Start staticd with PipeWire available to discover current streams.",
            ),
        ],
    )
}

fn sponsorblock_card(draft: &AppSettings) -> Element<'_, Message> {
    let enabled = draft.sponsorblock_enabled;
    let mut rows: Vec<Element<'_, Message>> = vec![
        settings_row(
            "Auto-skip SponsorBlock segments",
            "Skips community-marked sponsor, self-promo, intro/outro, interaction, preview, \
             and music-offtopic sections during playback.",
            setting_toggle(enabled, true, SettingToggle::SponsorblockEnabled),
        ),
        text(
            "Selected categories are skipped automatically, including after MPRIS seeks or \
             manual rewinds into a marked segment.",
        )
        .size(theme::SIZE_SUB)
        .color(if enabled {
            theme::TEXT_DIM
        } else {
            theme::TEXT_GHOST
        })
        .into(),
    ];

    for option in SPONSORBLOCK_CATEGORY_OPTIONS {
        let active = draft
            .sponsorblock_categories
            .iter()
            .any(|category| category == option.key);
        let mut control = toggler(active)
            .size(theme::TOGGLER_SIZE)
            .style(toggler_style);
        if enabled {
            control = control.on_toggle(move |next| {
                Message::Settings(Msg::SponsorCategoryToggled(option.key.to_string(), next))
            });
        }
        rows.push(category_row(
            option.label,
            option.description,
            enabled,
            control.into(),
        ));
    }

    settings_card(
        "SponsorBlock",
        "Choose whether Static should auto-skip community-marked segments, and which \
         categories count.",
        rows,
    )
}

fn category_row<'a>(
    title: &'a str,
    hint: &'a str,
    enabled: bool,
    control: Element<'a, Message>,
) -> Element<'a, Message> {
    let copy = column![
        text(title).size(theme::SIZE_SUBTITLE).color(if enabled {
            theme::TEXT_BRIGHT
        } else {
            theme::TEXT_FAINT
        }),
        text(hint).size(theme::SIZE_SUB).color(if enabled {
            theme::TEXT_DIM
        } else {
            theme::TEXT_GHOST
        }),
    ]
    .spacing(theme::SPACE_XXS)
    .width(Length::Fill);
    row![copy, control]
        .spacing(theme::SPACE_LG)
        .align_y(iced::Alignment::Center)
        .padding([theme::SPACE_XS, 0.0])
        .into()
}

fn offline_card<'a>(state: &'a State, draft: &'a AppSettings) -> Element<'a, Message> {
    let storage_root_controls = row![
        text_input(&state.default_storage_root, &state.storage_root_input)
            .on_input(|value| Message::Settings(Msg::StorageRootChanged(value)))
            .on_submit(Message::Settings(Msg::ApplyStorageRoot))
            .style(theme::entry)
            .padding(theme::SPACE_MD)
            .size(theme::SIZE_BODY)
            .width(Length::Fill),
        button(text("Move").size(theme::SIZE_BODY))
            .style(theme::secondary_button)
            .padding([theme::SPACE_SM, theme::SPACE_XL])
            .on_press(Message::Settings(Msg::ApplyStorageRoot)),
        button(text("Default").size(theme::SIZE_BODY))
            .style(theme::secondary_button)
            .padding([theme::SPACE_SM, theme::SPACE_XL])
            .on_press(Message::Settings(Msg::ResetStorageRootToDefault)),
    ]
    .spacing(theme::SPACE_MD)
    .align_y(iced::Alignment::Center);

    let mut storage_root_block = column![
        text("Static storage root")
            .size(theme::SIZE_SUBTITLE)
            .color(theme::TEXT_BRIGHT),
        text(
            "Moves Static\u{2019}s offline audio, artwork, and temp cache together. Leave \
             blank to fall back to the default root.",
        )
        .size(theme::SIZE_SUB)
        .color(theme::TEXT_DIM),
        storage_root_controls,
    ]
    .spacing(theme::SPACE_MD);
    if let Some(status) = &state.storage_root_status {
        storage_root_block = storage_root_block.push(
            text(status.clone())
                .size(theme::SIZE_SUB)
                .color(theme::TEXT_DIM),
        );
    }

    let mut rows = vec![
        settings_row(
            "Listen again cache",
            "Keep songs from Listen again offline separately from Static's heuristic offline \
             cache.",
            setting_toggle(
                draft.auto_cache_listen_again,
                true,
                SettingToggle::AutoCacheListenAgain,
            ),
        ),
        settings_row(
            "Listen again cache days",
            "How many days to keep Listen again songs offline after they were last recommended \
             or played.",
            stepper(
                SettingNumber::ListenAgainCacheDays,
                draft.listen_again_cache_days as f64,
                true,
            ),
        ),
        settings_row(
            "Offline cache",
            "Uses Static's play-history heuristics to keep a rolling offline library, without \
             touching pinned downloads or Listen again picks.",
            setting_toggle(
                draft.auto_manage_offline_cache,
                true,
                SettingToggle::AutoManageOfflineCache,
            ),
        ),
        settings_row(
            "Offline cache target",
            "How many play-history picks Static should keep ready offline through the \
             heuristic cache.",
            stepper(
                SettingNumber::OfflineCacheTarget,
                draft.offline_cache_target as f64,
                true,
            ),
        ),
        settings_row(
            "Offline after rewatches",
            "How many rewatches a song needs before it becomes eligible for the heuristic \
             offline cache.",
            stepper(
                SettingNumber::OfflineRewatchThreshold,
                draft.offline_rewatch_threshold as f64,
                true,
            ),
        ),
        settings_row(
            "Score decay window",
            "How slowly songs cool off after their last play. Lower means faster decay, higher \
             means old favorites stick around longer.",
            stepper(
                SettingNumber::ScoreDecayWindowDays,
                draft.score_decay_window_days as f64,
                true,
            ),
        ),
        settings_row(
            "Minimum offline score",
            "Anything below this score is ignored for downloads and evicted if it is already \
             auto-downloaded. `0` keeps the current behavior.",
            stepper(
                SettingNumber::MinimumOfflineScore,
                draft.minimum_offline_score,
                true,
            ),
        ),
        storage_root_block.into(),
    ];

    for (label, value) in [
        ("Cache root", &state.cache_root),
        ("Offline store", &state.offline_root),
        ("Temp prefetch cache", &state.temp_root),
    ] {
        rows.push(path_row(label, value.as_deref().unwrap_or("\u{2026}")));
    }

    settings_card(
        "Offline Library",
        "Control Listen again caching separately from Static's heuristic offline cache.",
        rows,
    )
}

fn path_row<'a>(label: &'a str, value: &str) -> Element<'a, Message> {
    row![
        text(label)
            .size(theme::SIZE_SUBTITLE)
            .color(theme::TEXT_BRIGHT),
        container(
            text(value.to_string())
                .size(theme::SIZE_CAPTION)
                .color(theme::TEXT_FAINT)
        )
        .width(Length::Fill)
        .align_x(iced::alignment::Horizontal::Right),
    ]
    .spacing(theme::SPACE_LG)
    .align_y(iced::Alignment::Center)
    .padding([theme::SPACE_XS, 0.0])
    .into()
}

fn import_card<'a>(state: &'a State, draft: &'a AppSettings) -> Element<'a, Message> {
    let path_controls = row![
        text_input(
            "Auto-detect Chromium browser profile",
            &state.chromium_path_input,
        )
        .on_input(|value| Message::Settings(Msg::ChromiumCookiePathChanged(value)))
        .on_submit(Message::Settings(Msg::SaveChromiumCookiePath))
        .style(theme::entry)
        .padding(theme::SPACE_MD)
        .size(theme::SIZE_BODY)
        .width(Length::Fill),
        button(text("Save").size(theme::SIZE_BODY))
            .style(theme::secondary_button)
            .padding([theme::SPACE_SM, theme::SPACE_XL])
            .on_press(Message::Settings(Msg::SaveChromiumCookiePath)),
        button(text("Auto").size(theme::SIZE_BODY))
            .style(theme::secondary_button)
            .padding([theme::SPACE_SM, theme::SPACE_XL])
            .on_press(Message::Settings(Msg::ClearChromiumCookiePath)),
    ]
    .spacing(theme::SPACE_MD)
    .align_y(iced::Alignment::Center);

    let path_block = column![
        text("Chromium cookie DB path")
            .size(theme::SIZE_SUBTITLE)
            .color(theme::TEXT_BRIGHT),
        text(
            "Examples: `~/.config/microsoft-edge/Default/Cookies`, \
             `~/.config/google-chrome/Default/Cookies`, `~/.config/chromium/Default/Cookies`.",
        )
        .size(theme::SIZE_SUB)
        .color(theme::TEXT_DIM),
        path_controls,
    ]
    .spacing(theme::SPACE_MD);

    let mut sync_block = column![
        button(text("Sync now").size(theme::SIZE_BODY))
            .style(theme::secondary_button)
            .padding([theme::SPACE_SM, theme::SPACE_XL])
            .on_press(Message::Settings(Msg::SyncChromiumCookieNow)),
    ]
    .spacing(theme::SPACE_MD);
    if let Some(status) = &state.status {
        sync_block = sync_block.push(
            text(status.clone())
                .size(theme::SIZE_SUB)
                .color(theme::TEXT_DIM),
        );
    }

    settings_card(
        "Chromium Cookie Import",
        "Auto-sync cookies from Edge or another Chromium-family browser. Leave the path blank \
         to auto-detect.",
        vec![
            settings_row(
                "Auto-sync Chromium cookies on startup",
                "If a saved cookie is missing or stale, Static will try your Chromium-family \
                 browser before asking for manual cookies.",
                setting_toggle(
                    draft.auto_sync_chromium_cookies,
                    true,
                    SettingToggle::AutoSyncChromiumCookies,
                ),
            ),
            path_block.into(),
            sync_block.into(),
        ],
    )
}

fn session_card<'a>() -> Element<'a, Message> {
    settings_card(
        "Session",
        "Sign out cleanly without touching your browser\u{2019}s own cookie jar.",
        vec![
            container(
                button(text("Log out").size(theme::SIZE_BODY))
                    .style(theme::danger_button)
                    .padding([theme::SPACE_SM, theme::SPACE_PAGE])
                    .on_press(Message::Settings(Msg::LogoutPressed)),
            )
            .into(),
        ],
    )
}

fn warning_card(warning: &PendingWarning) -> Element<'_, Message> {
    let (title, body, accept_label) = match warning {
        PendingWarning::EvictionRisk { change, impact } => eviction_warning_copy(*change, *impact),
        PendingWarning::Logout => (
            "Log out of YouTube Music?".to_string(),
            "Static will clear its saved session cookie and return to onboarding. Your \
             browser\u{2019}s own cookie jar is untouched."
                .to_string(),
            "Log out".to_string(),
        ),
    };

    container(
        column![
            text(title)
                .size(theme::SIZE_SECTION)
                .font(theme::FONT_BOLD)
                .color(theme::TEXT_BRIGHT),
            text(body).size(theme::SIZE_SUB).color(theme::TEXT_DIM),
            row![
                button(text("Cancel").size(theme::SIZE_BODY))
                    .style(theme::secondary_button)
                    .padding([theme::SPACE_SM, theme::SPACE_XL])
                    .on_press(Message::Settings(Msg::WarningDismissed)),
                button(text(accept_label).size(theme::SIZE_BODY))
                    .style(theme::danger_button)
                    .padding([theme::SPACE_SM, theme::SPACE_XL])
                    .on_press(Message::Settings(Msg::WarningConfirmed)),
            ]
            .spacing(theme::SPACE_LG),
        ]
        .spacing(theme::SPACE_LG),
    )
    .style(warning_card_style)
    .padding(theme::SPACE_XXL)
    .width(Length::Fill)
    .into()
}

fn eviction_warning_copy(change: EvictionChange, impact: usize) -> (String, String, String) {
    let song_label = if impact == 1 { "song" } else { "songs" };
    let accept = format!("Apply and evict {impact} {song_label}");
    let (title, reason) = match change {
        EvictionChange::Toggle {
            toggle: SettingToggle::AutoCacheListenAgain,
            ..
        } => (
            "Turn off Listen again cache?",
            "because they only qualify through Listen again cache right now",
        ),
        EvictionChange::Toggle {
            toggle: SettingToggle::AutoManageOfflineCache,
            ..
        } => (
            "Turn off offline cache?",
            "because they only qualify through the heuristic offline cache right now",
        ),
        EvictionChange::Toggle { .. } => (
            "Apply this change?",
            "because they would no longer qualify for the offline cache",
        ),
        EvictionChange::Number { number, .. } => match number {
            SettingNumber::ListenAgainCacheDays => (
                "Shorten Listen again cache?",
                "because they would fall outside the new Listen again cache window",
            ),
            SettingNumber::OfflineCacheTarget => (
                "Lower the offline cache target?",
                "to honor the smaller heuristic offline library size",
            ),
            SettingNumber::OfflineRewatchThreshold => (
                "Raise the offline replay threshold?",
                "because they no longer qualify for the heuristic offline cache",
            ),
            SettingNumber::ScoreDecayWindowDays => (
                "Tighten score decay?",
                "because the new decay window would knock them below the offline cutoff",
            ),
            SettingNumber::MinimumOfflineScore => (
                "Raise the minimum offline score?",
                "because they would fall below the new score floor",
            ),
            _ => (
                "Apply this change?",
                "because they would no longer qualify for the offline cache",
            ),
        },
    };
    (
        title.to_string(),
        format!("{impact} auto-downloaded {song_label} would be evicted immediately {reason}."),
        accept,
    )
}

// ---------------------------------------------------------------------------
// Controls (private widget builders)
// ---------------------------------------------------------------------------

fn setting_toggle<'a>(active: bool, enabled: bool, toggle: SettingToggle) -> Element<'a, Message> {
    let mut control = toggler(active)
        .size(theme::TOGGLER_SIZE)
        .style(toggler_style);
    if enabled {
        control = control.on_toggle(move |next| Message::Settings(Msg::Toggled(toggle, next)));
    }
    control.into()
}

/// Spin-button replacement: a "−  value  +" stepper bound to the UI range.
/// When the saved value sits outside the UI range the caller passes
/// `enabled: false` and renders the CLI-only note instead (never clamps).
fn stepper<'a>(number: SettingNumber, value: f64, enabled: bool) -> Element<'a, Message> {
    let (min, max, step, decimals) = number_meta(number);
    let display = format!("{value:.decimals$}");

    let dec_target = round_to(value - step, decimals).max(min);
    let inc_target = round_to(value + step, decimals).min(max);
    let dec_msg = (enabled && value > min + f64::EPSILON)
        .then_some(Message::Settings(Msg::NumberChanged(number, dec_target)));
    let inc_msg = (enabled && value < max - f64::EPSILON)
        .then_some(Message::Settings(Msg::NumberChanged(number, inc_target)));

    row![
        button(text("\u{2212}").size(theme::SIZE_BODY))
            .style(theme::secondary_button)
            .padding([theme::SPACE_XS, theme::SPACE_LG])
            .on_press_maybe(dec_msg),
        container(text(display).size(theme::SIZE_BODY).color(if enabled {
            theme::TEXT
        } else {
            theme::TEXT_GHOST
        }))
        .width(Length::Fixed(56.0))
        .align_x(iced::alignment::Horizontal::Center),
        button(text("+").size(theme::SIZE_BODY))
            .style(theme::secondary_button)
            .padding([theme::SPACE_XS, theme::SPACE_LG])
            .on_press_maybe(inc_msg),
    ]
    .align_y(iced::Alignment::Center)
    .spacing(theme::SPACE_XS)
    .into()
}

/// (ui_min, ui_max, step, decimals) per numeric setting — same ranges/steps
/// as the GTK `SpinButton::with_range` calls.
fn number_meta(number: SettingNumber) -> (f64, f64, f64, usize) {
    match number {
        SettingNumber::PrefetchAhead => (0.0, 12.0, 1.0, 0),
        SettingNumber::OfflineCacheTarget => (0.0, 500.0, 5.0, 0),
        SettingNumber::OfflineRewatchThreshold => (0.0, 50.0, 1.0, 0),
        SettingNumber::ListenAgainCacheDays => (1.0, 365.0, 1.0, 0),
        SettingNumber::ScoreDecayWindowDays => (1.0, 365.0, 1.0, 0),
        SettingNumber::MinimumOfflineScore => (0.0, 999.0, 0.1, 1),
        SettingNumber::AutoReleaseTrackLoopsAfter => (1.0, 100.0, 1.0, 0),
        SettingNumber::OtherMprisHaltAutoResumeSecs => (
            OTHER_MPRIS_HALT_AUTO_RESUME_UI_MIN_SECS as f64,
            OTHER_MPRIS_HALT_AUTO_RESUME_UI_MAX_SECS as f64,
            1.0,
            0,
        ),
        SettingNumber::PipewirePauseHaltAutoResumeSecs => (
            PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MIN_SECS as f64,
            PIPEWIRE_PAUSE_HALT_AUTO_RESUME_UI_MAX_SECS as f64,
            1.0,
            0,
        ),
        SettingNumber::PipewireDuckingThresholdSecs => (
            PIPEWIRE_DUCKING_THRESHOLD_UI_MIN_SECS as f64,
            PIPEWIRE_DUCKING_THRESHOLD_UI_MAX_SECS as f64,
            1.0,
            0,
        ),
        SettingNumber::PipewireDuckingVolume => (
            PIPEWIRE_DUCKING_VOLUME_UI_MIN,
            PIPEWIRE_DUCKING_VOLUME_UI_MAX,
            0.05,
            2,
        ),
    }
}

fn round_to(value: f64, decimals: usize) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

fn duck_volume_slider<'a>(value: f64, enabled: bool) -> Element<'a, Message> {
    if !enabled {
        return container(
            text(format!("{value:.2}"))
                .size(theme::SIZE_BODY)
                .color(theme::TEXT_GHOST),
        )
        .width(Length::Fixed(160.0))
        .align_x(iced::alignment::Horizontal::Center)
        .into();
    }
    row![
        slider(
            PIPEWIRE_DUCKING_VOLUME_UI_MIN..=PIPEWIRE_DUCKING_VOLUME_UI_MAX,
            value,
            |next| Message::Settings(Msg::NumberChanged(
                SettingNumber::PipewireDuckingVolume,
                next,
            )),
        )
        .step(0.05)
        .on_release(Message::Settings(Msg::NumberCommitted(
            SettingNumber::PipewireDuckingVolume,
        )))
        .style(duck_slider_style)
        .width(Length::Fixed(160.0)),
        container(
            text(format!("{value:.2}"))
                .size(theme::SIZE_CAPTION)
                .color(theme::TEXT_DIM)
        )
        .width(Length::Fixed(34.0))
        .align_x(iced::alignment::Horizontal::Right),
    ]
    .spacing(theme::SPACE_MD)
    .align_y(iced::Alignment::Center)
    .into()
}

/// `pick_list` wrapper for the foreign `CoordinationMode` (needs `Display`).
#[derive(Clone, Copy, PartialEq, Eq)]
struct ModeOption(CoordinationMode);

impl std::fmt::Display for ModeOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.label())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FilterModeOption(CoordinationFilterMode);

impl std::fmt::Display for FilterModeOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.label())
    }
}

fn coordination_mode_pick<'a>(
    current: CoordinationMode,
    enabled: bool,
    to_msg: fn(CoordinationMode) -> Msg,
) -> Element<'a, Message> {
    if !enabled {
        return disabled_pick_display(current.label());
    }
    let options: Vec<ModeOption> = CoordinationMode::ALL
        .iter()
        .copied()
        .map(ModeOption)
        .collect();
    pick_list(options, Some(ModeOption(current)), move |option| {
        Message::Settings(to_msg(option.0))
    })
    .text_size(theme::SIZE_BODY)
    .padding([theme::SPACE_SM, theme::SPACE_LG])
    .style(pick_style)
    .width(Length::Fixed(130.0))
    .into()
}

fn filter_mode_pick<'a>(
    current: CoordinationFilterMode,
    stack: CoordinationStack,
) -> Element<'a, Message> {
    let options: Vec<FilterModeOption> = CoordinationFilterMode::ALL
        .iter()
        .copied()
        .map(FilterModeOption)
        .collect();
    pick_list(options, Some(FilterModeOption(current)), move |option| {
        Message::Settings(Msg::FilterModeChanged(stack, option.0))
    })
    .text_size(theme::SIZE_BODY)
    .padding([theme::SPACE_SM, theme::SPACE_LG])
    .style(pick_style)
    .width(Length::Fixed(130.0))
    .into()
}

fn disabled_pick_display<'a>(label: &'a str) -> Element<'a, Message> {
    container(text(label).size(theme::SIZE_BODY).color(theme::TEXT_GHOST))
        .padding([theme::SPACE_SM, theme::SPACE_LG])
        .style(|_t| container::Style {
            background: Some(theme::white(0.03).into()),
            border: iced::Border {
                color: theme::BORDER_SUBTLE,
                width: 1.0,
                radius: theme::RADIUS_MD.into(),
            },
            ..container::Style::default()
        })
        .width(Length::Fixed(130.0))
        .align_x(iced::alignment::Horizontal::Center)
        .into()
}

/// "Choose..." row + (when open) the inline live target panel. GTK used a
/// floating `Popover` refreshed on a 1 s timer; iced has no anchored popover,
/// so the panel expands inline below the row with the same refresh loop.
fn coordination_popup_section<'a>(
    state: &'a State,
    draft: &'a AppSettings,
    stack: CoordinationStack,
    title: &'a str,
    hint: &'a str,
    popup_title: &'a str,
    unavailable_text: &'a str,
) -> Element<'a, Message> {
    let open = state.coordination_popup_open == Some(stack);
    let choose = button(text(if open { "Close" } else { "Choose..." }).size(theme::SIZE_BODY))
        .style(theme::secondary_button)
        .padding([theme::SPACE_SM, theme::SPACE_XL])
        .on_press(Message::Settings(Msg::CoordinationPopupToggled(stack)));

    let mut section = column![settings_row(title, hint, choose.into())].spacing(theme::SPACE_MD);

    if open {
        let mut panel = column![
            text(popup_title)
                .size(theme::SIZE_SUBTITLE)
                .color(theme::TEXT_BRIGHT),
            text("This list refreshes while it is open. Switches edit the current filter list.")
                .size(theme::SIZE_SUB)
                .color(theme::TEXT_DIM),
        ]
        .spacing(theme::SPACE_MD);

        if state.coordination_targets_unavailable {
            panel = panel.push(widgets::dim_label(unavailable_text));
        } else {
            match &state.coordination_targets {
                None => panel = panel.push(widgets::dim_label("Loading targets...")),
                Some(targets) => {
                    let targets = match stack {
                        CoordinationStack::Mpris => &targets.mpris,
                        CoordinationStack::Pipewire => &targets.pipewire,
                    };
                    if targets.is_empty() {
                        panel = panel.push(widgets::dim_label(
                            "No current targets found. Play audio in another app and this \
                             popup will refresh.",
                        ));
                    } else {
                        for target in targets {
                            panel = panel.push(coordination_target_row(draft, stack, target));
                        }
                    }
                }
            }
        }

        section = section.push(
            container(panel)
                .style(theme::popover)
                .padding(theme::SPACE_XL)
                .width(Length::Fill),
        );
    }

    section.into()
}

fn coordination_target_row<'a>(
    draft: &'a AppSettings,
    stack: CoordinationStack,
    target: &'a CoordinationTargetSnapshot,
) -> Element<'a, Message> {
    let detail = if !target.filterable {
        if target.detail.is_empty() {
            "No stable filter key exposed.".to_string()
        } else {
            format!("{} | no stable filter key", target.detail)
        }
    } else {
        let key_part = if target.detail.is_empty() {
            format!("key={}", target.key)
        } else {
            format!("{} | key={}", target.detail, target.key)
        };
        // Synthesized configured-but-offline entry (see pipewire_target_snapshots):
        // flag it so the user knows it is listed even though nothing is playing.
        if target.active {
            key_part
        } else {
            format!("not currently active | {key_part}")
        }
    };

    let listed = target.filterable && target_is_listed(draft, stack, target);
    let mut control = toggler(listed)
        .size(theme::TOGGLER_SIZE)
        .style(toggler_style);
    if target.filterable {
        let key = target.key.clone();
        let label = target.label.clone();
        control = control.on_toggle(move |next| {
            Message::Settings(if next {
                Msg::FilterEntryAdded(stack, key.clone())
            } else {
                Msg::FilterEntryRemoved(stack, vec![key.clone(), label.clone()])
            })
        });
    }

    let copy = column![
        text(target.label.as_str())
            .size(theme::SIZE_SUBTITLE)
            .color(theme::TEXT_BRIGHT),
        text(detail).size(theme::SIZE_SUB).color(theme::TEXT_DIM),
    ]
    .spacing(theme::SPACE_XXS)
    .width(Length::Fill);

    row![copy, control]
        .spacing(theme::SPACE_LG)
        .align_y(iced::Alignment::Center)
        .into()
}

// ---------------------------------------------------------------------------
// CLI-only out-of-range notes (AGENTS rule: communicate, never clamp)
// ---------------------------------------------------------------------------

fn cli_only_spin_note(current: u32, ui_min: u32, ui_max: u32, cli_key: &str) -> Option<String> {
    if (ui_min..=ui_max).contains(&current) {
        return None;
    }
    Some(format!(
        "CLI only right now: currently set to {current} seconds. The settings UI only edits \
         {ui_min}-{ui_max} seconds for this control. Use `static config set {cli_key} \
         <seconds>` to keep an advanced value, or move it back into the UI range to re-enable \
         this card."
    ))
}

fn cli_only_volume_note(current: f64) -> Option<String> {
    let clamped = current.clamp(
        PIPEWIRE_DUCKING_VOLUME_UI_MIN,
        PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
    );
    let within_ui = clamped.is_finite()
        && clamped >= PIPEWIRE_DUCKING_VOLUME_UI_MIN - 0.000_001
        && clamped <= PIPEWIRE_DUCKING_VOLUME_UI_MAX + 0.000_001;
    if within_ui {
        return None;
    }
    Some(format!(
        "CLI only right now: currently set to {current:.2}. The settings UI only edits \
         {:.1}-{:.1} for this control. Use `static config set pipewire_ducking_volume <value>` \
         to keep an advanced value, or move it back into the UI range to re-enable this card. \
         The CLI supports up to {:.1} here on purpose.",
        PIPEWIRE_DUCKING_VOLUME_UI_MIN,
        PIPEWIRE_DUCKING_VOLUME_UI_MAX,
        PIPEWIRE_DUCKING_VOLUME_CLI_MAX,
    ))
}

// ---------------------------------------------------------------------------
// Settings update
// ---------------------------------------------------------------------------

pub fn update(app: &mut App, msg: Msg) -> Task<Message> {
    match msg {
        Msg::Opened => {
            app.settings_page.draft = app.settings.clone();
            app.settings_page.chromium_path_input = app.settings.chromium_cookie_path.clone();
            app.settings_page.status = None;
            app.settings_page.storage_root_status = None;
            app.settings_page.pending_warning = None;
            app.settings_page.coordination_popup_open = None;
            app.settings_page.coordination_popup_serial += 1;
            load_paths_task(app)
        }

        Msg::PathsLoaded(roots) => {
            app.settings_page.storage_root_input = roots.storage_root.clone().unwrap_or_default();
            apply_loaded_roots(&mut app.settings_page, roots);
            Task::none()
        }

        Msg::Toggled(toggle, next) => {
            if app.settings_page.pending_warning.is_some() {
                return Task::none();
            }
            let previous = toggle_value(&app.settings, toggle);
            if next == previous {
                return Task::none();
            }
            if matches!(
                toggle,
                SettingToggle::AutoCacheListenAgain | SettingToggle::AutoManageOfflineCache
            ) {
                set_toggle(&mut app.settings_page.draft, toggle, next);
                return preview_eviction_task(
                    app,
                    EvictionChange::Toggle {
                        toggle,
                        next,
                        previous,
                    },
                );
            }
            let mut settings = app.settings.clone();
            set_toggle(&mut settings, toggle, next);
            persist_settings(app, settings)
        }

        Msg::NumberChanged(number, next) => {
            if number == SettingNumber::PipewireDuckingVolume {
                // Live slider preview; persisted on release.
                app.settings_page.draft.pipewire_ducking_volume = next;
                return Task::none();
            }
            if app.settings_page.pending_warning.is_some() {
                return Task::none();
            }
            let previous = number_value(&app.settings, number);
            if (next - previous).abs() < f64::EPSILON {
                return Task::none();
            }
            if matches!(
                number,
                SettingNumber::ListenAgainCacheDays
                    | SettingNumber::OfflineCacheTarget
                    | SettingNumber::OfflineRewatchThreshold
                    | SettingNumber::ScoreDecayWindowDays
                    | SettingNumber::MinimumOfflineScore
            ) {
                set_number(&mut app.settings_page.draft, number, next);
                return preview_eviction_task(
                    app,
                    EvictionChange::Number {
                        number,
                        next,
                        previous,
                    },
                );
            }
            let mut settings = app.settings.clone();
            set_number(&mut settings, number, next);
            persist_settings(app, settings)
        }

        Msg::NumberCommitted(number) => {
            let next = number_value(&app.settings_page.draft, number);
            if (next - number_value(&app.settings, number)).abs() <= 0.0001 {
                return Task::none();
            }
            let mut settings = app.settings.clone();
            set_number(&mut settings, number, next);
            persist_settings(app, settings)
        }

        Msg::EvictionPreviewed { change, impact } => {
            if app.settings_page.pending_warning.is_some() {
                return Task::none();
            }
            if eviction_warning_needed(change, impact) {
                app.settings_page.pending_warning =
                    Some(PendingWarning::EvictionRisk { change, impact });
                return Task::none();
            }
            persist_change(app, change)
        }

        Msg::WarningConfirmed => match app.settings_page.pending_warning.take() {
            Some(PendingWarning::EvictionRisk { change, .. }) => persist_change(app, change),
            Some(PendingWarning::Logout) => perform_logout(app),
            None => Task::none(),
        },

        Msg::WarningDismissed => {
            match app.settings_page.pending_warning.take() {
                Some(PendingWarning::EvictionRisk { change, .. }) => match change {
                    EvictionChange::Toggle {
                        toggle, previous, ..
                    } => set_toggle(&mut app.settings_page.draft, toggle, previous),
                    EvictionChange::Number {
                        number, previous, ..
                    } => set_number(&mut app.settings_page.draft, number, previous),
                },
                Some(PendingWarning::Logout) | None => {}
            }
            Task::none()
        }

        Msg::MprisModeChanged(mode) => {
            if mode == app.settings.other_mpris_mode {
                return Task::none();
            }
            let mut settings = app.settings.clone();
            settings.other_mpris_mode = mode;
            persist_settings(app, settings)
        }

        Msg::PipewirePauseModeChanged(mode) => {
            if mode == app.settings.pipewire_pause_mode {
                return Task::none();
            }
            let mut settings = app.settings.clone();
            settings.pipewire_pause_mode = mode;
            persist_settings(app, settings)
        }

        Msg::FilterModeChanged(stack, mode) => {
            if mode == filter_mode(&app.settings, stack) {
                return Task::none();
            }
            let mut settings = app.settings.clone();
            match stack {
                CoordinationStack::Mpris => settings.other_mpris_filter_mode = mode,
                CoordinationStack::Pipewire => settings.pipewire_filter_mode = mode,
            }
            persist_settings(app, settings)
        }

        Msg::FilterEntryAdded(stack, key) => {
            if key.trim().is_empty() {
                return Task::none();
            }
            let mut settings = app.settings.clone();
            {
                let list = filter_list_mut(&mut settings, stack);
                list.push(key);
                *list = sanitize_coordination_filter_entries(list);
            }
            persist_settings(app, settings)
        }

        Msg::FilterEntryRemoved(stack, identifiers) => {
            let mut settings = app.settings.clone();
            {
                let list = filter_list_mut(&mut settings, stack);
                list.retain(|entry| {
                    !coordination_filter_contains(std::slice::from_ref(entry), &identifiers)
                });
                *list = sanitize_coordination_filter_entries(list);
            }
            persist_settings(app, settings)
        }

        Msg::CoordinationPopupToggled(stack) => {
            app.settings_page.coordination_popup_serial += 1;
            if app.settings_page.coordination_popup_open == Some(stack) {
                app.settings_page.coordination_popup_open = None;
                return Task::none();
            }
            app.settings_page.coordination_popup_open = Some(stack);
            app.settings_page.coordination_targets = None;
            app.settings_page.coordination_targets_unavailable = false;
            fetch_coordination_targets_task(app, app.settings_page.coordination_popup_serial)
        }

        Msg::CoordinationPopupTick { serial } => {
            if serial != app.settings_page.coordination_popup_serial
                || app.settings_page.coordination_popup_open.is_none()
            {
                return Task::none();
            }
            fetch_coordination_targets_task(app, serial)
        }

        Msg::CoordinationTargetsLoaded { serial, result } => {
            if serial != app.settings_page.coordination_popup_serial
                || app.settings_page.coordination_popup_open.is_none()
            {
                return Task::none();
            }
            match result {
                Ok(targets) => {
                    app.settings_page.coordination_targets = Some(targets);
                    app.settings_page.coordination_targets_unavailable = false;
                }
                Err(err) => {
                    crate::app_log!("[SETTINGS] failed to fetch coordination targets: {err}");
                    app.settings_page.coordination_targets = None;
                    app.settings_page.coordination_targets_unavailable = true;
                }
            }
            app::after(
                COORDINATION_POPUP_REFRESH,
                Message::Settings(Msg::CoordinationPopupTick { serial }),
            )
        }

        Msg::SponsorCategoryToggled(key, active) => {
            let mut settings = app.settings.clone();
            let mut categories = settings.sponsorblock_categories.clone();
            if active {
                if !categories.iter().any(|category| category == &key) {
                    categories.push(key);
                }
            } else {
                categories.retain(|category| category != &key);
            }
            settings.sponsorblock_categories = sanitize_categories(&categories);
            persist_settings(app, settings)
        }

        Msg::ChromiumCookiePathChanged(value) => {
            app.settings_page.chromium_path_input = value;
            Task::none()
        }

        Msg::SaveChromiumCookiePath => {
            let path = app.settings_page.chromium_path_input.trim().to_string();
            app.settings_page.chromium_path_input = path.clone();
            let mut settings = app.settings.clone();
            settings.chromium_cookie_path = path;
            persist_settings(app, settings)
        }

        Msg::ClearChromiumCookiePath => {
            app.settings_page.chromium_path_input.clear();
            let mut settings = app.settings.clone();
            settings.chromium_cookie_path.clear();
            persist_settings(app, settings)
        }

        Msg::SyncChromiumCookieNow => {
            app.settings_page.status = Some("Syncing Chromium cookies...".to_string());
            chromium_restore_task(app, true)
        }

        Msg::StorageRootChanged(value) => {
            app.settings_page.storage_root_input = value;
            Task::none()
        }

        Msg::ApplyStorageRoot => apply_storage_root_task(app),

        Msg::ResetStorageRootToDefault => {
            app.settings_page.storage_root_input.clear();
            apply_storage_root_task(app)
        }

        Msg::StorageRootApplied { to_default, result } => {
            match result {
                Ok(roots) => {
                    app.settings_page.storage_root_input =
                        roots.storage_root.clone().unwrap_or_default();
                    apply_loaded_roots(&mut app.settings_page, roots);
                    app.settings_page.storage_root_status = Some(if to_default {
                        "Static is back on the default storage root.".to_string()
                    } else {
                        "Moved Static\u{2019}s local cache into the new root.".to_string()
                    });
                }
                Err(err) => {
                    app.settings_page.storage_root_status = Some(if to_default {
                        format!("Couldn\u{2019}t return to the default root yet: {err}")
                    } else {
                        format!("Couldn\u{2019}t move the local cache yet: {err}")
                    });
                }
            }
            Task::none()
        }

        Msg::LogoutPressed => {
            app.settings_page.pending_warning = Some(PendingWarning::Logout);
            Task::none()
        }

        Msg::LogoutFinished => {
            app.auth = AuthState::default();
            app.screen = Screen::Onboarding;
            Task::none()
        }

        Msg::SettingsSaved { result, previous } => {
            if let Err(err) = result {
                crate::app_log!("[SETTINGS] failed to persist settings: {err}");
                app.settings = previous.clone();
                app.settings_page.draft = previous;
                return app.show_toast("Couldn't save settings");
            }
            Task::none()
        }
    }
}

// ---------------------------------------------------------------------------
// Settings update helpers
// ---------------------------------------------------------------------------

fn toggle_value(settings: &AppSettings, toggle: SettingToggle) -> bool {
    match toggle {
        SettingToggle::PreferLocalCacheOnPlayback => settings.prefer_local_cache_on_playback,
        SettingToggle::PrefetchEnabled => settings.prefetch_enabled,
        SettingToggle::AutoManageOfflineCache => settings.auto_manage_offline_cache,
        SettingToggle::AutoCacheListenAgain => settings.auto_cache_listen_again,
        SettingToggle::AutoReleaseTrackLoops => settings.auto_release_track_loops,
        SettingToggle::SponsorblockEnabled => settings.sponsorblock_enabled,
        SettingToggle::PipewireDuckingEnabled => settings.pipewire_ducking_enabled,
        SettingToggle::AutoSyncChromiumCookies => settings.auto_sync_chromium_cookies,
    }
}

fn set_toggle(settings: &mut AppSettings, toggle: SettingToggle, value: bool) {
    match toggle {
        SettingToggle::PreferLocalCacheOnPlayback => {
            settings.prefer_local_cache_on_playback = value
        }
        SettingToggle::PrefetchEnabled => settings.prefetch_enabled = value,
        SettingToggle::AutoManageOfflineCache => settings.auto_manage_offline_cache = value,
        SettingToggle::AutoCacheListenAgain => settings.auto_cache_listen_again = value,
        SettingToggle::AutoReleaseTrackLoops => settings.auto_release_track_loops = value,
        SettingToggle::SponsorblockEnabled => settings.sponsorblock_enabled = value,
        SettingToggle::PipewireDuckingEnabled => settings.pipewire_ducking_enabled = value,
        SettingToggle::AutoSyncChromiumCookies => settings.auto_sync_chromium_cookies = value,
    }
}

fn number_value(settings: &AppSettings, number: SettingNumber) -> f64 {
    match number {
        SettingNumber::PrefetchAhead => settings.prefetch_ahead as f64,
        SettingNumber::OfflineCacheTarget => settings.offline_cache_target as f64,
        SettingNumber::OfflineRewatchThreshold => settings.offline_rewatch_threshold as f64,
        SettingNumber::ListenAgainCacheDays => settings.listen_again_cache_days as f64,
        SettingNumber::ScoreDecayWindowDays => settings.score_decay_window_days as f64,
        SettingNumber::MinimumOfflineScore => settings.minimum_offline_score,
        SettingNumber::AutoReleaseTrackLoopsAfter => settings.auto_release_track_loops_after as f64,
        SettingNumber::OtherMprisHaltAutoResumeSecs => {
            settings.other_mpris_halt_auto_resume_secs as f64
        }
        SettingNumber::PipewirePauseHaltAutoResumeSecs => {
            settings.pipewire_pause_halt_auto_resume_secs as f64
        }
        SettingNumber::PipewireDuckingThresholdSecs => {
            settings.pipewire_ducking_threshold_secs as f64
        }
        SettingNumber::PipewireDuckingVolume => settings.pipewire_ducking_volume,
    }
}

fn set_number(settings: &mut AppSettings, number: SettingNumber, value: f64) {
    match number {
        SettingNumber::PrefetchAhead => settings.prefetch_ahead = value.round() as u32,
        SettingNumber::OfflineCacheTarget => settings.offline_cache_target = value.round() as u32,
        SettingNumber::OfflineRewatchThreshold => {
            settings.offline_rewatch_threshold = value.round() as u32
        }
        SettingNumber::ListenAgainCacheDays => {
            settings.listen_again_cache_days = value.round() as u32
        }
        SettingNumber::ScoreDecayWindowDays => {
            settings.score_decay_window_days = value.round() as u32
        }
        SettingNumber::MinimumOfflineScore => settings.minimum_offline_score = round_to(value, 1),
        SettingNumber::AutoReleaseTrackLoopsAfter => {
            settings.auto_release_track_loops_after = value.round() as u32
        }
        SettingNumber::OtherMprisHaltAutoResumeSecs => {
            settings.other_mpris_halt_auto_resume_secs = value.round() as u32
        }
        SettingNumber::PipewirePauseHaltAutoResumeSecs => {
            settings.pipewire_pause_halt_auto_resume_secs = value.round() as u32
        }
        SettingNumber::PipewireDuckingThresholdSecs => {
            settings.pipewire_ducking_threshold_secs = value.round() as u32
        }
        SettingNumber::PipewireDuckingVolume => settings.pipewire_ducking_volume = value,
    }
}

fn filter_mode(settings: &AppSettings, stack: CoordinationStack) -> CoordinationFilterMode {
    match stack {
        CoordinationStack::Mpris => settings.other_mpris_filter_mode,
        CoordinationStack::Pipewire => settings.pipewire_filter_mode,
    }
}

fn filter_list(settings: &AppSettings, stack: CoordinationStack) -> &[String] {
    match stack {
        CoordinationStack::Mpris => &settings.other_mpris_filter_list,
        CoordinationStack::Pipewire => &settings.pipewire_filter_list,
    }
}

fn filter_list_mut(settings: &mut AppSettings, stack: CoordinationStack) -> &mut Vec<String> {
    match stack {
        CoordinationStack::Mpris => &mut settings.other_mpris_filter_list,
        CoordinationStack::Pipewire => &mut settings.pipewire_filter_list,
    }
}

fn target_is_listed(
    settings: &AppSettings,
    stack: CoordinationStack,
    target: &CoordinationTargetSnapshot,
) -> bool {
    if !target.filterable || target.key.trim().is_empty() {
        return false;
    }
    let identifiers = [target.key.clone(), target.label.clone()];
    coordination_filter_contains(filter_list(settings, stack), &identifiers)
}

fn apply_change(settings: &mut AppSettings, change: EvictionChange) {
    match change {
        EvictionChange::Toggle { toggle, next, .. } => set_toggle(settings, toggle, next),
        EvictionChange::Number { number, next, .. } => set_number(settings, number, next),
    }
}

fn eviction_warning_needed(change: EvictionChange, impact: usize) -> bool {
    if impact == 0 {
        return false;
    }
    match change {
        EvictionChange::Toggle { next, .. } => !next,
        EvictionChange::Number {
            number,
            next,
            previous,
        } => match number {
            SettingNumber::ListenAgainCacheDays | SettingNumber::OfflineCacheTarget => {
                next < previous
            }
            SettingNumber::OfflineRewatchThreshold
            | SettingNumber::ScoreDecayWindowDays
            | SettingNumber::MinimumOfflineScore => true,
            _ => false,
        },
    }
}

/// GTK `preview_auto_cache_evictions`: count how many auto-downloads the
/// candidate settings would evict, then route through `EvictionPreviewed`.
fn preview_eviction_task(app: &App, change: EvictionChange) -> Task<Message> {
    let cache = app.cache.clone();
    let mut settings = app.settings.clone();
    apply_change(&mut settings, change);
    app::blocking(
        move || {
            let guard = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .auto_download_eviction_count(&settings)
                .unwrap_or_else(|err| {
                    crate::app_log!("[SETTINGS] failed to preview cache evictions: {err}");
                    0
                })
        },
        move |impact| Message::Settings(Msg::EvictionPreviewed { change, impact }),
    )
}

fn persist_change(app: &mut App, change: EvictionChange) -> Task<Message> {
    let mut settings = app.settings.clone();
    apply_change(&mut settings, change);
    persist_settings(app, settings)
}

/// GTK `persist_and_sync_settings`: save through `MusicCache`, mirror into
/// `App.settings`, then nudge the daemon to reload (mandatory invariant).
fn persist_settings(app: &mut App, settings: AppSettings) -> Task<Message> {
    let previous = app.settings.clone();
    app.settings = settings.clone();
    app.settings_page.draft = settings.clone();
    let cache = app.cache.clone();
    app::blocking(
        move || {
            let result = {
                let guard = cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard
                    .save_settings(&settings)
                    .map_err(|err| err.to_string())
            };
            if result.is_ok() {
                crate::player::notify_daemon_settings_changed();
            }
            result
        },
        move |result| {
            Message::Settings(Msg::SettingsSaved {
                result,
                previous: previous.clone(),
            })
        },
    )
}

fn load_paths_task(app: &App) -> Task<Message> {
    let cache = app.cache.clone();
    app::blocking(
        move || {
            let storage_root =
                crate::network::get_storage_root().filter(|root| !root.trim().is_empty());
            let guard = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            LoadedRoots {
                storage_root,
                cache_root: guard.base_dir().display().to_string(),
                offline_root: guard.offline_dir().display().to_string(),
                temp_root: guard.temp_dir().display().to_string(),
            }
        },
        |roots| Message::Settings(Msg::PathsLoaded(roots)),
    )
}

fn apply_loaded_roots(state: &mut State, roots: LoadedRoots) {
    state.storage_root = roots.storage_root;
    state.cache_root = Some(roots.cache_root);
    state.offline_root = Some(roots.offline_root);
    state.temp_root = Some(roots.temp_root);
}

/// GTK `persist_storage_root_override`: migrate the cache directories first,
/// then persist (or delete) the keyring override.
fn apply_storage_root_task(app: &mut App) -> Task<Message> {
    let raw = app.settings_page.storage_root_input.trim().to_string();
    let to_default = raw.is_empty();
    app.settings_page.storage_root_status = None;
    let cache = app.cache.clone();
    app::blocking(
        move || -> Result<LoadedRoots, String> {
            let mut guard = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .migrate_storage_root(&raw)
                .map_err(|err| err.to_string())?;
            if raw.is_empty() {
                crate::network::delete_storage_root();
            } else {
                crate::network::store_storage_root(&raw).map_err(|err| err.to_string())?;
            }
            Ok(LoadedRoots {
                storage_root: crate::network::get_storage_root()
                    .filter(|root| !root.trim().is_empty()),
                cache_root: guard.base_dir().display().to_string(),
                offline_root: guard.offline_dir().display().to_string(),
                temp_root: guard.temp_dir().display().to_string(),
            })
        },
        move |result| Message::Settings(Msg::StorageRootApplied { to_default, result }),
    )
}

fn fetch_coordination_targets_task(app: &App, serial: u64) -> Task<Message> {
    let daemon = app.player.daemon.clone();
    app::blocking_or_timeout(
        app::DAEMON_TASK_TIMEOUT,
        move || daemon.coordination_targets().map_err(|err| err.to_string()),
        move |result| Message::Settings(Msg::CoordinationTargetsLoaded { serial, result }),
        Message::Settings(Msg::CoordinationTargetsLoaded {
            serial,
            result: Err("daemon coordination target request timed out".to_owned()),
        }),
    )
}

/// GTK logout: reset the player UI, drop the client + home cache, delete the
/// keyring cookie, then return to onboarding.
fn perform_logout(app: &mut App) -> Task<Message> {
    let stop_task = player_bar::reset_player_ui(app);
    app.client = None;
    app.invalidate_home_cache();
    app.active_nav = NavItem::Home;
    app.content_view = ContentView::Home;
    Task::batch([
        stop_task,
        app::blocking(crate::network::delete_cookie, |_| {
            Message::Settings(Msg::LogoutFinished)
        }),
    ])
}

// ---------------------------------------------------------------------------
// Auth update
// ---------------------------------------------------------------------------

pub fn update_auth(app: &mut App, msg: AuthMsg) -> Task<Message> {
    match msg {
        AuthMsg::CookieInput(value) => {
            app.auth.cookie_input = value;
            Task::none()
        }

        AuthMsg::ConnectPressed => {
            if app.auth.connecting {
                return Task::none();
            }
            let cookie = app.auth.cookie_input.clone();
            if cookie.is_empty() {
                app.auth.status = Some("Please paste a cookie first".to_string());
                return Task::none();
            }
            app.auth.connecting = true;
            app.auth.status = None;
            app::blocking_or(
                move || -> Result<Arc<NetworkState>, String> {
                    let client = Arc::new(
                        NetworkState::from_cookie(&cookie).map_err(|err| err.to_string())?,
                    );
                    if let Err(err) = client.check_auth() {
                        crate::app_log!(
                            "[AUTH] onboarding cookie auth check failed, continuing \
                             offline-first: {err}"
                        );
                    }
                    if let Err(err) = crate::network::store_cookie(&cookie) {
                        crate::app_log!("[AUTH] failed to store onboarding cookie: {err}");
                    }
                    Ok(client)
                },
                |result| Message::Auth(AuthMsg::ConnectFinished { result }),
                // A panicking worker must still clear `auth.connecting`.
                Message::Auth(AuthMsg::ConnectFinished {
                    result: Err("connect worker panicked".to_owned()),
                }),
            )
        }

        AuthMsg::ConnectFinished { result } => {
            app.auth.connecting = false;
            match result {
                Ok(client) => {
                    // Offline-first: always advance to vr_auth once the cookie
                    // parses, even when the auth check could not confirm it.
                    app.client = Some(client);
                    app.invalidate_home_cache();
                    app.screen = Screen::VrAuth;
                }
                Err(err) => {
                    app.auth.status = Some(format!("Invalid cookie: {err}"));
                }
            }
            Task::none()
        }

        AuthMsg::VrSignInPressed => {
            if !matches!(app.auth.vr_phase, VrPhase::Idle) {
                return Task::none();
            }
            app.auth.vr_phase = VrPhase::Starting;
            app.auth.vr_status = None;
            let client = app.app_client();
            app::blocking(
                move || match client {
                    Some(client) => client.start_vr_device_flow().map_err(|err| err.to_string()),
                    None => Err("Not connected".to_string()),
                },
                |result| Message::Auth(AuthMsg::VrFlowStarted { result }),
            )
        }

        AuthMsg::VrFlowStarted { result } => {
            match result {
                Ok((device_code, user_code, verification_url)) => {
                    app.auth.vr_device_code = Some(device_code);
                    app.auth.vr_user_code = Some(user_code);
                    app.auth.vr_verification_url = Some(verification_url);
                    app.auth.vr_phase = VrPhase::CodeShown;
                }
                Err(err) => {
                    app.auth.vr_phase = VrPhase::Idle;
                    app.auth.vr_status = Some(format!("Failed: {err}"));
                }
            }
            Task::none()
        }

        AuthMsg::VrDonePressed => {
            if !matches!(app.auth.vr_phase, VrPhase::CodeShown) {
                return Task::none();
            }
            app.auth.vr_phase = VrPhase::Polling;
            app.auth.vr_status = None;
            let client = app.app_client();
            let device_code = app.auth.vr_device_code.clone().unwrap_or_default();
            app::blocking(
                move || match client {
                    Some(client) => {
                        let token = client
                            .poll_vr_device_flow(&device_code)
                            .map_err(|err| err.to_string())?;
                        if let Err(err) = crate::network::store_vr_refresh_token(&token) {
                            crate::app_log!("[AUTH] failed to store VR refresh token: {err}");
                        }
                        Ok(token)
                    }
                    None => Err("Not connected".to_string()),
                },
                |result| Message::Auth(AuthMsg::VrPollFinished { result }),
            )
        }

        AuthMsg::VrPollFinished { result } => match result {
            Ok(_) => enter_main(app),
            Err(err) => {
                app.auth.vr_phase = VrPhase::CodeShown;
                app.auth.vr_status = Some(if err.contains("authorization_pending") {
                    "Still waiting... complete sign-in in your browser, then click Done again."
                        .to_string()
                } else {
                    format!("Error: {err}")
                });
                Task::none()
            }
        },

        AuthMsg::VrSkipPressed => enter_main(app),

        AuthMsg::KeyringRestoreFinished { outcome } => match outcome {
            RestoreOutcome::Authenticated(client) => {
                // Authenticated keyring cookie skips the Chromium phase.
                activate_restored_client(app, client)
            }
            RestoreOutcome::Rejected => Task::batch([
                app::blocking(crate::network::delete_cookie, |_| Message::Noop),
                chromium_restore_task(app, false),
            ]),
            RestoreOutcome::Deferred(client) => Task::batch([
                stage_restored_client(app, client),
                chromium_restore_task(app, false),
            ]),
        },

        AuthMsg::ChromiumRestoreFinished {
            outcome,
            cookie,
            manual,
        } => match outcome {
            Some(RestoreOutcome::Authenticated(client)) => {
                if manual {
                    app.settings_page.status = Some("Chromium cookies synced.".to_string());
                }
                let store = app::blocking(
                    move || {
                        if let Some(cookie) = cookie {
                            if let Err(err) = crate::network::store_cookie(&cookie) {
                                crate::app_log!("[AUTH] failed to store Chromium cookie: {err}");
                            }
                        }
                    },
                    |_| Message::Noop,
                );
                Task::batch([store, activate_restored_client(app, client)])
            }
            Some(RestoreOutcome::Deferred(client)) => {
                if manual {
                    app.settings_page.status = Some(
                        "Chromium cookie found, but it couldn't be verified yet — staged for \
                         offline-first use."
                            .to_string(),
                    );
                }
                stage_restored_client(app, client)
            }
            Some(RestoreOutcome::Rejected) => {
                if manual {
                    app.settings_page.status =
                        Some("Chromium cookie was rejected by YouTube Music.".to_string());
                }
                Task::none()
            }
            None => {
                if manual {
                    app.settings_page.status =
                        Some("No YouTube Music cookies found in the Chromium profile.".to_string());
                }
                Task::none()
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

/// GTK stack "main" transition: load home (with offline fallback) and refresh
/// sidebar history; no-op when the main page is already showing.
fn enter_main(app: &mut App) -> Task<Message> {
    if app.screen == Screen::Main {
        return Task::none();
    }
    app.screen = Screen::Main;
    app.active_nav = NavItem::Home;
    app.content_view = ContentView::Home;
    let mut tasks = vec![content_loading::bootstrap_home_task(app)];
    if app.client.is_some() {
        tasks.push(sidebar::load_history_task(app));
    } else {
        app.sidebar.recent = sidebar::RecentList::Status("Restoring history...".to_string());
    }
    Task::batch(tasks)
}

/// GTK `stage_restored_client`: install the client and drop caches that were
/// fetched unauthenticated; refresh history when main is already visible.
/// Unlike GTK (whose retained widgets kept the stale feed on screen), the
/// iced home view re-renders reactively from state — so when Home is the
/// active view, refetch it immediately instead of leaving a dead
/// "Loading recommendations..." placeholder.
fn stage_restored_client(app: &mut App, client: Arc<NetworkState>) -> Task<Message> {
    app.client = Some(client);
    app.invalidate_home_cache();
    if app.screen != Screen::Main {
        return Task::none();
    }
    let mut tasks = vec![sidebar::load_history_task(app)];
    if app.content_view == ContentView::Home && !app.home.loading {
        tasks.push(content_loading::load_home_task(app, false));
    }
    Task::batch(tasks)
}

/// GTK `activate_authenticated_client`: stage, then make sure the main page is
/// showing (entering it bootstraps home + history).
fn activate_restored_client(app: &mut App, client: Arc<NetworkState>) -> Task<Message> {
    if app.screen == Screen::Main {
        stage_restored_client(app, client)
    } else {
        app.client = Some(client);
        app.invalidate_home_cache();
        enter_main(app)
    }
}

/// Phase 2 of startup restore (and the settings card's "Sync now"): pull
/// cookies out of the configured Chromium profile and validate them.
fn chromium_restore_task(app: &App, manual: bool) -> Task<Message> {
    if !manual && !app.settings.auto_sync_chromium_cookies {
        return Task::none();
    }
    let settings = app.settings.clone();
    app::blocking(
        move || match crate::network::load_cookie_from_chromium_profile(&settings) {
            Ok(Some(cookie)) => {
                let outcome = restore_outcome(NetworkState::validate_cookie(
                    cookie.clone(),
                    "Chromium cookie auto-sync",
                ));
                (Some(outcome), Some(cookie))
            }
            Ok(None) => (None, None),
            Err(err) => {
                crate::app_log!("[AUTH] Chromium cookie auto-sync failed: {err}");
                (None, None)
            }
        },
        move |(outcome, cookie)| {
            Message::Auth(AuthMsg::ChromiumRestoreFinished {
                outcome,
                cookie,
                manual,
            })
        },
    )
}

fn restore_outcome(outcome: NetworkValidationOutcome) -> RestoreOutcome {
    match outcome {
        NetworkValidationOutcome::Authenticated(client) => RestoreOutcome::Authenticated(client),
        NetworkValidationOutcome::Rejected => RestoreOutcome::Rejected,
        NetworkValidationOutcome::Deferred(client) => RestoreOutcome::Deferred(client),
    }
}

/// Startup auth restore future (shell spec §8.3): validate the keyring cookie
/// (Authenticated → activate + switch to Main, skipping Chromium sync;
/// Rejected → delete cookie, fall through; Deferred → stage client), then
/// Chromium auto-sync when enabled.
pub fn startup_auth_restore_task(app: &App, saved_cookie: Option<String>) -> Task<Message> {
    match saved_cookie {
        Some(cookie) => app::blocking(
            move || restore_outcome(NetworkState::validate_cookie(cookie, "keyring cookie")),
            |outcome| Message::Auth(AuthMsg::KeyringRestoreFinished { outcome }),
        ),
        None => chromium_restore_task(app, false),
    }
}

// ---------------------------------------------------------------------------
// Private styles (built only from theme tokens)
// ---------------------------------------------------------------------------

fn toggler_style(_t: &iced::Theme, status: toggler::Status) -> toggler::Style {
    let (is_on, hovered, disabled) = match status {
        toggler::Status::Active { is_toggled } => (is_toggled, false, false),
        toggler::Status::Hovered { is_toggled } => (is_toggled, true, false),
        toggler::Status::Disabled { is_toggled } => (is_toggled, false, true),
    };

    let (background, background_border, foreground) = if disabled {
        (
            theme::white(0.04),
            theme::HAIRLINE,
            theme::white(if is_on { 0.25 } else { 0.15 }),
        )
    } else if is_on {
        (
            if hovered {
                iced::Color {
                    a: 0.30,
                    ..theme::ACCENT
                }
            } else {
                theme::ACCENT_BG
            },
            theme::ACCENT_BORDER,
            theme::ACCENT,
        )
    } else {
        (
            if hovered {
                theme::BG_ENTRY_FOCUS
            } else {
                theme::BG_ENTRY
            },
            theme::BORDER_SUBTLE,
            theme::white(0.45),
        )
    };

    toggler::Style {
        background: background.into(),
        background_border_width: 1.0,
        background_border_color: background_border,
        foreground: foreground.into(),
        foreground_border_width: 0.0,
        foreground_border_color: iced::Color::TRANSPARENT,
        text_color: None,
        border_radius: None,
        padding_ratio: 0.18,
    }
}

fn pick_style(_t: &iced::Theme, status: pick_list::Status) -> pick_list::Style {
    let hovered = matches!(
        status,
        pick_list::Status::Hovered | pick_list::Status::Opened { is_hovered: true }
    );
    pick_list::Style {
        text_color: theme::TEXT,
        placeholder_color: theme::TEXT_FAINT,
        handle_color: theme::TEXT_DIM,
        background: if hovered {
            theme::BG_ENTRY_FOCUS.into()
        } else {
            theme::BG_ENTRY.into()
        },
        border: iced::Border {
            color: if matches!(status, pick_list::Status::Opened { .. }) {
                theme::ACCENT_BORDER
            } else {
                theme::BORDER_SUBTLE
            },
            width: 1.0,
            radius: theme::RADIUS_MD.into(),
        },
    }
}

fn duck_slider_style(_t: &iced::Theme, status: slider::Status) -> slider::Style {
    let active = matches!(status, slider::Status::Hovered | slider::Status::Dragged);
    slider::Style {
        rail: slider::Rail {
            backgrounds: (theme::ACCENT_SOFT.into(), theme::white(0.10).into()),
            width: 4.0,
            border: iced::Border {
                color: iced::Color::TRANSPARENT,
                width: 0.0,
                radius: theme::RADIUS_PILL.into(),
            },
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle {
                radius: if active { 8.0 } else { 7.0 },
            },
            background: theme::ACCENT.into(),
            border_width: 0.0,
            border_color: iced::Color::TRANSPARENT,
        },
    }
}

fn warning_card_style(_t: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(theme::BG_POPOVER.into()),
        border: iced::Border {
            color: iced::Color {
                a: 0.35,
                ..theme::TEXT_DANGER
            },
            width: 1.0,
            radius: theme::RADIUS_PANEL.into(),
        },
        shadow: iced::Shadow {
            color: iced::Color {
                a: 0.45,
                ..iced::Color::BLACK
            },
            offset: iced::Vector::new(0.0, 6.0),
            blur_radius: 24.0,
        },
        ..container::Style::default()
    }
}
