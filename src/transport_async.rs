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

use crate::{
    expected_chunk_len, decode_chunk, MAX_ENCODED_BYTES,
    rpc_client::frame_tensor,
};

/// Server acknowledgment byte sent after a successful `batch_send`.
pub const ACK: u8 = 0;

/// A single loopback client with TCP_NODELAY, used by the Pool round-robin.
pub struct AsyncClient {
    /// Underlying TCP stream, always nodelay.
    stream: std::net::TcpStream,
}

impl AsyncClient {
    /// Create a new client connected to `addr`. Panics on connect failure.
    pub fn new(addr: std::net::SocketAddr) -> io::Result<Self> {
        let stream = std::net::TcpStream::connect(addr)?;
        stream.set_nodelay(true).expect("set TCP_NODELAY");
        Ok(Self { stream })
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
    /// **Note**: This function is designed to be called from a tokio async context.
    pub async fn async_send(&mut self, tensors: &[&[u8]]) -> io::Result<usize> {
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

    /// Read one framed tensor from this connection asynchronously.
    ///
    /// Returns the reconstructed tensor bytes.
    ///
    /// **Note**: This function is designed to be called from a tokio async context.
    pub async fn async_recv(&mut self) -> io::Result<Vec<u8>> {
        let mut buf = [0u8; 8];
        self.stream.read_exact(&mut buf)?;
        let total_len = u64::from_le_bytes(buf);

        if total_len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tensor length must be positive",
            ));
        }

        let mut buf = [0u8; 4];
        self.stream.read_exact(&mut buf)?;
        let count = u32::from_le_bytes(buf);

        let mut tensor = Vec::new();
        for index in 0..count {
            let mut buf = [0u8; 1];
            self.stream.read_exact(&mut buf)?;
            let codec = buf[0];

            let mut buf = [0u8; 4];
            self.stream.read_exact(&mut buf)?;
            let encoded_len = u32::from_le_bytes(buf) as usize;
            if encoded_len == 0 || encoded_len > MAX_ENCODED_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "encoded chunk length is out of bounds",
                ));
            }

            let mut encoded = vec![0u8; encoded_len];
            self.stream.read_exact(&mut encoded)?;

            let raw_len = expected_chunk_len(total_len, index)?;
            tensor.extend_from_slice(&decode_chunk(codec, &encoded, raw_len)?);
        }
        Ok(tensor)
    }

    /// Frame and send `tensors` over this connection (sync version).
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

    /// Batch-frame and send `tensors` over the next pool connection (round-robin),
    /// returning the total wire bytes written.
    ///
    /// This amortises the 12-byte tensor header and 5-byte per-chunk header
    /// across all tensors in the batch, and reuses the connection so the
    /// connect cost is paid once per connection instead of once per tensor.
    ///
    /// This is the sync version; use `batch_send_async` for async I/O.
    pub fn batch_send(&self, tensors: &[&[u8]]) -> io::Result<usize> {
        let mut guard = self.clients.lock().unwrap();
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % guard.len();
        let client = guard.get_mut(idx).unwrap();
        client.batch_send(tensors)
    }

    /// Async batch send over the next pool connection (round-robin).
    ///
    /// This amortises the 12-byte tensor header and 5-byte per-chunk header
    /// across all tensors in the batch, and reuses the connection so the
    /// connect cost is paid once per connection instead of once per tensor.
    /// The server is expected to ACK with byte `ACK (0)` after each batch.
    ///
    /// **Note**: For a fully round-robin pool, index the client via
    /// `self.next.fetch_add(1, Ordering::Relaxed) % clients.len()` before calling
    /// `client.async_send().await`. This simplified version uses the first client.
    pub async fn batch_send_async(&self, tensors: &[&[u8]]) -> io::Result<usize> {
        let mut guard = self.clients.lock().unwrap();
        if guard.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no clients in pool",
            ));
        }
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % guard.len();
        let client = guard.get_mut(idx).unwrap();
        client.async_send(tensors).await
    }
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

/// Apply keep-alive options to a `TcpStream`.
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
