# Secure Linux Worker Design (Task 7)

**Status: design plus an experimental Linux-only backend slice.** The backend
exists on `feat/secure-linux-worker` and is backed by loopback tests, but it has
not received an independent audit. This document makes no production,
benchmark, or secure-UX claim. The no-echo pairing UI and OS-keyring storage
remain unimplemented.

The design targets the M1 security gate and the M2 discovery/pairing boundary:

- explicit user consent for pairing;
- authenticated, encrypted peer transport before any model, prompt, activation,
  or KV/SSM state is sent;
- durable local trust and removal;
- Linux-first, CPU-only, loopback-only verification;
- no raw endpoint, IP, MAC, provider message, or secret in public JSON, logs,
  trust metadata, or committed artifacts.

## 1. Current state and constraints

Verified from this worktree:

- `src/pairing.rs` performs keyed-BLAKE3 challenge authentication and derives a
  32-byte session key, now held in a redacted zeroize-on-drop `SessionKey`
  wrapper. `PeerIdentity` and `PairingSession` no longer carry raw `addr`
  fields; routing remains transient socket state.
- `src/stage_auth.rs` is an epoch/hop-scoped keyed-BLAKE3 **MAC**. It provides
  integrity/authentication of activation frames, not confidentiality.
- `src/node_runtime.rs` moves activation frames over loopback TCP and checks the
  stage MAC. It does not encrypt transport and is not a remote security
  boundary.
- `docs/PROTOCOL.md` explicitly says the v0.1 transfer protocol is neither
  encrypted nor authenticated and permits only loopback listeners.
- `Cargo.lock` includes `snow` 0.10.0 with a minimal feature set for the
  experimental Linux-only `src/secure_worker.rs` slice; no TLS or production
  secure-transport claim is made.
- The experimental slice adds a durable trust/removal store. There is still no
  real distributed inference.
- Discovery output/candidates remain unpaired and untrusted. A Tailscale peer is
  not a FASTLINK worker and candidate metadata must not enter scheduling.

The design below adds a new security boundary without reusing the current MAC as
encryption or inventing a cryptographic construction.

## 2. Decision: Noise `XXpsk3` for the smallest secure v1

Recommend **Noise Protocol Framework** with the fixed suite:

```text
Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s
```

using the Rust `snow` 0.10 builder API. The Noise specification is
<https://noiseprotocol.org/noise.html>; the builder API is
<https://docs.rs/snow/0.10.0/snow/struct.Builder.html>.

The existing derived `PairingSession::session_key` is used **only as the Noise
PSK**, not as a stream cipher, AEAD key, MAC, or hand-rolled KDF. Noise itself
performs the ephemeral X25519 exchange, static-key authentication, key
derivation and ChaCha20-Poly1305 transport encryption.

### Why this is the smallest viable secure v1

- It matches the product ceremony exactly: one high-entropy pairing secret
  transferred by the user, no public CA, DNS PKI, or web-style server identity.
- `XXpsk3` provides mutual static-key authentication plus a PSK mixed at the
  third handshake message (index 3). It gives forward-secret ephemeral keys
  while binding both peers to the pre-shared secret.
- It avoids a custom TLS certificate verifier or private-CA lifecycle while
  keeping confidentiality and authentication in a reviewed protocol rather than
  app code.
- The authenticated handshake must complete before any application frame is
  parsed.

### Noise does not mean "audited"

`snow`'s own project notice says it is **not formally audited**. Using it is a
risk decision that requires a security review before a production release. This
design does not claim an audit, formal verification, or side-channel resistance
for `snow`, Noise, or any transitive dependency.

### Alternatives considered

| Option | Advantages | Problems for v1 |
| --- | --- | --- |
| Noise `XXpsk3` + `snow` | PSK matches explicit pairing; mutual static auth; forward secrecy; no CA/PKI; small protocol surface | New dependency; integration/state-machine correctness still needs review; `snow` not formally audited |
| rustls TLS 1.3 with certificate pinning | Mature TLS implementation and familiar certificate tooling; strong ecosystem | Requires a private CA or self-signed certificate bootstrap, peer-certificate distribution, rotation/removal, and a carefully reviewed pinning verifier. A custom verifier is easy to get wrong, and pinning an endpoint/hostname creates endpoint-privacy pressure. More moving parts than Task 7 needs. |

