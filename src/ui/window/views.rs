use super::*;

pub(super) fn load_sidebar_history(
    state: &Arc<Mutex<AppState>>,
    recent_list: &GtkBox,
    ps: &Rc<PlayerState>,
) {
    let state = state.clone();
    let recent_list = recent_list.clone();
    let ps = ps.clone();

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let result =
            gio::spawn_blocking(move || app_client(&state_bg).map(|ytm| ytm.get_history())).await;

        if let Ok(Some(Ok(tracks))) = result {
            clear_box(&recent_list);
            for track in tracks.iter().take(SIDEBAR_HISTORY_LIMIT) {
                let item = build_sidebar_recent_item(track, &state, &ps);
                recent_list.append(&item);
            }
            if tracks.is_empty() {
                let msg = Label::new(Some("No history yet"));
                msg.add_css_class("sidebar-dim");
                recent_list.append(&msg);
            }
        } else {
            clear_box(&recent_list);
            let msg = Label::new(Some("Could not load history"));
            msg.add_css_class("sidebar-dim");
            recent_list.append(&msg);
        }
    });
}

// ============================================================
// RENDERING
// ============================================================

pub(super) fn resume_snapshot_track(snapshot: &PlaybackSnapshot) -> Option<Track> {
    snapshot
        .queue
        .get(
            snapshot
                .queue_index
                .min(snapshot.queue.len().saturating_sub(1)),
        )
        .cloned()
        .or_else(|| {
            snapshot.current_track.as_ref().map(|track| Track {
                video_id: track.video_id.clone(),
                title: track.title.clone(),
                artists: vec![Artist {
                    name: track.artist.clone(),
                    id: None,
                }],
                album: None,
                duration: None,
                thumbnail_url: track.thumb_url.clone(),
                not_interested_feedback_token: None,
                undo_feedback_token: None,
            })
        })
}

pub(super) fn resume_snapshot_meta(snapshot: &PlaybackSnapshot) -> String {
    let queue_label = if snapshot.radio_virtual_playlist.is_some() {
        "Radio queue"
    } else if snapshot.playlist_context.is_some() {
        "Playlist"
    } else if snapshot.queue.len() > 1 {
        "Queue"
    } else {
        "Track"
    };
    let queue_copy = if snapshot.queue.len() > 1 {
        format!("{queue_label} \u{2022} {} songs", snapshot.queue.len())
    } else {
        queue_label.to_string()
    };

    match snapshot.position_millis {
        Some(position_millis) if position_millis > 0 => {
            let action = if snapshot.paused {
                "Paused at"
            } else {
                "Resume at"
            };
            format!(
                "{queue_copy} \u{2022} {action} {}",
                format_clock_time(ClockTime::from_mseconds(position_millis))
            )
        }
        _ => queue_copy,
    }
}

