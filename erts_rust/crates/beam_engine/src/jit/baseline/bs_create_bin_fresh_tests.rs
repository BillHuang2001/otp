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
//! Value pins for the **bare** `bs_create_bin` lowering ([`bitstr_inline::Route::Fresh`],
//! emitted by `create_fresh`): the packing word it builds is correct at **every** width and
//! for every field combination the route accepts, and its one refusal commits nothing.
//!
//! The sibling [`super`] file owns the *shape and mutation-order* arguments of the append
//! route and holds the bare route's smoke row (one field, width 8). This file is the value
//! half of that route, one concern per test:
//!
//! * [`every_field_width_from_one_to_sixty_four_packs_its_value_bitexactly`] — the whole
//!   `1..=64` width range, not a sample: byte-aligned and partial last bytes alike, over the
//!   values that make a wrong mask or a wrong shift visible (the small-integer extremes, the
//!   width's own extremes, and a value whose set bits lie *above* the width).
//! * [`multi_field_builds_pack_bitexactly_when_the_total_is_not_byte_aligned`] — the
//!   shifted-field packing (`4+4`, `1+7+8`, `12+20`, `5+11+8`, `3+3+3`, and 64-bit totals).
//! * [`a_negative_field_masks_to_its_own_width_beside_a_shifted_sibling`] — a two's-complement
//!   field masked to *its* width, beside a field shifted into the same byte.
//! * [`a_y_slot_destination_is_the_family_value_on_both_tiers`] — the frame-resident
//!   destination, whose address is re-derived after the helper behind the up-front guard.
//! * [`a_non_small_field_value_takes_the_fallback_and_writes_nothing`] — the `is_small`
//!   guard's refusal: the guard really fired at run time, and the fallback's re-execution
//!   still reads the destination register's pre-execution term (it is also the first field's
//!   value), so any commit leaked before the guard would change the outcome.
//! * [`a_non_integer_field_value_is_the_family_badarg_on_both_tiers`] — the same refusal for
//!   a value no masking could reproduce (an atom, a float, a binary): the family's exact
//!   `error:badarg`.
//! * [`the_expected_bytes_match_the_live_oracle`] — the expectation computer itself, checked
//!   against the system `erl` on the rows where the packing argument is subtlest (it skips
//!   loudly when no `erl` is on `PATH`; nothing else in this file depends on it).
//!
//! # Oracle discipline
//!
//! Two independent expectations per row, never one:
//!
//! 1. **The family's own result.** The interpreter tier runs the ordinary
//!    `bs_create_bin` family body, so its outcome *is* the reference lowering, and the
//!    baseline tier must reproduce it exactly (`assert_same_outcome`, and the bytes on both
//!    sides).
//! 2. **An expectation computed here**, from the input value alone — the existing
//!    [`concatenated_bytes`] (a plain shift accumulation over the field values, a two's
//!    complement truncation per field), so a mistake both tiers shared would still fail.
//!
//! The *route* claim rides the compile-time instrument, never a run-time helper count: a
//! cache-free compile must move the raw [`bitstr_inline::inlined`] counter for
//! [`SHAPE_CREATE_FRESH`] (and must not move [`SHAPE_CREATE_BIN`]), the census must visit the
//! `BsCreateBin` variant and record **zero** static deopts. On the run-time side the counted
//! baseline run must show **zero** `jit_bitstr` calls (no accumulator, so no append door) and
//! **zero** `jit_step_one` steps (the arm stayed in block), except in the rows that exist to
//! pin the refusal.

use std::process::Command;

use super::*;
use beam_term::{MAX_SMALL, MIN_SMALL};

// ---------------------------------------------------------------------------
// Fixtures: a **bare** build (no `private_append` group anywhere in the segments)
// ---------------------------------------------------------------------------
/// A module whose single `bs_create_bin` is a **bare** build: `fields` supplies every segment
/// group (there is no accumulator group to prepend), so
/// [`bitstr_inline::CreateBinShape::of`] reports [`bitstr_inline::Route::Fresh`] for it.
fn fresh_module(
    name: &str,
    arity: u32,
    dst: Operand,
    fields: impl FnOnce(&Module) -> Vec<Vec<Operand>>,
) -> Module {
    bin_module(name, arity, &[], move |module| {
        let mut segments = Vec::new();
        for group in fields(module) {
            segments.extend(group);
        }
        vec![create_bin(dst, segments, arity)]
    })
}

