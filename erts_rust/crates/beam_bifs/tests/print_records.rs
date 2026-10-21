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

//! Integration tests: **native-record printing** — `io:format`/`io_lib:format`
//! of the `RECORD_SUBTAG` term `records:create/4` makes, under `~w`, `~p`, `~W D`,
//! `~P D` and `io_lib:format/3`'s `{chars_limit, N}`.
//!
//! A native record is the one printable class whose rendering is not a leaf: both
//! the flat writer (`src/print/record.rs`'s `flat_record_chunk`, the
//! `io_lib:write1/4` shape) and the `io_lib_pretty` template model
//! (`src/print/pretty.rs`'s `build_record`/`record_fields_from`/`pp_record`/
//! `pp_field`/`rec_indent`) walk its fields, and the two depth accountings differ by
//! one (`~w`/`~W` spends 1 level, `~p`/`~P` spends 2). Every row below therefore
//! pins the **text** the oracle produces, not an invariant.
//!
//! # Route: the public registry
//!
//! The fixtures are built through the crate's own public surface —
//! `BifRegistry::call("records", "create", 4, …)` for the record and
//! `io`/`io_lib`'s `format` BIFs for the text — because the printer's entry points
//! (`render_chunk`, `RecordParts`, `Chunk`) are `pub(crate)` and the internal record
//! writers live in the private `access` module. An integration test can therefore
//! only reach the printer the way a runtime does, which is exactly the contract
//! being pinned (and it keeps the fixture's definition hash the real one
//! `records_create_4` computes — a `beam_codec::term::make_record` short cut would
//! have to reproduce that hash by hand).
//!
//! # Goldens
//!
//! Every expected string was measured on the system oracle (OTP 29.1.1 /
//! erts-17.1) with
//!
//! ```text
//! erl -noshell -eval 'Txt = io_lib:format(Fmt, Args[, Opts]),
//!                     io:format("~s", [lists:flatten(Txt)]), halt().'
//! ```
//!
//! and `native_records_match_the_live_oracle` re-derives the whole corpus live and
//! compares it with both the goldens and this crate's rendering. That test is
//! **fail-loud in the crate's live-oracle style**: with no usable `erl` it prints a
//! banner and returns (which is *not* a pass), and once the oracle runs, any
//! divergence panics with the transcript attached.
//!
//! `records:create/3` is `error:undef` and an options map without `is_exported` (or a
//! duplicate field key) is `error:badarg` on the oracle, so no fixture uses them.

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_codec::term::{make_big, make_binary, make_flatmap};
use beam_common::{BeamError, ExitReason, Result as BeamResult};
use beam_process::ProcessId;
use beam_term::{is_list, is_nil, signed_value, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// Scaffolding: the atom table, the context and the two format entry points
// ---------------------------------------------------------------------------

/// The atoms every fixture seeds.
///
/// `false` and `true` come **first** on purpose: a native-record definition stores
/// its `is_exported` flag as the frozen `beam_term::AM_FALSE`/`AM_TRUE` index
/// (`0`/`1`), so seeding them at those indices keeps the definition word identical
/// to the one the runtime builds (`src/records.rs`'s own tests do the same).
fn seeded_atoms() -> AtomTable {
    let mut names: Vec<String> = [
        "false",
        "true",
        "ok",
        "is_exported",
        "chars_limit",
        "recmod",
        "r",
        "empty",
        "outer",
        "other_mod",
        "longrec",
        "a",
        "b",
        "a b",
        "A B",
        "and",
        "X-y",
        "Weird Mod",
        "R R",
        "x",
        "tag",
        "k",
        "v",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    names.extend((1..=14).map(|i| format!("elem{i}")));
    names.extend((1..=12).map(|i| format!("f{i}")));
    let mut atoms = AtomTable::new();
    atoms.seed(names.iter().map(String::as_str));
    atoms
}

/// The heap every rendering gets (a record fixture is a few dozen words; the
/// `io_lib:format` result of the longest golden is ~110).
const HEAP_WORDS: usize = 1 << 20;

/// A flat proper list of the byte-sized integers of `text` (an Erlang string).
fn byte_list(ctx: &mut BifContext<'_>, text: &str) -> BeamResult<Eterm> {
    let elems: Vec<Eterm> = text
        .bytes()
        .map(|b| ctx.int(i64::from(b)))
        .collect::<BeamResult<Vec<Eterm>>>()?;
    ctx.list(&elems)
}

/// The byte values of a proper character list (`signed_value` of each element).
fn proper_bytes(term: Eterm) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        let head = beam_codec::term::cons_head(cur).expect("a live cons cell");
        let value = signed_value(head);
        assert!(
            (0..=255).contains(&value),
            "a character-list element out of byte range: {value}"
        );
        out.push(value as u8);
        cur = beam_codec::term::cons_tail(cur).expect("a live cons cell");
    }
    assert!(is_nil(cur), "not a proper list");
    out
}

// ---------------------------------------------------------------------------
// The record fixtures — one per shape the printer treats specially
// ---------------------------------------------------------------------------

/// A native-record fixture: an Erlang term built by `records:create/4`.
///
/// `erl_expr` is the **independent** oracle-side source of the same term, so the
/// live cross-check never derives its expectation from this crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fixture {
    /// F1 — `#recmod:r{a = 1,b = 2}` (the two-field reference record).
    R,
    /// F2 — a zero-field record (`records:create(recmod, empty, [], …)`).
    Empty,
    /// F3 — `a = "abc"` (a printable list: `~w` vs `~p` differ).
    FieldString,
    /// F3 — `a = 'a b'` (a quoted atom value).
    FieldSpacedAtom,
    /// F3 — `a = 'and'` (a reserved word, so quoted).
    FieldReservedAtom,
    /// F3 — `a = [1,2|3]` (an improper list).
    FieldImproper,
    /// F3 — `a = [1,[2,3],{x,4}]` (a mixed nested list).
    FieldNestedList,
    /// F3 — `a = 1.5` (a float).
    FieldFloat,
    /// F3 — `a = <<"xy">>` (`~w` vs `~p` differ).
    FieldBinary,
    /// F3 — `a = 2^64` (a bignum).
    FieldBig,
    /// F3 — `a = #{k => v}` (a one-pair map).
    FieldMap,
    /// F4 — `[R]`.
    ListOfR,
    /// F4 — `{tag,R}`.
    TupleTagR,
    /// F4 — `#{k => R}`.
    MapKR,
    /// F4 — `#recmod:outer{a = R}` (a record nested in a record).
    Outer,
    /// F5 — `#other_mod:r{a = 1}` built with `is_exported => true`.
    Other,
    /// F6 — the quoted field keys `'A B'`, `'and'`, `'X-y'`.
    QuotedKeys,
    /// F7 — a quoted module, record name and field key.
    WeirdNames,
    /// F8 — `#recmod:longrec{f1 = 1,…,f12 = 12}` (flat length 106).
    Long,
    /// F8 — `#recmod:r{a = [elem1,…,elem14]}` (a value that re-flows on its own).
    ElemList,
}

