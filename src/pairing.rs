//! Authenticated opt-in peer pairing and trusted-network policy.
//!
//! Discovery only advertises capability; participation requires explicit opt-in,
//! a trusted network tier (loopback, LAN, or Tailscale), and keyed BLAKE3
//! challenge-response authentication.

use std::collections::HashSet;
use std::fmt;

use zeroize::Zeroize;

use crate::{digest, Digest};

/// Network trust classification for a peer endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkTrust {
    Loopback,
    Lan,
    Tailscale,
    Untrusted,
}

impl NetworkTrust {
    /// Returns true only for explicitly supported private/paired network tiers.
    pub fn is_permitted(self) -> bool {
        matches!(self, Self::Loopback | Self::Lan | Self::Tailscale)
    }
}

/// Advertised peer identity prior to or during pairing. Routing addresses are
/// intentionally excluded: endpoints are transient local state and must never
/// enter pairing/persistence/debug output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pub peer_id: String,
    pub os: String,
    pub runtime: String,
    pub trust: NetworkTrust,
    pub opted_in: bool,
}

/// Server-issued challenge for opt-in pairing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingChallenge {
    pub peer_id: String,
    pub nonce: [u8; 32],
    pub epoch: u64,
}

/// Client proof computed with the shared pairing secret.
///
/// `Debug` is implemented manually so the keyed proof is never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingProof {
    pub peer_id: String,
    pub epoch: u64,
    pub static_public: [u8; 32],
    pub mac: Digest,
}

impl fmt::Debug for PairingProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingProof")
            .field("peer_id", &self.peer_id)
            .field("epoch", &self.epoch)
            .field("static_public", &"[PINNED]")
            .field("mac", &"[REDACTED]")
            .finish()
    }
}

/// 32-byte session PSK with redacted `Debug` and zeroize-on-drop.
///
/// Raw bytes are crate-private; callers get a non-secret fingerprint for
/// diagnostics and can move the value to the secure worker through its
/// `Secret32::from_session_key` adapter.
pub struct SessionKey([u8; 32]);

impl SessionKey {
    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Domain-separated, non-secret fingerprint of the session key.
    pub fn fingerprint(&self) -> Digest {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"FASTLINK-SESSION-FINGERPRINT-V1");
        hasher.update(&self.0);
        *hasher.finalize().as_bytes()
    }

    /// Hex rendering of [`Self::fingerprint`] for tests/diagnostics.
    pub fn fingerprint_hex(&self) -> String {
        crate::hex(&self.fingerprint())
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Zeroize for SessionKey {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for SessionKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionKey([REDACTED])")
    }
}

/// Authenticated session established after pairing succeeds.
///
/// `Debug` is implemented manually so the PSK/session key is never printed.
pub struct PairingSession {
    pub session_id: Digest,
    pub peer_id: String,
    pub trust: NetworkTrust,
    pub epoch: u64,
    pub session_key: SessionKey,
}

impl fmt::Debug for PairingSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingSession")
            .field("session_id", &self.session_id)
            .field("peer_id", &self.peer_id)
            .field("trust", &self.trust)
            .field("epoch", &self.epoch)
            .field("session_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingError {
    NotOptedIn,
    UntrustedNetwork,
    PeerMismatch,
    StaleOrReplayedEpoch,
    InvalidProof,
    InvalidPeerId,
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotOptedIn => write!(f, "peer has not explicitly opted in"),
            Self::UntrustedNetwork => write!(f, "untrusted network tier rejected"),
            Self::PeerMismatch => write!(f, "proof peer_id does not match challenge"),
            Self::StaleOrReplayedEpoch => write!(f, "stale or replayed pairing epoch"),
            Self::InvalidProof => write!(f, "invalid keyed-hash pairing proof"),
            Self::InvalidPeerId => write!(f, "peer_id must not be empty"),
        }
    }
}

impl std::error::Error for PairingError {}

/// Constant-time 32-byte digest comparison.
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn challenge_mac(
    shared_secret: &[u8; 32],
    challenge: &PairingChallenge,
    peer_static_public: &[u8; 32],
) -> Digest {
    let mut material = Vec::with_capacity(challenge.peer_id.len() + 84);
    material.extend_from_slice(b"FASTLINK-PAIR-V2");
    material.extend_from_slice(&(challenge.peer_id.len() as u64).to_le_bytes());
    material.extend_from_slice(challenge.peer_id.as_bytes());
    material.extend_from_slice(&challenge.nonce);
    material.extend_from_slice(&challenge.epoch.to_le_bytes());
    material.extend_from_slice(peer_static_public);
    *blake3::keyed_hash(shared_secret, &material).as_bytes()
}

