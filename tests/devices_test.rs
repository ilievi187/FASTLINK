use fastlink::devices::{
    collect_local_inventory, collect_local_inventory_with_paths, parse_devices_args,
    render_inventory_json, render_inventory_text, DevicesArgs, LinuxProbePaths, DEVICES_SCHEMA,
    DEVICES_SCHEMA_VERSION,
};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root(label: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "fastlink_devices_test_{label}_{}_{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp root");
    dir
}

fn write_file(base: &std::path::Path, rel: &str, contents: &str) {
    let path = base.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, contents).expect("write test file");
}

fn tokens(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn devices_args_parse_valid_and_reject_malformed() {
    assert_eq!(
        parse_devices_args(&tokens(&[])).expect("empty args"),
        DevicesArgs { json: false }
    );
    assert_eq!(
        parse_devices_args(&tokens(&["--json"])).expect("--json arg"),
        DevicesArgs { json: true }
    );
    assert!(parse_devices_args(&tokens(&["--bogus"])).is_err());
    assert!(parse_devices_args(&tokens(&["extra"])).is_err());
    assert!(parse_devices_args(&tokens(&["--json", "--json"])).is_err());
    assert!(parse_devices_args(&tokens(&["--json", "extra"])).is_err());
}

#[test]
fn populated_linux_host_evidence_is_reported_accurately() {
    let root = temp_root("populated");
    write_file(
        &root,
        "etc/machine-id",
        "0123456789abcdef0123456789abcdef\n",
    );
    write_file(&root, "proc/sys/kernel/hostname", "workstation-01\n");
    write_file(&root, "proc/sys/kernel/osrelease", "6.12.0-test\n");
    write_file(
        &root,
        "proc/meminfo",
        "MemTotal:       16384000 kB\nMemFree:         2048000 kB\nMemAvailable:    8192000 kB\n",
    );
    write_file(
        &root,
        "usr/share/hwdata/pci.ids",
        "14e4  Broadcom Inc.\n\t164e  Wrong Vendor Device\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t744c  Navi 31 [Radeon RX 7900 XT]\n",
    );
    write_file(
        &root,
        "sys/class/drm/card0/device/uevent",
        "DRIVER=amdgpu\n",
    );
    write_file(&root, "sys/class/drm/card0/device/vendor", "0x1002\n");
    write_file(&root, "sys/class/drm/card0/device/device", "0x164e\n");
    write_file(
        &root,
        "sys/class/drm/card0/device/product_name",
        "Integrated Graphics\n",
    );
    write_file(
        &root,
        "sys/class/drm/card0/device/mem_info_vram_total",
        "536870912\n",
    );
    write_file(
        &root,
        "sys/class/drm/card0/device/mem_info_vram_used",
        "36870912\n",
    );

    write_file(
        &root,
        "sys/class/drm/card1/device/uevent",
        "DRIVER=amdgpu\n",
    );
    write_file(&root, "sys/class/drm/card1/device/vendor", "0x1002\n");
    write_file(&root, "sys/class/drm/card1/device/device", "0x744c\n");
    write_file(
        &root,
        "sys/class/drm/card1/device/mem_info_vram_total",
        "21458059264\n",
    );
    write_file(
        &root,
        "sys/class/drm/card1/device/mem_info_vram_used",
        "1000000000\n",
    );

    // Connector and render nodes must not be treated as extra GPUs.
    write_file(&root, "sys/class/drm/card0-DP-1/status", "connected\n");
    write_file(&root, "sys/class/drm/renderD128/dev", "226:128\n");

    let paths = LinuxProbePaths::under_root(&root);
    let inv = collect_local_inventory_with_paths(&paths, Some(16), "linux", "x86_64");

    assert_eq!(inv.schema, DEVICES_SCHEMA);
    assert_eq!(inv.schema_version, DEVICES_SCHEMA_VERSION);
    assert_eq!(inv.nodes.len(), 1);
    let node = &inv.nodes[0];
    assert_eq!(node.node_id_source.as_deref(), Some("machine-id"));
    let node_id = node.node_id.as_deref().expect("node_id present");
    assert_eq!(node_id.len(), 32);
    assert_ne!(node_id, "0123456789abcdef0123456789abcdef");
    assert_eq!(node.hostname.as_deref(), Some("workstation-01"));
    assert_eq!(node.os, "linux");
    assert_eq!(node.os_release.as_deref(), Some("6.12.0-test"));
    assert_eq!(node.arch, "x86_64");
    assert_eq!(node.logical_cpu_count, Some(16));
    assert_eq!(node.ram_total_bytes, Some(16_384_000 * 1024));
    assert_eq!(node.ram_free_bytes, Some(2_048_000 * 1024));
    assert_eq!(node.ram_available_bytes, Some(8_192_000 * 1024));

    assert_eq!(node.gpus.len(), 2);
    assert_eq!(node.gpus[0].card, "card0");
    assert_eq!(node.gpus[0].name.as_deref(), Some("Integrated Graphics"));
    assert_eq!(node.gpus[0].vram_total_bytes, Some(536_870_912));
    assert_eq!(node.gpus[0].vram_free_bytes, Some(500_000_000));

    assert_eq!(node.gpus[1].card, "card1");
    assert_eq!(
        node.gpus[1].name.as_deref(),
        Some("Navi 31 [Radeon RX 7900 XT]")
    );
    assert_eq!(node.gpus[1].vram_total_bytes, Some(21_458_059_264));
    assert_eq!(node.gpus[1].vram_free_bytes, Some(20_458_059_264));

    assert_eq!(node.capabilities.capacity_bytes, None);
    assert_eq!(node.capabilities.throughput_bps, None);
    assert_eq!(node.capabilities.network_connectivity, "unknown");
    assert!(node.capabilities.remote_peers.is_empty());

    let json = render_inventory_json(&inv);
    assert!(json.contains("\"schema\":\"fastlink.devices.v1\""));
    assert!(json.contains("\"node_count\":1"));
    assert!(json.contains("\"hostname\":\"workstation-01\""));
    assert!(json.contains("\"name\":\"Navi 31 [Radeon RX 7900 XT]\""));
    assert!(json.contains("\"capacity_bytes\":null"));
    assert!(json.contains("\"throughput_bps\":null"));
    assert!(json.contains("\"remote_peers\":[]"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn missing_and_malformed_evidence_stays_null_and_explicitly_unknown() {
    let root = temp_root("missing");
    let paths = LinuxProbePaths::under_root(&root);
    let inv = collect_local_inventory_with_paths(&paths, None, "linux", "aarch64");

    assert_eq!(inv.nodes.len(), 1);
    let node = &inv.nodes[0];
    assert_eq!(node.node_id, None);
    assert_eq!(node.node_id_source, None);
    assert_eq!(node.hostname, None);
    assert_eq!(node.os_release, None);
    assert_eq!(node.logical_cpu_count, None);
    assert_eq!(node.ram_total_bytes, None);
    assert_eq!(node.ram_available_bytes, None);
    assert_eq!(node.ram_free_bytes, None);
    assert!(node.gpus.is_empty());

    for required_unknown in [
        "node_id",
        "hostname",
        "os_release",
        "logical_cpu_count",
        "ram_total_bytes",
        "ram_available_bytes",
        "ram_free_bytes",
        "gpus",
        "capabilities.capacity_bytes",
        "capabilities.throughput_bps",
        "capabilities.network_connectivity",
    ] {
        assert!(
            node.unknown_fields.iter().any(|f| f == required_unknown),
            "expected '{required_unknown}' in unknown_fields: {:?}",
            node.unknown_fields
        );
    }

    let json = render_inventory_json(&inv);
    assert!(json.contains("\"node_id\":null"));
    assert!(json.contains("\"hostname\":null"));
    assert!(json.contains("\"logical_cpu_count\":null"));
    assert!(json.contains("\"ram_total_bytes\":null"));
    assert!(json.contains("\"gpus\":[]"));

    let text = render_inventory_text(&inv);
    assert!(text.contains("node_id: unknown"));
    assert!(text.contains("hostname: unknown"));
    assert!(text.contains("gpus: 0"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn corrupt_meminfo_and_underflowing_vram_do_not_panic_or_fabricate() {
    let root = temp_root("malformed");
    write_file(&root, "proc/sys/kernel/hostname", "host-a\n");
    write_file(
        &root,
        "proc/meminfo",
        "MemTotal:       not-a-number kB\nMemFree:        1024 MiB\n",
    );
    write_file(
        &root,
        "usr/share/hwdata/pci.ids",
        "14e4  Broadcom\n\t164e  Wrong Vendor\n",
    );
    write_file(
        &root,
        "sys/class/drm/card0/device/uevent",
        "PCI_ID=1002:164E\n",
    );
    write_file(
        &root,
        "sys/class/drm/card0/device/mem_info_vram_total",
        "1000\n",
    );
    write_file(
        &root,
        "sys/class/drm/card0/device/mem_info_vram_used",
        "2000\n",
    );

    let paths = LinuxProbePaths::under_root(&root);
    let inv = collect_local_inventory_with_paths(&paths, Some(4), "linux", "x86_64");
    let node = &inv.nodes[0];

    assert_eq!(node.node_id_source.as_deref(), Some("hostname-derived"));
    assert_eq!(node.ram_total_bytes, None);
    assert_eq!(node.ram_free_bytes, None);
    assert_eq!(node.gpus.len(), 1);
    assert_eq!(node.gpus[0].name, None);
    assert_eq!(node.gpus[0].vendor_id.as_deref(), Some("0x1002"));
    assert_eq!(node.gpus[0].pci_device_id.as_deref(), Some("0x164e"));
    assert_eq!(node.gpus[0].vram_total_bytes, Some(1000));
    assert_eq!(node.gpus[0].vram_free_bytes, None);
    assert!(node.unknown_fields.iter().any(|f| f == "gpus[0].name"));
    assert!(node
        .unknown_fields
        .iter()
        .any(|f| f == "gpus[0].vram_free_bytes"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn live_host_inventory_and_cli_binary_report_single_local_node() {
    let inv = collect_local_inventory();
    assert_eq!(inv.nodes.len(), 1);
    assert_eq!(inv.nodes[0].os, std::env::consts::OS);
    assert_eq!(inv.nodes[0].arch, std::env::consts::ARCH);
    assert!(inv.nodes[0].capabilities.remote_peers.is_empty());

    let bin = env!("CARGO_BIN_EXE_fastlink");

    let json_out = Command::new(bin)
        .args(["devices", "--json"])
        .output()
        .expect("run fastlink devices --json");
    assert!(json_out.status.success());
    let stdout = String::from_utf8(json_out.stdout).expect("utf8 stdout");
    assert!(stdout.contains("\"schema\":\"fastlink.devices.v1\""));
    assert!(stdout.contains("\"schema_version\":1"));
    assert!(stdout.contains("\"node_count\":1"));
    assert!(stdout.contains("\"remote_peers\":[]"));

    let text_out = Command::new(bin)
        .args(["devices"])
        .output()
        .expect("run fastlink devices");
    assert!(text_out.status.success());
    let text_stdout = String::from_utf8(text_out.stdout).expect("utf8 stdout");
    assert!(text_stdout.contains("node_count: 1"));

    let bad_out = Command::new(bin)
        .args(["devices", "--bad-flag"])
        .output()
        .expect("run fastlink devices --bad-flag");
    assert!(!bad_out.status.success());
    let bad_stderr = String::from_utf8(bad_out.stderr).expect("utf8 stderr");
    assert!(bad_stderr.contains("FASTLINK error:"));
}
