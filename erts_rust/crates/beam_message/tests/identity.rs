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

//! **Term identity across the copy boundary**: a copied term is *observationally identical* to its
//! source, for every payload class.
//!
//! # Why this file exists
//!
//! `tests/traversals.rs` shows that a `{From, LiteralPayload}` send writes **1 object / 3 words**
//! while the same graph censuses as **28 objects / 73 words**. The obvious objection is "so the
//! copy loses the payload" — which is exactly what real ERTS does *not* do: a literal-area subterm
//! is **shared**, not dropped (`erts/emulator/beam/copy.c:720,744,767`; the send path carries a
//! live literal area so the `litopt != NULL` skip applies, `erl_message.c:722-723,771,797`). This
//! file is the evidence: for every class the copy is `=:=` the source (`beam_term::compare::eq`),
//! **byte-identical** under `term_to_binary` (`beam_codec::encode_term`), survives a copy-of-copy,
//! and `binary_to_term` of the copy's own image decodes back to a term `=:=` the source.
//!
//! So a shared literal is a **residency** decision, not a **term-shape** divergence: the receiver
//! observes the same term either way.
//!
//! # The corpus
//!
//! immediates (a separate test — they cross by identity, touching no destination word) and, in the
//! reported corpus: a cons list, a 1000-cell deep list, a nested tuple, a flat map (8 keys), a
//! HAMT/hash map (64 keys — past `MAP_SMALL_MAP_LIMIT`), a byte-aligned bitstring, a
//! non-byte-aligned bitstring (13 bits), a boxed float, a bignum, an `External` (`EXPORT_EXT`) fun,
//! an `Imported` (`NEW_FUN_EXT`) closure with a free variable, a `Local` (`make_fun3`) closure, a
//! **literal-tagged** payload graph and a tuple with a **literal-tagged child**.
//!
//! The one honest exception is the `Local` fun, whose wire image needs a `FunIdentity` the copier
//! does not carry (`beam_codec::error::unsupported_local_fun`); it is still asserted `=:=` and
//! copy-of-copy-faithful, and its typed refusal is printed rather than worked around. (`beam_rt`
//! installs that identity for a compiled module's closure — see `crates/CONTEXT.md`.)
//!
//! # Repro
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
//!     --test identity -- --nocapture
//! ```

use beam_alloc::Heap;
use beam_codec::{decode_term, encode_term, is_unsupported_local_fun};
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, size_of_copy, CopyContext};
use beam_term::prelude::*;
use beam_term::{compare, size_object};

/// A heap big enough for any single corpus entry's source graph.
fn src_heap() -> Heap {
    Heap::new(1 << 14).expect("source heap")
}

/// Copies `term` into the caller-owned `dst` across an **empty** source off-heap table (the
/// single-pass route) and returns the copied root.
fn copy_into(term: Eterm, dst: &mut Heap) -> Eterm {
    let src_off = OffHeapTable::new();
    let mut dst_off = OffHeapTable::new();
    let mut ctx = CopyContext::new(&src_off, &mut dst_off);
    copy_struct(term, dst, &mut ctx).expect("the copy succeeds")
}

/// The source process for one corpus entry: the heap its graph lives on and its atom table.
struct Src {
    heap: Heap,
    atoms: AtomTable,
}

impl Src {
    fn new() -> Src {
        Src {
            heap: src_heap(),
            atoms: AtomTable::new(),
        }
    }
}

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first.
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the cons fits");
    }
    acc
}

/// Writes a flat map (or HAMT map, past `MAP_SMALL_MAP_LIMIT`) on `heap`.
fn write_map_term(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    // SAFETY: `map_words_capacity` is a sound upper bound on the words `write_map` writes.
    unsafe {
        let hp = heap
            .alloc_words(map_words_capacity(pairs.len()))
            .expect("the map fits");
        write_map(hp, pairs, atoms).expect("the map builds")
    }
}

