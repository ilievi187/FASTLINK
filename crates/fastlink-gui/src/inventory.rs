//! Reader for the implemented `fastlink devices --json` contract.
//!
//! The GUI must not invent telemetry. Every node value comes from the inventory
//! command; this module only parses, validates and transports it. The local
//! device is identified by scope (`local_only` with exactly one node), never by
//! a literal `"local"` id: the backend reports an opaque hashed `node_id`.
//!
//! Unknown or `null` optional values stay [`None`] all the way to the UI.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(not(unix))]
use std::fs::{self, File, OpenOptions};
#[cfg(not(unix))]
use std::io::{Read, Seek, SeekFrom};
#[cfg(not(unix))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(unix))]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(unix)]
use std::process::{ChildStderr, ChildStdout};

use serde::Deserialize;

/// Schema emitted by the integrated `fastlink devices --json` command.
pub const SUPPORTED_SCHEMA: &str = "fastlink.devices.v1";
/// Schema version understood by this build of the GUI.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;
/// The only inventory scope implemented in this milestone.
pub const LOCAL_ONLY_SCOPE: &str = "local_only";
/// Default executable name looked up on `PATH`.
pub const DEFAULT_PROGRAM: &str = "fastlink";
/// Environment variable that overrides the CLI path.
pub const PROGRAM_ENV: &str = "FASTLINK_BIN";
/// Default upper bound for one inventory invocation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// Per-stream capture limit. On Linux/Unix the nonblocking reader enforces this
/// count in the capture path; on the non-Unix fallback it is a polling
/// threshold (best-effort, not an OS-enforced hard bound).
pub const MAX_CAPTURE_BYTES: u64 = 1024 * 1024;
/// How much stderr is retained for a diagnostic message.
pub const MAX_STDERR_DETAIL_BYTES: usize = 64 * 1024;
/// Maximum characters retained in the rendered error detail.
pub const UI_DETAIL_CHARS: usize = 900;
/// Maximum lines retained in the rendered error detail.
pub const UI_DETAIL_LINES: usize = 12;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
#[cfg(unix)]
const POST_EXIT_DRAIN: Duration = Duration::from_millis(50);
#[cfg(not(unix))]
const MAX_CAPTURE_CREATE_ATTEMPTS: usize = 16;
#[cfg(not(unix))]
static NEXT_CAPTURE_ID: AtomicU64 = AtomicU64::new(0);

/// Top-level inventory document.
///
/// Additive unknown fields are tolerated so a future schema minor change does
/// not break this shell; structural fields and the schema/scope are validated.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DeviceInventory {
    pub schema: String,
    pub schema_version: u32,
    pub scope: String,
    #[serde(default)]
    pub discovery_mode: Option<String>,
    pub node_count: usize,
    pub nodes: Vec<InventoryNode>,
}

/// One node reported by the inventory command.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InventoryNode {
    /// Opaque hashed backend identifier. Never invented, normalized or guessed.
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub node_id_source: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub os_release: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub logical_cpu_count: Option<u64>,
    #[serde(default)]
    pub ram_total_bytes: Option<u64>,
    #[serde(default)]
    pub ram_available_bytes: Option<u64>,
    #[serde(default)]
    pub ram_free_bytes: Option<u64>,
    #[serde(default)]
    pub gpus: Vec<GpuDevice>,
    pub capabilities: NodeCapabilities,
    #[serde(default)]
    pub unknown_fields: Vec<String>,
}

/// GPU metadata reported from passive host evidence.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GpuDevice {
    #[serde(default)]
    pub card: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub driver: Option<String>,
    #[serde(default)]
    pub vendor_id: Option<String>,
    #[serde(default)]
    pub pci_device_id: Option<String>,
    #[serde(default)]
    pub vram_total_bytes: Option<u64>,
    #[serde(default)]
    pub vram_free_bytes: Option<u64>,
}

/// Capabilities exactly as reported. Values that are not measured remain
/// [`None`]; the backend also uses the string `"unknown"` for connectivity.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct NodeCapabilities {
    #[serde(default)]
    pub capacity_bytes: Option<u64>,
    #[serde(default)]
    pub throughput_bps: Option<u64>,
    #[serde(default)]
    pub network_connectivity: Option<String>,
    #[serde(default)]
    pub remote_peers: Vec<String>,
}

impl DeviceInventory {
    /// True when the backend explicitly scoped this snapshot to the local host.
    #[must_use]
    pub fn is_local_only(&self) -> bool {
        self.scope == LOCAL_ONLY_SCOPE
    }

    /// The sole node in `scope:"local_only"` is the local device.
    #[must_use]
    pub fn local_node_index(&self) -> Option<usize> {
        (self.is_local_only() && self.nodes.len() == 1).then_some(0)
    }

    #[must_use]
    pub fn local_node(&self) -> Option<&InventoryNode> {
        self.local_node_index().map(|index| &self.nodes[index])
    }

    /// Opaque id of the local node, if the backend could derive one.
    #[must_use]
    pub fn local_node_id(&self) -> Option<&str> {
        self.local_node().and_then(|node| node.node_id.as_deref())
    }

    /// True only for the identified local node; there is no implicit `"local"`.
    #[must_use]
    pub fn is_local_node(&self, node_id: &str) -> bool {
        self.local_node_id() == Some(node_id)
    }
}

