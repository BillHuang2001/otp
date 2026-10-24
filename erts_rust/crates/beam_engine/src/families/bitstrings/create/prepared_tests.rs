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
//! The **prepared-versus-generic equivalence** suite for `bs_create_bin`, plus the pins
//! that make the specialisation observable.
//!
//! `exec` runs a segment program resolved once per instruction ([`prepared::prepare`])
//! instead of walking `[Type, Index, Unit, Flags, Value, Size]` on every element. The
//! two routes must be **indistinguishable** — same bytes, same bit count, and for a
//! malformed segment the same typed error — which is what this file asserts for every
//! segment kind `prepare` resolves:
//!
//! | prepared kind | covered by |
//! |---|---|
//! | `ByteInt` (8/16/32/64 bits, both endiannesses, `unit > 1`, negative, bignum) | [`the_byte_field_stride_matches_the_generic_walk_on_every_width`] |
//! | `Integer` (bit-granular, `dynamic` size, non-integer value) | [`the_integer_family_matches_the_generic_walk`] |
//! | `Append` (`private_append` in place, `append`, a later position, a non-bitstring) | [`the_append_family_matches_the_generic_walk`] |
//! | `Utf` (`utf8`/`utf16`/`utf32`, `-little`) | [`the_utf_family_matches_the_generic_walk`] |
//! | `Float` (`dynamic` width, a non-float value, a zero-width field) | [`the_float_family_matches_the_generic_walk`] |
//! | `Binary` (`all`, an explicit size, a `unit` spelling, a non-bitstring) | [`the_binary_family_matches_the_generic_walk`] |
//!
//! The byte-exactness of the shapes themselves is owned by `inplace_tests.rs` /
//! `grow_tests.rs` / `le_tests.rs` / `wide_tests.rs` / `float_seg_tests.rs`; what is
//! pinned **here** is that the specialisation changes nothing.
//!
//! Three further pins: the direct store really runs for a byte field and really does
//! **not** run for an unaligned position ([`a_byte_field_takes_the_direct_store`]), a
//! shape `prepare` declines never enters the byte-field path at all
//! ([`a_declined_shape_stores_no_byte_field`]), and the one-per-instruction capacity
//! reservation leaves the payload's growth schedule untouched
//! ([`one_reservation_matches_the_per_field_capacity_checks`]).
use super::super::{byte_field_fallbacks, byte_field_stores};
use super::*;
use beam_loader::NO_FAIL;
use crate::context::MinimalContext;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Which segment walk a case runs.
#[derive(Clone, Copy)]
enum Route {
    /// [`assemble`] — the flat walk, one capacity check per field.
    Generic,
    /// [`prepared::prepare`] + [`prepared::assemble_prepared`] — the resolved program.
    Prepared,
}

/// A module naming every type/flag atom these cases use, whose literal area holds
/// **one** flag list (`flags`, empty for a flagless segment) at index 0.
///
/// The type atoms are interned *after* the literal decode, so the literal's own atom
/// indices are untouched.
fn case_module(flags: &[&str]) -> Module {
    let mut module = wide_tests::literal_module(&[wide_tests::flags_etf(flags)]);
    for name in [
        "binary",
        "float",
        "string",
        "append",
        "private_append",
        "all",
        "utf8",
        "utf16",
        "utf32",
        "nonsense",
    ] {
        module.atom_table.intern(name);
    }
    module
}

/// The flags operand of a segment: the literal-at-0 list `case_module` built (`[]` for a
/// flagless module).
fn flags() -> Operand {
    Operand::Literal(0)
}

/// One `[Type, Index, Unit, Flags, Value, Size]` group.
fn seg(module: &Module, kind: &str, unit: i64, value: Operand, size: Operand) -> Vec<Operand> {
    vec![
        Operand::Atom(module.atom_table.lookup(kind).expect("the type atom")),
        Operand::Integer(1),
        Operand::Integer(unit),
        flags(),
        value,
        size,
    ]
}

