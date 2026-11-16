//! Synchronous blocking TCP loopback batching client and serialized pool for FASTLINK-RPC/1 framed tensors.
//!
//! Gated behind Cargo feature `rpc-v3-transport`. Provides the blocking client
//! `AsyncClient` (historical name — all socket I/O uses `std::net::TcpStream`),
//! `batch_send()`, and a mutex-serialized `Pool` of persistent connections so a
//! batch can be sent over an already-connected socket without reconnecting per
//! tensor.
//!
//! `ACK` is the expected 1-byte acknowledgment sent by the server after each batch.
//!
//! **No async I/O, no `async_recv`, and no TCP keep-alive:** despite the module
//! and `AsyncClient` names, this module provides no non-blocking/async transport,
//! no concurrent chunk-read API, and no OS/protocol keep-alive configuration.
//!
//! Wire layout (all integers little-endian):
//!
//! ```text
//! total_len: u64
//! chunk_count: u32
//! chunk_count × [codec: u8][encoded_len: u32][encoded bytes]
//! ```
//!
//! Each tensor in a batch retains its own 12-byte header and 5-byte per-chunk
//! adapter payloads from [`crate::rpc_client::FramedChunk::adapter_payload`].
//! All framed records in a `batch_send` call are concatenated into one buffer so
//! the batch is written with a single blocking `write_all` and confirmed with one
//! blocking `ACK` byte. An empty tensor is rejected by [`frame_tensor`].
//!
//! **Experimental and unauthenticated.** This is a synchronous loopback
//! measurement path, not a secure channel: there is no encryption,
//! authentication, replay protection or admission control. Do not expose it
//! beyond loopback; use SSH forwarding or a future authenticated transport.

use std::io::{self, Read, Write};
use std::sync::Mutex;
use std::time::Instant;

use crate::rpc_client::frame_tensor;
use crate::transport::MAX_TENSOR_BYTES;

/// Server acknowledgment byte sent after a successful `batch_send`.
pub const ACK: u8 = 0;

/// Historical name for a single synchronous, blocking loopback `TcpStream`
/// client with `TCP_NODELAY`.
///
/// Despite the legacy name `AsyncClient`, every operation on this type performs
/// blocking `std::net::TcpStream` I/O and provides no async API or TCP
/// keep-alive.
pub struct AsyncClient {
    /// Underlying blocking TCP stream, configured with TCP_NODELAY.
    stream: std::net::TcpStream,
    /// Cumulative wire bytes accounted across `batch_send` calls on this client.
    bytes_sent: usize,
    /// Elapsed framing-time samples (ms) collected from `batch_send` calls.
    latency_samples: Vec<f64>,
}

impl AsyncClient {
    /// Connect a synchronous blocking client to `addr`.
    ///
    /// Returns connection errors via `io::Result`; panics only if configuring
    /// `TCP_NODELAY` on the connected socket fails.
    pub fn new(addr: std::net::SocketAddr) -> io::Result<Self> {
        let stream = std::net::TcpStream::connect(addr)?;
        stream.set_nodelay(true).expect("set TCP_NODELAY");
        Ok(Self {
            stream,
            bytes_sent: 0,
            latency_samples: Vec::new(),
        })
    }

    /// Frame and synchronously write `tensors` over this blocking connection,
    /// then read and verify the server's 1-byte `ACK`.
    ///
    /// Each tensor is framed with `frame_tensor` and retains its own 12-byte
    /// header (`total_len:u64`, `chunk_count:u32`) plus every chunk's 5-byte
    /// `adapter_payload` (`codec:u8 + encoded_len:u32 + encoded bytes`). All
    /// tensors in the call are concatenated into one buffer so the batch uses a
    /// single blocking `write_all` and expects one `ACK` byte (`ack[0] == ACK`,
    /// returning `InvalidData` otherwise). Tensors larger than
    /// [`MAX_TENSOR_BYTES`] are rejected per tensor with `InvalidInput` before
    /// framing or transmission; aggregate batch memory across multiple tensors is
    /// not separately capped here.
    ///
    /// Returns the number of wire bytes written (including all headers).
    ///
    /// **Telemetry**: records framing elapsed time (ms) into
    /// `Self::latency_samples` and adds `wire.len()` to `Self::bytes_sent`.
    pub fn batch_send(&mut self, tensors: &[&[u8]]) -> io::Result<usize> {
        for tensor in tensors {
            if tensor.len() > MAX_TENSOR_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tensor length exceeds transport limit",
                ));
            }
        }
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
        if ack[0] != ACK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid batch ACK byte",
            ));
        }
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

/// A mutex-serialized pool of synchronous blocking loopback TCP connections.
///
/// Opens `n` persistent [`AsyncClient`] connections to `addr` at construction
/// time. Calls to [`Pool::batch_send`] acquire the internal `Mutex` and
/// dispatch synchronously in round-robin order without creating a new TCP
/// connection per batch, providing no async or concurrent I/O and no TCP
/// keep-alive.
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

    /// Synchronously batch-frame and send `tensors` over the next pool
    /// connection (serialized by the pool mutex, round-robin), returning the
    /// total wire bytes written.
    ///
    /// Reuses an existing persistent connection rather than opening a new
    /// connection per tensor; each tensor still keeps its own 12-byte header and
    /// 5-byte per-chunk headers.
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

    /// Get all framing-latency samples (ms) recorded across pool clients.
    pub fn all_latency_samples(&self) -> Vec<f64> {
        let guards = self.clients.lock().unwrap();
        guards
            .iter()
            .map(|c| c.latency_samples().clone())
            .collect::<Vec<_>>()
            .concat()
    }
}
