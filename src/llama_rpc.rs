//! Experimental `llama.cpp` RPC adapter & benchmark harness for FASTLINK.
//!
//! Spawns a loopback `llama-server` connected to one or more `ggml-rpc-server`
//! endpoints, toggles FASTLINK-RPC v4.1 environment flags (`GGML_RPC_FASTLINK_V4`,
//! `GGML_RPC_FASTLINK_PACK`, `GGML_RPC_FASTLINK_RLE`), streams `/completion` SSE
//! tokens to measure TTFT and decode throughput, and guarantees process cleanup
//! via RAII `Drop`.
//!
//! The SSE client is exposed as a callback/event API ([`stream_completion`],
//! [`spawn_completion_stream`], [`CompletionStream`]) so a GUI can render token
//! deltas as soon as they arrive, while [`run_streaming_completion`] keeps the
//! historical aggregate-only behavior.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::{digest, hex, Digest};

/// Maximum accepted size of a single HTTP chunk payload (guards `vec!` allocation).
const MAX_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum accepted length of an HTTP chunk-size / trailer line.
const MAX_CHUNK_HEADER_BYTES: usize = 1024;
/// Maximum accepted size of the HTTP response header block.
const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
/// Bounded event queue between the worker thread and its consumer.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

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
    /// The consumer dropped its event receiver, so the worker stopped early.
    StreamCancelled,
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
            Self::StreamCancelled => write!(f, "completion stream cancelled by its consumer"),
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

// ---------------------------------------------------------------------------
// Streaming completion: request, options, events, errors.
// ---------------------------------------------------------------------------

/// A `llama-server` `/completion` request used by the streaming client.
#[derive(Clone, Debug, PartialEq)]
pub struct CompletionRequest {
    pub prompt: String,
    pub n_predict: u32,
    pub temperature: f32,
    pub seed: u32,
}

impl CompletionRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            n_predict: 64,
            temperature: 0.0,
            seed: 42,
        }
    }

    pub fn with_n_predict(mut self, n_predict: u32) -> Self {
        self.n_predict = n_predict;
        self
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn with_seed(mut self, seed: u32) -> Self {
        self.seed = seed;
        self
    }

    /// Serializes the JSON body sent to `POST /completion` (`stream: true`).
    pub fn body_json(&self) -> String {
        serde_json::json!({
            "prompt": self.prompt,
            "n_predict": self.n_predict,
            "temperature": self.temperature,
            "seed": self.seed,
            "stream": true,
        })
        .to_string()
    }
}

/// Transport and parsing limits for one streaming completion.
#[derive(Clone, Debug)]
pub struct CompletionStreamOptions {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    /// Bound on a single reassembled SSE event payload; protects against
    /// unbounded buffering from a hostile or broken server.
    pub max_event_bytes: usize,
}

impl Default for CompletionStreamOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(120),
            max_event_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Event emitted while a streaming completion is decoded on the calling thread.
#[derive(Clone, Debug, PartialEq)]
pub enum CompletionEvent {
    /// One token/text delta, delivered as soon as its SSE event arrives.
    Delta {
        /// 0-based ordinal of this delta within the stream.
        index: u64,
        text: String,
        /// Milliseconds from request start to the first delta (the same value is
        /// repeated for later deltas; the aggregate TTFT lives on the metrics).
        ttft_ms: f64,
    },
    /// Final aggregate metrics; always the last event of a successful stream.
    Finished { metrics: LlamaCompletionMetrics },
}

/// Ordered events plus a join handle for a worker-thread completion stream.
///
/// The channel is bounded (1024 events), so a slow consumer applies backpressure
/// instead of growing memory without limit.
#[derive(Debug)]
pub struct CompletionStreamHandle {
    events: Receiver<CompletionEvent>,
    handle: JoinHandle<Result<LlamaCompletionMetrics, LlamaRpcError>>,
}

impl CompletionStreamHandle {
    /// Receiver for deltas/final metrics; drain it from the GUI event loop.
    pub fn events(&self) -> &Receiver<CompletionEvent> {
        &self.events
    }

    /// Splits off the receiver and the raw worker handle (for cancellation tests
    /// or custom joining).
    pub fn into_parts(
        self,
    ) -> (
        Receiver<CompletionEvent>,
        JoinHandle<Result<LlamaCompletionMetrics, LlamaRpcError>>,
    ) {
        (self.events, self.handle)
    }

