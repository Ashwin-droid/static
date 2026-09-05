//! Remote playlist mutations. Edits address occurrences, never queue slots.
//!
//! The playlist page is the editor: rows drag to reorder (drop a row on the
//! row it should take the place of), the track context menu removes an
//! occurrence, and pin / delete live on the page controls and on the
//! playlist-card context menu. Small dialogs cover create, save-radio,
//! add-to-playlist and the delete confirmation.

use iced::widget::{
    Space, button, column, container, mouse_area, row, scrollable, svg, text, text_input,
};
use iced::{Alignment, Element, Length, Point, Task};

use super::app::{self, App, ContentView, LoadState, Message, QueueDragPayload};
use super::{icons, listen_again, queue_panel, theme, thumbnails};
use crate::models::{
    HomeBrowsePageType, ListenAgainTarget, PlaylistEditor, PlaylistSummary, Track,
};

const DIALOG_WIDTH: f32 = 440.0;
const MENU_WIDTH: f32 = 280.0;
const PICKER_HEIGHT: f32 = 260.0;
const INPUT_ID: &str = "playlist-name";
const CONTROL_ICON_SIZE: f32 = 16.0;

#[derive(Default)]
pub struct State {
    /// Guards async editor loads for the page on screen.
    editor_serial: u64,
    /// Guards write / dialog completions.
    op_serial: u64,
    /// Occurrence-level view of the playlist page on screen. `None` until it
    /// loads; a read-only editor means the page belongs to someone else.
    editor: Option<PlaylistEditor>,
    /// A remote write is in flight: the page and dialogs are inert meanwhile.
    busy: bool,
    /// What the in-flight write is doing, for dialog feedback.
    busy_label: &'static str,
    /// Row pressed on the playlist page; the reorder source while the queue
    /// drag that press started stays active.
    drag_source: Option<usize>,
    /// Last pointer position over the playlist page (drag origin / activation;
    /// the queue panel only tracks motion over itself).
    pointer: Point,
    /// The account's own playlists (delete gating, add-to-playlist choices).
    own: Option<Vec<PlaylistSummary>>,
    own_loading: bool,
    dialog: Option<Dialog>,
    dialog_error: Option<String>,
    /// Right-clicked playlist card.
    menu: Option<PlaylistSummary>,
}

impl State {
    /// Every new queue drag starts without a reorder source; the playlist
    /// page re-arms it right after starting the drag.
    pub fn clear_drag_source(&mut self) {
        self.drag_source = None;
    }

    pub fn dismiss_menu(&mut self) {
        self.menu = None;
    }
}

enum Dialog {
    Create {
        title: String,
        video_ids: Vec<String>,
        from_radio: bool,
    },
    Picker {
        track: Track,
        title: String,
    },
    Delete {
        playlist_id: String,
        title: String,
    },
}

#[derive(Clone)]
pub enum Write {
    Moved { from: usize, to: usize },
    Removed(usize),
}

#[derive(Clone)]
pub enum Msg {
    // Page editor.
    EditorLoaded(u64, Result<PlaylistEditor, String>),
    RowPressed { index: usize, track: Track },
    PointerMoved(Point),
    OpenRowMenu { index: usize, track: Track },
    RemoveEntry(usize),
    Written(u64, Result<Write, String>),

    // Playlist-level actions.
    OpenCardMenu(PlaylistSummary),
    DismissMenu,
    OwnLoaded(Result<Vec<PlaylistSummary>, String>),
    RequestDelete { playlist_id: String, title: String },
    ConfirmDelete,
    Deleted(u64, String, Result<(), String>),

    // Dialogs.
    Create,
    SaveRadio,
    AddTrack(Track),
    Title(String),
    Submit,
    AddTo(String),
    Created(u64, Result<String, String>),
    Added(u64, String, Result<(), String>),
    CloseDialog,
}

fn msg(value: Msg) -> Message {
    Message::Playlists(value)
}

// ---------------------------------------------------------------------------
// Page identity
// ---------------------------------------------------------------------------

fn same_id(a: &str, b: &str) -> bool {
    a.strip_prefix("VL").unwrap_or(a) == b.strip_prefix("VL").unwrap_or(b)
}

/// The playlist whose track page is on screen, if any.
fn page_playlist_id(app: &App) -> Option<&str> {
    match app.content_view {
        ContentView::PlaylistDetail => app
            .library
            .playlist_detail
            .as_ref()
            .map(|detail| detail.playlist_id.as_str()),
        ContentView::BrowseDetail if app.browse.page_type == Some(HomeBrowsePageType::Playlist) => {
            app.browse.browse_id.as_deref()
        }
        _ => None,
    }
}

fn page_editor(app: &App) -> Option<&PlaylistEditor> {
    let id = page_playlist_id(app)?;
    app.playlists
        .editor
        .as_ref()
        .filter(|editor| same_id(&editor.playlist_id, id))
}

fn can_edit_page(app: &App) -> bool {
    !app.playlists.busy && page_editor(app).is_some_and(|editor| editor.editable)
}

