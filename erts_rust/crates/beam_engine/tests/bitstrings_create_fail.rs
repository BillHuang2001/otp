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

//! Bitstring `bs_create_bin` **segment-failure** integration tests: the exact
//! `{Class, Reason}` pair a **caught construction failure** produces, pinned end to
//! end against the real system `erl` oracle (OTP 29 / `erts-17.1`).
//!
//! Loads the committed oracle fixture `tests/fixtures/bsfailseg.beam` (produced by the
//! system `erlc`, see `tests/fixtures/README.md`) and runs each export through
//! [`Machine::run_export`] with a [`MinimalContext`] — the fixture is pure compiled
//! code, so the context needs **no** BIF hook. **Every expected value below was
//! derived from the same system `erl` oracle**, quoting the exact `erl -noshell
//! -eval` command beside the assertion; nothing is guessed. The `bs_create_bin`
//! segment each export actually emits was confirmed with `beam_disasm:file/1` (a
//! single `bs_create_bin` with the `{f,0}` (`no_fail`) label, so a segment failure is
//! a real raise rather than a branch).
//!
//! # Why the fixture has a `pinned_atoms/0` export
//!
//! The OTP compiler does **not** put the runtime's exception reason atoms into a
//! module's atom chunk — a clean `beam_lib:chunks("bsfailseg.beam",[atoms])` reports
//! neither `badarg` nor `system_limit` (the real runtime keeps them globally
//! interned, `erts/emulator/beam/beam_common.c:382-406`). The engine resolves a
//! construction failure's reason atom from the *running module's* atom table and
//! refuses to substitute a different atom (see `families/bitstrings/create.rs`
//! `raise_seg_fail`), so a module that cannot *name* the atom degrades to a typed
//! [`BeamError`] instead of a `{error, Atom}` term. The fixture's `pinned_atoms/0`
//! returns the literal list `[badarg, badmatch, system_limit, error]`, which
//! `beam_loader` interns into the module's atom table while decoding `LitT`, so every
//! failure below lands as a real `{error, ReasonAtom}` 2-tuple. (`bscreate.beam` does
//! **not** pin these atoms, which is why its rejection cases assert the typed
//! [`BeamError::BadArg`]/[`BeamError::SystemLimit`] instead; see
//! `tests/bitstrings_extra_create.rs`.)
//!
//! # Two harness limits worked around here
//!
//! 1. `terms::make_heap_binary` is **byte-granular**, so the non-byte-aligned subject
//!    `<<1:4>>` is built with the frozen `beam_term::bitstring` writer over an
//!    `ExecutionContext::alloc_words` reservation (this file's `bitstring` helper).
//! 2. `terms::binary_bytes` rejects a **non-byte-aligned** result, so a success such
//!    as `int_fixed(5)` ⇒ `<<5:3>>` is pinned bit-exactly with `beam_term::bit_at`
//!    (this file's `all_bits` helper).

use beam_common::BeamError;
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{make_small, Atom, Eterm};

/// The committed segment-failure fixture (`erlc bsfailseg.erl`).
const BSFAILSEG_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bsfailseg.beam");

fn load() -> Module {
    load_file(BSFAILSEG_BEAM).unwrap_or_else(|e| panic!("load {BSFAILSEG_BEAM}: {e:?}"))
}

/// Resolve an export (or a reason/class atom) by name; a missing atom is a stale
/// fixture, so the panic is the intended guard.
fn export_atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned"))
}

/// Run an export with `args` on a fresh machine over the caller-owned `ctx`.
fn run(
    module: &Module,
    name: &str,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_export(
        module,
        export_atom(module, name),
        args.len() as u32,
        args,
        ctx,
    )
}

/// A byte-aligned heap binary argument (the crate's public binary builder).
fn bin(ctx: &mut MinimalContext, data: &[u8]) -> Eterm {
    terms::make_heap_binary(ctx, data).expect("heap binary")
}

/// An on-heap bitstring term of exactly `n_bits` bits (the most-significant bit of
/// `bytes[0]` first), for a subject `terms::make_heap_binary` cannot express (a
/// non-byte-aligned window such as `<<1:4>>`).
///
/// SAFETY: the reservation and the write are the frozen `beam_term::bitstring`
/// contract; `hp` points at exactly `heap_bits_size_words(n_bits)` exclusively owned
/// words handed out by the context's pointer-stable arena, which outlives every term
/// built from it.
#[allow(unsafe_code)]
fn bitstring(ctx: &mut MinimalContext, bytes: &[u8], n_bits: usize) -> Eterm {
    let words = beam_term::heap_bits_size_words(n_bits);
    let hp = ctx.alloc_words(words).expect("reserve the heap-bits block");
    // SAFETY: as above — `hp` is a live, writable, pointer-stable block of `words`
    // words, exactly what `write_heap_bits` requires.
    unsafe { beam_term::write_heap_bits(hp, bytes, n_bits) }.expect("write the heap bitstring")
}

