//! Persistent SSM state and append-only attention KV, isolated per session.
//!
//! SSM layers keep one fixed-size record (`ssm_state_bytes`, f32 state, no
//! conv term). Attention layers append q4_0 KV spans. Records carry a tensor
//! id `(kind, layer, head, seq range)`, a content hash, a version, and the
//! owning session.
//!
//! A later `open` of an existing root is a worker restart: committed records
//! are reloaded after their bytes and hashes are checked, and the worker
//! generation advances so refs from the previous open are stale. Uncommitted
//! `.partial-*` files are removed and never promoted, same as
//! `recover_atomic_writes`. Reads reject a declared length above the kind's
//! cap before allocating, same as bounded decode.
//!
//! This module does not open a socket and does not use a GPU.

use crate::qwen35_4b::{
    is_attention_layer, is_recurrent_layer, kv_q4_0_bytes_per_token_per_head, ssm_state_bytes,
    BLOCK_COUNT, CONTEXT_LENGTH, KV_HEADS,
};
use crate::{digest, Digest, CHUNK_BYTES};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MANIFEST_MAGIC: &[u8; 4] = b"FLST";
const MANIFEST_VERSION: u16 = 1;
const MAX_RECORDS: u32 = 24 + 8 * 4 * 262_144;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TensorKind {
    Ssm = 0,
    AttentionKv = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TensorId {
    pub kind: TensorKind,
    pub layer: u32,
    pub head: u32,
    pub seq_start: u64,
    pub seq_end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StateRef {
    pub session: u64,
    pub id: TensorId,
    pub version: u64,
    pub hash: Digest,
    pub worker_generation: u64,
}

#[derive(Debug)]
pub enum StateError {
    Io(io::Error),
    Invalidated,
    StaleReference,
    SessionClosed,
    NotFound,
    Bounded,
    NotContiguous,
    WrongLayerKind,
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Invalidated => write!(f, "state record is invalid"),
            Self::StaleReference => {
                write!(f, "state reference is from a previous worker generation")
            }
            Self::SessionClosed => write!(f, "session is closed"),
            Self::NotFound => write!(f, "state record was not found"),
            Self::Bounded => write!(f, "state payload exceeds its cap"),
            Self::NotContiguous => write!(f, "KV append is not contiguous"),
            Self::WrongLayerKind => write!(f, "tensor kind does not match the layer"),
        }
    }
}

impl std::error::Error for StateError {}

impl From<io::Error> for StateError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

#[derive(Clone)]
struct Stored {
    id: TensorId,
    version: u64,
    hash: Digest,
    byte_len: u64,
}

#[derive(Clone, Copy)]
struct InvalidKey {
    id: TensorId,
    version: u64,
}

struct Session {
    records: Vec<Stored>,
    invalidated: Vec<InvalidKey>,
}

struct Inner {
    generation: u64,
    sessions: BTreeMap<u64, Session>,
}

pub struct PersistentStateCache {
    root: PathBuf,
    inner: Mutex<Inner>,
}

