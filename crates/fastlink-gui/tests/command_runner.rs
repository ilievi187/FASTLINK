//! End-to-end command tests.
//!
//! Fake executables verify argument passing, error reporting and the invocation
//! timeout. When `FASTLINK_BIN` is set, a dedicated test invokes the actual
//! integrated `fastlink devices --json` binary and proves the GUI parser
//! accepts its real output. No GUI window is launched.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fastlink_gui::inventory::{
    InventoryCommand, LOCAL_ONLY_SCOPE, PROGRAM_ENV, SUPPORTED_SCHEMA, SUPPORTED_SCHEMA_VERSION,
};

const FIXTURE: &str = include_str!("../testdata/devices-v1.json");

/// Fake executables are written and spawned by a shared test process. Some
/// filesystems briefly report `ETXTBSY` when a script is executed while another
/// thread is creating a script, so script tests run one at a time.
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

fn valid_script() -> String {
    format!(
        "#!/bin/sh
[ \"$1\" = \"devices\" ] && [ \"$2\" = \"--json\" ] || {{
    echo \"unexpected arguments: $*\" >&2
    exit 9
}}
cat <<'JSON'
{FIXTURE}JSON
"
    )
}

#[test]
fn runs_the_contract_command_and_parses_backend_shape() {
    let _guard = serial();
    let dir = TempDir::new("valid");
    let script = write_script(dir.path(), "fastlink", &valid_script());

    let inventory = InventoryCommand::new(&script)
        .with_timeout(Duration::from_secs(10))
        .run()
        .expect("fake CLI must succeed");

    assert_eq!(inventory.schema, SUPPORTED_SCHEMA);
    assert_eq!(inventory.schema_version, SUPPORTED_SCHEMA_VERSION);
    assert_eq!(inventory.scope, LOCAL_ONLY_SCOPE);
    assert_eq!(inventory.node_count, 1);
    assert_eq!(inventory.nodes.len(), 1);
    let node = inventory.local_node().expect("sole node is local");
    assert_eq!(
        node.node_id.as_deref(),
        Some("0123456789abcdef0123456789abcdef")
    );
    assert_ne!(node.node_id.as_deref(), Some("local"));
    assert_eq!(node.hostname.as_deref(), Some("fixture-host"));
    assert_eq!(node.os.as_deref(), Some("linux"));
    assert_eq!(node.os_release.as_deref(), Some("6.12.0-fixture"));
    assert_eq!(node.arch.as_deref(), Some("x86_64"));
    assert_eq!(node.logical_cpu_count, Some(16));
    assert_eq!(node.ram_total_bytes, Some(34_359_738_368));
    assert_eq!(node.ram_available_bytes, Some(8_589_934_592));
    assert_eq!(node.ram_free_bytes, Some(4_294_967_296));
    assert_eq!(node.gpus.len(), 2);
    assert_eq!(node.gpus[0].card, "card0");
    assert_eq!(node.gpus[0].vram_total_bytes, Some(536_870_912));
    assert_eq!(node.capabilities.capacity_bytes, None);
    assert_eq!(node.capabilities.throughput_bps, None);
    assert_eq!(
        node.capabilities.network_connectivity.as_deref(),
        Some("unknown")
    );
    assert!(node.capabilities.remote_peers.is_empty());
    assert_eq!(node.unknown_fields.len(), 3);
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
        "#!/bin/sh\nprintf '%s\\n' '{\"schema\":\"fastlink.devices.v1\",\"schema_version\":2,\"scope\":\"local_only\",\"node_count\":0,\"nodes\":[]}'\n",
    );

    let error = InventoryCommand::new(&script).run().expect_err("must fail");
    assert!(error.summary.contains(SUPPORTED_SCHEMA), "{error:?}");
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

#[test]
fn large_stdout_and_stderr_above_pipe_capacity_are_drained_and_parsed() {
    let _guard = serial();
    let dir = TempDir::new("large-streams");
    let script = write_script(
        dir.path(),
        "fastlink",
        &format!(
            "#!/bin/sh
head -c 131072 /dev/zero | tr '\\0' 'E' >&2
head -c 131072 /dev/zero | tr '\\0' ' '
cat <<'JSON'
{FIXTURE}JSON
"
        ),
    );

    let started = Instant::now();
    let inventory = InventoryCommand::new(&script)
        .with_timeout(Duration::from_secs(3))
        .run()
        .expect("large-but-bounded stdout/stderr must not deadlock");
    let elapsed = started.elapsed();

    assert_eq!(inventory.schema, SUPPORTED_SCHEMA);
    assert_eq!(inventory.nodes.len(), 1);
    assert_eq!(
        inventory.local_node_id(),
        Some("0123456789abcdef0123456789abcdef")
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "capture took {elapsed:?}; pipe draining likely deadlocked"
    );
}

