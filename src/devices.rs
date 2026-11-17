//! Local-only Linux device inventory (`fastlink devices [--json]`).
//!
//! The live command is Linux-only: `/proc` and `/sys` are required, and other
//! platforms return `io::ErrorKind::Unsupported` instead of a mostly-unknown
//! successful inventory. Fixture-based parsing remains platform-independent.
//!
//! Collects passive host evidence for the local node only:
//! - stable local node ID (hashed from `/etc/machine-id` or hostname fallback)
//! - hostname, OS, kernel release, architecture
//! - logical CPU count (`std::thread::available_parallelism`)
//! - RAM total/available/free bytes (`/proc/meminfo`)
//! - GPU metadata and VRAM only when directly exposed via passive sysfs (`/sys/class/drm`)
//!
//! Never scans LAN/Tailscale, binds ports, starts services, inspects private
//! network addresses, downloads models, or initializes/uses GPU compute.
//! Unknown fields are emitted as `null`/empty and listed explicitly in `unknown_fields`.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

pub const DEVICES_SCHEMA: &str = "fastlink.devices.v1";
pub const DEVICES_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevicesArgs {
    pub json: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuDevice {
    pub card: String,
    pub name: Option<String>,
    pub driver: Option<String>,
    pub vendor_id: Option<String>,
    pub pci_device_id: Option<String>,
    pub vram_total_bytes: Option<u64>,
    pub vram_free_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCapabilities {
    pub capacity_bytes: Option<u64>,
    pub throughput_bps: Option<u64>,
    pub network_connectivity: &'static str,
    pub remote_peers: Vec<String>,
}

impl Default for NodeCapabilities {
    fn default() -> Self {
        Self {
            capacity_bytes: None,
            throughput_bps: None,
            network_connectivity: "unknown",
            remote_peers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalNode {
    pub node_id: Option<String>,
    pub node_id_source: Option<String>,
    pub hostname: Option<String>,
    pub os: String,
    pub os_release: Option<String>,
    pub arch: String,
    pub logical_cpu_count: Option<usize>,
    pub ram_total_bytes: Option<u64>,
    pub ram_available_bytes: Option<u64>,
    pub ram_free_bytes: Option<u64>,
    pub gpus: Vec<GpuDevice>,
    pub capabilities: NodeCapabilities,
    pub unknown_fields: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInventory {
    pub schema: &'static str,
    pub schema_version: u32,
    pub scope: &'static str,
    pub discovery_mode: &'static str,
    pub nodes: Vec<LocalNode>,
}

#[derive(Clone, Debug)]
pub struct LinuxProbePaths {
    pub machine_id_paths: Vec<PathBuf>,
    pub hostname_paths: Vec<PathBuf>,
    pub osrelease_path: PathBuf,
    pub meminfo_path: PathBuf,
    pub drm_class_dir: PathBuf,
    pub pci_ids_paths: Vec<PathBuf>,
}

impl LinuxProbePaths {
    pub fn system() -> Self {
        Self {
            machine_id_paths: vec![
                PathBuf::from("/etc/machine-id"),
                PathBuf::from("/var/lib/dbus/machine-id"),
            ],
            hostname_paths: vec![
                PathBuf::from("/proc/sys/kernel/hostname"),
                PathBuf::from("/etc/hostname"),
            ],
            osrelease_path: PathBuf::from("/proc/sys/kernel/osrelease"),
            meminfo_path: PathBuf::from("/proc/meminfo"),
            drm_class_dir: PathBuf::from("/sys/class/drm"),
            pci_ids_paths: vec![
                PathBuf::from("/usr/share/hwdata/pci.ids"),
                PathBuf::from("/usr/share/misc/pci.ids"),
                PathBuf::from("/usr/share/pci.ids"),
            ],
        }
    }

    pub fn under_root(root: &Path) -> Self {
        Self {
            machine_id_paths: vec![
                root.join("etc/machine-id"),
                root.join("var/lib/dbus/machine-id"),
            ],
            hostname_paths: vec![
                root.join("proc/sys/kernel/hostname"),
                root.join("etc/hostname"),
            ],
            osrelease_path: root.join("proc/sys/kernel/osrelease"),
            meminfo_path: root.join("proc/meminfo"),
            drm_class_dir: root.join("sys/class/drm"),
            pci_ids_paths: vec![
                root.join("usr/share/hwdata/pci.ids"),
                root.join("usr/share/misc/pci.ids"),
                root.join("usr/share/pci.ids"),
            ],
        }
    }
}

pub fn parse_devices_args(args: &[String]) -> Result<DevicesArgs, String> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => {
                if json {
                    return Err("duplicate '--json' flag".to_string());
                }
                json = true;
            }
            flag => return Err(format!("unknown devices argument '{flag}'")),
        }
    }
    Ok(DevicesArgs { json })
}

/// Collects live local inventory on Linux.
///
/// Returns `io::ErrorKind::Unsupported` on non-Linux platforms rather than
/// reporting a successful inventory of mostly-unknown values.
pub fn collect_local_inventory() -> io::Result<DeviceInventory> {
    collect_local_inventory_for_platform(std::env::consts::OS)
}

/// Platform-coherent inventory seam used by tests and the public command.
///
/// Only a real Linux runtime with `os == "linux"` may probe `/proc` and `/sys`.
pub fn collect_local_inventory_for_platform(os: &str) -> io::Result<DeviceInventory> {
    if os != "linux" || !cfg!(target_os = "linux") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "local device inventory requires Linux /proc and /sys",
        ));
    }
    let logical_cpus = std::thread::available_parallelism()
        .ok()
        .map(|count| count.get());
    Ok(collect_local_inventory_with_paths(
        &LinuxProbePaths::system(),
        logical_cpus,
        os,
        std::env::consts::ARCH,
    ))
}

