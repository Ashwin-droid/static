use super::*;

pub(super) fn play_track(
    state: &Arc<Mutex<AppState>>,
    video_id: &str,
    title: &str,
    artist: &str,
    thumb_url: Option<&str>,
    ps: &Rc<PlayerState>,
) {
    let seed_track = Track {
        video_id: video_id.to_string(),
        title: title.to_string(),
        artists: vec![Artist {
            name: artist.to_string(),
            id: None,
        }],
        album: None,
        duration: None,
        thumbnail_url: thumb_url.map(String::from),
        not_interested_feedback_token: None,
        undo_feedback_token: None,
    };

    *ps.queue.borrow_mut() = vec![seed_track.clone()];
    ps.queue_index.set(0);
    set_queue_context(
        ps,
        None,
        Some(RadioVirtualPlaylist {
            source_video_id: seed_track.video_id.clone(),
            touched_len: 1,
            chips: Vec::new(),
        }),
    );
    ps.radio_append_in_flight.set(true);
    render_queue_panel(ps, state);
    sync_mpris_transport(ps, state);

    clear_sponsorblock_state(ps);
    ps.lyrics_request_serial
        .set(ps.lyrics_request_serial.get().wrapping_add(1));
    show_lyrics_loading(ps);
    ps.np_title.set_text(title);
    ps.np_artist.set_text(artist);
    ps.time_label.set_text("0:00 / 0:00");
    ps.progress_slider.set_value(0.0);
    ps.progress_slider.set_fill_level(0.0);
    set_button_icon(&ps.play_pause_btn, ICON_PLAY, 28);
    ps.play_pause_btn.set_sensitive(false);
    ps.like_btn.set_sensitive(false);
    ps.dislike_btn.set_sensitive(false);
    ps.menu_btn.set_sensitive(false);

    if let Some(url) = thumb_url {
        load_thumbnail_into(&ps.np_thumb, url, 80);
    } else {
        ps.np_thumb.set_paintable(None::<&gtk::gdk::Texture>);
    }

    let state = state.clone();
    let video_id = video_id.to_string();
    let ps = ps.clone();
    let request_serial = next_play_request_serial(&ps);
    let client = ps.player.daemon_client();
    let queue = ps.queue.borrow().clone();
    let radio_virtual_playlist = ps.radio_virtual_playlist.borrow().clone();
    let repeat_mode = ps.repeat_mode.get();

    glib::spawn_future_local(async move {
        let playback_result = gio::spawn_blocking(move || {
            client.play_queue_at(queue, 0, None, radio_virtual_playlist, repeat_mode)
        })
        .await;

        if !is_current_play_request(&ps, request_serial) {
            return;
        }

        match playback_result {
            Ok(Ok(snapshot)) => apply_remote_snapshot(&ps, &state, snapshot),
            Ok(Err(err)) => {
                ps.np_title.set_text(&err.to_string());
                ps.np_artist.set_text("");
                return;
            }
            Err(err) => {
                ps.np_title
                    .set_text(&format!("Playback start failed: {err:?}"));
                ps.np_artist.set_text("");
                return;
            }
        }

        // Fetch radio queue in background
        let state_bg = state.clone();
        let vid = video_id.clone();
        let radio_result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, None, None))
        })
        .await;

        if !is_current_play_request(&ps, request_serial) {
            return;
        }

        if let Ok(Some(Ok(mut radio))) = radio_result {
            // Ensure the seed track is always first in the queue
            let idx = radio.tracks.iter().position(|t| t.video_id == video_id);
            if let Some(i) = idx {
                // Move it to front if not already there
                if i > 0 {
                    let seed_track = radio.tracks.remove(i);
                    radio.tracks.insert(0, seed_track);
                }
            } else {
                // Not in queue at all — prepend it
                radio.tracks.insert(0, seed_track);
            }

            {
                let mut queue = ps.queue.borrow_mut();
                *queue = radio.tracks;
            }
            ps.queue_index.set(0);
            set_queue_context(
                &ps,
                None,
                Some(RadioVirtualPlaylist {
                    source_video_id: video_id.clone(),
                    touched_len: 1,
                    chips: radio.chips,
                }),
            );
            render_queue_panel(&ps, &state);
            sync_remote_queue(&ps);
            sync_mpris_transport(&ps, &state);
        }
        ps.radio_append_in_flight.set(false);
    });
}

/// Insert a track right after the currently-playing position.
pub(super) fn queue_play_next(track: Track, ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    let insert_at = ps.queue_index.get() + 1;
    ps.queue.borrow_mut().insert(insert_at, track);
    extend_radio_touched_for_insert(ps, insert_at, 1);
    *ps.last_radio_append_anchor.borrow_mut() = None;
    render_queue_panel(ps, state);
    sync_remote_queue(ps);
    sync_mpris_transport(ps, state);
}

