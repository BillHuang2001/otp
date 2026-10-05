//! Table-owned term storage — the A.1 copy boundary.
//!
//! [`TermStore`] is a **never-relocating bump arena** built over a
//! [`beam_alloc::AllocRef`] (`AllocType::Ets`): it keeps a chain of owned
//! allocation blocks and hands out stable word pointers, so interior pointers
//! handed out by the table never dangle across growth. An inserted object is
//! **deep-copied** into this arena ([`TermStore::copy`]); the table never retains
//! a pointer into a caller's or process heap.
//!
//! # Per-object reclamation
//!
//! A block is never moved — that is what keeps interior pointers stable — but the
//! *words inside* it are not immutable. [`TermStore::copy`] records the runs of
//! words it allocated for the object it returns, and [`TermStore::free`] puts
//! exactly those runs back on per-size free lists. A freed run is handed out again
//! only by a **later** `copy` that asks for a run of the same size, never before,
//! so reclaiming one object can never invalidate another live term. Blocks
//! themselves are released as a unit by [`TermStore::reset`] (which backs
//! `delete_all_objects`) and by [`Drop`].
//!
//! This is the *only* module of the crate allowed to contain `unsafe` (raw term
//! reads/writes) alongside the tuple-element accessor in [`crate::key`]; every
//! `unsafe fn` carries a `# Safety` section and every `unsafe` block a preceding
//! `// SAFETY:` rationale (workspace `CONTEXT.md` §3).
//!
//! # The copied subset (M1)
//!
//! [`TermStore::copy`] reproduces a term graph faithfully for: immediates
//! (returned unchanged — their value is the word), cons cells, tuples, **flat
//! maps**, and the pointer-free boxed leaves (float, bignum, internal ref, on-heap
//! bitstring). Every other boxed kind — funs, records, external pid/port/ref,
//! sub-bitstrings, off-heap binary references and hash maps — aliases storage that
//! lives outside the term graph (code, another heap, an MSO binary) and is rejected
//! with `badarg`; see the crate `CONTEXT.md`, *Known Issues*.
//!
//! # Flat-map layout (why `copy` reads the size word, not the header arity)
//!
//! A flat map is stored as `[MAP_HEADER_FLATMAP, n, keys, v0 … v(n-1)]`
//! (`../../erts/emulator/beam/erl_term.h:1320-1326` `MAP_HEADER`/`MAP_SZ`;
//! `erl_map.h:185-199`), where word `1` is the **raw pair count** `n` — an untagged
//! integer, *not* a term — and word `2` is the keys tuple term. The map header's
//! arity field is the constant `MAP_HEADER_FLATMAP` template value, so the number
//! of pairs must be read from the size word (`flatmap_get_size`); a copy that
//! dispatched on the header arity would mis-size the block and then dereference the
//! raw size word as if it were a term. [`TermStore::copy`] therefore treats a flat
//! map as `header + size word + keys tuple + n values`.

use core::alloc::Layout;
use core::ptr::NonNull;
use std::collections::HashMap;

use beam_alloc::AllocRef;
use beam_common::{AllocFailure, AllocKind, BeamError, Result};
use beam_term::prelude::*;

/// The number of words a fresh store block holds when the request is smaller
/// (`CHUNK`).
const BLOCK_WORDS: usize = 64;

/// One contiguous run of words handed out by [`TermStore::alloc_words`].
///
/// A run is identified by its block *index* (blocks are only ever appended, so an
/// index stays valid for as long as the block lives) plus a word offset inside
/// that block. Runs are the unit of per-object bookkeeping: [`TermStore::copy`]
/// records the runs it allocated for the object it returns and [`TermStore::free`]
/// returns exactly those runs to the store.
#[derive(Copy, Clone, Debug)]
struct AllocSlice {
    /// Index into [`TermStore::blocks`].
    block: usize,
    /// Offset, in [`Eterm`] words, of the run's first word inside its block.
    offset: usize,
    /// Number of words in the run.
    words: usize,
}

/// One owned, never-relocating block of the store's bump arena.
struct StoreBlock {
    /// Base pointer of this block's word storage.
    base: NonNull<Eterm>,
    /// Number of [`Eterm`] words this block can hold.
    capacity: usize,
    /// Words handed out of this block so far (the bump high-water mark). A block is
    /// never reallocated or moved, so `base + used` stays stable for as long as the
    /// store holds the block. The mark is never rewound: words reclaimed by
    /// [`TermStore::free`] become reusable through the free lists, not by re-bumping
    /// over them.
    used: usize,
}

