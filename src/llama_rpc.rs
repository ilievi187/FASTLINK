//! Experimental `llama.cpp` RPC adapter & benchmark harness for FASTLINK.
//!
//! Spawns a loopback `llama-server` connected to one or more `ggml-rpc-server`
//! endpoints, toggles FASTLINK-RPC v4.1 environment flags (`GGML_RPC_FASTLINK_V4`,
//! `GGML_RPC_FASTLINK_PACK`, `GGML_RPC_FASTLINK_RLE`), streams `/completion` SSE
//! tokens to measure TTFT and decode throughput, and guarantees process cleanup
//! via RAII `Drop`.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::{digest, hex, Digest};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlamaRpcConfig {
    pub server_bin: PathBuf,
    pub model_path: PathBuf,
    pub listen_host: String,
    pub listen_port: u16,
    pub rpc_servers: Vec<String>,
    pub n_gpu_layers: u32,
    pub ctx_size: u32,
    pub tensor_split: Option<String>,
    pub fastlink_v4: bool,
    pub fastlink_pack: bool,
    pub fastlink_rle: bool,
    pub log_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FastlinkRpcTelemetry {
    pub handshake_fastlink: bool,
    pub reuse_count: u64,
    pub bytes_cold: u64,
    pub bytes_warm: u64,
    pub logical_ops: u64,
    pub wire_batches: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LlamaCompletionMetrics {
    pub ttft_ms: f64,
    pub total_wall_ms: f64,
    pub prompt_ms: f64,
    pub predicted_ms: f64,
    pub predicted_n: u64,
    pub predicted_per_second: f64,
    pub content: String,
    pub content_digest: Digest,
}

#[derive(Debug)]
pub enum LlamaRpcError {
    NonLoopbackListen(String),
    Io(io::Error),
    StartupTimeout(String),
    HttpError(String),
    ParseError(String),
}

impl fmt::Display for LlamaRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonLoopbackListen(addr) => {
                write!(
                    f,
                    "llama-server listen address must be loopback, got {addr}"
                )
            }
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::StartupTimeout(msg) => write!(f, "llama-server startup timeout: {msg}"),
            Self::HttpError(msg) => write!(f, "HTTP error: {msg}"),
            Self::ParseError(msg) => write!(f, "parse error: {msg}"),
        }
    }
}

impl std::error::Error for LlamaRpcError {}

impl From<io::Error> for LlamaRpcError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Builds the CLI arguments for `llama-server` from `LlamaRpcConfig`.
pub fn build_server_args(cfg: &LlamaRpcConfig) -> Vec<String> {
    let mut args = vec![
        "-m".into(),
        cfg.model_path.to_string_lossy().into_owned(),
        "--host".into(),
        cfg.listen_host.clone(),
        "--port".into(),
        cfg.listen_port.to_string(),
        "-ngl".into(),
        cfg.n_gpu_layers.to_string(),
        "-c".into(),
        cfg.ctx_size.to_string(),
        "--no-webui".into(),
    ];
    if !cfg.rpc_servers.is_empty() {
        args.push("--rpc".into());
        args.push(cfg.rpc_servers.join(","));
    }
    if let Some(split) = &cfg.tensor_split {
        args.push("--tensor-split".into());
        args.push(split.clone());
    }
    args
}

/// Builds the FASTLINK-RPC environment overrides for `llama-server`.
pub fn build_fastlink_env(cfg: &LlamaRpcConfig) -> Vec<(&'static str, &'static str)> {
    let mut envs = Vec::new();
    if cfg.fastlink_v4 {
        envs.push(("GGML_RPC_FASTLINK_V4", "1"));
    }
    if cfg.fastlink_pack {
        envs.push(("GGML_RPC_FASTLINK_PACK", "1"));
    }
    if cfg.fastlink_rle {
        envs.push(("GGML_RPC_FASTLINK_RLE", "1"));
    }
    envs
}

/// RAII guard for a spawned `llama-server` child process.
/// Ensures termination and reaping on drop, error, or panic.
pub struct ManagedLlamaServer {
    child: Child,
    addr: SocketAddr,
    log_path: PathBuf,
}

