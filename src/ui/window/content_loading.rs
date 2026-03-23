use super::*;

pub(super) fn load_home(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
    force_refresh: bool,
) {
    if force_refresh {
        store_home_scroll_position(state, 0.0);
        content_scroll.vadjustment().set_value(0.0);
    }

    if !force_refresh {
        if let Some(page) = cached_home_page(state) {
            clear_box(content_box);
            content_box.add_css_class("home-view-active");
            home_pagination
                .continuation
                .replace(page.continuation.clone());
            home_pagination.loading.set(false);
            render_home_view(
                content_box,
                &page.sections,
                state,
                content_scroll,
                home_pagination,
                ps,
                recent_list,
                false,
            );
            restore_home_scroll_position_later(state, content_scroll);
            queue_home_lazy_load_check(
                state,
                content_box,
                content_scroll,
                home_pagination,
                ps,
                recent_list,
            );
            return;
        }
    }

    clear_box(content_box);
    content_box.add_css_class("home-view-active");
    home_pagination.continuation.replace(None);
    home_pagination.loading.set(true);
    let loading = Label::new(Some("Loading recommendations..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let home_pagination = home_pagination.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.get_home_page(None)))
                .await;

        home_pagination.loading.set(false);
        clear_box(&content_box);
        content_box.add_css_class("home-view-active");

        match result {
            Ok(Some(Ok(page))) => {
                store_home_cache(&state, &page);
                home_pagination
                    .continuation
                    .replace(page.continuation.clone());
                let listen_again_tracks = extract_listen_again_tracks(&page.sections);
                let state_bg = state.clone();
                let _ = gio::spawn_blocking(move || {
                    let guard = state_bg.lock().unwrap();
                    let listen_again_ids = listen_again_tracks
                        .iter()
                        .map(|track| track.video_id.clone())
                        .collect::<Vec<_>>();
                    for track in &listen_again_tracks {
                        let _ = guard.cache.upsert_track_metadata(
                            &track.video_id,
                            &track.title,
                            &artist_string(track),
                            history_thumbnail_candidate(track.thumbnail_url.as_deref()),
                        );
                    }
                    guard.cache.store_listen_again_snapshot(&listen_again_ids)
                })
                .await;
                render_home_view(
                    &content_box,
                    &page.sections,
                    &state,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                    false,
                );
                if force_refresh {
                    content_scroll.vadjustment().set_value(0.0);
                } else {
                    restore_home_scroll_position_later(&state, &content_scroll);
                }
                queue_home_lazy_load_check(
                    &state,
                    &content_box,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                );
            }
            Ok(Some(Err(e))) => {
                home_pagination.continuation.replace(None);
                content_box.append(&Label::new(Some(&format!("Error: {e}"))));
            }
            Ok(None) => {
                home_pagination.continuation.replace(None);
                content_box.append(&Label::new(Some("Not connected.")));
            }
            Err(e) => {
                home_pagination.continuation.replace(None);
                content_box.append(&Label::new(Some(&format!("Error: {e:?}"))));
            }
        }
    });
}

pub(super) fn restore_home_scroll_position_later(
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
) {
    let state = state.clone();
    let content_scroll = content_scroll.clone();
    glib::idle_add_local_once(move || {
        let adjustment = content_scroll.vadjustment();
        let max_value = (adjustment.upper() - adjustment.page_size()).max(0.0);
        let target = cached_home_scroll_position(&state).min(max_value);
        adjustment.set_value(target);
    });
}

pub(super) fn queue_home_lazy_load_check(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let home_pagination = home_pagination.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::idle_add_local_once(move || {
        maybe_load_more_home_if_needed(
            &state,
            &content_box,
            &content_scroll,
            &home_pagination,
            &ps,
            &recent_list,
        );
    });
}

pub(super) fn apply_vertical_scroll_delta(scroll: &ScrolledWindow, dy: f64) -> bool {
    if dy.abs() <= f64::EPSILON {
        return false;
    }

    let adjustment = scroll.vadjustment();
    let lower = adjustment.lower();
    let upper = (adjustment.upper() - adjustment.page_size()).max(lower);
    if upper <= lower {
        return false;
    }

    let delta = if dy.abs() <= 4.0 { dy * 48.0 } else { dy };
    let current = adjustment.value();
    let target = (adjustment.value() + delta).clamp(lower, upper);
    if (target - current).abs() <= f64::EPSILON {
        return false;
    }

    adjustment.set_value(target);
    true
}

