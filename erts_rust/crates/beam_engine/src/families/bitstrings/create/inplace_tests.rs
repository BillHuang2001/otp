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

//! The **in-place `private_append`** arm of `bs_create_bin` — C's
//! `erts_bs_private_append_checked` writable path (`erl_bits.c:1943-2020`).
//!
//! Every case here is checked against the **copy-rebuild** form (`append`, or a
//! `private_append` whose source is not a writable accumulator), which is the
//! reference this arm must stay byte-identical to; the oracle literals quoted at the
//! byte-exactness cases are the system OTP 29.1.1 / erts-17.1 `erlc`-compiled values.
//!
//! What each test pins is stated on the test itself. The invariants that make the
//! in-place mutation observationally safe are documented on
//! [`raw::InPlace`](crate::families::bitstrings::raw::InPlace).

use super::*;
use beam_loader::NO_FAIL;

use crate::context::{ExecutionContext, MinimalContext, RootSet};

/// A `MinimalContext` arena that counts the **rooted** allocation calls the build
/// makes — one per `[BinRef][ErlSubBits]` pair and per fresh payload reservation, which
/// is exactly what an in-place append must *not* pay.
struct Counting {
    arena: MinimalContext,
    rooted_calls: usize,
}

impl Counting {
    fn new() -> Counting {
        Counting {
            arena: MinimalContext::new(),
            rooted_calls: 0,
        }
    }
}

impl ExecutionContext for Counting {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.arena.call_bif(module, function, arity, args)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }

    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        self.rooted_calls += 1;
        self.arena.alloc_words_rooted(n, roots)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }
}

/// A module naming the segment type atoms these cases use, plus the `all` size atom.
fn seg_module() -> Module {
    let mut module = Module::default();
    for name in ["private_append", "append", "integer", "all"] {
        module.atom_table.intern(name);
    }
    module
}

/// One `private_append`/`append` segment over the accumulator in `x(0)` with size `all`.
fn append_segment(module: &Module, kind: &str) -> Vec<Operand> {
    vec![
        Operand::Atom(module.atom_table.lookup(kind).expect("the type atom")),
        Operand::Integer(1),
        Operand::Integer(1),
        Operand::Nil,
        Operand::X(0),
        Operand::Atom(module.atom_table.lookup("all").expect("the all atom")),
    ]
}

/// One `integer` segment of `bits` bits holding the small integer `value`.
fn int_segment(module: &Module, value: i64, bits: i64) -> Vec<Operand> {
    vec![
        Operand::Atom(module.atom_table.lookup("integer").expect("the type atom")),
        Operand::Integer(2),
        Operand::Integer(1),
        Operand::Nil,
        Operand::Integer(value),
        Operand::Integer(bits),
    ]
}

/// Run the segment walk and return the build's `(bytes, nbits)` view.
fn build_bytes(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    segments: &[Operand],
) -> (Vec<u8>, usize) {
    let built = assemble(machine, module, &LabelMap::default(), NO_FAIL, segments, ctx)
        .expect("assemble")
        .expect("the build succeeds");
    built_bytes(built)
}

/// Run the segment walk and return the built **term**.
fn build_term(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    segments: &[Operand],
) -> Eterm {
    let built = assemble(machine, module, &LabelMap::default(), NO_FAIL, segments, ctx)
        .expect("assemble")
        .expect("the build succeeds");
    built_term(built, machine, ctx).expect("the term materialises")
}

/// The `(payload base, window end)` of a live bitstring — the two facts an in-place
/// append must leave `base` and grow `end` on.
fn window(term: Eterm) -> (*const u8, usize, usize) {
    // `raw::heap_bits` answers `(base, bit size, bit offset)`; report it as
    // `(base, window start, window size)`.
    let (base, size, off) = raw::heap_bits(term).expect("a readable bitstring");
    (base, off, size)
}

/// The accumulated bits of a term, zero-padded to whole bytes.
fn bytes_of(term: Eterm) -> (Vec<u8>, usize) {
    built_bytes(Built::Term(term))
}

// ---------------------------------------------------------------------------
// 1. The compiler's accumulator loop appends IN PLACE
// ---------------------------------------------------------------------------