    /// Drains any remaining queued events, waits for the worker and returns the
    /// final aggregate metrics. Consume [`events`](Self::events) first if the
    /// deltas themselves are needed.
    pub fn join(self) -> Result<LlamaCompletionMetrics, LlamaRpcError> {
        let (events, handle) = self.into_parts();
        // Keep draining so a bounded queue cannot block the worker, then join.
        while events.recv().is_ok() {}
        join_worker(handle)
    }
}

fn join_worker(
    handle: JoinHandle<Result<LlamaCompletionMetrics, LlamaRpcError>>,
) -> Result<LlamaCompletionMetrics, LlamaRpcError> {
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(LlamaRpcError::ParseError(
            "completion stream worker panicked".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// SSE wire types (serde, bounded).
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct RawTimings {
    #[serde(default)]
    prompt_ms: Option<f64>,
    #[serde(default)]
    predicted_ms: Option<f64>,
    #[serde(default)]
    predicted_n: Option<f64>,
    #[serde(default)]
    predicted_per_second: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawChunk {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    stop: bool,
    #[serde(default)]
    timings: Option<RawTimings>,
    // Older/alternate servers report timings at the top level.
    #[serde(default)]
    prompt_ms: Option<f64>,
    #[serde(default)]
    predicted_ms: Option<f64>,
    #[serde(default)]
    predicted_n: Option<f64>,
    #[serde(default)]
    predicted_per_second: Option<f64>,
}

impl RawChunk {
    fn timings_quad(&self) -> (f64, f64, u64, f64) {
        let nested = self.timings.as_ref();
        let pick = |nested: Option<f64>, root: Option<f64>| nested.or(root).unwrap_or(0.0);
        let prompt_ms = pick(nested.and_then(|t| t.prompt_ms), self.prompt_ms);
        let predicted_ms = pick(nested.and_then(|t| t.predicted_ms), self.predicted_ms);
        let predicted_n = pick(nested.and_then(|t| t.predicted_n), self.predicted_n) as u64;
        let per_second = pick(
            nested.and_then(|t| t.predicted_per_second),
            self.predicted_per_second,
        );
        (prompt_ms, predicted_ms, predicted_n, per_second)
    }
}

/// Parses a single SSE `data: {...}` JSON line from `llama-server /completion` (stream=true).
///
/// Tolerant helper for callers that already have one complete line: returns
/// `None` for non-data lines, `[DONE]`, empty payloads and JSON that does not
/// parse. The streaming client itself ([`CompletionStream`]) is strict and
/// surfaces malformed payloads as [`LlamaRpcError::ParseError`].
pub fn parse_completion_sse_line(
    line: &str,
) -> Option<(Option<String>, Option<LlamaCompletionMetrics>)> {
    let trimmed = line.trim();
    let json = trimmed.strip_prefix("data:")?.trim();
    if json.is_empty() || json == "[DONE]" {
        return None;
    }
    let chunk: RawChunk = serde_json::from_str(json).ok()?;
    let metrics = if chunk.stop {
        let (prompt_ms, predicted_ms, predicted_n, predicted_per_second) = chunk.timings_quad();
        Some(LlamaCompletionMetrics {
            ttft_ms: 0.0,
            total_wall_ms: 0.0,
            prompt_ms,
            predicted_ms,
            predicted_n,
            predicted_per_second,
            content: String::new(),
            content_digest: [0u8; 32],
        })
    } else {
        None
    };
    Some((chunk.content, metrics))
}

// ---------------------------------------------------------------------------
// HTTP body framing + bounded line reader.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Framing {
    Chunked,
    Length(u64),
    CloseDelimited,
}

/// Decodes an HTTP response body (chunked, content-length, or close-delimited)
/// and yields bounded, UTF-8 SSE lines.
struct BodyReader {
    reader: BufReader<TcpStream>,
    framing: Framing,
    buffered: VecDeque<u8>,
    consumed: u64,
    eof: bool,
}

impl BodyReader {
    fn new(reader: BufReader<TcpStream>, framing: Framing) -> Self {
        Self {
            reader,
            framing,
            buffered: VecDeque::new(),
            consumed: 0,
            eof: false,
        }
    }

    fn read_byte(&mut self) -> Result<Option<u8>, LlamaRpcError> {
        let mut byte = [0u8; 1];
        loop {
            match self.reader.read(&mut byte) {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(byte[0])),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(LlamaRpcError::Io(e)),
            }
        }
    }

    fn read_exact_body(&mut self, buf: &mut [u8], what: &str) -> Result<(), LlamaRpcError> {
        for slot in buf.iter_mut() {
            match self.read_byte()? {
                Some(b) => *slot = b,
                None => {
                    return Err(LlamaRpcError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("truncated HTTP body while reading {what}"),
                    )))
                }
            }
        }
        Ok(())
    }

    /// Reads one raw (undecoded) line from the socket, used for chunk headers.
    fn read_raw_line(&mut self, limit: usize) -> Result<Option<String>, LlamaRpcError> {
        let mut bytes = Vec::new();
        loop {
            match self.read_byte()? {
                None => {
                    if bytes.is_empty() {
                        return Ok(None);
                    }
                    break;
                }
                Some(b'\n') => break,
                Some(b) => {
                    bytes.push(b);
                    if bytes.len() > limit {
                        return Err(LlamaRpcError::ParseError(format!(
                            "HTTP chunk header exceeds {limit} bytes"
                        )));
                    }
                }
            }
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Pulls the next framed body block into `buffered`; `false` means end of body.
    fn fill(&mut self) -> Result<bool, LlamaRpcError> {
        if self.eof {
            return Ok(false);
        }
        match self.framing {
            Framing::Chunked => {
                let Some(size_line) = self.read_raw_line(MAX_CHUNK_HEADER_BYTES)? else {
                    return Err(LlamaRpcError::ParseError(
                        "truncated chunked body: missing chunk size".into(),
                    ));
                };
                let size_text = size_line.split(';').next().unwrap_or("").trim();
                let size = u64::from_str_radix(size_text, 16).map_err(|_| {
                    LlamaRpcError::ParseError(format!("invalid HTTP chunk size: {size_line:?}"))
                })?;
                if size == 0 {
                    // Consume optional trailers up to the terminating blank line.
                    while let Some(trailer) = self.read_raw_line(MAX_CHUNK_HEADER_BYTES)? {
                        if trailer.is_empty() {
                            break;
                        }
                    }
                    self.eof = true;
                    return Ok(false);
                }
                if size > MAX_CHUNK_BYTES {
                    return Err(LlamaRpcError::ParseError(format!(
                        "HTTP chunk of {size} bytes exceeds {MAX_CHUNK_BYTES} byte limit"
                    )));
                }
                let mut data = vec![0u8; size as usize];
                self.read_exact_body(&mut data, "chunk payload")?;
                let mut terminator = [0u8; 2];
                self.read_exact_body(&mut terminator, "chunk terminator")?;
                if &terminator != b"\r\n" {
                    return Err(LlamaRpcError::ParseError(
                        "malformed HTTP chunk terminator".into(),
                    ));
                }
                self.buffered.extend(data);
                Ok(true)
            }
            Framing::Length(len) => {
                if self.consumed >= len {
                    self.eof = true;
                    return Ok(false);
                }
                match self.read_byte()? {
                    Some(b) => {
                        self.consumed += 1;
                        self.buffered.push_back(b);
                        Ok(true)
                    }
                    None => Err(LlamaRpcError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated content-length HTTP body",
                    ))),
                }
            }
            Framing::CloseDelimited => match self.read_byte()? {
                Some(b) => {
                    self.buffered.push_back(b);
                    Ok(true)
                }
                None => {
                    self.eof = true;
                    Ok(false)
                }
            },
        }
    }

    /// Next line of the body without its terminator (`\n` or `\r\n`).
    fn next_line(&mut self, limit: usize) -> Result<Option<String>, LlamaRpcError> {
        let mut bytes = Vec::new();
        loop {
            if self.buffered.is_empty() && !self.fill()? {
                if bytes.is_empty() {
                    return Ok(None);
                }
                break;
            }
            let Some(b) = self.buffered.pop_front() else {
                continue;
            };
            if b == b'\n' {
                break;
            }
            bytes.push(b);
            if bytes.len() > limit {
                return Err(LlamaRpcError::ParseError(format!(
                    "SSE event exceeds max_event_bytes limit of {limit} bytes"
                )));
            }
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| LlamaRpcError::ParseError("SSE line is not valid UTF-8".into()))
    }
}