/// Append a track to the queue, or to the virtual-playlist boundary for radio.
pub(super) fn queue_add_to_end(track: Track, ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    let queue_len = ps.queue.borrow().len();
    let insert_at = radio_touched_len(ps).unwrap_or(queue_len).min(queue_len);
    if insert_at == queue_len {
        ps.queue.borrow_mut().push(track);
    } else {
        ps.queue.borrow_mut().insert(insert_at, track);
        extend_radio_touched_for_insert(ps, insert_at, 1);
    }
    *ps.last_radio_append_anchor.borrow_mut() = None;
    render_queue_panel(ps, state);
    sync_remote_queue(ps);
    sync_mpris_transport(ps, state);
}

pub(super) fn dismiss_queue_after_current(ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    let keep_len = ps.queue_index.get().saturating_add(1);
    let mut changed = false;
    {
        let mut queue = ps.queue.borrow_mut();
        if queue.len() > keep_len {
            queue.truncate(keep_len);
            changed = true;
        }
    }

    if let Some(radio) = ps.radio_virtual_playlist.borrow_mut().as_mut() {
        if radio.touched_len != keep_len {
            radio.touched_len = keep_len;
            changed = true;
        }
    }

    if !changed {
        return;
    }

    ps.radio_append_in_flight.set(false);
    *ps.last_radio_append_anchor.borrow_mut() = None;
    render_queue_panel(ps, state);
    sync_remote_queue(ps);
    sync_mpris_transport(ps, state);
}

/// Remove a track from the queue by index, adjusting queue_index if needed.
pub(super) fn queue_remove(index: usize, ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    {
        let mut q = ps.queue.borrow_mut();
        if index >= q.len() {
            return;
        }
        q.remove(index);
        let current = ps.queue_index.get();
        if index < current {
            ps.queue_index.set(current - 1);
        }
    }
    shrink_radio_touched_for_remove(ps, index);
    *ps.last_radio_append_anchor.borrow_mut() = None;
    // Defer re-render to avoid destroying widget while its click handler is running
    let ps = ps.clone();
    let state = state.clone();
    glib::idle_add_local_once(move || {
        render_queue_panel(&ps, &state);
        sync_remote_queue(&ps);
        sync_mpris_transport(&ps, &state);
    });
}

pub(super) fn queue_download_track(track: Track, state: &Arc<Mutex<AppState>>) {
    let state = state.clone();
    glib::spawn_future_local(async move {
        let _ = gio::spawn_blocking(move || {
            let vid = track.video_id.clone();
            if ensure_track_available_offline(&state, &track, true, "DOWNLOAD") {
                eprintln!("[DOWNLOAD] pinned {vid} for offline playback");
            }
        })
        .await;
    });
}

pub(super) fn delete_track_from_offline(track: Track, state: &Arc<Mutex<AppState>>) {
    let state = state.clone();
    glib::spawn_future_local(async move {
        let _ = gio::spawn_blocking(move || {
            let vid = track.video_id.clone();
            let guard = state.lock().unwrap();
            if let Err(err) = guard.cache.remove_cached(&vid) {
                eprintln!("[DOWNLOAD] failed to remove {vid} from offline cache: {err}");
            } else {
                eprintln!("[DOWNLOAD] removed {vid} from offline cache");
            }
        })
        .await;
    });
}

pub(super) fn is_pinned_download(state: &Arc<Mutex<AppState>>, video_id: &str) -> bool {
    state.lock().unwrap().cache.is_pinned_offline(video_id)
}

pub(super) fn append_download_action(
    pop_box: &GtkBox,
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    popover: &Popover,
) {
    let is_pinned = is_pinned_download(state, &track.video_id);
    let button = if is_pinned {
        Button::with_label("Delete from offline")
    } else {
        Button::with_label("Download")
    };
    button.add_css_class("popover-action-btn");

    let track = track.clone();
    let state = state.clone();
    let popover = popover.clone();
    button.connect_clicked(move |_| {
        popover.popdown();
        if is_pinned {
            delete_track_from_offline(track.clone(), &state);
        } else {
            queue_download_track(track.clone(), &state);
        }
    });

    pop_box.append(&button);
}