pub(super) fn install_vertical_scroll_capture<W: IsA<gtk::Widget>>(
    widget: &W,
    scroll: &ScrolledWindow,
    _source: &'static str,
) {
    let scroll = scroll.clone();
    let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_propagation_limit(gtk::PropagationLimit::None);
    controller.connect_scroll(move |controller, dx, dy| {
        if dy.abs() <= f64::EPSILON || dy.abs() <= dx.abs() {
            return glib::Propagation::Proceed;
        }

        if controller
            .current_event_state()
            .contains(gtk::gdk::ModifierType::SHIFT_MASK)
        {
            return glib::Propagation::Proceed;
        }

        if apply_vertical_scroll_delta(&scroll, dy) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    widget.as_ref().add_controller(controller);
}

pub(super) fn build_horizontal_cards_scroll(parent_scroll: &ScrolledWindow) -> ScrolledWindow {
    let cards_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .build();
    cards_scroll.add_css_class("cards-scroll");
    install_vertical_scroll_passthrough(&cards_scroll, parent_scroll, "horizontal-cards");
    cards_scroll
}

pub(super) fn install_vertical_scroll_passthrough(
    child_scroll: &ScrolledWindow,
    parent_scroll: &ScrolledWindow,
    _source: &'static str,
) {
    let parent_scroll = parent_scroll.clone();
    let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.set_propagation_limit(gtk::PropagationLimit::None);
    controller.connect_scroll(move |controller, dx, dy| {
        if dy.abs() <= f64::EPSILON || dy.abs() <= dx.abs() {
            return glib::Propagation::Proceed;
        }

        if controller
            .current_event_state()
            .contains(gtk::gdk::ModifierType::SHIFT_MASK)
        {
            return glib::Propagation::Proceed;
        }

        if apply_vertical_scroll_delta(&parent_scroll, dy) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    child_scroll.add_controller(controller);
}

pub(super) fn maybe_load_more_home_if_needed(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    if !content_box.has_css_class("home-view-active") || home_pagination.loading.get() {
        return;
    }

    if home_pagination.continuation.borrow().is_none() {
        return;
    }

    let adjustment = content_scroll.vadjustment();
    let remaining = adjustment.upper() - (adjustment.value() + adjustment.page_size());
    if remaining > 720.0 {
        return;
    }

    load_more_home(
        state,
        content_box,
        content_scroll,
        home_pagination,
        ps,
        recent_list,
    );
}

pub(super) fn load_more_home(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let Some(continuation) = home_pagination.continuation.borrow().clone() else {
        return;
    };
    home_pagination.loading.set(true);

    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let home_pagination = home_pagination.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let token = continuation.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_home_page(Some(&token)))
        })
        .await;

        home_pagination.loading.set(false);

        let Ok(Some(Ok(page))) = result else {
            return;
        };

        if !content_box.has_css_class("home-view-active") {
            return;
        }

        let next_continuation = page
            .continuation
            .clone()
            .filter(|token| token != &continuation);
        home_pagination
            .continuation
            .replace(next_continuation.clone());

        {
            let mut guard = state.lock().unwrap();
            if let Some(cached) = guard.home_cache.as_mut() {
                cached.sections.extend(page.sections.clone());
                cached.continuation = next_continuation.clone();
            } else {
                guard.home_cache = Some(HomePage {
                    sections: page.sections.clone(),
                    continuation: next_continuation.clone(),
                });
            }
        }

        render_home_view(
            &content_box,
            &page.sections,
            &state,
            &content_scroll,
            &home_pagination,
            &ps,
            &recent_list,
            true,
        );
        queue_home_lazy_load_check(
            &state,
            &content_box,
            &content_scroll,
            &home_pagination,
            &ps,
            &recent_list,
        );
    });
}

pub(super) fn load_search(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    query: &str,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some(&format!("Searching \"{query}\"...")));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let home_pagination = home_pagination.clone();
    let query = query.to_string();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let q = query.clone();
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.search(&q))).await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(results))) => {
                render_search_view(
                    &content_box,
                    &query,
                    &results,
                    &state,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                );
            }
            Ok(Some(Err(e))) => {
                content_box.append(&Label::new(Some(&format!("Search error: {e}"))));
            }
            Ok(None) => {
                content_box.append(&Label::new(Some("Not connected.")));
            }
            Err(e) => {
                content_box.append(&Label::new(Some(&format!("Error: {e:?}"))));
            }
        }
    });
}

