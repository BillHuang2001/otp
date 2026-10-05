//! Heap-size policy: [`HeapPolicy`] plus the curated growth table ([`heap_sizes`] /
//! [`next_heap_size`]).
//!
//! Mirrors the C `erts_init_gc` growth table and `erts_next_heap_size`
//! (`erts/emulator/beam/erl_gc.c`): Fibonacci+1 up to index 23, then +20 %, capped at
//! `2^53 / word`. All of this module is safe Rust.

use std::sync::LazyLock;

/// Number of entries the Fibonacci+1 growth stage fills (the C `for (i = 2; i < 23; i++)`
/// bound): table indices `0..FIBONACCI_STAGE_LEN` come from Fibonacci+1, the rest from
/// the +20 % stage.
const FIBONACCI_STAGE_LEN: usize = 23;

/// The curated heap-size growth table, built once on first use.
static HEAP_SIZES: LazyLock<Vec<usize>> = LazyLock::new(build_heap_sizes);

/// Builds the growth table exactly as `erts_init_gc` does (`erl_gc.c:262-291`).
///
/// ```text
/// 12, 38, 51, 90, 142, 233, ...          (Fibonacci + 1, one word for the block header)
/// ... then +20 % per step, capped at MAX(52-bit) / sizeof(Eterm)
/// ```
fn build_heap_sizes() -> Vec<usize> {
    let mut sizes: Vec<usize> = Vec::with_capacity(FIBONACCI_STAGE_LEN + 128);

    // Growth stage 1 — Fibonacci + 1. `12, 38` hits 233, the old default.
    sizes.push(12);
    sizes.push(38);
    while sizes.len() < FIBONACCI_STAGE_LEN {
        let next = sizes[sizes.len() - 1] + sizes[sizes.len() - 2] + 1;
        sizes.push(next);
    }

    // 64-bit cap: MAX(52bit) / sizeof(Eterm). This crate targets the frozen 64-bit term
    // layout, so the 32-bit `(~0u32)/4` branch of the C is not reachable here.
    let cap: usize = ((1u64 << 53) / 8) as usize;

    // Growth stage 2 — +20 % steps, stopping *before* the entry that would exceed the cap
    // (the C writes the offending size, then discards it and ends the table one shorter).
    while let Some(&last) = sizes.last() {
        let next = last + last / 5;
        if next > cap {
            break;
        }
        sizes.push(next);
    }

    sizes
}

/// The process heap-size policy (the `erts_next_heap_size` analogue).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeapPolicy {
    /// Smallest heap the policy will ever size to, in term words.
    pub min_heap_words: usize,
    /// Largest heap the policy will ever size to, in term words (0 = unbounded).
    pub max_heap_words: usize,
    /// Number of minor collections before a fullsweep is forced (0 disables).
    pub fullsweep_after: u32,
    /// Initial size of the young (nursery) space, in term words.
    pub initial_young_words: usize,
    /// Initial size of the mature space, in term words.
    pub initial_mature_words: usize,
    /// Slack kept available so a collection's result still fits, in term words.
    pub need_reserve_words: usize,
}

impl HeapPolicy {
    /// The BEAM defaults: `fullsweep_after` 65535, a 233-word initial young space and
    /// the curated minimum heap size.
    pub fn default() -> HeapPolicy {
        HeapPolicy {
            min_heap_words: 0,
            max_heap_words: 0,
            fullsweep_after: 65535,
            initial_young_words: 233,
            initial_mature_words: 233,
            need_reserve_words: 64,
        }
    }

    /// A policy bounded by `min_words`..=`max_words` (0 = unbounded) with the other
    /// fields at their default values.
    pub fn from_min_max(min_words: usize, max_words: usize) -> HeapPolicy {
        HeapPolicy {
            min_heap_words: min_words,
            max_heap_words: max_words,
            ..HeapPolicy::default()
        }
    }

    /// The next size (in words) to grow `size` to, `offset` entries further along the
    /// growth table.
    ///
    /// Applies the policy bounds after the table lookup: at least [`Self::min_heap_words`],
    /// and at most [`Self::max_heap_words`] when that is not `0` (unbounded).
    pub fn next_words(&self, size: usize, offset: usize) -> usize {
        let size = next_heap_size(size, offset);
        let size = size.max(self.min_heap_words);
        if self.max_heap_words != 0 {
            size.min(self.max_heap_words)
        } else {
            size
        }
    }

