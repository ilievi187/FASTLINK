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
            flag => return Err(format!("unknown discover argument '{flag}'")),
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

#[cfg(any(not(target_os = "linux"), test))]
fn unsupported_platform_report(cancel: Option<&AtomicBool>) -> DiscoveryReport {
    let cancelled = cancel.is_some_and(|flag| flag.load(Ordering::Acquire));
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

/// Bounded O(N) detector for DNS `host:port` endpoints (at least one dot,
/// valid DNS labels with an alphabetic TLD-style label, and port `1..=65535`).
fn contains_dns_host_port(text: &str) -> bool {
    const MAX_DNS_HOST_LEN: usize = 253;
    let bytes = text.as_bytes();

    for (colon_idx, &b) in bytes.iter().enumerate() {
        if b != b':' || colon_idx == 0 {
            continue;
        }

        // Parse bounded numeric port `1..=65535` right of `:` (allowing zero-padding).
        const MAX_PORT_RAW_DIGITS: usize = 16;
        let p_start = colon_idx + 1;
        let mut p_end = p_start;
        while p_end < bytes.len()
            && bytes[p_end].is_ascii_digit()
            && p_end - p_start <= MAX_PORT_RAW_DIGITS
        {
            p_end += 1;
        }
        let raw_port_len = p_end - p_start;
        if !(1..=MAX_PORT_RAW_DIGITS).contains(&raw_port_len) {
            continue;
        }
        if let Some(&next) = bytes.get(p_end) {
            if next.is_ascii_alphanumeric() || next == b'_' || next == b'-' || next == b':' {
                continue;
            }
            if next == b'.' && bytes.get(p_end + 1).is_some_and(u8::is_ascii_digit) {
                continue;
            }
        }
        let trimmed_port = text[p_start..p_end].trim_start_matches('0');
        if trimmed_port.is_empty() || trimmed_port.len() > 5 {
            continue;
        }
        let Ok(port) = trimmed_port.parse::<u16>() else {
            continue;
        };
        if port == 0 {
            continue;
        }

        // Scan bounded DNS hostname slice left of `:`.
        let min_start = colon_idx.saturating_sub(MAX_DNS_HOST_LEN + 1);
        let mut h_start = colon_idx;
        while h_start > min_start && is_dns_host_byte(bytes[h_start - 1]) {
            h_start -= 1;
        }
        let raw_host = &text[h_start..colon_idx];
        let host = raw_host.strip_suffix('.').unwrap_or(raw_host);
        if host.len() <= MAX_DNS_HOST_LEN && is_valid_dns_host_with_dot(host) {
            return true;
        }
    }
    false
}

fn is_dns_host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_'
}

fn is_valid_dns_host_with_dot(host: &str) -> bool {
    if !host.contains('.') {
        return false;
    }
    let mut last_label: Option<&str> = None;
    let mut count = 0usize;
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
        count += 1;
    }
    if count < 2 {
        return false;
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
    if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
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
fn stop_child(child: &mut Child) {
    let pgid = child.id();
    if pgid > 0 {
        // SAFETY: `process_group(0)` placed `child` and its descendants in group `pgid`,
        // so signaling `-pgid` kills the entire tree without affecting the caller.
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "linux")]
fn capture_child(
    mut child: Child,
    timeout: Duration,
    max_output_bytes: usize,
    cancel: Option<&AtomicBool>,
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
    let status = loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            stop_child(&mut child);
            return Err(CaptureError::Cancelled);
        }
        if stdout.pump(overall_deadline, cancel).is_err() {
            stop_child(&mut child);
            return Err(CaptureError::IoFailed);
        }
        if stdout.cancelled {
            stop_child(&mut child);
            return Err(CaptureError::Cancelled);
        }
        if stdout.is_over() {
            stop_child(&mut child);
            return Err(CaptureError::OutputLimitExceeded);
        }

        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
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
    };

    let drain_deadline = Instant::now() + POST_EXIT_DRAIN;
    if stdout.pump(drain_deadline, cancel).is_err() {
        return Err(CaptureError::IoFailed);
    }
    if stdout.cancelled {
        return Err(CaptureError::Cancelled);
    }
    if stdout.is_over() {
        return Err(CaptureError::OutputLimitExceeded);
    }

    Ok(ProviderOutput {
        status,
        stdout: stdout.retained,
    })
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
        })
    }

    fn pump(&mut self, deadline: Instant, cancel: Option<&AtomicBool>) -> io::Result<()> {
        if self.eof {
            return Ok(());
        }
        let mut buffer = [0u8; 8192];
        loop {
            if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                self.cancelled = true;
                return Ok(());
            }
            if Instant::now() >= deadline || self.is_over() {
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
                    return Ok(());
                }
            } else if read == 0 {
                self.eof = true;
                self.handle.take();
                return Ok(());
            } else {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock => return Ok(()),
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
            "ws1.local:18777",
            "node-1.lan:8080",
            "api.example.com:443.",
            "endpoint=my.host.internal:65535",
            "host.example:0443",
            "host.example:00443",
            "host.example:065535",
        ] {
            let mut bad_ep = valid.clone();
            bad_ep.nodes[0].display_name = dns_endpoint.to_string();
            let err = render_discovery_json(&bad_ep).unwrap_err();
            assert!(!err.to_string().contains(dns_endpoint));
            assert!(render_discovery_text(&bad_ep).is_err());
        }

        for ordinary in [
            "Workstation One",
            "gpu-box.tailnet-example.ts.net",
            "Radeon RX 7900 XT",
            "Model: 70B",
            "Qwen2.5:72b",
            "llama-3.1:8b",
            "PCI 00:02.0",
            "cuda:0",
            "step-1.2:10",
            "v1.0.2:4",
            "bad.host:0",
            "bad.host:00",
            "bad.host:000",
            "bad.host:65536",
            "bad.host:065536",
            "bad.host:999999",
            "bad.host:0443abc",
            "bad.host:0443.1",
            "bad.host:0443:1",
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
}
