//! Table-owned term storage — the A.1 copy boundary.
//!
//! [`TermStore`] is a **never-relocating bump arena** built over a
//! [`beam_alloc::AllocRef`] (`AllocType::Ets`): it keeps a chain of owned
//! allocation blocks and hands out stable word pointers, so interior pointers
//! handed out by the table never dangle across growth. An inserted object is
//! **deep-copied** into this arena ([`TermStore::copy`]); the table never retains
//! a pointer into a caller's or process heap.
//!
//! This is the *only* module of the crate allowed to contain `unsafe` (raw term
//! reads/writes) alongside the tuple-element accessor in [`crate::key`]; every
//! `unsafe fn` carries a `# Safety` section and every `unsafe` block a preceding
//! `// SAFETY:` rationale (workspace `CONTEXT.md` §3).

use core::ptr::NonNull;

use beam_alloc::AllocRef;
use beam_common::Result;
use beam_term::prelude::*;

/// One owned, never-relocating block of the store's bump arena.
struct StoreBlock {
    /// Base pointer of this block's word storage.
    base: NonNull<Eterm>,
    /// Number of [`Eterm`] words this block can hold.
    capacity: usize,
}

/// A per-table bump arena over table-owned term storage.
///
/// Created with an explicit allocator ([`TermStore::new`]) or the registered ETS
/// allocator ([`TermStore::with_default_allocator`]). Dropping the store frees
/// every owned block through the allocator.
pub struct TermStore {
    /// The allocator every block is (de)allocated through.
    alloc: AllocRef,
    /// The chain of owned blocks, in allocation order.
    blocks: Vec<StoreBlock>,
    /// The total number of words handed out so far.
    words: usize,
}

impl TermStore {
    /// A store allocating through `alloc`.
    pub fn new(alloc: AllocRef) -> TermStore {
        unimplemented!("M1: implementation pending")
    }

    /// A store allocating through the registered ETS allocator
    /// ([`beam_alloc::AllocType::Ets`]).
    pub fn with_default_allocator() -> TermStore {
        unimplemented!("M1: implementation pending")
    }

    /// Deep-copies the term graph rooted at `root` into table-owned memory and
    /// returns the new root. Immediates are returned unchanged (copied by value).
    ///
    /// Fails with `badarg` for term kinds outside the supported subset (see the
    /// crate `CONTEXT.md`, Known Issues).
    pub fn copy(&mut self, root: Eterm) -> Result<Eterm> {
        unimplemented!("M1: implementation pending")
    }

    /// The total number of words handed out by this store.
    pub fn words(&self) -> usize {
        unimplemented!("M1: implementation pending")
    }

    /// The number of blocks this store owns.
    pub fn blocks(&self) -> usize {
        unimplemented!("M1: implementation pending")
    }
}
