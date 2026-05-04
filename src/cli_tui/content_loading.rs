use super::*;
use super::{settings_support::*, thumbnails::*, views::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn maybe_request_more_home(&mut self) {
        if !matches!(&self.content_view, TuiContentView::Home) {
            return;
        }

        if self.home_loading_more {
            return;
        }

        let continuation = match &self.home_status {
            HomeFeedStatus::Ready(page) => page.continuation.clone(),
            HomeFeedStatus::Loading | HomeFeedStatus::Error(_) => None,
        };
        let Some(continuation) = continuation.filter(|token| !token.is_empty()) else {
            return;
        };

        let viewport = self.home_viewport_rows.max(1);
        let remaining = self
            .home_doc
            .total_height
            .saturating_sub(self.home_scroll_offset.saturating_add(viewport));
        if remaining > HOME_LOAD_MORE_THRESHOLD_ROWS {
            return;
        }

        self.home_loading_more = true;
        self.status_line = "Loading more Home sections...".to_string();
        spawn_home_append(self.background_tx.clone(), continuation);
    }

    pub(in crate::cli_tui) fn drain_background_events(&mut self) {
        while let Ok(event) = self.background_rx.try_recv() {
            match event {
                TuiBackgroundEvent::HomeLoaded(Ok(page)) => {
                    self.home_loading_more = false;
                    self.home_status = HomeFeedStatus::Ready(page);
                    self.home_selected = 0;
                    self.home_scroll_offset = 0;
                    self.status_line = "Home feed loaded.".to_string();
                }
                TuiBackgroundEvent::HomeLoaded(Err(err)) => {
                    self.home_loading_more = false;
                    self.home_status = HomeFeedStatus::Error(err.clone());
                    self.home_selected = 0;
                    self.home_scroll_offset = 0;
                    self.status_line = err;
                }
                TuiBackgroundEvent::HomeAppended {
                    continuation,
                    result,
                } => {
                    self.home_loading_more = false;
                    match result {
                        Ok(mut page) => {
                            let next_continuation = page
                                .continuation
                                .clone()
                                .filter(|token| token != &continuation);
                            match &mut self.home_status {
                                HomeFeedStatus::Ready(existing)
                                    if existing.continuation.as_deref()
                                        == Some(continuation.as_str()) =>
                                {
                                    existing.sections.append(&mut page.sections);
                                    existing.continuation = next_continuation;
                                    self.status_line = format!(
                                        "Home feed expanded to {} sections.",
                                        existing.sections.len()
                                    );
                                }
                                _ => {}
                            }
                        }
                        Err(err) => {
                            self.status_line = format!("Home pagination failed: {err}");
                        }
                    }
                }
                TuiBackgroundEvent::BrowseLoaded { browse_id, result } => {
                    let Some(view) = (match &mut self.content_view {
                        TuiContentView::Browse(view) => Some(view),
                        _ => None,
                    }) else {
                        continue;
                    };

                    if view.card.browse_id != browse_id
                        || !matches!(&view.status, BrowseViewStatus::Loading)
                    {
                        continue;
                    }

                    match result {
                        Ok(BrowseLoadResult::Tracks(tracks)) => {
                            view.status = BrowseViewStatus::Tracks {
                                title: view.card.title.clone(),
                                thumbnail_url: view.card.thumbnail_url.clone(),
                                tracks,
                            };
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = format!("Opened {}", shorten(&view.card.title, 32));
                        }
                        Ok(BrowseLoadResult::Sections(page)) => {
                            let title = page
                                .title
                                .clone()
                                .unwrap_or_else(|| view.card.title.clone());
                            let thumbnail_url = page
                                .thumbnail_url
                                .clone()
                                .or_else(|| view.card.thumbnail_url.clone());
                            view.status = BrowseViewStatus::Sections {
                                title,
                                thumbnail_url,
                                artist_like: page.artist_like,
                                sections: page.sections,
                            };
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = format!("Opened {}", shorten(&view.card.title, 32));
                        }
                        Err(message) => {
                            view.status = BrowseViewStatus::Error(message.clone());
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = format!(
                                "Could not open {}: {message}",
                                shorten(&view.card.title, 28)
                            );
                        }
                    }
                }
                TuiBackgroundEvent::LikesLoaded(result) => {
                    let Some(view) = (match &mut self.content_view {
                        TuiContentView::Likes(view) => Some(view),
                        _ => None,
                    }) else {
                        continue;
                    };
                    if !matches!(view.status, TuiTrackPageStatus::Loading) {
                        continue;
                    }
                    match result {
                        Ok(tracks) => {
                            view.subtitle = if tracks.is_empty() {
                                "No liked songs yet".to_string()
                            } else {
                                format!("{} liked songs", tracks.len())
                            };
                            view.status = TuiTrackPageStatus::Ready {
                                tracks,
                                context: Some(PlaylistContext {
                                    playlist_id: "LM".to_string(),
                                    title: "Liked Songs".to_string(),
                                    thumbnail_url: None,
                                    subtitle: None,
                                    source: PlaylistContextSource::Remote,
                                }),
                            };
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = "Loaded liked songs.".to_string();
                        }
                        Err(message) => {
                            view.status = TuiTrackPageStatus::Error(message.clone());
                            self.status_line = format!("Likes failed: {message}");
                        }
                    }
                }
                TuiBackgroundEvent::PlaylistsLoaded(result) => {
                    let Some(view) = (match &mut self.content_view {
                        TuiContentView::Playlists(view) => Some(view),
                        _ => None,
                    }) else {
                        continue;
                    };
                    if !matches!(view.status, TuiCollectionPageStatus::Loading) {
                        continue;
                    }
                    match result {
                        Ok(items) => {
                            let count = items.len();
                            view.subtitle = if count == 0 {
                                "No playlists found".to_string()
                            } else {
                                format!("{count} playlists in your library")
                            };
                            view.status = TuiCollectionPageStatus::Ready {
                                items: items
                                    .into_iter()
                                    .map(|summary| TuiPlaylistCard {
                                        summary,
                                        source: TuiPlaylistSource::Remote,
                                    })
                                    .collect(),
                            };
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = "Loaded playlists.".to_string();
                        }
                        Err(message) => {
                            view.status = TuiCollectionPageStatus::Error(message.clone());
                            self.status_line = format!("Playlists failed: {message}");
                        }
                    }
                }
                TuiBackgroundEvent::TrackPageLoaded { key, result } => {
                    let Some(view) = (match &mut self.content_view {
                        TuiContentView::PlaylistDetail(view) => Some(view),
                        _ => None,
                    }) else {
                        continue;
                    };
                    if view.key != key || !matches!(view.status, TuiTrackPageStatus::Loading) {
                        continue;
                    }
                    match result {
                        Ok(tracks) => {
                            let context = match view.kind {
                                TuiTrackPageKind::Likes => Some(PlaylistContext {
                                    playlist_id: "LM".to_string(),
                                    title: view.title.clone(),
                                    thumbnail_url: view.thumbnail_url.clone(),
                                    subtitle: Some(track_count_label(tracks.len())),
                                    source: PlaylistContextSource::Remote,
                                }),
                                TuiTrackPageKind::RemotePlaylist => Some(PlaylistContext {
                                    playlist_id: view.key.trim_start_matches("remote:").to_string(),
                                    title: view.title.clone(),
                                    thumbnail_url: view.thumbnail_url.clone(),
                                    subtitle: Some(track_count_label(tracks.len())),
                                    source: PlaylistContextSource::Remote,
                                }),
                                TuiTrackPageKind::OfflinePlaylist => Some(PlaylistContext {
                                    playlist_id: view
                                        .key
                                        .trim_start_matches("offline:")
                                        .to_string(),
                                    title: view.title.clone(),
                                    thumbnail_url: view.thumbnail_url.clone(),
                                    subtitle: Some(track_count_label(tracks.len())),
                                    source: PlaylistContextSource::Offline,
                                }),
                            };
                            view.subtitle = track_count_label(tracks.len());
                            view.status = TuiTrackPageStatus::Ready { tracks, context };
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = format!("Opened {}", shorten(&view.title, 32));
                        }
                        Err(message) => {
                            view.status = TuiTrackPageStatus::Error(message.clone());
                            self.status_line =
                                format!("Could not open {}: {message}", shorten(&view.title, 28));
                        }
                    }
                }
                TuiBackgroundEvent::OfflineLoaded(result) => {
                    let Some(view) = (match &mut self.content_view {
                        TuiContentView::Offline(view) => Some(view),
                        _ => None,
                    }) else {
                        continue;
                    };
                    if !matches!(view.status, TuiOfflineStatus::Loading) {
                        continue;
                    }
                    match result {
                        Ok((playlists, tracks, all_tracks)) => {
                            view.status = TuiOfflineStatus::Ready {
                                playlists: playlists
                                    .into_iter()
                                    .map(|summary| TuiPlaylistCard {
                                        summary,
                                        source: TuiPlaylistSource::Offline,
                                    })
                                    .collect(),
                                tracks,
                                all_track_count: all_tracks.len(),
                            };
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = "Loaded offline library.".to_string();
                        }
                        Err(message) => {
                            view.status = TuiOfflineStatus::Error(message.clone());
                            self.status_line = format!("Offline failed: {message}");
                        }
                    }
                }
                TuiBackgroundEvent::SearchLoaded { query, result } => {
                    let Some(view) = (match &mut self.content_view {
                        TuiContentView::Search(view) => Some(view),
                        _ => None,
                    }) else {
                        continue;
                    };
                    if view.query != query || !matches!(view.status, TuiSearchStatus::Loading) {
                        continue;
                    }
                    match result {
                        Ok(results) => {
                            let total = search_results_total(&results);
                            view.status = TuiSearchStatus::Ready(results);
                            if self.submitted_search_query_to_clear.as_deref()
                                == Some(query.as_str())
                            {
                                self.completed_search_query_to_clear = Some(query.clone());
                                self.submitted_search_query_to_clear = None;
                            }
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = format!("Search returned {total} results.");
                        }
                        Err(message) => {
                            view.status = TuiSearchStatus::Error(message.clone());
                            self.status_line = format!("Search failed: {message}");
                        }
                    }
                }
                TuiBackgroundEvent::SearchSuggestionsLoaded {
                    request_id,
                    query,
                    result,
                } => {
                    let matches_request = self.search_suggestion_inflight.as_ref().is_some_and(
                        |(current_id, current_query)| {
                            *current_id == request_id && current_query == &query
                        },
                    );
                    if !matches_request {
                        continue;
                    }

                    self.search_suggestion_inflight = None;
                    if !self.search_input_active || self.search_query.trim() != query {
                        continue;
                    }

                    match result {
                        Ok(suggestions) => {
                            self.search_suggestions = suggestions.into_iter().take(8).collect();
                            self.search_suggestion_selected = 0;
                            self.search_suggestions_loading = false;
                        }
                        Err(message) => {
                            self.search_suggestions.clear();
                            self.search_suggestion_selected = 0;
                            self.search_suggestions_loading = false;
                            self.status_line = format!("Search suggestions failed: {message}");
                        }
                    }
                }
                TuiBackgroundEvent::SettingsLoaded(result) => {
                    let Some(view) = (match &mut self.content_view {
                        TuiContentView::Settings(view) => Some(view),
                        _ => None,
                    }) else {
                        continue;
                    };
                    if !matches!(view.status, TuiSettingsStatus::Loading) {
                        continue;
                    }
                    match result {
                        Ok(page) => {
                            view.status = TuiSettingsStatus::Ready(page);
                            self.home_selected = 0;
                            self.home_scroll_offset = 0;
                            self.status_line = "Loaded settings.".to_string();
                        }
                        Err(message) => {
                            view.status = TuiSettingsStatus::Error(message.clone());
                            self.status_line = format!("Settings failed: {message}");
                        }
                    }
                }
                TuiBackgroundEvent::HistoryLoaded(Ok(tracks)) => {
                    self.recent_tracks = tracks.into_iter().take(RECENT_LIMIT).collect();
                    self.recent_selected = self
                        .recent_selected
                        .min(self.recent_tracks.len().saturating_sub(1));
                }
                TuiBackgroundEvent::HistoryLoaded(Err(err)) => {
                    if self.recent_tracks.is_empty() {
                        self.status_line = format!("History unavailable: {err}");
                    }
                }
                TuiBackgroundEvent::LyricsLoaded { video_id, result } => {
                    let current_matches = self
                        .snapshot
                        .current_track
                        .as_ref()
                        .is_some_and(|track| track.video_id == video_id);
                    if !current_matches {
                        continue;
                    }

                    self.lyrics_state = match result {
                        Ok(Some(lyrics)) => LyricsState::Ready { video_id, lyrics },
                        Ok(None) => LyricsState::Missing {
                            video_id,
                            reason: "YouTube did not expose timed lyrics for this track."
                                .to_string(),
                        },
                        Err(message) => LyricsState::Error { video_id, message },
                    };
                }
                TuiBackgroundEvent::ThumbnailLoaded { url, result } => {
                    let asset = match result {
                        Ok(bytes) => match image::load_from_memory(&bytes) {
                            Ok(image) => ThumbnailAsset::Ready(center_crop_square(image)),
                            Err(err) => ThumbnailAsset::Failed(err.to_string()),
                        },
                        Err(message) => ThumbnailAsset::Failed(message),
                    };

                    self.thumbnail_assets.insert(url, asset);
                }
            }
        }
    }
}