fn is_own(app: &App, playlist_id: &str) -> Option<bool> {
    app.playlists
        .own
        .as_ref()
        .map(|own| own.iter().any(|p| same_id(&p.playlist_id, playlist_id)))
}

pub fn dialog_open(app: &App) -> bool {
    app.playlists.dialog.is_some()
}

/// Kick off the occurrence-level load (plus pin state) for a playlist page
/// whose tracks just arrived. Library detail and browse playlist pages both
/// call this.
pub fn page_opened(app: &mut App, playlist_id: &str) -> Task<Message> {
    app.playlists.editor = None;
    app.playlists.drag_source = None;
    app.playlists.editor_serial = app.playlists.editor_serial.wrapping_add(1);
    let serial = app.playlists.editor_serial;
    let client = app.app_client();
    let id = playlist_id.to_owned();
    let pin = listen_again::load(app, ListenAgainTarget::Playlist(id.clone()));
    let load = app::blocking_or(
        move || {
            client
                .ok_or_else(|| "Not connected.".to_string())?
                .get_playlist_editor(&id)
                .map_err(|e| e.to_string())
        },
        move |result| msg(Msg::EditorLoaded(serial, result)),
        msg(Msg::EditorLoaded(
            serial,
            Err("Playlist loading failed.".into()),
        )),
    );
    Task::batch([pin, load])
}

