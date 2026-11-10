//! Async TCP transport with connection pooling and batch sending for FASTLINK-RPC/1 framed tensors.
//!
//! Gated behind Cargo feature `rpc-v3-transport`. Adds `Pool<AsyncClient>` and
//! `batch_send()` that amortises the 12-byte tensor header + 5-byte per-chunk
//! framing over multiple tensors on a single connection, and reuses connections
//! to eliminate the ~0.039 ms loopback connect cost.
//!
//! `ACK` is the expected acknowledgment byte sent by the server after each batch.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;

use crate::rpc_client::frame_tensor;

/// Server acknowledgment byte sent after a successful `batch_send`.
pub const ACK: u8 = 0;

/// A single loopback client with TCP_NODELAY, used by the Pool round-robin.
pub struct AsyncClient {
    /// Underlying TCP stream, always nodelay.
    stream: TcpStream,
}

impl AsyncClient {
    /// Create a new client connected to `addr`. Panics on connect failure.
    pub fn new(addr: std::net::SocketAddr) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).expect("set TCP_NODELAY");
        Ok(Self { stream })
    }

    /// Frame and send `tensors` over this connection in one `write_all`, then
    /// read and return the server's ACK byte.
    ///
    /// Each tensor is framed with `frame_tensor`; the per-tensor wire payload
    /// is the 12-byte header (`total_len:u64`, `chunk_count:u32`) followed by
    /// every chunk's `adapter_payload` (codec:u8 + encoded_len:u32 + encoded bytes).
    /// All tensors are concatenated into one buffer so the cost of the 12-byte
    /// tensor header and 5-byte per-chunk header is amortised across all tensors.
    ///
    /// Returns the number of bytes written (including all headers).
    pub fn batch_send(&mut self, tensors: &[&[u8]]) -> io::Result<usize> {
        let mut wire = Vec::new();
        for tensor in tensors {
            let framed = frame_tensor(tensor)?;
            wire.extend_from_slice(&(tensor.len() as u64).to_le_bytes());
            wire.extend_from_slice(&(framed.len() as u32).to_le_bytes());
            for chunk in &framed {
                wire.extend_from_slice(&chunk.adapter_payload());
            }
        }
        self.stream.write_all(&wire)?;
        let mut ack = [0u8; 1];
        self.stream.read_exact(&mut ack)?;
        Ok(wire.len())
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
    /// Round-robin index protected by Mutex (single-threaded pool is fine for
    /// the test/bench use case; replace with RwLock + lease if multi-producer).
    clients: Mutex<Vec<AsyncClient>>,
    /// Next index for round-robin assignment.
    next: std::sync::atomic::AtomicUsize,
}

impl Pool {
    /// Create a pool with `n` connections all connecting to `addr`.
    pub fn new(n: usize, addr: std::net::SocketAddr) -> io::Result<Self> {
        let clients = (0..n)
            .map(|_| AsyncClient::new(addr))
            .collect::<std::io::Result<Vec<_>>>()?;

        Ok(Pool {
            clients: Mutex::new(clients),
            next: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Batch-frame and send `tensors` over the next pool connection (round-robin),
    /// returning the total wire bytes written.
    ///
    /// This amortises the 12-byte tensor header and 5-byte per-chunk header
    /// across all tensors in the batch, and reuses the connection so the
    /// connect cost is paid once per connection instead of once per tensor.
    pub fn batch_send(&self, tensors: &[&[u8]]) -> io::Result<usize> {
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.clients.lock().unwrap().len();
        let mut clients = self.clients.lock().unwrap();
        clients[idx].batch_send(tensors)
    }
}
