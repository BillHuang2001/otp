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

//! `SizingPolicy` — the caller-supplied half of `size_object_x(t, litopt)`.
//!
//! `size_object`/`size_object_bounded` publish the C's **source-occupancy** pricing
//! (`size_object_x(t, NULL)`); `size_object_with`/`size_object_bounded_with` take a
//! policy that re-prices the same walk for a consumer that copies the graph somewhere
//! (`size_object_litopt`, `erl_message.c:771,839`). This file pins:
//!
//! * **no behaviour change** — [`DefaultSizingPolicy`] reproduces both existing entry
//!   points byte-for-byte, for every term shape the crate can build;
//! * the **sharing** axis (`is_shared`: a literal-area subterm costs 0 and is not
//!   descended into — `copy.c:99`/`:119`);
//! * the **pricing** axis (`object_words`: a refc binary with no source off-heap handle
//!   prices as its flattened window — `copy.c:232-239` — not the 8-word on-heap pair);
//! * that the two axes are **independent**, that `object_words == None` prices nothing
//!   and descends nowhere, and that the error/cycle policy is untouched.

use beam_term::prelude::*;

// ---------------------------------------------------------------------------
// Policies under test
// ---------------------------------------------------------------------------

/// The canonical [`object_words`] answer — the pricing half every policy that only
/// cares about sharing inherits (the trait default, spelled out so the override in a
/// sibling method stays readable).
fn layout_words(header: Eterm, payload: *const Eterm) -> Option<usize> {
    // SAFETY: the walk only calls a policy with a live, initialized object of
    // `header`'s shape — `object_words`'s documented caller contract.
    Some(unsafe { object_words(header, payload) })
}

/// The sharing axis: a literal-area pointer is reused by identity, so it costs the
/// destination nothing (`litopt` in `copy.c:99`/`:119`).
fn shared_literal(t: Eterm) -> bool {
    // `is_literal` is a bare bit test on `TAG_LITERAL_PTR`: meaningful only for a
    // BOXED/LIST word, so the compound-word test comes first.
    (is_boxed(t) || is_list(t)) && is_literal(t)
}

/// The pricing axis: a refc binary whose payload **no source off-heap handle owns** is
/// materialised into a flattened window of `heap_bits_size_words(end - start)` words
/// (`copy.c:232-239`) instead of the 8-word on-heap `[BinRef][ErlSubBits]` pair.
///
/// Returns `None` for every shape it does not re-price.
fn flatten_refc_window(header: Eterm, payload: *const Eterm) -> Option<usize> {
    if header.raw() & TAG_HEADER_MASK != SUB_BITS_SUBTAG {
        return None;
    }
    // SAFETY: `header` heads a live `ErlSubBits`, so words 2..4 (`start`, `end`,
    // `orig`) are initialized.
    let (start, end, orig) = unsafe {
        (
            *payload.add(SUB_BITS_START_WORD),
            *payload.add(SUB_BITS_END_WORD),
            *payload.add(SUB_BITS_ORIG_WORD),
        )
    };
    if !is_boxed_term(orig) {
        return None;
    }
    // SAFETY: `orig` is a boxed term, so its header word is a live term word.
    if unsafe { *boxed_val(orig) } != HEADER_BIN_REF {
        return None;
    }
    Some(heap_bits_size_words(end.raw().saturating_sub(start.raw())))
}

/// Shares literal-area subterms only.
struct LiteralSharing;

impl SizingPolicy for LiteralSharing {
    fn is_shared(&self, t: Eterm) -> bool {
        shared_literal(t)
    }
}

/// Re-prices a no-handle refc binary as its flattened window only.
struct FlattenRefc;

impl SizingPolicy for FlattenRefc {
    fn object_words(&self, header: Eterm, payload: *const Eterm) -> Option<usize> {
        flatten_refc_window(header, payload).or_else(|| layout_words(header, payload))
    }
}

/// Both axes at once.
struct LiteralAndFlatten;

impl SizingPolicy for LiteralAndFlatten {
    fn is_shared(&self, t: Eterm) -> bool {
        shared_literal(t)
    }

