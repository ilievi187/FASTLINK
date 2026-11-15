use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use fastlink::discovery::{discover_with_config, render_discovery_json, DiscoveryConfig};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

/// Fake executables are written and spawned inside one shared test process.
/// Acquire this lock in every process-spawning or script-writing test so a
/// concurrent `Command::spawn` `clone()` cannot inherit an open writable fd
/// and cause `ETXTBSY` before reaching `execve`.
static SCRIPT_LOCK: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SCRIPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn temp_dir(label: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "fastlink_discovery_test_{label}_{}_{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn cli_rejects_unknown_or_duplicate_discover_flags() {
    let _guard = serial();
    let bin = env!("CARGO_BIN_EXE_fastlink");

    let bad = Command::new(bin)
        .args(["discover", "--bad-flag"])
        .output()
        .expect("run fastlink discover --bad-flag");
    assert!(!bad.status.success());
    let stderr = String::from_utf8(bad.stderr).expect("utf8 stderr");
    assert!(stderr.contains("FASTLINK error:"), "stderr was: {stderr}");

    let dup = Command::new(bin)
        .args(["discover", "--json", "--json"])
        .output()
        .expect("run fastlink discover --json --json");
    assert!(!dup.status.success());
}

#[test]
fn cli_rejects_unknown_discover_argument_without_echoing_input() {
    let _guard = serial();
    let bin = env!("CARGO_BIN_EXE_fastlink");
    let empty_bin_dir = temp_dir("unknown-arg-empty-path");

    let out = Command::new(bin)
        .args(["discover", "192.0.2.50"])
        .env("PATH", &empty_bin_dir)
        .output()
        .expect("run fastlink discover 192.0.2.50");
    let _ = fs::remove_dir_all(&empty_bin_dir);

    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert!(stdout.is_empty(), "unexpected stdout: {stdout}");
    assert!(
        stderr.contains("FASTLINK error: unknown discover argument"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        !stdout.contains("192.0.2.50"),
        "stdout echoed caller argument: {stdout}"
    );
    assert!(
        !stderr.contains("192.0.2.50"),
        "stderr echoed caller argument: {stderr}"
    );
}

#[test]
fn cli_discover_json_with_missing_providers_reports_unavailable_truthfully() {
    let _guard = serial();
    let bin = env!("CARGO_BIN_EXE_fastlink");
    let empty_bin_dir = temp_dir("empty-path");

    let output = Command::new(bin)
        .args(["discover", "--json"])
        .env("PATH", &empty_bin_dir)
        .output()
        .expect("run fastlink discover --json");
    let _ = fs::remove_dir_all(&empty_bin_dir);

    assert!(
        output.status.success(),
        "discover --json failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        stdout.contains("\"schema\":\"fastlink.discover.v1\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"scope\":\"user_opt_in_lan_tailscale\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"provider\":\"lan_mdns\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"provider\":\"tailscale\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"status\":\"unavailable\""),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("\"nodes\":[]"), "stdout: {stdout}");
}

#[test]
fn library_discover_handles_missing_binaries_and_precancelled_flag_cross_platform() {
    let _guard = serial();
    let dir = temp_dir("cross-platform-missing");
    let config = DiscoveryConfig {
        lan_mdns_program: dir.join("missing-avahi-browse"),
        tailscale_program: dir.join("missing-tailscale"),
        timeout: Duration::from_secs(1),
        max_output_bytes: 16 * 1024,
    };

    let missing_report = discover_with_config(&config, None);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(missing_report.schema, "fastlink.discover.v1");
    assert_eq!(missing_report.scope, "user_opt_in_lan_tailscale");
    assert_eq!(missing_report.providers.len(), 2);
    assert_eq!(missing_report.providers[0].status, "unavailable");
    assert_eq!(missing_report.providers[1].status, "unavailable");
    assert!(missing_report.nodes.is_empty());
    assert!(render_discovery_json(&missing_report).is_ok());

    let cancel = AtomicBool::new(true);
    let cancelled_report = discover_with_config(&config, Some(&cancel));
    assert_eq!(cancelled_report.providers[0].status, "cancelled");
    assert_eq!(cancelled_report.providers[1].status, "cancelled");
    assert!(cancelled_report.nodes.is_empty());
    assert!(render_discovery_json(&cancelled_report).is_ok());
}

#[cfg(not(target_os = "linux"))]
#[test]
fn unsupported_platform_reports_unavailable_without_spawning_binaries() {
    let existing_exe = std::env::current_exe().expect("current test executable");
    let config = DiscoveryConfig {
        lan_mdns_program: existing_exe.clone(),
        tailscale_program: existing_exe,
        timeout: Duration::from_secs(1),
        max_output_bytes: 16 * 1024,
    };

    let report = discover_with_config(&config, None);
    assert_eq!(report.providers[0].provider, "lan_mdns");
    assert_eq!(report.providers[0].status, "unavailable");
    assert_eq!(
        report.providers[0].message.as_deref(),
        Some("mDNS discovery is only supported on Linux")
    );
    assert_eq!(report.providers[1].provider, "tailscale");
    assert_eq!(report.providers[1].status, "unavailable");
    assert_eq!(
        report.providers[1].message.as_deref(),
        Some("Tailscale discovery is only supported on Linux")
    );
    assert!(report.nodes.is_empty());
    assert!(render_discovery_json(&report).is_ok());
}

