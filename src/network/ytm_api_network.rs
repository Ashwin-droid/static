use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
use rusqlite::{Connection as SqliteConnection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use url::Url;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

use crate::models::{
    BrowseSectionsPage, HomeItem, HomePage, PlaylistSummary, RadioQueue, SearchResult,
    SyncedLyricLine, SyncedLyrics, SyncedLyricsSource, Track,
};

use crate::network_parser as parsers;

const YTM_BASE_URL: &str = "https://music.youtube.com/youtubei/v1";
const YTM_PARAMS: &str = "prettyPrint=false";
const YTM_ORIGIN: &str = "https://music.youtube.com";
const YTM_WEB_REMIX_HEADER_CLIENT_NAME: &str = "67";
const YTM_WEB_REMIX_CLIENT_NAME: &str = "WEB_REMIX";
const YTM_WEB_REMIX_CLIENT_VERSION: &str = "1.20260426.12.00";
const YTM_MOBILE_CLIENT_NAME: &str = "ANDROID_MUSIC";
const YTM_MOBILE_CLIENT_VERSION: &str = "7.21.50";
const YTM_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 Edg/147.0.0.0";
const TRACKING_BROWSER_BRAND: &str = "Edge Chromium";
const TRACKING_BROWSER_VERSION: &str = "147.0.0.0";
const TRACKING_BROWSER_ENGINE: &str = "WebKit";
const TRACKING_BROWSER_ENGINE_VERSION: &str = "537.36";
const TRACKING_OS_NAME: &str = "X11";
const TRACKING_PLATFORM: &str = "DESKTOP";
const TRACKING_SEC_CH_UA: &str =
    "\"Microsoft Edge\";v=\"147\", \"Not.A/Brand\";v=\"8\", \"Chromium\";v=\"147\"";
const TRACKING_SEC_CH_UA_MOBILE: &str = "?0";
const TRACKING_SEC_CH_UA_PLATFORM: &str = "\"Linux\"";
const ATR_REPORT_DELAY: Duration = Duration::from_secs(4);
const YTM_WEB_PROFILE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const YTM_WEB_PROFILE_DB_FILE_NAME: &str = "data.db";
const YTM_WEB_PROFILE_TABLE: &str = "ytm_web_profile";
const YTM_WEB_PROFILE_ROW_ID: &str = "web_remix";
const YTM_WEB_SIGNATURE_TIMESTAMP_FALLBACK: u64 = 20_572;

// OAuth device flow for Android VR auth (premium content)
const VR_OAUTH_CLIENT_ID: &str =
    "652469312169-4lvs9bnhr9lpns9v451j5oivd81vjvu1.apps.googleusercontent.com";
const VR_OAUTH_CLIENT_SECRET: &str = "3fTWrBJI5Uojm1TK7_iJCW5Z";
const VR_OAUTH_TOKEN_URL: &str = "https://www.youtube.com/o/oauth2/token?prettyPrint=false";
const VR_OAUTH_DEVICE_CODE_URL: &str =
    "https://www.youtube.com/o/oauth2/device/code?prettyPrint=false";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SongRating {
    Like,
    Dislike,
    Indifferent,
}

impl SongRating {
    fn endpoint(self) -> &'static str {
        match self {
            SongRating::Like => "like/like",
            SongRating::Dislike => "like/dislike",
            SongRating::Indifferent => "like/removelike",
        }
    }

    fn from_ytmusic(value: &str) -> Self {
        match value {
            "LIKE" => SongRating::Like,
            "DISLIKE" => SongRating::Dislike,
            _ => SongRating::Indifferent,
        }
    }
}

/// Holds the watchtime tracking URL so we can report real listen time on navigate-away.
pub struct TrackingInfo {
    http: reqwest::Client,
    auth: Option<String>,
    watchtime_url: Option<String>,
    atr_url: Option<String>,
    cpn: String,
    referer_url: String,
    audio_fmt: String,
    video_fmt: String,
    length_millis: u64,
    flush_points_millis: Vec<u64>,
    default_flush_interval_millis: u64,
    last_reported_millis: AtomicU64,
    last_activity_millis: AtomicU64,
    atr_reported: AtomicBool,
    session_started_at: Instant,
    web_profile: YtmWebProfile,
    rt: Arc<tokio::runtime::Runtime>,
}

fn default_signature_timestamp() -> u64 {
    YTM_WEB_SIGNATURE_TIMESTAMP_FALLBACK
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct YtmWebProfile {
    client_name: String,
    header_client_name: String,
    client_version: String,
    #[serde(default)]
    player_js_url: Option<String>,
    #[serde(default = "default_signature_timestamp")]
    signature_timestamp: u64,
    page_cl: Option<String>,
    page_label: Option<String>,
    visitor_data: Option<String>,
    datasync_id: Option<String>,
    identity_token: Option<String>,
    hl: String,
    gl: String,
    user_agent: String,
    browser_name: String,
    browser_version: String,
    browser_engine: String,
    browser_engine_version: String,
    os_name: String,
    platform: String,
    sec_ch_ua: String,
    sec_ch_ua_mobile: String,
    sec_ch_ua_platform: String,
    time_zone: String,
    utc_offset_minutes: String,
    fetched_at_unix_millis: u64,
}

impl YtmWebProfile {
    fn fallback() -> Self {
        Self {
            client_name: YTM_WEB_REMIX_CLIENT_NAME.to_string(),
            header_client_name: YTM_WEB_REMIX_HEADER_CLIENT_NAME.to_string(),
            client_version: YTM_WEB_REMIX_CLIENT_VERSION.to_string(),
            player_js_url: None,
            signature_timestamp: YTM_WEB_SIGNATURE_TIMESTAMP_FALLBACK,
            page_cl: None,
            page_label: None,
            visitor_data: None,
            datasync_id: None,
            identity_token: None,
            hl: "en".to_string(),
            gl: "US".to_string(),
            user_agent: YTM_USER_AGENT.to_string(),
            browser_name: TRACKING_BROWSER_BRAND.to_string(),
            browser_version: TRACKING_BROWSER_VERSION.to_string(),
            browser_engine: TRACKING_BROWSER_ENGINE.to_string(),
            browser_engine_version: TRACKING_BROWSER_ENGINE_VERSION.to_string(),
            os_name: TRACKING_OS_NAME.to_string(),
            platform: TRACKING_PLATFORM.to_string(),
            sec_ch_ua: TRACKING_SEC_CH_UA.to_string(),
            sec_ch_ua_mobile: TRACKING_SEC_CH_UA_MOBILE.to_string(),
            sec_ch_ua_platform: TRACKING_SEC_CH_UA_PLATFORM.to_string(),
            time_zone: "Asia/Calcutta".to_string(),
            utc_offset_minutes: "330".to_string(),
            fetched_at_unix_millis: unix_epoch_millis_u64(),
        }
    }

    fn is_fresh(&self, now_unix_millis: u64) -> bool {
        now_unix_millis.saturating_sub(self.fetched_at_unix_millis)
            <= YTM_WEB_PROFILE_CACHE_TTL.as_millis() as u64
    }

    fn has_page_identity(&self) -> bool {
        self.identity_token
            .as_ref()
            .is_some_and(|token| !token.trim().is_empty())
    }

    fn is_fresh_for_tracking(&self, now_unix_millis: u64) -> bool {
        self.is_fresh(now_unix_millis) && self.has_page_identity()
    }

    fn device_header(&self) -> String {
        format!(
            "cbr={}&cbrver={}&ceng={}&cengver={}&cos={}&cplatform={}",
            self.browser_name.replace(' ', "+"),
            self.browser_version,
            self.browser_engine,
            self.browser_engine_version,
            self.os_name,
            self.platform,
        )
    }
}

struct PendingWatchtimeRequest {
    url: String,
    referer: String,
    event_time: String,
    request_time: String,
    start_millis: u64,
    watched_millis: u64,
    state: String,
    final_ping: bool,
}

struct PendingTrackingRequest {
    kind: &'static str,
    url: String,
    referer: String,
    event_time: String,
    request_time: String,
}

#[derive(Debug, Clone)]
struct CaptionTrack {
    base_url: String,
    language_code: Option<String>,
    kind: Option<String>,
    name: Option<String>,
}

impl CaptionTrack {
    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            base_url: value.get("baseUrl")?.as_str()?.to_string(),
            language_code: value
                .get("languageCode")
                .and_then(|language| language.as_str())
                .map(str::to_string),
            kind: value
                .get("kind")
                .and_then(|kind| kind.as_str())
                .map(str::to_string),
            name: text_from_navigation_text(value.get("name")),
        })
    }

    fn source(&self) -> SyncedLyricsSource {
        if self.kind.as_deref() == Some("asr") {
            SyncedLyricsSource::AutoCaptions
        } else {
            SyncedLyricsSource::Captions
        }
    }
}

fn lyrics_browse_id_from_next_response(value: &Value) -> Option<String> {
    let tabs = value
        .pointer(
            "/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs",
        )
        .and_then(|tabs| tabs.as_array())?;
    let tab = tabs.get(1)?.get("tabRenderer")?;
    if tab.get("unselectable").is_some() {
        return None;
    }
    tab.pointer("/endpoint/browseEndpoint/browseId")
        .and_then(|browse_id| browse_id.as_str())
        .map(str::to_string)
}

impl TrackingInfo {
    fn next_flush_after(&self, last_reported_millis: u64) -> Option<u64> {
        if let Some(next) = self
            .flush_points_millis
            .iter()
            .copied()
            .find(|flush_at| *flush_at > last_reported_millis)
        {
            return Some(next);
        }

        if self.default_flush_interval_millis == 0 {
            return None;
        }

        let anchor = self.flush_points_millis.last().copied().unwrap_or(0);
        let effective_last = last_reported_millis.max(anchor);
        let intervals_elapsed = (effective_last - anchor) / self.default_flush_interval_millis;
        Some(anchor + (intervals_elapsed + 1) * self.default_flush_interval_millis)
    }

    fn set_query_param(params: &mut HashMap<String, String>, key: &str, value: impl Into<String>) {
        params.insert(key.to_string(), value.into());
    }

    fn build_query_map(url: &str) -> Option<(Url, HashMap<String, String>)> {
        let mut parsed = Url::parse(url).ok()?;
        let params = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<HashMap<_, _>>();
        parsed.set_query(None);
        Some((parsed, params))
    }