    fn object_words(&self, header: Eterm, payload: *const Eterm) -> Option<usize> {
        flatten_refc_window(header, payload).or_else(|| layout_words(header, payload))
    }
}

/// Prices no boxed object at all (`object_words` is `None` for every shape).
struct Priceless;

impl SizingPolicy for Priceless {
    fn object_words(&self, header: Eterm, payload: *const Eterm) -> Option<usize> {
        let _ = (header, payload);
        None
    }
}

/// [`Priceless`] plus "lists are shared": a cons cell is header-less, so a policy can
/// only price it at 0 through the sharing axis (`Frame::Cons` is always 2 words).
struct PricelessSharedLists;

impl SizingPolicy for PricelessSharedLists {
    fn is_shared(&self, t: Eterm) -> bool {
        is_list(t)
    }

    fn object_words(&self, header: Eterm, payload: *const Eterm) -> Option<usize> {
        let _ = (header, payload);
        None
    }
}

/// Prices no tuple / native record, by structure.
struct NoTuples;

impl SizingPolicy for NoTuples {
    fn object_words(&self, header: Eterm, payload: *const Eterm) -> Option<usize> {
        if matches!(
            header.raw() & TAG_HEADER_MASK,
            ARITYVAL_SUBTAG | RECORD_SUBTAG
        ) {
            return None;
        }
        layout_words(header, payload)
    }
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// Every corpus term, plus the storage that has to outlive it.
///
/// `Heap`/`Vec` are moved but their *buffers* are not, so the interior pointers the
/// terms carry stay valid for the whole test.
struct Corpus {
    _heap: Heap,
    /// `write_map` layouts and the literal arena.
    _buffers: Vec<Vec<Eterm>>,
    /// The off-heap payload the refc binary points at (`Box`ed so its address is fixed
    /// before the term is built).
    _payload: Box<[u64; 1]>,
    terms: Vec<(&'static str, Eterm)>,
}

impl Corpus {
    fn terms(&self) -> &[(&'static str, Eterm)] {
        &self.terms
    }

    /// The refc binary (an 8-word on-heap pair over a 64-bit payload).
    fn refc_binary(&self) -> Eterm {
        self.find("refc binary")
    }

    /// The match-context sub-binary over an on-heap bitstring.
    fn match_context(&self) -> Eterm {
        self.find("match context (520-bit window)")
    }

    /// The literal-tagged tuple containing a nested tuple and a cons cell.
    fn literal(&self) -> Eterm {
        self.find("literal-tagged tuple")
    }

    fn find(&self, name: &str) -> Eterm {
        self.terms
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
            .unwrap_or_else(|| panic!("the corpus must contain {name}"))
    }
}

/// Builds one generous arena per test and every shape the sizing walk knows.
fn corpus() -> Corpus {
    let mut heap = Heap::new(4096);
    let mut buffers: Vec<Vec<Eterm>> = Vec::new();
    let mut terms: Vec<(&'static str, Eterm)> = Vec::new();

    let mut table = AtomTable::new();
    let atom = make_atom(table.intern("an_atom"));

    // --- immediates ---------------------------------------------------------
    terms.push(("small int", make_small(42)));
    terms.push(("nil", NIL));
    terms.push(("atom", atom));
    terms.push(("internal pid", make_internal_pid(make_pid_data(3, 5))));
    terms.push(("internal port", make_internal_port(make_port_data(5))));
    terms.push(("tuple0", tuple0()));

    // --- lists --------------------------------------------------------------
    let one = heap.cons(make_small(1), NIL).unwrap();
    terms.push(("one-cell list", one));
    let cell3 = heap.cons(make_small(3), NIL).unwrap();
    let cell23 = heap.cons(make_small(2), cell3).unwrap();
    let three = heap.cons(make_small(1), cell23).unwrap();
    terms.push(("three-cell list", three));
    let inner_cell = heap.cons(make_small(7), NIL).unwrap();
    let nested_tail = heap.cons(make_small(8), NIL).unwrap();
    let nested_list = heap.cons(inner_cell, nested_tail).unwrap();
    terms.push(("nested list", nested_list));

    // --- tuples -------------------------------------------------------------
    let inner_tuple = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
    let nested_tuple = heap.tuple(&[inner_tuple, make_small(3)]).unwrap();
    terms.push(("nested tuple", nested_tuple));

    // --- boxed leaves -------------------------------------------------------
    // SAFETY: `FLOAT_SIZE_WORDS` (2) words are reserved; the heap owns them.
    let float = unsafe { write_float(heap.alloc_words(FLOAT_SIZE_WORDS).unwrap(), 3.25) };
    terms.push(("float", float));
    // SAFETY: `big_size_words(2)` (3) words are reserved; the heap owns them.
    let big = unsafe {
        write_big(
            heap.alloc_words(big_size_words(2)).unwrap(),
            false,
            &[0xDEAD_BEEF_1234_5678, 1],
        )
    };
    terms.push(("2-digit bignum", big));

    // --- maps: flat at the boundary, HAMT past it ---------------------------
    for n in [0i64, 1, 3, 32, 33, 100] {
        let name: &'static str = match n {
            0 => "flat map (0 keys)",
            1 => "flat map (1 key)",
            3 => "flat map (3 keys)",
            32 => "flat map (32 keys)",
            33 => "HAMT map (33 keys)",
            _ => "HAMT map (100 keys)",
        };
        let pairs: Vec<(Eterm, Eterm)> = (1..=n)
            .map(|k| (make_small(k), make_small(k * 7)))
            .collect();
        let atoms = AtomTable::new();
        let mut buf = vec![Eterm::from_raw(0xDEAD_BEEF); map_words_capacity(pairs.len())];
        // SAFETY: `buf` owns exactly the `map_words_capacity` words `write_map` needs
        // and outlives the term (it is kept in `buffers`).
        let map = unsafe { write_map(buf.as_mut_ptr(), &pairs, &atoms) }.unwrap();
        terms.push((name, map));
        buffers.push(buf);
    }

    // --- funs ---------------------------------------------------------------
    let free = [make_small(1), make_small(2)];
    // SAFETY: `fun_words` sizes the block exactly; the heap owns it.
    let local_fun = unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::Local, free.len()))
            .unwrap();
        write_fun_local(hp, 2, 0x1234, 5, &free).unwrap()
    };
    terms.push(("local fun (2 free vars)", local_fun));
    // SAFETY: `fun_words(External, 0)` (4) words are reserved; the heap owns them.
    let external_fun = unsafe {
        let hp = heap.alloc_words(fun_words(FunKind::External, 0)).unwrap();
        write_fun_external(hp, 2, Atom::from_raw(1), Atom::from_raw(2)).unwrap()
    };
    terms.push(("external fun", external_fun));
    let env = [make_small(9)];
    // SAFETY: `fun_words(Imported, 1)` sizes the block exactly; the heap owns it.
    let imported_fun = unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::Imported, env.len()))
            .unwrap();
        write_fun_imported(
            hp,
            0,
            Atom::from_raw(1),
            &[0u8; 16],
            1,
            2,
            3,
            FunPid {
                node: Atom::from_raw(2),
                number: 4,
                serial: 5,
                creation: 6,
            },
            &env,
        )
        .unwrap()
    };
    terms.push(("imported fun (1 free var)", imported_fun));

    // --- binaries -----------------------------------------------------------
    let payload: Box<[u64; 1]> = Box::new([0x0102_0304_0506_0708]);
    // SAFETY: 8 words are reserved for the `[BinRef][ErlSubBits]` pair; the payload is
    // word-aligned and outlives the term (the box is kept in the returned `Corpus`).
    let refc = unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
        write_refc_binary(
            hp,
            payload.as_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.as_ptr() as *const u8,
            0,
            64,
        )
    };
    terms.push(("refc binary", refc));

    // SAFETY: `heap_bits_size_words(520)` (11) words are reserved; the heap owns them.
    let heap_bits = unsafe {
        let hp = heap.alloc_words(heap_bits_size_words(520)).unwrap();
        write_heap_bits(hp, &[0xAA; 65], 520).unwrap()
    };
    // SAFETY: 5 words for the `ErlSubBits`; `heap_bits` is a live heap bitstring.
    let match_ctx = unsafe {
        let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).unwrap();
        write_sub_bits(
            hp,
            Eterm::from_raw(0),
            Eterm::from_raw(0),
            Eterm::from_raw(520),
            heap_bits,
        )
    };
    terms.push(("match context (520-bit window)", match_ctx));

    // --- external identifiers ----------------------------------------------
    // SAFETY: `external_pid_size_words()` (4) words are reserved.
    let ext_pid = unsafe {
        let hp = heap.alloc_words(external_pid_size_words()).unwrap();
        write_external_pid(hp, make_atom(Atom::from_raw(1)), 7, 8, 9)
    };
    terms.push(("external pid", ext_pid));
    // SAFETY: `external_port_size_words()` (4) words are reserved.
    let ext_port = unsafe {
        let hp = heap.alloc_words(external_port_size_words()).unwrap();
        write_external_port(hp, make_atom(Atom::from_raw(1)), 0x1122_3344, 5)
    };
    terms.push(("external port", ext_port));
    let ids = [1u32, 2, 3];
    // SAFETY: `external_ref_size_words(ids.len())` words are reserved.
    let ext_ref = unsafe {
        let hp = heap
            .alloc_words(external_ref_size_words(ids.len()))
            .unwrap();
        write_external_ref(hp, make_atom(Atom::from_raw(1)), &ids, 5)
    };
    terms.push(("external ref", ext_ref));

    // --- a native record ----------------------------------------------------
    let fields = [
        (make_atom(table.intern("a")), make_small(1)),
        (make_atom(table.intern("b")), make_small(2)),
    ];
    // SAFETY: `record_block_words(2)` sizes the whole construction block.
    let record = unsafe {
        let hp = heap.alloc_words(record_block_words(fields.len())).unwrap();
        write_record(
            hp,
            table.intern("m"),
            table.intern("r"),
            false,
            &fields,
            make_small(0),
        )
        .unwrap()
    };
    terms.push(("native record (2 fields)", record));

    // --- a literal area -----------------------------------------------------
    // Arena: `{ {7}, [9] }` laid out header-first, exactly as the C literal-area walk
    // expects (`tag_words` steps over a header's covered words).
    let mut words = vec![NIL; 7];
    words[0] = make_arityval(2); // the outer tuple's header
    words[3] = make_arityval(1); // the nested tuple's header
    words[4] = make_small(7); // ... its single element
    words[5] = make_small(9); // the cons cell's CAR
    words[6] = NIL; // ... and its CDR
    let base = words.as_mut_ptr();
    // SAFETY: `words` owns 7 words and is never resized afterwards, so `base` stays
    // valid; the two pointer slots are written before the arena is tagged.
    unsafe {
        words[1] = make_boxed(base.add(3));
        words[2] = make_list(base.add(5));
    }
    let mut literal = make_tuple(base);
    tag_literal_area(&mut words, core::slice::from_mut(&mut literal));
    assert!(is_literal(literal), "the root must be literal-tagged");
    terms.push(("literal-tagged tuple", literal));
    buffers.push(words);

    // --- a composite of several shapes --------------------------------------
    let flat3 = terms
        .iter()
        .find(|(n, _)| *n == "flat map (3 keys)")
        .map(|(_, t)| *t)
        .unwrap();
    let composite = heap
        .tuple(&[float, refc, local_fun, one, flat3, big])
        .unwrap();
    terms.push(("composite tuple", composite));

    Corpus {
        _heap: heap,
        _buffers: buffers,
        _payload: payload,
        terms,
    }
}

