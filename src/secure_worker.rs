//! Experimental secure worker backend (pairing trust store and Noise transport).
//!
//! This module is **experimental** and has not received an independent security
//! review. It implements the pairing/trust boundary and a loopback-only
//! Noise-protected control transport described in
//! `docs/SECURE_WORKER_DESIGN.md`.
//!
//! Security boundaries in this slice:
//!
//! - Pairing requires an explicit [`PairingConsent`] and a trusted network
//!   tier. The pairing secret is generated with the operating-system CSPRNG;
//!   the no-echo interactive TTY/UI pairing ceremony and network exchange remain
//!   unimplemented here.
//! - Persistent secrets (installation static X25519 private key and per-peer
//!   pairing secrets) are stored in the Linux Secret Service keyring via
//!   `zbus-secret-service-keyring-store` (`rt-async-io-crypto-rust`), encoded as
//!   64-character UTF-8 hex for KDE Wallet compatibility and keyed by opaque
//!   BLAKE3 digests rather than raw peer IDs or filesystem paths.
//! - Requires a running, unlocked D-Bus Secret Service daemon on Linux; headless
//!   sessions without an unlocked Secret Service fail closed with
//!   [`SecureWorkerError::KeyringUnavailable`]. Non-Linux targets fail closed
//!   with [`SecureWorkerError::Unsupported`] for `TrustStore::open`.
//! - A fresh pairing secret is used as the Noise `XXpsk3` pre-shared key. It is
//!   never used directly as a MAC or stream cipher.
//! - Noise messages are capped at the protocol limit of 65,535 bytes including
//!   the 16-byte AEAD tag. Logical frames up to 256 KiB use bounded explicit
//!   chunking, never a single oversized Noise message.
//! - No application frame is accepted before a successful Noise handshake and
//!   explicit post-handshake confirmation.
//! - Persisted trust metadata contains no routing endpoint or secret material.
//!   Endpoints are held only in transient sockets.
//!
//! The `snow` crate (0.10.0) and third-party Secret Service/D-Bus crates are not
//! formally audited, and in-crate D-Bus transport buffers outside FASTLINK are
//! not zeroized by this module.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::pairing::{
    sign_challenge, NetworkTrust, PairingChallenge, PairingError, PairingProof, PairingRegistry,
    PairingSession, PeerIdentity,
};
use crate::Digest;

/// Fixed Noise protocol suite. No alternative suite or fallback is permitted.
pub const NOISE_PROTOCOL: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";

/// Noise hard limit: one ciphertext message including the 16-byte AEAD tag.
pub const MAX_NOISE_MESSAGE_BYTES: usize = 65_535;
/// AEAD tag length for `ChaChaPoly`.
pub const NOISE_TAG_BYTES: usize = 16;
/// Maximum transport plaintext for one Noise message.
pub const MAX_TRANSPORT_PLAINTEXT_BYTES: usize = MAX_NOISE_MESSAGE_BYTES - NOISE_TAG_BYTES;
/// Versioned inner header: `version:u8`, `frame_type:u8`, `flags:u16`.
pub const INNER_HEADER_BYTES: usize = 4;
/// Maximum application payload in one non-chunked frame.
pub const MAX_SINGLE_FRAME_PAYLOAD_BYTES: usize =
    MAX_TRANSPORT_PLAINTEXT_BYTES - INNER_HEADER_BYTES;
/// Chunk header: `transfer_id:u32`, `total_len:u32`, `offset:u32`.
pub const CHUNK_HEADER_BYTES: usize = 12;
/// Maximum application payload in one chunk frame.
pub const MAX_CHUNK_PAYLOAD_BYTES: usize =
    MAX_TRANSPORT_PLAINTEXT_BYTES - INNER_HEADER_BYTES - CHUNK_HEADER_BYTES;
/// Upper-layer logical-frame bound (existing 256 KiB protocol bound).
pub const MAX_LOGICAL_FRAME_BYTES: usize = 262_144;
/// Maximum transport messages per session before a re-handshake is required.
pub const MAX_SESSION_MESSAGES: u64 = 1_000_000;
/// Maximum accepted prologue length (defensive bound).
pub const MAX_PROLOGUE_BYTES: usize = 4_096;
/// Maximum opaque peer identifier length.
pub const MAX_PEER_ID_BYTES: usize = 256;
/// Maximum concurrent unfinished/active worker connections.
const MAX_WORKER_CONNECTIONS: usize = 16;
/// Pairing offers expire after five minutes.
pub const PAIRING_OFFER_TTL_SECS: u64 = 300;
/// Maximum number of received control frames retained per worker.
const MAX_RETAINED_CONTROLS: usize = 64;

const FRAME_VERSION: u8 = 1;
const FRAME_CONTROL: u8 = 0;
const FRAME_CHUNK: u8 = 1;
const FRAME_LOGICAL: u8 = 2;
const FRAME_CONFIRM: u8 = 3;
const CONFIRM_PAYLOAD: &[u8] = b"FASTLINK-CONFIRM-V1";

const _: () = assert!(MAX_NOISE_MESSAGE_BYTES == 65_535);
const _: () = assert!(MAX_TRANSPORT_PLAINTEXT_BYTES == 65_519);
const _: () = assert!(MAX_SINGLE_FRAME_PAYLOAD_BYTES == 65_515);
const _: () = assert!(MAX_CHUNK_PAYLOAD_BYTES == 65_503);
const _: () = assert!(MAX_LOGICAL_FRAME_BYTES == 262_144);

/// A 32-byte secret (pairing PSK or X25519 private key).
///
/// `Debug` is redacted and the bytes are zeroized on drop.
#[derive(Clone)]
pub struct Secret32([u8; 32]);

impl Secret32 {
    /// Generates a fresh secret from the operating-system CSPRNG.
    pub fn random() -> Result<Self, SecureWorkerError> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut *bytes).map_err(|_| SecureWorkerError::Entropy)?;
        Ok(Self::from_zeroizing(bytes))
    }

    /// Wraps caller-provided bytes. The caller remains responsible for the
    /// original buffer.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn from_zeroizing(mut bytes: Zeroizing<[u8; 32]>) -> Self {
        let secret = Self(*bytes);
        bytes.zeroize();
        secret
    }

    /// Copies a completed pairing session key into the Noise PSK owner.
    ///
    /// One copy is unavoidable because `SessionKey` owns and zeroizes its
    /// buffer independently; both owners zeroize on drop.
    pub fn from_session_key(key: &crate::pairing::SessionKey) -> Self {
        Self(*key.expose())
    }

    fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    fn constant_time_eq(&self, other: &Self) -> bool {
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl PartialEq for Secret32 {
    fn eq(&self, other: &Self) -> bool {
        self.constant_time_eq(other)
    }
}

impl Eq for Secret32 {}

impl fmt::Debug for Secret32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret32([REDACTED])")
    }
}

impl Zeroize for Secret32 {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl ZeroizeOnDrop for Secret32 {}

impl Drop for Secret32 {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Safe failure classes for the secure worker.
///
/// Variants deliberately carry no endpoint, key, PSK, prologue or raw peer
/// payload. `Io` and `Pairing` keep their standard (non-secret) causes.
#[derive(Debug)]
pub enum SecureWorkerError {
    Io(io::Error),
    Pairing(PairingError),
    PairingApprovalRequired,
    InvalidPairingOffer,
    InvalidPeerId,
    InvalidPeerKey,
    InvalidConfig,
    UntrustedPeer,
    StaticKeyMismatch,
    FrameTooLarge,
    ProtocolViolation,
    HandshakeFailed,
    Timeout,
    ConnectionClosed,
    SessionLimit,
    StoreCorrupt,
    StorePermission,
    KeyringUnavailable,
    KeyringConflict,
    Unsupported,
    Entropy,
    LockPoisoned,
    WorkerNotRunning,
}

impl fmt::Display for SecureWorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Io(_) => "secure worker i/o error",
            Self::Pairing(_) => "pairing policy rejected the request",
            Self::PairingApprovalRequired => "explicit pairing approval is required",
            Self::InvalidPairingOffer => "invalid or already used pairing offer",
            Self::InvalidPeerId => "invalid opaque peer identifier",
            Self::InvalidPeerKey => "invalid peer static public key",
            Self::InvalidConfig => "invalid secure worker configuration",
            Self::UntrustedPeer => "peer is not trusted",
            Self::StaticKeyMismatch => "peer static key does not match the pinned key",
            Self::FrameTooLarge => "frame exceeds the bounded secure-worker limit",
            Self::ProtocolViolation => "secure-worker protocol violation",
            Self::HandshakeFailed => "Noise handshake failed",
            Self::Timeout => "secure-worker operation timed out",
            Self::ConnectionClosed => "secure-worker connection closed",
            Self::SessionLimit => "secure-worker session message limit reached",
            Self::StoreCorrupt => "trust store is corrupt",
            Self::StorePermission => "trust store permissions are unsafe",
            Self::KeyringUnavailable => "operating-system keyring is unavailable or failed",
            Self::KeyringConflict => "operating-system keyring secret conflicts with legacy file",
            Self::Unsupported => "secure-worker storage is only supported on Linux",
            Self::Entropy => "operating-system CSPRNG failed",
            Self::LockPoisoned => "secure-worker lock is poisoned",
            Self::WorkerNotRunning => "secure worker is not running",
        };
        f.write_str(message)
    }
}

impl std::error::Error for SecureWorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Pairing(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for SecureWorkerError {
    fn from(err: io::Error) -> Self {
        map_io(err)
    }
}

impl From<PairingError> for SecureWorkerError {
    fn from(err: PairingError) -> Self {
        Self::Pairing(err)
    }
}

fn map_io(err: io::Error) -> SecureWorkerError {
    match err.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => SecureWorkerError::Timeout,
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe => SecureWorkerError::ConnectionClosed,
        _ => SecureWorkerError::Io(err),
    }
}

fn valid_peer_id(peer_id: &str) -> bool {
    !peer_id.trim().is_empty()
        && peer_id.len() <= MAX_PEER_ID_BYTES
        && !peer_id
            .bytes()
            .any(|byte| byte == b'\n' || byte == b'\r' || byte == 0 || byte.is_ascii_control())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn constant_time_eq_32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Explicit local confirmation for a pairing offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairingConsent {
    pub approved: bool,
}

/// Server-side pairing offer.
///
/// The pairing secret must be transferred out of band through a no-echo
/// interactive path. That UX is intentionally not implemented in this slice;
/// callers must not log or persist this value outside the trust store.
pub struct PairingOffer {
    peer_id: String,
    trust: NetworkTrust,
    epoch: u64,
    challenge: PairingChallenge,
    secret: Secret32,
}

impl PairingOffer {
    /// Opaque peer identifier bound by this offer.
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    /// Pairing/trust epoch bound by this offer.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Simulation helper for the remote side of the pairing ceremony.
    ///
    /// The peer static public key is bound into the keyed proof so the trust
    /// store can commit exactly the key that the peer authenticated.
    ///
    /// **This is not a substitute for the no-echo interactive transfer.** A
    /// production UI must display the one-time secret and obtain the peer's
    /// proof after local no-echo entry; this helper exists so loopback tests can
    /// exercise the trust-store path without a TTY.
    #[doc(hidden)]
    pub fn proof_for(&self, peer_static_public: [u8; 32]) -> PairingProof {
        sign_challenge(self.secret.expose(), &self.challenge, peer_static_public)
    }
}

impl fmt::Debug for PairingOffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingOffer")
            .field("peer_id", &self.peer_id)
            .field("trust", &self.trust)
            .field("epoch", &self.epoch)
            .field("challenge", &"[REDACTED]")
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

/// Builds the domain-separated, endpoint-free Noise prologue.
pub fn make_prologue(initiator: &str, responder: &str, session_id: &Digest, epoch: u64) -> Vec<u8> {
    let mut prologue = Vec::with_capacity(96 + initiator.len() + responder.len());
    prologue.extend_from_slice(b"FASTLINK secure-worker v1\ninitiator=");
    prologue.extend_from_slice(initiator.as_bytes());
    prologue.extend_from_slice(b"\nresponder=");
    prologue.extend_from_slice(responder.as_bytes());
    prologue.extend_from_slice(b"\nsession_id=");
    prologue.extend_from_slice(crate::hex(session_id).as_bytes());
    prologue.extend_from_slice(b"\nepoch=");
    prologue.extend_from_slice(epoch.to_string().as_bytes());
    prologue.push(b'\n');
    prologue
}

/// Noise handshake configuration.
///
/// `Debug` never prints the local private key, PSK, prologue contents or the
/// peer pin.
pub struct NoiseConfig {
    pub local_static_private: Secret32,
    pub psk: Secret32,
    pub prologue: Vec<u8>,
    pub expected_remote_static: Option<[u8; 32]>,
    pub timeout: Duration,
}

impl NoiseConfig {
    pub fn new(
        local_static_private: Secret32,
        psk: Secret32,
        prologue: Vec<u8>,
        expected_remote_static: Option<[u8; 32]>,
        timeout: Duration,
    ) -> Self {
        Self {
            local_static_private,
            psk,
            prologue,
            expected_remote_static,
            timeout,
        }
    }

    fn validate(&self) -> Result<(), SecureWorkerError> {
        if self.timeout.is_zero()
            || self.prologue.is_empty()
            || self.prologue.len() > MAX_PROLOGUE_BYTES
            || self.local_static_private.expose() == &[0u8; 32]
            || self.psk.expose() == &[0u8; 32]
        {
            return Err(SecureWorkerError::InvalidConfig);
        }
        if self.expected_remote_static.is_none() {
            // A missing pin would silently trust the first presented key.
            return Err(SecureWorkerError::InvalidConfig);
        }
        Ok(())
    }
}

impl fmt::Debug for NoiseConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NoiseConfig")
            .field("local_static_private", &"[REDACTED]")
            .field("psk", &"[REDACTED]")
            .field("prologue_len", &self.prologue.len())
            .field("expected_remote_static", &"[PINNED]")
            .field("timeout", &self.timeout)
            .finish()
    }
}

fn build_handshake(
    config: &NoiseConfig,
    initiator: bool,
) -> Result<snow::HandshakeState, SecureWorkerError> {
    config.validate()?;
    let params = NOISE_PROTOCOL
        .parse()
        .map_err(|_| SecureWorkerError::InvalidConfig)?;
    let builder = snow::Builder::new(params)
        .local_private_key(config.local_static_private.expose())
        .map_err(|_| SecureWorkerError::InvalidConfig)?
        .psk(3, config.psk.expose())
        .map_err(|_| SecureWorkerError::InvalidConfig)?
        .prologue(&config.prologue)
        .map_err(|_| SecureWorkerError::InvalidConfig)?;
    if initiator {
        builder
            .build_initiator()
            .map_err(|_| SecureWorkerError::HandshakeFailed)
    } else {
        builder
            .build_responder()
            .map_err(|_| SecureWorkerError::HandshakeFailed)
    }
}

fn set_stream_timeouts(stream: &TcpStream, timeout: Duration) -> Result<(), SecureWorkerError> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true)?;
    Ok(())
}

fn deadline_timeout(deadline: Instant) -> Result<Duration, SecureWorkerError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(SecureWorkerError::Timeout);
    }
    Ok(remaining
        .min(Duration::from_millis(250))
        .max(Duration::from_millis(1)))
}

fn read_exact_before(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<(), SecureWorkerError> {
    let mut filled = 0usize;
    while filled < buffer.len() {
        stream.set_read_timeout(Some(deadline_timeout(deadline)?))?;
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => return Err(SecureWorkerError::ConnectionClosed),
            Ok(read) => filled += read,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(err) => return Err(map_io(err)),
        }
    }
    Ok(())
}

fn write_all_before(
    stream: &mut TcpStream,
    buffer: &[u8],
    deadline: Instant,
) -> Result<(), SecureWorkerError> {
    let mut written = 0usize;
    while written < buffer.len() {
        stream.set_write_timeout(Some(deadline_timeout(deadline)?))?;
        match stream.write(&buffer[written..]) {
            Ok(0) => return Err(SecureWorkerError::ConnectionClosed),
            Ok(count) => written += count,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(err) => return Err(map_io(err)),
        }
    }
    Ok(())
}

fn write_frame_before(
    stream: &mut TcpStream,
    ciphertext: &[u8],
    min_len: usize,
    deadline: Instant,
) -> Result<(), SecureWorkerError> {
    if ciphertext.len() < min_len || ciphertext.len() > MAX_NOISE_MESSAGE_BYTES {
        return Err(SecureWorkerError::FrameTooLarge);
    }
    let len = u32::try_from(ciphertext.len()).map_err(|_| SecureWorkerError::FrameTooLarge)?;
    write_all_before(stream, &len.to_le_bytes(), deadline)?;
    write_all_before(stream, ciphertext, deadline)?;
    Ok(())
}

fn read_frame_before(
    stream: &mut TcpStream,
    min_len: usize,
    deadline: Instant,
) -> Result<Vec<u8>, SecureWorkerError> {
    let mut prefix = [0u8; 4];
    read_exact_before(stream, &mut prefix, deadline)?;
    let len = u32::from_le_bytes(prefix) as usize;
    if len > MAX_NOISE_MESSAGE_BYTES {
        return Err(SecureWorkerError::FrameTooLarge);
    }
    if len < min_len {
        return Err(SecureWorkerError::ProtocolViolation);
    }
    let mut buffer = vec![0u8; len];
    read_exact_before(stream, &mut buffer, deadline)?;
    Ok(buffer)
}

