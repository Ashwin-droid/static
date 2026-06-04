use super::*;
use super::{page_builders::*, views::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn play_content_section(
        &mut self,
        section_index: usize,
        shuffle: bool,
    ) -> Result<()> {
        let (section_title, mut tracks, context) = match &self.content_view {
            TuiContentView::Home => {
                let HomeFeedStatus::Ready(page) = &self.home_status else {
                    self.status_line = "Home feed is still loading.".to_string();
                    return Ok(());
                };
                let Some(section) = page.sections.get(section_index) else {
                    self.status_line = "That Home section is no longer available.".to_string();
                    return Ok(());
                };
                (
                    section.title.clone(),
                    home_section_tracks(section),
                    None::<PlaylistContext>,
                )
            }
            TuiContentView::Browse(view) => match &view.status {
                BrowseViewStatus::Loading => {
                    self.status_line = "This page is still loading.".to_string();
                    return Ok(());
                }
                BrowseViewStatus::Error(message) => {
                    self.status_line = format!("This page failed to load: {message}");
                    return Ok(());
                }
                BrowseViewStatus::Tracks { tracks, .. } => (
                    view.card.title.clone(),
                    tracks.clone(),
                    Some(browse_playlist_context(&view.card, tracks.len())),
                ),
                BrowseViewStatus::Sections { sections, .. } => {
                    let Some(section) = sections.get(section_index) else {
                        self.status_line = "That section is no longer available.".to_string();
                        return Ok(());
                    };
                    (section.title.clone(), home_section_tracks(section), None)
                }
            },
            TuiContentView::Likes(view) | TuiContentView::PlaylistDetail(view) => {
                let TuiTrackPageStatus::Ready { tracks, context } = &view.status else {
                    self.status_line = "This page is still loading.".to_string();
                    return Ok(());
                };
                (view.title.clone(), tracks.clone(), context.clone())
            }
            TuiContentView::Offline(view) => {
                let TuiOfflineStatus::Ready { tracks, .. } = &view.status else {
                    self.status_line = "Offline is still loading.".to_string();
                    return Ok(());
                };
                if section_index != 1 {
                    self.status_line =
                        "That section opens collections instead of playing all.".to_string();
                    return Ok(());
                }
                ("Offline songs".to_string(), tracks.clone(), None)
            }
            TuiContentView::Search(view) => {
                let TuiSearchStatus::Ready(results) = &view.status else {
                    self.status_line = "Search is still loading.".to_string();
                    return Ok(());
                };
                let Some(section) = results.sections.get(section_index) else {
                    self.status_line = "That search section is no longer available.".to_string();
                    return Ok(());
                };
                (
                    section.title.clone(),
                    search_section_tracks(results, section_index),
                    None,
                )
            }
            TuiContentView::Playlists(_) | TuiContentView::Settings(_) => {
                self.status_line = "This page does not have a play-all section.".to_string();
                return Ok(());
            }
        };

        if tracks.is_empty() {
            self.status_line = format!("{} has no playable tracks.", section_title);
            return Ok(());
        }

        if shuffle {
            use rand::seq::SliceRandom;

            let mut rng = rand::rng();
            tracks.shuffle(&mut rng);
        }

        let snapshot = self
            .client
            .play_queue_at(tracks, 0, context, None, RepeatMode::Off)?;
        self.apply_snapshot(snapshot);
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        self.status_line = if shuffle {
            format!("Shuffled {}", shorten(&section_title, 28))
        } else {
            format!("Playing all from {}", shorten(&section_title, 28))
        };
        Ok(())
    }

    pub(in crate::cli_tui) fn activate_selection(&mut self) -> Result<()> {
        match self.focus {
            TuiFocus::Home => self.activate_content_selection(),
            TuiFocus::Queue => self.activate_queue_selection(),
            TuiFocus::Recent => self.activate_recent_selection(),
            TuiFocus::Player => {
                let action = player_actions(&self.snapshot)
                    .get(self.player_selected)
                    .copied()
                    .unwrap_or(TuiAction::TogglePause);
                self.run_action(action)
            }
        }
    }

    pub(in crate::cli_tui) fn activate_click_target(
        &mut self,
        kind: &ClickTargetKind,
    ) -> Result<()> {
        match kind {
            ClickTargetKind::Action(action) => self.run_action(*action),
            ClickTargetKind::Nav(nav) => self.handle_nav(*nav),
            ClickTargetKind::SearchBox => {
                self.activate_search_input();
                Ok(())
            }
            ClickTargetKind::SearchSuggestion(index) => {
                self.search_suggestion_selected = *index;
                let preferred = self.search_suggestions.get(*index).cloned();
                self.submit_search(preferred);
                Ok(())
            }
            ClickTargetKind::ContentBack => {
                self.go_back_content();
                Ok(())
            }
            ClickTargetKind::PlayerBar => {
                self.focus = TuiFocus::Player;
                Ok(())
            }
            ClickTargetKind::DetailTab(tab) => {
                self.detail_tab = *tab;
                Ok(())
            }
            ClickTargetKind::RadioChipScroll(delta) => {
                self.scroll_radio_chips(*delta);
                Ok(())
            }
            ClickTargetKind::RadioChip(index) => self.activate_radio_chip(*index),
            ClickTargetKind::HomeSectionAction {
                section_index,
                shuffle,
            } => self.play_content_section(*section_index, *shuffle),
            ClickTargetKind::HomeItem(index) => {
                self.focus = TuiFocus::Home;
                self.home_selected = *index;
                self.ensure_home_visible();
                self.activate_content_selection()
            }
            ClickTargetKind::SettingsField(index) => {
                self.focus = TuiFocus::Home;
                self.home_selected = *index;
                self.ensure_home_visible();
                Ok(())
            }
            ClickTargetKind::SettingsToggle(key) => self.toggle_boolean_setting(key),
            ClickTargetKind::SettingsCycle(key, delta) => {
                self.cycle_coordination_setting_delta(key, *delta)
            }
            ClickTargetKind::SettingsAdjust(key, delta) => self.adjust_numeric_setting(key, *delta),
            ClickTargetKind::SettingsEdit(target) => {
                self.start_settings_text_edit(target.clone());
                Ok(())
            }
            ClickTargetKind::SettingsStorageRootDefault => self.reset_storage_root_override(),
            ClickTargetKind::SettingsSponsorCategory(key) => self.toggle_sponsorblock_category(key),
            ClickTargetKind::QueueItem(index) => {
                self.focus = TuiFocus::Queue;
                self.queue_selected = *index;
                self.ensure_queue_visible();
                self.activate_queue_selection()
            }
            ClickTargetKind::RecentItem(index) => {
                self.focus = TuiFocus::Recent;
                self.recent_selected = *index;
                self.ensure_recent_visible();
                self.activate_recent_selection()
            }
        }
    }

    pub(in crate::cli_tui) fn handle_nav(&mut self, nav: TuiNavItem) -> Result<()> {
        self.settings_text_edit = None;
        match nav {
            TuiNavItem::Home => {
                self.focus = TuiFocus::Home;
                if matches!(&self.content_view, TuiContentView::Home) {
                    self.status_line = "Home is already cached locally.".to_string();
                } else {
                    self.return_home_content();
                }
            }
            TuiNavItem::Likes => {
                if !matches!(self.content_view, TuiContentView::Likes(_)) {
                    self.open_likes();
                }
            }
            TuiNavItem::Playlists => {
                if !matches!(self.content_view, TuiContentView::Playlists(_)) {
                    self.open_playlists();
                }
            }
            TuiNavItem::Offline => {
                if !matches!(self.content_view, TuiContentView::Offline(_)) {
                    self.open_offline();
                }
            }
            TuiNavItem::Settings => {
                if !matches!(self.content_view, TuiContentView::Settings(_)) {
                    self.open_settings();
                }
            }
        }
        Ok(())
    }

    pub(in crate::cli_tui) fn activate_content_selection(&mut self) -> Result<()> {
        if matches!(self.content_view, TuiContentView::Settings(_)) {
            return self.activate_selected_setting();
        }

        let Some(card) = self.home_doc.cards.get(self.home_selected).cloned() else {
            self.status_line = match &self.content_view {
                TuiContentView::Home => "Home feed is still loading.".to_string(),
                TuiContentView::Browse(view) => match &view.status {
                    BrowseViewStatus::Loading => "This page is still loading.".to_string(),
                    BrowseViewStatus::Error(message) => {
                        format!("This page failed to load: {message}")
                    }
                    BrowseViewStatus::Tracks { .. } | BrowseViewStatus::Sections { .. } => {
                        "Nothing is selectable on this page yet.".to_string()
                    }
                },
                TuiContentView::Likes(_)
                | TuiContentView::PlaylistDetail(_)
                | TuiContentView::Playlists(_)
                | TuiContentView::Offline(_)
                | TuiContentView::Search(_)
                | TuiContentView::Settings(_) => {
                    "Nothing is selectable on this page yet.".to_string()
                }
            };
            return Ok(());
        };

        match card.item {
            TuiHomeItem::Browse(card) => {
                self.open_browse_detail(card);
                return Ok(());
            }
            TuiHomeItem::Playlist(card) => {
                self.open_playlist_detail(card);
                return Ok(());
            }
            TuiHomeItem::Track(track)
                if matches!(&self.content_view, TuiContentView::Home)
                    || matches!(
                        &self.content_view,
                        TuiContentView::Browse(TuiBrowseView {
                            status: BrowseViewStatus::Sections { .. },
                            ..
                        })
                    ) =>
            {
                let snapshot = play_track_with_radio(&self.client, track.clone())?;
                self.apply_snapshot(snapshot);
                self.detail_tab = DetailTab::Queue;
                self.status_line = format!("Playing {}", format_track_summary(&track));
            }
            TuiHomeItem::Track(track) => match &self.content_view {
                TuiContentView::Browse(view) => {
                    let BrowseViewStatus::Tracks { tracks, .. } = &view.status else {
                        return Ok(());
                    };
                    self.play_track_collection(
                        tracks.clone(),
                        tracks
                            .iter()
                            .position(|candidate| candidate.video_id == track.video_id)
                            .unwrap_or(0),
                        Some(browse_playlist_context(&view.card, tracks.len())),
                        &track,
                    )?;
                }
                TuiContentView::Likes(view) | TuiContentView::PlaylistDetail(view) => {
                    let TuiTrackPageStatus::Ready { tracks, context } = &view.status else {
                        return Ok(());
                    };
                    self.play_track_collection(
                        tracks.clone(),
                        tracks
                            .iter()
                            .position(|candidate| candidate.video_id == track.video_id)
                            .unwrap_or(0),
                        context.clone(),
                        &track,
                    )?;
                }
                TuiContentView::Offline(view) => {
                    let TuiOfflineStatus::Ready { tracks, .. } = &view.status else {
                        return Ok(());
                    };
                    self.play_track_collection(
                        tracks.clone(),
                        tracks
                            .iter()
                            .position(|candidate| candidate.video_id == track.video_id)
                            .unwrap_or(0),
                        None,
                        &track,
                    )?;
                }
                TuiContentView::Search(view) => {
                    let TuiSearchStatus::Ready(results) = &view.status else {
                        return Ok(());
                    };
                    let section_tracks = search_section_tracks(results, card.section_index);
                    self.play_track_collection(
                        section_tracks.clone(),
                        section_tracks
                            .iter()
                            .position(|candidate| candidate.video_id == track.video_id)
                            .unwrap_or(0),
                        None,
                        &track,
                    )?;
                }
                TuiContentView::Home
                | TuiContentView::Playlists(_)
                | TuiContentView::Settings(_) => return Ok(()),
            },
        }

        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        Ok(())
    }

    pub(in crate::cli_tui) fn activate_queue_selection(&mut self) -> Result<()> {
        if self.snapshot.queue.is_empty() {
            self.status_line = "Queue is empty.".to_string();
            return Ok(());
        }

        let index = self
            .queue_selected
            .min(self.snapshot.queue.len().saturating_sub(1));
        let snapshot = self.client.play_queue_at(
            self.snapshot.queue.clone(),
            index,
            self.snapshot.playlist_context.clone(),
            self.snapshot.radio_virtual_playlist.clone(),
            self.snapshot.repeat_mode,
        )?;
        self.apply_snapshot(snapshot);
        self.status_line = format!(
            "Playing {}",
            self.snapshot
                .queue
                .get(index)
                .map(format_track_summary)
                .unwrap_or_else(|| "selection".to_string())
        );
        Ok(())
    }

    pub(in crate::cli_tui) fn activate_recent_selection(&mut self) -> Result<()> {
        let Some(track) = self.recent_tracks.get(self.recent_selected).cloned() else {
            self.status_line = "No recent history yet.".to_string();
            return Ok(());
        };

        let snapshot = play_track_with_radio(&self.client, track.clone())?;
        self.apply_snapshot(snapshot);
        self.detail_tab = DetailTab::Queue;
        self.queue_selected = self
            .snapshot
            .queue_index
            .min(self.snapshot.queue.len().saturating_sub(1));
        self.status_line = format!("Playing {}", format_track_summary(&track));
        Ok(())
    }

    pub(in crate::cli_tui) fn play_track_collection(
        &mut self,
        tracks: Vec<Track>,
        start_index: usize,
        context: Option<PlaylistContext>,
        track: &Track,
    ) -> Result<()> {
        if tracks.is_empty() {
            self.status_line = "Nothing playable is in this section yet.".to_string();
            return Ok(());
        }
        let snapshot =
            self.client
                .play_queue_at(tracks, start_index, context, None, RepeatMode::Off)?;
        self.apply_snapshot(snapshot);
        self.detail_tab = DetailTab::Queue;
        self.status_line = format!("Playing {}", format_track_summary(track));
        Ok(())
    }
}