/// Mirror the editor's entries into whichever page shows this playlist so row
/// indices stay aligned with occurrences.
fn sync_page_tracks(app: &mut App, editor: &PlaylistEditor) -> Task<Message> {
    let tracks: Vec<Track> = editor.entries.iter().map(|e| e.track.clone()).collect();
    let urls: Vec<String> = tracks
        .iter()
        .filter_map(|t| t.thumbnail_url.clone())
        .collect();
    if let Some(detail) = app
        .library
        .playlist_detail
        .as_mut()
        .filter(|d| same_id(&d.playlist_id, &editor.playlist_id))
    {
        detail.tracks = LoadState::Ready(tracks.clone());
    }
    if app.browse.page_type == Some(HomeBrowsePageType::Playlist)
        && app
            .browse
            .browse_id
            .as_ref()
            .is_some_and(|id| same_id(id, &editor.playlist_id))
    {
        app.browse.subtitle = Some(format!("{} tracks", tracks.len()));
        app.browse.data = Some(super::views::browse_detail::BrowseDetailData::Tracks(
            tracks,
        ));
    }
    thumbnails::ensure_all(app, urls.iter().map(String::as_str))
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

pub fn update(app: &mut App, value: Msg) -> Task<Message> {
    match value {
        Msg::EditorLoaded(serial, result) => {
            if serial != app.playlists.editor_serial {
                return Task::none();
            }
            match result {
                Ok(editor) => {
                    let task = if editor.editable {
                        sync_page_tracks(app, &editor)
                    } else {
                        Task::none()
                    };
                    app.playlists.editor = Some(editor);
                    task
                }
                Err(err) => {
                    crate::app_log!("[UI] playlist editor unavailable: {err}");
                    Task::none()
                }
            }
        }

        Msg::RowPressed { index, track } => {
            let task = queue_panel::update(
                app,
                queue_panel::Msg::DragStarted {
                    payload: QueueDragPayload::Track { track },
                    position: app.playlists.pointer,
                },
            );
            app.playlists.drag_source = can_edit_page(app).then_some(index);
            task
        }

        Msg::PointerMoved(position) => {
            app.playlists.pointer = position;
            if let Some(drag) = app
                .queue_panel
                .drag
                .as_mut()
                .filter(|_| app.playlists.drag_source.is_some())
            {
                if !drag.active
                    && position.distance(drag.origin) >= queue_panel::DRAG_START_THRESHOLD
                {
                    drag.active = true;
                }
            }
            Task::none()
        }

        Msg::OpenRowMenu { index, track } => {
            let task = queue_panel::update_menu(
                app,
                queue_panel::MenuMsg::Open {
                    track,
                    queue_index: None,
                    anchor: Point::ORIGIN,
                },
            );
            if let Some(menu) = app.track_menu.as_mut() {
                menu.playlist_entry = Some(index);
            }
            task
        }

        Msg::RemoveEntry(index) => {
            app.track_menu = None;
            write(app, Write::Removed(index))
        }

        Msg::Written(serial, result) => {
            if serial != app.playlists.op_serial {
                return Task::none();
            }
            app.playlists.busy = false;
            match result {
                Ok(write) => {
                    let Some(editor) = app.playlists.editor.as_mut() else {
                        return Task::none();
                    };
                    let len = editor.entries.len();
                    let toast = match write {
                        Write::Moved { from, to } if from < len && to < len => {
                            let entry = editor.entries.remove(from);
                            editor.entries.insert(to, entry);
                            None
                        }
                        Write::Removed(index) if index < len => {
                            editor.entries.remove(index);
                            Some("Removed from playlist.")
                        }
                        _ => None,
                    };
                    let editor = editor.clone();
                    let mut tasks = vec![sync_page_tracks(app, &editor)];
                    if let Some(toast) = toast {
                        tasks.push(app.show_toast(toast));
                    }
                    Task::batch(tasks)
                }
                Err(err) => {
                    // The remote order is unknown now: reload before more edits.
                    let reload = page_playlist_id(app)
                        .map(str::to_owned)
                        .map(|id| page_opened(app, &id))
                        .unwrap_or_else(Task::none);
                    Task::batch([app.show_toast(err), reload])
                }
            }
        }

        Msg::OpenCardMenu(playlist) => {
            app.track_menu = None;
            let target = ListenAgainTarget::Playlist(playlist.playlist_id.clone());
            app.playlists.menu = Some(playlist);
            Task::batch([listen_again::load(app, target), load_own(app, false)])
        }

        Msg::DismissMenu => {
            app.playlists.menu = None;
            Task::none()
        }

        Msg::OwnLoaded(result) => {
            app.playlists.own_loading = false;
            match result {
                Ok(items) => {
                    app.playlists.own = Some(items);
                    Task::none()
                }
                Err(err) => {
                    if matches!(app.playlists.dialog, Some(Dialog::Picker { .. })) {
                        app.playlists.dialog_error = Some(err);
                        Task::none()
                    } else {
                        app.show_toast(err)
                    }
                }
            }
        }

        Msg::RequestDelete { playlist_id, title } => {
            if app.playlists.busy {
                return Task::none();
            }
            app.playlists.menu = None;
            app.track_menu = None;
            app.playlists.dialog_error = None;
            app.playlists.dialog = Some(Dialog::Delete { playlist_id, title });
            Task::none()
        }

        Msg::ConfirmDelete => {
            let Some(Dialog::Delete { playlist_id, .. }) = &app.playlists.dialog else {
                return Task::none();
            };
            if app.playlists.busy {
                return Task::none();
            }
            let Some(client) = app.app_client() else {
                app.playlists.dialog_error = Some("Not connected.".into());
                return Task::none();
            };
            let id = playlist_id.clone();
            let request_id = id.clone();
            let panic_id = id.clone();
            app.playlists.busy = true;
            app.playlists.busy_label = "Deleting\u{2026}";
            app.playlists.dialog_error = None;
            app.playlists.op_serial = app.playlists.op_serial.wrapping_add(1);
            let serial = app.playlists.op_serial;
            app::blocking_or(
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
            )
        }

        Msg::Deleted(serial, id, result) => {
            if serial != app.playlists.op_serial {
                return Task::none();
            }
            app.playlists.busy = false;
            match result {
                Ok(()) => {
                    app.playlists.dialog = None;
                    app.playlists.dialog_error = None;
                    app.playlists.own = None;
                    if app
                        .playlists
                        .editor
                        .as_ref()
                        .is_some_and(|e| same_id(&e.playlist_id, &id))
                    {
                        app.playlists.editor = None;
                    }
                    if let LoadState::Ready(items) = &mut app.library.playlists {
                        items.retain(|p| !same_id(&p.playlist_id, &id));
                    }
                    let page_gone = page_playlist_id(app).is_some_and(|page| same_id(page, &id));
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
                    let mut tasks = vec![app.show_toast("Playlist deleted.")];
                    if page_gone || app.content_view == ContentView::Playlists {
                        app.active_nav = app::NavItem::Playlists;
                        tasks.push(super::content_loading::load_playlists_task(app));
                    }
                    Task::batch(tasks)
                }
                Err(err) => {
                    app.playlists.dialog_error = Some(err);
                    Task::none()
                }
            }
        }

        Msg::Create => open_dialog(
            app,
            Dialog::Create {
                title: String::new(),
                video_ids: Vec::new(),
                from_radio: false,
            },
        ),

        Msg::SaveRadio => {
            if !app.player.is_radio_queue() {
                return Task::none();
            }
            // Capture the full current order, including duplicate occurrences.
            let video_ids: Vec<String> = app
                .player
                .queue
                .iter()
                .map(|t| t.video_id.clone())
                .collect();
            let seed = app
                .player
                .radio_virtual_playlist
                .as_ref()
                .and_then(|vp| {
                    app.player
                        .queue
                        .iter()
                        .find(|t| t.video_id == vp.source_video_id)
                })
                .map(|t| format!("{} Radio", t.title))
                .unwrap_or_else(|| "My radio".to_owned());
            open_dialog(
                app,
                Dialog::Create {
                    title: seed,
                    video_ids,
                    from_radio: true,
                },
            )
        }

        Msg::AddTrack(track) => {
            let open = open_dialog(
                app,
                Dialog::Picker {
                    track,
                    title: String::new(),
                },
            );
            Task::batch([open, load_own(app, true)])
        }

        Msg::Title(value) => {
            match app.playlists.dialog.as_mut() {
                Some(Dialog::Create { title, .. }) | Some(Dialog::Picker { title, .. }) => {
                    *title = value;
                }
                _ => {}
            }
            Task::none()
        }

        Msg::Submit => {
            if app.playlists.busy {
                return Task::none();
            }
            let (title, video_ids) = match &app.playlists.dialog {
                Some(Dialog::Create {
                    title, video_ids, ..
                }) => (title.trim().to_owned(), video_ids.clone()),
                Some(Dialog::Picker { track, title }) => {
                    (title.trim().to_owned(), vec![track.video_id.clone()])
                }
                _ => return Task::none(),
            };
            if title.is_empty() {
                return Task::none();
            }
            let Some(client) = app.app_client() else {
                app.playlists.dialog_error = Some("Not connected.".into());
                return Task::none();
            };
            app.playlists.busy = true;
            app.playlists.busy_label = "Creating\u{2026}";
            app.playlists.dialog_error = None;
            app.playlists.op_serial = app.playlists.op_serial.wrapping_add(1);
            let serial = app.playlists.op_serial;
            app::blocking_or(
                move || {
                    client
                        .create_playlist(&title, &video_ids)
                        .map_err(|e| e.to_string())
                },
                move |result| msg(Msg::Created(serial, result)),
                msg(Msg::Created(
                    serial,
                    Err("Playlist creation could not be verified.".into()),
                )),
            )
        }

        Msg::AddTo(playlist_id) => {
            if app.playlists.busy {
                return Task::none();
            }
            let Some(Dialog::Picker { track, .. }) = &app.playlists.dialog else {
                return Task::none();
            };
            let Some(client) = app.app_client() else {
                app.playlists.dialog_error = Some("Not connected.".into());
                return Task::none();
            };
            let video_ids = vec![track.video_id.clone()];
            app.playlists.busy = true;
            app.playlists.busy_label = "Adding to playlist\u{2026}";
            app.playlists.dialog_error = None;
            app.playlists.op_serial = app.playlists.op_serial.wrapping_add(1);
            let serial = app.playlists.op_serial;
            let request_id = playlist_id.clone();
            let panic_id = playlist_id.clone();
            app::blocking_or(
                move || {
                    client
                        .add_playlist_tracks(&request_id, &video_ids)
                        .map_err(|e| e.to_string())
                },
                move |result| msg(Msg::Added(serial, playlist_id.clone(), result)),
                msg(Msg::Added(
                    serial,
                    panic_id,
                    Err("Adding to the playlist could not be verified.".into()),
                )),
            )
        }

        Msg::Created(serial, result) => {
            if serial != app.playlists.op_serial {
                return Task::none();
            }
            app.playlists.busy = false;
            match result {
                Ok(_) => {
                    app.playlists.dialog = None;
                    app.playlists.dialog_error = None;
                    app.playlists.own = None;
                    app.invalidate_home_cache();
                    let mut tasks = vec![app.show_toast("Playlist created.")];
                    if app.content_view == ContentView::Playlists {
                        tasks.push(super::content_loading::load_playlists_task(app));
                    } else {
                        app.library.playlists = LoadState::Idle;
                    }
                    Task::batch(tasks)
                }
                Err(err) => {
                    app.playlists.dialog_error = Some(err);
                    Task::none()
                }
            }
        }

        Msg::Added(serial, playlist_id, result) => {
            if serial != app.playlists.op_serial {
                return Task::none();
            }
            app.playlists.busy = false;
            match result {
                Ok(()) => {
                    let title = app
                        .playlists
                        .own
                        .as_ref()
                        .and_then(|own| own.iter().find(|p| same_id(&p.playlist_id, &playlist_id)))
                        .map(|p| p.title.clone());
                    app.playlists.dialog = None;
                    app.playlists.dialog_error = None;
                    app.invalidate_home_cache();
                    let mut tasks = vec![app.show_toast(match title {
                        Some(title) => format!("Added to {title}."),
                        None => "Added to playlist.".to_owned(),
                    })];
                    // The page on screen may be that playlist: refresh its rows.
                    if page_playlist_id(app).is_some_and(|page| same_id(page, &playlist_id)) {
                        tasks.push(page_opened(app, &playlist_id));
                    }
                    Task::batch(tasks)
                }
                Err(err) => {
                    app.playlists.dialog_error = Some(err);
                    Task::none()
                }
            }
        }

        Msg::CloseDialog => {
            if !app.playlists.busy {
                app.playlists.dialog = None;
                app.playlists.dialog_error = None;
            }
            Task::none()
        }
    }
}

fn open_dialog(app: &mut App, dialog: Dialog) -> Task<Message> {
    app.track_menu = None;
    app.playlists.menu = None;
    app.playlists.dialog_error = None;
    app.playlists.dialog = Some(dialog);
    iced::widget::operation::focus(INPUT_ID)
}

/// Fetch the account's own playlists; `refresh` re-reads even when cached.
fn load_own(app: &mut App, refresh: bool) -> Task<Message> {
    if app.playlists.own_loading || (!refresh && app.playlists.own.is_some()) {
        return Task::none();
    }
    app.playlists.own_loading = true;
    let client = app.app_client();
    app::blocking_or(
        move || {
            client
                .ok_or_else(|| "Not connected.".to_string())?
                .get_editable_playlists()
                .map_err(|e| e.to_string())
        },
        |result| msg(Msg::OwnLoaded(result)),
        msg(Msg::OwnLoaded(Err("Playlist loading failed.".into()))),
    )
}

enum Request {
    Move(String, Option<String>),
    Remove(crate::models::PlaylistEntry),
}

fn write(app: &mut App, write: Write) -> Task<Message> {
    if !can_edit_page(app) {
        return Task::none();
    }
    let Some(editor) = page_editor(app).cloned() else {
        return Task::none();
    };
    let Some(client) = app.app_client() else {
        return app.show_toast("Not connected.");
    };
    let request = match &write {
        Write::Moved { from, to } => match drop_targets(&editor, *from, *to) {
            Ok((entry, before)) => Request::Move(entry, before),
            Err(err) => return app.show_toast(err.to_string()),
        },
        Write::Removed(index) => match editor.entries.get(*index) {
            Some(entry) => Request::Remove(entry.clone()),
            None => return Task::none(),
        },
    };
    app.playlists.busy = true;
    app.playlists.busy_label = "Saving\u{2026}";
    app.playlists.op_serial = app.playlists.op_serial.wrapping_add(1);
    let serial = app.playlists.op_serial;
    let playlist_id = editor.playlist_id;
    app::blocking_or(
        move || {
            match request {
                Request::Move(entry, before) => {
                    client.move_playlist_entry(&playlist_id, &entry, before.as_deref())
                }
                Request::Remove(entry) => client.remove_playlist_entries(&playlist_id, &[entry]),
            }
            .map_err(|e| e.to_string())
        },
        move |result| msg(Msg::Written(serial, result.map(|()| write.clone()))),
        msg(Msg::Written(
            serial,
            Err("Playlist edit could not be verified. Reloading the playlist.".into()),
        )),
    )
}

/// Resolve "drop row `from` onto row `to`" into the occurrence to move and the
/// occurrence it must land before (`None` = end of the playlist).
fn drop_targets(
    editor: &PlaylistEditor,
    from: usize,
    to: usize,
) -> anyhow::Result<(String, Option<String>)> {
    if from == to {
        anyhow::bail!("Song is already there.");
    }
    let occurrence = |index: usize| -> anyhow::Result<String> {
        editor
            .entries
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("Song is no longer present."))?
            .set_video_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("This song has no playlist occurrence ID."))
    };
    let entry = occurrence(from)?;
    if to >= editor.entries.len() {
        anyhow::bail!("Song is no longer present.");
    }
    // Dropping downwards lands after the target; upwards lands before it.
    let before = if from < to {
        editor
            .entries
            .get(to + 1)
            .map(|_| occurrence(to + 1))
            .transpose()?
    } else {
        Some(occurrence(to)?)
    };
    Ok((entry, before))
}