pub(super) fn append_track_feedback_actions(
    pop_box: &GtkBox,
    track: &Track,
    state: &Arc<Mutex<AppState>>,
) {
    let Some(not_interested_token) = track.not_interested_feedback_token.clone() else {
        return;
    };

    let not_interested_btn = Button::with_label("Not interested");
    not_interested_btn.add_css_class("popover-action-btn");
    pop_box.append(&not_interested_btn);

    let undo_btn = Button::with_label("Undo");
    undo_btn.add_css_class("popover-action-btn");
    undo_btn.set_visible(false);
    pop_box.append(&undo_btn);

    let in_flight = Rc::new(Cell::new(false));

    {
        let state = state.clone();
        let not_interested_btn = not_interested_btn.clone();
        let undo_btn = undo_btn.clone();
        let undo_token = track.undo_feedback_token.clone();
        let in_flight = in_flight.clone();
        let not_interested_click_btn = not_interested_btn.clone();

        not_interested_click_btn.connect_clicked(move |_| {
            if in_flight.replace(true) {
                return;
            }

            not_interested_btn.set_sensitive(false);
            undo_btn.set_sensitive(false);

            let state_bg = state.clone();
            let feedback_token = not_interested_token.clone();
            let not_interested_btn_done = not_interested_btn.clone();
            let undo_btn_done = undo_btn.clone();
            let undo_token_done = undo_token.clone();
            let in_flight_done = in_flight.clone();

            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || {
                    app_client(&state_bg).map_or_else(
                        || Err(anyhow::anyhow!("YouTube Music is not connected")),
                        |ytm| ytm.submit_feedback_token(&feedback_token),
                    )
                })
                .await;

                match result {
                    Ok(Ok(())) => {
                        not_interested_btn_done.set_visible(false);
                        if undo_token_done.is_some() {
                            undo_btn_done.set_visible(true);
                            undo_btn_done.set_sensitive(true);
                        }
                    }
                    Ok(Err(err)) => {
                        eprintln!("[FEEDBACK] failed to mark track as not interested: {err}");
                        not_interested_btn_done.set_sensitive(true);
                    }
                    Err(err) => {
                        eprintln!(
                            "[FEEDBACK] failed to run not interested request worker: {err:?}"
                        );
                        not_interested_btn_done.set_sensitive(true);
                    }
                }

                in_flight_done.set(false);
            });
        });
    }

    if let Some(undo_token) = track.undo_feedback_token.clone() {
        let state = state.clone();
        let not_interested_btn = not_interested_btn.clone();
        let undo_btn = undo_btn.clone();
        let in_flight = in_flight.clone();
        let undo_click_btn = undo_btn.clone();

        undo_click_btn.connect_clicked(move |_| {
            if in_flight.replace(true) {
                return;
            }

            not_interested_btn.set_sensitive(false);
            undo_btn.set_sensitive(false);

            let state_bg = state.clone();
            let feedback_token = undo_token.clone();
            let not_interested_btn_done = not_interested_btn.clone();
            let undo_btn_done = undo_btn.clone();
            let in_flight_done = in_flight.clone();

            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || {
                    app_client(&state_bg).map_or_else(
                        || Err(anyhow::anyhow!("YouTube Music is not connected")),
                        |ytm| ytm.submit_feedback_token(&feedback_token),
                    )
                })
                .await;

                match result {
                    Ok(Ok(())) => {
                        undo_btn_done.set_visible(false);
                        not_interested_btn_done.set_visible(true);
                        not_interested_btn_done.set_sensitive(true);
                    }
                    Ok(Err(err)) => {
                        eprintln!("[FEEDBACK] failed to undo not interested: {err}");
                        undo_btn_done.set_sensitive(true);
                    }
                    Err(err) => {
                        eprintln!("[FEEDBACK] failed to run undo feedback worker: {err:?}");
                        undo_btn_done.set_sensitive(true);
                    }
                }

                in_flight_done.set(false);
            });
        });
    }
}

