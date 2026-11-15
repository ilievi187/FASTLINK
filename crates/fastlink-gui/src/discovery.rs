//! Opt-in FASTLINK discovery contract (`fastlink.discover.v1`).
//!
//! Discovery is never automatic. This module only parses the bounded JSON that
//! the CLI returns after an explicit user action. It deliberately stores no
//! address, endpoint, MAC or raw provider message fields.

use std::collections::HashSet;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde::de::{Deserializer, IgnoredAny, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;

use crate::inventory::{InventoryCommand, LoadError};

/// Schema emitted by the opt-in discovery command.
pub const DISCOVERY_SCHEMA: &str = "fastlink.discover.v1";
/// The only permitted discovery scope.
pub const DISCOVERY_SCOPE: &str = "user_opt_in_lan_tailscale";
/// Upper bounds keep malicious output from growing the parser/UI.
pub const MAX_PROVIDERS: usize = 64;
pub const MAX_NODES: usize = 256;

/// Validated discovery result. Raw provider messages and unknown fields are
/// intentionally not retained.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DiscoveryInventory {
    pub schema: String,
    pub scope: String,
    pub providers: Vec<DiscoveryProvider>,
    pub nodes: Vec<CandidateNode>,
}

/// Provider availability. Raw provider error text is never stored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DiscoveryProvider {
    pub provider: String,
    pub status: ProviderStatus,
}

/// Only the contract's safe, enumerated statuses are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Available,
    Unavailable,
    #[serde(other)]
    Unknown,
}

/// Transport labels. Unknown labels render as Unknown rather than raw text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Lan,
    Tailscale,
    #[serde(other)]
    Unknown,
}

/// Explicit worker readiness state. This GUI never upgrades a candidate to
/// worker-ready on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Unverified,
    Verified,
    #[serde(other)]
    Unknown,
}

/// Pairing/trust state. Pairing is not implemented in this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    Unpaired,
    Paired,
    #[serde(other)]
    Unknown,
}

/// Unpaired, untrusted discovery candidate.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CandidateNode {
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub logical_cpu_count: Option<u64>,
    #[serde(default)]
    pub ram_total_bytes: Option<u64>,
    #[serde(default)]
    pub ram_available_bytes: Option<u64>,
    /// GPU entries are counted but their raw contents are dropped: the
    /// discovery contract does not define renderable GPU fields for candidates.
    #[serde(default, rename = "gpus", deserialize_with = "count_gpus")]
    pub gpu_count: usize,
    #[serde(default)]
    pub transports: Vec<Transport>,
    #[serde(default)]
    pub worker_state: Option<WorkerState>,
    #[serde(default)]
    pub trust_state: Option<TrustState>,
}

/// Structural/privacy problems in a discovery document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryParseError {
    Json,
    UnsupportedSchema,
    UnsupportedScope,
    TooManyProviders,
    TooManyNodes,
    ForbiddenAddressField,
    InvalidProvider { index: usize },
    InvalidNodeId { index: usize },
}

impl fmt::Display for DiscoveryParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Json => "invalid discovery JSON",
            Self::UnsupportedSchema => "unsupported discovery schema",
            Self::UnsupportedScope => "unsupported discovery scope",
            Self::TooManyProviders => "too many discovery providers",
            Self::TooManyNodes => "too many discovery candidates",
            Self::ForbiddenAddressField => "discovery output contained an address field",
            Self::InvalidProvider { .. } => "discovery output contained an invalid provider",
            Self::InvalidNodeId { .. } => "discovery output contained an invalid candidate id",
        };
        f.write_str(message)
    }
}

impl std::error::Error for DiscoveryParseError {}

/// Parse and validate a discovery document without retaining raw addresses or
/// provider error text.
pub fn parse_discovery_json(source: &str) -> Result<DiscoveryInventory, DiscoveryParseError> {
    let value: Value = serde_json::from_str(source).map_err(|_| DiscoveryParseError::Json)?;
    reject_address_fields(&value)?;
    let discovery: DiscoveryInventory =
        serde_json::from_value(value).map_err(|_| DiscoveryParseError::Json)?;
    validate_discovery(discovery)
}

