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

//! Integration tests for the **on-demand interning of *produced* tag atoms**
//! across `beam_bifs`, driven through the **public** API only
//! (`default_registry()` + `BifRegistry::call`).
//!
//! A BIF that *produces* a fixed atom (a result tag, a sentinel like `nomatch` or
//! `value`, …) must not resolve it with the lookup-only [`BifContext::atom`]: a
//! runtime whose boot seed lacks that literal (e.g. `beam_rt`'s `CORE_ATOMS` has
//! no `nomatch`/`latin1`/`none`/`invalid`/…) would then abort the whole process
//! with an uncatchable `BeamError::TermNotFound`. The producers therefore go
//! through [`BifContext::intern_atom`] (lookup-then-intern through the optional
//! [`AtomInterner`] seam; an already-interned name never needs the hook).
//!
//! Each case below builds a context whose atom table **deliberately omits** the
//! produced atom, installs an [`AtomInterner`] mock, calls the BIF through the
//! registry and reads the produced tag's *name* back out of the **interner's**
//! table (that is where the created atom lives). The negative control — the same
//! call with no interner — must be the typed `BeamError::TermNotFound`, never a
//! fabricated index.
//!
//! Every golden carries the system-OTP probe that pinned it (OTP 29.1.1 /
//! erts-17.1). A final test re-checks the same shapes against the live `erl`,
//! skipping loudly when the oracle is unavailable (the `tests/proc_reg.rs` /
//! `tests/os_bif.rs` / `tests/nif.rs` pattern).

use std::cell::RefCell;
use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_codec::term::{make_binary, make_flatmap, tuple_element};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, make_small, Atom, AtomTable, Eterm, NIL};

/// A heap that fits the small tests below (a map, a couple of tuples/lists, a
/// handful of binaries).
fn heap() -> Heap {
    Heap::new(4096).expect("test heap")
}

/// The context atom table.
///
/// It is **empty on purpose**: none of the produced tag atoms (`nomatch`,
/// `latin1`, `false`, `true`, `error`, `invalid`, `value`, `none`) is seeded, so
/// every case must create its produced atom through the [`AtomInterner`] seam.
fn seeded_atoms() -> AtomTable {
    AtomTable::new()
}

/// Calls `module:function/arity` through the full public registry.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm> {
    reg.call(module, function, arity, ctx, args)
}

/// A char list `"..."` as an Erlang term (the argument shape `os:getenv/1` and
/// friends accept).
fn string_term(ctx: &mut BifContext<'_>, s: &str) -> Eterm {
    let elems: Vec<Eterm> = s.bytes().map(|b| ctx.int(i64::from(b)).expect("fits")).collect();
    ctx.list(&elems).expect("the string fits")
}

/// An [`AtomInterner`] over its own table, **seeded as a clone** of the context's
/// so an already-interned name keeps the exact index the context's table spells
/// (the index coupling [`BifContext::intern_atom`] relies on).
///
/// The table outlives the context, so a created tag's name can be read back after
/// the context (and its `&mut` borrow of the interner) is gone.
struct Interner {
    table: RefCell<AtomTable>,
}

impl Interner {
    fn seeded_like(atoms: &AtomTable) -> Interner {
        Interner {
            table: RefCell::new(atoms.clone()),
        }
    }

    /// The name the interner's table spells for the atom term `term`, if any.
    fn name_of(&self, term: Eterm) -> Option<String> {
        self.table.borrow().name(atom_val(term)).map(str::to_string)
    }
}

impl AtomInterner for Interner {
    fn intern(&mut self, name: &str) -> Atom {
        self.table.borrow_mut().intern(name)
    }
}

// ---------------------------------------------------------------------------
// One case per producer: the produced tag is interned on demand
// ---------------------------------------------------------------------------

#[test]
fn binary_match_produces_nomatch() {
    // ORACLE (erts-17.1): `binary:match(<<"xyz">>, <<"ab">>)` => `nomatch`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut interner = Interner::seeded_like(&atoms);

    let term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let subject = make_binary(ctx.heap(), b"xyz").expect("fits");
        let pattern = make_binary(ctx.heap(), b"ab").expect("fits");
        call(&reg, &mut ctx, "binary", "match", 2, &[subject, pattern]).expect("a value")
    };

    assert_eq!(interner.name_of(term).as_deref(), Some("nomatch"));
}

