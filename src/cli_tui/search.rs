use super::*;
use super::{content_loading::*, widgets::*};

impl InteractiveTui {
    pub(in crate::cli_tui) fn activate_search_input(&mut self) {
        self.search_input_active = true;
        self.search_suggestion_selected = 0;
        self.schedule_search_suggestions();
        self.status_line =
            "Search mode: type a query, use ↑↓ for suggestions, Tab to fill, Enter to search."
                .to_string();
    }

    pub(in crate::cli_tui) fn clear_completed_search_query_if_leaving_search(&mut self) {
        let Some(query) = self.completed_search_query_to_clear.take() else {
            return;
        };

        let leaving_completed_search = matches!(
            &self.content_view,
            TuiContentView::Search(view)
                if view.query == query && matches!(view.status, TuiSearchStatus::Ready(_))
        );
        if leaving_completed_search && self.search_query.trim() == query {
            self.search_query.clear();
        }
    }

    pub(in crate::cli_tui) fn clear_search_suggestions(&mut self) {
        self.search_suggestions.clear();
        self.search_suggestions_loading = false;
        self.search_suggestion_selected = 0;
        self.search_suggestion_debounce_started_at = None;
        self.search_suggestion_inflight = None;
    }

    pub(in crate::cli_tui) fn schedule_search_suggestions(&mut self) {
        if !self.search_input_active {
            self.clear_search_suggestions();
            return;
        }

        self.search_suggestions.clear();
        self.search_suggestions_loading = true;
        self.search_suggestion_selected = 0;
        self.search_suggestion_debounce_started_at = Some(Instant::now());
    }

    pub(in crate::cli_tui) fn maybe_request_search_suggestions(&mut self) {
        let Some(started_at) = self.search_suggestion_debounce_started_at else {
            return;
        };
        if started_at.elapsed() < UI_TICK_INTERVAL {
            return;
        }

        let query = self.search_query.trim().to_string();
        self.search_suggestion_debounce_started_at = None;

        if !self.search_input_active {
            self.clear_search_suggestions();
            return;
        }

        if self
            .search_suggestion_inflight
            .as_ref()
            .is_some_and(|(_, current)| current == &query)
        {
            return;
        }

        self.search_suggestion_request_id = self.search_suggestion_request_id.wrapping_add(1);
        let request_id = self.search_suggestion_request_id;
        self.search_suggestion_inflight = Some((request_id, query.clone()));
        self.search_suggestions_loading = true;
        spawn_search_suggestions_load(self.background_tx.clone(), request_id, query);
    }

    pub(in crate::cli_tui) fn move_search_suggestion_selection(&mut self, delta: isize) {
        if self.search_suggestions.is_empty() {
            return;
        }
        self.search_suggestion_selected = move_index(
            self.search_suggestion_selected,
            self.search_suggestions.len(),
            delta,
        );
    }

    pub(in crate::cli_tui) fn fill_selected_search_suggestion(&mut self) {
        let Some(suggestion) = self
            .search_suggestions
            .get(self.search_suggestion_selected)
            .cloned()
        else {
            return;
        };
        self.search_query = suggestion;
        self.schedule_search_suggestions();
        self.status_line = "Search suggestion filled.".to_string();
    }

    pub(in crate::cli_tui) fn submit_search(&mut self, preferred_query: Option<String>) {
        self.search_input_active = false;
        self.search_suggestion_debounce_started_at = None;
        self.search_suggestion_inflight = None;
        self.search_suggestions_loading = false;

        let query = preferred_query
            .unwrap_or_else(|| self.search_query.trim().to_string())
            .trim()
            .to_string();
        self.search_suggestions.clear();
        self.search_suggestion_selected = 0;

        if query.is_empty() {
            self.status_line = "Type a query first.".to_string();
            return;
        }

        self.submitted_search_query_to_clear = Some(query.clone());
        self.completed_search_query_to_clear = None;
        self.search_query = query.clone();
        self.open_search(query);
    }
}