/// The non-term words `tests/non_term.rs` pins (bare `HEADER`-tagged words).
fn non_terms() -> [Eterm; 5] {
    [
        THE_NON_VALUE,
        make_header(0, FLOAT_SUBTAG),
        HEADER_FLONUM,
        Eterm::from_raw(0x3C),
        Eterm::from_raw(0x100),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
#[allow(
    clippy::default_constructed_unit_structs,
    reason = "the point is that `Default` is the canonical construction the walk relies \
              on, so spelling `default()` (rather than the unit literal) is deliberate"
)]
fn default_policy_reproduces_both_existing_entry_points() {
    let c = corpus();
    // Object safety: the walk takes `&dyn SizingPolicy`.
    let p: &dyn SizingPolicy = &DefaultSizingPolicy::default();
    assert_eq!(DefaultSizingPolicy::default(), DefaultSizingPolicy);

    for (name, t) in c.terms() {
        let n = size_object(*t);
        assert_eq!(
            size_object_with(*t, &DefaultSizingPolicy),
            n,
            "{name}: size_object_with under the default policy"
        );
        assert_eq!(
            size_object_with(*t, p),
            n,
            "{name}: the same through a `&dyn SizingPolicy`"
        );
        for limit in [0usize, 1, n.saturating_sub(1), n, n + 1, usize::MAX] {
            assert_eq!(
                size_object_bounded_with(*t, limit, &DefaultSizingPolicy),
                size_object_bounded(*t, limit),
                "{name}: bounded at limit {limit} (n = {n})"
            );
        }
    }
    // No corpus term is degenerate: the corpus really does exercise non-zero pricing.
    assert!(size_object(c.refc_binary()) > 0);
    assert_eq!(size_object(make_small(1)), 0);
}