fn validate_discovery(
    mut discovery: DiscoveryInventory,
) -> Result<DiscoveryInventory, DiscoveryParseError> {
    if discovery.schema != DISCOVERY_SCHEMA {
        return Err(DiscoveryParseError::UnsupportedSchema);
    }
    if discovery.scope != DISCOVERY_SCOPE {
        return Err(DiscoveryParseError::UnsupportedScope);
    }
    if discovery.providers.len() > MAX_PROVIDERS {
        return Err(DiscoveryParseError::TooManyProviders);
    }
    if discovery.nodes.len() > MAX_NODES {
        return Err(DiscoveryParseError::TooManyNodes);
    }

    let mut provider_names = HashSet::with_capacity(discovery.providers.len());
    for (index, provider) in discovery.providers.iter().enumerate() {
        if !is_safe_provider_name(&provider.provider) || !provider_names.insert(&provider.provider)
        {
            return Err(DiscoveryParseError::InvalidProvider { index });
        }
    }

    let mut node_ids = HashSet::with_capacity(discovery.nodes.len());
    for (index, node) in discovery.nodes.iter_mut().enumerate() {
        if let Some(id) = node.node_id.as_deref() {
            if !is_safe_node_id(id) || !node_ids.insert(id.to_owned()) {
                return Err(DiscoveryParseError::InvalidNodeId { index });
            }
        }
        node.display_name = sanitize_text(node.display_name.take(), 128);
        node.os = sanitize_text(node.os.take(), 64);
        node.arch = sanitize_text(node.arch.take(), 64);
    }

    Ok(discovery)
}

fn reject_address_fields(value: &Value) -> Result<(), DiscoveryParseError> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if is_forbidden_address_key(key) {
                    return Err(DiscoveryParseError::ForbiddenAddressField);
                }
                reject_address_fields(value)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                reject_address_fields(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_forbidden_address_key(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "ip" | "ip_address"
            | "addr"
            | "address"
            | "endpoint"
            | "endpoints"
            | "mac"
            | "mac_address"
            | "peer_ip"
            | "tailscale_ip"
    )
}

fn is_safe_provider_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn is_safe_node_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn sanitize_text(value: Option<String>, max_chars: usize) -> Option<String> {
    let trimmed = value?.trim().to_owned();
    if trimmed.is_empty()
        || trimmed.chars().count() > max_chars
        || trimmed.chars().any(char::is_control)
        || contains_address_like(&trimmed)
    {
        return None;
    }
    Some(trimmed)
}

fn contains_address_like(text: &str) -> bool {
    text_contains_address_like(text)
}

/// Shared privacy check for any string that might enter the UI.
pub(crate) fn text_contains_address_like(text: &str) -> bool {
    if text.contains("://") {
        return true;
    }
    let tokens_unsafe = text
        .split(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    ',' | ';'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '='
                        | '<'
                        | '>'
                        | '"'
                        | '\''
                        | '/'
                        | '@'
                )
        })
        .map(|token| token.trim_matches(|ch: char| matches!(ch, '.' | ':')))
        .any(looks_like_network_address);
    if tokens_unsafe {
        return true;
    }

    // Catch addresses embedded in a larger token, such as an address prefixed
    // or suffixed with hostname text. These scans are intentionally broad.
    if text
        .split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .map(|part| part.trim_matches('.'))
        .any(|part| !part.is_empty() && part.parse::<Ipv4Addr>().is_ok())
    {
        return true;
    }
    text.split(|ch: char| !(ch.is_ascii_hexdigit() || ch == ':' || ch == '-'))
        .map(|part| part.trim_matches(|ch: char| ch == ':' || ch == '-'))
        .filter(|part| !part.is_empty())
        .any(|part| {
            (part.matches(':').count() >= 2 && part.parse::<Ipv6Addr>().is_ok())
                || looks_like_mac(part)
        })
}

