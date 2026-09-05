use super::*;
use super::{content_loading::*, page_builders::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn refresh_snapshot(&mut self) {
        match self.client.state() {
            Ok(snapshot) => {
                self.apply_snapshot(snapshot);
            }
            Err(err) => {
                if let Err(recovery_err) = self.recover_daemon_from_tui_state() {
                    self.status_line =
                        format!("Daemon refresh failed: {err}. Recovery failed: {recovery_err}");
                }
            }
        }
        self.last_daemon_poll_at = Instant::now();
    }

    pub(in crate::cli_tui) fn push_content_snapshot(&mut self) {
        self.content_backstack.push(ContentSnapshot {
            view: self.content_view.clone(),
            selected: self.home_selected,
            scroll_offset: self.home_scroll_offset,
        });
    }

    pub(in crate::cli_tui) fn restore_content_snapshot(&mut self, snapshot: ContentSnapshot) {
        self.content_view = snapshot.view;
        self.home_selected = snapshot.selected;
        self.home_scroll_offset = snapshot.scroll_offset;
    }

    pub(in crate::cli_tui) fn open_browse_detail(&mut self, card: HomeBrowseCard) {
        self.clear_completed_search_query_if_leaving_search();
        self.push_content_snapshot();
        self.content_view = TuiContentView::Browse(TuiBrowseView {
            card: card.clone(),
            status: BrowseViewStatus::Loading,
        });
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.home_loading_more = false;
        self.status_line = format!("Loading {}...", shorten(&card.title, 32));
        spawn_browse_load(self.background_tx.clone(), card);
    }

    pub(in crate::cli_tui) fn open_likes(&mut self) {
        self.clear_completed_search_query_if_leaving_search();
        self.push_content_snapshot();
        self.content_view = TuiContentView::Likes(TuiTrackPageView {
            kind: TuiTrackPageKind::Likes,
            key: "likes".to_string(),
            title: "Liked Songs".to_string(),
            subtitle: "Your saved thumbs-up collection".to_string(),
            thumbnail_url: None,
            status: TuiTrackPageStatus::Loading,
        });
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.status_line = "Loading liked songs...".to_string();
        spawn_likes_load(self.background_tx.clone());
    }

    pub(in crate::cli_tui) fn open_playlists(&mut self) {
        self.clear_completed_search_query_if_leaving_search();
        self.push_content_snapshot();
        self.content_view = TuiContentView::Playlists(TuiCollectionPageView {
            title: "Your Playlists".to_string(),
            subtitle: "Library playlists from YouTube Music · P: create playlist".to_string(),
            status: TuiCollectionPageStatus::Loading,
        });
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.status_line = "Loading playlists...".to_string();
        spawn_playlists_load(self.background_tx.clone());
    }

    pub(in crate::cli_tui) fn open_playlist_detail(&mut self, playlist: TuiPlaylistCard) {
        self.clear_completed_search_query_if_leaving_search();
        self.push_content_snapshot();
        let kind = match playlist.source {
            TuiPlaylistSource::Remote => TuiTrackPageKind::RemotePlaylist,
            TuiPlaylistSource::Offline => TuiTrackPageKind::OfflinePlaylist,
        };
        self.content_view = TuiContentView::PlaylistDetail(TuiTrackPageView {
            kind,
            key: playlist_detail_key(&playlist),
            title: playlist.summary.title.clone(),
            subtitle: playlist
                .summary
                .subtitle
                .clone()
                .unwrap_or_else(|| playlist_source_label(playlist.source).to_string()),
            thumbnail_url: playlist.summary.thumbnail_url.clone(),
            status: TuiTrackPageStatus::Loading,
        });
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.status_line = format!("Loading {}...", shorten(&playlist.summary.title, 32));
        spawn_playlist_detail_load(self.background_tx.clone(), playlist);
    }

    pub(in crate::cli_tui) fn open_offline(&mut self) {
        self.clear_completed_search_query_if_leaving_search();
        self.push_content_snapshot();
        self.content_view = TuiContentView::Offline(TuiOfflineView {
            status: TuiOfflineStatus::Loading,
        });
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.status_line = "Loading offline library...".to_string();
        spawn_offline_load(self.background_tx.clone());
    }

    pub(in crate::cli_tui) fn open_search(&mut self, query: String) {
        if !matches!(self.content_view, TuiContentView::Search(_)) {
            self.push_content_snapshot();
        }
        self.clear_search_suggestions();
        self.search_input_active = false;
        self.content_view = TuiContentView::Search(TuiSearchView {
            query: query.clone(),
            status: TuiSearchStatus::Loading,
        });
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.status_line = format!("Searching for {}...", shorten(&query, 28));
        spawn_search_load(self.background_tx.clone(), query);
    }

    pub(in crate::cli_tui) fn open_settings(&mut self) {
        self.clear_completed_search_query_if_leaving_search();
        self.push_content_snapshot();
        self.content_view = TuiContentView::Settings(TuiSettingsView {
            status: TuiSettingsStatus::Loading,
        });
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.status_line = "Loading settings...".to_string();
        spawn_settings_load(self.background_tx.clone());
    }

    pub(in crate::cli_tui) fn refresh_current_view(&mut self) {
        self.settings_text_edit = None;
        self.clear_search_suggestions();

        match self.content_view.clone() {
            TuiContentView::Home => {
                self.home_status = HomeFeedStatus::Loading;
                self.home_loading_more = false;
                self.home_selected = 0;
                self.home_scroll_offset = 0;
                self.status_line = "Refreshing Home...".to_string();
                spawn_home_bootstrap(self.background_tx.clone());
            }
            TuiContentView::Browse(view) => {
                let card = view.card;
                self.content_view = TuiContentView::Browse(TuiBrowseView {
                    card: card.clone(),
                    status: BrowseViewStatus::Loading,
                });
                self.home_selected = 0;
                self.home_scroll_offset = 0;
                self.status_line = format!("Refreshing {}...", shorten(&card.title, 32));
                spawn_browse_load(self.background_tx.clone(), card);
            }
            TuiContentView::Likes(view) => {
                self.content_view = TuiContentView::Likes(TuiTrackPageView {
                    status: TuiTrackPageStatus::Loading,
                    ..view
                });
                self.home_selected = 0;
                self.home_scroll_offset = 0;
                self.status_line = "Refreshing liked songs...".to_string();
                spawn_likes_load(self.background_tx.clone());
            }
            TuiContentView::Playlists(view) => {
                self.content_view = TuiContentView::Playlists(TuiCollectionPageView {
                    status: TuiCollectionPageStatus::Loading,
                    ..view
                });
                self.home_selected = 0;
                self.home_scroll_offset = 0;
                self.status_line = "Refreshing playlists...".to_string();
                spawn_playlists_load(self.background_tx.clone());
            }
            TuiContentView::PlaylistDetail(view) => {
                let source = match view.kind {
                    TuiTrackPageKind::RemotePlaylist => TuiPlaylistSource::Remote,
                    TuiTrackPageKind::OfflinePlaylist => TuiPlaylistSource::Offline,
                    TuiTrackPageKind::Likes => {
                        self.refresh_current_view_from_likes(view);
                        return;
                    }
                };
                let playlist = TuiPlaylistCard {
                    summary: PlaylistSummary {
                        playlist_id: match source {
                            TuiPlaylistSource::Remote => {
                                view.key.trim_start_matches("remote:").to_string()
                            }
                            TuiPlaylistSource::Offline => {
                                view.key.trim_start_matches("offline:").to_string()
                            }
                        },
                        title: view.title.clone(),
                        thumbnail_url: view.thumbnail_url.clone(),
                        subtitle: Some(view.subtitle.clone()),
                    },
                    source,
                };
                self.content_view = TuiContentView::PlaylistDetail(TuiTrackPageView {
                    status: TuiTrackPageStatus::Loading,
                    ..view
                });
                self.home_selected = 0;
                self.home_scroll_offset = 0;
                self.status_line =
                    format!("Refreshing {}...", shorten(&playlist.summary.title, 32));
                spawn_playlist_detail_load(self.background_tx.clone(), playlist);
            }
            TuiContentView::Offline(_) => {
                self.content_view = TuiContentView::Offline(TuiOfflineView {
                    status: TuiOfflineStatus::Loading,
                });
                self.home_selected = 0;
                self.home_scroll_offset = 0;
                self.status_line = "Refreshing offline library...".to_string();
                spawn_offline_load(self.background_tx.clone());
            }
            TuiContentView::Search(view) => {
                let query = view.query;
                self.content_view = TuiContentView::Search(TuiSearchView {
                    query: query.clone(),
                    status: TuiSearchStatus::Loading,
                });
                self.home_selected = 0;
                self.home_scroll_offset = 0;
                self.status_line = format!("Refreshing {}...", shorten(&query, 28));
                spawn_search_load(self.background_tx.clone(), query);
            }
            TuiContentView::Settings(_) => {
                self.content_view = TuiContentView::Settings(TuiSettingsView {
                    status: TuiSettingsStatus::Loading,
                });
                self.home_selected = 0;
                self.home_scroll_offset = 0;
                self.status_line = "Refreshing settings...".to_string();
                spawn_settings_load(self.background_tx.clone());
            }
        }
    }

    pub(in crate::cli_tui) fn refresh_current_view_from_likes(&mut self, view: TuiTrackPageView) {
        self.content_view = TuiContentView::Likes(TuiTrackPageView {
            status: TuiTrackPageStatus::Loading,
            ..view
        });
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.status_line = "Refreshing liked songs...".to_string();
        spawn_likes_load(self.background_tx.clone());
    }

    pub(in crate::cli_tui) fn go_back_content(&mut self) {
        self.clear_completed_search_query_if_leaving_search();
        if let Some(snapshot) = self.content_backstack.pop() {
            self.restore_content_snapshot(snapshot);
            self.status_line = "Returned to the previous page.".to_string();
        } else {
            self.content_view = TuiContentView::Home;
            self.home_selected = 0;
            self.home_scroll_offset = 0;
            self.status_line = "Back on Home.".to_string();
        }
    }

    pub(in crate::cli_tui) fn return_home_content(&mut self) {
        self.clear_completed_search_query_if_leaving_search();
        while let Some(snapshot) = self.content_backstack.pop() {
            if matches!(snapshot.view, TuiContentView::Home) {
                self.restore_content_snapshot(snapshot);
                self.status_line = "Back on Home.".to_string();
                return;
            }
        }

        self.content_view = TuiContentView::Home;
        self.home_selected = 0;
        self.home_scroll_offset = 0;
        self.status_line = "Back on Home.".to_string();
    }
}

