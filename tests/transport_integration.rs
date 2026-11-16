//! Integration tests for the lightweight FASTLINK-RPC/1 TCP transport.

use fastlink::rpc_client::frame_chunk;
use fastlink::transport::{receive_tensor, send_tensor, MAX_TENSOR_BYTES};
use fastlink::{write_u32, write_u64, CHUNK_BYTES, MAX_ENCODED_BYTES};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

/// Deterministic, effectively incompressible bytes (splitmix64).
fn patterned_tensor(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    (0..len)
        .map(|_| {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) as u8
        })
        .collect()
}

/// One-shot loopback server: receive one tensor, ACK it, return the bytes.
fn spawn_receiver() -> (std::net::SocketAddr, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let handle = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let received = receive_tensor(&mut sock).expect("receive tensor");
        sock.write_all(&[0]).expect("ack");
        received
    });
    (addr, handle)
}

fn assert_roundtrip(tensor: &[u8]) {
    let (addr, server) = spawn_receiver();
    let mut client = TcpStream::connect(addr).expect("connect");
    client.set_nodelay(true).expect("client TCP_NODELAY");

    let sent = send_tensor(&mut client, tensor).expect("send tensor");
    let mut ack = [0u8; 1];
    client.read_exact(&mut ack).expect("read ack");
    assert_eq!(ack[0], 0, "server must ack");
    // Compression can make the wire smaller than the raw tensor, so only the
    // positive counter is checked here; the raw-length bound is covered by the
    // incompressible latency sanity test below.
    assert!(sent > 0, "wire byte counter must be positive");

    let received = server.join().expect("server thread");
    assert_eq!(received.as_slice(), tensor, "round-trip must be byte-exact");
}

#[test]
fn roundtrip_single_chunk_byte_exact() {
    assert_roundtrip(&patterned_tensor(CHUNK_BYTES));
    assert_roundtrip(&vec![0xAB; CHUNK_BYTES]);
}

#[test]
fn roundtrip_multi_chunk_byte_exact() {
    let mut mixed = vec![0xAB; CHUNK_BYTES];
    mixed.extend_from_slice(&patterned_tensor(CHUNK_BYTES));
    mixed.extend_from_slice(&[7u8; 1_000]);
    assert_roundtrip(&mixed);
    assert_roundtrip(&patterned_tensor(CHUNK_BYTES * 2 + 1));
    assert_roundtrip(&[0u8; 1]);
}

#[test]
fn roundtrip_8mib_byte_exact() {
    assert_roundtrip(&patterned_tensor(8 * 1024 * 1024));
}

#[test]
fn send_rejects_empty_tensor() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1];
        let _ = sock.read(&mut buf);
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    assert!(
        send_tensor(&mut client, &[]).is_err(),
        "empty tensor must be rejected"
    );
    drop(client);
    let _ = server.join();
}

#[test]
fn truncated_tensor_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        receive_tensor(&mut sock)
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    // The header promises two full chunks but only the first payload follows.
    write_u64(&mut client, (CHUNK_BYTES * 2) as u64).expect("write length");
    write_u32(&mut client, 2).expect("write count");
    let chunk = frame_chunk(0, &patterned_tensor(CHUNK_BYTES)).expect("frame chunk");
    client
        .write_all(&chunk.adapter_payload())
        .expect("write chunk");
    client.flush().expect("flush");
    client.shutdown(Shutdown::Write).expect("shutdown");
    assert!(
        server.join().expect("server thread").is_err(),
        "truncated tensor must be rejected"
    );
}