pub(in crate::cli_tui) fn spawn_home_bootstrap(tx: mpsc::Sender<TuiBackgroundEvent>) {
    std::thread::spawn(move || match saved_network_state() {
        Ok(network) => {
            let home = network.get_home_page(None).map_err(|err| err.to_string());
            let history = network.get_history().map_err(|err| err.to_string());
            let _ = tx.send(TuiBackgroundEvent::HomeLoaded(home));
            let _ = tx.send(TuiBackgroundEvent::HistoryLoaded(history));
        }
        Err(_) => {
            let _ = tx.send(TuiBackgroundEvent::HomeLoaded(Err(
                "Connect YouTube Music in the GTK app to load the full Home feed.".to_string(),
            )));
            let _ = tx.send(TuiBackgroundEvent::HistoryLoaded(Ok(Vec::new())));
        }
    });
}

pub(in crate::cli_tui) fn spawn_home_append(
    tx: mpsc::Sender<TuiBackgroundEvent>,
    continuation: String,
) {
    std::thread::spawn(move || {
        let result = saved_network_state()
            .and_then(|network| network.get_home_page(Some(&continuation)))
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::HomeAppended {
            continuation,
            result,
        });
    });
}

pub(in crate::cli_tui) fn spawn_browse_load(
    tx: mpsc::Sender<TuiBackgroundEvent>,
    card: HomeBrowseCard,
) {
    std::thread::spawn(move || {
        let browse_id = card.browse_id.clone();
        let result = saved_network_state()
            .and_then(|network| match card.page_type {
                HomeBrowsePageType::Artist | HomeBrowsePageType::UserChannel => network
                    .get_channel_page(&browse_id)
                    .map(BrowseLoadResult::Sections),
                HomeBrowsePageType::Playlist
                | HomeBrowsePageType::Album
                | HomeBrowsePageType::PodcastShow => network
                    .get_browse_tracks(&browse_id)
                    .map(BrowseLoadResult::Tracks),
            })
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::BrowseLoaded { browse_id, result });
    });
}