#[test]
fn the_default_policy_shares_nothing() {
    let c = corpus();
    let policy = DefaultSizingPolicy;
    for (name, t) in c.terms() {
        assert!(!policy.is_shared(*t), "{name} must not be shared");
    }
    // ... including the literal-tagged root, which a literal-aware policy *does* share.
    assert!(shared_literal(c.literal()));

    // `object_words` is the canonical layout answer.
    let mut heap = Heap::new(16);
    let tuple = heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .unwrap();
    // SAFETY: `tuple` is a live tuple of this heap.
    let (header, payload) = unsafe { (*boxed_val(tuple), boxed_val(tuple)) };
    assert_eq!(policy.object_words(header, payload), Some(4));
    // SAFETY: as above — a live tuple header at `payload`.
    assert_eq!(
        policy.object_words(header, payload),
        Some(unsafe { object_words(header, payload) })
    );
}

#[test]
fn a_literal_aware_policy_prices_a_shared_literal_at_zero() {
    let mut heap = Heap::new(64);
    // `{ {7}, [9] }`, tagged as a literal area: 3 (outer) + 2 (nested) + 2 (cons).
    let mut words = vec![NIL; 7];
    words[0] = make_arityval(2);
    words[3] = make_arityval(1);
    words[4] = make_small(7);
    words[5] = make_small(9);
    words[6] = NIL;
    let base = words.as_mut_ptr();
    // SAFETY: 7 owned words, never resized; both pointer slots written before tagging.
    unsafe {
        words[1] = make_boxed(base.add(3));
        words[2] = make_list(base.add(5));
    }
    let mut literal = make_tuple(base);
    tag_literal_area(&mut words, core::slice::from_mut(&mut literal));

    const INTERIOR: usize = 3 + 2 + 2;
    assert_eq!(
        size_object(literal),
        INTERIOR,
        "the default prices the interior"
    );
    assert_eq!(
        size_object_with(literal, &LiteralSharing),
        0,
        "a shared literal costs the destination nothing"
    );

    // Nested in a heap tuple the literal contributes 0, its interior otherwise.
    let holder = heap.tuple(&[literal, make_small(1)]).unwrap();
    assert_eq!(size_object(holder), 3 + INTERIOR);
    assert_eq!(size_object_with(holder, &LiteralSharing), 3);
    assert_eq!(
        size_object_bounded_with(holder, 3, &LiteralSharing),
        Ok(3),
        "the budget is the tuple's own words"
    );
    assert_eq!(
        size_object_bounded_with(holder, 2, &LiteralSharing),
        Err(TermError::TermGraphTooLarge)
    );
    // The same two budgets under the default policy do not fit.
    assert_eq!(
        size_object_bounded(holder, 3),
        Err(TermError::TermGraphTooLarge)
    );
    assert_eq!(size_object_bounded(holder, 3 + INTERIOR), Ok(3 + INTERIOR));

    // A shared root alone: 0 words, so a zero budget is exactly enough.
    assert_eq!(size_object_bounded_with(literal, 0, &LiteralSharing), Ok(0));
    assert_eq!(
        size_object_bounded(literal, 0),
        Err(TermError::TermGraphTooLarge)
    );
}

