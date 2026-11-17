use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
/// The returned handle lets callers join the server thread so handler panics
/// (test assertions inside the scripted response) fail the test instead of
/// being silently lost.
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
        max_event_bytes: 8192,
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
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
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
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
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
    let err = stream_completion(
        addr,
        &CompletionRequest::new("cut"),
        &options(),
        |_event| {},
    )
    .expect_err("missing stop event must fail");
    server.join().expect("fake server script completed");
    assert!(
        matches!(err, LlamaRpcError::ParseError(_)),
        "expected ParseError, got {err:?}"
    );
    assert!(err.to_string().contains("stop"), "{err}");

    // Truncated chunked stream: chunk header promises more data than delivered.
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        stream
            .write_all(b"40\r\ndata: {\"content\":\"cut\"")
            .unwrap();
        stream.flush().unwrap();
    });
    let err = stream_completion(
        addr,
        &CompletionRequest::new("cut"),
        &options(),
        |_event| {},
    )
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
    let err = stream_completion(
        addr,
        &CompletionRequest::new("nope"),
        &options(),
        |_event| {},
    )
    .expect_err("non-200 must be rejected");
    server.join().expect("fake server script completed");
    match err {
        LlamaRpcError::HttpError(msg) => assert!(msg.contains("503"), "{msg}"),
        other => panic!("expected HttpError, got {other:?}"),
    }
}

#[test]
fn accepts_sse_comments_crlf_and_multiline_data_fields() {
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        let mut out = close_delimited_ok_headers().to_string();
        out.push_str(": keep-alive comment\r\n\r\n");
        out.push_str("event: token\r\n");
        out.push_str("data:{\"content\":\"A\",\r\n");
        out.push_str("data: \"stop\":false}\r\n");
        out.push_str("\r\n");
        out.push_str(STOP_EVENT);
        stream.write_all(out.as_bytes()).unwrap();
        stream.flush().unwrap();
    });

    let metrics = stream_completion(
        addr,
        &CompletionRequest::new("multi"),
        &options(),
        |_event| {},
    )
    .expect("comments and multiline data are tolerated");
    server.join().expect("fake server script completed");
    assert_eq!(metrics.content, "A");
    assert_eq!(metrics.content_digest, digest(b"A"));
    assert_eq!(metrics.predicted_n, 32);
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
    let err = stream_completion(addr, &CompletionRequest::new("huge"), &tight, |_event| {})
        .expect_err("oversized event must be rejected");
    server.join().expect("fake server script completed");
    assert!(
        matches!(err, LlamaRpcError::ParseError(_)),
        "expected ParseError, got {err:?}"
    );
    assert!(err.to_string().to_lowercase().contains("exceed"), "{err}");
}

#[test]
fn dropping_the_event_receiver_cancels_the_worker() {
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let (addr, server) = spawn_fake_server(move |mut stream, _req| {
        stream.write_all(chunked_ok_headers().as_bytes()).unwrap();
        stream.flush().unwrap();
        // Wait until the consumer has dropped its receiver before streaming.
        go_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("test must signal after dropping the receiver");
        for _ in 0..64 {
            if stream.write_all(&http_chunk(&sse_delta("t"))).is_err() {
                return;
            }
            stream.flush().ok();
        }
        let _ = stream.write_all(&http_chunk(STOP_EVENT));
        let _ = stream.write_all(b"0\r\n\r\n");
    });
    let handle = spawn_completion_stream(
        addr,
        &CompletionRequest::new("cancel").with_n_predict(64),
        &options(),
    )
    .expect("spawn stream");
    let (events, worker) = handle.into_parts();
    drop(events);
    go_tx.send(()).expect("signal fake server");
    let err = worker
        .join()
        .expect("worker thread did not panic")
        .expect_err("cancelled worker reports an error");
    server.join().expect("fake server script completed");
    assert!(
        matches!(err, LlamaRpcError::StreamCancelled),
        "expected StreamCancelled, got {err:?}"
    );
}
