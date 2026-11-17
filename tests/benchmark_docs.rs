const SNAPSHOT_ID: &str = "rpc-latency-linux-2026-11-10-232742-db55920";
const SHARED_VALUES: &[&str] = &["8,103.0 MB/s", "601.6 MB/s", "13.5×"];

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
        "8103.0",
        "601.6",
        "8014.9",
        "1960.6",
        "0.101",
        "66700",
    ] {
        assert!(
            raw_json.contains(expected),
            "canonical JSON missing {expected}"
        );
    }
    for expected in [
        "throughput=8103.0",
        "raw=601.6 MB/s",
        "wire=601.6 MB/s",
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
fn ci_automatic_tests_do_not_execute_benchmarks() {
    let workflow = include_str!("../.github/workflows/ci.yml").replace("\r\n", "\n");

    assert!(
        workflow.contains("  workflow_dispatch:"),
        "CI must expose manual workflow_dispatch"
    );
    assert!(
        workflow.contains("cargo test --workspace --lib --bins --tests --examples"),
        "CI automatic tests must cover lib, bins, tests, and examples"
    );
    // Cargo rejects --doc combined with target-selecting options, so doctests
    // must remain a separate benchmark-free step.
    assert!(
        workflow.contains("cargo test --workspace --doc"),
        "CI automatic tests must cover doctests separately"
    );
    assert!(
        !workflow.contains("cargo test --all-targets"),
        "CI must not execute benchmarks through cargo test --all-targets"
    );

    let test_step = workflow
        .split("- name: Test")
        .nth(1)
        .expect("Test step")
        .split("- name:")
        .next()
        .expect("Test step body");
    assert!(
        test_step.contains("run: cargo test --workspace --lib --bins --tests --examples"),
        "automatic Test step must cover examples without benchmarks"
    );

    let doc_step = workflow
        .split("- name: Doc tests")
        .nth(1)
        .expect("Doc tests step")
        .split("- name:")
        .next()
        .expect("Doc tests step body");
    assert!(
        doc_step.contains("run: cargo test --workspace --doc"),
        "automatic Doc tests step must run doctests without benchmarks"
    );

    let benchmark_step = workflow
        .split("- name: Transport benchmark smoke test")
        .nth(1)
        .expect("Transport benchmark smoke test step")
        .split("- name:")
        .next()
        .expect("Transport benchmark smoke test body");
    assert!(
        benchmark_step.lines().any(|line| line.trim()
            == "if: github.event_name == 'workflow_dispatch' && runner.os == 'Linux'"),
        "benchmark smoke test must be workflow_dispatch-only on Linux"
    );
    assert!(
        benchmark_step.contains("run: cargo bench -p fastlink --bench rpc_latency"),
        "manual benchmark step must still invoke the benchmark"
    );
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

fn assert_mmap_benchmark_contract(harness: &str) {
    // Normalize CRLF checkouts on Windows before matching multi-line snippets.
    let harness = harness.replace("\r\n", "\n");

    // Top-level documentation describes the real CPU/POSIX measurement, not
    // GPU or zero-copy staging.
    assert!(harness.contains("CPU-only"));
    assert!(harness.contains("POSIX shared-memory"));
    assert!(harness.contains("does not exercise a GPU"));
    assert!(!harness.contains("GPU Tensor Transfer"));
    assert!(!harness.contains("zero-copy bandwidth"));
    assert!(!harness.contains("simulates GPU worker"));

    // Windows has an explicit unsupported entrypoint rather than fake mmap
    // producers/consumers or performance output.
    assert!(harness.contains("WINDOWS_UNSUPPORTED_MESSAGE"));
    assert!(harness.contains("unsupported on Windows"));
    assert!(harness.contains("no mmap, TCP, or FASTLINK"));
    assert!(harness.contains("eprintln!(\"{WINDOWS_UNSUPPORTED_MESSAGE}\");"));
    assert!(harness.contains(
        "#[cfg(windows)]\nfn main() {\n    eprintln!(\"{WINDOWS_UNSUPPORTED_MESSAGE}\");\n}"
    ));
    assert!(!harness.contains("std::process::exit(1)"));
    assert!(harness.contains("#[cfg(windows)]\nfn main()"));
    assert!(harness.contains("#[cfg(unix)]\nfn main()"));

    // The real benchmark is POSIX-only, and Windows create/open must fail
    // without allocating a fake independent buffer.
    assert!(harness.contains("fn bench_mmap_latency"));
    assert_eq!(
        harness.matches("std::io::ErrorKind::Unsupported").count(),
        2,
        "Windows create/open must both return ErrorKind::Unsupported"
    );
    assert!(!harness.contains("Box::leak"));
}

#[test]
fn mmap_benchmark_is_explicitly_unavailable_on_windows() {
    let harness = include_str!("../benches/mmap_transfer.rs");
    assert_mmap_benchmark_contract(harness);
}

#[test]
fn mmap_benchmark_contract_handles_windows_crlf_line_endings() {
    let crlf = include_str!("../benches/mmap_transfer.rs")
        .replace("\r\n", "\n")
        .replace('\n', "\r\n");
    assert_mmap_benchmark_contract(&crlf);
}
