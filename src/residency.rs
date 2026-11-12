//! Resident stage weights and KV/SSM state registry.
//!
//! Keeps model stage weights and per-stage KV/SSM state resident across token
//! steps, tracks generations and manifest epochs, verifies BLAKE3 digests
//! before reuse, and enforces per-node memory budgets.

use std::collections::HashMap;
use std::fmt;

use crate::Digest;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StageStateKind {
    Weights,
    KvCache,
    SsmState,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StateHandleId {
    pub stage_index: u16,
    pub kind: StageStateKind,
    pub slot: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResidentEntry {
    pub handle: StateHandleId,
    pub epoch: u64,
    pub generation: u64,
    pub byte_len: u64,
    pub digest: Digest,
    pub pinned: bool,
    last_access_tick: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResidencyError {
    ExceedsBudget { required: u64, budget: u64 },
    MissingHandle,
    StaleEpoch { entry_epoch: u64, active_epoch: u64 },
    DigestMismatch,
}

impl fmt::Display for ResidencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExceedsBudget { required, budget } => {
                write!(
                    f,
                    "resident allocation {required} B exceeds budget {budget} B"
                )
            }
            Self::MissingHandle => write!(f, "resident state handle not found"),
            Self::StaleEpoch {
                entry_epoch,
                active_epoch,
            } => write!(
                f,
                "resident state epoch {entry_epoch} invalidated by active epoch {active_epoch}"
            ),
            Self::DigestMismatch => write!(f, "resident state BLAKE3 digest mismatch"),
        }
    }
}

impl std::error::Error for ResidencyError {}

/// Tracks resident weights and KV/SSM state on a single worker node.
pub struct ResidencyTable {
    budget_bytes: u64,
    used_bytes: u64,
    active_epoch: u64,
    tick: u64,
    entries: HashMap<StateHandleId, ResidentEntry>,
}

impl ResidencyTable {
    pub fn new(budget_bytes: u64, active_epoch: u64) -> Self {
        Self {
            budget_bytes,
            used_bytes: 0,
            active_epoch,
            tick: 0,
            entries: HashMap::new(),
        }
    }

    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    pub fn used_bytes(&self) -> u64 {
        self.used_bytes
    }

    pub fn active_epoch(&self) -> u64 {
        self.active_epoch
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Registers or updates a resident entry, evicting oldest unpinned entries
    /// if needed to stay within `budget_bytes`.
    pub fn register(
        &mut self,
        handle: StateHandleId,
        epoch: u64,
        byte_len: u64,
        digest: Digest,
        pinned: bool,
    ) -> Result<u64, ResidencyError> {
        if epoch < self.active_epoch {
            return Err(ResidencyError::StaleEpoch {
                entry_epoch: epoch,
                active_epoch: self.active_epoch,
            });
        }

        let existing_len = self.entries.get(&handle).map_or(0, |e| e.byte_len);
        let existing_gen = self.entries.get(&handle).map_or(0, |e| e.generation);
        let needed_after_replace = self
            .used_bytes
            .saturating_sub(existing_len)
            .saturating_add(byte_len);

        if needed_after_replace > self.budget_bytes {
            self.evict_unpinned_until(
                &handle,
                self.budget_bytes
                    .saturating_sub(byte_len)
                    .saturating_add(existing_len),
            );
        }

        let final_used = self
            .used_bytes
            .saturating_sub(existing_len)
            .saturating_add(byte_len);
        if final_used > self.budget_bytes {
            return Err(ResidencyError::ExceedsBudget {
                required: final_used,
                budget: self.budget_bytes,
            });
        }

        self.tick += 1;
        let generation = existing_gen + 1;
        let entry = ResidentEntry {
            handle: handle.clone(),
            epoch,
            generation,
            byte_len,
            digest,
            pinned,
            last_access_tick: self.tick,
        };
        self.used_bytes = final_used;
        self.entries.insert(handle, entry);
        Ok(generation)
    }

    fn evict_unpinned_until(&mut self, protect: &StateHandleId, target_used_bytes: u64) {
        while self.used_bytes > target_used_bytes {
            let victim = self
                .entries
                .iter()
                .filter(|(k, v)| *k != protect && !v.pinned)
                .min_by_key(|(_, v)| v.last_access_tick)
                .map(|(k, _)| k.clone());

            let Some(victim_key) = victim else {
                break;
            };
            if let Some(removed) = self.entries.remove(&victim_key) {
                self.used_bytes = self.used_bytes.saturating_sub(removed.byte_len);
            }
        }
    }

    /// Verifies that a resident handle exists, matches the active epoch, and
    /// matches the expected BLAKE3 digest.
    pub fn verify_resident(
        &mut self,
        handle: &StateHandleId,
        expected_digest: &Digest,
    ) -> Result<&ResidentEntry, ResidencyError> {
        let entry = self
            .entries
            .get(handle)
            .ok_or(ResidencyError::MissingHandle)?;
        if entry.epoch < self.active_epoch {
            return Err(ResidencyError::StaleEpoch {
                entry_epoch: entry.epoch,
                active_epoch: self.active_epoch,
            });
        }
        if &entry.digest != expected_digest {
            return Err(ResidencyError::DigestMismatch);
        }
        self.tick += 1;
        let tick = self.tick;
        let entry_mut = self.entries.get_mut(handle).expect("checked");
        entry_mut.last_access_tick = tick;
        Ok(entry_mut)
    }

    /// Advances the active manifest epoch and purges all entries from older epochs.
    pub fn advance_epoch(&mut self, new_epoch: u64) -> usize {
        if new_epoch <= self.active_epoch {
            return 0;
        }
        self.active_epoch = new_epoch;
        let before = self.entries.len();
        self.entries.retain(|_, entry| entry.epoch >= new_epoch);
        self.used_bytes = self.entries.values().map(|e| e.byte_len).sum();
        before - self.entries.len()
    }
}