fn looks_like_network_address(token: &str) -> bool {
    let token = token.trim_matches(|ch: char| matches!(ch, '[' | ']' | '(' | ')'));
    if token.is_empty() {
        return false;
    }
    if token.parse::<Ipv4Addr>().is_ok() || token.parse::<Ipv6Addr>().is_ok() {
        return true;
    }
    if let Some((host, port)) = token.rsplit_once(':') {
        if port.bytes().all(|byte| byte.is_ascii_digit()) && host.parse::<Ipv4Addr>().is_ok() {
            return true;
        }
    }
    if looks_like_dns_host_port(token) {
        return true;
    }
    looks_like_mac(token)
}

/// Detect a narrow endpoint form: a DNS-style dotted hostname followed by a
/// valid numeric TCP/UDP port. Ordinary DNS names without a port remain usable.
fn looks_like_dns_host_port(token: &str) -> bool {
    let Some((host, port)) = token.rsplit_once(':') else {
        return false;
    };
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    if port == 0 {
        return false;
    }
    let host = host.trim_matches('.');
    if host.len() > 253 || !host.contains('.') {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn looks_like_mac(token: &str) -> bool {
    for separator in [':', '-'] {
        let groups: Vec<&str> = token.split(separator).collect();
        if groups.len() == 6
            && groups
                .iter()
                .all(|group| group.len() == 2 && group.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return true;
        }
    }
    false
}

fn count_gpus<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    struct CountVisitor;

    impl<'de> Visitor<'de> for CountVisitor {
        type Value = usize;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an array of GPU entries")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut count = 0usize;
            while sequence.next_element::<IgnoredAny>()?.is_some() {
                count = count.saturating_add(1);
            }
            Ok(count)
        }
    }

    deserializer.deserialize_seq(CountVisitor)
}

/// The opt-in discovery command: `<FASTLINK CLI> discover --json`.
///
/// It reuses the inventory runner for bounded capture, FASTLINK_BIN/path
/// selection and timeout/process cleanup.
#[derive(Debug, Clone)]
pub struct DiscoveryCommand {
    runner: InventoryCommand,
}

impl DiscoveryCommand {
    #[must_use]
    pub fn new(runner: InventoryCommand) -> Self {
        Self { runner }
    }

    #[must_use]
    pub fn command_line(&self) -> String {
        format!("{} discover --json", self.runner.program().display())
    }

    /// Attach a cancellation token that kills the CLI and its provider process
    /// group on Unix when set.
    #[must_use]
    pub fn with_cancel_token(self, token: Arc<AtomicBool>) -> Self {
        Self {
            runner: self.runner.with_cancel_token(token),
        }
    }

    /// Run discovery. All failures are reduced to safe UI text: no stderr,
    /// provider error output, addresses or raw parser values are returned.
    pub fn run(&self) -> Result<DiscoveryInventory, LoadError> {
        let command_line = self.command_line();
        let output = self
            .runner
            .run_capture(&["discover", "--json"], &command_line)
            .map_err(safe_runner_error)?;
        parse_discovery_json(&output.stdout).map_err(|_| {
            LoadError::new(
                "Discovery response did not match the expected schema. No candidates were shown.",
            )
        })
    }
}