    fn default_visibility(&self) -> &'static str {
        if self.video_fmt == "0" { "1" } else { "10" }
    }

    fn apply_default_client_params(&self, params: &mut HashMap<String, String>) {
        if !params.contains_key("c") {
            Self::set_query_param(params, "c", self.web_profile.client_name.clone());
        }
        if !params.contains_key("cver") {
            Self::set_query_param(params, "cver", self.web_profile.client_version.clone());
        }
        if !params.contains_key("cplayer") {
            Self::set_query_param(params, "cplayer", "UNIPLAYER");
        }
        if !params.contains_key("cos") {
            Self::set_query_param(params, "cos", self.web_profile.os_name.clone());
        }
        if !params.contains_key("cplatform") {
            Self::set_query_param(params, "cplatform", self.web_profile.platform.clone());
        }
        if !params.contains_key("cbr") {
            Self::set_query_param(params, "cbr", self.web_profile.browser_name.clone());
        }
        if !params.contains_key("cbrver") {
            Self::set_query_param(params, "cbrver", self.web_profile.browser_version.clone());
        }
    }

    fn finalize_tracking_url(url: Url, params: HashMap<String, String>) -> String {
        let mut url = url;
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in params {
                pairs.append_pair(&key, &value);
            }
        }
        url.into()
    }

    fn next_request_metadata(&self) -> (String, String, u64, u64) {
        let now_unix_millis = unix_epoch_millis();
        let wall_elapsed_millis = self.session_started_at.elapsed().as_millis() as u64;
        let previous_activity = self
            .last_activity_millis
            .swap(wall_elapsed_millis, Ordering::Relaxed);
        let lact_millis = wall_elapsed_millis.saturating_sub(previous_activity);
        (
            now_unix_millis.to_string(),
            now_unix_millis.to_string(),
            wall_elapsed_millis,
            lact_millis,
        )
    }

    fn next_report_deadline_seconds(
        &self,
        wall_elapsed_millis: u64,
        watched_millis: u64,
    ) -> Option<String> {
        if watched_millis >= self.length_millis {
            return None;
        }

        let next_flush_millis = self.next_flush_after(watched_millis)?;
        let wall_lag_millis = wall_elapsed_millis.saturating_sub(watched_millis);
        Some((((next_flush_millis + wall_lag_millis + 999) / 1000) + 1).to_string())
    }

    fn build_watchtime_request(
        &self,
        start_millis: u64,
        listened_millis: u64,
        state: &str,
        final_ping: bool,
    ) -> Option<PendingWatchtimeRequest> {
        let watchtime_url = self.watchtime_url.as_deref()?;
        let watched = listened_millis.min(self.length_millis);
        if watched == 0 || (!final_ping && watched <= start_millis) {
            return None;
        }

        let (url, mut params) = Self::build_query_map(watchtime_url)?;
        let (event_time, request_time, wall_elapsed_millis, lact_millis) =
            self.next_request_metadata();

        Self::set_query_param(&mut params, "cpn", self.cpn.clone());
        Self::set_query_param(&mut params, "ver", "2");
        Self::set_query_param(&mut params, "st", format_millis(start_millis));
        Self::set_query_param(&mut params, "et", format_millis(watched));
        Self::set_query_param(&mut params, "cmt", format_millis(watched));
        Self::set_query_param(&mut params, "fmt", self.video_fmt.clone());
        Self::set_query_param(&mut params, "afmt", self.audio_fmt.clone());
        Self::set_query_param(&mut params, "muted", "0");
        Self::set_query_param(&mut params, "volume", "100");
        let vis = params
            .get("vis")
            .cloned()
            .unwrap_or_else(|| self.default_visibility().to_string());
        Self::set_query_param(&mut params, "vis", vis);
        Self::set_query_param(&mut params, "state", state);
        Self::set_query_param(&mut params, "rt", format_millis(wall_elapsed_millis));
        Self::set_query_param(&mut params, "rti", (wall_elapsed_millis / 1000).to_string());
        Self::set_query_param(&mut params, "lact", lact_millis.to_string());

        if state == "playing" && !final_ping {
            if let Some(next_deadline) =
                self.next_report_deadline_seconds(wall_elapsed_millis, watched)
            {
                Self::set_query_param(&mut params, "rtn", next_deadline);
            } else {
                params.remove("rtn");
            }
            params.remove("final");
        } else if final_ping {
            Self::set_query_param(&mut params, "final", "1");
            params.remove("rtn");
        } else {
            params.remove("rtn");
            params.remove("final");
        }

        if !params.contains_key("referrer") {
            Self::set_query_param(&mut params, "referrer", self.referer_url.clone());
        }
        if !params.contains_key("len") {
            Self::set_query_param(&mut params, "len", format_millis(self.length_millis));
        }
        self.apply_default_client_params(&mut params);

        Some(PendingWatchtimeRequest {
            url: Self::finalize_tracking_url(url, params),
            referer: self.referer_url.clone(),
            event_time,
            request_time,
            start_millis,
            watched_millis: watched,
            state: state.to_string(),
            final_ping,
        })
    }

    fn build_playback_request(&self, playback_url: &str) -> Option<PendingTrackingRequest> {
        let watched =
            (self.session_started_at.elapsed().as_millis() as u64).min(self.length_millis);
        let (url, mut params) = Self::build_query_map(playback_url)?;
        let (event_time, request_time, wall_elapsed_millis, lact_millis) =
            self.next_request_metadata();

        Self::set_query_param(&mut params, "cpn", self.cpn.clone());
        Self::set_query_param(&mut params, "ver", "2");
        Self::set_query_param(&mut params, "cmt", format_millis(watched));
        Self::set_query_param(&mut params, "fmt", self.video_fmt.clone());
        Self::set_query_param(&mut params, "afmt", self.audio_fmt.clone());
        Self::set_query_param(&mut params, "muted", "0");
        Self::set_query_param(&mut params, "volume", "100");
        Self::set_query_param(&mut params, "vis", self.default_visibility());
        Self::set_query_param(&mut params, "rt", format_millis(wall_elapsed_millis));
        Self::set_query_param(&mut params, "lact", lact_millis.to_string());
        if let Some(next_deadline) = self.next_report_deadline_seconds(wall_elapsed_millis, watched)
        {
            Self::set_query_param(&mut params, "rtn", next_deadline);
        }
        if !params.contains_key("referrer") {
            Self::set_query_param(&mut params, "referrer", self.referer_url.clone());
        }
        if !params.contains_key("len") {
            Self::set_query_param(&mut params, "len", format_millis(self.length_millis));
        }
        if !params.contains_key("mos") {
            Self::set_query_param(&mut params, "mos", "0");
        }
        self.apply_default_client_params(&mut params);

        Some(PendingTrackingRequest {
            kind: "playback",
            url: Self::finalize_tracking_url(url, params),
            referer: self.referer_url.clone(),
            event_time,
            request_time,
        })
    }

    fn build_atr_request(&self, listened_millis: u64) -> Option<PendingTrackingRequest> {
        let atr_url = self.atr_url.as_deref()?;
        let watched = listened_millis.min(self.length_millis);
        if watched == 0 {
            return None;
        }

        let (url, mut params) = Self::build_query_map(atr_url)?;
        let (event_time, request_time, wall_elapsed_millis, lact_millis) =
            self.next_request_metadata();

        Self::set_query_param(&mut params, "cpn", self.cpn.clone());
        Self::set_query_param(&mut params, "ver", "2");
        Self::set_query_param(&mut params, "cmt", format_millis(watched));
        Self::set_query_param(&mut params, "fmt", self.video_fmt.clone());
        Self::set_query_param(&mut params, "afmt", self.audio_fmt.clone());
        Self::set_query_param(&mut params, "muted", "0");
        Self::set_query_param(&mut params, "volume", "100");
        Self::set_query_param(&mut params, "vis", self.default_visibility());
        Self::set_query_param(&mut params, "rt", format_millis(wall_elapsed_millis));
        Self::set_query_param(&mut params, "lact", lact_millis.to_string());
        if !params.contains_key("len") {
            Self::set_query_param(&mut params, "len", format_millis(self.length_millis));
        }
        if !params.contains_key("mos") {
            Self::set_query_param(&mut params, "mos", "0");
        }
        self.apply_default_client_params(&mut params);

        Some(PendingTrackingRequest {
            kind: "atr",
            url: Self::finalize_tracking_url(url, params),
            referer: self.referer_url.clone(),
            event_time,
            request_time,
        })
    }

    fn build_request(
        http: &reqwest::Client,
        auth: Option<&String>,
        web_profile: &YtmWebProfile,
        request_url: &str,
        referer: &str,
        event_time: &str,
        request_time: &str,
    ) -> reqwest::RequestBuilder {
        let mut req = http
            .get(request_url)
            .header("user-agent", web_profile.user_agent.clone())
            .header("sec-ch-ua", web_profile.sec_ch_ua.clone())
            .header("sec-ch-ua-mobile", web_profile.sec_ch_ua_mobile.clone())
            .header("sec-ch-ua-platform", web_profile.sec_ch_ua_platform.clone())
            .header("referer", referer)
            .header("x-goog-authuser", "0")
            .header("x-goog-event-time", event_time)
            .header("x-goog-request-time", request_time)
            .header(
                "x-youtube-client-name",
                web_profile.header_client_name.clone(),
            )
            .header(
                "x-youtube-client-version",
                web_profile.client_version.clone(),
            )
            .header("x-youtube-device", web_profile.device_header())
            .header("x-youtube-time-zone", web_profile.time_zone.clone())
            .header(
                "x-youtube-utc-offset",
                web_profile.utc_offset_minutes.clone(),
            );
        if let Some(auth) = auth {
            req = req.header("authorization", auth.clone());
        }
        if let Some(visitor_id) = web_profile.visitor_data.as_ref() {
            req = req.header("x-goog-visitor-id", visitor_id);
        }
        if let Some(datasync_id) = web_profile.datasync_id.as_ref() {
            req = req.header("x-youtube-datasync-id", datasync_id);
        }
        if let Some(identity_token) = web_profile.identity_token.as_ref() {
            req = req.header("x-youtube-identity-token", identity_token);
        }
        if let Some(page_cl) = web_profile.page_cl.as_ref() {
            req = req.header("x-youtube-page-cl", page_cl);
        }
        if let Some(page_label) = web_profile.page_label.as_ref() {
            req = req.header("x-youtube-page-label", page_label);
        }
        req
    }

    fn dispatch_watchtime_request(&self, request: PendingWatchtimeRequest) {
        let rt = self.rt.clone();
        let http = self.http.clone();
        let auth = self.auth.clone();
        let web_profile = self.web_profile.clone();
        rt.spawn(async move {
            let req = Self::build_request(
                &http,
                auth.as_ref(),
                &web_profile,
                &request.url,
                &request.referer,
                &request.event_time,
                &request.request_time,
            );
            match req.send().await {
                Ok(r) => eprintln!(
                    "[TRACKING] watchtime reported: {}..{} ms state={} final={} ({})",
                    request.start_millis,
                    request.watched_millis,
                    request.state,
                    request.final_ping,
                    r.status()
                ),
                Err(e) => eprintln!("[TRACKING] watchtime failed: {e}"),
            }
        });
    }

    fn dispatch_watchtime_request_blocking(&self, request: PendingWatchtimeRequest) {
        let rt = self.rt.clone();
        let http = self.http.clone();
        let auth = self.auth.clone();
        let web_profile = self.web_profile.clone();
        rt.block_on(async move {
            let req = Self::build_request(
                &http,
                auth.as_ref(),
                &web_profile,
                &request.url,
                &request.referer,
                &request.event_time,
                &request.request_time,
            );
            match req.send().await {
                Ok(r) => eprintln!(
                    "[TRACKING] watchtime reported: {}..{} ms state={} final={} ({})",
                    request.start_millis,
                    request.watched_millis,
                    request.state,
                    request.final_ping,
                    r.status()
                ),
                Err(e) => eprintln!("[TRACKING] watchtime failed: {e}"),
            }
        });
    }

    fn dispatch_tracking_request(&self, request: PendingTrackingRequest) {
        let rt = self.rt.clone();
        let http = self.http.clone();
        let auth = self.auth.clone();
        let web_profile = self.web_profile.clone();
        rt.spawn(async move {
            let req = Self::build_request(
                &http,
                auth.as_ref(),
                &web_profile,
                &request.url,
                &request.referer,
                &request.event_time,
                &request.request_time,
            );
            match req.send().await {
                Ok(r) => eprintln!("[TRACKING] {} reported ({})", request.kind, r.status()),
                Err(e) => eprintln!("[TRACKING] {} failed: {e}", request.kind),
            }
        });
    }

    fn send_watchtime_range(
        &self,
        start_millis: u64,
        listened_millis: u64,
        state: &str,
        final_ping: bool,
    ) {
        let watched = listened_millis.min(self.length_millis);
        let Some(request) =
            self.build_watchtime_request(start_millis, listened_millis, state, final_ping)
        else {
            return;
        };

        if !final_ping {
            self.last_reported_millis.store(watched, Ordering::Relaxed);
        }
        self.dispatch_watchtime_request(request);
    }

    pub fn maybe_report_watchtime(&self, listened_millis: u64) {
        let watched = listened_millis.min(self.length_millis);
        if watched == 0 {
            return;
        }

        let last_reported_millis = self.last_reported_millis.load(Ordering::Relaxed);
        let Some(next_flush_millis) = self.next_flush_after(last_reported_millis) else {
            return;
        };

        if watched < next_flush_millis && watched < self.length_millis {
            return;
        }

        self.send_watchtime_range(last_reported_millis, watched, "playing", false);
    }

    pub fn maybe_report_atr(&self, listened_millis: u64) {
        if self.atr_url.is_none()
            || listened_millis == 0
            || self.session_started_at.elapsed() < ATR_REPORT_DELAY
        {
            return;
        }

        if self
            .atr_reported
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let Some(request) = self.build_atr_request(listened_millis) else {
            self.atr_reported.store(false, Ordering::Relaxed);
            return;
        };

        self.dispatch_tracking_request(request);
    }

    /// Fire the watchtime URL with real listened time. Call this on song change/stop.
    pub fn report_watchtime(&self, listened_millis: u64) {
        let last_reported_millis = self.last_reported_millis.load(Ordering::Relaxed);
        let watched = listened_millis.min(self.length_millis);
        let paused_request =
            self.build_watchtime_request(last_reported_millis, watched, "paused", false);
        if paused_request.is_some() {
            self.last_reported_millis.store(watched, Ordering::Relaxed);
        }

        let final_request = self.build_watchtime_request(watched, watched, "paused", true);

        let rt = self.rt.clone();
        let http = self.http.clone();
        let auth = self.auth.clone();
        let web_profile = self.web_profile.clone();
        rt.spawn(async move {
            for request in [paused_request, final_request].into_iter().flatten() {
                let req = Self::build_request(
                    &http,
                    auth.as_ref(),
                    &web_profile,
                    &request.url,
                    &request.referer,
                    &request.event_time,
                    &request.request_time,
                );
                match req.send().await {
                    Ok(r) => eprintln!(
                        "[TRACKING] watchtime reported: {}..{} ms state={} final={} ({})",
                        request.start_millis,
                        request.watched_millis,
                        request.state,
                        request.final_ping,
                        r.status()
                    ),
                    Err(e) => eprintln!("[TRACKING] watchtime failed: {e}"),
                }
            }
        });
    }

    pub fn report_watchtime_on_exit(&self, listened_millis: u64) {
        let last_reported_millis = self.last_reported_millis.load(Ordering::Relaxed);
        let watched = listened_millis.min(self.length_millis);
        let paused_request =
            self.build_watchtime_request(last_reported_millis, watched, "paused", false);
        if paused_request.is_some() {
            self.last_reported_millis.store(watched, Ordering::Relaxed);
        }

        let final_request = self.build_watchtime_request(watched, watched, "paused", true);

        for request in [paused_request, final_request].into_iter().flatten() {
            self.dispatch_watchtime_request_blocking(request);
        }
    }
}

