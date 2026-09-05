//! Remote playlist mutations. The editor addresses occurrences, never queue slots.
use super::{
    app::{self, App, LoadState, Message},
    theme,
};
use crate::models::{PlaylistEditor, PlaylistSummary, Track};
use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Task};
use std::collections::HashSet;

#[derive(Default)]
pub struct State {
    open: bool,
    serial: u64,
    busy: bool,
    title: String,
    video_ids: Vec<String>,
    choices: Vec<PlaylistSummary>,
    editor: Option<PlaylistEditor>,
    selected: HashSet<usize>,
    error: Option<String>,
    notice: Option<String>,
    picker: bool,
    needs_reload: bool,
    confirming_delete: bool,
}
#[derive(Clone)]
pub enum Msg {
    Create,
    SaveRadio,
    AddTrack(Track),
    Edit(String),
    Close,
    Title(String),
    Choices(u64, Result<Vec<PlaylistSummary>, String>),
    Loaded(u64, Result<PlaylistEditor, String>),
    CreateSave,
    AddTo(String),
    Toggle(usize, bool),
    Remove,
    RequestDelete,
    CancelDelete,
    ConfirmDelete,
    Deleted(u64, String, Result<(), String>),
    Move(usize, bool),
    Saved(
        u64,
        Result<(PlaylistEditor, Result<Vec<PlaylistSummary>, String>), String>,
    ),
}
fn msg(value: Msg) -> Message {
    Message::Playlists(value)
}
fn reset(app: &mut App) {
    let serial = app.playlists.serial.wrapping_add(1);
    app.playlists = State {
        open: true,
        serial,
        ..State::default()
    };
    app.track_menu = None;
}
pub fn update(app: &mut App, value: Msg) -> Task<Message> {
    match value {
        Msg::Create => reset(app),
        Msg::SaveRadio => {
            if !app.player.is_radio_queue() {
                return Task::none();
            }
            reset(app);
            // Capture the full current order, including duplicate occurrences.
            app.playlists.video_ids = app
                .player
                .queue
                .iter()
                .map(|t| t.video_id.clone())
                .collect();
            app.playlists.title = "My radio playlist".into();
        }
        Msg::AddTrack(track) => {
            reset(app);
            app.playlists.picker = true;
            app.playlists.video_ids = vec![track.video_id];
            app.playlists.busy = true;
            let serial = app.playlists.serial;
            let client = app.app_client();
            return app::blocking_or(
                move || {
                    client
                        .ok_or_else(|| "Not connected.".to_string())?
                        .get_editable_playlists()
                        .map_err(|e| e.to_string())
                },
                move |r| msg(Msg::Choices(serial, r)),
                msg(Msg::Choices(serial, Err("Playlist loading failed.".into()))),
            );
        }
        Msg::Edit(id) => {
            reset(app);
            app.playlists.busy = true;
            let serial = app.playlists.serial;
            let client = app.app_client();
            return app::blocking_or(
                move || {
                    client
                        .ok_or_else(|| "Not connected.".to_string())?
                        .get_playlist_editor(&id)
                        .map_err(|e| e.to_string())
                },
                move |r| msg(Msg::Loaded(serial, r)),
                msg(Msg::Loaded(serial, Err("Playlist loading failed.".into()))),
            );
        }
        Msg::Close => {
            if !app.playlists.busy {
                app.playlists.open = false;
                app.playlists.serial = app.playlists.serial.wrapping_add(1);
            }
        }
        Msg::Title(title) => app.playlists.title = title,
        Msg::Choices(serial, result) if serial == app.playlists.serial => {
            app.playlists.busy = false;
            match result {
                Ok(items) => app.playlists.choices = items,
                Err(e) => app.playlists.error = Some(e),
            }
        }
        Msg::Loaded(serial, result) if serial == app.playlists.serial => {
            app.playlists.busy = false;
            match result {
                Ok(editor) => {
                    let target =
                        crate::models::ListenAgainTarget::Playlist(editor.playlist_id.clone());
                    app.playlists.editor = Some(editor);
                    return super::listen_again::load(app, target);
                }
                Err(e) => app.playlists.error = Some(e),
            }
        }
        Msg::RequestDelete if can_delete(&app.playlists) && !pin_busy(app) => {
            app.playlists.confirming_delete = true;
        }
        Msg::CancelDelete if !app.playlists.busy => {
            app.playlists.confirming_delete = false;
        }
        Msg::ConfirmDelete if can_confirm_delete(&app.playlists) && !pin_busy(app) => {
            let Some(client) = app.app_client() else {
                app.playlists.error = Some("Not connected.".into());
                return Task::none();
            };
            let id = app.playlists.editor.as_ref().unwrap().playlist_id.clone();
            let request_id = id.clone();
            let panic_id = id.clone();
            let serial = app.playlists.serial;
            app.playlists.busy = true;
            app.playlists.error = None;
            app.playlists.notice = None;
            return app::blocking_or(
                move || {
                    client
                        .delete_playlist(&request_id)
                        .map_err(|e| e.to_string())
                },
                move |result| msg(Msg::Deleted(serial, id.clone(), result)),
                msg(Msg::Deleted(
                    serial,
                    panic_id,
                    Err("Playlist deletion could not be verified.".into()),
                )),
            );
        }
        Msg::Deleted(serial, id, result) if serial == app.playlists.serial => {
            app.playlists.busy = false;
            app.playlists.confirming_delete = false;
            match result {
                Ok(()) => {
                    app.playlists.open = false;
                    app.playlists.editor = None;
                    app.playlists.selected.clear();
                    app.playlists.serial = app.playlists.serial.wrapping_add(1);
                    if let LoadState::Ready(items) = &mut app.library.playlists {
                        items.retain(|p| !same_id(&p.playlist_id, &id));
                    }
                    if app
                        .library
                        .playlist_detail
                        .as_ref()
                        .is_some_and(|p| same_id(&p.playlist_id, &id))
                    {
                        app.library.playlist_detail = None;
                    }
                    if app
                        .browse
                        .browse_id
                        .as_ref()
                        .is_some_and(|p| same_id(p, &id))
                    {
                        let serial = app.browse.request_serial.wrapping_add(1);
                        app.browse = super::views::browse_detail::State::default();
                        app.browse.request_serial = serial;
                    }
                    app.invalidate_home_cache();
                    app.search.cache = None;
                    app.active_nav = app::NavItem::Playlists;
                    return Task::batch([
                        super::content_loading::load_playlists_task(app),
                        app.show_toast("Playlist deleted."),
                    ]);
                }
                Err(error) => {
                    app.playlists.needs_reload = true;
                    app.playlists.error = Some(format!(
                        "{error} Reload the playlist to check whether it still exists before making more changes."
                    ));
                }
            }
        }
        Msg::Toggle(index, selected) if !app.playlists.busy && !app.playlists.confirming_delete => {
            if selected {
                app.playlists.selected.insert(index);
            } else {
                app.playlists.selected.remove(&index);
            }
        }
        operation @ (Msg::CreateSave | Msg::AddTo(_) | Msg::Remove | Msg::Move(..)) => {
            if app.playlists.busy
                || app.playlists.needs_reload
                || app.playlists.confirming_delete
                || pin_busy(app)
            {
                return Task::none();
            }
            if matches!(operation, Msg::CreateSave) && app.playlists.title.trim().is_empty() {
                return Task::none();
            }
            let Some(client) = app.app_client() else {
                app.playlists.error = Some("Not connected.".into());
                return Task::none();
            };
            let editor = app.playlists.editor.clone();
            if matches!(operation, Msg::Remove | Msg::Move(..))
                && !editor.as_ref().is_some_and(|e| e.editable)
            {
                return Task::none();
            }
            let selected = app.playlists.selected.clone();
            let title = app.playlists.title.trim().to_string();
            let ids = app.playlists.video_ids.clone();
            app.playlists.busy = true;
            app.playlists.error = None;
            app.playlists.notice = None;
            let serial = app.playlists.serial;
            return app::blocking_or(
                move || {
                    let write = || -> anyhow::Result<String> {
                        match operation {
                            Msg::CreateSave => client.create_playlist(&title, &ids),
                            Msg::AddTo(id) => {
                                client.add_playlist_tracks(&id, &ids)?;
                                Ok(id)
                            }
                            Msg::Remove => {
                                let editor = editor.as_ref().unwrap();
                                let entries: Vec<_> = editor
                                    .entries
                                    .iter()
                                    .enumerate()
                                    .filter(|(i, _)| selected.contains(i))
                                    .map(|(_, e)| e.clone())
                                    .collect();
                                if entries.is_empty() {
                                    anyhow::bail!("Select songs to remove.");
                                }
                                client.remove_playlist_entries(&editor.playlist_id, &entries)?;
                                Ok(editor.playlist_id.clone())
                            }
                            Msg::Move(index, down) => {
                                let editor = editor.as_ref().unwrap();
                                let (entry, before) = move_targets(editor, index, down)?;
                                client.move_playlist_entry(
                                    &editor.playlist_id,
                                    &entry,
                                    before.as_deref(),
                                )?;
                                Ok(editor.playlist_id.clone())
                            }
                            _ => unreachable!(),
                        }
                    };
                    let id = write().map_err(|e| e.to_string())?;
                    let editor = client.get_playlist_editor(&id).map_err(|e| format!("Saved, but refresh failed: {e}. Close and reopen the playlist before editing again."))?;
                    let library = client.get_library_playlists().map_err(|e| {
                        format!(
                            "Saved, but library refresh failed: {e}. Close and reopen the playlist."
                        )
                    });
                    Ok((editor, library))
                },
                move |r| msg(Msg::Saved(serial, r)),
                msg(Msg::Saved(
                    serial,
                    Err(
                        "Playlist operation failed. Reopen the playlist to check its state.".into(),
                    ),
                )),
            );
        }
        Msg::Saved(serial, result) if serial == app.playlists.serial => {
            app.playlists.busy = false;
            match result {
                Ok((editor, library)) => {
                    let tracks: Vec<_> = editor.entries.iter().map(|e| e.track.clone()).collect();
                    if let Some(detail) = app
                        .library
                        .playlist_detail
                        .as_mut()
                        .filter(|d| same_id(&d.playlist_id, &editor.playlist_id))
                    {
                        detail.tracks = LoadState::Ready(tracks.clone());
                    }
                    if app
                        .browse
                        .browse_id
                        .as_ref()
                        .is_some_and(|id| same_id(id, &editor.playlist_id))
                    {
                        app.browse.data = Some(
                            super::views::browse_detail::BrowseDetailData::Tracks(tracks),
                        );
                        // Discard a pre-edit browse response still in flight.
                        app.browse.request_serial = app.browse.request_serial.wrapping_add(1);
                        app.browse.loading = false;
                    }
                    match library {
                        Ok(items) => app.library.playlists = LoadState::Ready(items),
                        Err(error) => {
                            app.library.playlists = LoadState::Idle;
                            app.playlists.error = Some(error);
                        }
                    }
                    let pin_target =
                        crate::models::ListenAgainTarget::Playlist(editor.playlist_id.clone());
                    app.playlists.editor = Some(editor);
                    app.playlists.selected.clear();
                    app.playlists.picker = false;
                    app.playlists.notice = Some("Playlist saved.".into());
                    return super::listen_again::load(app, pin_target);
                }
                Err(e) => {
                    app.playlists.needs_reload = true;
                    app.playlists.error = Some(format!(
                        "{e} Reopen the playlist before making more changes."
                    ));
                }
            }
        }
        _ => {}
    }
    Task::none()
}
fn pin_busy(app: &App) -> bool {
    app.playlists.editor.as_ref().is_some_and(|editor| {
        super::listen_again::busy(
            app,
            &crate::models::ListenAgainTarget::Playlist(editor.playlist_id.clone()),
        )
    })
}
fn can_delete(state: &State) -> bool {
    state.open
        && !state.busy
        && !state.needs_reload
        && state.editor.as_ref().is_some_and(|e| e.editable)
}
fn can_confirm_delete(state: &State) -> bool {
    can_delete(state) && state.confirming_delete
}
fn same_id(a: &str, b: &str) -> bool {
    a.strip_prefix("VL").unwrap_or(a) == b.strip_prefix("VL").unwrap_or(b)
}
fn move_targets(
    editor: &PlaylistEditor,
    index: usize,
    down: bool,
) -> anyhow::Result<(String, Option<String>)> {
    let entry = editor
        .entries
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("Song is no longer present."))?;
    let id = entry
        .set_video_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("This song has no playlist occurrence ID."))?;
    if (!down && index == 0) || (down && index + 1 >= editor.entries.len()) {
        anyhow::bail!("Song is already at the boundary.");
    }
    let before_index = if down { index + 2 } else { index - 1 };
    let before = editor
        .entries
        .get(before_index)
        .map(|e| {
            e.set_video_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("The target song has no playlist occurrence ID."))
        })
        .transpose()?;
    Ok((id, before))
}
pub fn is_open(app: &App) -> bool {
    app.playlists.open
}
pub fn edit_button(id: String) -> Element<'static, Message> {
    action("Edit playlist", Some(Msg::Edit(id)))
}
fn action(label: &str, event: Option<Msg>) -> Element<'static, Message> {
    button(text(label.to_string()).size(theme::SIZE_SUB))
        .style(theme::secondary_button)
        .on_press_maybe(event.map(msg))
        .into()
}
pub fn layer(app: &App) -> Element<'_, Message> {
    let state = &app.playlists;
    let active = !state.busy;
    let mut body = column![
        row![
            text(if state.editor.is_some() {
                "Edit playlist"
            } else if state.picker {
                "Add to playlist"
            } else {
                "Create playlist"
            })
            .size(theme::SIZE_HEADER),
            iced::widget::Space::new().width(Length::Fill),
            action("Close", active.then_some(Msg::Close))
        ]
        .spacing(12)
    ]
    .spacing(12);
    if state.busy {
        body = body.push(text("Working…"));
    }
    if let Some(error) = &state.error {
        body = body.push(text(error));
    }
    if let Some(notice) = &state.notice {
        body = body.push(text(notice));
    }
    if let Some(editor) = &state.editor {
        body = body.push(text(&editor.title));
        body = body.push(super::listen_again::control(
            app,
            crate::models::ListenAgainTarget::Playlist(editor.playlist_id.clone()),
            active && !state.confirming_delete && !state.needs_reload,
        ));
        if !editor.editable {
            body = body.push(text(
                "This playlist is read-only. Choose one of your own playlists to edit.",
            ));
        }
        if state.needs_reload {
            body = body.push(action(
                "Reload playlist",
                active.then(|| Msg::Edit(editor.playlist_id.clone())),
            ));
        }
        if editor.editable {
            if state.confirming_delete {
                body = body.push(text(format!(
                    "Permanently delete \"{}\" from YouTube Music? This cannot be undone.",
                    editor.title
                )));
                body = body.push(
                    row![
                        action("Cancel", active.then_some(Msg::CancelDelete)),
                        action(
                            "Confirm delete playlist",
                            (can_confirm_delete(state) && !pin_busy(app))
                                .then_some(Msg::ConfirmDelete)
                        ),
                    ]
                    .spacing(12),
                );
            } else {
                body = body.push(action(
                    "Delete playlist",
                    (can_delete(state) && !pin_busy(app)).then_some(Msg::RequestDelete),
                ));
            }
        }
        let editable = active
            && editor.editable
            && !state.needs_reload
            && !state.confirming_delete
            && !pin_busy(app);
        body = body.push(action(
            "Remove selected songs",
            (editable && !state.selected.is_empty()).then_some(Msg::Remove),
        ));
        let mut entries = column![].spacing(8);
        for (i, entry) in editor.entries.iter().enumerate() {
            let editable = editable && entry.set_video_id.is_some();
            let mut check = checkbox(state.selected.contains(&i)).label(format!(
                "{}. {}",
                i + 1,
                entry.track.title
            ));
            if editable {
                check = check.on_toggle(move |v| msg(Msg::Toggle(i, v)));
            }
            entries = entries.push(
                row![
                    container(check).width(Length::Fill),
                    action("Up", (editable && i > 0).then_some(Msg::Move(i, false))),
                    action(
                        "Down",
                        (editable && i + 1 < editor.entries.len()).then_some(Msg::Move(i, true))
                    )
                ]
                .spacing(8),
            );
        }
        body = body.push(scrollable(entries).height(Length::Fixed(350.0)));
    } else {
        if state.picker {
            let choices = column(state.choices.iter().map(|p| {
                action(
                    &p.title,
                    (active && !state.needs_reload).then(|| Msg::AddTo(p.playlist_id.clone())),
                )
            }))
            .spacing(8);
            body = body.push(scrollable(choices).height(Length::Fixed(180.0)));
        }
        body = body.push(text(format!(
            "New private playlist · {} songs",
            state.video_ids.len()
        )));
        let mut input = text_input("Playlist name", &state.title).id("playlist-name");
        if active {
            input = input
                .on_input(|v| msg(Msg::Title(v)))
                .on_submit(msg(Msg::CreateSave));
        }
        body = body.push(input).push(action(
            "Create playlist",
            (active && !state.needs_reload && !state.title.trim().is_empty())
                .then_some(Msg::CreateSave),
        ));
    }
    container(
        container(body)
            .padding(24)
            .width(Length::Fixed(720.0))
            .style(theme::context_menu),
    )
    .center(Length::Fill)
    .style(theme::content_dim_overlay)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PlaylistEntry;
    fn editor() -> PlaylistEditor {
        PlaylistEditor {
            playlist_id: "PLtest".into(), title: "test".into(), editable: true,
            entries: ["first", "second", "third"].iter().map(|id| PlaylistEntry {
                track: serde_json::from_value(serde_json::json!({"video_id":"same-song", "title":"Duplicate song", "artists":[]})).unwrap(),
                set_video_id: Some((*id).into()),
            }).collect(),
        }
    }
    #[test]
    fn delete_requires_open_editable_editor_and_known_idle_state() {
        let mut state = State {
            open: true,
            editor: Some(editor()),
            ..State::default()
        };
        assert!(can_delete(&state));
        assert!(!can_confirm_delete(&state));
        state.confirming_delete = true;
        assert!(can_confirm_delete(&state));
        state.busy = true;
        assert!(!can_confirm_delete(&state));
        assert!(!can_delete(&state));
        state.busy = false;
        state.needs_reload = true;
        assert!(!can_delete(&state));
        state.needs_reload = false;
        state.editor.as_mut().unwrap().editable = false;
        assert!(!can_delete(&state));
        state.editor.as_mut().unwrap().editable = true;
        state.open = false;
        assert!(!can_delete(&state));
    }
    #[test]
    fn cache_matching_accepts_browse_prefix_without_matching_other_playlists() {
        assert!(same_id("VLPLtest", "PLtest"));
        assert!(same_id("PLtest", "VLPLtest"));
        assert!(!same_id("PLtest", "PLother"));
    }
    #[test]
    fn reorder_uses_occurrences_and_post_removal_destination() {
        let e = editor();
        assert_eq!(
            move_targets(&e, 0, true).unwrap(),
            ("first".into(), Some("third".into()))
        );
        assert_eq!(move_targets(&e, 1, true).unwrap(), ("second".into(), None));
        assert_eq!(
            move_targets(&e, 2, false).unwrap(),
            ("third".into(), Some("second".into()))
        );
    }
    #[test]
    fn reorder_rejects_missing_occurrence_and_boundaries() {
        let mut e = editor();
        assert!(move_targets(&e, 0, false).is_err());
        assert!(move_targets(&e, 2, true).is_err());
        e.entries[1].set_video_id = None;
        assert!(move_targets(&e, 1, false).is_err());
        assert!(move_targets(&e, 2, false).is_err());
    }
}
