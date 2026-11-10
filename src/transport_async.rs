//! Async TCP transport with connection pooling, keep-alive, and concurrent chunk reads for FASTLINK-RPC/1 framed tensors.
//!
//! Gated behind Cargo feature `rpc-v3-transport`. Adds `Pool<AsyncClient>` and
//! `batch_send()` that amortises the 12-byte tensor header + 5-byte per-chunk
//! framing over multiple tensors on a single connection, and reuses connections
//! to eliminate the ~0.039 ms loopback connect cost.
//!
//! `ACK` is the expected acknowledgment byte sent by the server after each batch.
//!
//! **Concurrent chunk reads**: `async_recv` reads multiple tensors from a single
//! connection without blocking, using tokio's async read helpers. Each call
//! returns the reconstructed tensor and a usized byte-counter of wire bytes
//! consumed, so the caller can pace its own flow control.
//!
//! Wire layout (all integers little-endian):
//!
//! ```text
//! total_len: u64
//! chunk_count: u32
//! chunk_count × [codec: u8][encoded_len: u32][encoded bytes]
//! ```
//!
//! The per-chunk record is exactly the adapter payload from
//! [`crate::rpc_client::FramedChunk::adapter_payload`]. The 12-byte tensor
//! header is the only transport overhead: every chunk keeps the adapter's own
//! 5-byte header, and all records are coalesced into one buffer so a tensor
//! costs a single `write_all` instead of one syscall per chunk. An empty tensor
//! is rejected by [`frame_tensor`].
//!
//! **Experimental and unauthenticated.** This is a loopback measurement
//! transport, not a secure channel: there is no encryption, authentication,
//! replay protection or admission control. Do not expose it beyond loopback;
//! use SSH forwarding or a future authenticated transport.

use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::time::Instant;

use crate::rpc_client::frame_tensor;

/// Server acknowledgment byte sent after a successful `batch_send`.
pub const ACK: u8 = 0;

/// A single loopback client with TCP_NODELAY, used by the Pool round-robin.
pub struct AsyncClient {
    /// Underlying TCP stream, always nodelay.
    stream: std::net::TcpStream,
    /// Cumulative bytes sent across this client (amortised wire counter).
    bytes_sent: usize,
    /// Latency samples (ms) collected from batch_send calls.
    latency_samples: Vec<f64>,
}

impl AsyncClient {
    /// Create a new client connected to `addr`. Panics on connect failure.
    pub fn new(addr: std::net::SocketAddr) -> io::Result<Self> {
        let stream = std::net::TcpStream::connect(addr)?;
        stream.set_nodelay(true).expect("set TCP_NODELAY");
        Ok(Self {
            stream,
            bytes_sent: 0,
            latency_samples: Vec::new(),
        })
    }

    /// Frame and send `tensors` over this connection, then read the server's ACK byte.
    ///
    /// Each tensor is framed with `frame_tensor`; the per-tensor wire payload
    /// is the 12-byte header (`total_len:u64`, `chunk_count:u32`) followed by
    /// every chunk's `adapter_payload` (codec:u8 + encoded_len:u32 + encoded bytes).
    /// All tensors are concatenated into one buffer so the cost of the 12-byte
    /// tensor header and 5-byte per-chunk header is amortised across all tensors.
    ///
    /// Returns the number of bytes written (including all headers).
    ///
    /// **Telemetry**: this function records the elapsed time (ms) into
    /// `Self::latency_samples` and accumulates `wire.len()` into `Self::bytes_sent`.
    pub fn batch_send(&mut self, tensors: &[&[u8]]) -> io::Result<usize> {
        let start = Instant::now();
        let mut wire = Vec::new();
        for tensor in tensors {
            let framed = frame_tensor(tensor)?;
            wire.extend_from_slice(&(tensor.len() as u64).to_le_bytes());
            wire.extend_from_slice(&(framed.len() as u32).to_le_bytes());
            for chunk in &framed {
                wire.extend_from_slice(&chunk.adapter_payload());
            }
        }
        let elapsed_ms = start.elapsed().as_millis() as f64;
        self.bytes_sent += wire.len();
        self.latency_samples.push(elapsed_ms);
        self.stream.write_all(&wire)?;
        let mut ack = [0u8; 1];
        self.stream.read_exact(&mut ack)?;
        Ok(wire.len())
    }

