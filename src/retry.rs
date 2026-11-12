//! FL3-A5: Retry, timeout, and reconnect policy for the FASTLINK Rust protocol.
//!
//! Implements the FL3-C3 fault-injection policy constants and equations:
//! - `POLICY_MAX_RETRIES_PER_RPC = 2` (up to 3 attempts per RPC / transfer step, covering
//!   transient loss $p \le 0.05$ with $1 - p^3 = 0.999875$ completion probability).
//! - `POLICY_TIMEOUT_FLOOR_MS = 50.0` ms and `POLICY_TIMEOUT_MULTIPLIER = 3.0`:
//!   $$\text{attempt\_timeout\_ms} = \max\left(50.0,\; 3.0 \times (\text{queue\_ms} + \text{rtt\_ms} + \text{serialization\_ms} + \text{wire\_ms})\right)$$
//!   where $\text{wire\_ms} = 1000.0 \times \text{payload\_mib} / \text{throughput\_mib\_s}$ is recomputed
//!   from the current link estimate.
//! - `POLICY_RECONNECT_BACKOFF_MS = [100, 400, 1600]` ms and `POLICY_MAX_RECONNECTS = 3`.
//! - Terminal failure after exhausting retries/reconnects aborts cleanly with **no partial output**
//!   committed (`outputs/<name>` is untouched and any session-scoped `.fastlink-partial.s<id>`
//!   temporary staging file is removed).

use std::io;
use std::path::Path;
use std::thread;
use std::time::Duration;

use crate::{send_file_once_with_timeout, TransferMetrics};

/// Retries per RPC / transfer attempt (`2` retries = `3` total attempts per connection cycle).
pub const POLICY_MAX_RETRIES_PER_RPC: u32 = 2;

/// Minimum per-attempt timeout floor in milliseconds (`50.0` ms).
pub const POLICY_TIMEOUT_FLOOR_MS: f64 = 50.0;

/// Multiplier applied to expected one-way/step latency (`3.0x`).
pub const POLICY_TIMEOUT_MULTIPLIER: f64 = 3.0;

/// Maximum reconnect cycles after disconnect/timeout exhaustion (`3` reconnects).
pub const POLICY_MAX_RECONNECTS: u32 = 3;

/// Reconnect backoff schedule in milliseconds (`100 ms`, `400 ms`, `1600 ms`).
pub const POLICY_RECONNECT_BACKOFF_MS: [u64; 3] = [100, 400, 1600];

/// Nominal boundary-hop RTT in milliseconds (`1.0` ms from FL3-C1/C3).
pub const NOMINAL_LINK_RTT_MS: f64 = 1.0;

/// Nominal boundary-hop effective throughput in MiB/s (`95.3674` MiB/s = 1 Gbps @ 0.8 efficiency).
pub const NOMINAL_LINK_THROUGHPUT_MIB_S: f64 = 95.367_431_640_625;

/// Nominal serialization overhead in milliseconds (`0.05` ms).
pub const NOMINAL_LINK_SERIALIZATION_MS: f64 = 0.05;

/// Nominal queue delay in milliseconds (`0.2` ms).
pub const NOMINAL_LINK_QUEUE_MS: f64 = 0.2;

/// Current link estimate used to compute adaptive per-attempt timeouts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkEstimate {
    pub rtt_ms: f64,
    pub throughput_mib_s: f64,
    pub serialization_ms: f64,
    pub queue_ms: f64,
}

impl Default for LinkEstimate {
    fn default() -> Self {
        Self::nominal()
    }
}

impl LinkEstimate {
    /// Nominal 1 Gbps boundary link from FL3-C1/C3 (`rtt=1.0 ms`, `95.3674 MiB/s`, `ser=0.05 ms`, `queue=0.2 ms`).
    pub const fn nominal() -> Self {
        Self {
            rtt_ms: NOMINAL_LINK_RTT_MS,
            throughput_mib_s: NOMINAL_LINK_THROUGHPUT_MIB_S,
            serialization_ms: NOMINAL_LINK_SERIALIZATION_MS,
            queue_ms: NOMINAL_LINK_QUEUE_MS,
        }
    }

    /// Return a link estimate whose bandwidth is scaled by `factor` (e.g. `1.0`, `0.5`, `0.1`, `0.01`).
    pub fn degraded(self, factor: f64) -> Self {
        let clamped = if factor.is_finite() && factor > 0.0 {
            factor
        } else {
            0.01
        };
        Self {
            throughput_mib_s: self.throughput_mib_s * clamped,
            ..self
        }
    }

    /// Wire transfer duration in milliseconds for `payload_bytes` at the current link throughput.
    pub fn wire_ms(&self, payload_bytes: u64) -> f64 {
        let mib = payload_bytes as f64 / (1024.0 * 1024.0);
        let bw = if self.throughput_mib_s.is_finite() && self.throughput_mib_s > 0.0 {
            self.throughput_mib_s
        } else {
            NOMINAL_LINK_THROUGHPUT_MIB_S
        };
        1000.0 * mib / bw
    }

