use super::super::session_network::get_cookie;
use super::{
    CaptionTrack, TrackingInfo, YTM_WEB_SIGNATURE_TIMESTAMP_FALLBACK, YtMusicClient, YtmWebProfile,
    finalize_synced_lyric_lines, generate_cpn, lyrics_browse_id_from_next_response,
    prepare_cookie_for_sapisid_auth, signature_timestamp_from_player_js, ytm_web_profile_from_html,
};
use crate::models::{SyncedLyricLine, SyncedLyricsSource};
use anyhow::Context;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::{Duration, Instant};
use uuid::Uuid;

const WEB_REMIX_HIGH_AUDIO_TEST_VIDEO_ID: &str = "qSU560anReg";
const WEB_REMIX_HIGH_AUDIO_MIN_BITRATE: u64 = 240_000;

fn test_tracking_info(
    watchtime_url: Option<&str>,
    atr_url: Option<&str>,
    video_fmt: &str,
    audio_fmt: &str,
    length_millis: u64,
    elapsed: Duration,
) -> TrackingInfo {
    TrackingInfo {
        http: reqwest::Client::new(),
        auth: None,
        watchtime_url: watchtime_url.map(str::to_string),
        atr_url: atr_url.map(str::to_string),
        cpn: "_Z3q2CnOA6jEC31l".to_string(),
        referer_url: "https://music.youtube.com/".to_string(),
        audio_fmt: audio_fmt.to_string(),
        video_fmt: video_fmt.to_string(),
        length_millis,
        flush_points_millis: vec![10_000, 20_000, 30_000],
        default_flush_interval_millis: 40_000,
        last_reported_millis: AtomicU64::new(0),
        last_activity_millis: AtomicU64::new(0),
        atr_reported: AtomicBool::new(false),
        session_started_at: Instant::now() - elapsed,
        web_profile: YtmWebProfile::fallback(),
        rt: Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime"),
        ),
    }
}

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
fn web_remix_audio_formats_prefer_high_ciphered_bitrate() {
    let json = json!({
        "streamingData": {
            "adaptiveFormats": [
                {
                    "itag": 251,
                    "mimeType": "audio/webm; codecs=\"opus\"",
                    "averageBitrate": 139000,
                    "bitrate": 160000,
                    "url": "https://example.com/low"
                },
                {
                    "itag": 774,
                    "mimeType": "audio/webm; codecs=\"opus\"",
                    "averageBitrate": 267000,
                    "signatureCipher": "s=abc&sp=sig&url=https%3A%2F%2Fexample.com%2Fhigh%3Fn%3Draw"
                },
                {
                    "itag": 141,
                    "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                    "averageBitrate": 257000,
                    "signatureCipher": "s=def&sp=sig&url=https%3A%2F%2Fexample.com%2Fm4a%3Fn%3Draw"
                }
            ]
        }
    });

    let format = YtMusicClient::best_web_remix_audio_format(&json).expect("best format");
    assert_eq!(format.get("itag").and_then(|itag| itag.as_u64()), Some(774));
}

#[test]
fn web_remix_stream_url_from_signature_cipher_parses_parts() {
    let format = json!({
        "mimeType": "audio/webm; codecs=\"opus\"",
        "signatureCipher": "s=encrypted_sig&sp=sig&url=https%3A%2F%2Frr.example.com%2Fvideoplayback%3Fn%3Draw_n"
    });

    let stream =
        YtMusicClient::web_remix_stream_url_from_format(&format).expect("stream url parts");
    assert!(stream.ciphered);
    assert_eq!(stream.sp, "sig");
    assert_eq!(stream.signature.as_deref(), Some("encrypted_sig"));
    assert_eq!(stream.url, "https://rr.example.com/videoplayback?n=raw_n");
}

#[test]
fn web_remix_stream_url_from_signature_cipher_defaults_signature_param() {
    let format = json!({
        "mimeType": "audio/webm; codecs=\"opus\"",
        "signatureCipher": "url=https%3A%2F%2Frr.example.com%2Fvideoplayback&s=encrypted_sig"
    });

    let stream =
        YtMusicClient::web_remix_stream_url_from_format(&format).expect("stream url parts");
    assert_eq!(stream.sp, "signature");
    assert_eq!(stream.signature.as_deref(), Some("encrypted_sig"));
}

