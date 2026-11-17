use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use fastlink::digest;
use fastlink::llama_rpc::{
    build_fastlink_env, build_server_args, parse_completion_sse_line, parse_fastlink_telemetry,
    run_streaming_completion, spawn_completion_stream, stream_completion, CompletionEvent,
    CompletionRequest, CompletionStream, CompletionStreamOptions, LlamaRpcConfig, LlamaRpcError,
};

fn sample_config(fastlink_v4: bool) -> LlamaRpcConfig {
    LlamaRpcConfig {
        server_bin: PathBuf::from("/tmp/llama-server"),
        model_path: PathBuf::from("/tmp/model.gguf"),
        listen_host: "127.0.0.1".into(),
        listen_port: 18890,
        rpc_servers: vec!["100.95.239.126:50052".into()],
        n_gpu_layers: 99,
        ctx_size: 2048,
        tensor_split: Some("0.7,0.3".into()),
        fastlink_v4,
        fastlink_pack: fastlink_v4,
        fastlink_rle: fastlink_v4,
        log_path: None,
    }
}

// ---------------------------------------------------------------------------
// Fake loopback llama-server harness (127.0.0.1 only, no real model/GPU).
// ---------------------------------------------------------------------------

const STOP_EVENT: &str = "data: {\"content\":\"\",\"stop\":true,\"timings\":{\"prompt_ms\":120.5,\"predicted_ms\":400.0,\"predicted_n\":32,\"predicted_per_second\":80.0}}\n\n";

fn chunked_ok_headers() -> &'static str {
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
}

fn close_delimited_ok_headers() -> &'static str {
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
}

fn http_chunk(payload: &str) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", payload.len()).into_bytes();
    out.extend_from_slice(payload.as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

fn sse_delta(text: &str) -> String {
    format!("data: {{\"content\":\"{text}\",\"stop\":false}}\n\n")
}

/// Reads a complete HTTP request (headers plus announced body) from `stream`.
fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => raw.push(byte[0]),
            _ => return raw,
        }
    }
    let header_text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    let content_length = header_text
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if stream.read_exact(&mut body).is_ok() {
        raw.extend_from_slice(&body);
    }
    raw
}

/// Accepts exactly one loopback connection and hands it to `handler`.
///
/// Joining the returned handle surfaces handler panics as test failures.
fn spawn_fake_server<F>(handler: F) -> (SocketAddr, thread::JoinHandle<()>)
where
    F: FnOnce(TcpStream, Vec<u8>) + Send + 'static,
{
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");
    let handle = thread::spawn(move || {
        let (mut stream, _peer) = listener.accept().expect("accept fake client");
        stream.set_nodelay(true).ok();
        let request = read_http_request(&mut stream);
        handler(stream, request);
    });
    (addr, handle)
}

fn options() -> CompletionStreamOptions {
    CompletionStreamOptions {
        connect_timeout: Duration::from_secs(5),
        read_timeout: Duration::from_secs(10),
        max_event_bytes: 1024 * 1024,
        max_response_bytes: 1024 * 1024,
    }
}

// ---------------------------------------------------------------------------
// Existing argument/telemetry behavior.
// ---------------------------------------------------------------------------

#[test]
fn server_args_and_env_reflect_fastlink_mode() {
    let fl_cfg = sample_config(true);
    let args = build_server_args(&fl_cfg);
    assert!(args
        .windows(2)
        .any(|w| w == ["--rpc", "100.95.239.126:50052"]));
    assert!(args.windows(2).any(|w| w == ["--tensor-split", "0.7,0.3"]));
    assert_eq!(build_fastlink_env(&fl_cfg).len(), 3);

    let stock_cfg = sample_config(false);
    assert!(build_fastlink_env(&stock_cfg).is_empty());
}

#[test]
fn parses_streaming_completion_sse_and_stop_timings() {
    let delta = "data: {\"content\":\"Hello\",\"stop\":false}\n";
    let (txt, done) = parse_completion_sse_line(delta).unwrap();
    assert_eq!(txt.as_deref(), Some("Hello"));
    assert!(done.is_none());

    let stop = "data: {\"content\":\"\",\"stop\":true,\"timings\":{\"prompt_ms\":120.5,\"predicted_ms\":400.0,\"predicted_n\":32,\"predicted_per_second\":80.0}}\n";
    let (_txt, done) = parse_completion_sse_line(stop).unwrap();
    let m = done.unwrap();
    assert_eq!(m.predicted_n, 32);
    assert!((m.prompt_ms - 120.5).abs() < 0.01);
    assert!((m.predicted_per_second - 80.0).abs() < 0.01);
}