#[cfg(target_os = "linux")]
mod linux_tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use fastlink::discovery::{discover_with_config, render_discovery_json, DiscoveryConfig};
    use serde_json::Value;

    use super::{serial, temp_dir};

    fn write_executable(dir: &Path, name: &str, script: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, script).expect("write fake provider script");
        let mut perms = fs::metadata(&path).expect("script metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod script");
        path
    }

    #[test]
    fn tailscale_generic_peers_are_unverified_with_opaque_ids_and_redacted_addresses() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("ts-valid");
        let payload = r#"{
  "BackendState": "Running",
  "Self": {
    "ID": "nSelf000",
    "PublicKey": "nodekey:0000self",
    "HostName": "local-workstation",
    "DNSName": "local-workstation.tailnet-abc.ts.net.",
    "OS": "linux",
    "Online": true,
    "TailscaleIPs": ["100.64.99.1", "fd7a:115c:a1e0::1"]
  },
  "Peer": {
    "nodekey:aaaa1111": {
      "ID": "nPeerAlpha",
      "PublicKey": "nodekey:aaaa1111",
      "HostName": "gpu-box",
      "DNSName": "gpu-box.tailnet-abc.ts.net.",
      "OS": "linux",
      "Online": true,
      "TailscaleIPs": ["100.64.99.42", "fd7a:115c:a1e0::42"],
      "CurAddr": "203.0.113.8:41641",
      "Addrs": ["192.168.1.50:41641", "203.0.113.8:41641"]
    },
    "nodekey:bbbb2222": {
      "ID": "nPeerBeta",
      "PublicKey": "nodekey:bbbb2222",
      "HostName": "offline-box",
      "OS": "windows",
      "Online": false,
      "TailscaleIPs": ["100.64.99.99"]
    }
  }
}"#;
        write_executable(
            &bin_dir,
            "tailscale",
            &format!(
                "#!/bin/sh
[ \"$1\" = \"status\" ] && [ \"$2\" = \"--json\" ] && [ \"$#\" -eq 2 ] || {{
  echo \"unexpected tailscale args: $*\" >&2
  exit 9
}}
printf '%s\\n' '{payload}'
"
            ),
        );

        let first = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json");
        let second = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json twice");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(first.status.success());
        assert!(second.status.success());
        let stdout = String::from_utf8(first.stdout).expect("utf8 stdout");
        let stdout2 = String::from_utf8(second.stdout).expect("utf8 stdout2");

        for forbidden in [
            "100.64.99.42",
            "fd7a:115c:a1e0::42",
            "203.0.113.8",
            "192.168.1.50",
            "41641",
            "100.64.99.1",
            "fd7a:115c:a1e0::1",
            "100.64.99.99",
            "nPeerAlpha",
            "aaaa1111",
            "FASTLINK worker",
            "worker-ready",
        ] {
            assert!(
                !stdout.contains(forbidden),
                "forbidden value '{forbidden}' leaked into output: {stdout}"
            );
        }

        let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
        let doc2: Value = serde_json::from_str(&stdout2).expect("valid JSON 2");
        assert_eq!(doc["schema"], "fastlink.discover.v1");
        assert_eq!(doc["scope"], "user_opt_in_lan_tailscale");

        let providers = doc["providers"].as_array().expect("providers array");
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0]["provider"], "lan_mdns");
        assert_eq!(providers[0]["status"], "unavailable");
        assert_eq!(providers[1]["provider"], "tailscale");
        assert_eq!(providers[1]["status"], "available");
        assert!(providers[1]["message"].is_null());

        let nodes = doc["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 1, "expected only online remote peer: {stdout}");
        let node = &nodes[0];
        let node_id = node["node_id"].as_str().expect("node_id string");
        assert_eq!(node_id.len(), 32);
        assert!(node_id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_eq!(node_id, doc2["nodes"][0]["node_id"].as_str().unwrap());
        assert_eq!(node["display_name"], "gpu-box");
        assert_eq!(node["os"], "linux");
        assert!(node["arch"].is_null());
        assert!(node["logical_cpu_count"].is_null());
        assert!(node["ram_total_bytes"].is_null());
        assert!(node["ram_available_bytes"].is_null());
        assert_eq!(node["gpus"].as_array().unwrap().len(), 0);
        assert_eq!(node["transports"], serde_json::json!(["tailscale"]));
        assert_eq!(node["worker_state"], "unverified");
        assert_eq!(node["trust_state"], "unpaired");
    }

    #[test]
    fn tailscale_stopped_backend_state_reports_unavailable_without_raw_ips() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("ts-stopped");
        write_executable(
            &bin_dir,
            "tailscale",
            "#!/bin/sh
printf '%s\n' '{\"BackendState\":\"Stopped\",\"Self\":{\"TailscaleIPs\":[\"100.64.10.1\"]}}'
",
        );

        let output = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
        assert!(!stdout.contains("100.64.10.1"), "leaked IP: {stdout}");
        let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
        assert_eq!(doc["providers"][1]["provider"], "tailscale");
        assert_eq!(doc["providers"][1]["status"], "unavailable");
        let msg = doc["providers"][1]["message"]
            .as_str()
            .expect("unavailable message");
        assert!(msg.contains("Stopped"), "unexpected message: {msg}");
        assert_eq!(doc["nodes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn tailscale_unknown_online_state_and_offline_peers_are_omitted() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("ts-online-strict");
        let payload = r#"{
  "BackendState": "Running",
  "Peer": {
    "nodekey:online": {
      "ID": "nPeerOn",
      "HostName": "online-box",
      "Online": true,
      "TailscaleIPs": ["100.64.20.1"]
    },
    "nodekey:offline": {
      "ID": "nPeerOff",
      "HostName": "offline-box",
      "Online": false,
      "TailscaleIPs": ["100.64.20.2"]
    },
    "nodekey:null": {
      "ID": "nPeerNull",
      "HostName": "null-online-box",
      "Online": null,
      "TailscaleIPs": ["100.64.20.3"]
    },
    "nodekey:omitted": {
      "ID": "nPeerOmitted",
      "HostName": "missing-online-box",
      "TailscaleIPs": ["100.64.20.4"]
    }
  }
}"#;
        write_executable(
            &bin_dir,
            "tailscale",
            &format!(
                "#!/bin/sh
printf '%s\n' '{payload}'
"
            ),
        );

        let output = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
        let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
        let nodes = doc["nodes"].as_array().expect("nodes array");
        assert_eq!(
            nodes.len(),
            1,
            "only Online: true peer should be emitted: {stdout}"
        );
        assert_eq!(nodes[0]["display_name"], "online-box");
        for omitted in [
            "offline-box",
            "null-online-box",
            "missing-online-box",
            "100.64.20.",
        ] {
            assert!(!stdout.contains(omitted), "omitted peer leaked: {omitted}");
        }
    }

    #[test]
    fn lan_mdns_parses_resolved_services_deduplicates_and_strips_raw_addresses() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("mdns-valid");
        let mdns_lines = "\
+;eth0;IPv4;Workstation\\;One;_fastlink._tcp;local\n\
=;eth0;IPv4;Workstation\\;One;_fastlink._tcp;local;ws1.local;192.168.55.77;18777;\"os=linux\" \"arch=x86_64\" \"logical_cpu_count=16\" \"ram_total_bytes=34359738368\" \"ram_available_bytes=17179869184\" \"gpu0_name=Radeon RX 7900 XT\" \"gpu0_vram_total_bytes=21458059264\"\n\
=;eth0;IPv6;Workstation\\;One;_fastlink._tcp;local;ws1.local;fe80::cafe:1;18777;\"os=linux\" \"arch=x86_64\" \"logical_cpu_count=16\" \"ram_total_bytes=34359738368\" \"ram_available_bytes=17179869184\" \"gpu0_name=Radeon RX 7900 XT\" \"gpu0_vram_total_bytes=21458059264\"\n\
=;wlan0;IPv4;Minimal Peer;_fastlink._tcp;local;minimal.local;192.168.55.88;18778;";

        write_executable(
        &bin_dir,
        "avahi-browse",
        &format!(
            "#!/bin/sh
[ \"$1\" = \"-t\" ] && [ \"$2\" = \"-p\" ] && [ \"$3\" = \"-r\" ] && [ \"$4\" = \"_fastlink._tcp\" ] && [ \"$#\" -eq 4 ] || {{
  echo \"unexpected avahi-browse args: $*\" >&2
  exit 9
}}
printf '%s\\n' '{mdns_lines}'
"
        ),
    );

        let first = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json");
        let second = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json twice");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(first.status.success());
        let stdout = String::from_utf8(first.stdout).expect("utf8 stdout");
        let stdout2 = String::from_utf8(second.stdout).expect("utf8 stdout2");

        for forbidden in [
            "192.168.55.77",
            "192.168.55.88",
            "fe80::cafe:1",
            "18777",
            "18778",
            "ws1.local",
            "minimal.local",
        ] {
            assert!(
                !stdout.contains(forbidden),
                "forbidden value '{forbidden}' leaked into output: {stdout}"
            );
        }

        let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
        let doc2: Value = serde_json::from_str(&stdout2).expect("valid JSON 2");
        assert_eq!(doc["providers"][0]["provider"], "lan_mdns");
        assert_eq!(doc["providers"][0]["status"], "available");
        assert!(doc["providers"][0]["message"].is_null());

        let nodes = doc["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 2, "expected deduplicated 2 nodes: {stdout}");

        assert_eq!(nodes[0]["display_name"], "Minimal Peer");
        assert!(nodes[0]["os"].is_null());
        assert!(nodes[0]["arch"].is_null());
        assert!(nodes[0]["logical_cpu_count"].is_null());
        assert!(nodes[0]["ram_total_bytes"].is_null());
        assert!(nodes[0]["ram_available_bytes"].is_null());
        assert_eq!(nodes[0]["gpus"].as_array().unwrap().len(), 0);
        assert_eq!(nodes[0]["transports"], serde_json::json!(["lan"]));
        assert_eq!(nodes[0]["worker_state"], "unverified");
        assert_eq!(nodes[0]["trust_state"], "unpaired");

        assert_eq!(nodes[1]["display_name"], "Workstation;One");
        assert_eq!(nodes[1]["os"], "linux");
        assert_eq!(nodes[1]["arch"], "x86_64");
        assert_eq!(nodes[1]["logical_cpu_count"], 16);
        assert_eq!(nodes[1]["ram_total_bytes"], 34_359_738_368u64);
        assert_eq!(nodes[1]["ram_available_bytes"], 17_179_869_184u64);
        assert_eq!(nodes[1]["gpus"].as_array().unwrap().len(), 1);
        assert_eq!(nodes[1]["gpus"][0]["name"], "Radeon RX 7900 XT");
        assert_eq!(nodes[1]["gpus"][0]["vram_total_bytes"], 21_458_059_264u64);
        assert_eq!(nodes[1]["transports"], serde_json::json!(["lan"]));
        assert_eq!(nodes[1]["worker_state"], "unverified");
        assert_eq!(nodes[1]["trust_state"], "unpaired");

        assert_eq!(nodes[0]["node_id"], doc2["nodes"][0]["node_id"]);
        assert_eq!(nodes[1]["node_id"], doc2["nodes"][1]["node_id"]);
    }

    fn assert_json_has_no_forbidden_keys_or_addresses(
        value: &Value,
        raw_forbidden_values: &[&str],
    ) {
        const FORBIDDEN_KEYS: &[&str] = &[
            "ip",
            "ips",
            "ip_address",
            "ip_addresses",
            "addr",
            "addrs",
            "address",
            "addresses",
            "endpoint",
            "endpoints",
            "mac",
            "mac_address",
            "cur_addr",
            "curaddr",
            "tailscale_ips",
            "tailscaleips",
            "port",
            "ports",
            "socket",
            "sockaddr",
            "hostname",
            "host_name",
        ];

        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    let normalized = k.to_ascii_lowercase();
                    assert!(
                        !FORBIDDEN_KEYS.contains(&normalized.as_str()),
                        "forbidden JSON key '{k}' found in discovery output"
                    );
                    assert_json_has_no_forbidden_keys_or_addresses(v, raw_forbidden_values);
                }
            }
            Value::Array(items) => {
                for item in items {
                    assert_json_has_no_forbidden_keys_or_addresses(item, raw_forbidden_values);
                }
            }
            Value::String(s) => {
                for forbidden in raw_forbidden_values {
                    assert!(
                        !s.contains(forbidden),
                        "forbidden address/secret '{forbidden}' found in JSON string '{s}'"
                    );
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    #[test]
    fn failing_providers_never_leak_raw_stdout_or_stderr_addresses_or_keys() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("provider-failure-redaction");

        write_executable(
            &bin_dir,
            "avahi-browse",
            "#!/bin/sh
printf '%s\n' 'raw stdout leak 10.22.33.44 fe80::1:2:3 aa:bb:cc:dd:ee:ff endpoint=10.22.33.44:18777'
printf '%s\n' 'raw stderr leak 192.168.77.88 mac=11:22:33:44:55:66 cur_addr=203.0.113.9:9999' >&2
exit 2
",
        );
        write_executable(
            &bin_dir,
            "tailscale",
            "#!/bin/sh
printf '%s\n' '{\"TailscaleIPs\":[\"100.64.55.66\"],\"CurAddr\":\"198.51.100.7:41641\"}'
printf '%s\n' 'failed connecting to 100.64.55.66 [fd7a:115c:a1e0::55]:41641' >&2
exit 3
",
        );

        let output = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
        let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");

        let raw_forbidden = [
            "10.22.33.44",
            "fe80::1:2:3",
            "aa:bb:cc:dd:ee:ff",
            "192.168.77.88",
            "11:22:33:44:55:66",
            "203.0.113.9",
            "100.64.55.66",
            "198.51.100.7",
            "fd7a:115c:a1e0::55",
            "18777",
            "41641",
        ];
        for leaked in raw_forbidden {
            assert!(
                !stdout.contains(leaked),
                "leaked '{leaked}' in stdout: {stdout}"
            );
            assert!(
                !stderr.contains(leaked),
                "leaked '{leaked}' in stderr: {stderr}"
            );
        }

        let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
        assert_json_has_no_forbidden_keys_or_addresses(&doc, &raw_forbidden);
        assert_eq!(doc["providers"][0]["status"], "unavailable");
        assert_eq!(doc["providers"][1]["status"], "unavailable");
        assert_eq!(doc["nodes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn adversarial_smuggled_ips_macs_and_endpoints_are_stripped_from_peer_fields() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("smuggled-addresses");

        write_executable(
        &bin_dir,
        "avahi-browse",
        "#!/bin/sh
printf '%s\n' '=;eth0;IPv4;192.168.99.10;_fastlink._tcp;local;badhost.local;192.168.99.10;18777;\"os=linux 10.0.0.2\" \"arch=aa:bb:cc:dd:ee:ff\" \"gpu0_name=RX at fd7a:115c:a1e0::99\"'
",
    );

        let ts_payload = r#"{
  "BackendState": "Running",
  "Peer": {
    "nodekey:bad": {
      "ID": "nBadPeer",
      "HostName": "10.0.0.2:41641",
      "DNSName": "192.168.99.10.ts.net.",
      "OS": "linux mac 00:11:22:33:44:55",
      "Online": true,
      "TailscaleIPs": ["100.64.88.1"],
      "CurAddr": "203.0.113.55:41641",
      "Addrs": ["[fe80::dead:beef]:41641"]
    }
  }
}"#;
        write_executable(
            &bin_dir,
            "tailscale",
            &format!(
                "#!/bin/sh
printf '%s\n' '{ts_payload}'
"
            ),
        );

        let output = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
        let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");

        let raw_forbidden = [
            "192.168.99.10",
            "10.0.0.2",
            "aa:bb:cc:dd:ee:ff",
            "fd7a:115c:a1e0::99",
            "00:11:22:33:44:55",
            "100.64.88.1",
            "203.0.113.55",
            "fe80::dead:beef",
            "badhost.local",
            "18777",
            "41641",
        ];
        for leaked in raw_forbidden {
            assert!(
                !stdout.contains(leaked),
                "leaked '{leaked}' in stdout: {stdout}"
            );
            assert!(
                !stderr.contains(leaked),
                "leaked '{leaked}' in stderr: {stderr}"
            );
        }

        let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
        assert_json_has_no_forbidden_keys_or_addresses(&doc, &raw_forbidden);
        let nodes = doc["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 2);

        let lan_node = nodes
            .iter()
            .find(|n| n["transports"] == serde_json::json!(["lan"]))
            .expect("lan node");
        assert_eq!(lan_node["display_name"], "Unverified LAN peer");
        assert!(lan_node["os"].is_null());
        assert!(lan_node["arch"].is_null());
        assert_eq!(lan_node["gpus"].as_array().unwrap().len(), 0);

        let ts_node = nodes
            .iter()
            .find(|n| n["transports"] == serde_json::json!(["tailscale"]))
            .expect("tailscale node");
        assert_eq!(ts_node["display_name"], "Unverified Tailscale peer");
        assert!(ts_node["os"].is_null());
    }

    #[test]
    fn adversarial_dns_host_port_endpoints_are_stripped_from_json_and_human_output() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("smuggled-dns-endpoints");

        let mdns_lines = "\
=;eth0;IPv4;ws1.local:18777;_fastlink._tcp;local;ws1.local;192.168.55.10;18777;\"os=linux node1.lan:8080\" \"arch=x86_64 api.example.com:443\" \"gpu0_name=RX via ws1.local:18777\"\n\
=;eth0;IPv4;gpu-box:18777;_fastlink._tcp;local;single.local;192.168.55.12;18779;\"os=linux gpu-box:0443\" \"arch=x86_64 ws-single:8080\" \"gpu0_name=RX via gpu-box:18777\"\n\
=;eth0;IPv4;gpu-box:00000000000000000000000018777;_fastlink._tcp;local;padzero.local;192.168.55.13;18781;\"os=linux gpu-box:0000000000000000000000000443\" \"arch=x86_64 padzero.lan:0000000000000000000000008080\" \"gpu0_name=RX via node:0000000000000000000000000443\"\n\
=;eth0;IPv4;Workstation One;_fastlink._tcp;local;ws2.local;192.168.55.11;18778;\"os=linux\" \"arch=x86_64\" \"gpu0_name=Radeon RX 7900 XT\"";
        write_executable(
            &bin_dir,
            "avahi-browse",
            &format!(
                "#!/bin/sh
printf '%s\\n' '{mdns_lines}'
"
            ),
        );

        let ts_payload = r#"{
  "BackendState": "Running",
  "Peer": {
    "nodekey:dnsbad": {
      "ID": "nDnsBadPeer",
      "HostName": "gpu-box.tailnet-example.ts.net:41641",
      "DNSName": "gpu-box.tailnet-example.ts.net:41641.",
      "OS": "linux gpu-box.tailnet-example.ts.net:41641",
      "Online": true,
      "TailscaleIPs": ["100.64.99.42"]
    },
    "nodekey:zeropad": {
      "ID": "nZeroPadPeer",
      "HostName": "host.example:0443",
      "DNSName": "host.example:0443.",
      "OS": "linux via host.example:0443",
      "Online": true,
      "TailscaleIPs": ["100.64.99.44"]
    },
    "nodekey:singlelabelbad": {
      "ID": "nSingleLabelBad",
      "HostName": "gpu-box:18777",
      "DNSName": "gpu-box:0443.",
      "OS": "linux via gpu-box:0443",
      "Online": true,
      "TailscaleIPs": ["100.64.99.45"]
    },
    "nodekey:nodezeropad": {
      "ID": "nNodeZeroPad",
      "HostName": "node:0443",
      "DNSName": "node:0443.",
      "OS": "linux via node:0443",
      "Online": true,
      "TailscaleIPs": ["100.64.99.46"]
    },
    "nodekey:longzeropad": {
      "ID": "nLongZeroPad",
      "HostName": "gpu-box:00000000000000000000000018777",
      "DNSName": "gpu-box:0000000000000000000000000443.",
      "OS": "linux via gpu-box:00000000000000000000000018777",
      "Online": true,
      "TailscaleIPs": ["100.64.99.47"]
    },
    "nodekey:normallabel": {
      "ID": "nOrdinaryPeer",
      "HostName": "cuda:0",
      "DNSName": "render-node.tailnet-example.ts.net.",
      "OS": "linux v2.1:8b",
      "Online": true,
      "TailscaleIPs": ["100.64.99.43"]
    }
  }
}"#;
        write_executable(
            &bin_dir,
            "tailscale",
            &format!(
                "#!/bin/sh
printf '%s\n' '{ts_payload}'
"
            ),
        );

        let json_out = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json");
        let text_out = Command::new(bin)
            .arg("discover")
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(json_out.status.success());
        assert!(text_out.status.success());
        let json_stdout = String::from_utf8(json_out.stdout).expect("utf8 json stdout");
        let json_stderr = String::from_utf8(json_out.stderr).expect("utf8 json stderr");
        let text_stdout = String::from_utf8(text_out.stdout).expect("utf8 text stdout");
        let text_stderr = String::from_utf8(text_out.stderr).expect("utf8 text stderr");

        let forbidden_endpoints = [
            "gpu-box.tailnet-example.ts.net:41641",
            "gpu-box:18777",
            "gpu-box:00000000000000000000000018777",
            "gpu-box:0443",
            "gpu-box:0000000000000000000000000443",
            "node:0000000000000000000000000443",
            "padzero.lan:0000000000000000000000008080",
            "padzero.lan",
            "node:0443",
            "ws-single:8080",
            "host.example:0443",
            "ws1.local:18777",
            "node1.lan:8080",
            "api.example.com:443",
            "41641",
            "0443",
            "18777",
            "18778",
            "18779",
            "18781",
            "8080",
            "443",
            "ws1.local",
            "ws2.local",
            "single.local",
            "padzero.local",
            "gpu-box",
            "ws-single",
            "192.168.55.10",
            "192.168.55.11",
            "192.168.55.12",
            "192.168.55.13",
            "100.64.99.42",
            "100.64.99.43",
            "100.64.99.44",
            "100.64.99.45",
            "100.64.99.46",
            "100.64.99.47",
        ];
        for leaked in forbidden_endpoints {
            assert!(
                !json_stdout.contains(leaked),
                "leaked endpoint '{leaked}' in JSON stdout: {json_stdout}"
            );
            assert!(
                !json_stderr.contains(leaked),
                "leaked endpoint '{leaked}' in JSON stderr: {json_stderr}"
            );
            assert!(
                !text_stdout.contains(leaked),
                "leaked endpoint '{leaked}' in human stdout: {text_stdout}"
            );
            assert!(
                !text_stderr.contains(leaked),
                "leaked endpoint '{leaked}' in human stderr: {text_stderr}"
            );
        }

        let doc: Value = serde_json::from_str(&json_stdout).expect("valid JSON");
        assert_json_has_no_forbidden_keys_or_addresses(&doc, &forbidden_endpoints);
        let nodes = doc["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 10);

        let bad_lan_nodes: Vec<&Value> = nodes
            .iter()
            .filter(|n| {
                n["transports"] == serde_json::json!(["lan"])
                    && n["display_name"] == "Unverified LAN peer"
            })
            .collect();
        assert_eq!(
            bad_lan_nodes.len(),
            3,
            "smuggled LAN nodes should fall back to generic name"
        );
        for bad_lan in bad_lan_nodes {
            assert!(bad_lan["os"].is_null());
            assert!(bad_lan["arch"].is_null());
            assert_eq!(bad_lan["gpus"].as_array().unwrap().len(), 0);
        }

        let bad_ts_count = nodes
            .iter()
            .filter(|n| {
                n["transports"] == serde_json::json!(["tailscale"])
                    && n["display_name"] == "Unverified Tailscale peer"
                    && n["os"].is_null()
            })
            .count();
        assert_eq!(
            bad_ts_count, 5,
            "smuggled Tailscale nodes should fall back to generic name with null os"
        );

        let good_lan = nodes
            .iter()
            .find(|n| n["display_name"] == "Workstation One")
            .expect("ordinary LAN node should not be over-redacted");
        assert_eq!(good_lan["os"], "linux");
        assert_eq!(good_lan["arch"], "x86_64");
        assert_eq!(good_lan["gpus"][0]["name"], "Radeon RX 7900 XT");

        let good_ts = nodes
            .iter()
            .find(|n| n["display_name"] == "cuda:0")
            .expect("ordinary non-DNS label should not be over-redacted");
        assert_eq!(good_ts["os"], "linux v2.1:8b");

        // Failing provider stderr containing single-label endpoints must never leak.
        let err_dir = temp_dir("smuggled-dns-endpoints-err");
        write_executable(
            &err_dir,
            "avahi-browse",
            "#!/bin/sh
echo 'dial gpu-box:18777 gpu-box:00000000000000000000000018777 gpu-box:0443 node:0443 ws1.local:18777' >&2
exit 2
",
        );
        write_executable(
            &err_dir,
            "tailscale",
            "#!/bin/sh
echo 'dial gpu-box:0443 node:0443 gpu-box:0000000000000000000000000443 host.example:0443' >&2
exit 2
",
        );
        let err_json = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &err_dir)
            .output()
            .expect("run fastlink discover --json on failing provider");
        let err_text = Command::new(bin)
            .arg("discover")
            .env("PATH", &err_dir)
            .output()
            .expect("run fastlink discover on failing provider");
        let _ = fs::remove_dir_all(&err_dir);

        assert!(err_json.status.success());
        assert!(err_text.status.success());
        let err_json_out = String::from_utf8(err_json.stdout).expect("utf8 err json");
        let err_text_out = String::from_utf8(err_text.stdout).expect("utf8 err text");
        for leaked in [
            "gpu-box:18777",
            "gpu-box:00000000000000000000000018777",
            "gpu-box:0443",
            "gpu-box:0000000000000000000000000443",
            "node:0443",
            "ws1.local:18777",
            "host.example:0443",
        ] {
            assert!(!err_json_out.contains(leaked));
            assert!(!err_text_out.contains(leaked));
        }
    }

    #[test]
    fn unicode_and_idn_host_port_endpoints_are_stripped_from_json_and_human_output() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("unicode-idn-endpoints");

        let mdns_lines = "\
=;eth0;IPv4;büro:41641;_fastlink._tcp;local;buero.local;192.168.55.20;18777;\"os=linux büro.local:41641\" \"arch=x86_64 пример.рф:8080\" \"gpu0_name=RX via bu\u{0308}ro:41641\"\n\
=;eth0;IPv4;\u{0915}\u{093F}:41641;_fastlink._tcp;local;devanagari.local;192.168.55.22;18779;\"os=linux \u{0915}\u{093F}:0443\" \"arch=x86_64 ①:80\" \"gpu0_name=RX via \u{0915}\u{093F}:41641\"\n\
=;eth0;IPv4;\u{0E01}\u{0E31}:41641;_fastlink._tcp;local;thai.local;192.168.55.23;18780;\"os=linux \u{0E01}\u{0E31}:0443\" \"arch=x86_64 \u{0E01}\u{0E31}.local:0443\" \"gpu0_name=RX via \u{0E01}\u{0E31}:41641\"\n\
=;eth0;IPv4;Büro Workstation;_fastlink._tcp;local;buero2.local;192.168.55.21;18778;\"os=linux service:port\" \"arch=x86_64\" \"gpu0_name=Radeon RX 7900 XT\"";
        write_executable(
            &bin_dir,
            "avahi-browse",
            &format!(
                "#!/bin/sh
printf '%s\\n' '{mdns_lines}'
"
            ),
        );

        let ts_payload = concat!(
            r#"{"BackendState":"Running","Peer":{"#,
            r#""nodekey:unicodesingle":{"ID":"nUniSingle","HostName":"büro:41641","DNSName":"büro.local:41641.","OS":"linux via büro:41641","Online":true,"TailscaleIPs":["100.64.99.51"]},"#,
            r#""nodekey:nfd":{"ID":"nUniNfd","HostName":"bu"#,
            "\u{0308}",
            r#"ro:41641","DNSName":"gpu.bu"#,
            "\u{0308}",
            r#"ro.ts.net:18777.","OS":"linux cafe"#,
            "\u{0301}",
            r#":0443","Online":true,"TailscaleIPs":["100.64.99.52"]},"#,
            r#""nodekey:idn":{"ID":"nUniIdn","HostName":"пример.рф:8080","DNSName":"пример.рф:8080.","OS":"linux пример.рф:8080","Online":true,"TailscaleIPs":["100.64.99.53"]},"#,
            r#""nodekey:devanagari":{"ID":"nUniDeva","HostName":""#,
            "\u{0915}\u{093F}:41641",
            r#"","DNSName":""#,
            "\u{0915}\u{093F}.local:0443.",
            r#"","OS":"linux "#,
            "\u{0915}\u{093F}:41641",
            r#"","Online":true,"TailscaleIPs":["100.64.99.55"]},"#,
            r#""nodekey:thai":{"ID":"nUniThai","HostName":""#,
            "\u{0E01}\u{0E31}:41641",
            r#"","DNSName":""#,
            "\u{0E01}\u{0E31}.local:0443.",
            r#"","OS":"linux "#,
            "\u{0E01}\u{0E31}:0443",
            r#"","Online":true,"TailscaleIPs":["100.64.99.57"]},"#,
            r#""nodekey:safeunicode":{"ID":"nSafeUni","HostName":"München Node","DNSName":"muenchen.tailnet-example.ts.net.","OS":"linux Qwen2.5:72b","Online":true,"TailscaleIPs":["100.64.99.54"]},"#,
            r#""nodekey:safedevanagari":{"ID":"nSafeDeva","HostName":""#,
            "\u{0915}\u{093F} Node",
            r#"","DNSName":"deva.tailnet-example.ts.net.","OS":"linux cuda:0","Online":true,"TailscaleIPs":["100.64.99.56"]},"#,
            r#""nodekey:safethai":{"ID":"nSafeThai","HostName":""#,
            "\u{0E01}\u{0E31} Node",
            r#"","DNSName":"thai.tailnet-example.ts.net.","OS":"linux "#,
            "\u{0E01}\u{0E31}:port",
            r#"","Online":true,"TailscaleIPs":["100.64.99.58"]}"#,
            r#"}}"#
        );
        write_executable(
            &bin_dir,
            "tailscale",
            &format!(
                "#!/bin/sh
printf '%s\\n' '{ts_payload}'
"
            ),
        );

        let json_out = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json");
        let text_out = Command::new(bin)
            .arg("discover")
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover");

        assert!(json_out.status.success());
        assert!(text_out.status.success());
        let json_stdout = String::from_utf8(json_out.stdout).expect("utf8 json stdout");
        let json_stderr = String::from_utf8(json_out.stderr).expect("utf8 json stderr");
        let text_stdout = String::from_utf8(text_out.stdout).expect("utf8 text stdout");
        let text_stderr = String::from_utf8(text_out.stderr).expect("utf8 text stderr");

        let forbidden_endpoints = [
            "büro:41641",
            "bu\u{0308}ro:41641",
            "\u{0915}\u{093F}:41641",
            "\u{0915}\u{093F}:0443",
            "\u{0915}\u{093F}.local:0443",
            "\u{0E01}\u{0E31}:41641",
            "\u{0E01}\u{0E31}:0443",
            "\u{0E01}\u{0E31}.local:0443",
            "①:80",
            "büro.local:41641",
            "gpu.bu\u{0308}ro.ts.net:18777",
            "cafe\u{0301}:0443",
            "пример.рф:8080",
            "41641",
            "18777",
            "18778",
            "18779",
            "18780",
            "0443",
            "8080",
            "buero.local",
            "buero2.local",
            "devanagari.local",
            "thai.local",
            "192.168.55.20",
            "192.168.55.21",
            "192.168.55.22",
            "192.168.55.23",
            "100.64.99.51",
            "100.64.99.52",
            "100.64.99.53",
            "100.64.99.54",
            "100.64.99.55",
            "100.64.99.56",
            "100.64.99.57",
            "100.64.99.58",
        ];
        for leaked in forbidden_endpoints {
            assert!(
                !json_stdout.contains(leaked),
                "leaked endpoint '{leaked}' in JSON stdout: {json_stdout}"
            );
            assert!(
                !json_stderr.contains(leaked),
                "leaked endpoint '{leaked}' in JSON stderr: {json_stderr}"
            );
            assert!(
                !text_stdout.contains(leaked),
                "leaked endpoint '{leaked}' in human stdout: {text_stdout}"
            );
            assert!(
                !text_stderr.contains(leaked),
                "leaked endpoint '{leaked}' in human stderr: {text_stderr}"
            );
        }

        let doc: Value = serde_json::from_str(&json_stdout).expect("valid JSON");
        assert_json_has_no_forbidden_keys_or_addresses(&doc, &forbidden_endpoints);
        let nodes = doc["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 12);

        let bad_lan_nodes: Vec<&Value> = nodes
            .iter()
            .filter(|n| {
                n["transports"] == serde_json::json!(["lan"])
                    && n["display_name"] == "Unverified LAN peer"
            })
            .collect();
        assert_eq!(
            bad_lan_nodes.len(),
            3,
            "smuggled Unicode LAN nodes should fall back to generic name"
        );
        for bad_lan in bad_lan_nodes {
            assert!(bad_lan["os"].is_null());
            assert!(bad_lan["arch"].is_null());
            assert_eq!(bad_lan["gpus"].as_array().unwrap().len(), 0);
        }

        let bad_ts_count = nodes
            .iter()
            .filter(|n| {
                n["transports"] == serde_json::json!(["tailscale"])
                    && n["display_name"] == "Unverified Tailscale peer"
                    && n["os"].is_null()
            })
            .count();
        assert_eq!(
            bad_ts_count, 5,
            "smuggled Unicode/IDN Tailscale nodes should fall back to generic name with null os"
        );

        let good_lan = nodes
            .iter()
            .find(|n| n["display_name"] == "Büro Workstation")
            .expect("ordinary Unicode LAN node should not be over-redacted");
        assert_eq!(good_lan["os"], "linux service:port");
        assert_eq!(good_lan["arch"], "x86_64");
        assert_eq!(good_lan["gpus"][0]["name"], "Radeon RX 7900 XT");
        assert!(text_stdout.contains("Büro Workstation"));

        let good_ts = nodes
            .iter()
            .find(|n| n["display_name"] == "München Node")
            .expect("ordinary Unicode Tailscale node should not be over-redacted");
        assert_eq!(good_ts["os"], "linux Qwen2.5:72b");
        assert!(text_stdout.contains("München Node"));

        let good_deva = nodes
            .iter()
            .find(|n| n["display_name"] == "\u{0915}\u{093F} Node")
            .expect("ordinary Devanagari node without port should not be over-redacted");
        assert_eq!(good_deva["os"], "linux cuda:0");
        assert!(text_stdout.contains("\u{0915}\u{093F} Node"));

        let good_thai = nodes
            .iter()
            .find(|n| n["display_name"] == "\u{0E01}\u{0E31} Node")
            .expect("ordinary Thai node without port should not be over-redacted");
        assert_eq!(good_thai["os"], "linux \u{0E01}\u{0E31}:port");
        assert!(text_stdout.contains("\u{0E01}\u{0E31} Node"));

        // Failing provider stderr containing raw Unicode/IDN endpoints must also never leak.
        write_executable(
            &bin_dir,
            "avahi-browse",
            "#!/bin/sh
echo 'dial büro:41641 bu\u{0308}ro.local:18777 пример.рф:8080 \u{0915}\u{093F}:41641 \u{0E01}\u{0E31}:41641 ①:80' >&2
exit 2
",
        );
        write_executable(
            &bin_dir,
            "tailscale",
            "#!/bin/sh
echo 'dial \u{0915}\u{093F}:0443 \u{0E01}\u{0E31}:0443 gpu-box:18777 node:0443' >&2
exit 2
",
        );
        let err_json = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json on failing provider");
        let err_text = Command::new(bin)
            .arg("discover")
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover on failing provider");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(err_json.status.success());
        assert!(err_text.status.success());
        let err_json_out = String::from_utf8(err_json.stdout).expect("utf8 err json");
        let err_text_out = String::from_utf8(err_text.stdout).expect("utf8 err text");
        for leaked in [
            "büro:41641",
            "bu\u{0308}ro.local:18777",
            "пример.рф:8080",
            "\u{0915}\u{093F}:41641",
            "\u{0915}\u{093F}:0443",
            "\u{0E01}\u{0E31}:41641",
            "\u{0E01}\u{0E31}:0443",
            "gpu-box:18777",
            "node:0443",
            "①:80",
        ] {
            assert!(!err_json_out.contains(leaked));
            assert!(!err_text_out.contains(leaked));
        }
    }

    #[test]
    fn malformed_provider_output_reports_error_safely_without_crashing_valid_peer() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("malformed-output");

        write_executable(
            &bin_dir,
            "avahi-browse",
            "#!/bin/sh
printf '%s\n' 'garbage;192.168.12.34;not_an_avahi_record'
",
        );
        write_executable(
        &bin_dir,
        "tailscale",
        "#!/bin/sh
printf '%s\n' '{\"BackendState\":\"Running\",\"Peer\":{\"k\":{\"ID\":\"nOk\",\"HostName\":\"good-peer\",\"Online\":true,\"TailscaleIPs\":[\"100.64.0.9\"]}}}'
",
    );

        let first = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json");
        assert!(first.status.success());
        let stdout = String::from_utf8(first.stdout).expect("utf8 stdout");
        assert!(!stdout.contains("192.168.12.34"), "leaked IP: {stdout}");
        assert!(!stdout.contains("100.64.0.9"), "leaked IP: {stdout}");
        let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
        assert_eq!(doc["providers"][0]["provider"], "lan_mdns");
        assert_eq!(doc["providers"][0]["status"], "error");
        assert_eq!(doc["providers"][1]["provider"], "tailscale");
        assert_eq!(doc["providers"][1]["status"], "available");
        assert_eq!(doc["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(doc["nodes"][0]["display_name"], "good-peer");

        // Now test malformed Tailscale JSON (non-json + raw IP noise).
        write_executable(
            &bin_dir,
            "tailscale",
            "#!/bin/sh
printf '%s\n' '{not-json 198.51.100.44'
",
        );
        let second = Command::new(bin)
            .args(["discover", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover --json with bad tailscale");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(second.status.success());
        let stdout2 = String::from_utf8(second.stdout).expect("utf8 stdout2");
        assert!(!stdout2.contains("198.51.100.44"), "leaked IP: {stdout2}");
        let doc2: Value = serde_json::from_str(&stdout2).expect("valid JSON 2");
        assert_eq!(doc2["providers"][0]["status"], "error");
        assert_eq!(doc2["providers"][1]["status"], "error");
        assert_eq!(doc2["nodes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn oversized_provider_output_is_rejected_and_bounded() {
        let _guard = serial();
        let dir = temp_dir("oversized-output");
        let mdns = write_executable(
            &dir,
            "fake-mdns",
            "#!/bin/sh
line='=;eth0;IPv4;SpamPeer;_fastlink._tcp;local;spam.local;192.168.1.99;18777;\"os=linux\"'
i=0
while [ \"$i\" -lt 2000 ]; do
  printf '%s\n' \"$line\"
  i=$((i + 1))
done
",
        );
        let missing_ts = dir.join("missing-tailscale");

        let config = DiscoveryConfig {
            lan_mdns_program: mdns,
            tailscale_program: missing_ts,
            timeout: Duration::from_secs(3),
            max_output_bytes: 8 * 1024,
        };

        let started = Instant::now();
        let report = discover_with_config(&config, None);
        let elapsed = started.elapsed();
        let _ = fs::remove_dir_all(&dir);

        assert!(
            elapsed < Duration::from_secs(2),
            "oversized stream took too long: {elapsed:?}"
        );
        assert_eq!(report.providers[0].provider, "lan_mdns");
        assert_eq!(report.providers[0].status, "error");
        let msg = report.providers[0]
            .message
            .as_deref()
            .expect("limit message");
        assert!(
            msg.contains("byte limit"),
            "expected byte limit message, got: {msg}"
        );
        let json = render_discovery_json(&report).expect("valid redacted json");
        assert!(!json.contains("192.168.1.99"));
        assert_eq!(report.nodes.len(), 0);
    }

    #[test]
    fn provider_timeout_kills_child_and_reports_error() {
        let _guard = serial();
        let dir = temp_dir("provider-timeout");
        let pid_file = dir.join("child.pid");
        let mdns = write_executable(
            &dir,
            "slow-mdns",
            &format!(
                "#!/bin/sh
printf '%s\n' \"$$\" > '{}'
exec /bin/sh -c 'sleep 5'
",
                pid_file.display()
            ),
        );
        let missing_ts = dir.join("missing-tailscale");

        let config = DiscoveryConfig {
            lan_mdns_program: mdns,
            tailscale_program: missing_ts,
            timeout: Duration::from_millis(150),
            max_output_bytes: 64 * 1024,
        };

        let started = Instant::now();
        let report = discover_with_config(&config, None);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "timeout too slow: {elapsed:?}"
        );
        assert_eq!(report.providers[0].provider, "lan_mdns");
        assert_eq!(report.providers[0].status, "error");
        assert!(report.providers[0]
            .message
            .as_deref()
            .unwrap_or("")
            .contains("timed out"));
        assert_eq!(report.providers[1].provider, "tailscale");
        assert_eq!(report.providers[1].status, "unavailable");

        if let Ok(pid_str) = fs::read_to_string(&pid_file) {
            let alive = Command::new("/bin/kill")
                .args(["-0", pid_str.trim()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(!alive, "timed-out provider child PID {pid_str} still alive");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancellation_stops_running_provider_and_skips_subsequent_provider() {
        let _guard = serial();
        let dir = temp_dir("provider-cancel");
        let ts_marker = dir.join("tailscale-invoked");
        let mdns = write_executable(
            &dir,
            "slow-mdns",
            "#!/bin/sh
sleep 5
",
        );
        let ts = write_executable(
            &dir,
            "fake-ts",
            &format!(
                "#!/bin/sh
printf 'invoked\n' > '{}'
exit 0
",
                ts_marker.display()
            ),
        );

        let config = DiscoveryConfig {
            lan_mdns_program: mdns,
            tailscale_program: ts,
            timeout: Duration::from_secs(5),
            max_output_bytes: 64 * 1024,
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_bg = Arc::clone(&cancel);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            cancel_bg.store(true, Ordering::Release);
        });

        let started = Instant::now();
        let report = discover_with_config(&config, Some(&cancel));
        let elapsed = started.elapsed();
        handle.join().expect("join cancel thread");

        assert!(
            elapsed < Duration::from_secs(2),
            "cancel took too long: {elapsed:?}"
        );
        assert_eq!(report.providers[0].provider, "lan_mdns");
        assert_eq!(report.providers[0].status, "cancelled");
        assert_eq!(report.providers[1].provider, "tailscale");
        assert_eq!(report.providers[1].status, "cancelled");
        assert!(
            !ts_marker.exists(),
            "subsequent tailscale provider should not spawn after cancellation"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inherited_stdout_descendant_does_not_hang_discovery() {
        let _guard = serial();
        let dir = temp_dir("inherited-stdout");
        let holder_pid_file = dir.join("holder.pid");
        let mdns = write_executable(
        &dir,
        "mdns-with-holder",
        &format!(
            "#!/bin/sh
printf '%s\n' '=;eth0;IPv4;FastNode;_fastlink._tcp;local;fast.local;192.168.44.10;18777;\"os=linux\"'
(sleep 4) &
printf '%s\n' \"$!\" > '{}'
exit 0
",
            holder_pid_file.display()
        ),
    );
        let missing_ts = dir.join("missing-tailscale");

        let config = DiscoveryConfig {
            lan_mdns_program: mdns,
            tailscale_program: missing_ts,
            timeout: Duration::from_secs(5),
            max_output_bytes: 64 * 1024,
        };

        let started = Instant::now();
        let report = discover_with_config(&config, None);
        let elapsed = started.elapsed();

        if let Ok(pid_str) = fs::read_to_string(&holder_pid_file) {
            let _ = Command::new("/bin/kill")
                .args(["-9", pid_str.trim()])
                .status();
        }
        let _ = fs::remove_dir_all(&dir);

        assert!(
            elapsed < Duration::from_secs(1),
            "discovery blocked waiting on inherited stdout pipe: {elapsed:?}"
        );
        assert_eq!(report.providers[0].provider, "lan_mdns");
        assert_eq!(report.providers[0].status, "available");
        assert_eq!(report.nodes.len(), 1);
        assert_eq!(report.nodes[0].display_name, "FastNode");
    }

    #[test]
    fn provider_timeout_kills_child_process_group_including_descendants() {
        let _guard = serial();
        let dir = temp_dir("timeout-pg");
        let desc_pid_file = dir.join("desc.pid");
        let lock_file = dir.join("desc.lock");
        let lock_ready = dir.join("desc.ready");
        let mdns = write_executable(
            &dir,
            "slow-tree-mdns",
            &format!(
                "#!/bin/sh
/usr/bin/flock '{}' /bin/sh -c 'printf ok > \"{}\"; sleep 5' &
printf '%s\n' \"$!\" > '{}'
wait
",
                lock_file.display(),
                lock_ready.display(),
                desc_pid_file.display()
            ),
        );
        let missing_ts = dir.join("missing-tailscale");

        let config = DiscoveryConfig {
            lan_mdns_program: mdns,
            tailscale_program: missing_ts,
            timeout: Duration::from_millis(250),
            max_output_bytes: 64 * 1024,
        };

        let report = discover_with_config(&config, None);
        assert_eq!(report.providers[0].status, "error");
        assert!(
            lock_ready.exists(),
            "descendant did not start locking before timeout"
        );

        thread::sleep(Duration::from_millis(30));
        let lock_released = Command::new("/usr/bin/flock")
            .arg("-n")
            .arg(&lock_file)
            .arg("true")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if let Ok(pid_str) = fs::read_to_string(&desc_pid_file) {
            let _ = Command::new("/bin/kill")
                .args(["-9", pid_str.trim()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = fs::remove_dir_all(&dir);
        assert!(
            lock_released,
            "timed-out provider descendant still held flock (process group was not killed)"
        );
    }

    #[test]
    fn cli_discover_without_json_prints_human_summary_without_raw_addresses() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("human-summary");

        write_executable(
        &bin_dir,
        "avahi-browse",
        "#!/bin/sh
printf '%s\n' '=;eth0;IPv4;Workstation\\;One;_fastlink._tcp;local;ws1.local;192.168.55.77;18777;\"os=linux\"'
",
    );
        write_executable(
        &bin_dir,
        "tailscale",
        "#!/bin/sh
printf '%s\n' '{\"BackendState\":\"Running\",\"Peer\":{\"k\":{\"ID\":\"nOne\",\"HostName\":\"gpu-box\",\"OS\":\"linux\",\"Online\":true,\"TailscaleIPs\":[\"100.64.99.42\"]}}}'
",
    );

        let output = Command::new(bin)
            .arg("discover")
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink discover");
        let _ = fs::remove_dir_all(&bin_dir);

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
        for expected in [
            "fastlink.discover.v1",
            "user_opt_in_lan_tailscale",
            "lan_mdns: available",
            "tailscale: available",
            "Workstation;One",
            "gpu-box",
            "unverified",
            "unpaired",
        ] {
            assert!(
                stdout.contains(expected),
                "expected '{expected}' in human summary:\n{stdout}"
            );
        }
        for forbidden in ["192.168.55.77", "18777", "ws1.local", "100.64.99.42"] {
            assert!(
                !stdout.contains(forbidden),
                "forbidden '{forbidden}' leaked in human summary:\n{stdout}"
            );
        }
    }

    #[test]
    fn non_discover_cli_commands_never_invoke_discovery_providers() {
        let _guard = serial();
        let bin = env!("CARGO_BIN_EXE_fastlink");
        let bin_dir = temp_dir("opt-in-only");
        let marker = bin_dir.join("discovery-ran");

        for cli in ["avahi-browse", "tailscale"] {
            write_executable(
                &bin_dir,
                cli,
                &format!(
                    "#!/bin/sh
printf '%s\n' '{}' > '{}'
exit 99
",
                    cli,
                    marker.display()
                ),
            );
        }

        let help_out = Command::new(bin)
            .arg("--help")
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink --help");

        let devices_out = Command::new(bin)
            .args(["devices", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink devices --json");

        let plan_out = Command::new(bin)
            .args(["plan", "--json"])
            .env("PATH", &bin_dir)
            .output()
            .expect("run fastlink plan --json");

        let ran = marker.exists();
        let _ = fs::remove_dir_all(&bin_dir);
        assert_eq!(help_out.status.code(), Some(2));
        assert!(devices_out.status.success());
        assert!(plan_out.status.success());
        assert!(
            !ran,
            "non-discover CLI command unexpectedly invoked a discovery provider CLI"
        );
    }
}