If TLS is later preferred, it must use a normal rustls verification path with a
locally generated CA/certificate trust anchor and an exact pinned leaf/SPKI
check. Verification, hostname/SAN checks, and certificate validity must not be
disabled; no "accept any certificate" verifier is permitted. That design is out
of scope here.

## 3. Threat model and non-goals

In scope:

- active network attacker on LAN/Tailscale;
- unpaired discovery candidate impersonation;
- replayed pairing proofs, handshake messages and transport frames;
- tampered/truncated/oversized frames;
- stale sessions after removal/restart;
- accidental plaintext leakage before authentication;
- secret/endpoint leakage through JSON, logs, trust metadata, or CLI arguments;
- local same-user file disclosure through permissive storage modes (best effort;
  a same-user attacker or root is outside strong isolation).

Out of scope for this milestone:

- protection from a compromised local OS/root, malicious kernel, or physical
  attacker;
- formal side-channel/audit claims;
- public internet listeners, port scanning, automatic discovery/pairing;
- model/prompt/KV transport or inference;
- cross-platform secure worker implementation (Linux first; Windows later);
- recovery from a compromised long-term identity key without an explicit
  re-pair/reset ceremony.

## 4. Pairing ceremony (explicit user consent)

### 4.1 Prerequisites

- The user has selected a specific discovery candidate or entered a local
  pairing request. Discovery alone never pairs.
- A human confirms the peer display name/opaque node ID on both sides.
- The pairing command must read the one-time secret interactively from a
  **no-echo TTY**. It must refuse non-interactive stdin and must never accept the
  secret through `argv`, environment variables, shell history, logs, or a
  provider message.

### 4.2 Secret generation and transfer

1. The initiating host generates a 32-byte `pairing_secret` with the operating
   system CSPRNG (`getrandom`/OS equivalent available to the implementation).
   Never reuse an old secret for a new pairing.
2. It displays a one-time grouped base32/hex code on the local screen. The code
   is sensitive: do not copy it into logs, JSON, diagnostics, or process
   arguments.
3. The user reads/types the code into the peer's no-echo pairing prompt. If the
   UI cannot guarantee no-echo local entry, the operation aborts.
4. Both sides display the candidate `peer_id`/opaque node ID and require an
   explicit "pair this device" confirmation.
5. A provisional record is stored locally as `Pending` with a short expiry
   (suggest: 5 minutes). It is not trusted yet and cannot accept a worker
   session.

### 4.3 Pairing authentication and completion

The existing `PairingRegistry` remains the pre-handshake authorization/epoch
check:

- exchange a fresh 32-byte challenge nonce and monotonically increasing epoch;
- exchange the keyed-BLAKE3 challenge proof;
- reject missing opt-in/trust, invalid proof, stale epoch and reused nonce;
- bind the peer's X25519 static public key into the keyed proof transcript and
  the session-key KDF (domain-separated V2 transcript);
- derive `session_key` with the existing KDF.

The resulting 32-byte `session_key` is passed to Noise as the PSK for this
session. It is not used directly for any data encryption or MAC. The PSK is not
logged. `PairingSession` carries it in a `SessionKey` wrapper with redacted
`Debug` and zeroize-on-drop; the Noise adapter makes one explicit copy because
the Noise builder needs a stable 32-byte buffer, and both owners zeroize.

The Noise `XXpsk3` handshake then proves possession of the secret, exchanges
fresh ephemeral keys and authenticated static public keys, and binds the
prologue. After a successful handshake:

- both sides confirm the peer's Noise static public key (see identity binding);
- the provisional record becomes `Trusted`;
- the paired static key and secret are persisted atomically;
- a short session ID and trust epoch are recorded;
- the connection is ready for worker control framing only.

Failure at any step deletes the provisional record, closes the socket, and
requires a new pairing code. No fallback to unauthenticated TCP or the old MAC
transport is allowed.

### 4.4 Peer identity binding

Each installation generates a long-term X25519 static keypair on first use and
stores the private key only in a mode-0600 file. During the first authenticated
pairing:

- the Noise handshake exchanges both static public keys;
- the keyed pairing proof authenticates the peer static public key, and the
  trust-commit path rejects a pin that does not match the proof;