/// Authenticated Noise transport over one TCP connection.
///
/// Construction performs the `XXpsk3` handshake, pins/compares the remote
/// static key and exchanges a bounded encrypted confirmation before any
/// application frame can be sent or accepted.
pub struct NoiseSession {
    stream: TcpStream,
    transport: snow::TransportState,
    remote_static: [u8; 32],
    frame_timeout: Duration,
    sent_messages: u64,
    received_messages: u64,
}

impl NoiseSession {
    /// Runs the initiator side of the handshake on an already-connected stream.
    pub fn handshake_initiator(
        mut stream: TcpStream,
        config: &NoiseConfig,
    ) -> Result<Self, SecureWorkerError> {
        let deadline = Instant::now() + config.timeout;
        set_stream_timeouts(&stream, config.timeout)?;
        let mut handshake = build_handshake(config, true)?;
        let mut out = vec![0u8; MAX_NOISE_MESSAGE_BYTES];

        let written = handshake
            .write_message(&[], &mut out)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        write_frame_before(&mut stream, &out[..written], 1, deadline)?;

        let message = read_frame_before(&mut stream, 1, deadline)?;
        let mut payload = Zeroizing::new(vec![0u8; MAX_NOISE_MESSAGE_BYTES]);
        handshake
            .read_message(&message, &mut payload)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        verify_remote_static(handshake.get_remote_static(), config)?;

        let written = handshake
            .write_message(&[], &mut out)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        write_frame_before(&mut stream, &out[..written], 1, deadline)?;

        let remote_static = copy_remote_static(handshake.get_remote_static())?;
        let transport = handshake
            .into_transport_mode()
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        let session = Self::new(stream, transport, remote_static, config.timeout);
        complete_confirmation(session, true, deadline)
    }

    /// Runs the responder side of the handshake on an already-accepted stream.
    pub fn handshake_responder(
        mut stream: TcpStream,
        config: &NoiseConfig,
    ) -> Result<Self, SecureWorkerError> {
        let deadline = Instant::now() + config.timeout;
        set_stream_timeouts(&stream, config.timeout)?;
        let mut handshake = build_handshake(config, false)?;
        let mut out = vec![0u8; MAX_NOISE_MESSAGE_BYTES];

        let message = read_frame_before(&mut stream, 1, deadline)?;
        let mut payload = Zeroizing::new(vec![0u8; MAX_NOISE_MESSAGE_BYTES]);
        handshake
            .read_message(&message, &mut payload)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;

        let written = handshake
            .write_message(&[], &mut out)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        write_frame_before(&mut stream, &out[..written], 1, deadline)?;

        let message = read_frame_before(&mut stream, 1, deadline)?;
        let mut payload = Zeroizing::new(vec![0u8; MAX_NOISE_MESSAGE_BYTES]);
        handshake
            .read_message(&message, &mut payload)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        verify_remote_static(handshake.get_remote_static(), config)?;

        let remote_static = copy_remote_static(handshake.get_remote_static())?;
        let transport = handshake
            .into_transport_mode()
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        let session = Self::new(stream, transport, remote_static, config.timeout);
        complete_confirmation(session, false, deadline)
    }

    fn new(
        stream: TcpStream,
        transport: snow::TransportState,
        remote_static: [u8; 32],
        frame_timeout: Duration,
    ) -> Self {
        Self {
            stream,
            transport,
            remote_static,
            frame_timeout,
            sent_messages: 0,
            received_messages: 0,
        }
    }

    /// Peer static public key authenticated by the Noise handshake.
    pub fn remote_static(&self) -> [u8; 32] {
        self.remote_static
    }

    /// Sends one bounded control frame.
    pub fn send_control(&mut self, payload: &[u8]) -> Result<(), SecureWorkerError> {
        if payload.len() > MAX_SINGLE_FRAME_PAYLOAD_BYTES {
            return Err(SecureWorkerError::FrameTooLarge);
        }
        self.send_inner(FRAME_CONTROL, payload)
    }

    /// Receives one bounded control frame.
    pub fn recv_control(&mut self) -> Result<Vec<u8>, SecureWorkerError> {
        let (frame_type, payload) = self.recv_inner()?;
        if frame_type != FRAME_CONTROL {
            return Err(SecureWorkerError::ProtocolViolation);
        }
        Ok(payload)
    }

    /// Sends a logical application frame, using bounded explicit chunking when
    /// the payload exceeds one Noise transport message.
    pub fn send_logical(&mut self, payload: &[u8]) -> Result<(), SecureWorkerError> {
        if payload.len() > MAX_LOGICAL_FRAME_BYTES {
            return Err(SecureWorkerError::FrameTooLarge);
        }
        if payload.len() <= MAX_SINGLE_FRAME_PAYLOAD_BYTES {
            return self.send_inner(FRAME_LOGICAL, payload);
        }

        let total_len =
            u32::try_from(payload.len()).map_err(|_| SecureWorkerError::FrameTooLarge)?;
        let transfer_id = self.next_transfer_id();
        let mut offset = 0usize;
        while offset < payload.len() {
            let end = (offset + MAX_CHUNK_PAYLOAD_BYTES).min(payload.len());
            let chunk = &payload[offset..end];
            let mut body = Vec::with_capacity(CHUNK_HEADER_BYTES + chunk.len());
            body.extend_from_slice(&transfer_id.to_le_bytes());
            body.extend_from_slice(&total_len.to_le_bytes());
            body.extend_from_slice(&(offset as u32).to_le_bytes());
            body.extend_from_slice(chunk);
            self.send_inner(FRAME_CHUNK, &body)?;
            offset = end;
        }
        Ok(())
    }

    /// Receives one logical application frame, reassembling bounded chunks.
    pub fn recv_logical(&mut self) -> Result<Vec<u8>, SecureWorkerError> {
        let mut expected_transfer: Option<u32> = None;
        let mut total_len: Option<usize> = None;
        let mut assembled = Vec::new();

        loop {
            let (frame_type, payload) = self.recv_inner()?;
            match frame_type {
                FRAME_LOGICAL => {
                    if expected_transfer.is_some() {
                        return Err(SecureWorkerError::ProtocolViolation);
                    }
                    return Ok(payload);
                }
                FRAME_CHUNK => {
                    if payload.len() < CHUNK_HEADER_BYTES {
                        return Err(SecureWorkerError::ProtocolViolation);
                    }
                    let transfer_id = u32::from_le_bytes(
                        payload[0..4]
                            .try_into()
                            .map_err(|_| SecureWorkerError::ProtocolViolation)?,
                    );
                    let declared_total = u32::from_le_bytes(
                        payload[4..8]
                            .try_into()
                            .map_err(|_| SecureWorkerError::ProtocolViolation)?,
                    ) as usize;
                    let offset = u32::from_le_bytes(
                        payload[8..12]
                            .try_into()
                            .map_err(|_| SecureWorkerError::ProtocolViolation)?,
                    ) as usize;
                    let chunk = &payload[CHUNK_HEADER_BYTES..];

                    if declared_total > MAX_LOGICAL_FRAME_BYTES || chunk.is_empty() {
                        return Err(SecureWorkerError::FrameTooLarge);
                    }
                    if offset != assembled.len() {
                        return Err(SecureWorkerError::ProtocolViolation);
                    }
                    if offset
                        .checked_add(chunk.len())
                        .map(|end| end > declared_total)
                        .unwrap_or(true)
                    {
                        return Err(SecureWorkerError::ProtocolViolation);
                    }
                    match expected_transfer {
                        Some(expected) if expected != transfer_id => {
                            return Err(SecureWorkerError::ProtocolViolation);
                        }
                        None => {
                            expected_transfer = Some(transfer_id);
                            total_len = Some(declared_total);
                            assembled.reserve_exact(declared_total);
                        }
                        _ => {}
                    }
                    if Some(declared_total) != total_len {
                        return Err(SecureWorkerError::ProtocolViolation);
                    }
                    assembled.extend_from_slice(chunk);
                    if assembled.len() == declared_total {
                        return Ok(assembled);
                    }
                }
                _ => return Err(SecureWorkerError::ProtocolViolation),
            }
        }
    }

    fn next_transfer_id(&mut self) -> u32 {
        // Transfer IDs are local demultiplexing labels only; Noise provides
        // authentication. Wrap is harmless for the bounded one-frame reassembly.
        self.sent_messages as u32
    }

    fn send_inner(&mut self, frame_type: u8, payload: &[u8]) -> Result<(), SecureWorkerError> {
        self.send_inner_frame(FRAME_VERSION, frame_type, 0, payload)
    }

    fn send_inner_frame(
        &mut self,
        version: u8,
        frame_type: u8,
        flags: u16,
        payload: &[u8],
    ) -> Result<(), SecureWorkerError> {
        self.send_inner_frame_before(
            version,
            frame_type,
            flags,
            payload,
            Instant::now() + self.frame_timeout,
        )
    }

    fn send_inner_frame_before(
        &mut self,
        version: u8,
        frame_type: u8,
        flags: u16,
        payload: &[u8],
        deadline: Instant,
    ) -> Result<(), SecureWorkerError> {
        if payload.len() > MAX_SINGLE_FRAME_PAYLOAD_BYTES {
            return Err(SecureWorkerError::FrameTooLarge);
        }
        if self.sent_messages >= MAX_SESSION_MESSAGES {
            return Err(SecureWorkerError::SessionLimit);
        }
        let mut plaintext = Zeroizing::new(Vec::with_capacity(INNER_HEADER_BYTES + payload.len()));
        plaintext.push(version);
        plaintext.push(frame_type);
        plaintext.extend_from_slice(&flags.to_le_bytes());
        plaintext.extend_from_slice(payload);

        let mut ciphertext = vec![0u8; plaintext.len() + NOISE_TAG_BYTES];
        let written = self
            .transport
            .write_message(&plaintext, &mut ciphertext)
            .map_err(|_| SecureWorkerError::ProtocolViolation)?;
        ciphertext.truncate(written);
        write_frame_before(&mut self.stream, &ciphertext, NOISE_TAG_BYTES, deadline)?;
        self.sent_messages += 1;
        Ok(())
    }

    fn recv_inner(&mut self) -> Result<(u8, Vec<u8>), SecureWorkerError> {
        self.recv_inner_before(Instant::now() + self.frame_timeout)
    }

    fn recv_inner_before(&mut self, deadline: Instant) -> Result<(u8, Vec<u8>), SecureWorkerError> {
        if self.received_messages >= MAX_SESSION_MESSAGES {
            return Err(SecureWorkerError::SessionLimit);
        }
        let ciphertext = read_frame_before(&mut self.stream, NOISE_TAG_BYTES, deadline)?;
        let mut plaintext = Zeroizing::new(vec![0u8; ciphertext.len()]);
        let read = self
            .transport
            .read_message(&ciphertext, &mut plaintext)
            .map_err(|_| SecureWorkerError::ProtocolViolation)?;
        self.received_messages += 1;
        if read < INNER_HEADER_BYTES {
            return Err(SecureWorkerError::ProtocolViolation);
        }
        let version = plaintext[0];
        let frame_type = plaintext[1];
        let flags = u16::from_le_bytes([plaintext[2], plaintext[3]]);
        if version != FRAME_VERSION || flags != 0 {
            return Err(SecureWorkerError::ProtocolViolation);
        }
        if !matches!(
            frame_type,
            FRAME_CONTROL | FRAME_CHUNK | FRAME_LOGICAL | FRAME_CONFIRM
        ) {
            return Err(SecureWorkerError::ProtocolViolation);
        }
        Ok((frame_type, plaintext[INNER_HEADER_BYTES..read].to_vec()))
    }
}

impl fmt::Debug for NoiseSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NoiseSession")
            .field("remote_static", &"[PINNED]")
            .field("sent_messages", &self.sent_messages)
            .field("received_messages", &self.received_messages)
            .field("frame_timeout", &self.frame_timeout)
            .finish()
    }
}

fn copy_remote_static(remote: Option<&[u8]>) -> Result<[u8; 32], SecureWorkerError> {
    let remote = remote.ok_or(SecureWorkerError::HandshakeFailed)?;
    if remote.len() != 32 {
        return Err(SecureWorkerError::HandshakeFailed);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(remote);
    Ok(key)
}

fn verify_remote_static(
    remote: Option<&[u8]>,
    config: &NoiseConfig,
) -> Result<(), SecureWorkerError> {
    let observed = copy_remote_static(remote)?;
    match config.expected_remote_static {
        Some(expected) if constant_time_eq_32(&observed, &expected) => Ok(()),
        Some(_) => Err(SecureWorkerError::StaticKeyMismatch),
        None => Err(SecureWorkerError::InvalidConfig),
    }
}

fn complete_confirmation(
    mut session: NoiseSession,
    initiator: bool,
    deadline: Instant,
) -> Result<NoiseSession, SecureWorkerError> {
    if initiator {
        session.send_inner_frame_before(
            FRAME_VERSION,
            FRAME_CONFIRM,
            0,
            CONFIRM_PAYLOAD,
            deadline,
        )?;
        let (frame_type, payload) = session.recv_inner_before(deadline)?;
        if frame_type != FRAME_CONFIRM || payload != CONFIRM_PAYLOAD {
            return Err(SecureWorkerError::HandshakeFailed);
        }
    } else {
        let (frame_type, payload) = session.recv_inner_before(deadline)?;
        if frame_type != FRAME_CONFIRM || payload != CONFIRM_PAYLOAD {
            return Err(SecureWorkerError::HandshakeFailed);
        }
        session.send_inner_frame_before(
            FRAME_VERSION,
            FRAME_CONFIRM,
            0,
            CONFIRM_PAYLOAD,
            deadline,
        )?;
    }
    Ok(session)
}

#[derive(Clone, Serialize, Deserialize)]
struct PeerMeta {
    peer_id: String,
    static_public: String,
    trust: String,
    epoch: u64,
    trusted: bool,
    created_at: u64,
}

const KEYRING_SERVICE: &str = "fastlink.secure-worker.v1";

/// Backend-neutral keyring item identifier using opaque hashed scopes/accounts.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct KeyringLocator {
    service: String,
    account: String,
    label: String,
}

/// Safe failure class for keyring storage providers.
///
/// Unit variants carry no secret material, D-Bus errors, peer IDs, or paths.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyringBackendError {
    Unavailable,
    AccessDenied,
    InvalidData,
    Unsupported,
}

/// Narrow secret-storage backend abstraction for OS keyring integration and
/// private in-crate unit testing.
trait KeyringBackend: Send + Sync {
    fn get_secret(
        &self,
        locator: &KeyringLocator,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, KeyringBackendError>;

    fn set_secret(
        &self,
        locator: &KeyringLocator,
        secret_utf8: &str,
    ) -> Result<(), KeyringBackendError>;

    /// Deletes any matching entry; returns `true` if deleted or `false` if absent.
    fn delete_secret(&self, locator: &KeyringLocator) -> Result<bool, KeyringBackendError>;
}

#[cfg(target_os = "linux")]
struct SecretServiceBackend {
    store: Arc<zbus_secret_service_keyring_store::Store>,
}

#[cfg(target_os = "linux")]
impl SecretServiceBackend {
    fn connect() -> Result<Self, SecureWorkerError> {
        let store = zbus_secret_service_keyring_store::Store::new()
            .map_err(|err| map_keyring_backend_error(map_keyring_core_error(&err)))?;
        Ok(Self { store })
    }

