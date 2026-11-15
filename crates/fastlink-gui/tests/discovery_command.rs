//! Discovery command tests. Fixtures only: no LAN/Tailscale discovery is run.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fastlink_gui::discovery::{
    DiscoveryCommand, ProviderStatus, Transport, TrustState, WorkerState, DISCOVERY_SCHEMA,
    DISCOVERY_SCOPE,
};
use fastlink_gui::inventory::InventoryCommand;

const FIXTURE: &str = include_str!("../testdata/discovery-v1.json");

/// Serialize fake-script creation/execution to avoid transient ETXTBSY races
/// across parallel tests.
static SCRIPT_LOCK: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SCRIPT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be after the epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fastlink-discovery-{tag}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("write script");
    let mut permissions = fs::metadata(&path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod script");
    path
}

#[test]
fn invokes_discover_json_with_fastlink_bin_selection() {
    let _guard = serial();
    let dir = TempDir::new("valid");
    let script = write_script(
        dir.path(),
        "fastlink",
        &format!(
            "#!/bin/sh
[ \"$1\" = \"discover\" ] && [ \"$2\" = \"--json\" ] || {{
    echo \"unexpected arguments: $*\" >&2
    exit 9
}}
cat <<'JSON'
{FIXTURE}JSON
"
        ),
    );

    let discovery = DiscoveryCommand::new(InventoryCommand::new(&script))
        .run()
        .expect("fixture command must parse");

    assert_eq!(discovery.schema, DISCOVERY_SCHEMA);
    assert_eq!(discovery.scope, DISCOVERY_SCOPE);
    assert_eq!(discovery.providers.len(), 2);
    assert_eq!(discovery.providers[0].status, ProviderStatus::Available);
    assert_eq!(discovery.providers[1].status, ProviderStatus::Unavailable);
    assert_eq!(discovery.nodes.len(), 2);
    assert_eq!(discovery.nodes[0].transports, [Transport::Lan]);
    assert_eq!(
        discovery.nodes[0].worker_state,
        Some(WorkerState::Unverified)
    );
    assert_eq!(discovery.nodes[0].trust_state, Some(TrustState::Unpaired));
    assert_eq!(
        discovery.nodes[1].transports,
        [Transport::Tailscale],
        "a generic tailnet peer must remain a mere candidate"
    );
}

#[test]
fn bad_schema_fails_closed_without_raw_output() {
    let _guard = serial();
    let dir = TempDir::new("bad-schema");
    let script = write_script(
        dir.path(),
        "fastlink",
        "#!/bin/sh\nprintf '%s\\n' '{\"schema\":\"wrong\",\"scope\":\"secret-192.168.1.7\",\"providers\":[],\"nodes\":[]}'\n",
    );

    let error = DiscoveryCommand::new(InventoryCommand::new(&script))
        .run()
        .expect_err("bad schema must fail");
    assert!(
        error.summary.contains("did not match the expected schema"),
        "{error:?}"
    );
    let rendered = format!("{error:?}");
    assert!(!rendered.contains("192.168.1.7"), "{rendered}");
}

#[test]
fn raw_provider_stderr_address_is_not_returned_to_the_ui() {
    let _guard = serial();
    let dir = TempDir::new("stderr-address");
    let script = write_script(
        dir.path(),
        "fastlink",
        "#!/bin/sh\necho 'provider failed at 192.168.1.99:41641' >&2\nexit 3\n",
    );

    let error = DiscoveryCommand::new(InventoryCommand::new(&script))
        .run()
        .expect_err("failed discovery must fail safely");
    let rendered = format!("{error:?}");
    assert!(!rendered.contains("192.168.1.99"), "{rendered}");
    assert!(!rendered.contains("41641"), "{rendered}");
}

#[test]
fn discovery_timeout_is_bounded_and_safe() {
    let _guard = serial();
    let dir = TempDir::new("timeout");
    let script = write_script(dir.path(), "fastlink", "#!/bin/sh\nexec sleep 5\n");

    let started = Instant::now();
    let error = DiscoveryCommand::new(
        InventoryCommand::new(&script).with_timeout(Duration::from_millis(250)),
    )
    .run()
    .expect_err("discovery must time out");
    let elapsed = started.elapsed();

    assert!(error.summary.contains("timed out"), "{error:?}");
    assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    assert!(!format!("{error:?}").contains("sleep"));
}