- both sides record the peer's static public key fingerprint in the trust store;
- the `peer_id`/opaque node ID is included in the authenticated Noise prologue
  and in the post-handshake control payload.

On every reconnect the remote static public key returned by the Noise handshake
must exactly match the pinned key. A mismatch is a hard failure requiring
removal and fresh pairing; the UI must not offer a silent TOFU upgrade.

The prologue is domain-separated and endpoint-free, for example:

```text
FASTLINK secure-worker v1
initiator=<opaque peer id>
responder=<opaque peer id>
session_id=<32-byte id hex>
epoch=<u64>
```

Both sides construct it from the authenticated pairing/session context. Any
mismatch aborts before application framing.

## 5. Trust store and removal

### 5.1 Storage

Recommended v1 durable location:

```text
$XDG_STATE_HOME/fastlink/secure-worker/v1/     (0700)
  identity.key                                 (0600, X25519 private key)
  peers/<opaque-id-hash>.meta.json             (0600, non-secret metadata)
  peers/<opaque-id-hash>.secret                (0600, pairing secret)
```

If `XDG_STATE_HOME` is unset, use `$HOME/.local/state/fastlink/secure-worker/v1`.
No endpoint, IP, MAC, provider message, or raw discovery field is stored.
The metadata contains only opaque peer ID, pinned static-key fingerprint,
transport labels, trust state, `min_epoch`/last accepted epoch, and timestamps.
Pairing secrets are separate from metadata and are never included in JSON
export/diagnostics.

Writes use a temporary file in the same directory, `fsync`, and atomic rename;
directory/file permissions are checked and corrected to 0700/0600 before use.
A corrupt/mismatched record fails closed and requires removal/re-pair, not
automatic trust.

**Residual:** file permissions do not protect against root, a same-user
attacker, backups, or a compromised host. A future OS-keyring or encrypted
store is desirable but is not required by v1. Local secret wrappers and
plaintext buffers use `zeroize`; `snow`'s internal handshake/transport key state
is not explicitly zeroized by this module.

### 5.2 Removal and re-pairing

An explicit local action (CLI/GUI) must:

1. mark the peer `Revoked` before deleting files;
2. cancel handshakes and close every active socket/session for that peer;
3. drop the Noise transport state and zero/replace session buffers on a
   best-effort basis;
4. delete the pinned peer static key and pairing secret;
5. persist the removal atomically and increment the local trust epoch;
6. reject reconnects using the old PSK/static key forever.

Removal is local and does not require the peer to be online. A removed peer
must perform a **fresh out-of-band pairing with a new secret**. Reusing an old
pairing code, session key, Noise transcript, or pinned static key is rejected.
Reconnect never silently re-pairs.

## 6. Secure worker transport

### 6.1 Handshake

- Suite: exactly `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`.
- `snow::Builder`: fixed protocol params, local static private key, and
  `.psk(3, session_key)`. No alternative suite or fallback.
- Handshake messages are length-prefixed and capped at the Noise hard limit of
  **65,535 bytes total per message, including the 16-byte AEAD tag where
  present**. Do not describe this as a 64 KiB application payload: application
  handshake payload is smaller by handshake tokens/tag bytes. Reject any
  handshake message above 65,535 before allocation. Use one **absolute
  handshake deadline** (suggest 5 seconds) shared by all XXpsk3 messages and the
  encrypted confirmation; each read/write derives its timeout from the
  remaining time so slow-drip peers cannot reset the budget. `snow` exposes the
  same `MAXMSGLEN = 65,535` limit.
- One handshake per TCP connection. No resumption, 0-RTT, renegotiation, or
  session tickets.
- A failed handshake closes the socket; no plaintext error detail containing
  endpoints or peer secrets is returned.
- The worker accepts no application frame until the Noise transport state is
  ready and the peer's static key matches the trust pin.

### 6.2 Session and epoch

- The Noise handshake hash plus the pairing session ID produces the local
  `session_id`; the trust epoch is included in the authenticated prologue.
- The existing `PairingRegistry` nonce/epoch check is retained as admission
  control. Replayed/non-increasing epochs and reused nonces are rejected.
