#![cfg(target_os = "linux")]

use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fastlink::hex;
use fastlink::pairing::{NetworkTrust, PairingError, PairingSession};
use fastlink::secure_worker::{
    make_prologue, NoiseConfig, NoiseSession, PairingConsent, Secret32, SecureWorkerError,
    TrustStore, WorkerConfig, WorkerServer, MAX_LOGICAL_FRAME_BYTES,
    MAX_SINGLE_FRAME_PAYLOAD_BYTES,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fastlink_secure_worker_{label}_{}_{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Paired {
    _server_dir: TempDir,
    _client_dir: TempDir,
    server_store: TrustStore,
    client_store: TrustStore,
    session: PairingSession,
    psk: Secret32,
    prologue: Vec<u8>,
}

fn paired(peer_id: &str) -> Paired {
    let server_dir = TempDir::new("server");
    let client_dir = TempDir::new("client");
    let mut server_store = TrustStore::open(server_dir.path()).expect("server store");
    let client_store = TrustStore::open(client_dir.path()).expect("client store");
    let offer = server_store
        .begin_pairing(peer_id, NetworkTrust::Lan, true)
        .expect("pairing offer");
    let proof = offer.proof();
    let session = server_store
        .accept_pairing(
            &offer,
            &proof,
            PairingConsent { approved: true },
            client_store.local_static_public(),
        )
        .expect("pairing accepted");
    let psk = Secret32::from_bytes(session.session_key);
    let prologue = make_prologue(peer_id, "server", &session.session_id, session.epoch);
    Paired {
        _server_dir: server_dir,
        _client_dir: client_dir,
        server_store,
        client_store,
        session,
        psk,
        prologue,
    }
}

fn server_noise(config: &Paired) -> NoiseConfig {
    NoiseConfig::new(
        config.server_store.local_static_private(),
        config.psk.clone(),
        config.prologue.clone(),
        Some(config.client_store.local_static_public()),
        Duration::from_secs(2),
    )
}

fn client_noise(config: &Paired) -> NoiseConfig {
    NoiseConfig::new(
        config.client_store.local_static_private(),
        config.psk.clone(),
        config.prologue.clone(),
        Some(config.server_store.local_static_public()),
        Duration::from_secs(2),
    )
}

fn spawn_server_handshake(
    config: NoiseConfig,
) -> (
    SocketAddr,
    thread::JoinHandle<Result<NoiseSession, SecureWorkerError>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        NoiseSession::handshake_responder(stream, &config)
    });
    (addr, handle)
}

fn connect_and_handshake(
    addr: SocketAddr,
    config: &NoiseConfig,
) -> Result<NoiseSession, SecureWorkerError> {
    let stream = TcpStream::connect(addr).expect("connect");
    NoiseSession::handshake_initiator(stream, config)
}