/// Play a track from the queue by index (no radio re-fetch).
pub(super) fn play_from_queue(state: &Arc<Mutex<AppState>>, ps: &Rc<PlayerState>, index: usize) {
    let queue = ps.queue.borrow();
    let Some(track) = queue.get(index) else {
        return;
    };
    ps.queue_index.set(index);
    mark_radio_touched_through(ps, index);

    let title = track.title.clone();
    let artist = artist_string(track);
    let thumb_url = track.thumbnail_url.clone();
    drop(queue);

    clear_sponsorblock_state(ps);
    ps.lyrics_request_serial
        .set(ps.lyrics_request_serial.get().wrapping_add(1));
    show_lyrics_loading(ps);

    // Flush UI
    ps.np_title.set_text(&title);
    ps.np_artist.set_text(&artist);
    ps.time_label.set_text("0:00 / 0:00");
    ps.progress_slider.set_value(0.0);
    ps.progress_slider.set_fill_level(0.0);
    set_button_icon(&ps.play_pause_btn, ICON_PLAY, 28);
    ps.play_pause_btn.set_sensitive(false);

    if let Some(url) = &thumb_url {
        load_thumbnail_into(&ps.np_thumb, url, 80);
    } else {
        ps.np_thumb.set_paintable(None::<&gtk::gdk::Texture>);
    }

    // Re-render queue to highlight current track
    render_queue_panel(ps, state);
    maybe_extend_radio_queue(state, ps);
    let state = state.clone();
    let ps = ps.clone();
    let request_serial = next_play_request_serial(&ps);
    let client = ps.player.daemon_client();
    let queue = ps.queue.borrow().clone();
    let playlist_context = ps.playlist_context.borrow().clone();
    let radio_virtual_playlist = ps.radio_virtual_playlist.borrow().clone();
    let repeat_mode = ps.repeat_mode.get();

    glib::spawn_future_local(async move {
        let playback_result = gio::spawn_blocking(move || {
            client.play_queue_at(
                queue,
                index,
                playlist_context,
                radio_virtual_playlist,
                repeat_mode,
            )
        })
        .await;

        if !is_current_play_request(&ps, request_serial) {
            return;
        }

        match playback_result {
            Ok(Ok(snapshot)) => apply_remote_snapshot(&ps, &state, snapshot),
            Ok(Err(err)) => {
                ps.np_title.set_text(&err.to_string());
                ps.np_artist.set_text("");
            }
            Err(err) => {
                ps.np_title
                    .set_text(&format!("Playback start failed: {err:?}"));
                ps.np_artist.set_text("");
            }
        }
    });
}

/// Set the queue to the given tracks and start playing from index 0.
/// If `shuffle` is true, the tracks are shuffled first.
pub(super) fn play_all(
    tracks: &[Track],
    shuffle: bool,
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    playlist_context: Option<PlaylistContext>,
) {
    if tracks.is_empty() {
        return;
    }

    let mut queue: Vec<Track> = tracks.to_vec();
    if shuffle {
        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        queue.shuffle(&mut rng);
    }

    *ps.queue.borrow_mut() = queue;
    ps.queue_index.set(0);
    set_queue_context(ps, playlist_context, None);

    play_from_queue(state, ps, 0);
}

