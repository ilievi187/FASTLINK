const SNAPSHOT_ID: &str = "rpc-latency-linux-2026-11-10-232427-2e3254a";
const SHARED_VALUES: &[&str] = &["8,329.0 MB/s", "607.8 MB/s", "13.7×"];

fn assert_snapshot_is_published(name: &str, text: &str) {
    assert!(
        text.contains(SNAPSHOT_ID),
        "{name} does not name {SNAPSHOT_ID}"
    );
    for value in SHARED_VALUES {
        assert!(
            text.contains(value),
            "{name} is missing shared value {value}"
        );
    }
}

#[test]
fn canonical_transport_snapshot_matches_all_current_reports() {
    let readme = include_str!("../README.md");
    let benchmarks = include_str!("../docs/BENCHMARKS.md");
    let progress = include_str!("../PROGRESS.md");
    let results = include_str!("../RESULTS.md");
    let raw_json = include_str!("../benches/results_rpc_latency.json");
    let raw_output = include_str!("../benches/results_rpc_latency_20261110.txt");

    for (name, text) in [
        ("README.md", readme),
        ("docs/BENCHMARKS.md", benchmarks),
        ("PROGRESS.md", progress),
        ("RESULTS.md", results),
    ] {
        assert_snapshot_is_published(name, text);
    }

    for expected in [
        SNAPSHOT_ID,
        "8329.0",
        "607.8",
        "6865.5",
        "1896.7",
        "0.099",
        "66700",
    ] {
        assert!(
            raw_json.contains(expected),
            "canonical JSON missing {expected}"
        );
    }
    for expected in [
        "throughput=8329.0",
        "raw=607.8 MB/s",
        "wire=607.9 MB/s",
        "wire_bytes_per_tensor=66700",
    ] {
        assert!(
            raw_output.contains(expected),
            "raw benchmark output missing {expected}"
        );
    }
}

#[test]
fn benchmark_checklist_matches_harness_and_transport_integrity_contract() {
    let checklist = include_str!("../docs/BENCHMARKS.md");
    let harness = include_str!("../benches/rpc_latency.rs");
    let transport = include_str!("../src/transport.rs");
    for expected in [
        "cargo bench --bench rpc_latency",
        "20 latency warmups + 500 RTT samples",
        "3 throughput warmups + 32 measured",
        "TCP_NODELAY",
        "does not establish chunk-content integrity",
    ] {
        assert!(checklist.contains(expected), "checklist missing {expected}");
    }
    for expected in [
        "const WARMUP_ROUNDS: usize = 20;",
        "const LATENCY_ROUNDS: usize = 500;",
        "const THROUGHPUT_WARMUP_ROUNDS: usize = 3;",
        "const THROUGHPUT_ROUNDS: usize = 32;",
        "const TENSOR_BYTES: usize = 8 * 1024 * 1024;",
        "TcpListener::bind(\"127.0.0.1:0\")",
    ] {
        assert!(
            harness.contains(expected),
            "harness changed: missing {expected}"
        );
    }
    assert!(transport.contains("no BLAKE3 chunk identity"));
    assert!(!transport.contains("verify every chunk"));
}

#[test]
fn results_do_not_mislabel_cpu_shared_memory_copy_as_gpu_zero_copy() {
    let results = include_str!("../RESULTS.md");
    assert!(results.contains("CPU-only shared-memory copy"));
    assert!(results.contains("does not exercise a GPU"));
    assert!(!results.contains("0 extra copy buffers"));
    assert!(!results.contains("zero-copy GPU tensor staging"));
    assert!(!results.contains("FLR1 frame verification"));
}
