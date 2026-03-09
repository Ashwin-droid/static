use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::models::{HomeSection, PlaylistSummary, RadioQueue, Track};

use super::cipher::Cipher;
use super::parsers;

const YTM_BASE_URL: &str = "https://music.youtube.com/youtubei/v1";
const YTM_PARAMS: &str = "prettyPrint=false";
const YTM_ORIGIN: &str = "https://music.youtube.com";

// OAuth device flow for Android VR auth (premium content)
const VR_OAUTH_CLIENT_ID: &str = "652469312169-4lvs9bnhr9lpns9v451j5oivd81vjvu1.apps.googleusercontent.com";
const VR_OAUTH_CLIENT_SECRET: &str = "3fTWrBJI5Uojm1TK7_iJCW5Z";
const VR_OAUTH_TOKEN_URL: &str = "https://www.youtube.com/o/oauth2/token?prettyPrint=false";
const VR_OAUTH_DEVICE_CODE_URL: &str = "https://www.youtube.com/o/oauth2/device/code?prettyPrint=false";

/// Holds the watchtime tracking URL so we can report real listen time on navigate-away.
pub struct TrackingInfo {
    http: reqwest::Client,
    auth: Option<String>,
    watchtime_url: String,
    cpn: String,
    length_secs: u64,
    rt: *const tokio::runtime::Runtime,
}

// TrackingInfo contains a raw pointer to the runtime which lives in YtMusicClient
// (always alive while AppState exists). We only use it from background threads via
// report_watchtime which is called from gio::spawn_blocking.
unsafe impl Send for TrackingInfo {}
unsafe impl Sync for TrackingInfo {}

impl TrackingInfo {
    /// Fire the watchtime URL with real listened seconds. Call this on song change/stop.
    pub fn report_watchtime(&self, listened_secs: u64) {
        let watched = listened_secs.min(self.length_secs);
        if watched == 0 {
            return;
        }
        let referer = "https://music.youtube.com/";
        let client_params = "&cbr=Static&cbrver=0.1.0&c=WEB_REMIX&cver=1.20260304.03.00\
             &cplayer=UNIPLAYER&cos=X11&cplatform=DESKTOP";
        let full = format!(
            "{}&cpn={}&ver=2&st=0&et={watched}&cmt={watched}\
             &fmt=251&afmt=251&muted=0&volume=100&vis=1\
             &rt={watched}&state=playing&rtn={}\
             &referrer=https%3A%2F%2Fmusic.youtube.com%2F{client_params}",
            self.watchtime_url, self.cpn, watched + 5
        );

        // Safety: the runtime outlives all TrackingInfo instances (lives in YtMusicClient in AppState)
        let rt = unsafe { &*self.rt };
        let http = self.http.clone();
        let auth = self.auth.clone();
        rt.spawn(async move {
            let mut req = http.get(&full).header("referer", referer);
            if let Some(auth) = &auth {
                req = req.header("authorization", auth.clone());
            }
            match req.send().await {
                Ok(r) => eprintln!("[TRACKING] watchtime reported: {}s ({})", watched, r.status()),
                Err(e) => eprintln!("[TRACKING] watchtime failed: {e}"),
            }
        });
    }
}

pub struct YtMusicClient {
    http: reqwest::Client,
    #[allow(dead_code)]
    cookie: String,
    rt: tokio::runtime::Runtime,
    sapisid: Option<String>,
    visitor_data: std::sync::Mutex<Option<String>>,
    #[allow(dead_code)]
    cipher: std::sync::Mutex<Option<Cipher>>,
    /// OAuth refresh token for Android VR (premium content)
    vr_refresh_token: std::sync::Mutex<Option<String>>,
    /// Cached VR access token + expiry (epoch secs)
    vr_access_token: std::sync::Mutex<Option<(String, u64)>>,
}

unsafe impl Send for YtMusicClient {}
unsafe impl Sync for YtMusicClient {}

