use anyhow::Result;
use serde_json::Value;

use crate::models::{HomePage, HomeSection};

pub(crate) fn parse_home(json: &Value) -> Result<Vec<HomeSection>> {
    super::shared_parser::parse_home(json)
}

pub(crate) fn parse_home_page(json: &Value) -> Result<HomePage> {
    super::shared_parser::parse_home_page(json)
}