- After the handshake, only Noise `TransportState` send/receive keys exist.
  Noise owns directional sending/receiving nonce counters. Application code
  must not derive or share raw transport keys.
- Sessions are short-lived and connection-scoped. Reconnect creates a new
  ephemeral handshake and a new Noise state. No key/nonce reuse across sockets.

### 6.3 Bounded application framing

Noise messages have a hard maximum of **65,535 bytes including the 16-byte
AEAD tag** (Noise message format; `snow` `MAXMSGLEN = 65,535`). The secure
transport must enforce that exact limit; it must **not** use the existing
256 KiB `MAX_FRAME_WIRE_BYTES` value as a single Noise ciphertext cap.

Derived limits:

```text
MAX_NOISE_MESSAGE_BYTES        = 65_535   (ciphertext incl. AEAD tag)
MAX_TRANSPORT_PLAINTEXT_BYTES  = 65_519   (65_535 - 16-byte tag)
INNER_HEADER_BYTES             = 4        (version u8, frame_type u8, flags u16)
MAX_SINGLE_FRAME_PAYLOAD_BYTES = 65_515   (65_519 - 4-byte inner header)
CHUNK_HEADER_BYTES             = 12       (transfer_id u32, total_len u32, offset u32)
MAX_CHUNK_PAYLOAD_BYTES        = 65_503   (65_519 - 4 - 12)
MAX_LOGICAL_FRAME_BYTES        = 262_144  (existing 256 KiB upper-layer bound)
```

Use an encrypted inner application frame with:

```text
u32 ciphertext_len (little-endian; 16..=65_535 for transport messages)
ciphertext = Noise transport encrypt(inner_header || payload)
```

- Reject zero, truncated (less than the 16-byte tag), or
  `ciphertext_len > 65_535` before allocation.
- Decrypt/authenticate first; only then parse the versioned inner header:
  `version:u8`, `frame_type:u8`, `flags:u16`, `payload`.
- Simple control frames carry at most 65,515 application payload bytes, after
  the 4-byte inner header.
- Logical frames larger than 65,515 bytes (up to the existing 256 KiB
  upper-layer bound) use explicit multi-message chunking. Each chunk is a
  separate Noise transport message with the 4-byte inner header plus a 12-byte
  chunk header (`transfer_id:u32`, `total_len:u32`, `offset:u32`) and at most
  65,503 payload bytes. At the current 256 KiB logical cap, this is at most five
  Noise messages.
- Reassembly is bounded: reject `total_len > 262_144`, duplicate/overlapping/
  out-of-order/missing chunks, mismatched transfer IDs, or a chunk that exceeds
  the declared total. Close the session on any violation.
- Handshake messages are capped at the same 65,535-byte Noise message limit and
  are never application-chunked.
- Reject unknown frame versions/types/flags unless explicitly negotiated.
- Apply read/write deadlines, a maximum frame count per session and a bounded
  queue/backpressure policy. On AEAD failure, nonce exhaustion, truncation, or
  timeout, close the session immediately; do not send a plaintext diagnostic.
- No model, prompt, activation, KV/SSM state, or worker command is sent before
  authentication. The worker's first accepted application frame after the
  handshake is a control/capability frame, never model data.

`stage_auth` may remain as an application-layer MAC around activation frames
for defense in depth and epoch/hop binding, but it is **not** the transport
security boundary and must never be described as encryption. The Noise AEAD is
mandatory for every byte on the wire.

### 6.4 Nonce/session limits

Set a conservative session maximum (suggest 1,000,000 transport messages) and
terminate/re-handshake when reached. Do not implement a homegrown rekey; if
Noise `rekey` is later used, it must be a reviewed, tested state transition.
This is an unresolved performance/policy choice, not an implementation claim.

## 7. Worker lifecycle and states

Recommended explicit states:

```text
Stopped -> Starting -> Listening -> AwaitingHandshake
        -> SessionReady -> Draining -> Stopping -> Stopped
                         -> Failed (safe terminal/restartable)
                         -> Revoked (trust removed; no reuse)
```

- No discovery event, GUI launch, or local inventory refresh starts a worker.
- `start` is explicit and binds only the configured listener. The first Linux
  implementation should keep the listener loopback-only for tests; any LAN or
  Tailscale bind is a separate explicit opt-in, never `0.0.0.0`, and must be
  paired/trusted before accepting a session.