/// Build a row with "Play All", "Shuffle", and optionally "Download All" buttons.
/// Pass `download_meta` to show the download button — `None` hides it.
pub(super) fn build_play_controls(
    tracks: &[Track],
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    download_meta: Option<DownloadPlaylistMeta>,
    playlist_context: Option<PlaylistContext>,
) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    row.set_margin_start(28);
    row.set_margin_bottom(12);

    let has_saved_resume =
        playlist_context.as_ref().is_some_and(
            |playlist_context| match load_persisted_playlist_state(playlist_context) {
                Ok(Some(persisted)) => !persisted.snapshot.queue.is_empty(),
                Ok(None) => false,
                Err(err) => {
                    eprintln!(
                        "[UI] failed to load persisted playlist state for {}: {err}",
                        playlist_context.playlist_id
                    );
                    false
                }
            },
        );

    if has_saved_resume {
        let resume_btn = Button::new();
        let resume_content = GtkBox::new(Orientation::Horizontal, 6);
        resume_content.append(&svg_image(ICON_PLAY, 14));
        resume_content.append(&Label::new(Some("Resume")));
        resume_btn.set_child(Some(&resume_content));
        resume_btn.add_css_class("nav-item");
        {
            let state = state.clone();
            let ps = ps.clone();
            let playlist_context = playlist_context.clone().unwrap();
            let resume_btn_for_handler = resume_btn.clone();
            resume_btn.connect_clicked(move |btn| {
                btn.set_sensitive(false);
                let state = state.clone();
                let ps = ps.clone();
                let resume_btn = resume_btn_for_handler.clone();
                let playlist_context = playlist_context.clone();
                glib::spawn_future_local(async move {
                    let playlist_context_for_load = playlist_context.clone();
                    let result =
                        gio::spawn_blocking(move || -> anyhow::Result<PlaybackSnapshot> {
                            let persisted =
                                load_persisted_playlist_state(&playlist_context_for_load)?
                                    .and_then(|persisted| {
                                        (!persisted.snapshot.queue.is_empty()).then_some(persisted)
                                    })
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("No saved state for this playlist")
                                    })?;
                            restore_playback_snapshot_via_daemon(persisted.snapshot, true)
                        })
                        .await;

                    match result {
                        Ok(Ok(snapshot)) => apply_remote_snapshot(&ps, &state, snapshot),
                        Ok(Err(err)) => {
                            eprintln!(
                                "[UI] failed to resume playlist {}: {err}",
                                playlist_context.playlist_id
                            );
                            resume_btn.set_sensitive(true);
                        }
                        Err(err) => {
                            eprintln!(
                                "[UI] playlist resume worker failed for {}: {err:?}",
                                playlist_context.playlist_id
                            );
                            resume_btn.set_sensitive(true);
                        }
                    }
                });
            });
        }
        row.append(&resume_btn);
    }

    let play_all_btn = Button::new();
    let play_all_content = GtkBox::new(Orientation::Horizontal, 6);
    play_all_content.append(&svg_image(ICON_PLAY, 14));
    play_all_content.append(&Label::new(Some("Play All")));
    play_all_btn.set_child(Some(&play_all_content));
    play_all_btn.add_css_class("nav-item");
    {
        let tracks = tracks.to_vec();
        let state = state.clone();
        let ps = ps.clone();
        let playlist_context = playlist_context.clone();
        play_all_btn.connect_clicked(move |_| {
            play_all(&tracks, false, &state, &ps, playlist_context.clone());
        });
    }
    row.append(&play_all_btn);

    let shuffle_btn = Button::new();
    let shuffle_content = GtkBox::new(Orientation::Horizontal, 6);
    shuffle_content.append(&svg_image(ICON_SHUFFLE, 14));
    shuffle_content.append(&Label::new(Some("Shuffle")));
    shuffle_btn.set_child(Some(&shuffle_content));
    shuffle_btn.add_css_class("nav-item");
    {
        let tracks = tracks.to_vec();
        let state = state.clone();
        let ps = ps.clone();
        let playlist_context = playlist_context.clone();
        shuffle_btn.connect_clicked(move |_| {
            play_all(&tracks, true, &state, &ps, playlist_context.clone());
        });
    }
    row.append(&shuffle_btn);

    if let Some(meta) = download_meta {
        let already_downloaded = {
            let guard = state.lock().unwrap();
            guard.cache.is_playlist_offline(&meta.playlist_id)
        };

        let dl_btn = Button::new();
        let dl_content = GtkBox::new(Orientation::Horizontal, 6);
        dl_content.append(&svg_image(ICON_DOWNLOAD, 14));
        if already_downloaded {
            dl_content.append(&Label::new(Some("Downloaded")));
            dl_btn.set_sensitive(false);
        } else {
            dl_content.append(&Label::new(Some("Download All")));
        }
        dl_btn.set_child(Some(&dl_content));
        dl_btn.add_css_class("nav-item");
        {
            let tracks = tracks.to_vec();
            let state = state.clone();
            let dl_btn = dl_btn.clone();
            dl_btn.connect_clicked(move |btn| {
                btn.set_sensitive(false);
                download_all_offline(tracks.clone(), state.clone(), Some(meta.clone()));
            });
        }
        row.append(&dl_btn);
    }

    row
}

/// Optional playlist metadata for download grouping.
#[derive(Clone)]
pub(super) struct DownloadPlaylistMeta {
    pub(super) playlist_id: String,
    pub(super) title: String,
    pub(super) thumbnail_url: Option<String>,
    pub(super) subtitle: Option<String>,
}

pub(super) fn cache_track_metadata(state: &Arc<Mutex<AppState>>, track: &Track) {
    let guard = state.lock().unwrap();
    let artist = if track.artists.is_empty() {
        "Unknown artist".to_string()
    } else {
        track
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = guard.cache.record_play(
        &track.video_id,
        &track.title,
        &artist,
        history_thumbnail_candidate(track.thumbnail_url.as_deref()),
        0,
    );
}

pub(super) fn ensure_track_available_offline(
    state: &Arc<Mutex<AppState>>,
    track: &Track,
    pin: bool,
    log_prefix: &str,
) -> bool {
    let vid = &track.video_id;

    {
        let guard = state.lock().unwrap();
        if guard.cache.is_cached(vid) {
            if pin {
                let _ = guard.cache.pin_offline(vid);
            }
            drop(guard);
            let _ = ensure_offline_sponsorblock_cache(state, vid);
            return true;
        }
    }

    cache_track_metadata(state, track);

    let url = {
        let Some(client) = app_client(state) else {
            return false;
        };
        match client.get_audio_url_no_track(vid) {
            Ok(url) => url,
            Err(err) => {
                eprintln!("[{log_prefix}] failed URL for {vid}: {err}");
                return false;
            }
        }
    };

    let audio = match DOWNLOAD_CLIENT
        .get(&url)
        .header("Range", "bytes=0-")
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.bytes())
    {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("[{log_prefix}] download failed for {vid}: {err}");
            return false;
        }
    };

    let art = track
        .thumbnail_url
        .as_ref()
        .and_then(|url| fetch_thumbnail_bytes(url));

    {
        let guard = state.lock().unwrap();
        let _ = guard.cache.store_audio(vid, &audio, art.as_deref());
        if pin {
            let _ = guard.cache.pin_offline(vid);
        }
    }
    let _ = ensure_offline_sponsorblock_cache(state, vid);
    true
}

