//! Heap-size policy: [`HeapPolicy`] plus the curated growth table ([`heap_sizes`] /
//! [`next_heap_size`]).
//!
//! Mirrors the C `erts_init_gc` growth table and `erts_next_heap_size`
//! (`erts/emulator/beam/erl_gc.c`): Fibonacci+1 up to index 23, then +20 %, capped at
//! `2^53 / word`. All of this module is safe Rust.

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
        unimplemented!("scaffold")
    }

    /// A policy bounded by `min_words`..=`max_words` (0 = unbounded) with the other
    /// fields at their default values.
    pub fn from_min_max(min_words: usize, max_words: usize) -> HeapPolicy {
        let _ = (min_words, max_words);
        unimplemented!("scaffold")
    }

    /// The next size (in words) to grow `size` to, `offset` entries further along the
    /// growth table.
    pub fn next_words(&self, size: usize, offset: usize) -> usize {
        let _ = (size, offset);
        unimplemented!("scaffold")
    }

    /// The heap size to use after a fullsweep, from the `live_words` a collection kept
    /// and the `need_words` the mutator still requires.
    pub fn size_after_fullsweep(&self, live_words: usize, need_words: usize) -> usize {
        let _ = (live_words, need_words);
        unimplemented!("scaffold")
    }

    /// Whether the heap at `heap_words` should be grown to satisfy `need_words`.
    pub fn should_grow(&self, heap_words: usize, need_words: usize) -> bool {
        let _ = (heap_words, need_words);
        unimplemented!("scaffold")
    }

    /// Whether the heap at `heap_words` should be shrunk after keeping `need_words`.
    pub fn should_shrink(&self, heap_words: usize, need_words: usize) -> bool {
        let _ = (heap_words, need_words);
        unimplemented!("scaffold")
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
    unimplemented!("scaffold")
}

/// The next heap size after `size`, `offset` entries along the growth table — the
/// `erts_next_heap_size` analogue.
pub fn next_heap_size(size: usize, offset: usize) -> usize {
    let _ = (size, offset);
    unimplemented!("scaffold")
}