/// The bytes of a byte-aligned result binary; a wrong shape is a test bug.
fn bytes(term: Eterm) -> Vec<u8> {
    terms::binary_bytes(term)
        .unwrap_or_else(|e| panic!("not a byte-aligned binary: {e:?}"))
        .to_vec()
}

/// Every bit (most-significant first) of a bitstring term, whatever its alignment.
///
/// SAFETY: `term` is a live bitstring just returned by a run, and `i < size` is
/// guaranteed by the range.
#[allow(unsafe_code)]
fn all_bits(term: Eterm) -> Vec<bool> {
    // SAFETY: `term` is a live bitstring term.
    let (_base, _offset, size) = unsafe { beam_term::bitstring_parts(term) };
    (0..size)
        // SAFETY: `i < size`, the term's exact bit size.
        .map(|i| unsafe { beam_term::bit_at(term, i) })
        .collect()
}

/// Assert `name(args)` returns the 2-tuple `{Class, Reason}` whose two elements are
/// the `class`/`reason` atoms the fixture pins in its atom table.
fn assert_raise(
    module: &Module,
    name: &str,
    args: &[Eterm],
    class: &str,
    reason: &str,
    ctx: &mut MinimalContext,
) {
    let term = run(module, name, args, ctx).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    let arity = terms::tuple_arity(term).unwrap_or_else(|e| panic!("{name}: not a tuple: {e:?}"));
    assert_eq!(arity, 2, "{name}: expected a {{Class,Reason}} 2-tuple");
    let got_class = terms::tuple_element(term, 0).expect("class element");
    let got_reason = terms::tuple_element(term, 1).expect("reason element");
    assert_eq!(
        got_class,
        beam_term::make_atom(export_atom(module, class)),
        "{name}: exception class"
    );
    assert_eq!(
        got_reason,
        beam_term::make_atom(export_atom(module, reason)),
        "{name}: failure reason"
    );
}

/// A `{error, Reason}` shorthand with the two pinned atoms this fixture uses.
fn assert_error(
    module: &Module,
    name: &str,
    args: &[Eterm],
    reason: &str,
    ctx: &mut MinimalContext,
) {
    assert_raise(module, name, args, "error", reason, ctx);
}

// ---------------------------------------------------------------------------
// The pinned reason atoms
// ---------------------------------------------------------------------------

#[test]
fn pinned_atoms_interns_the_reason_atoms() {
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:pinned_atoms()]),halt().'
    // [badarg,badmatch,system_limit,error]
    let module = load();
    let mut ctx = MinimalContext::new();
    let term = run(&module, "pinned_atoms", &[], &mut ctx).expect("pinned_atoms/0");

    let got = terms::list_elements(term).expect("a proper list");
    let expected = ["badarg", "badmatch", "system_limit", "error"]
        .map(|n| beam_term::make_atom(export_atom(&module, n)));
    assert_eq!(
        got, expected,
        "pinned_atoms/0 must return the four reason atoms"
    );
}

// ---------------------------------------------------------------------------
// `binary` segments
// ---------------------------------------------------------------------------

#[test]
fn binary_segment_needs_enough_source_bits() {
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_one(<<1:4>>)]),halt().'
    // {error,badarg}
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_one(<<1:8>>)]),halt().'
    // <<1>>
    let module = load();
    let mut ctx = MinimalContext::new();

    let short = bitstring(&mut ctx, &[0x10], 4);
    assert_error(&module, "bin_one", &[short], "badarg", &mut ctx);

    let full = bin(&mut ctx, &[1]);
    let ok = run(&module, "bin_one", &[full], &mut ctx).expect("bin_one(<<1:8>>)");
    assert_eq!(bytes(ok), vec![1], "bin_one(<<1:8>>) ⇒ <<1>>");
}

#[test]
fn binary_all_segment_returns_the_source() {
    // `<<X/binary>>` compiles to an `append` (whole-tail) segment; it succeeds for
    // any bitstring source.
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_all(<<1,2>>)]),halt().'
    // <<1,2>>
    let module = load();
    let mut ctx = MinimalContext::new();

    let src = bin(&mut ctx, &[1, 2]);
    let ok = run(&module, "bin_all", &[src], &mut ctx).expect("bin_all(<<1,2>>)");
    assert_eq!(bytes(ok), vec![1, 2], "bin_all(<<1,2>>) ⇒ <<1,2>>");
}

