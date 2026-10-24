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

//! The **construction seam** a native lowering of `bs_create_bin` builds on
//! ([`seam`](super::seam)).
//!
//! Three groups of cases:
//!
//! 1. [`append_plan`](super::seam::append_plan)'s accept set — the shapes it resolves to
//!    fields and widths, including the compiler's `z,5` `Operand::Typed` spelling of the
//!    accumulator and of every field value;
//! 2. its decline set, one shape per assert: the segments, flags, widths, units and
//!    operand kinds a lowering must not emit bytes for;
//! 3. [`open_append`](super::seam::open_append)'s run-time answer, and — the case that
//!    makes the seam worth having — **the word-identity pin**: for an accepted plan whose
//!    [`open_append`](super::seam::open_append) answer is `Some`, the family's own build
//!    publishes the accumulator `Eterm` itself, advanced by exactly the plan's tail, over
//!    a payload that never moved and never grew.
//!
//! The fixtures are the plain `bs_init_writable` accumulator the family's own suites
//! drive (`create/inplace_tests.rs`); the contrast case at the end drives the same
//! operands with a tail the payload cannot hold, so the `Some` answer is pinned to
//! *"no growth"* rather than merely *"the same term comes back"*.

use super::seam::{append_plan, open_append, AppendPlan, Field};
use super::*;

use beam_loader::{LiteralArea, Reg, TypeIndex, NO_FAIL};
use beam_term::make_small;

use crate::context::MinimalContext;

/// The fixture's `StrT` table — the `string` segment's byte source.
const STRT: &[u8] = b"abcdefQ";

/// A module naming every segment type these cases use, holding the `StrT` fixture.
fn seg_module() -> Module {
    let mut module = Module::default();
    for name in [
        "private_append",
        "append",
        "integer",
        "string",
        "binary",
        "float",
        "utf8",
        "all",
        "nonsense",
    ] {
        module.atom_table.intern(name);
    }
    module.strings = STRT.to_vec();
    module
}

/// The same module with its literal 0 set to the flag-atom list `names`.
fn flag_module(names: &[&str]) -> Module {
    let mut module = seg_module();
    let area = LiteralArea::decode(
        &[super::wide_tests::flags_etf(names)],
        &mut module.atom_table,
    )
    .expect("the flag literal decodes");
    module.literals = std::sync::Arc::new(area);
    module
}

/// The interned atom operand `name`.
fn atom(module: &Module, name: &str) -> Operand {
    Operand::Atom(module.atom_table.lookup(name).expect("the atom"))
}

/// The `all` size operand.
fn all(module: &Module) -> Operand {
    atom(module, "all")
}

/// One `[Type, Index, Unit, Flags, Value, Size]` segment group.
fn group(
    module: &Module,
    kind: &str,
    unit: i64,
    flags: Operand,
    value: Operand,
    size: Operand,
) -> Vec<Operand> {
    vec![
        atom(module, kind),
        Operand::Integer(1),
        Operand::Integer(unit),
        flags,
        value,
        size,
    ]
}

/// A `private_append`/`append` of the accumulator in `x(0)` with size `all`.
fn append(module: &Module, kind: &str, unit: i64) -> Vec<Operand> {
    group(module, kind, unit, Operand::Nil, Operand::X(0), all(module))
}

/// An `integer` field of `bits` bits reading `value`.
fn int(module: &Module, value: Operand, bits: i64) -> Vec<Operand> {
    group(
        module,
        "integer",
        1,
        Operand::Nil,
        value,
        Operand::Integer(bits),
    )
}

/// An `integer` field with an explicit flags operand.
fn flagged_int(module: &Module, flags: Operand, value: Operand, bits: i64) -> Vec<Operand> {
    group(
        module,
        "integer",
        1,
        flags,
        value,
        Operand::Integer(bits),
    )
}

/// The `(payload base, window start, window size)` of a live bitstring — exactly what an
/// in-place append publishes.
fn window(term: Eterm) -> (*const u8, usize, usize) {
    let (base, size, off) = raw::heap_bits(term).expect("a readable bitstring");
    (base, off, size)
}

/// The bits a [`Built`] build produced, zero-padded to whole bytes.
fn bytes_of(term: Eterm) -> (Vec<u8>, usize) {
    built_bytes(Built::Term(term))
}