#[test]
fn tolerant_sse_helper_decodes_escapes_and_rejects_malformed_json() {
    let escaped = "data: {\"content\":\"a\\\"b\\\\c\\u00e9\\n\",\"stop\":false}";
    let (txt, done) = parse_completion_sse_line(escaped).unwrap();
    assert_eq!(txt.as_deref(), Some("a\"b\\cé\n"));
    assert!(done.is_none());

    let root_timings =
        "data: {\"content\":\"\",\"stop\":true,\"prompt_ms\":10.0,\"predicted_ms\":20.0,\"predicted_n\":5,\"predicted_per_second\":250.0}";
    let (_txt, done) = parse_completion_sse_line(root_timings).unwrap();
    let m = done.unwrap();
    assert_eq!(m.predicted_n, 5);
    assert!((m.prompt_ms - 10.0).abs() < 0.01);
    assert!((m.predicted_ms - 20.0).abs() < 0.01);
    assert!((m.predicted_per_second - 250.0).abs() < 0.01);

    // Tolerant helper must not claim a chunk for unparseable payloads.
    assert!(parse_completion_sse_line("data: {\"content\":").is_none());
    assert!(parse_completion_sse_line("event: ping").is_none());
    assert!(parse_completion_sse_line("data: [DONE]").is_none());
    assert!(parse_completion_sse_line("data: {\"error\":{\"message\":\"x\"}}").is_none());
}

#[test]
fn parses_fastlink_rpc_telemetry_lines() {
    let log = "\
[FASTLINK-RPC-v4.1] handshake ok: server version 4.1.0, fastlink_caps=0xf1 (FASTLINK v4.1 ENABLED), transport: TCP\n\
[FASTLINK-RPC-v4] REUSE tensor blk.0.attn_q.weight (v1): raw=4194304 bytes -> 0 wire bytes\n\
[FASTLINK-RPC-v4] REUSE tensor blk.0.attn_k.weight (v1): raw=1048576 bytes -> 0 wire bytes\n\
[FASTLINK-RPC-v4] residency summary: bytes_cold=104857600 bytes_warm=5242880\n\
[FASTLINK-RPC-v4.1] transport summary: logical_ops=120 wire_batches=18\n";
    let t = parse_fastlink_telemetry(log);
    assert!(t.handshake_fastlink);
    assert_eq!(t.reuse_count, 2);
    assert_eq!(t.bytes_cold, 104_857_600);
    assert_eq!(t.bytes_warm, 5_242_880);
    assert_eq!(t.logical_ops, 120);
    assert_eq!(t.wire_batches, 18);
}

// ---------------------------------------------------------------------------
// Security: loopback-only plain HTTP, redacted Debug, sanitized errors.
// ---------------------------------------------------------------------------

#[test]
fn plain_http_client_refuses_non_loopback_endpoints_without_dialing() {
    let request = CompletionRequest::new("PROMPT-MUST-NOT-LEAK");
    let remote: SocketAddr = "192.0.2.10:8080".parse().expect("test-net ipv4");
    let remote_v6: SocketAddr = "[2001:db8::1]:8080".parse().expect("doc ipv6");

    for addr in [remote, remote_v6] {
        let err = stream_completion(addr, &request, &options(), |_event| {})
            .expect_err("non-loopback endpoint must be refused");
        assert!(
            matches!(err, LlamaRpcError::NonLoopbackEndpoint),
            "expected NonLoopbackEndpoint, got {err:?}"
        );
        let text = err.to_string();
        assert!(!text.contains("192.0.2.10"), "{text}");
        assert!(!text.contains("2001:db8"), "{text}");
        assert!(!text.contains("PROMPT-MUST-NOT-LEAK"), "{text}");

        assert!(matches!(
            CompletionStream::open(addr, &request, &options()),
            Err(LlamaRpcError::NonLoopbackEndpoint)
        ));
        assert!(matches!(
            spawn_completion_stream(addr, &request, &options()),
            Err(LlamaRpcError::NonLoopbackEndpoint)
        ));
        assert!(matches!(
            run_streaming_completion(addr, "PROMPT-MUST-NOT-LEAK", 4, 0.0, 1),
            Err(LlamaRpcError::NonLoopbackEndpoint)
        ));
    }
}

