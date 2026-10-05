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

use beam_alloc::AllocRef;
use beam_common::{AllocFailure, AllocKind, BeamError, Result};
use beam_term::prelude::*;

/// The number of words a fresh store block holds when the request is smaller
/// (`CHUNK`).
const BLOCK_WORDS: usize = 64;

/// One owned, never-relocating block of the store's bump arena.
struct StoreBlock {
    /// Base pointer of this block's word storage.
    base: NonNull<Eterm>,
    /// Number of [`Eterm`] words this block can hold.
    capacity: usize,
    /// Words handed out of this block so far (the bump high-water mark). A block is
    /// never reallocated or moved, so `base + used` stays stable until the store is
    /// dropped.
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
        TermStore {
            alloc,
            blocks: Vec::new(),
            words: 0,
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
    /// crate `CONTEXT.md`, Known Issues).
    ///
    /// `root` must be a live, well-formed term graph — the same precondition
    /// [`beam_term::compare::cmp`] states (a `Eterm` is a raw machine word, so no
    /// safe function can re-check it).
    pub fn copy(&mut self, root: Eterm) -> Result<Eterm> {
        self.copy_term(root)
    }

    /// The total number of words handed out by this store.
    pub fn words(&self) -> usize {
        self.words
    }

    /// The number of blocks this store owns.
    pub fn blocks(&self) -> usize {
        self.blocks.len()
    }

    /// Reserves `n` contiguous words of table-owned storage and returns their
    /// address. Blocks are never moved or reallocated, so a returned pointer stays
    /// valid (and address-stable) for the store's whole lifetime.
    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        // Fast path: the current (last) block still has room for `n` words.
        if let Some(block) = self.blocks.last_mut() {
            if block.capacity - block.used >= n {
                // SAFETY: `used + n <= capacity`, so the `n` words at `base + used`
                // are inside this block's allocation — exclusively owned by the
                // store and `Eterm`-aligned (blocks are allocated with
                // `align_of::<Eterm>()`).
                let p = unsafe { block.base.as_ptr().add(block.used) };
                block.used += n;
                self.words += n;
                return Ok(p);
            }
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
        self.blocks.push(StoreBlock {
            base,
            capacity,
            used: n,
        });
        self.words += n;
        Ok(base.as_ptr())
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
        for block in &self.blocks {
            // SAFETY: `block.base` came from `self.alloc.alloc` with exactly this
            // layout (recomputed from the stored capacity) and is freed here exactly
            // once — the store owns every block and is being dropped.
            //
            // The recomputed layout cannot fail: `alloc_words` only pushes a block
            // after `block_layout(capacity)` succeeded for that same capacity, and
            // `block_layout` is a pure function of it.
            let layout = block_layout(block.capacity)
                .expect("a capacity that was allocated has a valid layout");
            unsafe { self.alloc.dealloc(block.base.cast::<u8>(), layout) };
        }
    }
}

#[cfg(test)]
mod tests {
    //! Copy-boundary tests: structural equality with the source, distinct
    //! table-owned storage, and survival of the source heap's storage.
    //!
    //! Term fixtures are built with [`beam_term`] — the frozen representation is
    //! only ever constructed through its helpers, never by hand-rolling bit
    //! patterns beyond the documented thing layouts in the fixture builders below.

    use super::*;
    use beam_alloc::{AllocatorConfig, CarrierAllocator};

    /// A store over a private, small-carrier allocator, so tests never disturb the
    /// process-wide registry's counters.
    fn test_store() -> TermStore {
        let alloc = CarrierAllocator::new(
            AllocatorConfig::builder()
                .name("beam_ets_store_test")
                .carrier_size(64 * 1024)
                .build(),
        )
        .expect("a fresh carrier allocator must be constructible")
        .share();
        TermStore::new(alloc)
    }

