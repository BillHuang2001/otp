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

//! The **bare-build** half of the construction seam
//! ([`fresh_plan`](super::seam::fresh_plan) / [`make_fresh_binary`](super::seam::make_fresh_binary)).
//!
//! Four groups of cases:
//!
//! 1. `fresh_plan`'s **accept set** — the `emqx_base62:decode/2` shape (`<<DecodeChar>>`, the
//!    once-per-character build), every static field width, multi-field builds whose total fits
//!    one machine word, and the x/y register spellings the loader and the compiler produce.
//! 2. its **decline set**, one shape per assert: the flags that would change a field's layout,
//!    the segment types it does not pack, a run-time or `all` width, a literal-area value, a
//!    total past one word, a zero-width field and an empty operand list.
//! 3. the **word-identity pin**: for every width `1..=64` and for a multi-field build, the term
//!    [`make_fresh_binary`](super::seam::make_fresh_binary) materialises from the masked
//!    `packed` word is bit-identical to what the family's own `bs_create_bin` walk builds from
//!    the same operands — so a native lowering cannot diverge from the family's writer.
//! 4. the materializer's caller contract: a width outside `1..=64` is a typed
//!    `unsupported`, and a term built *before* the fresh build still reads correctly
//!    afterwards.

use super::prepared::SEGMENT_OPERANDS;
use super::seam::{fresh_plan, make_fresh_binary, Field, FreshPlan};
use super::*;
use beam_common::BeamError;
use beam_loader::{Reg, TypeIndex, NO_FAIL};
use beam_term::make_small;

use crate::context::{ExecutionContext, MinimalContext};

/// The fixture's `StrT` table — the `string` segment's byte source.
const STRT: &[u8] = b"abcdefQ";

