//! Opt-in Linux LAN mDNS and Tailscale candidate peer discovery (`fastlink discover [--json]`).
//!
//! Never runs on CLI or app startup; executes only when explicitly requested.
//! Keeps raw peer IP/MAC/endpoint details strictly transient and absent from
//! public types, serialized JSON, logs, and errors.

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
#[cfg(target_os = "linux")]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::thread;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(target_os = "linux")]
use serde::Deserialize;
use serde::Serialize;

pub const DISCOVER_SCHEMA: &str = "fastlink.discover.v1";
pub const DISCOVER_SCOPE: &str = "user_opt_in_lan_tailscale";
pub const DEFAULT_PROVIDER_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "linux")]
const POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(target_os = "linux")]
const POST_EXIT_DRAIN: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoverArgs {
    pub json: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderStatus {
    pub provider: String,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiscoveredGpu {
    pub name: Option<String>,
    pub vram_total_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiscoveredNode {
    pub node_id: String,
    pub display_name: String,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub logical_cpu_count: Option<usize>,
    pub ram_total_bytes: Option<u64>,
    pub ram_available_bytes: Option<u64>,
    pub gpus: Vec<DiscoveredGpu>,
    pub transports: Vec<String>,
    pub worker_state: String,
    pub trust_state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiscoveryReport {
    pub schema: String,
    pub scope: String,
    pub providers: Vec<ProviderStatus>,
    pub nodes: Vec<DiscoveredNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryConfig {
    pub lan_mdns_program: PathBuf,
    pub tailscale_program: PathBuf,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            lan_mdns_program: PathBuf::from("avahi-browse"),
            tailscale_program: PathBuf::from("tailscale"),
            timeout: DEFAULT_PROVIDER_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

#[cfg(target_os = "linux")]
/// Ephemeral routing info kept strictly inside provider parsing; never serialized
/// or returned in public discovery output.
#[allow(dead_code)]
#[derive(Debug)]
struct TransientEndpoint {
    addresses: Vec<String>,
    port: Option<u16>,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum CaptureError {
    NotFound,
    SpawnFailed,
    Timeout,
    Cancelled,
    OutputLimitExceeded,
    IoFailed,
}

#[cfg(target_os = "linux")]
struct ProviderOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

pub fn parse_discover_args(args: &[String]) -> Result<DiscoverArgs, String> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => {
                if json {
                    return Err("duplicate '--json' flag".to_string());
                }
                json = true;
            }
            _ => return Err("unknown discover argument".to_string()),
        }
    }
    Ok(DiscoverArgs { json })
}

pub fn discover_with_config(
    config: &DiscoveryConfig,
    cancel: Option<&AtomicBool>,
) -> DiscoveryReport {
    #[cfg(target_os = "linux")]
    {
        discover_linux(config, cancel)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        unsupported_platform_report(cancel)
    }
}

fn is_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::Acquire))
}

#[cfg(any(not(target_os = "linux"), test))]
fn unsupported_platform_report(cancel: Option<&AtomicBool>) -> DiscoveryReport {
    let cancelled = is_cancelled(cancel);
    let (status, lan_msg, ts_msg) = if cancelled {
        (
            "cancelled",
            "mDNS discovery cancelled",
            "Tailscale discovery cancelled",
        )
    } else {
        (
            "unavailable",
            "mDNS discovery is only supported on Linux",
            "Tailscale discovery is only supported on Linux",
        )
    };
    let mut report = DiscoveryReport {
        schema: DISCOVER_SCHEMA.to_string(),
        scope: DISCOVER_SCOPE.to_string(),
        providers: vec![
            ProviderStatus {
                provider: "lan_mdns".to_string(),
                status: status.to_string(),
                message: Some(lan_msg.to_string()),
            },
            ProviderStatus {
                provider: "tailscale".to_string(),
                status: status.to_string(),
                message: Some(ts_msg.to_string()),
            },
        ],
        nodes: Vec::new(),
    };
    enforce_report_redaction(&mut report);
    report
}

#[cfg(target_os = "linux")]
fn discover_linux(config: &DiscoveryConfig, cancel: Option<&AtomicBool>) -> DiscoveryReport {
    let mut providers = Vec::with_capacity(2);
    let mut nodes = Vec::new();

    let (mdns_status, mdns_nodes) = run_lan_mdns_provider(config, cancel);
    providers.push(mdns_status);
    nodes.extend(mdns_nodes);

    let (ts_status, ts_nodes) = run_tailscale_provider(config, cancel);
    providers.push(ts_status);
    nodes.extend(ts_nodes);

    nodes.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
            .then_with(|| a.node_id.cmp(&b.node_id))
    });

    let mut report = DiscoveryReport {
        schema: DISCOVER_SCHEMA.to_string(),
        scope: DISCOVER_SCOPE.to_string(),
        providers,
        nodes,
    };
    enforce_report_redaction(&mut report);
    report
}

#[cfg(target_os = "linux")]
const MDNS_SERVICE_TYPE: &str = "_fastlink._tcp";

#[cfg(target_os = "linux")]
fn run_lan_mdns_provider(
    config: &DiscoveryConfig,
    cancel: Option<&AtomicBool>,
) -> (ProviderStatus, Vec<DiscoveredNode>) {
    let output = match run_bounded_provider(
        &config.lan_mdns_program,
        &["-t", "-p", "-r", MDNS_SERVICE_TYPE],
        config.timeout,
        config.max_output_bytes,
        cancel,
    ) {
        Ok(output) => output,
        Err(err) => {
            return (
                map_provider_capture_error("lan_mdns", "mDNS", err),
                Vec::new(),
            )
        }
    };

    if !output.status.success() {
        return (
            ProviderStatus {
                provider: "lan_mdns".to_string(),
                status: "unavailable".to_string(),
                message: Some("mDNS CLI failed (output redacted)".to_string()),
            },
            Vec::new(),
        );
    }

    let Ok(stdout_str) = std::str::from_utf8(&output.stdout) else {
        return (malformed_mdns_status(), Vec::new());
    };

    let Ok(nodes) = parse_avahi_browse_output(stdout_str) else {
        return (malformed_mdns_status(), Vec::new());
    };

    (
        ProviderStatus {
            provider: "lan_mdns".to_string(),
            status: "available".to_string(),
            message: None,
        },
        nodes,
    )
}

#[cfg(target_os = "linux")]
fn malformed_mdns_status() -> ProviderStatus {
    ProviderStatus {
        provider: "lan_mdns".to_string(),
        status: "error".to_string(),
        message: Some("mDNS discovery output malformed".to_string()),
    }
}