#[test]
fn binary_all_unit_checks_the_source_bit_size() {
    // `<<X/binary-unit:8>>` is the whole-tail `append` segment with unit 8, so a
    // source whose bit size is not a multiple of 8 fails.
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_all_unit(<<1:4>>)]),halt().'
    // {error,badarg}
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_all_unit(<<1:8>>)]),halt().'
    // <<1>>
    let module = load();
    let mut ctx = MinimalContext::new();

    let short = bitstring(&mut ctx, &[0x10], 4);
    assert_error(&module, "bin_all_unit", &[short], "badarg", &mut ctx);

    let full = bin(&mut ctx, &[1]);
    let ok = run(&module, "bin_all_unit", &[full], &mut ctx).expect("bin_all_unit(<<1:8>>)");
    assert_eq!(bytes(ok), vec![1], "bin_all_unit(<<1:8>>) ⇒ <<1>>");
}

#[test]
fn fixed_binary_segment_width_is_size_times_unit() {
    // `<<X:8/binary>>` is a `binary` segment whose width is `size * unit` = 8 * 8 =
    // 64 bits, i.e. eight bytes.
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_fixed(<<1,2,3>>)]),halt().'
    // {error,badarg}
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_fixed(<<1,2,3,4,5,6,7,8>>)]),halt().'
    // <<1,2,3,4,5,6,7,8>>
    let module = load();
    let mut ctx = MinimalContext::new();

    let short = bin(&mut ctx, &[1, 2, 3]);
    assert_error(&module, "bin_fixed", &[short], "badarg", &mut ctx);

    let full = bin(&mut ctx, &[1, 2, 3, 4, 5, 6, 7, 8]);
    let ok = run(&module, "bin_fixed", &[full], &mut ctx).expect("bin_fixed(<<1..8>>)");
    assert_eq!(bytes(ok), vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn dynamic_binary_size_rejects_a_negative_size() {
    // `<<X:S/binary>>` with a negative size. `-(1 bsl 59)` is `MIN_SMALL`, a small
    // negative, so the C `signed_size >= 0` check (`emu/bs_instrs.tab:423`) fails.
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_size(<<1:4>>, -(1 bsl 59))]),halt().'
    // {error,badarg}
    let module = load();
    let mut ctx = MinimalContext::new();

    let src = bitstring(&mut ctx, &[0x10], 4);
    let neg = make_small(-(1i64 << 59));
    assert_error(&module, "bin_size", &[src, neg], "badarg", &mut ctx);
}

#[test]
fn dynamic_binary_size_rejects_a_positive_bignum_size() {
    // A size operand `2^59` — `MAX_SMALL + 1`, materialised as a bignum from the
    // module's literal area — is `system_limit` (`emu/bs_instrs.tab:438`).
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:bin_big_size(<<1:4>>)]),halt().'
    // {error,system_limit}
    let module = load();
    let mut ctx = MinimalContext::new();

    let src = bitstring(&mut ctx, &[0x10], 4);
    assert_error(&module, "bin_big_size", &[src], "system_limit", &mut ctx);
}

// ---------------------------------------------------------------------------
// `utf8` / `utf16` / `utf32` segments
// ---------------------------------------------------------------------------

#[test]
fn utf_segments_reject_non_scalars() {
    // A negative codepoint, a lone surrogate, a value above U+10FFFF and a
    // non-integer are all `{error,badarg}`, for every UTF segment kind:
    //
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:utf8(-1)]),halt().'
    // {error,badarg}
    //
    // and identically for `16#D800`, `16#110000` and the non-integer atom `oops`
    // under `utf8`/`utf16`/`utf16-little`/`utf32` (`erl_bits.c:1162`/`:1222`,
    // `erts_bs_put_utf8`/`_utf16`).
    let mut module = load();
    let mut ctx = MinimalContext::new();
    // `MinimalContext` has no atom interner, so the non-integer argument atom is
    // interned into the module's own table (`Module::atom_table` is public).
    let oops = beam_term::make_atom(module.atom_table.intern("oops"));
    for ctor in ["utf8", "utf16", "utf16le", "utf32"] {
        for value in [-1, 0xD800, 0x110000] {
            assert_error(&module, ctor, &[make_small(value)], "badarg", &mut ctx);
        }
        assert_error(&module, ctor, &[oops], "badarg", &mut ctx);
    }
}

