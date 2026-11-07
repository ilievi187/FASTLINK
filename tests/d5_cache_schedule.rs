//! FL3-D5: loopback transfer plus the in-memory budget cache, and the
//! public placement scheduler. Loopback only. No GPU.

use fastlink::cache_budget::MemoryBudgetCache;
use fastlink::scheduler::{
    select_placement, KvMode, LINK_REMOTE, LOCAL_DEDICATED, REMOTE_DEVICE, WORKLOAD_27B,
};
use fastlink::{client_sync, digest, server_sync, CHUNK_BYTES};
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

static ROOT_SEQ: AtomicUsize = AtomicUsize::new(0);

fn fresh_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fastlink-d5-{}-{}",
        std::process::id(),
        ROOT_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    ));
    fs::create_dir_all(root.join("chunks")).unwrap();
    fs::create_dir_all(root.join("outputs")).unwrap();
    root
}

struct LoopbackServer {
    handle: JoinHandle<()>,
    root: PathBuf,
}

impl LoopbackServer {
    fn start(root: &Path) -> (std::net::TcpStream, Self) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let root_for_thread = root.to_path_buf();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            if let Err(err) = server_sync(&mut stream, &root_for_thread) {
                eprintln!("d5 server_sync: {err}");
            }
        });
        let stream = std::net::TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        (
            stream,
            LoopbackServer {
                handle,
                root: root.to_path_buf(),
            },
        )
    }

    fn finish(self) -> PathBuf {
        let _ = self.handle.join();
        self.root
    }
}

#[test]
fn memory_budget_eviction_does_not_change_a_committed_output() {
    let root = fresh_root();
    let work = fresh_root();
    let name = "d5.bin";
    let first = vec![1u8; CHUNK_BYTES];
    let second = vec![2u8; CHUNK_BYTES];
    let file_bytes = [first.clone(), second.clone()].concat();
    let source = work.join(name);
    fs::write(&source, &file_bytes).unwrap();

    let (mut stream, server) = LoopbackServer::start(&root);
    let stats = client_sync(&mut stream, name, &source).unwrap();
    assert_eq!(stats.chunks_transferred, 2);
    let root = server.finish();
    let output = root.join("outputs").join(name);
    let committed = fs::read(&output).unwrap();
    assert_eq!(committed, file_bytes);

    let mut cache = MemoryBudgetCache::new(CHUNK_BYTES);
    let first_hash = digest(&first);
    let second_hash = digest(&second);
    assert!(cache.insert(first_hash, first).unwrap().is_empty());
    let evicted = cache.insert(second_hash, second).unwrap();
    assert_eq!(evicted.len(), 1);
    assert_eq!(evicted[0].digest, first_hash);
    assert_eq!(cache.get(&second_hash).unwrap().len(), CHUNK_BYTES);
    assert!(cache.get(&first_hash).is_none());
    assert_eq!(fs::read(&output).unwrap(), committed);

    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&work);
}

#[test]
fn public_scheduler_picks_the_reference_split() {
    let best = select_placement(
        WORKLOAD_27B,
        131_072,
        KvMode::Q4,
        LOCAL_DEDICATED,
        REMOTE_DEVICE,
        LINK_REMOTE,
    )
    .unwrap();
    assert_eq!(best.k_local, 53);
    assert_eq!(best.layers_remote, 11);
    assert!(best.feasible);
    let repeat = select_placement(
        WORKLOAD_27B,
        131_072,
        KvMode::Q4,
        LOCAL_DEDICATED,
        REMOTE_DEVICE,
        LINK_REMOTE,
    )
    .unwrap();
    assert_eq!(repeat.k_local, best.k_local);
    assert_eq!(
        repeat.decode_step_ms.to_bits(),
        best.decode_step_ms.to_bits()
    );
}
