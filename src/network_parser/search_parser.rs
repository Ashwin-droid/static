use anyhow::Result;
use serde_json::Value;

use crate::models::SearchResult;

pub(crate) fn parse_search_results(json: &Value) -> Result<SearchResult> {
    super::shared_parser::parse_search_results(json)
}

pub(crate) fn parse_search_suggestions(json: &Value) -> Vec<String> {
    super::shared_parser::parse_search_suggestions(json)
}
