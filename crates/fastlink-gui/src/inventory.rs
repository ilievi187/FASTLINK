//! Reader for the versioned `fastlink devices --json` inventory contract.
//!
//! The GUI must not invent telemetry. Every device value comes from the
//! inventory command; this module only parses, validates and transports it.
//! Missing or `null` optional values stay [`None`] all the way to the UI.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

/// Schema version understood by this build of the GUI.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;
/// Default executable name looked up on `PATH`.
pub const DEFAULT_PROGRAM: &str = "fastlink";
/// Environment variable that overrides the CLI path.
pub const PROGRAM_ENV: &str = "FASTLINK_BIN";
/// Default upper bound for one inventory invocation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_DETAIL_CHARS: usize = 900;
const MAX_DETAIL_LINES: usize = 12;

/// Top-level inventory document.
///
/// `deny_unknown_fields` is intentionally not used: the schema is versioned and
/// future additive fields must not break older shells.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DeviceInventory {
    pub schema_version: u32,
    pub devices: Vec<Device>,
    #[serde(default)]
    pub status: Option<String>,
}

/// One device reported by the inventory command.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Device {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub cpu_logical_cores: Option<u32>,
    #[serde(default)]
    pub ram_total_bytes: Option<u64>,
    #[serde(default)]
    pub ram_available_bytes: Option<u64>,
    /// Array entries have no shape in schema v1; keep them as raw JSON so the
    /// shell never guesses fields that the contract does not define.
    #[serde(default)]
    pub gpu_devices: Option<Vec<Value>>,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub network_interfaces: Option<Vec<Value>>,
}

impl Device {
    /// The inventory contract names the local device `local`.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.id == "local"
    }
}