impl Fixture {
    /// The oracle-side Erlang source of this fixture. Self-contained: a dependent
    /// shape inlines the record it wraps, so the generated transcript has no
    /// binding-order dependency.
    fn erl_expr(self) -> String {
        match self {
            Fixture::R => rec_expr("recmod", "r", "[{a,1},{b,2}]", false),
            Fixture::Empty => rec_expr("recmod", "empty", "[]", false),
            Fixture::FieldString => rec_expr("recmod", "r", "[{a,\"abc\"}]", false),
            Fixture::FieldSpacedAtom => rec_expr("recmod", "r", "[{a,'a b'}]", false),
            Fixture::FieldReservedAtom => rec_expr("recmod", "r", "[{a,'and'}]", false),
            Fixture::FieldImproper => rec_expr("recmod", "r", "[{a,[1,2|3]}]", false),
            Fixture::FieldNestedList => rec_expr("recmod", "r", "[{a,[1,[2,3],{x,4}]}]", false),
            Fixture::FieldFloat => rec_expr("recmod", "r", "[{a,1.5}]", false),
            Fixture::FieldBinary => rec_expr("recmod", "r", "[{a,<<\"xy\">>}]", false),
            Fixture::FieldBig => rec_expr("recmod", "r", "[{a,18446744073709551616}]", false),
            Fixture::FieldMap => rec_expr("recmod", "r", "[{a,#{k=>v}}]", false),
            Fixture::ListOfR => format!("[{}]", Fixture::R.erl_expr()),
            Fixture::TupleTagR => format!("{{tag,{}}}", Fixture::R.erl_expr()),
            Fixture::MapKR => format!("#{{k=>{}}}", Fixture::R.erl_expr()),
            Fixture::Outer => rec_expr(
                "recmod",
                "outer",
                &format!("[{{a,{}}}]", Fixture::R.erl_expr()),
                false,
            ),
            Fixture::Other => rec_expr("other_mod", "r", "[{a,1}]", true),
            Fixture::QuotedKeys => {
                rec_expr("recmod", "r", "[{'A B',1},{'and',2},{'X-y',3}]", false)
            }
            Fixture::WeirdNames => rec_expr("'Weird Mod'", "'R R'", "[{'a b',1}]", false),
            Fixture::Long => {
                let fields: Vec<String> = (1..=12).map(|i| format!("{{f{i},{i}}}")).collect();
                rec_expr("recmod", "longrec", &format!("[{}]", fields.join(",")), false)
            }
            Fixture::ElemList => {
                let elems: Vec<String> = (1..=14).map(|i| format!("elem{i}")).collect();
                rec_expr("recmod", "r", &format!("[{{a,[{}]}}]", elems.join(",")), false)
            }
        }
    }

    /// Materializes this fixture on `ctx`'s heap through the public `records:create/4`.
    fn build(self, ctx: &mut BifContext<'_>, reg: &BifRegistry) -> BeamResult<Eterm> {
        match self {
            Fixture::R => {
                let a = int(ctx, 1)?;
                let b = int(ctx, 2)?;
                record(ctx, reg, "recmod", "r", &[("a", a), ("b", b)], false)
            }
            Fixture::Empty => record(ctx, reg, "recmod", "empty", &[], false),
            Fixture::FieldString => {
                let value = byte_list(ctx, "abc")?;
                one_field(ctx, reg, "recmod", "r", "a", value, false)
            }
            Fixture::FieldSpacedAtom => {
                let value = atom(ctx, "a b");
                one_field(ctx, reg, "recmod", "r", "a", value, false)
            }
            Fixture::FieldReservedAtom => {
                let value = atom(ctx, "and");
                one_field(ctx, reg, "recmod", "r", "a", value, false)
            }
            Fixture::FieldImproper => {
                let one = int(ctx, 1)?;
                let two = int(ctx, 2)?;
                let three = int(ctx, 3)?;
                let tail = ctx.cons(two, three)?;
                let improper = ctx.cons(one, tail)?;
                one_field(ctx, reg, "recmod", "r", "a", improper, false)
            }
            Fixture::FieldNestedList => {
                let one = int(ctx, 1)?;
                let two = int(ctx, 2)?;
                let three = int(ctx, 3)?;
                let four = int(ctx, 4)?;
                let x = atom(ctx, "x");
                let inner = ctx.list(&[two, three])?;
                let tuple = ctx.tuple(&[x, four])?;
                let list = ctx.list(&[one, inner, tuple])?;
                one_field(ctx, reg, "recmod", "r", "a", list, false)
            }
            Fixture::FieldFloat => {
                let value = ctx.make_float(1.5)?;
                one_field(ctx, reg, "recmod", "r", "a", value, false)
            }
            Fixture::FieldBinary => {
                let value = make_binary(ctx.heap(), b"xy")?;
                one_field(ctx, reg, "recmod", "r", "a", value, false)
            }
            Fixture::FieldBig => {
                // `2^64` — the smallest bignum a 64-bit `ErtsDigit` needs two limbs for.
                let value = make_big(ctx.heap(), false, &[0, 1])?;
                one_field(ctx, reg, "recmod", "r", "a", value, false)
            }
            Fixture::FieldMap => {
                let key = atom(ctx, "k");
                let value = atom(ctx, "v");
                let map = make_flatmap(ctx.heap(), &[(key, value)])?;
                one_field(ctx, reg, "recmod", "r", "a", map, false)
            }
            Fixture::ListOfR => {
                let rec = Fixture::R.build(ctx, reg)?;
                ctx.list(&[rec])
            }
            Fixture::TupleTagR => {
                let rec = Fixture::R.build(ctx, reg)?;
                let tag = atom(ctx, "tag");
                ctx.tuple(&[tag, rec])
            }
            Fixture::MapKR => {
                let rec = Fixture::R.build(ctx, reg)?;
                let key = atom(ctx, "k");
                make_flatmap(ctx.heap(), &[(key, rec)])
            }
            Fixture::Outer => {
                let rec = Fixture::R.build(ctx, reg)?;
                one_field(ctx, reg, "recmod", "outer", "a", rec, false)
            }
            Fixture::Other => {
                let value = int(ctx, 1)?;
                one_field(ctx, reg, "other_mod", "r", "a", value, true)
            }
            Fixture::QuotedKeys => {
                let first = int(ctx, 1)?;
                let second = int(ctx, 2)?;
                let third = int(ctx, 3)?;
                record(
                    ctx,
                    reg,
                    "recmod",
                    "r",
                    &[("A B", first), ("and", second), ("X-y", third)],
                    false,
                )
            }
            Fixture::WeirdNames => {
                let value = int(ctx, 1)?;
                one_field(ctx, reg, "Weird Mod", "R R", "a b", value, false)
            }
            Fixture::Long => {
                let mut fields = Vec::with_capacity(12);
                for i in 1..=12 {
                    fields.push((format!("f{i}"), int(ctx, i)?));
                }
                let refs: Vec<(&str, Eterm)> = fields
                    .iter()
                    .map(|(name, value)| (name.as_str(), *value))
                    .collect();
                record(ctx, reg, "recmod", "longrec", &refs, false)
            }
            Fixture::ElemList => {
                let mut elems = Vec::with_capacity(14);
                for i in 1..=14 {
                    elems.push(ctx.atom(&format!("elem{i}"))?);
                }
                let list = ctx.list(&elems)?;
                record(ctx, reg, "recmod", "r", &[("a", list)], false)
            }
        }
    }
}