pub(super) fn parse_direct_music_url(input: &str) -> Option<DirectMusicUrlTarget> {
    let trimmed = input.trim();

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

pub(super) fn parse_direct_music_reference(input: &str) -> Option<DirectMusicUrlTarget> {
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

pub(super) fn normalize_playlist_browse_id(list_id: &str) -> String {
    if list_id.starts_with("VL") {
        list_id.to_string()
    } else {
        format!("VL{list_id}")
    }
}

pub(super) fn infer_browse_page_type(browse_id: &str) -> HomeBrowsePageType {
    if browse_id.starts_with("MPRE") {
        HomeBrowsePageType::Album
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

pub(super) fn autoplay_direct_music_url(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    raw_url: &str,
    target: DirectMusicUrlTarget,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    match target {
        DirectMusicUrlTarget::Video { video_id } => {
            autoplay_video_url(state, raw_url, &video_id, ps, recent_list)
        }
        DirectMusicUrlTarget::Browse {
            browse_id,
            page_type,
        } => load_browse_detail(
            state,
            content_box,
            content_scroll,
            home_pagination,
            &browse_id,
            raw_url,
            None,
            page_type,
            ps,
            recent_list,
            true,
        ),
    }
}

pub(super) fn autoplay_video_url(
    state: &Arc<Mutex<AppState>>,
    raw_url: &str,
    video_id: &str,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let state = state.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();
    let video_id = video_id.to_string();
    let fallback_title = raw_url.to_string();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let vid = video_id.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, None, None))
        })
        .await;

        let fallback_track = Track {
            video_id: video_id.clone(),
            title: fallback_title.clone(),
            artists: vec![Artist {
                name: "Unknown artist".to_string(),
                id: None,
            }],
            album: None,
            duration: None,
            thumbnail_url: None,
        };

        let track = match result {
            Ok(Some(Ok(radio))) => {
                let tracks = radio.tracks;
                tracks
                    .iter()
                    .find(|track| track.video_id == video_id)
                    .cloned()
                    .or_else(|| tracks.into_iter().next())
                    .unwrap_or(fallback_track)
            }
            _ => fallback_track,
        };

        play_track(
            &state,
            &track.video_id,
            &track.title,
            &artist_string(&track),
            track.thumbnail_url.as_deref(),
            &ps,
        );
        add_to_recent(&recent_list, &track, &state, &ps);
    });
}