impl PersistentStateCache {
    /// Load `root`, discard uncommitted partials, and check committed hashes.
    ///
    /// The first open writes worker generation 1. A later open advances the
    /// generation. Committed SSM and KV bytes stay readable through a new
    /// lookup. Refs captured before that open return [`StateError::StaleReference`].
    pub fn open(root: &Path) -> Result<Self, StateError> {
        ensure_not_symlink(root)?;
        fs::create_dir_all(root)?;
        discard_partials(root)?;
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir)?;
        discard_partials(&sessions_dir)?;
        let mut sessions = BTreeMap::new();
        for entry in fs::read_dir(&sessions_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let text = name.to_string_lossy();
            if text.starts_with(".partial") {
                continue;
            }
            let session =
                u64::from_str_radix(&text, 16).map_err(|_| invalid("bad session name"))?;
            if format!("{session:016x}") != text {
                return Err(invalid("bad session name"));
            }
            let session_dir = entry.path();
            ensure_not_symlink(&session_dir)?;
            discard_partials(&session_dir)?;
            let tensors = session_dir.join("tensors");
            if tensors.exists() {
                discard_partials(&tensors)?;
            }
            let mut loaded = read_manifest(&session_dir)?;
            verify_records(&session_dir, &mut loaded)?;
            delete_orphans(&session_dir, &loaded)?;
            write_manifest(&session_dir, &loaded)?;
            sessions.insert(session, loaded);
        }
        let generation_path = root.join("worker-generation");
        let generation = if generation_path.exists() {
            let previous = read_generation(&generation_path)?;
            previous
                .checked_add(1)
                .ok_or_else(|| invalid("worker generation overflow"))?
        } else {
            1
        };
        write_generation(&generation_path, generation)?;
        Ok(Self {
            root: root.to_path_buf(),
            inner: Mutex::new(Inner {
                generation,
                sessions,
            }),
        })
    }

    pub fn worker_generation(&self) -> Result<u64, StateError> {
        Ok(self.lock()?.generation)
    }

    /// Replace the fixed SSM record for `layer`. The previous version is invalidated.
    pub fn put_ssm(&self, session: u64, layer: u32, bytes: &[u8]) -> Result<StateRef, StateError> {
        if !is_recurrent_layer(layer) {
            return Err(StateError::WrongLayerKind);
        }
        if bytes.len() != ssm_state_bytes() {
            return Err(StateError::Bounded);
        }
        let id = TensorId {
            kind: TensorKind::Ssm,
            layer,
            head: 0,
            seq_start: 0,
            seq_end: 0,
        };
        self.store_new(session, id, bytes, true)
    }

    /// Append a contiguous q4_0 KV span. `seq_start` must equal the valid prefix end.
    pub fn append_kv(
        &self,
        session: u64,
        layer: u32,
        head: u32,
        seq_start: u64,
        bytes: &[u8],
    ) -> Result<StateRef, StateError> {
        if !is_attention_layer(layer) || layer >= BLOCK_COUNT {
            return Err(StateError::WrongLayerKind);
        }
        if head >= KV_HEADS {
            return Err(StateError::Bounded);
        }
        let stride = kv_q4_0_bytes_per_token_per_head();
        if bytes.is_empty() || bytes.len() > CHUNK_BYTES || !bytes.len().is_multiple_of(stride) {
            return Err(StateError::Bounded);
        }
        let tokens = (bytes.len() / stride) as u64;
        let seq_end = seq_start.checked_add(tokens).ok_or(StateError::Bounded)?;
        if seq_end > CONTEXT_LENGTH {
            return Err(StateError::Bounded);
        }
        let id = TensorId {
            kind: TensorKind::AttentionKv,
            layer,
            head,
            seq_start,
            seq_end,
        };
        self.store_new(session, id, bytes, false)
    }

    pub fn get(&self, reference: &StateRef) -> Result<Vec<u8>, StateError> {
        let guard = self.lock()?;
        let session = guard
            .sessions
            .get(&reference.session)
            .ok_or(StateError::SessionClosed)?;
        if reference.worker_generation != guard.generation {
            return Err(StateError::StaleReference);
        }
        if session
            .invalidated
            .iter()
            .any(|key| key.version == reference.version && key.id == reference.id)
        {
            return Err(StateError::Invalidated);
        }
        let record = session
            .records
            .iter()
            .find(|record| {
                record.version == reference.version
                    && record.id == reference.id
                    && record.hash == reference.hash
            })
            .ok_or(StateError::NotFound)?;
        let bytes = read_capped(
            &self.tensor_path(reference.session, record),
            record.byte_len,
            cap_for(record.id.kind),
        )?;
        if digest(&bytes) != reference.hash {
            return Err(StateError::Invalidated);
        }
        Ok(bytes)
    }

    pub fn lookup_ssm(&self, session: u64, layer: u32) -> Result<StateRef, StateError> {
        let guard = self.lock()?;
        let stored = guard
            .sessions
            .get(&session)
            .ok_or(StateError::SessionClosed)?;
        let record = stored
            .records
            .iter()
            .find(|record| record.id.kind == TensorKind::Ssm && record.id.layer == layer)
            .ok_or(StateError::NotFound)?;
        Ok(reference_for(session, guard.generation, record))
    }

    /// Invalidate a live record and delete its bytes so the slot can be reused.
    pub fn invalidate_memory_reuse(&self, reference: &StateRef) -> Result<(), StateError> {
        let mut guard = self.lock()?;
        if reference.worker_generation != guard.generation {
            return Err(StateError::StaleReference);
        }
        if !guard.sessions.contains_key(&reference.session) {
            return Err(StateError::SessionClosed);
        }
        let path = self.tensor_path(
            reference.session,
            &Stored {
                id: reference.id,
                version: reference.version,
                hash: reference.hash,
                byte_len: 0,
            },
        );
        {
            let session = guard
                .sessions
                .get_mut(&reference.session)
                .ok_or(StateError::SessionClosed)?;
            let position = session.records.iter().position(|record| {
                record.version == reference.version
                    && record.id == reference.id
                    && record.hash == reference.hash
            });
            let Some(position) = position else {
                if session
                    .invalidated
                    .iter()
                    .any(|key| key.version == reference.version && key.id == reference.id)
                {
                    return Err(StateError::Invalidated);
                }
                return Err(StateError::NotFound);
            };
            session.records.remove(position);
            session.invalidated.push(InvalidKey {
                id: reference.id,
                version: reference.version,
            });
        }
        let session = guard
            .sessions
            .get(&reference.session)
            .ok_or(StateError::SessionClosed)?;
        let dir = self.session_dir(reference.session);
        write_manifest(&dir, session)?;
        if path.exists() {
            ensure_not_symlink(&path)?;
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Advance the worker generation in this process. Committed bytes stay.
    pub fn note_worker_restart(&self) -> Result<u64, StateError> {
        let mut guard = self.lock()?;
        let generation = guard
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("worker generation overflow"))?;
        guard.generation = generation;
        write_generation(&self.root.join("worker-generation"), generation)?;
        Ok(generation)
    }

    pub fn end_session(&self, session: u64) -> Result<(), StateError> {
        let mut guard = self.lock()?;
        if guard.sessions.remove(&session).is_none() {
            return Err(StateError::SessionClosed);
        }
        let dir = self.session_dir(session);
        if dir.exists() {
            ensure_not_symlink(&dir)?;
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    fn store_new(
        &self,
        session: u64,
        id: TensorId,
        bytes: &[u8],
        replace_same_id: bool,
    ) -> Result<StateRef, StateError> {
        let hash = digest(bytes);
        let mut guard = self.lock()?;
        self.ensure_session(&mut guard, session)?;
        if !replace_same_id && id.kind == TensorKind::AttentionKv {
            let stored = guard
                .sessions
                .get(&session)
                .ok_or(StateError::SessionClosed)?;
            let prefix = prefix_end(stored, id.layer, id.head)?;
            if id.seq_start != prefix {
                return Err(StateError::NotContiguous);
            }
        }
        let generation = guard.generation;
        let version = {
            let stored = guard
                .sessions
                .get(&session)
                .ok_or(StateError::SessionClosed)?;
            let previous_record = stored
                .records
                .iter()
                .filter(|record| {
                    record.id.kind == id.kind
                        && record.id.layer == id.layer
                        && record.id.head == id.head
                })
                .map(|record| record.version);
            let previous_invalid = stored
                .invalidated
                .iter()
                .filter(|key| {
                    key.id.kind == id.kind && key.id.layer == id.layer && key.id.head == id.head
                })
                .map(|key| key.version);
            let previous = previous_record.chain(previous_invalid).max().unwrap_or(0);
            previous
                .checked_add(1)
                .ok_or_else(|| invalid("version overflow"))?
        };
        let record = Stored {
            id,
            version,
            hash,
            byte_len: bytes.len() as u64,
        };
        let dir = self.session_dir(session);
        fs::create_dir_all(dir.join("tensors"))?;
        atomic_write(&dir.join("tensors"), &tensor_file_name(&record), bytes)?;
        let retired = {
            let stored = guard
                .sessions
                .get_mut(&session)
                .ok_or(StateError::SessionClosed)?;
            let mut retired = Vec::new();
            if replace_same_id {
                let mut kept = Vec::new();
                for existing in stored.records.drain(..) {
                    if existing.id == id {
                        stored.invalidated.push(InvalidKey {
                            id: existing.id,
                            version: existing.version,
                        });
                        retired.push(existing);
                    } else {
                        kept.push(existing);
                    }
                }
                stored.records = kept;
            }
            stored.records.push(record.clone());
            write_manifest(&dir, stored)?;
            retired
        };
        for existing in &retired {
            let path = dir.join("tensors").join(tensor_file_name(existing));
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(reference_for(session, generation, &record))
    }

    fn ensure_session(&self, guard: &mut Inner, session: u64) -> Result<(), StateError> {
        if guard.sessions.contains_key(&session) {
            return Ok(());
        }
        let dir = self.session_dir(session);
        fs::create_dir_all(dir.join("tensors"))?;
        let session_state = Session {
            records: Vec::new(),
            invalidated: Vec::new(),
        };
        write_manifest(&dir, &session_state)?;
        guard.sessions.insert(session, session_state);
        Ok(())
    }

    fn session_dir(&self, session: u64) -> PathBuf {
        self.root.join("sessions").join(format!("{session:016x}"))
    }

    fn tensor_path(&self, session: u64, record: &Stored) -> PathBuf {
        self.session_dir(session)
            .join("tensors")
            .join(tensor_file_name(record))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, StateError> {
        self.inner
            .lock()
            .map_err(|_| invalid("state cache lock poisoned"))
    }
}

fn reference_for(session: u64, generation: u64, record: &Stored) -> StateRef {
    StateRef {
        session,
        id: record.id,
        version: record.version,
        hash: record.hash,
        worker_generation: generation,
    }
}

fn cap_for(kind: TensorKind) -> u64 {
    match kind {
        TensorKind::Ssm => ssm_state_bytes() as u64,
        TensorKind::AttentionKv => CHUNK_BYTES as u64,
    }
}

fn prefix_end(session: &Session, layer: u32, head: u32) -> Result<u64, StateError> {
    let mut spans: Vec<(u64, u64)> = session
        .records
        .iter()
        .filter(|record| {
            record.id.kind == TensorKind::AttentionKv
                && record.id.layer == layer
                && record.id.head == head
        })
        .map(|record| (record.id.seq_start, record.id.seq_end))
        .collect();
    spans.sort_unstable();
    let mut end = 0u64;
    for (start, stop) in spans {
        if start != end || stop < start {
            return Err(StateError::Invalidated);
        }
        end = stop;
    }
    Ok(end)
}

fn tensor_file_name(record: &Stored) -> String {
    let kind = match record.id.kind {
        TensorKind::Ssm => 0,
        TensorKind::AttentionKv => 1,
    };
    format!(
        "k{kind}-l{}-h{}-s{}-e{}-v{}.bin",
        record.id.layer, record.id.head, record.id.seq_start, record.id.seq_end, record.version
    )
}

fn invalid(message: &'static str) -> StateError {
    StateError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn ensure_not_symlink(path: &Path) -> Result<(), StateError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(invalid("refusing to follow a symlink")),
        _ => Ok(()),
    }
}

fn discard_partials(dir: &Path) -> Result<(), StateError> {
    ensure_not_symlink(dir)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(".partial") {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            return Err(invalid("uncommitted state path is a directory"));
        }
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

fn atomic_write(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), StateError> {
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".partial-{name}"));
    let final_path = dir.join(name);
    let write_res = (|| -> Result<(), StateError> {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    fs::rename(&tmp, final_path)?;
    Ok(())
}

fn read_generation(path: &Path) -> Result<u64, StateError> {
    let bytes = read_capped(path, 8, 8)?;
    let mut wide = [0u8; 8];
    wide.copy_from_slice(&bytes);
    Ok(u64::from_le_bytes(wide))
}

fn write_generation(path: &Path, generation: u64) -> Result<(), StateError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("missing generation parent"))?;
    atomic_write(parent, "worker-generation", &generation.to_le_bytes())
}

fn read_capped(path: &Path, len: u64, cap: u64) -> Result<Vec<u8>, StateError> {
    if len > cap {
        return Err(StateError::Bounded);
    }
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() || meta.len() != len {
        return Err(StateError::Bounded);
    }
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn verify_records(dir: &Path, session: &mut Session) -> Result<(), StateError> {
    let mut keep = Vec::new();
    for record in session.records.drain(..) {
        let path = dir.join("tensors").join(tensor_file_name(&record));
        let ok = match read_capped(&path, record.byte_len, cap_for(record.id.kind)) {
            Ok(bytes) => digest(&bytes) == record.hash,
            Err(_) => false,
        };
        if ok {
            keep.push(record);
        } else {
            session.invalidated.push(InvalidKey {
                id: record.id,
                version: record.version,
            });
            if path.exists()
                && fs::symlink_metadata(&path)
                    .map(|meta| meta.file_type().is_file())
                    .unwrap_or(false)
            {
                let _ = fs::remove_file(path);
            }
        }
    }
    session.records = keep;
    Ok(())
}

fn delete_orphans(dir: &Path, session: &Session) -> Result<(), StateError> {
    let tensors = dir.join("tensors");
    if !tensors.exists() {
        return Ok(());
    }
    ensure_not_symlink(&tensors)?;
    let live: Vec<String> = session.records.iter().map(tensor_file_name).collect();
    for entry in fs::read_dir(&tensors)? {
        let entry = entry?;
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text.starts_with(".partial") || live.iter().any(|live_name| live_name == text.as_ref()) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            return Err(invalid("tensor path is a directory"));
        }
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

fn write_manifest(dir: &Path, session: &Session) -> Result<(), StateError> {
    if session.records.len() > MAX_RECORDS as usize
        || session.invalidated.len() > MAX_RECORDS as usize
    {
        return Err(StateError::Bounded);
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MANIFEST_MAGIC);
    bytes.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    push_u32(&mut bytes, session.records.len() as u32);
    for record in &session.records {
        push_record(&mut bytes, record);
    }
    push_u32(&mut bytes, session.invalidated.len() as u32);
    for key in &session.invalidated {
        push_id(&mut bytes, key.id);
        bytes.extend_from_slice(&key.version.to_le_bytes());
    }
    atomic_write(dir, "manifest.bin", &bytes)
}

fn read_manifest(dir: &Path) -> Result<Session, StateError> {
    let path = dir.join("manifest.bin");
    if !path.exists() {
        return Ok(Session {
            records: Vec::new(),
            invalidated: Vec::new(),
        });
    }
    let meta = fs::symlink_metadata(&path)?;
    if !meta.file_type().is_file() || meta.len() > 64 * 1024 * 1024 {
        return Err(StateError::Bounded);
    }
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut cursor = bytes.as_slice();
    let mut magic = [0u8; 4];
    read_slice(&mut cursor, &mut magic)?;
    if &magic != MANIFEST_MAGIC || read_u16(&mut cursor)? != MANIFEST_VERSION {
        return Err(invalid("bad state manifest"));
    }
    let record_count = read_u32(&mut cursor)?;
    if record_count > MAX_RECORDS {
        return Err(StateError::Bounded);
    }
    let mut records = Vec::with_capacity(record_count as usize);
    for _ in 0..record_count {
        records.push(read_record(&mut cursor)?);
    }
    let invalid_count = read_u32(&mut cursor)?;
    if invalid_count > MAX_RECORDS {
        return Err(StateError::Bounded);
    }
    let mut invalidated = Vec::with_capacity(invalid_count as usize);
    for _ in 0..invalid_count {
        invalidated.push(InvalidKey {
            id: read_id(&mut cursor)?,
            version: read_u64(&mut cursor)?,
        });
    }
    if !cursor.is_empty() {
        return Err(invalid("trailing manifest bytes"));
    }
    Ok(Session {
        records,
        invalidated,
    })
}

fn push_record(out: &mut Vec<u8>, record: &Stored) {
    push_id(out, record.id);
    out.extend_from_slice(&record.version.to_le_bytes());
    out.extend_from_slice(&record.byte_len.to_le_bytes());
    out.extend_from_slice(&record.hash);
}

fn push_id(out: &mut Vec<u8>, id: TensorId) {
    out.push(match id.kind {
        TensorKind::Ssm => 0,
        TensorKind::AttentionKv => 1,
    });
    push_u32(out, id.layer);
    push_u32(out, id.head);
    out.extend_from_slice(&id.seq_start.to_le_bytes());
    out.extend_from_slice(&id.seq_end.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_record(cursor: &mut &[u8]) -> Result<Stored, StateError> {
    let id = read_id(cursor)?;
    let version = read_u64(cursor)?;
    let byte_len = read_u64(cursor)?;
    if byte_len > cap_for(id.kind) {
        return Err(StateError::Bounded);
    }
    let mut hash = [0u8; 32];
    read_slice(cursor, &mut hash)?;
    Ok(Stored {
        id,
        version,
        hash,
        byte_len,
    })
}

fn read_id(cursor: &mut &[u8]) -> Result<TensorId, StateError> {
    let mut kind_byte = [0u8; 1];
    read_slice(cursor, &mut kind_byte)?;
    let kind = match kind_byte[0] {
        0 => TensorKind::Ssm,
        1 => TensorKind::AttentionKv,
        _ => return Err(invalid("unknown tensor kind")),
    };
    Ok(TensorId {
        kind,
        layer: read_u32(cursor)?,
        head: read_u32(cursor)?,
        seq_start: read_u64(cursor)?,
        seq_end: read_u64(cursor)?,
    })
}

fn read_slice(cursor: &mut &[u8], dest: &mut [u8]) -> Result<(), StateError> {
    if cursor.len() < dest.len() {
        return Err(invalid("truncated state manifest"));
    }
    dest.copy_from_slice(&cursor[..dest.len()]);
    *cursor = &cursor[dest.len()..];
    Ok(())
}

fn read_u16(cursor: &mut &[u8]) -> Result<u16, StateError> {
    let mut bytes = [0u8; 2];
    read_slice(cursor, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(cursor: &mut &[u8]) -> Result<u32, StateError> {
    let mut bytes = [0u8; 4];
    read_slice(cursor, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(cursor: &mut &[u8]) -> Result<u64, StateError> {
    let mut bytes = [0u8; 8];
    read_slice(cursor, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
