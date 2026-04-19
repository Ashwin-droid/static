use anyhow::Result;

use crate::models::SearchResult;

use super::session_state::NetworkState;

impl NetworkState {
    pub fn search(&self, query: &str) -> Result<SearchResult> {
        self.client.search(query)
    }

    pub fn search_suggestions(&self, query: &str) -> Result<Vec<String>> {
        self.client.get_search_suggestions(query)
    }

    pub fn get_search_suggestions(&self, query: &str) -> Result<Vec<String>> {
        self.search_suggestions(query)
    }
}