fn unix_epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn unix_epoch_millis_u64() -> u64 {
    unix_epoch_millis().min(u64::MAX as u128) as u64
}

fn default_static_root() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("Music/Static")
}

fn resolve_storage_root(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return default_static_root();
    }

    if trimmed == "~" {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
    }

    if let Some(rest) = trimmed.strip_prefix("~/") {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join(rest);
    }

    PathBuf::from(trimmed)
}

fn static_storage_root() -> PathBuf {
    super::session_network::get_storage_root()
        .map(|raw| resolve_storage_root(&raw))
        .unwrap_or_else(default_static_root)
}

fn ytm_web_profile_database_path() -> PathBuf {
    static_storage_root().join(YTM_WEB_PROFILE_DB_FILE_NAME)
}

fn open_ytm_web_profile_database() -> Result<SqliteConnection> {
    let path = ytm_web_profile_database_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create YTM web profile database dir {}",
                parent.display()
            )
        })?;
    }

    let db = SqliteConnection::open(&path)
        .with_context(|| format!("Failed to open YTM web profile database {}", path.display()))?;
    db.execute(
        &format!(
            "CREATE TABLE IF NOT EXISTS {YTM_WEB_PROFILE_TABLE} (
                id TEXT PRIMARY KEY,
                profile_json TEXT NOT NULL,
                updated_at_unix_millis INTEGER NOT NULL
            )"
        ),
        [],
    )
    .context("Failed to initialize YTM web profile cache")?;
    Ok(db)
}

fn load_cached_ytm_web_profile() -> Result<Option<YtmWebProfile>> {
    let db = open_ytm_web_profile_database()?;
    let profile_json: Option<String> = db
        .query_row(
            &format!("SELECT profile_json FROM {YTM_WEB_PROFILE_TABLE} WHERE id = ?1"),
            params![YTM_WEB_PROFILE_ROW_ID],
            |row| row.get(0),
        )
        .optional()
        .context("Failed to read YTM web profile cache")?;

    profile_json
        .map(|json| {
            serde_json::from_str::<YtmWebProfile>(&json)
                .context("Failed to parse cached YTM web profile")
        })
        .transpose()
}

fn store_cached_ytm_web_profile(profile: &YtmWebProfile) -> Result<()> {
    let db = open_ytm_web_profile_database()?;
    let profile_json =
        serde_json::to_string(profile).context("Failed to encode YTM web profile")?;
    db.execute(
        &format!(
            "INSERT OR REPLACE INTO {YTM_WEB_PROFILE_TABLE}
             (id, profile_json, updated_at_unix_millis)
             VALUES (?1, ?2, ?3)"
        ),
        params![
            YTM_WEB_PROFILE_ROW_ID,
            profile_json,
            profile.fetched_at_unix_millis as i64
        ],
    )
    .context("Failed to store YTM web profile cache")?;
    Ok(())
}

fn extract_balanced_json_object(input: &str, start: usize) -> Option<&str> {
    let bytes = input.as_bytes();
    let object_start = bytes
        .get(start..)?
        .iter()
        .position(|byte| *byte == b'{')
        .map(|offset| start + offset)?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for idx in object_start..bytes.len() {
        let byte = bytes[idx];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'{' => depth = depth.saturating_add(1),
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return input.get(object_start..=idx);
                }
            }
            _ => {}
        }
    }

    None
}

fn ytcfg_string(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.as_i64().map(|number| number.to_string()))
                .or_else(|| value.as_u64().map(|number| number.to_string()))
        })
        .filter(|value| !value.trim().is_empty())
}

fn ytm_web_player_js_url(map: &serde_json::Map<String, Value>) -> Option<String> {
    let configs = map
        .get("WEB_PLAYER_CONTEXT_CONFIGS")
        .and_then(|value| value.as_object())?;
    let config = configs
        .get("WEB_PLAYER_CONTEXT_CONFIG_ID_MUSIC_WATCH")
        .or_else(|| configs.values().next())?;
    config
        .get("jsUrl")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn absolute_ytm_url(path_or_url: &str) -> Option<String> {
    let trimmed = path_or_url.trim();
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        return Some(trimmed.to_string());
    }
    if trimmed.starts_with('/') {
        return Some(format!("{YTM_ORIGIN}{trimmed}"));
    }
    None
}

fn signature_timestamp_from_player_js(js: &str) -> Option<u64> {
    let re = Regex::new(r"signatureTimestamp\s*:\s*(\d+)")
        .expect("signature timestamp regex should compile");
    re.captures(js)
        .and_then(|captures| captures.get(1))
        .and_then(|capture| capture.as_str().parse::<u64>().ok())
}