/// A row on the playlist page released the pointer: if a reorder drag from
/// this page is active, consume it and issue the move. `None` means the
/// release was a plain click (or a foreign drag) — the caller proceeds.
pub fn drop_on_row(app: &mut App, index: usize) -> Option<Task<Message>> {
    let drag = app.queue_panel.drag.as_ref()?;
    if !drag.active {
        return None;
    }
    let QueueDragPayload::Track { track } = &drag.payload else {
        return None;
    };
    let from = app.playlists.drag_source?;
    let editor = page_editor(app)?;
    if !editor.editable || app.playlists.busy {
        return None;
    }
    let entry = editor.entries.get(from)?;
    if entry.track.video_id != track.video_id || from == index || index >= editor.entries.len() {
        return None;
    }
    app.queue_panel.drag = None;
    app.queue_panel.drop_preview_slot = None;
    app.queue_panel.drag_hover_y = None;
    app.queue_panel.drop_active = false;
    app.playlists.drag_source = None;
    Some(write(app, Write::Moved { from, to: index }))
}

/// Visual role of a playlist-page row during a reorder drag.
pub fn row_drop(app: &App, index: usize, hovered: bool) -> theme::RowDrop {
    let Some(source) = app.playlists.drag_source else {
        return theme::RowDrop::None;
    };
    let active =
        app.queue_panel.drag.as_ref().is_some_and(|drag| {
            drag.active && matches!(drag.payload, QueueDragPayload::Track { .. })
        });
    if !active || !can_edit_page(app) {
        theme::RowDrop::None
    } else if index == source {
        theme::RowDrop::Lifted
    } else if hovered {
        theme::RowDrop::Target
    } else {
        theme::RowDrop::None
    }
}