/// `records:create(Module, Name, Fields, #{is_exported => Exported})` — the
/// oracle-side spelling of one fixture.
fn rec_expr(module: &str, name: &str, fields: &str, exported: bool) -> String {
    format!("records:create({module},{name},{fields},#{{is_exported=>{exported}}})")
}

/// A small integer on `ctx`'s heap.
fn int(ctx: &BifContext<'_>, value: i64) -> BeamResult<Eterm> {
    ctx.int(value)
}

/// A seeded atom on `ctx`'s heap.
fn atom(ctx: &BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("the atom is seeded")
}

/// `records:create/4` through the real registry.
fn record(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    module: &str,
    name: &str,
    fields: &[(&str, Eterm)],
    exported: bool,
) -> BeamResult<Eterm> {
    let module = ctx.atom(module)?;
    let name = ctx.atom(name)?;
    let mut pairs = Vec::with_capacity(fields.len());
    for (key, value) in fields {
        let key = ctx.atom(key)?;
        pairs.push(ctx.tuple(&[key, *value])?);
    }
    let fields = ctx.list(&pairs)?;
    let flag = ctx.atom(if exported { "true" } else { "false" })?;
    let tag = ctx.atom("is_exported")?;
    let opts = make_flatmap(ctx.heap(), &[(tag, flag)])?;
    reg.call("records", "create", 4, ctx, &[module, name, fields, opts])
}

/// `records:create/4` with exactly one field.
fn one_field(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    module: &str,
    name: &str,
    key: &str,
    value: Eterm,
    exported: bool,
) -> BeamResult<Eterm> {
    record(ctx, reg, module, name, &[(key, value)], exported)
}

// ---------------------------------------------------------------------------
// Rendering: the two format entry points
// ---------------------------------------------------------------------------

/// `io:format(Fmt, Args)` through a [`BytesSink`] — the `io` route.
fn render_io(fmt: &str, depth: Option<i64>, fixture: Fixture) -> BeamResult<String> {
    let reg = default_registry();
    let mut heap = Heap::new(HEAP_WORDS).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    let result = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let format = byte_list(&mut ctx, fmt)?;
        let mut args = vec![fixture.build(&mut ctx, &reg)?];
        if let Some(depth) = depth {
            args.push(int(&ctx, depth)?);
        }
        let args = ctx.list(&args)?;
        reg.call("io", "format", 2, &mut ctx, &[format, args])
    };
    result.map(|_| String::from_utf8(sink.contents().to_vec()).expect("the printer emits UTF-8"))
}

/// `io_lib:format(Fmt, Args[, [{chars_limit, N}]])` flattened to bytes — the
/// `io_lib` route (the only one that takes a `chars_limit`).
fn render_lib(
    fmt: &str,
    depth: Option<i64>,
    chars_limit: Option<i64>,
    fixture: Fixture,
) -> BeamResult<String> {
    let reg = default_registry();
    let mut heap = Heap::new(HEAP_WORDS).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let format = byte_list(&mut ctx, fmt)?;
    let mut args = vec![fixture.build(&mut ctx, &reg)?];
    if let Some(depth) = depth {
        args.push(int(&ctx, depth)?);
    }
    let args = ctx.list(&args)?;
    let deep = match chars_limit {
        None => reg.call("io_lib", "format", 2, &mut ctx, &[format, args])?,
        Some(limit) => {
            let tag = atom(&ctx, "chars_limit");
            let limit = int(&ctx, limit)?;
            let option = ctx.tuple(&[tag, limit])?;
            let options = ctx.list(&[option])?;
            reg.call(
                "io_lib",
                "format",
                3,
                &mut ctx,
                &[format, args, options],
            )?
        }
    };
    let flat = reg.call("lists", "flatten", 1, &mut ctx, &[deep])?;
    Ok(String::from_utf8(proper_bytes(flat)).expect("the printer emits UTF-8"))
}

