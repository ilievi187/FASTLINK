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