impl InventoryNode {
    /// Preserve the backend id exactly; `None` means it was not derivable.
    #[must_use]
    pub fn opaque_id(&self) -> Option<&str> {
        self.node_id.as_deref()
    }
}

/// Structural problems in an inventory document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryError {
    Json(String),
    UnsupportedSchema { found: String },
    UnsupportedSchemaVersion { found: u32 },
    UnsupportedScope { found: String },
    NodeCountMismatch { declared: usize, actual: usize },
    TooManyLocalNodes { count: usize },
    EmptyNodeId { index: usize },
    DuplicateNodeId { id: String },
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid inventory JSON: {message}"),
            Self::UnsupportedSchema { found } => write!(
                f,
                "unsupported inventory schema {found:?}; this build supports {SUPPORTED_SCHEMA:?}"
            ),
            Self::UnsupportedSchemaVersion { found } => write!(
                f,
                "unsupported inventory schema_version {found}; this build supports {SUPPORTED_SCHEMA_VERSION}"
            ),
            Self::UnsupportedScope { found } => write!(
                f,
                "unsupported inventory scope {found:?}; this build supports {LOCAL_ONLY_SCOPE:?}"
            ),
            Self::NodeCountMismatch { declared, actual } => {
                write!(f, "node_count is {declared} but {} nodes were reported", actual)
            }
            Self::TooManyLocalNodes { count } => write!(
                f,
                "scope {LOCAL_ONLY_SCOPE:?} must describe at most one local node, found {count}"
            ),
            Self::EmptyNodeId { index } => {
                write!(f, "node at index {index} has an empty node_id")
            }
            Self::DuplicateNodeId { id } => {
                write!(f, "node_id {id:?} appears more than once")
            }
        }
    }
}

impl std::error::Error for InventoryError {}

/// Parse and validate an inventory document.
pub fn parse_inventory_json(source: &str) -> Result<DeviceInventory, InventoryError> {
    let inventory: DeviceInventory =
        serde_json::from_str(source).map_err(|err| InventoryError::Json(err.to_string()))?;
    validate_inventory(inventory)
}

/// Validate already-deserialized inventory data.
pub fn validate_inventory(inventory: DeviceInventory) -> Result<DeviceInventory, InventoryError> {
    if inventory.schema != SUPPORTED_SCHEMA {
        return Err(InventoryError::UnsupportedSchema {
            found: inventory.schema,
        });
    }
    if inventory.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(InventoryError::UnsupportedSchemaVersion {
            found: inventory.schema_version,
        });
    }
    if inventory.scope != LOCAL_ONLY_SCOPE {
        return Err(InventoryError::UnsupportedScope {
            found: inventory.scope,
        });
    }
    if inventory.node_count != inventory.nodes.len() {
        return Err(InventoryError::NodeCountMismatch {
            declared: inventory.node_count,
            actual: inventory.nodes.len(),
        });
    }

    let mut seen = HashSet::with_capacity(inventory.nodes.len());
    for (index, node) in inventory.nodes.iter().enumerate() {
        if let Some(id) = &node.node_id {
            if id.trim().is_empty() {
                return Err(InventoryError::EmptyNodeId { index });
            }
            if !seen.insert(id.clone()) {
                return Err(InventoryError::DuplicateNodeId { id: id.clone() });
            }
        }
    }
    if inventory.nodes.len() > 1 {
        return Err(InventoryError::TooManyLocalNodes {
            count: inventory.nodes.len(),
        });
    }
    Ok(inventory)
}

/// A user-facing inventory failure: short summary, optional raw detail and an
/// actionable hint. No error is hidden behind a generic message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub summary: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl LoadError {
    #[must_use]
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            detail: None,
            hint: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = compact_detail(&detail.into());
        if !detail.is_empty() {
            self.detail = Some(detail);
        }
        self
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary)
    }
}

impl std::error::Error for LoadError {}

/// The command used to obtain inventory: `<program> devices --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryCommand {
    program: PathBuf,
    timeout: Duration,
}

