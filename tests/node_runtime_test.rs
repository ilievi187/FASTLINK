use fastlink::digest;
use fastlink::manifest::{PartitionManifest, StageSpec};
use fastlink::node_runtime::run_loopback_pipeline;
use fastlink::pipeline::TelemetrySource;

#[test]
fn three_stage_loopback_pipeline_forwards_p2p_and_accounts_bytes() {
    let model_digest = digest(b"test-3-stage-model");
    let specs = vec![
        StageSpec {
            node_id: "stage-0".into(),
            addr: "127.0.0.1:0".into(),
            layer_start: 0,
            layer_end: 10,
        },
        StageSpec {
            node_id: "stage-1".into(),
            addr: "127.0.0.1:0".into(),
            layer_start: 10,
            layer_end: 20,
        },
        StageSpec {
            node_id: "stage-2".into(),
            addr: "127.0.0.1:0".into(),
            layer_start: 20,
            layer_end: 30,
        },
    ];
    let manifest = PartitionManifest::build(1, model_digest, 4096, 30, &specs).expect("manifest");
    let tokens = vec![vec![1u8; 256], vec![2u8; 256], vec![3u8; 256]];

    let (outputs, summary) = run_loopback_pipeline(&manifest, &tokens, |stage_idx, input| {
        input
            .iter()
            .map(|b| b.wrapping_add(stage_idx as u8 + 1))
            .collect()
    })
    .expect("loopback pipeline");

    assert_eq!(outputs.len(), 3);
    // Each byte went through stages 0 (+1), 1 (+2), 2 (+3) => +6 total.
    assert_eq!(outputs[0], vec![7u8; 256]);
    assert_eq!(outputs[1], vec![8u8; 256]);
    assert_eq!(outputs[2], vec![9u8; 256]);
    assert_eq!(summary.source, TelemetrySource::Measured);
    assert_eq!(summary.total_tokens, 3);
    assert!(summary.total_wire_bytes > 0);
}

fn tiny_cpu_mlp() -> fastlink::cpu_inference::CpuMlpSpec {
    use fastlink::cpu_inference::{CpuActivation, CpuDenseLayer, CpuMlpSpec};

    let layer0 = CpuDenseLayer::new(
        3,
        4,
        vec![
            0.2, -0.1, 0.3, -0.4, 0.5, 0.1, 0.25, 0.25, -0.5, 0.1, 0.2, 0.15,
        ],
        vec![0.05, -0.1, 0.0, 0.2],
        CpuActivation::Relu,
    )
    .expect("layer 0");
    let layer1 = CpuDenseLayer::new(
        4,
        2,
        vec![0.3, -0.2, 0.4, 0.1, -0.5, 0.6, 0.2, -0.1],
        vec![0.1, -0.05],
        CpuActivation::Relu,
    )
    .expect("layer 1");
    let layer2 = CpuDenseLayer::new(
        2,
        2,
        vec![0.7, -0.3, 0.2, 0.9],
        vec![0.05, 0.0],
        CpuActivation::Identity,
    )
    .expect("layer 2");
    CpuMlpSpec::new(3, vec![layer0, layer1, layer2]).expect("tiny MLP")
}

fn tiny_cpu_manifest(spec: &fastlink::cpu_inference::CpuMlpSpec) -> PartitionManifest {
    let specs = vec![
        StageSpec {
            node_id: "cpu-stage-0".into(),
            addr: "127.0.0.1:0".into(),
            layer_start: 0,
            layer_end: 1,
        },
        StageSpec {
            node_id: "cpu-stage-1".into(),
            addr: "127.0.0.1:0".into(),
            layer_start: 1,
            layer_end: 2,
        },
        StageSpec {
            node_id: "cpu-stage-2".into(),
            addr: "127.0.0.1:0".into(),
            layer_start: 2,
            layer_end: 3,
        },
    ];
    PartitionManifest::build(7, spec.digest(), 16, 3, &specs).expect("cpu manifest")
}

#[test]
fn cpu_mlp_partitioned_authenticated_loopback_matches_monolithic_reference() {
    use fastlink::cpu_inference::run_authenticated_cpu_mlp_pipeline;
    use fastlink::pipeline::TelemetrySource;

    let spec = tiny_cpu_mlp();
    let manifest = tiny_cpu_manifest(&spec);
    let secret = [0x42u8; 32];
    let inputs = vec![
        vec![0.5, -0.25, 1.0],
        vec![-0.1, 0.4, 0.2],
        vec![0.0, 0.75, -0.5],
    ];
    let expected: Vec<Vec<f32>> = inputs
        .iter()
        .map(|input| spec.forward(input).expect("reference forward"))
        .collect();

    // Independent hand-computed values for the first token, so the test
    // checks dense math rather than only partitioned/reference consistency.
    let first_reference = spec.forward(&inputs[0]).expect("first reference");
    assert!((first_reference[0] - 0.24425).abs() <= 1e-6);
    assert!((first_reference[1] - 0.0555).abs() <= 1e-6);

    let (outputs, summary) = run_authenticated_cpu_mlp_pipeline(&spec, &manifest, &secret, &inputs)
        .expect("authenticated CPU MLP pipeline");

    assert_eq!(outputs.len(), expected.len());
    for (actual, reference) in outputs.iter().zip(&expected) {
        assert_eq!(actual.len(), reference.len());
        for (a, b) in actual.iter().zip(reference) {
            assert!(
                (a - b).abs() <= 1e-5,
                "partitioned output {a} differs from monolithic {b}"
            );
        }
    }
    assert_eq!(summary.source, TelemetrySource::Measured);
    assert_eq!(summary.total_tokens, inputs.len() as u64);
    assert!(summary.total_wire_bytes > 0);
}