pub(in crate::cli_tui) fn spawn_likes_load(tx: mpsc::Sender<TuiBackgroundEvent>) {
    std::thread::spawn(move || {
        let result = saved_network_state()
            .and_then(|network| network.get_playlist("LM"))
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::LikesLoaded(result));
    });
}

pub(in crate::cli_tui) fn spawn_playlists_load(tx: mpsc::Sender<TuiBackgroundEvent>) {
    std::thread::spawn(move || {
        let result = saved_network_state()
            .and_then(|network| network.get_library_playlists())
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::PlaylistsLoaded(result));
    });
}

pub(in crate::cli_tui) fn spawn_playlist_detail_load(
    tx: mpsc::Sender<TuiBackgroundEvent>,
    playlist: TuiPlaylistCard,
) {
    std::thread::spawn(move || {
        let key = playlist_detail_key(&playlist);
        let result = load_playlist_card_tracks(&playlist).map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::TrackPageLoaded { key, result });
    });
}

pub(in crate::cli_tui) fn spawn_offline_load(tx: mpsc::Sender<TuiBackgroundEvent>) {
    std::thread::spawn(move || {
        let result = load_offline_page_data_tui().map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::OfflineLoaded(result));
    });
}

pub(in crate::cli_tui) fn spawn_search_load(tx: mpsc::Sender<TuiBackgroundEvent>, query: String) {
    std::thread::spawn(move || {
        let result = saved_network_state()
            .and_then(|network| network.search(&query))
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::SearchLoaded { query, result });
    });
}