#[test]
fn node_binary_candidates_include_nvm_versions_when_path_is_plain() {
    let root = std::env::temp_dir().join(format!("static-node-test-{}", Uuid::new_v4()));
    let nvm_node = root.join(".nvm/versions/node/v24.13.0").join("bin/node");
    std::fs::create_dir_all(nvm_node.parent().expect("node parent")).expect("create nvm node dir");
    std::fs::write(&nvm_node, b"#!/bin/sh\n").expect("write fake node");

    let candidates = super::node_binary_candidates(Some(&root), None);

    assert_eq!(candidates.first(), Some(&PathBuf::from("node")));
    assert!(
        candidates.iter().any(|candidate| candidate == &nvm_node),
        "expected candidates to include NVM node path, got {candidates:?}"
    );

    let _ = std::fs::remove_dir_all(root);
}

fn live_test_cookie() -> Option<String> {
    std::env::var("STATIC_YTM_TEST_COOKIE")
        .ok()
        .map(|cookie| cookie.trim().to_string())
        .filter(|cookie| !cookie.is_empty())
        .or_else(get_cookie)
}

fn live_test_client() -> anyhow::Result<YtMusicClient> {
    let cookie = live_test_cookie().context(
        "set STATIC_YTM_TEST_COOKIE or save a YouTube Music cookie in Static's keyring first",
    )?;
    YtMusicClient::new(&cookie)
}

#[test]
#[ignore = "requires YouTube Premium, authenticated YouTube Music cookies, and live network access"]
fn web_remix_live_hardcoded_ytm_id_has_high_quality_audio() -> anyhow::Result<()> {
    let client = live_test_client()?;

    client.rt.block_on(async {
        let mut profile = client.ytm_web_profile(false).await;
        let cpn = generate_cpn();
        let mut json = client
            .tracking_player_response(
                WEB_REMIX_HIGH_AUDIO_TEST_VIDEO_ID,
                &cpn,
                None,
                None,
                &profile,
            )
            .await?;

        if YtMusicClient::player_response_requests_reload(&json) {
            profile = client.ytm_web_profile(true).await;
            json = client
                .tracking_player_response(
                    WEB_REMIX_HIGH_AUDIO_TEST_VIDEO_ID,
                    &cpn,
                    None,
                    None,
                    &profile,
                )
                .await?;
        }

        let format = YtMusicClient::best_web_remix_audio_format(&json)?;
        let bitrate = YtMusicClient::format_effective_bitrate(format);
        let itag = format
            .get("itag")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let audio_quality = format
            .get("audioQuality")
            .and_then(|value| value.as_str())
            .unwrap_or("missing");

        assert!(
            bitrate >= WEB_REMIX_HIGH_AUDIO_MIN_BITRATE,
            "expected WEB_REMIX high audio for {WEB_REMIX_HIGH_AUDIO_TEST_VIDEO_ID}; got itag {itag}, audioQuality {audio_quality}, bitrate {bitrate}"
        );
        assert!(
            YtMusicClient::format_signature_cipher(format).is_some()
                || YtMusicClient::format_direct_url(format).is_some(),
            "selected high audio format has no playable URL material"
        );

        Ok::<(), anyhow::Error>(())
    })
}

