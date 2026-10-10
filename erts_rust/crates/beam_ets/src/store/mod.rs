// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

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
//! [`TermStore::relinquish`] is the third disposition: an object is handed to the
//! caller (`ets:take/2`). Its words stay valid and are never handed out again, but
//! they stop counting toward [`TermStore::words`] — the arena cannot shrink, so the
//! words remain held by their block until `reset`/`Drop`, the same lifetime every
//! read result already has.
//!
//! This is the *only* module of the crate allowed to contain `unsafe` (raw term
//! reads/writes) alongside the tuple-element accessor in [`crate::key`]; every
//! `unsafe fn` carries a `# Safety` section and every `unsafe` block a preceding
//! `// SAFETY:` rationale (workspace `CONTEXT.md` §3).
//!
//! # The copied subset
//!
//! [`TermStore::copy`] reproduces a term graph faithfully for every kind the oracle
//! accepts in `ets:insert/2`: immediates (returned unchanged — their value is the
//! word), cons cells, tuples, **flat maps**, **hash maps** (HAMT nodes, collision
//! nodes and their leaf cells, copied node by node so no `AtomTable` is needed),
//! **funs** (all three kinds — the self-contained header/descriptor words verbatim
//! plus a deep-copied environment), the pointer-free boxed leaves (float, bignum,
//! internal ref, on-heap bitstring), **external pid/port/reference** (the raw data
//! words, with the `node` term slot deep-copied), and **sub-bitstrings** / off-heap
//! refc binaries, which are re-materialised as self-contained on-heap bitstrings
//! (the payload bytes are gathered and re-written, so no off-heap pointer survives).
//!
//! The residual typed gaps (`badarg`, never a wrong result) are terms the oracle
//! cannot produce here or that are not standalone terms at all: a bare `BinRef` (it
//! is only ever reached as the `orig` of the refc pair [`TermStore::copy_sub_bits`]
//! reconstructs), a native record (`RECORD_SUBTAG` — an OTP-29 feature this
//! toolchain does not enable), the reserved boxed subtags (`0x04`/`0x3C`), a bare
//! header word, and a malformed `ErlSubBits` whose base pointer is null or whose
//! `end` precedes its `start`. See the crate `CONTEXT.md`, *Known Issues*.
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

