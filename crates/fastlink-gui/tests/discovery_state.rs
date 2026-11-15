//! State/action tests for explicit discovery. Fixtures only; no network scan.

#![cfg(target_os = "linux")]
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fastlink_gui::inventory::InventoryCommand;
use fastlink_gui::state::{AppState, Screen};

const DEVICES_FIXTURE: &str = include_str!("../testdata/devices-v1.json");
const DISCOVERY_FIXTURE: &str = include_str!("../testdata/discovery-v1.json");

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
            "fastlink-discovery-state-{tag}-{}-{unique}",
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

fn write_fake_cli(dir: &Path, log: &Path) -> PathBuf {
    write_script(
        dir,
        "fastlink",
        &format!(
            "#!/bin/sh
echo \"$1 $2\" >> '{log}'
case \"$1\" in
  devices) cat <<'JSON'
{DEVICES_FIXTURE}JSON
    ;;
  discover) cat <<'JSON'
{DISCOVERY_FIXTURE}JSON
    ;;
  *) exit 9 ;;
esac
",
            log = log.display()
        ),
    )
}

fn log_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn wait_for_local(state: &mut AppState) {
    let mut waited = 0;
    while state.loader().is_busy() {
        assert!(waited < 200, "local worker did not finish");
        state.poll();
        thread::sleep(Duration::from_millis(10));
        waited += 1;
    }
}

fn wait_for_discovery(state: &mut AppState) {
    let mut waited = 0;
    while state.discovery_busy() {
        assert!(waited < 200, "discovery worker did not finish");
        state.poll();
        thread::sleep(Duration::from_millis(10));
        waited += 1;
    }
}

#[test]
fn app_construction_never_starts_discovery() {
    let _guard = serial();
    let dir = TempDir::new("launch");
    let log = dir.path().join("calls.log");
    let script = write_fake_cli(dir.path(), &log);

    let mut state = AppState::new(InventoryCommand::new(&script));
    thread::sleep(Duration::from_millis(50));
    state.poll();

    assert_eq!(state.screen(), Screen::Idle);
    assert!(!state.discovery_busy());
    assert!(state.discovery().is_none());
    assert!(state.discovery_error().is_none());
    assert!(log_lines(&log).is_empty(), "launch made a CLI call");
}

#[test]
fn local_refresh_does_not_start_discovery() {
    let _guard = serial();
    let dir = TempDir::new("refresh");
    let log = dir.path().join("calls.log");
    let script = write_fake_cli(dir.path(), &log);

    let mut state = AppState::new(InventoryCommand::new(&script));
    state.refresh();
    wait_for_local(&mut state);

    assert_eq!(state.screen(), Screen::Devices);
    assert!(state.discovery().is_none());
    assert!(!state.discovery_busy());
    assert_eq!(log_lines(&log), ["devices --json"]);
}

#[test]
fn explicit_discovery_action_invokes_discover_json() {
    let _guard = serial();
    let dir = TempDir::new("explicit");
    let log = dir.path().join("calls.log");
    let script = write_fake_cli(dir.path(), &log);

    let mut state = AppState::new(InventoryCommand::new(&script));
    assert!(state.discover());
    assert!(state.discovery_busy());
    wait_for_discovery(&mut state);

    let calls = log_lines(&log);
    assert!(calls.contains(&"discover --json".to_owned()), "{calls:?}");
    assert!(!calls.contains(&"devices --json".to_owned()), "{calls:?}");
    let discovery = state.discovery().expect("discovery result");
    assert_eq!(discovery.providers.len(), 2);
    assert_eq!(discovery.nodes.len(), 2);
    assert!(state.discovery_error().is_none());
    assert!(state.discovery_age().is_some());
}

#[test]
fn cancel_stops_discovery_and_provider_child_promptly() {
    let _guard = serial();
    let dir = TempDir::new("cancel");
    let provider_pidfile = dir.path().join("provider.pid");
    let cli_pidfile = dir.path().join("cli.pid");
    let script = write_script(
        dir.path(),
        "fastlink",
        &format!(
            "#!/bin/sh
if [ \"$1\" = \"discover\" ]; then
  sleep 60 &
  provider=$!
  echo \"$provider\" > '{}'
  echo \"$$\" > '{}'
  wait \"$provider\"
fi
exit 9
",
            provider_pidfile.display(),
            cli_pidfile.display()
        ),
    );

    let mut state = AppState::new(InventoryCommand::new(&script));
    assert!(state.discover(), "discovery must start");
    wait_for_file(&provider_pidfile);
    wait_for_file(&cli_pidfile);
    let provider_pid = read_pid(&provider_pidfile);
    let cli_pid = read_pid(&cli_pidfile);

    let started = Instant::now();
    assert!(state.cancel_discovery(), "active discovery must cancel");
    wait_for_discovery(&mut state);
    let elapsed = started.elapsed();

    assert!(elapsed < Duration::from_secs(2), "cancel took {elapsed:?}");
    assert!(state.discovery().is_none());
    let error = state.discovery_error().expect("cancelled error");
    assert!(error.summary.contains("cancelled"), "{error:?}");
    assert!(process_is_gone_within(cli_pid, Duration::from_secs(2)));
    assert!(process_is_gone_within(provider_pid, Duration::from_secs(2)));
}

fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("numeric pid")
}

fn wait_for_file(path: &Path) {
    let mut waited = 0;
    while !path.exists() {
        assert!(waited < 200, "timed out waiting for {}", path.display());
        thread::sleep(Duration::from_millis(10));
        waited += 1;
    }
}

fn process_is_gone_within(pid: u32, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        let alive = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("kill -0 {pid} 2>/dev/null"))
            .status()
            .expect("run kill -0")
            .success();
        if !alive {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}