/// Writes an on-heap bitstring of `n_bits` bits taken from `bytes`.
fn heap_bits(heap: &mut Heap, bytes: &[u8], n_bits: usize) -> Eterm {
    // SAFETY: `heap_bits_size_words(n_bits)` words are reserved and `bytes` covers `n_bits` bits.
    unsafe {
        let hp = heap
            .alloc_words(heap_bits_size_words(n_bits))
            .expect("the bitstring fits");
        write_heap_bits(hp, bytes, n_bits).expect("the payload fits")
    }
}

/// Writes a boxed float (`[HEADER_FLONUM, bits]`, 2 words).
fn boxed_float(heap: &mut Heap, value: f64) -> Eterm {
    // SAFETY: `FLOAT_SIZE_WORDS` (2) words are reserved for the block.
    unsafe {
        let hp = heap.alloc_words(FLOAT_SIZE_WORDS).expect("the float fits");
        write_float(hp, value)
    }
}

/// Writes a bignum of `digits` limbs.
fn big_number(heap: &mut Heap, negative: bool, digits: &[u64]) -> Eterm {
    // SAFETY: `big_size_words(digits.len())` words are reserved and `digits` is non-empty.
    unsafe {
        let hp = heap
            .alloc_words(big_size_words(digits.len()))
            .expect("the bignum fits");
        write_big(hp, negative, digits)
    }
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// Whether the ETF encoder can produce a wire image of the class without a `FunIdentity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    /// `term_to_binary` is byte-comparable and `binary_to_term` round-trips.
    Encodes,
    /// The `Local` (`make_fun3`) closure: the frozen block carries no module/MD5, so the encoder
    /// answers the typed `unsupported_local_fun` unless the caller supplies a `FunIdentity`.
    /// Copy fidelity is still asserted; only the wire half is skipped, loudly.
    LocalFunRefusal,
}

/// How much of a payload shares a literal area with the source — the axis the copy's *written*
/// word count depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sharing {
    /// Nothing is shared: the copy writes the whole graph and is a fresh object.
    None,
    /// A **literal-area subterm**, but not the root: the root is a fresh destination object and
    /// the copy writes strictly fewer words than `size_object` counts.
    LiteralChild,
    /// The **root itself** is a literal-area member: it crosses the boundary as the same word and
    /// writes no destination word at all.
    LiteralRoot,
}

/// One payload class of the corpus.
struct Payload {
    name: &'static str,
    build: fn(&mut Src) -> Eterm,
    /// What the graph shares with the source across the copy boundary.
    sharing: Sharing,
    wire: Wire,
}