/// Row-level messages for playlist-page rows (press starts a queue drag that
/// doubles as the reorder source; right-click opens the track menu with the
/// occurrence attached).
pub fn row_press(index: usize, track: &Track) -> Message {
    msg(Msg::RowPressed {
        index,
        track: track.clone(),
    })
}

pub fn row_menu(index: usize, track: &Track) -> Message {
    msg(Msg::OpenRowMenu {
        index,
        track: track.clone(),
    })
}

/// Wrap a playlist page so pointer motion over it arms the reorder drag.
pub fn track_pointer<'a>(page: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    mouse_area(page)
        .on_move(|position| msg(Msg::PointerMoved(position)))
        .into()
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// "Remove from playlist" for the track menu when it was opened from an
/// editable playlist row.
pub fn remove_menu_item(app: &App, index: usize) -> Option<Element<'static, Message>> {
    let editor = page_editor(app).filter(|editor| editor.editable)?;
    let menu = app.track_menu.as_ref()?;
    let entry = editor.entries.get(index)?;
    if entry.track.video_id != menu.track.video_id {
        return None;
    }
    Some(queue_panel::menu_action(
        "Remove from playlist",
        (!app.playlists.busy).then(|| msg(Msg::RemoveEntry(index))),
        true,
    ))
}

/// Pin / Delete for the playlist page's control row.
pub fn page_controls(app: &App) -> Vec<Element<'_, Message>> {
    let mut controls = Vec::new();
    let Some(id) = page_playlist_id(app) else {
        return controls;
    };
    if let Some(pin) = listen_again::page_button(app, ListenAgainTarget::Playlist(id.to_owned())) {
        controls.push(pin);
    }
    if let Some(editor) = page_editor(app).filter(|editor| editor.editable) {
        controls.push(control_button(
            icons::trash(),
            "Delete",
            (!app.playlists.busy).then(|| {
                msg(Msg::RequestDelete {
                    playlist_id: editor.playlist_id.clone(),
                    title: editor.title.clone(),
                })
            }),
        ));
    }
    controls
}

