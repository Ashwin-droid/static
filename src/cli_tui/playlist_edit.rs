use super::*;

pub(super) enum PlaylistDialog {
    Create {
        title: String,
        video_ids: Vec<String>,
    },
    Choose {
        track: Track,
        items: Vec<PlaylistSummary>,
        selected: usize,
    },
    Edit {
        editor: PlaylistEditor,
        selected: usize,
        marked: HashSet<usize>,
    },
    DeleteConfirm {
        editor: PlaylistEditor,
        buffer: String,
    },
    Loading,
}

pub(super) enum PlaylistReply {
    Choices {
        track: Track,
        items: Vec<PlaylistSummary>,
    },
    Editor(PlaylistEditor),
    Done(String),
    ListenAgainUpdated(bool),
    Deleted {
        id: String,
        title: String,
    },
}

impl InteractiveTui {
    pub(super) fn set_remote_listen_again_pin(&mut self, target: ListenAgainTarget, pinned: bool) {
        if self.playlist_busy {
            return;
        }
        self.playlist_job(move || {
            saved_network_state()?.set_listen_again_pin(&target, pinned)?;
            Ok(PlaylistReply::ListenAgainUpdated(pinned))
        });
        self.status_line = "Updating YouTube Music Listen again…".into();
    }

    fn playlist_job(&mut self, job: impl FnOnce() -> Result<PlaylistReply> + Send + 'static) {
        if self.playlist_busy {
            return;
        }
        self.playlist_busy = true;
        self.status_line = "Updating playlist…".into();
        let tx = self.background_tx.clone();
        std::thread::spawn(move || {
            let result = job().map_err(|err| err.to_string());
            let _ = tx.send(TuiBackgroundEvent::PlaylistOperation(result));
        });
    }

    pub(super) fn choose_playlist(&mut self, track: Track) {
        if self.playlist_busy {
            return;
        }
        self.playlist_dialog = Some(PlaylistDialog::Loading);
        self.playlist_job(move || {
            Ok(PlaylistReply::Choices {
                items: saved_network_state()?.get_editable_playlists()?,
                track,
            })
        });
    }

    pub(super) fn create_playlist_prompt(&mut self, radio: bool) {
        if self.playlist_busy {
            return;
        }
        if radio && self.snapshot.radio_virtual_playlist.is_none() {
            self.status_line = "Start a radio before saving it as a playlist.".into();
            return;
        }
        let video_ids = if radio {
            self.snapshot
                .queue
                .iter()
                .map(|t| t.video_id.clone())
                .collect()
        } else {
            vec![]
        };
        self.playlist_dialog = Some(PlaylistDialog::Create {
            title: String::new(),
            video_ids,
        });
    }

    pub(super) fn playlist_page_action(&mut self) {
        if self.playlist_busy {
            return;
        }
        match &self.content_view {
            TuiContentView::Playlists(_) => self.create_playlist_prompt(false),
            TuiContentView::PlaylistDetail(view)
                if view.kind == TuiTrackPageKind::RemotePlaylist =>
            {
                let id = view.key.trim_start_matches("remote:").to_owned();
                self.playlist_dialog = Some(PlaylistDialog::Loading);
                self.playlist_job(move || {
                    Ok(PlaylistReply::Editor(
                        saved_network_state()?.get_playlist_editor(&id)?,
                    ))
                });
            }
            _ => {
                self.status_line =
                    "Open Playlists to create a playlist, or a playlist detail to edit.".into()
            }
        }
    }

    pub(super) fn finish_playlist_operation(&mut self, result: Result<PlaylistReply, String>) {
        self.playlist_busy = false;
        match result {
            Ok(PlaylistReply::Choices { track, items }) => {
                self.status_line = if items.is_empty() {
                    "No editable playlists. Press N to create one with this song."
                } else {
                    "Choose a destination playlist."
                }
                .into();
                self.playlist_dialog = Some(PlaylistDialog::Choose {
                    track,
                    items,
                    selected: 0,
                });
            }
            Ok(PlaylistReply::Editor(editor)) => {
                let selected = match &self.playlist_dialog {
                    Some(PlaylistDialog::Edit {
                        editor: previous,
                        selected,
                        ..
                    }) => refreshed_playlist_selection(previous, *selected, &editor),
                    _ => 0,
                };
                self.status_line = if editor.editable {
                    "Playlist loaded. Space selects, Delete removes songs, [ / ] reorder, D deletes playlist. L/U pin/unpin Listen again."
                } else {
                    "This playlist is read-only. L/U pin/unpin in YouTube Music Listen again."
                }
                .into();
                self.playlist_dialog = Some(PlaylistDialog::Edit {
                    editor,
                    selected,
                    marked: HashSet::new(),
                });
            }
            Ok(PlaylistReply::Deleted { id, title }) => {
                self.playlist_dialog = None;
                self.open_playlists();
                self.content_backstack.retain(|snapshot| !matches!(&snapshot.view, TuiContentView::PlaylistDetail(view) if view.key == format!("remote:{id}")));
                self.status_line = format!("Deleted playlist {title}.");
            }
            Ok(PlaylistReply::ListenAgainUpdated(pinned)) => {
                let tx = self.background_tx.clone();
                std::thread::spawn(move || {
                    let result = saved_network_state()
                        .and_then(|network| network.get_home_page(None))
                        .map_err(|err| err.to_string());
                    let _ = tx.send(TuiBackgroundEvent::HomeLoaded(result));
                });
                self.status_line = if pinned {
                    "Pinned to YouTube Music Listen again."
                } else {
                    "Unpinned from YouTube Music Listen again."
                }
                .into();
            }
            Ok(PlaylistReply::Done(message)) => {
                self.playlist_dialog = None;
                self.refresh_current_view();
                self.status_line = message;
            }
            Err(err) => {
                self.playlist_dialog = None;
                self.status_line = format!(
                    "Playlist request failed: {err}. A submitted change may have succeeded; reopen and inspect the playlist before retrying."
                );
            }
        }
    }

