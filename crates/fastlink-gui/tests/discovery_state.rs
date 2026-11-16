//! State/action tests for explicit discovery. Fixtures only; no network scan.

#![cfg(target_os = "linux")]
#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
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

struct ReapOnDrop {
    child: Option<Child>,
    kill_first: bool,
}

impl ReapOnDrop {
    fn new(child: Child, kill_first: bool) -> Self {
        Self {
            child: Some(child),
            kill_first,
        }
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().expect("active child").id()
    }

    fn kill_and_wait(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn wait_reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        if self.kill_first {
            self.kill_and_wait();
        } else {
            self.wait_reap();
        }
    }
}

fn wait_until_child_exited_unreaped(child_pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: Zeroing `siginfo_t` satisfies `waitid` requirements, and `WNOWAIT`
        // waits for the direct child to enter zombie state without reaping its PID.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                libc::id_t::from(child_pid),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: `waitid` populated `info` when `rc == 0`.
            if unsafe { info.si_pid() } != 0 {
                return;
            }
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            panic!("waitid WNOWAIT failed for child {child_pid}: {err}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for child {child_pid} to become unreaped zombie"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn unreaped_zombie_child_is_reported_exited_and_reaped() {
    let _guard = serial();
    let child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn quick-exiting child");
    let mut zombie = ReapOnDrop::new(child, false);
    let pid = zombie.pid();
    wait_until_child_exited_unreaped(pid);

    let gone = process_is_gone_within(pid, Duration::from_millis(80));
    zombie.wait_reap();
    assert!(
        gone,
        "unreaped zombie child (pid {pid}) must be reported as no longer executing"
    );
}

#[test]
fn live_sleeping_child_stays_alive_until_killed_and_reaped() {
    let _guard = serial();
    let child = Command::new("/bin/sleep")
        .arg("10")
        .spawn()
        .expect("spawn sleeping child");
    let mut sleeper = ReapOnDrop::new(child, true);
    let pid = sleeper.pid();

    assert!(
        !process_is_gone_within(pid, Duration::from_millis(80)),
        "live sleeping child (pid {pid}) must still be reported alive"
    );

    sleeper.kill_and_wait();
    assert!(
        process_is_gone_within(pid, Duration::from_millis(500)),
        "killed and reaped child (pid {pid}) must be reported gone"
    );
}

#[test]
fn proc_stat_parser_handles_comm_parens_zombies_and_ambiguity() {
    assert_eq!(parse_proc_stat_state("42 (simple) Z 1 42"), Some('Z'));
    assert_eq!(
        parse_proc_stat_state("42 (weird (comm) name)) Z 1 42"),
        Some('Z')
    );
    assert_eq!(parse_proc_stat_state("42 (sleep 60) S 1 42"), Some('S'));
    assert_eq!(parse_proc_stat_state("42 (comm) "), None);
    assert_eq!(parse_proc_stat_state("42 (comm) ZZ 1"), None);
    assert_eq!(parse_proc_stat_state("missing_open) Z 1"), None);

    assert!(proc_stat_read_result_is_gone(Ok(
        "42 (weird (comm) )) Z 1 42".to_string()
    )));
    assert!(proc_stat_read_result_is_gone(Err(io::Error::from(
        io::ErrorKind::NotFound
    ))));
    assert!(!proc_stat_read_result_is_gone(Ok(
        "42 (sleep) S 1 42".to_string()
    )));
    assert!(!proc_stat_read_result_is_gone(Ok(
        "42 (ambiguous)".to_string()
    )));
    assert!(!proc_stat_read_result_is_gone(Err(
        io::Error::from_raw_os_error(libc::EPERM)
    )));
    assert!(!proc_stat_read_result_is_gone(Err(io::Error::from(
        io::ErrorKind::PermissionDenied
    ))));
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

fn parse_proc_stat_state(stat: &str) -> Option<char> {
    let (head, tail) = stat.rsplit_once(')')?;
    if !head.contains('(') {
        return None;
    }
    let state = tail.split_whitespace().next()?;
    let mut chars = state.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(ch)
}

fn proc_stat_read_result_is_gone(read_result: io::Result<String>) -> bool {
    match read_result {
        Ok(stat) => parse_proc_stat_state(&stat) == Some('Z'),
        Err(err) => err.kind() == io::ErrorKind::NotFound,
    }
}

fn proc_matches_current_pid_namespace() -> bool {
    let current = std::process::id().to_string();
    fs::read_link("/proc/self")
        .ok()
        .is_some_and(|target| target.to_str() == Some(current.as_str()))
}

struct FdGuard(libc::c_int);

impl Drop for FdGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` is an open file descriptor owned by this guard.
        unsafe {
            libc::close(self.0);
        }
    }
}

fn process_is_gone_in_namespace(pid: u32) -> bool {
    let Ok(pid_t) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid_t <= 0 {
        return false;
    }
    // SAFETY: `pidfd_open` is side-effect free and resolves `pid_t` within the
    // caller's active PID namespace, unlike `/proc` when `/proc` belongs to an
    // outer PID namespace.
    let raw_fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid_t, 0) } as libc::c_int;
    if raw_fd < 0 {
        let err = io::Error::last_os_error();
        return err.raw_os_error() == Some(libc::ESRCH);
    }
    let fd = FdGuard(raw_fd);
    let mut pfd = libc::pollfd {
        fd: fd.0,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `pfd` points to an initialized `pollfd` for open pidfd `fd.0`.
        let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
        if rc < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        return rc > 0 && (pfd.revents & libc::POLLIN) != 0;
    }
}

fn process_is_exited(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if proc_matches_current_pid_namespace() {
        return proc_stat_read_result_is_gone(fs::read_to_string(format!("/proc/{pid}/stat")));
    }
    process_is_gone_in_namespace(pid)
}

fn process_is_gone_within(pid: u32, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if process_is_exited(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}
