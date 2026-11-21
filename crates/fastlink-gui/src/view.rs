//! Presentation helpers.
//!
//! These functions are pure and headless-testable. They prove that unknown
//! backend values are rendered as unknown, that capabilities are only shown
//! from reported fields, and that network speed/connectivity is never guessed.

use std::time::Duration;

use crate::discovery::{
    CandidateNode, ProviderStatus, Transport, TrustState, WorkerState, DISCOVERY_SCOPE,
};
use crate::inventory::{GpuDevice, InventoryNode, NodeCapabilities};

/// Text used whenever the inventory did not report a value.
pub const UNKNOWN: &str = "Unknown";
/// Text used when a reported array contains no entries.
pub const NONE_REPORTED: &str = "None reported";

pub const OPT_IN_LABEL: &str = "Session-only visual intent (no effect)";
pub const OPT_IN_DETAIL: &str =
    "No operational effect: this does not enable compute, pair devices, open connections, or send data. It is discarded when the window closes.";
pub const OPT_IN_BADGE: &str = "Visual intent only (no effect)";

/// Footer summary for the session-only visual marker.
#[must_use]
pub fn session_marker_summary(count: usize) -> String {
    format!("Session visual markers active: {count} (no consent; no operational effect)")
}

/// Always shown for discovery candidates in this slice.
pub const DISCOVERY_UNPAIRED_WARNING: &str =
    "This GUI has not paired or verified this candidate. Pairing is not implemented; no Pair action is available.";
/// A tailnet listing is not evidence of a FASTLINK worker.
pub const DISCOVERY_TAILNET_WARNING: &str =
    "A Tailscale peer is not automatically a FASTLINK worker; metadata remains unverified.";

/// One label/value row rendered by the node, GPU and capability tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRow {
    pub label: &'static str,
    pub value: String,
    pub known: bool,
}

impl FieldRow {
    #[must_use]
    pub fn reported(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
            known: true,
        }
    }

    #[must_use]
    pub fn unknown(label: &'static str) -> Self {
        Self {
            label,
            value: UNKNOWN.to_owned(),
            known: false,
        }
    }

    #[must_use]
    pub fn optional_text(label: &'static str, value: Option<&str>) -> Self {
        match value.map(str::trim).filter(|text| !text.is_empty()) {
            Some(value) => Self::reported(label, value),
            None => Self::unknown(label),
        }
    }

    #[must_use]
    pub fn optional_bytes(label: &'static str, value: Option<u64>) -> Self {
        match value {
            Some(value) => Self::reported(label, bytes_label(value)),
            None => Self::unknown(label),
        }
    }
}

/// Render an optional string, treating blank values as unknown.
#[must_use]
pub fn optional_text(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map_or_else(|| UNKNOWN.to_owned(), ToOwned::to_owned)
}

/// Node title from the reported hostname; nothing is synthesized.
#[must_use]
pub fn node_title(hostname: Option<&str>) -> String {
    match hostname.map(str::trim).filter(|text| !text.is_empty()) {
        Some(hostname) => hostname.to_owned(),
        None => "Unnamed node".to_owned(),
    }
}

/// Role in the current local-only milestone.
#[must_use]
pub fn node_role(is_local: bool) -> &'static str {
    if is_local {
        "Local device"
    } else {
        "Reported node"
    }
}

/// Preserve the opaque backend `node_id` byte-for-byte; never special-case
/// `"local"` and never rewrite the hash.
#[must_use]
pub fn opaque_id_label(node_id: Option<&str>) -> String {
    node_id.map_or_else(|| UNKNOWN.to_owned(), ToOwned::to_owned)
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

/// Format logical CPU count exactly as reported.
#[must_use]
pub fn cpu_label(cores: Option<u64>) -> String {
    cores.map_or_else(
        || UNKNOWN.to_owned(),
        |cores| format!("{cores} logical cores"),
    )
}

/// Derived RAM presentation. `note` explains why no percentage is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamSummary {
    pub total: String,
    pub available: String,
    pub free: String,
    pub used_percent: Option<u8>,
    pub note: Option<&'static str>,
}