impl InventoryCommand {
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Resolve the CLI path, in order: explicit CLI flag, `FASTLINK_BIN`, then
    /// `fastlink` from `PATH`. This function is pure so tests do not mutate the
    /// process environment.
    #[must_use]
    pub fn resolve(cli_program: Option<PathBuf>, env_program: Option<std::ffi::OsString>) -> Self {
        let program = cli_program
            .or_else(|| env_program.map(PathBuf::from))
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PROGRAM));
        Self::new(program)
    }

    /// Resolve using this process's environment.
    #[must_use]
    pub fn from_environment() -> Self {
        Self::resolve(None, std::env::var_os(PROGRAM_ENV))
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Human-readable source shown in the UI and documentation.
    #[must_use]
    pub fn command_line(&self) -> String {
        format!("{} devices --json", self.program.display())
    }

    /// Invoke the inventory command. This is the only place the shell launches
    /// a process; it never opens sockets, scans the network or reads device
    /// state itself.
    ///
    /// On Linux/Unix, stdout/stderr are nonblocking pipes drained by this same
    /// worker loop. No reader threads are spawned, so there is nothing to leak.
    /// The byte count is enforced as data is read; an inherited writer can only
    /// fill the bounded kernel pipe, never grow disk usage. A grandchild holding
    /// a pipe cannot block the worker because reads never wait for EOF.
    ///
    /// The non-Unix fallback captures to temporary files and enforces the cap by
    /// polling. It is best-effort, not a hard OS-level bound; Linux is the
    /// supported runtime for this milestone.
    pub fn run(&self) -> Result<DeviceInventory, LoadError> {
        let captured = self.invoke_child()?;

        if !captured.status.success() {
            return Err(self.exit_error(captured.status, &captured.stderr));
        }

        let stdout = String::from_utf8_lossy(&captured.stdout);
        if stdout.trim().is_empty() {
            return Err(
                LoadError::new(format!("`{}` produced no JSON", self.command_line()))
                    .with_hint(
                        "The inventory command must print the versioned JSON document to stdout.",
                    )
                    .with_detail(captured.stderr.as_detail_text()),
            );
        }

        parse_inventory_json(&stdout).map_err(|err| {
            let mut detail = err.to_string();
            if !captured.stderr.bytes.is_empty() {
                detail.push_str("\nstderr: ");
                detail.push_str(&captured.stderr.as_detail_text());
            }
            LoadError::new(format!(
                "`{}` did not return a {SUPPORTED_SCHEMA} inventory",
                self.command_line()
            ))
            .with_detail(detail)
            .with_hint(
                "Check the JSON contract in docs/GUI.md and make sure the CLI and GUI schema versions match.",
            )
        })
    }

    #[cfg(unix)]
    fn invoke_child(&self) -> Result<CapturedOutput, LoadError> {
        let child = Command::new(&self.program)
            .args(["devices", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| self.spawn_error(&err))?;

        capture_child_unix(child, self.timeout).map_err(|err| self.wait_error(err))
    }

    #[cfg(not(unix))]
    fn invoke_child(&self) -> Result<CapturedOutput, LoadError> {
        let mut captures = CaptureFiles::create()
            .map_err(|err| self.capture_error("create output capture", err))?;
        let stdout = captures
            .stdout_stdio()
            .map_err(|err| self.capture_error("prepare stdout capture", err))?;
        let stderr = captures
            .stderr_stdio()
            .map_err(|err| self.capture_error("prepare stderr capture", err))?;

        let child = Command::new(&self.program)
            .args(["devices", "--json"])
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|err| self.spawn_error(&err))?;

        let status = wait_with_timeout_file(child, self.timeout, &captures)
            .map_err(|err| self.wait_error(err))?;

        if let Some((stream, observed)) = captures
            .oversized()
            .map_err(|err| self.capture_error("inspect output capture", err))?
        {
            return Err(self.capture_limit_error(stream, observed));
        }

        let stderr = captures
            .read_stderr_detail()
            .map_err(|err| self.capture_error("read stderr capture", err))?;
        if !status.success() {
            return Ok(CapturedOutput {
                status,
                stdout: Vec::new(),
                stderr,
            });
        }

        let (stdout, stdout_total, stdout_truncated) = captures
            .read_stdout()
            .map_err(|err| self.capture_error("read stdout capture", err))?;
        if stdout_truncated {
            return Err(self.capture_limit_error("stdout", stdout_total));
        }

        Ok(CapturedOutput {
            status,
            stdout,
            stderr,
        })
    }

    fn wait_error(&self, err: WaitError) -> LoadError {
        match err {
            WaitError::Timeout => LoadError::new(format!(
                "`{}` did not finish within {} s",
                self.command_line(),
                self.timeout.as_secs_f32()
            ))
            .with_hint(
                "The GUI refuses to hang forever. Check that the FASTLINK CLI is not waiting on input or on a network resource.",
            ),
            WaitError::CaptureLimit { stream, observed } => {
                self.capture_limit_error(stream, observed)
            }
            WaitError::Io(err) => LoadError::new(format!(
                "could not read output from `{}`: {err}",
                self.command_line()
            ))
            .with_hint("Run the CLI directly to inspect the failure."),
        }
    }

    #[cfg(not(unix))]
    fn capture_error(&self, action: &str, err: std::io::Error) -> LoadError {
        LoadError::new(format!(
            "could not {action} for `{}`: {err}",
            self.command_line()
        ))
        .with_hint("Check that the system temporary directory is writable and has space.")
    }

    fn capture_limit_error(&self, stream: &str, observed: u64) -> LoadError {
        LoadError::new(format!(
            "`{}` exceeded the {MAX_CAPTURE_BYTES} byte {stream} capture limit",
            self.command_line()
        ))
        .with_detail(format!(
            "Observed at least {observed} bytes in the {stream} capture."
        ))
        .with_hint(
            "A FASTLINK inventory should be small. Run the CLI directly and look for debug logging or runaway diagnostics.",
        )
    }

    fn spawn_error(&self, err: &std::io::Error) -> LoadError {
        let error = LoadError::new(format!("could not start `{}`: {err}", self.command_line()));
        if err.kind() == std::io::ErrorKind::NotFound {
            error
                .with_hint(format!(
                    "Build the CLI with `cargo build`, then pass --fastlink-bin /path/to/fastlink or set {PROGRAM_ENV}."
                ))
                .with_detail(format!("Looked for program: {}", self.program.display()))
        } else {
            error.with_hint("Check that the FASTLINK CLI path is executable by this user.")
        }
    }

    fn exit_error(&self, status: ExitStatus, stderr: &StderrDetail) -> LoadError {
        let stderr_text = String::from_utf8_lossy(&stderr.bytes);
        let mut error = LoadError::new(format!("`{}` failed ({status})", self.command_line()))
            .with_detail(stderr.as_detail_text());

        let lower = stderr_text.to_lowercase();
        let missing_subcommand = lower.contains("unrecognized subcommand")
            || lower.contains("unexpected argument")
            || lower.contains("unknown subcommand")
            || (lower.contains("usage") && lower.contains("devices"));
        if missing_subcommand {
            error = error.with_hint(
                "This FASTLINK binary may not implement `devices --json` yet. Point --fastlink-bin at an updated build.",
            );
        } else {
            error = error.with_hint(
                "Run the same command in a terminal to see the full CLI error, or select another binary.",
            );
        }
        error
    }
}