pub(in crate::cli_tui) fn load_resume_card_data(
    queue: Option<CrossDeviceQueue>,
) -> Option<ResumeCardData> {
    let local_resume = match load_local_resume_state() {
        Ok(resume) => resume,
        Err(err) => {
            tracing::debug!("Local resume unavailable: {err}");
            None
        }
    };

    if should_use_local_resume_for_cross_device(queue.as_ref(), local_resume.as_ref()) {
        if let Some(resume) = local_resume {
            return local_resume_card_data(resume);
        }
    }

    if let Some(queue) = queue {
        return cross_device_resume_card_data(queue);
    }

    local_resume_card_data(local_resume?)
}

fn local_resume_card_data(resume: PersistedPlaybackState) -> Option<ResumeCardData> {
    let snapshot = resume.snapshot;
    let track = resume_snapshot_track(&snapshot)?;
    Some(ResumeCardData {
        meta: resume_snapshot_meta(&snapshot),
        source: ResumeCardSource::Local(snapshot),
        track,
    })
}

fn cross_device_resume_card_data(queue: CrossDeviceQueue) -> Option<ResumeCardData> {
    let track = queue.tracks.get(queue.selected_index)?.clone();
    let meta = queue.source_label.clone().unwrap_or_else(|| {
        if queue.playlist_id.is_some() {
            "From another device playlist".to_string()
        } else {
            "From another device".to_string()
        }
    });
    Some(ResumeCardData {
        source: ResumeCardSource::CrossDevice(queue),
        track,
        meta,
    })
}

