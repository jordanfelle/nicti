//! Byte-budgeted, content-keyed cache tiers (VRAM / RAM ring / disk), per #44's own ticket body:
//! "Cache tiers: VRAM (current image + neighbors), RAM ring, disk (compressed half-float / mask
//! alpha). Per-stage invalidation." Nothing in the repo has this shape yet -- `spikes/sniff`'s
//! `cache.rs` is asset+tier keyed and never evicts (it's a persistent preview store, ADR-0029);
//! this is a content-keyed (`graph::RenderGraph::cache_key`), evicting, byte-budgeted cache for
//! baked render-stage output, a different problem.
//!
//! `Tier` is generic over the payload's byte size (an `impl Fn(&V) -> u64`, since a stored value
//! here is a full RGBA16F plane or a compressed byte blob, not something with a cheap fixed size
//! like sniff's preview blobs) so the same LRU/eviction logic backs the VRAM, RAM, and disk tiers
//! with different budgets and payload shapes.

use std::collections::HashMap;
use std::collections::VecDeque;

/// A byte-budgeted LRU keyed by [`blake3::Hash`] (a `graph::RenderGraph::cache_key`). Eviction
/// happens on insert, not lazily, so `len_bytes()` never exceeds `budget_bytes` after a call to
/// `put` returns (unless a single value alone exceeds the whole budget, in which case it's
/// rejected rather than evicting everything else for a payload that will just be evicted again
/// next insert).
pub struct Tier<V> {
    budget_bytes: u64,
    used_bytes: u64,
    entries: HashMap<blake3::Hash, V>,
    /// Most-recently-used at the back. A `VecDeque` rather than an ordered map keeps `touch`
    /// O(n) worst case but simple and obviously correct for the sizes this spike measures
    /// against (tens of resident images, not millions) -- a real integration would use an
    /// intrusive linked-hash-map. Documented here rather than silently accepted as "fine
    /// forever."
    order: VecDeque<blake3::Hash>,
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
            order: VecDeque::new(),
            size_of: Box::new(size_of),
        }
    }

    pub fn get(&mut self, key: &blake3::Hash) -> Option<&V> {
        if self.entries.contains_key(key) {
            self.touch(key);
        }
        self.entries.get(key)
    }

    pub fn contains(&self, key: &blake3::Hash) -> bool {
        self.entries.contains_key(key)
    }

    fn touch(&mut self, key: &blake3::Hash) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(*key);
    }

    /// Inserts `value`, evicting least-recently-used entries until the new value fits the budget.
    /// Returns `false` (and inserts nothing) if `value` alone is larger than the whole budget --
    /// this tier is the wrong place for it, not a bug in the eviction loop.
    pub fn put(&mut self, key: blake3::Hash, value: V) -> bool {
        let size = (self.size_of)(&value);
        if size > self.budget_bytes {
            return false;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.used_bytes -= (self.size_of)(&old);
            if let Some(pos) = self.order.iter().position(|k| *k == key) {
                self.order.remove(pos);
            }
        }
        while self.used_bytes + size > self.budget_bytes {
            let Some(victim) = self.order.pop_front() else {
                break;
            };
            if let Some(v) = self.entries.remove(&victim) {
                self.used_bytes -= (self.size_of)(&v);
            }
        }
        self.used_bytes += size;
        self.entries.insert(key, value);
        self.order.push_back(key);
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

/// Compresses a raw f16-plane byte buffer for the disk tier. `zstd` (level 3, a reasonable
/// interactive-cost default) generally beats `lz4_flex` on ratio for photographic half-float data
/// at some CPU cost; both are exposed so `bin/loaf.rs bench` can report the real tradeoff on the
/// reference machine rather than this module picking one blind.
pub fn compress_zstd(data: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(data, 3).expect("zstd encoding into a Vec never fails")
}

pub fn decompress_zstd(data: &[u8]) -> Vec<u8> {
    zstd::stream::decode_all(data).expect("well-formed zstd stream decodes")
}

pub fn compress_lz4(data: &[u8]) -> Vec<u8> {
    lz4_flex::compress_prepend_size(data)
}

pub fn decompress_lz4(data: &[u8]) -> Vec<u8> {
    lz4_flex::decompress_size_prepended(data).expect("well-formed lz4 stream decodes")
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
    fn zstd_and_lz4_round_trip_a_synthetic_f16_plane() {
        // A synthetic "half-float plane": not random noise (which doesn't compress, unlike real
        // photo content) -- a smooth gradient, closer to what an actual denoised/graded frame's
        // byte pattern looks like.
        let mut data = Vec::with_capacity(64 * 64 * 4 * 2);
        for i in 0..(64 * 64 * 4) {
            let v = half::f16::from_f32((i % 256) as f32 / 255.0);
            data.extend_from_slice(&v.to_le_bytes());
        }

        let zstd_out = compress_zstd(&data);
        assert_eq!(decompress_zstd(&zstd_out), data);
        assert!(
            zstd_out.len() < data.len(),
            "zstd should compress a smooth gradient smaller than raw"
        );

        let lz4_out = compress_lz4(&data);
        assert_eq!(decompress_lz4(&lz4_out), data);
    }

    /// Real zstd-vs-lz4 ratio/speed on a screen-resolution (3840-long-edge) synthetic RGBA16F
    /// plane -- `#[ignore]`d per this repo's convention for anything that reports a real number
    /// rather than just asserting correctness (`cargo test -- --ignored --nocapture` per
    /// CONTRIBUTING.md). A synthetic vertical gradient, not noise -- noise doesn't compress at all
    /// and would understate what a real graded/denoised frame's byte pattern looks like, but this
    /// is still not a real photo, so treat the ratio as directional, not a promise about real
    /// content.
    #[test]
    #[ignore]
    fn disk_tier_compression_ratio_on_a_screen_res_synthetic_plane() {
        let (w, h) = (3840usize, 2560usize);
        let mut data = Vec::with_capacity(w * h * 4 * 2);
        for y in 0..h {
            let v = half::f16::from_f32(y as f32 / h as f32);
            for _ in 0..(w * 4) {
                data.extend_from_slice(&v.to_le_bytes());
            }
        }
        let raw_mb = data.len() as f64 / (1024.0 * 1024.0);

        let start = std::time::Instant::now();
        let zstd_out = compress_zstd(&data);
        let zstd_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(decompress_zstd(&zstd_out), data);

        let start = std::time::Instant::now();
        let lz4_out = compress_lz4(&data);
        let lz4_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(decompress_lz4(&lz4_out), data);

        println!(
            "raw={raw_mb:.1}MB zstd={:.2}MB ({:.1}x, {zstd_ms:.1}ms) lz4={:.2}MB ({:.1}x, {lz4_ms:.1}ms)",
            zstd_out.len() as f64 / (1024.0 * 1024.0),
            data.len() as f64 / zstd_out.len() as f64,
            lz4_out.len() as f64 / (1024.0 * 1024.0),
            data.len() as f64 / lz4_out.len() as f64,
        );
    }
}