    /// Builds a flat map in the ERTS layout (`erl_term.h:1320-1326`):
    /// `[MAP_HEADER_FLATMAP, n, keys_tuple, v0 … v(n-1)]`, with the keys tuple
    /// allocated just before the map head.
    ///
    /// # Safety
    ///
    /// `heap` must have room for `(1 + n)` (keys tuple) `+ (3 + n)` (map) words and
    /// must outlive the returned term.
    unsafe fn build_flatmap(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let n = pairs.len();
        // SAFETY: `alloc_tuple` hands over `1 + n` exclusively-owned words, all of
        // which are written below before the term escapes.
        let keys = unsafe {
            heap.alloc_tuple(n)
                .expect("heap room for the keys tuple")
        };
        for (i, (key, _)) in pairs.iter().enumerate() {
            // SAFETY: element slot `1 + i` is inside the keys tuple.
            unsafe { *keys.add(1 + i) = *key };
        }
        // SAFETY: `alloc_words` hands over `n + 3` exclusively-owned words, all of
        // which are written below.
        let p = unsafe {
            heap.alloc_words(n + 3)
                .expect("heap room for the flat-map block")
        };
        // SAFETY: the block owns `n + 3` words: `[header, size, keys, v0 … v(n-1)]`.
        unsafe {
            *p = make_header(4, MAP_SUBTAG);
            *p.add(1) = Eterm::from_raw(n);
            *p.add(2) = make_tuple(keys);
            for (i, (_, value)) in pairs.iter().enumerate() {
                *p.add(3 + i) = *value;
            }
        }
        make_boxed(p as *const Eterm)
    }

    /// Builds a 64-bit float thing.
    ///
    /// # Safety
    ///
    /// `heap` must have room for 2 words and must outlive the returned term.
    unsafe fn build_float(heap: &mut Heap, value: f64) -> Eterm {
        // SAFETY: 2 exclusively-owned words, both written before the term escapes.
        let p = unsafe { heap.alloc_words(2).expect("heap room for a float") };
        // SAFETY: `p` owns exactly those 2 words.
        unsafe {
            *p = HEADER_FLONUM;
            *p.add(1) = Eterm::from_raw(value.to_bits() as usize);
        }
        make_boxed(p as *const Eterm)
    }

    /// Builds a one-limb bignum thing (`header + 1 payload word`).
    ///
    /// # Safety
    ///
    /// `heap` must have room for 2 words and must outlive the returned term.
    unsafe fn build_bignum(heap: &mut Heap, negative: bool) -> Eterm {
        let subtag = if negative {
            NEG_BIG_SUBTAG
        } else {
            POS_BIG_SUBTAG
        };
        // SAFETY: 2 exclusively-owned words, both written before the term escapes.
        let p = unsafe { heap.alloc_words(2).expect("heap room for a bignum") };
        // SAFETY: `p` owns exactly those 2 words.
        unsafe {
            *p = make_header(1, subtag);
            *p.add(1) = Eterm::from_raw(0x1234_5678_9ABC_DEF0);
        }
        make_boxed(p as *const Eterm)
    }

    /// Builds an internal reference thing (`header + 2 payload words`).
    ///
    /// # Safety
    ///
    /// `heap` must have room for 3 words and must outlive the returned term.
    unsafe fn build_internal_ref(heap: &mut Heap) -> Eterm {
        // SAFETY: 3 exclusively-owned words, all written before the term escapes.
        let p = unsafe { heap.alloc_words(3).expect("heap room for a ref") };
        // SAFETY: `p` owns exactly those 3 words.
        unsafe {
            *p = make_header(2, REF_SUBTAG);
            *p.add(1) = Eterm::from_raw(0xFFFF_FFFF_0000_0000);
            *p.add(2) = Eterm::from_raw(0x0000_0002_0000_0001);
        }
        make_boxed(p as *const Eterm)
    }