#[cfg(target_os = "linux")]
fn parse_avahi_browse_output(stdout: &str) -> Result<Vec<DiscoveredNode>, ()> {
    let mut nodes_by_id: BTreeMap<String, DiscoveredNode> = BTreeMap::new();

    for raw_line in stdout.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.trim().is_empty() {
            continue;
        }
        let fields = split_avahi_fields(line)?;
        match fields.first().map(String::as_str) {
            Some("+") | Some("-") => {
                if fields.len() != 6
                    || fields[1].trim().is_empty()
                    || fields[2].trim().is_empty()
                    || fields[3].trim().is_empty()
                    || fields[4].trim() != MDNS_SERVICE_TYPE
                    || fields[5].trim().is_empty()
                {
                    return Err(());
                }
            }
            Some("=") => {
                if fields.len() != 10 {
                    return Err(());
                }
                let iface = fields[1].trim();
                let proto = fields[2].trim();
                let service_name = &fields[3];
                let service_type = fields[4].trim();
                let domain = fields[5].trim();
                let hostname = fields[6].trim();
                let address = fields[7].trim();
                let port_str = fields[8].trim();
                let txt_raw = &fields[9];

                if iface.is_empty()
                    || proto.is_empty()
                    || service_name.trim().is_empty()
                    || service_type != MDNS_SERVICE_TYPE
                    || domain.is_empty()
                    || hostname.is_empty()
                    || address.is_empty()
                {
                    return Err(());
                }
                let port = port_str.parse::<u16>().map_err(|_| ())?;
                if port == 0 {
                    return Err(());
                }

                // Keep mDNS target hostname, IP address, and port strictly transient.
                let _transient = TransientEndpoint {
                    addresses: vec![hostname.to_string(), address.to_string()],
                    port: Some(port),
                };

                let txt = parse_avahi_txt_metadata(txt_raw)?;
                let identity_material = txt
                    .node_id
                    .unwrap_or_else(|| format!("{service_type}:{domain}:{service_name}"));
                let node_id = opaque_node_id("lan_mdns", &identity_material);

                let display_name = sanitize_display_name(service_name)
                    .unwrap_or_else(|| "Unverified LAN peer".to_string());

                let candidate = DiscoveredNode {
                    node_id: node_id.clone(),
                    display_name,
                    os: txt.os,
                    arch: txt.arch,
                    logical_cpu_count: txt.logical_cpu_count,
                    ram_total_bytes: txt.ram_total_bytes,
                    ram_available_bytes: txt.ram_available_bytes,
                    gpus: txt.gpus,
                    transports: vec!["lan".to_string()],
                    worker_state: "unverified".to_string(),
                    trust_state: "unpaired".to_string(),
                };

                match nodes_by_id.get_mut(&node_id) {
                    Some(existing) => merge_discovered_node(existing, candidate),
                    None => {
                        nodes_by_id.insert(node_id, candidate);
                    }
                }
            }
            _ => return Err(()),
        }
    }

    Ok(nodes_by_id.into_values().collect())
}

#[cfg(target_os = "linux")]
fn merge_discovered_node(existing: &mut DiscoveredNode, candidate: DiscoveredNode) {
    if existing.os.is_none() {
        existing.os = candidate.os;
    }
    if existing.arch.is_none() {
        existing.arch = candidate.arch;
    }
    if existing.logical_cpu_count.is_none() {
        existing.logical_cpu_count = candidate.logical_cpu_count;
    }
    if existing.ram_total_bytes.is_none() {
        existing.ram_total_bytes = candidate.ram_total_bytes;
    }
    if existing.ram_available_bytes.is_none() {
        existing.ram_available_bytes = candidate.ram_available_bytes;
    }
    if existing.gpus.is_empty() && !candidate.gpus.is_empty() {
        existing.gpus = candidate.gpus;
    }
    for transport in candidate.transports {
        if !existing.transports.contains(&transport) {
            existing.transports.push(transport);
        }
    }
}

#[cfg(target_os = "linux")]
fn split_avahi_fields(line: &str) -> Result<Vec<String>, ()> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match ch {
            ';' => {
                fields.push(std::mem::take(&mut current));
            }
            '\\' => match chars.next() {
                Some(';') => current.push(';'),
                Some('\\') => current.push('\\'),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => return Err(()),
            },
            other => current.push(other),
        }
    }
    fields.push(current);
    Ok(fields)
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct MdnsTxtMetadata {
    node_id: Option<String>,
    os: Option<String>,
    arch: Option<String>,
    logical_cpu_count: Option<usize>,
    ram_total_bytes: Option<u64>,
    ram_available_bytes: Option<u64>,
    gpus: Vec<DiscoveredGpu>,
}

#[cfg(target_os = "linux")]
fn parse_avahi_txt_metadata(raw: &str) -> Result<MdnsTxtMetadata, ()> {
    let entries = parse_avahi_txt_entries(raw)?;
    let mut meta = MdnsTxtMetadata::default();
    let mut gpus_by_index: BTreeMap<usize, (Option<String>, Option<u64>)> = BTreeMap::new();

    for (key, value) in entries {
        match key.as_str() {
            "node_id" => {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    meta.node_id = Some(trimmed.to_string());
                }
            }
            "os" => {
                meta.os = sanitize_metadata_label(&value);
            }
            "arch" => {
                meta.arch = sanitize_metadata_label(&value);
            }
            "logical_cpu_count" => {
                meta.logical_cpu_count = value.trim().parse::<usize>().ok().filter(|&n| n > 0);
            }
            "ram_total_bytes" => {
                meta.ram_total_bytes = value.trim().parse::<u64>().ok().filter(|&n| n > 0);
            }
            "ram_available_bytes" => {
                meta.ram_available_bytes = value.trim().parse::<u64>().ok();
            }
            other => {
                if let Some((idx, is_name)) = parse_gpu_txt_key(other) {
                    let entry = gpus_by_index.entry(idx).or_insert((None, None));
                    if is_name {
                        entry.0 = sanitize_metadata_label(&value);
                    } else {
                        entry.1 = value.trim().parse::<u64>().ok().filter(|&v| v > 0);
                    }
                }
            }
        }
    }

    meta.gpus = gpus_by_index
        .into_values()
        .filter(|(name, vram)| name.is_some() || vram.is_some())
        .map(|(name, vram_total_bytes)| DiscoveredGpu {
            name,
            vram_total_bytes,
        })
        .collect();

    Ok(meta)
}

#[cfg(target_os = "linux")]
fn parse_gpu_txt_key(key: &str) -> Option<(usize, bool)> {
    let rest = key.strip_prefix("gpu")?;
    if let Some(digits) = rest.strip_suffix("_name") {
        let idx = digits.parse::<usize>().ok()?;
        (idx < 64).then_some((idx, true))
    } else if let Some(digits) = rest.strip_suffix("_vram_total_bytes") {
        let idx = digits.parse::<usize>().ok()?;
        (idx < 64).then_some((idx, false))
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn parse_avahi_txt_entries(raw: &str) -> Result<Vec<(String, String)>, ()> {
    let mut entries = Vec::new();
    let mut chars = raw.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch.is_ascii_whitespace() {
            chars.next();
            continue;
        }
        if ch != '"' {
            return Err(());
        }
        chars.next();

        let mut item = String::new();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    closed = true;
                    break;
                }
                '\\' => {
                    let next = chars.next().ok_or(())?;
                    item.push(next);
                }
                other => item.push(other),
            }
        }
        if !closed {
            return Err(());
        }
        if let Some((key, value)) = item.split_once('=') {
            let trimmed_key = key.trim();
            if !trimmed_key.is_empty() {
                entries.push((trimmed_key.to_string(), value.to_string()));
            }
        }
    }

    Ok(entries)
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
struct TailscaleStatusDoc {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "Peer")]
    peer: Option<BTreeMap<String, TailscalePeer>>,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
struct TailscalePeer {
    #[serde(rename = "ID")]
    id: Option<String>,
    #[serde(rename = "PublicKey")]
    public_key: Option<String>,
    #[serde(rename = "HostName")]
    host_name: Option<String>,
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(rename = "OS")]
    os: Option<String>,
    #[serde(rename = "Online")]
    online: Option<bool>,
    #[serde(rename = "TailscaleIPs")]
    tailscale_ips: Option<Vec<String>>,
    #[serde(rename = "CurAddr")]
    cur_addr: Option<String>,
    #[serde(rename = "Addrs")]
    addrs: Option<Vec<String>>,
}

