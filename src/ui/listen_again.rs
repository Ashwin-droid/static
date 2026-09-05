//! Remote Listen again pins. UI state mirrors the server; writes go to YouTube.
use super::app::{self, App, ContentView, Message};
use crate::models::ListenAgainTarget;
use iced::{Element, Task};
use std::collections::HashMap;

#[derive(Default)]
pub struct State {
    serial: u64,
    entries: HashMap<String, PinState>,
}
#[derive(Default)]
struct PinState {
    serial: u64,
    pinned: Option<bool>,
    busy: bool,
    error: Option<String>,
}
#[derive(Clone)]
pub enum Msg {
    Load(ListenAgainTarget),
    Loaded(ListenAgainTarget, u64, Result<bool, String>),
    Toggle(ListenAgainTarget, u64),
    Saved(ListenAgainTarget, u64, bool, Result<(), String>),
}
fn msg(value: Msg) -> Message {
    Message::ListenAgain(value)
}
fn key(target: &ListenAgainTarget) -> String {
    match target {
        ListenAgainTarget::Track(id) => format!("track:{id}"),
        ListenAgainTarget::Playlist(id) => {
            format!("playlist:{}", id.strip_prefix("VL").unwrap_or(id))
        }
    }
}
pub fn load(app: &mut App, target: ListenAgainTarget) -> Task<Message> {
    update(app, Msg::Load(target))
}
pub fn update(app: &mut App, value: Msg) -> Task<Message> {
    match value {
        Msg::Load(target) => {
            let key = key(&target);
            if app
                .listen_again
                .entries
                .get(&key)
                .is_some_and(|state| state.busy)
            {
                return Task::none();
            }
            app.listen_again.serial = app.listen_again.serial.wrapping_add(1);
            let serial = app.listen_again.serial;
            app.listen_again.entries.insert(
                key,
                PinState {
                    serial,
                    busy: true,
                    ..PinState::default()
                },
            );
            let client = app.app_client();
            let request_target = target.clone();
            let panic_target = target.clone();
            return app::blocking_or(
                move || {
                    client
                        .ok_or_else(|| "Not connected.".to_string())?
                        .get_listen_again_pin(&request_target)
                        .map(|pin| pin.pinned)
                        .map_err(|e| e.to_string())
                },
                move |result| msg(Msg::Loaded(target.clone(), serial, result)),
                msg(Msg::Loaded(
                    panic_target,
                    serial,
                    Err("Could not load Listen again pin.".into()),
                )),
            );
        }
        Msg::Loaded(target, serial, result) => {
            if let Some(state) = app
                .listen_again
                .entries
                .get_mut(&key(&target))
                .filter(|s| s.serial == serial)
            {
                state.busy = false;
                match result {
                    Ok(pinned) => {
                        state.pinned = Some(pinned);
                        state.error = None;
                    }
                    Err(error) => {
                        state.error = Some(error);
                    }
                }
            }
        }
        Msg::Toggle(target, serial) => {
            let Some(state) = app
                .listen_again
                .entries
                .get_mut(&key(&target))
                .filter(|s| s.serial == serial)
            else {
                return Task::none();
            };
            let Some(pinned) = desired_pin(state) else {
                return Task::none();
            };
            state.busy = true;
            // The write reports through a toast; the menu it came from closes.
            app.track_menu = None;
            app.playlists.dismiss_menu();
            let client = app.app_client();
            let request_target = target.clone();
            let panic_target = target.clone();
            return app::blocking_or(
                move || {
                    client
                        .ok_or_else(|| "Not connected.".to_string())?
                        .set_listen_again_pin(&request_target, pinned)
                        .map_err(|e| e.to_string())
                },
                move |result| msg(Msg::Saved(target.clone(), serial, pinned, result)),
                msg(Msg::Saved(
                    panic_target,
                    serial,
                    pinned,
                    Err(
                        "Could not verify Listen again pin. Reload its state before retrying."
                            .into(),
                    ),
                )),
            );
        }
        Msg::Saved(target, serial, pinned, result) => {
            let Some(state) = app
                .listen_again
                .entries
                .get_mut(&key(&target))
                .filter(|s| s.serial == serial)
            else {
                return Task::none();
            };
            state.busy = false;
            match result {
                Ok(()) => {
                    state.pinned = Some(pinned);
                    state.error = None;
                    app.invalidate_home_cache();
                    let toast = app.show_toast(if pinned {
                        "Pinned to Listen again."
                    } else {
                        "Unpinned from Listen again."
                    });
                    if app.content_view == ContentView::Home {
                        return Task::batch([
                            toast,
                            super::content_loading::load_home_task(app, true),
                        ]);
                    }
                    return toast;
                }
                Err(error) => {
                    // Keep the last confirmed value, but require a new remote read.
                    state.error = Some(error.clone());
                    return app.show_toast(error);
                }
            }
        }
    }
    Task::none()
}
fn desired_pin(state: &PinState) -> Option<bool> {
    if state.busy || state.error.is_some() {
        None
    } else {
        state.pinned.map(|p| !p)
    }
}
/// Label + action for the current pin state, shared by the menu row and the
/// page button. `None` action means inert.
fn presentation(app: &App, target: &ListenAgainTarget) -> (Presentation, Option<Msg>) {
    let Some(state) = app.listen_again.entries.get(&key(target)) else {
        return (Presentation::Loading, None);
    };
    if state.busy {
        return (
            if state.pinned.is_none() {
                Presentation::Loading
            } else {
                Presentation::Updating
            },
            None,
        );
    }
    if state.error.is_some() {
        // A failed write keeps the last confirmed value but demands a fresh
        // read; a failed read means the item cannot be pinned from here.
        return if state.pinned.is_some() {
            (Presentation::Retry, Some(Msg::Load(target.clone())))
        } else {
            (Presentation::Unavailable, None)
        };
    }
    match state.pinned {
        Some(true) => (
            Presentation::Pinned,
            Some(Msg::Toggle(target.clone(), state.serial)),
        ),
        Some(false) => (
            Presentation::Unpinned,
            Some(Msg::Toggle(target.clone(), state.serial)),
        ),
        None => (Presentation::Loading, None),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Presentation {
    Loading,
    Updating,
    Pinned,
    Unpinned,
    Retry,
    Unavailable,
}

/// Context-menu row: "Pin to Listen again" / "Unpin from Listen again".
pub fn menu_item(app: &App, target: ListenAgainTarget) -> Element<'static, Message> {
    let (presentation, action) = presentation(app, &target);
    let label = match presentation {
        Presentation::Loading => "Pin to Listen again",
        Presentation::Updating => "Updating Listen again\u{2026}",
        Presentation::Pinned => "Unpin from Listen again",
        Presentation::Unpinned => "Pin to Listen again",
        Presentation::Retry => "Reload Listen again pin",
        Presentation::Unavailable => "Listen again unavailable",
    };
    super::queue_panel::menu_action(label, action.map(msg), false)
}

/// Page-control button: "Pin" / "Unpin" beside Play All. Hidden while the pin
/// state is unknown or the item cannot be pinned.
pub fn page_button(app: &App, target: ListenAgainTarget) -> Option<Element<'static, Message>> {
    let (presentation, action) = presentation(app, &target);
    let label = match presentation {
        Presentation::Loading | Presentation::Unavailable => return None,
        Presentation::Updating => "Updating\u{2026}",
        Presentation::Pinned => "Unpin",
        Presentation::Unpinned => "Pin",
        Presentation::Retry => "Reload pin",
    };
    Some(super::playlists::control_button(
        super::icons::pin(),
        label,
        action.map(msg),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn toggle_requires_confirmed_remote_state_and_no_pending_or_failed_write() {
        let mut state = PinState::default();
        assert_eq!(desired_pin(&state), None);
        state.pinned = Some(false);
        assert_eq!(desired_pin(&state), Some(true));
        state.pinned = Some(true);
        assert_eq!(desired_pin(&state), Some(false));
        state.busy = true;
        assert_eq!(desired_pin(&state), None);
        state.busy = false;
        state.error = Some("transport failed".into());
        assert_eq!(desired_pin(&state), None);
        assert_eq!(state.pinned, Some(true));
    }
    #[test]
    fn targets_keep_track_and_playlist_distinct_and_normalize_browse_prefix() {
        assert_ne!(
            key(&ListenAgainTarget::Track("PLx".into())),
            key(&ListenAgainTarget::Playlist("PLx".into()))
        );
        assert_eq!(
            key(&ListenAgainTarget::Playlist("VLPLx".into())),
            key(&ListenAgainTarget::Playlist("PLx".into()))
        );
    }
}
