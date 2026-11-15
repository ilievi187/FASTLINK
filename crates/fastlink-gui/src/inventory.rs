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
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
/// Hard per-stream capture limit. An inventory document is expected to be tiny;
/// this bounds memory, disk growth and reader work even for a misbehaving CLI.
pub const MAX_CAPTURE_BYTES: u64 = 1024 * 1024;
/// How much stderr is retained for a diagnostic message.
pub const MAX_STDERR_DETAIL_BYTES: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_DETAIL_CHARS: usize = 900;
const MAX_DETAIL_LINES: usize = 12;
const MAX_CAPTURE_CREATE_ATTEMPTS: usize = 16;
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
    /// stdout/stderr are captured into bounded temporary files instead of pipes.
    /// The kernel drains both streams concurrently, so a chatty CLI cannot fill
    /// a pipe and deadlock. A grandchild inheriting the file handles cannot keep
    /// the worker waiting for EOF either: regular-file reads never wait for all
    /// writers to close. The deadline covers child execution and capture-size
    /// monitoring; reading is capped at [`MAX_CAPTURE_BYTES`].
    pub fn run(&self) -> Result<DeviceInventory, LoadError> {
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

        let status = match wait_with_timeout(child, self.timeout, &captures) {
            Ok(status) => status,
            Err(WaitError::Timeout) => {
                return Err(LoadError::new(format!(
                    "`{}` did not finish within {} s",
                    self.command_line(),
                    self.timeout.as_secs_f32()
                ))
                .with_hint(
                    "The GUI refuses to hang forever. Check that the FASTLINK CLI is not waiting on input or on a network resource.",
                ));
            }
            Err(WaitError::CaptureLimit { stream, observed }) => {
                return Err(self.capture_limit_error(stream, observed));
            }
            Err(WaitError::Io(err)) => {
                return Err(LoadError::new(format!(
                    "could not wait for `{}`: {err}",
                    self.command_line()
                )));
            }
        };

        // A grandchild may still grow a capture after the owned child exits.
        if let Some((stream, observed)) = captures
            .oversized()
            .map_err(|err| self.capture_error("inspect output capture", err))?
        {
            return Err(self.capture_limit_error(stream, observed));
        }

        if !status.success() {
            let stderr = captures
                .read_stderr_detail()
                .map_err(|err| self.capture_error("read stderr capture", err))?;
            return Err(self.exit_error(status, &stderr));
        }

        let (stdout, stdout_total, stdout_truncated) = captures
            .read_stdout()
            .map_err(|err| self.capture_error("read stdout capture", err))?;
        if stdout_truncated {
            return Err(self.capture_limit_error("stdout", stdout_total));
        }
        let stdout = String::from_utf8_lossy(&stdout);
        if stdout.trim().is_empty() {
            let stderr = captures
                .read_stderr_detail()
                .map_err(|err| self.capture_error("read stderr capture", err))?;
            return Err(
                LoadError::new(format!("`{}` produced no JSON", self.command_line()))
                    .with_hint(
                        "The inventory command must print the versioned JSON document to stdout.",
                    )
                    .with_detail(stderr.as_detail_text()),
            );
        }

        parse_inventory_json(&stdout).map_err(|err| {
            let mut detail = err.to_string();
            match captures.read_stderr_detail() {
                Ok(stderr) if !stderr.bytes.is_empty() => {
                    detail.push_str("\nstderr: ");
                    detail.push_str(&stderr.as_detail_text());
                }
                Ok(_) => {}
                Err(read_err) => {
                    detail.push_str(&format!(
                        "\n(could not read stderr capture: {read_err})"
                    ));
                }
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

fn wait_with_timeout(
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

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Bounded regular-file capture for child stdout/stderr.
///
/// `File` handles left open by grandchildren never block reads. Files are
/// removed on drop even when the child failed or timed out.
#[derive(Debug)]
struct CaptureFiles {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout: Option<File>,
    stderr: Option<File>,
}

#[derive(Debug)]
struct StderrDetail {
    bytes: Vec<u8>,
    total: u64,
    truncated: bool,
}

impl StderrDetail {
    fn as_detail_text(&self) -> String {
        let mut text = String::new();
        if self.truncated {
            text.push_str(&format!(
                "(stderr truncated; showing {} of {} bytes)\n",
                self.bytes.len(),
                self.total
            ));
        }
        text.push_str(&String::from_utf8_lossy(&self.bytes));
        text
    }
}

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
        let (bytes, total, truncated) = read_capped(file, MAX_STDERR_DETAIL_BYTES as u64)?;
        Ok(StderrDetail {
            bytes,
            total,
            truncated,
        })
    }
}

impl Drop for CaptureFiles {
    fn drop(&mut self) {
        drop(self.stdout.take());
        drop(self.stderr.take());
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
    }
}

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

fn open_capture_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
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
    let lines: Vec<&str> = sanitized.lines().take(MAX_DETAIL_LINES).collect();
    let mut result = lines.join("\n");
    if sanitized.lines().count() > MAX_DETAIL_LINES {
        result.push_str("\n…");
    }
    if result.chars().count() > MAX_DETAIL_CHARS {
        result = result.chars().take(MAX_DETAIL_CHARS).collect();
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
        assert!(detail.lines().count() <= MAX_DETAIL_LINES + 1);
        assert!(detail.ends_with('…'));
    }
}