#[test]
fn cpu_mlp_rejects_malformed_shapes_and_partitioning() {
    use fastlink::cpu_inference::{
        decode_f32_le, encode_f32_le, run_authenticated_cpu_mlp_pipeline, CpuDenseLayer,
        CpuInferenceError, CpuMlpSpec,
    };

    assert_eq!(
        encode_f32_le(&[1.0, -2.5]),
        vec![0, 0, 128, 63, 0, 0, 32, 192]
    );
    assert!(matches!(
        decode_f32_le(&[0, 0, 0], 1),
        Err(CpuInferenceError::InvalidActivationEncoding {
            expected: 1,
            got: 3
        })
    ));
    assert!(matches!(
        CpuDenseLayer::new(
            1,
            1,
            vec![f32::NAN],
            vec![0.0],
            fastlink::cpu_inference::CpuActivation::Identity
        ),
        Err(CpuInferenceError::NonFiniteParameter { .. })
    ));
    assert!(matches!(
        CpuDenseLayer::new(
            3,
            4,
            vec![0.0; 11],
            vec![0.0; 4],
            fastlink::cpu_inference::CpuActivation::Identity
        ),
        Err(CpuInferenceError::WeightLength { .. })
    ));
    assert!(matches!(
        CpuDenseLayer::new(
            3,
            4,
            vec![0.0; 12],
            vec![0.0; 3],
            fastlink::cpu_inference::CpuActivation::Identity
        ),
        Err(CpuInferenceError::BiasLength { .. })
    ));
    assert!(matches!(
        CpuMlpSpec::new(
            3,
            vec![CpuDenseLayer::new(
                4,
                2,
                vec![0.0; 8],
                vec![0.0; 2],
                fastlink::cpu_inference::CpuActivation::Identity,
            )
            .expect("valid layer")]
        ),
        Err(CpuInferenceError::LayerInputMismatch { .. })
    ));

    let spec = tiny_cpu_mlp();
    let secret = [0x42u8; 32];
    let manifest = tiny_cpu_manifest(&spec);
    let inputs = vec![vec![0.5, -0.25, 1.0]];

    let bad_input = vec![vec![0.5, -0.25]];
    assert!(matches!(
        run_authenticated_cpu_mlp_pipeline(&spec, &manifest, &secret, &bad_input),
        Err(CpuInferenceError::InputLength { .. })
    ));
    assert!(matches!(
        run_authenticated_cpu_mlp_pipeline(&spec, &manifest, &secret, &[]),
        Err(CpuInferenceError::EmptyInput)
    ));

    let wrong_digest = PartitionManifest::build(
        7,
        digest(b"not-the-cpu-mlp"),
        16,
        3,
        &[
            StageSpec {
                node_id: "cpu-stage-0".into(),
                addr: "127.0.0.1:0".into(),
                layer_start: 0,
                layer_end: 1,
            },
            StageSpec {
                node_id: "cpu-stage-1".into(),
                addr: "127.0.0.1:0".into(),
                layer_start: 1,
                layer_end: 2,
            },
            StageSpec {
                node_id: "cpu-stage-2".into(),
                addr: "127.0.0.1:0".into(),
                layer_start: 2,
                layer_end: 3,
            },
        ],
    )
    .expect("wrong digest manifest");
    assert!(matches!(
        run_authenticated_cpu_mlp_pipeline(&spec, &wrong_digest, &secret, &inputs),
        Err(CpuInferenceError::ManifestMismatch(_))
    ));

    let wrong_layer_count = PartitionManifest::build(
        7,
        spec.digest(),
        16,
        4,
        &[
            StageSpec {
                node_id: "cpu-stage-0".into(),
                addr: "127.0.0.1:0".into(),
                layer_start: 0,
                layer_end: 2,
            },
            StageSpec {
                node_id: "cpu-stage-1".into(),
                addr: "127.0.0.1:0".into(),
                layer_start: 2,
                layer_end: 4,
            },
        ],
    )
    .expect("wrong layer count manifest");
    assert!(matches!(
        run_authenticated_cpu_mlp_pipeline(&spec, &wrong_layer_count, &secret, &inputs),
        Err(CpuInferenceError::ManifestMismatch(_))
    ));
}

