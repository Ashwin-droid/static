use anyhow::{Result, anyhow};

use crate::models::HomePage;

use super::session_state::NetworkState;

impl NetworkState {
    pub fn home_page(&self, page_index: usize) -> Result<HomePage> {
        let continuation = {
            let cache = self.home_cache.lock().unwrap();
            cache.tokens_by_page.get(page_index).cloned().flatten()
        };

        if page_index > 0 && continuation.is_none() {
            return Err(anyhow!("Home page {page_index} has not been discovered yet"));
        }

        let page = self.client.get_home_page(continuation.as_deref())?;
        let next_page = page_index + 1;

        let mut cache = self.home_cache.lock().unwrap();
        if cache.tokens_by_page.len() <= page_index {
            cache.tokens_by_page.resize(page_index + 1, None);
        }
        if cache.tokens_by_page.len() <= next_page {
            cache.tokens_by_page.resize(next_page + 1, None);
        }
        cache.tokens_by_page[page_index] = continuation;
        cache.tokens_by_page[next_page] = page.continuation.clone();
        Ok(page)
    }

    pub fn reset_home_pagination(&self) {
        let mut cache = self.home_cache.lock().unwrap();
        cache.tokens_by_page.clear();
        cache.tokens_by_page.push(None);
    }
}