fn expect_handshake_failure(config: &Paired, mutate: impl FnOnce(&mut NoiseConfig)) {
    let server = server_noise(config);
    let (addr, server_handle) = spawn_server_handshake(server);
    let mut client = client_noise(config);
    mutate(&mut client);
    assert!(connect_and_handshake(addr, &client).is_err());
    let _ = server_handle.join().expect("server join");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyMode {
    Pass,
    Capture,
    Tamper,
    Replay,
    Truncate(usize),
    RewriteLength(u32),
    Reflect,
}

struct ProxyState {
    mode: Mutex<ProxyMode>,
    captured: Mutex<Option<Vec<u8>>>,
}

struct Proxy {
    addr: SocketAddr,
    state: Arc<ProxyState>,
}

impl Proxy {
    fn start(server_addr: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("proxy bind");
        let addr = listener.local_addr().expect("proxy addr");
        let state = Arc::new(ProxyState {
            mode: Mutex::new(ProxyMode::Pass),
            captured: Mutex::new(None),
        });
        let thread_state = Arc::clone(&state);
        thread::spawn(move || {
            let (mut client, _) = listener.accept().expect("proxy accept");
            let mut server = TcpStream::connect(server_addr).expect("proxy upstream");
            let mut client_read = client.try_clone().expect("client clone");
            let mut client_write = client.try_clone().expect("client write clone");
            let mut server_write = server.try_clone().expect("server clone");
            let relay = thread::spawn(move || {
                let _ = io::copy(&mut server_write, &mut client_read);
            });

            loop {
                let mut prefix = [0u8; 4];
                if client.read_exact(&mut prefix).is_err() {
                    break;
                }
                let len = u32::from_le_bytes(prefix) as usize;
                if len > 70_000 {
                    break;
                }
                let mut body = vec![0u8; len];
                if client.read_exact(&mut body).is_err() {
                    break;
                }
                let mode = {
                    let mut mode = thread_state.mode.lock().expect("mode lock");
                    let current = *mode;
                    *mode = ProxyMode::Pass;
                    current
                };
                match mode {
                    ProxyMode::Pass => {
                        let _ = server.write_all(&prefix);
                        let _ = server.write_all(&body);
                    }
                    ProxyMode::Capture => {
                        let mut frame = prefix.to_vec();
                        frame.extend_from_slice(&body);
                        *thread_state.captured.lock().expect("capture lock") = Some(frame.clone());
                        let _ = server.write_all(&frame);
                    }
                    ProxyMode::Tamper => {
                        if let Some(last) = body.last_mut() {
                            *last ^= 1;
                        }
                        let _ = server.write_all(&prefix);
                        let _ = server.write_all(&body);
                    }
                    ProxyMode::Replay => {
                        let replay = thread_state.captured.lock().expect("capture lock").clone();
                        if let Some(frame) = replay {
                            let _ = server.write_all(&frame);
                        }
                    }
                    ProxyMode::Truncate(keep) => {
                        let mut frame = prefix.to_vec();
                        frame.extend_from_slice(&body);
                        let keep = keep.min(frame.len());
                        let _ = server.write_all(&frame[..keep]);
                        break;
                    }
                    ProxyMode::RewriteLength(new_len) => {
                        let _ = server.write_all(&new_len.to_le_bytes());
                        let _ = server.write_all(&body);
                        break;
                    }
                    ProxyMode::Reflect => {
                        let _ = client_write.write_all(&prefix);
                        let _ = client_write.write_all(&body);
                    }
                }
            }
            let _ = server.shutdown(Shutdown::Both);
            let _ = client.shutdown(Shutdown::Both);
            let _ = relay.join();
        });
        Self { addr, state }
    }

    fn arm(&self, mode: ProxyMode) {
        *self.state.mode.lock().expect("mode lock") = mode;
    }

    fn wait_consumed(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if *self.state.mode.lock().expect("mode lock") == ProxyMode::Pass {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("proxy did not consume armed mode");
    }

    fn take_captured(&self) -> Option<Vec<u8>> {
        self.state.captured.lock().expect("capture lock").take()
    }
}

fn proxy_pair(config: &Paired) -> (NoiseSession, NoiseSession, Proxy) {
    let server = server_noise(config);
    let (server_addr, server_handle) = spawn_server_handshake(server);
    let proxy = Proxy::start(server_addr);
    let client =
        connect_and_handshake(proxy.addr, &client_noise(config)).expect("client handshake");
    let server = server_handle
        .join()
        .expect("server join")
        .expect("server handshake");
    (client, server, proxy)
}

struct WorkerFixture {
    worker: WorkerServer,
    store: Arc<Mutex<TrustStore>>,
    client_config: NoiseConfig,
    psk: Secret32,
    prologue: Vec<u8>,
    expected_remote_static: [u8; 32],
    _server_dir: TempDir,
    _client_dir: TempDir,
}

impl WorkerFixture {
    fn start(config: Paired, handshake_timeout: Duration, frame_timeout: Duration) -> Self {
        let client_config = client_noise(&config);
        let expected_remote_static = config.client_store.local_static_public();
        let Paired {
            _server_dir,
            _client_dir,
            server_store,
            psk,
            prologue,
            ..
        } = config;
        let store = Arc::new(Mutex::new(server_store));
        let worker = WorkerServer::start(
            Arc::clone(&store),
            WorkerConfig {
                peer_id: "peer-a".to_string(),
                psk: psk.clone(),
                prologue: prologue.clone(),
                expected_remote_static,
                handshake_timeout,
                frame_timeout,
            },
        )
        .expect("worker start");
        Self {
            worker,
            store,
            client_config,
            psk,
            prologue,
            expected_remote_static,
            _server_dir,
            _client_dir,
        }
    }

    fn connect_client(&self) -> NoiseSession {
        NoiseSession::handshake_initiator(
            TcpStream::connect(self.worker.local_addr()).expect("worker connect"),
            &self.client_config,
        )
        .expect("worker client handshake")
    }
}

fn wait_for_control(worker: &WorkerServer, expected: &[u8]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if worker
            .received_controls()
            .iter()
            .any(|frame| frame == expected)
        {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn metadata_path(store_dir: &Path) -> PathBuf {
    fs::read_dir(store_dir.join("peers"))
        .expect("peers dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().ends_with(".meta.json"))
        .expect("metadata file")
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn pairing_requires_consent_rejects_replay_and_bad_policy() {
    let dir = TempDir::new("pairing");
    let client_dir = TempDir::new("pairing-client");
    let mut store = TrustStore::open(dir.path()).expect("store");
    let client_store = TrustStore::open(client_dir.path()).expect("client store");

    assert!(matches!(
        store.begin_pairing("untrusted", NetworkTrust::Untrusted, true),
        Err(SecureWorkerError::Pairing(PairingError::UntrustedNetwork))
    ));
    assert!(matches!(
        store.begin_pairing("unopted", NetworkTrust::Lan, false),
        Err(SecureWorkerError::Pairing(PairingError::NotOptedIn))
    ));
    assert!(matches!(
        store.begin_pairing("", NetworkTrust::Lan, true),
        Err(SecureWorkerError::Pairing(PairingError::InvalidPeerId))
    ));

    let declined_offer = store
        .begin_pairing("peer-a", NetworkTrust::Lan, true)
        .expect("offer");
    let declined_proof = declined_offer.proof();
    let no_consent = store.accept_pairing(
        &declined_offer,
        &declined_proof,
        PairingConsent { approved: false },
        client_store.local_static_public(),
    );
    assert!(matches!(
        no_consent,
        Err(SecureWorkerError::PairingApprovalRequired)
    ));

    // Declining burns the provisional offer: it cannot be silently retried.
    let retry_declined = store.accept_pairing(
        &declined_offer,
        &declined_proof,
        PairingConsent { approved: true },
        client_store.local_static_public(),
    );
    assert!(matches!(retry_declined, Err(SecureWorkerError::Pairing(_))));

    let offer = store
        .begin_pairing("peer-a", NetworkTrust::Lan, true)
        .expect("fresh offer");
    let proof = offer.proof();
    let session = store
        .accept_pairing(
            &offer,
            &proof,
            PairingConsent { approved: true },
            client_store.local_static_public(),
        )
        .expect("accepted");
    assert_eq!(session.peer_id, "peer-a");
    assert!(store.is_trusted("peer-a"));
    assert_eq!(
        store.peer_static("peer-a"),
        Some(client_store.local_static_public())
    );

    let replay = store.accept_pairing(
        &offer,
        &proof,
        PairingConsent { approved: true },
        client_store.local_static_public(),
    );
    assert!(matches!(replay, Err(SecureWorkerError::Pairing(_))));

    // A proof bound to a different peer ID or with a bad MAC is rejected
    // before any trust record is written.
    let offer = store
        .begin_pairing("peer-c", NetworkTrust::Tailscale, true)
        .expect("fresh offer c");
    let mut wrong_peer = offer.proof();
    wrong_peer.peer_id = "peer-other".to_string();
    let rejected = store.accept_pairing(
        &offer,
        &wrong_peer,
        PairingConsent { approved: true },
        client_store.local_static_public(),
    );
    assert!(matches!(
        rejected,
        Err(SecureWorkerError::Pairing(PairingError::PeerMismatch))
    ));

    let offer = store
        .begin_pairing("peer-c", NetworkTrust::Tailscale, true)
        .expect("fresh offer c");
    let mut bad_mac = offer.proof();
    bad_mac.mac[0] ^= 1;
    let rejected = store.accept_pairing(
        &offer,
        &bad_mac,
        PairingConsent { approved: true },
        client_store.local_static_public(),
    );
    assert!(matches!(
        rejected,
        Err(SecureWorkerError::Pairing(PairingError::InvalidProof))
    ));
    assert!(!store.is_trusted("peer-c"));
}

#[test]
fn stale_pending_offer_is_rejected_after_trust_epoch_advances() {
    let dir = TempDir::new("epoch");
    let client_dir = TempDir::new("epoch-client");
    let mut store = TrustStore::open(dir.path()).expect("store");
    let client_store = TrustStore::open(client_dir.path()).expect("client store");

    let offer_a = store
        .begin_pairing("peer-a", NetworkTrust::Lan, true)
        .expect("offer a");
    let proof_a = offer_a.proof();
    store
        .accept_pairing(
            &offer_a,
            &proof_a,
            PairingConsent { approved: true },
            client_store.local_static_public(),
        )
        .expect("accept a");

    let offer_b = store
        .begin_pairing("peer-b", NetworkTrust::Lan, true)
        .expect("offer b");
    let proof_b = offer_b.proof();

    // Removal advances the local trust epoch, invalidating older pending offers.
    store.remove_peer("peer-a").expect("remove a");
    let stale = store.accept_pairing(
        &offer_b,
        &proof_b,
        PairingConsent { approved: true },
        client_store.local_static_public(),
    );
    assert!(matches!(
        stale,
        Err(SecureWorkerError::Pairing(
            PairingError::StaleOrReplayedEpoch
        ))
    ));
    assert!(!store.is_trusted("peer-b"));
}

#[test]
fn trust_store_permissions_metadata_privacy_and_removal() {
    let config = paired("peer-a");
    let store_dir = config._server_dir.path();
    assert_eq!(mode_of(store_dir), 0o700);
    assert_eq!(mode_of(&store_dir.join("identity.key")), 0o600);
    assert_eq!(mode_of(&store_dir.join("state.json")), 0o600);
    assert_eq!(mode_of(&store_dir.join("peers")), 0o700);

    let meta_path = metadata_path(store_dir);
    let secret_path = meta_path
        .to_string_lossy()
        .trim_end_matches(".meta.json")
        .to_string();
    let secret_path = PathBuf::from(format!("{secret_path}.secret"));
    assert_eq!(mode_of(&meta_path), 0o600);
    assert_eq!(mode_of(&secret_path), 0o600);
    assert_eq!(
        fs::metadata(&secret_path).expect("secret metadata").len(),
        32
    );

    let meta_text = fs::read_to_string(&meta_path).expect("metadata text");
    let session_key_hex = hex(&config.session.session_key);
    assert!(!meta_text.contains("127.0.0.1"));
    assert!(!meta_text.contains("::1"));
    assert!(!meta_text.contains("addr"));
    assert!(!meta_text.contains(&session_key_hex));
    assert!(!meta_text.contains("session_key"));
    assert!(meta_text.contains("\"trusted\":true"));

    let mut store = config.server_store;
    assert!(store.remove_peer("peer-a").expect("remove"));
    assert!(!store.is_trusted("peer-a"));
    assert_eq!(store.peer_static("peer-a"), None);
    assert_eq!(store.peer_pairing_secret("peer-a"), None);
    assert!(!store.remove_peer("peer-a").expect("second removal"));
    assert!(fs::read_dir(store_dir.join("peers"))
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false));
}

#[test]
fn corrupt_records_fail_closed_and_stale_temporary_files_are_cleaned() {
    let dir = TempDir::new("corrupt");
    let client_dir = TempDir::new("corrupt-client");
    let client_store = TrustStore::open(client_dir.path()).expect("client store");
    let mut store = TrustStore::open(dir.path()).expect("store");
    let offer = store
        .begin_pairing("peer-a", NetworkTrust::Lan, true)
        .expect("offer");
    let proof = offer.proof();
    store
        .accept_pairing(
            &offer,
            &proof,
            PairingConsent { approved: true },
            client_store.local_static_public(),
        )
        .expect("accepted");
    let meta_path = metadata_path(dir.path());
    fs::write(&meta_path, b"not valid json").expect("corrupt metadata");
    fs::set_permissions(&meta_path, fs::Permissions::from_mode(0o600)).expect("mode");
    let stale_tmp = dir.path().join("peers").join(".leftover.tmp-1-1");
    fs::write(&stale_tmp, b"partial").expect("stale temp");
    drop(store);

    let mut reopened = TrustStore::open(dir.path()).expect("reopen");
    assert!(!reopened.is_trusted("peer-a"));
    assert_eq!(reopened.peer_static("peer-a"), None);
    assert!(!stale_tmp.exists());
    assert!(reopened.remove_peer("peer-a").expect("remove corrupt"));
    assert!(fs::read_dir(dir.path().join("peers"))
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false));

    let bad_identity_dir = TempDir::new("bad-identity");
    fs::write(bad_identity_dir.path().join("identity.key"), b"short").expect("bad identity");
    fs::set_permissions(
        bad_identity_dir.path().join("identity.key"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("mode");
    assert!(matches!(
        TrustStore::open(bad_identity_dir.path()),
        Err(SecureWorkerError::StoreCorrupt)
    ));
}

#[test]
fn trust_store_refuses_to_chmod_an_unrelated_directory() {
    let dir = TempDir::new("unrelated");
    fs::write(dir.path().join("notes.txt"), b"user data").expect("notes");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("mode");
    assert!(matches!(
        TrustStore::open(dir.path()),
        Err(SecureWorkerError::StorePermission)
    ));
    assert_eq!(mode_of(dir.path()), 0o755);
}

#[test]
fn wrong_psk_wrong_peer_prologue_and_bad_pin_fail_closed() {
    let config = paired("peer-a");
    expect_handshake_failure(&config, |client| {
        client.psk = Secret32::random().expect("psk");
    });
    expect_handshake_failure(&config, |client| {
        client.prologue = b"wrong prologue".to_vec();
    });
    expect_handshake_failure(&config, |client| {
        let session_id = config.session.session_id;
        client.prologue =
            make_prologue("peer-impostor", "server", &session_id, config.session.epoch);
    });
    expect_handshake_failure(&config, |client| {
        client.expected_remote_static = Some([0xAA; 32]);
    });
    let server = server_noise(&config);
    let (addr, server_handle) = spawn_server_handshake(server);
    let mut client = client_noise(&config);
    client.expected_remote_static = Some([0xAA; 32]);
    assert!(matches!(
        connect_and_handshake(addr, &client),
        Err(SecureWorkerError::StaticKeyMismatch)
    ));
    let _ = server_handle.join();
}

#[test]
fn loopback_control_roundtrip_and_exact_frame_limits() {
    let config = paired("peer-a");
    let (addr, server_handle) = spawn_server_handshake(server_noise(&config));
    let mut client = connect_and_handshake(addr, &client_noise(&config)).expect("client handshake");
    let mut server = server_handle
        .join()
        .expect("server join")
        .expect("server handshake");

    client.send_control(b"start").expect("send control");
    assert_eq!(server.recv_control().expect("recv control"), b"start");

    let max_control = vec![0x5A; MAX_SINGLE_FRAME_PAYLOAD_BYTES];
    client.send_control(&max_control).expect("max control");
    assert_eq!(server.recv_control().expect("max recv"), max_control);

    let too_large = vec![0u8; MAX_SINGLE_FRAME_PAYLOAD_BYTES + 1];
    assert!(matches!(
        client.send_control(&too_large),
        Err(SecureWorkerError::FrameTooLarge)
    ));

    // 65,516 bytes crosses the single-message boundary and must be chunked.
    let chunked = vec![0xA5; MAX_SINGLE_FRAME_PAYLOAD_BYTES + 1];
    client.send_logical(&chunked).expect("chunked logical");
    assert_eq!(server.recv_logical().expect("chunked recv"), chunked);

    let max_logical = vec![0xC3; MAX_LOGICAL_FRAME_BYTES];
    client.send_logical(&max_logical).expect("max logical");
    assert_eq!(
        server.recv_logical().expect("max logical recv"),
        max_logical
    );

    let over_logical = vec![0u8; MAX_LOGICAL_FRAME_BYTES + 1];
    assert!(matches!(
        client.send_logical(&over_logical),
        Err(SecureWorkerError::FrameTooLarge)
    ));
}

#[test]
fn transport_is_confidential_and_rejects_tamper_replay_truncation_and_oversize() {
    // No plaintext control payload is observable in the captured ciphertext.
    {
        let config = paired("peer-a");
        let (mut client, mut server, proxy) = proxy_pair(&config);
        let sentinel = b"PLAINTEXT-SENTINEL-model-prompt-KV";
        proxy.arm(ProxyMode::Capture);
        client.send_control(sentinel).expect("send sentinel");
        proxy.wait_consumed();
        let captured = proxy.take_captured().expect("captured frame");
        assert!(!captured
            .windows(sentinel.len())
            .any(|window| window == sentinel));
        assert!(captured.len() > sentinel.len() + 16);
        assert_eq!(server.recv_control().expect("recv sentinel"), sentinel);
    }

    // AEAD tamper: flipping one ciphertext bit closes the session.
    {
        let config = paired("peer-a");
        let (mut client, mut server, proxy) = proxy_pair(&config);
        proxy.arm(ProxyMode::Tamper);
        client.send_control(b"tamper").expect("send tamper");
        proxy.wait_consumed();
        assert!(server.recv_control().is_err());
    }

    // Replay: re-sending a previously valid ciphertext fails on the Noise nonce.
    {
        let config = paired("peer-a");
        let (mut client, mut server, proxy) = proxy_pair(&config);
        proxy.arm(ProxyMode::Capture);
        client.send_control(b"first").expect("first");
        proxy.wait_consumed();
        assert_eq!(server.recv_control().expect("first recv"), b"first");
        proxy.arm(ProxyMode::Replay);
        client.send_control(b"second").expect("second");
        proxy.wait_consumed();
        assert!(server.recv_control().is_err());
    }

    // Wrong direction: reflecting the client's ciphertext back to the client
    // fails because Noise directional keys differ.
    {
        let config = paired("peer-a");
        let (mut client, server, proxy) = proxy_pair(&config);
        proxy.arm(ProxyMode::Reflect);
        client.send_control(b"reflect").expect("send reflect");
        proxy.wait_consumed();
        assert!(client.recv_control().is_err());
        drop(server);
    }

    // Truncated frame: only a prefix of the declared ciphertext arrives.
    {
        let config = paired("peer-a");
        let (mut client, mut server, proxy) = proxy_pair(&config);
        proxy.arm(ProxyMode::Truncate(6));
        client.send_control(b"truncated").expect("send truncated");
        proxy.wait_consumed();
        assert!(server.recv_control().is_err());
    }

    // Declared ciphertext length above the Noise cap is rejected before
    // allocation, as is a zero length (shorter than the AEAD tag).
    {
        let config = paired("peer-a");
        let (mut client, mut server, proxy) = proxy_pair(&config);
        proxy.arm(ProxyMode::RewriteLength(65_536));
        client.send_control(b"oversize").expect("send oversize");
        proxy.wait_consumed();
        assert!(matches!(
            server.recv_control(),
            Err(SecureWorkerError::FrameTooLarge)
        ));
    }
    {
        let config = paired("peer-a");
        let (mut client, mut server, proxy) = proxy_pair(&config);
        proxy.arm(ProxyMode::RewriteLength(0));
        client.send_control(b"zero").expect("send zero");
        proxy.wait_consumed();
        assert!(matches!(
            server.recv_control(),
            Err(SecureWorkerError::ProtocolViolation)
        ));
    }
}

#[test]
fn tampered_and_oversized_handshake_messages_fail_closed() {
    {
        let config = paired("peer-a");
        let server = server_noise(&config);
        let (addr, server_handle) = spawn_server_handshake(server);
        let proxy = Proxy::start(addr);
        proxy.arm(ProxyMode::Tamper);
        assert!(connect_and_handshake(proxy.addr, &client_noise(&config)).is_err());
        assert!(server_handle.join().expect("server join").is_err());
    }
    {
        let config = paired("peer-a");
        let server = server_noise(&config);
        let (addr, server_handle) = spawn_server_handshake(server);
        let proxy = Proxy::start(addr);
        proxy.arm(ProxyMode::RewriteLength(65_536));
        assert!(connect_and_handshake(proxy.addr, &client_noise(&config)).is_err());
        assert!(server_handle.join().expect("server join").is_err());
    }
}

#[test]
fn worker_rejects_preauth_and_truncated_payloads_without_retaining_controls() {
    let fixture = WorkerFixture::start(
        paired("peer-a"),
        Duration::from_secs(2),
        Duration::from_secs(2),
    );

    // A framed model payload is parsed only as a Noise handshake and rejected.
    let mut framed = TcpStream::connect(fixture.worker.local_addr()).expect("connect framed");
    framed
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    framed.write_all(&5u32.to_le_bytes()).expect("len");
    framed.write_all(b"MODEL").expect("model");
    let mut byte = [0u8; 1];
    assert!(matches!(framed.read(&mut byte), Ok(0) | Err(_)));

    // Unframed plaintext is interpreted as an invalid length prefix and rejected.
    let mut raw = TcpStream::connect(fixture.worker.local_addr()).expect("connect raw");
    raw.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    raw.write_all(b"PROMPT-KV-MODEL").expect("raw prompt");
    assert!(matches!(raw.read(&mut byte), Ok(0) | Err(_)));

    // A truncated handshake body closes without processing application data.
    let mut truncated = TcpStream::connect(fixture.worker.local_addr()).expect("connect truncated");
    truncated
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    truncated.write_all(&100u32.to_le_bytes()).expect("len");
    truncated.write_all(b"short").expect("partial");
    truncated.shutdown(Shutdown::Write).expect("shutdown write");
    assert!(matches!(truncated.read(&mut byte), Ok(0) | Err(_)));

    assert!(fixture.worker.received_controls().is_empty());
    assert!(fixture.worker.is_running());
}

#[test]
fn worker_handshake_timeout_and_stop_close_owned_sessions() {
    let fixture = WorkerFixture::start(
        paired("peer-a"),
        Duration::from_millis(200),
        Duration::from_millis(200),
    );

    assert!(fixture.worker.local_addr().ip().is_loopback());
    let mut idle = TcpStream::connect(fixture.worker.local_addr()).expect("connect idle");
    idle.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let started = Instant::now();
    let mut byte = [0u8; 1];
    let _ = idle.read(&mut byte);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(fixture.worker.received_controls().is_empty());

    let mut client = fixture.connect_client();
    client.send_control(b"hello").expect("send hello");
    assert!(wait_for_control(&fixture.worker, b"hello"));

    let addr = fixture.worker.local_addr().to_string();
    let debug = format!("{:?}", fixture.worker);
    assert!(!debug.contains(&addr));
    assert!(!debug.contains("127.0.0.1"));

    let mut worker = fixture.worker;
    worker.stop();
    assert!(!worker.is_running());
    assert!(client.recv_control().is_err());
}

#[test]
fn worker_start_remove_revokes_and_restart_with_old_keys_fails() {
    let mut fixture = WorkerFixture::start(
        paired("peer-a"),
        Duration::from_secs(2),
        Duration::from_secs(2),
    );
    let mut client = fixture.connect_client();
    client.send_control(b"hello").expect("send");
    assert!(wait_for_control(&fixture.worker, b"hello"));

    assert!(fixture.worker.remove_peer().expect("remove"));
    assert!(!fixture.worker.is_running());
    {
        let store = fixture.store.lock().expect("store lock");
        assert!(!store.is_trusted("peer-a"));
        assert_eq!(store.peer_pairing_secret("peer-a"), None);
    }
    assert!(client.recv_control().is_err());

    let restart = WorkerServer::start(
        Arc::clone(&fixture.store),
        WorkerConfig {
            peer_id: "peer-a".to_string(),
            psk: fixture.psk.clone(),
            prologue: fixture.prologue.clone(),
            expected_remote_static: fixture.expected_remote_static,
            handshake_timeout: Duration::from_secs(1),
            frame_timeout: Duration::from_secs(1),
        },
    );
    assert!(matches!(restart, Err(SecureWorkerError::UntrustedPeer)));
}

#[test]
fn worker_frame_timeout_closes_an_idle_authenticated_session() {
    let fixture = WorkerFixture::start(
        paired("peer-a"),
        Duration::from_secs(2),
        Duration::from_millis(200),
    );
    let mut client = fixture.connect_client();
    client.send_control(b"still-here").expect("send");
    assert!(wait_for_control(&fixture.worker, b"still-here"));

    // The worker applies a bounded read deadline after authentication; an idle
    // session is closed rather than held forever.
    thread::sleep(Duration::from_millis(600));
    assert!(client.recv_control().is_err());
    assert!(fixture.worker.is_running());
}

#[test]
fn debug_output_and_errors_do_not_expose_secrets_or_endpoints() {
    let config = paired("peer-a");
    let key_hex = hex(&config.session.session_key);
    let session_debug = format!("{:?}", config.session);
    assert!(session_debug.contains("REDACTED"));
    assert!(!session_debug.contains(&key_hex));

    let debug_dir = TempDir::new("debug-store");
    let mut store = TrustStore::open(debug_dir.path()).expect("store");
    let offer = store
        .begin_pairing("peer-a", NetworkTrust::Lan, true)
        .expect("offer");
    let offer_debug = format!("{offer:?}");
    assert!(offer_debug.contains("REDACTED"));
    assert!(!offer_debug.contains("secret="));
    assert!(!offer_debug.contains("127.0.0.1"));
    let proof = offer.proof();
    let proof_debug = format!("{proof:?}");
    assert!(proof_debug.contains("REDACTED"));
    assert!(!proof_debug.contains(&hex(&proof.mac)));

    let noise_debug = format!("{:?}", server_noise(&config));
    assert!(!noise_debug.contains(&key_hex));
    assert!(noise_debug.contains("REDACTED"));

    let config_debug = format!(
        "{:?}",
        WorkerConfig {
            peer_id: "peer-a".to_string(),
            psk: config.psk.clone(),
            prologue: config.prologue.clone(),
            expected_remote_static: config.client_store.local_static_public(),
            handshake_timeout: Duration::from_secs(1),
            frame_timeout: Duration::from_secs(1),
        }
    );
    assert!(!config_debug.contains(&key_hex));
    assert!(config_debug.contains("REDACTED"));

    for error in [
        SecureWorkerError::StaticKeyMismatch,
        SecureWorkerError::HandshakeFailed,
        SecureWorkerError::UntrustedPeer,
    ] {
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains("127.0.0.1"));
        assert!(!rendered.contains(&key_hex));
    }
}