pub(in crate::cli_tui) fn spawn_search_suggestions_load(
    tx: mpsc::Sender<TuiBackgroundEvent>,
    request_id: u64,
    query: String,
) {
    std::thread::spawn(move || {
        let result = saved_network_state()
            .and_then(|network| network.get_search_suggestions(&query))
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::SearchSuggestionsLoaded {
            request_id,
            query,
            result,
        });
    });
}

pub(in crate::cli_tui) fn spawn_settings_load(tx: mpsc::Sender<TuiBackgroundEvent>) {
    std::thread::spawn(move || {
        let result = load_settings_page_data().map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::SettingsLoaded(result));
    });
}

pub(in crate::cli_tui) fn spawn_lyrics_load(
    tx: mpsc::Sender<TuiBackgroundEvent>,
    video_id: String,
) {
    std::thread::spawn(move || {
        let result = saved_network_state()
            .and_then(|network| network.get_synced_lyrics(&video_id))
            .map_err(|err| err.to_string());
        let _ = tx.send(TuiBackgroundEvent::LyricsLoaded { video_id, result });
    });
}

pub(in crate::cli_tui) fn open_tui_music_cache() -> Result<MusicCache> {
    let storage_root = get_storage_root();
    MusicCache::new_with_storage_root(storage_root.as_deref())
        .context("Failed to open Static settings store")
}

