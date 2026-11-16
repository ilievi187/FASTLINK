//! Experimental secure Linux worker backend (first slice).
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
//!   the no-echo interactive transfer and secure-at-rest keyring UX are **not**
//!   implemented here and remain a pending sub-gate.
//! - A fresh pairing secret is used as the Noise `XXpsk3` pre-shared key. It is
//!   never used directly as a MAC or stream cipher.
//! - Noise messages are capped at the protocol limit of 65,535 bytes including
//!   the 16-byte AEAD tag. Logical frames up to 256 KiB use bounded explicit
//!   chunking, never a single oversized Noise message.
//! - No application frame is accepted before a successful Noise handshake and
//!   explicit post-handshake confirmation.
//! - Persisted trust metadata contains no routing endpoint. Endpoints are held
//!   only in transient sockets.
//!
//! The `snow` crate (0.10.0) is not formally audited. See `SECURITY.md` and the
//! design document before enabling this code outside tests.

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

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
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| SecureWorkerError::Entropy)?;
        Ok(Self(bytes))
    }

    /// Wraps caller-provided bytes. The caller remains responsible for the
    /// original buffer.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
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
    /// **This is not a substitute for the no-echo interactive transfer.** A
    /// production UI must display the one-time secret and obtain the peer's
    /// proof after local no-echo entry; this helper exists so loopback tests can
    /// exercise the trust-store path without a TTY.
    #[doc(hidden)]
    pub fn proof(&self) -> PairingProof {
        sign_challenge(self.secret.expose(), &self.challenge)
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

fn write_frame(
    stream: &mut TcpStream,
    ciphertext: &[u8],
    min_len: usize,
    timeout: Duration,
) -> Result<(), SecureWorkerError> {
    if ciphertext.len() < min_len || ciphertext.len() > MAX_NOISE_MESSAGE_BYTES {
        return Err(SecureWorkerError::FrameTooLarge);
    }
    let deadline = Instant::now() + timeout;
    let len = u32::try_from(ciphertext.len()).map_err(|_| SecureWorkerError::FrameTooLarge)?;
    write_all_before(stream, &len.to_le_bytes(), deadline)?;
    write_all_before(stream, ciphertext, deadline)?;
    Ok(())
}

fn read_frame(
    stream: &mut TcpStream,
    min_len: usize,
    timeout: Duration,
) -> Result<Vec<u8>, SecureWorkerError> {
    let deadline = Instant::now() + timeout;
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
        set_stream_timeouts(&stream, config.timeout)?;
        let mut handshake = build_handshake(config, true)?;
        let mut out = vec![0u8; MAX_NOISE_MESSAGE_BYTES];

        let written = handshake
            .write_message(&[], &mut out)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        write_frame(&mut stream, &out[..written], 1, config.timeout)?;

        let message = read_frame(&mut stream, 1, config.timeout)?;
        let mut payload = Zeroizing::new(vec![0u8; MAX_NOISE_MESSAGE_BYTES]);
        handshake
            .read_message(&message, &mut payload)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        verify_remote_static(handshake.get_remote_static(), config)?;

        let written = handshake
            .write_message(&[], &mut out)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        write_frame(&mut stream, &out[..written], 1, config.timeout)?;

        let remote_static = copy_remote_static(handshake.get_remote_static())?;
        let transport = handshake
            .into_transport_mode()
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        let session = Self::new(stream, transport, remote_static, config.timeout);
        complete_confirmation(session, true)
    }

    /// Runs the responder side of the handshake on an already-accepted stream.
    pub fn handshake_responder(
        mut stream: TcpStream,
        config: &NoiseConfig,
    ) -> Result<Self, SecureWorkerError> {
        set_stream_timeouts(&stream, config.timeout)?;
        let mut handshake = build_handshake(config, false)?;
        let mut out = vec![0u8; MAX_NOISE_MESSAGE_BYTES];

        let message = read_frame(&mut stream, 1, config.timeout)?;
        let mut payload = Zeroizing::new(vec![0u8; MAX_NOISE_MESSAGE_BYTES]);
        handshake
            .read_message(&message, &mut payload)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;

        let written = handshake
            .write_message(&[], &mut out)
            .map_err(|_| SecureWorkerError::HandshakeFailed)?;
        write_frame(&mut stream, &out[..written], 1, config.timeout)?;

        let message = read_frame(&mut stream, 1, config.timeout)?;
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
        complete_confirmation(session, false)
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
        write_frame(
            &mut self.stream,
            &ciphertext,
            NOISE_TAG_BYTES,
            self.frame_timeout,
        )?;
        self.sent_messages += 1;
        Ok(())
    }

    fn recv_inner(&mut self) -> Result<(u8, Vec<u8>), SecureWorkerError> {
        if self.received_messages >= MAX_SESSION_MESSAGES {
            return Err(SecureWorkerError::SessionLimit);
        }
        let ciphertext = read_frame(&mut self.stream, NOISE_TAG_BYTES, self.frame_timeout)?;
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
) -> Result<NoiseSession, SecureWorkerError> {
    if initiator {
        session.send_inner(FRAME_CONFIRM, CONFIRM_PAYLOAD)?;
        let (frame_type, payload) = session.recv_inner()?;
        if frame_type != FRAME_CONFIRM || payload != CONFIRM_PAYLOAD {
            return Err(SecureWorkerError::HandshakeFailed);
        }
    } else {
        let (frame_type, payload) = session.recv_inner()?;
        if frame_type != FRAME_CONFIRM || payload != CONFIRM_PAYLOAD {
            return Err(SecureWorkerError::HandshakeFailed);
        }
        session.send_inner(FRAME_CONFIRM, CONFIRM_PAYLOAD)?;
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

#[derive(Serialize, Deserialize)]
struct StoreState {
    trust_epoch: u64,
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

/// Durable local trust store.
///
/// Directory mode is 0700 and secret/metadata files are 0600. Metadata contains
/// no routing endpoint. Corrupt records are ignored (treated as untrusted) so a
/// damaged store fails closed and requires re-pairing.
pub struct TrustStore {
    root: PathBuf,
    peers_dir: PathBuf,
    identity_private: Secret32,
    identity_public: [u8; 32],
    trust_epoch: u64,
    peers: HashMap<String, PeerRecord>,
    pending: HashMap<[u8; 32], PendingPairing>,
}

impl TrustStore {
    /// Opens (and if necessary initializes) the trust store at `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SecureWorkerError> {
        let root = root.as_ref().to_path_buf();
        ensure_root_dir(&root)?;
        let peers_dir = root.join("peers");
        ensure_peers_dir(&peers_dir)?;
        cleanup_temporary_files(&root)?;
        cleanup_temporary_files(&peers_dir)?;

        let (identity_private, identity_public) = load_or_create_identity(&root)?;
        let trust_epoch = load_state(&root)?;
        let mut store = Self {
            root,
            peers_dir,
            identity_private,
            identity_public,
            trust_epoch,
            peers: HashMap::new(),
            pending: HashMap::new(),
        };
        store.load_peers()?;
        Ok(store)
    }

    /// Installation X25519 private key (mode-0600 file).
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
    /// the authenticated pairing exchange. This method performs no network I/O;
    /// it binds the caller-supplied pin to the consented pairing record. A
    /// production caller must not accept this key from unauthenticated
    /// discovery data.
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
        if peer_static_public == [0u8; 32] {
            return Err(SecureWorkerError::InvalidPeerKey);
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
        let secret_path = self.peers_dir.join(format!("{stem}.secret"));
        let meta_path = self.peers_dir.join(format!("{stem}.meta.json"));
        atomic_write(&secret_path, pending.secret.expose(), 0o600)?;
        let meta = PeerMeta {
            peer_id: pending.peer_id.clone(),
            static_public: hex_encode(&peer_static_public),
            trust: trust_label(pending.trust).to_string(),
            epoch: pending.epoch,
            trusted: true,
            created_at: now_secs(),
        };
        let encoded = serde_json::to_vec(&meta).map_err(|_| SecureWorkerError::StoreCorrupt)?;
        if let Err(error) = atomic_write(&meta_path, &encoded, 0o600) {
            // Do not leave an orphaned pairing secret behind if the trust
            // metadata write fails.
            let _ = remove_file_if_present(&secret_path);
            return Err(error);
        }

        self.peers.insert(
            pending.peer_id.clone(),
            PeerRecord {
                static_public: peer_static_public,
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
    /// This is storage-level revocation. To close a live connection at the same
    /// time, use [`WorkerServer::remove_peer`].
    pub fn remove_peer(&mut self, peer_id: &str) -> Result<bool, SecureWorkerError> {
        let stem = peer_file_stem(peer_id);
        let meta_path = self.peers_dir.join(format!("{stem}.meta.json"));
        let secret_path = self.peers_dir.join(format!("{stem}.secret"));
        let existed =
            meta_path.exists() || secret_path.exists() || self.peers.contains_key(peer_id);
        if !existed {
            return Ok(false);
        }

        // Mark revoked before deletion so an interrupted removal fails closed.
        if meta_path.exists() {
            if let Ok(bytes) = fs::read(&meta_path) {
                if let Ok(mut meta) = serde_json::from_slice::<PeerMeta>(&bytes) {
                    meta.trusted = false;
                    if let Ok(encoded) = serde_json::to_vec(&meta) {
                        let _ = atomic_write(&meta_path, &encoded, 0o600);
                    }
                }
            }
        }
        remove_file_if_present(&meta_path)?;
        remove_file_if_present(&secret_path)?;
        self.peers.remove(peer_id);
        self.pending.retain(|_, pending| pending.peer_id != peer_id);
        self.trust_epoch = now_secs().max(self.trust_epoch.saturating_add(1));
        self.persist_state()?;
        Ok(true)
    }

    fn next_epoch(&mut self) -> Result<u64, SecureWorkerError> {
        let epoch = now_secs().max(self.trust_epoch.saturating_add(1));
        self.trust_epoch = epoch;
        self.persist_state()?;
        Ok(epoch)
    }

    fn persist_state(&self) -> Result<(), SecureWorkerError> {
        let state = StoreState {
            trust_epoch: self.trust_epoch,
        };
        let encoded = serde_json::to_vec(&state).map_err(|_| SecureWorkerError::StoreCorrupt)?;
        atomic_write(&self.root.join("state.json"), &encoded, 0o600)
    }

    fn expire_pending(&mut self) {
        let now = now_secs();
        self.pending.retain(|_, pending| pending.expires_at >= now);
    }

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
            let secret_path = self.peers_dir.join(format!("{stem}.secret"));
            let Ok(secret_meta) = fs::symlink_metadata(&secret_path) else {
                continue;
            };
            if secret_meta.file_type().is_symlink()
                || !secret_meta.is_file()
                || secret_meta.len() != 32
            {
                continue;
            }
            let Ok(secret_bytes) = fs::read(&secret_path) else {
                continue;
            };
            let secret_bytes = Zeroizing::new(secret_bytes);
            let mut secret = [0u8; 32];
            secret.copy_from_slice(&secret_bytes);
            if secret == [0u8; 32] {
                continue;
            }
            if fs::set_permissions(&meta_path, fs::Permissions::from_mode(0o600)).is_err()
                || fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600)).is_err()
            {
                continue;
            }
            let pairing_secret = Secret32::from_bytes(secret);
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
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| SecureWorkerError::StorePermission)?;
    Ok(())
}

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

fn ensure_peers_dir(path: &Path) -> Result<(), SecureWorkerError> {
    prepare_private_dir(path, |name| {
        name.ends_with(".meta.json") || name.ends_with(".secret")
    })
}

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

fn load_or_create_identity(root: &Path) -> Result<(Secret32, [u8; 32]), SecureWorkerError> {
    let path = root.join("identity.key");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() != 64 {
                return Err(SecureWorkerError::StoreCorrupt);
            }
            let bytes = Zeroizing::new(fs::read(&path)?);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(|_| SecureWorkerError::StorePermission)?;
            let mut private = [0u8; 32];
            let mut public = [0u8; 32];
            private.copy_from_slice(&bytes[0..32]);
            public.copy_from_slice(&bytes[32..64]);
            if private == [0u8; 32] || public == [0u8; 32] {
                return Err(SecureWorkerError::StoreCorrupt);
            }
            Ok((Secret32::from_bytes(private), public))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let params = NOISE_PROTOCOL
                .parse()
                .map_err(|_| SecureWorkerError::InvalidConfig)?;
            let mut keypair = snow::Builder::new(params)
                .generate_keypair()
                .map_err(|_| SecureWorkerError::Entropy)?;
            if keypair.private.len() != 32 || keypair.public.len() != 32 {
                return Err(SecureWorkerError::Entropy);
            }
            let mut material = Zeroizing::new(Vec::with_capacity(64));
            material.extend_from_slice(&keypair.private);
            material.extend_from_slice(&keypair.public);
            keypair.private.zeroize();
            atomic_write(&path, &material, 0o600)?;
            let mut private = [0u8; 32];
            let mut public = [0u8; 32];
            private.copy_from_slice(&material[0..32]);
            public.copy_from_slice(&material[32..64]);
            Ok((Secret32::from_bytes(private), public))
        }
        Err(err) => Err(map_io(err)),
    }
}

fn load_state(root: &Path) -> Result<u64, SecureWorkerError> {
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
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(|_| SecureWorkerError::StorePermission)?;
            Ok(state.trust_epoch)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(map_io(err)),
    }
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
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))
            .map_err(|_| SecureWorkerError::StorePermission)?;
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

    struct UnitTemp(PathBuf);

    impl UnitTemp {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "fastlink_secure_worker_unit_{label}_{}_{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp");
            Self(path)
        }
    }

    impl Drop for UnitTemp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn expired_pairing_offer_is_rejected_and_consumed() {
        let dir = UnitTemp::new("expiry");
        let mut store = TrustStore::open(&dir.0).expect("store");
        let offer = store
            .begin_pairing("peer-a", NetworkTrust::Lan, true)
            .expect("offer");
        let proof = offer.proof();
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
}