#[test]
fn a_flattening_policy_prices_a_no_handle_refc_binary_as_its_window() {
    let c = corpus();
    let refc = c.refc_binary();
    let ctx = c.match_context();

    // A refc binary is the whole on-heap `[BinRef(3)][ErlSubBits(5)]` pair by default;
    // with no source off-heap handle to share, its payload is materialised into a
    // flattened 64-bit window instead (`copy.c:232-239`).
    assert_eq!(size_object(refc), ERL_REFC_BITS_SIZE);
    assert_eq!(heap_bits_size_words(64), 3);
    assert_eq!(size_object_with(refc, &FlattenRefc), 3);
    assert_eq!(
        size_object_bounded_with(refc, 3, &FlattenRefc),
        Ok(3),
        "the flattened window fits a 3-word budget"
    );
    assert_eq!(
        size_object_bounded_with(refc, 2, &FlattenRefc),
        Err(TermError::TermGraphTooLarge)
    );

    // A match-context sub-binary already prices as its window under the default
    // (`copy.c:232-239`), so the flattening override does not double-apply.
    assert_eq!(size_object(ctx), heap_bits_size_words(520));
    assert_eq!(size_object_with(ctx, &FlattenRefc), size_object(ctx));
    assert_eq!(
        size_object_bounded_with(ctx, size_object(ctx), &FlattenRefc),
        Ok(size_object(ctx))
    );
}

