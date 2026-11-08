//! Per-session SSM and KV sync on the loopback FASTLINK server.
//!
//! A resumed session offers the content hash and sends payload bytes only when
//! the resident record is missing or the hash differs. SSM records are fixed
//! size. Attention KV is append-only: a span already stored under the same
//! tensor id is not sent again. Memory reuse deletes the layer. A version bump
//! invalidates the old ref and keeps the bytes, so the same hash re-validates
//! with zero payload. Worker restart advances the generation; committed hashes
//! still match, and only a layer whose record is gone is resent.
//!
//! KV is labeled with [`crate::qwen35_4b::kv_layer_home`] at 8K. That home
//! matches the 32K, 64K, and 128K rows for every attention layer. This module
//! does not open a GPU.

use crate::qwen35_4b::{
    is_attention_layer, is_recurrent_layer, kv_layer_home, kv_q4_0_bytes_per_token_per_head,
    ssm_state_bytes, WorkerHome, KV_HEADS,
};
use crate::server::{client_handshake, validate_connect_address};
use crate::state_cache::{PersistentStateCache, StateError, StateRef, TensorId, TensorKind};
use crate::{
    digest, read_u32, read_u64, read_u8, write_u32, write_u64, write_u8, Digest, CHUNK_BYTES,
};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub const CMD_STATE: u8 = 2;
/// Placement row used to label KV. Homes agree across the published table.
pub const KV_PLACEMENT_CONTEXT: u64 = 8_192;

const OP_SYNC_SSM: u8 = 1;
const OP_SYNC_KV: u8 = 2;
const OP_END: u8 = 3;
const OP_INVALIDATE: u8 = 4;
const OP_BUMP: u8 = 5;
const OP_RESTART: u8 = 6;
const OP_FETCH_SSM: u8 = 7;
const OP_FETCH_KV: u8 = 8;
const OP_CHECK_REF: u8 = 9;

