//! End-to-end tests for `InventoryCommand::run` using fake executables.
//!
//! These are Unix-only because they rely on executable shell scripts. The
//! parser, state and UI tests run on every platform.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fastlink_gui::inventory::{InventoryCommand, PROGRAM_ENV};

/// Fake executables are written and spawned by a shared test process. Some
/// filesystems briefly report `ETXTBSY` when a script is executed while another
/// thread is creating a script, so these tests run one at a time.
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
            "fastlink-gui-{tag}-{}-{unique}",
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

const VALID_OUTPUT: &str = r#"#!/bin/sh
[ "$1" = "devices" ] && [ "$2" = "--json" ] || {
    echo "unexpected arguments: $*" >&2
    exit 9
}
cat <<'JSON'
{
  "schema_version": 1,
  "devices": [{
    "id": "local",
    "name": "script-host",
    "os": "linux",
    "architecture": "x86_64",
    "cpu_logical_cores": 12,
    "ram_total_bytes": 34359738368,
    "ram_available_bytes": 17179869184,
    "gpu_devices": [],
    "backend": "cpu",
    "network_interfaces": []
  }],
  "status": "local_only"
}
JSON
"#;

#[test]
fn runs_the_contract_command_and_parses_output() {
    let _guard = serial();
    let dir = TempDir::new("valid");
    let script = write_script(dir.path(), "fastlink", VALID_OUTPUT);

    let inventory = InventoryCommand::new(&script)
        .with_timeout(Duration::from_secs(10))
        .run()
        .expect("fake CLI must succeed");

    assert_eq!(inventory.schema_version, 1);
    assert_eq!(inventory.status.as_deref(), Some("local_only"));
    assert_eq!(inventory.devices.len(), 1);
    let device = &inventory.devices[0];
    assert_eq!(device.id, "local");
    assert_eq!(device.name.as_deref(), Some("script-host"));
    assert_eq!(device.cpu_logical_cores, Some(12));
    assert_eq!(device.ram_total_bytes, Some(34_359_738_368));
    assert_eq!(device.ram_available_bytes, Some(17_179_869_184));
    assert_eq!(device.backend.as_deref(), Some("cpu"));
    assert_eq!(device.gpu_devices.as_deref(), Some(&[][..]));
}

#[test]
fn nonzero_exit_reports_stderr_and_an_actionable_hint() {
    let _guard = serial();
    let dir = TempDir::new("failure");
    let script = write_script(
        dir.path(),
        "fastlink",
        "#!/bin/sh\necho \"error: unrecognized subcommand 'devices'\" >&2\nexit 2\n",
    );

    let error = InventoryCommand::new(&script).run().expect_err("must fail");
    assert!(error.summary.contains("failed"), "{error:?}");
    assert!(
        error.hint.as_deref().unwrap().contains("devices --json"),
        "{error:?}"
    );
    assert!(
        error
            .detail
            .as_deref()
            .unwrap()
            .contains("unrecognized subcommand"),
        "{error:?}"
    );
}

#[test]
fn empty_stdout_is_an_explicit_error() {
    let _guard = serial();
    let dir = TempDir::new("empty");
    let script = write_script(dir.path(), "fastlink", "#!/bin/sh\nexit 0\n");

    let error = InventoryCommand::new(&script).run().expect_err("must fail");
    assert!(error.summary.contains("produced no JSON"), "{error:?}");
}

#[test]
fn wrong_schema_version_is_reported_with_detail() {
    let _guard = serial();
    let dir = TempDir::new("schema");
    let script = write_script(
        dir.path(),
        "fastlink",
        "#!/bin/sh\nprintf '%s\\n' '{\"schema_version\":2,\"devices\":[],\"status\":\"local_only\"}'\n",
    );

    let error = InventoryCommand::new(&script).run().expect_err("must fail");
    assert!(error.summary.contains("schema v1"), "{error:?}");
    assert!(
        error
            .detail
            .as_deref()
            .unwrap()
            .contains("schema_version 2"),
        "{error:?}"
    );
}

#[test]
fn missing_program_explains_how_to_configure_the_path() {
    let _guard = serial();
    let dir = TempDir::new("missing");
    let missing = dir.path().join("not-a-real-fastlink");

    let error = InventoryCommand::new(&missing)
        .run()
        .expect_err("must fail");
    assert!(error.summary.contains("could not start"), "{error:?}");
    let hint = error.hint.expect("hint");
    assert!(hint.contains("--fastlink-bin"), "{hint}");
    assert!(hint.contains(PROGRAM_ENV), "{hint}");
}

#[test]
fn hanging_inventory_is_bounded_by_the_timeout() {
    let _guard = serial();
    let dir = TempDir::new("timeout");
    let script = write_script(dir.path(), "fastlink", "#!/bin/sh\nexec sleep 5\n");

    let started = Instant::now();
    let error = InventoryCommand::new(&script)
        .with_timeout(Duration::from_millis(250))
        .run()
        .expect_err("must time out");
    let elapsed = started.elapsed();

    assert!(error.summary.contains("did not finish"), "{error:?}");
    assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
}