/// Summarize reported RAM. Percentages are only computed from two reported
/// values and are never used to fill in an unknown total.
#[must_use]
pub fn ram_summary(total: Option<u64>, available: Option<u64>, free: Option<u64>) -> RamSummary {
    let total_label = optional_bytes(total);
    let available_label = optional_bytes(available);
    let free_label = optional_bytes(free);
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
            let percent = (u128::from(used) * 100 + u128::from(total) / 2) / u128::from(total);
            used_percent = u8::try_from(percent).ok();
        }
        _ => {}
    }

    if note.is_none() && matches!((total, free), (Some(total), Some(free)) if free > total) {
        note = Some("Reported free RAM is larger than total RAM; values are shown as reported.");
    }

    RamSummary {
        total: total_label,
        available: available_label,
        free: free_label,
        used_percent,
        note,
    }
}

/// Row helpers for node facts. RAM is rendered separately because it also has
/// a derived usage percentage.
#[must_use]
pub fn node_rows(node: &InventoryNode) -> Vec<FieldRow> {
    vec![
        FieldRow::optional_text("Operating system", node.os.as_deref()),
        FieldRow::optional_text("OS release", node.os_release.as_deref()),
        FieldRow::optional_text("Architecture", node.arch.as_deref()),
        match node.logical_cpu_count {
            Some(cores) => FieldRow::reported("Logical CPU count", cpu_label(Some(cores))),
            None => FieldRow::unknown("Logical CPU count"),
        },
        FieldRow::optional_text("Node ID source", node.node_id_source.as_deref()),
    ]
}

/// Count-only label for reported arrays.
#[must_use]
pub fn reported_count(count: usize) -> String {
    match count {
        0 => NONE_REPORTED.to_owned(),
        1 => "1 reported".to_owned(),
        count => format!("{count} reported"),
    }
}

#[must_use]
pub fn gpu_count_label(gpus: &[GpuDevice]) -> String {
    reported_count(gpus.len())
}

/// Index label for a GPU entry; a missing name is not invented.
#[must_use]
pub fn gpu_title(index: usize, gpu: &GpuDevice) -> String {
    match gpu
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        Some(name) => name.to_owned(),
        None => format!("GPU {}", index + 1),
    }
}

/// Every GPU value the backend reported, with unknown fields still unknown.
#[must_use]
pub fn gpu_rows(gpu: &GpuDevice) -> Vec<FieldRow> {
    let card = gpu.card.trim();
    let card_row = if card.is_empty() {
        FieldRow::unknown("Card")
    } else {
        FieldRow::reported("Card", gpu.card.clone())
    };
    vec![
        card_row,
        FieldRow::optional_text("Name", gpu.name.as_deref()),
        FieldRow::optional_text("Driver", gpu.driver.as_deref()),
        FieldRow::optional_text("Vendor ID", gpu.vendor_id.as_deref()),
        FieldRow::optional_text("PCI device ID", gpu.pci_device_id.as_deref()),
        FieldRow::optional_bytes("VRAM total", gpu.vram_total_bytes),
        FieldRow::optional_bytes("VRAM free", gpu.vram_free_bytes),
    ]
}

/// Normalize only the backend's documented `"unknown"` sentinel; anything else
/// is shown exactly as reported, including a reported zero.
#[must_use]
pub fn connectivity_label(value: Option<&str>) -> String {
    match value.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) if !text.eq_ignore_ascii_case("unknown") => text.to_owned(),
        _ => UNKNOWN.to_owned(),
    }
}

#[must_use]
pub fn connectivity_known(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).filter(|text| !text.is_empty()),
        Some(text) if !text.eq_ignore_ascii_case("unknown")
    )
}