#[derive(Debug)]
struct CapturedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: StderrDetail,
}

/// Retained stderr bytes plus the total number of bytes the capture read.
/// `bytes` is bounded by [`MAX_STDERR_DETAIL_BYTES`]; the rendered detail is
/// further compacted by [`compact_detail`].
#[derive(Debug)]
struct StderrDetail {
    bytes: Vec<u8>,
    total: u64,
}

impl StderrDetail {
    fn as_detail_text(&self) -> String {
        if self.bytes.is_empty() && self.total == 0 {
            return String::new();
        }
        let text = String::from_utf8_lossy(&self.bytes);
        let retained_chars = text.chars().count();
        format!(
            "[stderr: {} bytes captured; {} bytes retained ({} chars); UI detail cap {} characters / {} lines]\n{}",
            self.total,
            self.bytes.len(),
            retained_chars,
            UI_DETAIL_CHARS,
            UI_DETAIL_LINES,
            text
        )
    }
}

#[derive(Debug)]
enum WaitError {
    Timeout,
    CaptureLimit { stream: &'static str, observed: u64 },
    Io(std::io::Error),
}

impl From<std::io::Error> for WaitError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// A nonblocking pipe reader owned by the waiting worker. There are no reader
/// threads; a grandchild holding the write end cannot block this reader.
#[cfg(unix)]
struct PipeReader<H: AsRawFd> {
    fd: RawFd,
    handle: Option<H>,
    retained: Vec<u8>,
    retained_limit: usize,
    total: u64,
    cap: u64,
    eof: bool,
    stream: &'static str,
}

#[cfg(unix)]
impl<H: AsRawFd> PipeReader<H> {
    fn new(
        handle: H,
        stream: &'static str,
        retained_limit: usize,
        cap: u64,
    ) -> std::io::Result<Self> {
        let fd = handle.as_raw_fd();
        set_nonblocking(fd)?;
        Ok(Self {
            fd,
            handle: Some(handle),
            retained: Vec::new(),
            retained_limit,
            total: 0,
            cap,
            eof: false,
            stream,
        })
    }

    fn pump(&mut self, deadline: Instant) -> std::io::Result<()> {
        if self.eof {
            return Ok(());
        }
        let mut buffer = [0u8; 8192];
        loop {
            if Instant::now() >= deadline || self.is_over() {
                return Ok(());
            }
            // SAFETY: `fd` is an open, nonblocking pipe read end owned by
            // `handle`; `buffer` is valid for writes of its full length.
            let read = unsafe {
                libc::read(
                    self.fd,
                    buffer.as_mut_ptr().cast::<libc::c_void>(),
                    buffer.len(),
                )
            };
            if read > 0 {
                self.account(&buffer[..read as usize]);
                if self.is_over() {
                    return Ok(());
                }
            } else if read == 0 {
                self.eof = true;
                self.handle.take();
                return Ok(());
            } else {
                let err = std::io::Error::last_os_error();
                match err.kind() {
                    std::io::ErrorKind::WouldBlock => return Ok(()),
                    std::io::ErrorKind::Interrupted => continue,
                    _ => return Err(err),
                }
            }
        }
    }

    fn account(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        let room = self.retained_limit.saturating_sub(self.retained.len());
        if room > 0 {
            self.retained
                .extend_from_slice(&chunk[..room.min(chunk.len())]);
        }
    }