/// The roster index of the payload at `base`.
fn payload_index(machine: &Machine, base: *const u8) -> usize {
    machine
        .refc_binaries()
        .iter()
        .position(|binary| binary.data_ptr() as *const u8 == base)
        .expect("the accumulator's payload is on the machine's off-heap roster")
}

// ---------------------------------------------------------------------------
// 1. The accept set
// ---------------------------------------------------------------------------

/// `<<Acc/binary, C:8>>`-shaped plans — an accumulator append plus one whole-byte
/// integer field — resolve to exactly one [`Field::Bits`] and the field's width.
#[test]
fn an_append_and_a_whole_byte_integer_is_a_plan() {
    let module = seg_module();
    for bits in [8i64, 16, 32, 64] {
        let segments: Vec<Operand> = [
            append(&module, "private_append", 1),
            int(&module, Operand::X(1), bits),
        ]
        .concat();
        let plan: AppendPlan = append_plan(&module, &segments).expect("an accepted plan");
        assert_eq!(plan.source, Operand::X(0), "{bits}: the accumulator operand");
        assert_eq!(plan.tail_bits, bits as u32, "{bits}: the summed tail");
        assert_eq!(
            plan.fields,
            vec![Field::Bits {
                value: Operand::X(1),
                bits: bits as u32,
            }],
            "{bits}: one whole-byte field"
        );
    }
}

/// A **sub-word but whole-byte** field (a 24-bit one, the shape the compiler emits for
/// `<<C:24>>`-style data) is accepted through the general `integer` form, not just the
/// 1/2/4/8-byte direct-store one.
#[test]
fn a_three_byte_field_is_accepted_through_the_general_integer_form() {
    let module = seg_module();
    let segments: Vec<Operand> = [
        append(&module, "private_append", 1),
        int(&module, Operand::X(2), 24),
    ]
    .concat();
    let plan = append_plan(&module, &segments).expect("an accepted plan");
    assert_eq!(plan.tail_bits, 24);
    assert_eq!(
        plan.fields,
        vec![Field::Bits {
            value: Operand::X(2),
            bits: 24,
        }]
    );
}

/// Several fields in one instruction accumulate in order, and their widths sum.
#[test]
fn consecutive_fields_are_listed_in_instruction_order() {
    let module = seg_module();
    let segments: Vec<Operand> = [
        append(&module, "private_append", 8),
        int(&module, Operand::X(1), 8),
        int(&module, Operand::X(2), 32),
    ]
    .concat();
    let plan = append_plan(&module, &segments).expect("an accepted plan");
    assert_eq!(plan.tail_bits, 40);
    assert_eq!(
        plan.fields,
        vec![
            Field::Bits {
                value: Operand::X(1),
                bits: 8,
            },
            Field::Bits {
                value: Operand::X(2),
                bits: 32,
            },
        ]
    );
}

/// The compiler's real spelling of both the accumulator and the field value is the `z,5`
/// `Operand::Typed` form (`{tr,{x,1},{t_bitstring,8,true}}`, `{tr,{x,0},{t_integer,{0,255}}}`,
/// from a live `erlc -S` of an accumulator loop), and it resolves to the same plan shape —
/// the operand is carried through verbatim so a lowering can bind it the ordinary way.
#[test]
fn the_compilers_typed_operand_spelling_is_accepted() {
    let module = seg_module();
    let typed_acc = Operand::Typed {
        reg: Reg::X(0),
        ty: TypeIndex(0),
    };
    let typed_value = Operand::Typed {
        reg: Reg::X(1),
        ty: TypeIndex(0),
    };
    let segments: Vec<Operand> = [
        group(&module, "private_append", 8, Operand::Nil, typed_acc.clone(), all(&module)),
        int(&module, typed_value.clone(), 8),
    ]
    .concat();
    let plan = append_plan(&module, &segments).expect("an accepted plan");
    assert_eq!(plan.source, typed_acc, "the accumulator operand is carried through");
    assert_eq!(plan.tail_bits, 8);
    assert_eq!(
        plan.fields,
        vec![Field::Bits {
            value: typed_value,
            bits: 8,
        }]
    );
}

