use fastlink::stage_auth::{derive_hop_key, open_stage_frame, seal_stage_frame, StageAuthError};

#[test]
fn sealed_frame_opens_with_matching_hop_key() {
    let secret = [0x5Au8; 32];
    let key = derive_hop_key(&secret, 3, 0, 1);
    let raw = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let packet = seal_stage_frame(&key, 3, 10, 42, 0, 1, &raw).unwrap();
    let (frame, decoded) = open_stage_frame(&key, &packet, 3, 1).unwrap();
    assert_eq!(frame.epoch, 3);
    assert_eq!(frame.seq_id, 10);
    assert_eq!(frame.token_pos, 42);
    assert_eq!(frame.src_stage, 0);
    assert_eq!(frame.dst_stage, 1);
    assert_eq!(decoded, raw);
}

#[test]
fn tampered_packet_wrong_key_or_wrong_stage_is_rejected() {
    let secret = [0x5Au8; 32];
    let key_01 = derive_hop_key(&secret, 3, 0, 1);
    let key_12 = derive_hop_key(&secret, 3, 1, 2);
    let raw = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let mut packet = seal_stage_frame(&key_01, 3, 10, 42, 0, 1, &raw).unwrap();

    assert_eq!(
        open_stage_frame(&key_12, &packet, 3, 1),
        Err(StageAuthError::InvalidAuthTag)
    );

    assert_eq!(
        open_stage_frame(&key_01, &packet, 3, 2),
        Err(StageAuthError::StageMismatch {
            expected: 2,
            got: 1,
        })
    );

    packet[10] ^= 0xFF;
    assert_eq!(
        open_stage_frame(&key_01, &packet, 3, 1),
        Err(StageAuthError::InvalidAuthTag)
    );

    assert_eq!(
        open_stage_frame(&key_01, &[0u8; 16], 3, 1),
        Err(StageAuthError::PacketTooShort)
    );
}