    fn is_over(&self) -> bool {
        self.total > self.cap
    }
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: `fd` is an open pipe descriptor owned by the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same descriptor; F_SETFL with O_NONBLOCK is a valid flag update.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn capture_child_unix(child: Child, timeout: Duration) -> Result<CapturedOutput, WaitError> {
    capture_child_unix_with(child, timeout, POLL_INTERVAL, MAX_CAPTURE_BYTES)
}

#[cfg(unix)]
fn capture_child_unix_with(
    mut child: Child,
    timeout: Duration,
    poll_interval: Duration,
    max_capture_bytes: u64,
) -> Result<CapturedOutput, WaitError> {
    let stdout_handle = match child.stdout.take() {
        Some(handle) => handle,
        None => {
            stop_child(&mut child);
            return Err(WaitError::Io(std::io::Error::other(
                "stdout pipe was not captured",
            )));
        }
    };
    let stderr_handle = match child.stderr.take() {
        Some(handle) => handle,
        None => {
            stop_child(&mut child);
            return Err(WaitError::Io(std::io::Error::other(
                "stderr pipe was not captured",
            )));
        }
    };

    let stdout_retain = max_capture_bytes.min(MAX_CAPTURE_BYTES) as usize;
    let mut stdout = match PipeReader::<ChildStdout>::new(
        stdout_handle,
        "stdout",
        stdout_retain,
        max_capture_bytes,
    ) {
        Ok(reader) => reader,
        Err(err) => {
            stop_child(&mut child);
            return Err(WaitError::Io(err));
        }
    };
    let stderr_retain = (MAX_STDERR_DETAIL_BYTES as u64).min(max_capture_bytes) as usize;
    let mut stderr = match PipeReader::<ChildStderr>::new(
        stderr_handle,
        "stderr",
        stderr_retain,
        max_capture_bytes,
    ) {
        Ok(reader) => reader,
        Err(err) => {
            stop_child(&mut child);
            return Err(WaitError::Io(err));
        }
    };

    let started = Instant::now();
    let overall_deadline = started + timeout;
    let status = loop {
        if let Err(err) = pump_pair(&mut stdout, &mut stderr, overall_deadline) {
            stop_child(&mut child);
            return Err(WaitError::Io(err));
        }
        if let Some(limit) = overflow_pair(&stdout, &stderr) {
            stop_child(&mut child);
            return Err(limit);
        }

        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    stop_child(&mut child);
                    return Err(WaitError::Timeout);
                }
                thread::sleep(poll_interval);
            }
            Err(err) => {
                stop_child(&mut child);
                return Err(WaitError::Io(err));
            }
        }
    };

    // The owned child has exited. It gets its own short bounded drain grace,
    // even if the worker observed the exit after the overall deadline. Skipping
    // buffered output here would misreport completed JSON as empty/malformed.
    // A still-running child is still killed at the overall deadline above.
    let drain_deadline = Instant::now() + POST_EXIT_DRAIN;
    if let Err(err) = pump_pair(&mut stdout, &mut stderr, drain_deadline) {
        return Err(WaitError::Io(err));
    }
    if let Some(limit) = overflow_pair(&stdout, &stderr) {
        return Err(limit);
    }

    Ok(CapturedOutput {
        status,
        stdout: stdout.retained,
        stderr: StderrDetail {
            bytes: stderr.retained,
            total: stderr.total,
        },
    })
}

#[cfg(unix)]
fn pump_pair<A: AsRawFd, B: AsRawFd>(
    stdout: &mut PipeReader<A>,
    stderr: &mut PipeReader<B>,
    deadline: Instant,
) -> std::io::Result<()> {
    stdout.pump(deadline)?;
    stderr.pump(deadline)
}

#[cfg(unix)]
fn overflow_pair<A: AsRawFd, B: AsRawFd>(
    stdout: &PipeReader<A>,
    stderr: &PipeReader<B>,
) -> Option<WaitError> {
    if stdout.is_over() {
        Some(WaitError::CaptureLimit {
            stream: stdout.stream,
            observed: stdout.total,
        })
    } else if stderr.is_over() {
        Some(WaitError::CaptureLimit {
            stream: stderr.stream,
            observed: stderr.total,
        })
    } else {
        None
    }
}

/// Non-Unix fallback: bounded temporary-file capture with polling.
///
/// This path cannot provide the Linux reader-enforced hard bound. It is kept
/// compile-safe for other targets and documented as best-effort.
#[cfg(not(unix))]
#[derive(Debug)]
struct CaptureFiles {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout: Option<File>,
    stderr: Option<File>,
}