const DECISION_REUSE: u8 = 0;
const DECISION_NEED: u8 = 1;
const DECISION_REJECT: u8 = 2;
const HOME_NONE: u8 = 255;
const HOME_LOCAL: u8 = 0;
const HOME_REMOTE: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSyncReport {
    pub payload_bytes: u64,
    pub reused: bool,
    pub version: u64,
    pub hash: Digest,
    pub worker_generation: u64,
    pub kv_home: Option<WorkerHome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefStatus {
    Valid,
    Stale,
    Invalidated,
    Missing,
}

struct Ack {
    version: u64,
    generation: u64,
    payload_bytes: u64,
    home: u8,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn state_io(err: StateError) -> io::Error {
    match err {
        StateError::Io(err) => err,
        other => invalid(other.to_string()),
    }
}

fn home_code(home: Option<WorkerHome>) -> u8 {
    match home {
        None => HOME_NONE,
        Some(WorkerHome::Local7900Xt) => HOME_LOCAL,
        Some(WorkerHome::Remote6600Xt) => HOME_REMOTE,
    }
}

fn home_from_code(code: u8) -> io::Result<Option<WorkerHome>> {
    match code {
        HOME_NONE => Ok(None),
        HOME_LOCAL => Ok(Some(WorkerHome::Local7900Xt)),
        HOME_REMOTE => Ok(Some(WorkerHome::Remote6600Xt)),
        _ => Err(invalid("unknown KV home")),
    }
}

fn kv_home_for(layer: u32) -> io::Result<WorkerHome> {
    kv_layer_home(KV_PLACEMENT_CONTEXT, layer)
        .map(|placed| placed.home)
        .map_err(|_| invalid("KV layer has no placement"))
}

fn write_ack(stream: &mut impl Write, ack: Ack) -> io::Result<()> {
    write_u64(stream, ack.version)?;
    write_u64(stream, ack.generation)?;
    write_u64(stream, ack.payload_bytes)?;
    write_u8(stream, ack.home)?;
    stream.flush()
}

fn read_ack(stream: &mut impl Read) -> io::Result<Ack> {
    Ok(Ack {
        version: read_u64(stream)?,
        generation: read_u64(stream)?,
        payload_bytes: read_u64(stream)?,
        home: read_u8(stream)?,
    })
}

fn report_from(hash: Digest, reused: bool, ack: Ack) -> io::Result<StateSyncReport> {
    Ok(StateSyncReport {
        payload_bytes: ack.payload_bytes,
        reused,
        version: ack.version,
        hash,
        worker_generation: ack.generation,
        kv_home: home_from_code(ack.home)?,
    })
}

pub fn handle_state_session(
    stream: &mut (impl Read + Write),
    cache: &PersistentStateCache,
) -> io::Result<()> {
    match read_u8(stream)? {
        OP_SYNC_SSM => handle_sync_ssm(stream, cache),
        OP_SYNC_KV => handle_sync_kv(stream, cache),
        OP_END => {
            let session = read_u64(stream)?;
            if let Err(err) = cache.end_session(session) {
                write_u8(stream, DECISION_REJECT)?;
                stream.flush()?;
                return Err(state_io(err));
            }
            write_u8(stream, DECISION_REUSE)?;
            stream.flush()
        }
        OP_INVALIDATE => {
            let session = read_u64(stream)?;
            let layer = read_u32(stream)?;
            let count = match cache.invalidate_layer(session, layer) {
                Ok(count) => count,
                Err(err) => {
                    write_u8(stream, DECISION_REJECT)?;
                    stream.flush()?;
                    return Err(state_io(err));
                }
            };
            write_u8(stream, DECISION_REUSE)?;
            write_u32(stream, count)?;
            stream.flush()
        }
        OP_BUMP => {
            let session = read_u64(stream)?;
            let layer = read_u32(stream)?;
            let refs = match cache.bump_layer_version(session, layer) {
                Ok(refs) => refs,
                Err(err) => {
                    write_u8(stream, DECISION_REJECT)?;
                    stream.flush()?;
                    return Err(state_io(err));
                }
            };
            let generation = refs.first().map(|item| item.worker_generation).unwrap_or(0);
            let count = u32::try_from(refs.len()).map_err(|_| invalid("bump count overflow"))?;
            write_u8(stream, DECISION_REUSE)?;
            write_u64(stream, generation)?;
            write_u32(stream, count)?;
            stream.flush()
        }
        OP_RESTART => {
            let generation = cache.note_worker_restart().map_err(state_io)?;
            write_u64(stream, generation)?;
            stream.flush()
        }
        OP_FETCH_SSM => handle_fetch_ssm(stream, cache),
        OP_FETCH_KV => handle_fetch_kv(stream, cache),
        OP_CHECK_REF => handle_check_ref(stream, cache),
        _ => Err(invalid("unknown state op")),
    }
}

fn handle_sync_ssm(
    stream: &mut (impl Read + Write),
    cache: &PersistentStateCache,
) -> io::Result<()> {
    let session = read_u64(stream)?;
    let layer = read_u32(stream)?;
    let mut hash = [0u8; 32];
    stream.read_exact(&mut hash)?;
    if !is_recurrent_layer(layer) {
        write_u8(stream, DECISION_REJECT)?;
        stream.flush()?;
        return Err(invalid("SSM sync on a non-recurrent layer"));
    }
    match cache.lookup_ssm(session, layer) {
        Ok(reference) if reference.hash == hash => {
            write_u8(stream, DECISION_REUSE)?;
            write_ack(
                stream,
                Ack {
                    version: reference.version,
                    generation: reference.worker_generation,
                    payload_bytes: 0,
                    home: HOME_NONE,
                },
            )
        }
        Ok(_) | Err(StateError::NotFound) | Err(StateError::SessionClosed) => {
            write_u8(stream, DECISION_NEED)?;
            stream.flush()?;
            let len = ssm_state_bytes();
            let mut bytes = vec![0u8; len];
            stream.read_exact(&mut bytes)?;
            if digest(&bytes) != hash {
                write_u8(stream, DECISION_REJECT)?;
                stream.flush()?;
                return Err(invalid("SSM payload hash mismatch"));
            }
            let stored = cache.put_ssm(session, layer, &bytes).map_err(state_io)?;
            write_u8(stream, DECISION_REUSE)?;
            write_ack(
                stream,
                Ack {
                    version: stored.version,
                    generation: stored.worker_generation,
                    payload_bytes: len as u64,
                    home: HOME_NONE,
                },
            )
        }
        Err(err) => Err(state_io(err)),
    }
}

fn handle_sync_kv(
    stream: &mut (impl Read + Write),
    cache: &PersistentStateCache,
) -> io::Result<()> {
    let session = read_u64(stream)?;
    let layer = read_u32(stream)?;
    let head = read_u32(stream)?;
    let seq_start = read_u64(stream)?;
    let mut hash = [0u8; 32];
    stream.read_exact(&mut hash)?;
    let len = read_u32(stream)? as usize;
    let stride = kv_q4_0_bytes_per_token_per_head();
    let home = match kv_home_for(layer) {
        Ok(home)
            if is_attention_layer(layer)
                && head < KV_HEADS
                && len > 0
                && len <= CHUNK_BYTES
                && len.is_multiple_of(stride) =>
        {
            home
        }
        _ => {
            write_u8(stream, DECISION_REJECT)?;
            stream.flush()?;
            return Err(invalid("KV offer is outside the placement or size cap"));
        }
    };
    let tokens = (len / stride) as u64;
    let Some(seq_end) = seq_start.checked_add(tokens) else {
        write_u8(stream, DECISION_REJECT)?;
        stream.flush()?;
        return Err(invalid("KV sequence overflow"));
    };
    let id = TensorId {
        kind: TensorKind::AttentionKv,
        layer,
        head,
        seq_start,
        seq_end,
    };
    match cache.lookup_tensor(session, id) {
        Ok(reference) if reference.hash == hash => {
            write_u8(stream, DECISION_REUSE)?;
            write_ack(
                stream,
                Ack {
                    version: reference.version,
                    generation: reference.worker_generation,
                    payload_bytes: 0,
                    home: home_code(Some(home)),
                },
            )
        }
        Ok(_) => {
            write_u8(stream, DECISION_REJECT)?;
            stream.flush()?;
            Err(invalid("resident KV span has a different hash"))
        }
        Err(StateError::NotFound) | Err(StateError::SessionClosed) => {
            write_u8(stream, DECISION_NEED)?;
            stream.flush()?;
            let mut bytes = vec![0u8; len];
            stream.read_exact(&mut bytes)?;
            if digest(&bytes) != hash {
                write_u8(stream, DECISION_REJECT)?;
                stream.flush()?;
                return Err(invalid("KV payload hash mismatch"));
            }
            let stored = match cache.append_kv(session, layer, head, seq_start, &bytes) {
                Ok(stored) => stored,
                Err(err) => {
                    write_u8(stream, DECISION_REJECT)?;
                    stream.flush()?;
                    return Err(state_io(err));
                }
            };
            write_u8(stream, DECISION_REUSE)?;
            write_ack(
                stream,
                Ack {
                    version: stored.version,
                    generation: stored.worker_generation,
                    payload_bytes: len as u64,
                    home: home_code(Some(home)),
                },
            )
        }
        Err(err) => Err(state_io(err)),
    }
}

fn handle_fetch_ssm(
    stream: &mut (impl Read + Write),
    cache: &PersistentStateCache,
) -> io::Result<()> {
    let session = read_u64(stream)?;
    let layer = read_u32(stream)?;
    let reference = match cache.lookup_ssm(session, layer) {
        Ok(reference) => reference,
        Err(_) => {
            write_u8(stream, DECISION_REJECT)?;
            return stream.flush();
        }
    };
    let bytes = cache.get(&reference).map_err(state_io)?;
    write_u8(stream, DECISION_REUSE)?;
    write_u32(stream, bytes.len() as u32)?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn handle_fetch_kv(
    stream: &mut (impl Read + Write),
    cache: &PersistentStateCache,
) -> io::Result<()> {
    let session = read_u64(stream)?;
    let layer = read_u32(stream)?;
    let head = read_u32(stream)?;
    let seq_start = read_u64(stream)?;
    let seq_end = read_u64(stream)?;
    let id = TensorId {
        kind: TensorKind::AttentionKv,
        layer,
        head,
        seq_start,
        seq_end,
    };
    let reference = match cache.lookup_tensor(session, id) {
        Ok(reference) => reference,
        Err(_) => {
            write_u8(stream, DECISION_REJECT)?;
            return stream.flush();
        }
    };
    let bytes = cache.get(&reference).map_err(state_io)?;
    write_u8(stream, DECISION_REUSE)?;
    write_u32(stream, bytes.len() as u32)?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn handle_check_ref(
    stream: &mut (impl Read + Write),
    cache: &PersistentStateCache,
) -> io::Result<()> {
    let reference = read_state_ref(stream)?;
    let status = match cache.get(&reference) {
        Ok(_) => 0u8,
        Err(StateError::StaleReference) => 1,
        Err(StateError::Invalidated) => 2,
        Err(StateError::NotFound) | Err(StateError::SessionClosed) => 3,
        Err(err) => return Err(state_io(err)),
    };
    write_u8(stream, status)?;
    stream.flush()
}

fn read_state_ref(stream: &mut impl Read) -> io::Result<StateRef> {
    let session = read_u64(stream)?;
    let kind = match read_u8(stream)? {
        0 => TensorKind::Ssm,
        1 => TensorKind::AttentionKv,
        _ => return Err(invalid("unknown tensor kind")),
    };
    let layer = read_u32(stream)?;
    let head = read_u32(stream)?;
    let seq_start = read_u64(stream)?;
    let seq_end = read_u64(stream)?;
    let version = read_u64(stream)?;
    let mut hash = [0u8; 32];
    stream.read_exact(&mut hash)?;
    let worker_generation = read_u64(stream)?;
    Ok(StateRef {
        session,
        id: TensorId {
            kind,
            layer,
            head,
            seq_start,
            seq_end,
        },
        version,
        hash,
        worker_generation,
    })
}

fn write_state_ref(stream: &mut impl Write, reference: &StateRef) -> io::Result<()> {
    write_u64(stream, reference.session)?;
    let kind = match reference.id.kind {
        TensorKind::Ssm => 0u8,
        TensorKind::AttentionKv => 1,
    };
    write_u8(stream, kind)?;
    write_u32(stream, reference.id.layer)?;
    write_u32(stream, reference.id.head)?;
    write_u64(stream, reference.id.seq_start)?;
    write_u64(stream, reference.id.seq_end)?;
    write_u64(stream, reference.version)?;
    stream.write_all(&reference.hash)?;
    write_u64(stream, reference.worker_generation)
}

fn connect(server: &str) -> io::Result<TcpStream> {
    validate_connect_address(server)?;
    let mut stream = TcpStream::connect(server)?;
    let timeout = Duration::from_secs(30);
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    client_handshake(&mut stream)?;
    Ok(stream)
}

fn finish_offer(
    stream: &mut TcpStream,
    hash: Digest,
    payload: Option<&[u8]>,
) -> io::Result<StateSyncReport> {
    match read_u8(stream)? {
        DECISION_REUSE => {
            let ack = read_ack(stream)?;
            report_from(hash, true, ack)
        }
        DECISION_NEED => {
            let bytes = payload.ok_or_else(|| invalid("server requested a missing payload"))?;
            stream.write_all(bytes)?;
            stream.flush()?;
            if read_u8(stream)? != DECISION_REUSE {
                return Err(invalid("server rejected the state payload"));
            }
            let ack = read_ack(stream)?;
            report_from(hash, false, ack)
        }
        _ => Err(invalid("server rejected the state offer")),
    }
}

pub fn sync_ssm(
    server: &str,
    session: u64,
    layer: u32,
    bytes: &[u8],
) -> io::Result<StateSyncReport> {
    if bytes.len() != ssm_state_bytes() {
        return Err(invalid("SSM payload is not the fixed state size"));
    }
    let mut stream = connect(server)?;
    write_u8(&mut stream, CMD_STATE)?;
    write_u8(&mut stream, OP_SYNC_SSM)?;
    write_u64(&mut stream, session)?;
    write_u32(&mut stream, layer)?;
    let hash = digest(bytes);
    stream.write_all(&hash)?;
    stream.flush()?;
    finish_offer(&mut stream, hash, Some(bytes))
}

pub fn sync_kv(
    server: &str,
    session: u64,
    layer: u32,
    head: u32,
    seq_start: u64,
    bytes: &[u8],
) -> io::Result<StateSyncReport> {
    if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
        return Err(invalid("KV payload is outside the append cap"));
    }
    let mut stream = connect(server)?;
    write_u8(&mut stream, CMD_STATE)?;
    write_u8(&mut stream, OP_SYNC_KV)?;
    write_u64(&mut stream, session)?;
    write_u32(&mut stream, layer)?;
    write_u32(&mut stream, head)?;
    write_u64(&mut stream, seq_start)?;
    let hash = digest(bytes);
    stream.write_all(&hash)?;
    write_u32(&mut stream, bytes.len() as u32)?;
    stream.flush()?;
    finish_offer(&mut stream, hash, Some(bytes))
}

pub fn end_state_session(server: &str, session: u64) -> io::Result<()> {
    let mut stream = connect(server)?;
    write_u8(&mut stream, CMD_STATE)?;
    write_u8(&mut stream, OP_END)?;
    write_u64(&mut stream, session)?;
    stream.flush()?;
    if read_u8(&mut stream)? != DECISION_REUSE {
        return Err(invalid("server rejected session end"));
    }
    Ok(())
}

pub fn invalidate_state_layer(server: &str, session: u64, layer: u32) -> io::Result<u32> {
    let mut stream = connect(server)?;
    write_u8(&mut stream, CMD_STATE)?;
    write_u8(&mut stream, OP_INVALIDATE)?;
    write_u64(&mut stream, session)?;
    write_u32(&mut stream, layer)?;
    stream.flush()?;
    if read_u8(&mut stream)? != DECISION_REUSE {
        return Err(invalid("server rejected layer invalidation"));
    }
    read_u32(&mut stream)
}

pub fn bump_state_layer(server: &str, session: u64, layer: u32) -> io::Result<u64> {
    let mut stream = connect(server)?;
    write_u8(&mut stream, CMD_STATE)?;
    write_u8(&mut stream, OP_BUMP)?;
    write_u64(&mut stream, session)?;
    write_u32(&mut stream, layer)?;
    stream.flush()?;
    if read_u8(&mut stream)? != DECISION_REUSE {
        return Err(invalid("server rejected version bump"));
    }
    let generation = read_u64(&mut stream)?;
    let _count = read_u32(&mut stream)?;
    Ok(generation)
}

pub fn restart_state_worker(server: &str) -> io::Result<u64> {
    let mut stream = connect(server)?;
    write_u8(&mut stream, CMD_STATE)?;
    write_u8(&mut stream, OP_RESTART)?;
    stream.flush()?;
    read_u64(&mut stream)
}

pub fn fetch_ssm(server: &str, session: u64, layer: u32) -> io::Result<Vec<u8>> {
    let mut stream = connect(server)?;
    write_u8(&mut stream, CMD_STATE)?;
    write_u8(&mut stream, OP_FETCH_SSM)?;
    write_u64(&mut stream, session)?;
    write_u32(&mut stream, layer)?;
    stream.flush()?;
    read_payload(&mut stream, ssm_state_bytes())
}

pub fn fetch_kv(
    server: &str,
    session: u64,
    layer: u32,
    head: u32,
    seq_start: u64,
    seq_end: u64,
) -> io::Result<Vec<u8>> {
    let mut stream = connect(server)?;
    write_u8(&mut stream, CMD_STATE)?;
    write_u8(&mut stream, OP_FETCH_KV)?;
    write_u64(&mut stream, session)?;
    write_u32(&mut stream, layer)?;
    write_u32(&mut stream, head)?;
    write_u64(&mut stream, seq_start)?;
    write_u64(&mut stream, seq_end)?;
    stream.flush()?;
    read_payload(&mut stream, CHUNK_BYTES)
}

fn read_payload(stream: &mut impl Read, cap: usize) -> io::Result<Vec<u8>> {
    if read_u8(stream)? != DECISION_REUSE {
        return Err(invalid("state payload is not resident"));
    }
    let len = read_u32(stream)? as usize;
    if len > cap {
        return Err(invalid("state payload exceeds its cap"));
    }
    let mut bytes = vec![0u8; len];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub fn check_state_ref(server: &str, reference: &StateRef) -> io::Result<RefStatus> {
    let mut stream = connect(server)?;
    write_u8(&mut stream, CMD_STATE)?;
    write_u8(&mut stream, OP_CHECK_REF)?;
    write_state_ref(&mut stream, reference)?;
    stream.flush()?;
    Ok(match read_u8(&mut stream)? {
        0 => RefStatus::Valid,
        1 => RefStatus::Stale,
        2 => RefStatus::Invalidated,
        3 => RefStatus::Missing,
        _ => return Err(invalid("unknown ref status")),
    })
}
