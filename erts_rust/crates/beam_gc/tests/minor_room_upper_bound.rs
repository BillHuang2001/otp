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

//! **The room a minor requires is a true upper bound on the words it copies.**
//!
//! [`Collector::collect`] picks minor-vs-major and [`Collector::collect_minor`] refuses before
//! copying on **one** input, `minor_room_words` (`src/collector.rs`) — the live promotion's exact
//! word count, `Copier::run`'s output volume. Because the count and the copy share their sizing
//! (`copy::move_size`) and child-slot (`copy::for_each_ref`) primitives, the count must equal the
//! copy volume **for every term shape**, not merely bound it: funs with inlined descriptors,
//! `ErlSubBits` match contexts (5 words, whose `orig` copies its base as a separate object), the
//! whole `[BinRef][ErlSubBits]` refc pair (8 words, copied as two allocations), flat maps with
//! their occupancy/size words and their reserved tail, cons cells and nested tuples.
//!
//! The contract is pinned **from the outside**, through the public API only, on one shape per
//! run:
//!
//! 1. a **measuring** collector with room to spare runs one minor over the shell and reports the
//!    copy volume `W` (`GcStats::words_copied` — the words the copier actually wrote);
//! 2. **(a) exactly `W` words of mature room admits the minor** — `collect_kind(Minor)` is `Ok`,
//!    the returned kind is `Minor` and `words_copied == W`, so the requirement is *not* merely an
//!    upper bound with slack: one word of room would have been wasted;
//! 3. **(b) `W - 1` words refuses it** — the typed heap exhaustion
//!    (`BeamError::Alloc(AllocFailure { kind: AllocKind::Heap, .. })`, whose byte figure is
//!    `W * 8`) with **both spaces and every counter untouched**, so the refusal happens before a
//!    single word is copied (never a half-forwarded nursery).
//!
//! Each shape is probed **twice**, which is what makes the equality a statement about both of the
//! requirement's code paths:
//!
//! * with a **clean** nursery (no dead words) the fill short-circuit answers — the chain-summed
//!   fill *is* the live volume;
//! * with `GARBAGE_WORDS` of **dead** words in the nursery beside the shell the fill exceeds the
//!   live volume, so the read-only live walk runs and must still answer exactly `W` — the dead
//!   words are not a room requirement, and they do not shrink the answer either.
//!
//! The `//!` doc of `tests/escalation_bound.rs` owns the *other* half of the same seam (which
//! collection kind a fill-vs-live requirement selects); this file owns the **equality**.
//!
//! Shapes: `map_and_list` (a 12-key flat map — the `[header][occupancy][size][key,value]…` shape
//! whose writer reserves an exact block — plus a cons list of nested tuples) and
//! `fun_sub_binary_and_refc` (a local fun with two boxed free variables, a 5-word `ErlSubBits`
//! match context over an on-heap bitstring, and an off-heap refc binary's 8-word on-heap pair).
//! The off-heap payload's bytes are never read here — the `[BinRef][ErlSubBits]` pair is. Nothing
//! is timed or sampled: the counts are deterministic, and the shapes are rebuilt identically in
//! each of the three collectors (the same code path over the same terms, so the same volume).
//!
//! `unsafe` below only writes words this test reserved on a heap it owns, or follows a pointer the
//! copier itself just wrote; every block carries a `// SAFETY:` rationale.

use beam_alloc::{default_allocator, RefcBinary};
use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_gc::prelude::{CollectionKind, Collector, HeapPolicy};
use beam_term::prelude::{
    boxed_val, fun_words, make_small, map_words_capacity, write_fun_local, write_heap_bits,
    heap_bits_size_words, write_map, write_refc_binary, write_sub_bits, AtomTable, Eterm, FunKind,
    NIL, ERL_HEAP_BITS_SIZE, ERL_REFC_BITS_SIZE, ERL_SUB_BITS_FLAG_MUTABLE, ERL_SUB_BITS_SIZE,
};

// ---------------------------------------------------------------------------
// Policy + collector helpers
// ---------------------------------------------------------------------------