/// `bs_init_writable` + a chain of `private_append` builds publishes **the same term**
/// every iteration: no fresh payload, no `[BinRef][ErlSubBits]` reservation, and the
/// payload's base pointer never changes — C's `sb->end = new_position`
/// (`erl_bits.c:1975`), which is what makes the accumulator loop amortised O(1) per
/// iteration instead of O(n).
#[test]
fn the_accumulator_loop_appends_in_place_and_allocates_once() {
    let module = seg_module();
    let mut machine = Machine::new();
    let mut ctx = Counting::new();
    let capacity = 64;
    let acc = raw::make_writable(&mut machine, &mut ctx, capacity).expect("a writable binary");
    let (base, start, size) = window(acc);
    assert_eq!((start, size), (0, 0), "`bs_init_writable` reports 0 bits");
    assert_eq!(
        ctx.rooted_calls, 1,
        "the accumulator itself is the build's only rooted reservation"
    );
    let payloads = machine.refc_binaries.len();
    assert_eq!(payloads, 1, "one payload backs the accumulator");

    // Twenty iterations of `<<Acc/bitstring, N:8>>` — every one of them in place.
    let mut term = acc;
    for n in 0..20i64 {
        machine.set_x(0, term).expect("x(0) in range");
        let mut segments = append_segment(&module, "private_append");
        segments.extend(int_segment(&module, n, 8));
        term = build_term(&module, &mut machine, &mut ctx, &segments);
        assert_eq!(
            term, acc,
            "iteration {n}: the build publishes the accumulator term itself"
        );
        let (b, s, e) = window(term);
        assert_eq!(b, base, "iteration {n}: the payload never moves");
        assert_eq!(s, 0, "iteration {n}: the window still starts at 0");
        assert_eq!(e, (n as usize + 1) * 8, "iteration {n}: `end` was bumped");
    }
    assert_eq!(
        ctx.rooted_calls, 1,
        "twenty in-place appends allocate NOTHING (a copy-rebuild would allocate 20 \
         payloads + 20 ErlSubBits pairs)"
    );
    assert_eq!(
        machine.refc_binaries.len(),
        payloads,
        "no payload was created after the accumulator"
    );
    let (bytes, nbits) = bytes_of(term);
    assert_eq!(nbits, 160);
    assert_eq!(bytes, (0..20u8).collect::<Vec<u8>>());
}

// ---------------------------------------------------------------------------
// 2. Byte-exactness against the copy-rebuild path
// ---------------------------------------------------------------------------

/// The same bits through the in-place and the copy-rebuild route are **byte-identical**
/// at every shape: byte-aligned, a 4-bit offset, the capacity boundary, an overflow
/// (which promotes mid-build), a 0-bit append, and a source that is not the accumulator.
///
/// Oracle: `erl -noshell -eval 'io:format("~p~n",[<< <<1,2,3>>/bitstring, 3:4, 5:4 >>]),halt().'`
/// ⇒ `<<1,2,3,53>>` — the `4-bit offset` row below.
#[test]
fn in_place_and_copy_rebuild_agree_byte_for_byte() {
    let module = seg_module();
    // (seed bytes, appended integer segments, expected bytes after the seed)
    let cases: &[(&[u8], &[(i64, i64)], &[u8])] = &[
        // Byte-aligned appends: the whole-byte fast path.
        (b"\x01\x02\x03", &[(7, 8), (255, 8)], b"\x07\xff"),
        // A 4-bit offset: `<<Seed/bitstring, 3:4, 5:4>>` = <<1,2,3,53>> (the oracle
        // line above).
        (b"\x01\x02\x03", &[(3, 4), (5, 4)], b"\x35"),
        // A 0-bit append contributes nothing.
        (b"\xAA\xBB", &[(0, 0)], b""),
        // A 12-bit append off a 4-bit offset: two bytes, the last 4 bits zero.
        (b"\x0F", &[(0xABC, 12)], b"\xAB\xC0"),
    ];
    for &(seed, appends, expected_tail) in cases {
        // The in-place route: a writable accumulator seeded with `seed`.
        let mut in_machine = Machine::new();
        let mut in_ctx = Counting::new();
        let acc = raw::make_writable(&mut in_machine, &mut in_ctx, 64).expect("writable");
        in_machine.set_x(0, acc).expect("x(0)");
        let mut segments = append_segment(&module, "private_append");
        for &(value, bits) in appends {
            segments.extend(int_segment(&module, value, bits));
        }
        let mut seed_segments = append_segment(&module, "private_append");
        seed_segments.extend(seed.iter().map(|&b| int_segment(&module, i64::from(b), 8)).flatten());
        let seeded = build_term(&module, &mut in_machine, &mut in_ctx, &seed_segments);
        in_machine.set_x(0, seeded).expect("x(0)");
        let in_place = build_term(&module, &mut in_machine, &mut in_ctx, &segments);

        // The copy-rebuild route over a source that is NOT writable.
        let mut cb_machine = Machine::new();
        let mut cb_ctx = MinimalContext::new();
        let source = raw::make_binary(&mut cb_machine, &mut cb_ctx, seed, seed.len() * 8)
            .expect("a heap binary");
        assert!(
            raw::writable_append_target(&cb_machine, source).is_none(),
            "a `make_binary` result is never a writable target"
        );
        cb_machine.set_x(0, source).expect("x(0)");
        let mut cb_segments = append_segment(&module, "private_append");
        for &(value, bits) in appends {
            cb_segments.extend(int_segment(&module, value, bits));
        }
        let copy_rebuild = build_bytes(&module, &mut cb_machine, &mut cb_ctx, &cb_segments);

        let mut expected = seed.to_vec();
        expected.extend_from_slice(expected_tail);
        let appended: usize = appends.iter().map(|&(_, bits)| bits as usize).sum();
        let bit_count = seed.len() * 8 + appended;
        assert_eq!(
            bytes_of(in_place),
            (expected.clone(), bit_count),
            "in place, seed={seed:?}, appends={appends:?}"
        );
        assert_eq!(
            copy_rebuild,
            (expected, bit_count),
            "copy-rebuild, seed={seed:?}, appends={appends:?}"
        );
    }
}

