//! Presentation helpers.
//!
//! These functions are deliberately pure and dependency-free (besides
//! `serde_json`) so headless tests can prove that unknown inventory values are
//! rendered as unknown, never as invented numbers.

use std::time::Duration;

use serde_json::Value;

/// Text used whenever the inventory did not report a value.
pub const UNKNOWN: &str = "Unknown";
/// Text used when an array field was reported but contains no entries.
pub const NONE_REPORTED: &str = "None reported";

pub const OPT_IN_LABEL: &str = "Allow this device to participate in FASTLINK compute";
pub const OPT_IN_DETAIL: &str =
    "Session-only opt-in. It creates no pairing, opens no socket and sends no data in this milestone.";

/// Render an optional string, treating blank values as unknown.
#[must_use]
pub fn optional_text(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map_or_else(|| UNKNOWN.to_owned(), ToOwned::to_owned)
}

/// A device name is never invented; an absent name is labelled as such.
#[must_use]
pub fn device_title(name: Option<&str>) -> String {
    match name.map(str::trim).filter(|text| !text.is_empty()) {
        Some(name) => name.to_owned(),
        None => "Unnamed device".to_owned(),
    }
}

/// Role in the current schema, which describes the local machine only.
#[must_use]
pub fn device_role(is_local: bool) -> &'static str {
    if is_local {
        "Local device"
    } else {
        "Non-local device"
    }
}

/// Format a byte count using binary units.
#[must_use]
pub fn bytes_label(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 1023.95 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Format an optional byte count without guessing missing values.
#[must_use]
pub fn optional_bytes(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| UNKNOWN.to_owned(), bytes_label)
}

/// Format logical CPU cores exactly as reported.
#[must_use]
pub fn cpu_label(cores: Option<u32>) -> String {
    cores.map_or_else(
        || UNKNOWN.to_owned(),
        |cores| format!("{cores} logical cores"),
    )
}

/// Derived RAM presentation. The note is shown verbatim by the UI when the
/// reported values cannot produce a meaningful percentage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamSummary {
    pub total: String,
    pub available: String,
    pub used_percent: Option<u8>,
    pub note: Option<&'static str>,
}

/// Summarize reported RAM. Percentages are only computed from two reported
/// values and are never used to fill in an unknown total.
#[must_use]
pub fn ram_summary(total: Option<u64>, available: Option<u64>) -> RamSummary {
    let total_label = optional_bytes(total);
    let available_label = optional_bytes(available);
    let mut note = None;
    let mut used_percent = None;

    match (total, available) {
        (Some(0), _) => {
            note = Some("Total RAM was reported as zero, so no percentage is shown.");
        }
        (Some(total), Some(available)) if available > total => {
            note = Some(
                "Reported available RAM is larger than total RAM; values are shown as reported.",
            );
        }
        (Some(total), Some(available)) => {
            let used = total - available;
            // Rounded integer percentage; both inputs came from the CLI.
            let percent = (u128::from(used) * 100 + u128::from(total) / 2) / u128::from(total);
            used_percent = u8::try_from(percent).ok();
        }
        _ => {}
    }

    RamSummary {
        total: total_label,
        available: available_label,
        used_percent,
        note,
    }
}

/// Label for an optional array field, distinguishing "unknown" from "empty".
#[must_use]
pub fn items_summary(items: Option<&[Value]>) -> String {
    match items {
        None => UNKNOWN.to_owned(),
        Some([]) => NONE_REPORTED.to_owned(),
        Some(items) => format!("{} reported", items.len()),
    }
}

