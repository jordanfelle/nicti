//! Byte-budgeted, content-keyed cache tiers (VRAM / RAM ring / disk), per ADR-0044's own ticket
//! body: "Cache tiers: VRAM (current image + neighbors), RAM ring, disk (compressed half-float /
//! mask alpha). Per-stage invalidation." `Tier` is generic over the payload's byte size (an `impl
//! Fn(&V) -> u64`, since a stored value here is a full RGBA16F plane or a compressed byte blob,
//! not something with a cheap fixed size) so the same LRU/eviction logic backs the VRAM, RAM, and
//! disk tiers with different budgets and payload shapes.
//!
//! Promoted from `spikes/loaf/src/cache.rs`, with its own self-documented gap fixed: recency is
//! tracked with a generation counter (`HashMap<Hash, (V, u64)>` plus a `BTreeMap<u64, Hash>`
//! ordered by generation) instead of a `VecDeque` scanned linearly on every touch, so `get`/`put`
//! are `O(log n)` rather than `O(n)`. The disk-tier codec (zstd/lz4) is left to #190, which needs
//! real baked output to choose a codec against -- this module is the eviction/budget primitive
//! only.

use std::collections::{BTreeMap, HashMap};

/// A byte-budgeted LRU keyed by [`blake3::Hash`] (a `graph::RenderGraph::cache_key`). Eviction
/// happens on insert, not lazily, so `len_bytes()` never exceeds `budget_bytes` after a call to
/// `put` returns (unless a single value alone exceeds the whole budget, in which case it's
/// rejected rather than evicting everything else for a payload that will just be evicted again
/// next insert).
pub struct Tier<V> {
    budget_bytes: u64,
    used_bytes: u64,
    entries: HashMap<blake3::Hash, (V, u64)>,
    /// Generation -> key, ascending; the front (`first_key_value`) is always the
    /// least-recently-used entry, so eviction and `touch` are both `O(log n)`.
    recency: BTreeMap<u64, blake3::Hash>,
    next_gen: u64,
    size_of: Box<dyn Fn(&V) -> u64 + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierStats {
    pub used_bytes: u64,
    pub budget_bytes: u64,
    pub entry_count: usize,
}

impl<V> Tier<V> {
    pub fn new(budget_bytes: u64, size_of: impl Fn(&V) -> u64 + Send + Sync + 'static) -> Self {
        Self {
            budget_bytes,
            used_bytes: 0,
            entries: HashMap::new(),
            recency: BTreeMap::new(),
            next_gen: 0,
            size_of: Box::new(size_of),
        }
    }

    pub fn get(&mut self, key: &blake3::Hash) -> Option<&V> {
        if self.entries.contains_key(key) {
            self.touch(key);
        }
        self.entries.get(key).map(|(v, _)| v)
    }

    pub fn contains(&self, key: &blake3::Hash) -> bool {
        self.entries.contains_key(key)
    }

    fn next_generation(&mut self) -> u64 {
        let gen = self.next_gen;
        self.next_gen += 1;
        gen
    }

    fn touch(&mut self, key: &blake3::Hash) {
        let new_gen = self.next_generation();
        if let Some((_, old_gen)) = self.entries.get_mut(key) {
            self.recency.remove(old_gen);
            self.recency.insert(new_gen, *key);
            *old_gen = new_gen;
        }
    }

    /// Inserts `value`, evicting least-recently-used entries until the new value fits the budget.
    /// Returns `false` (and inserts nothing) if `value` alone is larger than the whole budget --
    /// this tier is the wrong place for it, not a bug in the eviction loop.
    pub fn put(&mut self, key: blake3::Hash, value: V) -> bool {
        let size = (self.size_of)(&value);
        if size > self.budget_bytes {
            return false;
        }
        if let Some((old, old_gen)) = self.entries.remove(&key) {
            self.used_bytes -= (self.size_of)(&old);
            self.recency.remove(&old_gen);
        }
        while self.used_bytes + size > self.budget_bytes {
            let Some((&oldest_gen, &victim)) = self.recency.iter().next() else {
                break;
            };
            self.recency.remove(&oldest_gen);
            if let Some((v, _)) = self.entries.remove(&victim) {
                self.used_bytes -= (self.size_of)(&v);
            }
        }
        let gen = self.next_generation();
        self.used_bytes += size;
        self.entries.insert(key, (value, gen));
        self.recency.insert(gen, key);
        true
    }