#[test]
#[ignore = "requires YouTube Premium, authenticated YouTube Music cookies, Node.js, and live network access"]
fn web_remix_live_hardcoded_ytm_id_resolves_high_quality_url() -> anyhow::Result<()> {
    let client = live_test_client()?;
    let url = client.rt.block_on(async {
        client
            .try_web_remix_audio(WEB_REMIX_HIGH_AUDIO_TEST_VIDEO_ID)
            .await
    })?;
    let parsed = url::Url::parse(&url)?;
    let itag = parsed
        .query_pairs()
        .find_map(|(key, value)| (key == "itag").then(|| value.into_owned()))
        .unwrap_or_else(|| "missing".to_string());

    assert!(
        matches!(itag.as_str(), "774" | "141"),
        "expected a high WEB_REMIX audio itag for {WEB_REMIX_HIGH_AUDIO_TEST_VIDEO_ID}; got {itag}"
    );
    assert!(
        parsed
            .query_pairs()
            .any(|(key, _)| key == "sig" || key == "signature"),
        "resolved WEB_REMIX URL is missing a deciphered signature parameter"
    );

    Ok(())
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
fn finalize_synced_lyrics_clamps_overlapping_ranges_to_next_line_start() {
    let mut lines = vec![
        SyncedLyricLine {
            start_millis: 1_000,
            end_millis: Some(4_000),
            text: "First".to_string(),
        },
        SyncedLyricLine {
            start_millis: 3_500,
            end_millis: Some(5_000),
            text: "Second".to_string(),
        },
    ];

    finalize_synced_lyric_lines(&mut lines);

    assert_eq!(lines[0].end_millis, Some(3_500));
    assert_eq!(lines[1].end_millis, Some(5_000));
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

#[test]
fn final_watchtime_request_matches_current_ytm_shape() {
    let tracking = test_tracking_info(
        Some("https://music.youtube.com/api/stats/watchtime?ns=yt&docid=abc123"),
        None,
        "0",
        "141",
        188_000,
        Duration::from_secs(96),
    );

    let request = tracking
        .build_watchtime_request(94_518, 94_518, "paused", true)
        .expect("final watchtime request");
    let (_, params) = TrackingInfo::build_query_map(&request.url).expect("query params");

    assert_eq!(params.get("state").map(String::as_str), Some("paused"));
    assert_eq!(params.get("st").map(String::as_str), Some("94.518"));
    assert_eq!(params.get("et").map(String::as_str), Some("94.518"));
    assert_eq!(params.get("cmt").map(String::as_str), Some("94.518"));
    assert_eq!(params.get("final").map(String::as_str), Some("1"));
    assert_eq!(params.get("vis").map(String::as_str), Some("1"));
    assert!(!params.contains_key("pe"));
    assert!(!params.contains_key("rtn"));
}

#[test]
fn playback_request_uses_selected_formats_and_browser_identity() {
    let tracking = test_tracking_info(
        Some("https://music.youtube.com/api/stats/watchtime?ns=yt&docid=abc123"),
        Some("https://music.youtube.com/api/stats/atr?ns=yt&docid=abc123&ver=2"),
        "134",
        "140",
        179_839,
        Duration::from_millis(1_116),
    );

    let request = tracking
        .build_playback_request("https://music.youtube.com/api/stats/playback?ns=yt&docid=abc123")
        .expect("playback request");
    let (_, params) = TrackingInfo::build_query_map(&request.url).expect("query params");

    assert_eq!(params.get("fmt").map(String::as_str), Some("134"));
    assert_eq!(params.get("afmt").map(String::as_str), Some("140"));
    assert_eq!(params.get("vis").map(String::as_str), Some("10"));
    assert_eq!(params.get("mos").map(String::as_str), Some("0"));
    assert_eq!(params.get("cbr").map(String::as_str), Some("Edge Chromium"));
    assert_eq!(params.get("cbrver").map(String::as_str), Some("147.0.0.0"));
    assert_eq!(
        params.get("cver").map(String::as_str),
        Some("1.20260426.12.00")
    );
}

#[test]
fn atr_request_keeps_to_its_own_surface() {
    let tracking = test_tracking_info(
        None,
        Some("https://music.youtube.com/api/stats/atr?ns=yt&docid=abc123&ver=2"),
        "134",
        "140",
        179_839,
        Duration::from_secs(4),
    );

    let request = tracking.build_atr_request(2_764).expect("atr request");
    let (_, params) = TrackingInfo::build_query_map(&request.url).expect("query params");

    assert_eq!(params.get("cmt").map(String::as_str), Some("2.764"));
    assert_eq!(params.get("fmt").map(String::as_str), Some("134"));
    assert_eq!(params.get("afmt").map(String::as_str), Some("140"));
    assert_eq!(params.get("mos").map(String::as_str), Some("0"));
    assert!(!params.contains_key("referrer"));
    assert!(!params.contains_key("state"));
}

#[test]
fn generated_cpn_matches_youtube_style_charset() {
    let cpn = generate_cpn();
    assert_eq!(cpn.len(), 16);
    assert!(
        cpn.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    );
}

#[test]
fn ytm_web_profile_parses_page_bootstrap_config() {
    let html = r#"
        <script>
        ytcfg.set({"INNERTUBE_CONTEXT_CLIENT_NAME":67,
            "INNERTUBE_CONTEXT_CLIENT_VERSION":"1.20260426.12.00",
            "PAGE_CL":906031756,
            "PAGE_BUILD_LABEL":"youtube.music.web.client_20260426_12_RC00",
            "WEB_PLAYER_CONTEXT_CONFIGS": {
                "WEB_PLAYER_CONTEXT_CONFIG_ID_MUSIC_WATCH": {
                    "jsUrl": "/s/player/5afa7c36/player_es6.vflset/en_GB/base.js"
                }
            }});
        ytcfg.set({"VISITOR_DATA":"visitor-data",
            "DATASYNC_ID":"103628638237885667007||",
            "ID_TOKEN":"QUFFLUhqtoken",
            "INNERTUBE_CONTEXT_HL":"en-IN",
            "INNERTUBE_CONTEXT_GL":"IN"});
        </script>
    "#;

    let profile =
        ytm_web_profile_from_html(html, 123_456).expect("profile should parse from ytcfg");

    assert_eq!(profile.header_client_name, "67");
    assert_eq!(profile.client_name, "WEB_REMIX");
    assert_eq!(profile.client_version, "1.20260426.12.00");
    assert_eq!(
        profile.player_js_url.as_deref(),
        Some("/s/player/5afa7c36/player_es6.vflset/en_GB/base.js")
    );
    assert_eq!(
        profile.signature_timestamp,
        YTM_WEB_SIGNATURE_TIMESTAMP_FALLBACK
    );
    assert_eq!(profile.page_cl.as_deref(), Some("906031756"));
    assert_eq!(
        profile.page_label.as_deref(),
        Some("youtube.music.web.client_20260426_12_RC00")
    );
    assert_eq!(profile.visitor_data.as_deref(), Some("visitor-data"));
    assert_eq!(
        profile.datasync_id.as_deref(),
        Some("103628638237885667007||")
    );
    assert_eq!(profile.identity_token.as_deref(), Some("QUFFLUhqtoken"));
    assert_eq!(profile.hl, "en-IN");
    assert_eq!(profile.gl, "IN");
    assert_eq!(profile.fetched_at_unix_millis, 123_456);
}

#[test]
fn cached_tracking_profile_needs_page_identity() {
    let now = 1_000_000;
    let mut profile = YtmWebProfile::fallback();
    profile.fetched_at_unix_millis = now;

    assert!(profile.is_fresh(now));
    assert!(!profile.is_fresh_for_tracking(now));

    profile.identity_token = Some("QUFFLUhqtoken".to_string());
    assert!(profile.is_fresh_for_tracking(now));
}

#[test]
fn player_js_signature_timestamp_is_extracted() {
    let js = r#"x={splay:!1,lactMilliseconds:"0",signatureTimestamp:20572,vis:1};"#;

    assert_eq!(signature_timestamp_from_player_js(js), Some(20_572));
}

#[test]
fn reload_page_player_response_is_detected() {
    let json = json!({
        "playabilityStatus": {
            "status": "UNPLAYABLE",
            "reason": "Video unavailable",
            "errorScreen": {
                "playerErrorMessageRenderer": {
                    "reason": {
                        "runs": [
                            { "text": "The page needs to be reloaded." }
                        ]
                    },
                    "command": {
                        "signalAction": {
                            "signal": "RELOAD_PAGE"
                        }
                    }
                }
            }
        }
    });

    assert!(YtMusicClient::player_response_requests_reload(&json));
}

#[test]
fn tracking_player_body_carries_playlist_identity_and_cpn() {
    let profile = YtmWebProfile::fallback();
    let body = YtMusicClient::tracking_player_body(
        "Dx69Ut6Wyuk",
        &profile,
        "_Z3q2CnOA6jEC31l",
        Some("PLSeq4utvoqwBO9ZHP3ZN-JyGU9wbmphgv"),
        Some("opaque-item-token"),
    );

    assert_eq!(body["videoId"], "Dx69Ut6Wyuk");
    assert_eq!(body["cpn"], "_Z3q2CnOA6jEC31l");
    assert_eq!(body["playlistId"], "PLSeq4utvoqwBO9ZHP3ZN-JyGU9wbmphgv");
    assert_eq!(body["params"], "opaque-item-token");
    assert_eq!(body["captionParams"], json!({}));
    assert_eq!(
        body.pointer("/playbackContext/contentPlaybackContext/signatureTimestamp")
            .and_then(|value| value.as_u64()),
        Some(YTM_WEB_SIGNATURE_TIMESTAMP_FALLBACK)
    );
    assert_eq!(
        body.pointer("/playbackContext/contentPlaybackContext/referer")
            .and_then(|value| value.as_str()),
        Some("https://music.youtube.com/")
    );
}

#[test]
fn cross_device_queue_body_requests_server_queue_restore() {
    let profile = YtmWebProfile::fallback();
    let body = YtMusicClient::cross_device_queue_body(&profile);

    assert_eq!(body["watchNextType"], "WATCH_NEXT_TYPE_GET_QUEUE");
    assert_eq!(body["queueContextParams"], "");
    assert!(body.get("fetchLiveState").is_none());
    assert_eq!(
        body.pointer("/context/client/clientName")
            .and_then(|value| value.as_str()),
        Some(profile.client_name.as_str())
    );
    assert_eq!(
        body.pointer("/context/client/clientVersion")
            .and_then(|value| value.as_str()),
        Some(profile.client_version.as_str())
    );
}