pub(in crate::cli_tui) fn load_playlist_card_tracks(card: &TuiPlaylistCard) -> Result<Vec<Track>> {
    match card.source {
        TuiPlaylistSource::Remote => saved_network_state()
            .and_then(|network| network.get_playlist(&card.summary.playlist_id)),
        TuiPlaylistSource::Offline => {
            Ok(load_offline_playlist_tracks_tui(&card.summary.playlist_id)?)
        }
    }
}

pub(in crate::cli_tui) fn load_offline_page_data_tui()
-> Result<(Vec<PlaylistSummary>, Vec<Track>, Vec<Track>)> {
    let cache = open_tui_music_cache()?;
    let settings = cache.load_settings()?;

    let mut offline_playlists: Vec<PlaylistSummary> = cache
        .offline_playlists()?
        .into_iter()
        .map(
            |(playlist_id, title, thumbnail_url, subtitle)| PlaylistSummary {
                playlist_id,
                title,
                thumbnail_url,
                subtitle,
            },
        )
        .collect();

    let listen_again_track_ids = cache.offline_listen_again_track_ids(&settings)?;
    if let Some(first_video_id) = listen_again_track_ids.first() {
        offline_playlists.insert(
            0,
            PlaylistSummary {
                playlist_id: "__listen_again__".to_string(),
                title: "Listen again".to_string(),
                thumbnail_url: offline_thumbnail_url_tui(&cache, first_video_id),
                subtitle: Some("Auto-kept from Listen again".to_string()),
            },
        );
    }

    let all_tracks = cache
        .offline_tracks()?
        .into_iter()
        .map(|(video_id, title, artist, _)| Track {
            video_id: video_id.clone(),
            title,
            artists: if artist.is_empty() {
                Vec::new()
            } else {
                vec![crate::models::Artist {
                    name: artist,
                    id: None,
                }]
            },
            album: None,
            duration: None,
            thumbnail_url: offline_thumbnail_url_tui(&cache, &video_id),
            playback_playlist_id: None,
            playback_params: None,
            not_interested_feedback_token: None,
            undo_feedback_token: None,
        })
        .collect::<Vec<_>>();

    let playlist_track_ids = offline_playlists
        .iter()
        .flat_map(|playlist| {
            if playlist.playlist_id == "__listen_again__" {
                listen_again_track_ids.clone()
            } else {
                cache
                    .offline_playlist_track_ids(&playlist.playlist_id)
                    .unwrap_or_default()
            }
        })
        .collect::<HashSet<_>>();

    let display_tracks = all_tracks
        .iter()
        .filter(|track| !playlist_track_ids.contains(&track.video_id))
        .cloned()
        .collect::<Vec<_>>();

    Ok((offline_playlists, display_tracks, all_tracks))
}

pub(in crate::cli_tui) fn load_offline_playlist_tracks_tui(
    playlist_id: &str,
) -> Result<Vec<Track>> {
    let cache = open_tui_music_cache()?;
    let settings = cache.load_settings()?;
    let video_ids = if playlist_id == "__listen_again__" {
        cache.offline_listen_again_track_ids(&settings)?
    } else {
        cache.offline_playlist_track_ids(playlist_id)?
    };

    Ok(video_ids
        .into_iter()
        .map(|video_id| {
            let thumbnail_url = offline_thumbnail_url_tui(&cache, &video_id);
            let (title, artist) = cache
                .get_track_meta(&video_id)
                .unwrap_or_else(|| (video_id.clone(), String::new()));
            Track {
                video_id,
                title,
                artists: if artist.is_empty() {
                    Vec::new()
                } else {
                    vec![crate::models::Artist {
                        name: artist,
                        id: None,
                    }]
                },
                album: None,
                duration: None,
                thumbnail_url,
                playback_playlist_id: None,
                playback_params: None,
                not_interested_feedback_token: None,
                undo_feedback_token: None,
            }
        })
        .collect())
}

pub(in crate::cli_tui) fn offline_thumbnail_url_tui(
    cache: &MusicCache,
    video_id: &str,
) -> Option<String> {
    cache
        .get_cached_art(video_id)
        .map(|path| format!("file://{}", path.display()))
        .or_else(|| cache.get_thumbnail_url(video_id))
}

pub(in crate::cli_tui) fn load_settings_page_data() -> Result<TuiSettingsPageData> {
    let cache = open_tui_music_cache()?;
    let settings = cache.load_settings()?;
    Ok(TuiSettingsPageData {
        sections: build_tui_settings_sections(&settings, &cache)?,
    })
}