    fn entry_for(
        &self,
        locator: &KeyringLocator,
    ) -> Result<keyring_core::Entry, KeyringBackendError> {
        use keyring_core::api::CredentialStoreApi;

        let modifiers = HashMap::from([("label", locator.label.as_str())]);
        self.store
            .build(&locator.service, &locator.account, Some(&modifiers))
            .map_err(|err| map_keyring_core_error(&err))
    }
}

#[cfg(target_os = "linux")]
impl KeyringBackend for SecretServiceBackend {
    fn get_secret(
        &self,
        locator: &KeyringLocator,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, KeyringBackendError> {
        let entry = self.entry_for(locator)?;
        match entry.get_secret() {
            Ok(bytes) => Ok(Some(Zeroizing::new(bytes))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(err) => Err(map_keyring_core_error(&err)),
        }
    }

    fn set_secret(
        &self,
        locator: &KeyringLocator,
        secret_utf8: &str,
    ) -> Result<(), KeyringBackendError> {
        let entry = self.entry_for(locator)?;
        entry
            .set_secret(secret_utf8.as_bytes())
            .map_err(|err| map_keyring_core_error(&err))
    }

    fn delete_secret(&self, locator: &KeyringLocator) -> Result<bool, KeyringBackendError> {
        let entry = self.entry_for(locator)?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(err) => Err(map_keyring_core_error(&err)),
        }
    }
}

#[cfg(target_os = "linux")]
fn map_keyring_core_error(err: &keyring_core::Error) -> KeyringBackendError {
    match err {
        keyring_core::Error::NoStorageAccess(_) => KeyringBackendError::AccessDenied,
        keyring_core::Error::BadEncoding(_)
        | keyring_core::Error::BadDataFormat(_, _)
        | keyring_core::Error::BadStoreFormat(_) => KeyringBackendError::InvalidData,
        keyring_core::Error::NotSupportedByStore(_) => KeyringBackendError::Unsupported,
        _ => KeyringBackendError::Unavailable,
    }
}

fn map_keyring_backend_error(err: KeyringBackendError) -> SecureWorkerError {
    match err {
        KeyringBackendError::Unavailable | KeyringBackendError::AccessDenied => {
            SecureWorkerError::KeyringUnavailable
        }
        KeyringBackendError::InvalidData => SecureWorkerError::StoreCorrupt,
        KeyringBackendError::Unsupported => SecureWorkerError::Unsupported,
    }
}

#[derive(Serialize, Deserialize)]
struct StoreState {
    trust_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity_public: Option<String>,
}

struct PeerRecord {
    static_public: [u8; 32],
    trust: NetworkTrust,
    pairing_secret: Secret32,
}

struct PendingPairing {
    peer_id: String,
    trust: NetworkTrust,
    epoch: u64,
    challenge: PairingChallenge,
    secret: Secret32,
    expires_at: u64,
}

#[cfg(test)]
#[derive(Default)]
struct PersistFaults {
    fail_meta_write: bool,
    fail_state_write: bool,
    fail_meta_remove: bool,
}

/// Durable local trust store.
///
/// Metadata directory mode is 0700 and metadata/state files are 0600 on Unix.
/// Persistent identity private keys and per-peer pairing secrets are stored in
/// the Linux Secret Service keyring rather than plaintext files. Metadata
/// contains no routing endpoint or secret material. Corrupt peer records are
/// ignored (treated as untrusted) so a damaged store fails closed and requires
/// re-pairing.
///
/// Production builds only expose the OS-keyring-backed constructor:
///
/// ```no_run
/// use fastlink::secure_worker::TrustStore;
///
/// let _store = TrustStore::open("/var/lib/fastlink-trust")?;
/// # Ok::<(), fastlink::secure_worker::SecureWorkerError>(())
/// ```
///
/// External crates cannot access or inject alternate keyring backends:
///
/// ```compile_fail
/// use fastlink::secure_worker::KeyringBackend;
/// ```
///
/// ```compile_fail
/// use fastlink::secure_worker::KeyringLocator;
/// ```
///
/// ```compile_fail
/// use fastlink::secure_worker::KeyringBackendError;
/// ```
///
/// ```compile_fail
/// use fastlink::secure_worker::TrustStore;
///
/// fn open_custom(root: &std::path::Path) -> Result<TrustStore, fastlink::secure_worker::SecureWorkerError> {
///     TrustStore::open_with_keyring_backend(root, todo!())
/// }
/// ```
pub struct TrustStore {
    root: PathBuf,
    peers_dir: PathBuf,
    store_scope: String,
    keyring: Arc<dyn KeyringBackend>,
    identity_private: Secret32,
    identity_public: [u8; 32],
    trust_epoch: u64,
    peers: HashMap<String, PeerRecord>,
    pending: HashMap<[u8; 32], PendingPairing>,
    #[cfg(test)]
    persist_faults: PersistFaults,
}

impl TrustStore {
    /// Opens (and if necessary initializes or migrates) the trust store at
    /// `root`, backed by the Linux Secret Service keyring.
    ///
    /// Fails closed if the Secret Service keyring is unavailable or locked, and
    /// returns [`SecureWorkerError::Unsupported`] on non-Linux targets.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SecureWorkerError> {
        #[cfg(target_os = "linux")]
        {
            let backend = Arc::new(SecretServiceBackend::connect()?);
            Self::open_with_keyring_backend(root, backend)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = root;
            Err(SecureWorkerError::Unsupported)
        }
    }

    /// Opens (and if necessary initializes or migrates) the trust store using an
    /// injected keyring backend implementation.
    #[cfg(any(target_os = "linux", test))]
    fn open_with_keyring_backend(
        root: impl AsRef<Path>,
        keyring: Arc<dyn KeyringBackend>,
    ) -> Result<Self, SecureWorkerError> {
        let root = root.as_ref().to_path_buf();
        ensure_root_dir(&root)?;
        let peers_dir = root.join("peers");
        ensure_peers_dir(&peers_dir)?;
        cleanup_temporary_files(&root)?;
        cleanup_temporary_files(&peers_dir)?;

        let canonical_root = fs::canonicalize(&root)?;
        let store_scope = store_scope_digest(&canonical_root);
        let state = load_state(&root)?;
        let has_peers = has_any_peer_metadata(&peers_dir)?;
        let (identity_private, identity_public) = load_or_create_identity(
            &root,
            &store_scope,
            &*keyring,
            state.trust_epoch,
            state.identity_public,
            has_peers,
        )?;
        let mut store = Self {
            root,
            peers_dir,
            store_scope,
            keyring,
            identity_private,
            identity_public,
            trust_epoch: state.trust_epoch,
            peers: HashMap::new(),
            pending: HashMap::new(),
            #[cfg(test)]
            persist_faults: PersistFaults::default(),
        };
        store.load_peers()?;
        Ok(store)
    }

    /// Installation X25519 private key (backed by OS keyring).
    pub fn local_static_private(&self) -> Secret32 {
        self.identity_private.clone()
    }

    /// Installation X25519 public key.
    pub fn local_static_public(&self) -> [u8; 32] {
        self.identity_public
    }

    /// Begins an explicit pairing ceremony.
    ///
    /// The returned offer is provisional; it is not trusted and cannot accept a
    /// worker session until [`Self::accept_pairing`] succeeds.
    pub fn begin_pairing(
        &mut self,
        peer_id: &str,
        trust: NetworkTrust,
        opted_in: bool,
    ) -> Result<PairingOffer, SecureWorkerError> {
        if !valid_peer_id(peer_id) {
            return Err(SecureWorkerError::Pairing(PairingError::InvalidPeerId));
        }
        if !opted_in {
            return Err(SecureWorkerError::Pairing(PairingError::NotOptedIn));
        }
        if !trust.is_permitted() {
            return Err(SecureWorkerError::Pairing(PairingError::UntrustedNetwork));
        }
        self.expire_pending();

        let secret = Secret32::random()?;
        let nonce_secret = Secret32::random()?;
        let nonce = *nonce_secret.expose();
        let epoch = self.next_epoch()?;
        let challenge = PairingChallenge {
            peer_id: peer_id.to_string(),
            nonce,
            epoch,
        };
        let expires_at = now_secs().saturating_add(PAIRING_OFFER_TTL_SECS);
        self.pending.insert(
            nonce,
            PendingPairing {
                peer_id: peer_id.to_string(),
                trust,
                epoch,
                challenge: challenge.clone(),
                secret: secret.clone(),
                expires_at,
            },
        );
        Ok(PairingOffer {
            peer_id: peer_id.to_string(),
            trust,
            epoch,
            challenge,
            secret,
        })
    }

    /// Completes pairing after explicit local confirmation and a verified proof.
    ///
    /// The caller supplies the peer's X25519 static public key observed during
    /// the authenticated pairing exchange. The proof must authenticate that
    /// exact key; a valid proof for a different key is rejected and the trust
    /// record is not written. A production caller must not accept this key from
    /// unauthenticated discovery data.
    pub fn accept_pairing(
        &mut self,
        offer: &PairingOffer,
        proof: &PairingProof,
        consent: PairingConsent,
        peer_static_public: [u8; 32],
    ) -> Result<PairingSession, SecureWorkerError> {
        let pending =
            self.pending
                .remove(&offer.challenge.nonce)
                .ok_or(SecureWorkerError::Pairing(
                    PairingError::StaleOrReplayedEpoch,
                ))?;

        if now_secs() > pending.expires_at || pending.epoch < self.trust_epoch {
            return Err(SecureWorkerError::Pairing(
                PairingError::StaleOrReplayedEpoch,
            ));
        }
        if pending.peer_id != offer.peer_id
            || pending.trust != offer.trust
            || pending.epoch != offer.epoch
            || !pending.secret.constant_time_eq(&offer.secret)
            || pending.challenge != offer.challenge
        {
            return Err(SecureWorkerError::InvalidPairingOffer);
        }
        if !consent.approved {
            return Err(SecureWorkerError::PairingApprovalRequired);
        }
        if peer_static_public == [0u8; 32] || proof.static_public == [0u8; 32] {
            return Err(SecureWorkerError::InvalidPeerKey);
        }
        if !constant_time_eq_32(&proof.static_public, &peer_static_public) {
            return Err(SecureWorkerError::Pairing(PairingError::InvalidProof));
        }

        let peer = PeerIdentity {
            peer_id: pending.peer_id.clone(),
            os: String::new(),
            runtime: String::new(),
            trust: pending.trust,
            opted_in: true,
        };
        let mut registry = PairingRegistry::new(*pending.secret.expose(), 0);
        registry.issue_challenge(&peer, pending.challenge.nonce, pending.epoch)?;
        let session = registry.verify_and_admit(&peer, &pending.challenge, proof)?;
        if session.peer_id != pending.peer_id {
            return Err(SecureWorkerError::InvalidPairingOffer);
        }

        let stem = peer_file_stem(&pending.peer_id);
        let meta_path = self.peers_dir.join(format!("{stem}.meta.json"));
        let peer_loc = peer_locator(&self.store_scope, &pending.peer_id);
        write_and_verify_keyring_secret(&*self.keyring, &peer_loc, &pending.secret)?;
        let meta = PeerMeta {
            peer_id: pending.peer_id.clone(),
            static_public: hex_encode(&proof.static_public),
            trust: trust_label(pending.trust).to_string(),
            epoch: pending.epoch,
            trusted: true,
            created_at: now_secs(),
        };
        let encoded = serde_json::to_vec(&meta).map_err(|_| SecureWorkerError::StoreCorrupt)?;
        if let Err(error) = atomic_write(&meta_path, &encoded, 0o600) {
            // Do not leave an orphaned pairing secret in the keyring if the
            // trust metadata write fails.
            let _ = self.keyring.delete_secret(&peer_loc);
            return Err(error);
        }

        self.peers.insert(
            pending.peer_id.clone(),
            PeerRecord {
                static_public: proof.static_public,
                trust: pending.trust,
                pairing_secret: pending.secret,
            },
        );
        Ok(session)
    }

    /// Returns true only for a fully loaded, trusted record.
    pub fn is_trusted(&self, peer_id: &str) -> bool {
        self.peers.contains_key(peer_id)
    }

    /// Pinned X25519 static public key for a trusted peer.
    pub fn peer_static(&self, peer_id: &str) -> Option<[u8; 32]> {
        self.peers.get(peer_id).map(|record| record.static_public)
    }

    /// Pairing secret for a trusted peer (used to derive a fresh session PSK in
    /// a future reconnect ceremony).
    pub fn peer_pairing_secret(&self, peer_id: &str) -> Option<Secret32> {
        self.peers
            .get(peer_id)
            .map(|record| record.pairing_secret.clone())
    }

    /// Trust tier recorded for a trusted peer.
    pub fn peer_trust(&self, peer_id: &str) -> Option<NetworkTrust> {
        self.peers.get(peer_id).map(|record| record.trust)
    }

    /// Removes trust and credentials for `peer_id`.
    ///
    /// Revokes in-memory trust, marks metadata revoked on disk, deletes the
    /// keyring entry idempotently, and deletes metadata/legacy files. Fails
    /// closed if keyring deletion fails.
    ///
    /// This is storage-level revocation. To close a live connection at the same
    /// time, use [`WorkerServer::remove_peer`].
    pub fn remove_peer(&mut self, peer_id: &str) -> Result<bool, SecureWorkerError> {
        let stem = peer_file_stem(peer_id);
        let meta_path = self.peers_dir.join(format!("{stem}.meta.json"));
        let secret_path = self.peers_dir.join(format!("{stem}.secret"));
        let peer_loc = peer_locator(&self.store_scope, peer_id);

        let local_existed =
            meta_path.exists() || secret_path.exists() || self.peers.contains_key(peer_id);

        fn record_error(
            first_error: &mut Option<SecureWorkerError>,
            res: Result<(), SecureWorkerError>,
        ) {
            if let Err(err) = res {
                if first_error.is_none() {
                    *first_error = Some(err);
                }
            }
        }

        // Revoke live trust and pending offers immediately so the process fails closed.
        self.peers.remove(peer_id);
        self.pending.retain(|_, pending| pending.peer_id != peer_id);

        let mut first_error = None;
        let mut meta_safe_on_disk = !meta_path.exists();
        if meta_path.exists() {
            match fs::read(&meta_path) {
                Ok(bytes) => match serde_json::from_slice::<PeerMeta>(&bytes) {
                    Ok(mut meta) => {
                        meta.trusted = false;
                        match serde_json::to_vec(&meta) {
                            Ok(encoded) => {
                                let res = self.write_peer_meta(&meta_path, &encoded);
                                if res.is_ok() {
                                    meta_safe_on_disk = true;
                                }
                                record_error(&mut first_error, res);
                            }
                            Err(_) => {
                                record_error(
                                    &mut first_error,
                                    Err(SecureWorkerError::StoreCorrupt),
                                );
                            }
                        }
                    }
                    Err(_) => {
                        // Corrupt metadata is ignored by load_peers and unlinked below.
                        meta_safe_on_disk = true;
                    }
                },
                Err(err) => record_error(&mut first_error, Err(map_io(err))),
            }
        }
        if local_existed {
            self.trust_epoch = now_secs().max(self.trust_epoch.saturating_add(1));
            record_error(&mut first_error, self.persist_state());
        }

        let keyring_deleted = match self
            .keyring
            .delete_secret(&peer_loc)
            .map_err(map_keyring_backend_error)
        {
            Ok(deleted) => deleted,
            Err(err) => {
                record_error(&mut first_error, Err(err));
                if !meta_safe_on_disk {
                    record_error(&mut first_error, self.remove_peer_meta(&meta_path));
                    record_error(&mut first_error, remove_file_if_present(&secret_path));
                }
                return Err(first_error.unwrap_or(SecureWorkerError::KeyringUnavailable));
            }
        };
        if !local_existed && !keyring_deleted {
            if let Some(err) = first_error {
                return Err(err);
            }
            return Ok(false);
        }
        if !local_existed && keyring_deleted {
            self.trust_epoch = now_secs().max(self.trust_epoch.saturating_add(1));
            record_error(&mut first_error, self.persist_state());
        }

        record_error(&mut first_error, self.remove_peer_meta(&meta_path));
        record_error(&mut first_error, remove_file_if_present(&secret_path));
        if let Some(err) = first_error {
            return Err(err);
        }
        Ok(true)
    }

    fn next_epoch(&mut self) -> Result<u64, SecureWorkerError> {
        let epoch = now_secs().max(self.trust_epoch.saturating_add(1));
        self.trust_epoch = epoch;
        self.persist_state()?;
        Ok(epoch)
    }

    fn write_peer_meta(&self, path: &Path, encoded: &[u8]) -> Result<(), SecureWorkerError> {
        #[cfg(test)]
        if self.persist_faults.fail_meta_write {
            return Err(SecureWorkerError::Io(io::Error::other(
                "injected peer metadata write failure",
            )));
        }
        atomic_write(path, encoded, 0o600)
    }

    fn remove_peer_meta(&self, path: &Path) -> Result<(), SecureWorkerError> {
        #[cfg(test)]
        if self.persist_faults.fail_meta_remove {
            return Err(SecureWorkerError::Io(io::Error::other(
                "injected peer metadata remove failure",
            )));
        }
        remove_file_if_present(path)
    }

    fn persist_state(&self) -> Result<(), SecureWorkerError> {
        #[cfg(test)]
        if self.persist_faults.fail_state_write {
            return Err(SecureWorkerError::Io(io::Error::other(
                "injected state write failure",
            )));
        }
        persist_state_fields(&self.root, self.trust_epoch, Some(&self.identity_public))
    }

    fn expire_pending(&mut self) {
        let now = now_secs();
        self.pending.retain(|_, pending| pending.expires_at >= now);
    }