/// `f(A0, A1, …)` — one statically sized unsigned big-endian `integer` field per entry of
/// `widths`, its value in `x(index)`, and the built binary written back into `x(0)` (the
/// compiler's own shape when the built binary replaces its only source).
fn multi_field_module(name: &str, widths: &[u32]) -> Module {
    let arity = widths.len() as u32;
    fresh_module(name, arity, Operand::X(0), move |module| {
        widths
            .iter()
            .enumerate()
            .map(|(index, bits)| int_seg(module, Operand::X(index as u32), i64::from(*bits)))
            .collect()
    })
}

/// [`multi_field_module`] with exactly one field of `bits` bits.
fn single_field_module(name: &str, bits: u32) -> Module {
    multi_field_module(name, &[bits])
}

/// `f(Value)` — the same one-field build, but written into the current frame's **`Y(0)`**
/// slot, which is pre-loaded with the argument so the row also pins that the destination is
/// whatever the build produced and never a leftover. The built term is copied back into
/// `x(0)` before the frame is dropped so the function returns it.
fn y_dst_module(name: &str, bits: u32) -> Module {
    bin_module(name, 1, &[], move |module| {
        vec![
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 1,
            },
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::Y(0),
            },
            create_bin(
                Operand::Y(0),
                int_seg(module, Operand::X(0), i64::from(bits)),
                1,
            ),
            Instr::Move {
                src: Operand::Y(0),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
        ]
    })
}

// ---------------------------------------------------------------------------
// The instruments
// ---------------------------------------------------------------------------
/// The module's single `bs_create_bin` must be the **bare** route: `CreateBinShape::of` reports
/// `Route::Fresh` for its own operands.
fn assert_fresh_route(module: &Module) {
    let (dst, segments) = create_bin_operands(module);
    let shape = bitstr_inline::CreateBinShape::of(module, &dst, &segments)
        .expect("a bare build's fields are an inlinable shape");
    assert_eq!(
        shape.route(),
        bitstr_inline::Route::Fresh,
        "a build with no `private_append` group is the bare route"
    );
}

/// **The non-vacuity claim for the bare arm**, read off the raw counters: a cache-free compile
/// moves [`SHAPE_CREATE_FRESH`] (and not [`SHAPE_CREATE_BIN`]), visits the `BsCreateBin`
/// variant, and records no static deopt.
///
/// The `before`/`after` pair is the *actual* thread-local counter, not a computed difference:
/// a shape the arm declined would leave the counter where it was.
fn assert_fresh_emitted(module: &Module, label: &str) {
    let before = bitstr_inline::inlined(SHAPE_CREATE_FRESH);
    let (emitted, snapshot) = emitted_shapes(module);
    let after = bitstr_inline::inlined(SHAPE_CREATE_FRESH);
    assert!(
        after > before,
        "{label}: a cache-free compile must emit the bare arm (SHAPE_CREATE_FRESH \
         {before} -> {after})"
    );
    assert!(
        emitted.contains(&SHAPE_CREATE_FRESH),
        "{label}: the bare arm must be one of the emitted shapes (emitted {emitted:?})"
    );
    assert!(
        !emitted.contains(&SHAPE_CREATE_BIN),
        "{label}: no accumulator is involved, so the append shape must not emit (emitted \
         {emitted:?})"
    );
    assert!(
        visited_instances(&snapshot, "BsCreateBin") >= 1,
        "{label}: `BsCreateBin` must be the census's variant name (visited {:?}); a wrong name \
         would make the deopt assertion vacuous",
        snapshot.instr_instances
    );
    assert_eq!(
        static_deopts(&snapshot, "BsCreateBin"),
        0,
        "{label}: the bare shape must lower, never statically deopt (deopts {:?})",
        snapshot.instr_deopts
    );
}