#[test]
fn roundtrip_exact_max_tensor_bytes_cap() {
    assert_eq!(MAX_TENSOR_BYTES, CHUNK_BYTES * 1024);
    assert_eq!(MAX_TENSOR_BYTES, 64 * 1024 * 1024);

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let received = receive_tensor(&mut sock).expect("receive exact-cap tensor");
        assert_eq!(received.len(), MAX_TENSOR_BYTES);
        assert!(received.iter().all(|&byte| byte == 0xA5));
        sock.write_all(&[0]).expect("ack");
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    client.set_nodelay(true).expect("client TCP_NODELAY");
    // Uniform bytes compress cleanly via RLE so the wire frame stays compact (~521 KiB).
    let tensor = vec![0xA5u8; MAX_TENSOR_BYTES];
    let sent = send_tensor(&mut client, &tensor).expect("send exact-cap tensor");
    assert!(sent > 0);
    let mut ack = [0u8; 1];
    client.read_exact(&mut ack).expect("read ack");
    assert_eq!(ack[0], 0);
    server.join().expect("server thread");
}

#[test]
fn oversized_header_is_rejected_before_tensor_payload_read() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("set read timeout");
        receive_tensor(&mut sock)
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    // Send only the oversized 8-byte total_len declaration and keep the socket
    // open (no chunk_count, payload, or FIN) so the receiver must fail promptly
    // with InvalidData rather than blocking until timeout or EOF.
    let declared_len = MAX_TENSOR_BYTES as u64 + 1;
    write_u64(&mut client, declared_len).expect("write length");
    client.flush().expect("flush");

    let error = server
        .join()
        .expect("server thread")
        .expect_err("oversized header must fail");
    drop(client);
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        error
            .to_string()
            .contains("tensor length exceeds transport limit"),
        "receiver must reject the declaration before waiting for count/payload/EOF: {error}"
    );
}

#[test]
fn sender_rejects_tensor_over_buffered_transport_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let drain = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut wire = Vec::new();
        sock.read_to_end(&mut wire).expect("drain request");
        wire.len()
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    let tensor = vec![0u8; MAX_TENSOR_BYTES + 1];
    let error = send_tensor(&mut client, &tensor).expect_err("oversized local tensor must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    drop(client);
    assert_eq!(
        drain.join().expect("drain thread"),
        0,
        "rejected send writes no bytes"
    );
}

#[cfg(feature = "rpc-v3-transport")]
fn drain_batch_and_ack(listener: TcpListener) -> usize {
    use fastlink::{read_u32, read_u8};

    let (mut sock, _) = listener.accept().expect("accept");
    sock.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut first = [0u8; 1];
    let n = sock.read(&mut first).expect("read first byte");
    if n == 0 {
        return 0;
    }

    let mut rest_of_len = [0u8; 7];
    sock.read_exact(&mut rest_of_len).expect("read rest of u64");
    let mut total_len_bytes = [0u8; 8];
    total_len_bytes[0] = first[0];
    total_len_bytes[1..].copy_from_slice(&rest_of_len);
    let _total_len = u64::from_le_bytes(total_len_bytes);
    let count = read_u32(&mut sock).expect("read count");
    let mut observed = 12usize;
    for _ in 0..count {
        let _codec = read_u8(&mut sock).expect("read codec");
        let encoded_len = read_u32(&mut sock).expect("read encoded_len") as usize;
        let mut chunk = vec![0u8; encoded_len];
        sock.read_exact(&mut chunk).expect("read chunk payload");
        observed += 5 + encoded_len;
    }
    sock.write_all(&[fastlink::ACK]).expect("send ack");
    observed
}

#[cfg(feature = "rpc-v3-transport")]
#[test]
fn async_client_batch_send_rejects_tensor_over_transport_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let drain = thread::spawn(move || drain_batch_and_ack(listener));

    let mut client = fastlink::AsyncClient::new(addr).expect("connect");
    let tensor = vec![0u8; MAX_TENSOR_BYTES + 1];
    let error = client
        .batch_send(&[&tensor])
        .expect_err("oversized tensor must fail in batch_send");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        error
            .to_string()
            .contains("tensor length exceeds transport limit"),
        "unexpected error: {error}"
    );
    assert_eq!(client.bytes_sent(), 0);
    drop(client);
    assert_eq!(
        drain.join().expect("drain thread"),
        0,
        "rejected batch_send writes no bytes"
    );
}