/// The corpus. Every entry is a real term of its class, built through the crate's own
/// heap-agnostic `beam_term` writers.
const CORPUS: &[Payload] = &[
    Payload {
        name: "cons list (10)",
        build: |s| build_list(&mut s.heap, &(1..=10).map(make_small).collect::<Vec<_>>()),
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "deep list (1000 cells)",
        build: |s| build_list(&mut s.heap, &(0..1000).map(make_small).collect::<Vec<_>>()),
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "nested tuple",
        build: |s| {
            let inner = s.heap.tuple(&[make_small(1), make_small(2)]).expect("inner fits");
            let atom = make_atom(s.atoms.intern("tagged"));
            s.heap
                .tuple(&[atom, inner, NIL])
                .expect("the outer tuple fits")
        },
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "flat map (8 keys)",
        build: |s| {
            let pairs: Vec<(Eterm, Eterm)> = (0..8)
                .map(|i| (make_small(i), make_small(i * 10 + 3)))
                .collect();
            write_map_term(&mut s.heap, &pairs, &s.atoms)
        },
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "hash map (64 keys)",
        build: |s| {
            let pairs: Vec<(Eterm, Eterm)> = (0..64)
                .map(|i| (make_small(i * 3), make_small(i * 1000 + 7)))
                .collect();
            write_map_term(&mut s.heap, &pairs, &s.atoms)
        },
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "bitstring (12 B)",
        build: |s| heap_bits(&mut s.heap, b"hello world", 11 * 8),
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "bitstring (13 bits)",
        build: |s| heap_bits(&mut s.heap, &[0b1011_0110, 0b0101_0000], 13),
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "boxed float",
        build: |s| boxed_float(&mut s.heap, core::f64::consts::PI),
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "bignum (2 limbs)",
        build: |s| big_number(&mut s.heap, false, &[0x0123_4567_89AB_CDEF, 0x0011_2233_4455_6677]),
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "external fun (EXPORT_EXT)",
        build: |s| {
            let module = s.atoms.intern("lists");
            let function = s.atoms.intern("reverse");
            // SAFETY: `fun_words(External, 0)` words (the 4-word block) are reserved.
            unsafe {
                let hp = s.heap.alloc_words(fun_words(FunKind::External, 0)).expect("fits");
                write_fun_external(hp, 2, module, function).expect("a valid external fun")
            }
        },
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "imported fun (1 free var)",
        build: |s| {
            let module = s.atoms.intern("m");
            let mut uniq = [0u8; 16];
            for (i, byte) in uniq.iter_mut().enumerate() {
                *byte = (i as u8).wrapping_mul(7).wrapping_add(1);
            }
            let creator = FunPid {
                node: s.atoms.intern("n1@host"),
                number: 0xAAAA_BBBB,
                serial: 0xCCCC_DDDD,
                creation: 0x1122_3344,
            };
            let free = s
                .heap
                .tuple(&[make_small(1), make_small(2)])
                .expect("the free var fits");
            // SAFETY: `fun_words(Imported, 1)` words are reserved; `free` is a live term.
            unsafe {
                let hp = s.heap.alloc_words(fun_words(FunKind::Imported, 1)).expect("fits");
                write_fun_imported(hp, 2, module, &uniq, 0x0102_0304, 7, 3, creator, &[free])
                    .expect("a valid imported fun")
            }
        },
        sharing: Sharing::None,
        wire: Wire::Encodes,
    },
    Payload {
        name: "local fun (make_fun3)",
        build: |s| {
            let free = s
                .heap
                .tuple(&[make_small(7), make_small(8)])
                .expect("the free var fits");
            // SAFETY: `fun_words(Local, 1)` words are reserved; `free` is a live term.
            unsafe {
                let hp = s.heap.alloc_words(fun_words(FunKind::Local, 1)).expect("fits");
                write_fun_local(hp, 2, 0x1122_3344_5566_7788, 5, &[free]).expect("a local fun")
            }
        },
        sharing: Sharing::None,
        wire: Wire::LocalFunRefusal,
    },
    Payload {
        name: "literal payload graph",
        build: |s| {
            // The `copy_bound` 5-key payload, tagged as a literal area member. The technique is
            // `tests/destination_pricing.rs:126,181`: build the graph, then OR `TAG_LITERAL_PTR`
            // into the root — the copier's `is_shared` test then shares it by identity and never
            // reaches its interior.
            let payload = copy_bound_payload(s);
            make_literal(payload)
        },
        sharing: Sharing::LiteralRoot,
        wire: Wire::Encodes,
    },
    Payload {
        name: "tuple with literal child",
        build: |s| {
            let inner = s
                .heap
                .tuple(&[make_small(1), make_small(2), make_small(3)])
                .expect("the inner tuple fits");
            let literal = make_literal(inner);
            s.heap
                .tuple(&[literal, make_small(9)])
                .expect("the outer tuple fits")
        },
        sharing: Sharing::LiteralChild,
        wire: Wire::Encodes,
    },
];

