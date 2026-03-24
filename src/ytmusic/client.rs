use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
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

use super::parsers;

const STATIC_APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const YTM_BASE_URL: &str = "https://music.youtube.com/youtubei/v1";
const YTM_PARAMS: &str = "prettyPrint=false";
const YTM_ORIGIN: &str = "https://music.youtube.com";
const YTM_MOBILE_CLIENT_NAME: &str = "ANDROID_MUSIC";
const YTM_MOBILE_CLIENT_VERSION: &str = "7.21.50";

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
    watchtime_url: String,
    cpn: String,
    referer_url: String,
    visitor_id: Option<String>,
    audio_fmt: String,
    video_fmt: String,
    length_millis: u64,
    flush_points_millis: Vec<u64>,
    default_flush_interval_millis: u64,
    last_reported_millis: AtomicU64,
    last_activity_millis: AtomicU64,
    session_started_at: Instant,
    rt: *const tokio::runtime::Runtime,
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

// TrackingInfo contains a raw pointer to the runtime which lives in YtMusicClient
// (always alive while AppState exists). We only use it from background threads via
// report_watchtime which is called from gio::spawn_blocking.
unsafe impl Send for TrackingInfo {}
unsafe impl Sync for TrackingInfo {}

impl TrackingInfo {
    fn startup_bootstrap_target(&self) -> u64 {
        self.length_millis.min(30_000)
    }

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

    fn build_watchtime_request(
        &self,
        start_millis: u64,
        listened_millis: u64,
        state: &str,
        final_ping: bool,
    ) -> Option<PendingWatchtimeRequest> {
        let watched = listened_millis.min(self.length_millis);
        if watched == 0 || watched <= start_millis {
            return None;
        }

        let (mut url, mut params) = Self::build_query_map(&self.watchtime_url)?;
        let now_unix_millis = unix_epoch_millis();
        let wall_elapsed_millis = self.session_started_at.elapsed().as_millis() as u64;
        let previous_activity = self
            .last_activity_millis
            .swap(wall_elapsed_millis, Ordering::Relaxed);
        let lact_millis = wall_elapsed_millis.saturating_sub(previous_activity);

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
            .unwrap_or_else(|| "10".to_string());
        Self::set_query_param(&mut params, "vis", vis);
        Self::set_query_param(&mut params, "state", state);
        Self::set_query_param(&mut params, "rt", format_millis(wall_elapsed_millis));
        Self::set_query_param(&mut params, "rti", (wall_elapsed_millis / 1000).to_string());
        Self::set_query_param(&mut params, "lact", lact_millis.to_string());

        if state == "playing" && !final_ping {
            Self::set_query_param(
                &mut params,
                "rtn",
                ((wall_elapsed_millis / 1000) + 10).to_string(),
            );
            params.remove("final");
            params.remove("pe");
        } else if final_ping {
            Self::set_query_param(&mut params, "final", "1");
            Self::set_query_param(&mut params, "pe", "1");
            params.remove("rtn");
        } else {
            params.remove("rtn");
            params.remove("final");
            params.remove("pe");
        }

        if !params.contains_key("referrer") {
            Self::set_query_param(&mut params, "referrer", self.referer_url.clone());
        }
        if !params.contains_key("len") {
            Self::set_query_param(&mut params, "len", format_millis(self.length_millis));
        }
        if !params.contains_key("c") {
            Self::set_query_param(&mut params, "c", "WEB_REMIX");
        }
        if !params.contains_key("cver") {
            Self::set_query_param(&mut params, "cver", "1.20260304.03.00");
        }
        if !params.contains_key("cplayer") {
            Self::set_query_param(&mut params, "cplayer", "UNIPLAYER");
        }
        if !params.contains_key("cos") {
            Self::set_query_param(&mut params, "cos", "X11");
        }
        if !params.contains_key("cplatform") {
            Self::set_query_param(&mut params, "cplatform", "DESKTOP");
        }
        if !params.contains_key("cbr") {
            Self::set_query_param(&mut params, "cbr", "Static");
        }
        if !params.contains_key("cbrver") {
            Self::set_query_param(&mut params, "cbrver", STATIC_APP_VERSION);
        }

        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in params {
                pairs.append_pair(&key, &value);
            }
        }