#[cfg(feature = "rpc-v3-transport")]
#[test]
fn async_client_batch_send_rejects_oversized_tensor_in_multi_item_batch_without_writing_prior_items(
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let drain = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let mut first = [0u8; 1];
        if sock.read(&mut first).expect("read") == 0 {
            return 0usize;
        }
        // Fallback if baseline ever writes partial multi-tensor batch.
        let _ = sock.write_all(&[fastlink::ACK]);
        1usize
    });

    let mut client = fastlink::AsyncClient::new(addr).expect("connect");
    let ok_tensor = vec![0x11u8; CHUNK_BYTES];
    let oversized = vec![0u8; MAX_TENSOR_BYTES + 1];
    let error = client
        .batch_send(&[&ok_tensor, &oversized])
        .expect_err("multi-item batch containing oversized tensor must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(client.bytes_sent(), 0);
    drop(client);
    assert_eq!(
        drain.join().expect("drain thread"),
        0,
        "rejected batch must write no wire bytes even for preceding small tensors"
    );
}

#[cfg(feature = "rpc-v3-transport")]
#[test]
fn async_client_batch_send_accepts_exact_max_tensor_bytes_cap() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        let received = receive_tensor(&mut sock).expect("receive exact-cap batch tensor");
        assert_eq!(received.len(), MAX_TENSOR_BYTES);
        assert!(received.iter().all(|&byte| byte == 0xA5));
        sock.write_all(&[fastlink::ACK]).expect("ack");
    });

    let mut client = fastlink::AsyncClient::new(addr).expect("connect");
    let tensor = vec![0xA5u8; MAX_TENSOR_BYTES];
    let sent = client
        .batch_send(&[&tensor])
        .expect("exact-cap tensor must succeed in batch_send");
    assert!(sent > 0);
    assert_eq!(client.bytes_sent(), sent);
    server.join().expect("server thread");
}

/// Start a server that expects `receive_tensor` to fail, then send `payload`
/// and confirm the read is rejected.
fn assert_rejected(payload: &[u8]) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        receive_tensor(&mut sock)
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    client.write_all(payload).expect("write payload");
    client.flush().expect("flush");
    client.shutdown(Shutdown::Write).expect("shutdown");
    assert!(
        server.join().expect("server thread").is_err(),
        "invalid tensor must be rejected"
    );
}

#[test]
fn rejects_invalid_tensor_headers() {
    // Zero length.
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u64.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());
    assert_rejected(&payload);

    // Chunk count does not match the length.
    let mut payload = Vec::new();
    payload.extend_from_slice(&(CHUNK_BYTES as u64).to_le_bytes());
    payload.extend_from_slice(&2u32.to_le_bytes());
    assert_rejected(&payload);

    // Encoded length beyond the codec bound.
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u64.to_le_bytes());
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.push(0); // RAW
    payload.extend_from_slice(&((MAX_ENCODED_BYTES + 1) as u32).to_le_bytes());
    assert_rejected(&payload);
}

#[test]
fn loopback_latency_and_byte_counter_sanity() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_nodelay(true).expect("server TCP_NODELAY");
        loop {
            match receive_tensor(&mut sock) {
                Ok(_) => {
                    if sock.write_all(&[0]).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let mut client = TcpStream::connect(addr).expect("connect");
    client.set_nodelay(true).expect("client TCP_NODELAY");
    let tensor = patterned_tensor(CHUNK_BYTES);
    let rounds = 64;
    let mut sent = 0usize;
    let mut ack = [0u8; 1];
    let started = Instant::now();
    for _ in 0..rounds {
        sent += send_tensor(&mut client, &tensor).expect("send tensor");
        client.read_exact(&mut ack).expect("read ack");
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed > Duration::ZERO,
        "loopback round-trip must be measurable"
    );
    assert!(
        sent >= tensor.len() * rounds,
        "byte counter {sent} must cover {} raw bytes",
        tensor.len() * rounds
    );
    drop(client);
    let _ = server.join();
}