/// The grown-generation capacity a **measuring** collector is built with: comfortably more than
/// any shell below, so the measuring minor always fits.
const GENEROUS_MATURE_WORDS: usize = 4 * 1024;

/// The nursery capacity every probe is built with. Identical for the measuring and the two
/// boundary collectors, so the shapes are laid out exactly the same way in each of them.
const YOUNG_WORDS: usize = 4 * 1024;

/// Dead words scattered into the nursery beside a shell, to force the requirement's live walk
/// (the fill short-circuit needs the chain-summed fill to *exceed* the live volume).
const GARBAGE_WORDS: usize = 257;

/// The mature capacity the dead-fill-free probes construct their boundary collectors with is
/// derived from the measured copy volume, never from this policy.
fn policy(mature_words: usize) -> HeapPolicy {
    HeapPolicy {
        initial_young_words: YOUNG_WORDS,
        initial_mature_words: mature_words,
        // The fullsweep cadence must never fire during a probe: the probes are about the **minor**
        // room rule, and a cadence-driven major would hide it.
        fullsweep_after: 65_535,
        ..HeapPolicy::default()
    }
}

/// A collector whose grown generation starts as exactly `mature_words` words of capacity.
fn collector(mature_words: usize) -> Collector {
    Collector::new(policy(mature_words)).expect("collector")
}

/// The `(used, capacity)` word pair of an owned [`beam_gc::HeapSpace`].
fn space_words(space: &beam_gc::HeapSpace) -> (usize, usize) {
    (space.used_words(), space.capacity_words())
}

/// Writes a `words`-word tuple of distinct small integers into the nursery and returns nothing:
/// it is **never rooted**, so it is dead the moment it is written.
fn scatter_garbage(c: &mut Collector, words: usize) {
    let arity = words.saturating_sub(1).max(1);
    let elems: Vec<Eterm> = (0..arity as i64).map(make_small).collect();
    let _dead = c
        .young_mut()
        .heap_mut()
        .tuple(&elems)
        .expect("the nursery has room for the garbage tuple");
}

// ---------------------------------------------------------------------------
// Shape 1: a flat map plus a list of tuples
// ---------------------------------------------------------------------------

/// Keys of the flat map (`<= MAP_SMALL_MAP_LIMIT`, so the block a writer reserves is exactly the
/// block it writes — a flat map's `4 + 2n` words, `beam_term`'s `flatmap_size_words`).
const FLAT_KEYS: usize = 12;

/// Elements of the cons list; each element is a 3-word tuple.
const LIST_LEN: i64 = 16;

/// Builds the shell into `c`'s nursery: a flat map and a list of 3-word tuples, plus a tuple
/// holding the map (so the map is reached through a boxed object as well as through the root
/// vector). No off-heap payload is named.
fn map_and_list(c: &mut Collector) -> (Vec<Eterm>, Option<RefcBinary>) {
    let atoms = AtomTable::new();
    let pairs: Vec<(Eterm, Eterm)> = (0..FLAT_KEYS as i64)
        .map(|i| (make_small(i), make_small(i * 7)))
        .collect();
    // SAFETY: `map_words_capacity(FLAT_KEYS)` exclusively-owned, writable words are reserved at
    // the heap top and a flat map writes a contiguous prefix of them (here the whole block: the
    // capacity formula for `n <= MAP_SMALL_MAP_LIMIT` is the size the writer lays down), so the
    // returned root names initialised words of this heap.
    let map = unsafe {
        let hp = c
            .young_mut()
            .heap_mut()
            .alloc_words(map_words_capacity(FLAT_KEYS))
            .expect("the map's block");
        write_map(hp, &pairs, &atoms).expect("build the flat map")
    };

    let mut list = NIL;
    for i in (0..LIST_LEN).rev() {
        let tuple = c
            .young_mut()
            .heap_mut()
            .tuple(&[make_small(i), make_small(-i), make_small(i * i)])
            .expect("a 3-word tuple");
        list = c
            .young_mut()
            .heap_mut()
            .cons(tuple, list)
            .expect("a cons cell");
    }

    // A 2-word tuple reaching the map through a boxed element, and a nested pair reaching the
    // list the same way: the shell is not a flat root vector of independent terms.
    let holder = c
        .young_mut()
        .heap_mut()
        .tuple(&[map, list])
        .expect("the holder tuple");

    (vec![map, list, holder], None)
}