/// Capabilities are rendered only from reported fields. Null capacity or
/// throughput means unknown, not zero.
#[must_use]
pub fn capability_rows(capabilities: &NodeCapabilities) -> Vec<FieldRow> {
    let throughput = match capabilities.throughput_bps {
        Some(value) => FieldRow::reported("Network throughput", format!("{value} bps")),
        None => FieldRow::unknown("Network throughput"),
    };
    vec![
        FieldRow::optional_bytes("Storage capacity", capabilities.capacity_bytes),
        throughput,
        FieldRow {
            label: "Network connectivity",
            value: connectivity_label(capabilities.network_connectivity.as_deref()),
            known: connectivity_known(capabilities.network_connectivity.as_deref()),
        },
        FieldRow::reported(
            "Remote peers",
            reported_count(capabilities.remote_peers.len()),
        ),
    ]
}

/// Count label for the backend's explicit unknown-field list.
#[must_use]
pub fn unknown_fields_count(fields: &[String]) -> String {
    reported_count(fields.len())
}

#[must_use]
pub fn unknown_field_entries(fields: &[String]) -> Vec<String> {
    fields.to_vec()
}

/// Schema headline, exactly as reported.
#[must_use]
pub fn schema_headline(schema: &str, version: u32) -> String {
    format!("{schema} · schema v{version}")
}

/// Scope headline. Only the implemented `local_only` value is translated.
#[must_use]
pub fn scope_headline(scope: Option<&str>) -> String {
    match scope.map(str::trim).filter(|text| !text.is_empty()) {
        Some("local_only") => "Local-only".to_owned(),
        Some(other) => other.to_owned(),
        None => "Scope unknown".to_owned(),
    }
}

/// Discovery mode exactly as reported.
#[must_use]
pub fn discovery_label(mode: Option<&str>) -> String {
    optional_text(mode)
}

/// Opt-in discovery scope from the contract.
#[must_use]
pub fn discovery_scope_label(scope: &str) -> &'static str {
    if scope == DISCOVERY_SCOPE {
        "LAN + Tailscale (user opt-in)"
    } else {
        "Restricted discovery scope"
    }
}

/// Provider availability without raw provider error text.
#[must_use]
pub fn provider_status_label(status: ProviderStatus) -> &'static str {
    match status {
        ProviderStatus::Available => "Available",
        ProviderStatus::Unavailable => "Unavailable",
        ProviderStatus::Unknown => UNKNOWN,
    }
}

/// Candidate title from a sanitized display name.
#[must_use]
pub fn candidate_title(display_name: Option<&str>) -> String {
    match display_name.map(str::trim).filter(|text| !text.is_empty()) {
        Some(name) => name.to_owned(),
        None => "Unnamed candidate".to_owned(),
    }
}

#[must_use]
pub fn candidate_unpaired_warning() -> &'static str {
    DISCOVERY_UNPAIRED_WARNING
}

#[must_use]
pub fn candidate_tailnet_warning() -> &'static str {
    DISCOVERY_TAILNET_WARNING
}

#[must_use]
pub fn candidate_uses_tailscale(node: &CandidateNode) -> bool {
    node.transports.contains(&Transport::Tailscale)
}

#[must_use]
pub fn transport_label(transport: Transport) -> &'static str {
    match transport {
        Transport::Lan => "LAN",
        Transport::Tailscale => "Tailscale",
        Transport::Unknown => "Unknown transport",
    }
}

#[must_use]
pub fn worker_state_label(state: Option<WorkerState>) -> String {
    match state {
        Some(WorkerState::Unverified) => "Unverified".to_owned(),
        Some(WorkerState::Verified) => "Reported verified (not trusted by this GUI)".to_owned(),
        Some(WorkerState::Unknown) | None => UNKNOWN.to_owned(),
    }
}

#[must_use]
pub fn trust_state_label(state: Option<TrustState>) -> String {
    match state {
        Some(TrustState::Unpaired) => "Unpaired".to_owned(),
        Some(TrustState::Paired) => "Reported paired (not verified by this GUI)".to_owned(),
        Some(TrustState::Unknown) | None => UNKNOWN.to_owned(),
    }
}