/// Whether `term` is a literal this store may **share** (return as-is) rather than
/// deep-copy.
///
/// ETS shares literals instead of duplicating them: the C sizes every copied term
/// with `litopt`, which skips each literal outside the purge area
/// (`copy.c:105`/`119`), and takes the same shortcut for the global empty tuple
/// whatever the option (`copy.c:130-140`, `ERTS_GLOBAL_LIT_EMPTY_TUPLE`).
///
/// This store can only *prove* liveness for the **global empty tuple** — the one
/// literal `beam_term::tuple::tuple0` backs by a leaked, never-reclaimed word.
/// A literal built in a `beam_term::literal::LiteralArea` is deliberately **not**
/// shared: that area can be dropped (the C guards exactly this with
/// `in_literal_purge_area`), and this crate holds no handle that would keep it
/// alive, so retaining such a pointer in table storage would be a dangling read.
/// Those literals are deep-copied instead, which is always correct.
fn is_shareable_literal(term: Eterm) -> bool {
    // `is_literal` is a bare bit test that is only meaningful on a BOXED/LIST word;
    // `copy_term` calls this only after ruling out the immediate and header tags,
    // so `term` is one of those two. Testing it first keeps the (atomic) global
    // lookup off the common path of every ordinary boxed/list subterm.
    is_literal(term) && term == tuple0()
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

// `unsafe impl`, not derived, because `StoreBlock` holds a `NonNull<Eterm>`: the
// compiler cannot know the arena is an owned allocation rather than an alias of
// thread-local or aliased state.
//
// SAFETY: a `TermStore` is a self-contained *owner* of term storage, so moving one
// across threads is a plain address reinterpretation with no thread-affine state
// to rebind or invalidate. Every field is plain data — `StoreBlock { base:
// NonNull<Eterm>, capacity, used }`, the block `Vec`, the `live_words` counter and
// `free_runs`/`pending`/`tracked` (plain `Vec`s/maps of `AllocSlice`/`Eterm`) — and
// none of them is thread-affine: no TLS handle, no file descriptor, no `Rc`, no
// `Cell`. `base` points into a block allocated through the store's own
// `AllocRef = Arc<dyn Allocator>`, and `beam_alloc::Allocator` is declared
// `Send + Sync + 'static` (`beam_alloc/src/allocator.rs`), so the `Arc` may itself
// be moved to — and its blocks freed on — the destination thread.
unsafe impl Send for TermStore {}

// SAFETY: `TermStore` has **no interior mutability**. Every operation that hands
// out or writes storage — `copy`/`free`/`relinquish`/`reset`, i.e. everything that
// dereferences a block's `base` — takes `&mut self`, so the borrow checker already
// serialises it; the only `&self` methods (`words`/`blocks`/`memory`) merely read
// the block vector's length/base/capacity bookkeeping, never a word through a block
// pointer and never a field that another thread could be mutating. Sharing the
// store is only ever reached through `Table`, and `Table` hands out shared access
// exclusively through `SharedTable`'s `Arc<RwLock<Table>>`: a reader must hold the
// read guard to obtain the `&Table` (hence `&TermStore`), and a writer takes the
// same lock exclusively, so no `&TermStore` can overlap a `&mut TermStore` across
// threads. A shared `&TermStore` therefore performs no data race — which is exactly
// `Sync`. (The term-layout writers that write raw words through these pointers,
// `run_addr`/`copy_term` below, are `&mut self` entry points reached from `copy`.)
unsafe impl Sync for TermStore {}

// Compile-time proof that the two hand-written `unsafe impl`s above stay *sound*:
// if `TermStore` — or the `AllocRef` block owner it hands blocks to — ever stopped
// being `Send + Sync`, type-checking this (never-called) closure fails the build,
// instead of the unsound `unsafe impl` silently surviving. The same assertion is
// instantiated for `Table`/`SharedTable`/`TermStore` in `tests/ets_shared.rs`.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TermStore>();
    assert_send_sync::<AllocRef>();
};
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

    /// Hands the storage of the object whose root [`copy`](TermStore::copy)
    /// returned as `root` over to the caller — the "check out" behind `ets:take/2`.
    ///
    /// After this call the object is still **valid** for the store's lifetime (its
    /// words are *not* returned to the free lists, so no later `copy` can overwrite
    /// them), but it is no longer counted as table-owned storage: [`words`](TermStore::words)
    /// drops by exactly its word count, so an object that has left the table stops
    /// counting toward `ets:info(Tab, memory)`. The arena itself never shrinks, so the
    /// words stay held by their block until [`reset`](TermStore::reset)/drop releases
    /// it — the same lifetime guarantee every read result already has.
    ///
    /// This is the difference from [`free`](TermStore::free): `free` reclaims the words
    /// for reuse by a later `copy` (which would then hand the caller's pointer out
    /// again), whereas `relinquish` leaves them alone. A checked-out root must not be
    /// passed back to `free` or `relinquish` — the second call is a no-op (the root is
    /// no longer tracked) that returns `0`.
    ///
    /// Relinquishing a term this store never copied (an immediate, a foreign term, or
    /// a root already freed/checked out) is a harmless no-op that returns `0`.
    pub fn relinquish(&mut self, root: Eterm) -> usize {
        let runs = match self.tracked.remove(&root) {
            Some(runs) => runs,
            None => return 0,
        };
        let mut words = 0;
        for run in runs {
            words += run.words;
            self.live_words = self
                .live_words
                .checked_sub(run.words)
                .expect("a checked-out run was counted live when it was handed out");
            // Deliberately **not** `release`d: the words stay valid and are never
            // handed out again, so a later `copy` cannot overwrite the caller's term.
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
    /// [`free`](TermStore::free) or handed to a caller by
    /// [`relinquish`](TermStore::relinquish). It never counts a word twice and
    /// never underflows.
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
        if is_shareable_literal(term) {
            // A `'static` global literal is **shared, not copied** — exactly what
            // the C does for `ERTS_GLOBAL_LIT_EMPTY_TUPLE` (`copy.c:130-140`) and,
            // more generally, for any literal with `litopt` that is not in the
            // literal purge area (`copy.c:105`/`119`). The global empty tuple is the
            // *only* literal whose liveness this crate can prove (it is one leaked,
            // never-reclaimed word — see `beam_term::tuple::tuple0`), so it is the
            // only one it shares. See [`is_shareable_literal`].
            return Ok(term);
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
                    // A >32-key map is a HAMT: a head node (array/bitmap) plus inner
                    // bitmap nodes and collision nodes, with the leaf `[Key, Value]`
                    // cons cells. Every node block is copied recursively into table
                    // storage (see [`TermStore::copy_hashmap`]).
                    self.copy_hashmap(term, hdr)
                }
            }

            // A fun (local `make_fun3`, external `EXPORT_EXT` or imported
            // `NEW_FUN_EXT`): the header and descriptor words are self-contained
            // (atoms + raw scalars), the environment is deep-copied.
            Some(BoxedTag::Fun) => self.copy_fun(term, hdr),

            // A sub-bitstring (a match context or a refc binary's window): its data
            // lives in storage outside the term graph (off-heap payload, or another
            // heap), so it is re-materialised as a self-contained on-heap bitstring.
            Some(BoxedTag::SubBits) => self.copy_sub_bits(term),

            // External pid / port / reference: the three-word head plus a raw
            // `u32`/`u64` payload — no off-heap pointer survives the copy.
            Some(BoxedTag::ExternalPid)
            | Some(BoxedTag::ExternalPort)
            | Some(BoxedTag::ExternalRef) => self.copy_external(term),

            // A bare `BinRef` is never a standalone term (it is reached only as the
            // `orig` of the refc pair `copy_sub_bits` reconstructs), a native record
            // (`RECORD_SUBTAG`) is an OTP-29 feature this toolchain does not enable,
            // and `0x04`/`0x3C` are reserved subtags: each is a typed `badarg`.
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

    /// Copies a **hash map** node (`term`'s header word is `hdr`, not a flat-map
    /// header).
    ///
    /// A HAMT node is `[header][arity words][slot …]`: the two head kinds
    /// (`MAP_HEADER_TAG_HAMT_HEAD_ARRAY`/`_BITMAP`) carry one raw `size` word after
    /// the header (`header_arity(hdr) == 1`), an inner node (`_NODE_BITMAP`) none
    /// (`header_arity(hdr) == 0`), and one slot per set occupancy bit
    /// (`map_header_val(hdr).count_ones()`). A slot is either a `[Key, Value]` cons
    /// cell, a sub-node, or a collision node (an ordinary arityval tuple of cons
    /// cells); each is copied recursively. This reproduces the ERTS interior exactly,
    /// without needing the `AtomTable` that `beam_term::map::write_map` requires.
    fn copy_hashmap(&mut self, term: Eterm, hdr: Eterm) -> Result<Eterm> {
        let src = boxed_val(term);
        // SAFETY: `term` is a live hash-map node whose header word is `hdr`.
        let words = unsafe { object_words(hdr, src) };
        let arity = header_arity(hdr);
        let nslots = map_header_val(hdr).count_ones() as usize;
        let p = self.alloc_words(words)?;
        // SAFETY: `p` addresses `words` freshly reserved words; the header plus the
        // head kinds' single raw `size` word are never terms, so they are reproduced
        // word for word.
        unsafe { core::ptr::copy_nonoverlapping(src, p, 1 + arity) };
        for i in 0..nslots {
            let off = 1 + arity + i;
            // SAFETY: slot `off` lies inside the source node's `words` words.
            let slot = unsafe { *src.add(off) };
            let slot = if is_list(slot) {
                self.copy_term(slot)?
            } else if is_boxed_term(slot) {
                // SAFETY: a non-leaf slot is a live sub-node or collision tuple.
                let shdr = unsafe { *boxed_val(slot) };
                self.copy_boxed(slot, shdr)?
            } else {
                return Err(crate::error::badarg());
            };
            // SAFETY: slot `off` lies inside the freshly reserved block.
            unsafe { *p.add(off) = slot };
        }
        Ok(make_boxed(p as *const Eterm))
    }

    /// Copies a **fun** block (`term`'s header word is `hdr`).
    ///
    /// The header and the descriptor words carry only atom immediates and raw
    /// scalars (the M1 inlined-descriptor substitution — `beam_term::fun`), so they
    /// are reproduced word for word; the `num_free` environment words
    /// (`env[i]` at `1 + desc_words(kind) + i`) are deep-copied. One arm covers all
    /// three kinds ([`FunKind::Local`]/[`FunKind::External`]/[`FunKind::Imported`]).
    fn copy_fun(&mut self, term: Eterm, hdr: Eterm) -> Result<Eterm> {
        let desc = fun_desc_words(fun_kind_of_header(hdr));
        let num_free = fun_num_free(hdr);
        let words = 1 + desc + num_free;
        let p = self.alloc_words(words)?;
        // SAFETY: `p` addresses `words` freshly reserved words and the source fun block
        // is exactly that size; header + descriptor contain no interior term pointer.
        unsafe { core::ptr::copy_nonoverlapping(boxed_val(term), p, 1 + desc) };
        for i in 0..num_free {
            // SAFETY: `env[i]` lives at `1 + desc + i` inside the source fun block.
            let var = unsafe { *boxed_val(term).add(1 + desc + i) };
            let var = self.copy_term(var)?;
            // SAFETY: the environment slot is inside the reserved block.
            unsafe { *p.add(1 + desc + i) = var };
        }
        Ok(make_boxed(p as *const Eterm))
    }

    /// Copies a **sub-bitstring** (`ErlSubBits`) as a self-contained on-heap
    /// bitstring, so the copy never aliases the source's storage — an off-heap
    /// refc payload (or the source's own heap) that may be reclaimed independently.
    ///
    /// The bit window (`start`/`end`, a bit range, not a byte range) is gathered
    /// through the source's masked base pointer and re-materialised with
    /// `beam_term::bitstring::write_heap_bits` over `heap_bits_size_words(size_bits)`
    /// words. A window whose base is null (a malformed synthetic `ErlSubBits`, which
    /// the C would dereference) or whose `end` precedes its `start` is a typed
    /// `badarg` rather than a fault.
    fn copy_sub_bits(&mut self, term: Eterm) -> Result<Eterm> {
        // SAFETY: `term` is a live `ErlSubBits` — the caller dispatched on its header.
        let (base_flags, start, end, _orig) = unsafe { sub_bits_parts(term) };
        let start_bits = start.raw();
        let end_bits = end.raw();
        let size_bits = match end_bits.checked_sub(start_bits) {
            Some(size) => size,
            None => return Err(crate::error::badarg()),
        };
        let base = (base_flags.raw() & !(ERL_SUB_BITS_FLAG_MASK as usize)) as *const u8;
        if base.is_null() && size_bits > 0 {
            // No readable payload: reject instead of dereferencing a null base.
            return Err(crate::error::badarg());
        }
        let bytes = gather_bits(base, start_bits, size_bits);
        let p = self.alloc_words(heap_bits_size_words(size_bits))?;
        // SAFETY: `p` addresses `heap_bits_size_words(size_bits)` words — exactly what
        // `write_heap_bits` writes for a `size_bits`-bit string — and `bytes` holds the
        // gathered window.
        let copied = unsafe { write_heap_bits(p, &bytes, size_bits) }?;
        Ok(copied)
    }

    /// Copies an external pid/port/reference block (`[header, node, next, payload]`).
    ///
    /// The tail words are a raw `u32`/`u64` payload (never terms) and `next` carries
    /// the node creation (the M1 substitution — `beam_term::thing`), so the block is
    /// reproduced verbatim. Word 1 (`node`) **is** a term slot and is deep-copied when
    /// it is not an immediate, so a boxed node never left pointing at the source.
    fn copy_external(&mut self, term: Eterm) -> Result<Eterm> {
        let src = boxed_val(term);
        // SAFETY: `term` is a live external pid/port/reference, so word 0 is its header
        // and the block is `object_words` words long.
        let words = unsafe { object_words(*src, src) };
        let p = self.alloc_words(words)?;
        // SAFETY: `p` addresses `words` freshly reserved words and the source block is
        // exactly that size; both are live and non-overlapping.
        unsafe { core::ptr::copy_nonoverlapping(src, p, words) };
        // SAFETY: word 1 of an external thing is its `node` term slot.
        let node = unsafe { *src.add(1) };
        if !is_immediate(node) {
            let node = self.copy_term(node)?;
            // SAFETY: word 1 lies inside the reserved block.
            unsafe { *p.add(1) = node };
        }
        Ok(make_boxed(p as *const Eterm))
    }
}