/// The `copy_bound` benchmark payload (`tools/bench/src/workloads/copy_bound.rs`): the exact graph
/// `tests/traversals.rs` and `tests/counters.rs` send.
fn copy_bound_payload(s: &mut Src) -> Eterm {
    let ints = build_list(&mut s.heap, &(1..=10).map(make_small).collect::<Vec<_>>());
    let hello = heap_bits(&mut s.heap, b"hello world", 11 * 8);
    let float = boxed_float(&mut s.heap, 3.5);
    let ok = make_atom(s.atoms.intern("ok"));
    let b = s.heap.tuple(&[ok, hello, float]).expect("the tuple fits");
    let c = heap_bits(&mut s.heap, &(1..=12u8).collect::<Vec<u8>>(), 12 * 8);
    let d = build_list(
        &mut s.heap,
        &"a char list"
            .bytes()
            .map(|ch| make_small(ch as i64))
            .collect::<Vec<_>>(),
    );

    let ka = make_atom(s.atoms.intern("a"));
    let kb = make_atom(s.atoms.intern("b"));
    let kc = make_atom(s.atoms.intern("c"));
    let kd = make_atom(s.atoms.intern("d"));
    let ke = make_atom(s.atoms.intern("e"));
    let pairs = [
        (ka, ints),
        (kb, b),
        (kc, c),
        (kd, d),
        (ke, make_small(-42)),
    ];
    write_map_term(&mut s.heap, &pairs, &s.atoms)
}

// ---------------------------------------------------------------------------
// The evidence
// ---------------------------------------------------------------------------

/// One corpus entry's evidence. Every `bool` is asserted where it is produced; the struct exists to
/// be printed.
#[derive(Clone, Copy, Debug)]
struct Evidence {
    /// `size_object(term)` — the source graph's flat word count.
    source_words: usize,
    /// The destination heap's `used()` advance (the words the copy actually wrote).
    written_words: usize,
    /// Bytes of the source's ETF image.
    etf_bytes: usize,
    /// `compare::eq(source, copy)`.
    equal: bool,
    /// `term_to_binary(source) == term_to_binary(copy)`, byte for byte.
    bytes_identical: bool,
    /// `compare::eq(copy, copy_of_copy)` and the copy-of-copy's image equals the source's.
    copy_of_copy: bool,
    /// `compare::eq(source, binary_to_term(term_to_binary(copy)))`.
    round_trip: bool,
    /// The copy is the *same word* as the source — true only for a shared literal area member (or
    /// an immediate), i.e. the sharing is by identity, not by value.
    shared_by_identity: bool,
    /// The wire half was not attempted because the class needs a `FunIdentity`.
    wire_refused: bool,
}