pub fn collect_local_inventory_with_paths(
    paths: &LinuxProbePaths,
    logical_cpu_count: Option<usize>,
    os: &str,
    arch: &str,
) -> DeviceInventory {
    let hostname = read_first_nonempty(&paths.hostname_paths);
    let os_release = read_trimmed_file(&paths.osrelease_path);
    let (node_id, node_id_source) = derive_local_node_id(paths, hostname.as_deref(), os, arch);
    let meminfo = read_meminfo(&paths.meminfo_path);
    let (gpus, drm_accessible) = discover_drm_gpus(&paths.drm_class_dir, &paths.pci_ids_paths);

    let mut unknown_fields = Vec::new();
    if node_id.is_none() {
        unknown_fields.push("node_id".to_string());
    }
    if hostname.is_none() {
        unknown_fields.push("hostname".to_string());
    }
    if os_release.is_none() {
        unknown_fields.push("os_release".to_string());
    }
    if logical_cpu_count.is_none() {
        unknown_fields.push("logical_cpu_count".to_string());
    }
    if meminfo.total_bytes.is_none() {
        unknown_fields.push("ram_total_bytes".to_string());
    }
    if meminfo.available_bytes.is_none() {
        unknown_fields.push("ram_available_bytes".to_string());
    }
    if meminfo.free_bytes.is_none() {
        unknown_fields.push("ram_free_bytes".to_string());
    }
    if !drm_accessible {
        unknown_fields.push("gpus".to_string());
    } else {
        for (idx, gpu) in gpus.iter().enumerate() {
            if gpu.name.is_none() {
                unknown_fields.push(format!("gpus[{idx}].name"));
            }
            if gpu.driver.is_none() {
                unknown_fields.push(format!("gpus[{idx}].driver"));
            }
            if gpu.vendor_id.is_none() {
                unknown_fields.push(format!("gpus[{idx}].vendor_id"));
            }
            if gpu.pci_device_id.is_none() {
                unknown_fields.push(format!("gpus[{idx}].pci_device_id"));
            }
            if gpu.vram_total_bytes.is_none() {
                unknown_fields.push(format!("gpus[{idx}].vram_total_bytes"));
            }
            if gpu.vram_free_bytes.is_none() {
                unknown_fields.push(format!("gpus[{idx}].vram_free_bytes"));
            }
        }
    }
    unknown_fields.push("capabilities.capacity_bytes".to_string());
    unknown_fields.push("capabilities.throughput_bps".to_string());
    unknown_fields.push("capabilities.network_connectivity".to_string());

    let node = LocalNode {
        node_id,
        node_id_source,
        hostname,
        os: os.to_string(),
        os_release,
        arch: arch.to_string(),
        logical_cpu_count,
        ram_total_bytes: meminfo.total_bytes,
        ram_available_bytes: meminfo.available_bytes,
        ram_free_bytes: meminfo.free_bytes,
        gpus,
        capabilities: NodeCapabilities::default(),
        unknown_fields,
    };

    DeviceInventory {
        schema: DEVICES_SCHEMA,
        schema_version: DEVICES_SCHEMA_VERSION,
        scope: "local_only",
        discovery_mode: "passive_host_evidence",
        nodes: vec![node],
    }
}