/// The values a `bits`-wide field is fed: `0`, `1`, `-1`, both extremes of the 60-bit **small
/// integer** range (which is the range the inlined route accepts — anything wider takes the
/// guard's fallback), the width's own maximum and minimum where those are small integers, and
/// a value whose set bits lie **above** the width (which the family truncates).
///
/// The above-width value only exists for widths that leave room above them inside the small
/// range (`bits <= 58`): at `bits >= 59` every small integer already fits the field, so there
/// is no small value to truncate — the truncation of a *wider* value is covered by the
/// bignum-fallback row instead. Duplicates are dropped, so a width that coincides with an
/// extreme contributes it once.
fn sweep_values(bits: u32) -> Vec<i64> {
    let width = i64::from(bits);
    let mut values = vec![0i64, 1, -1, MAX_SMALL, MIN_SMALL];
    if width < 60 {
        // The width's own maximum (`2^width - 1`), and a value with bits above the width
        // (`2^width | 0b101`) when that value is still a small integer.
        values.push((1i64 << width) - 1);
        let oversized = (1i64 << width) | 0b101;
        if oversized <= MAX_SMALL {
            values.push(oversized);
        }
    }
    if width <= 60 {
        // The width's own minimum (`-2^(width-1)`).
        values.push(-(1i64 << (width - 1)));
    }
    let mut unique: Vec<i64> = Vec::with_capacity(values.len());
    for value in values {
        assert!(
            (MIN_SMALL..=MAX_SMALL).contains(&value),
            "{value} is not a small integer"
        );
        if !unique.contains(&value) {
            unique.push(value);
        }
    }
    unique
}

/// **The value row**: run `f/<args.len()>` on both tiers, assert they agree, and assert that
/// **each** side's built bitstring has `fields`' total width and exactly the payload bytes an
/// independent shift accumulation over the values produces.
///
/// The bytes are compared against [`concatenated_bytes`] *and* the interpreter's own (the
/// family's) — the tier agreement alone would also pass for two tiers sharing one bug.
fn assert_fresh_row(label: &str, module: &Module, args_spec: &[ArgKind], fields: &[(i64, usize)]) {
    let width: usize = fields.iter().map(|(_, bits)| *bits).sum();
    let expected = concatenated_bytes(fields);
    let name = atom(module, "f");
    with_both_tiers_args(module, name, args_spec, |interpreter, baseline| {
        assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
            let value = returned(label, tier, outcome);
            assert_eq!(
                bit_size(value),
                Some(width),
                "{tier}: {label}: the built bitstring's own width"
            );
            assert_eq!(
                returned_storage_bytes(label, tier, value),
                expected,
                "{tier}: {label}: the payload bytes, bit-exact and with the partial last \
                 byte's padding left zero"
            );
        }
    });
}

/// **The in-block claim for one row**: a counted baseline run whose result is the same bytes,
/// with **zero** family-helper calls (the bare route consults no append door) and **zero**
/// `jit_step_one` steps (the whole instruction ran natively).
fn assert_fresh_counted(label: &str, module: &Module, args_spec: &[ArgKind], fields: &[(i64, usize)]) {
    let width: usize = fields.iter().map(|(_, bits)| *bits).sum();
    let expected = concatenated_bytes(fields);
    let name = atom(module, "f");
    let report = run_counting(module, name, args_spec, Tier::BaselineJit);
    assert_eq!(
        report.bits,
        Some(width),
        "{label}: the compiled run's published width"
    );
    assert_eq!(
        report.append_door,
        (0, 0),
        "{label}: a bare build opens no append door"
    );
    assert_eq!(
        report.bitstr_calls, 0,
        "{label}: the bare route never reaches `jit_bitstr`, so the family helper must not run"
    );
    assert_eq!(
        report.counters.step_one, 0,
        "{label}: the arm must stay native (jit_step_one ran {} times)",
        report.counters.step_one
    );
    if width % 8 == 0 {
        assert_eq!(
            report.bytes.as_deref(),
            Some(&expected[..]),
            "{label}: the compiled run's bytes"
        );
    } else {
        assert!(
            report.bytes.is_none(),
            "{label}: a partial last byte is not a byte-aligned binary"
        );
    }
}