/// A module naming every segment type these cases use and holding the `STRT` fixture, with —
/// when `flags` is non-empty — that flag-atom list as its literal 0.
fn seg_module(flags: &[&str]) -> Module {
    let mut module = super::wide_tests::seg_module(flags);
    for name in [
        "binary",
        "string",
        "float",
        "utf8",
        "append",
        "private_append",
        "all",
        "nonsense",
    ] {
        module.atom_table.intern(name);
    }
    module.strings = STRT.to_vec();
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

/// The compiler's `z,5` spelling of the register `reg` (`{tr,{x,0},{t_integer,{0,255}}}`).
fn typed(reg: Reg) -> Operand {
    Operand::Typed {
        reg,
        ty: TypeIndex(0),
    }
}

/// One `[Type, Index, Unit, Flags, Value, Size]` segment group.
fn group(
    module: &Module,
    kind: &str,
    flags: Operand,
    value: Operand,
    size: Operand,
) -> Vec<Operand> {
    vec![
        atom(module, kind),
        Operand::Integer(1),
        Operand::Integer(1),
        flags,
        value,
        size,
    ]
}

/// An `integer` field of `bits` bits reading `value`, with no flags.
fn int(module: &Module, value: Operand, bits: i64) -> Vec<Operand> {
    group(module, "integer", Operand::Nil, value, Operand::Integer(bits))
}

/// An `integer` field with an explicit flags operand.
fn flagged_int(module: &Module, flags: Operand, value: Operand, bits: i64) -> Vec<Operand> {
    group(module, "integer", flags, value, Operand::Integer(bits))
}

/// The `(bytes, nbits)` a term's bits are, through the family's own byte view.
fn bits_of(term: Eterm) -> (Vec<u8>, usize) {
    built_bytes(Built::Term(term))
}

/// The field's own masked `packed` contribution: the low `bits` bits of a small integer, which
/// is exactly what the family's unsigned big-endian write stores.
fn masked(value: i64, bits: u32) -> u64 {
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    (value as u64) & mask
}

/// Run the **family's own** `bs_create_bin` walk over `segments` (the values are already in the
/// registers it reads) and answer the term it materialises.
fn family_term(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    segments: &[Operand],
) -> Eterm {
    let built = assemble(machine, module, &LabelMap::default(), NO_FAIL, segments, ctx)
        .expect("assemble")
        .expect("the build succeeds");
    built_term(built, machine, ctx).expect("materialises")
}

/// Assert `err` is the seam's typed `unsupported` raise naming `what`.
fn assert_unsupported(err: BeamError, what: &str) {
    match err {
        BeamError::Raise(raise) => {
            let reason = raise.reason.as_atom().expect("an atom reason");
            assert!(
                reason.starts_with("unsupported_opcode: "),
                "the typed unsupported reason, got {reason}"
            );
            assert!(reason.contains(what), "{reason} names {what}");
        }
        other => panic!("a typed `unsupported` raise, got {other}"),
    }
}

// ---------------------------------------------------------------------------
// 1. The accept set
// ---------------------------------------------------------------------------

/// The shape `emqx_base62:decode/2` executes once per character —
/// `{bs_create_bin,{f,0},0,1,8,{x,0},{list,[{atom,integer},1,1,nil,
/// {tr,{x,0},{t_integer,{0,255}}},{integer,8}]}}` — is a fresh plan of one 8-bit field.
#[test]
fn the_emqx_bare_build_shape_is_a_fresh_plan() {
    let module = seg_module(&[]);
    let value = typed(Reg::X(0));
    let segments = int(&module, value.clone(), 8);
    let plan: FreshPlan = fresh_plan(&module, &segments).expect("the emqx shape is a fresh plan");
    assert_eq!(plan.total_bits, 8, "exactly the built bitstring's own size");
    assert_eq!(plan.fields, vec![Field::Bits { value, bits: 8 }]);
}

/// Every static width from one bit to one machine word is one field of that width.
#[test]
fn every_static_width_up_to_one_word_is_one_field() {
    let module = seg_module(&[]);
    for bits in [1i64, 3, 7, 8, 12, 16, 33, 63, 64] {
        let segments = int(&module, Operand::X(0), bits);
        let plan = fresh_plan(&module, &segments).unwrap_or_else(|| panic!("{bits} bits plans"));
        assert_eq!(plan.total_bits, bits as u32, "{bits}: the summed width");
        assert_eq!(
            plan.fields,
            vec![Field::Bits {
                value: Operand::X(0),
                bits: bits as u32,
            }],
            "{bits}: one field of that width"
        );
    }
}

/// Several fields in one bare build are listed in instruction order and their widths sum — the
/// bare build has no accumulator group, so the **first** group is the first field.
#[test]
fn a_multi_field_build_lists_its_fields_in_order() {
    let module = seg_module(&[]);
    for widths in [
        &[4i64, 4][..],
        &[8, 8][..],
        &[1, 7, 8][..],
        &[12, 20][..],
    ] {
        let mut segments = Vec::new();
        for (index, bits) in widths.iter().enumerate() {
            segments.extend(int(&module, Operand::X(index as u32), *bits));
        }
        let plan = fresh_plan(&module, &segments)
            .unwrap_or_else(|| panic!("{widths:?} plans"));
        assert_eq!(
            plan.total_bits,
            widths.iter().sum::<i64>() as u32,
            "{widths:?}: the summed width"
        );
        let expected: Vec<Field> = widths
            .iter()
            .enumerate()
            .map(|(index, bits)| Field::Bits {
                value: Operand::X(index as u32),
                bits: *bits as u32,
            })
            .collect();
        assert_eq!(plan.fields, expected, "{widths:?}: the fields in order");
    }
}

/// An x- and a y-register value are both locatable, in the plain `Operand::X`/`Operand::Y`
/// spelling and in the compiler's `Operand::Typed` one — carried through verbatim, so a
/// lowering binds them the ordinary way.
#[test]
fn an_x_or_a_y_register_in_either_spelling_is_locatable() {
    let module = seg_module(&[]);
    for value in [
        Operand::X(0),
        Operand::Y(1),
        typed(Reg::X(2)),
        typed(Reg::Y(3)),
    ] {
        let segments = int(&module, value.clone(), 8);
        let plan = fresh_plan(&module, &segments).expect("a register value plans");
        assert_eq!(plan.fields, vec![Field::Bits { value, bits: 8 }]);
    }
}

// ---------------------------------------------------------------------------
// 2. The decline set
// ---------------------------------------------------------------------------

/// The three flags that change a field's layout — `little`, `signed` and `native` — are
/// declined on their own (writing such a field big-endian unsigned would be a **wrong** value),
/// at a direct-store width and at a bit-granular one; the defaults (`big`/`unsigned`) decode to
/// `0` and stay accepted.
#[test]
fn layout_changing_flags_are_declined() {
    for (names, expected) in [
        (&["little"][..], false),
        (&["signed"][..], false),
        (&["native"][..], false),
        (&["big", "unsigned"][..], true),
    ] {
        let module = seg_module(names);
        let flags = super::wide_tests::flag_operand(names);
        // 8/16 take the direct-store (`ByteInt`) form, 5 the general (`Integer`) one.
        for bits in [8i64, 16, 5] {
            let segments = flagged_int(&module, flags.clone(), Operand::X(0), bits);
            assert_eq!(
                fresh_plan(&module, &segments).is_some(),
                expected,
                "flags {names:?} at {bits} bits"
            );
        }
    }
}

/// A bare build packs `integer` fields only: every other segment type the family executes is
/// declined.
#[test]
fn a_non_integer_segment_is_declined() {
    let module = seg_module(&[]);
    let decliners = [
        // A whole-binary field needs a run-time copy, not a packed word.
        group(&module, "binary", Operand::Nil, Operand::X(0), Operand::Integer(8)),
        // A `string` constant is compile-time bytes, not a field.
        group(
            &module,
            "string",
            Operand::Nil,
            Operand::Integer(0),
            Operand::Integer(2),
        ),
        group(&module, "float", Operand::Nil, Operand::X(0), Operand::Integer(64)),
        group(&module, "utf8", Operand::Nil, Operand::X(0), Operand::Integer(8)),
        group(&module, "append", Operand::Nil, Operand::X(0), all(&module)),
    ];
    for segments in decliners {
        assert_eq!(
            fresh_plan(&module, &segments),
            None,
            "{:?}: not a packable field",
            segments[0]
        );
    }
}

/// A field width that is only known at run time (`<<V:N>>`) or is the source's own (`all`)
/// carries no static width, so it carries no static offset either.
#[test]
fn a_run_time_or_all_field_width_is_declined() {
    let module = seg_module(&[]);
    let dynamic = group(
        &module,
        "integer",
        Operand::Nil,
        Operand::X(0),
        typed(Reg::X(1)),
    );
    assert_eq!(fresh_plan(&module, &dynamic), None, "a run-time size");
    let all_width = group(&module, "integer", Operand::Nil, Operand::X(0), all(&module));
    assert_eq!(fresh_plan(&module, &all_width), None, "`all`");
}

/// A literal-area value operand cannot be bound by a lowering (it would need a literal-area
/// lookup), at a direct-store width and at a bit-granular one.
#[test]
fn a_literal_area_value_is_declined() {
    let module = seg_module(&[]);
    for bits in [8i64, 5] {
        let segments = int(&module, Operand::Literal(0), bits);
        assert_eq!(
            fresh_plan(&module, &segments),
            None,
            "{bits}: a literal-area value"
        );
    }
}

/// The packed word is one machine word, so a total past 64 bits is declined even when every
/// field on its own is acceptable.
#[test]
fn a_total_past_one_machine_word_is_declined() {
    let module = seg_module(&[]);
    let segments: Vec<Operand> = [
        int(&module, Operand::X(0), 40),
        int(&module, Operand::X(1), 40),
    ]
    .concat();
    assert_eq!(fresh_plan(&module, &segments), None, "80 bits");
    // The same two fields inside the budget do plan.
    let fits: Vec<Operand> = [
        int(&module, Operand::X(0), 32),
        int(&module, Operand::X(1), 32),
    ]
    .concat();
    assert_eq!(
        fresh_plan(&module, &fits).map(|plan| plan.total_bits),
        Some(64),
        "64 bits is the budget"
    );
}

/// A zero-width field contributes no bits and no offsets; a build of only such fields has
/// nothing to pack, so both are declined rather than planned as an empty result.
#[test]
fn a_zero_width_field_is_declined() {
    let module = seg_module(&[]);
    assert_eq!(
        fresh_plan(&module, &int(&module, Operand::X(0), 0)),
        None,
        "a single 0-bit field"
    );
    // 0 + 8 would sum to 8: the zero-width field itself is what refuses.
    let mixed: Vec<Operand> = [
        int(&module, Operand::X(0), 0),
        int(&module, Operand::X(1), 8),
    ]
    .concat();
    assert_eq!(fresh_plan(&module, &mixed), None, "a 0-bit field beside a real one");
}

/// An empty operand list is declined, and so is a truncated group (no fields at all, or a
/// trailing partial descriptor).
#[test]
fn an_empty_or_truncated_segment_list_is_declined() {
    let module = seg_module(&[]);
    assert_eq!(fresh_plan(&module, &[]), None, "no segments");
    let full = int(&module, Operand::X(0), 8);
    assert_eq!(
        fresh_plan(&module, &full[..SEGMENT_OPERANDS - 1]),
        None,
        "a truncated group"
    );
}

// ---------------------------------------------------------------------------
// 3. The word-identity pin
// ---------------------------------------------------------------------------

/// **For every field width `1..=64` and a spread of values, `make_fresh_binary`'s term is
/// bit-identical to the family's own bare build** — the same bytes, the same exact bit count,
/// and `beam_term`-equal. The `packed` word is the field's masked low bits, which is what the
/// family's unsigned big-endian write stores.
#[test]
fn a_fresh_build_is_bit_identical_to_the_family_at_every_width() {
    let module = seg_module(&[]);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    // `0x1FF` exceeds a narrow field; the negatives exercise the two's-complement mask.
    let values = [0i64, 1, 5, 0x1FF, -1, -2, 0x1234_5678_9ABC, -0x1234_5678_9ABC];
    for bits in 1..=64usize {
        for value in values {
            machine.set_x(0, make_small(value)).expect("x(0) in range");
            let segments = int(&module, Operand::X(0), bits as i64);
            let family = family_term(&mut machine, &mut ctx, &module, &segments);
            let (family_bytes, family_bits) = bits_of(family);
            assert_eq!(family_bits, bits, "{bits}/{value}: the family's bit count");

            let fresh = make_fresh_binary(&mut machine, &mut ctx, masked(value, bits as u32), bits)
                .expect("a fresh build");
            let (fresh_bytes, fresh_bits) = bits_of(fresh);
            assert_eq!(fresh_bits, bits, "{bits}/{value}: the fresh bit count");
            assert_eq!(fresh_bytes, family_bytes, "{bits}/{value}: the same bytes");
            assert!(
                beam_term::eq(fresh, family, &module.atom_table),
                "{bits}/{value}: `beam_term`-equal"
            );
        }
    }
}

/// The mask is the whole of the field's value rule: `<<-1:8>>` is `<<255>>` and `<<511:8>>` is
/// too, so `packed` is the small integer's low `nbits` bits — no compile-time range or type
/// check is needed, exactly as the module docs state. A field narrower than a byte shows the
/// other half of the rule: `<<-2:4>>` is `<<14:4>>`, whose single byte reads `0xE0` (the bits
/// sit at the **top** of the byte, the low four bits are padding).
#[test]
fn a_negative_or_overwide_value_is_masked_into_the_field() {
    let module = seg_module(&[]);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    for (value, bits, expected) in [(-1i64, 8usize, 0xFFu8), (0x1FF, 8, 0xFF), (-2, 4, 0xE0)] {
        machine.set_x(0, make_small(value)).expect("x(0) in range");
        let segments = int(&module, Operand::X(0), bits as i64);
        let family = family_term(&mut machine, &mut ctx, &module, &segments);
        let fresh = make_fresh_binary(&mut machine, &mut ctx, masked(value, bits as u32), bits)
            .expect("a fresh build");
        assert_eq!(
            bits_of(fresh),
            (vec![expected], bits),
            "{value} at {bits} bits"
        );
        assert_eq!(bits_of(family), bits_of(fresh), "{value}/{bits}: the family agrees");
    }
}

/// A multi-field build packs as first-field-most-significant, and the resulting term is again
/// identical to the family's — including a bit-granular mix whose total is not byte-aligned.
#[test]
fn a_multi_field_fresh_build_is_bit_identical_to_the_family() {
    let module = seg_module(&[]);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    // Two whole-byte fields.
    for (a, b) in [(0x12i64, 0x34i64), (0xFF, 0x00), (-1, 0x7F)] {
        machine.set_x(0, make_small(a)).expect("x(0) in range");
        machine.set_x(1, make_small(b)).expect("x(1) in range");
        let segments: Vec<Operand> = [
            int(&module, Operand::X(0), 8),
            int(&module, Operand::X(1), 8),
        ]
        .concat();
        let family = family_term(&mut machine, &mut ctx, &module, &segments);
        let packed = (masked(a, 8) << 8) | masked(b, 8);
        let fresh = make_fresh_binary(&mut machine, &mut ctx, packed, 16).expect("a fresh build");
        assert_eq!(bits_of(fresh), bits_of(family), "{a}/{b}: the same bytes");
        assert!(
            beam_term::eq(fresh, family, &module.atom_table),
            "{a}/{b}: `beam_term`-equal"
        );
    }
    // `<<A:1, B:7, C:8>>` — 16 bits across three fields, the first two bit-granular.
    machine.set_x(0, make_small(1)).expect("x(0) in range");
    machine.set_x(1, make_small(5)).expect("x(1) in range");
    machine.set_x(2, make_small(0x3F)).expect("x(2) in range");
    let segments: Vec<Operand> = [
        int(&module, Operand::X(0), 1),
        int(&module, Operand::X(1), 7),
        int(&module, Operand::X(2), 8),
    ]
    .concat();
    let plan = fresh_plan(&module, &segments).expect("a 16-bit three-field plan");
    assert_eq!(plan.total_bits, 16);
    let family = family_term(&mut machine, &mut ctx, &module, &segments);
    let packed = (masked(1, 1) << 15) | (masked(5, 7) << 8) | masked(0x3F, 8);
    let fresh = make_fresh_binary(&mut machine, &mut ctx, packed, plan.total_bits as usize)
        .expect("a fresh build");
    assert_eq!(bits_of(fresh), (vec![0x85, 0x3F], 16), "1, 5, 0x3F packed");
    assert_eq!(bits_of(fresh), bits_of(family), "the family agrees");
    assert!(beam_term::eq(fresh, family, &module.atom_table));
}

// ---------------------------------------------------------------------------
// 4. The materializer's caller contract
// ---------------------------------------------------------------------------

/// A width outside `1..=64` is the caller's own bug — never a reachable runtime case — and is
/// answered with the typed `unsupported` raise rather than an approximation.
#[test]
fn a_width_outside_one_to_sixty_four_bits_is_a_typed_error() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    for nbits in [0usize, 65, 128] {
        let err = make_fresh_binary(&mut machine, &mut ctx, 0, nbits)
            .expect_err("a width outside the band is refused");
        assert_unsupported(err, "fresh build width");
    }
}