// ---------------------------------------------------------------------------
// Pull-based streaming client.
// ---------------------------------------------------------------------------

/// Incremental `/completion` SSE client.
///
/// [`CompletionStream::next_event`] blocks until the next delta or the final
/// metrics arrive, so a GUI can render tokens immediately without buffering the
/// whole response.
pub struct CompletionStream {
    body: BodyReader,
    started: Instant,
    pending: String,
    queue: VecDeque<CompletionEvent>,
    next_index: u64,
    first_delta_ms: Option<f64>,
    content: String,
    finished: bool,
    max_event_bytes: usize,
}

impl CompletionStream {
    /// Connects, sends the streaming request and validates the HTTP response.
    pub fn open(
        addr: SocketAddr,
        request: &CompletionRequest,
        options: &CompletionStreamOptions,
    ) -> Result<Self, LlamaRpcError> {
        let started = Instant::now();
        let mut stream = TcpStream::connect_timeout(&addr, options.connect_timeout)?;
        stream.set_read_timeout(Some(options.read_timeout))?;
        stream.set_nodelay(true)?;

        let body = request.body_json();
        let request_head = format!(
            "POST /completion HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request_head.as_bytes())?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;
        if status_line.is_empty() {
            return Err(LlamaRpcError::ParseError(
                "empty HTTP response from /completion".into(),
            ));
        }
        let code = status_line.split_whitespace().nth(1).unwrap_or("");
        if code != "200" {
            return Err(LlamaRpcError::HttpError(status_line.trim().to_string()));
        }

        let mut content_length: Option<u64> = None;
        let mut chunked = false;
        let mut header_bytes = 0usize;
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                return Err(LlamaRpcError::ParseError(
                    "truncated HTTP response headers".into(),
                ));
            }
            header_bytes += read;
            if header_bytes > MAX_HTTP_HEADER_BYTES {
                return Err(LlamaRpcError::ParseError(format!(
                    "HTTP response headers exceed {MAX_HTTP_HEADER_BYTES} bytes"
                )));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                content_length = Some(value.trim().parse::<u64>().map_err(|_| {
                    LlamaRpcError::ParseError(format!("invalid Content-Length header: {line:?}"))
                })?);
            }
            if let Some(value) = lower.strip_prefix("transfer-encoding:") {
                if value.contains("chunked") {
                    chunked = true;
                }
            }
        }