fn ytm_web_profile_from_html(html: &str, fetched_at_unix_millis: u64) -> Result<YtmWebProfile> {
    let re = Regex::new(r"ytcfg\.set\s*\(").expect("ytcfg regex should compile");
    let mut config = serde_json::Map::<String, Value>::new();

    for mat in re.find_iter(html) {
        let Some(raw_object) = extract_balanced_json_object(html, mat.end()) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(raw_object) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        for (key, value) in object {
            config.insert(key.clone(), value.clone());
        }
    }

    let mut profile = YtmWebProfile::fallback();
    profile.fetched_at_unix_millis = fetched_at_unix_millis;
    profile.header_client_name = ytcfg_string(&config, "INNERTUBE_CONTEXT_CLIENT_NAME")
        .unwrap_or_else(|| YTM_WEB_REMIX_HEADER_CLIENT_NAME.to_string());
    profile.client_name = YTM_WEB_REMIX_CLIENT_NAME.to_string();
    profile.client_version = ytcfg_string(&config, "INNERTUBE_CONTEXT_CLIENT_VERSION")
        .unwrap_or_else(|| YTM_WEB_REMIX_CLIENT_VERSION.to_string());
    profile.player_js_url = ytm_web_player_js_url(&config);
    profile.page_cl =
        ytcfg_string(&config, "PAGE_CL").or_else(|| ytcfg_string(&config, "PAGE_BUILD_CL"));
    profile.page_label = ytcfg_string(&config, "PAGE_BUILD_LABEL");
    profile.visitor_data = ytcfg_string(&config, "VISITOR_DATA");
    profile.datasync_id = ytcfg_string(&config, "DATASYNC_ID")
        .or_else(|| ytcfg_string(&config, "DELEGATED_SESSION_ID"));
    profile.identity_token = ytcfg_string(&config, "ID_TOKEN");
    profile.hl = ytcfg_string(&config, "INNERTUBE_CONTEXT_HL").unwrap_or(profile.hl);
    profile.gl = ytcfg_string(&config, "INNERTUBE_CONTEXT_GL").unwrap_or(profile.gl);

    if profile.client_version.trim().is_empty() {
        bail!("YTM web profile did not include a client version");
    }

    Ok(profile)
}

fn parse_secs_to_millis(value: Option<&str>) -> Option<u64> {
    value
        .and_then(|raw| raw.parse::<f64>().ok())
        .map(|secs| (secs * 1000.0).round() as u64)
}

fn parse_json_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| {
            value
                .as_i64()
                .and_then(|number| (number >= 0).then_some(number as u64))
        })
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
}

fn text_from_navigation_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    value
        .get("simpleText")
        .and_then(|text| text.as_str())
        .map(str::to_string)
        .or_else(|| {
            value
                .get("runs")
                .and_then(|runs| runs.as_array())
                .map(|runs| {
                    runs.iter()
                        .filter_map(|run| run.get("text").and_then(|text| text.as_str()))
                        .collect::<String>()
                })
                .filter(|text| !text.trim().is_empty())
        })
}

fn decode_basic_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

fn normalize_synced_lyric_text(raw: &str) -> String {
    let without_zero_width = raw.replace('\u{200b}', "");
    let decoded = decode_basic_html_entities(&without_zero_width);
    decoded
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn finalize_synced_lyric_lines(lines: &mut [SyncedLyricLine]) {
    if lines.is_empty() {
        return;
    }

    lines.sort_by_key(|line| line.start_millis);
    for index in 0..lines.len() {
        let next_start = lines.get(index + 1).map(|line| line.start_millis);
        let computed_end = match (lines[index].end_millis, next_start) {
            // The UI can only highlight one line at a time, so let each line
            // hand off to the next line's start instead of keeping overlaps.
            (_, Some(next_start)) => next_start,
            (Some(end_millis), None) => end_millis,
            (None, None) => lines[index].start_millis.saturating_add(4_000),
        };
        lines[index].end_millis = Some(computed_end.max(lines[index].start_millis + 1));
    }
}

fn format_millis(millis: u64) -> String {
    let mut rendered = format!("{:.3}", millis as f64 / 1000.0);
    while rendered.contains('.') && rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.pop();
    }
    rendered
}

pub struct YtMusicClient {
    http: reqwest::Client,
    #[allow(dead_code)]
    cookie: String,
    rt: Arc<tokio::runtime::Runtime>,
    sapisid: Option<String>,
    visitor_data: std::sync::Mutex<Option<String>>,
    /// OAuth refresh token for Android VR (premium content)
    vr_refresh_token: std::sync::Mutex<Option<String>>,
    /// Cached VR access token + expiry (epoch secs)
    vr_access_token: std::sync::Mutex<Option<(String, u64)>>,
}

unsafe impl Send for YtMusicClient {}
unsafe impl Sync for YtMusicClient {}

fn cookie_value(cookie: &str, name: &str) -> Option<String> {
    cookie.split(';').map(|s| s.trim()).find_map(|cookie| {
        let (cookie_name, value) = cookie.split_once('=')?;
        (cookie_name == name && !value.is_empty()).then(|| value.to_string())
    })
}

fn prepare_cookie_for_sapisid_auth(cookie: &str) -> (String, Option<String>, Option<&'static str>) {
    if let Some(sapisid) = cookie_value(cookie, "SAPISID") {
        return (cookie.to_string(), Some(sapisid), Some("SAPISID"));
    }

    // Chromium exports sometimes keep only the secure PAPISID cookies.
    // Mirroring them into a plain SAPISID cookie restores authenticated
    // youtubei/player responses, while APISID alone is not sufficient.
    for fallback_name in ["__Secure-3PAPISID", "__Secure-1PAPISID"] {
        if let Some(sapisid) = cookie_value(cookie, fallback_name) {
            let mut normalized_cookie = cookie.trim().trim_end_matches(';').to_string();
            if !normalized_cookie.is_empty() {
                normalized_cookie.push_str("; ");
            }
            normalized_cookie.push_str("SAPISID=");
            normalized_cookie.push_str(&sapisid);
            return (normalized_cookie, Some(sapisid), Some(fallback_name));
        }
    }

    (cookie.to_string(), None, None)
}

fn generate_cpn() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let hash = Sha1::digest(nanos.to_le_bytes());
    const ALPHABET: &[u8; 64] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";
    hash.iter()
        .take(16)
        .map(|byte| ALPHABET[(byte & 0x3f) as usize] as char)
        .collect()
}

fn generate_sapisidhash(sapisid: &str, origin: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let input = format!("{timestamp} {sapisid} {origin}");
    let hash = Sha1::digest(input.as_bytes());
    format!("SAPISIDHASH {timestamp}_{hash:x}")
}

fn rewrite_tracking_base_url(url: &str) -> String {
    url.replace("https://s.youtube.com/", "https://music.youtube.com/")
}