/// Gathers `size_bits` bits starting at bit `offset` of `base` into a fresh byte
/// buffer of [`bit_count_to_bytes`]`(size_bits)` bytes, most-significant bit of the
/// first byte first — the ERTS bitstring convention (`beam_term::bitstring`).
///
/// The byte-aligned case (the common one: a full refc binary, or a byte-aligned
/// match window) is a plain slice copy; a non-byte-aligned window is re-packed bit
/// by bit.
///
/// # Safety
///
/// `base` must address storage covering bits `offset .. offset + size_bits` (a null
/// `base` is only ever passed with `size_bits == 0`, which reads nothing).
fn gather_bits(base: *const u8, offset: usize, size_bits: usize) -> Vec<u8> {
    let n_bytes = bit_count_to_bytes(size_bits);
    let mut bytes = vec![0u8; n_bytes];
    if n_bytes == 0 {
        return bytes;
    }
    // SAFETY: the caller guarantees `base` covers the requested bit window.
    unsafe {
        if offset % 8 == 0 {
            let src = core::slice::from_raw_parts(base.add(offset / 8), n_bytes);
            bytes.copy_from_slice(src);
            return bytes;
        }
        for i in 0..size_bits {
            let bit = (*base.add((offset + i) >> 3) >> (7 - ((offset + i) & 7))) & 1;
            bytes[i >> 3] |= bit << (7 - (i & 7));
        }
    }
    bytes
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