#[test]
fn completion_request_debug_redacts_the_prompt() {
    let request = CompletionRequest::new("USER-PROMPT-SECRET").with_n_predict(7);
    let rendered = format!("{request:?}");
    assert!(!rendered.contains("USER-PROMPT-SECRET"), "{rendered}");
    assert!(rendered.contains("redacted"), "{rendered}");
    assert!(rendered.contains('7'), "{rendered}");
}

#[test]
fn errors_never_echo_prompt_output_or_raw_server_lines() {
    let prompt = "PROMPT-SECRET-9911";

    // Untrusted status line / reason phrase.
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let _ = stream.write_all(
            b"HTTP/1.1 503 X-ECHO-OUTPUT-SECRET\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    });
    let err = stream_completion(addr, &CompletionRequest::new(prompt), &options(), |_| {})
        .expect_err("503 must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::HttpError(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("503"), "{text}");
    assert!(!text.contains("X-ECHO-OUTPUT-SECRET"), "{text}");
    assert!(!text.contains(prompt), "{text}");
    assert!(!text.contains("127.0.0.1"), "{text}");

    // Non-numeric status line.
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let _ = stream.write_all(b"HTTP/1.1 NOT-A-STATUS OUTPUT-SECRET\r\n\r\n");
    });
    let err = stream_completion(addr, &CompletionRequest::new(prompt), &options(), |_| {})
        .expect_err("malformed status must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
    let text = err.to_string();
    assert!(!text.contains("NOT-A-STATUS"), "{text}");
    assert!(!text.contains("OUTPUT-SECRET"), "{text}");
    assert!(!text.contains("127.0.0.1"), "{text}");

    // Malformed JSON payload must not be echoed.
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        stream
            .write_all(&http_chunk("data: {\"content\": OUTPUT-SECRET}\n\n"))
            .unwrap();
        stream.write_all(b"0\r\n\r\n").unwrap();
    });
    let err = stream_completion(addr, &CompletionRequest::new(prompt), &options(), |_| {})
        .expect_err("malformed JSON must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.to_lowercase().contains("json"), "{text}");
    assert!(!text.contains("OUTPUT-SECRET"), "{text}");

    // Malformed chunk-size line must not be echoed.
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        let _ = stream.write_all(b"zzOUTPUT-SECRET\r\n");
    });
    let err = stream_completion(addr, &CompletionRequest::new(prompt), &options(), |_| {})
        .expect_err("bad chunk size must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
    let text = err.to_string();
    assert!(!text.contains("OUTPUT-SECRET"), "{text}");
}

// ---------------------------------------------------------------------------
// Bounds: status/header lines, total response bytes, chunk framing.
// ---------------------------------------------------------------------------

#[test]
fn oversized_status_and_header_lines_are_bounded_without_echo() {
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let mut line = b"HTTP/1.1 200 ".to_vec();
        line.extend(std::iter::repeat_n(b'x', 64 * 1024));
        line.extend_from_slice(b"\r\n\r\n");
        let _ = stream.write_all(&line);
    });
    let err = stream_completion(addr, &CompletionRequest::new("big"), &options(), |_| {})
        .expect_err("oversized status line must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
    assert!(!err.to_string().contains("xxxxxxxx"), "{err}");

    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nX-Oversized: ");
        let _ = stream.write_all(&vec![b'y'; 64 * 1024]);
        let _ = stream.write_all(b"\r\n\r\n");
    });
    let err = stream_completion(addr, &CompletionRequest::new("big"), &options(), |_| {})
        .expect_err("oversized header line must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
    assert!(!err.to_string().contains("yyyyyyyy"), "{err}");
}

#[test]
fn total_response_bytes_are_capped_before_buffering() {
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let _ = stream.write_all(chunked_ok_headers().as_bytes());
        for _ in 0..64 {
            if stream
                .write_all(&http_chunk(
                    "data: {\"content\":\"0123456789\",\"stop\":false}\n\n",
                ))
                .is_err()
            {
                return;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    });
    let tight = CompletionStreamOptions {
        max_response_bytes: 512,
        ..options()
    };
    let err = stream_completion(addr, &CompletionRequest::new("cap"), &tight, |_| {})
        .expect_err("response cap must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
    assert!(err.to_string().contains("512"), "{err}");

    // A chunk declaration beyond the cap must fail before allocating it.
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let _ = stream.write_all(chunked_ok_headers().as_bytes());
        let _ = stream.write_all(b"ffffffff\r\n");
    });
    let err = stream_completion(addr, &CompletionRequest::new("cap"), &options(), |_| {})
        .expect_err("oversized chunk must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
}