fn safe_runner_error(error: LoadError) -> LoadError {
    let summary = if error.summary.contains("cancelled") {
        "Discovery was cancelled. No candidates were shown."
    } else if error.summary.contains("timed out") {
        "Discovery timed out before a safe result was received."
    } else if error.summary.contains("could not start") {
        "Discovery could not start. Check the configured FASTLINK binary."
    } else if error.summary.contains("capture limit") {
        "Discovery output exceeded the safety limit. No candidates were shown."
    } else if error.summary.contains("failed") {
        "Discovery command failed. No candidates were shown."
    } else {
        "Discovery failed safely. No candidates were shown."
    };
    LoadError::new(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../testdata/discovery-v1.json");

    #[test]
    fn parses_contract_fixture_and_provider_states() {
        let discovery = parse_discovery_json(FIXTURE).expect("fixture must parse");
        assert_eq!(discovery.schema, DISCOVERY_SCHEMA);
        assert_eq!(discovery.scope, DISCOVERY_SCOPE);
        assert_eq!(discovery.providers.len(), 2);
        assert_eq!(discovery.providers[0].provider, "lan_mdns");
        assert_eq!(discovery.providers[0].status, ProviderStatus::Available);
        assert_eq!(discovery.providers[1].provider, "tailscale");
        assert_eq!(discovery.providers[1].status, ProviderStatus::Unavailable);
        assert_eq!(discovery.nodes.len(), 2);
    }

    #[test]
    fn parses_candidate_metadata_and_tailscale_candidate() {
        let discovery = parse_discovery_json(FIXTURE).expect("fixture must parse");
        let lan = &discovery.nodes[0];
        assert_eq!(lan.node_id.as_deref(), Some("candidate-lan-1"));
        assert_eq!(lan.display_name.as_deref(), Some("Example LAN device"));
        assert_eq!(lan.os.as_deref(), Some("linux"));
        assert_eq!(lan.arch.as_deref(), Some("x86_64"));
        assert_eq!(lan.logical_cpu_count, Some(8));
        assert_eq!(lan.ram_total_bytes, Some(17_179_869_184));
        assert_eq!(lan.ram_available_bytes, Some(8_589_934_592));
        assert_eq!(lan.gpu_count, 1);
        assert!(!format!("{lan:?}").contains("Example GPU"));
        assert_eq!(lan.transports, [Transport::Lan]);
        assert_eq!(lan.worker_state, Some(WorkerState::Unverified));
        assert_eq!(lan.trust_state, Some(TrustState::Unpaired));

        let tailnet = &discovery.nodes[1];
        assert_eq!(tailnet.transports, [Transport::Tailscale]);
        assert_eq!(tailnet.os, None);
        assert_eq!(tailnet.arch, None);
        assert_eq!(tailnet.logical_cpu_count, None);
        assert_eq!(tailnet.ram_total_bytes, None);
        assert_eq!(tailnet.ram_available_bytes, None);
        assert_eq!(tailnet.gpu_count, 0);
        assert_eq!(tailnet.worker_state, Some(WorkerState::Unverified));
        assert_eq!(tailnet.trust_state, Some(TrustState::Unpaired));
    }

    #[test]
    fn rejects_bad_schema_and_scope() {
        let wrong_schema = FIXTURE.replace(DISCOVERY_SCHEMA, "fastlink.discover.v0");
        assert_eq!(
            parse_discovery_json(&wrong_schema).expect_err("must fail closed"),
            DiscoveryParseError::UnsupportedSchema
        );

        let wrong_scope = FIXTURE.replace(DISCOVERY_SCOPE, "global_scan");
        assert_eq!(
            parse_discovery_json(&wrong_scope).expect_err("must fail closed"),
            DiscoveryParseError::UnsupportedScope
        );
    }

    #[test]
    fn rejects_forbidden_address_fields_without_echoing_values() {
        let json = FIXTURE.replace(
            "\"display_name\":\"Example LAN device\"",
            "\"display_name\":\"Example LAN device\",\"ip\":\"192.168.1.50\"",
        );
        let error = parse_discovery_json(&json).expect_err("must fail closed");
        assert_eq!(error, DiscoveryParseError::ForbiddenAddressField);
        assert!(!error.to_string().contains("192.168.1.50"));
    }

    #[test]
    fn raw_provider_messages_and_unknown_address_fields_are_not_retained() {
        let json = r#"{
            "schema": "fastlink.discover.v1",
            "scope": "user_opt_in_lan_tailscale",
            "providers": [
                {"provider": "tailscale", "status": "unavailable",
                 "message": "tailscale at 100.64.1.9 refused", "peer_ip": "100.64.1.9"}
            ],
            "nodes": []
        }"#;
        let error = parse_discovery_json(json).expect_err("address field must fail closed");
        assert_eq!(error, DiscoveryParseError::ForbiddenAddressField);
        assert!(!error.to_string().contains("100.64.1.9"));
    }

    #[test]
    fn provider_message_values_are_never_retained_even_without_address_fields() {
        let json = r#"{
            "schema": "fastlink.discover.v1",
            "scope": "user_opt_in_lan_tailscale",
            "providers": [
                {"provider": "tailscale", "status": "unavailable",
                 "message": "cannot reach 100.64.1.9"}
            ],
            "nodes": []
        }"#;
        let discovery = parse_discovery_json(json).expect("safe envelope still parses");
        let rendered = format!("{discovery:?}");
        assert!(!rendered.contains("100.64.1.9"), "{rendered}");
        assert!(!rendered.contains("cannot reach"), "{rendered}");
    }

    #[test]
    fn rejects_invalid_provider_records_without_echoing_them() {
        for provider in ["192.168.1.4", "gpu-box.tailnet-example.ts.net:41641"] {
            let json = format!(
                r#"{{
                    "schema": "fastlink.discover.v1",
                    "scope": "user_opt_in_lan_tailscale",
                    "providers": [{{"provider": "{provider}", "status": "available"}}],
                    "nodes": []
                }}"#
            );
            let error = parse_discovery_json(&json).expect_err("must reject");
            assert_eq!(error, DiscoveryParseError::InvalidProvider { index: 0 });
            assert!(!error.to_string().contains(provider));
        }

        let duplicate = r#"{
            "schema": "fastlink.discover.v1",
            "scope": "user_opt_in_lan_tailscale",
            "providers": [
                {"provider": "lan_mdns", "status": "available"},
                {"provider": "lan_mdns", "status": "unavailable"}
            ],
            "nodes": []
        }"#;
        assert_eq!(
            parse_discovery_json(duplicate).expect_err("must reject duplicates"),
            DiscoveryParseError::InvalidProvider { index: 1 }
        );
    }

    #[test]
    fn address_like_display_name_is_withheld() {
        for name in [
            "192.168.1.50",
            "office-192.168.1.50.local",
            "fe80::1",
            "aa:bb:cc:dd:ee:ff",
            "https://example.local/device",
            "gpu-box.tailnet-example.ts.net:41641",
            "GPU-BOX.TAILNET-EXAMPLE.TS.NET:443",
            "host.example:65535",
            "192.168.1.50:41641",
            "gpu-box.tailnet-example.ts.net:41641/path",
        ] {
            let json = format!(
                r#"{{
                    "schema": "fastlink.discover.v1",
                    "scope": "user_opt_in_lan_tailscale",
                    "providers": [],
                    "nodes": [{{
                        "node_id": "candidate-1",
                        "display_name": "{name}",
                        "os": "{name}",
                        "arch": "{name}",
                        "gpus": [],
                        "transports": ["lan"],
                        "worker_state": "unverified",
                        "trust_state": "unpaired"
                    }}]
                }}"#
            );
            let discovery = parse_discovery_json(&json).expect("safe candidate still parses");
            assert_eq!(discovery.nodes[0].display_name, None, "{name}");
            assert_eq!(discovery.nodes[0].os, None, "{name}");
            assert_eq!(discovery.nodes[0].arch, None, "{name}");
            let rendered = crate::view::candidate_rows(&discovery.nodes[0])
                .into_iter()
                .map(|row| row.value)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!rendered.contains(name), "{name}");
            assert!(!format!("{:?}", discovery).contains(name), "{name}");
        }
    }

    #[test]
    fn ordinary_dns_display_name_without_port_remains_usable() {
        let name = "gpu-box.tailnet-example.ts.net";
        let json = format!(
            r#"{{
                "schema": "fastlink.discover.v1",
                "scope": "user_opt_in_lan_tailscale",
                "providers": [],
                "nodes": [{{
                    "node_id": "candidate-1",
                    "display_name": "{name}",
                    "os": "linux",
                    "arch": "x86_64",
                    "gpus": [],
                    "transports": ["lan"],
                    "worker_state": "unverified",
                    "trust_state": "unpaired"
                }}]
            }}"#
        );
        let discovery = parse_discovery_json(&json).expect("ordinary DNS name is allowed");
        let node = &discovery.nodes[0];
        assert_eq!(node.display_name.as_deref(), Some(name));
        assert_eq!(node.os.as_deref(), Some("linux"));
        assert_eq!(node.arch.as_deref(), Some("x86_64"));

        for label in ["service:443", "version 1.2", "Example LAN device"] {
            let json = format!(
                r#"{{
                    "schema": "fastlink.discover.v1",
                    "scope": "user_opt_in_lan_tailscale",
                    "providers": [],
                    "nodes": [{{
                        "node_id": "candidate-1",
                        "display_name": "{label}",
                        "gpus": [],
                        "transports": ["lan"]
                    }}]
                }}"#
            );
            let discovery = parse_discovery_json(&json).expect("ordinary label is allowed");
            assert_eq!(discovery.nodes[0].display_name.as_deref(), Some(label));
        }
    }

    #[test]
    fn address_like_or_malformed_node_id_fails_closed() {
        for node_id in ["fe80::1", "gpu-box.tailnet-example.ts.net:41641"] {
            let json = format!(
                r#"{{
                    "schema": "fastlink.discover.v1",
                    "scope": "user_opt_in_lan_tailscale",
                    "providers": [],
                    "nodes": [{{
                        "node_id": "{node_id}",
                        "gpus": [],
                        "transports": ["lan"]
                    }}]
                }}"#
            );
            let error = parse_discovery_json(&json).expect_err("must fail closed");
            assert_eq!(error, DiscoveryParseError::InvalidNodeId { index: 0 });
            assert!(!error.to_string().contains(node_id));
        }
    }

    #[test]
    fn unknown_optional_metadata_stays_unknown() {
        let json = r#"{
            "schema": "fastlink.discover.v1",
            "scope": "user_opt_in_lan_tailscale",
            "providers": [],
            "nodes": [{
                "node_id": "candidate-2",
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
        let discovery = parse_discovery_json(json).expect("nulls are allowed");
        let node = &discovery.nodes[0];
        assert_eq!(node.display_name, None);
        assert_eq!(node.os, None);
        assert_eq!(node.arch, None);
        assert_eq!(node.logical_cpu_count, None);
        assert_eq!(node.ram_total_bytes, None);
        assert_eq!(node.ram_available_bytes, None);
        assert_eq!(node.transports, []);
        assert_eq!(node.worker_state, None);
        assert_eq!(node.trust_state, None);
    }

    #[test]
    fn unknown_status_and_transport_labels_render_as_unknown_enum_values() {
        let json = r#"{
            "schema": "fastlink.discover.v1",
            "scope": "user_opt_in_lan_tailscale",
            "providers": [{"provider": "future_provider", "status": "future_status"}],
            "nodes": [{
                "node_id": "candidate-3",
                "gpus": [],
                "transports": ["future_transport"],
                "worker_state": "future_worker_state",
                "trust_state": "future_trust_state"
            }]
        }"#;
        let discovery = parse_discovery_json(json).expect("unknown labels parse safely");
        assert_eq!(discovery.providers[0].status, ProviderStatus::Unknown);
        assert_eq!(discovery.nodes[0].transports, [Transport::Unknown]);
        assert_eq!(discovery.nodes[0].worker_state, Some(WorkerState::Unknown));
        assert_eq!(discovery.nodes[0].trust_state, Some(TrustState::Unknown));
    }

    #[test]
    fn rejects_malformed_json_and_oversized_arrays() {
        assert_eq!(
            parse_discovery_json("{ not json").expect_err("must fail"),
            DiscoveryParseError::Json
        );

        let mut providers = Vec::new();
        for index in 0..=MAX_PROVIDERS {
            providers.push(format!(
                "{{\"provider\":\"provider_{index}\",\"status\":\"available\"}}"
            ));
        }
        let too_many = format!(
            "{{\"schema\":\"{DISCOVERY_SCHEMA}\",\"scope\":\"{DISCOVERY_SCOPE}\",\"providers\":[{}],\"nodes\":[]}}",
            providers.join(",")
        );
        assert_eq!(
            parse_discovery_json(&too_many).expect_err("must bound providers"),
            DiscoveryParseError::TooManyProviders
        );
    }
}