fn read_trimmed_file(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn read_first_nonempty(paths: &[PathBuf]) -> Option<String> {
    paths.iter().find_map(|path| read_trimmed_file(path))
}

fn derive_local_node_id(
    paths: &LinuxProbePaths,
    hostname: Option<&str>,
    os: &str,
    arch: &str,
) -> (Option<String>, Option<String>) {
    for (idx, path) in paths.machine_id_paths.iter().enumerate() {
        if let Some(machine_id) = read_trimmed_file(path) {
            let source = if idx == 0 {
                "machine-id"
            } else {
                "dbus-machine-id"
            };
            let input = format!("fastlink-local-node-v1:{source}:{machine_id}");
            let hash = blake3::hash(input.as_bytes()).to_hex().to_string();
            return (Some(hash[..32].to_string()), Some(source.to_string()));
        }
    }
    if let Some(host) = hostname {
        let input = format!("fastlink-local-node-v1:host:{host}:{os}:{arch}");
        let hash = blake3::hash(input.as_bytes()).to_hex().to_string();
        return (
            Some(hash[..32].to_string()),
            Some("hostname-derived".to_string()),
        );
    }
    (None, None)
}

#[derive(Default)]
struct MemInfo {
    total_bytes: Option<u64>,
    available_bytes: Option<u64>,
    free_bytes: Option<u64>,
}

fn read_meminfo(path: &Path) -> MemInfo {
    let Ok(content) = fs::read_to_string(path) else {
        return MemInfo::default();
    };
    let mut info = MemInfo::default();
    for line in content.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let bytes = parse_meminfo_bytes(rest);
        match key.trim() {
            "MemTotal" => info.total_bytes = bytes,
            "MemAvailable" => info.available_bytes = bytes,
            "MemFree" => info.free_bytes = bytes,
            _ => {}
        }
    }
    info
}

fn parse_meminfo_bytes(value_part: &str) -> Option<u64> {
    let mut tokens = value_part.split_whitespace();
    let num_str = tokens.next()?;
    let value = num_str.parse::<u64>().ok()?;
    match tokens.next() {
        Some(unit) if unit.eq_ignore_ascii_case("kb") => value.checked_mul(1024),
        None => Some(value),
        Some(_) => None,
    }
}

fn is_drm_card_name(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn card_sort_key(name: &str) -> (u64, &str) {
    let num = name
        .strip_prefix("card")
        .and_then(|rest| rest.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    (num, name)
}

fn discover_drm_gpus(drm_dir: &Path, pci_ids_paths: &[PathBuf]) -> (Vec<GpuDevice>, bool) {
    let Ok(entries) = fs::read_dir(drm_dir) else {
        return (Vec::new(), false);
    };

    let mut card_names = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if is_drm_card_name(name) {
            card_names.push(name.to_string());
        }
    }
    card_names.sort_by(|a, b| card_sort_key(a).cmp(&card_sort_key(b)));

    let mut gpus = Vec::new();
    for card in card_names {
        let device_dir = drm_dir.join(&card).join("device");
        if !device_dir.exists() {
            continue;
        }
        let uevent = read_trimmed_file(&device_dir.join("uevent"));
        let driver = uevent
            .as_deref()
            .and_then(|text| parse_uevent_field(text, "DRIVER"));
        let (uevent_vendor, uevent_device) = uevent
            .as_deref()
            .and_then(|text| parse_uevent_field(text, "PCI_ID"))
            .and_then(|id| parse_pci_id_pair(&id))
            .map_or((None, None), |(v, d)| (Some(v), Some(d)));

        let vendor_id = read_trimmed_file(&device_dir.join("vendor"))
            .and_then(|v| normalize_hex_id(&v))
            .or(uevent_vendor);
        let pci_device_id = read_trimmed_file(&device_dir.join("device"))
            .and_then(|d| normalize_hex_id(&d))
            .or(uevent_device);

        let name = read_trimmed_file(&device_dir.join("product_name")).or_else(|| {
            match (vendor_id.as_deref(), pci_device_id.as_deref()) {
                (Some(v), Some(d)) => lookup_pci_device_name(pci_ids_paths, v, d),
                _ => None,
            }
        });

        let vram_total_bytes = read_trimmed_file(&device_dir.join("mem_info_vram_total"))
            .and_then(|s| s.parse::<u64>().ok());
        let vram_used_bytes = read_trimmed_file(&device_dir.join("mem_info_vram_used"))
            .and_then(|s| s.parse::<u64>().ok());
        let vram_free_bytes = match (vram_total_bytes, vram_used_bytes) {
            (Some(total), Some(used)) => total.checked_sub(used),
            _ => None,
        };

        gpus.push(GpuDevice {
            card,
            name,
            driver,
            vendor_id,
            pci_device_id,
            vram_total_bytes,
            vram_free_bytes,
        });
    }

    (gpus, true)
}

fn parse_uevent_field(uevent: &str, key: &str) -> Option<String> {
    for line in uevent.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn parse_pci_id_pair(value: &str) -> Option<(String, String)> {
    let (v, d) = value.split_once(':')?;
    Some((normalize_hex_id(v)?, normalize_hex_id(d)?))
}

fn normalize_hex_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if hex.is_empty() || hex.len() > 8 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("0x{}", hex.to_ascii_lowercase()))
}