    /// Builds a 16-bit on-heap binary (`[header, size, 1 data word]`).
    ///
    /// # Safety
    ///
    /// `heap` must have room for 3 words and must outlive the returned term.
    unsafe fn build_heap_binary(heap: &mut Heap, value: u16) -> Eterm {
        // SAFETY: 3 exclusively-owned words, all written before the term escapes.
        let p = unsafe { heap.alloc_words(3).expect("heap room for a binary") };
        // SAFETY: `p` owns exactly those 3 words.
        unsafe {
            *p = make_header(2, HEAP_BITS_SUBTAG);
            *p.add(1) = Eterm::from_raw(16); // bit size
            *p.add(2) = Eterm::from_raw(value as usize);
        }
        make_boxed(p as *const Eterm)
    }

    /// Asserts that `copy` reproduced the whole thing of `source` word for word
    /// (used for the pointer-free leaves).
    fn assert_copied_verbatim(name: &str, source: Eterm, copy: Eterm) {
        assert_ne!(boxed_val(copy), boxed_val(source), "{name}: must be copied");
        // SAFETY: `source` is a live thing of `header_arity + 1` words, and its copy
        // is a live thing of the same size.
        unsafe {
            let n = header_arity(*boxed_val(source)) + 1;
            for i in 0..n {
                assert_eq!(
                    *boxed_val(copy).add(i),
                    *boxed_val(source).add(i),
                    "{name}: word {i} differs"
                );
            }
        }
    }

    #[test]
    fn immediates_copy_by_value_and_cost_no_words() {
        let mut store = test_store();
        let terms = [
            make_small(0),
            make_small(1234),
            make_small(-7),
            NIL,
            make_atom(Atom::from_raw(3)),
            make_catch(17),
            make_internal_pid(make_pid_data(1, 2)),
            make_internal_port(make_port_data(9)),
        ];
        for t in terms {
            assert_eq!(store.copy(t).expect("an immediate is copyable"), t);
        }
        assert_eq!(store.words(), 0, "immediates consume no store words");
        assert_eq!(store.blocks(), 0, "immediates map no block");
    }

    #[test]
    fn cons_list_copy_is_equal_and_lives_in_table_storage() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(16);
        let three = heap.cons(make_small(3), NIL).unwrap();
        let two_three = heap.cons(make_small(2), three).unwrap();
        let one_two_three = heap.cons(make_small(1), two_three).unwrap();

        let mut store = test_store();
        let copied = store.copy(one_two_three).expect("a list is copyable");

        assert!(is_list(copied));
        assert_ne!(
            list_val(copied),
            list_val(one_two_three),
            "the copy must live in table-owned storage"
        );
        assert!(
            eq(one_two_three, copied, &atoms),
            "the copy must be structurally equal to the source"
        );
        // SAFETY: both lists are live and the store outlives the test.
        unsafe {
            assert_eq!(car(copied), make_small(1));
            assert_eq!(car(cdr(copied)), make_small(2));
            assert_eq!(car(cdr(cdr(copied))), make_small(3));
            assert_eq!(cdr(cdr(cdr(copied))), NIL);
        }
        // 3 cons cells x 2 words.
        assert_eq!(store.words(), 6);
        assert_eq!(store.blocks(), 1);
    }

    #[test]
    fn tuple_and_nested_tuple_copy() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(32);
        let inner = heap.tuple(&[make_small(7), make_small(8)]).unwrap();
        let outer = heap.tuple(&[make_small(1), inner, make_small(9)]).unwrap();

        let mut store = test_store();
        let copied = store.copy(outer).expect("a tuple is copyable");

