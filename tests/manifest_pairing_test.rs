use fastlink::digest;
use fastlink::manifest::{
    ManifestCoordinator, ManifestError, NodeLifecycle, PartitionManifest, StageSpec,
};
use fastlink::pairing::{
    sign_challenge, NetworkTrust, PairingError, PairingRegistry, PeerIdentity,
};

fn test_peer(id: &str, trust: NetworkTrust, opted_in: bool) -> PeerIdentity {
    PeerIdentity {
        peer_id: id.to_string(),
        os: "linux".to_string(),
        runtime: "rocm".to_string(),
        addr: format!("100.64.0.{}:9400", id.len() + 10),
        trust,
        opted_in,
    }
}

#[test]
fn pairing_admits_opted_in_tailscale_peer_and_rejects_replay() {
    let secret = [0x42u8; 32];
    let mut registry = PairingRegistry::new(secret, 1);
    let peer = test_peer("node-7900xt", NetworkTrust::Tailscale, true);
    let nonce = [0x11u8; 32];

    let challenge = registry
        .issue_challenge(&peer, nonce, 1)
        .expect("challenge");
    let proof = sign_challenge(&secret, &challenge);
    let session = registry
        .verify_and_admit(&peer, &challenge, &proof)
        .expect("admit");

    assert_eq!(session.peer_id, "node-7900xt");
    assert_eq!(session.trust, NetworkTrust::Tailscale);
    assert_ne!(session.session_key, [0u8; 32]);

    // Replay with the same nonce must be rejected.
    let replay = registry.verify_and_admit(&peer, &challenge, &proof);
    assert_eq!(replay, Err(PairingError::StaleOrReplayedEpoch));
}

#[test]
fn pairing_rejects_untrusted_network_non_opt_in_and_bad_mac() {
    let secret = [0x55u8; 32];
    let mut registry = PairingRegistry::new(secret, 1);

    let untrusted = test_peer("guest-wan", NetworkTrust::Untrusted, true);
    assert_eq!(
        registry.issue_challenge(&untrusted, [1u8; 32], 1),
        Err(PairingError::UntrustedNetwork)
    );

    let not_opted = test_peer("laptop-work", NetworkTrust::Lan, false);
    assert_eq!(
        registry.issue_challenge(&not_opted, [2u8; 32], 1),
        Err(PairingError::NotOptedIn)
    );

    let valid_peer = test_peer("node-6600xt", NetworkTrust::Lan, true);
    let challenge = registry
        .issue_challenge(&valid_peer, [3u8; 32], 1)
        .expect("challenge");
    let wrong_proof = sign_challenge(&[0x99u8; 32], &challenge);
    assert_eq!(
        registry.verify_and_admit(&valid_peer, &challenge, &wrong_proof),
        Err(PairingError::InvalidProof)
    );
}

#[test]
fn manifest_builds_direct_p2p_links_and_verifies_blake3_digest() {
    let model_digest = digest(b"qwen-72b-q4km");
    let specs = vec![
        StageSpec {
            node_id: "node-a".into(),
            addr: "100.64.0.10:9400".into(),
            layer_start: 0,
            layer_end: 40,
        },
        StageSpec {
            node_id: "node-b".into(),
            addr: "100.64.0.11:9400".into(),
            layer_start: 40,
            layer_end: 80,
        },
    ];

    let mut manifest =
        PartitionManifest::build(1, model_digest, 32768, 80, &specs).expect("build manifest");
    manifest.verify().expect("digest valid");

    assert_eq!(
        manifest.stages[0].next_stage_addr.as_deref(),
        Some("100.64.0.11:9400")
    );
    assert_eq!(manifest.stages[1].next_stage_addr, None);

    // Tampering with layer_end invalidates the BLAKE3 manifest digest.
    manifest.stages[0].layer_end = 41;
    manifest.stages[1].layer_start = 41;
    assert_eq!(manifest.verify(), Err(ManifestError::DigestMismatch));
}

#[test]
fn coordinator_handles_join_evacuation_and_atomic_epoch_reshard() {
    let mut coord = ManifestCoordinator::new();
    coord.register_joining("node-a");
    coord.register_joining("node-b");
    coord.register_joining("node-c");

    let model_digest = digest(b"llama3-70b-q4");
    let initial = vec![
        StageSpec {
            node_id: "node-a".into(),
            addr: "127.0.0.1:9401".into(),
            layer_start: 0,
            layer_end: 40,
        },
        StageSpec {
            node_id: "node-b".into(),
            addr: "127.0.0.1:9402".into(),
            layer_start: 40,
            layer_end: 80,
        },
    ];
    let m1 = coord
        .commit_reshard(model_digest, 8192, 80, &initial)
        .expect("initial commit");
    assert_eq!(m1.epoch, 1);
    assert_eq!(coord.node_state("node-a"), Some(NodeLifecycle::Active));

    // Evacuate node-b and reshard onto node-a + node-c.
    coord.begin_evacuation("node-b").expect("begin evac");
    assert_eq!(coord.node_state("node-b"), Some(NodeLifecycle::Draining));
    coord.complete_evacuation("node-b").expect("complete evac");
    assert_eq!(coord.node_state("node-b"), Some(NodeLifecycle::Evacuated));

    let reshard = vec![
        StageSpec {
            node_id: "node-a".into(),
            addr: "127.0.0.1:9401".into(),
            layer_start: 0,
            layer_end: 36,
        },
        StageSpec {
            node_id: "node-c".into(),
            addr: "127.0.0.1:9403".into(),
            layer_start: 36,
            layer_end: 80,
        },
    ];
    let m2 = coord
        .commit_reshard(model_digest, 8192, 80, &reshard)
        .expect("reshard commit");
    assert_eq!(m2.epoch, 2);
    assert_eq!(coord.node_state("node-c"), Some(NodeLifecycle::Active));
    assert!(matches!(
        coord.validate_epoch(1),
        Err(ManifestError::StaleEpoch { got: 1, .. })
    ));
    assert!(coord.validate_epoch(2).is_ok());
}