pub(super) fn download_all_offline(
    tracks: Vec<Track>,
    state: Arc<Mutex<AppState>>,
    playlist_meta: Option<DownloadPlaylistMeta>,
) {
    let count = tracks.len();
    eprintln!("[DOWNLOAD] queuing {count} tracks for offline download");

    glib::spawn_future_local(async move {
        let tracks = tracks.clone();
        let state = state.clone();
        gio::spawn_blocking(move || {
            for (i, track) in tracks.iter().enumerate() {
                let vid = &track.video_id;
                eprintln!("[DOWNLOAD] {}/{count} downloading {vid}...", i + 1);
                if ensure_track_available_offline(&state, track, true, "DOWNLOAD") {
                    eprintln!("[DOWNLOAD] {}/{count} ready: {vid}", i + 1);
                }
            }

            // Store playlist metadata + cover art if this is a playlist download
            if let Some(meta) = &playlist_meta {
                // Download playlist cover art to disk
                let cover_art_path = if let Some(url) = &meta.thumbnail_url {
                    fetch_thumbnail_bytes(url).map(|bytes| {
                        let guard = state.lock().unwrap();
                        let p = guard
                            .cache
                            .art_dir
                            .join(format!("playlist_{}.jpg", meta.playlist_id));
                        let _ = std::fs::write(&p, &bytes);
                        format!("file://{}", p.display())
                    })
                } else {
                    None
                };

                let video_ids: Vec<String> = tracks.iter().map(|t| t.video_id.clone()).collect();
                let guard = state.lock().unwrap();
                let _ = guard.cache.store_offline_playlist(
                    &meta.playlist_id,
                    &meta.title,
                    cover_art_path.as_deref().or(meta.thumbnail_url.as_deref()),
                    meta.subtitle.as_deref(),
                    &video_ids,
                );
                eprintln!(
                    "[DOWNLOAD] stored playlist meta: {} ({} tracks)",
                    meta.title,
                    video_ids.len()
                );
            }

            eprintln!("[DOWNLOAD] finished all {count} tracks");
        })
        .await
        .ok();
    });
}