/// A `string` segment's `StrT` window is resolved at plan time and copied out, so a
/// lowering never bakes the table offset into code.
#[test]
fn a_string_segment_carries_its_literal_bytes() {
    let module = seg_module();
    // `StrT` is `b"abcdefQ"`: offset 2 + 3 bytes is `b"cde"`.
    let segments: Vec<Operand> = [
        append(&module, "private_append", 1),
        group(
            &module,
            "string",
            1,
            Operand::Nil,
            Operand::Integer(2),
            Operand::Integer(3),
        ),
    ]
    .concat();
    let plan = append_plan(&module, &segments).expect("an accepted plan");
    assert_eq!(plan.tail_bits, 24, "three bytes of constant");
    assert_eq!(
        plan.fields,
        vec![Field::ConstBytes {
            bytes: b"cde".to_vec(),
        }]
    );
}

/// An integer field and a `string` constant mix in one plan, in instruction order.
#[test]
fn an_integer_and_a_string_mix_in_one_plan() {
    let module = seg_module();
    let segments: Vec<Operand> = [
        append(&module, "private_append", 8),
        int(&module, Operand::X(1), 8),
        group(
            &module,
            "string",
            1,
            Operand::Nil,
            Operand::Integer(0),
            Operand::Integer(2),
        ),
    ]
    .concat();
    let plan = append_plan(&module, &segments).expect("an accepted plan");
    assert_eq!(plan.tail_bits, 24);
    assert_eq!(
        plan.fields,
        vec![
            Field::Bits {
                value: Operand::X(1),
                bits: 8,
            },
            Field::ConstBytes {
                bytes: b"ab".to_vec(),
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// 2. The decline set — one shape per assert
// ---------------------------------------------------------------------------

/// Every shape a lowering must not emit bytes for is declined. The list is deliberately
/// exhaustive over the prepared program's variants: the `append` positions, the segment
/// kinds, the width forms, the flags, the append units and the operand kinds.
#[test]
fn every_shape_outside_the_accept_set_is_declined() {
    let module = seg_module();
    let acc = append(&module, "private_append", 1);
    let field = int(&module, Operand::X(1), 8);

    // A plain (`private: false`) append: not the writable accumulator form.
    let plain: Vec<Operand> = [append(&module, "append", 1), field.clone()].concat();
    assert_eq!(append_plan(&module, &plain), None, "a plain `append`");

    // An append that is not the first segment.
    let later: Vec<Operand> = [field.clone(), acc.clone()].concat();
    assert_eq!(append_plan(&module, &later), None, "an append at position 1");

    // A second append after an accepted one.
    let twice: Vec<Operand> = [acc.clone(), field.clone(), append(&module, "private_append", 1)]
        .concat();
    assert_eq!(append_plan(&module, &twice), None, "a second append");

    // An append with no fields at all: nothing to emit.
    assert_eq!(
        append_plan(&module, &acc),
        None,
        "an append with no fields"
    );

    // An append unit that does not divide 8 keeps the family's `src_bits % unit` gate
    // live, which the seam cannot decide statically.
    let odd_unit: Vec<Operand> = [append(&module, "private_append", 3), field.clone()].concat();
    assert_eq!(append_plan(&module, &odd_unit), None, "an append unit 3");

    // Every other segment kind.
    let binary_all: Vec<Operand> = [
        acc.clone(),
        group(&module, "binary", 8, Operand::Nil, Operand::X(1), all(&module)),
    ]
    .concat();
    assert_eq!(
        append_plan(&module, &binary_all),
        None,
        "a `binary` segment sized `all`"
    );
    let binary_fixed: Vec<Operand> = [
        acc.clone(),
        group(
            &module,
            "binary",
            8,
            Operand::Nil,
            Operand::X(1),
            Operand::Integer(1),
        ),
    ]
    .concat();
    assert_eq!(
        append_plan(&module, &binary_fixed),
        None,
        "a statically sized `binary` segment"
    );
    let float: Vec<Operand> = [
        acc.clone(),
        group(
            &module,
            "float",
            1,
            Operand::Nil,
            Operand::X(1),
            Operand::Integer(64),
        ),
    ]
    .concat();
    assert_eq!(append_plan(&module, &float), None, "a `float` segment");
    let utf: Vec<Operand> = [
        acc.clone(),
        group(&module, "utf8", 1, Operand::Nil, Operand::X(1), Operand::Nil),
    ]
    .concat();
    assert_eq!(append_plan(&module, &utf), None, "a `utf8` segment");

    // A runtime (`Dynamic`) width, and an `all` integer size.
    let dynamic: Vec<Operand> = [acc.clone(), int(&module, Operand::X(1), 0)].concat();
    assert_eq!(append_plan(&module, &dynamic), None, "a zero-width field");
    let dynamic: Vec<Operand> = [
        acc.clone(),
        group(
            &module,
            "integer",
            1,
            Operand::Nil,
            Operand::X(1),
            Operand::X(2),
        ),
    ]
    .concat();
    assert_eq!(
        append_plan(&module, &dynamic),
        None,
        "a runtime (`size(Expr)`) width"
    );
    let all_int: Vec<Operand> = [
        acc.clone(),
        group(&module, "integer", 1, Operand::Nil, Operand::X(1), all(&module)),
    ]
    .concat();
    assert_eq!(
        append_plan(&module, &all_int),
        None,
        "an `all` integer size"
    );

    // Widths a lowering cannot write from one register.
    assert_eq!(
        append_plan(&module, &[acc.clone(), int(&module, Operand::X(1), 6)].concat()),
        None,
        "a six-bit field"
    );
    assert_eq!(
        append_plan(
            &module,
            &[acc.clone(), int(&module, Operand::X(1), 128)].concat()
        ),
        None,
        "a 128-bit field"
    );

    // Operand kinds the seam cannot bind.
    assert_eq!(
        append_plan(&module, &[acc.clone(), int(&module, Operand::Y(0), 8)].concat()),
        None,
        "a Y-register field value"
    );
    assert_eq!(
        append_plan(
            &module,
            &[acc.clone(), int(&module, Operand::Literal(0), 8)].concat()
        ),
        None,
        "a literal-area field value"
    );
    let y_source: Vec<Operand> = [
        group(&module, "private_append", 1, Operand::Nil, Operand::Y(0), all(&module)),
        field.clone(),
    ]
    .concat();
    assert_eq!(
        append_plan(&module, &y_source),
        None,
        "a Y-register accumulator"
    );

    // A `StrT` window the module cannot resolve.
    let bad_string: Vec<Operand> = [
        acc.clone(),
        group(
            &module,
            "string",
            1,
            Operand::Nil,
            Operand::Integer(100),
            Operand::Integer(2),
        ),
    ]
    .concat();
    assert_eq!(
        append_plan(&module, &bad_string),
        None,
        "a `StrT` window outside the table"
    );

    // A truncated segment group, and an unknown segment type.
    assert_eq!(
        append_plan(&module, &field[..5]),
        None,
        "a truncated segment group"
    );
    let unknown: Vec<Operand> = [
        acc.clone(),
        group(
            &module,
            "nonsense",
            1,
            Operand::Nil,
            Operand::X(1),
            Operand::Integer(8),
        ),
    ]
    .concat();
    assert_eq!(
        append_plan(&module, &unknown),
        None,
        "an unknown segment type"
    );
}

/// The two flags that change a field's layout — `little` and `signed` — are declined on
/// their own; the defaults (`big`/`unsigned`) contribute nothing and stay accepted.
#[test]
fn layout_changing_flags_are_declined() {
    let with_little = flag_module(&["little"]);
    let with_signed = flag_module(&["signed"]);
    let with_defaults = flag_module(&["big", "unsigned"]);
    let flags = Operand::Literal(0);
    for (module, expected) in [
        (&with_little, false),
        (&with_signed, false),
        (&with_defaults, true),
    ] {
        let segments: Vec<Operand> = [
            append(module, "private_append", 1),
            flagged_int(module, flags.clone(), Operand::X(1), 16),
        ]
        .concat();
        assert_eq!(
            append_plan(module, &segments).is_some(),
            expected,
            "flags {flags:?} on a 16-bit field"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. `open_append`: the run-time half
// ---------------------------------------------------------------------------

/// `open_append` reports the payload base and the window end, and declines a tail the
/// payload cannot hold.
#[test]
fn open_append_reports_the_payload_and_the_window_end() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    // An 8-byte payload is 64 bits.
    let acc = raw::make_writable(&mut machine, &mut ctx, 8).expect("a writable accumulator");
    let (base, start, end) = window(acc);
    assert_eq!((start, end), (0, 0), "`bs_init_writable` reports 0 bits");
    assert_eq!(
        open_append(&machine, acc, 64),
        Some((base as usize, 0)),
        "the whole payload is available from bit 0"
    );
    assert_eq!(open_append(&machine, acc, 65), None, "one bit too many");

    // Fill 8 bits through the family's own build, then re-ask.
    machine.set_x(0, acc).expect("x(0) in range");
    machine.set_x(1, make_small(0x41)).expect("x(1) in range");
    let segments: Vec<Operand> = {
        let module = seg_module();
        [append(&module, "private_append", 1), int(&module, Operand::X(1), 8)].concat()
    };
    let module = seg_module();
    let built = assemble(
        &mut machine,
        &module,
        &LabelMap::default(),
        NO_FAIL,
        &segments,
        &mut ctx,
    )
    .expect("assemble")
    .expect("the build succeeds");
    let term = built_term(built, &mut machine, &mut ctx).expect("materialises");
    assert_eq!(term, acc, "the build publishes the accumulator");
    assert_eq!(window(term).2, 8, "the window end advanced by 8 bits");
    assert_eq!(
        open_append(&machine, acc, 56),
        Some((base as usize, 8)),
        "the remaining 56 bits are still available"
    );
    assert_eq!(
        open_append(&machine, acc, 57),
        None,
        "one bit past the remaining room"
    );
}

/// A source that is not a writable accumulator is declined: an on-heap small binary, an
/// off-heap ordinary (non-writable) refc binary, and an accumulator whose window is not
/// whole-byte aligned.
#[test]
fn open_append_declines_a_source_it_may_not_write() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    // An on-heap `ErlHeapBits` (at/below the 64-byte on-heap limit) has no payload.
    let small = raw::make_binary(&mut machine, &mut ctx, b"abcd", 32).expect("a small binary");
    assert_eq!(open_append(&machine, small, 8), None, "an on-heap binary");
    // Above the limit it is an off-heap refc binary — but not one allocated for
    // appending, so it is not writable.
    let big = raw::make_binary(&mut machine, &mut ctx, &[0u8; 100], 800).expect("a refc binary");
    assert_eq!(
        open_append(&machine, big, 8),
        None,
        "an off-heap binary that was not allocated for appending"
    );
    // A writable accumulator whose window end is not whole-byte aligned: a 3-bit field
    // leaves `end == 3`, where the family's append-unit gate is no longer decidable.
    let module = seg_module();
    let acc = raw::make_writable(&mut machine, &mut ctx, 8).expect("a writable accumulator");
    machine.set_x(0, acc).expect("x(0) in range");
    machine.set_x(1, make_small(1)).expect("x(1) in range");
    let segments: Vec<Operand> = {
        let mut segments = append(&module, "private_append", 1);
        segments.extend(group(
            &module,
            "integer",
            1,
            Operand::Nil,
            Operand::X(1),
            Operand::Integer(3),
        ));
        segments
    };
    let built = assemble(
        &mut machine,
        &module,
        &LabelMap::default(),
        NO_FAIL,
        &segments,
        &mut ctx,
    )
    .expect("assemble")
    .expect("the build succeeds");
    let term = built_term(built, &mut machine, &mut ctx).expect("materialises");
    assert_eq!(window(term).2, 3, "the window end is 3 bits");
    assert_eq!(
        open_append(&machine, acc, 8),
        None,
        "a window that is not whole-byte aligned"
    );
}

// ---------------------------------------------------------------------------
// 4. The word-identity pin — what makes the seam worth having
// ---------------------------------------------------------------------------

/// **For an accepted plan, `open_append`'s `Some` means the family's build is the
/// accumulator `Eterm` itself, advanced by exactly the plan's tail, over a payload that
/// never moved and never grew.**
///
/// This is the promise a native lowering stores the source register on, driven here
/// end-to-end through the real `assemble` entry on the very operands the plan was built
/// from: the built term is `Eterm`-identical to the accumulator, the payload base is
/// unchanged, the window end advanced by `tail_bits`, no payload was allocated and the
/// payload's growth counters are untouched.
#[test]
fn an_accepted_plan_builds_word_identically_and_grows_nothing() {
    let module = seg_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let acc = raw::make_writable(&mut machine, &mut ctx, 8).expect("a writable accumulator");
    machine.set_x(0, acc).expect("x(0) in range");
    machine.set_x(1, make_small(0x41)).expect("x(1) in range");
    // `<<Acc/binary, C:8, "bc">>` — one integer field and a `string` constant.
    let segments: Vec<Operand> = {
        let mut segments = append(&module, "private_append", 8);
        segments.extend(int(&module, Operand::X(1), 8));
        segments.extend(group(
            &module,
            "string",
            1,
            Operand::Nil,
            Operand::Integer(1),
            Operand::Integer(2),
        ));
        segments
    };
    let plan = append_plan(&module, &segments).expect("an accepted plan");
    assert_eq!(plan.tail_bits, 24, "one byte + two constant bytes");
    let (base, start, end) = window(acc);
    assert_eq!(
        open_append(&machine, acc, plan.tail_bits),
        Some((base as usize, end)),
        "the seam promises room for exactly the plan's tail"
    );
    let index = payload_index(&machine, base);
    let growth = machine.refc_binaries()[index].growth_counters();
    let capacity = machine.refc_binaries()[index].capacity();
    let payloads = machine.refc_binaries().len();

    let built = assemble(
        &mut machine,
        &module,
        &LabelMap::default(),
        NO_FAIL,
        &segments,
        &mut ctx,
    )
    .expect("assemble")
    .expect("the build succeeds");
    let term = built_term(built, &mut machine, &mut ctx).expect("materialises");

    // The promise: the build publishes the accumulator word itself.
    assert_eq!(term, acc, "the build publishes the accumulator term");
    let (after_base, after_start, after_end) = window(term);
    assert_eq!(after_base, base, "the payload never moves");
    assert_eq!(after_start, start, "the window still starts where it did");
    assert_eq!(
        after_end,
        end + plan.tail_bits as usize,
        "the window end advanced by exactly the plan's tail"
    );
    assert_eq!(
        payload_index(&machine, after_base),
        index,
        "the roster entry is the same payload"
    );
    assert_eq!(
        machine.refc_binaries().len(),
        payloads,
        "no payload was allocated"
    );
    assert_eq!(
        machine.refc_binaries()[index].growth_counters(),
        growth,
        "no in-place grow, no move, no byte copy"
    );
    assert_eq!(
        machine.refc_binaries()[index].capacity(),
        capacity,
        "the capacity is untouched"
    );
    // And the bits are the plan's fields, in order.
    let (bytes, nbits) = bytes_of(term);
    assert_eq!(nbits, plan.tail_bits as usize, "the exact bit count");
    assert_eq!(bytes, vec![0x41, b'b', b'c'], "the fields' bytes");
}

/// The contrast that proves `Some` means *"no growth"* rather than merely *"the same term
/// comes back"*: a tail the payload cannot hold is declined by the seam, and the family's
/// own build reaches the same term **by growing the payload** — which is exactly what a
/// lowering must not let happen mid-instruction (a move republishes two pointer words).
#[test]
fn a_tail_beyond_the_capacity_is_declined_because_the_family_must_grow() {
    let module = seg_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    // A 4-byte payload is 32 bits; the plan below writes 64.
    let acc = raw::make_writable(&mut machine, &mut ctx, 4).expect("a writable accumulator");
    machine.set_x(0, acc).expect("x(0) in range");
    machine.set_x(1, make_small(7)).expect("x(1) in range");
    let segments: Vec<Operand> = [
        append(&module, "private_append", 1),
        int(&module, Operand::X(1), 64),
    ]
    .concat();
    let plan = append_plan(&module, &segments).expect("still a valid plan");
    assert_eq!(plan.tail_bits, 64);
    let (base, _, end) = window(acc);
    let index = payload_index(&machine, base);
    let growth = machine.refc_binaries()[index].growth_counters();
    assert_eq!(
        open_append(&machine, acc, plan.tail_bits),
        None,
        "the payload cannot hold the plan's tail"
    );

    let built = assemble(
        &mut machine,
        &module,
        &LabelMap::default(),
        NO_FAIL,
        &segments,
        &mut ctx,
    )
    .expect("assemble")
    .expect("the build still succeeds");
    let term = built_term(built, &mut machine, &mut ctx).expect("materialises");
    // The term is still the accumulator's word (a move republishes the pair) …
    assert_eq!(term, acc, "the build publishes the accumulator term");
    assert_eq!(window(term).2, end + 64, "the window end advanced");
    // … but only after the payload grew, which is the cost `None` sends the tier around.
    let after = machine.refc_binaries()[payload_index(&machine, window(term).0)].growth_counters();
    assert!(
        after.in_place_grows + after.moves > growth.in_place_grows + growth.moves,
        "the family had to grow the payload: {growth:?} -> {after:?}"
    );
}