#[cfg(not(unix))]
impl CaptureFiles {
    fn create() -> std::io::Result<Self> {
        let mut last_error = None;
        for _ in 0..MAX_CAPTURE_CREATE_ATTEMPTS {
            let id = NEXT_CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let base = format!("fastlink-gui-inventory-{}-{nanos}-{id}", std::process::id());
            let stdout_path = std::env::temp_dir().join(format!("{base}.stdout"));
            let stderr_path = std::env::temp_dir().join(format!("{base}.stderr"));

            let stdout = match open_capture_file(&stdout_path) {
                Ok(file) => file,
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_error = Some(err);
                    continue;
                }
                Err(err) => return Err(err),
            };
            let stderr = match open_capture_file(&stderr_path) {
                Ok(file) => file,
                Err(err) => {
                    drop(stdout);
                    let _ = fs::remove_file(&stdout_path);
                    if err.kind() == std::io::ErrorKind::AlreadyExists {
                        last_error = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            };

            return Ok(Self {
                stdout_path,
                stderr_path,
                stdout: Some(stdout),
                stderr: Some(stderr),
            });
        }
        Err(last_error
            .unwrap_or_else(|| std::io::Error::other("could not create a unique capture file")))
    }

    fn stdout_stdio(&self) -> std::io::Result<Stdio> {
        Ok(Stdio::from(self.stdout_file()?.try_clone()?))
    }

    fn stderr_stdio(&self) -> std::io::Result<Stdio> {
        Ok(Stdio::from(self.stderr_file()?.try_clone()?))
    }

    fn stdout_file(&self) -> std::io::Result<&File> {
        self.stdout
            .as_ref()
            .ok_or_else(|| std::io::Error::other("stdout capture is not open"))
    }

    fn stderr_file(&self) -> std::io::Result<&File> {
        self.stderr
            .as_ref()
            .ok_or_else(|| std::io::Error::other("stderr capture is not open"))
    }

    fn stdout_size(&self) -> std::io::Result<u64> {
        Ok(self.stdout_file()?.metadata()?.len())
    }

    fn stderr_size(&self) -> std::io::Result<u64> {
        Ok(self.stderr_file()?.metadata()?.len())
    }

    fn oversized(&self) -> std::io::Result<Option<(&'static str, u64)>> {
        let stdout = self.stdout_size()?;
        if stdout > MAX_CAPTURE_BYTES {
            return Ok(Some(("stdout", stdout)));
        }
        let stderr = self.stderr_size()?;
        if stderr > MAX_CAPTURE_BYTES {
            return Ok(Some(("stderr", stderr)));
        }
        Ok(None)
    }

    fn read_stdout(&mut self) -> std::io::Result<(Vec<u8>, u64, bool)> {
        let file = self
            .stdout
            .as_mut()
            .ok_or_else(|| std::io::Error::other("stdout capture is not open"))?;
        read_capped(file, MAX_CAPTURE_BYTES)
    }

    fn read_stderr_detail(&mut self) -> std::io::Result<StderrDetail> {
        let file = self
            .stderr
            .as_mut()
            .ok_or_else(|| std::io::Error::other("stderr capture is not open"))?;
        let (bytes, total, _truncated) = read_capped(file, MAX_STDERR_DETAIL_BYTES as u64)?;
        Ok(StderrDetail { bytes, total })
    }
}

#[cfg(not(unix))]
impl Drop for CaptureFiles {
    fn drop(&mut self) {
        drop(self.stdout.take());
        drop(self.stderr.take());
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
    }
}

#[cfg(not(unix))]
fn wait_with_timeout_file(
    mut child: Child,
    timeout: Duration,
    captures: &CaptureFiles,
) -> Result<ExitStatus, WaitError> {
    let started = Instant::now();
    loop {
        match captures.oversized() {
            Ok(Some((stream, observed))) => {
                stop_child(&mut child);
                return Err(WaitError::CaptureLimit { stream, observed });
            }
            Ok(None) => {}
            Err(err) => {
                stop_child(&mut child);
                return Err(WaitError::Io(err));
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some((stream, observed)) = captures.oversized()? {
                    return Err(WaitError::CaptureLimit { stream, observed });
                }
                return Ok(status);
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    stop_child(&mut child);
                    return Err(WaitError::Timeout);
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(err) => {
                stop_child(&mut child);
                return Err(WaitError::Io(err));
            }
        }
    }
}

#[cfg(not(unix))]
fn read_capped(file: &mut File, limit: u64) -> std::io::Result<(Vec<u8>, u64, bool)> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    let total = file.metadata()?.len();
    let truncated = bytes.len() as u64 > limit || total > limit;
    if bytes.len() as u64 > limit {
        bytes.truncate(limit as usize);
    }
    let observed = total.max(bytes.len() as u64);
    Ok((bytes, observed, truncated))
}

#[cfg(not(unix))]
fn open_capture_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    options.open(path)
}