/// Render the queue panel with chip filters and track list.
pub(super) fn render_queue_panel(ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    ps.queue_panel.set_visible(true);
    clear_queue_drop_preview(ps);
    ps.queue_drag_source_index.set(None);

    let queue = ps.queue.borrow();
    let current_idx = ps.queue_index.get();
    let can_dismiss = current_idx.saturating_add(1) < queue.len();
    ps.queue_dismiss_btn.set_sensitive(can_dismiss);

    // -- Chips --
    clear_box(&ps.chip_bar);
    let chips = radio_chips(ps);
    for chip in &chips {
        let btn = Button::with_label(&chip.label);
        btn.add_css_class("queue-chip");

        let state_c = state.clone();
        let ps_c = ps.clone();
        let playlist_id = chip.playlist_id.clone();
        let params = chip.params.clone();

        btn.connect_clicked(move |_| {
            load_radio_filter(&state_c, &ps_c, &playlist_id, params.as_deref());
        });
        ps.chip_bar.append(&btn);
    }
    // -- Track list --
    clear_box(&ps.queue_list_box);
    let radio_tail_start = if is_radio_queue(ps) {
        let touched_len = radio_touched_len(ps).unwrap_or(0).min(queue.len());
        (touched_len < queue.len()).then_some(touched_len)
    } else {
        None
    };

    for (i, track) in queue.iter().enumerate() {
        if radio_tail_start == Some(i) {
            ps.queue_list_box.append(&build_queue_virtual_divider());
        }

        let preview = QueuePreviewData {
            title: track.title.clone(),
            subtitle: artist_string(track),
            thumbnail_url: track.thumbnail_url.clone(),
            duration: track.duration.clone(),
        };
        let row = build_queue_row_visual(&preview, i == current_idx, true, None);

        // Wrap row content in a GtkBox (not Button) to avoid drag conflicts
        let row_wrapper = GtkBox::new(Orientation::Vertical, 0);
        row_wrapper.add_css_class("queue-row-btn");
        row_wrapper.set_hexpand(true);
        row_wrapper.set_halign(Align::Fill);
        if let Some(tail_start) = radio_tail_start {
            if i >= tail_start {
                row_wrapper.add_css_class("queue-row-radio-generated");
                if i == tail_start {
                    row_wrapper.add_css_class("queue-row-radio-generated-first");
                }
            }
        }
        row_wrapper.append(&row);

        // Click to play
        let click = GestureClick::new();
        click.set_button(1);
        {
            let state = state.clone();
            let ps = ps.clone();
            click.connect_released(move |gesture, _, _, _| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                play_from_queue(&state, &ps, i);
            });
        }
        row_wrapper.add_controller(click);

        // Right-click context menu
        if i != current_idx {
            let popover = Popover::new();
            popover.add_css_class("context-menu");
            popover.set_has_arrow(false);
            popover.set_position(gtk::PositionType::Bottom);

            let pop_box = GtkBox::new(Orientation::Vertical, 0);

            append_track_feedback_actions(&pop_box, track, state);
            append_download_action(&pop_box, track, state, &popover);

            let remove_btn = Button::with_label("Remove from queue");
            remove_btn.add_css_class("popover-action-btn");
            {
                let ps = ps.clone();
                let state = state.clone();
                let popover = popover.clone();
                remove_btn.connect_clicked(move |_| {
                    popover.popdown();
                    queue_remove(i, &ps, &state);
                });
            }
            pop_box.append(&remove_btn);

            popover.set_child(Some(&pop_box));
            popover.set_parent(&row_wrapper);

            // On open: dim all siblings, highlight this row
            {
                let queue_list = ps.queue_list_box.clone();
                let row_wrapper_ref = row_wrapper.clone();
                popover.connect_notify_local(Some("visible"), move |pop, _| {
                    if pop.is_visible() {
                        queue_list.add_css_class("queue-ctx-active");
                        row_wrapper_ref.add_css_class("queue-row-focused");
                    } else {
                        queue_list.remove_css_class("queue-ctx-active");
                        row_wrapper_ref.remove_css_class("queue-row-focused");
                    }
                });
            }

            let rc_gesture = GestureClick::new();
            rc_gesture.set_button(3);
            {
                let popover = popover.clone();
                rc_gesture.connect_pressed(move |_, _, _, _| {
                    popover.popup();
                });
            }
            row_wrapper.add_controller(rc_gesture);
        }

        // Drag source
        let drag_source = DragSource::new();
        drag_source.set_actions(gtk::gdk::DragAction::MOVE);
        {
            let payload = QueueDragPayload::QueueRow { index: i };
            drag_source.connect_prepare(move |_, _, _| queue_drag_provider(&payload));
        }
        {
            let ps = ps.clone();
            let row_wrapper_ref = row_wrapper.clone();
            let preview_payload = QueueDragPayload::QueueRow { index: i };
            drag_source.connect_drag_begin(move |_, drag| {
                if let Some(preview) = queue_preview_data_for_payload(&ps, &preview_payload) {
                    let (width, height) = drag_preview_size(&ps);
                    set_drag_icon_from_preview(drag, &preview, width, height);
                }
                ps.queue_drag_source_index.set(Some(i));
                ps.queue_drag_preview_payload
                    .replace(Some(preview_payload.clone()));
                ps.queue_list_box.add_css_class("queue-drag-active");
                row_wrapper_ref.add_css_class("queue-row-dragging");
            });
        }
        {
            let ps = ps.clone();
            let row_wrapper_ref = row_wrapper.clone();
            drag_source.connect_drag_end(move |_, _, _| {
                row_wrapper_ref.remove_css_class("queue-row-dragging");
                ps.queue_drag_source_index.set(None);
                ps.queue_drag_preview_payload.replace(None);
                ps.queue_list_box.remove_css_class("queue-drag-active");
                stop_queue_autoscroll(&ps);
                clear_queue_drop_preview(&ps);
            });
        }
        row_wrapper.add_controller(drag_source);

        // Drop target
        let drop_target = DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
        {
            let ps = ps.clone();
            let row_wrapper_ref = row_wrapper.clone();
            drop_target.connect_enter(move |_, _, y| {
                let raw_slot = preview_slot_for_pointer(i, y, row_wrapper_ref.allocated_height());
                set_queue_drop_active(&ps, true);
                let preview_slot = normalized_preview_slot(&ps, raw_slot);
                set_queue_drop_preview(&ps, preview_slot);
                let viewport_y = row_wrapper_ref
                    .translate_coordinates(&ps.queue_list_box, 0.0, y)
                    .map(|(_, y)| y - ps.queue_scroll.vadjustment().value())
                    .unwrap_or(y);
                update_queue_drag_hover(&ps, Some(viewport_y));
                gtk::gdk::DragAction::MOVE
            });
        }
        {
            let ps = ps.clone();
            let row_wrapper_ref = row_wrapper.clone();
            drop_target.connect_motion(move |_, _, y| {
                let raw_slot = preview_slot_for_pointer(i, y, row_wrapper_ref.allocated_height());
                set_queue_drop_active(&ps, true);
                let preview_slot = normalized_preview_slot(&ps, raw_slot);
                set_queue_drop_preview(&ps, preview_slot);
                let viewport_y = row_wrapper_ref
                    .translate_coordinates(&ps.queue_list_box, 0.0, y)
                    .map(|(_, y)| y - ps.queue_scroll.vadjustment().value())
                    .unwrap_or(y);
                update_queue_drag_hover(&ps, Some(viewport_y));
                gtk::gdk::DragAction::MOVE
            });
        }
        {
            let ps = ps.clone();
            drop_target.connect_leave(move |_| {
                stop_queue_autoscroll(&ps);
                clear_queue_drop_preview(&ps);
                set_queue_drop_active(&ps, false);
            });
        }
        {
            let ps = ps.clone();
            let state = state.clone();
            let row_wrapper_ref = row_wrapper.clone();
            drop_target.connect_drop(move |_, value, _, y| {
                let Some(payload) = parse_queue_drag_payload(value) else {
                    return false;
                };
                let raw_slot = preview_slot_for_pointer(i, y, row_wrapper_ref.allocated_height());
                stop_queue_autoscroll(&ps);
                clear_queue_drop_preview(&ps);
                set_queue_drop_active(&ps, false);
                handle_queue_drop_payload(payload, raw_slot, &ps, &state)
            });
        }
        row_wrapper.add_controller(drop_target);

        ps.queue_list_box.append(&row_wrapper);
    }
}

