use fastlink::digest;
use fastlink::kv_delta::{
    apply_kv_delta_packet, build_kv_delta_packet, KvDeltaError, KvPageSnapshot,
};

#[test]
fn kv_delta_transfers_only_changed_pages_and_verifies_digests() {
    let p0 = vec![0x11u8; 4096];
    let p1 = vec![0x22u8; 4096];
    let p2_old = vec![0x33u8; 4096];
    let p2_new = vec![0x44u8; 4096];

    let snap_old = KvPageSnapshot {
        epoch: 1,
        generation: 1,
        layer_start: 0,
        layer_end: 40,
        page_digests: vec![digest(&p0), digest(&p1), digest(&p2_old)],
    };
    let snap_new = KvPageSnapshot {
        epoch: 2,
        generation: 2,
        layer_start: 0,
        layer_end: 40,
        page_digests: vec![digest(&p0), digest(&p1), digest(&p2_new)],
    };

    let packet = build_kv_delta_packet(Some(&snap_old), &snap_new, &[&p0, &p1, &p2_new])
        .expect("build delta");
    assert_eq!(packet.deltas.len(), 1);
    assert_eq!(packet.deltas[0].page_index, 2);

    let mut target = vec![p0.clone(), p1.clone(), p2_old];
    let updated = apply_kv_delta_packet(&packet, &mut target).expect("apply delta");
    assert_eq!(updated, 1);
    assert_eq!(target[2], p2_new);

    // Corrupting a delta entry must be rejected.
    let mut corrupt = packet.clone();
    corrupt.deltas[0].expected_digest[0] ^= 0xFF;
    assert!(matches!(
        apply_kv_delta_packet(&corrupt, &mut target),
        Err(KvDeltaError::DigestMismatch(2))
    ));
}