// ---------------------------------------------------------------------------
// Shape 2: a local fun, a match-context sub-binary and a refc-binary pair
// ---------------------------------------------------------------------------

/// The 16-byte payload of the refc binary (its bytes are never read: only the on-heap pair is).
const REFC_BYTES: &[u8] = b"minor room upper bound!";

/// The on-heap bitstring the match context windows into: 128 bits = 4 words
/// (`heap_bits_size_words(128) == 2 + 2`).
const BITS_BITS: usize = 128;

/// The bit window the match context selects: bits 16..80.
const SUB_START: usize = 16;
/// See [`SUB_START`].
const SUB_END: usize = 80;

/// Builds the shell into `c`'s nursery: a `Local` fun with two boxed free variables (a tuple and
/// a flat map), an `ErlSubBits` match context over an on-heap bitstring, and the 8-word on-heap
/// pair of an off-heap refc binary. The payload handle is returned so the caller keeps the block
/// alive for as long as the pair names it.
fn fun_sub_binary_and_refc(c: &mut Collector) -> (Vec<Eterm>, Option<RefcBinary>) {
    const UNIQ: u64 = 0x5EED_0000_0000_0001;
    const INDEX: u32 = 7;

    let atoms = AtomTable::new();
    let pairs: Vec<(Eterm, Eterm)> = (0..4i64).map(|i| (make_small(i), make_small(i + 100))).collect();
    // SAFETY: as in `map_and_list` — a 4-key flat map writes exactly the `4 + 2 * 4` words the
    // capacity formula reserves.
    let map = unsafe {
        let hp = c
            .young_mut()
            .heap_mut()
            .alloc_words(map_words_capacity(4))
            .expect("the fun's map free variable");
        write_map(hp, &pairs, &atoms).expect("build the flat map")
    };
    let tuple = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(42), make_small(-42)])
        .expect("the fun's tuple free variable");

    // The fun's block is `1 + desc_words(Local) + num_free` words.
    let env = [tuple, map];
    let fun = {
        let words = fun_words(FunKind::Local, env.len());
        // SAFETY: `words` exclusively-owned, writable words are reserved at the heap top;
        // `write_fun_local` writes every one of them (header, the 2-word `Local` descriptor and
        // the two free variables) and returns a root into that block.
        unsafe {
            let hp = c
                .young_mut()
                .heap_mut()
                .alloc_words(words)
                .expect("the fun block");
            write_fun_local(hp, 2, UNIQ, INDEX, &env).expect("write a local fun")
        }
    };

    // An on-heap bitstring and a match context windowing into it (`orig` names the bitstring, so
    // the copier reaches the bits through the context's own reference word).
    let bytes: [u8; BITS_BITS / 8] = core::array::from_fn(|i| 0xA5u8.wrapping_mul(i as u8 + 1));
    let bits = {
        let words = heap_bits_size_words(BITS_BITS);
        // SAFETY: `words` exclusively-owned, writable words are reserved; `write_heap_bits`
        // writes all of them (header, bit size, zeroed data words plus the payload).
        unsafe {
            let hp = c
                .young_mut()
                .heap_mut()
                .alloc_words(words)
                .expect("the heap-bits block");
            write_heap_bits(hp, &bytes, BITS_BITS).expect("write an on-heap bitstring")
        }
    };
    // SAFETY: `bits` is the live block just written, so its data words start at word 2 and cover
    // the `BITS_BITS` bits the context selects.
    let data_base = unsafe { boxed_val(bits).add(ERL_HEAP_BITS_SIZE) };
    // `ERL_SUB_BITS_FLAG_MUTABLE` is exactly the state in which the C allows `orig` to name an
    // on-heap bitstring instead of a `BinRef` (`erl_bits.h`).
    let base_flags = Eterm::from_raw(data_base as usize | ERL_SUB_BITS_FLAG_MUTABLE);
    let sub_bits = {
        // SAFETY: `ERL_SUB_BITS_SIZE` (5) exclusively-owned words are reserved; `write_sub_bits`
        // writes all of them, `base_flags` names storage covering bits `SUB_START..SUB_END`, and
        // `orig` is the live boxed bitstring above.
        unsafe {
            let hp = c
                .young_mut()
                .heap_mut()
                .alloc_words(ERL_SUB_BITS_SIZE)
                .expect("the ErlSubBits block");
            write_sub_bits(
                hp,
                base_flags,
                Eterm::from_raw(SUB_START),
                Eterm::from_raw(SUB_END),
                bits,
            )
        }
    };

    // The off-heap refc binary and its on-heap pair.
    let payload = RefcBinary::with_allocator(default_allocator().clone(), REFC_BYTES)
        .expect("the off-heap payload");
    let refc = {
        let size_bits = REFC_BYTES.len() * 8;
        // SAFETY: `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved: the 3-word
        // `BinRef` followed by the 5-word `ErlSubBits`. `payload.data_ptr()` is a live,
        // word-aligned pointer to `size_bits` bits of off-heap storage kept alive by the handle
        // this function returns, and `null` is the (empty) off-heap chain link.
        unsafe {
            let hp = c
                .young_mut()
                .heap_mut()
                .alloc_words(ERL_REFC_BITS_SIZE)
                .expect("the BinRef/ErlSubBits pair");
            write_refc_binary(
                hp,
                payload.data_ptr() as *mut Eterm,
                core::ptr::null_mut(),
                payload.data_ptr(),
                0,
                size_bits,
            )
        }
    };

    (vec![fun, sub_bits, refc, tuple, map], Some(payload))
}