- `AwaitingHandshake` accepts only bounded handshake frames; no worker/model
  commands.
- `SessionReady` requires: paired trust, matching static-key pin, valid Noise
  handshake, valid epoch/session ID, and explicit worker start.
- `stop` cancels handshakes, closes sessions/listeners, clears transport state,
  and returns to `Stopped`.
- `remove` transitions affected peers to `Revoked`, closes their sessions and
  requires fresh pairing. It does not silently forget and reconnect.
- A failed handshake or policy rejection is reported as a safe failure class
  (timeout, authentication failed, policy rejected, revoked), with no endpoint
  or provider output.
- This design does not load a model, start inference, or establish a GPU worker.
  Tests use a CPU-only fake closure and control frames.

## 8. Endpoint and secret privacy

- The socket layer may hold a peer endpoint transiently for routing only.
  It must not be serialized, logged, persisted, displayed, or included in
  `PeerIdentity`/trust metadata.
- Pairing codes, session keys, static private keys and transport keys are never
  placed in argv, environment variables, JSON, provider messages, panic text,
  debug output, or trust metadata.
- Logs name provider/peer only by opaque ID and safe failure class; raw CLI
  output, mDNS records, Tailscale peer fields and exceptions are dropped.
- Discovery candidates remain `unpaired`/`unverified`; pairing/trust is a
  separate explicit ceremony. A Tailscale peer is not worker-ready merely
  because it appears in a tailnet.
- The trust store is keyed by opaque peer ID/hash, never IP/MAC/endpoint.

## 9. Test matrix (loopback, CPU-only, fixtures only)

All tests must use `127.0.0.1` ephemeral ports and fake CPU closures. No live
LAN/Tailscale discovery, GPU, model download/load, inference, benchmark, public
listener, or network scan.

### Pairing and trust

- OS-CSPRNG secret is 32 bytes and fresh across generations (injectable RNG in
  tests; do not assert entropy statistically).
- Pairing code accepted only via no-echo interactive path; argv/env/log capture
  contains no secret.
- Explicit user confirmation required on both sides; cancel deletes pending
  state.
- Wrong pairing code, wrong peer ID, unpaired candidate, untrusted network tier,
  reused nonce, stale epoch and replayed proof are rejected with safe classes.
- Removal closes active sockets, wipes/drops session state, persists revocation,
  rejects old static key/PSK, and accepts only a fresh pair with a new secret.
- Corrupt/over-permissive trust store fails closed; file/dir modes are
  0700/0600; atomic update survives simulated interrupted write.
- No endpoint/IP/MAC/provider message appears in trust metadata, JSON, errors or
  logs (sentinel-value tests).

### Noise handshake and transport

- Two loopback workers complete `XXpsk3`; both authenticate and can exchange a
  bounded control frame.
- Wrong PSK, wrong static key pin, wrong prologue/node ID, tampered handshake
  message, invalid suite/fallback, handshake timeout and replayed handshake
  fail closed.
- A valid challenge proof bound to static key A cannot commit key B; matching
  key+proof succeeds; tampered key or MAC fails closed.
- A slow-drip peer that stays inside each per-message timeout but exceeds the
  absolute handshake deadline fails closed within that total budget; a normal
  handshake with the same deadline succeeds.
- `SessionKey`/PSK wrappers redact debug output and zeroize on drop/explicit
  zeroization; the single Noise-adapter copy is documented.
- Handshake messages at the 65,535-byte Noise limit are accepted only if valid;
  65,536 is rejected before allocation. Application handshake payload limits
  account for handshake tokens/tag bytes.
- Capture loopback bytes: no plaintext model/prompt/activation/control payload
  is observable; only Noise ciphertext/framing.
- AEAD tamper, truncation, wrong direction, replayed ciphertext, zero length,
  invalid inner version/type and nonce-limit behavior close the session safely.
- Transport ciphertext of 65,535 bytes including its 16-byte tag is the maximum;
  65,536 is rejected before allocation. A single normal frame carries at most
  65,515 application payload bytes after the 4-byte inner header; 65,516 is
  rejected.