    pub fn stats(&self) -> TierStats {
        TierStats {
            used_bytes: self.used_bytes,
            budget_bytes: self.budget_bytes,
            entry_count: self.entries.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> blake3::Hash {
        blake3::hash(&[byte])
    }

    #[test]
    fn stays_within_budget_after_eviction() {
        let mut tier: Tier<Vec<u8>> = Tier::new(100, |v: &Vec<u8>| v.len() as u64);
        for i in 0..10u8 {
            tier.put(key(i), vec![0u8; 30]);
        }
        let stats = tier.stats();
        assert!(
            stats.used_bytes <= stats.budget_bytes,
            "used {} exceeds budget {}",
            stats.used_bytes,
            stats.budget_bytes
        );
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let mut tier: Tier<Vec<u8>> = Tier::new(100, |v: &Vec<u8>| v.len() as u64);
        tier.put(key(1), vec![0u8; 40]);
        tier.put(key(2), vec![0u8; 40]);
        // Touch key(1) so key(2) becomes the LRU entry.
        tier.get(&key(1));
        tier.put(key(3), vec![0u8; 40]);
        assert!(tier.contains(&key(1)));
        assert!(!tier.contains(&key(2)), "key(2) should have been evicted");
        assert!(tier.contains(&key(3)));
    }

    #[test]
    fn a_value_larger_than_the_whole_budget_is_rejected_not_partially_stored() {
        let mut tier: Tier<Vec<u8>> = Tier::new(50, |v: &Vec<u8>| v.len() as u64);
        let accepted = tier.put(key(1), vec![0u8; 200]);
        assert!(!accepted);
        assert!(!tier.contains(&key(1)));
        assert_eq!(tier.stats().used_bytes, 0);
    }

    #[test]
    fn re_inserting_an_existing_key_does_not_double_count_its_bytes() {
        let mut tier: Tier<Vec<u8>> = Tier::new(100, |v: &Vec<u8>| v.len() as u64);
        tier.put(key(1), vec![0u8; 40]);
        tier.put(key(1), vec![0u8; 40]);
        assert_eq!(tier.stats().used_bytes, 40);
        assert_eq!(tier.stats().entry_count, 1);
    }

    #[test]
    fn frequent_touches_via_get_keep_an_entry_resident_across_thousands_of_evictions() {
        // Stress test for the O(log n) recency structure: `get()` (touch) one entry right before
        // every one of 5,000 unrelated inserts that churn the tier's eviction. If `touch`'s
        // `entries`/`recency` maps ever desynced (e.g. a stale generation left in one map but not
        // the other), the touched entry would eventually be evicted despite being the most
        // recently used at every eviction decision -- a plain "insert 10k, check the budget"
        // smoke test can't catch that, since it never calls `get()`/`touch()` at all.
        let mut tier: Tier<u64> = Tier::new(10, |_| 1);
        for i in 0..10u64 {
            tier.put(key(i as u8), i);
        }
        let kept = key(0);
        for i in 10..5_000u64 {
            assert!(
                tier.get(&kept).is_some(),
                "kept entry evicted despite being touched every iteration (i={i})"
            );
            tier.put(key((i % 256) as u8), i);
        }
        assert!(tier.contains(&kept));
        assert_eq!(tier.stats().entry_count, 10);
        assert!(tier.stats().used_bytes <= tier.stats().budget_bytes);
    }
}
