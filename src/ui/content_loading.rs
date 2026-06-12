//! Async content loaders: every network/disk fetch that fills a view,
//! including home bootstrap with offline fallback, infinite scroll, search,
//! direct-URL autoplay, library pages, and the offline library.
//!
//! Spec: content_loading.md (all sections),
//! shell.md §6, library.md §5.
//! GTK source: src/ui/window/content_loading.rs.
//!
//! Pattern (CONTRACT.md §6): each function here snapshots what it needs from
//! `App` (client Arc, cache Arc, serials), runs the blocking fetch via
//! `app::blocking`, and maps the result into the OWNING VIEW's `Msg` (e.g.
//! `Message::Home(home::Msg::Loaded { ... })`). Serial/stale-guarding happens
//! in the receiving update, not here. These builders do not mutate `App`
//! except to flip the relevant `loading` flags / bump request serials before
//! spawning.
//!
//! Error-string convention (consumed by the receiving updates): worker errors
//! arrive already display-ready — the exact GTK label text (`Not connected.`,
//! `Error: {e}`, `Search error: {e}`). `NOT_CONNECTED` is exported so the home
//! bootstrap handler can map it to its "session still restoring" log wording.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use iced::Task;
use iced::widget::scrollable::AbsoluteOffset;

use crate::models::{
    Artist, CrossDeviceQueue, HomeBrowsePageType, HomeItem, HomePage, HomeSection, PlaylistSummary,
    Track,
};
use crate::network_state::NetworkState;
use crate::offline::{AppSettings, MusicCache};

use super::app::{self, App, ContentView, LoadState, Message};

/// Error payload delivered when a loader's blocking worker panics (resets the
/// owning view's loading flag instead of leaving it stuck; see
/// `app::blocking_or`).
const WORKER_PANICKED: &str = "Error: background worker panicked";
use super::sidebar;
use super::thumbnails;
use super::views::{browse_detail, home, library, search};

/// The exact "no client / session still restoring" label (GTK parity text).
pub const NOT_CONNECTED: &str = "Not connected.";

/// Sentinel `attempt` for non-bootstrap home loads (nav click / Ctrl+R /
/// browse back button): `home::Msg::Loaded` errors carrying this attempt
/// render inline and never retry or fall back offline. Bootstrap attempts are
/// `0..=HOME_BOOTSTRAP_BACKOFF_SECS.len()` (failure of the final attempt
/// triggers the offline fallback).
pub const NAV_LOAD_ATTEMPT: u32 = u32::MAX;

/// Targets recognized by `parse_direct_music_url` (shell spec §3.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectMusicUrlTarget {
    Video {
        video_id: String,
    },
    Browse {
        browse_id: String,
        page_type: HomeBrowsePageType,
    },
}

// ---------------------------------------------------------------------------
// Home
// ---------------------------------------------------------------------------

/// Startup home load with offline fallback (shell spec §6.3): up to 4
/// attempts with 1/2/4 s backoff ("Retry n/3 in Xs" status), aborts silently
/// if the user navigated off Home, and on final failure switches the active
/// nav to Offline and loads the offline library. Also fetches the
/// cross-device queue (errors tolerated) and persists listen-again metadata.
///
/// The retry orchestration lives in `views::home::update`: every fetch result
/// arrives as `home::Msg::Loaded { attempt, .. }`; failed attempts < the
/// backoff length schedule `home::Msg::RetryTimerFired { attempt + 1 }` via
/// `app::after`, whose handler bumps `home.bootstrap_attempt` and calls this
/// builder again (it reads the attempt back from state — `&App` here).
pub fn bootstrap_home_task(app: &mut App) -> Task<Message> {
    if app.home.page.is_some() {
        // Warm cache: the view re-renders synchronously from state; restore
        // the saved scroll offset (content_loading spec §4.2 step 2) and
        // re-arm the lazy fill (GTK ran queue_home_lazy_load_check after
        // every cached render).
        let scroll_position = app.home.scroll_position;
        return Task::batch([
            restore_content_scroll(scroll_position),
            home::lazy_fill_task(app),
        ]);
    }
    home_fetch_task(app, false, app.home.bootstrap_attempt)
}

