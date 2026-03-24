use super::*;

pub(super) fn load_likes(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some("Loading liked songs..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.get_playlist("LM")))
                .await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(tracks))) => {
                let header = Label::new(Some("Liked Songs"));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(20);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if tracks.is_empty() {
                    let msg = Label::new(Some("No liked songs yet."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                let count_label = Label::new(Some(&format!("{} liked songs", tracks.len())));
                count_label.add_css_class("dim-label");
                count_label.set_halign(Align::Start);
                count_label.set_margin_start(28);
                count_label.set_margin_bottom(4);
                content_box.append(&count_label);

                content_box.append(&build_play_controls(
                    &tracks,
                    &state,
                    &ps,
                    Some(DownloadPlaylistMeta {
                        playlist_id: "LM".to_string(),
                        title: "Liked Songs".to_string(),
                        thumbnail_url: None,
                        subtitle: None,
                    }),
                ));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                let tracks_rc = Rc::new(tracks);
                for (i, track) in tracks_rc.iter().enumerate() {
                    let row = build_track_row(
                        track,
                        i,
                        &tracks_rc,
                        &state,
                        &ps,
                        &recent_list,
                        TrackRowClickBehavior::PlayFromContext,
                    );
                    grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
                }

                content_box.append(&grid);
            }
            Ok(Some(Err(e))) => {
                content_box.append(&Label::new(Some(&format!("Error: {e}"))));
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

pub(super) fn load_playlists(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some("Loading playlists..."));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_library_playlists())
        })
        .await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(playlists))) => {
                let header = Label::new(Some("Your Playlists"));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(20);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if playlists.is_empty() {
                    let msg = Label::new(Some("No playlists found."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                let flow = gtk::FlowBox::new();
                flow.set_orientation(Orientation::Horizontal);
                flow.set_homogeneous(false);
                flow.set_min_children_per_line(1);
                flow.set_max_children_per_line(12);
                flow.set_column_spacing(14);
                flow.set_row_spacing(14);
                flow.set_selection_mode(gtk::SelectionMode::None);
                flow.set_halign(Align::Fill);
                flow.set_valign(Align::Start);
                flow.set_hexpand(true);
                flow.set_margin_start(24);
                flow.set_margin_end(24);
                flow.set_margin_top(8);

                for playlist in &playlists {
                    let card =
                        build_playlist_card(playlist, &state, &content_box, &ps, &recent_list);
                    flow.insert(&card, -1);
                }

                content_box.append(&flow);
            }
            Ok(Some(Err(e))) => {
                content_box.append(&Label::new(Some(&format!("Error: {e}"))));
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

pub(super) fn build_home_item_card(
    item: &HomeItem,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> Button {
    match item {
        HomeItem::Track(track) => build_cover_card(track, state, ps, recent_list),
        HomeItem::BrowseCard(card) => build_home_browse_card(
            card,
            state,
            content_box,
            content_scroll,
            home_pagination,
            ps,
            recent_list,
        ),
    }
}

pub(super) fn build_home_browse_card(
    card: &HomeBrowseCard,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> Button {
    let button = build_browse_summary_card(
        &card.title,
        card.subtitle.as_deref(),
        card.thumbnail_url.as_deref(),
    );

    if let Some(payload) = queue_drag_payload_for_browse_card(card) {
        attach_queue_drag_source(&button, payload, ps);
    }

    {
        let state = state.clone();
        let content_box = content_box.clone();
        let content_scroll = content_scroll.clone();
        let home_pagination = home_pagination.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let browse_id = card.browse_id.clone();
        let title = card.title.clone();
        let thumb = card.thumbnail_url.clone();
        let page_type = card.page_type;

        button.connect_clicked(move |_| {
            load_browse_detail(
                &state,
                &content_box,
                &content_scroll,
                &home_pagination,
                &browse_id,
                &title,
                thumb.as_deref(),
                page_type,
                &ps,
                &recent_list,
                false,
            );
        });
    }

    button
}

pub(super) fn build_home_browse_row(
    card: &HomeBrowseCard,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> GtkBox {
    let wrapper = GtkBox::new(Orientation::Horizontal, 0);
    wrapper.add_css_class("track-row-wrapper");

    let row_btn = Button::new();
    row_btn.add_css_class("track-row");
    row_btn.set_hexpand(true);

    let row = GtkBox::new(Orientation::Horizontal, 10);

    let art_container = GtkBox::new(Orientation::Vertical, 0);
    art_container.add_css_class("track-art");
    art_container.set_size_request(44, 44);
    art_container.set_valign(Align::Center);
    art_container.set_overflow(gtk::Overflow::Hidden);

    if let Some(url) = card.thumbnail_url.as_deref() {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(true);
        picture.set_size_request(44, 44);
        art_container.append(&picture);
        load_thumbnail_into(&picture, url, 88);
    } else {
        let initial = card
            .title
            .chars()
            .next()
            .unwrap_or('\u{266B}')
            .to_uppercase()
            .to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("track-art-letter");
        letter.set_vexpand(true);
        letter.set_valign(Align::Center);
        letter.set_halign(Align::Center);
        art_container.append(&letter);
    }
    row.append(&art_container);

    let info = GtkBox::new(Orientation::Vertical, 1);
    info.set_hexpand(true);
    info.set_valign(Align::Center);

    let title_label = Label::new(Some(&card.title));
    title_label.set_halign(Align::Start);
    title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title_label.add_css_class("track-title");
    info.append(&title_label);

    let subtitle = card
        .subtitle
        .clone()
        .unwrap_or_else(|| match card.page_type {
            HomeBrowsePageType::Album => "Album".to_string(),
            HomeBrowsePageType::Playlist => "Playlist".to_string(),
            HomeBrowsePageType::Artist => "Artist".to_string(),
            HomeBrowsePageType::UserChannel => "Channel".to_string(),
            HomeBrowsePageType::PodcastShow => "Show".to_string(),
        });
    let subtitle_label = Label::new(Some(&subtitle));
    subtitle_label.set_halign(Align::Start);
    subtitle_label.add_css_class("track-artist");
    subtitle_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info.append(&subtitle_label);

    row.append(&info);
    row_btn.set_child(Some(&row));

    if let Some(payload) = queue_drag_payload_for_browse_card(card) {
        attach_queue_drag_source(&row_btn, payload, ps);
    }

    {
        let state = state.clone();
        let content_box = content_box.clone();
        let content_scroll = content_scroll.clone();
        let home_pagination = home_pagination.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let browse_id = card.browse_id.clone();
        let title = card.title.clone();
        let thumb = card.thumbnail_url.clone();
        let page_type = card.page_type;

        row_btn.connect_clicked(move |_| {
            load_browse_detail(
                &state,
                &content_box,
                &content_scroll,
                &home_pagination,
                &browse_id,
                &title,
                thumb.as_deref(),
                page_type,
                &ps,
                &recent_list,
                false,
            );
        });
    }

    wrapper.append(&row_btn);
    wrapper
}

pub(super) fn queue_drag_payload_for_browse_card(
    card: &HomeBrowseCard,
) -> Option<QueueDragPayload> {
    match card.page_type {
        HomeBrowsePageType::Album
        | HomeBrowsePageType::Playlist
        | HomeBrowsePageType::PodcastShow => Some(QueueDragPayload::BrowseCollection {
            browse_id: card.browse_id.clone(),
            title: card.title.clone(),
            thumbnail_url: card.thumbnail_url.clone(),
            subtitle: card.subtitle.clone(),
            page_type: card.page_type,
        }),
        HomeBrowsePageType::Artist | HomeBrowsePageType::UserChannel => None,
    }
}

pub(super) fn build_playlist_card(
    playlist: &PlaylistSummary,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> Button {
    let card_btn = build_browse_summary_card(
        &playlist.title,
        playlist.subtitle.as_deref(),
        playlist.thumbnail_url.as_deref(),
    );

    {
        let state = state.clone();
        let content_box = content_box.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let playlist_id = playlist.playlist_id.clone();
        let playlist_title = playlist.title.clone();
        let playlist_thumb = playlist.thumbnail_url.clone();

        card_btn.connect_clicked(move |_| {
            load_playlist_detail(
                &state,
                &content_box,
                &playlist_id,
                &playlist_title,
                playlist_thumb.as_deref(),
                &ps,
                &recent_list,
            );
        });
    }

    attach_queue_drag_source(
        &card_btn,
        QueueDragPayload::Playlist {
            playlist_id: playlist.playlist_id.clone(),
            title: playlist.title.clone(),
            thumbnail_url: playlist.thumbnail_url.clone(),
            subtitle: playlist.subtitle.clone(),
            source: QueuePlaylistSource::Remote,
        },
        ps,
    );

    card_btn
}

pub(super) fn build_browse_summary_card(
    title: &str,
    subtitle: Option<&str>,
    thumbnail_url: Option<&str>,
) -> Button {
    let card_btn = Button::new();
    card_btn.add_css_class("cover-card");
    card_btn.set_size_request(166, -1);
    card_btn.set_width_request(166);
    card_btn.set_hexpand(false);
    card_btn.set_halign(Align::Start);
    card_btn.set_valign(Align::Start);

    let card = GtkBox::new(Orientation::Vertical, 6);
    card.set_size_request(150, -1);
    card.set_width_request(150);
    card.set_halign(Align::Start);
    card.set_valign(Align::Start);
    card.set_hexpand(false);

    let art_container = build_square_cover_art(thumbnail_url, title, 150);
    card.append(&art_container);

    let name = Label::new(Some(title));
    name.add_css_class("cover-card-title");
    name.set_width_request(150);
    name.set_halign(Align::Start);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_max_width_chars(18);
    card.append(&name);

    if let Some(subtitle) = subtitle {
        let subtitle_label = Label::new(Some(subtitle));
        subtitle_label.add_css_class("cover-card-artist");
        subtitle_label.set_width_request(150);
        subtitle_label.set_halign(Align::Start);
        subtitle_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        subtitle_label.set_max_width_chars(18);
        card.append(&subtitle_label);
    }

    card_btn.set_child(Some(&card));
    card_btn
}

pub(super) fn load_playlist_detail(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    playlist_id: &str,
    playlist_title: &str,
    playlist_thumb: Option<&str>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some(&format!("Loading \"{}\"...", playlist_title)));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();
    let playlist_id = playlist_id.to_string();
    let playlist_title = playlist_title.to_string();
    let playlist_thumb = playlist_thumb.map(|s| s.to_string());

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let pid = playlist_id.clone();
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.get_playlist(&pid)))
                .await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(tracks))) => {
                // Back button
                let back_btn = Button::with_label("\u{2190} Back to Playlists");
                back_btn.add_css_class("nav-item");
                back_btn.set_halign(Align::Start);
                back_btn.set_margin_start(24);
                back_btn.set_margin_top(12);
                {
                    let state = state.clone();
                    let content_box = content_box.clone();
                    let ps = ps.clone();
                    let recent_list = recent_list.clone();
                    back_btn.connect_clicked(move |_| {
                        load_playlists(&state, &content_box, &ps, &recent_list);
                    });
                }
                content_box.append(&back_btn);

                let header = Label::new(Some(&playlist_title));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(12);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if tracks.is_empty() {
                    let msg = Label::new(Some("This playlist is empty."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                let count_label = Label::new(Some(&format!("{} tracks", tracks.len())));
                count_label.add_css_class("dim-label");
                count_label.set_halign(Align::Start);
                count_label.set_margin_start(28);
                count_label.set_margin_bottom(4);
                content_box.append(&count_label);

                content_box.append(&build_play_controls(
                    &tracks,
                    &state,
                    &ps,
                    Some(DownloadPlaylistMeta {
                        playlist_id: playlist_id.clone(),
                        title: playlist_title.clone(),
                        thumbnail_url: playlist_thumb.clone(),
                        subtitle: Some(format!("{} tracks", tracks.len())),
                    }),
                ));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                let tracks_rc = Rc::new(tracks);
                for (i, track) in tracks_rc.iter().enumerate() {
                    let row = build_track_row(
                        track,
                        i,
                        &tracks_rc,
                        &state,
                        &ps,
                        &recent_list,
                        TrackRowClickBehavior::PlayFromContext,
                    );
                    grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
                }

                content_box.append(&grid);
            }
            Ok(Some(Err(e))) => {
                content_box.append(&Label::new(Some(&format!("Error: {e}"))));
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

pub(super) fn load_browse_detail(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    browse_id: &str,
    browse_title: &str,
    browse_thumb: Option<&str>,
    page_type: HomeBrowsePageType,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
    autoplay: bool,
) {
    clear_box(content_box);
    let loading = Label::new(Some(&format!("Loading \"{}\"...", browse_title)));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let home_pagination = home_pagination.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();
    let browse_id = browse_id.to_string();
    let browse_title = browse_title.to_string();
    let browse_thumb = browse_thumb.map(str::to_string);

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let bid = browse_id.clone();
        let page_type_bg = page_type;
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| match page_type_bg {
                HomeBrowsePageType::UserChannel | HomeBrowsePageType::Artist => {
                    ytm.get_channel_page(&bid).map(BrowseDetailData::Sections)
                }
                _ => ytm.get_browse_tracks(&bid).map(BrowseDetailData::Tracks),
            })
        })
        .await;

        clear_box(&content_box);

        match result {
            Ok(Some(Ok(BrowseDetailData::Sections(page)))) => {
                let display_title = page.title.clone().unwrap_or_else(|| browse_title.clone());
                let display_thumb = page.thumbnail_url.clone().or_else(|| browse_thumb.clone());
                let artist_like = page.artist_like;
                let sections = page.sections;
                let back_btn = Button::with_label("\u{2190} Back to Home");
                back_btn.add_css_class("nav-item");
                back_btn.set_halign(Align::Start);
                back_btn.set_margin_start(24);
                back_btn.set_margin_top(12);
                {
                    let state = state.clone();
                    let content_box = content_box.clone();
                    let content_scroll = content_scroll.clone();
                    let home_pagination = home_pagination.clone();
                    let ps = ps.clone();
                    let recent_list = recent_list.clone();
                    back_btn.connect_clicked(move |_| {
                        load_home(
                            &state,
                            &content_box,
                            &content_scroll,
                            &home_pagination,
                            &ps,
                            &recent_list,
                            false,
                        );
                    });
                }
                content_box.append(&back_btn);

                if sections.is_empty() {
                    let msg = Label::new(Some("No sections found for this channel."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                if autoplay {
                    let tracks = flatten_section_tracks(&sections);
                    if let Some(first_track) = tracks.first() {
                        play_from_context(&state, &ps, &tracks, 0);
                        add_to_recent(&recent_list, first_track, &state, &ps);
                    } else if let Some(card) = first_playable_browse_card(&sections) {
                        let state_bg = state.clone();
                        let card = card.clone();
                        if let Ok(Some(Some(fallback_tracks))) = gio::spawn_blocking(move || {
                            let client = app_client(&state_bg)?;
                            let tracks = match card.page_type {
                                HomeBrowsePageType::Playlist
                                | HomeBrowsePageType::Album
                                | HomeBrowsePageType::PodcastShow => {
                                    client.get_browse_tracks(&card.browse_id).ok()
                                }
                                HomeBrowsePageType::Artist | HomeBrowsePageType::UserChannel => {
                                    client
                                        .get_channel_page(&card.browse_id)
                                        .ok()
                                        .map(|page| flatten_section_tracks(&page.sections))
                                }
                            }?;
                            Some(Some(tracks))
                        })
                        .await
                        {
                            if let Some(first_track) = fallback_tracks.first() {
                                play_from_context(&state, &ps, &fallback_tracks, 0);
                                add_to_recent(&recent_list, first_track, &state, &ps);
                            }
                        }
                    }
                }

                let use_artist_chrome = page_type == HomeBrowsePageType::Artist
                    || artist_like
                    || is_artist_sections(&sections);
                render_artist_view(
                    &content_box,
                    if use_artist_chrome {
                        "Artist"
                    } else {
                        "Channel"
                    },
                    &display_title,
                    display_thumb.as_deref(),
                    &sections,
                    &state,
                    &content_scroll,
                    &home_pagination,
                    &ps,
                    &recent_list,
                );
            }
            Ok(Some(Ok(BrowseDetailData::Tracks(tracks)))) => {
                let back_btn = Button::with_label("\u{2190} Back to Home");
                back_btn.add_css_class("nav-item");
                back_btn.set_halign(Align::Start);
                back_btn.set_margin_start(24);
                back_btn.set_margin_top(12);
                {
                    let state = state.clone();
                    let content_box = content_box.clone();
                    let content_scroll = content_scroll.clone();
                    let home_pagination = home_pagination.clone();
                    let ps = ps.clone();
                    let recent_list = recent_list.clone();
                    back_btn.connect_clicked(move |_| {
                        load_home(
                            &state,
                            &content_box,
                            &content_scroll,
                            &home_pagination,
                            &ps,
                            &recent_list,
                            false,
                        );
                    });
                }
                content_box.append(&back_btn);

                let header = Label::new(Some(&browse_title));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(12);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if tracks.is_empty() {
                    let msg = Label::new(Some("No playable tracks found for this page."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                if autoplay {
                    play_from_context(&state, &ps, &tracks, 0);
                    if let Some(first_track) = tracks.first() {
                        add_to_recent(&recent_list, first_track, &state, &ps);
                    }
                }

                let count_label = Label::new(Some(&format!("{} tracks", tracks.len())));
                count_label.add_css_class("dim-label");
                count_label.set_halign(Align::Start);
                count_label.set_margin_start(28);
                count_label.set_margin_bottom(4);
                content_box.append(&count_label);

                let download_meta = matches!(
                    page_type,
                    HomeBrowsePageType::Playlist | HomeBrowsePageType::Album
                )
                .then(|| DownloadPlaylistMeta {
                    playlist_id: browse_id.clone(),
                    title: browse_title.clone(),
                    thumbnail_url: browse_thumb.clone(),
                    subtitle: Some(format!("{} tracks", tracks.len())),
                });

                content_box.append(&build_play_controls(&tracks, &state, &ps, download_meta));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                let tracks_rc = Rc::new(tracks);
                for (i, track) in tracks_rc.iter().enumerate() {
                    let row = build_track_row(
                        track,
                        i,
                        &tracks_rc,
                        &state,
                        &ps,
                        &recent_list,
                        TrackRowClickBehavior::PlayFromContext,
                    );
                    grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
                }

                content_box.append(&grid);
            }
            Ok(Some(Err(e))) => {
                content_box.append(&Label::new(Some(&format!("Error: {e}"))));
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

pub(super) fn build_offline_playlist_card(
    playlist: &PlaylistSummary,
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> Button {
    let card_btn = Button::new();
    card_btn.add_css_class("cover-card");
    card_btn.set_size_request(166, -1);
    card_btn.set_width_request(166);
    card_btn.set_hexpand(false);
    card_btn.set_halign(Align::Start);
    card_btn.set_valign(Align::Start);

    let card = GtkBox::new(Orientation::Vertical, 6);
    card.set_size_request(150, -1);
    card.set_width_request(150);
    card.set_halign(Align::Start);
    card.set_valign(Align::Start);
    card.set_hexpand(false);

    let art_container =
        build_square_cover_art(playlist.thumbnail_url.as_deref(), &playlist.title, 150);
    card.append(&art_container);

    let name = Label::new(Some(&playlist.title));
    name.add_css_class("cover-card-title");
    name.set_width_request(150);
    name.set_halign(Align::Start);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_max_width_chars(18);
    card.append(&name);

    if let Some(sub) = &playlist.subtitle {
        let subtitle = Label::new(Some(sub));
        subtitle.add_css_class("cover-card-artist");
        subtitle.set_width_request(150);
        subtitle.set_halign(Align::Start);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        subtitle.set_max_width_chars(18);
        card.append(&subtitle);
    }

    card_btn.set_child(Some(&card));

    {
        let state = state.clone();
        let content_box = content_box.clone();
        let content_scroll = content_scroll.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let playlist_id = playlist.playlist_id.clone();
        let playlist_title = playlist.title.clone();

        card_btn.connect_clicked(move |_| {
            load_offline_playlist_detail(
                &state,
                &content_box,
                &content_scroll,
                &playlist_id,
                &playlist_title,
                &ps,
                &recent_list,
            );
        });
    }

    attach_queue_drag_source(
        &card_btn,
        QueueDragPayload::Playlist {
            playlist_id: playlist.playlist_id.clone(),
            title: playlist.title.clone(),
            thumbnail_url: playlist.thumbnail_url.clone(),
            subtitle: playlist.subtitle.clone(),
            source: QueuePlaylistSource::Offline,
        },
        ps,
    );

    card_btn
}

pub(super) fn load_offline_playlist_detail(
    state: &Arc<Mutex<AppState>>,
    content_box: &GtkBox,
    content_scroll: &ScrolledWindow,
    playlist_id: &str,
    playlist_title: &str,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    clear_box(content_box);
    let loading = Label::new(Some(&format!("Loading \"{}\"...", playlist_title)));
    loading.add_css_class("dim-label");
    loading.set_margin_top(60);
    content_box.append(&loading);

    let state = state.clone();
    let content_box = content_box.clone();
    let content_scroll = content_scroll.clone();
    let ps = ps.clone();
    let recent_list = recent_list.clone();
    let playlist_id = playlist_id.to_string();
    let playlist_title = playlist_title.to_string();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let playlist_id_bg = playlist_id.clone();
        let result =
            gio::spawn_blocking(move || load_offline_playlist_tracks(&state_bg, &playlist_id_bg))
                .await;

        clear_box(&content_box);

        match result {
            Ok(tracks) => {
                let back_btn = Button::with_label("\u{2190} Back to Offline");
                back_btn.add_css_class("nav-item");
                back_btn.set_halign(Align::Start);
                back_btn.set_margin_start(24);
                back_btn.set_margin_top(12);
                {
                    let state = state.clone();
                    let content_box = content_box.clone();
                    let ps = ps.clone();
                    let recent_list = recent_list.clone();
                    back_btn.connect_clicked(move |_| {
                        load_offline(&state, &content_box, &content_scroll, &ps, &recent_list);
                    });
                }
                content_box.append(&back_btn);

                let header = Label::new(Some(&playlist_title));
                header.add_css_class("section-header");
                header.set_halign(Align::Start);
                header.set_margin_start(28);
                header.set_margin_top(12);
                header.set_margin_bottom(8);
                content_box.append(&header);

                if tracks.is_empty() {
                    let msg = Label::new(Some("No tracks downloaded for this playlist."));
                    msg.add_css_class("dim-label");
                    msg.set_margin_top(20);
                    content_box.append(&msg);
                    return;
                }

                let count_label = Label::new(Some(&format!("{} tracks", tracks.len())));
                count_label.add_css_class("dim-label");
                count_label.set_halign(Align::Start);
                count_label.set_margin_start(28);
                count_label.set_margin_bottom(4);
                content_box.append(&count_label);

                content_box.append(&build_play_controls(&tracks, &state, &ps, None));

                let grid = gtk::Grid::new();
                grid.set_column_spacing(12);
                grid.set_row_spacing(8);
                grid.set_column_homogeneous(true);
                grid.set_margin_top(4);
                grid.set_margin_start(20);
                grid.set_margin_end(20);

                let tracks_rc = Rc::new(tracks);
                for (i, track) in tracks_rc.iter().enumerate() {
                    let row = build_track_row(
                        track,
                        i,
                        &tracks_rc,
                        &state,
                        &ps,
                        &recent_list,
                        TrackRowClickBehavior::PlayFromContext,
                    );
                    grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
                }

                content_box.append(&grid);
            }
            Err(e) => {
                content_box.append(&Label::new(Some(&format!(
                    "Offline playlist load failed: {e:?}"
                ))));
            }
        }
    });
}