/// The `all` size operand.
fn all(module: &Module) -> Operand {
    Operand::Atom(module.atom_table.lookup("all").expect("the all atom"))
}

/// An atom term usable as a (malformed) segment value.
fn atom(module: &Module) -> Operand {
    Operand::Atom(module.atom_table.lookup("all").expect("the all atom"))
}

/// A small-integer term in register `index`.
fn set_small(machine: &mut Machine, index: u32, value: i64) {
    let term = beam_term::try_make_small(value).expect("a small integer");
    machine.set_x(index as usize, term).expect("register in range");
}

/// Run one route over `segments` and reduce it to `(bytes, nbits)` or an error string.
///
/// `fail` is [`NO_FAIL`] throughout, so a segment failure is the raise the oracle
/// reports (the bare `badarg`/`system_limit`) rather than a branch `Step`.
fn outcome(
    route: Route,
    module: &Module,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    segments: &[Operand],
) -> std::result::Result<(Vec<u8>, usize), String> {
    let labels = LabelMap::default();
    let assembled = match route {
        Route::Generic => assemble(machine, module, &labels, NO_FAIL, segments, ctx),
        Route::Prepared => {
            let program = prepared::prepare(module, segments)
                .unwrap_or_else(|| panic!("`prepare` declined this case: {segments:?}"));
            prepared::assemble_prepared(machine, module, &labels, NO_FAIL, &program, ctx)
        }
    };
    match assembled {
        Ok(Ok(built)) => Ok(built_bytes(built)),
        Ok(Err(step)) => Err(format!("segment failure branch: {step:?}")),
        Err(err) => Err(format!("{err:?}")),
    }
}

/// Run **both** routes over `build` (called once per route, so an in-place case gets its
/// own accumulator each time) and return the bytes they agree on.
fn compare<F>(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    build: &F,
) -> (Vec<u8>, usize)
where
    F: Fn(&mut Machine, &mut MinimalContext, &Module) -> Vec<Operand>,
{
    let generic_segments = build(machine, ctx, module);
    let generic = outcome(Route::Generic, module, machine, ctx, &generic_segments);
    let specialised_segments = build(machine, ctx, module);
    let specialised = outcome(Route::Prepared, module, machine, ctx, &specialised_segments);
    assert_eq!(
        generic, specialised,
        "the prepared walk must agree with the generic walk, byte for byte and error for error"
    );
    generic.expect("the case succeeds on both routes")
}

/// [`compare`] for a case whose two routes must fail identically; returns the error.
fn compare_err<F>(module: &Module, machine: &mut Machine, ctx: &mut MinimalContext, build: F) -> String
where
    F: Fn(&mut Machine, &mut MinimalContext, &Module) -> Vec<Operand>,
{
    let segments = build(machine, ctx, module);
    let generic = outcome(Route::Generic, module, machine, ctx, &segments);
    let segments = build(machine, ctx, module);
    let specialised = outcome(Route::Prepared, module, machine, ctx, &segments);
    assert_eq!(
        generic, specialised,
        "a malformed segment must fail identically on both routes"
    );
    generic.expect_err("the case fails on both routes")
}

/// A writable accumulator (`bs_init_writable`'s term) of `capacity` bytes in `x(0)`.
fn accumulator(machine: &mut Machine, ctx: &mut MinimalContext, capacity: usize) -> Eterm {
    let term = raw::make_writable(machine, ctx, capacity).expect("a writable binary");
    machine.set_x(0, term).expect("x(0) in range");
    term
}

/// The `private_append`/`append` accumulator source the compiler emits.
fn acc_operand() -> Operand {
    Operand::X(0)
}

// ---------------------------------------------------------------------------
// 1. Byte fields: the specialised `PreparedSeg::ByteInt` shape
// ---------------------------------------------------------------------------