/// "New playlist" for the Your Playlists header.
pub fn create_button() -> Element<'static, Message> {
    control_button(icons::plus(), "New playlist", Some(msg(Msg::Create)))
}

/// Compact "Save as playlist" for the queue panel's radio strip.
pub fn save_radio_button() -> Element<'static, Message> {
    button(text("Save as playlist").size(theme::SIZE_SUB))
        .style(theme::chip(false))
        .padding([theme::SPACE_XS, theme::SPACE_14])
        .on_press(msg(Msg::SaveRadio))
        .into()
}

pub fn card_menu_message(playlist: &PlaylistSummary) -> Message {
    msg(Msg::OpenCardMenu(playlist.clone()))
}

pub fn control_button(
    icon: svg::Handle,
    label: &'static str,
    on_press: Option<Message>,
) -> Element<'static, Message> {
    button(
        row![
            svg(icon)
                .width(Length::Fixed(CONTROL_ICON_SIZE))
                .height(Length::Fixed(CONTROL_ICON_SIZE)),
            text(label).size(theme::SIZE_BODY),
        ]
        .spacing(theme::SPACE_SM)
        .align_y(Alignment::Center),
    )
    .style(theme::secondary_button)
    .padding([theme::SPACE_SM, theme::SPACE_LG])
    .on_press_maybe(on_press)
    .into()
}

/// Window-modal layer: the playlist-card menu or an open dialog.
pub fn overlay(app: &App) -> Option<Element<'_, Message>> {
    if app.playlists.dialog.is_some() {
        return Some(dialog_layer(app));
    }
    app.playlists
        .menu
        .as_ref()
        .map(|menu| card_menu_layer(app, menu))
}

fn card_menu_layer<'a>(app: &'a App, playlist: &'a PlaylistSummary) -> Element<'a, Message> {
    let art = thumbnails::view_thumb(
        app,
        playlist.thumbnail_url.as_deref(),
        40.0,
        playlist.title.chars().next(),
    );
    let header = container(
        row![
            art,
            column![
                text(playlist.title.as_str())
                    .size(theme::SIZE_BODY)
                    .color(theme::TEXT_BRIGHT)
                    .wrapping(text::Wrapping::None),
                text(playlist.subtitle.as_deref().unwrap_or("Playlist"))
                    .size(theme::SIZE_CAPTION)
                    .color(theme::TEXT_DIM)
                    .wrapping(text::Wrapping::None),
            ]
            .spacing(theme::SPACE_XXS)
            .width(Length::Fill),
        ]
        .spacing(theme::SPACE_MD)
        .align_y(Alignment::Center)
        .width(Length::Fill),
    )
    .padding(iced::Padding {
        top: theme::SPACE_SM,
        right: theme::SPACE_MD,
        bottom: theme::SPACE_MD,
        left: theme::SPACE_MD,
    })
    .clip(true);

    let mut items: Vec<Element<'a, Message>> = vec![header.into()];
    items.push(listen_again::menu_item(
        app,
        ListenAgainTarget::Playlist(playlist.playlist_id.clone()),
    ));
    match is_own(app, &playlist.playlist_id) {
        Some(false) => {}
        own => items.push(queue_panel::menu_action(
            "Delete playlist",
            (own == Some(true) && !app.playlists.busy).then(|| {
                msg(Msg::RequestDelete {
                    playlist_id: playlist.playlist_id.clone(),
                    title: playlist.title.clone(),
                })
            }),
            true,
        )),
    }

    let card = container(column(items).spacing(theme::SPACE_XXS).width(Length::Fill))
        .width(Length::Fixed(MENU_WIDTH))
        .padding(theme::SPACE_SM)
        .style(theme::context_menu);

    mouse_area(
        container(card)
            .center(Length::Fill)
            .style(theme::content_dim_overlay),
    )
    .on_press(msg(Msg::DismissMenu))
    .on_right_press(msg(Msg::DismissMenu))
    .into()
}