#[test]
fn truncated_and_malformed_http_framing_is_rejected() {
    // Headers end at EOF without the blank line.
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n");
    });
    let err = stream_completion(addr, &CompletionRequest::new("cut"), &options(), |_| {})
        .expect_err("truncated headers must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");

    // Invalid chunk-size line.
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        let _ = stream.write_all(b"zz\r\n");
    });
    let err = stream_completion(addr, &CompletionRequest::new("cut"), &options(), |_| {})
        .expect_err("invalid chunk size must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
    assert!(!err.to_string().contains("zz"), "{err}");

    // Missing chunk terminator.
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        let _ = stream.write_all(b"4\r\ndataXX");
    });
    let err = stream_completion(addr, &CompletionRequest::new("cut"), &options(), |_| {})
        .expect_err("bad chunk terminator must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");

    // Truncated trailer sequence after the terminating zero-size chunk.
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        let _ = stream.write_all(b"0\r\n");
    });
    let err = stream_completion(addr, &CompletionRequest::new("cut"), &options(), |_| {})
        .expect_err("truncated trailers must fail");
    server.join().expect("server script");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
}

// ---------------------------------------------------------------------------
// SSE events: boundaries, multiline/empty data, error events, [DONE].
// ---------------------------------------------------------------------------

#[test]
fn parses_events_by_boundary_with_multiline_and_empty_data() {
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let mut out = close_delimited_ok_headers().to_string();
        out.push_str(": keep-alive comment\r\n\r\n");
        out.push_str("event: message\r\n");
        out.push_str("data:{\"content\":\"A\",\r\n");
        out.push_str("data: \"stop\":false}\r\n");
        out.push_str("\r\n");
        out.push_str("data:\r\n\r\n"); // legal empty data event: ignored
        out.push_str("id: 7\r\nretry: 100\r\n\r\n");
        out.push_str(&sse_delta("B"));
        out.push_str(STOP_EVENT);
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });

    let mut texts = Vec::new();
    let metrics = stream_completion(
        addr,
        &CompletionRequest::new("multi"),
        &options(),
        |event| {
            if let CompletionEvent::Delta { text, .. } = event {
                texts.push(text);
            }
        },
    )
    .expect("event boundaries, comments and multiline data are tolerated");
    server.join().expect("server script");
    assert_eq!(texts, vec!["A".to_string(), "B".to_string()]);
    assert_eq!(metrics.content, "AB");
    assert_eq!(metrics.content_digest, digest(b"AB"));
    assert_eq!(metrics.predicted_n, 32);
}

#[test]
fn event_error_and_json_error_payloads_fail_promptly_and_sanitized() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream
            .write_all(close_delimited_ok_headers().as_bytes())
            .unwrap();
        stream
            .write_all(b"event: error\r\ndata: {\"message\":\"OUTPUT-SECRET\"}\r\n\r\n")
            .unwrap();
        stream.flush().unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
    });
    let started = Instant::now();
    let err = stream_completion(addr, &CompletionRequest::new("err"), &options(), |_| {})
        .expect_err("event:error must fail");
    assert!(matches!(err, LlamaRpcError::CompletionError), "{err:?}");
    assert!(!err.to_string().contains("OUTPUT-SECRET"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "event:error must be reported promptly"
    );
    release_tx.send(()).expect("release server");
    server.join().expect("server script");

    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream
            .write_all(close_delimited_ok_headers().as_bytes())
            .unwrap();
        stream
            .write_all(b"data: {\"error\":{\"message\":\"OUTPUT-SECRET\"}}\n\n")
            .unwrap();
        stream.flush().unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
    });
    let started = Instant::now();
    let err = stream_completion(addr, &CompletionRequest::new("err"), &options(), |_| {})
        .expect_err("JSON error payload must fail");
    assert!(matches!(err, LlamaRpcError::CompletionError), "{err:?}");
    assert!(!err.to_string().contains("OUTPUT-SECRET"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2));
    release_tx.send(()).expect("release server");
    server.join().expect("server script");
}