        let framing = if chunked {
            Framing::Chunked
        } else if let Some(len) = content_length {
            Framing::Length(len)
        } else {
            Framing::CloseDelimited
        };

        Ok(Self {
            body: BodyReader::new(reader, framing),
            started,
            pending: String::new(),
            queue: VecDeque::new(),
            next_index: 0,
            first_delta_ms: None,
            content: String::new(),
            finished: false,
            max_event_bytes: options.max_event_bytes,
        })
    }

    /// True once the final `stop:true` event has been decoded.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Blocks until the next event; `Ok(None)` after the final event.
    pub fn next_event(&mut self) -> Result<Option<CompletionEvent>, LlamaRpcError> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Ok(Some(event));
            }
            if self.finished {
                return Ok(None);
            }
            let Some(line) = self.body.next_line(self.max_event_bytes)? else {
                if !self.pending.is_empty() {
                    let payload = std::mem::take(&mut self.pending);
                    self.dispatch(&payload)?;
                    continue;
                }
                return Err(LlamaRpcError::ParseError(
                    "completion stream ended before the final stop:true event".into(),
                ));
            };

            if line.is_empty() {
                if !self.pending.is_empty() {
                    let payload = std::mem::take(&mut self.pending);
                    self.dispatch(&payload)?;
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // SSE comment / keep-alive
            }
            let Some(rest) = line.strip_prefix("data:") else {
                continue; // event:, id:, retry: and unknown fields are ignored
            };
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            if payload.trim().is_empty() {
                continue;
            }
            if payload.trim() == "[DONE]" {
                // End-of-stream marker; the final stop event must have been seen.
                continue;
            }

            if self.pending.is_empty() && serde_json::from_str::<serde_json::Value>(payload).is_ok()
            {
                // Fast path: complete single-line event.
                self.dispatch(payload)?;
                continue;
            }
            if !self.pending.is_empty() {
                self.pending.push('\n');
            }
            self.pending.push_str(payload);
            if self.pending.len() > self.max_event_bytes {
                return Err(LlamaRpcError::ParseError(format!(
                    "SSE event exceeds max_event_bytes limit of {} bytes",
                    self.max_event_bytes
                )));
            }
            // A multi-line event is complete once the payload parses as JSON.
            if serde_json::from_str::<serde_json::Value>(&self.pending).is_ok() {
                let payload = std::mem::take(&mut self.pending);
                self.dispatch(&payload)?;
            }
        }
    }

    fn dispatch(&mut self, payload: &str) -> Result<(), LlamaRpcError> {
        if payload.len() > self.max_event_bytes {
            return Err(LlamaRpcError::ParseError(format!(
                "SSE event exceeds max_event_bytes limit of {} bytes",
                self.max_event_bytes
            )));
        }
        let chunk: RawChunk = serde_json::from_str(payload).map_err(|e| {
            LlamaRpcError::ParseError(format!("invalid SSE JSON payload: {e}: {payload:?}"))
        })?;

        if let Some(text) = chunk.content.as_deref() {
            if !text.is_empty() {
                let ttft_ms = match self.first_delta_ms {
                    Some(ttft) => ttft,
                    None => {
                        let ttft = self.started.elapsed().as_secs_f64() * 1000.0;
                        self.first_delta_ms = Some(ttft);
                        ttft
                    }
                };
                self.content.push_str(text);
                let index = self.next_index;
                self.next_index += 1;
                self.queue.push_back(CompletionEvent::Delta {
                    index,
                    text: text.to_string(),
                    ttft_ms,
                });
            }
        }

        if chunk.stop {
            let (prompt_ms, predicted_ms, predicted_n, predicted_per_second) = chunk.timings_quad();
            let total_wall_ms = self.started.elapsed().as_secs_f64() * 1000.0;
            let mut metrics = LlamaCompletionMetrics {
                ttft_ms: self.first_delta_ms.unwrap_or(total_wall_ms),
                total_wall_ms,
                prompt_ms,
                predicted_ms,
                predicted_n,
                predicted_per_second,
                content: std::mem::take(&mut self.content),
                content_digest: [0u8; 32],
            };
            metrics.content_digest = digest(metrics.content.as_bytes());
            self.finished = true;
            self.queue.push_back(CompletionEvent::Finished { metrics });
        }
        Ok(())
    }
}