        assert!(unsafe { is_tuple(copied) });
        assert_ne!(boxed_val(copied), boxed_val(outer));
        assert!(eq(outer, copied, &atoms), "nested tuples copy structurally");
        // SAFETY: `copied` is a live store-owned tuple.
        unsafe {
            assert_eq!(tuple_size(copied), 3);
            assert_eq!(tuple_element(copied, 0), make_small(1));
            assert_eq!(tuple_element(copied, 2), make_small(9));
            let inner_copy = tuple_element(copied, 1);
            assert!(is_tuple(inner_copy));
            assert_ne!(
                boxed_val(inner_copy),
                boxed_val(inner),
                "the inner tuple must be deep-copied, not aliased"
            );
            assert_eq!(tuple_size(inner_copy), 2);
            assert_eq!(tuple_element(inner_copy, 0), make_small(7));
            assert_eq!(tuple_element(inner_copy, 1), make_small(8));
        }
        // outer (4 words) + inner (3 words).
        assert_eq!(store.words(), 7);
    }

    #[test]
    fn flatmap_copy_round_trips_the_erts_layout() {
        let mut atoms = AtomTable::new();
        let a = atoms.intern("a");
        let b = atoms.intern("b");
        let mut heap = Heap::new(64);
        // SAFETY: the heap has room (keys tuple 3 words + map block 5 words) and
        // outlives the map.
        let map = unsafe {
            build_flatmap(
                &mut heap,
                &[(make_atom(a), make_small(1)), (make_atom(b), make_small(2))],
            )
        };

        let mut store = test_store();
        let copied = store.copy(map).expect("a flat map is copyable");

        assert!(unsafe { is_flatmap(copied) });
        assert_ne!(boxed_val(copied), boxed_val(map));
        // SAFETY: both maps are live across the whole test.
        unsafe {
            let src = boxed_val(map);
            let dst = boxed_val(copied);
            // The header and the raw size word are reproduced verbatim.
            assert_eq!(*dst, *src);
            assert_eq!((*dst.add(1)).raw(), 2, "the raw pair count is preserved");
            // The keys tuple is deep-copied into table storage.
            let keys = *dst.add(2);
            assert!(is_tuple(keys));
            assert_ne!(boxed_val(keys), boxed_val(*src.add(2)));
            assert_eq!(tuple_size(keys), 2);
            assert_eq!(tuple_element(keys, 0), make_atom(a));
            assert_eq!(tuple_element(keys, 1), make_atom(b));
            // The values follow the keys slot.
            assert_eq!(*dst.add(3), make_small(1));
            assert_eq!(*dst.add(4), make_small(2));
        }
        // map block `n + 3` = 5 words + keys tuple `1 + 2` = 3 words.
        assert_eq!(store.words(), 8);
    }

    #[test]
    fn empty_flatmap_copy_keeps_its_three_word_block() {
        let mut heap = Heap::new(16);
        // SAFETY: the heap has room (keys tuple 1 word + map block 3 words).
        let map = unsafe { build_flatmap(&mut heap, &[]) };

        let mut store = test_store();
        let copied = store.copy(map).expect("an empty flat map is copyable");

        assert!(unsafe { is_flatmap(copied) });
        // SAFETY: both maps are live.
        unsafe {
            let src = boxed_val(map);
            let dst = boxed_val(copied);
            assert_eq!(*dst, *src);
            assert_eq!((*dst.add(1)).raw(), 0);
            // The zero-arity keys tuple is still deep-copied.
            let keys = *dst.add(2);
            assert!(is_tuple(keys));
            assert_eq!(tuple_size(keys), 0);
            assert_ne!(boxed_val(keys), boxed_val(*src.add(2)));
        }
        // map 3 words + keys tuple 1 word.
        assert_eq!(store.words(), 4);
    }

    #[test]
    fn flatmap_copy_sizes_from_the_size_word_not_the_header_arity() {
        // A flat-map head whose header arity field is 0 — the shape
        // `beam_term::compare`'s own tests build — still carries its pair count in
        // the size word. Sizing the copy from `map_header_arity` would truncate the
        // block to 3 words and then drop both values.
        let mut heap = Heap::new(32);
        // SAFETY: the heap has room for the keys tuple (3 words) and the block
        // (5 words), and outlives the map.
        let map = unsafe {
            let keys = heap.alloc_tuple(2).expect("heap room for the keys tuple");
            *keys.add(1) = make_small(7);
            *keys.add(2) = make_small(8);
            let p = heap.alloc_words(5).expect("heap room for the map block");
            *p = make_header(0, HAMT_SUBTAG_HEAD_FLATMAP); // arity field is 0 ...
            *p.add(1) = Eterm::from_raw(2); // ... yet there are two pairs
            *p.add(2) = make_tuple(keys);
            *p.add(3) = make_small(10);
            *p.add(4) = make_small(20);
            make_boxed(p as *const Eterm)
        };

        let mut store = test_store();
        let copied = store.copy(map).expect("a flat map is copyable");

        assert!(unsafe { is_flatmap(copied) });
        // SAFETY: both maps are live for the rest of the test.
        unsafe {
            let src = boxed_val(map);
            let dst = boxed_val(copied);
            assert_eq!(*dst, *src);
            assert_eq!((*dst.add(1)).raw(), 2);
            let keys = *dst.add(2);
            assert_ne!(boxed_val(keys), boxed_val(*src.add(2)));
            assert_eq!(tuple_size(keys), 2);
            assert_eq!(tuple_element(keys, 0), make_small(7));
            assert_eq!(tuple_element(keys, 1), make_small(8));
            assert_eq!(*dst.add(3), make_small(10));
            assert_eq!(*dst.add(4), make_small(20));
        }
        // map block `n + 3` = 5 words + keys tuple 3 words.
        assert_eq!(store.words(), 8);
    }

    #[test]
    fn pointer_free_leaves_are_copied_verbatim() {
        let mut heap = Heap::new(64);
        // SAFETY: the heap has room for every fixture below and outlives them.
        let (float, pos_big, neg_big, internal_ref, binary) = unsafe {
            (
                build_float(&mut heap, 1.5),
                build_bignum(&mut heap, false),
                build_bignum(&mut heap, true),
                build_internal_ref(&mut heap),
                build_heap_binary(&mut heap, 0xBEEF),
            )
        };

        let mut store = test_store();
        let float_copy = store.copy(float).expect("a float is copyable");
        assert_copied_verbatim("float", float, float_copy);
        let pos_big_copy = store.copy(pos_big).expect("a bignum is copyable");
        assert_copied_verbatim("pos_big", pos_big, pos_big_copy);
        let neg_big_copy = store.copy(neg_big).expect("a negative bignum is copyable");
        assert_copied_verbatim("neg_big", neg_big, neg_big_copy);
        let ref_copy = store.copy(internal_ref).expect("a ref is copyable");
        assert_copied_verbatim("internal_ref", internal_ref, ref_copy);
        let binary_copy = store.copy(binary).expect("a heap bitstring is copyable");
        assert_copied_verbatim("heap_bitstring", binary, binary_copy);

        // The copies keep their exact classification (and the binary its bit size).
        // SAFETY: every copy is a live store-owned term.
        unsafe {
            assert!(is_float(float_copy));
            assert!(is_big(pos_big_copy));
            assert!(is_big(neg_big_copy));
            assert!(is_internal_ref(ref_copy));
            assert!(is_bitstring(binary_copy));
            assert!(is_binary(binary_copy));
            assert_eq!(bitstring_size(binary_copy), 16);
        }
        // 2 + 2 + 2 + 3 + 3 words.
        assert_eq!(store.words(), 12);
    }

    #[test]
    fn copy_survives_clobbering_and_dropping_the_source_heap() {
        let mut heap = Heap::new(32);
        let inner = heap.tuple(&[make_small(11), make_small(12)]).unwrap();
        let outer = heap.tuple(&[inner, make_small(99)]).unwrap();

        let mut store = test_store();
        let copied = store.copy(outer).expect("a tuple is copyable");

        // Rewind the source arena and write different values over the exact words
        // the source tuples occupied, clobbering them in place.
        heap.reset();
        let _scratch = heap.tuple(&[make_small(-1), make_small(-2)]).unwrap();
        assert_eq!(heap.used(), 3, "the clobber re-used the inner tuple's words");

        // SAFETY: `copied` is store-owned and the store is alive.
        unsafe {
            assert_eq!(tuple_size(copied), 2);
            assert_eq!(tuple_element(copied, 1), make_small(99));
            let inner_copy = tuple_element(copied, 0);
            assert_eq!(tuple_element(inner_copy, 0), make_small(11));
            assert_eq!(tuple_element(inner_copy, 1), make_small(12));
        }

        // Dropping the source arena must not affect the copy either.
        drop(heap);
        // SAFETY: as above.
        unsafe {
            assert_eq!(tuple_element(copied, 1), make_small(99));
        }
    }

    #[test]
    fn block_growth_is_non_relocating() {
        const N: i64 = 64;
        let mut heap = Heap::new(4 * N as usize);
        let sources: Vec<Eterm> = (0..N)
            .map(|i| {
                heap.tuple(&[make_small(i), make_small(i * 10)])
                    .expect("heap room for the source tuples")
            })
            .collect();

        let mut store = test_store();
        let copies: Vec<Eterm> = sources
            .iter()
            .map(|&t| store.copy(t).expect("a tuple is copyable"))
            .collect();

        // Each copy is a 3-word tuple: 192 words, well past `BLOCK_WORDS` (64), so
        // the store must have grown past its first block.
        assert!(
            store.blocks() > 1,
            "expected block growth, got {} block(s)",
            store.blocks()
        );
        assert_eq!(store.words(), 3 * N as usize);

        // Every copy landed in distinct storage...
        let mut addresses: Vec<usize> = copies.iter().map(|&c| boxed_val(c) as usize).collect();
        addresses.sort_unstable();
        let before = addresses.len();
        addresses.dedup();
        assert_eq!(before, addresses.len(), "two copies share storage");

        // ...and every earlier root still reads back its original value, which is
        // what proves the earlier blocks were never moved by the later allocations.
        for (i, &c) in copies.iter().enumerate() {
            // SAFETY: every `c` is store-owned and the store is alive.
            unsafe {
                assert_eq!(tuple_size(c), 2);
                assert_eq!(tuple_element(c, 0), make_small(i as i64), "root {i}");
                assert_eq!(tuple_element(c, 1), make_small(i as i64 * 10), "root {i}");
            }
        }
    }

    #[test]
    fn empty_store_reports_no_words_and_no_blocks() {
        let store = TermStore::with_default_allocator();
        assert_eq!(store.words(), 0);
        assert_eq!(store.blocks(), 0);
    }

    #[test]
    fn dropping_a_populated_store_frees_every_block() {
        let alloc = CarrierAllocator::new(
            AllocatorConfig::builder()
                .name("beam_ets_store_drop_test")
                .carrier_size(64 * 1024)
                .build(),
        )
        .expect("a fresh carrier allocator must be constructible")
        .share();
        let mut heap = Heap::new(64);
        let tail = heap.cons(NIL, NIL).unwrap();
        let list = heap.cons(make_small(1), tail).unwrap();

        {
            let mut store = TermStore::new(alloc.clone());
            store.copy(list).expect("a list is copyable");
            assert!(store.blocks() > 0);
            assert!(
                alloc.stats().live_bytes > 0,
                "the store's block must be live while the store lives"
            );
        }
        assert_eq!(
            alloc.stats().live_bytes,
            0,
            "dropping the store must free every block it owns"
        );
    }

    /// The `badarg` path for term kinds outside the copied subset.
    #[test]
    fn sub_bitstring_is_rejected_with_badarg() {
        let mut heap = Heap::new(16);
        // SAFETY: an `ErlSubBits` thing is 5 words; all of them are written here.
        let sub_bits = unsafe {
            let p = heap.alloc_words(5).expect("heap room for a sub-bits thing");
            *p = HEADER_SUB_BITS;
            *p.add(1) = Eterm::from_raw(0);
            *p.add(2) = Eterm::from_raw(4);
            *p.add(3) = Eterm::from_raw(16);
            *p.add(4) = make_small(0);
            make_boxed(p as *const Eterm)
        };
        let mut store = test_store();
        // `BeamError` has no `PartialEq`, so match the variant.
        assert!(matches!(store.copy(sub_bits), Err(BeamError::BadArg)));
        assert_eq!(store.words(), 0, "a rejected term allocates nothing");
        assert_eq!(store.blocks(), 0);
    }
}