#[test]
fn done_marker_finishes_without_waiting_for_connection_close() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream
            .write_all(close_delimited_ok_headers().as_bytes())
            .unwrap();
        let mut body = sse_delta("A");
        body.push_str(STOP_EVENT);
        body.push_str("data: [DONE]\n\n");
        stream.write_all(body.as_bytes()).unwrap();
        stream.flush().unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
    });
    let mut stream = CompletionStream::open(
        addr,
        &CompletionRequest::new("done").with_n_predict(1),
        &options(),
    )
    .expect("connect");
    let mut finished = false;
    while let Some(event) = stream.next_event().expect("event") {
        if matches!(event, CompletionEvent::Finished { .. }) {
            finished = true;
        }
    }
    assert!(finished, "final metrics must arrive");
    release_tx.send(()).expect("release server");
    server.join().expect("server script");

    // [DONE] without a final stop event must fail immediately, not wait for EOF.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream
            .write_all(close_delimited_ok_headers().as_bytes())
            .unwrap();
        let mut body = sse_delta("A");
        body.push_str("data: [DONE]\n\n");
        stream.write_all(body.as_bytes()).unwrap();
        stream.flush().unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
    });
    let started = Instant::now();
    let err = stream_completion(addr, &CompletionRequest::new("done"), &options(), |_| {})
        .expect_err("[DONE] without stop must fail");
    assert!(matches!(err, LlamaRpcError::ParseError(_)), "{err:?}");
    assert!(err.to_string().contains("stop"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2));
    release_tx.send(()).expect("release server");
    server.join().expect("server script");
}

#[test]
fn final_event_without_trailing_blank_line_still_completes() {
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let mut out = close_delimited_ok_headers().to_string();
        out.push_str(&sse_delta("A"));
        out.push_str(STOP_EVENT.trim_end_matches("\n\n"));
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });

    let metrics = stream_completion(addr, &CompletionRequest::new("tail"), &options(), |_| {})
        .expect("a final event without its blank line must still complete");
    server.join().expect("server script");
    assert_eq!(metrics.content, "A");
    assert_eq!(metrics.predicted_n, 32);
}

// ---------------------------------------------------------------------------
// Streaming callback/event API.
// ---------------------------------------------------------------------------

#[test]
fn streams_ordered_deltas_immediately_then_final_metrics() {
    // The fake server only releases the final stop event after the client
    // callback has already observed the matching delta. A buffering client
    // (one that waits for the whole body) would deadlock and fail here.
    let (ack_tx, ack_rx) = mpsc::channel::<u64>();
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        // Split an SSE line across two HTTP chunks to exercise re-assembly.
        let first = sse_delta("Hel");
        let (head, tail) = first.split_at(20);
        stream.write_all(&http_chunk(head)).unwrap();
        stream.write_all(&http_chunk(tail)).unwrap();
        stream.flush().unwrap();
        ack_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("client callback must fire before the next event is sent");
        for token in ["lo", " world"] {
            stream.write_all(&http_chunk(&sse_delta(token))).unwrap();
            stream.flush().unwrap();
            ack_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("client callback must fire per delta");
        }
        stream.write_all(&http_chunk(STOP_EVENT)).unwrap();
        stream.write_all(b"0\r\n\r\n").unwrap();
        stream.flush().unwrap();
    });

    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    let signed = Arc::clone(&events);
    let metrics = stream_completion(
        addr,
        &CompletionRequest::new("say hello").with_n_predict(8),
        &options(),
        move |event| match &event {
            CompletionEvent::Delta {
                index,
                text,
                ttft_ms,
            } => {
                assert!(*ttft_ms >= 0.0);
                signed.lock().unwrap().push(format!("{index}:{text}"));
                ack_tx.send(*index).unwrap();
            }
            CompletionEvent::Finished { metrics } => {
                assert_eq!(metrics.predicted_n, 32);
                observed.lock().unwrap().push("finished".into());
            }
        },
    )
    .expect("streaming completion succeeds");
    server.join().expect("fake server script completed");

    assert_eq!(
        events.lock().unwrap().clone(),
        vec!["0:Hel", "1:lo", "2: world", "finished"]
    );
    assert_eq!(metrics.content, "Hello world");
    assert_eq!(metrics.content_digest, digest(b"Hello world"));
    assert_eq!(metrics.predicted_n, 32);
    assert!(metrics.ttft_ms >= 0.0);
    assert!(metrics.total_wall_ms >= metrics.ttft_ms);
    assert!((metrics.prompt_ms - 120.5).abs() < 0.01);
    assert!((metrics.predicted_ms - 400.0).abs() < 0.01);
    assert!((metrics.predicted_per_second - 80.0).abs() < 0.01);
}