    pub(super) fn playlist_dialog_key(&mut self, code: KeyCode) {
        if self.playlist_busy {
            return;
        }
        if code == KeyCode::Esc {
            if matches!(self.playlist_dialog, Some(PlaylistDialog::Edit { .. })) {
                self.refresh_current_view();
            }
            self.playlist_dialog = None;
            return;
        }
        let Some(mut dialog) = self.playlist_dialog.take() else {
            return;
        };
        match &mut dialog {
            PlaylistDialog::Create { title, video_ids } => match code {
                KeyCode::Char(c) => title.push(c),
                KeyCode::Backspace => {
                    title.pop();
                }
                KeyCode::Enter if !title.trim().is_empty() => {
                    let title = title.trim().to_owned();
                    let ids = video_ids.clone();
                    self.playlist_job(move || {
                        saved_network_state()?.create_playlist(&title, &ids)?;
                        Ok(PlaylistReply::Done(format!(
                            "Created private playlist {title}."
                        )))
                    });
                }
                _ => {}
            },
            PlaylistDialog::Choose {
                track,
                items,
                selected,
            } => match code {
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.playlist_dialog = Some(PlaylistDialog::Create {
                        title: String::new(),
                        video_ids: vec![track.video_id.clone()],
                    });
                    return;
                }
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => *selected = (*selected + 1).min(items.len().saturating_sub(1)),
                KeyCode::Enter => {
                    if let Some(item) = items.get(*selected) {
                        let id = item.playlist_id.clone();
                        let title = item.title.clone();
                        let ids = vec![track.video_id.clone()];
                        self.playlist_job(move || {
                            saved_network_state()?.add_playlist_tracks(&id, &ids)?;
                            Ok(PlaylistReply::Done(format!("Added song to {title}.")))
                        });
                    }
                }
                _ => {}
            },
            PlaylistDialog::DeleteConfirm { editor, buffer } => match code {
                KeyCode::Char(c) => buffer.push(c),
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Enter if playlist_delete_confirmed(editor, buffer) => {
                    let id = editor.playlist_id.clone();
                    let title = editor.title.clone();
                    self.playlist_job(move || {
                        saved_network_state()?.delete_playlist(&id)?;
                        Ok(PlaylistReply::Deleted { id, title })
                    });
                }
                _ => {}
            },
            PlaylistDialog::Edit {
                editor,
                selected,
                marked,
            } => match code {
                KeyCode::Char('L') | KeyCode::Char('U') => {
                    self.set_remote_listen_again_pin(
                        ListenAgainTarget::Playlist(editor.playlist_id.clone()),
                        code == KeyCode::Char('L'),
                    );
                }
                KeyCode::Char('D') if editor.editable => {
                    self.playlist_dialog = Some(PlaylistDialog::DeleteConfirm {
                        editor: editor.clone(),
                        buffer: String::new(),
                    });
                    return;
                }
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    *selected = (*selected + 1).min(editor.entries.len().saturating_sub(1))
                }
                KeyCode::Char(' ') if editor.editable && !editor.entries.is_empty() => {
                    if !marked.remove(selected) {
                        marked.insert(*selected);
                    }
                }
                KeyCode::Delete if editor.editable && !editor.entries.is_empty() => {
                    let entries: Vec<_> = editor
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| {
                            if marked.is_empty() {
                                *i == *selected
                            } else {
                                marked.contains(i)
                            }
                        })
                        .map(|(_, e)| e.clone())
                        .collect();
                    if entries.iter().any(|e| e.set_video_id.is_none()) {
                        self.status_line =
                            "Cannot remove entries without playlist occurrence IDs.".into();
                    } else {
                        let id = editor.playlist_id.clone();
                        self.playlist_job(move || {
                            let network = saved_network_state()?;
                            network.remove_playlist_entries(&id, &entries)?;
                            Ok(match network.get_playlist_editor(&id) {
                                Ok(editor) => PlaylistReply::Editor(editor),
                                Err(err) => PlaylistReply::Done(format!("Playlist saved, but refresh failed: {err}. Reopen the editor to reload before editing again.")),
                            })
                        });
                    }
                }
                KeyCode::Char('[') | KeyCode::Char(']') if editor.editable => {
                    if let Some((entry, before)) =
                        playlist_move_ids(editor, *selected, code == KeyCode::Char(']'))
                    {
                        let id = editor.playlist_id.clone();
                        self.playlist_job(move || {
                            let network = saved_network_state()?;
                            network.move_playlist_entry(&id, &entry, before.as_deref())?;
                            Ok(match network.get_playlist_editor(&id) {
                                Ok(editor) => PlaylistReply::Editor(editor),
                                Err(err) => PlaylistReply::Done(format!("Playlist saved, but refresh failed: {err}. Reopen the editor to reload before editing again.")),
                            })
                        });
                    }
                }
                _ => {}
            },
            PlaylistDialog::Loading => {}
        }
        self.playlist_dialog = Some(dialog);
    }

    pub(super) fn render_playlist_dialog(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &TuiTheme,
    ) {
        let Some(dialog) = &self.playlist_dialog else {
            return;
        };
        let width = area.width.saturating_sub(4).min(90);
        let height = area.height.saturating_sub(4).min(24);
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let mut lines = Vec::new();
        let mut selection = 0;
        let title = match dialog {
            PlaylistDialog::Loading => {
                lines.push(Line::from("Loading playlists…"));
                "Playlists".to_owned()
            }
            PlaylistDialog::Create { title, video_ids } => {
                lines.push(Line::from(format!(
                    "Private playlist · {} songs",
                    video_ids.len()
                )));
                lines.push(Line::from(format!("Name: {title}▏")));
                "Create playlist · Enter saves · Esc cancels".into()
            }
            PlaylistDialog::Choose {
                items, selected, ..
            } => {
                selection = *selected;
                for (i, item) in items.iter().enumerate() {
                    lines.push(Line::from(format!(
                        "{} {}",
                        if i == *selected { "›" } else { " " },
                        item.title
                    )));
                }
                "Add to playlist · Enter adds · N new · Esc closes".into()
            }
            PlaylistDialog::DeleteConfirm { editor, buffer } => {
                lines.push(Line::from(format!("Delete playlist: {}", editor.title)));
                lines.push(Line::from("This permanently deletes the remote playlist."));
                lines.push(Line::from(
                    "Type DELETE and press Enter to confirm. Esc cancels.",
                ));
                lines.push(Line::from(format!("> {buffer}▏")));
                "Delete playlist confirmation".into()
            }
            PlaylistDialog::Edit {
                editor,
                selected,
                marked,
            } => {
                selection = *selected;
                for (i, entry) in editor.entries.iter().enumerate() {
                    lines.push(Line::from(format!(
                        "{} [{}] {}",
                        if i == *selected { "›" } else { " " },
                        if marked.contains(&i) { "x" } else { " " },
                        entry.track.title
                    )));
                }
                format!(
                    "{} · Del songs · [/] move · D delete · L/U pin/unpin · Esc",
                    editor.title
                )
            }
        };
        if self.playlist_busy {
            lines.insert(0, Line::from("Saving…"));
        }
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((
                    selection.saturating_sub(height.saturating_sub(4) as usize) as u16,
                    0,
                ))
                .style(Style::default().fg(theme.text).bg(theme.elevated_alt_bg))
                .block(Block::default().borders(Borders::ALL).title(title)),
            rect,
        );
    }
}