/// A build that exactly fills the payload stays in place; **one more bit** promotes to
/// the copy-rebuild form and the promoted result is still byte-identical to the
/// copy-rebuild path (the promotion copies the already-written bits out).
#[test]
fn an_append_at_and_past_the_capacity_boundary_is_exact() {
    let module = seg_module();
    let mut machine = Machine::new();
    let mut ctx = Counting::new();
    let capacity = 8;
    let acc = raw::make_writable(&mut machine, &mut ctx, capacity).expect("writable");
    machine.set_x(0, acc).expect("x(0)");
    // 64 bits fills the payload exactly — still in place.
    let mut segments = append_segment(&module, "private_append");
    for _ in 0..8 {
        segments.extend(int_segment(&module, 0xAB, 8));
    }
    let full = build_term(&module, &mut machine, &mut ctx, &segments);
    assert_eq!(
        full, acc,
        "a build that exactly fills the payload stays in place"
    );
    assert_eq!(window(full).2, 64, "the window is the whole payload");
    assert_eq!(ctx.rooted_calls, 1, "no allocation at the boundary");

    // One more byte outgrows the payload: the writer promotes mid-build.
    machine.set_x(0, full).expect("x(0)");
    let mut over = append_segment(&module, "private_append");
    over.extend(int_segment(&module, 0xCD, 8));
    let promoted = assemble(&mut machine, &module, &LabelMap::default(), NO_FAIL, &over, &mut ctx)
        .expect("assemble")
        .expect("the build succeeds");
    assert!(
        matches!(promoted, Built::Bytes(..)),
        "an overflowing build promotes to the copy-rebuild form"
    );
    let (bytes, nbits) = built_bytes(promoted);
    assert_eq!(nbits, 72);
    let mut expected = vec![0xABu8; 8];
    expected.push(0xCD);
    assert_eq!(bytes, expected, "the promoted bytes are the copy-rebuild's");
    // The accumulator's own window is UNBUMPED: the promotion publishes a new term.
    assert_eq!(window(full).2, 64, "the source window was left unbumped");
}

// ---------------------------------------------------------------------------
// 3. When the copy-rebuild path is taken
// ---------------------------------------------------------------------------