impl ManagedLlamaServer {
    pub fn spawn(cfg: &LlamaRpcConfig) -> Result<Self, LlamaRpcError> {
        let addr_str = format!("{}:{}", cfg.listen_host, cfg.listen_port);
        let addr: SocketAddr = addr_str
            .parse()
            .map_err(|_| LlamaRpcError::NonLoopbackListen(addr_str.clone()))?;
        if !addr.ip().is_loopback() {
            return Err(LlamaRpcError::NonLoopbackListen(addr_str));
        }

        let log_path = cfg.log_path.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("fastlink-llama-{}.log", cfg.listen_port))
        });
        let log_file = File::create(&log_path)?;
        let log_err = log_file.try_clone()?;

        let mut cmd = Command::new(&cfg.server_bin);
        cmd.args(build_server_args(cfg))
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err));

        if let Some(parent) = cfg.server_bin.parent() {
            let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
            let combined = if existing.is_empty() {
                parent.to_string_lossy().into_owned()
            } else {
                format!("{}:{existing}", parent.display())
            };
            cmd.env("LD_LIBRARY_PATH", combined);
        }

        for (k, v) in build_fastlink_env(cfg) {
            cmd.env(k, v);
        }

        let child = cmd.spawn()?;
        Ok(Self {
            child,
            addr,
            log_path,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub fn wait_ready(&mut self, timeout: Duration) -> Result<(), LlamaRpcError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait()? {
                let tail = fs::read_to_string(&self.log_path).unwrap_or_default();
                return Err(LlamaRpcError::StartupTimeout(format!(
                    "llama-server exited early with {status}: {}",
                    tail.lines().rev().take(10).collect::<Vec<_>>().join(" | ")
                )));
            }
            if let Ok(mut stream) =
                TcpStream::connect_timeout(&self.addr, Duration::from_millis(500))
            {
                let req = format!(
                    "GET /health HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    self.addr
                );
                if stream.write_all(req.as_bytes()).is_ok() {
                    let mut resp = String::new();
                    let _ = stream.read_to_string(&mut resp);
                    if resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200") {
                        return Ok(());
                    }
                }
            }
            thread::sleep(Duration::from_millis(250));
        }
        Err(LlamaRpcError::StartupTimeout(format!(
            "timed out waiting for /health on {}",
            self.addr
        )))
    }

    pub fn read_log(&self) -> io::Result<String> {
        fs::read_to_string(&self.log_path)
    }

    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ManagedLlamaServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn extract_json_number(json: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\":");
    let idx = json.find(&needle)? + needle.len();
    let rest = json[idx..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-' && c != 'e' && c != 'E')
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let idx = json.find(&needle)? + needle.len();
    let mut out = String::new();
    let mut chars = json[idx..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(esc) = chars.next() {
                    match esc {
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        other => out.push(other),
                    }
                }
            }
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

/// Parses a single SSE `data: {...}` JSON line from `llama-server /completion` (stream=true).
pub fn parse_completion_sse_line(
    line: &str,
) -> Option<(Option<String>, Option<LlamaCompletionMetrics>)> {
    let trimmed = line.trim();
    let json = trimmed.strip_prefix("data:")?.trim();
    if json == "[DONE]" {
        return None;
    }
    let chunk_text = extract_json_string(json, "content");
    let is_stop = json.contains("\"stop\":true");
    if is_stop {
        let prompt_ms = extract_json_number(json, "prompt_ms").unwrap_or(0.0);
        let predicted_ms = extract_json_number(json, "predicted_ms").unwrap_or(0.0);
        let predicted_n = extract_json_number(json, "predicted_n").unwrap_or(0.0) as u64;
        let predicted_per_second = extract_json_number(json, "predicted_per_second").unwrap_or(0.0);
        let metrics = LlamaCompletionMetrics {
            ttft_ms: 0.0,
            total_wall_ms: 0.0,
            prompt_ms,
            predicted_ms,
            predicted_n,
            predicted_per_second,
            content: String::new(),
            content_digest: [0u8; 32],
        };
        Some((chunk_text, Some(metrics)))
    } else {
        Some((chunk_text, None))
    }
}

/// Sends a streaming `/completion` request to a running `llama-server` and measures
/// TTFT, total wall time, decode throughput, and BLAKE3 output digest.
pub fn run_streaming_completion(
    addr: SocketAddr,
    prompt: &str,
    n_predict: u32,
    temperature: f32,
    seed: u32,
) -> Result<LlamaCompletionMetrics, LlamaRpcError> {
    let body = format!(
        "{{\"prompt\":{},\"n_predict\":{n_predict},\"temperature\":{temperature},\"seed\":{seed},\"stream\":true}}",
        json_escape(prompt)
    );
    let req = format!(
        "POST /completion HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let started = Instant::now();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    if !status_line.contains("200") {
        return Err(LlamaRpcError::HttpError(status_line.trim().to_string()));
    }
    loop {
        let mut header_line = String::new();
        reader.read_line(&mut header_line)?;
        if header_line == "\r\n" || header_line.is_empty() {
            break;
        }
    }

    let mut ttft_ms: Option<f64> = None;
    let mut full_content = String::new();
    let mut final_metrics: Option<LlamaCompletionMetrics> = None;

    for line_res in reader.lines() {
        let line = line_res?;
        if let Some((maybe_text, maybe_done)) = parse_completion_sse_line(&line) {
            if let Some(text) = maybe_text {
                if !text.is_empty() {
                    if ttft_ms.is_none() {
                        ttft_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
                    }
                    full_content.push_str(&text);
                }
            }
            if let Some(m) = maybe_done {
                final_metrics = Some(m);
                break;
            }
        }
    }

    let total_wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut metrics = final_metrics
        .ok_or_else(|| LlamaRpcError::ParseError("missing final stop:true SSE event".into()))?;
    metrics.ttft_ms = ttft_ms.unwrap_or(total_wall_ms);
    metrics.total_wall_ms = total_wall_ms;
    metrics.content_digest = digest(full_content.as_bytes());
    metrics.content = full_content;
    Ok(metrics)
}

fn extract_u64_after(line: &str, key: &str) -> Option<u64> {
    let idx = line.find(key)? + key.len();
    let rest = line[idx..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse::<u64>().ok()
}

/// Parses `[FASTLINK-RPC-v4]` and `[FASTLINK-RPC-v4.1]` lines from `llama-server` stderr.
pub fn parse_fastlink_telemetry(log: &str) -> FastlinkRpcTelemetry {
    let mut out = FastlinkRpcTelemetry::default();
    for line in log.lines() {
        if line.contains("FASTLINK v4.1 ENABLED") {
            out.handshake_fastlink = true;
        }
        if line.contains("[FASTLINK-RPC-v4] REUSE tensor") {
            out.reuse_count += 1;
        }
        if line.contains("residency summary:") {
            if let Some(v) = extract_u64_after(line, "bytes_cold=") {
                out.bytes_cold = v;
            }
            if let Some(v) = extract_u64_after(line, "bytes_warm=") {
                out.bytes_warm = v;
            }
        }
        if line.contains("transport summary:") {
            if let Some(v) = extract_u64_after(line, "logical_ops=") {
                out.logical_ops = v;
            }
            if let Some(v) = extract_u64_after(line, "wire_batches=") {
                out.wire_batches = v;
            }
        }
    }
    out
}

pub fn format_metrics_json(
    mode: &str,
    metrics: &LlamaCompletionMetrics,
    telemetry: &FastlinkRpcTelemetry,
) -> String {
    format!(
        "{{\"mode\":{},\"ttft_ms\":{:.2},\"total_wall_ms\":{:.2},\"prompt_ms\":{:.2},\"predicted_ms\":{:.2},\"predicted_n\":{},\"predicted_per_second\":{:.2},\"output_blake3\":\"{}\",\"fastlink_enabled\":{},\"reuse_count\":{},\"bytes_cold\":{},\"bytes_warm\":{},\"logical_ops\":{},\"wire_batches\":{}}}",
        json_escape(mode),
        metrics.ttft_ms,
        metrics.total_wall_ms,
        metrics.prompt_ms,
        metrics.predicted_ms,
        metrics.predicted_n,
        metrics.predicted_per_second,
        hex(&metrics.content_digest),
        telemetry.handshake_fastlink,
        telemetry.reuse_count,
        telemetry.bytes_cold,
        telemetry.bytes_warm,
        telemetry.logical_ops,
        telemetry.wire_batches,
    )
}

/// CLI entrypoint for `fastlink rpc-bench`:
/// `fastlink rpc-bench --server PATH --model PATH [--rpc HOST:PORT] [--tensor-split S] [--ngl N] [--ctx N] [--port P] [--n-predict N] [--runs N] [--fastlink]`
pub fn run_rpc_bench_cli(args: &[String]) -> Result<String, LlamaRpcError> {
    let mut server_bin: Option<PathBuf> = None;
    let mut model_path: Option<PathBuf> = None;
    let mut rpc_servers: Vec<String> = Vec::new();
    let mut tensor_split: Option<String> = None;
    let mut n_gpu_layers: u32 = 99;
    let mut ctx_size: u32 = 2048;
    let mut listen_port: u16 = 18891;
    let mut n_predict: u32 = 48;
    let mut runs: usize = 2;
    let mut fastlink_v4 = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--server" if i + 1 < args.len() => {
                server_bin = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--model" if i + 1 < args.len() => {
                model_path = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--rpc" if i + 1 < args.len() => {
                rpc_servers.extend(args[i + 1].split(',').map(str::to_string));
                i += 2;
            }
            "--tensor-split" if i + 1 < args.len() => {
                tensor_split = Some(args[i + 1].clone());
                i += 2;
            }
            "--ngl" if i + 1 < args.len() => {
                n_gpu_layers = args[i + 1]
                    .parse()
                    .map_err(|_| LlamaRpcError::ParseError("invalid --ngl".into()))?;
                i += 2;
            }
            "--ctx" if i + 1 < args.len() => {
                ctx_size = args[i + 1]
                    .parse()
                    .map_err(|_| LlamaRpcError::ParseError("invalid --ctx".into()))?;
                i += 2;
            }
            "--port" if i + 1 < args.len() => {
                listen_port = args[i + 1]
                    .parse()
                    .map_err(|_| LlamaRpcError::ParseError("invalid --port".into()))?;
                i += 2;
            }
            "--n-predict" if i + 1 < args.len() => {
                n_predict = args[i + 1]
                    .parse()
                    .map_err(|_| LlamaRpcError::ParseError("invalid --n-predict".into()))?;
                i += 2;
            }
            "--runs" if i + 1 < args.len() => {
                runs = args[i + 1]
                    .parse()
                    .map_err(|_| LlamaRpcError::ParseError("invalid --runs".into()))?;
                i += 2;
            }
            "--fastlink" => {
                fastlink_v4 = true;
                i += 1;
            }
            other => {
                return Err(LlamaRpcError::ParseError(format!(
                    "unknown or incomplete flag: {other}"
                )));
            }
        }
    }

    let cfg = LlamaRpcConfig {
        server_bin: server_bin
            .ok_or_else(|| LlamaRpcError::ParseError("missing --server PATH".into()))?,
        model_path: model_path
            .ok_or_else(|| LlamaRpcError::ParseError("missing --model PATH".into()))?,
        listen_host: "127.0.0.1".into(),
        listen_port,
        rpc_servers,
        n_gpu_layers,
        ctx_size,
        tensor_split,
        fastlink_v4,
        fastlink_pack: fastlink_v4,
        fastlink_rle: fastlink_v4,
        log_path: None,
    };

    let mut srv = ManagedLlamaServer::spawn(&cfg)?;
    srv.wait_ready(Duration::from_secs(120))?;
    let mode = if fastlink_v4 {
        "fastlink-v4"
    } else {
        "stock-rpc"
    };
    let mut lines = Vec::new();
    let prompt = "Explain in two short sentences why reducing unnecessary network data movement helps distributed AI inference.";
    for run_idx in 0..runs.max(1) {
        let m = run_streaming_completion(srv.addr(), prompt, n_predict, 0.0, 42)?;
        let log = srv.read_log().unwrap_or_default();
        let tel = parse_fastlink_telemetry(&log);
        lines.push(format!(
            "{{\"run\":{},\"result\":{}}}",
            run_idx + 1,
            format_metrics_json(mode, &m, &tel)
        ));
    }
    srv.stop();
    Ok(format!("[{}]\n", lines.join(",")))
}