#[test]
fn run_streaming_completion_keeps_aggregate_behavior() {
    let captured = Arc::new(Mutex::new(None));
    let request_slot = Arc::clone(&captured);
    let (addr, server) = spawn_fake_server(move |mut stream, request| {
        *request_slot.lock().unwrap() = Some(request);
        let mut body = String::new();
        body.push_str(&sse_delta("Hello"));
        body.push_str(&sse_delta(" world"));
        body.push_str(STOP_EVENT);
        let mut out = close_delimited_ok_headers().to_string();
        out.push_str(&body);
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });

    let metrics = run_streaming_completion(addr, "hello", 8, 0.0, 42).expect("aggregate run");
    server.join().expect("fake server script completed");

    assert_eq!(metrics.content, "Hello world");
    assert_eq!(metrics.content_digest, digest(b"Hello world"));
    assert_eq!(metrics.predicted_n, 32);
    assert!(metrics.ttft_ms >= 0.0);
    assert!(metrics.total_wall_ms >= metrics.ttft_ms);

    let request = captured.lock().unwrap().clone().expect("request captured");
    let request = String::from_utf8_lossy(&request);
    assert!(request.starts_with("POST /completion HTTP/1.1\r\n"));
    assert!(request.contains("\"stream\":true"));
    assert!(request.contains("\"prompt\":\"hello\""));
    assert!(request.contains("\"n_predict\":8"));
}

#[test]
fn pull_api_yields_delta_then_finished_then_end_of_stream() {
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let mut out = close_delimited_ok_headers().to_string();
        out.push_str(&sse_delta("A"));
        out.push_str(&sse_delta("B"));
        out.push_str(STOP_EVENT);
        out.push_str("data: [DONE]\n\n");
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });

    let mut stream = CompletionStream::open(
        addr,
        &CompletionRequest::new("ab").with_n_predict(2),
        &options(),
    )
    .expect("connect");
    server.join().expect("fake server script completed");

    let mut texts = Vec::new();
    let mut finished = None;
    loop {
        match stream.next_event().expect("event") {
            Some(CompletionEvent::Delta { index, text, .. }) => {
                assert_eq!(index as usize, texts.len());
                texts.push(text);
            }
            Some(CompletionEvent::Finished { metrics }) => finished = Some(metrics),
            None => break,
        }
    }
    assert_eq!(texts, vec!["A".to_string(), "B".to_string()]);
    let metrics = finished.expect("finished event");
    assert_eq!(metrics.content, "AB");
    assert_eq!(metrics.content_digest, digest(b"AB"));
    assert!(stream.is_finished());
}

#[test]
fn channel_api_forwards_deltas_to_a_consumer_thread() {
    let (addr, server) = spawn_fake_server(|mut stream, _req| {
        let mut out = chunked_ok_headers().to_string();
        for token in ["one", "two", "three"] {
            out.push_str(&String::from_utf8(http_chunk(&sse_delta(token))).unwrap());
        }
        out.push_str(&String::from_utf8(http_chunk(STOP_EVENT)).unwrap());
        out.push_str("0\r\n\r\n");
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });

    let handle = spawn_completion_stream(
        addr,
        &CompletionRequest::new("count").with_n_predict(3),
        &options(),
    )
    .expect("spawn stream");

    let mut received = Vec::new();
    for event in handle.events() {
        if let CompletionEvent::Delta { index, text, .. } = event {
            received.push((index, text));
        }
    }
    server.join().expect("fake server script completed");
    let metrics = handle.join().expect("joined stream");
    assert_eq!(
        received,
        vec![
            (0, "one".to_string()),
            (1, "two".to_string()),
            (2, "three".to_string())
        ]
    );
    assert_eq!(metrics.content, "onetwothree");
    assert_eq!(metrics.content_digest, digest(b"onetwothree"));
}

// ---------------------------------------------------------------------------
// Malformed / hostile responses.
// ---------------------------------------------------------------------------

#[test]
fn malformed_sse_json_reports_parse_error_after_delivering_valid_deltas() {
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        let mut out = chunked_ok_headers().to_string();
        out.push_str(&String::from_utf8(http_chunk(&sse_delta("ok"))).unwrap());
        out.push_str(&String::from_utf8(http_chunk("data: {\"content\": broken}\n\n")).unwrap());
        out.push_str("0\r\n\r\n");
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });

    let deltas = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&deltas);
    let err = stream_completion(
        addr,
        &CompletionRequest::new("bad"),
        &options(),
        move |event| {
            if let CompletionEvent::Delta { text, .. } = event {
                sink.lock().unwrap().push(text);
            }
        },
    )
    .expect_err("malformed JSON must fail the stream");
    server.join().expect("fake server script completed");

    assert!(
        matches!(err, LlamaRpcError::ParseError(_)),
        "expected ParseError, got {err:?}"
    );
    assert!(err.to_string().to_lowercase().contains("json"), "{err}");
    assert_eq!(deltas.lock().unwrap().clone(), vec!["ok".to_string()]);
}

