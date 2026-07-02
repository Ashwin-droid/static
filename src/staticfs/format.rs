//! fsmod serialization (contract B4) — `FsMode` plus the pure serializer
//! functions every slice renders through.
//!
//! Contract B4 (verbatim):
//! "`settings/fsmod` holds a per-mount session mode: `awk` (default) | `jq`.
//! Stored in the staticfs process only (e.g. AtomicU8/Mutex). Writing
//! anything else → EINVAL.
//! - Scalar files (single value: position, volume, play_count, state,
//!   ducked, ...) are ALWAYS raw value + trailing \"\\n\" regardless of fsmod.
//! - awk mode: one record per line, fields TAB-separated, column order as
//!   specced per file. No header line.
//! - jq mode: serde_json — lists → JSON array of objects, records → single
//!   JSON object, key/value files (overview) → single JSON object. Trailing
//!   \"\\n\". Use serde_json::to_string (compact), not pretty."
//!
//! Awk-mode escaping policy: TAB is the field separator and NEWLINE the
//! record separator, so any embedded `\t`, `\n`, or `\r` inside a field value
//! (or key/value key) is replaced with a single space. jq mode never mangles
//! values — serde_json string escaping preserves them exactly.
//!
//! jq-mode key order: objects keep the caller-specced field/column order
//! (serde_json's default Map sorts keys, so objects are assembled manually
//! with serde_json::to_string handling string escaping).

/// Per-mount session output mode (contract B4). Default is `awk`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum FsMode {
    #[default]
    Awk,
    Jq,
}

impl FsMode {
    /// Parses a token written to `settings/fsmod`: "awk" | "jq" (already
    /// trimmed by the write path). Anything else -> `None` (the caller maps
    /// that to EINVAL).
    pub(crate) fn parse(token: &str) -> Option<FsMode> {
        match token {
            "awk" => Some(FsMode::Awk),
            "jq" => Some(FsMode::Jq),
            _ => None,
        }
    }

    /// The value rendered when reading `settings/fsmod`.
    pub(crate) fn label(self) -> &'static str {
        match self {
            FsMode::Awk => "awk",
            FsMode::Jq => "jq",
        }
    }
}

/// Scalar files: ALWAYS the raw value + trailing "\n" regardless of fsmod;
/// an empty value renders as empty content (no newline).
pub(crate) fn scalar(value: &str) -> Vec<u8> {
    if value.is_empty() {
        return Vec::new();
    }
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
}

/// A single record (e.g. playback/track, playback/radio/seed). `fields` is
/// the specced column order as (key, value) pairs.
/// - awk: one line, values TAB-separated (keys omitted). No header line.
/// - jq: one compact JSON object + trailing "\n".
///
/// No fields (nothing playing / no radio session) -> empty content (both modes).
pub(crate) fn record(mode: FsMode, fields: &[(&str, String)]) -> Vec<u8> {
    if fields.is_empty() {
        return Vec::new();
    }
    match mode {
        FsMode::Awk => {
            let mut line = fields
                .iter()
                .map(|(_, value)| awk_field(value))
                .collect::<Vec<_>>()
                .join("\t");
            line.push('\n');
            line.into_bytes()
        }
        FsMode::Jq => {
            let mut out = json_object(fields.iter().map(|(key, value)| (*key, value.as_str())));
            out.push('\n');
            out.into_bytes()
        }
    }
}

/// Awk mode cannot represent TABs (field separator) or newlines (record
/// separator) inside a field, so each embedded `\t`, `\n`, or `\r` is
/// replaced with a single space.
fn awk_field(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

/// A compact JSON object with keys in iteration order (serde_json's default
/// Map sorts keys alphabetically, which would break the specced column
/// order). serde_json::to_string on each &str does the escaping.
fn json_object<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let body = pairs
        .map(|(key, value)| {
            format!(
                "{}:{}",
                serde_json::to_string(key).expect("string serialization is infallible"),
                serde_json::to_string(value).expect("string serialization is infallible"),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{body}}}")
}

/// A list file (queue items, active_peers, radio chips, stats top lists).
/// `columns` is the specced column order; each row supplies one value per
/// column.
/// - awk: one record per line, fields TAB-separated. No header line.
/// - jq: JSON array of objects keyed by `columns` + trailing "\n".
///
/// Empty `rows` -> empty content in awk mode, "[]\n" in jq mode.
///
/// Rows shorter than `columns` are padded with empty strings; extra values
/// beyond `columns` are ignored (callers are expected to match exactly).
pub(crate) fn list(mode: FsMode, columns: &[&str], rows: &[Vec<String>]) -> Vec<u8> {
    match mode {
        FsMode::Awk => {
            let mut out = String::new();
            for row in rows {
                let line = (0..columns.len())
                    .map(|i| awk_field(row.get(i).map(String::as_str).unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join("\t");
                out.push_str(&line);
                out.push('\n');
            }
            out.into_bytes()
        }
        FsMode::Jq => {
            let objects =
                rows.iter()
                    .map(|row| {
                        json_object(
                            columns.iter().enumerate().map(|(i, key)| {
                                (*key, row.get(i).map(String::as_str).unwrap_or(""))
                            }),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
            format!("[{objects}]\n").into_bytes()
        }
    }
}

/// A key/value file (stats/overview), keys in specced order.
/// - awk: "key\tvalue" lines.
/// - jq: single compact JSON object + trailing "\n".
///
/// Empty `pairs` -> empty content in awk mode, "{}\n" in jq mode.
pub(crate) fn key_values(mode: FsMode, pairs: &[(&str, String)]) -> Vec<u8> {
    match mode {
        FsMode::Awk => {
            let mut out = String::new();
            for (key, value) in pairs {
                out.push_str(&awk_field(key));
                out.push('\t');
                out.push_str(&awk_field(value));
                out.push('\n');
            }
            out.into_bytes()
        }
        FsMode::Jq => {
            let mut out = json_object(pairs.iter().map(|(key, value)| (*key, value.as_str())));
            out.push('\n');
            out.into_bytes()
        }
    }
}

#[cfg(test)]
#[path = "tests/format.rs"]
mod tests;
