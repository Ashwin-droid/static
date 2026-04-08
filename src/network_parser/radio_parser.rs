use anyhow::Result;
use serde_json::Value;

use crate::models::RadioQueue;

pub(crate) fn parse_radio(json: &Value) -> Result<RadioQueue> {
    super::shared_parser::parse_radio(json)
}