/// The layout a block of `capacity` words is allocated with (and freed with).
///
/// `Layout::array` is deterministic, so one capacity always yields one layout —
/// which is what lets [`Drop`] recover, from the stored capacity alone, the exact
/// layout a block must be freed with.
fn block_layout(capacity: usize) -> Result<Layout> {
    Layout::array::<Eterm>(capacity)
        .map_err(|_| BeamError::Alloc(AllocFailure::new(AllocKind::Other, capacity)))
}

/// A per-table bump arena over table-owned term storage with per-object
/// reclamation.
///
/// Created with an explicit allocator ([`TermStore::new`]) or the registered ETS
/// allocator ([`TermStore::with_default_allocator`]). Dropping the store frees
/// every owned block through the allocator.
pub struct TermStore {
    /// The allocator every block is (de)allocated through.
    alloc: AllocRef,
    /// The chain of owned blocks, in allocation order. Between [`TermStore::reset`]
    /// calls the chain only ever grows, so a block index is a stable name for the
    /// block's words.
    blocks: Vec<StoreBlock>,
    /// Words currently live in `blocks`: handed out and not yet returned by
    /// [`TermStore::free`]. This is what [`TermStore::words`] reports.
    live_words: usize,
    /// Runs returned by [`TermStore::free`] (or rolled back from a failed
    /// [`TermStore::copy`]) and available for reuse, bucketed by their **exact**
    /// word count. An exact-fit bucket keeps a reuse perfect, so a reused run never
    /// overlaps a live term and no word is wasted.
    free_runs: Vec<(usize, Vec<AllocSlice>)>,
    /// The runs the [`TermStore::copy`] call currently in progress has allocated.
    /// Drained when it completes (into `tracked`) or fails (back to `free_runs`).
    pending: Vec<AllocSlice>,
    /// The runs backing every live object, keyed by the root [`TermStore::copy`]
    /// returned for it — the handle [`TermStore::free`] is given. An object whose
    /// copy consumed no words (an immediate) owns no storage and is not tracked.
    tracked: HashMap<Eterm, Vec<AllocSlice>>,
}

impl TermStore {
    /// A store allocating through `alloc`.
    pub fn new(alloc: AllocRef) -> TermStore {
        TermStore {
            alloc,
            blocks: Vec::new(),
            live_words: 0,
            free_runs: Vec::new(),
            pending: Vec::new(),
            tracked: HashMap::new(),
        }
    }

    /// A store allocating through the registered ETS allocator
    /// ([`beam_alloc::AllocType::Ets`]).
    pub fn with_default_allocator() -> TermStore {
        // `allocator` hands out the process-wide `&'static AllocRef`; clone the
        // `Arc` so the store owns a handle that outlives the registry borrow.
        TermStore::new(beam_alloc::allocator(beam_alloc::AllocType::Ets).clone())
    }

    /// Deep-copies the term graph rooted at `root` into table-owned memory and
    /// returns the new root. Immediates are returned unchanged (copied by value).
    ///
    /// Fails with `badarg` for term kinds outside the supported subset (see the
    /// crate `CONTEXT.md`, Known Issues). A failed copy hands the words it had
    /// already allocated back to the store, so it leaks nothing.
    ///
    /// `root` must be a live, well-formed term graph — the same precondition
    /// [`beam_term::compare::cmp`] states (a `Eterm` is a raw machine word, so no
    /// safe function can re-check it).
    pub fn copy(&mut self, root: Eterm) -> Result<Eterm> {
        debug_assert!(
            self.pending.is_empty(),
            "a previous `copy` left its runs pending"
        );
        self.pending.clear();
        match self.copy_term(root) {
            Ok(copied) => {
                if !self.pending.is_empty() {
                    // Only a boxed/list root can have consumed words, so this is
                    // exactly the "the object owns storage" case.
                    self.tracked
                        .insert(copied, core::mem::take(&mut self.pending));
                }
                Ok(copied)
            }
            Err(err) => {
                // The partial copy is unreachable — its root was never handed to a
                // caller — so return its runs instead of leaking them.
                for run in core::mem::take(&mut self.pending) {
                    self.release(run);
                }
                Err(err)
            }
        }
    }