/// Nav-click / Ctrl+R home load (single attempt, inline errors;
/// `force_refresh` bypasses the cache and resets scroll).
pub fn load_home_task(app: &mut App, force_refresh: bool) -> Task<Message> {
    app.content_view = ContentView::Home;

    if force_refresh {
        app.home.scroll_position = 0.0;
        app.home.page = None;
    } else if app.home.page.is_some() {
        // Cached path: fully synchronous re-render, no network, no
        // placeholder (content_loading spec §4.1 step 3).
        let (continuation, thumb_urls) = {
            let page = app.home.page.as_ref().expect("checked above");
            (page.continuation.clone(), home_thumbnail_urls(page))
        };
        app.home.continuation = continuation;
        app.home.loading = false;
        app.home.status = None;
        let scroll_position = app.home.scroll_position;
        let thumbs = thumbnails::ensure_all(app, thumb_urls.iter().map(String::as_str));
        // GTK ran queue_home_lazy_load_check after every cached render: a
        // short cached feed with a live continuation tops itself up.
        let lazy_fill = home::lazy_fill_task(app);
        return Task::batch([thumbs, restore_content_scroll(scroll_position), lazy_fill]);
    }

    app.home.continuation = None;
    app.home.loading = true;
    app.home.status = Some("Loading recommendations...".to_owned());

    let fetch = home_fetch_task(app, force_refresh, NAV_LOAD_ATTEMPT);
    if force_refresh {
        Task::batch([restore_content_scroll(0.0), fetch])
    } else {
        fetch
    }
}

/// Infinite-scroll continuation fetch (guards: home active, not loading,
/// continuation present — already enforced by the shell scroll handler).
pub fn load_more_home_task(app: &mut App) -> Task<Message> {
    let Some(token) = app.home.continuation.clone() else {
        return Task::none();
    };
    if app.home.loading {
        return Task::none();
    }
    app.home.loading = true;

    let client = app.app_client();
    app::blocking_or(
        move || {
            let ytm = client.ok_or_else(|| NOT_CONNECTED.to_owned())?;
            let mut page = ytm
                .get_home_page(Some(&token))
                .map_err(|err| format!("Error: {err}"))?;
            // A continuation identical to the one just consumed means
            // end-of-feed (prevents an infinite refetch loop).
            let next = page.continuation.take().filter(|next| next != &token);
            page.continuation = next;
            Ok(page)
        },
        |result| Message::Home(home::Msg::MoreLoaded { result }),
        Message::Home(home::Msg::MoreLoaded {
            result: Err(WORKER_PANICKED.to_owned()),
        }),
    )
}

/// Spawn the shared home + cross-device fetch (content_loading spec §4.1
/// step 5). Listen-again metadata persistence (spec §4.3 step 4) happens in
/// the same worker right after a successful fetch, so it runs on every
/// successful first-page load regardless of which path triggered it.
fn home_fetch_task(app: &App, force_refresh: bool, attempt: u32) -> Task<Message> {
    let client = app.app_client();
    let cache = app.cache.clone();
    let settings = app.settings.clone();
    app::blocking_or(
        move || fetch_home_and_cross_device(client, &cache, &settings),
        move |result| {
            Message::Home(home::Msg::Loaded {
                force_refresh,
                attempt,
                result,
            })
        },
        Message::Home(home::Msg::Loaded {
            force_refresh,
            attempt,
            result: Err(WORKER_PANICKED.to_owned()),
        }),
    )
}

fn fetch_home_and_cross_device(
    client: Option<Arc<NetworkState>>,
    cache: &Arc<Mutex<MusicCache>>,
    settings: &AppSettings,
) -> Result<(Option<CrossDeviceQueue>, HomePage), String> {
    let Some(ytm) = client else {
        return Err(NOT_CONNECTED.to_owned());
    };
    let cross_device_queue = match ytm.get_cross_device_queue() {
        Ok(queue) => queue,
        Err(err) => {
            crate::app_log!("[HOME] failed to fetch cross-device queue: {err}");
            None
        }
    };
    let page = ytm
        .get_home_page(None)
        .map_err(|err| format!("Error: {err}"))?;
    persist_listen_again_snapshot(cache, settings, &page.sections);
    Ok((cross_device_queue, page))
}