#[test]
fn disconnect_before_final_event_is_an_error() {
    // Connection-close framed stream that ends without stop:true.
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        let mut out = close_delimited_ok_headers().to_string();
        out.push_str(&sse_delta("partial"));
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });
    let err = stream_completion(addr, &CompletionRequest::new("cut"), &options(), |_| {})
        .expect_err("missing stop event must fail");
    server.join().expect("fake server script completed");
    assert!(
        matches!(err, LlamaRpcError::ParseError(_)),
        "expected ParseError, got {err:?}"
    );
    assert!(err.to_string().contains("stop"), "{err}");

    // Truncated chunked stream: chunk declares more data than delivered.
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        stream
            .write_all(b"40\r\ndata: {\"content\":\"cut\"")
            .unwrap();
        stream.flush().unwrap();
    });
    let err = stream_completion(addr, &CompletionRequest::new("cut"), &options(), |_| {})
        .expect_err("truncated chunked stream must fail");
    server.join().expect("fake server script completed");
    assert!(
        matches!(
            err,
            LlamaRpcError::Io(_) | LlamaRpcError::ParseError(_) | LlamaRpcError::HttpError(_)
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn non_200_response_is_reported_as_http_error() {
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        stream.flush().unwrap();
    });
    let err = stream_completion(addr, &CompletionRequest::new("nope"), &options(), |_| {})
        .expect_err("non-200 must be rejected");
    server.join().expect("fake server script completed");
    match err {
        LlamaRpcError::HttpError(msg) => assert!(msg.contains("503"), "{msg}"),
        other => panic!("expected HttpError, got {other:?}"),
    }
}

#[test]
fn oversized_event_is_bounded_and_rejected() {
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        let huge = "x".repeat(4096);
        let mut out = close_delimited_ok_headers().to_string();
        out.push_str(&format!(
            "data: {{\"content\":\"{huge}\",\"stop\":false}}\n\n"
        ));
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });
    let tight = CompletionStreamOptions {
        max_event_bytes: 256,
        ..options()
    };
    let err = stream_completion(addr, &CompletionRequest::new("huge"), &tight, |_| {})
        .expect_err("oversized event must be rejected");
    server.join().expect("fake server script completed");
    assert!(
        matches!(err, LlamaRpcError::ParseError(_)),
        "expected ParseError, got {err:?}"
    );
    assert!(err.to_string().to_lowercase().contains("exceed"), "{err}");

    // Accumulated multiline data must respect the same bound.
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        let mut out = close_delimited_ok_headers().to_string();
        for _ in 0..8 {
            out.push_str(&format!("data: {}\n", "y".repeat(100)));
        }
        out.push('\n');
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });
    let err = stream_completion(addr, &CompletionRequest::new("huge"), &tight, |_| {})
        .expect_err("oversized multiline event must be rejected");
    server.join().expect("fake server script completed");
    assert!(
        matches!(err, LlamaRpcError::ParseError(_)),
        "expected ParseError, got {err:?}"
    );
    assert!(err.to_string().to_lowercase().contains("exceed"), "{err}");
}

// ---------------------------------------------------------------------------
// Worker-thread spawning, cancellation and time bounds.
// ---------------------------------------------------------------------------

#[test]
fn spawn_returns_before_connect_and_cancel_interrupts_a_stalled_read() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (addr, server) = spawn_fake_server(move |_stream, _req| {
        // Never send response headers: the worker must stay blocked in read.
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });

    let started = Instant::now();
    let handle = spawn_completion_stream(
        addr,
        &CompletionRequest::new("stall").with_n_predict(4),
        &options(),
    )
    .expect("loopback endpoint accepted");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "spawn must not block on connect/response headers"
    );

    // Give the worker time to block inside the header read.
    thread::sleep(Duration::from_millis(150));
    let started = Instant::now();
    handle.cancel();
    let err = handle
        .join()
        .expect_err("cancelled worker reports an error");
    assert!(
        matches!(err, LlamaRpcError::StreamCancelled),
        "expected StreamCancelled, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cancel must interrupt a stalled socket read"
    );

    release_tx.send(()).expect("release server");
    server.join().expect("server script");
}