/// Streams a completion and invokes `on_event` for every delta and the final
/// metrics, returning the same aggregate metrics as [`run_streaming_completion`].
///
/// `on_event` runs synchronously on the calling thread, so keep it cheap (for a
/// GUI, prefer [`spawn_completion_stream`] and drain the channel from the UI loop).
pub fn stream_completion<F>(
    addr: SocketAddr,
    request: &CompletionRequest,
    options: &CompletionStreamOptions,
    mut on_event: F,
) -> Result<LlamaCompletionMetrics, LlamaRpcError>
where
    F: FnMut(CompletionEvent),
{
    let mut stream = CompletionStream::open(addr, request, options)?;
    let mut metrics = None;
    while let Some(event) = stream.next_event()? {
        if let CompletionEvent::Finished { metrics: done } = &event {
            metrics = Some(done.clone());
        }
        on_event(event);
    }
    metrics.ok_or_else(|| {
        LlamaRpcError::ParseError("completion stream ended without final metrics".into())
    })
}

/// Runs a streaming completion on a worker thread and forwards events through a
/// channel, for GUIs that must not block their event loop.
///
/// Errors during streaming are returned by [`CompletionStreamHandle::join`];
/// dropping the receiver cancels the worker with [`LlamaRpcError::StreamCancelled`].
pub fn spawn_completion_stream(
    addr: SocketAddr,
    request: &CompletionRequest,
    options: &CompletionStreamOptions,
) -> Result<CompletionStreamHandle, LlamaRpcError> {
    let mut stream = CompletionStream::open(addr, request, options)?;
    let (sender, events) = mpsc::sync_channel(EVENT_CHANNEL_CAPACITY);
    let handle = thread::spawn(move || -> Result<LlamaCompletionMetrics, LlamaRpcError> {
        let mut metrics = None;
        while let Some(event) = stream.next_event()? {
            if let CompletionEvent::Finished { metrics: done } = &event {
                metrics = Some(done.clone());
            }
            if sender.send(event).is_err() {
                return Err(LlamaRpcError::StreamCancelled);
            }
        }
        metrics.ok_or_else(|| {
            LlamaRpcError::ParseError("completion stream ended without final metrics".into())
        })
    });
    Ok(CompletionStreamHandle { events, handle })
}

/// Sends a streaming `/completion` request to a running `llama-server` and measures
/// TTFT, total wall time, decode throughput, and BLAKE3 output digest.
///
/// Kept as the aggregate-only convenience wrapper around [`stream_completion`];
/// deltas are accumulated exactly as before and no callback is required.
pub fn run_streaming_completion(
    addr: SocketAddr,
    prompt: &str,
    n_predict: u32,
    temperature: f32,
    seed: u32,
) -> Result<LlamaCompletionMetrics, LlamaRpcError> {
    let request = CompletionRequest {
        prompt: prompt.to_string(),
        n_predict,
        temperature,
        seed,
    };
    stream_completion(
        addr,
        &request,
        &CompletionStreamOptions::default(),
        |_event| {},
    )
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