/// Upsert listen-again track metadata into the play-history DB (never with
/// `file://` thumbnails) and record the snapshot that feeds the offline
/// "Listen again" collection. Errors are swallowed (GTK parity).
fn persist_listen_again_snapshot(
    cache: &Arc<Mutex<MusicCache>>,
    settings: &AppSettings,
    sections: &[HomeSection],
) {
    let listen_again_tracks = extract_listen_again_tracks(sections);
    let listen_again_ids: Vec<String> = listen_again_tracks
        .iter()
        .map(|track| track.video_id.clone())
        .collect();
    let cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for track in &listen_again_tracks {
        let _ = cache.upsert_track_metadata(
            &track.video_id,
            &track.title,
            &artist_string(track),
            history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        );
    }
    let _ = cache.store_listen_again_snapshot(&listen_again_ids, settings);
}

/// The section titled exactly "Listen again" (case-insensitive), else the
/// first whose lowercased title contains it (GTK `extract_listen_again_tracks`).
fn extract_listen_again_tracks(sections: &[HomeSection]) -> Vec<Track> {
    sections
        .iter()
        .find(|section| section.title.eq_ignore_ascii_case("Listen again"))
        .or_else(|| {
            sections
                .iter()
                .find(|section| section.title.to_ascii_lowercase().contains("listen again"))
        })
        .map(|section| {
            section
                .contents
                .iter()
                .filter_map(|item| match item {
                    HomeItem::Track(track) => Some(track.clone()),
                    HomeItem::BrowseCard(_) => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn home_thumbnail_urls(page: &HomePage) -> Vec<String> {
    page.sections
        .iter()
        .flat_map(|section| section.contents.iter())
        .filter_map(|item| match item {
            HomeItem::Track(track) => track.thumbnail_url.clone(),
            HomeItem::BrowseCard(card) => card.thumbnail_url.clone(),
        })
        .collect()
}

fn restore_content_scroll(position: f32) -> Task<Message> {
    iced::widget::operation::scroll_to(
        app::CONTENT_SCROLL_ID,
        AbsoluteOffset {
            x: None,
            y: Some(position.max(0.0)),
        },
    )
}

// ---------------------------------------------------------------------------
// Search + direct URLs
// ---------------------------------------------------------------------------

/// Full search fetch; result maps to `views::search::Msg::Loaded` carrying
/// `request_id` so stale responses are dropped.
pub fn load_search_task(app: &mut App, query: String, request_id: u64) -> Task<Message> {
    app.content_view = ContentView::Search;
    app.search.loading_query = Some(query.clone());
    app.search.error = None;

    let client = app.app_client();
    let fetch_query = query.clone();
    let panic_query = query.clone();
    app::blocking_or(
        move || match client {
            None => Err(NOT_CONNECTED.to_owned()),
            Some(ytm) => ytm
                .search(&fetch_query)
                .map_err(|err| format!("Search error: {err}")),
        },
        move |result| {
            Message::Search(search::Msg::Loaded {
                request_id,
                query: query.clone(),
                result,
            })
        },
        Message::Search(search::Msg::Loaded {
            request_id,
            query: panic_query,
            result: Err(WORKER_PANICKED.to_owned()),
        }),
    )
}

/// Suggestion fetch after the 250 ms debounce; maps to
/// `sidebar::Msg::SuggestionsLoaded` with the serial.
pub fn load_suggestions_task(app: &App, query: String, serial: u64) -> Task<Message> {
    let client = app.app_client();
    app::blocking(
        move || match client {
            None => Err(NOT_CONNECTED.to_owned()),
            Some(ytm) => ytm
                .get_search_suggestions(&query)
                .map_err(|err| err.to_string()),
        },
        move |result| Message::Sidebar(sidebar::Msg::SuggestionsLoaded { serial, result }),
    )
}

/// Detect a pasted YouTube / YT Music URL (watch, playlist, browse, channel).
pub fn parse_direct_music_url(query: &str) -> Option<DirectMusicUrlTarget> {
    let trimmed = query.trim();

    if let Some(target) = parse_direct_music_reference(trimmed) {
        return Some(target);
    }

    let normalized = if trimmed.starts_with("music.youtube.com/")
        || trimmed.starts_with("youtube.com/")
        || trimmed.starts_with("www.youtube.com/")
        || trimmed.starts_with("youtu.be/")
    {
        format!("https://{trimmed}")
    } else {
        trimmed.to_string()
    };

    let url = url::Url::parse(&normalized).ok()?;
    let host = url.host_str()?.trim_start_matches("www.");

    match host {
        "youtu.be" => {
            let video_id = url
                .path_segments()
                .and_then(|mut segments| segments.next())
                .filter(|segment| !segment.is_empty())?;
            Some(DirectMusicUrlTarget::Video {
                video_id: video_id.to_string(),
            })
        }
        "youtube.com" | "music.youtube.com" => {
            let path = url.path().trim_end_matches('/');
            if path == "/watch" {
                if let Some(video_id) = url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "v").then_some(value.into_owned()))
                {
                    return Some(DirectMusicUrlTarget::Video { video_id });
                }

                if let Some(list_id) = url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "list").then_some(value.into_owned()))
                {
                    return Some(DirectMusicUrlTarget::Browse {
                        browse_id: normalize_playlist_browse_id(&list_id),
                        page_type: infer_browse_page_type(&list_id),
                    });
                }
            }

            if path == "/playlist" {
                let list_id = url
                    .query_pairs()
                    .find_map(|(key, value)| (key == "list").then_some(value.into_owned()))?;
                return Some(DirectMusicUrlTarget::Browse {
                    browse_id: normalize_playlist_browse_id(&list_id),
                    page_type: infer_browse_page_type(&list_id),
                });
            }

            if let Some(browse_id) = path
                .strip_prefix("/browse/")
                .filter(|value| !value.is_empty())
            {
                return Some(DirectMusicUrlTarget::Browse {
                    browse_id: browse_id.to_string(),
                    page_type: infer_browse_page_type(browse_id),
                });
            }

            if let Some(browse_id) = path
                .strip_prefix("/channel/")
                .filter(|value| !value.is_empty())
            {
                return Some(DirectMusicUrlTarget::Browse {
                    browse_id: browse_id.to_string(),
                    page_type: infer_browse_page_type(browse_id),
                });
            }

            None
        }
        _ => None,
    }
}

/// Non-URL references: `/browse/<id>`, `/channel/<id>`, or a bare
/// `UC*`/`MPRE*`/`VL*`/`OLAK*`/`RD*` id (optionally `/`-prefixed).
fn parse_direct_music_reference(input: &str) -> Option<DirectMusicUrlTarget> {
    let normalized = input.trim().trim_end_matches('/');

    if let Some(browse_id) = normalized
        .strip_prefix("/browse/")
        .or_else(|| normalized.strip_prefix("/channel/"))
        .filter(|value| !value.is_empty())
    {
        return Some(DirectMusicUrlTarget::Browse {
            browse_id: browse_id.to_string(),
            page_type: infer_browse_page_type(browse_id),
        });
    }

    let bare = normalized.trim_start_matches('/');
    if !bare.is_empty()
        && (bare.starts_with("UC")
            || bare.starts_with("MPRE")
            || bare.starts_with("VL")
            || bare.starts_with("OLAK")
            || bare.starts_with("RD"))
    {
        return Some(DirectMusicUrlTarget::Browse {
            browse_id: bare.to_string(),
            page_type: infer_browse_page_type(bare),
        });
    }

    None
}

fn normalize_playlist_browse_id(list_id: &str) -> String {
    if list_id.starts_with("VL") {
        list_id.to_string()
    } else {
        format!("VL{list_id}")
    }
}

fn infer_browse_page_type(browse_id: &str) -> HomeBrowsePageType {
    if browse_id.starts_with("MPRE") {
        HomeBrowsePageType::Album
    } else if browse_id.starts_with("MPSP") {
        HomeBrowsePageType::PodcastShow
    } else if browse_id.starts_with("UC") {
        HomeBrowsePageType::UserChannel
    } else if browse_id.starts_with("VL")
        || browse_id.starts_with("OLAK")
        || browse_id.starts_with("RD")
    {
        HomeBrowsePageType::Playlist
    } else {
        HomeBrowsePageType::Artist
    }
}

/// Autoplay/open a direct URL target (video → play+radio; browse → detail
/// page with autoplay).
///
/// Video targets resolve real metadata via `get_radio(id, None, None)` (the
/// matching track, else the first, else a fallback titled with the raw input
/// and "Unknown artist") and route the resolved track through
/// `home::Msg::TrackCardClicked`, whose handler is exactly the GTK behavior:
/// `play_track` (daemon `start_radio_from_seed(vec![seed], 0, 0, 1, None,
/// repeat_mode, true)`) plus `sidebar::add_to_recent`. The content area is
/// not navigated. Browse targets open browse detail with autoplay, titled
/// with the raw input until the page resolves.
pub fn autoplay_direct_music_url_task(
    app: &mut App,
    target: DirectMusicUrlTarget,
) -> Task<Message> {
    let raw_input = app.sidebar.search_text.trim().to_owned();
    match target {
        DirectMusicUrlTarget::Video { video_id } => {
            let fallback_title = if raw_input.is_empty() {
                video_id.clone()
            } else {
                raw_input
            };
            let client = app.app_client();
            app::blocking(
                move || resolve_direct_video_track(client, &video_id, &fallback_title),
                |track| Message::Home(home::Msg::TrackCardClicked(track)),
            )
        }
        DirectMusicUrlTarget::Browse {
            browse_id,
            page_type,
        } => {
            let title = if raw_input.is_empty() {
                browse_id.clone()
            } else {
                raw_input
            };
            load_browse_detail_task(app, browse_id, title, None, page_type, true)
        }
    }
}

fn resolve_direct_video_track(
    client: Option<Arc<NetworkState>>,
    video_id: &str,
    fallback_title: &str,
) -> Track {
    let fallback_track = Track {
        video_id: video_id.to_owned(),
        title: fallback_title.to_owned(),
        artists: vec![Artist {
            name: "Unknown artist".to_owned(),
            id: None,
        }],
        album: None,
        duration: None,
        thumbnail_url: None,
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    };

    let Some(ytm) = client else {
        return fallback_track;
    };
    match ytm.get_radio(video_id, None, None) {
        Ok(radio) => {
            let tracks = radio.tracks;
            tracks
                .iter()
                .find(|track| track.video_id == video_id)
                .cloned()
                .or_else(|| tracks.into_iter().next())
                .unwrap_or(fallback_track)
        }
        Err(_) => fallback_track,
    }
}

// ---------------------------------------------------------------------------
// Library / browse / offline loaders
// ---------------------------------------------------------------------------

pub fn load_likes_task(app: &mut App) -> Task<Message> {
    app.content_view = ContentView::Likes;
    app.library.likes = LoadState::Loading;

    let client = app.app_client();
    Task::batch([
        // GTK opened every library page at the top (clear_box clamped the
        // vadjustment); the shared content scrollable keeps its offset
        // otherwise.
        restore_content_scroll(0.0),
        app::blocking_or(
            move || match client {
                None => Err(NOT_CONNECTED.to_owned()),
                Some(ytm) => ytm
                    .get_playlist("LM")
                    .map_err(|err| format!("Error: {err}")),
            },
            |result| Message::Library(library::Msg::LikesLoaded(result)),
            Message::Library(library::Msg::LikesLoaded(Err(WORKER_PANICKED.to_owned()))),
        ),
    ])
}

pub fn load_playlists_task(app: &mut App) -> Task<Message> {
    app.content_view = ContentView::Playlists;
    app.library.playlists = LoadState::Loading;

    let client = app.app_client();
    Task::batch([
        restore_content_scroll(0.0),
        app::blocking_or(
            move || match client {
                None => Err(NOT_CONNECTED.to_owned()),
                Some(ytm) => ytm
                    .get_library_playlists()
                    .map_err(|err| format!("Error: {err}")),
            },
            |result| Message::Library(library::Msg::PlaylistsLoaded(result)),
            Message::Library(library::Msg::PlaylistsLoaded(Err(
                WORKER_PANICKED.to_owned()
            ))),
        ),
    ])
}

pub fn load_playlist_detail_task(
    app: &mut App,
    playlist_id: String,
    title: String,
    thumbnail_url: Option<String>,
) -> Task<Message> {
    app.content_view = ContentView::PlaylistDetail;
    app.library.playlist_detail = Some(library::PlaylistDetail {
        playlist_id: playlist_id.clone(),
        title,
        thumbnail_url,
        tracks: LoadState::Loading,
    });

    let client = app.app_client();
    let fetch_id = playlist_id.clone();
    let panic_id = playlist_id.clone();
    Task::batch([
        restore_content_scroll(0.0),
        app::blocking_or(
            move || match client {
                None => Err(NOT_CONNECTED.to_owned()),
                Some(ytm) => ytm
                    .get_playlist(&fetch_id)
                    .map_err(|err| format!("Error: {err}")),
            },
            move |result| {
                Message::Library(library::Msg::PlaylistDetailLoaded {
                    playlist_id: playlist_id.clone(),
                    result,
                })
            },
            Message::Library(library::Msg::PlaylistDetailLoaded {
                playlist_id: panic_id,
                result: Err(WORKER_PANICKED.to_owned()),
            }),
        ),
    ])
}

/// Browse activation: pushes the back target, then loads tracks or sections
/// for the browse id (album/playlist/artist/channel/podcast).
///
/// Artist/UserChannel resolve via `get_channel_page` (sections); Playlist/
/// Album/PodcastShow via `get_browse_tracks`. The single-track-album
/// short-circuit (library spec §5.4) is decided by `browse_detail::update`
/// on `Msg::Loaded` using `should_play_album_as_track`.
pub fn load_browse_detail_task(
    app: &mut App,
    browse_id: String,
    title: String,
    thumbnail_url: Option<String>,
    page_type: HomeBrowsePageType,
    autoplay: bool,
) -> Task<Message> {
    app.prepare_browse_back_target();

    app.browse.request_serial = app.browse.request_serial.wrapping_add(1);
    let serial = app.browse.request_serial;
    app.browse.browse_id = Some(browse_id.clone());
    app.browse.title = title;
    app.browse.subtitle = None;
    app.browse.thumbnail_url = thumbnail_url;
    app.browse.page_type = Some(page_type);
    app.browse.data = None;
    app.browse.loading = true;
    app.browse.error = None;
    app.browse.autoplay_pending = autoplay;

    let client = app.app_client();
    app::blocking_or(
        move || {
            let ytm = client.ok_or_else(|| NOT_CONNECTED.to_owned())?;
            match page_type {
                HomeBrowsePageType::Artist | HomeBrowsePageType::UserChannel => ytm
                    .get_channel_page(&browse_id)
                    .map(browse_detail::LoadedData::Sections)
                    .map_err(|err| format!("Error: {err}")),
                HomeBrowsePageType::Playlist
                | HomeBrowsePageType::Album
                | HomeBrowsePageType::PodcastShow => ytm
                    .get_browse_tracks(&browse_id)
                    .map(browse_detail::LoadedData::Tracks)
                    .map_err(|err| format!("Error: {err}")),
            }
        },
        move |result| Message::Browse(browse_detail::Msg::Loaded { serial, result }),
        Message::Browse(browse_detail::Msg::Loaded {
            serial,
            result: Err(WORKER_PANICKED.to_owned()),
        }),
    )
}

/// Offline landing page: playlists + cached tracks (display + full lists),
/// best-effort metadata repair (content_loading spec §7.3).
pub fn load_offline_task(app: &mut App) -> Task<Message> {
    app.content_view = ContentView::Offline;
    app.library.offline = LoadState::Loading;

    let client = app.app_client();
    let cache = app.cache.clone();
    let settings = app.settings.clone();
    Task::batch([
        restore_content_scroll(0.0),
        app::blocking_or(
            move || Ok::<_, String>(load_offline_page_data(client, &cache, &settings)),
            |result| Message::Library(library::Msg::OfflineLoaded(result)),
            Message::Library(library::Msg::OfflineLoaded(Err(WORKER_PANICKED.to_owned()))),
        ),
    ])
}

pub fn load_offline_playlist_detail_task(
    app: &mut App,
    playlist_id: String,
    title: String,
) -> Task<Message> {
    app.content_view = ContentView::OfflinePlaylistDetail;
    app.library.offline_playlist_detail = Some(library::OfflinePlaylistDetail {
        playlist_id: playlist_id.clone(),
        title,
        tracks: LoadState::Loading,
    });

    let cache = app.cache.clone();
    let settings = app.settings.clone();
    let fetch_id = playlist_id.clone();
    let panic_id = playlist_id.clone();
    Task::batch([
        restore_content_scroll(0.0),
        app::blocking_or(
            move || Ok::<_, String>(load_offline_playlist_tracks(&cache, &settings, &fetch_id)),
            move |result| {
                Message::Library(library::Msg::OfflinePlaylistLoaded {
                    playlist_id: playlist_id.clone(),
                    result,
                })
            },
            Message::Library(library::Msg::OfflinePlaylistLoaded {
                playlist_id: panic_id,
                result: Err(WORKER_PANICKED.to_owned()),
            }),
        ),
    ])
}

/// Blocking offline-page worker (content_loading spec §7.2): metadata repair
/// first, then playlists (with the synthetic "Listen again" collection at
/// index 0), the full cached track list, and the loose-track display list
/// (tracks belonging to any collection are excluded).
fn load_offline_page_data(
    client: Option<Arc<NetworkState>>,
    cache: &Arc<Mutex<MusicCache>>,
    settings: &AppSettings,
) -> library::OfflineLoadedData {
    repair_offline_track_metadata(client, cache);

    let cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let mut playlists: Vec<PlaylistSummary> = cache
        .offline_playlists()
        .unwrap_or_default()
        .into_iter()
        .map(|(playlist_id, title, thumbnail_url, subtitle)| {
            let thumbnail_url = thumbnail_url.or_else(|| {
                cache
                    .offline_playlist_track_ids(&playlist_id)
                    .ok()
                    .and_then(|ids| ids.first().cloned())
                    .and_then(|vid| cache.get_cached_art(&vid))
                    .map(|path| format!("file://{}", path.display()))
            });

            PlaylistSummary {
                playlist_id,
                title,
                thumbnail_url,
                subtitle,
            }
        })
        .collect();

    let listen_again_track_ids = cache
        .offline_listen_again_track_ids(settings)
        .unwrap_or_default();
    if let Some(first_video_id) = listen_again_track_ids.first() {
        playlists.insert(
            0,
            PlaylistSummary {
                playlist_id: app::OFFLINE_LISTEN_AGAIN_PLAYLIST_ID.to_owned(),
                title: "Listen again".to_owned(),
                thumbnail_url: offline_thumbnail_url(&cache, first_video_id),
                subtitle: Some("Auto-kept from Listen again".to_owned()),
            },
        );
    }

    let all_tracks: Vec<Track> = cache
        .offline_tracks()
        .unwrap_or_default()
        .into_iter()
        .map(|(video_id, title, artist, _art)| {
            let thumbnail_url = offline_thumbnail_url(&cache, &video_id);
            offline_track(video_id, title, artist, thumbnail_url)
        })
        .collect();

    let playlist_track_ids: HashSet<String> = playlists
        .iter()
        .flat_map(|playlist| {
            if playlist.playlist_id == app::OFFLINE_LISTEN_AGAIN_PLAYLIST_ID {
                listen_again_track_ids.clone()
            } else {
                cache
                    .offline_playlist_track_ids(&playlist.playlist_id)
                    .unwrap_or_default()
            }
        })
        .collect();

    let display_tracks = all_tracks
        .iter()
        .filter(|track| !playlist_track_ids.contains(&track.video_id))
        .cloned()
        .collect();

    library::OfflineLoadedData {
        playlists,
        display_tracks,
        all_tracks,
    }
}

/// Self-healing for offline rows with broken metadata: at most 8 per page
/// load, `get_radio` as the metadata oracle, silently skipped when offline.
fn repair_offline_track_metadata(
    client: Option<Arc<NetworkState>>,
    cache: &Arc<Mutex<MusicCache>>,
) {
    let repair_ids = {
        let cache = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .offline_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(video_id, title, artist, _art)| {
                let thumbnail_url = offline_thumbnail_url(&cache, &video_id);
                let needs_repair = title.trim().is_empty()
                    || title == video_id
                    || artist.trim().is_empty()
                    || thumbnail_url.is_none();
                needs_repair.then_some(video_id)
            })
            .take(8)
            .collect::<Vec<_>>()
    };

    if repair_ids.is_empty() {
        return;
    }

    let Some(client) = client else {
        return;
    };

    for video_id in repair_ids {
        let mut radio = match client.get_radio(&video_id, None, None) {
            Ok(radio) => radio,
            Err(err) => {
                crate::app_log!("[OFFLINE] failed to repair metadata for {video_id}: {err}");
                continue;
            }
        };

        let repaired = radio
            .tracks
            .iter()
            .find(|track| track.video_id == video_id)
            .cloned()
            .or_else(|| radio.tracks.drain(..).next());

        let Some(track) = repaired else {
            continue;
        };

        let cache = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = cache.upsert_track_metadata(
            &track.video_id,
            &track.title,
            &artist_string(&track),
            history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        );
    }
}