#[cfg(target_os = "linux")]
fn run_tailscale_provider(
    config: &DiscoveryConfig,
    cancel: Option<&AtomicBool>,
) -> (ProviderStatus, Vec<DiscoveredNode>) {
    let output = match run_bounded_provider(
        &config.tailscale_program,
        &["status", "--json"],
        config.timeout,
        config.max_output_bytes,
        cancel,
    ) {
        Ok(output) => output,
        Err(err) => {
            return (
                map_provider_capture_error("tailscale", "Tailscale", err),
                Vec::new(),
            )
        }
    };

    if !output.status.success() {
        return (
            ProviderStatus {
                provider: "tailscale".to_string(),
                status: "unavailable".to_string(),
                message: Some("Tailscale CLI failed (output redacted)".to_string()),
            },
            Vec::new(),
        );
    }

    let doc: TailscaleStatusDoc = match serde_json::from_slice(&output.stdout) {
        Ok(doc) => doc,
        Err(_) => {
            return (
                ProviderStatus {
                    provider: "tailscale".to_string(),
                    status: "error".to_string(),
                    message: Some("Tailscale status JSON malformed".to_string()),
                },
                Vec::new(),
            )
        }
    };

    if doc.backend_state != "Running" {
        let state_label = sanitize_safe_token(&doc.backend_state).unwrap_or("Unknown");
        return (
            ProviderStatus {
                provider: "tailscale".to_string(),
                status: "unavailable".to_string(),
                message: Some(format!("Tailscale is not running (state: {state_label})")),
            },
            Vec::new(),
        );
    }

    let mut nodes = Vec::new();
    if let Some(peers) = doc.peer {
        for (map_key, peer) in peers {
            if peer.online != Some(true) {
                continue;
            }
            // Hold raw addresses strictly transiently inside provider scope only.
            let mut transient_addrs = peer.tailscale_ips.unwrap_or_default();
            if let Some(cur) = peer.cur_addr {
                transient_addrs.push(cur);
            }
            if let Some(addrs) = peer.addrs {
                transient_addrs.extend(addrs);
            }
            let _transient = TransientEndpoint {
                addresses: transient_addrs,
                port: None,
            };

            let identity_material = peer
                .id
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or(peer.public_key.as_deref().filter(|s| !s.trim().is_empty()))
                .unwrap_or(map_key.as_str());
            let node_id = opaque_node_id("tailscale", identity_material);

            let display_name = peer
                .host_name
                .as_deref()
                .and_then(sanitize_display_name)
                .or_else(|| {
                    peer.dns_name
                        .as_deref()
                        .filter(|dns| !contains_network_address_or_mac(dns))
                        .and_then(|dns| dns.trim_end_matches('.').split('.').next())
                        .and_then(sanitize_display_name)
                })
                .unwrap_or_else(|| "Unverified Tailscale peer".to_string());

            let os = peer.os.as_deref().and_then(sanitize_metadata_label);

            nodes.push(DiscoveredNode {
                node_id,
                display_name,
                os,
                arch: None,
                logical_cpu_count: None,
                ram_total_bytes: None,
                ram_available_bytes: None,
                gpus: Vec::new(),
                transports: vec!["tailscale".to_string()],
                worker_state: "unverified".to_string(),
                trust_state: "unpaired".to_string(),
            });
        }
    }

    (
        ProviderStatus {
            provider: "tailscale".to_string(),
            status: "available".to_string(),
            message: None,
        },
        nodes,
    )
}

#[cfg(target_os = "linux")]
fn map_provider_capture_error(provider: &str, label: &str, err: CaptureError) -> ProviderStatus {
    let (status, message) = match err {
        CaptureError::NotFound => ("unavailable", format!("{label} CLI unavailable")),
        CaptureError::SpawnFailed => ("unavailable", format!("{label} CLI failed to start")),
        CaptureError::Timeout => ("error", format!("{label} provider timed out")),
        CaptureError::Cancelled => ("cancelled", format!("{label} discovery cancelled")),
        CaptureError::OutputLimitExceeded => {
            ("error", format!("{label} output exceeded byte limit"))
        }
        CaptureError::IoFailed => ("error", format!("{label} capture failed (output redacted)")),
    };
    ProviderStatus {
        provider: provider.to_string(),
        status: status.to_string(),
        message: Some(message),
    }
}

#[cfg(any(target_os = "linux", test))]
fn opaque_node_id(provider: &str, material: &str) -> String {
    let input = format!("fastlink-discover-v1:{provider}:{material}");
    let hex = blake3::hash(input.as_bytes()).to_hex().to_string();
    hex[..32].to_string()
}

#[cfg(target_os = "linux")]
fn sanitize_safe_token(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 48
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some(trimmed)
}

fn sanitize_display_name(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || contains_network_address_or_mac(trimmed) {
        return None;
    }
    Some(trimmed.to_string())
}

fn sanitize_metadata_label(raw: &str) -> Option<String> {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("unknown")
        || contains_network_address_or_mac(trimmed)
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn contains_network_address_or_mac(text: &str) -> bool {
    contains_ipv4(text) || contains_ipv6(text) || contains_mac(text) || contains_dns_host_port(text)
}

fn contains_ipv4(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let slice = text[start..i].trim_matches('.');
            if slice.parse::<Ipv4Addr>().is_ok() {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}

fn contains_ipv6(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_ipv6_char(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_ipv6_char(bytes[i]) {
                i += 1;
            }
            let run = &text[start..i];
            if run.contains(':') {
                let candidate = run.split('%').next().unwrap_or(run);
                if candidate.parse::<Ipv6Addr>().is_ok() {
                    return true;
                }
                for (offset, ch) in candidate.char_indices() {
                    if ch == ':' || ch.is_ascii_hexdigit() {
                        let sub = &candidate[offset..];
                        if sub.contains(':') && sub.parse::<Ipv6Addr>().is_ok() {
                            return true;
                        }
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    false
}

fn is_ipv6_char(b: u8) -> bool {
    b.is_ascii_hexdigit() || b == b':' || b == b'.' || b == b'%'
}

fn contains_mac(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() < 17 {
        return false;
    }
    for start in 0..=bytes.len() - 17 {
        let sep = bytes[start + 2];
        if sep != b':' && sep != b'-' {
            continue;
        }
        let left_ok = start == 0 || !bytes[start - 1].is_ascii_hexdigit();
        let right_ok = start + 17 == bytes.len() || !bytes[start + 17].is_ascii_hexdigit();
        if !left_ok || !right_ok {
            continue;
        }
        let window = &bytes[start..start + 17];
        let matched = window.chunks(3).enumerate().all(|(idx, chunk)| {
            if idx < 5 {
                chunk.len() == 3
                    && chunk[0].is_ascii_hexdigit()
                    && chunk[1].is_ascii_hexdigit()
                    && chunk[2] == sep
            } else {
                chunk.len() == 2 && chunk[0].is_ascii_hexdigit() && chunk[1].is_ascii_hexdigit()
            }
        });
        if matched {
            return true;
        }
    }
    false
}

/// Bounded O(N) detector for `host:port` endpoints with valid nonzero ports
/// (`1..=65535`, including zero-padded ports) and either a valid ASCII
/// hostname (single- or multi-label) or a bounded non-ASCII non-whitespace /
/// non-control token.
fn contains_dns_host_port(text: &str) -> bool {
    const MAX_HOST_SCAN_CHARS: usize = 253;
    const MAX_HOST_SCAN_BYTES: usize = 512;
    let bytes = text.as_bytes();

    for (colon_idx, &b) in bytes.iter().enumerate() {
        if b != b':' || colon_idx == 0 {
            continue;
        }

        // Normalize leading zeros in a streaming pass (`O(1)` auxiliary space;
        // digit runs right of distinct `:` positions are disjoint, so `O(N)` total).
        let p_start = colon_idx + 1;
        let mut p_end = p_start;
        let mut nonzero_digits = 0usize;
        let mut port: u32 = 0;
        while p_end < bytes.len() && bytes[p_end].is_ascii_digit() {
            let digit = bytes[p_end];
            if nonzero_digits > 0 || digit != b'0' {
                nonzero_digits += 1;
                if nonzero_digits <= 5 {
                    port = port * 10 + u32::from(digit - b'0');
                }
            }
            p_end += 1;
        }
        if p_end == p_start {
            continue;
        }
        if let Some(next_ch) = text[p_end..].chars().next() {
            if next_ch.is_alphanumeric() || matches!(next_ch, '_' | '-' | ':') {
                continue;
            }
            if next_ch == '.' && bytes.get(p_end + 1).is_some_and(u8::is_ascii_digit) {
                continue;
            }
        }
        if nonzero_digits == 0 || nonzero_digits > 5 || !(1..=65535).contains(&port) {
            continue;
        }

        // Scan bounded host token slice left of `:`.
        let mut h_start = colon_idx;
        let mut char_count = 0usize;
        let mut hit_bound = false;
        for (idx, ch) in text[..colon_idx].char_indices().rev() {
            if !is_host_token_char(ch) {
                break;
            }
            char_count += 1;
            if char_count > MAX_HOST_SCAN_CHARS + 1 || colon_idx - idx > MAX_HOST_SCAN_BYTES + 1 {
                hit_bound = true;
                break;
            }
            h_start = idx;
        }
        let raw_host = &text[h_start..colon_idx];
        let host = raw_host.strip_suffix('.').unwrap_or(raw_host);
        if host.is_empty() {
            continue;
        }
        if host.is_ascii() {
            if !hit_bound && is_valid_ascii_hostname(host) {
                return true;
            }
        } else {
            return true;
        }
    }
    false
}

fn is_host_token_char(ch: char) -> bool {
    if ch.is_ascii() {
        ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_')
    } else {
        !ch.is_whitespace() && !ch.is_control()
    }
}

fn is_valid_ascii_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let mut last_label: Option<&str> = None;
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        let lb = label.as_bytes();
        if !lb[0].is_ascii_alphanumeric()
            || !lb[lb.len() - 1].is_ascii_alphanumeric()
            || !lb
                .iter()
                .all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return false;
        }
        last_label = Some(label);
    }
    last_label.is_some_and(|tld| !tld.contains('_') && tld.bytes().any(|b| b.is_ascii_alphabetic()))
}

#[cfg(target_os = "linux")]
fn run_bounded_provider(
    program: &Path,
    args: &[&str],
    timeout: Duration,
    max_output_bytes: usize,
    cancel: Option<&AtomicBool>,
) -> Result<ProviderOutput, CaptureError> {
    if is_cancelled(cancel) {
        return Err(CaptureError::Cancelled);
    }

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);

    let child = cmd.spawn().map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            CaptureError::NotFound
        } else {
            CaptureError::SpawnFailed
        }
    })?;

    capture_child(child, timeout, max_output_bytes, cancel)
}

