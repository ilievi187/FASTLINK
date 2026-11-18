use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Worker {
    login: String,
    ssh_port: u16,
    rpc_port: u16,
}

pub(crate) fn parse_workers(input: &str) -> Result<Vec<Worker>, String> {
    let mut workers = Vec::new();
    for (index, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        let invalid = || {
            format!(
                "Worker line {} must be user@host SSH_PORT RPC_PORT.",
                index + 1
            )
        };
        let [login, ssh_port, rpc_port] = fields.as_slice() else {
            return Err(invalid());
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
            login: (*login).to_owned(),
            ssh_port: port(ssh_port).ok_or_else(invalid)?,
            rpc_port: port(rpc_port).ok_or_else(invalid)?,
        };
        if workers.contains(&worker) {
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

fn ssh_args(worker: &Worker, local_port: u16) -> Vec<String> {
    let mut args: Vec<String> = [
        "-F",
        "/dev/null",
        "-N",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
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
    args.extend([
        "-p".into(),
        worker.ssh_port.to_string(),
        "-L".into(),
        format!("127.0.0.1:{local_port}:127.0.0.1:{}", worker.rpc_port),
        "--".into(),
        worker.login.clone(),
    ]);
    args
}

pub(crate) struct Tunnel {
    child: Child,
    address: SocketAddr,
}

impl Tunnel {
    pub(crate) fn open(worker: &Worker, log: &Path, cancel: &AtomicBool) -> Result<Self, String> {
        let reservation =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|err| err.to_string())?;
        let address = reservation.local_addr().map_err(|err| err.to_string())?;
        let stderr = std::fs::File::create(log).map_err(|err| err.to_string())?;
        if cancel.load(Ordering::Relaxed) {
            return Err("Startup cancelled".into());
        }
        drop(reservation);
        let child = Command::new("/usr/bin/ssh")
            .args(ssh_args(worker, address.port()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .map_err(|err| format!("Could not start OpenSSH: {err}"))?;
        let mut tunnel = Self { child, address };
        let deadline = Instant::now() + Duration::from_secs(12);
        while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
            if !tunnel.is_running().map_err(|err| err.to_string())? {
                let reason = std::fs::read_to_string(log).unwrap_or_default();
                return Err(format!("SSH forwarding failed. {}", reason.trim()));
            }
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                return Ok(tunnel);
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(if cancel.load(Ordering::Relaxed) {
            "Startup cancelled".into()
        } else {
            "Timed out opening the SSH forward.".into()
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
    fn ssh_requires_known_hosts_and_forwards_only_loopback() {
        let workers = parse_workers("alice@worker-1 2222 50052\nbob@worker-2 22 50052").unwrap();
        assert_eq!(workers.len(), 2);
        let args = ssh_args(&workers[0], 12345);
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
