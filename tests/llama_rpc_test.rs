use std::path::PathBuf;

use fastlink::llama_rpc::{
    build_fastlink_env, build_server_args, parse_completion_sse_line, parse_fastlink_telemetry,
    LlamaRpcConfig,
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