fn lookup_pci_device_name(
    pci_ids_paths: &[PathBuf],
    vendor_id: &str,
    device_id: &str,
) -> Option<String> {
    let vendor_hex = vendor_id.strip_prefix("0x").unwrap_or(vendor_id);
    let device_hex = device_id.strip_prefix("0x").unwrap_or(device_id);

    for path in pci_ids_paths {
        let Ok(file) = File::open(path) else {
            continue;
        };
        let reader = BufReader::new(file);
        let mut in_vendor = false;
        for line_result in reader.lines() {
            let Ok(line) = line_result else {
                break;
            };
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !line.starts_with('\t') {
                if in_vendor {
                    break;
                }
                if let Some((v_code, _)) = line.split_once("  ") {
                    if v_code.trim().eq_ignore_ascii_case(vendor_hex) {
                        in_vendor = true;
                    }
                }
            } else if in_vendor && !line.starts_with("\t\t") {
                let stripped = line.trim_start_matches('\t');
                if let Some((d_code, d_name)) = stripped.split_once("  ") {
                    if d_code.trim().eq_ignore_ascii_case(device_hex) {
                        let name = d_name.trim();
                        if !name.is_empty() {
                            return Some(name.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

pub fn render_inventory_json(inventory: &DeviceInventory) -> String {
    let mut out = String::from("{");
    let _ = write!(
        out,
        "\"schema\":\"{}\",\"schema_version\":{},\"scope\":\"{}\",\"discovery_mode\":\"{}\",\"node_count\":{},\"nodes\":[",
        json_escape(inventory.schema),
        inventory.schema_version,
        json_escape(inventory.scope),
        json_escape(inventory.discovery_mode),
        inventory.nodes.len()
    );
    for (idx, node) in inventory.nodes.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push('{');
        out.push_str("\"node_id\":");
        push_opt_str(&mut out, node.node_id.as_deref());
        out.push_str(",\"node_id_source\":");
        push_opt_str(&mut out, node.node_id_source.as_deref());
        out.push_str(",\"hostname\":");
        push_opt_str(&mut out, node.hostname.as_deref());
        let _ = write!(out, ",\"os\":\"{}\"", json_escape(&node.os));
        out.push_str(",\"os_release\":");
        push_opt_str(&mut out, node.os_release.as_deref());
        let _ = write!(out, ",\"arch\":\"{}\"", json_escape(&node.arch));
        out.push_str(",\"logical_cpu_count\":");
        push_opt_u64(&mut out, node.logical_cpu_count.map(|n| n as u64));
        out.push_str(",\"ram_total_bytes\":");
        push_opt_u64(&mut out, node.ram_total_bytes);
        out.push_str(",\"ram_available_bytes\":");
        push_opt_u64(&mut out, node.ram_available_bytes);
        out.push_str(",\"ram_free_bytes\":");
        push_opt_u64(&mut out, node.ram_free_bytes);
        out.push_str(",\"gpus\":[");
        for (g_idx, gpu) in node.gpus.iter().enumerate() {
            if g_idx > 0 {
                out.push(',');
            }
            let _ = write!(out, "{{\"card\":\"{}\"", json_escape(&gpu.card));
            out.push_str(",\"name\":");
            push_opt_str(&mut out, gpu.name.as_deref());
            out.push_str(",\"driver\":");
            push_opt_str(&mut out, gpu.driver.as_deref());
            out.push_str(",\"vendor_id\":");
            push_opt_str(&mut out, gpu.vendor_id.as_deref());
            out.push_str(",\"pci_device_id\":");
            push_opt_str(&mut out, gpu.pci_device_id.as_deref());
            out.push_str(",\"vram_total_bytes\":");
            push_opt_u64(&mut out, gpu.vram_total_bytes);
            out.push_str(",\"vram_free_bytes\":");
            push_opt_u64(&mut out, gpu.vram_free_bytes);
            out.push('}');
        }
        out.push_str("],\"capabilities\":{\"capacity_bytes\":");
        push_opt_u64(&mut out, node.capabilities.capacity_bytes);
        out.push_str(",\"throughput_bps\":");
        push_opt_u64(&mut out, node.capabilities.throughput_bps);
        let _ = write!(
            out,
            ",\"network_connectivity\":\"{}\",\"remote_peers\":[",
            json_escape(node.capabilities.network_connectivity)
        );
        for (p_idx, peer) in node.capabilities.remote_peers.iter().enumerate() {
            if p_idx > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", json_escape(peer));
        }
        out.push_str("]},\"unknown_fields\":[");
        for (u_idx, field) in node.unknown_fields.iter().enumerate() {
            if u_idx > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", json_escape(field));
        }
        out.push_str("]}");
    }
    out.push_str("]}\n");
    out
}

pub fn render_inventory_text(inventory: &DeviceInventory) -> String {
    let mut out = String::from("FASTLINK local device inventory (local-only; no network scan)\n");
    let _ = writeln!(out, "schema: {}", inventory.schema);
    let _ = writeln!(out, "node_count: {}", inventory.nodes.len());
    for node in &inventory.nodes {
        let _ = writeln!(
            out,
            "node_id: {} (source: {})",
            node.node_id.as_deref().unwrap_or("unknown"),
            node.node_id_source.as_deref().unwrap_or("unknown")
        );
        let _ = writeln!(
            out,
            "hostname: {}",
            node.hostname.as_deref().unwrap_or("unknown")
        );
        let _ = writeln!(
            out,
            "os: {} ({}) arch: {}",
            node.os,
            node.os_release.as_deref().unwrap_or("unknown"),
            node.arch
        );
        let _ = writeln!(
            out,
            "logical_cpu_count: {}",
            fmt_opt_u64(node.logical_cpu_count.map(|n| n as u64))
        );
        let _ = writeln!(
            out,
            "ram_total_bytes: {}",
            fmt_opt_u64(node.ram_total_bytes)
        );
        let _ = writeln!(
            out,
            "ram_available_bytes: {}",
            fmt_opt_u64(node.ram_available_bytes)
        );
        let _ = writeln!(out, "ram_free_bytes: {}", fmt_opt_u64(node.ram_free_bytes));
        let _ = writeln!(out, "gpus: {}", node.gpus.len());
        for gpu in &node.gpus {
            let _ = writeln!(
                out,
                "  - {}: name={} driver={} vendor={} device={} vram_total_bytes={} vram_free_bytes={}",
                gpu.card,
                gpu.name.as_deref().unwrap_or("unknown"),
                gpu.driver.as_deref().unwrap_or("unknown"),
                gpu.vendor_id.as_deref().unwrap_or("unknown"),
                gpu.pci_device_id.as_deref().unwrap_or("unknown"),
                fmt_opt_u64(gpu.vram_total_bytes),
                fmt_opt_u64(gpu.vram_free_bytes),
            );
        }
        let _ = writeln!(
            out,
            "capabilities: capacity=unknown throughput=unknown network_connectivity={} remote_peers={}",
            node.capabilities.network_connectivity,
            node.capabilities.remote_peers.len()
        );
        if node.unknown_fields.is_empty() {
            let _ = writeln!(out, "unknown_fields: none");
        } else {
            let _ = writeln!(out, "unknown_fields: {}", node.unknown_fields.join(", "));
        }
    }
    out
}

pub fn run_devices(args: &[String]) -> io::Result<String> {
    let parsed = parse_devices_args(args)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
    let inventory = collect_local_inventory()?;
    if parsed.json {
        Ok(render_inventory_json(&inventory))
    } else {
        Ok(render_inventory_text(&inventory))
    }
}

fn push_opt_str(out: &mut String, value: Option<&str>) {
    match value {
        Some(v) => {
            let _ = write!(out, "\"{}\"", json_escape(v));
        }
        None => out.push_str("null"),
    }
}

fn push_opt_u64(out: &mut String, value: Option<u64>) {
    match value {
        Some(v) => {
            let _ = write!(out, "{v}");
        }
        None => out.push_str("null"),
    }
}

fn fmt_opt_u64(value: Option<u64>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "unknown".to_string(),
    }
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}