pub(super) fn render_resume_dropped_section(
    content_box: &GtkBox,
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) -> bool {
    if !should_show_dropped_resume(ps) {
        return false;
    }

    let persisted = match load_persisted_playback_state() {
        Ok(Some(persisted)) if !persisted.snapshot.queue.is_empty() => persisted,
        Ok(_) => return false,
        Err(err) => {
            eprintln!("[UI] failed to read dropped playback state: {err}");
            return false;
        }
    };

    let Some(track) = resume_snapshot_track(&persisted.snapshot) else {
        return false;
    };

    let header = Label::new(Some("Resume dropped"));
    header.add_css_class("section-header");
    header.set_halign(Align::Start);
    content_box.append(&header);

    let resume_btn = Button::new();
    resume_btn.add_css_class("track-row");
    resume_btn.add_css_class("resume-drop-row");
    resume_btn.set_hexpand(true);
    resume_btn.set_halign(Align::Fill);

    let row = GtkBox::new(Orientation::Horizontal, 12);

    let art = GtkBox::new(Orientation::Vertical, 0);
    art.add_css_class("track-art");
    art.add_css_class("resume-drop-art");
    art.set_size_request(56, 56);
    art.set_overflow(gtk::Overflow::Hidden);
    if let Some(url) = track.thumbnail_url.as_deref() {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(true);
        picture.set_size_request(56, 56);
        art.append(&picture);
        load_thumbnail_into(&picture, url, 112);
    } else {
        let initial = track
            .title
            .chars()
            .next()
            .unwrap_or('\u{266B}')
            .to_uppercase()
            .to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("track-art-letter");
        letter.set_halign(Align::Center);
        letter.set_valign(Align::Center);
        letter.set_hexpand(true);
        letter.set_vexpand(true);
        art.append(&letter);
    }
    row.append(&art);

    let info = GtkBox::new(Orientation::Vertical, 2);
    info.set_hexpand(true);
    info.set_valign(Align::Center);

    let title = Label::new(Some(&track.title));
    title.add_css_class("track-title");
    title.set_halign(Align::Start);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info.append(&title);

    let subtitle = Label::new(Some(&artist_string(&track)));
    subtitle.add_css_class("track-artist");
    subtitle.set_halign(Align::Start);
    subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info.append(&subtitle);

    let meta = Label::new(Some(&resume_snapshot_meta(&persisted.snapshot)));
    meta.add_css_class("resume-drop-meta");
    meta.set_halign(Align::Start);
    meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info.append(&meta);

    row.append(&info);

    let badge = Label::new(Some("Resume"));
    badge.add_css_class("resume-drop-badge");
    badge.set_valign(Align::Center);
    row.append(&badge);

    resume_btn.set_child(Some(&row));
    content_box.append(&resume_btn);

    {
        let resume_btn_for_handler = resume_btn.clone();
        let state = state.clone();
        let content_box = content_box.clone();
        let content_scroll = content_scroll.clone();
        let home_pagination = home_pagination.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let resume_snapshot = persisted.snapshot.clone();

        resume_btn.connect_clicked(move |_| {
            resume_btn_for_handler.set_sensitive(false);
            let state = state.clone();
            let content_box = content_box.clone();
            let content_scroll = content_scroll.clone();
            let home_pagination = home_pagination.clone();
            let ps = ps.clone();
            let recent_list = recent_list.clone();
            let resume_btn = resume_btn_for_handler.clone();
            let resume_snapshot = resume_snapshot.clone();

            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || {
                    restore_playback_snapshot_via_daemon(resume_snapshot, true)
                })
                .await;

                match result {
                    Ok(Ok(snapshot)) => {
                        apply_remote_snapshot(&ps, &state, snapshot);
                        load_home(
                            &state,
                            &content_box,
                            &content_scroll,
                            &home_pagination,
                            &ps,
                            &recent_list,
                            false,
                        );
                    }
                    Ok(Err(err)) => {
                        eprintln!("[UI] failed to resume dropped playback: {err}");
                        resume_btn.set_sensitive(true);
                    }
                    Err(err) => {
                        eprintln!("[UI] dropped playback resume worker failed: {err:?}");
                        resume_btn.set_sensitive(true);
                    }
                }
            });
        });
    }

    true
}