fn extract_sapisid(cookie: &str) -> Option<String> {
    cookie
        .split(';')
        .map(|s| s.trim())
        .find(|s| s.starts_with("SAPISID="))
        .map(|s| s.trim_start_matches("SAPISID=").to_string())
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

impl YtMusicClient {
    pub fn new(cookie: &str) -> Result<Self> {
        let sapisid = extract_sapisid(cookie);

        let mut headers = HeaderMap::new();
        headers.insert("cookie", HeaderValue::from_str(cookie)?);
        headers.insert("user-agent", HeaderValue::from_static(
            "Mozilla/5.0 (X11; Linux x86_64; rv:136.0) Gecko/20100101 Firefox/136.0"
        ));
        headers.insert("accept", HeaderValue::from_static("*/*"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("origin", HeaderValue::from_static(YTM_ORIGIN));
        headers.insert("referer", HeaderValue::from_static("https://music.youtube.com/"));
        headers.insert("x-youtube-client-name", HeaderValue::from_static("67"));
        headers.insert("x-youtube-client-version", HeaderValue::from_static("1.20240101.01.00"));
        headers.insert("x-origin", HeaderValue::from_static(YTM_ORIGIN));

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("Failed to build HTTP client")?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to create tokio runtime")?;

        Ok(Self {
            http,
            cookie: cookie.to_string(),
            rt,
            sapisid,
            visitor_data: std::sync::Mutex::new(None),
            cipher: std::sync::Mutex::new(None),
            vr_refresh_token: std::sync::Mutex::new(None),
            vr_access_token: std::sync::Mutex::new(None),
        })
    }

    fn auth_header(&self) -> Option<String> {
        self.sapisid.as_ref().map(|id| generate_sapisidhash(id, YTM_ORIGIN))
    }

    #[allow(dead_code)]
    fn auth_header_for_origin(&self, origin: &str) -> Option<String> {
        self.sapisid.as_ref().map(|id| generate_sapisidhash(id, origin))
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

    /// Get or fetch the cipher (cached per session).
    #[allow(dead_code)]
    async fn ensure_cipher(&self) -> Result<Cipher> {
        {
            let guard = self.cipher.lock().unwrap();
            if let Some(c) = guard.as_ref() {
                return Ok(c.clone());
            }
        }

        eprintln!("[CIPHER] fetching and extracting cipher...");
        let c = Cipher::from_player(&self.http).await?;

        let mut guard = self.cipher.lock().unwrap();
        *guard = Some(c.clone());
        Ok(c)
    }

    pub fn get_home(&self) -> Result<Vec<HomeSection>> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["browseId"] = json!("FEmusic_home");

            let resp = self.post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch home page")?;

            let json: Value = resp.json().await.context("Failed to parse home response")?;
            parsers::parse_home(&json)
        })
    }

    pub fn get_history(&self) -> Result<Vec<Track>> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["browseId"] = json!("FEmusic_history");

            let resp = self.post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch history")?;

            let json: Value = resp.json().await.context("Failed to parse history response")?;
            let sections = parsers::parse_home(&json)?;
            Ok(sections.into_iter().flat_map(|s| s.contents).collect())
        })
    }

    pub fn search(&self, query: &str) -> Result<Vec<Track>> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["query"] = json!(query);
            body["params"] = json!("EgWKAQIIAWoMEA4QChADEAQQCRAF");

            let resp = self.post(&format!("{YTM_BASE_URL}/search?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to search")?;

            let json: Value = resp.json().await.context("Failed to parse search response")?;
            parsers::parse_search_results(&json)
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

            let resp = self.post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch playlist")?;

            let json: Value = resp.json().await.context("Failed to parse playlist response")?;
            parsers::parse_playlist_tracks(&json)
        })
    }

    pub fn get_library_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        self.rt.block_on(async {
            let mut body = Self::client_context();
            body["browseId"] = json!("FEmusic_liked_playlists");

            let resp = self.post(&format!("{YTM_BASE_URL}/browse?{YTM_PARAMS}"))
                .json(&body)
                .send()
                .await
                .context("Failed to fetch library playlists")?;

            let json: Value = resp.json().await.context("Failed to parse library playlists response")?;
            parsers::parse_library_playlists(&json)
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

            let json: Value = resp.json().await.context("Failed to parse radio response")?;
            parsers::parse_radio(&json)
        })
    }

    /// Report playback start to YouTube and return tracking info.
    /// Call this off the playback-critical path so local files can start instantly.
    pub fn start_playback_tracking(&self, video_id: &str) -> Option<TrackingInfo> {
        self.rt.block_on(self.start_playback_tracking_async(video_id))
    }

    /// Get just the streaming URL without reporting anything to YouTube.
    /// Use this for cache downloads and other non-playback fetches.
    pub fn get_audio_url_no_track(&self, video_id: &str) -> Result<String> {
        self.rt.block_on(async {
            // Try Android VR client first — returns direct URLs (no cipher needed)
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

            if let Ok(audio) = Self::best_audio_format(&json) {
                if let Some(url) = audio.get("url").and_then(|u| u.as_str()) {
                    eprintln!("[PLAYER] got direct URL via web client");
                    return Ok(url.to_string());
                }
            }

            // If we get here, neither path gave a direct URL
            bail!("Could not get audio URL: {vr_err}")
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

            let mut req = http.post(&format!("{YTM_BASE_URL}/player?{YTM_PARAMS}"));
            if let Some(ref auth) = auth {
                req = req.header("authorization", auth.clone());
            }

            let Ok(resp) = req.json(&body).send().await else { return None };
            let Ok(json): Result<Value, _> = resp.json().await else { return None };

            let tracking = &json["playbackTracking"];
            let rewrite_url = |key: &str| -> Option<String> {
                tracking
                    .get(key)
                    .and_then(|u| u["baseUrl"].as_str())
                    .map(|s| s.replace("https://s.youtube.com/", "https://music.youtube.com/"))
            };

            let playback_url = rewrite_url("videostatsPlaybackUrl");
            let watchtime_url = rewrite_url("videostatsWatchtimeUrl");

            let length = json
                .pointer("/videoDetails/lengthSeconds")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(300);

            let cpn = generate_cpn();

            let client_params = "&cbr=Static&cbrver=0.1.0&c=WEB_REMIX&cver=1.20260304.03.00\
                 &cplayer=UNIPLAYER&cos=X11&cplatform=DESKTOP";
            let referer = "https://music.youtube.com/";

            // Fire playback URL immediately (start event)
            if let Some(url) = &playback_url {
                let full = format!(
                    "{url}&cpn={cpn}&ver=2&cmt=0&fmt=251&afmt=251\
                     &muted=0&volume=100&vis=1{client_params}"
                );
                let mut req = http.get(&full).header("referer", referer);
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
                length_secs: length,
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
            .header("user-agent", "Mozilla/5.0 (X11; Linux x86_64; rv:136.0) Gecko/20100101 Firefox/136.0")
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
                let msg = e.to_string();
                // Only escalate to OAuth for premium-gated content
                let is_premium_gate = msg.contains("Music Premium")
                    || msg.contains("Premium members");
                let has_vr_auth = self.vr_refresh_token.lock().unwrap().is_some();

                if is_premium_gate && has_vr_auth {
                    eprintln!("[PLAYER] premium-gated, retrying with VR OAuth...");
                    let token = self.ensure_vr_access_token().await?;
                    return self.vr_player_request(video_id, Some(&token)).await;
                }
                return Err(e);
            }
        }
    }

    /// Send a single Android VR player request, optionally with OAuth Bearer auth.
    async fn vr_player_request(&self, video_id: &str, access_token: Option<&str>) -> Result<String> {
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

        let resp = req.json(&body).send().await
            .context("Failed to fetch Android VR player data")?;

        let json: Value = resp.json().await.context("Failed to parse Android VR response")?;

        if let Some(reason) = json.pointer("/playabilityStatus/reason").and_then(|r| r.as_str()) {
            bail!("Unplayable: {reason}");
        }

        let audio = Self::best_audio_format(&json)?;
        audio
            .get("url")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .context("Android VR stream has no direct URL")
    }

    async fn player_request(&self, body: &Value) -> Result<Value> {
        let resp = self.post(&format!("{YTM_BASE_URL}/player?{YTM_PARAMS}"))
            .json(body)
            .send()
            .await
            .context("Failed to fetch player data")?;
        resp.json().await.context("Failed to parse player response")
    }

    fn best_audio_format(json: &Value) -> Result<&Value> {
        let formats = json
            .pointer("/streamingData/adaptiveFormats")
            .and_then(|f| f.as_array())
            .context("No streaming data")?;

        formats
            .iter()
            .filter(|f| {
                f.get("mimeType")
                    .and_then(|m| m.as_str())
                    .is_some_and(|m| m.starts_with("audio/"))
            })
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
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
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

        let json: Value = resp.json().await.context("Failed to parse VR token response")?;

        let access_token = json.get("access_token")
            .and_then(|v| v.as_str())
            .context("No access_token in VR token response")?
            .to_string();
        let expires_in = json.get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
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
            let device_code = json.get("device_code").and_then(|v| v.as_str())
                .context("No device_code")?.to_string();
            let user_code = json.get("user_code").and_then(|v| v.as_str())
                .context("No user_code")?.to_string();
            let verification_url = json.get("verification_url").and_then(|v| v.as_str())
                .unwrap_or("https://www.google.com/device").to_string();

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

            let refresh_token = json.get("refresh_token").and_then(|v| v.as_str())
                .context("No refresh_token in response")?.to_string();

            // Also load the access token we got
            if let Some(access_token) = json.get("access_token").and_then(|v| v.as_str()) {
                let expires_in = json.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(3600);
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                *self.vr_access_token.lock().unwrap() = Some((access_token.to_string(), now + expires_in - 60));
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