fn derive_session_key(
    shared_secret: &[u8; 32],
    challenge: &PairingChallenge,
    peer_static_public: &[u8; 32],
) -> SessionKey {
    let mut material = Vec::with_capacity(challenge.peer_id.len() + 100);
    material.extend_from_slice(b"FASTLINK-SESSION-V2");
    material.extend_from_slice(&(challenge.peer_id.len() as u64).to_le_bytes());
    material.extend_from_slice(challenge.peer_id.as_bytes());
    material.extend_from_slice(&challenge.nonce);
    material.extend_from_slice(&challenge.epoch.to_le_bytes());
    material.extend_from_slice(peer_static_public);
    SessionKey::from_bytes(*blake3::keyed_hash(shared_secret, &material).as_bytes())
}

/// Computes the client response proof for a pairing challenge, binding the
/// peer's X25519 static public key into the authenticated transcript.
pub fn sign_challenge(
    shared_secret: &[u8; 32],
    challenge: &PairingChallenge,
    peer_static_public: [u8; 32],
) -> PairingProof {
    PairingProof {
        peer_id: challenge.peer_id.clone(),
        epoch: challenge.epoch,
        static_public: peer_static_public,
        mac: challenge_mac(shared_secret, challenge, &peer_static_public),
    }
}

/// Validates peer eligibility, prevents replay, and verifies the pairing proof.
pub struct PairingRegistry {
    shared_secret: [u8; 32],
    min_epoch: u64,
    used_nonces: HashSet<[u8; 32]>,
}

impl PairingRegistry {
    pub fn new(shared_secret: [u8; 32], min_epoch: u64) -> Self {
        Self {
            shared_secret,
            min_epoch,
            used_nonces: HashSet::new(),
        }
    }

    pub fn issue_challenge(
        &self,
        peer: &PeerIdentity,
        nonce: [u8; 32],
        epoch: u64,
    ) -> Result<PairingChallenge, PairingError> {
        if peer.peer_id.trim().is_empty() {
            return Err(PairingError::InvalidPeerId);
        }
        if !peer.opted_in {
            return Err(PairingError::NotOptedIn);
        }
        if !peer.trust.is_permitted() {
            return Err(PairingError::UntrustedNetwork);
        }
        if epoch < self.min_epoch || self.used_nonces.contains(&nonce) {
            return Err(PairingError::StaleOrReplayedEpoch);
        }
        Ok(PairingChallenge {
            peer_id: peer.peer_id.clone(),
            nonce,
            epoch,
        })
    }

    pub fn verify_and_admit(
        &mut self,
        peer: &PeerIdentity,
        challenge: &PairingChallenge,
        proof: &PairingProof,
    ) -> Result<PairingSession, PairingError> {
        if peer.peer_id.trim().is_empty() {
            return Err(PairingError::InvalidPeerId);
        }
        if !peer.opted_in {
            return Err(PairingError::NotOptedIn);
        }
        if !peer.trust.is_permitted() {
            return Err(PairingError::UntrustedNetwork);
        }
        if peer.peer_id != challenge.peer_id || proof.peer_id != challenge.peer_id {
            return Err(PairingError::PeerMismatch);
        }
        if challenge.epoch < self.min_epoch
            || proof.epoch != challenge.epoch
            || self.used_nonces.contains(&challenge.nonce)
        {
            return Err(PairingError::StaleOrReplayedEpoch);
        }

        if proof.static_public == [0u8; 32] {
            return Err(PairingError::InvalidProof);
        }
        let expected = challenge_mac(&self.shared_secret, challenge, &proof.static_public);
        if !constant_time_eq(&expected, &proof.mac) {
            return Err(PairingError::InvalidProof);
        }

        self.used_nonces.insert(challenge.nonce);
        let session_key = derive_session_key(&self.shared_secret, challenge, &proof.static_public);
        let mut sid_input = Vec::with_capacity(peer.peer_id.len() + 40);
        sid_input.extend_from_slice(peer.peer_id.as_bytes());
        sid_input.extend_from_slice(&challenge.nonce);
        sid_input.extend_from_slice(&challenge.epoch.to_le_bytes());

        Ok(PairingSession {
            session_id: digest(&sid_input),
            peer_id: peer.peer_id.clone(),
            trust: peer.trust,
            epoch: challenge.epoch,
            session_key,
        })
    }

    pub fn advance_min_epoch(&mut self, new_min_epoch: u64) {
        if new_min_epoch > self.min_epoch {
            self.min_epoch = new_min_epoch;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroize;

    #[test]
    fn session_key_is_redacted_and_explicitly_zeroizable() {
        let mut key = SessionKey::from_bytes([0x5Au8; 32]);
        assert_eq!(key.fingerprint_hex().len(), 64);
        let rendered = format!("{key:?}");
        assert!(rendered.contains("REDACTED"));
        assert!(!rendered.contains("5a"));
        key.zeroize();
        assert_eq!(
            key.fingerprint_hex(),
            SessionKey::from_bytes([0u8; 32]).fingerprint_hex()
        );
    }
}