#[test]
fn object_words_none_prices_nothing_and_descends_nowhere() {
    let c = corpus();

    // A cons cell is header-less, so `object_words` is never consulted for it: even a
    // policy that prices no boxed object leaves its 2 words. That is the documented
    // representation-level floor (`Frame::Cons`).
    let mut heap = Heap::new(128);
    let cell = heap.cons(make_small(1), make_small(2)).unwrap();
    assert_eq!(size_object_with(cell, &Priceless), 2);

    // Every boxed term — maps, tuples, funs, binaries, external ids, records — is 0.
    for (name, t) in c.terms() {
        if !is_list(*t) {
            assert_eq!(
                size_object_with(*t, &Priceless),
                0,
                "{name}: a `None` pricing contributes nothing"
            );
            assert_eq!(
                size_object_bounded_with(*t, 0, &Priceless),
                Ok(0),
                "{name}: and fits a zero budget"
            );
        }
    }

    // With lists shared as well, the *whole* corpus — composites and maps included —
    // prices 0: `None` never descends, so nothing inside a priceless object is reached.
    let list = heap.cons(make_small(1), NIL).unwrap();
    let composite = heap.tuple(&[list, c.refc_binary(), c.literal()]).unwrap();
    assert!(size_object(composite) > 3);
    for (name, t) in c.terms() {
        assert_eq!(
            size_object_with(*t, &PricelessSharedLists),
            0,
            "{name}: nothing is priced and nothing is descended into"
        );
    }
    assert_eq!(size_object_with(composite, &PricelessSharedLists), 0);

    // A tuple-only `None` policy makes `{1, [2,3,4]}` free: the tuple prices nothing and
    // the list inside it is never reached. The default prices 3 + 6.
    let inner = heap.cons(make_small(4), NIL).unwrap();
    let inner2 = heap.cons(make_small(3), inner).unwrap();
    let list3 = heap.cons(make_small(2), inner2).unwrap();
    let holder = heap.tuple(&[make_small(1), list3]).unwrap();
    assert_eq!(size_object(holder), 3 + 6);
    assert_eq!(size_object_with(holder, &NoTuples), 0);
    // The list on its own is still priced (it is not a tuple).
    assert_eq!(size_object_with(list3, &NoTuples), 6);
}