    /// Returns the table-owned storage of the object whose root
    /// [`copy`](TermStore::copy) returned as `root`, and reports the number of
    /// words reclaimed.
    ///
    /// The reclaimed runs go onto the store's free lists and are handed out again
    /// only by a **later** `copy` that needs a run of the same size, so no other
    /// live term is ever invalidated. After this call `root` — and every interior
    /// pointer into it — dangles and must not be read.
    ///
    /// Freeing a term this store never copied (an immediate, a foreign term, or a
    /// root that was already freed) is a harmless no-op that returns `0`.
    ///
    /// An `Eterm` is a raw word, so a freed root stays *nameable*: once the store
    /// hands that address out again, a stale handle for the old object equals the new
    /// root, and freeing it would reclaim the **new** object. A caller must therefore
    /// drop its handle when it frees — the table does, since it removes the object
    /// from its interior in the same step.
    pub fn free(&mut self, root: Eterm) -> usize {
        let runs = match self.tracked.remove(&root) {
            Some(runs) => runs,
            None => return 0,
        };
        let mut words = 0;
        for run in runs {
            words += run.words;
            self.release(run);
        }
        words
    }

    /// Releases **every** block this store owns through the allocator, so the
    /// memory it holds drops to ~0 — the storage side of `ets:delete_all_objects`.
    ///
    /// Every term the store has handed out dangles afterwards, and the store is
    /// immediately reusable (later [`copy`](TermStore::copy) calls map fresh
    /// blocks).
    pub fn reset(&mut self) {
        for block in core::mem::take(&mut self.blocks) {
            let layout = block_layout(block.capacity)
                .expect("a capacity that was allocated has a valid layout");
            // SAFETY: `block.base` came from `self.alloc.alloc` with exactly this
            // layout (recomputed from the stored capacity) and is freed here exactly
            // once — the block was just moved out of `self.blocks`, so nothing else
            // can free it.
            unsafe { self.alloc.dealloc(block.base.cast::<u8>(), layout) };
        }
        self.live_words = 0;
        self.free_runs.clear();
        self.pending.clear();
        self.tracked.clear();
    }

    /// The number of words currently **live** in this store: handed out by
    /// [`copy`](TermStore::copy) and not yet returned by
    /// [`free`](TermStore::free). It never counts a word twice and never
    /// underflows.
    pub fn words(&self) -> usize {
        self.live_words
    }

    /// The number of blocks this store owns.
    pub fn blocks(&self) -> usize {
        self.blocks.len()
    }

    /// The bytes of term storage this store currently holds from its allocator:
    /// the sum of its blocks' capacities. Freed runs are still held — their block
    /// stays mapped until [`reset`](TermStore::reset) — so the figure never
    /// under-reports reclaimable storage. (It excludes this struct's own
    /// bookkeeping vectors.)
    pub fn memory(&self) -> usize {
        self.blocks
            .iter()
            .map(|block| block.capacity)
            .sum::<usize>()
            * core::mem::size_of::<Eterm>()
    }