pub(super) fn playlist_move_ids(
    editor: &PlaylistEditor,
    index: usize,
    down: bool,
) -> Option<(String, Option<String>)> {
    if index >= editor.entries.len()
        || (!down && index == 0)
        || (down && index + 1 >= editor.entries.len())
    {
        return None;
    }
    let moving = editor.entries[index].set_video_id.clone()?;
    let before_index = if down { index + 2 } else { index - 1 };
    let before = match editor.entries.get(before_index) {
        Some(entry) => Some(entry.set_video_id.clone()?),
        None => None,
    };
    Some((moving, before))
}

pub(super) fn refreshed_playlist_selection(
    previous: &PlaylistEditor,
    selected: usize,
    current: &PlaylistEditor,
) -> usize {
    if previous.playlist_id == current.playlist_id {
        if let Some(id) = previous
            .entries
            .get(selected)
            .and_then(|entry| entry.set_video_id.as_deref())
        {
            if let Some(index) = current
                .entries
                .iter()
                .position(|entry| entry.set_video_id.as_deref() == Some(id))
            {
                return index;
            }
        }
    }
    selected.min(current.entries.len().saturating_sub(1))
}

pub(super) fn playlist_delete_confirmed(editor: &PlaylistEditor, buffer: &str) -> bool {
    editor.editable && !editor.playlist_id.is_empty() && buffer == "DELETE"
}