        Some(PendingWatchtimeRequest {
            url: url.into(),
            referer: self.referer_url.clone(),
            event_time: now_unix_millis.to_string(),
            request_time: now_unix_millis.to_string(),
            start_millis,
            watched_millis: watched,
            state: state.to_string(),
            final_ping,
        })
    }

    fn dispatch_watchtime_request(&self, request: PendingWatchtimeRequest) {
        let rt = unsafe { &*self.rt };
        let http = self.http.clone();
        let auth = self.auth.clone();
        let visitor_id = self.visitor_id.clone();
        rt.spawn(async move {
            let mut req = http
                .get(&request.url)
                .header("referer", request.referer)
                .header("x-goog-authuser", "0")
                .header("x-goog-event-time", request.event_time)
                .header("x-goog-request-time", request.request_time);
            if let Some(auth) = &auth {
                req = req.header("authorization", auth.clone());
            }
            if let Some(visitor_id) = &visitor_id {
                req = req.header("x-goog-visitor-id", visitor_id);
            }
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
        let rt = unsafe { &*self.rt };
        let http = self.http.clone();
        let auth = self.auth.clone();
        let visitor_id = self.visitor_id.clone();
        rt.block_on(async move {
            let mut req = http
                .get(&request.url)
                .header("referer", request.referer)
                .header("x-goog-authuser", "0")
                .header("x-goog-event-time", request.event_time)
                .header("x-goog-request-time", request.request_time);
            if let Some(auth) = &auth {
                req = req.header("authorization", auth.clone());
            }
            if let Some(visitor_id) = &visitor_id {
                req = req.header("x-goog-visitor-id", visitor_id);
            }
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

    pub fn maybe_bootstrap_watchtime(&self) {
        let target = self.startup_bootstrap_target();
        if target == 0 {
            return;
        }

        if self
            .last_reported_millis
            .compare_exchange(0, target, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let client_params = format!(
            "&cbr=Static&cbrver={STATIC_APP_VERSION}&c=WEB_REMIX&cver=1.20260304.03.00\
             &cplayer=UNIPLAYER&cos=X11&cplatform=DESKTOP"
        );
        let full = format!(
            "{}&cpn={}&ver=2&st={}&et={}&cmt={}\
             &fmt=251&afmt=251&muted=0&volume=100&vis=1\
             &rt={}&state=playing&rtn={}\
             &referrer=https%3A%2F%2Fmusic.youtube.com%2F{}",
            self.watchtime_url,
            self.cpn,
            format_millis(0),
            format_millis(target),
            format_millis(target),
            format_millis(target),
            (target / 1000) + 5,
            client_params,
        );

        let rt = unsafe { &*self.rt };
        let http = self.http.clone();
        let auth = self.auth.clone();
        rt.spawn(async move {
            let mut req = http
                .get(&full)
                .header("referer", "https://music.youtube.com/");
            if let Some(auth) = &auth {
                req = req.header("authorization", auth.clone());
            }
            match req.send().await {
                Ok(r) => eprintln!(
                    "[TRACKING] startup watchtime reported: 0..{} ms ({})",
                    target,
                    r.status()
                ),
                Err(e) => eprintln!("[TRACKING] startup watchtime failed: {e}"),
            }
        });
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

        let final_start = watched.saturating_sub(7);
        let final_end = watched.max(final_start + 1);
        let final_request = self.build_watchtime_request(final_start, final_end, "paused", true);

        let rt = unsafe { &*self.rt };
        let http = self.http.clone();
        let auth = self.auth.clone();
        let visitor_id = self.visitor_id.clone();
        rt.spawn(async move {
            for request in [paused_request, final_request].into_iter().flatten() {
                let mut req = http
                    .get(&request.url)
                    .header("referer", request.referer)
                    .header("x-goog-authuser", "0")
                    .header("x-goog-event-time", request.event_time)
                    .header("x-goog-request-time", request.request_time);
                if let Some(auth) = &auth {
                    req = req.header("authorization", auth.clone());
                }
                if let Some(visitor_id) = &visitor_id {
                    req = req.header("x-goog-visitor-id", visitor_id);
                }
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

        let final_start = watched.saturating_sub(7);
        let final_end = watched.max(final_start + 1);
        let final_request = self.build_watchtime_request(final_start, final_end, "paused", true);

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
            (Some(end_millis), Some(next_start)) => end_millis.max(next_start),
            (Some(end_millis), None) => end_millis,
            (None, Some(next_start)) => next_start,
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
    rt: tokio::runtime::Runtime,
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
    format!("{:x}", hash)[..16].to_string()
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
        headers.insert(
            "user-agent",
            HeaderValue::from_static(
                "Mozilla/5.0 (X11; Linux x86_64; rv:136.0) Gecko/20100101 Firefox/136.0",
            ),
        );
        headers.insert("accept", HeaderValue::from_static("*/*"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("origin", HeaderValue::from_static(YTM_ORIGIN));
        headers.insert(
            "referer",
            HeaderValue::from_static("https://music.youtube.com/"),
        );
        headers.insert("x-youtube-client-name", HeaderValue::from_static("67"));
        headers.insert(
            "x-youtube-client-version",
            HeaderValue::from_static("1.20240101.01.00"),
        );
        headers.insert("x-origin", HeaderValue::from_static(YTM_ORIGIN));

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("Failed to build HTTP client")?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to create tokio runtime")?;

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

    fn client_context() -> Value {
        json!({
            "context": {
                "client": {
                    "clientName": "WEB_REMIX",
                    "clientVersion": "1.20240101.01.00",
                    "hl": "en",
                    "gl": "US",
                }
            }
        })
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
    pub fn start_playback_tracking(&self, video_id: &str) -> Option<TrackingInfo> {
        self.rt
            .block_on(self.start_playback_tracking_async(video_id))
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

    /// Report playback start to YouTube and return a TrackingInfo for deferred watchtime reporting.
    /// Fires the playback URL immediately; the caller fires watchtime later via TrackingInfo.
    /// NOTE: Must be called from an async context running on self.rt (e.g. inside block_on).
    async fn start_playback_tracking_async(&self, video_id: &str) -> Option<TrackingInfo> {
        let http = self.http.clone();
        let auth = self.auth_header();
        let video_id = video_id.to_string();
        let rt_ptr: *const tokio::runtime::Runtime = &self.rt;

        {
            if auth.is_none() {
                eprintln!(
                    "[TRACKING] no SAPISID-derived auth cookie is available for {video_id}; player bootstrap may omit playbackTracking"
                );
            }

            let mut body = json!({
                "context": {
                    "client": {
                        "clientName": "WEB_REMIX",
                        "clientVersion": "1.20240101.01.00",
                        "hl": "en",
                        "gl": "US",
                    }
                }
            });
            body["videoId"] = json!(&video_id);

            let Ok(resp) = self
                .post(&format!("{YTM_BASE_URL}/player?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
            else {
                eprintln!("[TRACKING] player bootstrap request failed for {video_id}");
                return None;
            };
            let Ok(json): Result<Value, _> = resp.json().await else {
                eprintln!("[TRACKING] player bootstrap response was not valid JSON for {video_id}");
                return None;
            };

            let tracking = &json["playbackTracking"];
            let rewrite_url = |key: &str| -> Option<String> {
                tracking
                    .get(key)
                    .and_then(|u| u["baseUrl"].as_str())
                    .map(rewrite_tracking_base_url)
            };

            let playback_url = rewrite_url("videostatsPlaybackUrl");
            let watchtime_url = rewrite_url("videostatsWatchtimeUrl");
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

            let audio_fmt = Self::best_audio_format(&json)
                .ok()
                .and_then(|format| format.get("itag").and_then(|v| v.as_u64()))
                .unwrap_or(141)
                .to_string();
            let video_fmt = "0".to_string();
            let visitor_id = json
                .pointer("/responseContext/visitorData")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let referer_url = playback_url
                .as_deref()
                .and_then(|url| TrackingInfo::build_query_map(url))
                .and_then(|(_, params)| params.get("referrer").cloned())
                .unwrap_or_else(|| format!("{YTM_ORIGIN}/watch?v={video_id}"));
            let length_millis = watchtime_url
                .as_deref()
                .and_then(|url| TrackingInfo::build_query_map(url))
                .and_then(|(_, params)| parse_secs_to_millis(params.get("len").map(|s| s.as_str())))
                .or_else(|| {
                    parse_secs_to_millis(
                        json.pointer("/videoDetails/lengthSeconds")
                            .and_then(|v| v.as_str()),
                    )
                })
                .unwrap_or(300_000);

            let cpn = generate_cpn();

            // Fire playback URL immediately (start event)
            if let Some(url) = &playback_url {
                let client_params = format!(
                    "&cbr=Static&cbrver={STATIC_APP_VERSION}&c=WEB_REMIX&cver=1.20260304.03.00\
                     &cplayer=UNIPLAYER&cos=X11&cplatform=DESKTOP"
                );
                let full = format!(
                    "{url}&cpn={cpn}&ver=2&cmt=0&fmt=251&afmt=251\
                     &muted=0&volume=100&vis=1{}",
                    client_params
                );
                let mut req = http
                    .get(&full)
                    .header("referer", "https://music.youtube.com/");
                if let Some(ref auth) = auth {
                    req = req.header("authorization", auth.clone());
                }
                match req.send().await {
                    Ok(r) => eprintln!("[TRACKING] playback reported ({})", r.status()),
                    Err(e) => eprintln!("[TRACKING] playback failed: {e}"),
                }
            }

            // Return watchtime info for deferred reporting
            watchtime_url.map(|wt_url| TrackingInfo {
                http: http.clone(),
                auth: auth.clone(),
                watchtime_url: wt_url,
                cpn,
                referer_url,
                visitor_id,
                audio_fmt,
                video_fmt,
                length_millis,
                flush_points_millis,
                default_flush_interval_millis,
                last_reported_millis: AtomicU64::new(0),
                last_activity_millis: AtomicU64::new(0),
                session_started_at: Instant::now(),
                rt: rt_ptr,
            })
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
            .header(
                "user-agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:136.0) Gecko/20100101 Firefox/136.0",
            )
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
        let json: Value = response
            .json()
            .await
            .with_context(|| format!("Failed to parse timed lyrics browse response for {browse_id}"))?;
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

#[cfg(test)]
mod tests {
    use super::{
        CaptionTrack, YtMusicClient, lyrics_browse_id_from_next_response,
        prepare_cookie_for_sapisid_auth,
    };
    use crate::models::SyncedLyricsSource;
    use serde_json::json;

    #[test]
    fn sorted_audio_formats_prefer_direct_urls_over_non_url_entries() {
        let json = json!({
            "streamingData": {
                "adaptiveFormats": [
                    {
                        "mimeType": "audio/webm",
                        "bitrate": 256000,
                        "contentLength": "12345"
                    },
                    {
                        "mimeType": "audio/webm",
                        "bitrate": 128000,
                        "url": "https://example.com/direct"
                    }
                ]
            }
        });

        let formats = YtMusicClient::sorted_audio_formats(&json).expect("audio formats");
        assert_eq!(
            YtMusicClient::format_direct_url(formats[0]),
            Some("https://example.com/direct")
        );
    }

    #[test]
    fn parse_synced_lyrics_json3_normalizes_lines_and_end_times() {
        let track = CaptionTrack {
            base_url: "https://example.com/api/timedtext".to_string(),
            language_code: Some("en".to_string()),
            kind: None,
            name: Some("English".to_string()),
        };
        let json = json!({
            "events": [
                {
                    "tStartMs": "1000",
                    "dDurationMs": "1500",
                    "segs": [
                        { "utf8": "Hello   " },
                        { "utf8": "world" }
                    ]
                },
                {
                    "tStartMs": "3500",
                    "segs": [
                        { "utf8": "Next &amp; final" }
                    ]
                }
            ]
        });

        let lyrics = YtMusicClient::parse_synced_lyrics(track, &json).expect("synced lyrics");
        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(lyrics.lines[0].text, "Hello world");
        assert_eq!(lyrics.lines[0].start_millis, 1000);
        assert_eq!(lyrics.lines[0].end_millis, Some(3500));
        assert_eq!(lyrics.lines[1].text, "Next & final");
        assert_eq!(lyrics.lines[1].end_millis, Some(7500));
    }

    #[test]
    fn lyrics_browse_id_is_extracted_from_watch_next_tabs() {
        let json = json!({
            "contents": {
                "singleColumnMusicWatchNextResultsRenderer": {
                    "tabbedRenderer": {
                        "watchNextTabbedResultsRenderer": {
                            "tabs": [
                                { "tabRenderer": {} },
                                {
                                    "tabRenderer": {
                                        "endpoint": {
                                            "browseEndpoint": {
                                                "browseId": "MPLYt_test_browse"
                                            }
                                        }
                                    }
                                }
                            ]
                        }
                    }
                }
            }
        });

        assert_eq!(
            lyrics_browse_id_from_next_response(&json).as_deref(),
            Some("MPLYt_test_browse")
        );
    }

    #[test]
    fn parse_timed_lyrics_browse_extracts_mobile_lyrics_model() {
        let json = json!({
            "contents": {
                "elementRenderer": {
                    "newElement": {
                        "type": {
                            "componentType": {
                                "model": {
                                    "timedLyricsModel": {
                                        "lyricsData": {
                                            "sourceMessage": { "simpleText": "Source: LyricFind" },
                                            "timedLyricsData": [
                                                {
                                                    "lyricLine": "I was a liar",
                                                    "cueRange": {
                                                        "startTimeMilliseconds": "9200",
                                                        "endTimeMilliseconds": "10630",
                                                        "metadata": { "id": "1" }
                                                    }
                                                },
                                                {
                                                    "lyricLine": "I gave in to the fire",
                                                    "cueRange": {
                                                        "startTimeMilliseconds": "10680",
                                                        "endTimeMilliseconds": "12540",
                                                        "metadata": { "id": "2" }
                                                    }
                                                }
                                            ]
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        let lyrics = YtMusicClient::parse_timed_lyrics_browse(&json).expect("timed lyrics");
        assert_eq!(lyrics.source, SyncedLyricsSource::TimedLyrics);
        assert_eq!(lyrics.track_name.as_deref(), Some("Source: LyricFind"));
        assert_eq!(lyrics.lines.len(), 2);
        assert_eq!(lyrics.lines[0].text, "I was a liar");
        assert_eq!(lyrics.lines[0].start_millis, 9200);
        assert_eq!(lyrics.lines[0].end_millis, Some(10680));
        assert_eq!(lyrics.lines[1].end_millis, Some(12540));
    }

    #[test]
    fn prepare_cookie_for_sapisid_auth_keeps_plain_sapisid() {
        let cookie = "SID=sid; SAPISID=plain-sapisid; __Secure-3PAPISID=secure-3p";
        let (normalized, sapisid, source) = prepare_cookie_for_sapisid_auth(cookie);

        assert_eq!(normalized, cookie);
        assert_eq!(sapisid.as_deref(), Some("plain-sapisid"));
        assert_eq!(source, Some("SAPISID"));
    }

    #[test]
    fn prepare_cookie_for_sapisid_auth_uses_secure_3papisid_fallback() {
        let cookie = "SID=sid; __Secure-3PAPISID=secure-3p";
        let (normalized, sapisid, source) = prepare_cookie_for_sapisid_auth(cookie);

        assert_eq!(
            normalized,
            "SID=sid; __Secure-3PAPISID=secure-3p; SAPISID=secure-3p"
        );
        assert_eq!(sapisid.as_deref(), Some("secure-3p"));
        assert_eq!(source, Some("__Secure-3PAPISID"));
    }

    #[test]
    fn prepare_cookie_for_sapisid_auth_uses_secure_1papisid_when_needed() {
        let cookie = "SID=sid; __Secure-1PAPISID=secure-1p";
        let (normalized, sapisid, source) = prepare_cookie_for_sapisid_auth(cookie);

        assert_eq!(
            normalized,
            "SID=sid; __Secure-1PAPISID=secure-1p; SAPISID=secure-1p"
        );
        assert_eq!(sapisid.as_deref(), Some("secure-1p"));
        assert_eq!(source, Some("__Secure-1PAPISID"));
    }

    #[test]
    fn prepare_cookie_for_sapisid_auth_does_not_promote_apisid() {
        let cookie = "SID=sid; APISID=apisid-only";
        let (normalized, sapisid, source) = prepare_cookie_for_sapisid_auth(cookie);

        assert_eq!(normalized, cookie);
        assert_eq!(sapisid, None);
        assert_eq!(source, None);
    }
}