/// Only the first `private_append` of a *writable* accumulator is in place. A plain
/// `append`, a non-zero `start`, and an ordinary (non-writable) refc source all take
/// the copy-rebuild path and produce a **fresh** term.
#[test]
fn a_plain_append_a_non_zero_start_and_a_foreign_source_copy_rebuild() {
    let module = seg_module();

    // (a) a plain `append` segment of a writable accumulator.
    let mut machine = Machine::new();
    let mut ctx = Counting::new();
    let acc = raw::make_writable(&mut machine, &mut ctx, 64).expect("writable");
    machine.set_x(0, acc).expect("x(0)");
    let mut segments = append_segment(&module, "append");
    segments.extend(int_segment(&module, 1, 8));
    let plain = build_term(&module, &mut machine, &mut ctx, &segments);
    assert_ne!(plain, acc, "a plain `append` keeps the copy-rebuild route");
    assert_eq!(bytes_of(plain), (vec![1], 8));
    assert_eq!(window(acc).2, 0, "the accumulator was not extended");

    // (b) `start != 0` — C *asserts* `sb->start == 0` for both append forms.
    let mut machine = Machine::new();
    let mut ctx = Counting::new();
    let acc = raw::make_writable(&mut machine, &mut ctx, 64).expect("writable");
    machine.set_x(0, acc).expect("x(0)");
    let mut seed = append_segment(&module, "private_append");
    seed.extend(int_segment(&module, 0x5A, 8));
    let seeded = build_term(&module, &mut machine, &mut ctx, &seed);
    assert_eq!(seeded, acc, "the seed was in place");
    raw::set_ctx_start(seeded, 4);
    assert_eq!(window(seeded).1, 4, "the window now starts at bit 4");
    machine.set_x(0, seeded).expect("x(0)");
    let mut next = append_segment(&module, "private_append");
    next.extend(int_segment(&module, 0xFF, 8));
    let copied = build_term(&module, &mut machine, &mut ctx, &next);
    assert_ne!(copied, acc, "`start != 0` copy-rebuilds");
    // The copy carries the *windowed* bits: 4 bits of 0x5A then eight 1s.
    assert_eq!(bytes_of(copied), (vec![0xAF, 0xF0], 12));

    // (c) an ordinary refc binary (not a `bs_init_writable` payload).
    let mut machine = Machine::new();
    let mut ctx = Counting::new();
    let mut seed_bytes = vec![0u8; 80];
    seed_bytes[79] = 0x11;
    let source = raw::make_binary(&mut machine, &mut ctx, &seed_bytes, 640).expect("a refc binary");
    machine.set_x(0, source).expect("x(0)");
    let mut segments = append_segment(&module, "private_append");
    segments.extend(int_segment(&module, 0x22, 8));
    let copied = build_term(&module, &mut machine, &mut ctx, &segments);
    assert_ne!(
        copied, source,
        "an ordinary refc binary is never written in place"
    );
    let (bytes, nbits) = bytes_of(copied);
    assert_eq!(nbits, 648);
    assert_eq!(&bytes[..80], &seed_bytes[..]);
    assert_eq!(bytes[80], 0x22);
    // The source's own bytes are untouched.
    assert_eq!(raw::subject_bytes(source).expect("readable").0, &seed_bytes[..]);

    // (d) an on-heap (`ErlHeapBits`) source has no payload to write into.
    let mut machine = Machine::new();
    let mut ctx = Counting::new();
    let heap = raw::make_binary(&mut machine, &mut ctx, &[9, 8, 7], 24).expect("a heap binary");
    assert!(raw::writable_append_target(&machine, heap).is_none());
    machine.set_x(0, heap).expect("x(0)");
    let mut segments = append_segment(&module, "private_append");
    segments.extend(int_segment(&module, 6, 8));
    let copied = build_term(&module, &mut machine, &mut ctx, &segments);
    assert_ne!(copied, heap);
    assert_eq!(bytes_of(copied), (vec![9, 8, 7, 6], 32));
}

// ---------------------------------------------------------------------------
// 4. A failed build leaves no observable trace
// ---------------------------------------------------------------------------

/// A segment failure **after** the in-place append writes bits must leave the
/// accumulator's visible window exactly as it was: `end` is only bumped by
/// [`raw::InPlace::finish`], which a failing build never reaches. (Deliberately
/// stricter than C, which bumps the window in its sizing pass first —
/// `erl_bits.c:1975`.)
#[test]
fn a_failed_build_leaves_the_sources_visible_bits_unchanged() {
    let module = seg_module();
    let mut machine = Machine::new();
    let mut ctx = Counting::new();
    let acc = raw::make_writable(&mut machine, &mut ctx, 64).expect("writable");
    machine.set_x(0, acc).expect("x(0)");
    let mut seed = append_segment(&module, "private_append");
    seed.extend(int_segment(&module, 0x77, 8));
    let seeded = build_term(&module, &mut machine, &mut ctx, &seed);
    let before = bytes_of(seeded);

    // A negative `integer` size is the oracle's `badarg` (`<<1:-1>>`).
    machine.set_x(0, seeded).expect("x(0)");
    let mut failing = append_segment(&module, "private_append");
    failing.extend(int_segment(&module, 0xFF, 8));
    failing.extend(int_segment(&module, 1, -1));
    let err = match assemble(
        &mut machine,
        &module,
        &LabelMap::default(),
        NO_FAIL,
        &failing,
        &mut ctx,
    ) {
        Err(err) => err,
        Ok(_) => panic!("the build must fail"),
    };
    assert_eq!(err.as_atom(), Some("badarg"), "the failure reason");
    assert_eq!(
        window(seeded).2,
        8,
        "the accumulator's window was NOT bumped by the failed build"
    );
    assert_eq!(
        bytes_of(seeded),
        before,
        "the visible bits are exactly the pre-build ones"
    );

    // A later successful append still lands exactly where it should — the failed
    // attempt's bytes were invisible and are overwritten, not inherited.
    machine.set_x(0, seeded).expect("x(0)");
    let mut next = append_segment(&module, "private_append");
    next.extend(int_segment(&module, 0x01, 8));
    let extended = build_term(&module, &mut machine, &mut ctx, &next);
    assert_eq!(bytes_of(extended), (vec![0x77, 0x01], 16));
}