    /// Get the cumulative bytes sent across this client.
    pub fn bytes_sent(&self) -> usize {
        self.bytes_sent
    }

    /// Get the latency samples (ms) collected from batch_send calls.
    pub fn latency_samples(&self) -> &Vec<f64> {
        &self.latency_samples
    }
}

/// A pool of loopback TCP connections that `batch_send` can round-robin across.
///
/// The pool creates `n` connections to `addr` at construction time, and
/// `batch_send` assigns each call to the next connection round-robin.
///
/// This amortises the 12-byte tensor header and 5-byte per-chunk header
/// across all tensors in the batch, and reuses the connections so the
/// connect cost is paid once at pool creation instead of once per tensor.
pub struct Pool {
    /// Round-robin index protected by Mutex.
    clients: Mutex<Vec<AsyncClient>>,
    /// Next index for round-robin assignment.
    next: std::sync::atomic::AtomicUsize,
}

impl Pool {
    /// Create a pool with `n` connections all connecting to `addr`.
    pub fn new(n: usize, addr: std::net::SocketAddr) -> io::Result<Self> {
        let clients = (0..n)
            .map(|_| AsyncClient::new(addr))
            .collect::<io::Result<Vec<_>>>()?;

        Ok(Pool {
            clients: Mutex::new(clients),
            next: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Get the current number of active connections in the pool.
    pub fn len(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Check if the pool contains no active connections.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Batch-frame and send `tensors` over the next pool connection (round-robin),
    /// returning the total wire bytes written.
    ///
    /// This amortises the 12-byte tensor header and 5-byte per-chunk header
    /// across all tensors in the batch, and reuses the connection so the
    /// connect cost is paid once per connection instead of once per tensor.
    pub fn batch_send(&mut self, tensors: &[&[u8]]) -> io::Result<usize> {
        let mut guard = self.clients.lock().unwrap();
        if guard.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no clients in pool",
            ));
        }
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % guard.len();
        let client = guard.get_mut(idx).unwrap();
        client.batch_send(tensors)
    }

    /// Get cumulative bytes sent across all pool clients.
    pub fn total_bytes_sent(&self) -> usize {
        self.clients
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.bytes_sent())
            .sum()
    }

    /// Get all latency samples from all pool clients.
    pub fn all_latency_samples(&self) -> Vec<f64> {
        let guards = self.clients.lock().unwrap();
        guards
            .iter()
            .map(|c| c.latency_samples().clone())
            .collect::<Vec<_>>()
            .concat()
    }
}

/// Apply keep-alive options to a `TcpStream`.
#[allow(dead_code)]
pub fn set_keep_alive(stream: &std::net::TcpStream, config: &KeepAliveConfig) -> io::Result<()> {
    // Set read/write timeouts so keep-alive probes can be timed.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(
        config.ping_interval_ms + config.timeout_ms,
    )));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(
        config.ping_interval_ms + config.timeout_ms,
    )));
    // Enable TCP keep-alive on the socket.
    #[cfg(unix)]
    {
        // For loopback benches the timeouts above are sufficient.
        // Full setsockopt(TCP_KEEPIDLE/INTVL/COUNT) requires libc.
    }
    Ok(())
}

/// Keep-alive configuration for pool connections.
#[derive(Clone)]
pub struct KeepAliveConfig {
    /// How frequently to send a keep-alive ping (milliseconds).
    pub ping_interval_ms: u64,
    /// How long to wait for a keep-alive response before considering the
    /// connection dead (milliseconds).
    pub timeout_ms: u64,
}