/// The rendering of one corpus row through the route the objective names: a
/// `chars_limit` row must go through `io_lib:format/3`, everything else through
/// the `io:format/2` entry point.
fn render(row: &Row) -> BeamResult<String> {
    match row.chars_limit {
        Some(limit) => render_lib(row.fmt, row.depth, Some(limit), row.fixture),
        None => render_io(row.fmt, row.depth, row.fixture),
    }
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// One pinned rendering: the directive, the fixture and the exact oracle text.
struct Row {
    /// The `F<n>_…` name (the group prefix selects the test that owns the row).
    name: &'static str,
    fixture: Fixture,
    /// The directive, including its width (line length) and `.` precision (column).
    fmt: &'static str,
    /// The `D` of `~W D`/`~P D`.
    depth: Option<i64>,
    /// `io_lib:format/3`'s `{chars_limit, N}`.
    chars_limit: Option<i64>,
    /// The oracle's text.
    expected: &'static str,
}

/// `#recmod:longrec{…}` — the 12-field record's flat text (106 characters), which
/// is the `~w` rendering at every budget ≥ 106.
const LONG_FLAT: &str = "#recmod:longrec{f1 = 1,f2 = 2,f3 = 3,f4 = 4,f5 = 5,f6 = 6,f7 = 7,f8 = 8,f9 = 9,f10 = 10,f11 = 11,f12 = 12}";

/// The whole pinned corpus, in `F`-group order.
fn corpus() -> Vec<Row> {
    vec![
        // ---- F1: the two-field record at every depth ------------------------
        row("F1_w", Fixture::R, "~w", None, None, "#recmod:r{a = 1,b = 2}"),
        row("F1_p", Fixture::R, "~p", None, None, "#recmod:r{a = 1,b = 2}"),
        row("F1_W0", Fixture::R, "~W", Some(0), None, "..."),
        row("F1_P0", Fixture::R, "~P", Some(0), None, "..."),
        row("F1_W1", Fixture::R, "~W", Some(1), None, "#recmod:r{...}"),
        row("F1_P1", Fixture::R, "~P", Some(1), None, "#recmod:r{...}"),
        row("F1_P2", Fixture::R, "~P", Some(2), None, "#recmod:r{...}"),
        row(
            "F1_W2",
            Fixture::R,
            "~W",
            Some(2),
            None,
            "#recmod:r{a = 1,...}",
        ),
        row(
            "F1_P3",
            Fixture::R,
            "~P",
            Some(3),
            None,
            "#recmod:r{a = 1,...}",
        ),
        row("F1_W3", Fixture::R, "~W", Some(3), None, "#recmod:r{a = 1,b = 2}"),
        row("F1_P4", Fixture::R, "~P", Some(4), None, "#recmod:r{a = 1,b = 2}"),
        // ---- F2: a zero-field record --------------------------------------
        row("F2_w", Fixture::Empty, "~w", None, None, "#recmod:empty{}"),
        row("F2_p", Fixture::Empty, "~p", None, None, "#recmod:empty{}"),
        row("F2_W0", Fixture::Empty, "~W", Some(0), None, "..."),
        row("F2_W1", Fixture::Empty, "~W", Some(1), None, "#recmod:empty{}"),
        row("F2_P1", Fixture::Empty, "~P", Some(1), None, "#recmod:empty{}"),
        row("F2_P2", Fixture::Empty, "~P", Some(2), None, "#recmod:empty{}"),
        // ---- F3: one field, every leaf class and its depth sweeps ---------
        row(
            "F3_spaced_atom_w",
            Fixture::FieldSpacedAtom,
            "~w",
            None,
            None,
            "#recmod:r{a = 'a b'}",
        ),
        row(
            "F3_spaced_atom_p",
            Fixture::FieldSpacedAtom,
            "~p",
            None,
            None,
            "#recmod:r{a = 'a b'}",
        ),
        row(
            "F3_reserved_atom_w",
            Fixture::FieldReservedAtom,
            "~w",
            None,
            None,
            "#recmod:r{a = 'and'}",
        ),
        row(
            "F3_string_w",
            Fixture::FieldString,
            "~w",
            None,
            None,
            "#recmod:r{a = [97,98,99]}",
        ),
        row(
            "F3_string_p",
            Fixture::FieldString,
            "~p",
            None,
            None,
            "#recmod:r{a = \"abc\"}",
        ),
        row(
            "F3_improper_w",
            Fixture::FieldImproper,
            "~w",
            None,
            None,
            "#recmod:r{a = [1,2|3]}",
        ),
        row(
            "F3_nested_list_w",
            Fixture::FieldNestedList,
            "~w",
            None,
            None,
            "#recmod:r{a = [1,[2,3],{x,4}]}",
        ),
        row(
            "F3_float_w",
            Fixture::FieldFloat,
            "~w",
            None,
            None,
            "#recmod:r{a = 1.5}",
        ),
        row(
            "F3_binary_w",
            Fixture::FieldBinary,
            "~w",
            None,
            None,
            "#recmod:r{a = <<120,121>>}",
        ),
        row(
            "F3_binary_p",
            Fixture::FieldBinary,
            "~p",
            None,
            None,
            "#recmod:r{a = <<\"xy\">>}",
        ),
        row(
            "F3_big_w",
            Fixture::FieldBig,
            "~w",
            None,
            None,
            "#recmod:r{a = 18446744073709551616}",
        ),
        row(
            "F3_map_w",
            Fixture::FieldMap,
            "~w",
            None,
            None,
            "#recmod:r{a = #{k => v}}",
        ),
        row(
            "F3_string_W2",
            Fixture::FieldString,
            "~W",
            Some(2),
            None,
            "#recmod:r{a = [...]}",
        ),
        row(
            "F3_string_W3",
            Fixture::FieldString,
            "~W",
            Some(3),
            None,
            "#recmod:r{a = [97|...]}",
        ),
        row(
            "F3_string_P3",
            Fixture::FieldString,
            "~P",
            Some(3),
            None,
            "#recmod:r{a = [...]}",
        ),
        row(
            "F3_map_W2",
            Fixture::FieldMap,
            "~W",
            Some(2),
            None,
            "#recmod:r{a = #{}}",
        ),
        row(
            "F3_map_P3",
            Fixture::FieldMap,
            "~P",
            Some(3),
            None,
            "#recmod:r{a = #{...}}",
        ),
        // The single-pair map's trailing `,...` at `~W 3` is the oracle's own quirk.
        row(
            "F3_map_W3",
            Fixture::FieldMap,
            "~W",
            Some(3),
            None,
            "#recmod:r{a = #{k => v,...}}",
        ),
        row(
            "F3_map_P4",
            Fixture::FieldMap,
            "~P",
            Some(4),
            None,
            "#recmod:r{a = #{k => v}}",
        ),
        row(
            "F3_map_W4",
            Fixture::FieldMap,
            "~W",
            Some(4),
            None,
            "#recmod:r{a = #{k => v}}",
        ),
        // ---- F4: a record nested in a list / tuple / map / record ---------
        row(
            "F4_list_w",
            Fixture::ListOfR,
            "~w",
            None,
            None,
            "[#recmod:r{a = 1,b = 2}]",
        ),
        row(
            "F4_tuple_w",
            Fixture::TupleTagR,
            "~w",
            None,
            None,
            "{tag,#recmod:r{a = 1,b = 2}}",
        ),
        row(
            "F4_map_w",
            Fixture::MapKR,
            "~w",
            None,
            None,
            "#{k => #recmod:r{a = 1,b = 2}}",
        ),
        row(
            "F4_outer_w",
            Fixture::Outer,
            "~w",
            None,
            None,
            "#recmod:outer{a = #recmod:r{a = 1,b = 2}}",
        ),
        row(
            "F4_list_W2",
            Fixture::ListOfR,
            "~W",
            Some(2),
            None,
            "[#recmod:r{...}]",
        ),
        row(
            "F4_list_W3",
            Fixture::ListOfR,
            "~W",
            Some(3),
            None,
            "[#recmod:r{a = 1,...}]",
        ),
        row(
            "F4_tuple_W3",
            Fixture::TupleTagR,
            "~W",
            Some(3),
            None,
            "{tag,#recmod:r{...}}",
        ),
        row("F4_map_W1", Fixture::MapKR, "~W", Some(1), None, "#{}"),
        row("F4_map_P1", Fixture::MapKR, "~P", Some(1), None, "#{...}"),
        row(
            "F4_map_W2",
            Fixture::MapKR,
            "~W",
            Some(2),
            None,
            "#{k => #recmod:r{...},...}",
        ),
        row(
            "F4_outer_W2",
            Fixture::Outer,
            "~W",
            Some(2),
            None,
            "#recmod:outer{a = #recmod:r{...}}",
        ),
        row(
            "F4_outer_W3",
            Fixture::Outer,
            "~W",
            Some(3),
            None,
            "#recmod:outer{a = #recmod:r{a = 1,...}}",
        ),
        row(
            "F4_outer_P3",
            Fixture::Outer,
            "~P",
            Some(3),
            None,
            "#recmod:outer{a = #recmod:r{...}}",
        ),
        row(
            "F4_outer_P4",
            Fixture::Outer,
            "~P",
            Some(4),
            None,
            "#recmod:outer{a = #recmod:r{...}}",
        ),
        // ---- F5: a foreign, exported record ------------------------------
        row(
            "F5_w",
            Fixture::Other,
            "~w",
            None,
            None,
            "#other_mod:r{a = 1}",
        ),
        row(
            "F5_p",
            Fixture::Other,
            "~p",
            None,
            None,
            "#other_mod:r{a = 1}",
        ),
        row(
            "F5_W1",
            Fixture::Other,
            "~W",
            Some(1),
            None,
            "#other_mod:r{...}",
        ),
        row(
            "F5_P1",
            Fixture::Other,
            "~P",
            Some(1),
            None,
            "#other_mod:r{...}",
        ),
        row(
            "F5_P2",
            Fixture::Other,
            "~P",
            Some(2),
            None,
            "#other_mod:r{...}",
        ),
        // ---- F6: quoted field keys ---------------------------------------
        row(
            "F6_w",
            Fixture::QuotedKeys,
            "~w",
            None,
            None,
            "#recmod:r{'A B' = 1,'and' = 2,'X-y' = 3}",
        ),
        row(
            "F6_p",
            Fixture::QuotedKeys,
            "~p",
            None,
            None,
            "#recmod:r{'A B' = 1,'and' = 2,'X-y' = 3}",
        ),
        row(
            "F6_W2",
            Fixture::QuotedKeys,
            "~W",
            Some(2),
            None,
            "#recmod:r{'A B' = 1,...}",
        ),
        // ---- F7: a quoted module, name and field -------------------------
        row(
            "F7_w",
            Fixture::WeirdNames,
            "~w",
            None,
            None,
            "#'Weird Mod':'R R'{'a b' = 1}",
        ),
        row(
            "F7_p",
            Fixture::WeirdNames,
            "~p",
            None,
            None,
            "#'Weird Mod':'R R'{'a b' = 1}",
        ),
        // ---- F8: the 12-field record's re-flow ---------------------------
        // `~Lp` sets the line length; `~.Cp` sets the initial column.
        row(
            "F8_p",
            Fixture::Long,
            "~p",
            None,
            None,
            "#recmod:longrec{f1 = 1,f2 = 2,f3 = 3,f4 = 4,f5 = 5,f6 = 6,f7 = 7,f8 = 8,\n                f9 = 9,f10 = 10,f11 = 11,f12 = 12}",
        ),
        row(
            "F8_12p",
            Fixture::Long,
            "~12p",
            None,
            None,
            "#recmod:longrec{\n f1 = 1,\n f2 = 2,\n f3 = 3,\n f4 = 4,\n f5 = 5,\n f6 = 6,\n f7 = 7,\n f8 = 8,\n f9 = 9,\n f10 = 10,\n f11 = 11,\n f12 = 12}",
        ),
        row(
            "F8_16p",
            Fixture::Long,
            "~16p",
            None,
            None,
            "#recmod:longrec{\n    f1 = 1,\n    f2 = 2,\n    f3 = 3,\n    f4 = 4,\n    f5 = 5,\n    f6 = 6,\n    f7 = 7,\n    f8 = 8,\n    f9 = 9,\n    f10 = 10,\n    f11 = 11,\n    f12 = 12}",
        ),
        row(
            "F8_40p",
            Fixture::Long,
            "~40p",
            None,
            None,
            "#recmod:longrec{f1 = 1,f2 = 2,f3 = 3,\n                f4 = 4,f5 = 5,f6 = 6,\n                f7 = 7,f8 = 8,f9 = 9,\n                f10 = 10,f11 = 11,\n                f12 = 12}",
        ),
        row(
            "F8_20p",
            Fixture::Long,
            "~20p",
            None,
            None,
            "#recmod:longrec{\n    f1 = 1,f2 = 2,\n    f3 = 3,f4 = 4,\n    f5 = 5,f6 = 6,\n    f7 = 7,f8 = 8,\n    f9 = 9,\n    f10 = 10,\n    f11 = 11,\n    f12 = 12}",
        ),
        row(
            "F8_dot10p",
            Fixture::Long,
            "~.10p",
            None,
            None,
            "#recmod:longrec{f1 = 1,f2 = 2,f3 = 3,f4 = 4,f5 = 5,f6 = 6,f7 = 7,\n                         f8 = 8,f9 = 9,f10 = 10,f11 = 11,f12 = 12}",
        ),
        row(
            "F8_dot40p",
            Fixture::Long,
            "~.40p",
            None,
            None,
            "#recmod:longrec{\n                                        f1 = 1,f2 = 2,f3 = 3,f4 = 4,f5 = 5,\n                                        f6 = 6,f7 = 7,f8 = 8,f9 = 9,f10 = 10,\n                                        f11 = 11,f12 = 12}",
        ),
        row(
            "F8_10p",
            Fixture::Long,
            "~10p",
            None,
            None,
            "#recmod:longrec{\n f1 = 1,\n f2 = 2,\n f3 = 3,\n f4 = 4,\n f5 = 5,\n f6 = 6,\n f7 = 7,\n f8 = 8,\n f9 = 9,\n f10 =\n  10,\n f11 =\n  11,\n f12 =\n  12}",
        ),
        row(
            "F8_8p",
            Fixture::Long,
            "~8p",
            None,
            None,
            "#recmod:longrec{\n f1 =\n  1,\n f2 =\n  2,\n f3 =\n  3,\n f4 =\n  4,\n f5 =\n  5,\n f6 =\n  6,\n f7 =\n  7,\n f8 =\n  8,\n f9 =\n  9,\n f10 =\n  10,\n f11 =\n  11,\n f12 =\n  12}",
        ),
        row("F8_0p", Fixture::Long, "~0p", None, None, LONG_FLAT),
        // A long *value* re-flows under its own `[`, at the tagged-tuple indent.
        row(
            "F8_elem_p",
            Fixture::ElemList,
            "~p",
            None,
            None,
            "#recmod:r{a = [elem1,elem2,elem3,elem4,elem5,elem6,elem7,elem8,elem9,elem10,\n               elem11,elem12,elem13,elem14]}",
        ),
        // ---- F9: `{chars_limit, N}` on the two-field record, both writers --
        row("F9_p_0", Fixture::R, "~p", None, Some(0), "..."),
        row("F9_p_1", Fixture::R, "~p", None, Some(1), "#recmod:r{...}"),
        row("F9_p_4", Fixture::R, "~p", None, Some(4), "#recmod:r{...}"),
        row("F9_p_10", Fixture::R, "~p", None, Some(10), "#recmod:r{...}"),
        row("F9_p_13", Fixture::R, "~p", None, Some(13), "#recmod:r{...}"),
        row("F9_p_14", Fixture::R, "~p", None, Some(14), "#recmod:r{...}"),
        row(
            "F9_p_15",
            Fixture::R,
            "~p",
            None,
            Some(15),
            "#recmod:r{a = 1,...}",
        ),
        row(
            "F9_p_16",
            Fixture::R,
            "~p",
            None,
            Some(16),
            "#recmod:r{a = 1,...}",
        ),
        row(
            "F9_p_17",
            Fixture::R,
            "~p",
            None,
            Some(17),
            "#recmod:r{a = 1,b = 2}",
        ),
        row("F9_w_0", Fixture::R, "~w", None, Some(0), "..."),
        row("F9_w_1", Fixture::R, "~w", None, Some(1), "#recmod:r{...}"),
        row("F9_w_4", Fixture::R, "~w", None, Some(4), "#recmod:r{...}"),
        row("F9_w_10", Fixture::R, "~w", None, Some(10), "#recmod:r{...}"),
        row("F9_w_13", Fixture::R, "~w", None, Some(13), "#recmod:r{...}"),
        row("F9_w_14", Fixture::R, "~w", None, Some(14), "#recmod:r{...}"),
        row(
            "F9_w_15",
            Fixture::R,
            "~w",
            None,
            Some(15),
            "#recmod:r{a = 1,...}",
        ),
        row(
            "F9_w_16",
            Fixture::R,
            "~w",
            None,
            Some(16),
            "#recmod:r{a = 1,...}",
        ),
        row(
            "F9_w_17",
            Fixture::R,
            "~w",
            None,
            Some(17),
            "#recmod:r{a = 1,b = 2}",
        ),
        // ---- F10: `~w` never breaks a line --------------------------------
        row(
            "F10_w_40",
            Fixture::Long,
            "~w",
            None,
            Some(40),
            "#recmod:longrec{f1 = 1,f2 = 2,f3 = 3,...}",
        ),
        row("F10_w_106", Fixture::Long, "~w", None, Some(106), LONG_FLAT),
        row("F10_w_150", Fixture::Long, "~w", None, Some(150), LONG_FLAT),
        row("F10_w_500", Fixture::Long, "~w", None, Some(500), LONG_FLAT),
        row("F10_w_plain", Fixture::Long, "~w", None, None, LONG_FLAT),
        // The contrast: `~p` at the same budget does break.
        row(
            "F10_p_106",
            Fixture::Long,
            "~p",
            None,
            Some(106),
            "#recmod:longrec{f1 = 1,f2 = 2,f3 = 3,f4 = 4,f5 = 5,f6 = 6,f7 = 7,f8 = 8,\n                f9 = 9,f10 = 10,f11 = 11,f12 = 12}",
        ),
    ]
}

/// Builds a corpus row (a small helper so the table above stays one line per row).
const fn row(
    name: &'static str,
    fixture: Fixture,
    fmt: &'static str,
    depth: Option<i64>,
    chars_limit: Option<i64>,
    expected: &'static str,
) -> Row {
    Row {
        name,
        fixture,
        fmt,
        depth,
        chars_limit,
        expected,
    }
}

/// Asserts every corpus row whose name starts with `group` renders the pinned text.
#[track_caller]
fn check_group(group: &str) {
    let rows = corpus();
    let selected: Vec<&Row> = rows.iter().filter(|r| r.name.starts_with(group)).collect();
    assert!(
        !selected.is_empty(),
        "no corpus row belongs to group {group:?}"
    );
    for row in selected {
        let got = render(row).unwrap_or_else(|e| {
            panic!(
                "{}: io formatting a native record failed: {e:?}",
                row.name
            )
        });
        assert_eq!(
            got, row.expected,
            "{} ({} at depth {:?}, chars_limit {:?}) does not match the oracle",
            row.name, row.fmt, row.depth, row.chars_limit
        );
    }
}

// ---------------------------------------------------------------------------
// F1..F10 — one test per surface
// ---------------------------------------------------------------------------

/// F1: the two-field record's text at every depth. A record costs **1** level under
/// `~w`/`~W` and **2** under `~p`/`~P`, so `~W 2` and `~P 3` are the same shape.
#[test]
fn f1_a_two_field_record_at_every_depth() {
    check_group("F1_");
}

/// F2: a zero-field record is `#recmod:empty{}` at every positive depth — the empty
/// field clause precedes the depth-`1` `"..."` clause — and `...` only at depth `0`.
#[test]
fn f2_a_zero_field_record_keeps_its_braces_at_every_depth() {
    check_group("F2_");
}

/// F3: one field, every leaf class. `~w` renders a string as a list of integers and a
/// binary as bytes; `~p` renders them printable. Quoted atoms, an improper list, a
/// nested list, a float, a bignum and a map all follow the leaf rules; a depth-limited
/// field's value re-flows on its own (the single-pair map's `~W 3` trailing `,...`
/// included).
#[test]
fn f3_one_field_of_every_printable_leaf_class() {
    check_group("F3_");
}

/// F4: a record nested in a list, a tuple, a map and another record, at the depths
/// where the nesting shows (the map's `~W 1` is `#{}` while `~P 1` is `#{...}`).
#[test]
fn f4_a_record_nested_in_every_container() {
    check_group("F4_");
}

/// F5: `is_exported => true` never changes the text — a foreign record prints
/// `#other_mod:r{a = 1}`, exactly as the non-exported spelling does.
#[test]
fn f5_a_foreign_exported_record_renders_identically() {
    check_group("F5_");
    // The flag is invisible: the exported and non-exported shapes of the same
    // definition print the same text.
    let exported = render_io("~w", None, Fixture::Other).expect("renders");
    let plain = render_io("~w", None, Fixture::FieldString).expect("renders");
    assert!(exported.starts_with("#other_mod:r{"), "{exported}");
    assert!(plain.starts_with("#recmod:r{"), "{plain}");
}

/// F6: field keys that need quoting (`'A B'`, the reserved word `'and'`, `'X-y'`)
/// are quoted by both writers, and truncation elides from the second field on.
#[test]
fn f6_quoted_field_keys() {
    check_group("F6_");
}

/// F7: a module, a record name and a field key that all need quoting.
#[test]
fn f7_quoted_module_name_and_field() {
    check_group("F7_");
}

/// F8: the 12-field record's re-flow. A directive width sets the line length
/// (`~12p`/`~16p`/`~20p`/`~40p`) and a `.` precision sets the initial column
/// (`~.10p`/`~.40p`); a value too wide for the remaining budget breaks after its
/// `" ="` separator; `~0p` is the oracle's flat `Ll =:= 0` clause. A long *value*
/// re-flows under its own `[` (indent 15), not under the record's tagged-tuple indent.
#[test]
fn f8_the_twelve_field_record_reflows() {
    check_group("F8_");
}

/// F9: `{chars_limit, N}` on the two-field record, through `io_lib:format/3`, for
/// **both** writers. The braces and the `#recmod:r` prefix are charged first, so the
/// first field appears only at `N = 15`.
#[test]
fn f9_chars_limit_on_both_writers() {
    check_group("F9_");
}

/// F10: `~w` never inserts a newline, at any `chars_limit` — the flat writer is
/// line-break free. The same budget under `~p` does break (the contrast row).
#[test]
fn f10_the_flat_writer_never_breaks_a_line() {
    check_group("F10_");
    let rows = corpus();
    for row in rows.iter().filter(|r| r.name.starts_with("F10_")) {
        let text = render(row).expect("renders");
        if row.fmt == "~p" {
            assert!(
                text.contains('\n'),
                "{}: `~p` at this budget must break, got {text:?}",
                row.name
            );
        } else {
            assert!(
                !text.contains('\n'),
                "{}: `~w` must never break a line, got {text:?}",
                row.name
            );
        }
    }
}

/// Both entry points agree: `io:format/2` (the sink route) and
/// `io_lib:format/2` flattened produce the same text for every no-`chars_limit` row.
#[test]
fn io_and_io_lib_agree_on_every_row() {
    let mut compared = 0usize;
    for row in corpus().iter().filter(|r| r.chars_limit.is_none()) {
        let io = render_io(row.fmt, row.depth, row.fixture)
            .unwrap_or_else(|e| panic!("{}: io:format failed: {e:?}", row.name));
        let lib = render_lib(row.fmt, row.depth, None, row.fixture)
            .unwrap_or_else(|e| panic!("{}: io_lib:format failed: {e:?}", row.name));
        assert_eq!(io, lib, "{}: io vs io_lib", row.name);
        compared += 1;
    }
    assert!(compared >= 70, "the corpus shrank to {compared} rows");
}

// ---------------------------------------------------------------------------
// The "no bare BadArg" property
// ---------------------------------------------------------------------------

/// Whether `err` is the crate's documented typed capability gap
/// (`error:unsupported_bif`) rather than an argument error.
fn is_typed_unsupported(err: &BeamError) -> bool {
    matches!(err, BeamError::Raise(raise) if raise.reason == ExitReason::atom("unsupported_bif"))
}

/// **No shape reaches a bare `BadArg`.** A native record is a well-formed term, so a
/// printer that reports `BadArg` for one is inventing an argument error: the only
/// acceptable outcomes are a rendering or the documented typed `unsupported_bif` gap.
///
/// The matrix sweeps every fixture at every depth `0..=5` under both depth-limited
/// writers, at every `chars_limit` in `0,1,2,3,5,10,14,15,16,17,40,106` under both
/// writers, and with no limit at all.
#[test]
fn no_native_record_shape_reaches_a_bare_badarg() {
    const FIXTURES: [Fixture; 20] = [
        Fixture::R,
        Fixture::Empty,
        Fixture::FieldString,
        Fixture::FieldSpacedAtom,
        Fixture::FieldReservedAtom,
        Fixture::FieldImproper,
        Fixture::FieldNestedList,
        Fixture::FieldFloat,
        Fixture::FieldBinary,
        Fixture::FieldBig,
        Fixture::FieldMap,
        Fixture::ListOfR,
        Fixture::TupleTagR,
        Fixture::MapKR,
        Fixture::Outer,
        Fixture::Other,
        Fixture::QuotedKeys,
        Fixture::WeirdNames,
        Fixture::Long,
        Fixture::ElemList,
    ];
    const LIMITS: [i64; 12] = [0, 1, 2, 3, 5, 10, 14, 15, 16, 17, 40, 106];

    let mut checked = 0usize;
    let mut rendered = 0usize;
    let mut unsupported = 0usize;
    for fixture in FIXTURES {
        // The unlimited writers.
        for fmt in ["~w", "~p"] {
            let row = Row {
                name: "matrix",
                fixture,
                fmt,
                depth: None,
                chars_limit: None,
                expected: "",
            };
            match render(&row) {
                Ok(_) => rendered += 1,
                Err(ref e) if is_typed_unsupported(e) => unsupported += 1,
                Err(e) => panic!(
                    "{fixture:?} {fmt} (unlimited): expected a rendering or the typed \
                     unsupported_bif gap, got {e:?}"
                ),
            }
            checked += 1;
        }
        // The depth-limited writers.
        for fmt in ["~W", "~P"] {
            for depth in 0..=5 {
                let row = Row {
                    name: "matrix",
                    fixture,
                    fmt,
                    depth: Some(depth),
                    chars_limit: None,
                    expected: "",
                };
                match render(&row) {
                    Ok(_) => rendered += 1,
                    Err(ref e) if is_typed_unsupported(e) => unsupported += 1,
                    Err(e) => panic!(
                        "{fixture:?} {fmt} {depth}: expected a rendering or the typed \
                         unsupported_bif gap, got {e:?}"
                    ),
                }
                checked += 1;
            }
        }
        // The chars-limited writers.
        for fmt in ["~w", "~p"] {
            for limit in LIMITS {
                let row = Row {
                    name: "matrix",
                    fixture,
                    fmt,
                    depth: None,
                    chars_limit: Some(limit),
                    expected: "",
                };
                match render(&row) {
                    Ok(_) => rendered += 1,
                    Err(ref e) if is_typed_unsupported(e) => unsupported += 1,
                    Err(e) => panic!(
                        "{fixture:?} {fmt} chars_limit {limit}: expected a rendering or the \
                         typed unsupported_bif gap, got {e:?}"
                    ),
                }
                checked += 1;
            }
        }
    }
    assert_eq!(checked, FIXTURES.len() * (2 + 12 + 24), "the matrix changed size");
    assert!(
        rendered > checked / 2,
        "{rendered} of {checked} shapes rendered; {unsupported} answered the typed gap"
    );
}

// ---------------------------------------------------------------------------
// The live-oracle cross-check
// ---------------------------------------------------------------------------

/// Whether `path` is an executable file (`$ERTS_ORACLE` may point at one).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `name` in the first `$PATH` directory that holds an executable of that name.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The system `erl` — `$ERTS_ORACLE` when it names an executable, else `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    match std::env::var_os("ERTS_ORACLE") {
        Some(value) => {
            let program = PathBuf::from(&value);
            if is_executable(&program) {
                Some(program)
            } else {
                eprintln!(
                    "[beam_bifs print_records] $ERTS_ORACLE='{}' is not an existing \
                     executable — treating it as no oracle.",
                    program.display()
                );
                None
            }
        }
        None => on_path("erl"),
    }
}