    #[cfg(any(target_os = "linux", test))]
    fn load_peers(&mut self) -> Result<(), SecureWorkerError> {
        let entries = fs::read_dir(&self.peers_dir)?;
        for entry in entries {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".meta.json") else {
                continue;
            };
            let meta_path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&meta_path) else {
                continue;
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 16_384 {
                continue;
            }
            let Ok(bytes) = fs::read(&meta_path) else {
                continue;
            };
            let Ok(meta) = serde_json::from_slice::<PeerMeta>(&bytes) else {
                continue;
            };
            if !meta.trusted
                || meta.peer_id.trim().is_empty()
                || peer_file_stem(&meta.peer_id) != stem
            {
                continue;
            }
            let Some(static_public) = hex_decode_32(&meta.static_public) else {
                continue;
            };
            if static_public == [0u8; 32] {
                continue;
            }
            let Some(trust) = parse_trust(&meta.trust) else {
                continue;
            };
            if !trust.is_permitted() {
                continue;
            }
            if restrict_permissions(&meta_path, 0o600).is_err() {
                continue;
            }

            let peer_loc = peer_locator(&self.store_scope, &meta.peer_id);
            let secret_path = self.peers_dir.join(format!("{stem}.secret"));
            let pairing_secret = match fs::symlink_metadata(&secret_path) {
                Ok(secret_meta) => {
                    if secret_meta.file_type().is_symlink()
                        || !secret_meta.is_file()
                        || secret_meta.len() != 32
                    {
                        continue;
                    }
                    restrict_permissions(&secret_path, 0o600)?;
                    let secret_bytes = Zeroizing::new(fs::read(&secret_path)?);
                    if secret_bytes.len() != 32 {
                        continue;
                    }
                    let mut raw = Zeroizing::new([0u8; 32]);
                    raw.copy_from_slice(&secret_bytes);
                    if *raw == [0u8; 32] {
                        continue;
                    }
                    let legacy_secret = Secret32::from_zeroizing(raw);
                    migrate_or_reconcile_legacy_secret(&*self.keyring, &peer_loc, &legacy_secret)?;
                    fs::remove_file(&secret_path).map_err(map_io)?;
                    legacy_secret
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    let Some(keyring_secret) = read_keyring_secret32(&*self.keyring, &peer_loc)?
                    else {
                        continue;
                    };
                    keyring_secret
                }
                Err(err) => return Err(map_io(err)),
            };

            self.trust_epoch = self.trust_epoch.max(meta.epoch);
            self.peers.insert(
                meta.peer_id.clone(),
                PeerRecord {
                    static_public,
                    trust,
                    pairing_secret,
                },
            );
        }
        Ok(())
    }
}

impl fmt::Debug for TrustStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustStore")
            .field("peer_count", &self.peers.len())
            .field("pending_count", &self.pending.len())
            .field("trust_epoch", &self.trust_epoch)
            .finish()
    }
}

#[cfg(any(target_os = "linux", test))]
fn store_scope_digest(canonical_root: &Path) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"FASTLINK-KEYRING-ROOT-V1");
    hasher.update(canonical_root.as_os_str().as_encoded_bytes());
    hasher.finalize().to_hex().to_string()
}

#[cfg(any(target_os = "linux", test))]
fn identity_locator(store_scope: &str) -> KeyringLocator {
    let short = &store_scope[..16];
    KeyringLocator {
        service: KEYRING_SERVICE.to_string(),
        account: format!("id-{store_scope}"),
        label: format!("fastlink-identity-{short}"),
    }
}

fn peer_locator(store_scope: &str, peer_id: &str) -> KeyringLocator {
    let peer_hash = peer_file_stem(peer_id);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"FASTLINK-KEYRING-PEER-V1");
    hasher.update(store_scope.as_bytes());
    hasher.update(peer_hash.as_bytes());
    let account_hash = hasher.finalize().to_hex().to_string();
    let short = &account_hash[..16];
    KeyringLocator {
        service: KEYRING_SERVICE.to_string(),
        account: format!("peer-{account_hash}"),
        label: format!("fastlink-peer-{short}"),
    }
}

fn secret32_to_hex(secret: &Secret32) -> Zeroizing<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Zeroizing::new(String::with_capacity(64));
    for &byte in secret.expose() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn secret32_from_hex_bytes(bytes: &[u8]) -> Result<Secret32, SecureWorkerError> {
    if bytes.len() != 64 {
        return Err(SecureWorkerError::StoreCorrupt);
    }
    let mut decoded = Zeroizing::new([0u8; 32]);
    for index in 0..32 {
        let high = hex_nibble(bytes[index * 2]).ok_or(SecureWorkerError::StoreCorrupt)?;
        let low = hex_nibble(bytes[index * 2 + 1]).ok_or(SecureWorkerError::StoreCorrupt)?;
        decoded[index] = (high << 4) | low;
    }
    if *decoded == [0u8; 32] {
        return Err(SecureWorkerError::StoreCorrupt);
    }
    Ok(Secret32::from_zeroizing(decoded))
}

fn read_keyring_secret32(
    backend: &dyn KeyringBackend,
    locator: &KeyringLocator,
) -> Result<Option<Secret32>, SecureWorkerError> {
    let Some(raw) = backend
        .get_secret(locator)
        .map_err(map_keyring_backend_error)?
    else {
        return Ok(None);
    };
    let secret = secret32_from_hex_bytes(&raw)?;
    Ok(Some(secret))
}

fn write_and_verify_keyring_secret(
    backend: &dyn KeyringBackend,
    locator: &KeyringLocator,
    secret: &Secret32,
) -> Result<(), SecureWorkerError> {
    let encoded = secret32_to_hex(secret);
    backend
        .set_secret(locator, encoded.as_str())
        .map_err(map_keyring_backend_error)?;
    let readback =
        read_keyring_secret32(backend, locator)?.ok_or(SecureWorkerError::KeyringUnavailable)?;
    if !readback.constant_time_eq(secret) {
        return Err(SecureWorkerError::KeyringUnavailable);
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn migrate_or_reconcile_legacy_secret(
    backend: &dyn KeyringBackend,
    locator: &KeyringLocator,
    legacy_secret: &Secret32,
) -> Result<(), SecureWorkerError> {
    if let Some(existing) = read_keyring_secret32(backend, locator)? {
        if !existing.constant_time_eq(legacy_secret) {
            return Err(SecureWorkerError::KeyringConflict);
        }
        return Ok(());
    }
    write_and_verify_keyring_secret(backend, locator, legacy_secret)
}

fn restrict_permissions(path: &Path, mode: u32) -> Result<(), SecureWorkerError> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|_| SecureWorkerError::StorePermission)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn prepare_private_dir(
    path: &Path,
    allowed_entry: impl Fn(&str) -> bool,
) -> Result<(), SecureWorkerError> {
    let existed = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SecureWorkerError::StoreCorrupt);
            }
            true
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            false
        }
        Err(err) => return Err(map_io(err)),
    };
    if existed {
        // Refuse to chmod an unrelated directory passed by mistake. Only the
        // known store layout and temporary files are accepted.
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(SecureWorkerError::StorePermission);
            };
            if !name.starts_with('.') && !allowed_entry(name) {
                return Err(SecureWorkerError::StorePermission);
            }
        }
    }
    restrict_permissions(path, 0o700)?;
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn ensure_root_dir(path: &Path) -> Result<(), SecureWorkerError> {
    if path.as_os_str().is_empty()
        || path == Path::new("/")
        || path == Path::new(".")
        || path == Path::new("..")
    {
        return Err(SecureWorkerError::StorePermission);
    }
    prepare_private_dir(path, |name| {
        matches!(name, "identity.key" | "state.json" | "peers")
    })
}

#[cfg(any(target_os = "linux", test))]
fn ensure_peers_dir(path: &Path) -> Result<(), SecureWorkerError> {
    prepare_private_dir(path, |name| {
        name.ends_with(".meta.json") || name.ends_with(".secret")
    })
}

#[cfg(any(target_os = "linux", test))]
fn has_any_peer_metadata(peers_dir: &Path) -> Result<bool, SecureWorkerError> {
    for entry in fs::read_dir(peers_dir)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if name.ends_with(".meta.json") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(any(target_os = "linux", test))]
fn cleanup_temporary_files(dir: &Path) -> Result<(), SecureWorkerError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if name.contains(".tmp-") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn load_or_create_identity(
    root: &Path,
    store_scope: &str,
    keyring: &dyn KeyringBackend,
    trust_epoch: u64,
    existing_identity_public: Option<[u8; 32]>,
    has_peers: bool,
) -> Result<(Secret32, [u8; 32]), SecureWorkerError> {
    let id_loc = identity_locator(store_scope);
    let legacy_path = root.join("identity.key");
    match fs::symlink_metadata(&legacy_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != 64 {
                return Err(SecureWorkerError::StoreCorrupt);
            }
            let bytes = Zeroizing::new(fs::read(&legacy_path)?);
            if bytes.len() != 64 {
                return Err(SecureWorkerError::StoreCorrupt);
            }
            restrict_permissions(&legacy_path, 0o600)?;
            let mut private_buf = Zeroizing::new([0u8; 32]);
            let mut public = [0u8; 32];
            private_buf.copy_from_slice(&bytes[0..32]);
            public.copy_from_slice(&bytes[32..64]);
            if *private_buf == [0u8; 32] || public == [0u8; 32] {
                return Err(SecureWorkerError::StoreCorrupt);
            }
            if let Some(state_pub) = existing_identity_public {
                if !constant_time_eq_32(&state_pub, &public) {
                    return Err(SecureWorkerError::StoreCorrupt);
                }
            }
            let legacy_private = Secret32::from_zeroizing(private_buf);
            let derived_public = derive_x25519_public(&legacy_private)?;
            if derived_public == [0u8; 32] || !constant_time_eq_32(&derived_public, &public) {
                return Err(SecureWorkerError::StoreCorrupt);
            }
            migrate_or_reconcile_legacy_secret(keyring, &id_loc, &legacy_private)?;
            persist_state_fields(root, trust_epoch, Some(&public))?;
            fs::remove_file(&legacy_path).map_err(map_io)?;
            Ok((legacy_private, public))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(existing_secret) = read_keyring_secret32(keyring, &id_loc)? {
                let public = existing_identity_public.ok_or(SecureWorkerError::StoreCorrupt)?;
                let derived_public = derive_x25519_public(&existing_secret)?;
                if derived_public == [0u8; 32] || !constant_time_eq_32(&derived_public, &public) {
                    return Err(SecureWorkerError::StoreCorrupt);
                }
                return Ok((existing_secret, public));
            }
            if existing_identity_public.is_some() || has_peers {
                return Err(SecureWorkerError::StoreCorrupt);
            }

            let params = NOISE_PROTOCOL
                .parse()
                .map_err(|_| SecureWorkerError::InvalidConfig)?;
            let mut keypair = snow::Builder::new(params)
                .generate_keypair()
                .map_err(|_| SecureWorkerError::Entropy)?;
            if keypair.private.len() != 32 || keypair.public.len() != 32 {
                keypair.private.zeroize();
                return Err(SecureWorkerError::Entropy);
            }
            let mut private_buf = Zeroizing::new([0u8; 32]);
            let mut public = [0u8; 32];
            private_buf.copy_from_slice(&keypair.private);
            public.copy_from_slice(&keypair.public);
            keypair.private.zeroize();
            if *private_buf == [0u8; 32] || public == [0u8; 32] {
                return Err(SecureWorkerError::Entropy);
            }
            let private = Secret32::from_zeroizing(private_buf);
            write_and_verify_keyring_secret(keyring, &id_loc, &private)?;
            if let Err(err) = persist_state_fields(root, trust_epoch, Some(&public)) {
                let _ = keyring.delete_secret(&id_loc);
                return Err(err);
            }
            Ok((private, public))
        }
        Err(err) => Err(map_io(err)),
    }
}

#[cfg(any(target_os = "linux", test))]
fn derive_x25519_public(private: &Secret32) -> Result<[u8; 32], SecureWorkerError> {
    use snow::resolvers::{CryptoResolver, DefaultResolver};

    let mut dh = DefaultResolver
        .resolve_dh(&snow::params::DHChoice::Curve25519)
        .ok_or(SecureWorkerError::InvalidConfig)?;
    if dh.priv_len() != 32 || dh.pub_len() != 32 {
        return Err(SecureWorkerError::InvalidConfig);
    }
    dh.set(private.expose());
    let pub_slice = dh.pubkey();
    if pub_slice.len() != 32 {
        let zero = Zeroizing::new([0u8; 32]);
        dh.set(&*zero);
        return Err(SecureWorkerError::StoreCorrupt);
    }
    let mut derived = [0u8; 32];
    derived.copy_from_slice(pub_slice);
    let zero = Zeroizing::new([0u8; 32]);
    dh.set(&*zero);
    Ok(derived)
}

#[cfg(any(target_os = "linux", test))]
struct LoadedState {
    trust_epoch: u64,
    identity_public: Option<[u8; 32]>,
}

#[cfg(any(target_os = "linux", test))]
fn load_state(root: &Path) -> Result<LoadedState, SecureWorkerError> {
    let path = root.join("state.json");
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SecureWorkerError::StoreCorrupt);
        }
    }
    match fs::read(&path) {
        Ok(bytes) => {
            if bytes.len() > 4_096 {
                return Err(SecureWorkerError::StoreCorrupt);
            }
            let state: StoreState =
                serde_json::from_slice(&bytes).map_err(|_| SecureWorkerError::StoreCorrupt)?;
            restrict_permissions(&path, 0o600)?;
            let identity_public = match state.identity_public {
                Some(ref text) => {
                    let pub_key = hex_decode_32(text).ok_or(SecureWorkerError::StoreCorrupt)?;
                    if pub_key == [0u8; 32] {
                        return Err(SecureWorkerError::StoreCorrupt);
                    }
                    Some(pub_key)
                }
                None => None,
            };
            Ok(LoadedState {
                trust_epoch: state.trust_epoch,
                identity_public,
            })
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(LoadedState {
            trust_epoch: 0,
            identity_public: None,
        }),
        Err(err) => Err(map_io(err)),
    }
}

fn persist_state_fields(
    root: &Path,
    trust_epoch: u64,
    identity_public: Option<&[u8; 32]>,
) -> Result<(), SecureWorkerError> {
    let state = StoreState {
        trust_epoch,
        identity_public: identity_public.map(|key| hex_encode(key)),
    };
    let encoded = serde_json::to_vec(&state).map_err(|_| SecureWorkerError::StoreCorrupt)?;
    atomic_write(&root.join("state.json"), &encoded, 0o600)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<(), SecureWorkerError> {
    let parent = path.parent().ok_or(SecureWorkerError::StoreCorrupt)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SecureWorkerError::StoreCorrupt)?;
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()));
    let result = (|| -> Result<(), SecureWorkerError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(mode);
        let mut file = options.open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        restrict_permissions(&temporary, mode)?;
        fs::rename(&temporary, path)?;
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_file_if_present(path: &Path) -> Result<(), SecureWorkerError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(map_io(err)),
    }
}