#[test]
fn the_two_axes_are_independent() {
    let c = corpus();
    let mut heap = Heap::new(64);
    let literal = c.literal();
    let refc = c.refc_binary();
    let combo = heap.tuple(&[literal, refc]).unwrap();

    const LITERAL_INTERIOR: usize = 3 + 2 + 2;
    const REFC_PAIR: usize = ERL_REFC_BITS_SIZE;
    const WINDOW: usize = 3; // `heap_bits_size_words(64)`

    assert_eq!(size_object(combo), 3 + LITERAL_INTERIOR + REFC_PAIR);
    assert_eq!(
        size_object_with(combo, &LiteralSharing),
        3 + REFC_PAIR,
        "sharing alone drops only the literal"
    );
    assert_eq!(
        size_object_with(combo, &FlattenRefc),
        3 + LITERAL_INTERIOR + WINDOW,
        "flattening alone drops only the refc pair"
    );
    assert_eq!(
        size_object_with(combo, &LiteralAndFlatten),
        3 + WINDOW,
        "both axes compose: header plus the flattened window"
    );
    assert_eq!(
        size_object_bounded_with(combo, 3 + WINDOW, &LiteralAndFlatten),
        Ok(3 + WINDOW)
    );
    assert_eq!(
        size_object_bounded_with(combo, 2 + WINDOW, &LiteralAndFlatten),
        Err(TermError::TermGraphTooLarge)
    );
}

#[test]
fn a_cyclic_graph_stays_typed_under_a_policy() {
    let mut heap = Heap::new(4);
    // `L = [1 | L]`: a cons cell whose `cdr` is the cell itself.
    // SAFETY: 2 words are reserved for the cons cell and both are written before the
    // term is handed to any accessor.
    let p = unsafe { heap.alloc_words(2).unwrap() };
    let cell = make_list(p);
    // SAFETY: as above — the `cdr` slot is live and this only rewrites it.
    unsafe {
        *p = make_small(1);
        *p.add(1) = cell;
    }

    assert_eq!(
        size_object_bounded_with(cell, usize::MAX, &DefaultSizingPolicy),
        Err(TermError::TermGraphCyclic)
    );
    for limit in [0usize, 1, 1_000] {
        assert_eq!(
            size_object_bounded_with(cell, limit, &DefaultSizingPolicy),
            Err(TermError::TermGraphCyclic),
            "limit {limit}: cyclic outranks too-large"
        );
    }
    // `size_object_with` keeps the documented total partial count (the cell itself).
    assert_eq!(size_object_with(cell, &DefaultSizingPolicy), 2);

    // The sensitivity control: a policy that shares cons cells never descends, so the
    // cycle is never reached and the graph prices `Ok(0)` — the sharing axis really does
    // reach the acyclicity pass.
    assert_eq!(
        size_object_bounded_with(cell, usize::MAX, &PricelessSharedLists),
        Ok(0)
    );
    assert_eq!(
        size_object_bounded_with(cell, 0, &PricelessSharedLists),
        Ok(0)
    );
    assert_eq!(size_object_with(cell, &PricelessSharedLists), 0);
}

#[test]
fn non_terms_price_zero_under_any_policy() {
    for nt in non_terms() {
        assert_eq!(size_object_with(nt, &DefaultSizingPolicy), 0);
        assert_eq!(size_object_with(nt, &Priceless), 0);
        assert_eq!(size_object_with(nt, &PricelessSharedLists), 0);
        assert_eq!(size_object_bounded_with(nt, 0, &DefaultSizingPolicy), Ok(0));
        assert_eq!(size_object_bounded_with(nt, 0, &Priceless), Ok(0));
    }
}