- A 256 KiB logical activation frame is reassembled from at most five Noise
  messages with chunk headers; duplicate, overlapping, out-of-order, missing,
  mismatched-transfer and over-total chunks are rejected. No test may construct
  a single 256 KiB Noise message/ciphertext.
- Frame read/write timeouts and cancellation terminate promptly; no thread or
  socket leak.
- Sentinel `MODEL`/`PROMPT`/`KV` bytes sent before authentication are never
  parsed, processed, or forwarded; no model load is attempted.
- Handshake/auth failure never produces `SessionReady`; no worker command is
  accepted.

### Worker lifecycle and boundary

- App launch/discovery/local refresh never starts a worker.
- Explicit start -> handshake -> ready; explicit stop closes listener/sessions.
- Remove while a session is active closes it and revokes trust; reconnect with
  the old credentials fails; fresh pairing succeeds only after a new secret.
- Public/non-loopback listening is rejected in v1 tests; loopback only.
- CPU-only fake worker tests prove no GPU/model/provider side effects.
- Existing stage auth tests remain MAC-only; transport tests verify Noise AEAD
  confidentiality/integrity and do not claim stage_auth provides encryption.

## 10. Implementation boundaries and evidence

- This document does not modify `src/`, `Cargo.toml`, `Cargo.lock`, GUI code,
  `PROGRESS.md`, or `CHECKPOINTS.md`.
- Adding `snow` (and any zeroization/key-storage support) is a separate reviewed
  code task; this design makes no dependency or audit claim.
- Existing `PairingRegistry`/`stage_auth`/loopback runtime tests are evidence
  only for their current MAC/loopback behavior, not for secure transport.
- No live remote test, benchmark, model, or inference is authorized here.

## 11. Unresolved decisions

1. **`snow` acceptance:** version pin, minimal feature set and transitive
   dependency/license review are recorded, but the project's "not formally
   audited" notice still requires security-owner acceptance before any
   production claim.
2. **Static identity lifecycle:** whether the installation static key is global
   or per-peer; rotation/reset UX; behavior when a peer reinstalls but reuses a
   pairing code (new pin must require explicit confirmation).
3. **Pairing UX:** one-time code format, TTY no-echo implementation, SAS/fingerprint
   comparison, and cancellation/timeout behaviour in GUI and headless CLI.
4. **Secret storage at rest:** 0600 files vs OS keyring; `zeroize` is now used
   for local secret wrappers, but at-rest encryption/keyring storage, backup and
   crash semantics, and root/same-user disclosure acceptance remain open.
5. **Durable replay policy:** persist last accepted epoch vs a bounded nonce
   ledger; crash consistency between challenge issuance and trust update.
6. **Transport limits:** Noise per-message cap is fixed at 65,535 bytes including
   the 16-byte tag; the logical frame cap is 256 KiB and requires the explicit
   chunking above. Confirm session message cap (1,000,000 proposed), rekey vs
   close-on-exhaustion, and backpressure policy.
7. **Endpoint refactor:** `addr` is removed from public `PeerIdentity` and
   `PairingSession`; remaining discovery/CLI endpoint migration still needs a
   separate review.
8. **Listener policy:** loopback-only first implementation vs explicit private
   bind; discovery-to-connect flow without storing addresses; Tailscale/LAN
   scope enforcement.
9. **Removal integration:** CLI/GUI surface, scheduler/placement invalidation,
   handling a revoked peer currently in a pipeline, and Windows parity.
10. **Security review/audit:** external review of the selected Noise suite,
    `snow` usage, framing, key storage and removal before any production claim.

## 12. Sources

- Noise Protocol Framework specification:
  <https://noiseprotocol.org/noise.html> (message format and 65,535-byte
  maximum message size)
- `snow` 0.10 `Builder` API:
  <https://docs.rs/snow/0.10.0/snow/struct.Builder.html>
- `snow` 0.10 crate/transport API and `MAXMSGLEN = 65,535`:
  <https://docs.rs/snow/0.10.0/snow/>
- Project constraints and claims: local `AGENTS.md`, `docs/PROTOCOL.md`,
  `docs/ROADMAP.md`, `docs/M2_DISCOVERY_CONTRACT.md`,
  `docs/MULTI_DEVICE_NORTH_STAR.md`.
