use super::*;

pub(super) fn sync_mpris_transport(ps: &Rc<PlayerState>, state: &Arc<Mutex<AppState>>) {
    if let Ok(snapshot) = ps.player.replace_queue(
        ps.queue.borrow().clone(),
        ps.queue_index.get(),
        ps.playlist_context.borrow().clone(),
        ps.radio_virtual_playlist.borrow().clone(),
        ps.repeat_mode.get(),
    ) {
        apply_remote_snapshot(ps, state, snapshot);
    }
}

pub(super) fn sync_mpris_position(_ps: &Rc<PlayerState>) {}

pub(super) fn sync_mpris_metadata(_ps: &Rc<PlayerState>, _state: &Arc<Mutex<AppState>>) {}

pub(super) fn sync_mpris_volume(ps: &Rc<PlayerState>) {
    let _ = ps.player.set_volume(ps.volume_slider.value());
}

pub(super) fn next_play_request_serial(ps: &PlayerState) -> u64 {
    let next = ps.play_request_serial.get().wrapping_add(1);
    ps.play_request_serial.set(next);
    next
}

pub(super) fn is_current_play_request(ps: &PlayerState, request_serial: u64) -> bool {
    ps.play_request_serial.get() == request_serial
}

pub(super) fn active_track_snapshot_from_track(track: &Track) -> ActiveTrackSnapshot {
    ActiveTrackSnapshot {
        video_id: track.video_id.clone(),
        title: track.title.clone(),
        artist: artist_string(track),
        thumb_url: track.thumbnail_url.clone(),
        rating: SongRating::Indifferent,
    }
}

pub(super) fn playback_snapshot_from_ui_state(ps: &PlayerState) -> PlaybackSnapshot {
    let mut snapshot = ps.player.snapshot();
    snapshot.halt_auto_resume_pending = false;
    let use_live_ui_queue = !ps.queue.borrow().is_empty();
    let queue = if use_live_ui_queue {
        ps.queue.borrow().clone()
    } else {
        snapshot.queue.clone()
    };

    snapshot.queue = queue;
    snapshot.queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    if use_live_ui_queue && !snapshot.queue.is_empty() {
        snapshot.queue_index = ps
            .queue_index
            .get()
            .min(snapshot.queue.len().saturating_sub(1));
    } else {
        snapshot.queue_index = snapshot
            .queue_index
            .min(snapshot.queue.len().saturating_sub(1));
    }

    let radio_virtual_playlist = if use_live_ui_queue {
        ps.radio_virtual_playlist.borrow().clone()
    } else {
        snapshot.radio_virtual_playlist.clone()
    };
    let playlist_context = if use_live_ui_queue {
        ps.playlist_context.borrow().clone()
    } else {
        snapshot.playlist_context.clone()
    };
    snapshot.playlist_context = playlist_context;
    snapshot.radio_virtual_playlist = radio_virtual_playlist;
    if use_live_ui_queue {
        snapshot.repeat_mode = ps.repeat_mode.get();
        if snapshot.repeat_mode != RepeatMode::One {
            snapshot.track_repeat_loops_left = None;
        }
    }
    snapshot.volume = ps.volume_slider.value();
    if snapshot.probe_active {
        snapshot.current_track = snapshot
            .queue
            .get(snapshot.queue_index)
            .map(active_track_snapshot_from_track);
        snapshot.position_millis = snapshot.probe_restore_position_millis;
        snapshot.duration_millis = None;
        snapshot.buffered_fraction = 0.0;
        snapshot.paused = true;
        snapshot.probe_active = false;
        snapshot.probe_restore_position_millis = None;
    } else {
        snapshot.position_millis = ps
            .now_playing_track
            .borrow()
            .as_ref()
            .map(|track| track.last_observed_position_millis)
            .filter(|position| *position > 0)
            .or(snapshot.position_millis);
    }

    if snapshot.current_track.is_none() {
        snapshot.current_track = snapshot
            .queue
            .get(snapshot.queue_index)
            .map(active_track_snapshot_from_track);
    }

    snapshot
}

pub(super) fn should_show_dropped_resume(ps: &PlayerState) -> bool {
    let snapshot = playback_snapshot_from_ui_state(ps);
    snapshot.current_track.is_none() && snapshot.queue.is_empty()
}