/// Every static whole-byte integer width — both endiannesses, a negative value, a
/// `unit > 1` spelling of the same width, and the widths that stay bit-granular —
/// builds identical bytes on both routes.
#[test]
fn the_byte_field_stride_matches_the_generic_walk_on_every_width() {
    for (little, value, size, unit) in [
        (false, 200i64, 8i64, 1i64),
        (true, 200, 8, 1),
        (false, 0x1234, 16, 1),
        (true, 0x1234, 16, 1),
        (false, -5, 32, 1),
        (true, -5, 32, 1),
        (false, i64::MIN, 64, 1),
        (true, 0x0102_0304_0506_0708, 64, 1),
        // `unit > 1`: the width is `size * unit` = 16 bits.
        (false, 0x1234, 8, 2),
        // Widths the program deliberately leaves specialisable-never: 12 bits.
        (false, 0xABC, 12, 1),
        // Zero bits: nothing is written by either route.
        (false, 7, 0, 1),
    ] {
        let names = wide_tests::flag_names(little, false);
        let module = case_module(&names);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            seg(
                module,
                "integer",
                unit,
                Operand::Integer(value),
                Operand::Integer(size),
            )
        };
        let (bytes, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!(nbits, (size * unit) as usize, "the field's bit count");
        assert_eq!(bytes.len(), nbits.div_ceil(8), "the byte count follows the bits");
    }
}

/// The flag operand really decides the byte order — otherwise the comparison above
/// would pass for a monotone no-op.
#[test]
fn the_endianness_flag_really_changes_the_bytes() {
    let mut big = Vec::new();
    let mut little = Vec::new();
    for (names, out) in [(Vec::new(), &mut big), (vec!["little"], &mut little)] {
        let module = case_module(&names);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            seg(
                module,
                "integer",
                1,
                Operand::Integer(0x1234),
                Operand::Integer(16),
            )
        };
        let (bytes, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!(nbits, 16);
        *out = bytes;
    }
    assert_eq!(big, vec![0x12, 0x34], "`<<16#1234:16>>`");
    assert_eq!(little, vec![0x34, 0x12], "`<<16#1234:16/little>>`");
    assert_ne!(big, little, "the flag operand is not ignored");
}

/// A **bignum** into a narrow byte field: the truncating `mod 2^nbits` write the OTP
/// compiler sweep depends on, identical on both routes at every specialised width.
#[test]
fn a_bignum_into_a_narrow_byte_field_matches_the_generic_walk() {
    for size in [8i64, 16, 32, 64] {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            let value = wide_tests::bignum(machine, ctx, false, &[5, 1 << 6]);
            machine.set_x(2, value).expect("x(2) in range");
            seg(module, "integer", 1, Operand::X(2), Operand::Integer(size))
        };
        let (bytes, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!(nbits, size as usize);
        assert_eq!(bytes.len(), nbits / 8);
    }
}

// ---------------------------------------------------------------------------
// 2. The integer family's remaining shapes
// ---------------------------------------------------------------------------

/// Bit-granular widths, an unaligned byte field behind one, a `dynamic` size, and the
/// malformed (non-integer) value all behave as the generic walk does.
#[test]
fn the_integer_family_matches_the_generic_walk() {
    for (value, size) in [(1i64, 1i64), (0xABC, 12), (0x12345, 20)] {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            seg(
                module,
                "integer",
                1,
                Operand::Integer(value),
                Operand::Integer(size),
            )
        };
        let (_, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!(nbits, size as usize);
    }

    // A bit-granular field followed by a byte field: the byte field's position is
    // unaligned, so the bit writers must serve it on both routes.
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            let mut segments = seg(module, "integer", 1, Operand::Integer(5), Operand::Integer(4));
            segments.extend(seg(
                module,
                "integer",
                1,
                Operand::Integer(0xAB),
                Operand::Integer(8),
            ));
            segments
        };
        let (_, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!(nbits, 12, "4 unaligned bits plus a byte field");
    }

    // A runtime (`dynamic`) size: `prepare` resolves the size operand, never the width.
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            set_small(machine, 1, 16);
            seg(module, "integer", 1, Operand::Integer(0x1234), Operand::X(1))
        };
        let (_, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!(nbits, 16);
    }

    // A non-integer value is the bare `badarg` on both routes.
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let value = atom(&module);
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            seg(module, "integer", 1, value.clone(), Operand::Integer(8))
        };
        let error = compare_err(&module, &mut machine, &mut ctx, build);
        assert!(error.contains("badarg"), "the oracle's bare badarg: {error}");
    }
}

