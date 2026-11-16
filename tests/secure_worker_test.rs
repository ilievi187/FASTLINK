use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use fastlink::digest;
use fastlink::pairing::{NetworkTrust, PairingError};
#[cfg(not(target_os = "linux"))]
use fastlink::secure_worker::TrustStore;
use fastlink::secure_worker::{
    make_prologue, NoiseConfig, NoiseSession, PairingConsent, Secret32, SecureWorkerError,
    MAX_CHUNK_PAYLOAD_BYTES, MAX_LOGICAL_FRAME_BYTES, MAX_NOISE_MESSAGE_BYTES,
    MAX_SINGLE_FRAME_PAYLOAD_BYTES, NOISE_PROTOCOL,
};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[cfg(not(target_os = "linux"))]
struct TempDir(tempfile::TempDir);

#[cfg(not(target_os = "linux"))]
impl TempDir {
    fn new(label: &str) -> Self {
        let prefix = format!("fastlink_secure_worker_public_{label}_");
        let dir = tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir()
            .expect("secure temp dir");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        self.0.path()
    }
}

#[test]
fn secret32_public_api_zeroizes_and_redacts_debug() {
    fn assert_zeroize_on_drop<T: Zeroize + ZeroizeOnDrop>(_val: &T) {}

    let mut secret = Secret32::from_bytes([0x5Cu8; 32]);
    assert_zeroize_on_drop(&secret);
    let clone = secret.clone();
    assert_eq!(secret, clone);
    let rendered = format!("{secret:?}");
    assert!(rendered.contains("REDACTED"));
    assert!(!rendered.contains("5c"));
    assert!(!rendered.contains("5C"));

    secret.zeroize();
    assert_eq!(secret, Secret32::from_bytes([0u8; 32]));
    let random_a = Secret32::random().expect("csprng secret a");
    let random_b = Secret32::random().expect("csprng secret b");
    assert_ne!(random_a, Secret32::from_bytes([0u8; 32]));
    assert_ne!(random_a, random_b);
}

#[test]
fn noise_config_prologue_and_protocol_constants_enforce_public_contract() {
    assert_eq!(NOISE_PROTOCOL, "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s");
    assert_eq!(MAX_NOISE_MESSAGE_BYTES, 65_535);
    assert_eq!(MAX_SINGLE_FRAME_PAYLOAD_BYTES, 65_515);
    assert_eq!(MAX_CHUNK_PAYLOAD_BYTES, 65_503);
    assert_eq!(MAX_LOGICAL_FRAME_BYTES, 262_144);

    let session_id = digest(b"public-api-session");
    let prologue = make_prologue("peer-a", "peer-b", &session_id, 42);
    let prologue_text = String::from_utf8(prologue.clone()).expect("utf8");
    assert!(prologue_text.contains("initiator=peer-a"));
    assert!(prologue_text.contains("responder=peer-b"));
    assert!(prologue_text.contains("epoch=42"));
    assert!(!prologue_text.contains("127.0.0.1"));

    let config = NoiseConfig::new(
        Secret32::from_bytes([0x11u8; 32]),
        Secret32::from_bytes([0x22u8; 32]),
        prologue,
        Some([0x33u8; 32]),
        Duration::from_secs(2),
    );
    let debug = format!("{config:?}");
    assert!(debug.contains("REDACTED"));
    assert!(!debug.contains("1111"));
    assert!(!debug.contains("2222"));
    assert!(!debug.contains("3333"));

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");

    let stream = TcpStream::connect(addr).expect("connect loopback");
    let bad_zero_psk = NoiseConfig::new(
        Secret32::from_bytes([0x11u8; 32]),
        Secret32::from_bytes([0u8; 32]),
        vec![1, 2, 3],
        Some([0x33u8; 32]),
        Duration::from_secs(2),
    );
    assert!(matches!(
        NoiseSession::handshake_initiator(stream, &bad_zero_psk),
        Err(SecureWorkerError::InvalidConfig)
    ));

    let stream = TcpStream::connect(addr).expect("connect loopback");
    let bad_missing_pin = NoiseConfig::new(
        Secret32::from_bytes([0x11u8; 32]),
        Secret32::from_bytes([0x22u8; 32]),
        vec![1, 2, 3],
        None,
        Duration::from_secs(2),
    );
    assert!(matches!(
        NoiseSession::handshake_initiator(stream, &bad_missing_pin),
        Err(SecureWorkerError::InvalidConfig)
    ));
}

#[test]
fn secure_worker_errors_and_public_pairing_consent_are_safe() {
    let consent = PairingConsent { approved: false };
    assert!(!consent.approved);
    assert!(NetworkTrust::Lan.is_permitted());

    let errors = [
        SecureWorkerError::KeyringUnavailable,
        SecureWorkerError::KeyringConflict,
        SecureWorkerError::Unsupported,
        SecureWorkerError::StoreCorrupt,
        SecureWorkerError::Pairing(PairingError::InvalidProof),
    ];
    for err in errors {
        let rendered = err.to_string();
        assert!(!rendered.is_empty());
        assert!(!rendered.contains("127.0.0.1"));
        assert!(!rendered.contains("/home"));
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn non_linux_trust_store_open_reports_unsupported() {
    let dir = TempDir::new("unsupported-os");
    assert!(matches!(
        TrustStore::open(dir.path()),
        Err(SecureWorkerError::Unsupported)
    ));
    assert!(!dir.path().join("state.json").exists());
    assert!(!dir.path().join("identity.key").exists());
}