/// The materializer allocates through the **rooted** reserve
/// ([`raw::make_bits`](super::super::raw) → `RootSet::ALL_LIVE`), so a term that was live
/// before the build is still readable afterwards and the returned bits are the packed ones.
///
/// Limitation, stated rather than implied: `MinimalContext`'s arena grows **in place** and its
/// `ensure_heap_full` is the defaulted no-relocation door, so this case pins the rooted door's
/// shape (the reserve happens before any operand is read, and every earlier term survives it)
/// but not a *moving* collection — this node's seam suites have no relocating host.
#[test]
fn a_fresh_build_leaves_an_earlier_term_readable() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let live = raw::make_binary(&mut machine, &mut ctx, b"abcdef", 48).expect("a live binary");
    machine.set_x(1, live).expect("x(1) in range");
    let (before_bytes, before_bits) = bits_of(live);
    assert_eq!((before_bytes.clone(), before_bits), (b"abcdef".to_vec(), 48));

    let fresh = make_fresh_binary(&mut machine, &mut ctx, 0xAB, 8).expect("a fresh build");
    assert_eq!(bits_of(fresh), (vec![0xAB], 8), "the packed bits");

    let after = machine.x(1).expect("x(1) in range");
    assert_eq!(
        bits_of(after),
        (before_bytes, before_bits),
        "the earlier term still reads its own bytes"
    );
}