// ---------------------------------------------------------------------------
// The probe
// ---------------------------------------------------------------------------

/// A shape: builds its live shell into a collector's nursery and hands back the roots (plus the
/// off-heap payload the shell names, kept alive by the caller).
type Shape = fn(&mut Collector) -> (Vec<Eterm>, Option<RefcBinary>);

/// Runs the three-collector probe for one `shape` and one garbage configuration.
///
/// `garbage_words == 0` exercises the requirement's **fill short-circuit** (the chain-summed fill
/// equals the live volume); any positive figure forces the **live walk** instead. Both paths must
/// answer the same number — and that number must be exactly the copy volume.
fn probe(name: &str, shape: Shape, garbage_words: usize) {
    let tag = if garbage_words == 0 {
        "clean nursery".to_string()
    } else {
        format!("{garbage_words} dead words")
    };

    // (1) The measuring collector: room to spare, so the minor runs and its copy volume is read
    // off the collector itself.
    let mut generous = collector(GENEROUS_MATURE_WORDS);
    if garbage_words > 0 {
        scatter_garbage(&mut generous, garbage_words);
    }
    let (mut roots, _payload) = shape(&mut generous);
    let measured = generous
        .collect_kind(CollectionKind::Minor, &mut roots)
        .expect("the measuring minor has room to spare");
    assert_eq!(
        measured.kind,
        CollectionKind::Minor,
        "{name} ({tag}): the measuring collection must be the pinned minor"
    );
    let volume = measured.stats.words_copied;
    assert!(
        volume > 0 && volume < GENEROUS_MATURE_WORDS,
        "{name} ({tag}): the shell's copy volume ({volume} words) must be a non-trivial fraction \
         of the measuring mature space ({GENEROUS_MATURE_WORDS} words)"
    );

    // (2) `collect` — the door that *chooses* — must pick a minor for the same shape at exactly
    // this room, i.e. the choice and the minor's own refusal read the same requirement.
    let mut choosing = collector(volume);
    if garbage_words > 0 {
        scatter_garbage(&mut choosing, garbage_words);
    }
    let (mut roots, _payload) = shape(&mut choosing);
    let chosen = choosing
        .collect(&mut roots)
        .expect("a minor at exactly the copy volume's room runs");
    assert_eq!(
        chosen.kind,
        CollectionKind::Minor,
        "{name} ({tag}): {volume} words of room is enough, so `collect` must not escalate"
    );
    assert_eq!(
        chosen.stats.words_copied, volume,
        "{name} ({tag}): the same shape copies the same words in a second collector"
    );

    // (a) Exactly the copy volume of mature room admits the minor, with no slack: one word less
    // (case (b)) refuses. `words_copied == volume` is the equality the doc claims.
    let mut exact = collector(volume);
    if garbage_words > 0 {
        scatter_garbage(&mut exact, garbage_words);
    }
    let (mut roots, _payload) = shape(&mut exact);
    let admitted = exact
        .collect_kind(CollectionKind::Minor, &mut roots)
        .expect("exactly the copy volume's room admits the minor");
    assert_eq!(admitted.kind, CollectionKind::Minor);
    assert_eq!(
        admitted.stats.words_copied, volume,
        "{name} ({tag}): the requirement is tight — a mature space of exactly the measured copy \
         volume shares it word for word"
    );
    assert_eq!(
        exact.mature().used_words(),
        volume,
        "{name} ({tag}): the minor wrote exactly the requirement's words into the mature space"
    );

    // (b) One word less refuses — before copying anything.
    let mut short = collector(volume - 1);
    if garbage_words > 0 {
        scatter_garbage(&mut short, garbage_words);
    }
    let (mut roots, _payload) = shape(&mut short);
    let roots_before = roots.clone();
    let young_before = space_words(short.young());
    let mature_before = space_words(short.mature());
    let stats_before = short.stats();
    let err = short
        .collect_kind(CollectionKind::Minor, &mut roots)
        .expect_err("one word short of the copy volume must refuse the minor");
    match err {
        BeamError::Alloc(AllocFailure {
            requested,
            kind: AllocKind::Heap,
            ..
        }) => assert_eq!(
            requested,
            volume * core::mem::size_of::<Eterm>(),
            "{name} ({tag}): the refusal reports the words it wanted"
        ),
        other => panic!("{name} ({tag}): the refusal must be the typed heap exhaustion, got {other:?}"),
    }
    assert_eq!(
        space_words(short.young()),
        young_before,
        "{name} ({tag}): the nursery is untouched — nothing was evacuated"
    );
    assert_eq!(
        space_words(short.mature()),
        mature_before,
        "{name} ({tag}): the mature space is untouched — nothing was written"
    );
    assert_eq!(
        (short.stats().minor_collections, short.stats().major_collections),
        (stats_before.minor_collections, stats_before.major_collections),
        "{name} ({tag}): no counter advanced — no collection ran"
    );
    assert_eq!(
        (short.stats().words_copied, short.stats().words_reclaimed),
        (stats_before.words_copied, stats_before.words_reclaimed),
        "{name} ({tag}): the copy/reclaim counters stay put"
    );
    assert_eq!(
        short.minor_gcs(),
        0,
        "{name} ({tag}): a refused minor is not a minor — `minor_gcs` stays put"
    );
    assert_eq!(
        roots, roots_before,
        "{name} ({tag}): the refusal relocated nothing — the roots still name the nursery's terms"
    );
}