// ---------------------------------------------------------------------------
// `integer` segments
// ---------------------------------------------------------------------------

#[test]
fn integer_segment_dynamic_size_validates_like_binary() {
    // `<<V:S/integer>>` shares the size validation: a negative size is `badarg`, a
    // positive-bignum size is `system_limit`.
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:int_size(1,-1)]),halt().'
    // {error,badarg}
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:int_size(1,1 bsl 59)]),halt().'
    // {error,system_limit}
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:int_size(1,3)]),halt().'
    // <<1:3>>
    let module = load();
    let mut ctx = MinimalContext::new();

    let one = make_small(1);
    assert_error(
        &module,
        "int_size",
        &[one, make_small(-1)],
        "badarg",
        &mut ctx,
    );

    // `1 bsl 59` is built by the test as an exact bignum (`terms::make_integer`).
    let big = terms::make_integer(&mut ctx, 1i64 << 59).expect("bignum size");
    assert_error(&module, "int_size", &[one, big], "system_limit", &mut ctx);

    let ok = run(&module, "int_size", &[one, make_small(3)], &mut ctx).expect("int_size(1,3)");
    assert_eq!(
        all_bits(ok),
        vec![false, false, true],
        "int_size(1,3) ⇒ <<1:3>>"
    );
}

#[test]
fn integer_segment_rejects_a_non_integer_value() {
    // `<<V:3/integer>>` with a non-integer value is `badarg`
    // (`erts_bs_put_integer_*`, `erl_bits.c:926`); `<<5:3>>` succeeds.
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:int_fixed(oops)]),halt().'
    // {error,badarg}
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:int_fixed(5)]),halt().'
    // <<5:3>>
    let mut module = load();
    let mut ctx = MinimalContext::new();

    let oops = beam_term::make_atom(module.atom_table.intern("oops"));
    assert_error(&module, "int_fixed", &[oops], "badarg", &mut ctx);

    let ok = run(&module, "int_fixed", &[make_small(5)], &mut ctx).expect("int_fixed(5)");
    assert_eq!(
        all_bits(ok),
        vec![true, false, true],
        "int_fixed(5) ⇒ <<5:3>>"
    );
}

// ---------------------------------------------------------------------------
// `float` segments
// ---------------------------------------------------------------------------

#[test]
fn float_segment_width_must_be_16_32_or_64() {
    // `<<F:S/float>>` accepts only 16/32/64 bits; every other width (including 0) is
    // `badarg` (`erts_bs_put_float`, `erl_bits.c:1341`/`:1449`).
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:float_size(1.0,0)]),halt().'
    // {error,badarg}
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:float_size(1.0,24)]),halt().'
    // {error,badarg}
    // $ erl -noshell -eval 'io:format("~p~n",[bsfailseg:float_size(1.0,32)]),halt().'
    // <<63,128,0,0>>
    let module = load();
    let mut ctx = MinimalContext::new();

    let f = terms::make_float(&mut ctx, 1.0).expect("float arg");
    assert_error(
        &module,
        "float_size",
        &[f, make_small(0)],
        "badarg",
        &mut ctx,
    );
    assert_error(
        &module,
        "float_size",
        &[f, make_small(24)],
        "badarg",
        &mut ctx,
    );

    let ok =
        run(&module, "float_size", &[f, make_small(32)], &mut ctx).expect("float_size(1.0,32)");
    assert_eq!(
        bytes(ok),
        vec![63, 128, 0, 0],
        "float_size(1.0,32) ⇒ binary32 1.0"
    );
}

// ---------------------------------------------------------------------------
// Sanity: the fixture's exports and pinned atoms are the ones the tests rely on
// ---------------------------------------------------------------------------

#[test]
fn every_failure_export_and_pinned_atom_is_present() {
    // A guard against a stale/rebuilt fixture: each export the tests call, and each
    // reason/class atom they assert on, must resolve in the module (a missing one
    // fails the lookup).
    let module = load();
    for name in [
        "pinned_atoms",
        "bin_one",
        "bin_all",
        "bin_all_unit",
        "bin_fixed",
        "bin_size",
        "bin_big_size",
        "utf8",
        "utf16",
        "utf16le",
        "utf32",
        "int_size",
        "int_fixed",
        "float_size",
    ] {
        let _: Atom = export_atom(&module, name);
    }
    for atom in ["badarg", "badmatch", "system_limit", "error"] {
        let _: Atom = export_atom(&module, atom);
    }
}