#[test]
fn cpu_mlp_rejects_nonfinite_accumulator_before_relu() {
    use fastlink::cpu_inference::{CpuActivation, CpuDenseLayer, CpuInferenceError, CpuMlpSpec};

    let layer = CpuDenseLayer::new(2, 1, vec![2.0, -2.0], vec![0.0], CpuActivation::Relu)
        .expect("finite overflow layer");
    let spec = CpuMlpSpec::new(2, vec![layer]).expect("finite overflow spec");

    let result = spec.forward(&[f32::MAX, f32::MAX]);
    assert!(
        matches!(result, Err(CpuInferenceError::NonFiniteActivation)),
        "NaN produced inside the dot product must not be hidden by ReLU; got {result:?}"
    );
}

#[test]
fn authenticated_cpu_mlp_surfaces_nonfinite_activation() {
    use fastlink::cpu_inference::{
        run_authenticated_cpu_mlp_pipeline, CpuActivation, CpuDenseLayer, CpuInferenceError,
        CpuMlpSpec,
    };

    let layer = CpuDenseLayer::new(2, 1, vec![2.0, -2.0], vec![0.0], CpuActivation::Relu)
        .expect("finite overflow layer");
    let spec = CpuMlpSpec::new(2, vec![layer]).expect("finite overflow spec");
    let manifest = PartitionManifest::build(
        7,
        spec.digest(),
        16,
        1,
        &[StageSpec {
            node_id: "cpu-stage-0".into(),
            addr: "127.0.0.1:0".into(),
            layer_start: 0,
            layer_end: 1,
        }],
    )
    .expect("single-stage manifest");

    let result = run_authenticated_cpu_mlp_pipeline(
        &spec,
        &manifest,
        &[0x42u8; 32],
        &[vec![f32::MAX, f32::MAX]],
    );
    assert!(
        matches!(result, Err(CpuInferenceError::NonFiniteActivation)),
        "the authenticated wrapper must surface the layer failure; got {result:?}"
    );
}

#[cfg(target_os = "linux")]
fn linux_thread_count() -> usize {
    std::fs::read_dir("/proc/self/task")
        .expect("read /proc/self/task")
        .count()
}

#[cfg(target_os = "linux")]
fn wait_for_thread_count_at_most(limit: usize) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        if linux_thread_count() <= limit {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(target_os = "linux")]
fn nonfinal_cpu_error_case() {
    use fastlink::cpu_inference::{
        run_authenticated_cpu_mlp_pipeline, CpuActivation, CpuDenseLayer, CpuInferenceError,
        CpuMlpSpec,
    };

    let layer0 = CpuDenseLayer::new(2, 1, vec![2.0, -2.0], vec![0.0], CpuActivation::Relu)
        .expect("overflowing layer 0");
    let layer1 =
        CpuDenseLayer::new(1, 1, vec![1.0], vec![0.0], CpuActivation::Identity).expect("layer 1");
    let layer2 =
        CpuDenseLayer::new(1, 1, vec![1.0], vec![0.0], CpuActivation::Identity).expect("layer 2");
    let spec = CpuMlpSpec::new(2, vec![layer0, layer1, layer2]).expect("three-layer spec");
    let manifest = PartitionManifest::build(
        7,
        spec.digest(),
        16,
        3,
        &[
            StageSpec {
                node_id: "cpu-stage-0".into(),
                addr: "127.0.0.1:0".into(),
                layer_start: 0,
                layer_end: 1,
            },
            StageSpec {
                node_id: "cpu-stage-1".into(),
                addr: "127.0.0.1:0".into(),
                layer_start: 1,
                layer_end: 2,
            },
            StageSpec {
                node_id: "cpu-stage-2".into(),
                addr: "127.0.0.1:0".into(),
                layer_start: 2,
                layer_end: 3,
            },
        ],
    )
    .expect("three-stage manifest");

    let baseline = linux_thread_count();
    let result = run_authenticated_cpu_mlp_pipeline(
        &spec,
        &manifest,
        &[0x42u8; 32],
        &[vec![f32::MAX, f32::MAX]],
    );
    let after = linux_thread_count();
    let no_leak = wait_for_thread_count_at_most(baseline);
    let safe_error = matches!(result, Err(CpuInferenceError::NonFiniteActivation));
    assert!(
        safe_error && no_leak,
        "expected NonFiniteActivation and no leaked stage threads; \
         result={result:?}, baseline={baseline}, after={after}, no_leak={no_leak}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn cpu_nonfinal_stage_error_does_not_leak_downstream_threads() {
    if std::env::var_os("FASTLINK_CPU_LEAK_CHILD").is_some() {
        nonfinal_cpu_error_case();
        return;
    }

    // Linux-only test: re-exec the current test binary in an isolated process
    // so unrelated harness threads cannot affect the /proc/self/task check.
    let output = std::process::Command::new("/proc/self/exe")
        .arg("--exact")
        .arg("cpu_nonfinal_stage_error_does_not_leak_downstream_threads")
        .arg("--nocapture")
        .env("FASTLINK_CPU_LEAK_CHILD", "1")
        .output()
        .expect("spawn isolated thread-leak test");
    assert!(
        output.status.success(),
        "isolated leak test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