/// Blocking resolver for an offline collection's tracks (content_loading
/// spec §7.4): listen-again ids for the synthetic collection, the playlist
/// table otherwise; metadata from the play-history cache.
fn load_offline_playlist_tracks(
    cache: &Arc<Mutex<MusicCache>>,
    settings: &AppSettings,
    playlist_id: &str,
) -> Vec<Track> {
    let cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let video_ids = if playlist_id == app::OFFLINE_LISTEN_AGAIN_PLAYLIST_ID {
        cache
            .offline_listen_again_track_ids(settings)
            .unwrap_or_default()
    } else {
        cache
            .offline_playlist_track_ids(playlist_id)
            .unwrap_or_default()
    };

    video_ids
        .into_iter()
        .map(|video_id| {
            let thumbnail_url = offline_thumbnail_url(&cache, &video_id);
            let (title, artist) = cache
                .get_track_meta(&video_id)
                .unwrap_or_else(|| (video_id.clone(), String::new()));
            offline_track(video_id, title, artist, thumbnail_url)
        })
        .collect()
}

/// Cached art (`file://`) preferred over the play-history thumbnail URL.
fn offline_thumbnail_url(cache: &MusicCache, video_id: &str) -> Option<String> {
    cache
        .get_cached_art(video_id)
        .map(|path| format!("file://{}", path.display()))
        .or_else(|| cache.get_thumbnail_url(video_id))
}

fn offline_track(
    video_id: String,
    title: String,
    artist: String,
    thumbnail_url: Option<String>,
) -> Track {
    let artists = if artist.is_empty() {
        Vec::new()
    } else {
        vec![Artist {
            name: artist,
            id: None,
        }]
    };

    Track {
        video_id,
        title,
        artists,
        album: None,
        duration: None,
        thumbnail_url,
        playback_playlist_id: None,
        playback_params: None,
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    }
}

fn artist_string(track: &Track) -> String {
    if track.artists.is_empty() {
        "Unknown artist".to_owned()
    } else {
        track
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Never persist local-file art into the play-history DB.
fn history_thumbnail_candidate(url: Option<&str>) -> Option<&str> {
    url.filter(|value| !value.starts_with("file://"))
}

#[cfg(test)]
#[path = "../tests/ui_content_loading.rs"]
mod tests;