pub(in crate::cli_tui) fn playlist_source_label(source: TuiPlaylistSource) -> &'static str {
    match source {
        TuiPlaylistSource::Remote => "PLAYLIST",
        TuiPlaylistSource::Offline => "OFFLINE",
    }
}

pub(in crate::cli_tui) fn playlist_detail_key(playlist: &TuiPlaylistCard) -> String {
    match playlist.source {
        TuiPlaylistSource::Remote => format!("remote:{}", playlist.summary.playlist_id),
        TuiPlaylistSource::Offline => format!("offline:{}", playlist.summary.playlist_id),
    }
}

pub(in crate::cli_tui) fn track_count_label(count: usize) -> String {
    if count == 1 {
        "1 track".to_string()
    } else {
        format!("{count} tracks")
    }
}

pub(in crate::cli_tui) fn search_results_total(results: &SearchResult) -> usize {
    results
        .sections
        .iter()
        .map(|section| section.contents.len())
        .sum()
}

pub(in crate::cli_tui) fn search_results_summary(results: &SearchResult) -> String {
    let mut tracks = 0usize;
    let mut playlists = 0usize;
    let mut artists = 0usize;
    let mut albums = 0usize;
    let mut shows = 0usize;
    let mut channels = 0usize;

    for section in &results.sections {
        for item in &section.contents {
            match item {
                HomeItem::Track(_) => tracks += 1,
                HomeItem::BrowseCard(card) => match card.page_type {
                    HomeBrowsePageType::Playlist => playlists += 1,
                    HomeBrowsePageType::Album => albums += 1,
                    HomeBrowsePageType::Artist => artists += 1,
                    HomeBrowsePageType::UserChannel => channels += 1,
                    HomeBrowsePageType::PodcastShow => shows += 1,
                },
            }
        }
    }

    let mut parts = Vec::new();
    if tracks > 0 {
        parts.push(format!("{tracks} tracks"));
    }
    if playlists > 0 {
        parts.push(format!("{playlists} playlists"));
    }
    if artists > 0 {
        parts.push(format!("{artists} artists"));
    }
    if albums > 0 {
        parts.push(format!("{albums} albums"));
    }
    if channels > 0 {
        parts.push(format!("{channels} channels"));
    }
    if shows > 0 {
        parts.push(format!("{shows} shows"));
    }

    if parts.is_empty() {
        "No results".to_string()
    } else {
        parts.join("  •  ")
    }
}

pub(in crate::cli_tui) fn search_section_tracks(
    results: &SearchResult,
    section_index: usize,
) -> Vec<Track> {
    results
        .sections
        .get(section_index)
        .map(home_section_tracks)
        .unwrap_or_default()
}