/// Switch radio filter (chip click).
pub(super) fn load_radio_filter(
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
    playlist_id: &str,
    params: Option<&str>,
) {
    let state = state.clone();
    let ps = ps.clone();
    let source_video_id = radio_source_video_id(&ps).or_else(|| {
        let q = ps.queue.borrow();
        q.first().map(|track| track.video_id.clone())
    });
    let Some(source_video_id) = source_video_id else {
        return;
    };
    let prefix_len = radio_touched_len(&ps)
        .unwrap_or_else(|| ps.queue_index.get().saturating_add(1))
        .max(1);
    let source_track = {
        let q = ps.queue.borrow();
        q.iter()
            .find(|track| track.video_id == source_video_id)
            .cloned()
            .or_else(|| q.first().cloned())
    };
    let playlist_id = playlist_id.to_string();
    let params = params.map(String::from);

    glib::spawn_future_local(async move {
        let state_bg = state.clone();
        let vid = source_video_id.clone();
        let plid = playlist_id.clone();
        let p = params.clone();
        let result = gio::spawn_blocking(move || {
            app_client(&state_bg).map(|ytm| ytm.get_radio(&vid, Some(&plid), p.as_deref()))
        })
        .await;

        if let Ok(Some(Ok(mut radio))) = result {
            // Ensure the source song is present if the API omits it.
            let idx = radio
                .tracks
                .iter()
                .position(|track| track.video_id == source_video_id);
            if let Some(i) = idx {
                if i > 0 {
                    let t = radio.tracks.remove(i);
                    radio.tracks.insert(0, t);
                }
            } else if let Some(t) = source_track {
                radio.tracks.insert(0, t);
            }

            replace_radio_tail(&ps, prefix_len, radio.tracks, radio.chips);
            render_queue_panel(&ps, &state);
            sync_remote_queue(&ps);
            sync_mpris_transport(&ps, &state);
        }
    });
}

pub(super) fn add_to_recent(
    recent_list: &GtkBox,
    track: &Track,
    state: &Arc<Mutex<AppState>>,
    ps: &Rc<PlayerState>,
) {
    let item = build_sidebar_recent_item(track, state, ps);
    recent_list.prepend(&item);

    // Keep the sidebar aligned with the full history payload we preload.
    let mut count = 0;
    let mut child = recent_list.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        count += 1;
        if count > SIDEBAR_HISTORY_LIMIT {
            recent_list.remove(&c);
        }
        child = next;
    }
}