#[test]
fn idle_stalled_stream_is_bounded_by_the_read_timeout() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream
            .write_all(close_delimited_ok_headers().as_bytes())
            .unwrap();
        stream.flush().unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });

    let quick = CompletionStreamOptions {
        read_timeout: Duration::from_millis(200),
        ..options()
    };
    let started = Instant::now();
    let handle = spawn_completion_stream(
        addr,
        &CompletionRequest::new("idle").with_n_predict(4),
        &quick,
    )
    .expect("spawn stream");
    let err = handle.join().expect_err("idle stream must time out");
    assert!(
        matches!(err, LlamaRpcError::Io(_) | LlamaRpcError::ParseError(_)),
        "expected a bounded timeout error, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "read timeout must bound a stalled stream"
    );

    release_tx.send(()).expect("release server");
    server.join().expect("server script");
}

#[test]
fn dropping_the_receiver_does_not_cancel_an_idle_read_but_cancel_does() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream
            .write_all(close_delimited_ok_headers().as_bytes())
            .unwrap();
        stream.flush().unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });

    let handle = spawn_completion_stream(
        addr,
        &CompletionRequest::new("idle").with_n_predict(4),
        &options(),
    )
    .expect("spawn stream");
    let (events, cancel, worker) = handle.into_parts();
    drop(events);

    // Documented semantics: dropping the receiver alone does not interrupt a
    // blocked socket read; an explicit cancel or the read timeout is required.
    thread::sleep(Duration::from_millis(300));
    assert!(
        !worker.is_finished(),
        "worker must still be blocked in the idle read"
    );
    cancel.store(true, Ordering::SeqCst);
    let err = worker
        .join()
        .expect("worker thread did not panic")
        .expect_err("cancelled worker reports an error");
    assert!(
        matches!(err, LlamaRpcError::StreamCancelled),
        "expected StreamCancelled, got {err:?}"
    );

    release_tx.send(()).expect("release server");
    server.join().expect("server script");
}

#[test]
fn dropping_the_receiver_stops_delivery_at_the_next_event() {
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        for _ in 0..256 {
            if stream.write_all(&http_chunk(&sse_delta("t"))).is_err() {
                return;
            }
            stream.flush().ok();
            thread::sleep(Duration::from_millis(1));
        }
        let _ = stream.write_all(&http_chunk(STOP_EVENT));
        let _ = stream.write_all(b"0\r\n\r\n");
    });

    let handle = spawn_completion_stream(
        addr,
        &CompletionRequest::new("cancel").with_n_predict(256),
        &options(),
    )
    .expect("spawn stream");
    let (events, _cancel, worker) = handle.into_parts();
    drop(events);
    let err = worker
        .join()
        .expect("worker thread did not panic")
        .expect_err("worker stops when delivery fails");
    assert!(
        matches!(err, LlamaRpcError::StreamCancelled),
        "expected StreamCancelled, got {err:?}"
    );
    server.join().expect("server script");
}

#[test]
fn completion_events_do_not_debug_print_model_output() {
    let event = CompletionEvent::Delta {
        index: 3,
        text: "MODEL-OUTPUT-SECRET".to_string(),
        ttft_ms: 1.0,
    };
    let rendered = format!("{event:?}");
    assert!(!rendered.contains("MODEL-OUTPUT-SECRET"), "{rendered}");
    assert!(rendered.contains('3'), "{rendered}");
}

#[test]
fn atomic_flag_is_shared_with_into_parts() {
    // Guards the documented into_parts contract used by advanced callers.
    let handle = {
        let (addr, server) = spawn_fake_server(move |mut stream, _req| {
            let mut out = close_delimited_ok_headers().to_string();
            out.push_str(STOP_EVENT);
            stream.write_all(out.as_bytes()).unwrap();
            stream.flush().unwrap();
        });
        let handle = spawn_completion_stream(
            addr,
            &CompletionRequest::new("flag").with_n_predict(1),
            &options(),
        )
        .expect("spawn stream");
        server.join().expect("server script");
        handle
    };
    let (events, cancel, worker) = handle.into_parts();
    assert!(!cancel.load(Ordering::SeqCst));
    // Keep the receiver alive while draining; a dropped receiver turns the
    // worker's final send into StreamCancelled by design.
    while events.recv().is_ok() {}
    let metrics = worker
        .join()
        .expect("worker did not panic")
        .expect("stream");
    assert_eq!(metrics.content, "");
    let _: Arc<AtomicBool> = cancel;
}