pub(super) fn load_offline_page_data(
    state: &Arc<Mutex<AppState>>,
) -> (Vec<PlaylistSummary>, Vec<Track>, Vec<Track>) {
    repair_offline_track_metadata(state);

    let guard = state.lock().unwrap();

    let mut offline_playlists: Vec<PlaylistSummary> = guard
        .cache
        .offline_playlists()
        .unwrap_or_default()
        .into_iter()
        .map(|(playlist_id, title, thumbnail_url, subtitle)| {
            let thumbnail_url = thumbnail_url.or_else(|| {
                guard
                    .cache
                    .offline_playlist_track_ids(&playlist_id)
                    .ok()
                    .and_then(|ids| ids.first().cloned())
                    .and_then(|vid| guard.cache.get_cached_art(&vid))
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

    let listen_again_track_ids = guard
        .cache
        .offline_listen_again_track_ids()
        .unwrap_or_default();
    if let Some(first_video_id) = listen_again_track_ids.first() {
        offline_playlists.insert(
            0,
            PlaylistSummary {
                playlist_id: OFFLINE_LISTEN_AGAIN_PLAYLIST_ID.to_string(),
                title: "Listen again".to_string(),
                thumbnail_url: offline_thumbnail_url(&guard.cache, first_video_id),
                subtitle: Some("Auto-kept from Listen again".to_string()),
            },
        );
    }

    let all_tracks: Vec<Track> = guard
        .cache
        .offline_tracks()
        .unwrap_or_default()
        .into_iter()
        .map(|(video_id, title, artist, _art)| {
            let artists = if artist.is_empty() {
                vec![]
            } else {
                vec![Artist {
                    name: artist,
                    id: None,
                }]
            };
            let thumbnail_url = offline_thumbnail_url(&guard.cache, &video_id);

            Track {
                video_id,
                title,
                artists,
                album: None,
                duration: None,
                thumbnail_url,
            }
        })
        .collect();

    let playlist_track_ids: HashSet<String> = offline_playlists
        .iter()
        .flat_map(|playlist| {
            if playlist.playlist_id == OFFLINE_LISTEN_AGAIN_PLAYLIST_ID {
                listen_again_track_ids.clone()
            } else {
                guard
                    .cache
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

    (offline_playlists, display_tracks, all_tracks)
}

pub(super) fn repair_offline_track_metadata(state: &Arc<Mutex<AppState>>) {
    let repair_ids = {
        let guard = state.lock().unwrap();
        guard
            .cache
            .offline_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(video_id, title, artist, _art)| {
                let thumbnail_url = offline_thumbnail_url(&guard.cache, &video_id);
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

    let Some(client) = app_client(state) else {
        return;
    };

    for video_id in repair_ids {
        let mut radio = match client.get_radio(&video_id, None, None) {
            Ok(radio) => radio,
            Err(err) => {
                eprintln!("[OFFLINE] failed to repair metadata for {video_id}: {err}");
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

        let guard = state.lock().unwrap();
        let _ = guard.cache.upsert_track_metadata(
            &track.video_id,
            &track.title,
            &artist_string(&track),
            history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        );
    }
}

pub(super) fn load_offline_playlist_tracks(
    state: &Arc<Mutex<AppState>>,
    playlist_id: &str,
) -> Vec<Track> {
    let guard = state.lock().unwrap();
    let video_ids = if playlist_id == OFFLINE_LISTEN_AGAIN_PLAYLIST_ID {
        guard
            .cache
            .offline_listen_again_track_ids()
            .unwrap_or_default()
    } else {
        guard
            .cache
            .offline_playlist_track_ids(playlist_id)
            .unwrap_or_default()
    };

    video_ids
        .into_iter()
        .map(|video_id| {
            let thumbnail_url = offline_thumbnail_url(&guard.cache, &video_id);
            let (title, artist) = guard
                .cache
                .get_track_meta(&video_id)
                .unwrap_or_else(|| (video_id.clone(), String::new()));
            let artists = if artist.is_empty() {
                vec![]
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
            }
        })
        .collect()
}

pub(super) fn offline_thumbnail_url(cache: &MusicCache, video_id: &str) -> Option<String> {
    cache
        .get_cached_art(video_id)
        .map(|path| format!("file://{}", path.display()))
        .or_else(|| cache.get_thumbnail_url(video_id))
}

pub(super) fn load_offline(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some("Loading offline library..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || load_offline_page_data(&state_bg)).await;

        clear_box(&content_box);

        match result {
            Ok((offline_playlists, tracks, _playable_tracks)) => {
                if tracks.is_empty() && offline_playlists.is_empty() {
                    let empty = Label::new(Some(
                        "No offline songs yet. Play songs to build your cache.",
                    ));
                    empty.add_css_class("dim-label");
                    empty.set_margin_top(60);
                    content_box.append(&empty);
                    return;
                }

                let header = Label::new(Some("Offline"));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(20);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if !offline_playlists.is_empty() {
                    let pl_header = Label::new(Some("Offline Collections"));
                    pl_header.add_css_class("section-header");
                    pl_header.set_halign(Align::Start);
                    pl_header.set_margin_start(28);
                    pl_header.set_margin_top(8);
                    pl_header.set_margin_bottom(4);
                    content_box.append(&pl_header);

                    let cards_scroll = build_horizontal_cards_scroll(&content_scroll);

                    let cards_box = GtkBox::new(Orientation::Horizontal, 14);
                    cards_box.set_margin_top(4);
                    cards_box.set_margin_bottom(12);
                    cards_box.set_margin_start(24);

                    for playlist in &offline_playlists {
                        let card = build_offline_playlist_card(
                            playlist,
                            &state,
                            &content_box,
                            &content_scroll,
                            &ps,
                            &recent_list,
                        );
                        cards_box.append(&card);
                    }

                    cards_scroll.set_child(Some(&cards_box));
                    content_box.append(&cards_scroll);
                    content_box.append(&Separator::new(Orientation::Horizontal));
                }

                if !tracks.is_empty() {
                    let count_label =
                        Label::new(Some(&format!("{} songs available offline", tracks.len())));
                    count_label.add_css_class("dim-label");
                    count_label.set_halign(Align::Start);
                    count_label.set_margin_start(28);
                    count_label.set_margin_top(8);
                    count_label.set_margin_bottom(4);
                    content_box.append(&count_label);

                    content_box.append(&build_play_controls(&tracks, &state, &ps, None));

                    let grid = GtkBox::new(Orientation::Vertical, 6);
                    grid.set_margin_start(20);
                    grid.set_margin_end(20);

                    let tracks_rc = Rc::new(tracks);
                    for (i, track) in tracks_rc.iter().enumerate() {
                        grid.append(&build_track_row(
                            track,
                            i,
                            &tracks_rc,
                            &state,
                            &ps,
                            &recent_list,
                            TrackRowClickBehavior::PlayFromContext,
                        ));
                    }

                    content_box.append(&grid);
                }
            }
            Err(e) => {
                content_box.append(&Label::new(Some(&format!("Offline load failed: {e:?}"))));
            }
        }
    });
}