/// Render each reported array entry. Schema v1 does not define entry fields,
/// so objects are shown as compact JSON rather than being guessed at.
#[must_use]
pub fn item_entries(items: Option<&[Value]>) -> Vec<String> {
    items
        .unwrap_or_default()
        .iter()
        .map(|item| match item {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// Status headline. Only the documented `local_only` value is translated;
/// anything else is shown exactly as reported.
#[must_use]
pub fn status_headline(status: Option<&str>) -> String {
    match status.map(str::trim).filter(|text| !text.is_empty()) {
        Some("local_only") => "Local-only".to_owned(),
        Some(other) => other.to_owned(),
        None => "Status unknown".to_owned(),
    }
}

/// Backend value exactly as reported.
#[must_use]
pub fn backend_label(backend: Option<&str>) -> String {
    optional_text(backend)
}

/// Short relative timestamp for the footer.
#[must_use]
pub fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 5 {
        "just now".to_owned()
    } else if seconds < 60 {
        format!("{seconds} s ago")
    } else if seconds < 3600 {
        format!("{} min ago", seconds / 60)
    } else {
        format!("{} h {} min ago", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_values_are_never_invented() {
        assert_eq!(optional_text(None), UNKNOWN);
        assert_eq!(optional_text(Some("   ")), UNKNOWN);
        assert_eq!(optional_text(Some("linux")), "linux");
        assert_eq!(optional_bytes(None), UNKNOWN);
        assert_eq!(cpu_label(None), UNKNOWN);
        assert_eq!(backend_label(None), UNKNOWN);
        assert_eq!(status_headline(None), "Status unknown");
        assert_eq!(device_title(None), "Unnamed device");
    }

    #[test]
    fn null_ram_has_no_percentage() {
        let summary = ram_summary(None, None);
        assert_eq!(summary.total, UNKNOWN);
        assert_eq!(summary.available, UNKNOWN);
        assert_eq!(summary.used_percent, None);
        assert_eq!(summary.note, None);

        let partial = ram_summary(Some(16 * 1024 * 1024 * 1024), None);
        assert_eq!(partial.available, UNKNOWN);
        assert_eq!(partial.used_percent, None);
    }

    #[test]
    fn ram_percentage_uses_reported_values_only() {
        let summary = ram_summary(Some(8_000), Some(2_000));
        assert_eq!(summary.total, "7.8 KiB");
        assert_eq!(summary.available, "2.0 KiB");
        assert_eq!(summary.used_percent, Some(75));
        assert_eq!(summary.note, None);
    }

    #[test]
    fn inconsistent_ram_is_flagged_not_clamped() {
        let summary = ram_summary(Some(1_000), Some(2_000));
        assert_eq!(summary.used_percent, None);
        assert!(summary.note.unwrap().contains("larger than total"));
        assert_eq!(summary.available, "2.0 KiB");
    }

    #[test]
    fn zero_total_ram_is_flagged() {
        let summary = ram_summary(Some(0), Some(0));
        assert_eq!(summary.used_percent, None);
        assert!(summary.note.unwrap().contains("zero"));
    }

    #[test]
    fn byte_units_are_binary() {
        assert_eq!(bytes_label(0), "0 B");
        assert_eq!(bytes_label(1023), "1023 B");
        assert_eq!(bytes_label(1024), "1.0 KiB");
        assert_eq!(bytes_label(1536), "1.5 KiB");
        assert_eq!(bytes_label(1024 * 1024), "1.0 MiB");
        assert_eq!(bytes_label(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn empty_and_unknown_arrays_are_distinct() {
        assert_eq!(items_summary(None), UNKNOWN);
        assert_eq!(items_summary(Some(&[])), "None reported");
        let entries = [json!("gpu0"), json!({"name": "gpu1"})];
        assert_eq!(items_summary(Some(&entries)), "2 reported");
        assert_eq!(
            item_entries(Some(&entries)),
            ["gpu0", "{\"name\":\"gpu1\"}"]
        );
    }

    #[test]
    fn documented_status_is_translated_but_unknown_status_is_raw() {
        assert_eq!(status_headline(Some("local_only")), "Local-only");
        assert_eq!(status_headline(Some("future_status")), "future_status");
    }

    #[test]
    fn ages_are_deterministic() {
        assert_eq!(format_age(Duration::from_secs(0)), "just now");
        assert_eq!(format_age(Duration::from_secs(12)), "12 s ago");
        assert_eq!(format_age(Duration::from_secs(90)), "1 min ago");
        assert_eq!(format_age(Duration::from_secs(3_900)), "1 h 5 min ago");
    }
}