    /// Reserves `n` contiguous words of table-owned storage and returns their
    /// address. Blocks are never moved or reallocated, so a returned pointer stays
    /// valid (and address-stable) until it is freed.
    ///
    /// A run returned by [`free`](TermStore::free) is preferred over bumping, but a
    /// run is only ever accepted when it is **exactly** `n` words long, so no live
    /// word is ever overwritten.
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        // Reuse path: a freed run of exactly `n` words. Only `free` puts runs here,
        // so the run is not live and cannot alias any term still in use.
        if let Some(run) = self.take_free_run(n) {
            // SAFETY: `run` was handed out of one of this store's blocks by a
            // previous `alloc_words` call (and then returned by `free`); blocks are
            // freed only by `reset`/`Drop`, both of which also discard every free
            // run, so `base + offset` addresses live storage.
            let p = unsafe { self.run_addr(run) };
            self.commit(run);
            return Ok(p);
        }
        // Fast path: the current (last) block still has room for `n` words.
        if let Some(run) = self.bump_last_block(n) {
            // SAFETY: `bump_last_block` reserved `n` words at this block's old bump
            // offset. `used + n <= capacity`, so the words are inside this block's
            // allocation — exclusively owned by the store and `Eterm`-aligned
            // (blocks are allocated with `align_of::<Eterm>()`).
            let p = unsafe { self.run_addr(run) };
            self.commit(run);
            return Ok(p);
        }
        // Slow path: map one fresh block of at least `n` words, so a single term
        // never spans two blocks and its words stay contiguous.
        let capacity = n.max(BLOCK_WORDS);
        let layout = block_layout(capacity)?;
        let raw = self.alloc.alloc(layout)?;
        // The allocator honours `layout.align()` (== `align_of::<Eterm>()`), so
        // `raw` already is a valid `Eterm` pointer: reinterpreting it needs no
        // `unsafe`.
        let base = raw.cast::<Eterm>();
        let block = self.blocks.len();
        self.blocks.push(StoreBlock {
            base,
            capacity,
            used: n,
        });
        self.commit(AllocSlice {
            block,
            offset: 0,
            words: n,
        });
        Ok(base.as_ptr())
    }

    /// Reserves `n` words out of the store's current (last) block when it has room,
    /// reporting the run reserved. Nothing is committed: the caller must
    /// [`commit`](TermStore::commit) the run.
    fn bump_last_block(&mut self, n: usize) -> Option<AllocSlice> {
        let block = self.blocks.len().checked_sub(1)?;
        let last = &mut self.blocks[block];
        if last.capacity - last.used < n {
            return None;
        }
        let offset = last.used;
        last.used += n;
        Some(AllocSlice {
            block,
            offset,
            words: n,
        })
    }

    /// Pops a free run of exactly `n` words, if the store has one.
    fn take_free_run(&mut self, n: usize) -> Option<AllocSlice> {
        let bucket = self.free_runs.iter().position(|(words, _)| *words == n)?;
        let run = self.free_runs[bucket].1.pop();
        if self.free_runs[bucket].1.is_empty() {
            // The bucket is keyed by size, so it can never be refilled by a
            // different size; dropping it keeps the list short.
            self.free_runs.swap_remove(bucket);
        }
        run
    }

    /// Counts a reserved run live and records it for the
    /// [`copy`](TermStore::copy) in progress, so that a completed copy can hand the
    /// run to [`free`](TermStore::free).
    fn commit(&mut self, run: AllocSlice) {
        self.live_words += run.words;
        self.pending.push(run);
    }

    /// Returns a committed run to the free lists, for reuse by a later
    /// [`alloc_words`](TermStore::alloc_words).
    fn release(&mut self, run: AllocSlice) {
        self.live_words = self
            .live_words
            .checked_sub(run.words)
            .expect("a released run was counted live when it was handed out");
        match self
            .free_runs
            .iter_mut()
            .find(|(words, _)| *words == run.words)
        {
            Some((_, runs)) => runs.push(run),
            None => self.free_runs.push((run.words, vec![run])),
        }
    }

    /// The address of `run`'s first word.
    ///
    /// # Safety
    ///
    /// `run` must name words inside one of this store's blocks (a run handed out by
    /// [`alloc_words`](TermStore::alloc_words), or one returned to the free lists
    /// by [`release`](TermStore::release)), and that block must still be alive.
    unsafe fn run_addr(&self, run: AllocSlice) -> *mut Eterm {
        // SAFETY: guaranteed by the caller — `run.block` indexes a live block and
        // `offset` lies inside its allocation.
        unsafe { self.blocks[run.block].base.as_ptr().add(run.offset) }
    }

    /// The recursive core of [`TermStore::copy`].
    fn copy_term(&mut self, term: Eterm) -> Result<Eterm> {
        if is_immediate(term) {
            // Small ints, atoms, `NIL`, catch terms and internal pid/ports carry
            // their value in the word itself — nothing to copy.
            return Ok(term);
        }
        if is_header(term) {
            // A bare header word (`TAG_PRIMARY_HEADER`) is never a standalone value
            // (this includes the debug `THE_NON_VALUE`). Reject it before any
            // dereference: the C `is_boxed` bit trick is true for this pattern too.
            return Err(crate::error::badarg());
        }
        if is_list(term) {
            // SAFETY: a `TAG_PRIMARY_LIST` word is a live, 2-word `[CAR, CDR]` cell.
            let (car, cdr) = unsafe { (car(term), cdr(term)) };
            let car = self.copy_term(car)?;
            let cdr = self.copy_term(cdr)?;
            let p = self.alloc_words(2)?;
            // SAFETY: `p` addresses 2 freshly reserved, exclusively-owned words.
            unsafe {
                *p.add(CAR_OFFSET) = car;
                *p.add(CDR_OFFSET) = cdr;
            }
            return Ok(make_list(p as *const Eterm));
        }
        if is_boxed(term) {
            // SAFETY: a boxed word points at a live, aligned header word.
            let hdr = unsafe { *boxed_val(term) };
            return self.copy_boxed(term, hdr);
        }
        // Unreachable for a well-formed term: `is_immediate`, `is_header`, `is_list`
        // and `is_boxed` partition the four primary-tag patterns. Kept as a total,
        // non-panicking fallback rather than an `unreachable!()`.
        Err(crate::error::badarg())
    }

    /// Copies a boxed term, dispatching on its header subtag.
    ///
    /// `hdr` is the already-read header word ([`copy_term`](TermStore::copy_term)
    /// reads it to classify the term).
    fn copy_boxed(&mut self, term: Eterm, hdr: Eterm) -> Result<Eterm> {
        match BoxedTag::from_raw(header_subtag(hdr)) {
            Some(BoxedTag::ArityVal) => {
                let arity = thing_arityval(hdr);
                let p = self.alloc_words(arity + 1)?;
                // SAFETY: `p` addresses `arity + 1` freshly reserved words; the
                // header is written before any element.
                unsafe { *p = hdr };
                for i in 0..arity {
                    // SAFETY: `term` is a live tuple of `arity` elements, so `i` is
                    // within its payload.
                    let elem = unsafe { tuple_element(term, i) };
                    let elem = self.copy_term(elem)?;
                    // SAFETY: element slot `1 + i` is inside the `arity + 1` words.
                    unsafe { *p.add(1 + i) = elem };
                }
                Ok(make_boxed(p as *const Eterm))
            }

            // Pointer-free leaves: the whole thing is the header plus raw payload
            // words with no interior term pointers, so it is copied verbatim.
            // `HeapBits` belongs here deliberately — an **on-heap** bitstring owns
            // its data words, unlike the off-heap `BinRef`/`SubBits` kinds below.
            Some(BoxedTag::Float)
            | Some(BoxedTag::PosBig)
            | Some(BoxedTag::NegBig)
            | Some(BoxedTag::Ref)
            | Some(BoxedTag::HeapBits) => self.copy_leaf(term, hdr),

            Some(BoxedTag::Map) => {
                if is_flatmap_header(hdr) {
                    self.copy_flatmap(term, hdr)
                } else {
                    // Hash maps (HAMT heads, bitmap/array nodes and collision nodes)
                    // share node blocks reachable from several terms; not copyable
                    // in M1.
                    Err(crate::error::badarg())
                }
            }

            // Fun, Record, SubBits, BinRef, ExternalPid/Port/Ref and the reserved
            // subtags: each aliases storage outside the term graph.
            _ => Err(crate::error::badarg()),
        }
    }

    /// Copies a pointer-free boxed leaf (`hdr` is its header word) verbatim.
    fn copy_leaf(&mut self, term: Eterm, hdr: Eterm) -> Result<Eterm> {
        // A float/bignum/ref/heap-bitstring thing is `header_arity(hdr)` payload
        // words following its header.
        let n = header_arity(hdr) + 1;
        let p = self.alloc_words(n)?;
        // SAFETY: `p` addresses `n` freshly reserved words and the source thing is
        // exactly `n` words long. Both regions are live, `Eterm`-aligned and
        // non-overlapping (the destination is table-owned, the source is the
        // caller's), so a word-for-word copy is sound.
        unsafe { core::ptr::copy_nonoverlapping(boxed_val(term), p, n) };
        Ok(make_boxed(p as *const Eterm))
    }

    /// Copies a flat map (`term`'s header word is `hdr`, a `MAP_HEADER_FLATMAP`).
    ///
    /// Layout (see the module docs): `[header, n, keys, v0 … v(n-1)]`, with `n` a
    /// raw integer. The header and the size word are reproduced verbatim; the keys
    /// tuple and every value are deep-copied.
    fn copy_flatmap(&mut self, term: Eterm, hdr: Eterm) -> Result<Eterm> {
        // SAFETY: `term` is a live flat map, so word `1` of its block is the size
        // word (a raw pair count) — readable, and not a term.
        let n = unsafe { (*boxed_val(term).add(1)).raw() };
        // `header + size word + keys + n values`.
        let p = self.alloc_words(n + 3)?;
        // SAFETY: `p` addresses `n + 3` freshly reserved words; the header and the
        // raw size word are written before any term slot. Word `1` of the source is
        // the size word itself, so it is copied word-for-word instead of being
        // re-encoded as a term.
        unsafe {
            *p = hdr;
            *p.add(1) = *boxed_val(term).add(1);
        }
        // SAFETY: word `2` of a flat-map block is the keys tuple term.
        let keys = unsafe { *boxed_val(term).add(2) };
        let keys = self.copy_term(keys)?;
        // SAFETY: the keys slot is inside the freshly reserved block.
        unsafe { *p.add(2) = keys };
        for i in 0..n {
            // SAFETY: value slot `3 + i` is inside the source's `n + 3` words.
            let value = unsafe { *boxed_val(term).add(3 + i) };
            let value = self.copy_term(value)?;
            // SAFETY: the value slot is inside the reserved block.
            unsafe { *p.add(3 + i) = value };
        }
        Ok(make_boxed(p as *const Eterm))
    }
}

impl Drop for TermStore {
    fn drop(&mut self) {
        // Route the release through `reset` rather than duplicating the block loop,
        // so the free/reuse bookkeeping is torn down with the blocks in one place.
        self.reset();
    }
}

#[cfg(test)]
mod tests;
