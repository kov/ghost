//! Cheap hit/miss accounting shared by the caches across ghost's render pipeline
//! (shaping, the glyph atlas, per-session surfaces, fleet frames, ...).
//!
//! The counters are plain integers on an owned struct, bumped with a single add on
//! the single-threaded render path — so they are always on and effectively free,
//! and serve as one source of truth for both the `RUST_LOG` cache view and the
//! tests that guard against quietly losing cache hits (a dead cache passes every
//! correctness test while silently costing what the cache was meant to save).

/// Hit / miss / insert / evict tallies for one cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheCounters {
    /// Lookups served from the cache without recomputing.
    pub hits: u64,
    /// Lookups that had to compute or load the value (a cache miss).
    pub misses: u64,
    /// Entries inserted — equals the misses that were stored (it can trail `misses`
    /// when a miss is deliberately not cached, e.g. an oversize value).
    pub inserts: u64,
    /// Entries dropped from the cache (LRU / pruning / invalidation).
    pub evictions: u64,
}

impl CacheCounters {
    #[inline]
    pub fn hit(&mut self) {
        self.hits += 1;
    }
    #[inline]
    pub fn miss(&mut self) {
        self.misses += 1;
    }
    #[inline]
    pub fn insert(&mut self) {
        self.inserts += 1;
    }
    #[inline]
    pub fn evict(&mut self, n: u64) {
        self.evictions += n;
    }

    /// Total lookups (`hits + misses`).
    pub fn lookups(&self) -> u64 {
        self.hits + self.misses
    }

    /// Fraction of lookups served from cache, in `[0, 1]`. A cache with no lookups
    /// is vacuously perfect (`1.0`) so an idle subsystem never reads as a regression.
    pub fn hit_rate(&self) -> f64 {
        match self.lookups() {
            0 => 1.0,
            n => self.hits as f64 / n as f64,
        }
    }

    /// Field-by-field difference `self - earlier`, for a per-frame or per-test
    /// window over the running totals. `earlier` must be an earlier snapshot of the
    /// same counters (each field monotonically grows), else this underflows.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            hits: self.hits - earlier.hits,
            misses: self.misses - earlier.misses,
            inserts: self.inserts - earlier.inserts,
            evictions: self.evictions - earlier.evictions,
        }
    }
}

impl std::fmt::Display for CacheCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.1}% ({}/{} hit, {} ins, {} evict)",
            self.hit_rate() * 100.0,
            self.hits,
            self.lookups(),
            self.inserts,
            self.evictions,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_rate_of_an_untouched_cache_is_vacuously_perfect() {
        assert_eq!(CacheCounters::default().hit_rate(), 1.0);
        assert_eq!(CacheCounters::default().lookups(), 0);
    }

    #[test]
    fn hit_rate_counts_only_lookups_not_inserts_or_evictions() {
        let mut c = CacheCounters::default();
        c.miss();
        c.insert();
        c.hit();
        c.hit();
        c.hit();
        c.evict(2);
        // 3 hits of 4 lookups; inserts/evictions don't move the rate.
        assert_eq!(c.lookups(), 4);
        assert_eq!(c.hit_rate(), 0.75);
    }

    #[test]
    fn since_windows_the_running_totals() {
        let start = CacheCounters {
            hits: 10,
            misses: 3,
            inserts: 3,
            evictions: 1,
        };
        let mut now = start;
        now.hit();
        now.hit();
        now.miss();
        now.insert();
        let delta = now.since(start);
        assert_eq!(
            delta,
            CacheCounters {
                hits: 2,
                misses: 1,
                inserts: 1,
                evictions: 0,
            }
        );
    }
}
