use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::models::{HomeSection, Track};

use super::cipher::Cipher;
use super::parsers;

const YTM_BASE_URL: &str = "https://music.youtube.com/youtubei/v1";
const YTM_PARAMS: &str = "prettyPrint=false";
const YTM_ORIGIN: &str = "https://music.youtube.com";

pub struct YtMusicClient {
    http: reqwest::Client,
    #[allow(dead_code)]
    cookie: String,
    rt: tokio::runtime::Runtime,
    sapisid: Option<String>,
    visitor_data: std::sync::Mutex<Option<String>>,
    #[allow(dead_code)]
    cipher: std::sync::Mutex<Option<Cipher>>,
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

    #[allow(dead_code)]
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

    pub fn get_audio_url(&self, video_id: &str) -> Result<String> {
        self.rt.block_on(async {
            // Try Android VR client first — returns direct URLs (no cipher needed)
            eprintln!("[PLAYER] trying Android VR client for {video_id}...");
            let vr_err = match self.try_android_vr_audio(video_id).await {
                Ok(url) => {
                    eprintln!("[PLAYER] got direct URL via Android VR");
                    self.spawn_playback_report(video_id);
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

            // Try without cipher first — some streams have direct URLs
            let json = self.player_request(&body).await?;

            if let Ok(audio) = Self::best_audio_format(&json) {
                if let Some(url) = audio.get("url").and_then(|u| u.as_str()) {
                    eprintln!("[PLAYER] got direct URL via web client");
                    Self::fire_tracking_urls(&self.http, self.auth_header(), &json);
                    return Ok(url.to_string());
                }
            }

            // If we get here, neither path gave a direct URL
            bail!("Could not get audio URL: {vr_err}")
        })
    }

    /// Report playback to YouTube for history tracking.
    /// Makes an authenticated WEB_REMIX player request to get tracking URLs,
    /// then fires both playback + watchtime URLs so the play counts in history.
    fn spawn_playback_report(&self, video_id: &str) {
        let http = self.http.clone();
        let auth = self.auth_header();
        let video_id = video_id.to_string();

        tokio::spawn(async move {
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

            let Ok(resp) = req.json(&body).send().await else { return };
            let Ok(json): Result<Value, _> = resp.json().await else { return };

            Self::fire_tracking_urls(&http, auth, &json);
        });
    }

    /// Fire playback + watchtime URLs so the play registers in history.
    /// Rewrites tracking URLs to use music.youtube.com (matching the real web player)
    /// and adds required client metadata.
    fn fire_tracking_urls(http: &reqwest::Client, auth: Option<String>, player_json: &Value) {
        let tracking = &player_json["playbackTracking"];

        let rewrite_url = |key: &str| -> Option<String> {
            tracking
                .get(key)
                .and_then(|u| u["baseUrl"].as_str())
                .map(|s| s.replace("https://s.youtube.com/", "https://music.youtube.com/"))
        };

        let playback_url = rewrite_url("videostatsPlaybackUrl");
        let watchtime_url = rewrite_url("videostatsWatchtimeUrl");

        if playback_url.is_none() && watchtime_url.is_none() {
            eprintln!("[TRACKING] no tracking URLs in player response");
            return;
        }

        let length = player_json
            .pointer("/videoDetails/lengthSeconds")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        let cpn = generate_cpn();
        let http = http.clone();
        let referer = "https://music.youtube.com/".to_string();

        // Client metadata matching real WEB_REMIX player
        let client_params = format!(
            "&cbr=Static&cbrver=0.1.0&c=WEB_REMIX&cver=1.20260304.03.00\
             &cplayer=UNIPLAYER&cos=X11&cplatform=DESKTOP"
        );

        tokio::spawn(async move {
            // 1. Fire playback URL
            if let Some(url) = &playback_url {
                let full = format!(
                    "{url}&cpn={cpn}&ver=2&cmt=0&fmt=251&afmt=251\
                     &muted=0&volume=100&vis=1{client_params}"
                );

                let mut req = http.get(&full).header("referer", &referer);
                if let Some(ref auth) = auth {
                    req = req.header("authorization", auth.clone());
                }
                match req.send().await {
                    Ok(r) => eprintln!("[TRACKING] playback reported ({})", r.status()),
                    Err(e) => eprintln!("[TRACKING] playback failed: {e}"),
                }
            }

            // Small delay like real player
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            // 2. Fire watchtime URL
            if let Some(url) = &watchtime_url {
                let watched = length.min(30);
                let full = format!(
                    "{url}&cpn={cpn}&ver=2&st=0&et={watched}&cmt={watched}\
                     &fmt=251&afmt=251&muted=0&volume=100&vis=1\
                     &rt={watched}&state=playing&rtn={}\
                     &referrer=https%3A%2F%2Fmusic.youtube.com%2F{client_params}",
                    watched + 5
                );

                let mut req = http.get(&full).header("referer", &referer);
                if let Some(ref auth) = auth {
                    req = req.header("authorization", auth.clone());
                }
                match req.send().await {
                    Ok(r) => eprintln!("[TRACKING] watchtime reported ({})", r.status()),
                    Err(e) => eprintln!("[TRACKING] watchtime failed: {e}"),
                }
            }
        });
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
        let visitor_data = self.ensure_visitor_data().await?;

        let body = json!({
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
                    "visitorData": visitor_data,
                }
            },
            "videoId": video_id,
        });

        let vr_ua = "com.google.android.apps.youtube.vr.oculus/1.71.26 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip";
        let vr_client = reqwest::Client::new();
        let resp = vr_client
            .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
            .header("content-type", "application/json")
            .header("user-agent", vr_ua)
            .header("origin", "https://www.youtube.com")
            .header("x-youtube-client-name", "28")
            .header("x-youtube-client-version", "1.71.26")
            .header("x-goog-visitor-id", &visitor_data)
            .json(&body)
            .send()
            .await
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