// ---------------------------------------------------------------------------
// 1. The width sweep — 1..=64, single field
// ---------------------------------------------------------------------------
/// **Every field width from 1 to 64 bits packs its value bit-exactly.**
///
/// The packing sequence's whole claim is `pack = Σ (value_i & mask(bits_i)) << (bits after i)`
/// followed by a left-aligned byte split of `total_bits` — so the widths where a wrong shift
/// count, a wrong mask or a wrong rounding of the byte count shows up are *all* of them, not a
/// representative sample. Each width runs the full [`sweep_values`] set, and every row carries
/// three separate claims: the raw counter says the arm emitted for that width, the two tiers
/// agree with an expectation computed from the value, and the counted run stayed in block.
#[test]
fn every_field_width_from_one_to_sixty_four_packs_its_value_bitexactly() {
    for bits in 1..=64u32 {
        let module = single_field_module(&format!("bscb_fresh_w{bits}"), bits);
        let label = format!("bare integer {bits}");
        assert_fresh_route(&module);
        assert_fresh_emitted(&module, &label);
        for value in sweep_values(bits) {
            let row = format!("{label} of {value}");
            let args = [ArgKind::Int(value)];
            assert_fresh_row(&row, &module, &args, &[(value, bits as usize)]);
            assert_fresh_counted(&row, &module, &args, &[(value, bits as usize)]);
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Multi-field packing
// ---------------------------------------------------------------------------
/// **Several fields packed into one word, including totals that are not a whole byte.**
///
/// The values are chosen so that a field written at the wrong offset — or with the wrong
/// shift — changes the bytes: no field is a repeated byte, and the two fields of a pair differ
/// in more than one bit. The 64-bit totals exercise the boundary at which the packing word is
/// completely filled (the shift that would leave the CPU's 6-bit shift count wrapping is the
/// first field's identity move).
#[test]
fn multi_field_builds_pack_bitexactly_when_the_total_is_not_byte_aligned() {
    let cases: Vec<(&str, Vec<u32>, Vec<Vec<i64>>)> = vec![
        (
            "4 + 4",
            vec![4, 4],
            vec![vec![0xA, 0x5], vec![0x1, 0xE], vec![0xF, 0x0]],
        ),
        (
            "1 + 7 + 8",
            vec![1, 7, 8],
            vec![
                vec![1, 0x2A, 0x35],
                vec![0, 0x7F, 0xFF],
                vec![1, 0x55, 0xAA],
            ],
        ),
        (
            "12 + 20",
            vec![12, 20],
            vec![
                vec![0xABC, 0x12345],
                vec![0xFFF, 0xFFFFF],
                vec![0x123, 0x45678],
            ],
        ),
        (
            "5 + 11 + 8",
            vec![5, 11, 8],
            vec![
                vec![0x1F, 0x7FF, 0xA5],
                vec![0b1_0101, 0b001_0101_1010, 0x5A],
            ],
        ),
        (
            "3 + 3 + 3",
            vec![3, 3, 3],
            vec![vec![0b101, 0b010, 0b111], vec![0b001, 0b100, 0b011]],
        ),
        // 64-bit totals: the packing word exactly full, byte-aligned and not.
        (
            "32 + 32",
            vec![32, 32],
            vec![
                vec![0xDEAD_BEEF, 0x1234_5678],
                vec![-1, 0x0000_0001],
            ],
        ),
        (
            "60 + 4",
            vec![60, 4],
            vec![vec![(1i64 << 59) - 1, 0xF], vec![0x0123_4567_89AB_CDE, 0x3]],
        ),
        (
            "1 + 63",
            vec![1, 63],
            vec![vec![1, (1i64 << 59) - 1], vec![0, -1]],
        ),
    ];
    for (case, widths, value_rows) in &cases {
        let module = multi_field_module(&format!("bscb_fresh_m{}", widths.len()), widths);
        assert_fresh_route(&module);
        assert_fresh_emitted(&module, case);
        for values in value_rows {
            assert_eq!(
                values.len(),
                widths.len(),
                "{case}: one value per field"
            );
            let fields: Vec<(i64, usize)> = values
                .iter()
                .copied()
                .zip(widths.iter().map(|bits| *bits as usize))
                .collect();
            let label = format!("{case} of {values:?}");
            let args: Vec<ArgKind> = values.iter().map(|value| ArgKind::Int(*value)).collect();
            assert_fresh_row(&label, &module, &args, &fields);
            assert_fresh_counted(&label, &module, &args, &fields);
        }
    }
}

// ---------------------------------------------------------------------------
// 3. A negative field beside a shifted sibling
// ---------------------------------------------------------------------------
/// **A two's-complement field masked to its *own* width, beside a field in the same byte.**
///
/// The mask argument the packing rests on is `value & mask(bits)`: for a negative value the
/// low `bits` of its two's complement. A field that kept its sign extension instead would set
/// the bits the *other* field occupies, so these rows pair a negative field with a positive
/// one whose value is not all-ones, at totals both byte-aligned and not, with the negative
/// field first and last. `-2` (rather than `-1`, whose low bits are all ones at every width)
/// is the value that makes a missing mask change the bytes.
#[test]
fn a_negative_field_masks_to_its_own_width_beside_a_shifted_sibling() {
    let cases: Vec<(&str, Vec<u32>, Vec<i64>)> = vec![
        ("12-bit -1 then 4-bit 5", vec![12, 4], vec![-1, 5]),
        ("12-bit -2 then 3-bit 5 (15 bits)", vec![12, 3], vec![-2, 5]),
        (
            "12-bit -0x800 then 5-bit 5 (17 bits)",
            vec![12, 5],
            vec![-(1i64 << 11), 5],
        ),
        ("4-bit 5 then 12-bit -0x123", vec![4, 12], vec![5, -0x123]),
        ("7-bit -0x40 then 9-bit -2", vec![7, 9], vec![-(1i64 << 6), -2]),
        ("63-bit -1 then 1-bit 1", vec![63, 1], vec![-1, 1]),
        (
            "1-bit 1 then 63-bit -0x1234567",
            vec![1, 63],
            vec![1, -0x123_4567],
        ),
    ];
    for (index, (case, widths, values)) in cases.into_iter().enumerate() {
        let module = multi_field_module(&format!("bscb_fresh_neg{index}"), &widths);
        assert_fresh_route(&module);
        assert_fresh_emitted(&module, case);
        let fields: Vec<(i64, usize)> = values
            .iter()
            .copied()
            .zip(widths.iter().map(|bits| *bits as usize))
            .collect();
        let args: Vec<ArgKind> = values.iter().map(|value| ArgKind::Int(*value)).collect();
        assert_fresh_row(case, &module, &args, &fields);
        assert_fresh_counted(case, &module, &args, &fields);
    }
}

// ---------------------------------------------------------------------------
// 4. A `Y`-slot destination
// ---------------------------------------------------------------------------
/// **The same build written to a frame slot is the family's value on both tiers.**
///
/// A `Y` destination takes the emitted sequence's other arm: the slot's address is re-derived
/// *after* the helper call (which allocates and may relocate the heap, but pushes no frame, so
/// the published window still holds the slot) behind the one up-front frame-window guard. The
/// fixture loads the slot with the argument first, so a run that stored nothing — or stored a
/// stale term — would return the argument rather than the built bitstring.
#[test]
fn a_y_slot_destination_is_the_family_value_on_both_tiers() {
    for bits in [8u32, 12, 24, 64] {
        let module = y_dst_module(&format!("bscb_fresh_y{bits}"), bits);
        let (dst, _segments) = create_bin_operands(&module);
        assert!(
            matches!(dst, Operand::Y(0)),
            "the fixture's destination must be a frame slot, not {dst:?}"
        );
        let label = format!("bare integer {bits} into a Y slot");
        assert_fresh_route(&module);
        assert_fresh_emitted(&module, &label);
        for value in sweep_values(bits) {
            let row = format!("{label} of {value}");
            let args = [ArgKind::Int(value)];
            assert_fresh_row(&row, &module, &args, &[(value, bits as usize)]);
            assert_fresh_counted(&row, &module, &args, &[(value, bits as usize)]);
        }
    }
}

// ---------------------------------------------------------------------------
// 5. The guard's refusal writes nothing
// ---------------------------------------------------------------------------
/// **A non-small field value takes the `is_small` guard's fallback, and the fallback sees an
/// untouched machine.**
///
/// The value is a **bignum** (`(1 << 60) | 0x5A`, one bit above the 60-bit small range), so
/// the family's masking (which truncates a wide integer to the field's low bits) is what the
/// fallback's re-execution produces — not a raise, which makes this the row where "the guard
/// committed nothing" is observable in the *value*: the destination is the first field's own
/// register, so a pack or store that leaked a commit before the guard would be re-read by the
/// re-execution as a bitstring where an integer belongs, answering `badarg` instead.
///
/// What each instrument pins, read off the code rather than assumed:
///
/// * [`SHAPE_CREATE_FRESH`] moves on the **compile**, once per emitted instruction, before the
///   first byte — a run-time fallback does not un-emit it, so the emission count is not the
///   fallback's evidence and is not asserted as such;
/// * `jit_step_one` moves on the **run**: that is the guard firing (the arm declined at run
///   time) and the family writing the term;
/// * `jit_bitstr` stays at zero (the fallback is one interpreter step of the family body, not
///   the tier's family helper) and no append door runs.
///
/// A second fixture pins the same refusal at a **wide** field (56 bits of a 64-bit total),
/// where the family's mask keeps only the value's low bits.
#[test]
fn a_non_small_field_value_takes_the_fallback_and_writes_nothing() {
    let narrow = multi_field_module("bscb_fresh_guard8", &[8, 8]);
    assert_fresh_route(&narrow);
    assert_fresh_emitted(&narrow, "the two-field guard fixture");
    let name = atom(&narrow, "f");
    for second in [(1i64 << 60) | 0x5A, -((1i64 << 60) + 0x33)] {
        let label = format!("bare integer 8, integer 8 of 0x41 and the bignum {second}");
        let args = [ArgKind::Int(0x41), ArgKind::Int(second)];
        let fields = [(0x41i64, 8usize), (second, 8)];
        assert_fresh_row(&label, &narrow, &args, &fields);
        let report = run_counting(&narrow, name, &args, Tier::BaselineJit);
        assert_eq!(report.bits, Some(16), "{label}: the family's own width");
        assert_eq!(
            report.bytes.as_deref(),
            Some(&concatenated_bytes(&fields)[..]),
            "{label}: the fallback's re-execution builds the family's bytes — the destination \
             register still held the first field's value when the guard fired"
        );
        assert!(
            report.counters.step_one >= 1,
            "{label}: the second field's tag guard must take the fallback (jit_step_one ran {} \
             times)",
            report.counters.step_one
        );
        assert_eq!(
            report.bitstr_calls, 0,
            "{label}: the fallback is one interpreter step, so the tier's family helper must \
             not run"
        );
        assert_eq!(
            report.append_door,
            (0, 0),
            "{label}: a bare build opens no append door, fallback or not"
        );
    }

    let wide = multi_field_module("bscb_fresh_guard56", &[8, 56]);
    assert_fresh_route(&wide);
    assert_fresh_emitted(&wide, "the wide guard fixture");
    let name = atom(&wide, "f");
    let second = (1i64 << 60) | 0x5A;
    let label = "bare integer 8, integer 56 of the bignum";
    let args = [ArgKind::Int(0x41), ArgKind::Int(second)];
    let fields = [(0x41i64, 8usize), (second, 56)];
    assert_fresh_row(label, &wide, &args, &fields);
    let report = run_counting(&wide, name, &args, Tier::BaselineJit);
    assert_eq!(report.bits, Some(64), "{label}: the family's own width");
    assert_eq!(
        report.bytes.as_deref(),
        Some(&concatenated_bytes(&fields)[..]),
        "{label}: the family masks the bignum to the field's low 56 bits"
    );
    assert!(
        report.counters.step_one >= 1,
        "{label}: the tag guard must take the fallback (jit_step_one ran {} times)",
        report.counters.step_one
    );
    assert_eq!(report.bitstr_calls, 0, "{label}: no family helper call");
}

// ---------------------------------------------------------------------------
// 6. The non-integer field value
// ---------------------------------------------------------------------------
/// The class and the reason of one side's exception, in both of the spellings the engine uses
/// for the same BEAM exception: a reason-carrying [`beam_common::Raise`] (class + reason term)
/// and a **fieldless** reason variant such as [`beam_common::BeamError::BadArg`], which the
/// engine converts into an `error:<atom>` raise and whose atom
/// [`beam_common::BeamError::as_atom`] names. The sibling suites compare raised reasons only;
/// these rows exist to pin the class as well.
fn class_and_reason(error: &beam_common::BeamError) -> (String, String) {
    match error {
        beam_common::BeamError::Raise(raise) => (
            raise.class.as_str().to_string(),
            raise
                .reason
                .as_atom()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{:?}", raise.reason)),
        ),
        fieldless => (
            "error".to_string(),
            fieldless
                .as_atom()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{fieldless:?}")),
        ),
    }
}

/// **An atom, a float and a binary are the family's exact `error:badarg`.**
///
/// No masking can reproduce a value that is not an integer, so the sequence's one tag test
/// (`is_small`) sends all three to the fallback, and the family raises. The row pins the
/// *whole* exception the oracle reports — class `error` **and** the bare reason atom
/// `badarg` — on both tiers, plus the two run-time instruments (the guard fired; the tier's
/// family helper did not run).
#[test]
fn a_non_integer_field_value_is_the_family_badarg_on_both_tiers() {
    let module = single_field_module("bscb_fresh_nonint", 8);
    assert_fresh_route(&module);
    assert_fresh_emitted(&module, "the non-integer field fixture");
    let name = atom(&module, "f");
    let cases = [
        ("an atom", ArgKind::Atom(atom(&module, "f"))),
        ("a float", ArgKind::Float(1.5)),
        ("a binary", ArgKind::Bytes(&b"xy"[..])),
    ];
    for (case, arg) in cases {
        let label = format!("bare integer 8 of {case}");
        with_both_tiers_args(
            &module,
            name,
            std::slice::from_ref(&arg),
            |interpreter, baseline| {
                for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
                    match outcome {
                        Err(error) => assert_eq!(
                            class_and_reason(error),
                            ("error".to_string(), "badarg".to_string()),
                            "{tier}: {label}: the family's own exception ({error:?})"
                        ),
                        Ok(outcome) => {
                            panic!("{tier}: {label}: expected a raise, got {outcome:?}")
                        }
                    }
                }
                assert_same_outcome(&label, interpreter, baseline, &module.atom_table);
            },
        );
        let report = run_counting(&module, name, &[arg], Tier::BaselineJit);
        assert_eq!(report.bits, None, "{label}: no bitstring is built");
        assert!(
            report.counters.step_one >= 1,
            "{label}: the tag guard must take the fallback (jit_step_one ran {} times)",
            report.counters.step_one
        );
        assert_eq!(
            report.bitstr_calls, 0,
            "{label}: the family raises from its own body, not through the tier helper"
        );
    }
}

// ---------------------------------------------------------------------------
// 7. The expectation computer, checked against the live oracle
// ---------------------------------------------------------------------------
/// Run `expr` (an Erlang expression yielding a bitstring) under the system `erl` and return
/// its `(bit_size, payload bytes)` — the bitstring zero-padded to a whole byte, so the printed
/// form is a plain `<<B1,B2,…>>` list and no bit-syntax term has to be parsed. `None` when no
/// `erl` is on `PATH`, which the one caller reports loudly.
fn oracle_bits(expr: &str) -> Option<(usize, Vec<u8>)> {
    // `B` is a *bitstring*, so the byte-padding copy must splice it as one
    // (`/binary` would demand an already byte-aligned subject); `halt()` is what ends the
    // `-eval` process, and the crash-dump override keeps a failing oracle from writing
    // `erl_crash.dump` into the repository.
    let eval = format!(
        "B = {expr}, N = bit_size(B), Pad = (8 - N rem 8) rem 8, \
         io:format(\"~w ~w~n\", [N, <<B/bitstring, 0:Pad>>]), halt()."
    );
    let output = Command::new("erl")
        .args(["-noshell", "-env", "ERL_CRASH_DUMP", "/dev/null", "-eval", &eval])
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for `{eval}`\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.trim();
    let (bits, list) = line
        .split_once(' ')
        .unwrap_or_else(|| panic!("unexpected oracle output for `{eval}`: {line:?}"));
    let bits: usize = bits
        .parse()
        .unwrap_or_else(|_| panic!("unexpected oracle bit size for `{eval}`: {bits:?}"));
    let inner = list
        .strip_prefix("<<")
        .and_then(|rest| rest.strip_suffix(">>"))
        .unwrap_or_else(|| panic!("unexpected oracle bytes for `{eval}`: {list:?}"));
    let bytes = if inner.is_empty() {
        Vec::new()
    } else {
        inner
            .split(',')
            .map(|byte| {
                byte.trim()
                    .parse::<u8>()
                    .unwrap_or_else(|_| panic!("unexpected oracle byte {byte:?} for `{eval}`"))
            })
            .collect()
    };
    Some((bits, bytes))
}

/// **The expectation this file compares against is Erlang's own semantics, not this file's
/// assumption** — [`concatenated_bytes`] (and its total width) checked against the live
/// oracle, on the rows where the packing argument is subtlest.
///
/// The oracle is asked for the built bitstring's **bit size** and its payload bytes, both
/// directly (`bit_size/1` and a byte-padded copy of the term), so the golden covers the two
/// things a hand-written expectation could get wrong on its own: that `<<A:w1, B:w2>>` *is*
/// the fields' bit concatenation MSB-first, and that a negative or over-wide field is the
/// truncation of its two's complement. A missing `erl` skips this row loudly — every other
/// row in this file is independent of it (its oracle is the family, which runs in-process).
#[test]
fn the_expected_bytes_match_the_live_oracle() {
    let rows: Vec<(&str, Vec<(i64, usize)>)> = vec![
        // The smallest and the widest fields the route accepts.
        ("<<1:1>>", vec![(1, 1)]),
        ("<<(-1):64>>", vec![(-1, 64)]),
        // Totals that are not a whole byte, with the negative field first and last.
        ("<<(16#ABC):12, (16#12345):20>>", vec![(0xABC, 12), (0x12345, 20)]),
        ("<<(-1):12, 5:4>>", vec![(-1, 12), (5, 4)]),
        ("<<1:1, 42:7, 53:8>>", vec![(1, 1), (42, 7), (53, 8)]),
        ("<<(-2):3, 21:5>>", vec![(-2, 3), (21, 5)]),
        // A 64-bit total, and a field masked from a value wider than itself.
        ("<<(-1):60, 5:4>>", vec![(-1, 60), (5, 4)]),
        (
            "<<((1 bsl 60) bor 16#5A):8, 3:3>>",
            vec![((1i64 << 60) | 0x5A, 8), (3, 3)],
        ),
    ];
    for (expr, fields) in rows {
        let Some((bits, bytes)) = oracle_bits(expr) else {
            eprintln!("skipping `the_expected_bytes_match_the_live_oracle`: no `erl` on PATH");
            return;
        };
        let width: usize = fields.iter().map(|(_, bits)| *bits).sum();
        assert_eq!(bits, width, "{expr}: the oracle's bit size against the fields' sum");
        assert_eq!(
            bytes,
            concatenated_bytes(&fields),
            "{expr}: the oracle's bytes against this file's expectation"
        );
    }
}