#[test]
fn oversized_stdout_is_rejected_deterministically() {
    let _guard = serial();
    let dir = TempDir::new("oversized-stdout");
    let script = write_script(
        dir.path(),
        "fastlink",
        &format!(
            "#!/bin/sh
head -c 2097152 /dev/zero | tr '\\0' ' '
cat <<'JSON'
{FIXTURE}JSON
"
        ),
    );

    let started = Instant::now();
    let error = InventoryCommand::new(&script)
        .with_timeout(Duration::from_secs(3))
        .run()
        .expect_err("oversized stdout must fail");
    let elapsed = started.elapsed();

    assert!(error.summary.contains("capture limit"), "{error:?}");
    assert!(!error.summary.contains("did not finish"), "{error:?}");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

#[test]
fn oversized_stderr_is_rejected_deterministically() {
    let _guard = serial();
    let dir = TempDir::new("oversized-stderr");
    let script = write_script(
        dir.path(),
        "fastlink",
        &format!(
            "#!/bin/sh
head -c 2097152 /dev/zero | tr '\\0' 'E' >&2
cat <<'JSON'
{FIXTURE}JSON
"
        ),
    );

    let started = Instant::now();
    let error = InventoryCommand::new(&script)
        .with_timeout(Duration::from_secs(3))
        .run()
        .expect_err("oversized stderr must fail");
    let elapsed = started.elapsed();

    assert!(error.summary.contains("capture limit"), "{error:?}");
    assert!(!error.summary.contains("did not finish"), "{error:?}");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
}

#[test]
fn exit_stderr_is_retained_with_a_truncation_marker() {
    let _guard = serial();
    let dir = TempDir::new("stderr-truncated");
    let script = write_script(
        dir.path(),
        "fastlink",
        "#!/bin/sh
head -c 131072 /dev/zero | tr '\\0' 'E' >&2
exit 3
",
    );

    let error = InventoryCommand::new(&script)
        .with_timeout(Duration::from_secs(3))
        .run()
        .expect_err("nonzero exit must fail");
    assert!(error.summary.contains("failed"), "{error:?}");
    let detail = error.detail.as_deref().unwrap_or_default();
    assert!(detail.contains("stderr truncated"), "{detail}");
    assert!(!error.summary.contains("did not finish"), "{error:?}");
}

#[test]
fn timeout_kills_the_owned_child_process() {
    let _guard = serial();
    let dir = TempDir::new("timeout-kill");
    let pidfile = dir.path().join("child.pid");
    let script = write_script(
        dir.path(),
        "fastlink",
        &format!(
            "#!/bin/sh
echo $$ > '{}'
exec sleep 5
",
            pidfile.display()
        ),
    );

    let error = InventoryCommand::new(&script)
        .with_timeout(Duration::from_millis(250))
        .run()
        .expect_err("must time out");
    assert!(error.summary.contains("did not finish"), "{error:?}");

    let pid = fs::read_to_string(&pidfile).expect("pid file");
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("kill -0 {} 2>/dev/null", pid.trim()))
        .status()
        .expect("run kill -0");
    assert!(
        !status.success(),
        "owned child is still running after timeout"
    );
}

#[test]
fn inherited_stream_holder_does_not_block_after_child_exit() {
    let _guard = serial();
    let dir = TempDir::new("grandchild");
    let pidfile = dir.path().join("grandchild.pid");
    let script = write_script(
        dir.path(),
        "fastlink",
        &format!(
            "#!/bin/sh
sleep 2 &
echo $! > '{}'
cat <<'JSON'
{FIXTURE}JSON
",
            pidfile.display()
        ),
    );

    let started = Instant::now();
    let inventory = InventoryCommand::new(&script)
        .with_timeout(Duration::from_secs(3))
        .run()
        .expect("a grandchild holding the output must not block the worker");
    let elapsed = started.elapsed();

    assert_eq!(inventory.schema, SUPPORTED_SCHEMA);
    assert_eq!(inventory.nodes.len(), 1);
    assert!(
        elapsed < Duration::from_secs(1),
        "worker waited {elapsed:?} for an inherited output holder"
    );

    if let Ok(pid) = fs::read_to_string(&pidfile) {
        let _ = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("kill {} 2>/dev/null || true", pid.trim()))
            .status();
    }
}

/// This test is intentionally environment-gated: `cargo test` must pass in
/// environments that only build the GUI package, while a full integration run
/// can set `FASTLINK_BIN` to the actual integrated binary and prove the parser
/// accepts its live output. It never launches a GUI window.
#[test]
fn actual_fastlink_binary_from_environment_satisfies_parser() {
    let Some(program) = std::env::var_os(PROGRAM_ENV) else {
        eprintln!("{PROGRAM_ENV} is not set; skipping actual-binary contract check");
        return;
    };
    let program = PathBuf::from(program);
    assert!(
        !program.as_os_str().is_empty(),
        "{PROGRAM_ENV} must not be empty"
    );

    eprintln!(
        "invoking actual FASTLINK binary from {PROGRAM_ENV}: {}",
        program.display()
    );
    let inventory = InventoryCommand::new(&program)
        .with_timeout(Duration::from_secs(30))
        .run()
        .expect("actual `fastlink devices --json` must satisfy the GUI parser");

    assert_eq!(inventory.schema, SUPPORTED_SCHEMA);
    assert_eq!(inventory.schema_version, SUPPORTED_SCHEMA_VERSION);
    assert_eq!(inventory.scope, LOCAL_ONLY_SCOPE);
    assert_eq!(inventory.node_count, inventory.nodes.len());
    assert_eq!(inventory.nodes.len(), 1);
    let local = inventory.local_node().expect("sole node is local");
    assert_ne!(local.node_id.as_deref(), Some("local"));
    if let Some(id) = local.node_id.as_deref() {
        assert_eq!(id.len(), 32, "hashed node_id must be preserved");
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "node_id must remain a lowercase hex hash"
        );
    }
    // Capabilities are reported fields; unknown stays unknown downstream.
    eprintln!(
        "accepted actual inventory: schema={} scope={} nodes={} node_id_present={} unknown_fields={}",
        inventory.schema,
        inventory.scope,
        inventory.nodes.len(),
        local.node_id.is_some(),
        local.unknown_fields.len()
    );
}
