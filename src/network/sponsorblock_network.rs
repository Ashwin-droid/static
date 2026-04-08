use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;

const API_BASE_URL: &str = "https://sponsor.ajay.app/api";
const SERVICE: &str = "YouTube";
const SKIP_ACTIONS_JSON: &str = r#"["skip"]"#;

#[derive(Debug, Clone, Copy)]
pub struct SponsorCategoryOption {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

pub const SPONSORBLOCK_CATEGORY_OPTIONS: [SponsorCategoryOption; 7] = [
    SponsorCategoryOption {
        key: "sponsor",
        label: "Sponsor",
        description: "Paid placements and direct sponsor reads.",
    },
    SponsorCategoryOption {
        key: "selfpromo",
        label: "Self-promo",
        description: "Calls to subscribe, follow, or check out the creator's own work.",
    },
    SponsorCategoryOption {
        key: "interaction",
        label: "Interaction",
        description: "Like, comment, subscribe, bell, and audience-engagement asks.",
    },
    SponsorCategoryOption {
        key: "intro",
        label: "Intro",
        description: "Non-musical intros before the actual content starts.",
    },
    SponsorCategoryOption {
        key: "outro",
        label: "Outro",
        description: "End cards, wrap-up chatter, and post-content outros.",
    },
    SponsorCategoryOption {
        key: "preview",
        label: "Preview",
        description: "Teasers or recaps for upcoming or previous content.",
    },
    SponsorCategoryOption {
        key: "music_offtopic",
        label: "Music off-topic",
        description: "Non-music sections inside music uploads, like skits or commentary.",
    },
];

pub fn default_skip_categories() -> Vec<String> {
    SPONSORBLOCK_CATEGORY_OPTIONS
        .iter()
        .map(|option| option.key.to_string())
        .collect()
}

pub fn sanitize_categories(categories: &[String]) -> Vec<String> {
    let mut sanitized = Vec::new();
    for option in SPONSORBLOCK_CATEGORY_OPTIONS {
        if categories.iter().any(|category| category == option.key) {
            sanitized.push(option.key.to_string());
        }
    }
    sanitized
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SponsorSegment {
    #[serde(rename = "UUID")]
    pub uuid: String,
    pub category: String,
    #[serde(rename = "actionType", default)]
    pub action_type: String,
    pub segment: Vec<f64>,
}

pub fn normalize_segments(mut segments: Vec<SponsorSegment>) -> Vec<SponsorSegment> {
    let allowed_categories: HashSet<&'static str> = SPONSORBLOCK_CATEGORY_OPTIONS
        .iter()
        .map(|option| option.key)
        .collect();

    segments.retain(|segment| {
        segment.bounds().is_some()
            && segment.action_type == "skip"
            && allowed_categories.contains(segment.category.as_str())
    });
    segments.sort_by(|a, b| {
        let a_start = a.bounds().map(|(start, _)| start).unwrap_or(f64::MAX);
        let b_start = b.bounds().map(|(start, _)| start).unwrap_or(f64::MAX);
        a_start
            .partial_cmp(&b_start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    segments
}

pub fn filter_segments_for_categories(
    segments: &[SponsorSegment],
    categories: &[String],
) -> Vec<SponsorSegment> {
    let categories = sanitize_categories(categories);
    if categories.is_empty() {
        return Vec::new();
    }

    let allowed: HashSet<&str> = categories.iter().map(String::as_str).collect();
    normalize_segments(
        segments
            .iter()
            .filter(|segment| allowed.contains(segment.category.as_str()))
            .cloned()
            .collect(),
    )
}

impl SponsorSegment {
    pub fn bounds(&self) -> Option<(f64, f64)> {
        let start = *self.segment.first()?;
        let end = *self.segment.get(1)?;
        if !start.is_finite() || !end.is_finite() || end <= start {
            return None;
        }
        Some((start.max(0.0), end))
    }

    pub fn should_skip_at(&self, position_seconds: f64) -> bool {
        let Some((start, end)) = self.bounds() else {
            return false;
        };

        let lead_in = if self.category == "intro" || self.category == "outro" {
            0.25
        } else {
            0.1
        };
        let safe_end = 0.05;
        position_seconds + lead_in >= start && position_seconds + safe_end < end
    }

    pub fn seek_target_seconds(&self, position_seconds: f64) -> Option<f64> {
        let (_, end) = self.bounds()?;
        Some((end + 0.05).max(position_seconds + 0.05))
    }
}

#[derive(Clone)]
pub struct SponsorBlockClient {
    http: reqwest::blocking::Client,
}

impl SponsorBlockClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(format!("Static/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Failed to build SponsorBlock HTTP client")?;

        Ok(Self { http })
    }

    pub fn fetch_skip_segments(
        &self,
        video_id: &str,
        categories: &[String],
    ) -> Result<Vec<SponsorSegment>> {
        let categories = sanitize_categories(categories);
        if categories.is_empty() {
            return Ok(Vec::new());
        }

        let categories_json = json!(categories).to_string();
        let response = self
            .http
            .get(format!("{API_BASE_URL}/skipSegments"))
            .query(&[
                ("videoID", video_id),
                ("categories", categories_json.as_str()),
                ("actionTypes", SKIP_ACTIONS_JSON),
                ("service", SERVICE),
            ])
            .send()
            .with_context(|| format!("Failed to request SponsorBlock segments for {video_id}"))?;

        match response.status() {
            StatusCode::OK => {
                let segments = response
                    .json::<Vec<SponsorSegment>>()
                    .context("Failed to decode SponsorBlock response")?;
                Ok(normalize_segments(segments))
            }
            StatusCode::NOT_FOUND => Ok(Vec::new()),
            status => {
                let body = response.text().unwrap_or_default();
                bail!("SponsorBlock returned {status}: {body}");
            }
        }
    }

    pub fn mark_segment_viewed(&self, uuid: &str) -> Result<()> {
        let response = self
            .http
            .post(format!("{API_BASE_URL}/viewedVideoSponsorTime"))
            .query(&[("UUID", uuid)])
            .send()
            .with_context(|| format!("Failed to report SponsorBlock view for segment {uuid}"))?;

        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            bail!("SponsorBlock returned {status}: {body}");
        }
    }
}