// ---------------------------------------------------------------------------
// 3. The append family (and the in-place accumulator)
// ---------------------------------------------------------------------------

/// `private_append` in the first position (the in-place accumulator), a plain `append`,
/// a `private_append` in a **later** position, and a non-bitstring append source all
/// build identical bytes on both routes.
#[test]
fn the_append_family_matches_the_generic_walk() {
    // (a) The compiler's loop shape: `private_append` first, then a byte field.
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 8);
            let mut segments = seg(module, "private_append", 1, acc_operand(), all(module));
            segments.extend(seg(
                module,
                "integer",
                1,
                Operand::Integer(0x7F),
                Operand::Integer(8),
            ));
            segments
        };
        let (bytes, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!((bytes, nbits), (vec![0x7F], 8));
    }

    // (b) `append` (the copy-rebuild form), then a byte field.
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            let source = raw::make_binary(machine, ctx, &[0xAA, 0xBB], 16).expect("a binary");
            machine.set_x(0, source).expect("x(0) in range");
            let mut segments = seg(module, "append", 1, acc_operand(), all(module));
            segments.extend(seg(
                module,
                "integer",
                1,
                Operand::Integer(0xCC),
                Operand::Integer(8),
            ));
            segments
        };
        let (bytes, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!((bytes, nbits), (vec![0xAA, 0xBB, 0xCC], 24));
    }

    // (c) A `private_append` in a LATER position: it cannot be the in-place opener, so
    // the writer is already a copy-rebuild one.
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 8);
            let source = raw::make_binary(machine, ctx, &[0x11, 0x22], 16).expect("a binary");
            machine.set_x(1, source).expect("x(1) in range");
            let mut segments = seg(
                module,
                "integer",
                1,
                Operand::Integer(0x01),
                Operand::Integer(8),
            );
            segments.extend(seg(module, "private_append", 1, Operand::X(1), all(module)));
            segments
        };
        let (bytes, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!((bytes, nbits), (vec![0x01, 0x11, 0x22], 24));
    }

    // (d) A malformed append: the source is not a bitstring -> bare `badarg`.
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let value = atom(&module);
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 8);
            seg(module, "append", 1, value.clone(), all(module))
        };
        let error = compare_err(&module, &mut machine, &mut ctx, build);
        assert!(error.contains("badarg"), "the oracle's bare badarg: {error}");
    }
}

// ---------------------------------------------------------------------------
// 4. utf8 / utf16 / utf32 and float
// ---------------------------------------------------------------------------

/// The three UTF segment kinds (and `utf16`/`utf32` little-endian) match the generic walk.
#[test]
fn the_utf_family_matches_the_generic_walk() {
    for (kind, little, value) in [
        ("utf8", false, 0x41i64),
        ("utf8", false, 0x20AC),
        ("utf8", false, 0x1_F600),
        ("utf16", false, 0x41),
        ("utf16", true, 0x20AC),
        ("utf32", false, 0x1_F600),
        ("utf32", true, 0x1_F600),
    ] {
        let names = wide_tests::flag_names(little, false);
        let module = case_module(&names);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            seg(module, kind, 1, Operand::Integer(value), Operand::Integer(0))
        };
        let (bytes, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert!(nbits > 0, "{kind}: a code point writes at least one bit");
        assert_eq!(bytes.len(), nbits.div_ceil(8), "{kind}: the byte count follows");
    }
}