/// Keep stderr useful but bounded: first lines, first characters, no control
/// characters beyond newlines.
fn compact_detail(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let sanitized: String = trimmed
        .chars()
        .filter(|ch| *ch == '\n' || *ch == '\t' || !ch.is_control())
        .collect();
    let all_lines: Vec<&str> = sanitized.lines().collect();
    let line_truncated = all_lines.len() > UI_DETAIL_LINES;
    let kept_lines = if line_truncated {
        &all_lines[..UI_DETAIL_LINES - 1]
    } else {
        &all_lines[..]
    };
    let mut result = kept_lines.join("\n");
    if line_truncated {
        result.push_str("\n…");
    }
    if result.chars().count() > UI_DETAIL_CHARS {
        result = result.chars().take(UI_DETAIL_CHARS - 1).collect();
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../testdata/devices-v1.json");

    #[test]
    fn parses_the_backend_fixture_exactly() {
        let inventory = parse_inventory_json(FIXTURE).expect("backend fixture must parse");
        assert_eq!(inventory.schema, SUPPORTED_SCHEMA);
        assert_eq!(inventory.schema_version, SUPPORTED_SCHEMA_VERSION);
        assert_eq!(inventory.scope, LOCAL_ONLY_SCOPE);
        assert_eq!(
            inventory.discovery_mode.as_deref(),
            Some("passive_host_evidence")
        );
        assert_eq!(inventory.node_count, 1);
        assert_eq!(inventory.nodes.len(), 1);
        assert_eq!(inventory.local_node_index(), Some(0));

        let node = inventory.local_node().expect("sole node is local");
        assert_eq!(node.opaque_id(), Some("0123456789abcdef0123456789abcdef"));
        assert_ne!(node.node_id.as_deref(), Some("local"));
        assert_eq!(node.node_id_source.as_deref(), Some("machine-id"));
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
        assert_eq!(node.gpus[0].name.as_deref(), Some("Example integrated GPU"));
        assert_eq!(node.gpus[0].driver.as_deref(), Some("example-driver"));
        assert_eq!(node.gpus[0].vendor_id.as_deref(), Some("0x0000"));
        assert_eq!(node.gpus[0].pci_device_id.as_deref(), Some("0x0001"));
        assert_eq!(node.gpus[0].vram_total_bytes, Some(536_870_912));
        assert_eq!(node.gpus[0].vram_free_bytes, Some(520_122_368));

        assert_eq!(node.capabilities.capacity_bytes, None);
        assert_eq!(node.capabilities.throughput_bps, None);
        assert_eq!(
            node.capabilities.network_connectivity.as_deref(),
            Some("unknown")
        );
        assert!(node.capabilities.remote_peers.is_empty());
        assert_eq!(
            node.unknown_fields,
            [
                "capabilities.capacity_bytes",
                "capabilities.throughput_bps",
                "capabilities.network_connectivity"
            ]
        );
    }

    #[test]
    fn keeps_null_optional_values_unknown() {
        let json = r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "discovery_mode": null,
            "node_count": 1,
            "nodes": [{
                "node_id": null,
                "node_id_source": null,
                "hostname": null,
                "os": null,
                "os_release": null,
                "arch": null,
                "logical_cpu_count": null,
                "ram_total_bytes": null,
                "ram_available_bytes": null,
                "ram_free_bytes": null,
                "gpus": [],
                "capabilities": {
                    "capacity_bytes": null,
                    "throughput_bps": null,
                    "network_connectivity": null,
                    "remote_peers": []
                },
                "unknown_fields": ["node_id", "hostname"]
            }]
        }"#;
        let inventory = parse_inventory_json(json).expect("null optionals are allowed");
        let node = inventory.local_node().expect("sole node is local");
        assert_eq!(inventory.discovery_mode, None);
        assert_eq!(node.node_id, None);
        assert_eq!(node.hostname, None);
        assert_eq!(node.os, None);
        assert_eq!(node.arch, None);
        assert_eq!(node.logical_cpu_count, None);
        assert_eq!(node.ram_total_bytes, None);
        assert_eq!(node.ram_available_bytes, None);
        assert_eq!(node.ram_free_bytes, None);
        assert_eq!(node.capabilities.capacity_bytes, None);
        assert_eq!(node.capabilities.throughput_bps, None);
        assert_eq!(node.capabilities.network_connectivity, None);
        assert_eq!(inventory.local_node_id(), None);
    }

    #[test]
    fn accepts_future_additive_fields() {
        let json = r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "discovery_mode": "passive_host_evidence",
            "node_count": 1,
            "future_top_level": true,
            "nodes": [{
                "node_id": "abc",
                "hostname": "future-host",
                "gpus": [{"card": "card0", "future_gpu_field": 7}],
                "capabilities": {
                    "capacity_bytes": 1,
                    "throughput_bps": 2,
                    "network_connectivity": "unknown",
                    "remote_peers": []
                },
                "unknown_fields": [],
                "future_node_field": {"x": 1}
            }]
        }"#;
        assert!(parse_inventory_json(json).is_ok());
    }

    #[test]
    fn rejects_malformed_json() {
        let err = parse_inventory_json("{ not json").expect_err("must reject");
        assert!(matches!(err, InventoryError::Json(_)), "{err:?}");
    }

    #[test]
    fn rejects_wrong_schema_version_scope_and_count() {
        let wrong_schema = FIXTURE.replace(SUPPORTED_SCHEMA, "fastlink.devices.v0");
        assert_eq!(
            parse_inventory_json(&wrong_schema).expect_err("must reject"),
            InventoryError::UnsupportedSchema {
                found: "fastlink.devices.v0".to_owned()
            }
        );

        let wrong_version = FIXTURE.replace("\"schema_version\":1", "\"schema_version\":2");
        assert_eq!(
            parse_inventory_json(&wrong_version).expect_err("must reject"),
            InventoryError::UnsupportedSchemaVersion { found: 2 }
        );
        assert!(parse_inventory_json(&wrong_version)
            .unwrap_err()
            .to_string()
            .contains("schema_version 2"));

        let wrong_scope = FIXTURE.replace("\"local_only\"", "\"lan\"");
        assert_eq!(
            parse_inventory_json(&wrong_scope).expect_err("must reject"),
            InventoryError::UnsupportedScope {
                found: "lan".to_owned()
            }
        );

        let wrong_count = FIXTURE.replace("\"node_count\":1", "\"node_count\":2");
        assert_eq!(
            parse_inventory_json(&wrong_count).expect_err("must reject"),
            InventoryError::NodeCountMismatch {
                declared: 2,
                actual: 1
            }
        );
    }

    #[test]
    fn rejects_multiple_nodes_in_local_only_scope() {
        let json = r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "discovery_mode": "passive_host_evidence",
            "node_count": 2,
            "nodes": [
                {"node_id": "a", "capabilities": {"remote_peers": []}},
                {"node_id": "b", "capabilities": {"remote_peers": []}}
            ]
        }"#;
        assert_eq!(
            parse_inventory_json(json).expect_err("must reject"),
            InventoryError::TooManyLocalNodes { count: 2 }
        );
    }

    #[test]
    fn rejects_empty_and_duplicate_node_ids() {
        let empty = r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "node_count": 1,
            "nodes": [{"node_id": "  ", "capabilities": {}}]
        }"#;
        assert_eq!(
            parse_inventory_json(empty).expect_err("must reject"),
            InventoryError::EmptyNodeId { index: 0 }
        );

        let duplicate = r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "node_count": 2,
            "nodes": [
                {"node_id": "same", "capabilities": {}},
                {"node_id": "same", "capabilities": {}}
            ]
        }"#;
        assert_eq!(
            parse_inventory_json(duplicate).expect_err("must reject"),
            InventoryError::DuplicateNodeId {
                id: "same".to_owned()
            }
        );
    }

    #[test]
    fn empty_nodes_are_a_valid_empty_local_inventory() {
        let json = r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "discovery_mode": "passive_host_evidence",
            "node_count": 0,
            "nodes": []
        }"#;
        let inventory = parse_inventory_json(json).expect("empty inventory is valid");
        assert_eq!(inventory.node_count, 0);
        assert!(inventory.nodes.is_empty());
        assert!(inventory.local_node().is_none());
        assert!(inventory.local_node_id().is_none());
    }

    #[test]
    fn rejects_missing_structural_fields() {
        let missing_capabilities = r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "node_count": 1,
            "nodes": [{"node_id": "a"}]
        }"#;
        assert!(matches!(
            parse_inventory_json(missing_capabilities),
            Err(InventoryError::Json(_))
        ));

        let missing_node_count = r#"{
            "schema": "fastlink.devices.v1",
            "schema_version": 1,
            "scope": "local_only",
            "nodes": []
        }"#;
        assert!(matches!(
            parse_inventory_json(missing_node_count),
            Err(InventoryError::Json(_))
        ));
    }

    #[test]
    fn resolves_cli_before_environment_before_default() {
        let cli = PathBuf::from("/opt/fastlink-cli");
        let env = std::ffi::OsString::from("/opt/fastlink-env");
        assert_eq!(
            InventoryCommand::resolve(Some(cli.clone()), Some(env.clone())).program(),
            cli.as_path()
        );
        assert_eq!(
            InventoryCommand::resolve(None, Some(env)).program(),
            Path::new("/opt/fastlink-env")
        );
        assert_eq!(
            InventoryCommand::resolve(None, None).program(),
            Path::new(DEFAULT_PROGRAM)
        );
        assert_eq!(
            InventoryCommand::resolve(None, Some(std::ffi::OsString::new())).program(),
            Path::new(DEFAULT_PROGRAM)
        );
    }

    #[test]
    fn command_line_is_explicit() {
        let command = InventoryCommand::new("/tmp/fastlink");
        assert_eq!(command.command_line(), "/tmp/fastlink devices --json");
    }

    #[test]
    fn detail_is_bounded_and_control_free() {
        let raw = format!("line\u{7}\n{}", "x\n".repeat(50));
        let detail = compact_detail(&raw);
        assert!(!detail.contains('\u{7}'));
        assert!(detail.lines().count() <= UI_DETAIL_LINES);
        assert!(detail.chars().count() <= UI_DETAIL_CHARS);
        assert!(detail.ends_with('…'));
    }

    #[cfg(unix)]
    fn spawn_capture_child(script: &str) -> Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn capture test child")
    }

    #[cfg(unix)]
    #[test]
    fn completed_child_after_expired_deadline_is_still_drained() {
        let script = format!(
            "sleep 0.05
cat <<'JSON'
{FIXTURE}JSON
"
        );
        let child = spawn_capture_child(&script);

        // The poll interval is much longer than the timeout. The child finishes
        // during that sleep, so the worker observes the exit only after the
        // overall deadline has expired.
        let captured = capture_child_unix_with(
            child,
            Duration::from_millis(20),
            Duration::from_millis(500),
            MAX_CAPTURE_BYTES,
        )
        .expect("completed child must be captured");

        assert!(captured.status.success());
        let inventory = parse_inventory_json(&String::from_utf8_lossy(&captured.stdout))
            .expect("buffered valid JSON must not be discarded as empty");
        assert_eq!(inventory.nodes.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn already_finished_child_is_drained_with_zero_timeout() {
        let script = format!(
            "cat <<'JSON'
{FIXTURE}JSON
"
        );
        let child = spawn_capture_child(&script);
        thread::sleep(Duration::from_millis(100));

        let captured =
            capture_child_unix_with(child, Duration::ZERO, POLL_INTERVAL, MAX_CAPTURE_BYTES)
                .expect("already-finished child must be captured at timeout=0");

        assert!(captured.status.success());
        let inventory = parse_inventory_json(&String::from_utf8_lossy(&captured.stdout))
            .expect("zero-timeout drain must parse completed output");
        assert_eq!(inventory.nodes.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn post_exit_drain_still_enforces_the_capture_cap() {
        let script = "sleep 0.05
head -c 32768 /dev/zero | tr '\\0' 'E'
";
        let child = spawn_capture_child(script);

        let error = capture_child_unix_with(
            child,
            Duration::from_millis(20),
            Duration::from_millis(500),
            16 * 1024,
        )
        .expect_err("buffered output above the cap must still fail");

        match error {
            WaitError::CaptureLimit { stream, observed } => {
                assert_eq!(stream, "stdout");
                assert!(observed > 16 * 1024);
            }
            other => panic!("expected capture limit, got {other:?}"),
        }
    }
}