fn peer_file_stem(peer_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"FASTLINK-PEER-STORE-V1");
    hasher.update(peer_id.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn trust_label(trust: NetworkTrust) -> &'static str {
    match trust {
        NetworkTrust::Loopback => "loopback",
        NetworkTrust::Lan => "lan",
        NetworkTrust::Tailscale => "tailscale",
        NetworkTrust::Untrusted => "untrusted",
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_trust(label: &str) -> Option<NetworkTrust> {
    match label {
        "loopback" => Some(NetworkTrust::Loopback),
        "lan" => Some(NetworkTrust::Lan),
        "tailscale" => Some(NetworkTrust::Tailscale),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(any(target_os = "linux", test))]
fn hex_decode_32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = text.as_bytes();
    for index in 0..32 {
        let high = hex_nibble(bytes[index * 2])?;
        let low = hex_nibble(bytes[index * 2 + 1])?;
        out[index] = (high << 4) | low;
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Explicit worker configuration.
///
/// The PSK is the pairing session key derived by [`TrustStore::accept_pairing`].
/// It is used only as the Noise `psk3` value.
pub struct WorkerConfig {
    pub peer_id: String,
    pub psk: Secret32,
    pub prologue: Vec<u8>,
    pub expected_remote_static: [u8; 32],
    pub handshake_timeout: Duration,
    pub frame_timeout: Duration,
}

impl fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("peer_id", &self.peer_id)
            .field("psk", &"[REDACTED]")
            .field("prologue_len", &self.prologue.len())
            .field("expected_remote_static", &"[PINNED]")
            .field("handshake_timeout", &self.handshake_timeout)
            .field("frame_timeout", &self.frame_timeout)
            .finish()
    }
}

/// Loopback-only, explicitly started worker listener.
///
/// Start is explicit. No discovery, GUI launch or inventory refresh starts a
/// worker. `Drop` stops the listener and closes tracked sessions.
pub struct WorkerServer {
    local_addr: SocketAddr,
    running: Arc<AtomicBool>,
    controls: Arc<Mutex<Vec<Vec<u8>>>>,
    active: Arc<Mutex<Vec<TcpStream>>>,
    join: Option<JoinHandle<()>>,
    store: Arc<Mutex<TrustStore>>,
    peer_id: String,
}

impl WorkerServer {
    /// Starts a loopback-only worker after verifying the peer's durable trust
    /// record and pinned static key.
    pub fn start(
        store: Arc<Mutex<TrustStore>>,
        config: WorkerConfig,
    ) -> Result<Self, SecureWorkerError> {
        if !valid_peer_id(&config.peer_id)
            || config.handshake_timeout.is_zero()
            || config.frame_timeout.is_zero()
            || config.prologue.is_empty()
            || config.prologue.len() > MAX_PROLOGUE_BYTES
            || config.expected_remote_static == [0u8; 32]
            || config.psk.expose() == &[0u8; 32]
        {
            return Err(SecureWorkerError::InvalidConfig);
        }
        {
            let store = store.lock().map_err(|_| SecureWorkerError::LockPoisoned)?;
            if !store.is_trusted(&config.peer_id) {
                return Err(SecureWorkerError::UntrustedPeer);
            }
            match store.peer_static(&config.peer_id) {
                Some(pinned) if constant_time_eq_32(&pinned, &config.expected_remote_static) => {}
                Some(_) => return Err(SecureWorkerError::StaticKeyMismatch),
                None => return Err(SecureWorkerError::UntrustedPeer),
            }
        }

        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let running = Arc::new(AtomicBool::new(true));
        let controls = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new(Vec::new()));

        let thread_running = Arc::clone(&running);
        let thread_controls = Arc::clone(&controls);
        let thread_active = Arc::clone(&active);
        let thread_store = Arc::clone(&store);
        let thread_peer = config.peer_id.clone();
        let handshake_timeout = config.handshake_timeout;
        let frame_timeout = config.frame_timeout;
        let expected_remote_static = config.expected_remote_static;
        let psk = config.psk.clone();
        let prologue = config.prologue.clone();

        let join = thread::spawn(move || {
            let local_private = match thread_store.lock() {
                Ok(store) => store.local_static_private(),
                Err(_) => {
                    thread_running.store(false, Ordering::SeqCst);
                    return;
                }
            };
            let noise_config = NoiseConfig::new(
                local_private,
                psk,
                prologue,
                Some(expected_remote_static),
                handshake_timeout,
            );
            while thread_running.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let active_count = thread_active
                            .lock()
                            .map(|active| active.len())
                            .unwrap_or(usize::MAX);
                        if active_count >= MAX_WORKER_CONNECTIONS {
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            continue;
                        }
                        if set_stream_timeouts(&stream, handshake_timeout).is_err() {
                            continue;
                        }
                        if let Ok(clone) = stream.try_clone() {
                            if let Ok(mut active) = thread_active.lock() {
                                active.push(clone);
                            }
                        }
                        let connection_running = Arc::clone(&thread_running);
                        let connection_controls = Arc::clone(&thread_controls);
                        let connection_active = Arc::clone(&thread_active);
                        let connection_store = Arc::clone(&thread_store);
                        let connection_peer = thread_peer.clone();
                        let connection_noise = NoiseConfig::new(
                            noise_config.local_static_private.clone(),
                            noise_config.psk.clone(),
                            noise_config.prologue.clone(),
                            noise_config.expected_remote_static,
                            handshake_timeout,
                        );
                        thread::spawn(move || {
                            run_connection(
                                stream,
                                connection_noise,
                                connection_store,
                                connection_peer,
                                connection_running,
                                connection_controls,
                                connection_active,
                                frame_timeout,
                            );
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            if let Ok(mut active) = thread_active.lock() {
                for stream in active.drain(..) {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
            thread_running.store(false, Ordering::SeqCst);
        });

        Ok(Self {
            local_addr,
            running,
            controls,
            active,
            join: Some(join),
            store,
            peer_id: config.peer_id,
        })
    }

    /// Bound loopback address (transient routing state; not persisted).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether the listener is currently running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Snapshot of bounded control frames received after authentication.
    pub fn received_controls(&self) -> Vec<Vec<u8>> {
        self.controls
            .lock()
            .map(|controls| controls.clone())
            .unwrap_or_default()
    }

    /// Explicitly stops the listener, closes tracked sessions and drops
    /// retained control frames.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Ok(mut active) = self.active.lock() {
            for stream in active.iter() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            active.clear();
        }
        if let Ok(mut controls) = self.controls.lock() {
            controls.clear();
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// Revokes the peer in the trust store, stops the worker and closes live
    /// sessions. Returns whether a stored trust record existed.
    pub fn remove_peer(&mut self) -> Result<bool, SecureWorkerError> {
        self.stop();
        let removed = self
            .store
            .lock()
            .map_err(|_| SecureWorkerError::LockPoisoned)?
            .remove_peer(&self.peer_id)?;
        Ok(removed)
    }
}

impl Drop for WorkerServer {
    fn drop(&mut self) {
        self.stop();
    }
}

impl fmt::Debug for WorkerServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerServer")
            .field("peer_id", &self.peer_id)
            .field("running", &self.is_running())
            .field("control_count", &self.received_controls().len())
            .finish()
    }
}

#[allow(clippy::too_many_arguments)]
fn run_connection(
    stream: TcpStream,
    config: NoiseConfig,
    store: Arc<Mutex<TrustStore>>,
    peer_id: String,
    running: Arc<AtomicBool>,
    controls: Arc<Mutex<Vec<Vec<u8>>>>,
    active: Arc<Mutex<Vec<TcpStream>>>,
    frame_timeout: Duration,
) {
    let expected_remote_static = config.expected_remote_static.unwrap_or([0u8; 32]);
    let peer_clone = stream.try_clone().ok();
    let mut session = match NoiseSession::handshake_responder(stream, &config) {
        Ok(session) => session,
        Err(_) => {
            if let Some(ref stream) = peer_clone {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            remove_active(&active, &peer_clone);
            return;
        }
    };

    {
        let trusted = store
            .lock()
            .map(|store| {
                store
                    .peer_static(&peer_id)
                    .map(|pinned| constant_time_eq_32(&pinned, &expected_remote_static))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !trusted || !running.load(Ordering::SeqCst) {
            let _ = peer_clone
                .as_ref()
                .map(|stream| stream.shutdown(std::net::Shutdown::Both));
            remove_active(&active, &peer_clone);
            return;
        }
    }

    // Keep the socket timeout bounded for each authenticated frame.
    let _ = session.stream.set_read_timeout(Some(frame_timeout));
    let _ = session.stream.set_write_timeout(Some(frame_timeout));

    while running.load(Ordering::SeqCst) {
        match session.recv_control() {
            Ok(payload) => {
                // Re-check revocation before retaining any post-auth command.
                let still_trusted = store
                    .lock()
                    .map(|store| {
                        store
                            .peer_static(&peer_id)
                            .map(|pinned| constant_time_eq_32(&pinned, &expected_remote_static))
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if !still_trusted {
                    break;
                }
                if let Ok(mut controls) = controls.lock() {
                    if controls.len() < MAX_RETAINED_CONTROLS {
                        controls.push(payload);
                    }
                }
            }
            Err(_) => break,
        }
    }

    let _ = session.stream.shutdown(std::net::Shutdown::Both);
    if let Some(ref stream) = peer_clone {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    remove_active(&active, &peer_clone);
}

fn remove_active(active: &Arc<Mutex<Vec<TcpStream>>>, target: &Option<TcpStream>) {
    let Some(target) = target else {
        return;
    };
    let Ok(target_addr) = target.peer_addr() else {
        return;
    };
    let Ok(target_local) = target.local_addr() else {
        return;
    };
    if let Ok(mut active) = active.lock() {
        active.retain(|stream| {
            !matches!(
                (stream.peer_addr(), stream.local_addr()),
                (Ok(peer), Ok(local)) if peer == target_addr && local == target_local
            )
        });
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::hex;
    #[cfg(target_os = "linux")]
    use std::net::Shutdown;

    #[test]
    fn limits_match_the_corrected_design() {
        assert_eq!(MAX_NOISE_MESSAGE_BYTES, 65_535);
        assert_eq!(MAX_TRANSPORT_PLAINTEXT_BYTES, 65_519);
        assert_eq!(MAX_SINGLE_FRAME_PAYLOAD_BYTES, 65_515);
        assert_eq!(MAX_CHUNK_PAYLOAD_BYTES, 65_503);
        assert_eq!(MAX_LOGICAL_FRAME_BYTES, 262_144);
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret32::from_bytes([0x5A; 32]);
        let rendered = format!("{secret:?}");
        assert!(rendered.contains("REDACTED"));
        assert!(!rendered.contains("5a"));
        assert!(!rendered.contains("5A"));
    }

    #[test]
    fn prologue_is_domain_separated_and_endpoint_free() {
        let session_id = crate::digest(b"session");
        let prologue = make_prologue("peer-a", "peer-b", &session_id, 7);
        let text = String::from_utf8(prologue).expect("utf8");
        assert!(text.starts_with("FASTLINK secure-worker v1\n"));
        assert!(text.contains("initiator=peer-a\n"));
        assert!(text.contains("responder=peer-b\n"));
        assert!(text.contains("epoch=7\n"));
        assert!(!text.contains("127.0.0.1"));
        assert!(!text.contains("addr="));
    }

    #[test]
    fn hex_roundtrip() {
        let key = [0xA5u8; 32];
        let encoded = hex_encode(&key);
        assert_eq!(hex_decode_32(&encoded), Some(key));
        assert_eq!(hex_decode_32("zz"), None);
    }

    #[test]
    fn random_secrets_are_fresh() {
        let first = Secret32::random().expect("first");
        let second = Secret32::random().expect("second");
        assert_ne!(first, second);
    }

    fn session_pair() -> (NoiseSession, NoiseSession) {
        use std::net::{TcpListener, TcpStream};
        use std::thread;

        let params = NOISE_PROTOCOL
            .parse::<snow::params::NoiseParams>()
            .expect("params");
        let client_keys = snow::Builder::new(params.clone())
            .generate_keypair()
            .expect("client keys");
        let server_keys = snow::Builder::new(params)
            .generate_keypair()
            .expect("server keys");
        let client_private: [u8; 32] = client_keys
            .private
            .as_slice()
            .try_into()
            .expect("client private");
        let client_public: [u8; 32] = client_keys
            .public
            .as_slice()
            .try_into()
            .expect("client public");
        let server_private: [u8; 32] = server_keys
            .private
            .as_slice()
            .try_into()
            .expect("server private");
        let server_public: [u8; 32] = server_keys
            .public
            .as_slice()
            .try_into()
            .expect("server public");
        let psk = Secret32::random().expect("psk");
        let prologue = b"FASTLINK secure-worker unit test prologue".to_vec();
        let timeout = Duration::from_secs(2);

        let client_config = NoiseConfig::new(
            Secret32::from_bytes(client_private),
            psk.clone(),
            prologue.clone(),
            Some(server_public),
            timeout,
        );
        let server_config = NoiseConfig::new(
            Secret32::from_bytes(server_private),
            psk,
            prologue,
            Some(client_public),
            timeout,
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            NoiseSession::handshake_responder(stream, &server_config)
        });
        let client = NoiseSession::handshake_initiator(
            TcpStream::connect(addr).expect("connect"),
            &client_config,
        )
        .expect("client handshake");
        let server = handle
            .join()
            .expect("server join")
            .expect("server handshake");
        (client, server)
    }

    fn chunk_body(transfer: u32, total: u32, offset: u32, payload: &[u8]) -> Vec<u8> {
        let mut body = Vec::with_capacity(CHUNK_HEADER_BYTES + payload.len());
        body.extend_from_slice(&transfer.to_le_bytes());
        body.extend_from_slice(&total.to_le_bytes());
        body.extend_from_slice(&offset.to_le_bytes());
        body.extend_from_slice(payload);
        body
    }

    #[test]
    fn chunk_reassembly_rejects_out_of_order_first_chunk() {
        let (mut client, mut server) = session_pair();
        client
            .send_inner(FRAME_CHUNK, &chunk_body(1, 100, 10, &[0u8; 10]))
            .expect("send");
        assert!(matches!(
            server.recv_logical(),
            Err(SecureWorkerError::ProtocolViolation)
        ));
    }

    #[test]
    fn chunk_reassembly_rejects_over_total_and_oversized_declarations() {
        let (mut client, mut server) = session_pair();
        client
            .send_inner(FRAME_CHUNK, &chunk_body(1, 5, 0, &[0u8; 10]))
            .expect("send");
        assert!(matches!(
            server.recv_logical(),
            Err(SecureWorkerError::ProtocolViolation)
        ));

        let (mut client, mut server) = session_pair();
        client
            .send_inner(
                FRAME_CHUNK,
                &chunk_body(1, (MAX_LOGICAL_FRAME_BYTES + 1) as u32, 0, &[0u8; 10]),
            )
            .expect("send");
        assert!(matches!(
            server.recv_logical(),
            Err(SecureWorkerError::FrameTooLarge)
        ));
    }

    #[test]
    fn chunk_reassembly_rejects_mismatched_and_overlapping_chunks() {
        let (mut client, mut server) = session_pair();
        client
            .send_inner(FRAME_CHUNK, &chunk_body(1, 200, 0, &[0x11u8; 100]))
            .expect("first");
        client
            .send_inner(FRAME_CHUNK, &chunk_body(2, 200, 100, &[0x22u8; 100]))
            .expect("second");
        assert!(matches!(
            server.recv_logical(),
            Err(SecureWorkerError::ProtocolViolation)
        ));

        let (mut client, mut server) = session_pair();
        client
            .send_inner(FRAME_CHUNK, &chunk_body(1, 200, 0, &[0x11u8; 100]))
            .expect("first");
        client
            .send_inner(FRAME_CHUNK, &chunk_body(1, 200, 50, &[0x22u8; 100]))
            .expect("overlap");
        assert!(matches!(
            server.recv_logical(),
            Err(SecureWorkerError::ProtocolViolation)
        ));
    }

    #[test]
    fn chunk_reassembly_rejects_interleaved_control_and_empty_chunks() {
        let (mut client, mut server) = session_pair();
        client
            .send_inner(FRAME_CHUNK, &chunk_body(1, 200, 0, &[0x11u8; 100]))
            .expect("first");
        client.send_control(b"interleaved").expect("control");
        assert!(matches!(
            server.recv_logical(),
            Err(SecureWorkerError::ProtocolViolation)
        ));

        let (mut client, mut server) = session_pair();
        client
            .send_inner(FRAME_CHUNK, &chunk_body(1, 200, 0, &[]))
            .expect("empty chunk");
        assert!(matches!(
            server.recv_logical(),
            Err(SecureWorkerError::FrameTooLarge)
        ));
    }

    #[test]
    fn invalid_inner_versions_types_and_flags_fail_closed() {
        let (mut client, mut server) = session_pair();
        client
            .send_inner_frame(2, FRAME_CONTROL, 0, b"bad-version")
            .expect("send");
        assert!(matches!(
            server.recv_control(),
            Err(SecureWorkerError::ProtocolViolation)
        ));

        let (mut client, mut server) = session_pair();
        client
            .send_inner_frame(FRAME_VERSION, FRAME_CONTROL, 1, b"bad-flags")
            .expect("send");
        assert!(matches!(
            server.recv_control(),
            Err(SecureWorkerError::ProtocolViolation)
        ));

        let (mut client, mut server) = session_pair();
        client
            .send_inner_frame(FRAME_VERSION, 99, 0, b"bad-type")
            .expect("send");
        assert!(matches!(
            server.recv_control(),
            Err(SecureWorkerError::ProtocolViolation)
        ));
    }

    #[test]
    fn oversized_inner_payload_and_session_limit_fail_closed() {
        let (mut client, _server) = session_pair();
        let too_large = vec![0u8; MAX_SINGLE_FRAME_PAYLOAD_BYTES + 1];
        assert!(matches!(
            client.send_inner(FRAME_CHUNK, &too_large),
            Err(SecureWorkerError::FrameTooLarge)
        ));
        client.sent_messages = MAX_SESSION_MESSAGES;
        assert!(matches!(
            client.send_control(b"limit"),
            Err(SecureWorkerError::SessionLimit)
        ));

        let (mut client, mut server) = session_pair();
        server.received_messages = MAX_SESSION_MESSAGES;
        client.send_control(b"limit").expect("send");
        assert!(matches!(
            server.recv_control(),
            Err(SecureWorkerError::SessionLimit)
        ));
    }

    type MockEntryMap = HashMap<(String, String), Zeroizing<Vec<u8>>>;

    #[derive(Default)]
    struct MockState {
        entries: MockEntryMap,
        delete_calls: usize,
        locators: Vec<KeyringLocator>,
        fail_get: Option<KeyringBackendError>,
        fail_set: Option<KeyringBackendError>,
        fail_delete: Option<KeyringBackendError>,
        corrupt_next_readback: Option<Vec<u8>>,
        corrupt_after_set: Option<Vec<u8>>,
        drop_next_set: bool,
    }

    #[derive(Default)]
    struct MockKeyringBackend {
        state: Mutex<MockState>,
    }

    impl MockKeyringBackend {
        fn count(&self) -> usize {
            self.state.lock().expect("mock lock").entries.len()
        }

        fn locators(&self) -> Vec<KeyringLocator> {
            self.state.lock().expect("mock lock").locators.clone()
        }

        fn raw_entries(&self) -> Vec<(String, String, Vec<u8>)> {
            self.state
                .lock()
                .expect("mock lock")
                .entries
                .iter()
                .map(|((s, a), v)| (s.clone(), a.clone(), v.to_vec()))
                .collect()
        }

        fn set_fail_get(&self, err: Option<KeyringBackendError>) {
            self.state.lock().expect("mock lock").fail_get = err;
        }

        fn set_fail_set(&self, err: Option<KeyringBackendError>) {
            self.state.lock().expect("mock lock").fail_set = err;
        }

        fn set_fail_delete(&self, err: Option<KeyringBackendError>) {
            self.state.lock().expect("mock lock").fail_delete = err;
        }

        fn delete_calls(&self) -> usize {
            self.state.lock().expect("mock lock").delete_calls
        }

        fn set_corrupt_readback_after_next_set(&self, data: Option<Vec<u8>>) {
            self.state.lock().expect("mock lock").corrupt_after_set = data;
        }

        fn set_drop_next_set(&self, drop_write: bool) {
            self.state.lock().expect("mock lock").drop_next_set = drop_write;
        }
    }

    impl KeyringBackend for MockKeyringBackend {
        fn get_secret(
            &self,
            locator: &KeyringLocator,
        ) -> Result<Option<Zeroizing<Vec<u8>>>, KeyringBackendError> {
            let mut state = self.state.lock().expect("mock lock");
            state.locators.push(locator.clone());
            if let Some(err) = state.fail_get.take() {
                return Err(err);
            }
            if let Some(corrupt) = state.corrupt_next_readback.take() {
                return Ok(Some(Zeroizing::new(corrupt)));
            }
            Ok(state
                .entries
                .get(&(locator.service.clone(), locator.account.clone()))
                .map(|bytes| Zeroizing::new(bytes.to_vec())))
        }

        fn set_secret(
            &self,
            locator: &KeyringLocator,
            secret_utf8: &str,
        ) -> Result<(), KeyringBackendError> {
            let mut state = self.state.lock().expect("mock lock");
            state.locators.push(locator.clone());
            if let Some(err) = state.fail_set.take() {
                return Err(err);
            }
            if state.drop_next_set {
                state.drop_next_set = false;
                return Ok(());
            }
            if let Some(corrupt) = state.corrupt_after_set.take() {
                state.corrupt_next_readback = Some(corrupt);
                return Ok(());
            }
            state.entries.insert(
                (locator.service.clone(), locator.account.clone()),
                Zeroizing::new(secret_utf8.as_bytes().to_vec()),
            );
            Ok(())
        }

        fn delete_secret(&self, locator: &KeyringLocator) -> Result<bool, KeyringBackendError> {
            let mut state = self.state.lock().expect("mock lock");
            state.locators.push(locator.clone());
            state.delete_calls += 1;
            if let Some(err) = state.fail_delete.take() {
                return Err(err);
            }
            Ok(state
                .entries
                .remove(&(locator.service.clone(), locator.account.clone()))
                .is_some())
        }
    }

    fn open_mock_store(
        root: &Path,
    ) -> Result<(TrustStore, Arc<MockKeyringBackend>), SecureWorkerError> {
        let backend = Arc::new(MockKeyringBackend::default());
        let store = open_mock_store_with(root, Arc::clone(&backend))?;
        Ok((store, backend))
    }

    fn open_mock_store_with(
        root: &Path,
        backend: Arc<MockKeyringBackend>,
    ) -> Result<TrustStore, SecureWorkerError> {
        TrustStore::open_with_keyring_backend(root, backend)
    }

    struct TempDir(tempfile::TempDir);

    impl TempDir {
        fn new(label: &str) -> Self {
            let prefix = format!("fastlink_secure_worker_{label}_");
            let dir = tempfile::Builder::new()
                .prefix(&prefix)
                .tempdir()
                .expect("secure temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    struct Paired {
        _server_dir: TempDir,
        _client_dir: TempDir,
        server_backend: Arc<MockKeyringBackend>,
        _client_backend: Arc<MockKeyringBackend>,
        server_store: TrustStore,
        client_store: TrustStore,
        session: PairingSession,
        psk: Secret32,
        #[cfg(target_os = "linux")]
        prologue: Vec<u8>,
    }

    fn paired(peer_id: &str) -> Paired {
        let server_dir = TempDir::new("server");
        let client_dir = TempDir::new("client");
        let (mut server_store, server_backend) =
            open_mock_store(server_dir.path()).expect("server store");
        let (client_store, client_backend) =
            open_mock_store(client_dir.path()).expect("client store");
        let offer = server_store
            .begin_pairing(peer_id, NetworkTrust::Lan, true)
            .expect("pairing offer");
        let client_static = client_store.local_static_public();
        let proof = offer.proof_for(client_static);
        let session = server_store
            .accept_pairing(
                &offer,
                &proof,
                PairingConsent { approved: true },
                client_static,
            )
            .expect("pairing accepted");
        let psk = Secret32::from_session_key(&session.session_key);
        #[cfg(target_os = "linux")]
        let prologue = make_prologue(peer_id, "server", &session.session_id, session.epoch);
        Paired {
            _server_dir: server_dir,
            _client_dir: client_dir,
            server_backend,
            _client_backend: client_backend,
            server_store,
            client_store,
            session,
            psk,
            #[cfg(target_os = "linux")]
            prologue,
        }
    }

    #[cfg(target_os = "linux")]
    fn server_noise(config: &Paired) -> NoiseConfig {
        NoiseConfig::new(
            config.server_store.local_static_private(),
            config.psk.clone(),
            config.prologue.clone(),
            Some(config.client_store.local_static_public()),
            Duration::from_secs(2),
        )
    }

    #[cfg(target_os = "linux")]
    fn client_noise(config: &Paired) -> NoiseConfig {
        NoiseConfig::new(
            config.client_store.local_static_private(),
            config.psk.clone(),
            config.prologue.clone(),
            Some(config.server_store.local_static_public()),
            Duration::from_secs(2),
        )
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    fn connect_and_handshake(
        addr: SocketAddr,
        config: &NoiseConfig,
    ) -> Result<NoiseSession, SecureWorkerError> {
        let stream = TcpStream::connect(addr).expect("connect");
        NoiseSession::handshake_initiator(stream, config)
    }

    #[cfg(target_os = "linux")]
    fn expect_handshake_failure(config: &Paired, mutate: impl FnOnce(&mut NoiseConfig)) {
        let server = server_noise(config);
        let (addr, server_handle) = spawn_server_handshake(server);
        let mut client = client_noise(config);
        mutate(&mut client);
        assert!(connect_and_handshake(addr, &client).is_err());
        let _ = server_handle.join().expect("server join");
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    struct ProxyState {
        mode: Mutex<ProxyMode>,
        captured: Mutex<Option<Vec<u8>>>,
    }

    #[cfg(target_os = "linux")]
    struct Proxy {
        addr: SocketAddr,
        state: Arc<ProxyState>,
    }

    #[cfg(target_os = "linux")]
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
                            *thread_state.captured.lock().expect("capture lock") =
                                Some(frame.clone());
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
                            let replay =
                                thread_state.captured.lock().expect("capture lock").clone();
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    struct DelayProxy;

    #[cfg(target_os = "linux")]
    impl DelayProxy {
        fn start(server_addr: SocketAddr, delay: Duration) -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").expect("delay proxy bind");
            let addr = listener.local_addr().expect("delay proxy addr");
            thread::spawn(move || {
                let (mut client, _) = listener.accept().expect("delay proxy accept");
                let mut server = TcpStream::connect(server_addr).expect("delay proxy upstream");
                let mut server_read = server.try_clone().expect("delay server clone");
                let mut client_write = client.try_clone().expect("delay client clone");
                let relay = thread::spawn(move || loop {
                    let mut prefix = [0u8; 4];
                    if server_read.read_exact(&mut prefix).is_err() {
                        break;
                    }
                    let len = u32::from_le_bytes(prefix) as usize;
                    if len > 70_000 {
                        break;
                    }
                    let mut body = vec![0u8; len];
                    if server_read.read_exact(&mut body).is_err() {
                        break;
                    }
                    thread::sleep(delay);
                    if client_write.write_all(&prefix).is_err()
                        || client_write.write_all(&body).is_err()
                    {
                        break;
                    }
                });
                let _ = io::copy(&mut client, &mut server);
                let _ = server.shutdown(Shutdown::Both);
                let _ = client.shutdown(Shutdown::Both);
                let _ = relay.join();
            });
            addr
        }
    }

    #[cfg(target_os = "linux")]
    struct DripProxy;

    #[cfg(target_os = "linux")]
    impl DripProxy {
        fn start(
            server_addr: SocketAddr,
            first_frame_delay: Duration,
            chunk_bytes: usize,
            chunk_interval: Duration,
        ) -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").expect("drip proxy bind");
            let addr = listener.local_addr().expect("drip proxy addr");
            thread::spawn(move || {
                let (mut client, _) = listener.accept().expect("drip proxy accept");
                let mut server = TcpStream::connect(server_addr).expect("drip proxy upstream");
                let mut server_read = server.try_clone().expect("drip server clone");
                let mut client_write = client.try_clone().expect("drip client clone");
                let relay = thread::spawn(move || {
                    let mut frame_index = 0usize;
                    loop {
                        let mut prefix = [0u8; 4];
                        if server_read.read_exact(&mut prefix).is_err() {
                            break;
                        }
                        let len = u32::from_le_bytes(prefix) as usize;
                        if len > 70_000 {
                            break;
                        }
                        let mut body = vec![0u8; len];
                        if server_read.read_exact(&mut body).is_err() {
                            break;
                        }
                        let mut frame = prefix.to_vec();
                        frame.extend_from_slice(&body);
                        if frame_index == 0 {
                            thread::sleep(first_frame_delay);
                            if client_write.write_all(&frame).is_err() {
                                break;
                            }
                        } else if frame_index == 1 {
                            // Drip exactly one handshake frame in small chunks. Each
                            // chunk arrives far sooner than the old per-socket read
                            // timeout, but the frame completes beyond the absolute
                            // handshake deadline once the first frame delay is added.
                            for chunk in frame.chunks(chunk_bytes.max(1)) {
                                thread::sleep(chunk_interval);
                                if client_write.write_all(chunk).is_err() {
                                    return;
                                }
                            }
                        } else if client_write.write_all(&frame).is_err() {
                            break;
                        }
                        frame_index += 1;
                    }
                });
                let _ = io::copy(&mut client, &mut server);
                let _ = server.shutdown(Shutdown::Both);
                let _ = client.shutdown(Shutdown::Both);
                let _ = relay.join();
            });
            addr
        }
    }

    #[cfg(target_os = "linux")]
    struct WorkerFixture {
        worker: WorkerServer,
        store: Arc<Mutex<TrustStore>>,
        client_config: NoiseConfig,
        psk: Secret32,
        prologue: Vec<u8>,
        expected_remote_static: [u8; 32],
        _server_dir: TempDir,
        _client_dir: TempDir,
        _server_backend: Arc<MockKeyringBackend>,
        _client_backend: Arc<MockKeyringBackend>,
    }

    #[cfg(target_os = "linux")]
    impl WorkerFixture {
        fn start(config: Paired, handshake_timeout: Duration, frame_timeout: Duration) -> Self {
            let client_config = client_noise(&config);
            let expected_remote_static = config.client_store.local_static_public();
            let Paired {
                _server_dir,
                _client_dir,
                server_backend,
                _client_backend,
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
                _server_backend: server_backend,
                _client_backend,
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

    #[cfg(target_os = "linux")]
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

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn pairing_requires_consent_rejects_replay_and_bad_policy() {
        let dir = TempDir::new("pairing");
        let client_dir = TempDir::new("pairing-client");
        let (mut store, _backend) = open_mock_store(dir.path()).expect("store");
        let (client_store, _client_backend) =
            open_mock_store(client_dir.path()).expect("client store");

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
        let declined_proof = declined_offer.proof_for(client_store.local_static_public());
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
        let proof = offer.proof_for(client_store.local_static_public());
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
        let mut wrong_peer = offer.proof_for(client_store.local_static_public());
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
        let mut bad_mac = offer.proof_for(client_store.local_static_public());
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
    fn pairing_pin_must_be_bound_to_the_challenge_proof() {
        let dir = TempDir::new("pin-binding");
        let (mut store, _backend) = open_mock_store(dir.path()).expect("store");
        let key_a = [0xAAu8; 32];
        let key_b = [0xBBu8; 32];

        let offer = store
            .begin_pairing("peer-a", NetworkTrust::Lan, true)
            .expect("offer");
        let proof_a = offer.proof_for(key_a);
        let result =
            store.accept_pairing(&offer, &proof_a, PairingConsent { approved: true }, key_b);
        assert!(
            result.is_err(),
            "a proof that does not bind key B must not allow key B to be stored"
        );
        assert!(!store.is_trusted("peer-a"));
        assert_eq!(store.peer_static("peer-a"), None);

        // The matching key and proof commit successfully.
        let offer = store
            .begin_pairing("peer-a", NetworkTrust::Lan, true)
            .expect("fresh offer");
        let proof_a = offer.proof_for(key_a);
        let session = store
            .accept_pairing(&offer, &proof_a, PairingConsent { approved: true }, key_a)
            .expect("matching key proof");
        assert_eq!(session.peer_id, "peer-a");
        assert_eq!(store.peer_static("peer-a"), Some(key_a));

        // A tampered key with an otherwise valid MAC fails closed.
        let offer = store
            .begin_pairing("peer-b", NetworkTrust::Lan, true)
            .expect("tampered-key offer");
        let mut tampered_key = offer.proof_for(key_a);
        tampered_key.static_public = key_b;
        let rejected = store.accept_pairing(
            &offer,
            &tampered_key,
            PairingConsent { approved: true },
            key_b,
        );
        assert!(matches!(
            rejected,
            Err(SecureWorkerError::Pairing(PairingError::InvalidProof))
        ));
        assert!(!store.is_trusted("peer-b"));

        // A tampered MAC fails closed even with the expected key.
        let offer = store
            .begin_pairing("peer-c", NetworkTrust::Lan, true)
            .expect("tampered-mac offer");
        let mut tampered_mac = offer.proof_for(key_a);
        tampered_mac.mac[0] ^= 1;
        let rejected = store.accept_pairing(
            &offer,
            &tampered_mac,
            PairingConsent { approved: true },
            key_a,
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
        let (mut store, _backend) = open_mock_store(dir.path()).expect("store");
        let (client_store, _client_backend) =
            open_mock_store(client_dir.path()).expect("client store");

        let offer_a = store
            .begin_pairing("peer-a", NetworkTrust::Lan, true)
            .expect("offer a");
        let proof_a = offer_a.proof_for(client_store.local_static_public());
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
        let proof_b = offer_b.proof_for(client_store.local_static_public());

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
        assert!(!store_dir.join("identity.key").exists());
        #[cfg(unix)]
        {
            assert_eq!(mode_of(store_dir), 0o700);
            assert_eq!(mode_of(&store_dir.join("state.json")), 0o600);
            assert_eq!(mode_of(&store_dir.join("peers")), 0o700);
        }

        let meta_path = metadata_path(store_dir);
        let secret_path = meta_path
            .to_string_lossy()
            .trim_end_matches(".meta.json")
            .to_string();
        let secret_path = PathBuf::from(format!("{secret_path}.secret"));
        assert!(!secret_path.exists());
        #[cfg(unix)]
        assert_eq!(mode_of(&meta_path), 0o600);
        assert_eq!(config.server_backend.count(), 2);

        let meta_text = fs::read_to_string(&meta_path).expect("metadata text");
        let session_key_hex = config.session.session_key.fingerprint_hex();
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
        assert_eq!(config.server_backend.count(), 1);
        assert!(!store.remove_peer("peer-a").expect("second removal"));
        assert!(fs::read_dir(store_dir.join("peers"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false));
    }

    #[test]
    fn corrupt_records_fail_closed_and_stale_temporary_files_are_cleaned() {
        let dir = TempDir::new("corrupt");
        let client_dir = TempDir::new("corrupt-client");
        let (client_store, _client_backend) =
            open_mock_store(client_dir.path()).expect("client store");
        let (mut store, backend) = open_mock_store(dir.path()).expect("store");
        let offer = store
            .begin_pairing("peer-a", NetworkTrust::Lan, true)
            .expect("offer");
        let proof = offer.proof_for(client_store.local_static_public());
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
        #[cfg(unix)]
        fs::set_permissions(&meta_path, fs::Permissions::from_mode(0o600)).expect("mode");
        let stale_tmp = dir.path().join("peers").join(".leftover.tmp-1-1");
        fs::write(&stale_tmp, b"partial").expect("stale temp");
        drop(store);

        let mut reopened = open_mock_store_with(dir.path(), Arc::clone(&backend)).expect("reopen");
        assert!(!reopened.is_trusted("peer-a"));
        assert_eq!(reopened.peer_static("peer-a"), None);
        assert!(!stale_tmp.exists());
        assert!(reopened.remove_peer("peer-a").expect("remove corrupt"));
        assert_eq!(backend.count(), 1);
        assert!(fs::read_dir(dir.path().join("peers"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false));

        let bad_identity_dir = TempDir::new("bad-identity");
        fs::write(bad_identity_dir.path().join("identity.key"), b"short").expect("bad identity");
        #[cfg(unix)]
        fs::set_permissions(
            bad_identity_dir.path().join("identity.key"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("mode");
        assert!(matches!(
            open_mock_store(bad_identity_dir.path()),
            Err(SecureWorkerError::StoreCorrupt)
        ));
    }

    #[test]
    fn trust_store_refuses_to_chmod_an_unrelated_directory() {
        let dir = TempDir::new("unrelated");
        fs::write(dir.path().join("notes.txt"), b"user data").expect("notes");
        #[cfg(unix)]
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("mode");
        assert!(matches!(
            open_mock_store(dir.path()),
            Err(SecureWorkerError::StorePermission)
        ));
        #[cfg(unix)]
        assert_eq!(mode_of(dir.path()), 0o755);
    }

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn loopback_control_roundtrip_and_exact_frame_limits() {
        let config = paired("peer-a");
        let (addr, server_handle) = spawn_server_handshake(server_noise(&config));
        let mut client =
            connect_and_handshake(addr, &client_noise(&config)).expect("client handshake");
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn handshake_uses_one_absolute_deadline_across_all_messages() {
        let config = paired("peer-a");
        let mut server_config = server_noise(&config);
        server_config.timeout = Duration::from_secs(2);
        let (server_addr, server_handle) = spawn_server_handshake(server_config);
        let proxy_addr = DelayProxy::start(server_addr, Duration::from_millis(350));
        let mut client_config = client_noise(&config);
        client_config.timeout = Duration::from_millis(500);

        let started = Instant::now();
        let result = connect_and_handshake(proxy_addr, &client_config);
        let elapsed = started.elapsed();
        assert!(
            result.is_err(),
            "slow-drip handshake must exhaust one absolute deadline, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1_500),
            "handshake exceeded the absolute deadline: {elapsed:?}"
        );
        let _ = server_handle.join();

        // A normal loopback handshake with the same short deadline still succeeds.
        let server_config = server_noise(&config);
        let (server_addr, server_handle) = spawn_server_handshake(server_config);
        let client = connect_and_handshake(server_addr, &client_config);
        assert!(client.is_ok(), "normal handshake failed: {client:?}");
        let _ = server_handle.join();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn partial_byte_handshake_frame_cannot_extend_absolute_deadline() {
        let config = paired("peer-a");
        let mut server_config = server_noise(&config);
        server_config.timeout = Duration::from_secs(3);
        let (server_addr, server_handle) = spawn_server_handshake(server_config);
        let proxy_addr = DripProxy::start(
            server_addr,
            Duration::from_millis(500),
            3,
            Duration::from_millis(60),
        );
        let mut client_config = client_noise(&config);
        client_config.timeout = Duration::from_millis(1_200);

        let started = Instant::now();
        let result = connect_and_handshake(proxy_addr, &client_config);
        let elapsed = started.elapsed();
        assert!(
            result.is_err(),
            "a partial handshake frame must not extend the handshake past the absolute deadline; got {result:?} after {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(2_500),
            "handshake did not fail closed within a bounded ceiling: {elapsed:?}"
        );
        let _ = server_handle.join();
    }

    #[cfg(target_os = "linux")]
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
        let mut truncated =
            TcpStream::connect(fixture.worker.local_addr()).expect("connect truncated");
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
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
        let key_hex = config.session.session_key.fingerprint_hex();
        let psk_debug = format!("{:?}", config.psk);
        assert!(psk_debug.contains("REDACTED"));
        let store_debug = format!("{:?}", config.server_store);
        assert!(!store_debug.contains("peer-a"));
        assert!(!store_debug.contains(&key_hex));
        let client_debug = format!("{:?}", config.client_store);
        assert!(!client_debug.contains("peer-a"));
        assert!(!client_debug.contains(&key_hex));
        let session_debug = format!("{:?}", config.session);
        assert!(session_debug.contains("REDACTED"));
        assert!(!session_debug.contains(&key_hex));

        let debug_dir = TempDir::new("debug-store");
        let (mut store, _backend) = open_mock_store(debug_dir.path()).expect("store");
        let offer = store
            .begin_pairing("peer-a", NetworkTrust::Lan, true)
            .expect("offer");
        let offer_debug = format!("{offer:?}");
        assert!(offer_debug.contains("REDACTED"));
        assert!(!offer_debug.contains("secret="));
        assert!(!offer_debug.contains("127.0.0.1"));
        let proof = offer.proof_for([0x42u8; 32]);
        let proof_debug = format!("{proof:?}");
        assert!(proof_debug.contains("REDACTED"));
        assert!(!proof_debug.contains(&hex(&proof.mac)));

        #[cfg(target_os = "linux")]
        {
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
        }

        for error in [
            SecureWorkerError::StaticKeyMismatch,
            SecureWorkerError::HandshakeFailed,
            SecureWorkerError::UntrustedPeer,
            SecureWorkerError::KeyringUnavailable,
            SecureWorkerError::KeyringConflict,
            SecureWorkerError::Unsupported,
        ] {
            let rendered = format!("{error} {error:?}");
            assert!(!rendered.contains("127.0.0.1"));
            assert!(!rendered.contains(&key_hex));
        }
    }

    fn test_hex_decode_32(bytes: &[u8]) -> [u8; 32] {
        let text = std::str::from_utf8(bytes).expect("UTF-8 hex");
        assert_eq!(text.len(), 64);
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("hex byte");
        }
        out
    }

    fn find_raw_entry(backend: &MockKeyringBackend, prefix: &str) -> [u8; 32] {
        backend
            .raw_entries()
            .into_iter()
            .find(|(_, account, _)| account.starts_with(prefix))
            .map(|(_, _, value)| test_hex_decode_32(&value))
            .expect("expected keyring entry")
    }

    struct LegacyFixture {
        id_private: Secret32,
        id_public: [u8; 32],
        id_file_bytes: Vec<u8>,
        peer_id: String,
        peer_secret: Secret32,
        peer_secret_raw: Vec<u8>,
        peer_public: [u8; 32],
        meta_path: PathBuf,
        secret_path: PathBuf,
        meta_bytes: Vec<u8>,
    }

    fn seed_legacy_store_dir(
        target_dir: &Path,
        peer_id: &str,
        peer_public: [u8; 32],
        trust_epoch: u64,
    ) -> LegacyFixture {
        let seed_dir = TempDir::new("legacy-seed");
        let (mut seed_store, seed_backend) = open_mock_store(seed_dir.path()).expect("seed store");
        let id_private = seed_store.local_static_private();
        let id_public = seed_store.local_static_public();
        let id_priv_raw = find_raw_entry(&seed_backend, "id-");

        let offer = seed_store
            .begin_pairing(peer_id, NetworkTrust::Lan, true)
            .expect("seed offer");
        let proof = offer.proof_for(peer_public);
        seed_store
            .accept_pairing(
                &offer,
                &proof,
                PairingConsent { approved: true },
                peer_public,
            )
            .expect("seed accept");
        let peer_secret = seed_store
            .peer_pairing_secret(peer_id)
            .expect("seed peer secret");
        let peer_secret_raw = find_raw_entry(&seed_backend, "peer-").to_vec();

        let seed_meta_path = metadata_path(seed_dir.path());
        let peer_file_name = seed_meta_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("peer meta name")
            .to_string();
        let peer_stem = peer_file_name
            .strip_suffix(".meta.json")
            .expect("peer stem")
            .to_string();
        let meta_bytes = fs::read(&seed_meta_path).expect("read seed meta");

        let peers_dir = target_dir.join("peers");
        fs::create_dir_all(&peers_dir).expect("create legacy peers dir");
        let mut id_file_bytes = Vec::with_capacity(64);
        id_file_bytes.extend_from_slice(&id_priv_raw);
        id_file_bytes.extend_from_slice(&id_public);
        let id_path = target_dir.join("identity.key");
        fs::write(&id_path, &id_file_bytes).expect("write legacy identity.key");

        let state_path = target_dir.join("state.json");
        fs::write(&state_path, format!("{{\"trust_epoch\":{trust_epoch}}}"))
            .expect("write legacy state.json");

        let meta_path = peers_dir.join(&peer_file_name);
        fs::write(&meta_path, &meta_bytes).expect("write legacy meta.json");
        let secret_path = peers_dir.join(format!("{peer_stem}.secret"));
        fs::write(&secret_path, &peer_secret_raw).expect("write legacy .secret");

        #[cfg(unix)]
        {
            fs::set_permissions(target_dir, fs::Permissions::from_mode(0o700)).expect("root mode");
            fs::set_permissions(&peers_dir, fs::Permissions::from_mode(0o700)).expect("peers mode");
            fs::set_permissions(&id_path, fs::Permissions::from_mode(0o600)).expect("id mode");
            fs::set_permissions(&state_path, fs::Permissions::from_mode(0o600))
                .expect("state mode");
            fs::set_permissions(&meta_path, fs::Permissions::from_mode(0o600)).expect("meta mode");
            fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600))
                .expect("secret mode");
        }

        LegacyFixture {
            id_private,
            id_public,
            id_file_bytes,
            peer_id: peer_id.to_string(),
            peer_secret,
            peer_secret_raw,
            peer_public,
            meta_path,
            secret_path,
            meta_bytes,
        }
    }

    #[test]
    fn keyring_backed_create_read_reopen_delete_and_redaction() {
        let dir = TempDir::new("keyring-crud");
        let backend = Arc::new(MockKeyringBackend::default());
        let mut store = open_mock_store_with(dir.path(), Arc::clone(&backend))
            .expect("open with injected keyring");

        assert!(!dir.path().join("identity.key").exists());
        assert_eq!(backend.count(), 1);
        let id_priv = store.local_static_private();
        let id_pub = store.local_static_public();
        let id_priv_hex = hex(&find_raw_entry(&backend, "id-"));
        let state_text = fs::read_to_string(dir.path().join("state.json")).expect("state.json");
        assert!(state_text.contains(&hex(&id_pub)));
        assert!(!state_text.contains(&id_priv_hex));

        let peer_id = "opaque-peer-secret-test";
        let offer = store
            .begin_pairing(peer_id, NetworkTrust::Lan, true)
            .expect("offer");
        let peer_static = [0x42u8; 32];
        let proof = offer.proof_for(peer_static);
        let _session = store
            .accept_pairing(
                &offer,
                &proof,
                PairingConsent { approved: true },
                peer_static,
            )
            .expect("accept");
        assert_eq!(backend.count(), 2);
        assert!(store.is_trusted(peer_id));
        let expected_peer_secret = store.peer_pairing_secret(peer_id).expect("peer secret");
        let meta_path = metadata_path(dir.path());
        let secret_path = PathBuf::from(format!(
            "{}.secret",
            meta_path.to_string_lossy().trim_end_matches(".meta.json")
        ));
        assert!(!secret_path.exists());
        let peer_secret_hex = hex(&find_raw_entry(&backend, "peer-"));
        let meta_text = fs::read_to_string(&meta_path).expect("meta.json");
        assert!(!meta_text.contains(&peer_secret_hex));

        let root_str = dir.path().to_string_lossy().to_string();
        for locator in backend.locators() {
            assert!(!locator.service.contains(peer_id));
            assert!(!locator.account.contains(peer_id));
            assert!(!locator.label.contains(peer_id));
            assert!(!locator.service.contains(&root_str));
            assert!(!locator.account.contains(&root_str));
            assert!(!locator.label.contains(&root_str));
        }
        for (_, _, value) in backend.raw_entries() {
            let text = std::str::from_utf8(&value).expect("UTF-8 hex secret for KDE compatibility");
            assert_eq!(text.len(), 64);
            assert!(text.bytes().all(|b| b.is_ascii_hexdigit()));
        }

        drop(store);
        let mut reopened = open_mock_store_with(dir.path(), Arc::clone(&backend)).expect("reopen");
        assert_eq!(reopened.local_static_private(), id_priv);
        assert_eq!(reopened.local_static_public(), id_pub);
        assert!(reopened.is_trusted(peer_id));
        assert_eq!(reopened.peer_static(peer_id), Some(peer_static));
        assert_eq!(reopened.peer_trust(peer_id), Some(NetworkTrust::Lan));
        assert_eq!(
            reopened.peer_pairing_secret(peer_id),
            Some(expected_peer_secret)
        );

        assert!(reopened.remove_peer(peer_id).expect("remove peer"));
        assert!(!reopened.is_trusted(peer_id));
        assert!(!meta_path.exists());
        assert_eq!(backend.count(), 1);
        assert!(!reopened.remove_peer(peer_id).expect("idempotent removal"));
    }

    #[test]
    fn first_open_migrates_legacy_identity_and_peer_secret_files() {
        let dir = TempDir::new("legacy-migrate");
        let fixture = seed_legacy_store_dir(dir.path(), "legacy-peer", [0x44u8; 32], 9);

        let backend = Arc::new(MockKeyringBackend::default());
        let store =
            open_mock_store_with(dir.path(), Arc::clone(&backend)).expect("migrate legacy store");

        assert!(!dir.path().join("identity.key").exists());
        assert!(!fixture.secret_path.exists());
        assert!(fixture.meta_path.exists());
        assert_eq!(backend.count(), 2);

        assert_eq!(store.local_static_private(), fixture.id_private);
        assert_eq!(store.local_static_public(), fixture.id_public);
        assert!(store.is_trusted(&fixture.peer_id));
        assert_eq!(
            store.peer_static(&fixture.peer_id),
            Some(fixture.peer_public)
        );
        assert_eq!(
            store.peer_pairing_secret(&fixture.peer_id),
            Some(fixture.peer_secret.clone())
        );

        let state_text = fs::read_to_string(dir.path().join("state.json")).expect("state text");
        assert!(state_text.contains("\"trust_epoch\":9"));
        assert!(state_text.contains(&hex(&fixture.id_public)));
        drop(store);

        let reopened =
            open_mock_store_with(dir.path(), Arc::clone(&backend)).expect("reopen migrated");
        assert_eq!(reopened.local_static_private(), fixture.id_private);
        assert_eq!(reopened.local_static_public(), fixture.id_public);
        assert!(reopened.is_trusted(&fixture.peer_id));
        assert_eq!(
            reopened.peer_pairing_secret(&fixture.peer_id),
            Some(fixture.peer_secret)
        );
    }

    #[test]
    fn keyring_failures_and_readback_errors_fail_closed_and_preserve_legacy_files() {
        let dir = TempDir::new("legacy-failures");
        let fixture = seed_legacy_store_dir(dir.path(), "legacy-peer", [0x55u8; 32], 5);
        let backend = Arc::new(MockKeyringBackend::default());

        backend.set_fail_get(Some(KeyringBackendError::AccessDenied));
        assert!(matches!(
            open_mock_store_with(dir.path(), Arc::clone(&backend)),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert_eq!(
            fs::read(dir.path().join("identity.key")).expect("identity preserved"),
            fixture.id_file_bytes
        );
        assert_eq!(
            fs::read(&fixture.secret_path).expect("secret preserved"),
            fixture.peer_secret_raw
        );
        assert_eq!(backend.count(), 0);

        backend.set_fail_set(Some(KeyringBackendError::Unavailable));
        assert!(matches!(
            open_mock_store_with(dir.path(), Arc::clone(&backend)),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert_eq!(
            fs::read(dir.path().join("identity.key")).expect("identity preserved"),
            fixture.id_file_bytes
        );
        assert_eq!(backend.count(), 0);

        backend.set_drop_next_set(true);
        assert!(matches!(
            open_mock_store_with(dir.path(), Arc::clone(&backend)),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert_eq!(
            fs::read(dir.path().join("identity.key")).expect("identity preserved"),
            fixture.id_file_bytes
        );
        assert_eq!(backend.count(), 0);

        backend.set_corrupt_readback_after_next_set(Some(b"not-valid-hex".to_vec()));
        assert!(matches!(
            open_mock_store_with(dir.path(), Arc::clone(&backend)),
            Err(SecureWorkerError::StoreCorrupt)
        ));
        assert_eq!(
            fs::read(dir.path().join("identity.key")).expect("identity preserved"),
            fixture.id_file_bytes
        );

        backend.set_corrupt_readback_after_next_set(Some(vec![b'e'; 64]));
        assert!(matches!(
            open_mock_store_with(dir.path(), Arc::clone(&backend)),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert_eq!(
            fs::read(dir.path().join("identity.key")).expect("identity preserved"),
            fixture.id_file_bytes
        );

        // Pre-initialize identity cleanly, then add legacy peer files and test peer migration failures.
        let pre_dir = TempDir::new("pre-migrated-id");
        let peer_fail_seed = TempDir::new("peer-fail-seed");
        let peer_fixture =
            seed_legacy_store_dir(peer_fail_seed.path(), "peer-fail", [0x66u8; 32], 3);
        let peer_backend = Arc::new(MockKeyringBackend::default());
        let pre_store =
            open_mock_store_with(pre_dir.path(), Arc::clone(&peer_backend)).expect("init id");
        drop(pre_store);

        let meta_name = peer_fixture.meta_path.file_name().expect("meta file name");
        let secret_name = peer_fixture
            .secret_path
            .file_name()
            .expect("secret file name");
        fs::write(
            pre_dir.path().join("peers").join(meta_name),
            &peer_fixture.meta_bytes,
        )
        .expect("write meta");
        let target_secret_path = pre_dir.path().join("peers").join(secret_name);
        fs::write(&target_secret_path, &peer_fixture.peer_secret_raw).expect("write secret");
        #[cfg(unix)]
        fs::set_permissions(&target_secret_path, fs::Permissions::from_mode(0o600)).expect("mode");

        peer_backend.set_fail_set(Some(KeyringBackendError::Unavailable));
        assert!(matches!(
            open_mock_store_with(pre_dir.path(), Arc::clone(&peer_backend)),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert_eq!(
            fs::read(&target_secret_path).expect("peer secret preserved"),
            peer_fixture.peer_secret_raw
        );
        assert_eq!(peer_backend.count(), 1);

        peer_backend.set_drop_next_set(true);
        assert!(matches!(
            open_mock_store_with(pre_dir.path(), Arc::clone(&peer_backend)),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert_eq!(
            fs::read(&target_secret_path).expect("peer secret preserved"),
            peer_fixture.peer_secret_raw
        );
        assert_eq!(peer_backend.count(), 1);

        peer_backend.set_corrupt_readback_after_next_set(Some(b"not-valid-hex".to_vec()));
        assert!(matches!(
            open_mock_store_with(pre_dir.path(), Arc::clone(&peer_backend)),
            Err(SecureWorkerError::StoreCorrupt)
        ));
        assert_eq!(
            fs::read(&target_secret_path).expect("peer secret preserved"),
            peer_fixture.peer_secret_raw
        );
        assert_eq!(peer_backend.count(), 1);

        peer_backend.set_corrupt_readback_after_next_set(Some(vec![b'f'; 64]));
        assert!(matches!(
            open_mock_store_with(pre_dir.path(), Arc::clone(&peer_backend)),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert_eq!(
            fs::read(&target_secret_path).expect("peer secret preserved"),
            peer_fixture.peer_secret_raw
        );
        assert_eq!(peer_backend.count(), 1);

        let mut store = open_mock_store_with(pre_dir.path(), Arc::clone(&peer_backend))
            .expect("recover migrate");
        assert!(!target_secret_path.exists());
        assert!(store.is_trusted(&peer_fixture.peer_id));

        let offer = store
            .begin_pairing("peer-write-fail", NetworkTrust::Lan, true)
            .expect("offer");
        let proof = offer.proof_for([0x77u8; 32]);
        peer_backend.set_fail_set(Some(KeyringBackendError::AccessDenied));
        assert!(matches!(
            store.accept_pairing(
                &offer,
                &proof,
                PairingConsent { approved: true },
                [0x77u8; 32]
            ),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert!(!store.is_trusted("peer-write-fail"));
        assert_eq!(store.peer_pairing_secret("peer-write-fail"), None);

        let offer2 = store
            .begin_pairing("peer-drop-fail", NetworkTrust::Lan, true)
            .expect("offer2");
        let proof2 = offer2.proof_for([0x88u8; 32]);
        peer_backend.set_drop_next_set(true);
        assert!(matches!(
            store.accept_pairing(
                &offer2,
                &proof2,
                PairingConsent { approved: true },
                [0x88u8; 32]
            ),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert!(!store.is_trusted("peer-drop-fail"));

        // Existing identity_public in state.json with missing keyring item fails closed.
        drop(store);
        let empty_backend = Arc::new(MockKeyringBackend::default());
        assert!(matches!(
            open_mock_store_with(pre_dir.path(), empty_backend),
            Err(SecureWorkerError::StoreCorrupt)
        ));
    }

    #[test]
    fn legacy_migration_conflicts_refuse_overwrite_and_fail_closed() {
        // 1. Conflicting identity.key vs existing keyring identity entry.
        let id_dir = TempDir::new("conflict-id");
        let id_backend = Arc::new(MockKeyringBackend::default());
        let orig_store =
            open_mock_store_with(id_dir.path(), Arc::clone(&id_backend)).expect("init");
        drop(orig_store);
        fs::remove_file(id_dir.path().join("state.json")).expect("remove state");
        let conflict_seed_dir = TempDir::new("conflict-id-seed");
        let conflicting_id_bytes =
            seed_legacy_store_dir(conflict_seed_dir.path(), "other", [0x29u8; 32], 1).id_file_bytes;
        fs::write(id_dir.path().join("identity.key"), &conflicting_id_bytes)
            .expect("write conflict id");
        #[cfg(unix)]
        fs::set_permissions(
            id_dir.path().join("identity.key"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("mode");
        let id_snapshot = id_backend.raw_entries();
        assert!(matches!(
            open_mock_store_with(id_dir.path(), Arc::clone(&id_backend)),
            Err(SecureWorkerError::KeyringConflict)
        ));
        assert_eq!(id_backend.raw_entries(), id_snapshot);
        assert_eq!(
            fs::read(id_dir.path().join("identity.key")).expect("legacy conflict preserved"),
            conflicting_id_bytes
        );

        // 2. Conflicting peer .secret vs existing keyring peer entry + matching crash recovery.
        let peer_dir = TempDir::new("conflict-peer");
        let peer_backend = Arc::new(MockKeyringBackend::default());
        let mut store =
            open_mock_store_with(peer_dir.path(), Arc::clone(&peer_backend)).expect("init");
        let offer = store
            .begin_pairing("conflict-peer", NetworkTrust::Lan, true)
            .expect("offer");
        let proof = offer.proof_for([0x99u8; 32]);
        store
            .accept_pairing(
                &offer,
                &proof,
                PairingConsent { approved: true },
                [0x99u8; 32],
            )
            .expect("accept");
        let meta_path = metadata_path(peer_dir.path());
        let secret_path = PathBuf::from(format!(
            "{}.secret",
            meta_path.to_string_lossy().trim_end_matches(".meta.json")
        ));
        let expected_secret = store.peer_pairing_secret("conflict-peer").expect("secret");
        drop(store);

        let conflicting_secret = vec![0x5Au8; 32];
        fs::write(&secret_path, &conflicting_secret).expect("write conflict secret");
        #[cfg(unix)]
        fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600)).expect("mode");
        let peer_snapshot = peer_backend.raw_entries();
        assert!(matches!(
            open_mock_store_with(peer_dir.path(), Arc::clone(&peer_backend)),
            Err(SecureWorkerError::KeyringConflict)
        ));
        assert_eq!(peer_backend.raw_entries(), peer_snapshot);
        assert_eq!(
            fs::read(&secret_path).expect("preserved"),
            conflicting_secret
        );

        // Interrupted-delete recovery: matching secret reconciles cleanly and deletes legacy file.
        let matching_raw = find_raw_entry(&peer_backend, "peer-");
        fs::write(&secret_path, matching_raw).expect("write matching secret");
        #[cfg(unix)]
        fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600)).expect("mode");
        let recovered =
            open_mock_store_with(peer_dir.path(), Arc::clone(&peer_backend)).expect("recover");
        assert!(!secret_path.exists());
        assert!(recovered.is_trusted("conflict-peer"));
        assert_eq!(
            recovered.peer_pairing_secret("conflict-peer"),
            Some(expected_secret)
        );
    }

    #[test]
    fn remove_peer_handles_keyring_failure_idempotently() {
        let dir = TempDir::new("remove-failure");
        let backend = Arc::new(MockKeyringBackend::default());
        let mut store = open_mock_store_with(dir.path(), Arc::clone(&backend)).expect("open");

        let offer = store
            .begin_pairing("peer-del", NetworkTrust::Lan, true)
            .expect("offer");
        let proof = offer.proof_for([0x31u8; 32]);
        store
            .accept_pairing(
                &offer,
                &proof,
                PairingConsent { approved: true },
                [0x31u8; 32],
            )
            .expect("accept");
        let meta_path = metadata_path(dir.path());

        backend.set_fail_delete(Some(KeyringBackendError::Unavailable));
        assert!(matches!(
            store.remove_peer("peer-del"),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
        assert!(!store.is_trusted("peer-del"));
        assert_eq!(store.peer_pairing_secret("peer-del"), None);
        let meta_after_fail = fs::read_to_string(&meta_path).expect("meta remains marked revoked");
        assert!(meta_after_fail.contains("\"trusted\":false"));
        assert_eq!(backend.count(), 2);

        assert!(store.remove_peer("peer-del").expect("retry remove"));
        assert!(!meta_path.exists());
        assert_eq!(backend.count(), 1);
        assert!(!store
            .remove_peer("peer-del")
            .expect("idempotent second remove"));

        backend.set_fail_delete(Some(KeyringBackendError::AccessDenied));
        assert!(matches!(
            store.remove_peer("never-paired"),
            Err(SecureWorkerError::KeyringUnavailable)
        ));
    }

    #[test]
    fn mismatched_keyring_identity_and_persisted_public_key_fails_closed() {
        let dir_a = TempDir::new("id-mismatch-a");
        let backend_a = Arc::new(MockKeyringBackend::default());
        let store_a = open_mock_store_with(dir_a.path(), Arc::clone(&backend_a)).expect("open a");
        let pub_a = store_a.local_static_public();
        let priv_a_hex = hex(&find_raw_entry(&backend_a, "id-"));
        drop(store_a);

        // Matching keypair reopen must still succeed.
        let reopened_a = open_mock_store_with(dir_a.path(), backend_a).expect("matching reopen a");
        assert_eq!(reopened_a.local_static_public(), pub_a);
        drop(reopened_a);

        let dir_b = TempDir::new("id-mismatch-b");
        let backend_b = Arc::new(MockKeyringBackend::default());
        let store_b = open_mock_store_with(dir_b.path(), Arc::clone(&backend_b)).expect("open b");
        let pub_b = store_b.local_static_public();
        assert_ne!(pub_a, pub_b);
        drop(store_b);

        // Overwrite dir_b's keyring entry with key A's private secret while leaving
        // state.json.identity_public at pub_b (with identity.key absent).
        let id_locator_b = backend_b
            .locators()
            .into_iter()
            .find(|loc| loc.account.starts_with("id-"))
            .expect("id locator for b");
        backend_b
            .set_secret(&id_locator_b, &priv_a_hex)
            .expect("overwrite b keyring entry with key a");
        assert!(!dir_b.path().join("identity.key").exists());

        assert!(matches!(
            open_mock_store_with(dir_b.path(), backend_b),
            Err(SecureWorkerError::StoreCorrupt)
        ));
    }

    #[test]
    fn expired_pairing_offer_is_rejected_and_consumed() {
        let dir = TempDir::new("expiry");
        let (mut store, _keyring) = open_mock_store(dir.path()).expect("store");
        let offer = store
            .begin_pairing("peer-a", NetworkTrust::Lan, true)
            .expect("offer");
        let proof = offer.proof_for([0x11u8; 32]);
        let pending = store
            .pending
            .get_mut(&offer.challenge.nonce)
            .expect("pending");
        pending.expires_at = 0;
        let result = store.accept_pairing(
            &offer,
            &proof,
            PairingConsent { approved: true },
            [0x11u8; 32],
        );
        assert!(matches!(
            result,
            Err(SecureWorkerError::Pairing(
                PairingError::StaleOrReplayedEpoch
            ))
        ));
        assert!(!store.pending.contains_key(&offer.challenge.nonce));
    }

    #[test]
    fn secret32_is_zeroize_on_drop_and_hex_checks_reject_bad_inputs() {
        fn assert_zeroize_on_drop<T: Zeroize + ZeroizeOnDrop>(_val: &T) {}

        let mut secret = Secret32::from_bytes([0x5Cu8; 32]);
        assert_zeroize_on_drop(&secret);
        let hex = secret32_to_hex(&secret);
        assert_eq!(hex.len(), 64);
        assert_eq!(
            secret32_from_hex_bytes(hex.as_bytes()).expect("valid hex"),
            secret
        );
        secret.zeroize();
        assert_eq!(secret.expose(), &[0u8; 32]);

        assert!(matches!(
            secret32_from_hex_bytes(b"abcd"),
            Err(SecureWorkerError::StoreCorrupt)
        ));
        assert!(matches!(
            secret32_from_hex_bytes(&[b'0'; 64]),
            Err(SecureWorkerError::StoreCorrupt)
        ));
        let mut bad_hex = [b'a'; 64];
        bad_hex[63] = b'z';
        assert!(matches!(
            secret32_from_hex_bytes(&bad_hex),
            Err(SecureWorkerError::StoreCorrupt)
        ));
    }

    #[test]
    fn keyring_locators_are_opaque_and_deterministic() {
        let dir = TempDir::new("locator-secret-path");
        let scope = store_scope_digest(dir.path());
        assert_eq!(scope, store_scope_digest(dir.path()));

        let id_loc = identity_locator(&scope);
        let peer_loc = peer_locator(&scope, "secret-peer-name-123");
        let dir_str = dir.path().to_string_lossy();
        for loc in [&id_loc, &peer_loc] {
            assert!(!loc.service.contains(dir_str.as_ref()));
            assert!(!loc.account.contains(dir_str.as_ref()));
            assert!(!loc.label.contains(dir_str.as_ref()));
            assert!(!loc.service.contains("secret-peer-name-123"));
            assert!(!loc.account.contains("secret-peer-name-123"));
            assert!(!loc.label.contains("secret-peer-name-123"));
        }
        assert_ne!(id_loc.account, peer_loc.account);
    }

    fn pair_unit_peer(root: &Path, peer_id: &str) -> (TrustStore, Arc<MockKeyringBackend>) {
        let (mut store, keyring) = open_mock_store(root).expect("open");
        let offer = store
            .begin_pairing(peer_id, NetworkTrust::Lan, true)
            .expect("offer");
        let proof = offer.proof_for([0x22u8; 32]);
        store
            .accept_pairing(
                &offer,
                &proof,
                PairingConsent { approved: true },
                [0x22u8; 32],
            )
            .expect("accept");
        assert!(store.is_trusted(peer_id));
        assert_eq!(keyring.count(), 2);
        (store, keyring)
    }

    #[test]
    fn remove_peer_metadata_write_failure_propagates_error() {
        let dir = TempDir::new("remove-meta-write-fail");
        let (mut store, keyring) = pair_unit_peer(dir.path(), "peer-a");
        store.persist_faults.fail_meta_write = true;

        assert!(
            store.remove_peer("peer-a").is_err(),
            "metadata write failure must not be ignored"
        );
        assert!(!store.is_trusted("peer-a"));
        assert_eq!(store.peer_pairing_secret("peer-a"), None);
        assert_eq!(keyring.delete_calls(), 1);
        assert_eq!(keyring.count(), 1);
    }

    #[test]
    fn remove_peer_state_write_failure_still_deletes_keyring_and_clears_trust() {
        let dir = TempDir::new("remove-state-write-fail");
        let (mut store, keyring) = pair_unit_peer(dir.path(), "peer-a");
        store.persist_faults.fail_state_write = true;

        assert!(
            store.remove_peer("peer-a").is_err(),
            "state write failure must fail explicitly"
        );
        assert!(!store.is_trusted("peer-a"));
        assert_eq!(store.peer_pairing_secret("peer-a"), None);
        assert_eq!(keyring.delete_calls(), 1);
        assert_eq!(keyring.count(), 1);

        let reopened = open_mock_store_with(dir.path(), Arc::clone(&keyring)).expect("reopen");
        assert!(!reopened.is_trusted("peer-a"));
        assert_eq!(reopened.peer_pairing_secret("peer-a"), None);
    }

    #[test]
    fn remove_peer_combined_metadata_write_state_write_and_remove_failures_fail_closed() {
        let dir = TempDir::new("remove-combined-fail");
        let (mut store, keyring) = pair_unit_peer(dir.path(), "peer-a");
        store.persist_faults.fail_meta_write = true;
        store.persist_faults.fail_state_write = true;
        store.persist_faults.fail_meta_remove = true;

        assert!(
            store.remove_peer("peer-a").is_err(),
            "combined persistence failure must fail explicitly"
        );
        assert!(!store.is_trusted("peer-a"));
        assert_eq!(store.peer_pairing_secret("peer-a"), None);
        assert_eq!(keyring.delete_calls(), 1);
        assert_eq!(keyring.count(), 1);

        let reopened = open_mock_store_with(dir.path(), Arc::clone(&keyring)).expect("reopen");
        assert!(!reopened.is_trusted("peer-a"));
        assert_eq!(reopened.peer_pairing_secret("peer-a"), None);
    }

    #[test]
    fn remove_peer_multi_backend_and_keyring_failure_attempts_all_revocations_and_fails_closed() {
        let dir = TempDir::new("remove-multi-backend-fail");
        let (mut store, keyring) = pair_unit_peer(dir.path(), "peer-a");
        store.persist_faults.fail_meta_write = true;
        store.persist_faults.fail_state_write = true;
        keyring.set_fail_delete(Some(KeyringBackendError::Unavailable));

        assert!(
            store.remove_peer("peer-a").is_err(),
            "multi-backend failure must fail explicitly"
        );
        assert!(!store.is_trusted("peer-a"));
        assert_eq!(store.peer_pairing_secret("peer-a"), None);
        assert_eq!(keyring.delete_calls(), 1);
    }

    #[test]
    fn map_keyring_backend_error_maps_all_failure_classes() {
        assert!(matches!(
            map_keyring_backend_error(KeyringBackendError::Unavailable),
            SecureWorkerError::KeyringUnavailable
        ));
        assert!(matches!(
            map_keyring_backend_error(KeyringBackendError::AccessDenied),
            SecureWorkerError::KeyringUnavailable
        ));
        assert!(matches!(
            map_keyring_backend_error(KeyringBackendError::InvalidData),
            SecureWorkerError::StoreCorrupt
        ));
        assert!(matches!(
            map_keyring_backend_error(KeyringBackendError::Unsupported),
            SecureWorkerError::Unsupported
        ));
    }
}