/// Structural problems in an inventory document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryError {
    Json(String),
    UnsupportedSchemaVersion { found: u32 },
    EmptyDeviceId { index: usize },
    DuplicateDeviceId { id: String },
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid inventory JSON: {message}"),
            Self::UnsupportedSchemaVersion { found } => write!(
                f,
                "unsupported inventory schema_version {found}; this build supports {SUPPORTED_SCHEMA_VERSION}"
            ),
            Self::EmptyDeviceId { index } => {
                write!(f, "device at index {index} has an empty id")
            }
            Self::DuplicateDeviceId { id } => write!(f, "device id {id:?} appears more than once"),
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
    if inventory.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(InventoryError::UnsupportedSchemaVersion {
            found: inventory.schema_version,
        });
    }
    let mut seen = HashSet::with_capacity(inventory.devices.len());
    for (index, device) in inventory.devices.iter().enumerate() {
        let id = device.id.trim();
        if id.is_empty() {
            return Err(InventoryError::EmptyDeviceId { index });
        }
        if !seen.insert(id.to_owned()) {
            return Err(InventoryError::DuplicateDeviceId { id: id.to_owned() });
        }
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
    pub fn run(&self) -> Result<DeviceInventory, LoadError> {
        let child = Command::new(&self.program)
            .args(["devices", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| self.spawn_error(&err))?;

        let output = match wait_with_timeout(child, self.timeout) {
            Ok(output) => output,
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
            Err(WaitError::Io(err)) => {
                return Err(LoadError::new(format!(
                    "could not wait for `{}`: {err}",
                    self.command_line()
                )));
            }
        };

        if !output.status.success() {
            return Err(self.exit_error(output.status, &output.stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Err(
                LoadError::new(format!("`{}` produced no JSON", self.command_line()))
                    .with_hint(
                        "The inventory command must print the versioned JSON document to stdout.",
                    )
                    .with_detail(String::from_utf8_lossy(&output.stderr).into_owned()),
            );
        }

        parse_inventory_json(&stdout).map_err(|err| {
            LoadError::new(format!(
                "`{}` did not return a schema v{SUPPORTED_SCHEMA_VERSION} inventory",
                self.command_line()
            ))
            .with_detail(err.to_string())
            .with_hint(
                "Check the JSON contract in docs/GUI.md and make sure the CLI and GUI schema versions match.",
            )
        })
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

    fn exit_error(&self, status: ExitStatus, stderr: &[u8]) -> LoadError {
        let stderr = String::from_utf8_lossy(stderr);
        let mut error = LoadError::new(format!("`{}` failed ({status})", self.command_line()))
            .with_detail(stderr.clone().into_owned());

        let lower = stderr.to_lowercase();
        let missing_subcommand = lower.contains("unrecognized subcommand")
            || lower.contains("unexpected argument")
            || lower.contains("unknown subcommand")
            || (lower.contains("usage") && lower.contains("devices"));
        if missing_subcommand {
            error = error.with_hint(
                "This FASTLINK binary may not implement `devices --json` yet. Point --fastlink-bin at a build that includes the device-inventory work.",
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
    Io(std::io::Error),
}

fn wait_with_timeout(
    mut child: Child,
    timeout: Duration,
) -> Result<std::process::Output, WaitError> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(WaitError::Timeout);
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WaitError::Io(err));
            }
        }
    }
    child.wait_with_output().map_err(WaitError::Io)
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

    const CONTRACT: &str = r#"{
        "schema_version": 1,
        "devices": [{
            "id": "local",
            "name": "test-host",
            "os": "linux",
            "architecture": "x86_64",
            "cpu_logical_cores": 8,
            "ram_total_bytes": 17179869184,
            "ram_available_bytes": 8589934592,
            "gpu_devices": [],
            "backend": "cpu",
            "network_interfaces": []
        }],
        "status": "local_only"
    }"#;

    #[test]
    fn parses_the_documented_contract() {
        let inventory = parse_inventory_json(CONTRACT).expect("contract must parse");
        assert_eq!(inventory.schema_version, 1);
        assert_eq!(inventory.status.as_deref(), Some("local_only"));
        assert_eq!(inventory.devices.len(), 1);

        let device = &inventory.devices[0];
        assert_eq!(device.id, "local");
        assert!(device.is_local());
        assert_eq!(device.name.as_deref(), Some("test-host"));
        assert_eq!(device.os.as_deref(), Some("linux"));
        assert_eq!(device.architecture.as_deref(), Some("x86_64"));
        assert_eq!(device.cpu_logical_cores, Some(8));
        assert_eq!(device.ram_total_bytes, Some(17_179_869_184));
        assert_eq!(device.ram_available_bytes, Some(8_589_934_592));
        assert_eq!(device.backend.as_deref(), Some("cpu"));
        assert_eq!(device.gpu_devices.as_deref(), Some(&[][..]));
        assert_eq!(device.network_interfaces.as_deref(), Some(&[][..]));
    }

    #[test]
    fn keeps_null_and_missing_optional_values_unknown() {
        let json = r#"{
            "schema_version": 1,
            "devices": [{
                "id": "local",
                "name": null,
                "cpu_logical_cores": null,
                "ram_total_bytes": null,
                "ram_available_bytes": null,
                "gpu_devices": null,
                "backend": null,
                "network_interfaces": null
            }],
            "status": null
        }"#;
        let inventory = parse_inventory_json(json).expect("null optionals are allowed");
        let device = &inventory.devices[0];
        assert_eq!(device.name, None);
        assert_eq!(device.os, None);
        assert_eq!(device.architecture, None);
        assert_eq!(device.cpu_logical_cores, None);
        assert_eq!(device.ram_total_bytes, None);
        assert_eq!(device.ram_available_bytes, None);
        assert_eq!(device.gpu_devices, None);
        assert_eq!(device.backend, None);
        assert_eq!(device.network_interfaces, None);
        assert_eq!(inventory.status, None);
    }

    #[test]
    fn accepts_future_additive_fields() {
        let json = r#"{
            "schema_version": 1,
            "devices": [{"id": "local", "future_field": {"x": 1}}],
            "status": "local_only",
            "future_top_level": true
        }"#;
        assert!(parse_inventory_json(json).is_ok());
    }

    #[test]
    fn rejects_malformed_json() {
        let err = parse_inventory_json("{ not json").expect_err("must reject");
        assert!(matches!(err, InventoryError::Json(_)), "{err:?}");
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let json = r#"{"schema_version": 2, "devices": [], "status": "local_only"}"#;
        let err = parse_inventory_json(json).expect_err("must reject");
        assert_eq!(err, InventoryError::UnsupportedSchemaVersion { found: 2 });
        assert!(err.to_string().contains("schema_version 2"));
    }

    #[test]
    fn rejects_missing_schema_version() {
        let json = r#"{"devices": [], "status": "local_only"}"#;
        assert!(matches!(
            parse_inventory_json(json),
            Err(InventoryError::Json(_))
        ));
    }

    #[test]
    fn rejects_missing_devices() {
        let json = r#"{"schema_version": 1, "status": "local_only"}"#;
        assert!(matches!(
            parse_inventory_json(json),
            Err(InventoryError::Json(_))
        ));
    }

    #[test]
    fn rejects_empty_and_duplicate_device_ids() {
        let empty = r#"{"schema_version": 1, "devices": [{"id": "  "}], "status": "local_only"}"#;
        assert_eq!(
            parse_inventory_json(empty).expect_err("must reject"),
            InventoryError::EmptyDeviceId { index: 0 }
        );

        let duplicate = r#"{
            "schema_version": 1,
            "devices": [{"id": "local"}, {"id": "local"}],
            "status": "local_only"
        }"#;
        assert_eq!(
            parse_inventory_json(duplicate).expect_err("must reject"),
            InventoryError::DuplicateDeviceId {
                id: "local".to_owned()
            }
        );
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