    /// The heap size to use after a fullsweep, from the `live_words` a collection kept
    /// and the `need_words` the mutator still requires.
    ///
    /// `need_reserve_words` of slack is requested on top, so the result is always at least
    /// `live_words + need_words` (and therefore a fullsweep can always fit its result) —
    /// unless `max_heap_words` clamps it down, which is the caller's explicit choice.
    pub fn size_after_fullsweep(&self, live_words: usize, need_words: usize) -> usize {
        self.next_words(live_words + need_words + self.need_reserve_words, 0)
    }

    /// Whether the heap at `heap_words` should be grown to satisfy `need_words`.
    pub fn should_grow(&self, heap_words: usize, need_words: usize) -> bool {
        heap_words < need_words || 3 * heap_words < 4 * need_words
    }

    /// Whether the heap at `heap_words` should be shrunk after keeping `need_words`.
    pub fn should_shrink(&self, heap_words: usize, need_words: usize) -> bool {
        heap_words > 4 * need_words && heap_words > self.min_heap_words
    }
}

impl Default for HeapPolicy {
    fn default() -> Self {
        HeapPolicy::default()
    }
}

/// The curated heap-size growth table (the C `heap_sizes`: Fibonacci+1 to index 23,
/// then +20 %, capped at `2^53 / word`).
pub fn heap_sizes() -> &'static [usize] {
    &HEAP_SIZES
}

