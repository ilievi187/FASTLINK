//! In-memory content-addressed chunk cache with a hard payload budget.
//!
//! The budget is the sum of resident chunk payloads. Allocator and map
//! overhead are outside the budget. Eviction is FIFO by admission sequence:
//! the smallest sequence leaves first. A repeated insert of the same verified
//! bytes is a no-op and does not refresh that sequence. `get` does not
//! refresh it either. This cache does not delete durable on-disk objects.

use crate::{digest, Digest, CHUNK_BYTES};
use std::collections::{BTreeMap, HashMap};

/// Why an insert was refused. The cache is left unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheError {
    EmptyPayload,
    ExceedsChunkLimit { len: usize, limit: usize },
    ExceedsBudget { len: usize, budget: usize },
    DigestMismatch,
}

/// One resident chunk dropped to make room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictedChunk {
    pub digest: Digest,
    pub payload_bytes: usize,
    pub sequence: u64,
}

#[derive(Debug)]
struct Resident {
    bytes: Vec<u8>,
}

/// Bounded resident set of verified chunks.
#[derive(Debug)]
pub struct MemoryBudgetCache {
    budget: usize,
    used: usize,
    next_seq: u64,
    entries: HashMap<Digest, Resident>,
    order: BTreeMap<u64, Digest>,
}

impl MemoryBudgetCache {
    /// `budget` is the maximum sum of resident payload lengths.
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            next_seq: 0,
            entries: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    pub fn budget_bytes(&self) -> usize {
        self.budget
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, hash: &Digest) -> bool {
        self.entries.contains_key(hash)
    }

    /// Borrow a resident payload. Does not change eviction order.
    pub fn get(&self, hash: &Digest) -> Option<&[u8]> {
        self.entries.get(hash).map(|entry| entry.bytes.as_slice())
    }

    /// Digests from oldest admission to newest.
    pub fn eviction_order(&self) -> Vec<Digest> {
        self.order.values().copied().collect()
    }

    /// Insert a verified chunk.
    ///
    /// Returns the chunks evicted to make room, oldest first. When `bytes`
    /// are already resident under `hash`, the result is an empty list and the
    /// admission sequence stays the same.
    pub fn insert(
        &mut self,
        hash: Digest,
        bytes: Vec<u8>,
    ) -> Result<Vec<EvictedChunk>, CacheError> {
        if bytes.is_empty() {
            return Err(CacheError::EmptyPayload);
        }
        if bytes.len() > CHUNK_BYTES {
            return Err(CacheError::ExceedsChunkLimit {
                len: bytes.len(),
                limit: CHUNK_BYTES,
            });
        }
        if digest(&bytes) != hash {
            return Err(CacheError::DigestMismatch);
        }
        if bytes.len() > self.budget {
            return Err(CacheError::ExceedsBudget {
                len: bytes.len(),
                budget: self.budget,
            });
        }
        if self.entries.contains_key(&hash) {
            return Ok(Vec::new());
        }

        let mut evicted = Vec::new();
        while self.used + bytes.len() > self.budget {
            let Some((sequence, old_hash)) =
                self.order.iter().next().map(|(seq, hash)| (*seq, *hash))
            else {
                break;
            };
            self.order.remove(&sequence);
            if let Some(entry) = self.entries.remove(&old_hash) {
                self.used = self.used.saturating_sub(entry.bytes.len());
                evicted.push(EvictedChunk {
                    digest: old_hash,
                    payload_bytes: entry.bytes.len(),
                    sequence,
                });
            }
        }
        if self.used + bytes.len() > self.budget {
            return Err(CacheError::ExceedsBudget {
                len: bytes.len(),
                budget: self.budget,
            });
        }

        let sequence = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.used += bytes.len();
        self.order.insert(sequence, hash);
        self.entries.insert(hash, Resident { bytes });
        Ok(evicted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(byte: u8, len: usize) -> (Digest, Vec<u8>) {
        let bytes = vec![byte; len];
        (digest(&bytes), bytes)
    }

    #[test]
    fn fifo_evicts_oldest_admission_first() {
        let (a_hash, a) = chunk(1, 8);
        let (b_hash, b) = chunk(2, 8);
        let (c_hash, c) = chunk(3, 8);
        let mut cache = MemoryBudgetCache::new(16);
        assert!(cache.insert(a_hash, a.clone()).unwrap().is_empty());
        assert!(cache.insert(b_hash, b.clone()).unwrap().is_empty());
        let evicted = cache.insert(c_hash, c.clone()).unwrap();
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].digest, a_hash);
        assert_eq!(evicted[0].sequence, 0);
        assert_eq!(evicted[0].payload_bytes, 8);
        assert!(!cache.contains(&a_hash));
        assert_eq!(cache.get(&b_hash), Some(b.as_slice()));
        assert_eq!(cache.get(&c_hash), Some(c.as_slice()));
        assert_eq!(cache.used_bytes(), 16);
        assert_eq!(cache.eviction_order(), vec![b_hash, c_hash]);
    }

