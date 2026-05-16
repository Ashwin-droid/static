use super::*;
use super::{content_loading::*, thumbnails::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn new(client: DaemonClient) -> Result<Self> {
        let snapshot = client.state()?;
        let (background_tx, background_rx) = mpsc::channel();
        spawn_home_bootstrap(background_tx.clone());

        let mut app = Self {
            client,
            snapshot: snapshot.clone(),
            queue_selected: snapshot
                .queue_index
                .min(snapshot.queue.len().saturating_sub(1)),
            queue_scroll_offset: 0,
            queue_viewport_rows: 10,
            recent_tracks: Vec::new(),
            recent_selected: 0,
            recent_scroll_offset: 0,
            recent_viewport_rows: 8,
            home_status: HomeFeedStatus::Loading,
            home_doc: HomeDocument::default(),
            home_selected: 0,
            home_scroll_offset: 0,
            home_viewport_rows: 12,
            home_loading_more: false,
            content_view: TuiContentView::Home,
            content_backstack: Vec::new(),
            search_query: String::new(),
            search_input_active: false,
            search_suggestions: Vec::new(),
            search_suggestions_loading: false,
            search_suggestion_selected: 0,
            search_suggestion_request_id: 0,
            search_suggestion_debounce_started_at: None,
            search_suggestion_inflight: None,
            submitted_search_query_to_clear: None,
            completed_search_query_to_clear: None,
            radio_chip_scroll_offset: 0,
            settings_field_offsets: Vec::new(),
            settings_total_height: 0,
            settings_text_edit: None,
            focus: TuiFocus::Home,
            player_selected: 1,
            detail_tab: DetailTab::Queue,
            resume_card: None,
            lyrics_state: LyricsState::Idle,
            status_line: "Use / or click Search to query YouTube Music. Home stays cached now."
                .to_string(),
            last_snapshot_at: Instant::now(),
            last_daemon_poll_at: Instant::now(),
            feedback_undo_ready: HashSet::new(),
            click_targets: Vec::new(),
            rendered_layout: RenderedLayout::default(),
            background_tx,
            background_rx,
            thumbnail_picker: None,
            thumbnail_assets: HashMap::new(),
            thumbnail_requests: HashSet::new(),
            thumbnail_slots: HashMap::new(),
            thumbnail_mode_label: "art off".to_string(),
            pending_mouse_press: None,
            drag_state: None,
            context_menu: None,
            rendered_context_menu: RenderedContextMenu::default(),
            help_visible: false,
            rendered_help_rect: None,
        };
        app.maybe_request_lyrics();
        Ok(app)
    }

    pub(in crate::cli_tui) fn run(&mut self) -> Result<()> {
        let _guard = TerminalGuard::enter()?;
        self.initialize_terminal_images();
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).context("Failed to initialize terminal UI")?;
        terminal.clear().ok();

        loop {
            self.drain_background_events();
            self.maybe_request_search_suggestions();

            let mut click_targets = Vec::new();
            let mut rendered_layout = RenderedLayout::default();
            terminal
                .draw(|frame| {
                    let (targets, layout) = self.render_frame(frame);
                    click_targets = targets;
                    rendered_layout = layout;
                })
                .context("Failed to draw TUI frame")?;
            self.click_targets = click_targets;
            self.rendered_layout = rendered_layout;

            let timeout = UI_TICK_INTERVAL;

            if event::poll(timeout).context("Failed to poll terminal events")? {
                match event::read().context("Failed to read terminal event")? {
                    Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                        if self.handle_key(key_event)? {
                            break;
                        }
                    }
                    Event::Mouse(mouse_event) => self.handle_mouse(mouse_event)?,
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }

            if self.last_daemon_poll_at.elapsed() >= DAEMON_POLL_INTERVAL {
                self.refresh_snapshot();
            }
        }

        Ok(())
    }

    pub(in crate::cli_tui) fn initialize_terminal_images(&mut self) {
        let (picker, queried_stdio) = match Picker::from_query_stdio() {
            Ok(picker) => (picker, true),
            Err(_) => (Picker::from_fontsize((10, 20)), false),
        };

        self.thumbnail_mode_label = if queried_stdio {
            format!("art {}", thumbnail_protocol_label(picker.protocol_type()))
        } else {
            format!("art {}?", thumbnail_protocol_label(picker.protocol_type()))
        };
        self.thumbnail_picker = Some(picker);
    }

    pub(in crate::cli_tui) fn terminal_images_enabled(&self) -> bool {
        self.thumbnail_picker.is_some()
    }

    pub(in crate::cli_tui) fn current_nav_item(&self) -> Option<TuiNavItem> {
        match &self.content_view {
            TuiContentView::Home => Some(TuiNavItem::Home),
            TuiContentView::Likes(_) => Some(TuiNavItem::Likes),
            TuiContentView::Playlists(_) => Some(TuiNavItem::Playlists),
            TuiContentView::PlaylistDetail(view) => match view.kind {
                TuiTrackPageKind::Likes => Some(TuiNavItem::Likes),
                TuiTrackPageKind::RemotePlaylist => Some(TuiNavItem::Playlists),
                TuiTrackPageKind::OfflinePlaylist => Some(TuiNavItem::Offline),
            },
            TuiContentView::Offline(_) => Some(TuiNavItem::Offline),
            TuiContentView::Settings(_) => Some(TuiNavItem::Settings),
            TuiContentView::Browse(_) | TuiContentView::Search(_) => None,
        }
    }

    pub(in crate::cli_tui) fn content_has_back_button(&self) -> bool {
        matches!(
            self.content_view,
            TuiContentView::Browse(_)
                | TuiContentView::PlaylistDetail(_)
                | TuiContentView::Search(_)
        )
    }
}