/// The triple evidence for one already-built term, all of it asserted here.
///
/// The destination heaps are owned by this function so the copied terms cannot outlive them.
fn check(name: &str, term: Eterm, src: &Src, sharing: Sharing, wire: Wire) -> Evidence {
    // 1. The copy itself. A destination sized by the source count always suffices (the copier never
    //    writes more than `size_object` — literal sharing only ever writes less).
    let source_words = size_object(term);
    let mut dst = Heap::new(source_words + 8).expect("destination heap");
    let copy = copy_into(term, &mut dst);
    let written_words = dst.used();
    assert!(
        written_words <= source_words,
        "{name}: a copy never writes more than the source occupies ({written_words} vs \
         {source_words})"
    );
    match sharing {
        Sharing::None => {
            assert_eq!(
                written_words, source_words,
                "{name}: with nothing shared, the copy writes the whole graph \
                 (== `size_of_copy` == `size_object`)"
            );
            assert_eq!(
                written_words,
                size_of_copy(term),
                "{name}: `size_of_copy` is the source count forwarder"
            );
        }
        Sharing::LiteralChild | Sharing::LiteralRoot => {
            assert!(
                written_words < source_words,
                "{name}: a literal-area member costs strictly fewer destination words \
                 ({written_words} vs {source_words})"
            );
        }
    }

    // 2. Structural equality: the receiver's term is `=:=` the sender's.
    let equal = compare::eq(term, copy, &src.atoms);
    assert!(equal, "{name}: the copy must be =:= the original");

    // 3. Copy of a copy: the boundary is idempotent (and a second crossing changes nothing).
    let mut dst2 = Heap::new(source_words + 8).expect("second destination heap");
    let copy2 = copy_into(copy, &mut dst2);
    let copy_of_copy = compare::eq(copy, copy2, &src.atoms);
    assert!(copy_of_copy, "{name}: a copy of the copy must be =:= the copy");

    // 4. The shared-identity question, answered without a wire image.
    let shared_by_identity = copy.raw() == term.raw();
    match sharing {
        Sharing::LiteralRoot => {
            assert!(
                shared_by_identity,
                "{name}: a literal root crosses the boundary as the same word"
            );
            assert!(is_literal(copy), "{name}: and it still carries TAG_LITERAL_PTR");
            assert_eq!(
                written_words, 0,
                "{name}: so it costs the destination no word at all"
            );
        }
        Sharing::LiteralChild => {
            assert!(
                !shared_by_identity,
                "{name}: the root is a fresh destination object (only the literal child is shared)"
            );
            assert_ne!(
                object_addr(copy),
                object_addr(term),
                "{name}: the root really is a distinct object"
            );
        }
        Sharing::None => {
            assert!(
                !shared_by_identity,
                "{name}: a non-literal compound is a fresh object in the destination"
            );
            assert_ne!(
                object_addr(copy),
                object_addr(term),
                "{name}: the copy is a distinct object"
            );
        }
    }

    // 5. The wire: `term_to_binary` byte equality and a `binary_to_term` round trip.
    if wire == Wire::LocalFunRefusal {
        let error = encode_term(term, &src.atoms)
            .expect_err("a Local fun has no self-contained wire image");
        assert!(
            is_unsupported_local_fun(&error),
            "{name}: the encoder's refusal is the typed local-fun one, got {error:?}"
        );
        println!(
            "  {name:<28} copy =:= source, copy-of-copy =:= copy, \
             wire: typed `unsupported_local_fun` (needs a `FunIdentity` seam)"
        );
        return Evidence {
            source_words,
            written_words,
            etf_bytes: 0,
            equal,
            bytes_identical: false,
            copy_of_copy,
            round_trip: false,
            shared_by_identity,
            wire_refused: true,
        };
    }

    let source_image = encode_term(term, &src.atoms).unwrap_or_else(|e| {
        panic!("{name}: `term_to_binary` of the source must succeed, got {e:?}")
    });
    let copy_image = encode_term(copy, &src.atoms)
        .unwrap_or_else(|e| panic!("{name}: `term_to_binary` of the copy must succeed, got {e:?}"));
    let bytes_identical = source_image == copy_image;
    assert!(
        bytes_identical,
        "{name}: the copy must encode to the same ETF bytes as its source"
    );
    let copy2_image = encode_term(copy2, &src.atoms)
        .unwrap_or_else(|e| panic!("{name}: `term_to_binary` of the copy-of-copy failed: {e:?}"));
    assert_eq!(
        copy2_image, source_image,
        "{name}: the copy-of-copy must encode to the same bytes too"
    );

    // The decoder interns into its own table, so it starts as a clone of the sender's (identical
    // indices, plus room for names the image adds).
    let mut rt_heap = Heap::new(source_image.len() * 8 + 256).expect("round-trip heap");
    let mut rt_atoms = src.atoms.clone();
    let rt = decode_term(&source_image, &mut rt_heap, &mut rt_atoms)
        .unwrap_or_else(|e| panic!("{name}: `binary_to_term` of the image must succeed: {e:?}"));
    let round_trip = compare::eq(term, rt, &rt_atoms);
    assert!(
        round_trip,
        "{name}: `binary_to_term(term_to_binary(copy))` must be =:= the source"
    );

    Evidence {
        source_words,
        written_words,
        etf_bytes: source_image.len(),
        equal,
        bytes_identical,
        copy_of_copy,
        round_trip,
        shared_by_identity,
        wire_refused: false,
    }
}