/// The next heap size after `size`, `offset` entries along the growth table — the
/// `erts_next_heap_size` analogue.
///
/// With `offset == 0` this is the smallest curated size `>= size`; with `offset == 1` the
/// next size strictly greater than `size`.
///
/// Unlike the C, which calls `erts_exit` when `size` exceeds the table, an out-of-range
/// `size` (or an `offset` that would run past the end) clamps to the largest curated size:
/// the M1 safe fallback, since a policy must never abort the runtime.
pub fn next_heap_size(size: usize, offset: usize) -> usize {
    let table = heap_sizes();
    // The table is never empty and always starts at 12 (`heap_sizes[0]`).
    let smallest = table[0];
    if size < smallest {
        return smallest;
    }

    // The clamping fallback for sizes beyond the table and for offsets that leave it.
    let largest = table[table.len() - 1];

    let mut low = 0usize;
    let mut high = table.len();
    while low < high {
        let mid = low + (high - low) / 2;
        if size < table[mid] {
            high = mid;
        } else if size == table[mid] {
            return table.get(mid + offset).copied().unwrap_or(largest);
        } else if mid + 1 < table.len() && size < table[mid + 1] {
            return table.get(mid + offset + 1).copied().unwrap_or(largest);
        } else {
            low = mid + 1;
        }
    }

    largest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 64-bit cap the growth table is built against (`MAX(52bit) / sizeof(Eterm)`).
    fn cap() -> usize {
        ((1u64 << 53) / 8) as usize
    }

    #[test]
    fn growth_table_shape_mirrors_the_c_heap_sizes() {
        let table = heap_sizes();
        // Growth stage 1: Fibonacci + 1, reaching 233 at index 5 (the old default).
        assert_eq!(&table[..6], &[12, 38, 51, 90, 142, 233]);
        assert!(table.contains(&233));
        assert!(table.len() > FIBONACCI_STAGE_LEN);
        // Strictly increasing, so the binary search is well-defined.
        assert!(table.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn growth_table_stage_two_is_twenty_percent_capped() {
        let table = heap_sizes();
        for i in FIBONACCI_STAGE_LEN..table.len() {
            assert_eq!(table[i], table[i - 1] + table[i - 1] / 5);
        }
        // Every entry stays within the cap, and the table stopped exactly before the step
        // that would have exceeded it.
        let largest = table[table.len() - 1];
        assert!(largest <= cap());
        assert!(largest + largest / 5 > cap());
    }

    #[test]
    fn next_heap_size_matches_the_documented_lookup() {
        assert_eq!(next_heap_size(0, 0), 12);
        assert_eq!(next_heap_size(11, 0), 12);
        assert_eq!(next_heap_size(12, 0), 12);
        assert_eq!(next_heap_size(13, 0), 38);
        assert_eq!(next_heap_size(38, 0), 38);
        assert_eq!(next_heap_size(39, 0), 51);
        assert_eq!(next_heap_size(12, 1), 38);
        assert_eq!(next_heap_size(13, 1), 51);

        // Every curated size is a fixed point with offset 0 ...
        let table = heap_sizes();
        for &size in table {
            assert_eq!(next_heap_size(size, 0), size);
        }
        // ... and steps to its successor with offset 1.
        for pair in table.windows(2) {
            assert_eq!(next_heap_size(pair[0], 1), pair[1]);
        }
    }

    #[test]
    fn next_heap_size_clamps_out_of_range_sizes() {
        let largest = heap_sizes()[heap_sizes().len() - 1];
        assert_eq!(next_heap_size(usize::MAX, 0), largest);
        assert_eq!(next_heap_size(largest + 1, 0), largest);
        // An offset that would leave the table clamps too (no abort, no panic).
        assert_eq!(next_heap_size(largest, 1), largest);
        assert_eq!(next_heap_size(usize::MAX, 1), largest);
    }

    #[test]
    fn default_policy_matches_the_beam_defaults() {
        let policy = HeapPolicy::default();
        assert_eq!(policy.min_heap_words, 0);
        assert_eq!(policy.max_heap_words, 0);
        assert_eq!(policy.fullsweep_after, 65535);
        assert_eq!(policy.initial_young_words, 233);
        assert_eq!(policy.initial_mature_words, 233);
        assert_eq!(policy.need_reserve_words, 64);
        // The `Default` impl forwards to the inherent constructor.
        let via_trait: HeapPolicy = Default::default();
        assert_eq!(via_trait, policy);
    }

    #[test]
    fn from_min_max_overrides_only_the_bounds() {
        let policy = HeapPolicy::from_min_max(233, 100_000);
        let default = HeapPolicy::default();
        assert_eq!(policy.min_heap_words, 233);
        assert_eq!(policy.max_heap_words, 100_000);
        assert_eq!(policy.fullsweep_after, default.fullsweep_after);
        assert_eq!(policy.initial_young_words, default.initial_young_words);
        assert_eq!(policy.initial_mature_words, default.initial_mature_words);
        assert_eq!(policy.need_reserve_words, default.need_reserve_words);
    }

    #[test]
    fn next_words_honours_the_policy_bounds() {
        let policy = HeapPolicy::default();
        assert_eq!(policy.next_words(0, 0), 12);
        assert_eq!(policy.next_words(13, 0), 38);
        assert_eq!(policy.next_words(12, 1), 38);

        let floored = HeapPolicy {
            min_heap_words: 500,
            ..HeapPolicy::default()
        };
        assert_eq!(floored.next_words(10, 0), 500);
        assert_eq!(floored.next_words(300, 0), 500); // table says 376, the floor wins
        assert_eq!(floored.next_words(400, 0), 610); // table is already above the floor

        let capped = HeapPolicy {
            max_heap_words: 100,
            ..HeapPolicy::default()
        };
        assert_eq!(capped.next_words(10, 0), 12);
        assert_eq!(capped.next_words(300, 0), 100);
    }

    #[test]
    fn size_after_fullsweep_always_fits_the_live_set() {
        let policy = HeapPolicy::default();
        for live in [0usize, 1, 12, 100, 233, 10_000] {
            for need in [0usize, 1, 50, 233, 5_000] {
                let size = policy.size_after_fullsweep(live, need);
                assert!(
                    size >= live + need,
                    "size {size} < live + need {}",
                    live + need
                );
                // The collection always asks for its reserve on top.
                assert!(size > live + need);
            }
        }

        // A tiny heap still gets a curated size: the request is 0 + 0 + 64 = 64 words, so the
        // next curated size is 90.
        assert_eq!(policy.size_after_fullsweep(0, 0), 90);
        // Explicit bounds win.
        let bounded = HeapPolicy::from_min_max(1000, 2000);
        assert_eq!(bounded.size_after_fullsweep(0, 0), 1000);
        assert_eq!(bounded.size_after_fullsweep(1_000_000, 0), 2000);
    }

    #[test]
    fn grow_and_shrink_boundaries() {
        let policy = HeapPolicy::default();

        // Growth triggers when the heap is too small, or when it is below 4/3 of the need.
        assert!(!policy.should_grow(100, 75)); // 3*100 == 4*75: exactly enough
        assert!(policy.should_grow(100, 76)); // one over
        assert!(policy.should_grow(100, 101)); // outright too small
        assert!(!policy.should_grow(1000, 750));
        assert!(policy.should_grow(1000, 751));
        assert!(policy.should_grow(0, 1));
        assert!(!policy.should_grow(4, 3));

        // Shrinking triggers only when more than 4x the need is kept.
        assert!(!policy.should_shrink(100, 25)); // 100 == 4*25: not yet
        assert!(policy.should_shrink(100, 24));
        assert!(!policy.should_shrink(0, 0));
        assert!(policy.should_shrink(1000, 0));

        // The minimum heap size floors the shrink decision.
        let floored = HeapPolicy {
            min_heap_words: 500,
            ..HeapPolicy::default()
        };
        assert!(!floored.should_shrink(500, 100));
        assert!(floored.should_shrink(501, 100));
    }
}
