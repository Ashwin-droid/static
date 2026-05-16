use anyhow::Result;
use serde_json::Value;

use crate::models::{CrossDeviceQueue, RadioQueue};

pub(crate) fn parse_radio(json: &Value) -> Result<RadioQueue> {
    super::shared_parser::parse_radio(json)
}

pub(crate) fn parse_cross_device_queue(json: &Value) -> Result<Option<CrossDeviceQueue>> {
    super::shared_parser::parse_cross_device_queue(json)
}