/// The address of the object a non-immediate compound term points at.
fn object_addr(term: Eterm) -> usize {
    if is_list(term) {
        list_val(term) as usize
    } else {
        boxed_val(term) as usize
    }
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// Immediates are shared by identity and touch no destination word — the degenerate end of the
/// sharing spectrum (the C's `is_immediate` case of `copy_struct`).
#[test]
fn immediates_cross_the_boundary_by_identity() {
    let mut atoms = AtomTable::new();
    let atom = make_atom(atoms.intern("an_atom"));
    for term in [make_small(0), make_small(-7), atom, NIL] {
        let mut dst = Heap::new(8).expect("destination heap");
        let copy = copy_into(term, &mut dst);
        assert_eq!(copy.raw(), term.raw(), "an immediate is returned by identity");
        assert_eq!(dst.used(), 0, "and touches no destination word");
        assert_eq!(size_object(term), 0, "`size_object` agrees: zero words");
        // The wire half still works for an immediate.
        let image = encode_term(term, &atoms).expect("an immediate encodes");
        assert_eq!(
            image,
            encode_term(copy, &atoms).expect("so does its copy"),
            "byte-identical images"
        );
        let mut rt_heap = Heap::new(64).expect("round-trip heap");
        let mut rt_atoms = atoms.clone();
        let rt = decode_term(&image, &mut rt_heap, &mut rt_atoms).expect("the image decodes");
        assert!(
            compare::eq(term, rt, &rt_atoms),
            "`binary_to_term` of an immediate image is =:= the original"
        );
    }
}

/// **The literal evidence, explicitly.** A literal-tagged subterm must come out of the copy (a)
/// equal by `compare::eq`, (b) byte-identical under `term_to_binary`, and (c) still *shared by
/// identity* — the same literal pointer. That triple is why sharing a literal is a residency
/// decision and not a term-shape divergence.
#[test]
fn a_literal_subterm_stays_equal_byte_identical_and_shared_by_identity() {
    let mut src = Src::new();
    let inner = src
        .heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the inner tuple fits");
    let literal = make_literal(inner);
    let root = src
        .heap
        .tuple(&[literal, make_small(9)])
        .expect("the outer tuple fits");

    let source_words = size_object(root);
    let mut dst = Heap::new(source_words + 8).expect("destination heap");
    let copy = copy_into(root, &mut dst);

    // (a) equal by `compare::eq`.
    assert!(
        compare::eq(root, copy, &src.atoms),
        "the copy is =:= the source"
    );
    // (b) byte-identical under `term_to_binary`.
    let source_image = encode_term(root, &src.atoms).expect("the source encodes");
    let copy_image = encode_term(copy, &src.atoms).expect("the copy encodes");
    assert_eq!(
        source_image, copy_image,
        "sharing a literal must not change a single wire byte"
    );
    // (c) still shared by identity: the child word in the copy IS the source's literal pointer.
    // SAFETY: `copy` is a live 2-tuple, so element slot 0 is readable.
    let copied_child = unsafe { tuple_element(copy, 0) };
    assert_eq!(
        copied_child.raw(),
        literal.raw(),
        "the copied child is the *same* literal word, not a fresh copy of it"
    );
    assert!(is_literal(copied_child), "and it still carries TAG_LITERAL_PTR");
    // The outer object itself is a genuine copy in the destination (sharing is the literal's
    // property, not the parent's).
    assert_ne!(
        boxed_val(copy),
        boxed_val(root),
        "the outer tuple is materialised in the destination"
    );
    assert!(
        dst.used() < source_words,
        "and the literal's interior cost the destination nothing ({} of {source_words} words)",
        dst.used()
    );

    // `binary_to_term` of the copy's image produces the same *value* on a plain heap — the
    // receiver observes the identical term whether or not the sender shared a literal area.
    let mut rt_heap = Heap::new(source_image.len() * 8 + 256).expect("round-trip heap");
    let mut rt_atoms = src.atoms.clone();
    let rt = decode_term(&source_image, &mut rt_heap, &mut rt_atoms).expect("the image decodes");
    assert!(
        compare::eq(root, rt, &rt_atoms),
        "the decoded image is =:= the literal-bearing source"
    );

    // A literal *root* is shared even more directly: the copy IS the source word, and it costs the
    // destination exactly 0 words.
    let mut dst_root = Heap::new(8).expect("destination heap");
    let root_copy = copy_into(literal, &mut dst_root);
    assert_eq!(root_copy.raw(), literal.raw(), "a literal root crosses by identity");
    assert_eq!(dst_root.used(), 0, "and writes no destination word at all");
    assert!(
        compare::eq(literal, root_copy, &src.atoms),
        "the shared root is still =:= the source"
    );
    assert_eq!(
        encode_term(literal, &src.atoms).expect("the literal root encodes"),
        encode_term(root_copy, &src.atoms).expect("its shared copy encodes"),
        "byte-identical, because it is the same term"
    );
}

/// The whole corpus, one printed row each. Every row asserts the full evidence triple (structural
/// equality, byte-identical ETF, decode round trip) plus copy-of-copy idempotence, and the
/// final line names the corpus result.
#[test]
fn report_identity_corpus() {
    println!();
    println!(
        "identity: {} payload classes, each copied across the boundary and re-encoded \
         (Eterm = {} bytes)",
        CORPUS.len(),
        core::mem::size_of::<Eterm>()
    );
    println!(
        "{:<28} {:>8} {:>9} {:>10} {:>7} {:>7} {:>7} {:>7} {:>9}",
        "class",
        "src_words",
        "w_words",
        "etf_bytes",
        "=:=",
        "bytes=",
        "c_of_c",
        "roundt",
        "root_shrd"
    );

    let mut checked = 0usize;
    let mut wire_checked = 0usize;
    let mut shared = 0usize;
    for payload in CORPUS {
        let mut src = Src::new();
        let term = (payload.build)(&mut src);
        assert!(
            !is_immediate(term),
            "{}: every corpus entry is a compound term",
            payload.name
        );
        let evidence = check(
            payload.name,
            term,
            &src,
            payload.sharing,
            payload.wire,
        );
        if evidence.wire_refused {
            println!(
                "{:<28} {:>8} {:>9} {:>10} {:>7} {:>7} {:>7} {:>7} {:>7}",
                payload.name,
                evidence.source_words,
                evidence.written_words,
                "-",
                yes(evidence.equal),
                "n/a",
                yes(evidence.copy_of_copy),
                "n/a",
                yes(evidence.shared_by_identity)
            );
        } else {
            println!(
                "{:<28} {:>8} {:>9} {:>10} {:>7} {:>7} {:>7} {:>7} {:>7}",
                payload.name,
                evidence.source_words,
                evidence.written_words,
                evidence.etf_bytes,
                yes(evidence.equal),
                yes(evidence.bytes_identical),
                yes(evidence.copy_of_copy),
                yes(evidence.round_trip),
                yes(evidence.shared_by_identity)
            );
            wire_checked += 1;
        }
        assert!(evidence.equal, "{}: =:= the source", payload.name);
        assert!(evidence.copy_of_copy, "{}: the copy is idempotent", payload.name);
        checked += 1;
        if evidence.shared_by_identity {
            shared += 1;
        }
    }

    println!();
    println!(
        "identity: {checked} classes copied and compared; {wire_checked} also byte-compared and \
         round-tripped through ETF; {shared} crossed by identity at its root (the literal-area \
         member), and the literal-child row wrote fewer words while staying a fresh object"
    );
    // The corpus must actually contain every point of the sharing spectrum, or the report would be
    // silent about the very case it exists to explain.
    assert_eq!(
        shared, 1,
        "exactly one corpus entry crosses by identity at its root — the literal payload graph"
    );
    assert_eq!(checked, CORPUS.len(), "every corpus entry is measured");
    assert_eq!(
        wire_checked,
        CORPUS.len() - 1,
        "only the Local fun defers its wire image"
    );
}

/// `yes`/`no` for the printed boolean columns.
fn yes(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}