/// Rows for an unverified discovery candidate. Empty transports/worker/trust
/// are unknown, not invented capabilities.
#[must_use]
pub fn candidate_rows(node: &CandidateNode) -> Vec<FieldRow> {
    let transports = if node.transports.is_empty() {
        FieldRow::unknown("Transports")
    } else {
        FieldRow::reported(
            "Transports",
            node.transports
                .iter()
                .map(|transport| transport_label(*transport))
                .collect::<Vec<_>>()
                .join(", "),
        )
    };
    vec![
        FieldRow::optional_text("Operating system", node.os.as_deref()),
        FieldRow::optional_text("Architecture", node.arch.as_deref()),
        match node.logical_cpu_count {
            Some(cores) => FieldRow::reported("Logical CPU count", cpu_label(Some(cores))),
            None => FieldRow::unknown("Logical CPU count"),
        },
        FieldRow::optional_bytes("RAM total", node.ram_total_bytes),
        FieldRow::optional_bytes("RAM available", node.ram_available_bytes),
        FieldRow::reported("GPUs", reported_count(node.gpu_count)),
        transports,
        match node.worker_state {
            Some(state) => FieldRow::reported("Worker state", worker_state_label(Some(state))),
            None => FieldRow::unknown("Worker state"),
        },
        match node.trust_state {
            Some(state) => FieldRow::reported("Trust state", trust_state_label(Some(state))),
            None => FieldRow::unknown("Trust state"),
        },
    ]
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
    use crate::inventory::parse_inventory_json;

    const FIXTURE: &str = include_str!("../testdata/devices-v1.json");

    fn fixture_node() -> InventoryNode {
        parse_inventory_json(FIXTURE)
            .expect("fixture must parse")
            .local_node()
            .expect("fixture has a local node")
            .clone()
    }

    #[test]
    fn unknown_values_are_never_invented() {
        assert_eq!(optional_text(None), UNKNOWN);
        assert_eq!(optional_text(Some("   ")), UNKNOWN);
        assert_eq!(optional_text(Some("linux")), "linux");
        assert_eq!(optional_bytes(None), UNKNOWN);
        assert_eq!(cpu_label(None), UNKNOWN);
        assert_eq!(node_title(None), "Unnamed node");
        assert_eq!(scope_headline(None), "Scope unknown");
        assert_eq!(discovery_label(None), UNKNOWN);
    }

    #[test]
    fn opaque_node_id_is_preserved_exactly() {
        assert_eq!(opaque_id_label(None), UNKNOWN);
        assert_eq!(
            opaque_id_label(Some("0123456789abcdef0123456789abcdef")),
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(opaque_id_label(Some("AbC_123")), "AbC_123");
        assert_ne!(
            opaque_id_label(Some("0123456789abcdef0123456789abcdef")),
            "local"
        );
    }

    #[test]
    fn fixture_rows_use_reported_backend_fields() {
        let node = fixture_node();
        let rows = node_rows(&node);
        let value = |label: &str| {
            rows.iter()
                .find(|row| row.label == label)
                .unwrap_or_else(|| panic!("missing {label}"))
        };
        assert_eq!(value("Operating system").value, "linux");
        assert_eq!(value("OS release").value, "6.12.0-fixture");
        assert_eq!(value("Architecture").value, "x86_64");
        assert_eq!(value("Logical CPU count").value, "16 logical cores");
        assert_eq!(value("Node ID source").value, "machine-id");

        let ram = ram_summary(
            node.ram_total_bytes,
            node.ram_available_bytes,
            node.ram_free_bytes,
        );
        assert_eq!(ram.total, "32.0 GiB");
        assert_eq!(ram.available, "8.0 GiB");
        assert_eq!(ram.free, "4.0 GiB");
        assert_eq!(ram.used_percent, Some(75));
        assert_eq!(ram.note, None);
    }

    #[test]
    fn capabilities_use_only_reported_values() {
        let capabilities = NodeCapabilities {
            capacity_bytes: None,
            throughput_bps: None,
            network_connectivity: Some("unknown".to_owned()),
            remote_peers: Vec::new(),
        };
        let rows = capability_rows(&capabilities);
        assert_eq!(rows[0].value, UNKNOWN);
        assert!(!rows[0].known);
        assert_eq!(rows[1].value, UNKNOWN);
        assert!(!rows[1].known);
        assert_eq!(rows[2].value, UNKNOWN);
        assert!(!rows[2].known);
        assert_eq!(rows[3].value, NONE_REPORTED);
        assert!(rows[3].known);

        let reported = NodeCapabilities {
            capacity_bytes: Some(1024),
            throughput_bps: Some(0),
            network_connectivity: Some("loopback_only".to_owned()),
            remote_peers: vec!["peer-a".to_owned(), "peer-b".to_owned()],
        };
        let rows = capability_rows(&reported);
        assert_eq!(rows[0].value, "1.0 KiB");
        assert_eq!(rows[1].value, "0 bps");
        assert!(rows[1].known);
        assert_eq!(rows[2].value, "loopback_only");
        assert!(rows[2].known);
        assert_eq!(rows[3].value, "2 reported");
    }

    #[test]
    fn unknown_connectivity_stays_unknown() {
        for value in [
            None,
            Some(""),
            Some("   "),
            Some("unknown"),
            Some("UNKNOWN"),
        ] {
            assert_eq!(connectivity_label(value), UNKNOWN, "{value:?}");
            assert!(!connectivity_known(value), "{value:?}");
        }
        assert_eq!(connectivity_label(Some("loopback")), "loopback");
        assert!(connectivity_known(Some("loopback")));
    }

    #[test]
    fn gpu_rows_expose_only_reported_fields() {
        let gpu = GpuDevice {
            card: "card1".to_owned(),
            name: None,
            driver: None,
            vendor_id: None,
            pci_device_id: None,
            vram_total_bytes: None,
            vram_free_bytes: Some(0),
        };
        assert_eq!(gpu_title(0, &gpu), "GPU 1");
        let rows = gpu_rows(&gpu);
        assert_eq!(rows[0].value, "card1");
        assert_eq!(rows[1].value, UNKNOWN);
        assert_eq!(rows[1].label, "Name");
        assert!(!rows[1].known);
        assert_eq!(rows[5].value, UNKNOWN);
        assert!(!rows[5].known);
        assert_eq!(rows[6].value, "0 B");
        assert!(rows[6].known);
        assert_eq!(gpu_count_label(&[]), NONE_REPORTED);
        assert_eq!(gpu_count_label(std::slice::from_ref(&gpu)), "1 reported");
    }

    #[test]
    fn ram_percentage_uses_reported_values_only() {
        let summary = ram_summary(Some(8_000), Some(2_000), None);
        assert_eq!(summary.total, "7.8 KiB");
        assert_eq!(summary.available, "2.0 KiB");
        assert_eq!(summary.free, UNKNOWN);
        assert_eq!(summary.used_percent, Some(75));
        assert_eq!(summary.note, None);

        let partial = ram_summary(Some(16 * 1024 * 1024 * 1024), None, None);
        assert_eq!(partial.available, UNKNOWN);
        assert_eq!(partial.free, UNKNOWN);
        assert_eq!(partial.used_percent, None);
    }

    #[test]
    fn inconsistent_ram_is_flagged_not_clamped() {
        let summary = ram_summary(Some(1_000), Some(2_000), Some(3_000));
        assert_eq!(summary.used_percent, None);
        assert!(summary.note.unwrap().contains("larger than total"));
        assert_eq!(summary.available, "2.0 KiB");
        assert_eq!(summary.free, "2.9 KiB");

        let free_inconsistent = ram_summary(Some(1_000), Some(500), Some(3_000));
        assert!(free_inconsistent
            .note
            .unwrap()
            .contains("free RAM is larger"));
        assert_eq!(free_inconsistent.used_percent, Some(50));

        let zero_total = ram_summary(Some(0), Some(0), Some(0));
        assert_eq!(zero_total.used_percent, None);
        assert!(zero_total.note.unwrap().contains("zero"));
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
    fn schema_scope_and_unknown_fields_are_explicit() {
        assert_eq!(
            schema_headline("fastlink.devices.v1", 1),
            "fastlink.devices.v1 · schema v1"
        );
        assert_eq!(scope_headline(Some("local_only")), "Local-only");
        assert_eq!(scope_headline(Some("future_scope")), "future_scope");
        assert_eq!(unknown_fields_count(&[]), NONE_REPORTED);
        assert_eq!(
            unknown_field_entries(&["capabilities.capacity_bytes".to_owned()]),
            ["capabilities.capacity_bytes"]
        );
    }

    #[test]
    fn ages_are_deterministic() {
        assert_eq!(format_age(Duration::from_secs(0)), "just now");
        assert_eq!(format_age(Duration::from_secs(12)), "12 s ago");
        assert_eq!(format_age(Duration::from_secs(90)), "1 min ago");
        assert_eq!(format_age(Duration::from_secs(3_900)), "1 h 5 min ago");
    }

    #[test]
    fn discovery_candidate_rows_are_honest_and_unpaired() {
        let discovery =
            crate::discovery::parse_discovery_json(include_str!("../testdata/discovery-v1.json"))
                .expect("discovery fixture must parse");
        let lan = &discovery.nodes[0];
        let rows = candidate_rows(lan);
        let value = |label: &str| {
            rows.iter()
                .find(|row| row.label == label)
                .unwrap_or_else(|| panic!("missing {label}"))
        };
        assert_eq!(value("Operating system").value, "linux");
        assert_eq!(value("Architecture").value, "x86_64");
        assert_eq!(value("Logical CPU count").value, "8 logical cores");
        assert_eq!(value("RAM total").value, "16.0 GiB");
        assert_eq!(value("RAM available").value, "8.0 GiB");
        assert_eq!(value("GPUs").value, "1 reported");
        assert_eq!(value("Transports").value, "LAN");
        assert_eq!(value("Worker state").value, "Unverified");
        assert_eq!(value("Trust state").value, "Unpaired");
        assert!(candidate_unpaired_warning().contains("Pairing is not implemented"));

        let tailnet = &discovery.nodes[1];
        let rows = candidate_rows(tailnet);
        assert_eq!(
            rows.iter()
                .find(|row| row.label == "Transports")
                .unwrap()
                .value,
            "Tailscale"
        );
        assert!(candidate_uses_tailscale(tailnet));
        assert!(candidate_tailnet_warning().contains("not automatically a FASTLINK worker"));
    }

    #[test]
    fn discovery_unknown_resources_stay_unknown() {
        let json = r#"{
            "schema": "fastlink.discover.v1",
            "scope": "user_opt_in_lan_tailscale",
            "providers": [{"provider": "tailscale", "status": "unavailable"}],
            "nodes": [{
                "node_id": "candidate-9",
                "display_name": null,
                "os": null,
                "arch": null,
                "logical_cpu_count": null,
                "ram_total_bytes": null,
                "ram_available_bytes": null,
                "gpus": [],
                "transports": [],
                "worker_state": null,
                "trust_state": null
            }]
        }"#;
        let discovery =
            crate::discovery::parse_discovery_json(json).expect("null resources are allowed");
        let node = &discovery.nodes[0];
        let rows = candidate_rows(node);
        for label in [
            "Operating system",
            "Architecture",
            "Logical CPU count",
            "RAM total",
            "RAM available",
            "Transports",
            "Worker state",
            "Trust state",
        ] {
            let row = rows.iter().find(|row| row.label == label).unwrap();
            assert_eq!(row.value, UNKNOWN, "{label}");
            assert!(!row.known, "{label}");
        }
        assert_eq!(
            rows.iter().find(|row| row.label == "GPUs").unwrap().value,
            NONE_REPORTED
        );
        assert_eq!(candidate_title(None), "Unnamed candidate");
        assert_eq!(
            provider_status_label(crate::discovery::ProviderStatus::Unavailable),
            "Unavailable"
        );
        assert_eq!(
            discovery_scope_label(crate::discovery::DISCOVERY_SCOPE),
            "LAN + Tailscale (user opt-in)"
        );
    }
}