#[test]
fn io_printable_range_produces_latin1() {
    // ORACLE (erts-17.1): `io:printable_range()` => `latin1` (no `+pc`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut interner = Interner::seeded_like(&atoms);

    let term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        call(&reg, &mut ctx, "io", "printable_range", 0, &[]).expect("a value")
    };

    assert_eq!(interner.name_of(term).as_deref(), Some("latin1"));
}

#[test]
fn os_getenv_produces_false() {
    // ORACLE (erts-17.1): `os:getenv("ZZ_NO_SUCH_ENV_ZZ")` => `false`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut interner = Interner::seeded_like(&atoms);

    let term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let key = string_term(&mut ctx, "ZZ_NO_SUCH_ENV_ZZ");
        call(&reg, &mut ctx, "os", "getenv", 1, &[key]).expect("a value")
    };

    assert_eq!(interner.name_of(term).as_deref(), Some("false"));
}

#[test]
fn os_putenv_and_unsetenv_produce_true() {
    // ORACLE (erts-17.1): `os:putenv("ZZ_PRODUCED_ATOM_TEST", "1")` => `true`;
    //                     `os:unsetenv("ZZ_PRODUCED_ATOM_TEST")` => `true`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut interner = Interner::seeded_like(&atoms);

    let (put, unset) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let key = string_term(&mut ctx, "ZZ_PRODUCED_ATOM_TEST");
        let value = string_term(&mut ctx, "1");
        let put = call(&reg, &mut ctx, "os", "putenv", 2, &[key, value]).expect("a value");
        let unset_key = string_term(&mut ctx, "ZZ_PRODUCED_ATOM_TEST");
        let unset =
            call(&reg, &mut ctx, "os", "unsetenv", 1, &[unset_key]).expect("a value");
        (put, unset)
    };

    assert_eq!(interner.name_of(put).as_deref(), Some("true"));
    assert_eq!(interner.name_of(unset).as_deref(), Some("true"));
}

#[test]
fn decode_packet_produces_error_and_invalid() {
    // ORACLE (erts-17.1): `erlang:decode_packet(4, <<255,255,255,255>>, [])`
    // => `{error,invalid}` — the 4-byte-big-endian declared length `0xFFFFFFFF`
    // exceeds the default ceiling `INT_MAX - 4` (`packet_parser.c`'s `remain:`
    // label), which is the only arm that produces `invalid`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut interner = Interner::seeded_like(&atoms);

    let term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let packet = make_binary(ctx.heap(), &[255, 255, 255, 255]).expect("fits");
        call(
            &reg,
            &mut ctx,
            "erlang",
            "decode_packet",
            3,
            &[make_small(4), packet, NIL],
        )
        .expect("a value")
    };

    let tag = tuple_element(term, 0).expect("the tag");
    let reason = tuple_element(term, 1).expect("the reason");
    assert_eq!(interner.name_of(tag).as_deref(), Some("error"));
    assert_eq!(interner.name_of(reason).as_deref(), Some("invalid"));
}

#[test]
fn lists_keysearch_produces_value() {
    // ORACLE (erts-17.1): `lists:keysearch(2, 1, [{1,10},{2,20}])`
    // => `{value,{2,20}}`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut interner = Interner::seeded_like(&atoms);

    let term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let first = ctx.tuple(&[make_small(1), make_small(10)]).expect("fits");
        let second = ctx.tuple(&[make_small(2), make_small(20)]).expect("fits");
        let list = ctx.list(&[first, second]).expect("fits");
        call(
            &reg,
            &mut ctx,
            "lists",
            "keysearch",
            3,
            &[make_small(2), make_small(1), list],
        )
        .expect("a value")
    };

    let tag = tuple_element(term, 0).expect("the tag");
    assert_eq!(interner.name_of(tag).as_deref(), Some("value"));
}

#[test]
fn maps_find_produces_error() {
    // ORACLE (erts-17.1): `maps:find(9, #{1 => 2})` => `error`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut interner = Interner::seeded_like(&atoms);

    let term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let map = make_flatmap(ctx.heap(), &[(make_small(1), make_small(2))]).expect("fits");
        call(&reg, &mut ctx, "maps", "find", 2, &[make_small(9), map]).expect("a value")
    };

    assert_eq!(interner.name_of(term).as_deref(), Some("error"));
}