impl YtMusicClient {
    pub fn new(cookie: &str) -> Result<Self> {
        let (normalized_cookie, sapisid, sapisid_source) = prepare_cookie_for_sapisid_auth(cookie);

        let mut headers = HeaderMap::new();
        headers.insert("cookie", HeaderValue::from_str(&normalized_cookie)?);
        headers.insert("user-agent", HeaderValue::from_static(YTM_USER_AGENT));
        headers.insert("accept", HeaderValue::from_static("*/*"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("origin", HeaderValue::from_static(YTM_ORIGIN));
        headers.insert(
            "referer",
            HeaderValue::from_static("https://music.youtube.com/"),
        );
        headers.insert(
            "x-youtube-client-name",
            HeaderValue::from_static(YTM_WEB_REMIX_HEADER_CLIENT_NAME),
        );
        headers.insert(
            "x-youtube-client-version",
            HeaderValue::from_static(YTM_WEB_REMIX_CLIENT_VERSION),
        );
        headers.insert("x-origin", HeaderValue::from_static(YTM_ORIGIN));

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("Failed to build HTTP client")?;

        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("Failed to create tokio runtime")?,
        );

        if let Some(source) = sapisid_source.filter(|source| *source != "SAPISID") {
            eprintln!("[AUTH] using {source} as a SAPISID fallback for authenticated requests");
        }

        Ok(Self {
            http,
            cookie: normalized_cookie,
            rt,
            sapisid,
            visitor_data: std::sync::Mutex::new(None),
            vr_refresh_token: std::sync::Mutex::new(None),
            vr_access_token: std::sync::Mutex::new(None),
        })
    }

    fn auth_header(&self) -> Option<String> {
        self.sapisid
            .as_ref()
            .map(|id| generate_sapisidhash(id, YTM_ORIGIN))
    }

    #[allow(dead_code)]
    fn auth_header_for_origin(&self, origin: &str) -> Option<String> {
        self.sapisid
            .as_ref()
            .map(|id| generate_sapisidhash(id, origin))
    }

    fn post(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.post(url);
        if let Some(auth) = self.auth_header() {
            req = req.header("authorization", auth);
        }
        req
    }

    fn apply_ytm_web_profile_headers(
        &self,
        req: reqwest::RequestBuilder,
        profile: &YtmWebProfile,
    ) -> reqwest::RequestBuilder {
        let mut req = req
            .header("user-agent", profile.user_agent.clone())
            .header("sec-ch-ua", profile.sec_ch_ua.clone())
            .header("sec-ch-ua-mobile", profile.sec_ch_ua_mobile.clone())
            .header("sec-ch-ua-platform", profile.sec_ch_ua_platform.clone())
            .header("x-youtube-client-name", profile.header_client_name.clone())
            .header("x-youtube-client-version", profile.client_version.clone())
            .header("x-youtube-device", profile.device_header())
            .header("x-youtube-time-zone", profile.time_zone.clone())
            .header("x-youtube-utc-offset", profile.utc_offset_minutes.clone());

        if let Some(visitor_data) = profile.visitor_data.as_ref() {
            req = req.header("x-goog-visitor-id", visitor_data);
        }
        if let Some(datasync_id) = profile.datasync_id.as_ref() {
            req = req.header("x-youtube-datasync-id", datasync_id);
        }
        if let Some(identity_token) = profile.identity_token.as_ref() {
            req = req.header("x-youtube-identity-token", identity_token);
        }
        if let Some(page_cl) = profile.page_cl.as_ref() {
            req = req.header("x-youtube-page-cl", page_cl);
        }
        if let Some(page_label) = profile.page_label.as_ref() {
            req = req.header("x-youtube-page-label", page_label);
        }

        req
    }

    async fn fetch_ytm_web_profile(&self) -> Result<YtmWebProfile> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .context("Failed to build YTM web profile HTTP client")?;
        let req = http
            .get(YTM_ORIGIN)
            .header("cookie", self.cookie.clone())
            .header("user-agent", YTM_USER_AGENT)
            .header(
                "accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("accept-language", "en-US,en;q=0.9,en-IN;q=0.8")
            .header("referer", format!("{YTM_ORIGIN}/"))
            .header("sec-ch-ua", TRACKING_SEC_CH_UA)
            .header("sec-ch-ua-mobile", TRACKING_SEC_CH_UA_MOBILE)
            .header("sec-ch-ua-platform", TRACKING_SEC_CH_UA_PLATFORM);

        let html = req
            .send()
            .await
            .context("Failed to fetch YouTube Music web shell")?
            .error_for_status()
            .context("YouTube Music rejected web shell request")?
            .text()
            .await
            .context("Failed to read YouTube Music web shell")?;
        let mut profile = ytm_web_profile_from_html(&html, unix_epoch_millis_u64())?;

        if let Some(player_js_url) = profile.player_js_url.as_deref().and_then(absolute_ytm_url) {
            match http
                .get(&player_js_url)
                .header("user-agent", YTM_USER_AGENT)
                .header("referer", format!("{YTM_ORIGIN}/"))
                .send()
                .await
            {
                Ok(resp) => match resp.error_for_status() {
                    Ok(resp) => match resp.text().await {
                        Ok(js) => {
                            if let Some(signature_timestamp) =
                                signature_timestamp_from_player_js(&js)
                            {
                                profile.signature_timestamp = signature_timestamp;
                            } else {
                                eprintln!(
                                    "[TRACKING] YTM player JS did not expose signatureTimestamp; using {}",
                                    profile.signature_timestamp
                                );
                            }
                        }
                        Err(err) => {
                            eprintln!(
                                "[TRACKING] failed to read YTM player JS for signatureTimestamp: {err}"
                            );
                        }
                    },
                    Err(err) => {
                        eprintln!(
                            "[TRACKING] YTM player JS request for signatureTimestamp was rejected: {err}"
                        );
                    }
                },
                Err(err) => {
                    eprintln!(
                        "[TRACKING] failed to fetch YTM player JS for signatureTimestamp: {err}"
                    );
                }
            }
        }

        Ok(profile)
    }

    async fn ytm_web_profile(&self, force_refresh: bool) -> YtmWebProfile {
        let cached = match load_cached_ytm_web_profile() {
            Ok(cached) => cached,
            Err(err) => {
                eprintln!("[TRACKING] failed to read cached YTM WEB_REMIX profile: {err}");
                None
            }
        };

        if !force_refresh {
            if let Some(profile) = cached.as_ref() {
                if profile.is_fresh_for_tracking(unix_epoch_millis_u64()) {
                    return profile.clone();
                }
                if profile.is_fresh(unix_epoch_millis_u64()) && !profile.has_page_identity() {
                    eprintln!(
                        "[TRACKING] cached YTM WEB_REMIX profile is missing page identity; refreshing"
                    );
                }
            }
        }

        match self.fetch_ytm_web_profile().await {
            Ok(profile) => {
                if let Err(err) = store_cached_ytm_web_profile(&profile) {
                    eprintln!("[TRACKING] failed to store YTM WEB_REMIX profile: {err}");
                }
                profile
            }
            Err(err) => {
                eprintln!("[TRACKING] failed to refresh YTM WEB_REMIX profile: {err}");
                cached.unwrap_or_else(YtmWebProfile::fallback)
            }
        }
    }

    fn client_context() -> Value {
        Self::client_context_with(YTM_WEB_REMIX_CLIENT_NAME, YTM_WEB_REMIX_CLIENT_VERSION)
    }

    fn client_context_with(client_name: &str, client_version: &str) -> Value {
        json!({
            "context": {
                "client": {
                    "clientName": client_name,
                    "clientVersion": client_version,
                    "hl": "en",
                    "gl": "US",
                }
            }
        })
    }

    fn tracking_player_body(
        video_id: &str,
        profile: &YtmWebProfile,
        cpn: &str,
        playlist_id: Option<&str>,
        params: Option<&str>,
    ) -> Value {
        let mut body = Self::client_context_with(&profile.client_name, &profile.client_version);
        body["context"]["client"]["hl"] = json!(profile.hl);
        body["context"]["client"]["gl"] = json!(profile.gl);
        body["context"]["client"]["userAgent"] = json!(profile.user_agent);
        body["context"]["client"]["browserName"] = json!(profile.browser_name);
        body["context"]["client"]["browserVersion"] = json!(profile.browser_version);
        body["context"]["client"]["osName"] = json!(profile.os_name);
        body["context"]["client"]["platform"] = json!(profile.platform);
        body["videoId"] = json!(video_id);
        body["cpn"] = json!(cpn);
        body["playbackContext"] = json!({
            "contentPlaybackContext": {
                "html5Preference": "HTML5_PREF_WANTS",
                "lactMilliseconds": "0",
                "referer": format!("{YTM_ORIGIN}/"),
                "signatureTimestamp": profile.signature_timestamp,
                "autonavState": "STATE_OFF",
                "autoCaptionsDefaultOn": false,
                "vis": 1
            },
            "devicePlaybackCapabilities": {
                "supportXhr": true
            }
        });
        body["captionParams"] = json!({});
        if let Some(playlist_id) = playlist_id {
            body["playlistId"] = json!(playlist_id);
        }
        if let Some(params) = params {
            body["params"] = json!(params);
        }
        body
    }

    fn mobile_client_context() -> Value {
        json!({
            "context": {
                "client": {
                    "clientName": YTM_MOBILE_CLIENT_NAME,
                    "clientVersion": YTM_MOBILE_CLIENT_VERSION,
                    "hl": "en",
                    "gl": "US",
                },
                "user": {}
            }
        })
    }

    pub fn get_home_page(&self, continuation: Option<&str>) -> Result<HomePage> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            if let Some(continuation) = continuation {
                body["continuation"] = json!(continuation);
            } else {
                body["browseId"] = json!("FEmusic_home");
            }

            let resp = self
                .post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch home page")?;

            let json: Value = resp.json().await.context("Failed to parse home response")?;
            parsers::parse_home_page(&json)
        })
    }

    pub fn get_history(&self) -> Result<Vec<Track>> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["browseId"] = json!("FEmusic_history");

            let resp = self
                .post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch history")?;

            let json: Value = resp
                .json()
                .await
                .context("Failed to parse history response")?;
            let sections = parsers::parse_home(&json)?;
            Ok(sections
                .into_iter()
                .flat_map(|section| section.contents)
                .filter_map(|item| match item {
                    HomeItem::Track(track) => Some(track),
                    HomeItem::BrowseCard(_) => None,
                })
                .collect())
        })
    }

    pub fn search(&self, query: &str) -> Result<SearchResult> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["query"] = json!(query);

            let resp = self
                .post(&format!("{YTM_BASE_URL}/search?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to search")?;

            let json: Value = resp
                .json()
                .await
                .context("Failed to parse search response")?;
            parsers::parse_search_results(&json)
        })
    }

    pub fn get_search_suggestions(&self, query: &str) -> Result<Vec<String>> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["input"] = json!(query);

            let resp = self
                .post(&format!(
                    "{YTM_BASE_URL}/music/get_search_suggestions?{YTM_PARAMS}"
                ))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch search suggestions")?;

            let json: Value = resp
                .json()
                .await
                .context("Failed to parse search suggestions response")?;
            Ok(parsers::parse_search_suggestions(&json))
        })
    }

    pub fn get_playlist(&self, playlist_id: &str) -> Result<Vec<Track>> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            let browse_id = if playlist_id.starts_with("VL") {
                playlist_id.to_string()
            } else {
                format!("VL{playlist_id}")
            };
            body["browseId"] = json!(browse_id);

            let resp = self
                .post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch playlist")?;

            let json: Value = resp
                .json()
                .await
                .context("Failed to parse playlist response")?;
            parsers::parse_playlist_tracks(&json)
        })
    }

    pub fn get_browse_tracks(&self, browse_id: &str) -> Result<Vec<Track>> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["browseId"] = json!(browse_id);

            let resp = self
                .post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .with_context(|| format!("Failed to fetch browse page {browse_id}"))?;

            let json: Value = resp
                .json()
                .await
                .with_context(|| format!("Failed to parse browse response for {browse_id}"))?;
            parsers::parse_browse_tracks(&json)
        })
    }

    pub fn get_channel_page(&self, browse_id: &str) -> Result<BrowseSectionsPage> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["browseId"] = json!(browse_id);

            let resp = self
                .post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .with_context(|| format!("Failed to fetch channel page {browse_id}"))?;

            let json: Value = resp
                .json()
                .await
                .with_context(|| format!("Failed to parse channel response for {browse_id}"))?;
            parsers::parse_channel_page(&json)
        })
    }

    pub fn get_library_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["browseId"] = json!("FEmusic_liked_playlists");

            let resp = self
                .post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch library playlists")?;

            let json: Value = resp
                .json()
                .await
                .context("Failed to parse library playlists response")?;
            parsers::parse_library_playlists(&json)
        })
    }

    pub fn get_song_rating(&self, video_id: &str) -> Result<SongRating> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["videoId"] = json!(video_id);
            body["playlistId"] = json!(format!("RDAMVM{video_id}"));
            body["isAudioOnly"] = json!(true);
            body["tunerSettingValue"] = json!("AUTOMIX_SETTING_NORMAL");

            let resp = self
                .post(&format!("{YTM_BASE_URL}/next?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch song rating")?;

            let json: Value = resp
                .json()
                .await
                .context("Failed to parse song rating response")?;

            Ok(Self::find_like_status(&json)
                .map(SongRating::from_ytmusic)
                .unwrap_or(SongRating::Indifferent))
        })
    }

    pub fn get_synced_lyrics(&self, video_id: &str) -> Result<Option<SyncedLyrics>> {
        self.rt.block_on(async {
            if let Some(browse_id) = self.get_lyrics_browse_id(video_id).await? {
                if let Some(lyrics) = self.get_timed_lyrics_from_browse_id(&browse_id).await? {
                    return Ok(Some(lyrics));
                }
            }

            let mut body = Self::client_context();
            body["videoId"] = json!(video_id);

            let player = self.player_request(&body).await?;
            let Some(track) = Self::best_caption_track(&player) else {
                return Ok(None);
            };
            let caption_url = Self::json3_caption_url(&track.base_url)?;
            let response = self
                .http
                .get(&caption_url)
                .send()
                .await
                .with_context(|| format!("Failed to fetch synced lyrics for {video_id}"))?;
            let captions: Value = response.json().await.with_context(|| {
                format!("Failed to parse synced lyrics response for {video_id}")
            })?;
            Ok(Self::parse_synced_lyrics(track, &captions))
        })
    }

    pub fn rate_song(&self, video_id: &str, rating: SongRating) -> Result<()> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["target"] = json!({ "videoId": video_id });

            self.post(&format!(
                "{YTM_BASE_URL}/{}?{YTM_PARAMS}",
                rating.endpoint()
            ))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to submit rating for {video_id}"))?
            .error_for_status()
            .with_context(|| format!("YouTube Music rejected rating for {video_id}"))?;

            Ok(())
        })
    }

    pub fn submit_feedback_token(&self, feedback_token: &str) -> Result<()> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["feedbackTokens"] = json!([feedback_token]);

            self.post(&format!("{YTM_BASE_URL}/feedback?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to submit feedback")?
                .error_for_status()
                .context("YouTube Music rejected feedback request")?;

            Ok(())
        })
    }

    /// Start a radio from a video, or switch to a different radio filter.
    ///
    /// - To start the default radio: `get_radio("VIDEO_ID", None, None)`
    /// - To switch filter: `get_radio("VIDEO_ID", Some(chip.playlist_id), Some(chip.params))`
    pub fn get_radio(
        &self,
        video_id: &str,
        playlist_id: Option<&str>,
        params: Option<&str>,
    ) -> Result<RadioQueue> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["videoId"] = json!(video_id);
            body["playlistId"] = json!(playlist_id.unwrap_or(&format!("RDAMVM{video_id}")));
            body["isAudioOnly"] = json!(true);
            body["tunerSettingValue"] = json!("AUTOMIX_SETTING_NORMAL");
            if let Some(p) = params {
                body["params"] = json!(p);
            }

            let resp = self
                .post(&format!("{YTM_BASE_URL}/next?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch radio")?;

            let json: Value = resp
                .json()
                .await
                .context("Failed to parse radio response")?;
            parsers::parse_radio(&json)
        })
    }

    /// Report playback start to YouTube and return tracking info.
    /// Call this off the playback-critical path so local files can start instantly.
    pub fn start_playback_tracking(
        &self,
        video_id: &str,
        session_started_at: Instant,
        playlist_id: Option<&str>,
        params: Option<&str>,
    ) -> Option<TrackingInfo> {
        self.rt.block_on(self.start_playback_tracking_async(
            video_id,
            session_started_at,
            playlist_id,
            params,
        ))
    }

    /// Get just the streaming URL without reporting anything to YouTube.
    /// Use this for cache downloads and other non-playback fetches.
    pub fn get_audio_url_no_track(&self, video_id: &str) -> Result<String> {
        self.rt.block_on(async {
            // Try Android VR client first — this is the primary path for playable URLs.
            eprintln!("[PLAYER] trying Android VR client for {video_id}...");
            let vr_err = match self.try_android_vr_audio(video_id).await {
                Ok(url) => {
                    eprintln!("[PLAYER] got direct URL via Android VR");
                    return Ok(url);
                }
                Err(e) => {
                    eprintln!("[PLAYER] Android VR failed: {e}, trying web client...");
                    e
                }
            };

            // Fallback: web client (may return direct URL without cipher)
            let mut body = Self::client_context();
            body["videoId"] = json!(video_id);

            let json = self.player_request(&body).await?;
            let formats = Self::sorted_audio_formats(&json)?;
            for audio in &formats {
                if let Some(url) = Self::format_direct_url(audio) {
                    eprintln!("[PLAYER] got direct URL via web client");
                    return Ok(url.to_string());
                }
            }

            bail!("Could not get audio URL: {vr_err}; web client returned no direct audio URL")
        })
    }

    async fn tracking_player_response(
        &self,
        video_id: &str,
        cpn: &str,
        playlist_id: Option<&str>,
        params: Option<&str>,
        profile: &YtmWebProfile,
    ) -> Result<Value> {
        let body = Self::tracking_player_body(video_id, profile, cpn, playlist_id, params);
        let req = self
            .apply_ytm_web_profile_headers(
                self.post(&format!("{YTM_BASE_URL}/player?{YTM_PARAMS}")),
                profile,
            )
            .json(&body);
        let resp = req
            .send()
            .await
            .with_context(|| format!("WEB_REMIX player bootstrap request failed for {video_id}"))?;
        resp.json().await.with_context(|| {
            format!("WEB_REMIX player bootstrap response was not JSON for {video_id}")
        })
    }

    fn tracking_urls_from_player_response(
        json: &Value,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let tracking = &json["playbackTracking"];
        let playback_url = tracking
            .get("videostatsPlaybackUrl")
            .and_then(|url| url.get("baseUrl"))
            .and_then(|url| url.as_str())
            .map(rewrite_tracking_base_url);
        let watchtime_url = tracking
            .get("videostatsWatchtimeUrl")
            .and_then(|url| url.get("baseUrl"))
            .and_then(|url| url.as_str())
            .map(rewrite_tracking_base_url);
        let atr_url = tracking
            .get("atrUrl")
            .and_then(|url| url.get("baseUrl"))
            .and_then(|url| url.as_str())
            .map(rewrite_tracking_base_url);

        (playback_url, watchtime_url, atr_url)
    }

    fn collect_player_error_texts(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if let Some(text) = map.get("simpleText").and_then(|value| value.as_str()) {
                    out.push(text.to_string());
                }
                if let Some(runs) = map.get("runs").and_then(|value| value.as_array()) {
                    for run in runs {
                        if let Some(text) = run.get("text").and_then(|value| value.as_str()) {
                            out.push(text.to_string());
                        }
                    }
                }
                for value in map.values() {
                    Self::collect_player_error_texts(value, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    Self::collect_player_error_texts(item, out);
                }
            }
            _ => {}
        }
    }

    fn player_error_texts(json: &Value) -> Vec<String> {
        let mut texts = Vec::new();
        Self::collect_player_error_texts(&json["playabilityStatus"]["errorScreen"], &mut texts);
        let mut seen = std::collections::HashSet::new();
        texts
            .into_iter()
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty() && seen.insert(text.clone()))
            .collect()
    }

    fn player_response_requests_reload(json: &Value) -> bool {
        let status = json
            .pointer("/playabilityStatus/status")
            .and_then(|value| value.as_str());
        let reason = json
            .pointer("/playabilityStatus/reason")
            .and_then(|value| value.as_str());
        if status != Some("UNPLAYABLE") || reason != Some("Video unavailable") {
            return false;
        }

        Self::player_error_texts(json)
            .iter()
            .any(|text| text.eq_ignore_ascii_case("The page needs to be reloaded."))
            || json["playabilityStatus"]
                .to_string()
                .contains("RELOAD_PAGE")
    }

    fn log_tracking_bootstrap_missing(video_id: &str, json: &Value) {
        let status = json
            .pointer("/playabilityStatus/status")
            .and_then(|value| value.as_str())
            .unwrap_or("missing");
        let reason = json
            .pointer("/playabilityStatus/reason")
            .and_then(|value| value.as_str())
            .unwrap_or("missing");
        let texts = Self::player_error_texts(json);
        if texts.is_empty() {
            eprintln!(
                "[TRACKING] WEB_REMIX player bootstrap for {video_id} returned no tracking URLs (playability status={status}, reason={reason})"
            );
        } else {
            eprintln!(
                "[TRACKING] WEB_REMIX player bootstrap for {video_id} returned no tracking URLs (playability status={status}, reason={reason}, message={})",
                texts.join(" | ")
            );
        }
    }

    /// Report playback start to YouTube and return a TrackingInfo for deferred watchtime reporting.
    /// Fires the playback URL immediately; the caller fires watchtime later via TrackingInfo.
    /// NOTE: Must be called from an async context running on self.rt (e.g. inside block_on).
    async fn start_playback_tracking_async(
        &self,
        video_id: &str,
        session_started_at: Instant,
        playlist_id: Option<&str>,
        params: Option<&str>,
    ) -> Option<TrackingInfo> {
        let video_id = video_id.to_string();
        let rt = self.rt.clone();
        let auth = self.auth_header();
        let cpn = generate_cpn();

        {
            if auth.is_none() {
                eprintln!(
                    "[TRACKING] no SAPISID-derived auth cookie is available for {video_id}; player bootstrap may omit playbackTracking"
                );
            }

            let mut web_profile = self.ytm_web_profile(false).await;
            let mut json = match self
                .tracking_player_response(&video_id, &cpn, playlist_id, params, &web_profile)
                .await
            {
                Ok(json) => json,
                Err(err) => {
                    eprintln!("[TRACKING] WEB_REMIX player bootstrap failed for {video_id}: {err}");
                    web_profile = self.ytm_web_profile(true).await;
                    match self
                        .tracking_player_response(
                            &video_id,
                            &cpn,
                            playlist_id,
                            params,
                            &web_profile,
                        )
                        .await
                    {
                        Ok(json) => json,
                        Err(err) => {
                            eprintln!(
                                "[TRACKING] WEB_REMIX player bootstrap still failed after profile refresh for {video_id}: {err}"
                            );
                            return None;
                        }
                    }
                }
            };

            let (mut playback_url, mut watchtime_url, mut atr_url) =
                Self::tracking_urls_from_player_response(&json);
            if playback_url.is_none() && watchtime_url.is_none() && atr_url.is_none() {
                Self::log_tracking_bootstrap_missing(&video_id, &json);
                if Self::player_response_requests_reload(&json) {
                    eprintln!(
                        "[TRACKING] refreshing YTM WEB_REMIX profile after reload-page response for {video_id}"
                    );
                    web_profile = self.ytm_web_profile(true).await;
                    match self
                        .tracking_player_response(
                            &video_id,
                            &cpn,
                            playlist_id,
                            params,
                            &web_profile,
                        )
                        .await
                    {
                        Ok(retry_json) => {
                            json = retry_json;
                            (playback_url, watchtime_url, atr_url) =
                                Self::tracking_urls_from_player_response(&json);
                        }
                        Err(err) => {
                            eprintln!(
                                "[TRACKING] WEB_REMIX retry failed after profile refresh for {video_id}: {err}"
                            );
                            return None;
                        }
                    }
                }
            }

            if playback_url.is_none() && watchtime_url.is_none() && atr_url.is_none() {
                Self::log_tracking_bootstrap_missing(&video_id, &json);
                eprintln!("[TRACKING] playbackTracking block missing for {video_id}");
                eprintln!("[TRACKING] playback bootstrap URL missing for {video_id}");
                eprintln!("[TRACKING] watchtime URL missing for {video_id}");
                return None;
            };

            let tracking = &json["playbackTracking"];
            if tracking.is_null() {
                eprintln!("[TRACKING] playbackTracking block missing for {video_id}");
            }
            if playback_url.is_none() {
                eprintln!("[TRACKING] playback bootstrap URL missing for {video_id}");
            }
            if watchtime_url.is_none() {
                eprintln!("[TRACKING] watchtime URL missing for {video_id}");
            }
            let flush_points_millis = tracking
                .get("videostatsScheduledFlushWalltimeSeconds")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_u64())
                        .map(|secs| secs * 1000)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let default_flush_interval_millis = tracking
                .get("videostatsDefaultFlushIntervalSeconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(40)
                * 1000;

            let best_audio_format = Self::best_audio_format(&json).ok();
            let audio_fmt = best_audio_format
                .and_then(|format| format.get("itag").and_then(|v| v.as_u64()))
                .unwrap_or(141)
                .to_string();
            let video_fmt = "0".to_string();
            let referer_url = playback_url
                .as_deref()
                .and_then(|url| TrackingInfo::build_query_map(url))
                .and_then(|(_, params)| params.get("referrer").cloned())
                .unwrap_or_else(|| format!("{YTM_ORIGIN}/watch?v={video_id}"));
            let length_millis = best_audio_format
                .and_then(|format| parse_json_u64(format.get("approxDurationMs")))
                .or_else(|| {
                    watchtime_url
                        .as_deref()
                        .and_then(|url| TrackingInfo::build_query_map(url))
                        .and_then(|(_, params)| {
                            parse_secs_to_millis(params.get("len").map(|s| s.as_str()))
                        })
                })
                .or_else(|| {
                    parse_secs_to_millis(
                        json.pointer("/videoDetails/lengthSeconds")
                            .and_then(|v| v.as_str()),
                    )
                })
                .unwrap_or(300_000);

            let tracking_info = TrackingInfo {
                http: self.http.clone(),
                auth,
                watchtime_url,
                atr_url,
                cpn,
                referer_url,
                audio_fmt,
                video_fmt,
                length_millis,
                flush_points_millis,
                default_flush_interval_millis,
                last_reported_millis: AtomicU64::new(0),
                last_activity_millis: AtomicU64::new(0),
                atr_reported: AtomicBool::new(false),
                session_started_at,
                web_profile,
                rt,
            };

            if let Some(url) = playback_url.as_deref() {
                if let Some(request) = tracking_info.build_playback_request(url) {
                    tracking_info.dispatch_tracking_request(request);
                }
            }

            Some(tracking_info)
        }
    }

    /// Fetch visitorData from YouTube (cached per session).
    async fn ensure_visitor_data(&self) -> Result<String> {
        {
            let guard = self.visitor_data.lock().unwrap();
            if let Some(ref vd) = *guard {
                return Ok(vd.clone());
            }
        }

        eprintln!("[VISITOR] fetching visitorData from youtube.com...");
        let clean = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap();
        let page = clean
            .get("https://www.youtube.com")
            .header("user-agent", YTM_USER_AGENT)
            .send()
            .await
            .context("Failed to fetch youtube.com")?
            .text()
            .await?;

        let re = Regex::new(r#""visitorData":"([^"]+)""#).unwrap();
        let vd = re
            .captures(&page)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .context("Could not find visitorData in YouTube page")?;

        eprintln!("[VISITOR] got visitorData: {}...", &vd[..vd.len().min(30)]);
        let mut guard = self.visitor_data.lock().unwrap();
        *guard = Some(vd.clone());
        Ok(vd)
    }

    async fn try_android_vr_audio(&self, video_id: &str) -> Result<String> {
        // Always try unauthenticated first
        match self.vr_player_request(video_id, None).await {
            Ok(url) => return Ok(url),
            Err(e) => {
                let has_vr_auth = self.vr_refresh_token.lock().unwrap().is_some();

                if has_vr_auth {
                    eprintln!("[PLAYER] Android VR failed, retrying with VR OAuth: {e}");
                    let token = self.ensure_vr_access_token().await?;
                    return self.vr_player_request(video_id, Some(&token)).await;
                }
                return Err(e);
            }
        }
    }

    /// Send a single Android VR player request, optionally with OAuth Bearer auth.
    async fn vr_player_request(
        &self,
        video_id: &str,
        access_token: Option<&str>,
    ) -> Result<String> {
        let vr_ua = "com.google.android.apps.youtube.vr.oculus/1.71.26 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip";
        let vr_client = reqwest::Client::new();

        let mut body = json!({
            "context": {
                "client": {
                    "clientName": "ANDROID_VR",
                    "clientVersion": "1.71.26",
                    "deviceMake": "Oculus",
                    "deviceModel": "Quest 3",
                    "androidSdkVersion": 32,
                    "osName": "Android",
                    "osVersion": "12L",
                    "hl": "en",
                    "gl": "US",
                }
            },
            "videoId": video_id,
        });

        // Only include visitorData for unauthenticated requests
        if access_token.is_none() {
            let vd = self.ensure_visitor_data().await?;
            body["context"]["client"]["visitorData"] = json!(vd);
        }

        let mut req = vr_client
            .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
            .header("content-type", "application/json")
            .header("user-agent", vr_ua)
            .header("origin", "https://www.youtube.com")
            .header("x-youtube-client-name", "28")
            .header("x-youtube-client-version", "1.71.26");

        if let Some(token) = access_token {
            req = req.header("authorization", format!("Bearer {token}"));
        }

        let resp = req
            .json(&body)
            .send()
            .await
            .context("Failed to fetch Android VR player data")?;

        let json: Value = resp
            .json()
            .await
            .context("Failed to parse Android VR response")?;

        if let Some(reason) = json
            .pointer("/playabilityStatus/reason")
            .and_then(|r| r.as_str())
        {
            bail!("Unplayable: {reason}");
        }

        let formats = Self::sorted_audio_formats(&json)?;
        formats
            .into_iter()
            .find_map(Self::format_direct_url)
            .map(|s| s.to_string())
            .context("Android VR stream has no direct URL")
    }

    async fn get_lyrics_browse_id(&self, video_id: &str) -> Result<Option<String>> {
        let mut body = Self::client_context();
        body["enablePersistentPlaylistPanel"] = json!(true);
        body["videoId"] = json!(video_id);
        body["playlistId"] = json!(format!("RDAMVM{video_id}"));
        body["isAudioOnly"] = json!(true);
        body["tunerSettingValue"] = json!("AUTOMIX_SETTING_NORMAL");
        body["watchEndpointMusicSupportedConfigs"] = json!({
            "watchEndpointMusicConfig": {
                "hasPersistentPlaylistPanel": true,
                "musicVideoType": "MUSIC_VIDEO_TYPE_ATV"
            }
        });

        let response = self
            .post(&format!("{YTM_BASE_URL}/next?{YTM_PARAMS}"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to fetch lyrics browse id for {video_id}"))?;
        let json: Value = response
            .json()
            .await
            .with_context(|| format!("Failed to parse lyrics browse id response for {video_id}"))?;
        Ok(lyrics_browse_id_from_next_response(&json))
    }

    async fn get_timed_lyrics_from_browse_id(
        &self,
        browse_id: &str,
    ) -> Result<Option<SyncedLyrics>> {
        let mut body = Self::mobile_client_context();
        body["browseId"] = json!(browse_id);

        let response = self
            .post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to fetch timed lyrics browse page {browse_id}"))?;
        let json: Value = response.json().await.with_context(|| {
            format!("Failed to parse timed lyrics browse response for {browse_id}")
        })?;
        Ok(Self::parse_timed_lyrics_browse(&json))
    }

    async fn player_request(&self, body: &Value) -> Result<Value> {
        let resp = self
            .post(&format!("{YTM_BASE_URL}/player?{YTM_PARAMS}"))
            .json(body)
            .send()
            .await
            .context("Failed to fetch player data")?;
        resp.json().await.context("Failed to parse player response")
    }

    fn parse_timed_lyric_line(raw_line: &Value) -> Option<SyncedLyricLine> {
        let text = raw_line
            .get("lyricLine")
            .and_then(|text| text.as_str())
            .map(normalize_synced_lyric_text)
            .filter(|text| !text.is_empty())?;
        let cue_range = raw_line.get("cueRange")?;
        let start_millis = parse_json_u64(cue_range.get("startTimeMilliseconds"))?;
        let end_millis = parse_json_u64(cue_range.get("endTimeMilliseconds"))
            .filter(|end_millis| *end_millis > start_millis);

        Some(SyncedLyricLine {
            start_millis,
            end_millis,
            text,
        })
    }

    fn parse_timed_lyrics_browse(json: &Value) -> Option<SyncedLyrics> {
        let lyrics_data = json.pointer(
            "/contents/elementRenderer/newElement/type/componentType/model/timedLyricsModel/lyricsData",
        )?;
        let mut lines = lyrics_data
            .get("timedLyricsData")
            .and_then(|lines| lines.as_array())?
            .iter()
            .filter_map(Self::parse_timed_lyric_line)
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return None;
        }

        finalize_synced_lyric_lines(&mut lines);
        Some(SyncedLyrics {
            source: SyncedLyricsSource::TimedLyrics,
            language_code: None,
            track_name: lyrics_data
                .get("sourceMessage")
                .and_then(|message| text_from_navigation_text(Some(message)))
                .or_else(|| {
                    lyrics_data
                        .get("sourceMessage")
                        .and_then(|message| message.as_str())
                        .map(str::to_string)
                })
                .filter(|message| !message.trim().is_empty()),
            lines,
        })
    }

    fn best_caption_track(json: &Value) -> Option<CaptionTrack> {
        json.pointer("/captions/playerCaptionsTracklistRenderer/captionTracks")
            .and_then(|tracks| tracks.as_array())
            .and_then(|tracks| {
                tracks
                    .iter()
                    .filter_map(CaptionTrack::from_value)
                    .max_by_key(Self::caption_track_score)
            })
    }

    fn caption_track_score(track: &CaptionTrack) -> (u8, u8, u8, u8) {
        let has_manual_timings = u8::from(track.kind.as_deref() != Some("asr"));
        let mentions_lyrics = u8::from(
            track
                .name
                .as_deref()
                .map(|name| name.to_ascii_lowercase().contains("lyric"))
                .unwrap_or(false),
        );
        let english_preference = track.language_code.as_deref().map_or(0, |language| {
            if matches!(language, "en" | "en-US" | "en-GB") {
                2
            } else if language.starts_with("en-") {
                1
            } else {
                0
            }
        });
        let has_label = u8::from(track.name.is_some());
        (
            has_manual_timings,
            mentions_lyrics,
            english_preference,
            has_label,
        )
    }

    fn json3_caption_url(base_url: &str) -> Result<String> {
        let mut url = Url::parse(base_url).context("Caption track URL was invalid")?;
        let params = url
            .query_pairs()
            .filter(|(key, _)| key != "fmt")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        url.set_query(None);
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in params {
                pairs.append_pair(&key, &value);
            }
            pairs.append_pair("fmt", "json3");
        }
        Ok(url.to_string())
    }

    fn parse_synced_lyrics(track: CaptionTrack, json: &Value) -> Option<SyncedLyrics> {
        let events = json.get("events").and_then(|events| events.as_array())?;
        let mut lines = Vec::new();

        for event in events {
            let Some(start_millis) = parse_json_u64(event.get("tStartMs")) else {
                continue;
            };
            let text = event
                .get("segs")
                .and_then(|segments| segments.as_array())
                .map(|segments| {
                    segments
                        .iter()
                        .filter_map(|segment| segment.get("utf8").and_then(|text| text.as_str()))
                        .collect::<String>()
                })
                .map(|text| normalize_synced_lyric_text(&text))
                .unwrap_or_default();
            if text.is_empty() {
                continue;
            }

            let end_millis = parse_json_u64(event.get("dDurationMs"))
                .filter(|duration| *duration > 0)
                .map(|duration| start_millis.saturating_add(duration));

            lines.push(SyncedLyricLine {
                start_millis,
                end_millis,
                text,
            });
        }

        if lines.is_empty() {
            return None;
        }
        finalize_synced_lyric_lines(&mut lines);

        Some(SyncedLyrics {
            source: track.source(),
            language_code: track
                .language_code
                .filter(|language| !language.trim().is_empty()),
            track_name: track.name.filter(|name| !name.trim().is_empty()),
            lines,
        })
    }

    fn find_like_status(value: &Value) -> Option<&str> {
        if let Some(status) = value.get("likeStatus").and_then(|status| status.as_str()) {
            return Some(status);
        }

        match value {
            Value::Array(items) => items.iter().find_map(Self::find_like_status),
            Value::Object(map) => map.values().find_map(Self::find_like_status),
            _ => None,
        }
    }

    fn audio_formats<'a>(json: &'a Value) -> Result<Vec<&'a Value>> {
        let formats = json
            .pointer("/streamingData/adaptiveFormats")
            .and_then(|f| f.as_array())
            .context("No streaming data")?;

        let audio_formats = formats
            .iter()
            .filter(|f| {
                f.get("mimeType")
                    .and_then(|m| m.as_str())
                    .is_some_and(|m| m.starts_with("audio/"))
            })
            .collect::<Vec<_>>();

        if audio_formats.is_empty() {
            bail!("No audio streams found");
        }

        Ok(audio_formats)
    }

    fn format_direct_url(format: &Value) -> Option<&str> {
        format.get("url").and_then(|u| u.as_str())
    }

    fn audio_format_score(format: &Value) -> (u8, u64) {
        let playability_rank = u8::from(Self::format_direct_url(format).is_some());
        let bitrate = format.get("bitrate").and_then(|b| b.as_u64()).unwrap_or(0);
        (playability_rank, bitrate)
    }

    fn sorted_audio_formats<'a>(json: &'a Value) -> Result<Vec<&'a Value>> {
        let mut formats = Self::audio_formats(json)?;
        formats.sort_by(|a, b| Self::audio_format_score(b).cmp(&Self::audio_format_score(a)));
        Ok(formats)
    }

    fn best_audio_format(json: &Value) -> Result<&Value> {
        Self::audio_formats(json)?
            .into_iter()
            .max_by_key(|f| f.get("bitrate").and_then(|b| b.as_u64()).unwrap_or(0))
            .context("No audio streams found")
    }

    // ── VR OAuth (premium content) ───────────────────────────

    /// Set the VR refresh token (loaded from keyring at startup).
    pub fn set_vr_refresh_token(&self, token: &str) {
        *self.vr_refresh_token.lock().unwrap() = Some(token.to_string());
        eprintln!("[VR-AUTH] refresh token loaded");
    }

    #[allow(dead_code)]
    pub fn has_vr_auth(&self) -> bool {
        self.vr_refresh_token.lock().unwrap().is_some()
    }

    /// Get a valid VR access token, refreshing if expired.
    async fn ensure_vr_access_token(&self) -> Result<String> {
        // Check cached token
        {
            let guard = self.vr_access_token.lock().unwrap();
            if let Some((ref token, expiry)) = *guard {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                if now < expiry {
                    return Ok(token.clone());
                }
            }
        }

        let refresh_token = {
            let guard = self.vr_refresh_token.lock().unwrap();
            guard.clone().context("No VR refresh token available")?
        };

        eprintln!("[VR-AUTH] refreshing access token...");
        let body = json!({
            "client_id": VR_OAUTH_CLIENT_ID,
            "client_secret": VR_OAUTH_CLIENT_SECRET,
            "refresh_token": refresh_token,
            "grant_type": "refresh_token",
        });

        let resp = reqwest::Client::new()
            .post(VR_OAUTH_TOKEN_URL)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to refresh VR access token")?;

        let json: Value = resp
            .json()
            .await
            .context("Failed to parse VR token response")?;

        let access_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .context("No access_token in VR token response")?
            .to_string();
        let expires_in = json
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expiry = now + expires_in - 60; // refresh 60s early

        eprintln!("[VR-AUTH] got access token (expires in {expires_in}s)");
        *self.vr_access_token.lock().unwrap() = Some((access_token.clone(), expiry));
        Ok(access_token)
    }

    /// Start the OAuth device flow. Returns (device_code, user_code, verification_url).
    pub fn start_vr_device_flow(&self) -> Result<(String, String, String)> {
        self.rt.block_on(async {
            let body = json!({
                "client_id": VR_OAUTH_CLIENT_ID,
                "scope": "https://www.googleapis.com/auth/youtube",
                "device_id": uuid::Uuid::new_v4().to_string(),
                "device_model": "ANDROID_VR",
            });

            let resp = reqwest::Client::new()
                .post(VR_OAUTH_DEVICE_CODE_URL)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .context("Failed to start VR device flow")?;

            let json: Value = resp.json().await?;
            let device_code = json
                .get("device_code")
                .and_then(|v| v.as_str())
                .context("No device_code")?
                .to_string();
            let user_code = json
                .get("user_code")
                .and_then(|v| v.as_str())
                .context("No user_code")?
                .to_string();
            let verification_url = json
                .get("verification_url")
                .and_then(|v| v.as_str())
                .unwrap_or("https://www.google.com/device")
                .to_string();

            Ok((device_code, user_code, verification_url))
        })
    }

    /// Poll for the refresh token after the user has authorized the device code.
    /// Returns the refresh token on success, or an error if still pending/expired.
    pub fn poll_vr_device_flow(&self, device_code: &str) -> Result<String> {
        self.rt.block_on(async {
            let body = json!({
                "client_id": VR_OAUTH_CLIENT_ID,
                "client_secret": VR_OAUTH_CLIENT_SECRET,
                "code": device_code,
                "grant_type": "http://oauth.net/grant_type/device/1.0",
            });

            let resp = reqwest::Client::new()
                .post(VR_OAUTH_TOKEN_URL)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .context("Failed to poll VR device flow")?;

            let json: Value = resp.json().await?;

            if let Some(error) = json.get("error").and_then(|v| v.as_str()) {
                bail!("{error}");
            }

            let refresh_token = json
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .context("No refresh_token in response")?
                .to_string();

            // Also load the access token we got
            if let Some(access_token) = json.get("access_token").and_then(|v| v.as_str()) {
                let expires_in = json
                    .get("expires_in")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3600);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                *self.vr_access_token.lock().unwrap() =
                    Some((access_token.to_string(), now + expires_in - 60));
            }

            *self.vr_refresh_token.lock().unwrap() = Some(refresh_token.clone());
            eprintln!("[VR-AUTH] device flow complete, got refresh token");
            Ok(refresh_token)
        })
    }

    pub fn check_auth(&self) -> Result<bool> {
        self.rt.block_on(async {
            let body = Self::client_context();

            let resp = self.post(&format!("{YTM_BASE_URL}/account/account_menu?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to check auth")?;

            let json: Value = resp.json().await?;

            let is_authed = json
                .pointer("/actions/0/openPopupAction/popup/multiPageMenuRenderer/header/activeAccountHeaderRenderer/accountName")
                .is_some()
                || json
                    .pointer("/actions/0/openPopupAction/popup/multiPageMenuRenderer/header")
                    .is_some();

            Ok(is_authed)
        })
    }
}

/// Extract a YouTube video id from a URL (with or without scheme) or return
/// the input unchanged if it already looks like a bare id.
pub fn extract_video_id_from_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let normalized = if trimmed.starts_with("music.youtube.com/")
        || trimmed.starts_with("youtube.com/")
        || trimmed.starts_with("www.youtube.com/")
        || trimmed.starts_with("youtu.be/")
    {
        format!("https://{trimmed}")
    } else {
        trimmed.to_string()
    };

    let parsed = Url::parse(&normalized).ok()?;
    let host = parsed.host_str()?.trim_start_matches("www.");

    match host {
        "youtu.be" => parsed
            .path_segments()
            .and_then(|mut segments| segments.next())
            .filter(|segment| !segment.is_empty())
            .map(str::to_string),
        "youtube.com" | "music.youtube.com" => {
            if parsed.path().trim_end_matches('/') == "/watch" {
                return parsed
                    .query_pairs()
                    .find_map(|(key, value)| (key == "v").then_some(value.into_owned()));
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "../tests/network_ytm_api_network.rs"]
mod tests;