/// **The requirement equals the copy volume on a map-and-list shell.**
///
/// The flat map contributes its `[header][occupancy][size][key, value]…` block; the list its cons
/// cells and 3-word tuples. Probed with a clean nursery (the fill short-circuit answers) and with
/// dead words beside the shell (the live walk answers).
#[test]
fn a_map_and_list_shell_needs_exactly_its_copy_volume() {
    probe("map_and_list", map_and_list, 0);
    probe("map_and_list", map_and_list, GARBAGE_WORDS);
}

/// **The requirement equals the copy volume on a fun / sub-binary / refc-binary shell.**
///
/// The three interiors the copier treats specially: the `Local` fun's inlined descriptor (skipped
/// opaquely, its two free variables forwarded), the 5-word `ErlSubBits` match context (whose
/// `orig` copies the on-heap bitstring as a separate object) and the 8-word refc pair (a `BinRef`
/// copied as its own 3-word object plus the 5-word context).
#[test]
fn a_fun_sub_binary_and_refc_shell_needs_exactly_its_copy_volume() {
    probe("fun_sub_binary_and_refc", fun_sub_binary_and_refc, 0);
    probe(
        "fun_sub_binary_and_refc",
        fun_sub_binary_and_refc,
        GARBAGE_WORDS,
    );
}