#[test]
fn maps_next_on_an_empty_map_produces_none() {
    // ORACLE (erts-17.1): `maps:next(maps:iterator(#{}))` => `none`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut interner = Interner::seeded_like(&atoms);

    let term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let map = make_flatmap(ctx.heap(), &[]).expect("the empty map fits");
        let iter = call(&reg, &mut ctx, "maps", "iterator", 1, &[map]).expect("an iterator");
        call(&reg, &mut ctx, "maps", "next", 1, &[iter]).expect("a value")
    };

    assert_eq!(interner.name_of(term).as_deref(), Some("none"));
}

#[test]
fn unicode_characters_to_binary_produces_error() {
    // ORACLE (erts-17.1): `unicode:characters_to_binary([16#D800])`
    // => `{error,<<>>,[55296]}` (a lone surrogate is unencodable) — the tag is
    // the produced atom `error`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut interner = Interner::seeded_like(&atoms);

    let term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let data = ctx.list(&[make_small(0xD800)]).expect("fits");
        call(&reg, &mut ctx, "unicode", "characters_to_binary", 1, &[data]).expect("a value")
    };

    let tag = tuple_element(term, 0).expect("the tag");
    assert_eq!(interner.name_of(tag).as_deref(), Some("error"));
}

// ---------------------------------------------------------------------------
// Negative control: no interner => the typed gap, never a fabricated atom
// ---------------------------------------------------------------------------

#[test]
fn without_an_interner_a_produced_atom_is_the_typed_gap() {
    // The same producers, but with **no** `AtomInterner` installed and a table
    // lacking the produced atom: the crate's documented fallback is the typed
    // `BeamError::TermNotFound`, never a fabricated index and never a wrong atom.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    assert!(matches!(
        call(&reg, &mut ctx, "io", "printable_range", 0, &[]),
        Err(BeamError::TermNotFound)
    ));

    let subject = make_binary(ctx.heap(), b"xyz").expect("fits");
    let pattern = make_binary(ctx.heap(), b"ab").expect("fits");
    assert!(matches!(
        call(&reg, &mut ctx, "binary", "match", 2, &[subject, pattern]),
        Err(BeamError::TermNotFound)
    ));

    let key = string_term(&mut ctx, "ZZ_NO_SUCH_ENV_ZZ");
    assert!(matches!(
        call(&reg, &mut ctx, "os", "getenv", 1, &[key]),
        Err(BeamError::TermNotFound)
    ));
}

// ---------------------------------------------------------------------------
// Live cross-check against the system `erl` (skips loudly when absent)
// ---------------------------------------------------------------------------

/// Runs `erl -noshell -eval <statement> -s init stop` and returns its stdout, or
/// `None` (after a loud `SKIP`) when no usable oracle is on `PATH`.
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

#[test]
fn live_oracle_agrees_on_the_produced_tag_names() {
    // One live cross-check against the system ERTS (OTP 29.1.1 / erts-17.1): every
    // produced tag the (a) cases above read back must also come out of the real
    // `erl` today. Skips loudly when the oracle is unavailable.
    let Some(output) = oracle_eval(
        "io:format(\"~p|~p|~p|~p|~p|~p|~p|~p|~p~n\",[\
             binary:match(<<\"xyz\">>, <<\"ab\">>),\
             io:printable_range(),\
             os:getenv(\"ZZ_NO_SUCH_ENV_ZZ\"),\
             os:putenv(\"ZZ_PRODUCED_ATOM_TEST\", \"1\"),\
             erlang:decode_packet(4, <<255,255,255,255>>, []),\
             lists:keysearch(2, 1, [{1,10},{2,20}]),\
             maps:find(9, #{1 => 2}),\
             maps:next(maps:iterator(#{})),\
             element(1, unicode:characters_to_binary([16#D800]))])",
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "nomatch|latin1|false|true|{error,invalid}|{value,{2,20}}|error|none|error",
        "the live oracle disagrees with the pinned produced-atom goldens"
    );
}