pub(super) fn restore_playback_snapshot_via_daemon(
    snapshot: PlaybackSnapshot,
    resume_playback: bool,
) -> anyhow::Result<PlaybackSnapshot> {
    crate::ipc::ensure_daemon_running()?;
    let client = DaemonClient::new()?;
    let queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    let mut restored = client.set_volume(snapshot.volume)?;
    if snapshot.queue.is_empty() {
        return Ok(restored);
    }

    restored = if resume_playback {
        client.play_queue_at(
            snapshot.queue.clone(),
            queue_index,
            snapshot.playlist_context.clone(),
            snapshot.radio_virtual_playlist.clone(),
            snapshot.repeat_mode,
        )?
    } else {
        client.replace_queue(
            snapshot.queue.clone(),
            queue_index,
            snapshot.playlist_context.clone(),
            snapshot.radio_virtual_playlist.clone(),
            snapshot.repeat_mode,
        )?
    };

    if resume_playback {
        if let Some(position_millis) = snapshot.position_millis.filter(|position| *position > 0) {
            restored = client.seek_to_millis(position_millis)?;
        }
        if snapshot.paused {
            restored = client.pause()?;
        } else if restored.paused {
            restored = client.resume()?;
        }
    }

    Ok(restored)
}

pub(super) fn restore_paused_playback_snapshot_via_daemon(
    snapshot: PlaybackSnapshot,
) -> anyhow::Result<PlaybackSnapshot> {
    crate::ipc::ensure_daemon_running()?;
    let client = DaemonClient::new()?;
    let queue_index = snapshot
        .queue_index
        .min(snapshot.queue.len().saturating_sub(1));

    let mut restored = client.set_volume(snapshot.volume)?;
    if snapshot.queue.is_empty() {
        return Ok(restored);
    }

    restored = client.restore_queue_at(
        snapshot.queue.clone(),
        queue_index,
        snapshot.playlist_context.clone(),
        snapshot.radio_virtual_playlist.clone(),
        snapshot.repeat_mode,
        false,
        true,
        snapshot.position_millis,
    )?;
    Ok(restored)
}

pub(super) fn maybe_restore_daemon_from_ui_state(
    ps: &Rc<PlayerState>,
    state: &Arc<Mutex<AppState>>,
) {
    if ps.daemon_recovery_in_flight.get() {
        return;
    }

    let snapshot = playback_snapshot_from_ui_state(ps);
    let has_recoverable_state = snapshot.current_track.is_some() || !snapshot.queue.is_empty();
    if !has_recoverable_state {
        return;
    }

    let resume_playback = snapshot.current_track.is_some();
    ps.daemon_recovery_in_flight.set(true);
    let ps_for_ui = ps.clone();
    let state_for_ui = state.clone();

    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            if snapshot.paused && snapshot.current_track.is_some() {
                restore_paused_playback_snapshot_via_daemon(snapshot)
            } else {
                restore_playback_snapshot_via_daemon(snapshot, resume_playback)
            }
        })
        .await;

        ps_for_ui.daemon_recovery_in_flight.set(false);

        match result {
            Ok(Ok(restored)) => {
                eprintln!("[UI] restarted staticd and restored playback from live UI state");
                apply_remote_snapshot(&ps_for_ui, &state_for_ui, restored);
            }
            Ok(Err(err)) => eprintln!("[UI] failed to restore staticd from live UI state: {err}"),
            Err(err) => eprintln!("[UI] daemon restore worker failed: {err:?}"),
        }
    });
}

pub(super) fn persist_resume_snapshot_from_ui_state(
    ps: &PlayerState,
    state: &Arc<Mutex<AppState>>,
) {
    let mut snapshot = playback_snapshot_from_ui_state(ps);
    let has_recoverable_state = snapshot.current_track.is_some() || !snapshot.queue.is_empty();
    if !has_recoverable_state {
        return;
    }

    let settings = app_settings(state);
    snapshot.track_repeat_loops_left = if snapshot.repeat_mode == RepeatMode::One
        && snapshot.current_track.is_some()
        && settings.auto_release_track_loops
    {
        Some(settings.auto_release_track_loops_after.max(1))
    } else {
        None
    };

    let persisted = PersistedPlaybackState {
        snapshot,
        saved_at_unix_millis: current_unix_millis(),
    };

    if let Err(err) = store_persisted_playback_state(&persisted) {
        eprintln!("[UI] failed to persist resume snapshot while closing: {err}");
    }
    if let Some(playlist_context) = persisted.snapshot.playlist_context.as_ref() {
        if let Err(err) = store_persisted_playlist_state(playlist_context, &persisted) {
            eprintln!(
                "[UI] failed to persist playlist resume state for {}: {err}",
                playlist_context.playlist_id
            );
        }
    }
}