    #[test]
    fn same_insert_sequence_evicts_the_same_digests() {
        let mut first = MemoryBudgetCache::new(16);
        let mut second = MemoryBudgetCache::new(16);
        let mut evicted_first = Vec::new();
        let mut evicted_second = Vec::new();
        for byte in [4u8, 5, 6, 7] {
            let (hash, bytes) = chunk(byte, 8);
            evicted_first.extend(first.insert(hash, bytes.clone()).unwrap());
            evicted_second.extend(second.insert(hash, bytes).unwrap());
        }
        assert_eq!(evicted_first, evicted_second);
        assert_eq!(first.eviction_order(), second.eviction_order());
    }

    #[test]
    fn get_and_reinsert_do_not_refresh_admission() {
        let (a_hash, a) = chunk(9, 8);
        let (b_hash, b) = chunk(10, 8);
        let (c_hash, c) = chunk(11, 8);
        let mut cache = MemoryBudgetCache::new(16);
        cache.insert(a_hash, a.clone()).unwrap();
        cache.insert(b_hash, b).unwrap();
        assert_eq!(cache.get(&a_hash), Some(a.as_slice()));
        assert!(cache.insert(a_hash, a).unwrap().is_empty());
        assert_eq!(cache.eviction_order()[0], a_hash);
        let evicted = cache.insert(c_hash, c).unwrap();
        assert_eq!(evicted[0].digest, a_hash);
        assert!(cache.contains(&b_hash));
    }

    #[test]
    fn oversized_and_mismatched_inserts_leave_the_cache_unchanged() {
        let (hash, bytes) = chunk(1, 8);
        let mut cache = MemoryBudgetCache::new(8);
        cache.insert(hash, bytes).unwrap();
        let too_big = vec![2u8; 9];
        let err = cache
            .insert(digest(&too_big), too_big)
            .expect_err("over budget");
        assert_eq!(err, CacheError::ExceedsBudget { len: 9, budget: 8 });
        let wrong = vec![3u8; 4];
        assert_eq!(
            cache.insert(hash, wrong).unwrap_err(),
            CacheError::DigestMismatch
        );
        assert_eq!(
            cache.insert(digest(&[]), Vec::new()).unwrap_err(),
            CacheError::EmptyPayload
        );
        let huge = vec![4u8; CHUNK_BYTES + 1];
        assert!(matches!(
            cache.insert(digest(&huge), huge).unwrap_err(),
            CacheError::ExceedsChunkLimit { .. }
        ));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.used_bytes(), 8);
        assert!(cache.contains(&hash));
    }

    #[test]
    fn zero_budget_holds_nothing() {
        let (hash, bytes) = chunk(1, 1);
        let mut cache = MemoryBudgetCache::new(0);
        assert_eq!(
            cache.insert(hash, bytes).unwrap_err(),
            CacheError::ExceedsBudget { len: 1, budget: 0 }
        );
        assert!(cache.is_empty());
    }
}