    /// Expected transfer latency in milliseconds: `queue_ms + rtt_ms + serialization_ms + wire_ms`.
    pub fn expected_transfer_ms(&self, payload_bytes: u64) -> f64 {
        self.queue_ms + self.rtt_ms + self.serialization_ms + self.wire_ms(payload_bytes)
    }

    /// Compute the FL3-C3 per-attempt timeout in milliseconds:
    /// `max(50.0 ms, 3.0 * (queue_ms + rtt_ms + serialization_ms + wire_ms))`.
    pub fn attempt_timeout_ms(&self, payload_bytes: u64) -> f64 {
        let expected = self.expected_transfer_ms(payload_bytes);
        POLICY_TIMEOUT_FLOOR_MS.max(POLICY_TIMEOUT_MULTIPLIER * expected)
    }

    /// Compute the FL3-C3 per-attempt timeout as a [`Duration`].
    pub fn attempt_timeout(&self, payload_bytes: u64) -> Duration {
        Duration::from_secs_f64(self.attempt_timeout_ms(payload_bytes) / 1000.0)
    }
}

/// Retry, timeout, and reconnect policy configuration (defaults to FL3-C3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    /// Maximum retries per RPC/transfer (`2` retries = `3` attempts per reconnect cycle).
    pub max_retries_per_rpc: u32,
    /// Minimum per-attempt timeout floor in milliseconds (`50.0` ms).
    pub timeout_floor_ms: f64,
    /// Multiplier over expected `(queue + rtt + ser + wire)` latency (`3.0`).
    pub timeout_multiplier: f64,
    /// Backoff schedule in milliseconds between reconnect cycles (`[100, 400, 1600]`).
    pub reconnect_backoff_ms: [u64; 3],
    /// Maximum number of reconnect cycles before terminal abort (`3`).
    pub max_reconnects: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries_per_rpc: POLICY_MAX_RETRIES_PER_RPC,
            timeout_floor_ms: POLICY_TIMEOUT_FLOOR_MS,
            timeout_multiplier: POLICY_TIMEOUT_MULTIPLIER,
            reconnect_backoff_ms: POLICY_RECONNECT_BACKOFF_MS,
            max_reconnects: POLICY_MAX_RECONNECTS,
        }
    }
}

impl RetryPolicy {
    /// Compute per-attempt timeout in milliseconds from `link` and `payload_bytes`.
    pub fn attempt_timeout_ms(&self, link: &LinkEstimate, payload_bytes: u64) -> f64 {
        let expected = link.expected_transfer_ms(payload_bytes);
        self.timeout_floor_ms
            .max(self.timeout_multiplier * expected)
    }

    /// Compute per-attempt timeout as a [`Duration`].
    pub fn attempt_timeout(&self, link: &LinkEstimate, payload_bytes: u64) -> Duration {
        Duration::from_secs_f64(self.attempt_timeout_ms(link, payload_bytes) / 1000.0)
    }

    /// Return the reconnect backoff in milliseconds for `reconnect_index` (`0 -> 100`, `1 -> 400`, `2+ -> 1600`).
    pub fn backoff_ms_for(&self, reconnect_index: u32) -> u64 {
        let idx = (reconnect_index as usize).min(self.reconnect_backoff_ms.len().saturating_sub(1));
        self.reconnect_backoff_ms[idx]
    }

    /// Worst-case total transfer budget in milliseconds across all reconnect cycles and retries
    /// (`max_reconnects * (1 + max_retries_per_rpc) * attempt_timeout_ms`, matching FL3-C3 R4).
    pub fn worst_case_budget_ms(&self, link: &LinkEstimate, payload_bytes: u64) -> f64 {
        let attempts_per_cycle = 1u64 + self.max_retries_per_rpc as u64;
        let cycles = self.max_reconnects.max(1) as u64;
        (cycles * attempts_per_cycle) as f64 * self.attempt_timeout_ms(link, payload_bytes)
    }
}

/// Deterministic fault-injection schedule for testing degraded links, forced timeouts, and mid-transfer disconnects.
#[derive(Debug, Clone, Default)]
pub struct FaultInjectionPlan {
    /// For each attempt index `0, 1, ...`, optional `Some(k)` aborts the transfer after sending `k` chunks,
    /// or `Some(u32::MAX)` forces an immediate pre-transfer timeout (`ErrorKind::TimedOut`).
    pub per_attempt_faults: Vec<AttemptFault>,
    /// If false (default in unit tests), records backoff durations in `PolicyTransferReport::backoff_schedule_ms`
    /// without sleeping the test thread for hundreds of milliseconds. Set true to sleep in wall-clock time.
    pub sleep_on_backoff: bool,
}

/// Fault injected on a specific attempt in [`FaultInjectionPlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptFault {
    /// Normal attempt with no injected fault.
    None,
    /// Simulate a link timeout (`io::ErrorKind::TimedOut`) after sending `chunks_before_timeout` chunks.
    /// Meanwhile the link estimate degrades by `degrade_factor` (e.g. `0.1` or `0.01`).
    TimeoutAfterChunks {
        chunks_before_timeout: u32,
        degrade_factor: u32, // percentage 1..=100
    },
    /// Simulate a worker disconnect (`io::ErrorKind::ConnectionAborted`) after sending `chunks_before_abort` chunks.
    DisconnectAfterChunks(u32),
}