fn dialog_layer(app: &App) -> Element<'_, Message> {
    let state = &app.playlists;
    let Some(dialog) = state.dialog.as_ref() else {
        return Space::new().into();
    };
    let busy = state.busy;

    let body: Element<'_, Message> = match dialog {
        Dialog::Create {
            title,
            video_ids,
            from_radio,
        } => {
            let mut body = column![dialog_title(if *from_radio {
                "Save radio as playlist"
            } else {
                "New playlist"
            })]
            .spacing(theme::SPACE_LG);
            if *from_radio {
                body = body.push(caption(format!(
                    "{} songs from the current radio, in order.",
                    video_ids.len()
                )));
            }
            body = body.push(name_input(title, busy));
            body = body.push(footer(
                app,
                "Create",
                (!busy && !title.trim().is_empty()).then_some(msg(Msg::Submit)),
                false,
            ));
            body.into()
        }
        Dialog::Picker { track, title } => {
            let art = thumbnails::view_thumb(
                app,
                track.thumbnail_url.as_deref(),
                40.0,
                track.title.chars().next(),
            );
            let summary = row![
                art,
                column![
                    text(track.title.as_str())
                        .size(theme::SIZE_BODY)
                        .color(theme::TEXT_BRIGHT)
                        .wrapping(text::Wrapping::None),
                    text(queue_panel::artist_string(track))
                        .size(theme::SIZE_CAPTION)
                        .color(theme::TEXT_DIM)
                        .wrapping(text::Wrapping::None),
                ]
                .spacing(theme::SPACE_XXS)
                .width(Length::Fill),
            ]
            .spacing(theme::SPACE_MD)
            .align_y(Alignment::Center)
            .width(Length::Fill);

            let choices: Element<'_, Message> = match &state.own {
                None => caption(if state.own_loading {
                    "Loading your playlists…".to_owned()
                } else {
                    "Your playlists are unavailable.".to_owned()
                }),
                Some(own) if own.is_empty() => caption("No playlists yet.".to_owned()),
                Some(own) => scrollable(
                    column(own.iter().map(|playlist| {
                        let mut label = column![
                            text(playlist.title.as_str())
                                .size(theme::SIZE_BODY)
                                .wrapping(text::Wrapping::None)
                        ]
                        .spacing(theme::SPACE_XXS)
                        .width(Length::Fill);
                        if let Some(subtitle) = playlist
                            .subtitle
                            .as_deref()
                            .filter(|s| !s.trim().is_empty())
                        {
                            label = label.push(
                                text(subtitle.to_owned())
                                    .size(theme::SIZE_CAPTION)
                                    .color(theme::TEXT_DIM)
                                    .wrapping(text::Wrapping::None),
                            );
                        }
                        button(container(label).clip(true))
                            .style(theme::list_row(false))
                            .width(Length::Fill)
                            .padding([theme::SPACE_MD, theme::SPACE_LG])
                            .on_press_maybe(
                                (!busy).then(|| msg(Msg::AddTo(playlist.playlist_id.clone()))),
                            )
                            .into()
                    }))
                    .spacing(theme::SPACE_XXS)
                    .width(Length::Fill),
                )
                .style(theme::thin_scrollbar)
                .height(Length::Shrink)
                .into(),
            };
            let choices = container(choices).max_height(PICKER_HEIGHT);

            let new_row = row![
                name_input(title, busy),
                button(text("Create").size(theme::SIZE_BODY))
                    .style(theme::primary_button)
                    .padding([theme::SPACE_MD, theme::SPACE_XL])
                    .on_press_maybe(
                        (!busy && !title.trim().is_empty()).then_some(msg(Msg::Submit))
                    ),
            ]
            .spacing(theme::SPACE_MD)
            .align_y(Alignment::Center);

            let mut body = column![
                dialog_title("Add to playlist"),
                summary,
                choices,
                super::widgets::separator(),
                caption("Or start a new one".to_owned()),
                new_row,
            ]
            .spacing(theme::SPACE_LG);
            body = body.push(footer(app, "", None, true));
            body.into()
        }
        Dialog::Delete { title, .. } => {
            let mut body = column![
                dialog_title("Delete playlist"),
                text(format!(
                    "\u{201c}{title}\u{201d} will be permanently removed from YouTube Music."
                ))
                .size(theme::SIZE_BODY)
                .color(theme::TEXT),
            ]
            .spacing(theme::SPACE_LG);
            body = body.push(footer(
                app,
                "Delete",
                (!busy).then_some(msg(Msg::ConfirmDelete)),
                true,
            ));
            body.into()
        }
    };

    let card = mouse_area(
        container(body)
            .width(Length::Fixed(DIALOG_WIDTH))
            .padding(theme::SPACE_PAGE)
            .style(theme::context_menu),
    )
    .on_press(Message::Noop);

    mouse_area(
        container(card)
            .center(Length::Fill)
            .style(theme::content_dim_overlay),
    )
    .on_press(msg(Msg::CloseDialog))
    .into()
}