pub(super) fn render_home_view(
    content_box: &GtkBox,
    sections: &[HomeSection],
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
    prepend_separator: bool,
) {
    let rendered_resume_dropped = if prepend_separator {
        false
    } else {
        render_resume_dropped_section(
            content_box,
            state,
            content_scroll,
            home_pagination,
            ps,
            recent_list,
        )
    };

    if sections.is_empty() {
        let msg = Label::new(Some(
            "No recommendations returned. Your cookie might be expired.",
        ));
        msg.add_css_class("dim-label");
        msg.set_margin_top(60);
        content_box.append(&msg);
        return;
    }

    // First section -> horizontal cover art cards
    if let Some(first) = sections.first() {
        let first_tracks = home_section_tracks(first);
        if rendered_resume_dropped || (prepend_separator && content_box.first_child().is_some()) {
            content_box.append(&Separator::new(Orientation::Horizontal));
        }
        let header = Label::new(Some(&first.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        content_box.append(&header);

        if should_show_home_play_controls(first, &first_tracks) {
            content_box.append(&build_play_controls(&first_tracks, state, ps, None, None));
        }

        let cards_scroll = build_horizontal_cards_scroll(content_scroll);

        let cards_box = GtkBox::new(Orientation::Horizontal, 14);
        cards_box.set_margin_top(4);
        cards_box.set_margin_bottom(8);

        for item in &first.contents {
            cards_box.append(&build_home_item_card(
                item,
                state,
                content_box,
                content_scroll,
                home_pagination,
                ps,
                recent_list,
            ));
        }

        cards_scroll.set_child(Some(&cards_box));
        content_box.append(&cards_scroll);
    }

    // Remaining sections -> 2-column grid
    for section in sections.iter().skip(1) {
        content_box.append(&Separator::new(Orientation::Horizontal));

        let header = Label::new(Some(&section.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        header.set_margin_top(8);
        content_box.append(&header);

        let tracks = home_section_tracks(section);
        let browse_cards = home_section_browse_cards(section);
        if should_show_home_play_controls(section, &tracks) {
            content_box.append(&build_play_controls(&tracks, state, ps, None, None));
        }
        if tracks.len() == section.contents.len() {
            let grid = gtk::Grid::new();
            grid.set_column_spacing(12);
            grid.set_row_spacing(8);
            grid.set_column_homogeneous(true);
            grid.set_margin_top(4);

            let section_tracks = Rc::new(tracks);
            for (i, track) in section_tracks.iter().enumerate() {
                let row = build_track_row(
                    track,
                    i,
                    &section_tracks,
                    None,
                    state,
                    ps,
                    recent_list,
                    TrackRowClickBehavior::StartRadio,
                );
                grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
            }

            content_box.append(&grid);
        } else if browse_cards.len() == section.contents.len() && !browse_cards.is_empty() {
            let grid = gtk::Grid::new();
            grid.set_column_spacing(12);
            grid.set_row_spacing(8);
            grid.set_column_homogeneous(true);
            grid.set_margin_top(4);

            for (i, card) in browse_cards.iter().enumerate() {
                let row = build_home_browse_row(
                    card,
                    state,
                    content_box,
                    content_scroll,
                    home_pagination,
                    ps,
                    recent_list,
                );
                grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
            }

            content_box.append(&grid);
        } else {
            let cards_scroll = build_horizontal_cards_scroll(content_scroll);

            let cards_box = GtkBox::new(Orientation::Horizontal, 14);
            cards_box.set_margin_top(4);
            cards_box.set_margin_bottom(8);

            for item in &section.contents {
                cards_box.append(&build_home_item_card(
                    item,
                    state,
                    content_box,
                    content_scroll,
                    home_pagination,
                    ps,
                    recent_list,
                ));
            }

            cards_scroll.set_child(Some(&cards_box));
            content_box.append(&cards_scroll);
        }
    }
}

pub(super) fn render_artist_view(
    content_box: &GtkBox,
    eyebrow_label: &str,
    artist_name: &str,
    thumbnail_url: Option<&str>,
    sections: &[HomeSection],
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let ArtistSections {
        top_songs,
        remaining,
    } = split_artist_sections(sections);

    let hero = GtkBox::new(Orientation::Horizontal, 18);
    hero.set_margin_top(8);
    hero.set_margin_bottom(8);
    hero.set_margin_start(28);
    hero.set_margin_end(28);

    let art_frame = GtkBox::new(Orientation::Vertical, 0);
    art_frame.add_css_class("cover-art");
    art_frame.set_size_request(180, 180);
    art_frame.set_overflow(gtk::Overflow::Hidden);
    if let Some(url) = thumbnail_url {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(false);
        picture.set_size_request(180, 180);
        art_frame.append(&picture);
        load_thumbnail_into(&picture, url, 300);
    } else {
        let initial = artist_name
            .chars()
            .next()
            .unwrap_or('\u{266B}')
            .to_uppercase()
            .to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("cover-art-letter");
        letter.set_vexpand(true);
        letter.set_valign(Align::Center);
        art_frame.append(&letter);
    }
    hero.append(&art_frame);

    let meta = GtkBox::new(Orientation::Vertical, 8);
    meta.set_hexpand(true);
    meta.set_valign(Align::Center);

    let eyebrow = Label::new(Some(eyebrow_label));
    eyebrow.add_css_class("dim-label");
    eyebrow.set_halign(Align::Start);
    meta.append(&eyebrow);

    let title = Label::new(Some(artist_name));
    title.add_css_class("section-header");
    title.set_halign(Align::Start);
    title.set_wrap(true);
    meta.append(&title);

    let summary = if let Some(top_songs) = top_songs {
        format!(
            "{} top songs  •  {} featured sections",
            home_section_tracks(top_songs).len(),
            remaining.len()
        )
    } else {
        format!("{} featured sections", remaining.len())
    };
    let summary_label = Label::new(Some(&summary));
    summary_label.add_css_class("dim-label");
    summary_label.set_halign(Align::Start);
    meta.append(&summary_label);

    if let Some(top_songs) = top_songs {
        let tracks = home_section_tracks(top_songs);
        if !tracks.is_empty() {
            meta.append(&build_play_controls(&tracks, state, ps, None, None));
        }
    }

    hero.append(&meta);
    content_box.append(&hero);

    if let Some(top_songs) = top_songs {
        content_box.append(&Separator::new(Orientation::Horizontal));

        let header = Label::new(Some(&top_songs.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        header.set_margin_top(8);
        content_box.append(&header);

        let tracks = home_section_tracks(top_songs);
        if !tracks.is_empty() {
            let grid = gtk::Grid::new();
            grid.set_column_spacing(12);
            grid.set_row_spacing(8);
            grid.set_column_homogeneous(true);
            grid.set_margin_top(4);

            let section_tracks = Rc::new(tracks);
            for (i, track) in section_tracks.iter().enumerate() {
                let row = build_track_row(
                    track,
                    i,
                    &section_tracks,
                    None,
                    state,
                    ps,
                    recent_list,
                    TrackRowClickBehavior::StartRadio,
                );
                grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
            }

            content_box.append(&grid);
        }
    }

    for section in remaining {
        content_box.append(&Separator::new(Orientation::Horizontal));

        let header = Label::new(Some(&section.title));
        header.add_css_class("section-header");
        header.set_halign(Align::Start);
        header.set_margin_top(8);
        content_box.append(&header);

        let tracks = home_section_tracks(section);
        let browse_cards = home_section_browse_cards(section);
        if tracks.len() == section.contents.len() && !tracks.is_empty() {
            content_box.append(&build_play_controls(&tracks, state, ps, None, None));
            let grid = gtk::Grid::new();
            grid.set_column_spacing(12);
            grid.set_row_spacing(8);
            grid.set_column_homogeneous(true);
            grid.set_margin_top(4);

            let section_tracks = Rc::new(tracks);
            for (i, track) in section_tracks.iter().enumerate() {
                let row = build_track_row(
                    track,
                    i,
                    &section_tracks,
                    None,
                    state,
                    ps,
                    recent_list,
                    TrackRowClickBehavior::StartRadio,
                );
                grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
            }

            content_box.append(&grid);
        } else if browse_cards.len() == section.contents.len() && !browse_cards.is_empty() {
            let grid = gtk::Grid::new();
            grid.set_column_spacing(12);
            grid.set_row_spacing(8);
            grid.set_column_homogeneous(true);
            grid.set_margin_top(4);

            for (i, card) in browse_cards.iter().enumerate() {
                let row = build_home_browse_row(
                    card,
                    state,
                    content_box,
                    content_scroll,
                    home_pagination,
                    ps,
                    recent_list,
                );
                grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
            }

            content_box.append(&grid);
        } else {
            let cards_scroll = build_horizontal_cards_scroll(content_scroll);

            let cards_box = GtkBox::new(Orientation::Horizontal, 14);
            cards_box.set_margin_top(4);
            cards_box.set_margin_bottom(8);

            for item in &section.contents {
                cards_box.append(&build_home_item_card(
                    item,
                    state,
                    content_box,
                    content_scroll,
                    home_pagination,
                    ps,
                    recent_list,
                ));
            }

            cards_scroll.set_child(Some(&cards_box));
            content_box.append(&cards_scroll);
        }
    }
}

pub(super) fn extract_listen_again_tracks(sections: &[HomeSection]) -> Vec<Track> {
    sections
        .iter()
        .find(|section| section.title.eq_ignore_ascii_case("Listen again"))
        .or_else(|| {
            sections
                .iter()
                .find(|section| section.title.to_ascii_lowercase().contains("listen again"))
        })
        .map(home_section_tracks)
        .unwrap_or_default()
}

pub(super) fn should_show_home_play_controls(section: &HomeSection, tracks: &[Track]) -> bool {
    !tracks.is_empty()
        && (tracks.len() == section.contents.len() || is_listen_again_section(&section.title))
}

pub(super) fn is_listen_again_section(title: &str) -> bool {
    title.eq_ignore_ascii_case("Listen again")
        || title.to_ascii_lowercase().contains("listen again")
}

pub(super) fn home_section_tracks(section: &HomeSection) -> Vec<Track> {
    section
        .contents
        .iter()
        .filter_map(|item| match item {
            HomeItem::Track(track) => Some(track.clone()),
            HomeItem::BrowseCard(_) => None,
        })
        .collect()
}

pub(super) fn home_section_browse_cards(section: &HomeSection) -> Vec<HomeBrowseCard> {
    section
        .contents
        .iter()
        .filter_map(|item| match item {
            HomeItem::BrowseCard(card) => Some(card.clone()),
            HomeItem::Track(_) => None,
        })
        .collect()
}

pub(super) fn flatten_section_tracks(sections: &[HomeSection]) -> Vec<Track> {
    sections.iter().flat_map(home_section_tracks).collect()
}

pub(super) fn first_playable_browse_card(sections: &[HomeSection]) -> Option<HomeBrowseCard> {
    sections.iter().find_map(|section| {
        section.contents.iter().find_map(|item| match item {
            HomeItem::BrowseCard(card) => Some(card.clone()),
            HomeItem::Track(_) => None,
        })
    })
}

pub(super) fn is_artist_sections(sections: &[HomeSection]) -> bool {
    sections.iter().any(|section| {
        matches!(
            section.title.as_str(),
            "Top songs" | "Albums" | "Singles & EPs" | "Featured on" | "Fans might also like"
        )
    })
}

pub(super) fn split_artist_sections<'a>(sections: &'a [HomeSection]) -> ArtistSections<'a> {
    let mut top_songs = None;
    let mut remaining = Vec::new();

    for section in sections {
        if top_songs.is_none() && section.title == "Top songs" {
            top_songs = Some(section);
        } else {
            remaining.push(section);
        }
    }

    ArtistSections {
        top_songs,
        remaining,
    }
}

pub(super) fn render_search_view(
    content_box: &GtkBox,
    query: &str,
    results: &SearchResult,
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let total_results: usize = results
        .sections
        .iter()
        .map(|section| section.contents.len())
        .sum();
    let header = Label::new(Some(&format!(
        "Results for \"{}\" \u{2014} {} results",
        query, total_results
    )));
    header.add_css_class("section-header");
    header.set_halign(Align::Start);
    content_box.append(&header);

    if total_results == 0 {
        let msg = Label::new(Some("No results found."));
        msg.add_css_class("dim-label");
        msg.set_margin_top(20);
        content_box.append(&msg);
        return;
    }

    if let Some(summary_text) = search_results_summary(results) {
        let summary = Label::new(Some(&summary_text));
        summary.add_css_class("dim-label");
        summary.set_halign(Align::Start);
        summary.set_margin_bottom(4);
        content_box.append(&summary);
    }

    let mut rendered_sections = 0;

    for section in &results.sections {
        if section.contents.is_empty() {
            continue;
        }

        render_search_section(
            content_box,
            if rendered_sections > 0 {
                Some(Separator::new(Orientation::Horizontal))
            } else {
                None
            },
            section,
            state,
            content_scroll,
            home_pagination,
            ps,
            recent_list,
        );
        rendered_sections += 1;
    }
}

fn search_results_summary(results: &SearchResult) -> Option<String> {
    let mut tracks = 0;
    let mut playlists = 0;
    let mut artists = 0;
    let mut albums = 0;
    let mut shows = 0;
    let mut channels = 0;

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

    (!parts.is_empty()).then(|| parts.join("  •  "))
}

pub(super) fn render_search_section(
    content_box: &GtkBox,
    separator: Option<Separator>,
    section: &HomeSection,
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    if let Some(separator) = separator {
        content_box.append(&separator);
    }

    if !section.title.trim().is_empty() {
        let section_header = Label::new(Some(&section.title));
        section_header.add_css_class("section-header");
        section_header.set_halign(Align::Start);
        section_header.set_margin_top(8);
        content_box.append(&section_header);
    }

    let tracks = home_section_tracks(section);
    let browse_cards = home_section_browse_cards(section);

    if tracks.len() == section.contents.len() && !tracks.is_empty() {
        render_search_track_grid(content_box, &tracks, state, ps, recent_list);
        return;
    }

    if browse_cards.len() == section.contents.len() && !browse_cards.is_empty() {
        render_search_browse_grid(
            content_box,
            &browse_cards,
            state,
            content_scroll,
            home_pagination,
            ps,
            recent_list,
        );
        return;
    }

    render_search_mixed_grid(
        content_box,
        section,
        &tracks,
        state,
        content_scroll,
        home_pagination,
        ps,
        recent_list,
    );
}

fn render_search_track_grid(
    content_box: &GtkBox,
    tracks: &[Track],
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    if tracks.len() > 1 {
        content_box.append(&build_play_controls(tracks, state, ps, None, None));
    }

    let grid = gtk::Grid::new();
    grid.set_column_spacing(12);
    grid.set_row_spacing(8);
    grid.set_column_homogeneous(true);
    grid.set_margin_top(4);

    let tracks_rc = Rc::new(tracks.to_vec());
    for (i, track) in tracks_rc.iter().enumerate() {
        let row = build_track_row(
            track,
            i,
            &tracks_rc,
            None,
            state,
            ps,
            recent_list,
            TrackRowClickBehavior::PlayFromContext,
        );
        grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
    }

    content_box.append(&grid);
}

fn render_search_browse_grid(
    content_box: &GtkBox,
    items: &[HomeBrowseCard],
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    let grid = gtk::Grid::new();
    grid.set_column_spacing(12);
    grid.set_row_spacing(8);
    grid.set_column_homogeneous(true);
    grid.set_margin_top(4);

    for (i, item) in items.iter().enumerate() {
        let row = build_home_browse_row(
            item,
            state,
            content_box,
            content_scroll,
            home_pagination,
            ps,
            recent_list,
        );
        grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
    }

    content_box.append(&grid);
}

fn render_search_mixed_grid(
    content_box: &GtkBox,
    section: &HomeSection,
    tracks: &[Track],
    state: &Arc<Mutex<AppState>>,
    content_scroll: &ScrolledWindow,
    home_pagination: &Rc<HomePaginationState>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
) {
    if tracks.len() > 1 {
        content_box.append(&build_play_controls(tracks, state, ps, None, None));
    }

    let grid = gtk::Grid::new();
    grid.set_column_spacing(12);
    grid.set_row_spacing(8);
    grid.set_column_homogeneous(true);
    grid.set_margin_top(4);

    let playable_tracks = Rc::new(tracks.to_vec());
    let mut track_index = 0;

    for (i, item) in section.contents.iter().enumerate() {
        let row = match item {
            HomeItem::Track(track) => {
                let row = build_track_row(
                    track,
                    track_index,
                    &playable_tracks,
                    None,
                    state,
                    ps,
                    recent_list,
                    TrackRowClickBehavior::PlayFromContext,
                );
                track_index += 1;
                row
            }
            HomeItem::BrowseCard(card) => build_home_browse_row(
                card,
                state,
                content_box,
                content_scroll,
                home_pagination,
                ps,
                recent_list,
            ),
        };
        grid.attach(&row, (i % 2) as i32, (i / 2) as i32, 1, 1);
    }

    content_box.append(&grid);
}

// ============================================================
// WIDGETS
// ============================================================

pub(super) fn build_square_cover_art(
    thumbnail_url: Option<&str>,
    fallback_label: &str,
    load_size: i32,
) -> GtkBox {
    let art_container = GtkBox::new(Orientation::Vertical, 0);
    art_container.add_css_class("cover-art");
    art_container.set_size_request(150, 150);
    art_container.set_width_request(150);
    art_container.set_height_request(150);
    art_container.set_halign(Align::Start);
    art_container.set_valign(Align::Start);
    art_container.set_hexpand(false);
    art_container.set_vexpand(false);
    art_container.set_overflow(gtk::Overflow::Hidden);

    if let Some(url) = thumbnail_url {
        let picture = Picture::new();
        picture.set_size_request(150, 150);
        picture.set_width_request(150);
        picture.set_height_request(150);
        picture.set_halign(Align::Start);
        picture.set_valign(Align::Start);
        picture.set_hexpand(false);
        picture.set_vexpand(false);
        picture.set_keep_aspect_ratio(false);
        art_container.append(&picture);
        load_thumbnail_into(&picture, url, load_size);
    } else {
        let initial = fallback_label
            .chars()
            .next()
            .unwrap_or('\u{266B}')
            .to_uppercase()
            .to_string();
        let letter = Label::new(Some(&initial));
        letter.add_css_class("cover-art-letter");
        letter.set_halign(Align::Center);
        letter.set_valign(Align::Center);
        letter.set_hexpand(true);
        letter.set_vexpand(true);
        art_container.append(&letter);
    }

    art_container
}

pub(super) fn build_cover_card(
    track: &Track,
    state: &Arc<Mutex<AppState>>,
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

    let art_container = build_square_cover_art(track.thumbnail_url.as_deref(), &track.title, 300);
    card.append(&art_container);

    let name = Label::new(Some(&track.title));
    name.add_css_class("cover-card-title");
    name.set_halign(Align::Start);
    let name_sw = label_scroller(&name, 20);
    name_sw.set_width_request(150);
    name_sw.set_max_content_width(150);
    attach_hover_marquee(&card_btn, &name_sw, Some(&name));
    card.append(&name_sw);

    let artist_str = artist_string(track);
    let artist = Label::new(Some(&artist_str));
    artist.add_css_class("cover-card-artist");
    artist.set_width_request(150);
    artist.set_halign(Align::Start);
    artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
    artist.set_max_width_chars(18);
    card.append(&artist);

    card_btn.set_child(Some(&card));

    {
        let state = state.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let track = track.clone();
        let video_id = track.video_id.clone();
        let title = track.title.clone();
        let artist_str = artist_str.clone();
        let thumb_url = track.thumbnail_url.clone();

        card_btn.connect_clicked(move |_| {
            play_track(
                &state,
                &video_id,
                &title,
                &artist_str,
                thumb_url.as_deref(),
                &ps,
            );
            add_to_recent(&recent_list, &track, &state, &ps);
        });
    }

    let popover = Popover::new();
    popover.add_css_class("context-menu");
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);

    let pop_box = GtkBox::new(Orientation::Vertical, 0);

    let play_next_btn = Button::with_label("Play next");
    play_next_btn.add_css_class("popover-action-btn");
    {
        let track = track.clone();
        let ps = ps.clone();
        let state = state.clone();
        let popover = popover.clone();
        play_next_btn.connect_clicked(move |_| {
            popover.popdown();
            queue_play_next(track.clone(), &ps, &state);
        });
    }
    pop_box.append(&play_next_btn);

    let add_queue_btn = Button::with_label("Add to queue");
    add_queue_btn.add_css_class("popover-action-btn");
    {
        let track = track.clone();
        let ps = ps.clone();
        let state = state.clone();
        let popover = popover.clone();
        add_queue_btn.connect_clicked(move |_| {
            popover.popdown();
            queue_add_to_end(track.clone(), &ps, &state);
        });
    }
    pop_box.append(&add_queue_btn);

    append_track_feedback_actions(&pop_box, track, state);
    append_download_action(&pop_box, track, state, &popover);

    popover.set_child(Some(&pop_box));
    popover.set_parent(&card_btn);

    let gesture = GestureClick::new();
    gesture.set_button(3);
    {
        let popover = popover.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            popover.popup();
        });
    }
    card_btn.add_controller(gesture);

    {
        let ps = ps.clone();
        popover.connect_notify_local(Some("visible"), move |pop, _| {
            set_content_dimming(&ps, pop.is_visible());
        });
    }

    attach_queue_drag_source(
        &card_btn,
        QueueDragPayload::Track {
            track: track.clone(),
        },
        ps,
    );

    card_btn
}

pub(super) fn build_track_row(
    track: &Track,
    index: usize,
    all_tracks: &Rc<Vec<Track>>,
    playlist_context: Option<PlaylistContext>,
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    recent_list: &GtkBox,
    click_behavior: TrackRowClickBehavior,
) -> GtkBox {
    let wrapper = GtkBox::new(Orientation::Horizontal, 0);
    wrapper.add_css_class("track-row-wrapper");

    let row_btn = Button::new();
    row_btn.add_css_class("track-row");
    row_btn.set_hexpand(true);

    let row = GtkBox::new(Orientation::Horizontal, 10);

    // Small thumbnail
    let art_container = GtkBox::new(Orientation::Vertical, 0);
    art_container.add_css_class("track-art");
    art_container.set_size_request(44, 44);
    art_container.set_valign(Align::Center);
    art_container.set_overflow(gtk::Overflow::Hidden);

    if let Some(url) = &track.thumbnail_url {
        let picture = Picture::new();
        picture.set_keep_aspect_ratio(true);
        picture.set_size_request(44, 44);
        art_container.append(&picture);
        load_thumbnail_into(&picture, url, 88);
    } else {
        let initial = track
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

    let title_label = Label::new(Some(&track.title));
    title_label.set_halign(Align::Start);
    title_label.add_css_class("track-title");
    let title_sw = label_scroller(&title_label, 20);
    title_sw.set_hexpand(true);
    attach_hover_marquee(&row_btn, &title_sw, Some(&title_label));
    info.append(&title_sw);

    let artist_str = artist_string(track);
    let artist_label = Label::new(Some(&artist_str));
    artist_label.set_halign(Align::Start);
    artist_label.add_css_class("track-artist");
    artist_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info.append(&artist_label);

    row.append(&info);

    if let Some(duration) = &track.duration {
        let dur = Label::new(Some(duration));
        dur.add_css_class("track-duration");
        dur.set_valign(Align::Center);
        row.append(&dur);
    }

    row_btn.set_child(Some(&row));

    // Left-click behavior depends on the surface:
    // discovery sections start radio from the clicked song, while playlist-like
    // views play from the current list context.
    {
        let state = state.clone();
        let ps = ps.clone();
        let recent_list = recent_list.clone();
        let track = track.clone();
        let all_tracks = all_tracks.clone();
        let playlist_context = playlist_context.clone();
        let video_id = track.video_id.clone();
        let title = track.title.clone();
        let artist_str = artist_str.clone();
        let thumb_url = track.thumbnail_url.clone();

        row_btn.connect_clicked(move |_| {
            match click_behavior {
                TrackRowClickBehavior::PlayFromContext => {
                    play_from_context(&state, &ps, &all_tracks, index, playlist_context.clone());
                }
                TrackRowClickBehavior::StartRadio => {
                    play_track(
                        &state,
                        &video_id,
                        &title,
                        &artist_str,
                        thumb_url.as_deref(),
                        &ps,
                    );
                }
            }
            add_to_recent(&recent_list, &track, &state, &ps);
        });
    }

    wrapper.append(&row_btn);

    // Right-click context menu
    let popover = Popover::new();
    popover.add_css_class("context-menu");
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);

    let pop_box = GtkBox::new(Orientation::Vertical, 0);

    let play_next_btn = Button::with_label("Play next");
    play_next_btn.add_css_class("popover-action-btn");
    {
        let track = track.clone();
        let ps = ps.clone();
        let state = state.clone();
        let popover = popover.clone();
        play_next_btn.connect_clicked(move |_| {
            popover.popdown();
            queue_play_next(track.clone(), &ps, &state);
        });
    }
    pop_box.append(&play_next_btn);

    let add_queue_btn = Button::with_label("Add to queue");
    add_queue_btn.add_css_class("popover-action-btn");
    {
        let track = track.clone();
        let ps = ps.clone();
        let state = state.clone();
        let popover = popover.clone();
        add_queue_btn.connect_clicked(move |_| {
            popover.popdown();
            queue_add_to_end(track.clone(), &ps, &state);
        });
    }
    pop_box.append(&add_queue_btn);

    append_track_feedback_actions(&pop_box, track, state);
    append_download_action(&pop_box, track, state, &popover);

    popover.set_child(Some(&pop_box));
    popover.set_parent(&wrapper);

    let gesture = GestureClick::new();
    gesture.set_button(3);
    {
        let popover = popover.clone();
        gesture.connect_pressed(move |_, _, _, _| {
            popover.popup();
        });
    }
    wrapper.add_controller(gesture);

    {
        let ps = ps.clone();
        popover.connect_notify_local(Some("visible"), move |pop, _| {
            set_content_dimming(&ps, pop.is_visible());
        });
    }

    attach_queue_drag_source(
        &wrapper,
        QueueDragPayload::Track {
            track: track.clone(),
        },
        ps,
    );

    wrapper
}
