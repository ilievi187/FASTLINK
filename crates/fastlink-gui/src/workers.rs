use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WorkerBackendMode {
    #[default]
    Cpu,
    Gpu,
    Both,
}

impl WorkerBackendMode {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Gpu => "GPU",
            Self::Both => "GPU + CPU",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum WorkerBackend {
    #[default]
    Cpu,
    Gpu(String),
    GpuAndCpu(String),
}

impl WorkerBackend {
    /// Returns the `--device` argument value for upstream `ggml-rpc-server`
    /// (`b12503`: `-d, --device <dev1,dev2,...>`).
    pub(crate) fn device_arg(&self) -> String {
        match self {
            Self::Cpu => "CPU".into(),
            Self::Gpu(dev) => dev.clone(),
            Self::GpuAndCpu(dev) => format!("{dev},CPU"),
        }
    }

    pub(crate) fn uses_cpu(&self) -> bool {
        matches!(self, Self::Cpu | Self::GpuAndCpu(_))
    }
}

/// Validates a single non-CPU GPU backend token against an explicit allow-list
/// of upstream `ggml` accelerator device names (`Vulkan0`, `CUDA0`, `ROCm0`,
/// `HIP0`, `SYCL0`, `OpenCL0`, `Metal`, `Metal0`, etc.).
///
/// Rejects `CPU`, `RPC`, shell metacharacters, flags, whitespace, and unknown
/// tokens so `Gpu` and `GpuAndCpu` never inject arguments or double-count CPU.
pub(crate) fn validate_gpu_backend_token(input: &str) -> Result<String, String> {
    let token = input.trim();
    if token.is_empty() || token.len() > 24 {
        return Err(
            "Enter an allow-listed GPU backend token (for example Vulkan0, CUDA0, or ROCm0)."
                .into(),
        );
    }
    if token.eq_ignore_ascii_case("CPU") {
        return Err(
            "Select the CPU profile instead of entering CPU as a GPU backend token.".into(),
        );
    }
    if token == "Metal" {
        return Ok(token.to_owned());
    }
    const INDEXED_PREFIXES: &[&str] = &["Vulkan", "CUDA", "ROCm", "HIP", "SYCL", "OpenCL", "Metal"];
    for prefix in INDEXED_PREFIXES {
        if let Some(suffix) = token.strip_prefix(prefix) {
            if !suffix.is_empty()
                && suffix.len() <= 2
                && suffix.bytes().all(|byte| byte.is_ascii_digit())
                && (suffix.len() == 1 || !suffix.starts_with('0'))
            {
                return Ok(token.to_owned());
            }
        }
    }
    Err(format!(
        "Unsupported GPU backend token '{token}'. Use an allow-listed device such as Vulkan0, CUDA0, ROCm0, HIP0, SYCL0, OpenCL0, or Metal."
    ))
}

pub(crate) fn build_worker_backend(
    mode: WorkerBackendMode,
    gpu_device: &str,
) -> Result<WorkerBackend, String> {
    match mode {
        WorkerBackendMode::Cpu => Ok(WorkerBackend::Cpu),
        WorkerBackendMode::Gpu => Ok(WorkerBackend::Gpu(validate_gpu_backend_token(gpu_device)?)),
        WorkerBackendMode::Both => Ok(WorkerBackend::GpuAndCpu(validate_gpu_backend_token(
            gpu_device,
        )?)),
    }
}

fn parse_backend_spec(spec: &str) -> Result<WorkerBackend, String> {
    let parts: Vec<&str> = spec.split(',').collect();
    match parts.as_slice() {
        ["CPU"] => Ok(WorkerBackend::Cpu),
        [single] => Ok(WorkerBackend::Gpu(validate_gpu_backend_token(single)?)),
        ["CPU", gpu] | [gpu, "CPU"] => {
            Ok(WorkerBackend::GpuAndCpu(validate_gpu_backend_token(gpu)?))
        }
        _ => Err(format!(
            "Unsupported worker backend '{spec}'. Use CPU, an allow-listed GPU token (e.g. Vulkan0), or <GPU>,CPU."
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Worker {
    login: String,
    ssh_port: u16,
    rpc_port: u16,
    backend: WorkerBackend,
}

impl Worker {
    fn same_endpoint(&self, other: &Self) -> bool {
        self.login == other.login
            && self.ssh_port == other.ssh_port
            && self.rpc_port == other.rpc_port
    }

    #[cfg(test)]
    pub(crate) fn backend(&self) -> &WorkerBackend {
        &self.backend
    }
}

pub(crate) fn discovered_worker(
    user: &str,
    host: Ipv4Addr,
    backend: WorkerBackend,
) -> Result<Worker, String> {
    if user.is_empty()
        || user
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == b'@')
    {
        return Err("Enter a valid SSH username in Chat.".into());
    }
    let mut worker =
        parse_workers(&format!("{user}@{host} 22 50052")).map(|mut workers| workers.remove(0))?;
    worker.backend = backend;
    Ok(worker)
}

pub(crate) fn merge_workers(
    manual: &str,
    added: impl Iterator<Item = Worker>,
) -> Result<Vec<Worker>, String> {
    let mut workers = parse_workers(manual)?;
    for worker in added {
        if !workers
            .iter()
            .any(|existing| existing.same_endpoint(&worker))
        {
            workers.push(worker);
        }
    }
    if workers.len() > 16 {
        return Err("Configure at most 16 worker endpoints.".into());
    }
    Ok(workers)
}

pub(crate) fn parse_workers(input: &str) -> Result<Vec<Worker>, String> {
    let mut workers: Vec<Worker> = Vec::new();
    for (index, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        let invalid = || {
            format!(
                "Worker line {} must be user@host SSH_PORT RPC_PORT [CPU|GPU|GPU,CPU].",
                index + 1
            )
        };
        let (login, ssh_port, rpc_port, backend) = match fields.as_slice() {
            [login, ssh_port, rpc_port] => (*login, *ssh_port, *rpc_port, WorkerBackend::Cpu),
            [login, ssh_port, rpc_port, backend_spec] => {
                let backend = parse_backend_spec(backend_spec)
                    .map_err(|err| format!("Worker line {}: {err}", index + 1))?;
                (*login, *ssh_port, *rpc_port, backend)
            }
            _ => return Err(invalid()),
        };
        let (user, host) = login.split_once('@').ok_or_else(invalid)?;
        let valid_name = |name: &str| {
            name.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        };
        if !valid_name(user) || !valid_name(host) {
            return Err(invalid());
        }
        let port = |value: &str| value.parse::<u16>().ok().filter(|port| *port != 0);
        let worker = Worker {
            login: login.to_owned(),
            ssh_port: port(ssh_port).ok_or_else(invalid)?,
            rpc_port: port(rpc_port).ok_or_else(invalid)?,
            backend,
        };
        if workers
            .iter()
            .any(|existing| existing.same_endpoint(&worker))
        {
            return Err(format!(
                "Worker line {} duplicates an earlier endpoint.",
                index + 1
            ));
        }
        if workers.len() == 16 {
            return Err("Configure at most 16 worker endpoints.".into());
        }
        workers.push(worker);
    }
    Ok(workers)
}

pub(crate) fn ssh_file(input: &str, label: &str) -> Result<Option<String>, String> {
    if input.is_empty() {
        return Ok(None);
    }
    // OpenSSH expands tokens and supports multiple paths in this option.
    // Accept literal absolute paths only.
    if !Path::new(input).is_absolute()
        || !input
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
    {
        return Err(format!("Enter an absolute {label} path using letters, numbers, dots, underscores, and hyphens."));
    }
    Ok(Some(input.into()))
}

#[derive(Default)]
pub(crate) struct SshOptions {
    pub known_hosts: Option<String>,
    pub identity_file: Option<String>,
    pub start_rpc: bool,
}

fn rpc_command(port: u16, backend: &WorkerBackend) -> String {
    let device = backend.device_arg();
    let threads = if backend.uses_cpu() {
        " --threads 2"
    } else {
        ""
    };
    format!(
        "exec /bin/sh -c 'if ! command -v ggml-rpc-server >/dev/null 2>&1; then PATH=\"$HOME/.local/bin:$PATH\"; export PATH; fi; \
        command -v ggml-rpc-server >/dev/null 2>&1 || {{ echo \"ggml-rpc-server is missing on this worker\"; exit 127; }}; \
        exec ggml-rpc-server --host 127.0.0.1 --port {port} --device {device}{threads}'"
    )
}

fn ssh_args(worker: &Worker, local_port: u16, options: &SshOptions) -> Vec<String> {
    let mut args: Vec<String> = [
        "-F",
        "/dev/null",
        "-vv",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "UpdateHostKeys=no",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=2",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ControlMaster=no",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if options.start_rpc {
        // A remote foreground process tied to a PTY gets SIGHUP when the SSH
        // session closes. No detached worker or broad process cleanup.
        args.push("-tt".into());
    } else {
        args.extend(["-N".into(), "-T".into()]);
    }
    if let Some(path) = &options.identity_file {
        args.extend([
            "-i".into(),
            path.clone(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
        ]);
    }
    if let Some(path) = &options.known_hosts {
        args.extend([
            "-o".into(),
            format!("UserKnownHostsFile={path}"),
            "-o".into(),
            "GlobalKnownHostsFile=none".into(),
        ]);
    }
    args.extend([
        "-p".into(),
        worker.ssh_port.to_string(),
        "-L".into(),
        format!("127.0.0.1:{local_port}:127.0.0.1:{}", worker.rpc_port),
        "--".into(),
        worker.login.clone(),
    ]);
    if options.start_rpc {
        args.push(rpc_command(worker.rpc_port, &worker.backend));
    }
    args
}

fn read_ssh_log(log: &Path) -> String {
    let mut content = Vec::new();
    if let Ok(file) = std::fs::File::open(log) {
        let _ = file.take(256 * 1024).read_to_end(&mut content);
    }
    String::from_utf8_lossy(&content).into_owned()
}

fn forwarded_channel_confirmed(log: &str) -> bool {
    let mut forwards = std::collections::HashSet::new();
    for line in log.lines() {
        let Some((prefix, event)) = line.split_once(": ") else {
            continue;
        };
        if !matches!(prefix, "debug1" | "debug2") {
            continue;
        }
        let Some((channel, action)) = event
            .strip_prefix("channel ")
            .and_then(|event| event.split_once(": "))
        else {
            continue;
        };
        if action.starts_with("new direct-tcpip ") {
            forwards.insert(channel);
        } else if action.starts_with("open confirm ") && forwards.contains(channel) {
            return true;
        } else if action.starts_with("free:") {
            forwards.remove(channel);
        }
    }
    false
}

fn ssh_failure(log: &str, ready_failure: &str) -> String {
    if log.contains("Host key verification failed")
        || log.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        return "SSH host-key verification failed. Verify the worker's fingerprint and add its key to known_hosts.".into();
    }
    if log.contains("Permission denied (") {
        return "SSH authentication failed. Check the username and select a key or load it into ssh-agent.".into();
    }
    if log.contains("ggml-rpc-server is missing on this worker") {
        return "Install the matching ggml-rpc-server on the worker and make it available on PATH or in ~/.local/bin.".into();
    }
    if log.contains("PTY allocation request failed") {
        return "SSH could not allocate a terminal for the worker. Allow SSH PTY allocation or start the RPC worker manually.".into();
    }
    log.lines()
        .rev()
        .find(|line| !line.starts_with("debug") && !line.trim().is_empty())
        .map_or_else(
            || ready_failure.to_owned(),
            |line| {
                format!(
                    "{ready_failure} {}",
                    line.chars().take(350).collect::<String>()
                )
            },
        )
}

pub(crate) struct Tunnel {
    child: Child,
    address: SocketAddr,
}

impl Tunnel {
    pub(crate) fn open(
        worker: &Worker,
        log: &Path,
        options: &SshOptions,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        let reservation =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|err| err.to_string())?;
        let address = reservation.local_addr().map_err(|err| err.to_string())?;
        let stderr = std::fs::File::create(log).map_err(|err| err.to_string())?;
        let stdout = stderr.try_clone().map_err(|err| err.to_string())?;
        if cancel.load(Ordering::Relaxed) {
            return Err("Startup cancelled".into());
        }
        drop(reservation);
        let child = Command::new("/usr/bin/ssh")
            .args(ssh_args(worker, address.port(), options))
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|err| format!("Could not start OpenSSH: {err}"))?;
        let mut tunnel = Self { child, address };
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut probe_after = Instant::now();
        while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
            let output = read_ssh_log(log);
            if !tunnel.is_running().map_err(|err| err.to_string())? {
                return Err(ssh_failure(&output, "SSH worker exited."));
            }
            if forwarded_channel_confirmed(&output) {
                return Ok(tunnel);
            }
            if Instant::now() >= probe_after {
                // A local accept alone proves only that SSH is listening.
                // Wait for the remote direct-tcpip channel to confirm its bind.
                let _ = TcpStream::connect_timeout(&address, Duration::from_millis(100));
                probe_after = Instant::now() + Duration::from_millis(400);
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(if cancel.load(Ordering::Relaxed) {
            "Startup cancelled".into()
        } else {
            ssh_failure(
                &read_ssh_log(log),
                "RPC is unreachable through SSH. Check the worker's RPC port and SSH forwarding.",
            )
        })
    }

    pub(crate) fn endpoint(&self) -> String {
        self.address.to_string()
    }

    pub(crate) fn is_running(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_workers_share_the_normal_load_path_without_duplicates() {
        let selected =
            discovered_worker("alice", "192.0.2.5".parse().unwrap(), WorkerBackend::Cpu).unwrap();
        let workers =
            merge_workers("alice@192.0.2.5 22 50052", [selected.clone()].into_iter()).unwrap();
        assert_eq!(workers, vec![selected]);
        assert!(discovered_worker(
            "alice\nbob",
            "192.0.2.5".parse().unwrap(),
            WorkerBackend::Cpu
        )
        .is_err());
    }

    #[test]
    fn worker_backend_defaults_to_cpu_and_supports_opt_in_gpu_and_both_without_cpu_double_count() {
        let workers = parse_workers(
            "alice@w1 22 50052\nbob@w2 22 50052 Vulkan0\ncarol@w3 22 50052 CUDA0,CPU\ndave@w4 22 50052 CPU,ROCm1",
        )
        .unwrap();
        assert_eq!(workers[0].backend(), &WorkerBackend::Cpu);
        assert_eq!(workers[1].backend(), &WorkerBackend::Gpu("Vulkan0".into()));
        assert_eq!(
            workers[2].backend(),
            &WorkerBackend::GpuAndCpu("CUDA0".into())
        );
        assert_eq!(
            workers[3].backend(),
            &WorkerBackend::GpuAndCpu("ROCm1".into())
        );

        let opts = SshOptions {
            start_rpc: true,
            ..SshOptions::default()
        };
        let cpu_cmd = ssh_args(&workers[0], 12345, &opts).pop().unwrap();
        assert!(cpu_cmd.contains(
            "exec ggml-rpc-server --host 127.0.0.1 --port 50052 --device CPU --threads 2'"
        ));

        let gpu_cmd = ssh_args(&workers[1], 12346, &opts).pop().unwrap();
        assert!(gpu_cmd
            .contains("exec ggml-rpc-server --host 127.0.0.1 --port 50052 --device Vulkan0'"));
        assert!(!gpu_cmd.contains("--threads"));

        let both_cmd = ssh_args(&workers[2], 12347, &opts).pop().unwrap();
        assert!(both_cmd.contains(
            "exec ggml-rpc-server --host 127.0.0.1 --port 50052 --device CUDA0,CPU --threads 2'"
        ));
        assert_eq!(both_cmd.matches("CPU").count(), 1);
    }

    #[test]
    fn backend_allow_list_rejects_unsafe_unknown_and_double_counted_tokens() {
        for bad in [
            "",
            "CPU",
            "RPC0",
            "Vulkan",
            "Vulkan00",
            "Vulkan0;id",
            "Vulkan0 --cache",
            "-dVulkan0",
            "CUDA0,Vulkan0",
            "CPU,CPU",
            "Unknown0",
        ] {
            assert!(
                validate_gpu_backend_token(bad).is_err(),
                "token {bad:?} must be rejected"
            );
        }
        assert!(parse_workers("alice@w1 22 50052 CPU,CPU").is_err());
        assert!(parse_workers("alice@w1 22 50052 Vulkan0;id").is_err());
        assert!(parse_workers("alice@w1 22 50052 Vulkan0,CUDA0").is_err());
        assert!(parse_workers("alice@w1 22 50052 CPU\nalice@w1 22 50052 Vulkan0").is_err());
    }

    #[test]
    fn rpc_readiness_requires_the_remote_forward_instead_of_the_ssh_session() {
        let session_only = "debug2: channel 1: open confirm rwindow 0";
        let rejected = "debug1: channel 2: new direct-tcpip [direct-tcpip]\nchannel 2: open failed: connect failed: Connection refused";
        assert!(!forwarded_channel_confirmed(session_only));
        assert!(!forwarded_channel_confirmed(rejected));
        assert!(forwarded_channel_confirmed(&format!("{rejected}\ndebug1: channel 3: new direct-tcpip [direct-tcpip]\ndebug2: channel 3: open confirm rwindow 0")));
    }

    #[test]
    fn managed_rpc_binds_only_remote_loopback_and_uses_a_foreground_cpu_process() {
        let workers = parse_workers("alice@host 22 50052").unwrap();
        let args = ssh_args(
            &workers[0],
            12345,
            &SshOptions {
                start_rpc: true,
                ..SshOptions::default()
            },
        );
        assert!(args.contains(&"-tt".into()));
        assert!(!args.contains(&"-N".into()));
        assert!(args
            .last()
            .unwrap()
            .contains("exec ggml-rpc-server --host 127.0.0.1 --port 50052 --device CPU"));
    }

    #[test]
    fn validates_every_worker_before_starting_and_rejects_ssh_options() {
        for invalid in [
            "-oProxyCommand=test 22 50052",
            "alice@-host 22 50052",
            "alice@host 0 50052",
            "alice@host 22 65536",
            "alice@host 22 50052 extra",
            "alice@host 22 50052\nalice@host 22 50052",
        ] {
            assert!(parse_workers(invalid).is_err());
        }
    }

    #[test]
    fn dedicated_host_keys_override_the_default_trust_stores() {
        assert!(ssh_file("/tmp/trust file", "known-hosts").is_err());
        assert!(ssh_file("/tmp/%h", "known-hosts").is_err());
        let hosts = ssh_file("/tmp/cluster-hosts", "known-hosts").unwrap();
        let workers = parse_workers("alice@worker 22 50052").unwrap();
        let args = ssh_args(
            &workers[0],
            12345,
            &SshOptions {
                known_hosts: hosts,
                ..SshOptions::default()
            },
        );
        assert!(args.contains(&"UserKnownHostsFile=/tmp/cluster-hosts".to_owned()));
        assert!(args.contains(&"GlobalKnownHostsFile=none".to_owned()));
    }

    #[test]
    fn ssh_requires_known_hosts_and_forwards_only_loopback() {
        let workers = parse_workers("alice@worker-1 2222 50052\nbob@worker-2 22 50052").unwrap();
        assert_eq!(workers.len(), 2);
        let args = ssh_args(&workers[0], 12345, &SshOptions::default());
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-L", "127.0.0.1:12345:127.0.0.1:50052"]));
        assert!(args.contains(&"StrictHostKeyChecking=yes".to_owned()));
        assert_eq!(
            &args[args.len() - 4..],
            [
                "-L",
                "127.0.0.1:12345:127.0.0.1:50052",
                "--",
                "alice@worker-1"
            ]
        );
    }
}