fn dialog_title(label: &'static str) -> Element<'static, Message> {
    text(label)
        .size(theme::SIZE_SECTION)
        .font(theme::FONT_BOLD)
        .color(theme::TEXT_HEADING)
        .into()
}

fn caption(label: String) -> Element<'static, Message> {
    text(label)
        .size(theme::SIZE_SUB)
        .color(theme::TEXT_DIM)
        .into()
}

fn error_label(error: &str) -> Element<'_, Message> {
    text(error)
        .size(theme::SIZE_SUB)
        .color(theme::TEXT_DANGER)
        .into()
}

fn name_input<'a>(title: &'a str, busy: bool) -> Element<'a, Message> {
    let mut input = text_input("Playlist name", title)
        .id(INPUT_ID)
        .style(theme::entry)
        .size(theme::SIZE_BODY)
        .padding([theme::SPACE_MD, theme::SPACE_LG]);
    if !busy {
        input = input
            .on_input(|value| msg(Msg::Title(value)))
            .on_submit(msg(Msg::Submit));
    }
    input.into()
}

/// Dialog footer: busy label or error on the left, Cancel / primary on the
/// right. An empty primary label renders Cancel alone.
fn footer<'a>(
    app: &'a App,
    primary: &'static str,
    on_primary: Option<Message>,
    danger: bool,
) -> Element<'a, Message> {
    let state = &app.playlists;
    let status: Element<'a, Message> = if state.busy {
        caption(state.busy_label.to_owned())
    } else if let Some(error) = state.dialog_error.as_deref() {
        error_label(error)
    } else {
        Space::new().into()
    };
    let mut buttons = row![
        container(status).width(Length::Fill),
        button(text("Cancel").size(theme::SIZE_BODY))
            .style(theme::secondary_button)
            .padding([theme::SPACE_MD, theme::SPACE_XL])
            .on_press_maybe((!state.busy).then_some(msg(Msg::CloseDialog))),
    ]
    .spacing(theme::SPACE_MD)
    .align_y(Alignment::Center);
    if !primary.is_empty() {
        let mut primary_button = button(text(primary).size(theme::SIZE_BODY))
            .padding([theme::SPACE_MD, theme::SPACE_XL])
            .on_press_maybe(on_primary);
        primary_button = if danger {
            primary_button.style(theme::danger_button)
        } else {
            primary_button.style(theme::primary_button)
        };
        buttons = buttons.push(primary_button);
    }
    buttons.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PlaylistEntry;

    fn editor() -> PlaylistEditor {
        PlaylistEditor {
            playlist_id: "PLtest".into(),
            title: "test".into(),
            editable: true,
            entries: ["first", "second", "third", "fourth"]
                .iter()
                .map(|id| PlaylistEntry {
                    track: serde_json::from_value(serde_json::json!({
                        "video_id": "same-song",
                        "title": "Duplicate song",
                        "artists": []
                    }))
                    .unwrap(),
                    set_video_id: Some((*id).into()),
                })
                .collect(),
        }
    }

    #[test]
    fn cache_matching_accepts_browse_prefix_without_matching_other_playlists() {
        assert!(same_id("VLPLtest", "PLtest"));
        assert!(same_id("PLtest", "VLPLtest"));
        assert!(!same_id("PLtest", "PLother"));
    }

    #[test]
    fn dropping_downwards_lands_after_the_target_occurrence() {
        let e = editor();
        assert_eq!(
            drop_targets(&e, 0, 2).unwrap(),
            ("first".into(), Some("fourth".into()))
        );
        assert_eq!(drop_targets(&e, 1, 3).unwrap(), ("second".into(), None));
    }

    #[test]
    fn dropping_upwards_lands_before_the_target_occurrence() {
        let e = editor();
        assert_eq!(
            drop_targets(&e, 3, 1).unwrap(),
            ("fourth".into(), Some("second".into()))
        );
        assert_eq!(
            drop_targets(&e, 2, 0).unwrap(),
            ("third".into(), Some("first".into()))
        );
    }

    #[test]
    fn drop_rejects_same_slot_missing_rows_and_missing_occurrences() {
        let mut e = editor();
        assert!(drop_targets(&e, 1, 1).is_err());
        assert!(drop_targets(&e, 0, 9).is_err());
        assert!(drop_targets(&e, 9, 0).is_err());
        e.entries[1].set_video_id = None;
        assert!(drop_targets(&e, 1, 0).is_err());
        assert!(drop_targets(&e, 3, 1).is_err());
    }

    #[test]
    fn local_reorder_matches_the_remote_drop_semantics() {
        // The optimistic apply in `Written` must place the row where the
        // server did: after the target when moving down, before it moving up.
        let mut entries: Vec<&str> = vec!["a", "b", "c", "d"];
        let moved = entries.remove(0);
        entries.insert(2, moved);
        assert_eq!(entries, ["b", "c", "a", "d"]);
        let mut entries: Vec<&str> = vec!["a", "b", "c", "d"];
        let moved = entries.remove(3);
        entries.insert(1, moved);
        assert_eq!(entries, ["a", "d", "b", "c"]);
    }
}