/// Decodes a lowercase/uppercase hex string (`binary:encode_hex/1`'s output).
fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Builds and runs the oracle-side transcript: one `erl -noshell -eval` invocation
/// that constructs every fixture with the **real** `records:create/4` and prints each
/// row as `name<TAB>hex`.
fn oracle_transcript(erl: &Path, rows: &[Row]) -> Result<Vec<(String, String)>, String> {
    // Assign a variable per distinct fixture, in first-use order.
    let mut fixtures: Vec<Fixture> = Vec::new();
    for row in rows {
        if !fixtures.contains(&row.fixture) {
            fixtures.push(row.fixture);
        }
    }
    let var = |f: Fixture| -> String {
        let index = fixtures
            .iter()
            .position(|candidate| *candidate == f)
            .expect("the fixture was collected");
        format!("T{index}")
    };

    let mut body = String::new();
    for fixture in &fixtures {
        body.push_str(&format!("{} = {}, ", var(*fixture), fixture.erl_expr()));
    }
    body.push_str(
        "P = fun(N, T) -> io:format(\"~s\\t~s~n\", \
         [N, binary:encode_hex(iolist_to_binary(lists:flatten(T)))]) end, ",
    );
    for row in rows {
        let args = match row.depth {
            Some(depth) => format!("[{}, {}]", var(row.fixture), depth),
            None => format!("[{}]", var(row.fixture)),
        };
        let call = match row.chars_limit {
            Some(limit) => format!(
                "io_lib:format(\"{}\", {}, [{{chars_limit,{}}}])",
                row.fmt, args, limit
            ),
            None => format!("io_lib:format(\"{}\", {})", row.fmt, args),
        };
        body.push_str(&format!("P(\"{}\", {}), ", row.name, call));
    }
    body.push_str("halt()");

    let output = Command::new(erl)
        .args(["-noshell", "-eval", &body])
        .output()
        .map_err(|e| format!("could not start {}: {e}", erl.display()))?;
    if !output.status.success() {
        return Err(format!(
            "the oracle `erl` failed ({}); stderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut transcript = Vec::with_capacity(rows.len());
    for line in stdout.lines() {
        let Some((name, hex)) = line.split_once('\t') else {
            return Err(format!("unparsable oracle line: {line:?}"));
        };
        let bytes = hex_decode(hex).ok_or_else(|| format!("unparsable oracle hex: {hex:?}"))?;
        let text = String::from_utf8(bytes)
            .map_err(|e| format!("the oracle emitted non-UTF-8 text: {e}"))?;
        transcript.push((name.to_string(), text));
    }
    Ok(transcript)
}

/// **The live-oracle cross-check.** Every corpus row is rendered by the system
/// oracle (OTP 29.1.1 / erts-17.1) and by this crate, and both are compared with the
/// pinned golden — so the test never asserts a value against itself.
///
/// With no usable `erl` it prints a loud banner and returns, which is **not** a pass
/// (the crate's convention, `tests/zlib_stream.rs` / `tests/fun_info_oracle.rs`).
#[test]
fn native_records_match_the_live_oracle() {
    let rows = corpus();
    let Some(erl) = resolve_oracle() else {
        eprintln!(
            "\n[beam_bifs print_records] SKIPPED: no `erl` oracle on PATH (and no usable \
             $ERTS_ORACLE) — the live cross-check did NOT run. This is not a pass; the \
             committed goldens in this file are the no-oracle evidence.\n"
        );
        return;
    };
    eprintln!(
        "[beam_bifs print_records] cross-checking {} native-record renderings against {}",
        rows.len(),
        erl.display()
    );
    let transcript = match oracle_transcript(&erl, &rows) {
        Ok(transcript) => transcript,
        Err(message) => panic!("[beam_bifs print_records] oracle failure: {message}"),
    };
    assert_eq!(
        transcript.len(),
        rows.len(),
        "the oracle printed {} rows for {} corpus rows",
        transcript.len(),
        rows.len()
    );
    let by_name: std::collections::HashMap<&str, &str> = transcript
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_str()))
        .collect();

    let mut compared = 0usize;
    let mut divergences = Vec::new();
    for row in &rows {
        let Some(oracle) = by_name.get(row.name) else {
            divergences.push(format!("{}: the oracle printed no such row", row.name));
            continue;
        };
        let ours = match render(row) {
            Ok(text) => text,
            Err(e) => {
                divergences.push(format!("{}: our rendering failed: {e:?}", row.name));
                continue;
            }
        };
        compared += 1;
        if ours != *oracle {
            divergences.push(format!(
                "{} ({} depth {:?} chars_limit {:?}):\n  oracle: {:?}\n  ours:   {:?}",
                row.name, row.fmt, row.depth, row.chars_limit, oracle, ours
            ));
        } else if ours != row.expected {
            divergences.push(format!(
                "{}: the oracle agrees with ours but the pinned golden is stale:\n  \
                 golden: {:?}\n  both:   {:?}",
                row.name, row.expected, ours
            ));
        }
    }
    assert!(
        divergences.is_empty(),
        "[beam_bifs print_records] {} of {} rows disagree with the live oracle:\n{}",
        divergences.len(),
        compared,
        divergences.join("\n")
    );
    assert_eq!(compared, rows.len(), "some rows were not compared");
    eprintln!(
        "[beam_bifs print_records] {compared} rows agree with the live oracle"
    );
}