/// The float segment's prepared shapes: a `dynamic` width over a non-float value, and
/// the zero-width field, both rejected identically on the two routes.
#[test]
fn the_float_family_matches_the_generic_walk() {
    // A runtime width, over an integer value (not a float).
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            set_small(machine, 1, 64);
            seg(module, "float", 1, Operand::Integer(0), Operand::X(1))
        };
        let error = compare_err(&module, &mut machine, &mut ctx, build);
        assert!(error.contains("badarg"), "a non-float float segment: {error}");
    }

    // A zero-width float field (`<<1.0:0/float>>` is a `badarg` on the oracle).
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            seg(module, "float", 1, Operand::Integer(0), Operand::Integer(0))
        };
        let error = compare_err(&module, &mut machine, &mut ctx, build);
        assert!(error.contains("badarg"), "a zero-width float field: {error}");
    }
}

// ---------------------------------------------------------------------------
// 5. The binary segment
// ---------------------------------------------------------------------------

/// `binary all`, an explicit size (and a `unit` spelling of the same width), and a
/// non-bitstring source all match the generic walk.
#[test]
fn the_binary_family_matches_the_generic_walk() {
    // (a) `all`.
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            let source = raw::make_binary(machine, ctx, &[1, 2, 3], 24).expect("a binary");
            machine.set_x(1, source).expect("x(1) in range");
            let mut segments = seg(module, "binary", 1, Operand::X(1), all(module));
            segments.extend(seg(
                module,
                "integer",
                1,
                Operand::Integer(0xFF),
                Operand::Integer(8),
            ));
            segments
        };
        let (bytes, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!((bytes, nbits), (vec![1, 2, 3, 0xFF], 32));
    }

    // (b) An explicit size, and a `unit` spelling of the same width.
    for (unit, size) in [(1i64, 16i64), (8, 2)] {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            let source = raw::make_binary(machine, ctx, &[0x12, 0x34], 16).expect("a binary");
            machine.set_x(1, source).expect("x(1) in range");
            seg(module, "binary", unit, Operand::X(1), Operand::Integer(size))
        };
        let (bytes, nbits) = compare(&module, &mut machine, &mut ctx, &build);
        assert_eq!(
            (bytes, nbits),
            (vec![0x12, 0x34], 16),
            "unit {unit} size {size}"
        );
    }

    // (c) A non-bitstring source.
    {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let value = atom(&module);
        let build = |machine: &mut Machine, ctx: &mut MinimalContext, module: &Module| {
            accumulator(machine, ctx, 64);
            seg(module, "binary", 1, value.clone(), all(module))
        };
        let error = compare_err(&module, &mut machine, &mut ctx, build);
        assert!(
            error.contains("badarg"),
            "a non-bitstring binary segment: {error}"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Shapes `prepare` declines
// ---------------------------------------------------------------------------

/// The segment shapes `prepare` does **not** resolve — an unknown type, a `string`
/// window the module's `StrT` table cannot serve, an `all`-sized integer, a runtime
/// unit, a flags operand that is not a list — are declined, so the generic walk reports
/// them exactly as before.
#[test]
fn declined_shapes_stay_on_the_generic_walk() {
    let module = case_module(&[]);

    let unknown = {
        let mut segments = seg(&module, "integer", 1, Operand::Integer(0), Operand::Integer(8));
        segments[0] = Operand::Atom(module.atom_table.lookup("nonsense").expect("the atom"));
        segments
    };
    assert!(
        prepared::prepare(&module, &unknown).is_none(),
        "an unknown segment type is the generic walk's typed error"
    );

    let string = seg(
        &module,
        "string",
        1,
        Operand::Integer(0),
        Operand::Integer(1),
    );
    assert!(
        prepared::prepare(&module, &string).is_none(),
        "a string window this module cannot serve is declined"
    );

    let integer_all = seg(&module, "integer", 1, Operand::Integer(1), all(&module));
    assert!(
        prepared::prepare(&module, &integer_all).is_none(),
        "`<<V:all>>` keeps the generic arm's typed error"
    );

    let runtime_unit = {
        let mut segments = seg(&module, "integer", 1, Operand::Integer(1), Operand::Integer(8));
        segments[2] = Operand::X(7);
        segments
    };
    assert!(
        prepared::prepare(&module, &runtime_unit).is_none(),
        "a runtime unit cannot be folded into a static width"
    );

    let bad_flags = {
        let mut segments = seg(&module, "integer", 1, Operand::Integer(1), Operand::Integer(8));
        segments[3] = Operand::Integer(0);
        segments
    };
    assert!(
        prepared::prepare(&module, &bad_flags).is_none(),
        "a flags operand that is not a list is declined"
    );
}

// ---------------------------------------------------------------------------
// 7. Pins: the direct store is taken, and only for a specialised shape
// ---------------------------------------------------------------------------

/// A byte field at a **byte-aligned** position takes the direct store; the same field
/// after a bit-granular segment does not (it falls back to the bit writers), and every
/// field of the fallback is still byte-identical to the generic route.
#[test]
fn a_byte_field_takes_the_direct_store() {
    let module = case_module(&[]);

    // (a) Aligned: four byte fields, four direct stores, no fallback.
    {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let program = {
            let mut segments = seg(&module, "integer", 1, Operand::Integer(1), Operand::Integer(8));
            for size in [8i64, 16, 32, 64] {
                segments.extend(seg(
                    &module,
                    "integer",
                    1,
                    Operand::Integer(0x0102_0304),
                    Operand::Integer(size),
                ));
            }
            prepared::prepare(&module, &segments).expect("a prepared program")
        };
        assert!(
            program
                .segments
                .iter()
                .all(|s| matches!(s, prepared::PreparedSeg::ByteInt { .. })),
            "every static whole-byte width resolves to the direct-store kind"
        );
        let before_stores = byte_field_stores();
        let before_fallbacks = byte_field_fallbacks();
        let segments = {
            let mut segments = seg(&module, "integer", 1, Operand::Integer(1), Operand::Integer(8));
            for size in [8i64, 16, 32, 64] {
                segments.extend(seg(
                    &module,
                    "integer",
                    1,
                    Operand::Integer(0x0102_0304),
                    Operand::Integer(size),
                ));
            }
            segments
        };
        let built = prepared::assemble_prepared(
            &mut machine,
            &module,
            &LabelMap::default(),
            NO_FAIL,
            &program,
            &mut ctx,
        )
        .expect("assemble")
        .expect("the build succeeds");
        assert_eq!(byte_field_stores() - before_stores, 4, "one store per field");
        assert_eq!(
            byte_field_fallbacks() - before_fallbacks,
            0,
            "an aligned byte field never falls back"
        );
        assert_eq!(built_bytes(built).1, 8 + 8 + 16 + 32 + 64);
    }

    // (b) Unaligned: a 4-bit field first, so the byte field behind it must fall back.
    {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let mut segments = seg(&module, "integer", 1, Operand::Integer(5), Operand::Integer(4));
        segments.extend(seg(
            &module,
            "integer",
            1,
            Operand::Integer(0xAB),
            Operand::Integer(8),
        ));
        let program = prepared::prepare(&module, &segments).expect("a prepared program");
        assert!(
            matches!(program.segments[1], prepared::PreparedSeg::ByteInt { bytes: 1, .. }),
            "the width is still specialised — only the position is not"
        );
        let before_stores = byte_field_stores();
        let before_fallbacks = byte_field_fallbacks();
        let built = prepared::assemble_prepared(
            &mut machine,
            &module,
            &LabelMap::default(),
            NO_FAIL,
            &program,
            &mut ctx,
        )
        .expect("assemble")
        .expect("the build succeeds");
        assert_eq!(
            byte_field_stores() - before_stores,
            0,
            "an unaligned position cannot take the direct store"
        );
        assert_eq!(
            byte_field_fallbacks() - before_fallbacks,
            1,
            "the unaligned field takes the bit-exact writers"
        );
    }
}

/// A shape the prepared program does not own — a `dynamic`-width integer, i.e. the
/// generic `PreparedSeg::Integer` kind, and a program `prepare` declines outright —
/// never enters the byte-field path.
#[test]
fn a_declined_shape_stores_no_byte_field() {
    let module = case_module(&[]);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();

    // A runtime width: resolved as `Integer { width: Dynamic }`, never `ByteInt`.
    let mut segments = seg(&module, "integer", 1, Operand::Integer(0x12), Operand::X(1));
    segments.extend(seg(
        &module,
        "integer",
        1,
        Operand::Integer(0x34),
        Operand::Integer(8),
    ));
    let program = prepared::prepare(&module, &segments).expect("a prepared program");
    assert!(
        matches!(
            program.segments[0],
            prepared::PreparedSeg::Integer {
                width: prepared::Width::Dynamic { .. },
                ..
            }
        ),
        "a runtime width keeps the generic integer kind"
    );
    set_small(&mut machine, 1, 8);
    let before_stores = byte_field_stores();
    prepared::assemble_prepared(
        &mut machine,
        &module,
        &LabelMap::default(),
        NO_FAIL,
        &program,
        &mut ctx,
    )
    .expect("assemble")
    .expect("the build succeeds");
    assert_eq!(
        byte_field_stores(),
        before_stores,
        "a dynamic width is not a direct store"
    );
}

// ---------------------------------------------------------------------------
// 8. The one-per-instruction capacity reservation
// ---------------------------------------------------------------------------

/// The summed-static-width reservation changes **no** observable: the same accumulator
/// chain ends with the same bytes and the same `RefcBinary::growth_counters` whether the
/// program armed a reservation (prepared) or ran the per-field checks (generic).
///
/// Both the fits-in-the-payload case (no growth at all) and the must-grow case are
/// covered, because the reservation must never *suppress* a growth the per-field route
/// would have made.
#[test]
fn one_reservation_matches_the_per_field_capacity_checks() {
    /// One accumulator chain of `count` byte fields, run on its own machine, reported as
    /// `(bytes, nbits, in_place_grows, moves, bytes_copied)`.
    fn chain(route: Route, capacity: usize, count: usize) -> (Vec<u8>, usize, u64, u64, u64) {
        let module = case_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        accumulator(&mut machine, &mut ctx, capacity);
        let mut segments = seg(&module, "private_append", 1, acc_operand(), all(&module));
        for _ in 0..count {
            segments.extend(seg(
                &module,
                "integer",
                1,
                Operand::Integer(0x7F),
                Operand::Integer(8),
            ));
        }
        let labels = LabelMap::default();
        let built = match route {
            Route::Generic => assemble(&mut machine, &module, &labels, NO_FAIL, &segments, &mut ctx),
            Route::Prepared => {
                let program = prepared::prepare(&module, &segments).expect("a prepared program");
                assert_eq!(
                    program.reserve_after[0],
                    Some(count * 8),
                    "the reservation is the summed static width of the tail"
                );
                prepared::assemble_prepared(&mut machine, &module, &labels, NO_FAIL, &program, &mut ctx)
            }
        }
        .expect("assemble")
        .expect("the build succeeds");
        let (bytes, nbits) = built_bytes(built);
        let counters = machine
            .refc_binaries()
            .first()
            .expect("the accumulator's payload")
            .growth_counters();
        (
            bytes,
            nbits,
            counters.in_place_grows,
            counters.moves,
            counters.bytes_copied,
        )
    }

    // Fits: 16 bytes of fields into a payload with room — neither route grows.
    let generic = chain(Route::Generic, 64, 16);
    let specialised = chain(Route::Prepared, 64, 16);
    assert_eq!(generic, specialised, "a reservation that covers the tail");
    assert_eq!(
        (generic.2, generic.3),
        (0, 0),
        "the payload already had the room"
    );
    assert_eq!(generic.1, 128, "16 byte fields");
    assert_eq!(generic.0, vec![0x7F; 16]);

    // Must grow: 4 KiB of fields into an 8-byte payload — both routes take the same
    // growth door the same number of times.
    let generic = chain(Route::Generic, 8, 4096);
    let specialised = chain(Route::Prepared, 8, 4096);
    assert_eq!(generic, specialised, "a reservation shorter than the tail");
    assert!(
        generic.2 + generic.3 > 0,
        "the chain outgrows the payload: {generic:?}"
    );
    assert_eq!(generic.1, 4096 * 8);
}
