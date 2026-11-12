use fastlink::cluster_orchestrator::LayerMigration;
use fastlink::kv_delta::KvPageSnapshot;
use fastlink::migration_runner::{execute_layer_migration, MigrationError};
use fastlink::{digest, Digest};

fn source_snapshot(layer_start: u32, layer_end: u32, digests: Vec<Digest>) -> KvPageSnapshot {
    KvPageSnapshot {
        epoch: 7,
        generation: 3,
        layer_start,
        layer_end,
        page_digests: digests,
    }
}

#[test]
fn cold_migration_transfers_all_pages_and_verifies_digests() {
    let pages: Vec<Vec<u8>> = vec![vec![0x11u8; 4096], vec![0x22u8; 4096], vec![0x33u8; 4096]];
    let digests: Vec<Digest> = pages.iter().map(|p| digest(p)).collect();
    let source = source_snapshot(0, 3, digests);
    let migration = LayerMigration {
        from_node: "node-a".into(),
        to_node: "node-b".into(),
        layer_start: 0,
        layer_end: 3,
    };
    let mut target_pages: Vec<Vec<u8>> = Vec::new();
    let report =
        execute_layer_migration(&migration, 7, 3, &pages, &source, &mut target_pages, None)
            .expect("cold migration must succeed");
    assert_eq!(report.deltas_transferred, pages.len());
    assert_eq!(report.layer_start, 0);
    assert_eq!(report.layer_end, 3);
    assert_eq!(report.target_epoch, 7);
    assert_eq!(report.from_node, "node-a");
    assert_eq!(report.to_node, "node-b");
    assert!(report.total_wire_bytes > 0);
    for (idx, page) in pages.iter().enumerate() {
        assert_eq!(target_pages[idx], *page);
    }
}

#[test]
fn warm_migration_transfers_only_changed_pages() {
    let pages: Vec<Vec<u8>> = vec![vec![0x11u8; 4096], vec![0x22u8; 4096], vec![0x33u8; 4096]];
    let changed_page = vec![0x99u8; 4096];
    let source_pages: Vec<Vec<u8>> = vec![pages[0].clone(), changed_page.clone(), pages[2].clone()];
    let source = source_snapshot(
        0,
        3,
        vec![digest(&pages[0]), digest(&changed_page), digest(&pages[2])],
    );
    let stale_page = vec![0xAAu8; 4096];
    let target = KvPageSnapshot {
        epoch: 5,
        generation: 1,
        layer_start: 0,
        layer_end: 3,
        page_digests: vec![digest(&pages[0]), digest(&stale_page), digest(&pages[2])],
    };
    let mut target_pages: Vec<Vec<u8>> = vec![pages[0].clone(), stale_page, pages[2].clone()];
    let migration = LayerMigration {
        from_node: "node-a".into(),
        to_node: "node-b".into(),
        layer_start: 0,
        layer_end: 3,
    };
    let report = execute_layer_migration(
        &migration,
        7,
        3,
        &source_pages,
        &source,
        &mut target_pages,
        Some(&target),
    )
    .expect("warm migration must succeed");
    assert_eq!(report.deltas_transferred, 1);
    assert!(report.deltas_transferred < source_pages.len());
    assert_eq!(target_pages[1], changed_page);
}

#[test]
fn mismatched_layer_range_is_rejected() {
    let migration = LayerMigration {
        from_node: "node-a".into(),
        to_node: "node-b".into(),
        layer_start: 0,
        layer_end: 4,
    };
    let source = source_snapshot(0, 2, Vec::new());
    let pages: Vec<Vec<u8>> = Vec::new();
    let mut target_pages: Vec<Vec<u8>> = Vec::new();
    assert_eq!(
        execute_layer_migration(&migration, 7, 3, &pages, &source, &mut target_pages, None),
        Err(MigrationError::LayerRangeMismatch)
    );
}

#[test]
fn invalid_layer_range_is_rejected() {
    let migration = LayerMigration {
        from_node: "node-a".into(),
        to_node: "node-b".into(),
        layer_start: 4,
        layer_end: 4,
    };
    let source = source_snapshot(4, 4, Vec::new());
    let pages: Vec<Vec<u8>> = Vec::new();
    let mut target_pages: Vec<Vec<u8>> = Vec::new();
    assert_eq!(
        execute_layer_migration(&migration, 7, 3, &pages, &source, &mut target_pages, None),
        Err(MigrationError::InvalidLayerRange)
    );
}