pub(super) fn apply_remote_snapshot(
    ps: &Rc<PlayerState>,
    state: &Arc<Mutex<AppState>>,
    snapshot: PlaybackSnapshot,
) {
    ps.player.apply_snapshot(snapshot.clone());

    let queue_changed = {
        let queue = ps.queue.borrow();
        *queue != snapshot.queue || ps.queue_index.get() != snapshot.queue_index
    };
    if queue_changed {
        *ps.queue.borrow_mut() = snapshot.queue.clone();
        ps.queue_index.set(snapshot.queue_index);
    }

    if *ps.radio_virtual_playlist.borrow() != snapshot.radio_virtual_playlist {
        let is_radio = snapshot.radio_virtual_playlist.is_some();
        *ps.radio_virtual_playlist.borrow_mut() = snapshot.radio_virtual_playlist.clone();
        if !is_radio {
            ps.radio_append_in_flight.set(false);
            *ps.last_radio_append_anchor.borrow_mut() = None;
        }
    }

    if *ps.playlist_context.borrow() != snapshot.playlist_context {
        *ps.playlist_context.borrow_mut() = snapshot.playlist_context.clone();
    }

    if ps.repeat_mode.get() != snapshot.repeat_mode {
        ps.repeat_mode.set(snapshot.repeat_mode);
        update_repeat_button_ui(&ps.repeat_btn, snapshot.repeat_mode);
    }
    update_sleep_button_ui(&ps.sleep_btn, &snapshot);

    ps.halt_auto_cancel_btn
        .set_visible(snapshot.halt_auto_resume_pending);
    ps.halt_auto_resume_btn
        .set_visible(snapshot.halt_auto_resume_pending);
    ps.halt_auto_cancel_btn
        .set_sensitive(snapshot.halt_auto_resume_pending);
    ps.halt_auto_resume_btn
        .set_sensitive(snapshot.halt_auto_resume_pending);

    if let Some(loops_left) = snapshot
        .track_repeat_loops_left
        .filter(|_| snapshot.current_track.is_some())
    {
        let loop_label = if loops_left == 1 { "loop" } else { "loops" };
        ps.np_repeat_loops_left
            .set_text(&format!("{loops_left} {loop_label} left"));
        ps.np_repeat_loops_left.set_visible(true);
    } else {
        ps.np_repeat_loops_left.set_text("");
        ps.np_repeat_loops_left.set_visible(false);
    }

    let previous_video_id = ps
        .now_playing_track
        .borrow()
        .as_ref()
        .map(|track| track.video_id.clone());

    match snapshot.current_track.as_ref() {
        Some(track) => {
            let is_new_track = previous_video_id.as_deref() != Some(track.video_id.as_str());
            let persisted_rating = ps
                .now_playing_track
                .borrow()
                .as_ref()
                .filter(|current| current.video_id == track.video_id)
                .map(|current| current.rating)
                .unwrap_or(track.rating);
            let session_id = if is_new_track {
                let next = ps.playback_session_id.get().wrapping_add(1);
                ps.playback_session_id.set(next);
                next
            } else {
                ps.now_playing_track
                    .borrow()
                    .as_ref()
                    .map(|current| current.session_id)
                    .unwrap_or_else(|| {
                        let next = ps.playback_session_id.get().wrapping_add(1);
                        ps.playback_session_id.set(next);
                        next
                    })
            };

            *ps.now_playing_track.borrow_mut() = Some(NowPlayingTrack {
                session_id,
                video_id: track.video_id.clone(),
                rating: persisted_rating,
                last_observed_position_millis: snapshot.position_millis.unwrap_or(0),
            });

            ps.np_title.set_text(&track.title);
            ps.np_artist.set_text(&track.artist);
            if let Some(url) = track.thumb_url.as_deref() {
                load_thumbnail_into(&ps.np_thumb, url, 40);
            } else {
                ps.np_thumb.set_paintable(None::<&gtk::gdk::Texture>);
            }

            let playing = !snapshot.paused;
            set_button_icon(
                &ps.play_pause_btn,
                if playing { ICON_PAUSE } else { ICON_PLAY },
                28,
            );
            ps.play_pause_btn.set_sensitive(true);
            ps.prev_btn.set_sensitive(!snapshot.queue.is_empty());
            ps.next_btn.set_sensitive(!snapshot.queue.is_empty());
            ps.like_btn.set_sensitive(true);
            ps.dislike_btn.set_sensitive(true);
            ps.share_btn.set_sensitive(true);
            ps.menu_btn.set_sensitive(true);
            update_rating_buttons_ui(&ps.like_btn, &ps.dislike_btn, persisted_rating);

            if is_new_track {
                spawn_song_rating_sync(state, ps, &track.video_id, session_id);
                spawn_synced_lyrics_sync(state, ps, &track.video_id, session_id);
            }

            apply_snapshot_playback_ui(ps, &snapshot);
        }
        None => {
            ps.now_playing_track.borrow_mut().take();
            ps.last_mpris_length_micros.set(None);
            ps.lyrics_request_serial
                .set(ps.lyrics_request_serial.get().wrapping_add(1));
            show_lyrics_idle(ps);
            ps.np_title.set_text("Nothing playing");
            ps.np_repeat_loops_left.set_text("");
            ps.np_repeat_loops_left.set_visible(false);
            ps.np_artist.set_text("");
            ps.np_thumb.set_paintable(None::<&gtk::gdk::Texture>);
            ps.time_label.set_text("0:00 / 0:00");
            ps.progress_slider.set_value(0.0);
            ps.progress_slider.set_fill_level(0.0);
            set_button_icon(&ps.play_pause_btn, ICON_PLAY, 28);
            ps.play_pause_btn.set_sensitive(false);
            ps.prev_btn.set_sensitive(false);
            ps.next_btn.set_sensitive(false);
            ps.halt_auto_cancel_btn.set_visible(false);
            ps.halt_auto_resume_btn.set_visible(false);
            ps.halt_auto_cancel_btn.set_sensitive(false);
            ps.halt_auto_resume_btn.set_sensitive(false);
            ps.like_btn.set_sensitive(false);
            ps.dislike_btn.set_sensitive(false);
            ps.share_btn.set_sensitive(false);
            ps.menu_btn.set_sensitive(false);
            update_rating_buttons_ui(&ps.like_btn, &ps.dislike_btn, SongRating::Indifferent);
        }
    }

    if (ps.volume_slider.value() - snapshot.volume).abs() > 0.01 {
        ps.volume_slider.set_value(snapshot.volume);
    }
    if queue_changed {
        render_queue_panel(ps, state);
    }
}