/// Telemetry report returned by [`send_file_with_policy`].
#[derive(Debug, Clone)]
pub struct PolicyTransferReport {
    pub filename: String,
    pub final_metrics: Option<TransferMetrics>,
    pub total_attempts: u32,
    pub rpc_retries_used: u32,
    pub reconnects_used: u32,
    pub attempt_timeouts_ms: Vec<f64>,
    pub backoff_schedule_ms: Vec<u64>,
    pub terminal_abort: bool,
}

fn is_retryable_io_error(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::Interrupted
    )
}

/// Execute a file transfer governed by [`RetryPolicy`] and [`LinkEstimate`], reusing
/// resume-by-manifest so any chunks verified before a timeout or disconnect are not re-sent.
///
/// - Each reconnect cycle allows `1 + policy.max_retries_per_rpc` RPC attempts (`3` attempts when `retries=2`).
/// - On each attempt, the socket timeout is set to `policy.attempt_timeout(&current_link, remaining_bytes)`
///   (`max(50 ms, 3x(queue + rtt + ser + wire))`).
/// - If a cycle exhausts its per-RPC retries or encounters a disconnect, the client backs off
///   according to `policy.reconnect_backoff_ms` (`100 ms`, `400 ms`, `1600 ms`) up to
///   `policy.max_reconnects` (`3`) reconnects and resumes by manifest.
/// - On terminal failure (all retries and reconnects exhausted), returns `Err` and the server
///   commits **no partial output**.
pub fn send_file_with_policy(
    file_path: &Path,
    server: &str,
    policy: RetryPolicy,
    initial_link: LinkEstimate,
    fault_plan: Option<&FaultInjectionPlan>,
) -> io::Result<PolicyTransferReport> {
    let file_size = std::fs::metadata(file_path)?.len();
    let mut current_link = initial_link;
    let mut total_attempts = 0u32;
    let mut rpc_retries_used = 0u32;
    let mut reconnects_used = 0u32;
    let mut attempt_timeouts_ms = Vec::new();
    let mut backoff_schedule_ms = Vec::new();
    let mut last_err: Option<io::Error> = None;

    for reconnect_idx in 0..=policy.max_reconnects {
        if reconnect_idx > 0 {
            let backoff_ms = policy.backoff_ms_for(reconnect_idx - 1);
            backoff_schedule_ms.push(backoff_ms);
            reconnects_used += 1;
            if fault_plan.is_none_or(|p| p.sleep_on_backoff) {
                thread::sleep(Duration::from_millis(backoff_ms));
            }
        }

        for retry_idx in 0..=policy.max_retries_per_rpc {
            if retry_idx > 0 {
                rpc_retries_used += 1;
            }
            let attempt_idx = total_attempts as usize;
            total_attempts += 1;

            let fault = fault_plan
                .and_then(|p| p.per_attempt_faults.get(attempt_idx))
                .copied()
                .unwrap_or(AttemptFault::None);

            // Recompute per-attempt timeout from the CURRENT link estimate
            let timeout_ms = policy.attempt_timeout_ms(&current_link, file_size.max(65_536));
            attempt_timeouts_ms.push(timeout_ms);
            let socket_timeout = Duration::from_secs_f64((timeout_ms / 1000.0).max(0.050));

            let (abort_after, inject_timeout, next_degrade) = match fault {
                AttemptFault::None => (None, false, None),
                AttemptFault::TimeoutAfterChunks {
                    chunks_before_timeout,
                    degrade_factor,
                } => (
                    Some(chunks_before_timeout),
                    true,
                    Some((degrade_factor.max(1) as f64) / 100.0),
                ),
                AttemptFault::DisconnectAfterChunks(k) => (Some(k), false, None),
            };

            let outcome =
                send_file_once_with_timeout(file_path, server, abort_after, socket_timeout);

            if let Some(factor) = next_degrade {
                current_link = initial_link.degraded(factor);
            }

            match outcome {
                Ok((filename, metrics)) => {
                    return Ok(PolicyTransferReport {
                        filename,
                        final_metrics: Some(metrics),
                        total_attempts,
                        rpc_retries_used,
                        reconnects_used,
                        attempt_timeouts_ms,
                        backoff_schedule_ms,
                        terminal_abort: false,
                    });
                }
                Err(err) => {
                    let mapped_err = if inject_timeout {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("per-attempt timeout ({timeout_ms:.1} ms) exceeded: {err}"),
                        )
                    } else {
                        err
                    };

                    if !is_retryable_io_error(mapped_err.kind()) {
                        return Err(mapped_err);
                    }

                    let is_disconnect = matches!(
                        mapped_err.kind(),
                        io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                            | io::ErrorKind::UnexpectedEof
                    );
                    last_err = Some(mapped_err);

                    // A hard disconnect advances directly to the reconnect + backoff cycle
                    if is_disconnect {
                        break;
                    }
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal abort: retry and reconnect budget exhausted",
        )
    }))
}