#[cfg(target_os = "linux")]
fn kill_process_group(pgid: u32) {
    if let Ok(pgid) = libc::pid_t::try_from(pgid) {
        if pgid > 1 {
            // SAFETY: `process_group(0)` placed `child` and its descendants in group `pgid`,
            // and `pgid > 1` ensures `-pgid < -1` so we only signal the owned group.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn stop_child(child: &mut Child) {
    kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "linux")]
fn child_has_exited(child: &Child) -> io::Result<bool> {
    let pid = libc::id_t::from(child.id());
    loop {
        // SAFETY: Zeroing `siginfo_t` satisfies `waitid` requirements, and `WNOWAIT` inspects
        // exit state without reaping `child` so `pgid` stays pinned until after group cleanup.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: `waitid` populated `info` when `rc == 0`; `si_pid != 0` iff `child` exited.
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

#[cfg(target_os = "linux")]
trait DrainObserver {
    fn on_post_exit_read(&mut self, _child: &Child, _drain_deadline: Instant) {}
    fn after_post_exit_drain(
        &mut self,
        _child: &Child,
        _drain_deadline: Instant,
        _drain_result: &Result<(), CaptureError>,
        _stdout: &PipeReader,
    ) {
    }
    fn after_post_exit_cleanup(&mut self, _drain_deadline: Instant) {}
}

#[cfg(target_os = "linux")]
struct NoopDrainObserver;

#[cfg(target_os = "linux")]
impl DrainObserver for NoopDrainObserver {}

#[cfg(target_os = "linux")]
fn pump_bounded_with<F>(
    stdout: &mut PipeReader,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
    on_read: F,
) -> Result<(), CaptureError>
where
    F: FnMut(),
{
    if stdout.pump_with(deadline, cancel, on_read).is_err() {
        return Err(CaptureError::IoFailed);
    }
    if stdout.cancelled {
        return Err(CaptureError::Cancelled);
    }
    if stdout.is_over() {
        return Err(CaptureError::OutputLimitExceeded);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn pump_bounded(
    stdout: &mut PipeReader,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<(), CaptureError> {
    pump_bounded_with(stdout, deadline, cancel, || {})
}

#[cfg(target_os = "linux")]
fn capture_child(
    child: Child,
    timeout: Duration,
    max_output_bytes: usize,
    cancel: Option<&AtomicBool>,
) -> Result<ProviderOutput, CaptureError> {
    capture_child_with_observer(
        child,
        timeout,
        max_output_bytes,
        cancel,
        &mut NoopDrainObserver,
    )
}

#[cfg(target_os = "linux")]
fn capture_child_with_observer(
    mut child: Child,
    timeout: Duration,
    max_output_bytes: usize,
    cancel: Option<&AtomicBool>,
    observer: &mut impl DrainObserver,
) -> Result<ProviderOutput, CaptureError> {
    let Some(stdout_handle) = child.stdout.take() else {
        stop_child(&mut child);
        return Err(CaptureError::IoFailed);
    };

    let mut stdout = match PipeReader::new(stdout_handle, max_output_bytes) {
        Ok(reader) => reader,
        Err(_) => {
            stop_child(&mut child);
            return Err(CaptureError::IoFailed);
        }
    };

    let started = Instant::now();
    let overall_deadline = started + timeout;
    loop {
        if is_cancelled(cancel) {
            stop_child(&mut child);
            return Err(CaptureError::Cancelled);
        }
        if let Err(err) = pump_bounded(&mut stdout, overall_deadline, cancel) {
            stop_child(&mut child);
            return Err(err);
        }

        match child_has_exited(&child) {
            Ok(true) => break,
            Ok(false) => {
                if started.elapsed() >= timeout {
                    stop_child(&mut child);
                    return Err(CaptureError::Timeout);
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                stop_child(&mut child);
                return Err(CaptureError::IoFailed);
            }
        }
    }

    let drain_deadline = Instant::now() + POST_EXIT_DRAIN;
    let drain_result = pump_bounded_with(&mut stdout, drain_deadline, cancel, || {
        observer.on_post_exit_read(&child, drain_deadline);
    });
    observer.after_post_exit_drain(&child, drain_deadline, &drain_result, &stdout);
    if let Err(err) = drain_result {
        stop_child(&mut child);
        return Err(err);
    }
    if is_cancelled(cancel) {
        stop_child(&mut child);
        return Err(CaptureError::Cancelled);
    }

    kill_process_group(child.id());
    let Ok(status) = child.wait() else {
        return Err(CaptureError::IoFailed);
    };
    observer.after_post_exit_cleanup(drain_deadline);
    if is_cancelled(cancel) {
        return Err(CaptureError::Cancelled);
    }

    Ok(ProviderOutput {
        status,
        stdout: stdout.retained,
    })
}

#[cfg(all(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PumpStop {
    Eof,
    WouldBlock,
    Deadline,
    Over,
    Cancelled,
}

#[cfg(target_os = "linux")]
struct PipeReader {
    fd: RawFd,
    handle: Option<ChildStdout>,
    retained: Vec<u8>,
    cap: usize,
    total: usize,
    eof: bool,
    cancelled: bool,
    #[cfg(test)]
    last_stop: Option<PumpStop>,
}

#[cfg(target_os = "linux")]
impl PipeReader {
    fn new(handle: ChildStdout, cap: usize) -> io::Result<Self> {
        let fd = handle.as_raw_fd();
        set_nonblocking(fd)?;
        Ok(Self {
            fd,
            handle: Some(handle),
            retained: Vec::new(),
            cap,
            total: 0,
            eof: false,
            cancelled: false,
            #[cfg(test)]
            last_stop: None,
        })
    }

    fn pump_with<F>(
        &mut self,
        deadline: Instant,
        cancel: Option<&AtomicBool>,
        mut on_read: F,
    ) -> io::Result<()>
    where
        F: FnMut(),
    {
        if self.eof {
            #[cfg(test)]
            {
                self.last_stop = Some(PumpStop::Eof);
            }
            return Ok(());
        }
        let mut buffer = [0u8; 8192];
        loop {
            if is_cancelled(cancel) {
                self.cancelled = true;
                #[cfg(test)]
                {
                    self.last_stop = Some(PumpStop::Cancelled);
                }
                return Ok(());
            }
            if self.is_over() {
                #[cfg(test)]
                {
                    self.last_stop = Some(PumpStop::Over);
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                #[cfg(test)]
                {
                    self.last_stop = Some(PumpStop::Deadline);
                }
                return Ok(());
            }
            // SAFETY: `fd` is an open, nonblocking pipe descriptor owned by `handle`.
            let read = unsafe {
                libc::read(
                    self.fd,
                    buffer.as_mut_ptr().cast::<libc::c_void>(),
                    buffer.len(),
                )
            };
            if read > 0 {
                let n = read as usize;
                self.total = self.total.saturating_add(n);
                let room = self.cap.saturating_sub(self.retained.len());
                if room > 0 {
                    self.retained.extend_from_slice(&buffer[..room.min(n)]);
                }
                if self.is_over() {
                    #[cfg(test)]
                    {
                        self.last_stop = Some(PumpStop::Over);
                    }
                    return Ok(());
                }
                on_read();
            } else if read == 0 {
                self.eof = true;
                self.handle.take();
                #[cfg(test)]
                {
                    self.last_stop = Some(PumpStop::Eof);
                }
                return Ok(());
            } else {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock => {
                        #[cfg(test)]
                        {
                            self.last_stop = Some(PumpStop::WouldBlock);
                        }
                        return Ok(());
                    }
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(err),
                }
            }
        }
    }

    fn is_over(&self) -> bool {
        self.total > self.cap
    }
}

#[cfg(target_os = "linux")]
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is an open descriptor owned by the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `F_SETFL` with `O_NONBLOCK` is valid on the pipe read end.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn enforce_report_redaction(report: &mut DiscoveryReport) {
    report.schema = DISCOVER_SCHEMA.to_string();
    report.scope = DISCOVER_SCOPE.to_string();

    for provider in &mut report.providers {
        if let Some(msg) = provider.message.as_ref() {
            if msg.chars().any(char::is_control) || contains_network_address_or_mac(msg) {
                provider.message = Some("provider message redacted".to_string());
            }
        }
    }

    report.nodes.retain(|node| is_opaque_node_id(&node.node_id));
    for node in &mut report.nodes {
        let safe_fallback = if node.transports.iter().any(|t| t == "lan") {
            "Unverified LAN peer"
        } else if node.transports.iter().any(|t| t == "tailscale") {
            "Unverified Tailscale peer"
        } else {
            "Unverified peer"
        };
        node.display_name =
            sanitize_display_name(&node.display_name).unwrap_or_else(|| safe_fallback.to_string());
        node.os = node.os.as_deref().and_then(sanitize_metadata_label);
        node.arch = node.arch.as_deref().and_then(sanitize_metadata_label);
        for gpu in &mut node.gpus {
            gpu.name = gpu.name.as_deref().and_then(sanitize_metadata_label);
        }
        node.gpus
            .retain(|gpu| gpu.name.is_some() || gpu.vram_total_bytes.is_some());
        node.transports
            .retain(|t| matches!(t.as_str(), "lan" | "tailscale"));
        node.worker_state = "unverified".to_string();
        node.trust_state = "unpaired".to_string();
    }
    report.nodes.retain(|node| !node.transports.is_empty());

    if validate_discovery_report(report).is_err() {
        report.nodes.clear();
        for provider in &mut report.providers {
            provider.status = "error".to_string();
            provider.message = Some("discovery output redacted by validation gate".to_string());
        }
    }
}

pub fn validate_discovery_report(report: &DiscoveryReport) -> Result<(), String> {
    if report.schema != DISCOVER_SCHEMA {
        return Err("discovery report schema mismatch".to_string());
    }
    if report.scope != DISCOVER_SCOPE {
        return Err("discovery report scope mismatch".to_string());
    }

    for provider in &report.providers {
        if !matches!(provider.provider.as_str(), "lan_mdns" | "tailscale") {
            return Err("unknown discovery provider in report".to_string());
        }
        if !matches!(
            provider.status.as_str(),
            "available" | "unavailable" | "cancelled" | "error"
        ) {
            return Err("invalid discovery provider status".to_string());
        }
        if let Some(message) = provider.message.as_deref() {
            validate_clean_text(message, "provider message")?;
        }
    }

    for node in &report.nodes {
        if !is_opaque_node_id(&node.node_id) {
            return Err("discovery node_id must be a 32-char lowercase hex digest".to_string());
        }
        if node.worker_state != "unverified" {
            return Err("discovery node worker_state must remain unverified".to_string());
        }
        if node.trust_state != "unpaired" {
            return Err("discovery node trust_state must remain unpaired".to_string());
        }
        if node.display_name.trim().is_empty() {
            return Err("discovery node display_name must not be empty".to_string());
        }
        validate_clean_text(&node.display_name, "node display_name")?;
        if let Some(os) = node.os.as_deref() {
            if os.trim().is_empty() {
                return Err("discovery node os must be null rather than empty".to_string());
            }
            validate_clean_text(os, "node os")?;
        }
        if let Some(arch) = node.arch.as_deref() {
            if arch.trim().is_empty() {
                return Err("discovery node arch must be null rather than empty".to_string());
            }
            validate_clean_text(arch, "node arch")?;
        }
        for gpu in &node.gpus {
            if let Some(name) = gpu.name.as_deref() {
                if name.trim().is_empty() {
                    return Err("discovery GPU name must be null rather than empty".to_string());
                }
                validate_clean_text(name, "node GPU name")?;
            }
        }
        if node.transports.is_empty()
            || !node
                .transports
                .iter()
                .all(|t| matches!(t.as_str(), "lan" | "tailscale"))
        {
            return Err("discovery node transports failed validation".to_string());
        }
    }

    let json_val =
        serde_json::to_value(report).map_err(|_| "failed to inspect discovery JSON".to_string())?;
    validate_json_value_redaction(&json_val)?;
    Ok(())
}

fn validate_clean_text(text: &str, field_name: &str) -> Result<(), String> {
    if text.chars().any(char::is_control) {
        return Err(format!("{field_name} contains control characters"));
    }
    if contains_network_address_or_mac(text) {
        return Err(format!("{field_name} failed address redaction check"));
    }
    Ok(())
}

fn is_opaque_node_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn validate_json_value_redaction(value: &serde_json::Value) -> Result<(), String> {
    const FORBIDDEN_KEYS: &[&str] = &[
        "ip",
        "ips",
        "ipaddress",
        "ipaddresses",
        "addr",
        "addrs",
        "address",
        "addresses",
        "endpoint",
        "endpoints",
        "mac",
        "macaddress",
        "curaddr",
        "tailscaleips",
        "port",
        "ports",
        "socket",
        "sockaddr",
        "hostname",
    ];

    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map {
                let normalized: String = key
                    .chars()
                    .filter(|c| *c != '_' && *c != '-')
                    .flat_map(char::to_lowercase)
                    .collect();
                if FORBIDDEN_KEYS.contains(&normalized.as_str()) {
                    return Err("forbidden address key in discovery payload".to_string());
                }
                validate_json_value_redaction(inner)?;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                validate_json_value_redaction(item)?;
            }
        }
        serde_json::Value::String(s) => {
            if contains_network_address_or_mac(s) {
                return Err("raw network address or MAC detected in discovery payload".to_string());
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
    Ok(())
}

pub fn render_discovery_json(report: &DiscoveryReport) -> io::Result<String> {
    validate_discovery_report(report)
        .map_err(|msg| io::Error::new(io::ErrorKind::InvalidData, msg))?;
    let mut json = serde_json::to_string(report)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "failed to serialize report"))?;
    json.push('\n');
    Ok(json)
}

pub fn render_discovery_text(report: &DiscoveryReport) -> io::Result<String> {
    validate_discovery_report(report)
        .map_err(|msg| io::Error::new(io::ErrorKind::InvalidData, msg))?;
    let mut out = String::from("FASTLINK opt-in discovery (LAN mDNS and Tailscale)\n");
    let _ = writeln!(out, "schema: {}", report.schema);
    let _ = writeln!(out, "scope: {}", report.scope);
    for provider in &report.providers {
        let _ = writeln!(
            out,
            "provider {}: {} ({})",
            provider.provider,
            provider.status,
            provider.message.as_deref().unwrap_or("ok")
        );
    }
    let _ = writeln!(out, "nodes: {}", report.nodes.len());
    for node in &report.nodes {
        let _ = writeln!(
            out,
            "- {} [{}] state={} trust={} transports={}",
            node.display_name,
            node.node_id,
            node.worker_state,
            node.trust_state,
            node.transports.join(",")
        );
    }
    Ok(out)
}

pub fn run_discover(args: &[String]) -> io::Result<String> {
    let parsed = parse_discover_args(args)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    let report = discover_with_config(&DiscoveryConfig::default(), None);
    if parsed.json {
        render_discovery_json(&report)
    } else {
        render_discovery_text(&report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_sample_report() -> DiscoveryReport {
        DiscoveryReport {
            schema: DISCOVER_SCHEMA.to_string(),
            scope: DISCOVER_SCOPE.to_string(),
            providers: vec![ProviderStatus {
                provider: "lan_mdns".to_string(),
                status: "available".to_string(),
                message: None,
            }],
            nodes: vec![DiscoveredNode {
                node_id: opaque_node_id("lan_mdns", "sample-peer"),
                display_name: "Sample Peer".to_string(),
                os: Some("linux".to_string()),
                arch: Some("x86_64".to_string()),
                logical_cpu_count: Some(8),
                ram_total_bytes: Some(16_000_000_000),
                ram_available_bytes: Some(8_000_000_000),
                gpus: vec![DiscoveredGpu {
                    name: Some("Integrated GPU".to_string()),
                    vram_total_bytes: None,
                }],
                transports: vec!["lan".to_string()],
                worker_state: "unverified".to_string(),
                trust_state: "unpaired".to_string(),
            }],
        }
    }

    #[test]
    fn validate_discovery_report_accepts_valid_and_rejects_raw_addresses_or_bad_states() {
        let valid = valid_sample_report();
        assert!(validate_discovery_report(&valid).is_ok());
        assert!(render_discovery_json(&valid).is_ok());

        for smuggled in [
            "192.168.1.50",
            "fe80::1",
            "::ffff:192.0.2.1",
            "aa:bb:cc:dd:ee:ff",
        ] {
            let mut bad_name = valid.clone();
            bad_name.nodes[0].display_name = smuggled.to_string();
            let err = render_discovery_json(&bad_name).unwrap_err();
            assert!(!err.to_string().contains(smuggled));

            let mut bad_msg = valid.clone();
            bad_msg.providers[0].message = Some(format!("failed at {smuggled}"));
            let err = validate_discovery_report(&bad_msg).unwrap_err();
            assert!(!err.contains(smuggled));

            let mut bad_gpu = valid.clone();
            bad_gpu.nodes[0].gpus[0].name = Some(format!("GPU {smuggled}"));
            assert!(validate_discovery_report(&bad_gpu).is_err());
        }

        let mut bad_worker = valid.clone();
        bad_worker.nodes[0].worker_state = "ready".to_string();
        assert!(validate_discovery_report(&bad_worker).is_err());

        let mut bad_trust = valid.clone();
        bad_trust.nodes[0].trust_state = "paired".to_string();
        assert!(validate_discovery_report(&bad_trust).is_err());

        let mut bad_id = valid.clone();
        bad_id.nodes[0].node_id = "not-a-32-hex-digest".to_string();
        assert!(validate_discovery_report(&bad_id).is_err());

        let mut bad_schema = valid.clone();
        bad_schema.schema = "fastlink.discover.v0".to_string();
        assert!(render_discovery_json(&bad_schema).is_err());

        for dns_endpoint in [
            "gpu-box.tailnet-example.ts.net:41641",
            "gpu-box:18777",
            "gpu-box:00000000000000000000000018777",
            "gpu-box:0443",
            "host.example:0000000000000000000000000443",
            "node:0443",
            "endpoint=node:0443",
            "endpoint=gpu-box:0443",
            "ws1.local:18777",
            "node-1.lan:8080",
            "api.example.com:443.",
            "endpoint=my.host.internal:65535",
            "host.example:0443",
            "host.example:00443",
            "host.example:065535",
            "büro:41641",
            "bu\u{0308}ro:41641",
            "कि:41641",
            "\u{0915}\u{093F}:00000000000000000000000041641",
            "\u{0915}\u{093C}:41641",
            "\u{0915}\u{093F}:0443",
            "\u{0E01}\u{0E31}:41641",
            "\u{0E01}\u{0E31}.local:0443",
            "endpoint=cafe\u{0301}:0443",
            "büro.local:41641",
            "gpu.bu\u{0308}ro.ts.net:18777",
            "пример.рф:8080",
            "api.büro.de:443.",
            "①:80",
        ] {
            let mut bad_ep = valid.clone();
            bad_ep.nodes[0].display_name = dns_endpoint.to_string();
            let err = render_discovery_json(&bad_ep).unwrap_err();
            assert!(!err.to_string().contains(dns_endpoint));
            assert!(render_discovery_text(&bad_ep).is_err());

            let mut bad_os = valid.clone();
            bad_os.nodes[0].os = Some(format!("linux {dns_endpoint}"));
            assert!(render_discovery_json(&bad_os).is_err());
            assert!(render_discovery_text(&bad_os).is_err());

            let mut bad_arch = valid.clone();
            bad_arch.nodes[0].arch = Some(format!("x86_64 {dns_endpoint}"));
            assert!(render_discovery_json(&bad_arch).is_err());
            assert!(render_discovery_text(&bad_arch).is_err());

            let mut bad_gpu = valid.clone();
            bad_gpu.nodes[0].gpus[0].name = Some(format!("GPU {dns_endpoint}"));
            assert!(render_discovery_json(&bad_gpu).is_err());
            assert!(render_discovery_text(&bad_gpu).is_err());

            let mut bad_msg = valid.clone();
            bad_msg.providers[0].message = Some(format!("dial {dns_endpoint}"));
            let msg_err = render_discovery_json(&bad_msg).unwrap_err();
            assert!(!msg_err.to_string().contains(dns_endpoint));
            assert!(render_discovery_text(&bad_msg).is_err());
        }

        for ordinary in [
            "Workstation One",
            "gpu-box",
            "Büro Workstation",
            "Bu\u{0308}ro Workstation",
            "München Node",
            "कि Node",
            "\u{0E01}\u{0E31} Node",
            "gpu-box.tailnet-example.ts.net",
            "büro.local",
            "bu\u{0308}ro.local",
            "Radeon RX 7900 XT",
            "Model: 70B",
            "Qwen2.5:72b",
            "llama-3.1:8b",
            "service:port",
            "büro:port",
            "bu\u{0308}ro:port",
            "कि:port",
            "\u{0E01}\u{0E31}:port",
            "PCI 00:02.0",
            "cuda:0",
            "node:0",
            "step-1.2:10",
            "v1.0.2:4",
            "gpu-box:0",
            "gpu-box:00",
            "gpu-box:0000000000000000000000000",
            "gpu-box:65536",
            "gpu-box:0443abc",
            "gpu-box:00000000000000000000000018777abc",
            "bad.host:0",
            "bad.host:00",
            "bad.host:000",
            "bad.host:65536",
            "bad.host:065536",
            "bad.host:00000000000000000000000065536",
            "bad.host:999999",
            "bad.host:000000000000000000000000999999",
            "bad.host:0443abc",
            "bad.host:0443.1",
            "bad.host:0443:1",
            "büro:0",
            "bu\u{0308}ro:0",
            "कि:0",
            "\u{0E01}\u{0E31}:0",
            "कि:65536",
            "büro:65536",
            "büro:0443abc",
        ] {
            assert!(
                !contains_network_address_or_mac(ordinary),
                "ordinary label falsely redacted: {ordinary}"
            );
        }
    }

    #[test]
    fn unsupported_platform_report_is_truthful_without_nodes() {
        let unavailable = unsupported_platform_report(None);
        assert!(validate_discovery_report(&unavailable).is_ok());
        assert_eq!(unavailable.providers.len(), 2);
        assert_eq!(unavailable.providers[0].provider, "lan_mdns");
        assert_eq!(unavailable.providers[0].status, "unavailable");
        assert_eq!(
            unavailable.providers[0].message.as_deref(),
            Some("mDNS discovery is only supported on Linux")
        );
        assert_eq!(unavailable.providers[1].provider, "tailscale");
        assert_eq!(unavailable.providers[1].status, "unavailable");
        assert_eq!(
            unavailable.providers[1].message.as_deref(),
            Some("Tailscale discovery is only supported on Linux")
        );
        assert!(unavailable.nodes.is_empty());

        let cancel = AtomicBool::new(true);
        let cancelled = unsupported_platform_report(Some(&cancel));
        assert!(validate_discovery_report(&cancelled).is_ok());
        assert_eq!(cancelled.providers[0].status, "cancelled");
        assert_eq!(cancelled.providers[1].status, "cancelled");
        assert!(cancelled.nodes.is_empty());
    }

    #[test]
    fn parse_discover_args_never_echoes_unknown_tokens() {
        assert_eq!(parse_discover_args(&[]), Ok(DiscoverArgs { json: false }));
        assert_eq!(
            parse_discover_args(&["--json".to_string()]),
            Ok(DiscoverArgs { json: true })
        );
        assert_eq!(
            parse_discover_args(&["--json".to_string(), "--json".to_string()]),
            Err("duplicate '--json' flag".to_string())
        );

        for bad in ["192.0.2.50", "gpu-box:18777", "--bad-flag"] {
            let err = parse_discover_args(&[bad.to_string()]).unwrap_err();
            assert_eq!(err, "unknown discover argument");
            assert!(!err.contains(bad));
        }
    }

    #[cfg(target_os = "linux")]
    static DRAIN_TEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[cfg(target_os = "linux")]
    fn is_lock_free(lock_file: &std::path::Path) -> bool {
        Command::new("/usr/bin/flock")
            .arg("-n")
            .arg(lock_file)
            .arg("true")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[cfg(target_os = "linux")]
    fn wait_for_lock_release(lock_file: &std::path::Path, wait_for: Duration) -> bool {
        let lock_deadline = Instant::now() + wait_for;
        loop {
            if is_lock_free(lock_file) {
                return true;
            }
            if Instant::now() >= lock_deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    const STREAMING_DESCENDANT_SCRIPT: &str = "n=0\n\
         while [ \"$n\" -lt 8 ]; do\n\
           printf 'stream-chunk\\n'\n\
           n=$((n + 1))\n\
         done\n\
         printf ok > desc.ready\n\
         exec /usr/bin/yes stream-chunk";

    #[cfg(target_os = "linux")]
    fn run_capture_on_exited_locking_child_with<O, M>(
        label: &str,
        body_before_desc: &str,
        descendant_cmd: &str,
        body_after_ready: &str,
        max_output_bytes: usize,
        cancel: Option<&AtomicBool>,
        make_observer: M,
    ) -> (Result<ProviderOutput, CaptureError>, bool, O)
    where
        O: DrainObserver,
        M: FnOnce(&std::path::Path) -> O,
    {
        let seq = DRAIN_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "fastlink_capture_drain_{label}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let lock_file = dir.join("desc.lock");

        let script = format!(
            "{body_before_desc}\n\
             /usr/bin/flock desc.lock /bin/sh -c '{descendant_cmd}' &\n\
             attempt=0\n\
             while [ ! -f desc.ready ]; do\n\
               attempt=$((attempt + 1))\n\
               [ \"$attempt\" -lt 200 ] || exit 7\n\
               sleep 0.01\n\
             done\n\
             {body_after_ready}\n"
        );
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn exited test child");

        let exit_deadline = Instant::now() + Duration::from_secs(2);
        while !child_has_exited(&child).expect("poll child exit") {
            assert!(
                Instant::now() < exit_deadline,
                "timed out waiting for test child exit"
            );
            thread::sleep(Duration::from_millis(5));
        }

        let pgid = child.id();
        let mut observer = make_observer(&lock_file);
        let result = capture_child_with_observer(
            child,
            Duration::ZERO,
            max_output_bytes,
            cancel,
            &mut observer,
        );

        let lock_released = wait_for_lock_release(&lock_file, Duration::from_millis(150));
        if !lock_released {
            kill_process_group(pgid);
        }
        let _ = std::fs::remove_dir_all(&dir);
        (result, lock_released, observer)
    }

    #[cfg(target_os = "linux")]
    fn run_capture_on_exited_locking_child(
        label: &str,
        body: &str,
        max_output_bytes: usize,
    ) -> (Result<ProviderOutput, CaptureError>, bool) {
        let (result, lock_released, _) = run_capture_on_exited_locking_child_with(
            label,
            "",
            "printf ok > desc.ready; sleep 5",
            body,
            max_output_bytes,
            None,
            |_| NoopDrainObserver,
        );
        (result, lock_released)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_child_post_exit_drain_kills_descendant_on_output_limit() {
        let (result, lock_released) =
            run_capture_on_exited_locking_child("cap", "printf '0123456789abcdef0'\nexit 0", 16);
        assert!(matches!(result, Err(CaptureError::OutputLimitExceeded)));
        assert!(
            lock_released,
            "descendant still held flock after post-exit drain output limit"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_child_post_exit_drain_preserves_exit_code_and_output_while_killing_descendant() {
        let (result, lock_released) = run_capture_on_exited_locking_child(
            "code",
            "printf 'buffered-payload\\n'\nexit 42",
            1024,
        );
        let output = result.expect("drain exited child output");
        assert_eq!(output.status.code(), Some(42));
        assert_eq!(output.stdout, b"buffered-payload\n");
        assert!(
            lock_released,
            "descendant still held flock after normal post-exit drain"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_child_post_exit_drain_deadline_preserves_exit_code_and_output_while_killing_streaming_descendant(
    ) {
        struct DeadlineObserver {
            lock_file: PathBuf,
            read_count: usize,
            child_exited_at_drain: bool,
            lock_held_during_drain: bool,
            stop_reason: Option<PumpStop>,
            ended_after_deadline: bool,
        }

        impl DrainObserver for DeadlineObserver {
            fn on_post_exit_read(&mut self, child: &Child, _drain_deadline: Instant) {
                self.read_count += 1;
                if self.read_count == 1 {
                    self.child_exited_at_drain = child_has_exited(child).unwrap_or(false);
                    self.lock_held_during_drain = !is_lock_free(&self.lock_file);
                }
                // Pace 8 KiB reads so the streaming descendant keeps the pipe non-empty across
                // the full 50ms POST_EXIT_DRAIN without overflowing the 4 MiB capture cap.
                thread::sleep(Duration::from_millis(2));
            }

            fn after_post_exit_drain(
                &mut self,
                _child: &Child,
                drain_deadline: Instant,
                drain_result: &Result<(), CaptureError>,
                stdout: &PipeReader,
            ) {
                assert!(drain_result.is_ok(), "unexpected drain error");
                self.stop_reason = stdout.last_stop;
                self.ended_after_deadline = Instant::now() >= drain_deadline;
            }
        }

        let cap = 4 * 1024 * 1024;
        let (result, lock_released, observer) = run_capture_on_exited_locking_child_with(
            "deadline",
            "printf 'buffered-banner\\n'",
            STREAMING_DESCENDANT_SCRIPT,
            "sleep 0.005\nexit 37",
            cap,
            None,
            |lock_file| DeadlineObserver {
                lock_file: lock_file.to_path_buf(),
                read_count: 0,
                child_exited_at_drain: false,
                lock_held_during_drain: false,
                stop_reason: None,
                ended_after_deadline: false,
            },
        );

        let output = result.expect("drain deadline path should preserve completed child output");
        assert!(observer.read_count > 0, "expected streaming reads in drain");
        assert!(
            observer.child_exited_at_drain,
            "direct child must have exited before post-exit drain"
        );
        assert!(
            observer.lock_held_during_drain,
            "streaming descendant must hold lock during post-exit drain"
        );
        assert_eq!(
            observer.stop_reason,
            Some(PumpStop::Deadline),
            "post-exit drain must hit deadline, not WouldBlock/Over/Cancelled/Eof"
        );
        assert!(
            observer.ended_after_deadline,
            "post-exit drain must finish at or after drain deadline"
        );
        assert_eq!(output.status.code(), Some(37));
        assert!(output.stdout.starts_with(b"buffered-banner\n"));
        assert!(
            output
                .stdout
                .windows(b"stream-chunk\n".len())
                .any(|w| w == b"stream-chunk\n"),
            "expected drained descendant chunks in retained stdout"
        );
        assert!(output.stdout.len() < cap);
        assert!(
            lock_released,
            "streaming descendant still held flock after drain deadline"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_child_cancellation_during_post_exit_drain_kills_streaming_descendant() {
        struct MidDrainCancelObserver<'a> {
            cancel: &'a AtomicBool,
            lock_file: PathBuf,
            read_count: usize,
            raised_in_active_drain: bool,
            stop_reason: Option<PumpStop>,
        }

        impl DrainObserver for MidDrainCancelObserver<'_> {
            fn on_post_exit_read(&mut self, child: &Child, drain_deadline: Instant) {
                self.read_count += 1;
                if self.read_count == 1 {
                    let exited = child_has_exited(child).unwrap_or(false);
                    let drain_active = Instant::now() < drain_deadline;
                    let lock_held = !is_lock_free(&self.lock_file);
                    assert!(
                        exited,
                        "direct child must already have exited before cancel"
                    );
                    assert!(
                        drain_active,
                        "50ms drain must be active when cancel is raised"
                    );
                    assert!(lock_held, "descendant must still hold lock during drain");
                    self.cancel.store(true, Ordering::Release);
                    self.raised_in_active_drain = true;
                }
            }

            fn after_post_exit_drain(
                &mut self,
                _child: &Child,
                _drain_deadline: Instant,
                _drain_result: &Result<(), CaptureError>,
                stdout: &PipeReader,
            ) {
                self.stop_reason = stdout.last_stop;
            }
        }

        let cancel = AtomicBool::new(false);
        let (result, lock_released, observer) = run_capture_on_exited_locking_child_with(
            "mid-drain-cancel",
            "printf 'buffered-banner\\n'",
            STREAMING_DESCENDANT_SCRIPT,
            "sleep 0.005\nexit 0",
            4 * 1024 * 1024,
            Some(&cancel),
            |lock_file| MidDrainCancelObserver {
                cancel: &cancel,
                lock_file: lock_file.to_path_buf(),
                read_count: 0,
                raised_in_active_drain: false,
                stop_reason: None,
            },
        );

        assert!(
            observer.raised_in_active_drain,
            "cancellation must be raised inside active post-exit drain"
        );
        assert_eq!(observer.stop_reason, Some(PumpStop::Cancelled));
        assert!(
            matches!(result, Err(CaptureError::Cancelled)),
            "expected Cancelled during post-exit drain"
        );
        assert!(
            lock_released,
            "descendant still held flock after post-exit drain cancellation"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_child_post_exit_drain_boundary_cancellation_returns_cancelled_and_kills_descendant()
    {
        struct BoundaryCancelObserver<'a> {
            cancel: &'a AtomicBool,
            lock_file: PathBuf,
            on_cleanup_instead: bool,
            raised: bool,
        }

        impl DrainObserver for BoundaryCancelObserver<'_> {
            fn after_post_exit_drain(
                &mut self,
                child: &Child,
                drain_deadline: Instant,
                drain_result: &Result<(), CaptureError>,
                stdout: &PipeReader,
            ) {
                if self.on_cleanup_instead {
                    return;
                }
                assert!(drain_result.is_ok());
                assert_eq!(stdout.last_stop, Some(PumpStop::WouldBlock));
                assert!(
                    child_has_exited(child).unwrap_or(false),
                    "direct child must already have exited before boundary cancellation"
                );
                assert!(
                    Instant::now() < drain_deadline,
                    "50ms drain window must still be active when boundary cancel is raised"
                );
                assert!(
                    !is_lock_free(&self.lock_file),
                    "descendant must still hold lock when boundary cancel is raised"
                );
                self.cancel.store(true, Ordering::Release);
                self.raised = true;
            }

            fn after_post_exit_cleanup(&mut self, drain_deadline: Instant) {
                if !self.on_cleanup_instead {
                    return;
                }
                assert!(
                    Instant::now() < drain_deadline,
                    "50ms drain window must still be active before returning output"
                );
                self.cancel.store(true, Ordering::Release);
                self.raised = true;
            }
        }

        for (label, on_cleanup_instead) in [
            ("boundary-after-pump", false),
            ("boundary-after-cleanup", true),
        ] {
            let cancel = AtomicBool::new(false);
            let (result, lock_released, observer) = run_capture_on_exited_locking_child_with(
                label,
                "printf 'buffered-payload\\n'",
                "printf ok > desc.ready; sleep 5",
                "exit 0",
                1024,
                Some(&cancel),
                |lock_file| BoundaryCancelObserver {
                    cancel: &cancel,
                    lock_file: lock_file.to_path_buf(),
                    on_cleanup_instead,
                    raised: false,
                },
            );

            assert!(observer.raised, "failed to raise cancel at {label}");
            assert!(
                matches!(result, Err(CaptureError::Cancelled)),
                "expected Cancelled at {label}, got {}",
                match result {
                    Ok(out) => format!(
                        "Ok(status={:?}, len={})",
                        out.status.code(),
                        out.stdout.len()
                    ),
                    Err(err) => format!("Err({err:?})"),
                }
            );
            assert!(
                lock_released,
                "descendant still held flock after {label} cancellation"
            );
        }
    }
}