pub(super) fn apply_snapshot_playback_ui(ps: &PlayerState, snapshot: &PlaybackSnapshot) {
    let pos = snapshot.position_millis.map(ClockTime::from_mseconds);
    let dur = snapshot.duration_millis.map(ClockTime::from_mseconds);

    if let Some(position_millis) = snapshot.position_millis {
        note_playback_position(ps, position_millis);
    }
    update_active_lyric_line(ps, snapshot.position_millis);

    match (pos, dur) {
        (Some(pos), Some(dur)) => {
            let dur_ns = dur.nseconds();
            if dur_ns > 0 {
                let frac = pos.nseconds() as f64 / dur_ns as f64;
                ps.progress_slider.set_value(frac.clamp(0.0, 1.0));
            } else {
                ps.progress_slider.set_value(0.0);
            }
            ps.time_label.set_text(&format!(
                "{} / {}",
                format_clock_time(pos),
                format_clock_time(dur),
            ));
        }
        (Some(pos), None) => {
            ps.progress_slider.set_value(0.0);
            ps.time_label
                .set_text(&format!("{} / --:--", format_clock_time(pos)));
        }
        (None, Some(dur)) => {
            ps.progress_slider.set_value(0.0);
            ps.time_label
                .set_text(&format!("0:00 / {}", format_clock_time(dur)));
        }
        (None, None) => {}
    }

    ps.progress_slider
        .set_fill_level(snapshot.buffered_fraction);
}
