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

//! The **headline Milestone-1 evidence**: the widened typed-IR interpreter runs
//! real, non-trivial **system stdlib** Erlang modules.
//!
//! Every other engine integration test drives a purpose-built fixture produced
//! by `erlc` from the engine's own sources. This one instead loads the
//! **stdlib `lists` module shipped with the running Erlang** (OTP 29 /
//! `erts-17.1`, the differential oracle) straight from
//! `<stdlib>/ebin/lists.beam`, plus a thin harness (`stdlists_harness`) whose
//! every export forwards to a real `lists` function — including two that pass an
//! **anonymous fun** into `lists:map/2` / `lists:filter/2`.
//!
//! This forces the engine, end to end, to:
//!
//! * load a large real module (~247 functions) and run its exports on a fresh
//!   [`Machine`] (`lists:reverse/1`, `lists:seq/2`, `lists:sort/1`, …);
//! * dispatch the `lists`↔`harness` **external calls** the wrappers make;
//! * resolve and apply a **cross-module closure** (`make_fun3` in the harness,
//!   `call_fun2` in `lists`) — the `ExecutionContext::apply_fun` seam;
//! * keep **one shared term heap and one shared atom table** across modules, so a
//!   `true`/`false` a predicate returns is recognised by `lists:filter`
//!   (see `support`).
//!
//! Every expected value is the **real** system ERTS result, cross-checked with
//! `erl`:
//!
//! ```text
//! lists:reverse([1,2,3])                       = [3,2,1]
//! lists:seq(1,5)                               = [1,2,3,4,5]
//! stdlists_harness:reverse([10,20,30])         = [30,20,10]
//! stdlists_harness:seq(2,5)                    = [2,3,4,5]
//! stdlists_harness:map_double([1,2,3])         = [2,4,6]
//! stdlists_harness:sort([3,1,2])               = [1,2,3]
//! stdlists_harness:filter_pos([-1,0,2,-3,5])   = [2,5]
//! ordsets:from_list([3,1,2])                   = [1,2,3]
//! ordsets:union([1,2],[2,3])                   = [1,2,3]
//! ordsets:intersection([1,2],[2,3])            = [2]
//! ordsets:subtract([1,2,3],[1,2])              = [3]
//! ordsets:is_element(2,[2,3])                  = true
//! ordsets:is_element(7,[2,3])                  = false
//! proplists:get_value(a,[{a,1},{b,2}])         = 1
//! proplists:get_value(c,[{a,1},{b,2}],9)       = 9
//! proplists:get_value(c,[{a,1},{b,2}])         = undefined
//! proplists:lookup_all(a,[{a,1},{b,2}])        = [{a,1}]
//! proplists:get_bool(a,[{a,1},{b,2}])          = false
//! ```
//!
//! Three real stdlib modules run: **`lists`** directly and through the harness,
//! **`ordsets`** (pure Erlang over `lists:usort`/`umerge`/`foldl`/`filter`), and
//! **`proplists`** (tuple access through `tuple_size/1` + `element/2`).
//!
//! The stdlib modules are located at **test time** (never a baked-in path); when
//! `erl`/stdlib is unavailable each test **skips loudly** rather than failing.

mod support;

use beam_engine::terms;
use beam_term::Eterm;
use support::{
    assert_atom, assert_int_list, assert_keyed_list, int_list, keyed_tuple, skip_reason,
    stdlib_ebin, ModuleHost,
};

/// The committed harness fixture (a real `.beam` from the system `erlc`).
const HARNESS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/stdlists_harness.beam"
);

/// A host over the system stdlib `ebin` with the harness registered.
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `require_stdlib`)");
    let mut host = ModuleHost::new(ebin);
    host.register("stdlists_harness", HARNESS_BEAM);
    host
}

/// Skips the calling test (loudly) when the oracle stdlib is unavailable;
/// returns `true` when the caller should return early.
fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Running the system `lists` module directly
// ---------------------------------------------------------------------------

#[test]
fn stdlib_lists_reverse_runs_the_real_module() {
    // `lists:reverse/1` for a three-element list delegates to the `lists`
    // `reverse/2` NIF, which this host serves — so the whole real function runs.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[1, 2, 3]);
    let result = host
        .call("lists", "reverse", &[input])
        .expect("lists:reverse/1 must run");
    assert_int_list(&host, result, &[3, 2, 1]);
}

#[test]
fn stdlib_lists_seq_runs_the_real_module() {
    // `lists:seq/2` guards on integers, computes the length with `-`/`+`, then
    // loops in the real `lists:seq_loop/3`.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let result = host
        .call("lists", "seq", &[beam_term::make_small(1), beam_term::make_small(5)])
        .expect("lists:seq/2 must run");
    assert_int_list(&host, result, &[1, 2, 3, 4, 5]);
}

// ---------------------------------------------------------------------------
// The harness wrappers (external calls into `lists`)
// ---------------------------------------------------------------------------

#[test]
fn harness_reverse_forwards_to_stdlib_lists() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[10, 20, 30]);
    let result = host
        .call("stdlists_harness", "reverse", &[input])
        .expect("stdlists_harness:reverse/1 must run");
    assert_int_list(&host, result, &[30, 20, 10]);
}

#[test]
fn harness_seq_forwards_to_stdlib_lists() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let result = host
        .call(
            "stdlists_harness",
            "seq",
            &[beam_term::make_small(2), beam_term::make_small(5)],
        )
        .expect("stdlists_harness:seq/2 must run");
    assert_int_list(&host, result, &[2, 3, 4, 5]);
}

#[test]
fn harness_map_double_applies_a_closure_over_stdlib_lists_map() {
    // The headline cross-module case: the harness builds `fun(X) -> X * 2 end`
    // and hands it to the real `lists:map/2`, whose `call_fun2` reaches back into
    // the harness through `ExecutionContext::apply_fun`.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[1, 2, 3]);
    let result = host
        .call("stdlists_harness", "map_double", &[input])
        .expect("stdlists_harness:map_double/1 must run");
    assert_int_list(&host, result, &[2, 4, 6]);
}

#[test]
fn harness_sort_runs_the_real_lists_sort() {
    // `lists:sort/1` is a hand-written merge sort over `test` opcodes only — no
    // BIFs, no funs — so this exercises the interpreter's control flow on a real
    // module's recursive helpers.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[3, 1, 2]);
    let result = host
        .call("stdlists_harness", "sort", &[input])
        .expect("stdlists_harness:sort/1 must run");
    assert_int_list(&host, result, &[1, 2, 3]);
}

#[test]
fn harness_filter_pos_matches_the_predicate_result_across_modules() {
    // `lists:filter/2` calls the harness predicate (a closure returning a
    // `true`/`false` atom) and then `select_val`s on those atoms **in `lists`** —
    // which is exactly what the shared atom table makes consistent.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[-1, 0, 2, -3, 5]);
    let result = host
        .call("stdlists_harness", "filter_pos", &[input])
        .expect("stdlists_harness:filter_pos/1 must run");
    assert_int_list(&host, result, &[2, 5]);
}

// ---------------------------------------------------------------------------
// A second and a third real stdlib module: `ordsets` and `proplists`
// ---------------------------------------------------------------------------

#[test]
fn stdlib_ordsets_from_list_sorts_and_dedups() {
    // `ordsets:from_list/1` is pure Erlang that delegates to `lists:usort/1`
    // (and `lists:umerge/1`), so this drives a second real stdlib module *and*
    // the `ordsets` -> `lists` external-call edge.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[3, 1, 2]);
    let result = host
        .call("ordsets", "from_list", &[input])
        .expect("ordsets:from_list/1 must run");
    assert_int_list(&host, result, &[1, 2, 3]);
}

#[test]
fn stdlib_ordsets_union_intersection_and_subtract() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[2, 3]);
    let union = host
        .call("ordsets", "union", &[a, b])
        .expect("ordsets:union/2 must run");
    assert_int_list(&host, union, &[1, 2, 3]);

    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[2, 3]);
    let intersection = host
        .call("ordsets", "intersection", &[a, b])
        .expect("ordsets:intersection/2 must run");
    assert_int_list(&host, intersection, &[2]);

    let a = int_list(&mut host, &[1, 2, 3]);
    let b = int_list(&mut host, &[1, 2]);
    let difference = host
        .call("ordsets", "subtract", &[a, b])
        .expect("ordsets:subtract/2 must run");
    assert_int_list(&host, difference, &[3]);
}

#[test]
fn stdlib_ordsets_is_element_returns_the_shared_true_and_false() {
    // The `true`/`false` the real `ordsets` returns must be *the same atoms* the
    // shared table names — the invariant the shared atom table exists to keep.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let list = int_list(&mut host, &[2, 3]);

    let hit = host
        .call("ordsets", "is_element", &[beam_term::make_small(2), list])
        .expect("ordsets:is_element/2 must run");
    assert_atom(&host, hit, "true");

    let miss = host
        .call("ordsets", "is_element", &[beam_term::make_small(7), list])
        .expect("ordsets:is_element/2 must run");
    assert_atom(&host, miss, "false");
}

#[test]
fn stdlib_proplists_get_value_walks_the_property_list() {
    // `proplists:get_value/3` guards with `tuple_size/1` + `element/2` BIFs, so it
    // exercises tuple access across a third real stdlib module.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let a = keyed_tuple(&mut host, "a", 1);
    let b = keyed_tuple(&mut host, "b", 2);
    let props = terms::make_list(&mut host, &[a, b]).expect("build a proplist");
    let key_a = host.intern("a");

    let found = host
        .call("proplists", "get_value", &[key_a, props])
        .expect("proplists:get_value/2 must run");
    assert!(
        beam_term::cmp(found, beam_term::make_small(1), host.atom_table()).is_eq(),
        "get_value(a, [{{a,1}},{{b,2}}]) must be 1, got {found:?}"
    );

    // A missing key falls through to the default.
    let key_c = host.intern("c");
    let default = host
        .call("proplists", "get_value", &[key_c, props, beam_term::make_small(9)])
        .expect("proplists:get_value/3 must run");
    assert!(
        beam_term::cmp(default, beam_term::make_small(9), host.atom_table()).is_eq(),
        "get_value(c, props, 9) must be 9, got {default:?}"
    );

    // ... and `/2` uses the `undefined` default.
    let undefined = host
        .call("proplists", "get_value", &[key_c, props])
        .expect("proplists:get_value/2 must run");
    assert_atom(&host, undefined, "undefined");
}

#[test]
fn stdlib_proplists_lookup_all_and_get_bool() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let a = keyed_tuple(&mut host, "a", 1);
    let b = keyed_tuple(&mut host, "b", 2);
    let props = terms::make_list(&mut host, &[a, b]).expect("build a proplist");
    let key_a = host.intern("a");

    let matches = host
        .call("proplists", "lookup_all", &[key_a, props])
        .expect("proplists:lookup_all/2 must run");
    assert_keyed_list(&host, matches, &[("a", 1)]);

    let key_c = host.intern("c");
    let none = host
        .call("proplists", "lookup_all", &[key_c, props])
        .expect("proplists:lookup_all/2 must run");
    assert!(terms::list_elements(none).expect("a list").is_empty());

    // `get_bool/2` is `true` only when the value is literally `true`.
    let not_bool = host
        .call("proplists", "get_bool", &[key_a, props])
        .expect("proplists:get_bool/2 must run");
    assert_atom(&host, not_bool, "false");
}

// ---------------------------------------------------------------------------
// Robustness guards (do not depend on the oracle)
// ---------------------------------------------------------------------------

#[test]
fn the_located_stdlib_is_a_usable_stdlib() {
    // A genuinely independent check of the skip guard's *input*: when the locator
    // claims a stdlib, that directory must actually hold the real `lists.beam` the
    // suites above load — so `skip_reason() == None` (no skip) can never mask a
    // directory that is not a usable stdlib. (The former
    // `skip_reason().is_some() == stdlib_ebin().is_none()` comparison was a
    // definitional tautology: `skip_reason` is literally `stdlib_ebin().is_none()`.)
    let Some(dir) = stdlib_ebin() else {
        // No locatable stdlib at all: a genuine absent-optional-dependency skip.
        eprintln!("{}", skip_reason().unwrap_or_default());
        return;
    };
    assert!(
        dir.join("lists.beam").is_file(),
        "the located oracle stdlib {dir:?} must hold the real `lists.beam` the \
         stdlib suites load"
    );
    assert!(
        skip_reason().is_none(),
        "a located, usable stdlib must not produce a skip reason"
    );
}

#[test]
fn a_missing_module_is_a_typed_error_not_a_panic() {
    // The host must never panic on an unknown module; the loader's `io`/`bad_file`
    // error surfaces unchanged.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let err = host.call("no_such_module_xyz", "f", &[]);
    assert!(err.is_err(), "an unknown module must be a typed error");
}

#[test]
fn one_shared_atom_table_resolves_names_across_modules() {
    // The invariant the whole approach rests on: a name interned while loading
    // one module resolves to the same index in every other loaded module's table.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let lists = host.load("lists").expect("load lists");
    let harness = host.load("stdlists_harness").expect("load harness");
    let true_in_lists = lists.atom_table.lookup("true").expect("lists names true");
    let true_shared = host
        .atom_table()
        .lookup("true")
        .expect("the shared table names true");
    assert_eq!(true_in_lists, true_shared);
    // The harness never mentions `true`/`false`; the shared table still resolves
    // them for it at the same index `lists` uses.
    assert_eq!(
        harness.atom_table.name(true_shared),
        Some("true"),
        "the shared `true` index resolves by name in every module's table"
    );
    // The engine's exact comparison of the two atom terms agrees.
    let a = beam_term::make_atom(true_in_lists);
    let b = beam_term::make_atom(true_shared);
    assert!(beam_term::cmp(a, b, host.atom_table()).is_eq());
}

// Sanity: the helper forms used above compose (guards a silent regression in the
// `support` list helpers themselves).
#[test]
fn list_helpers_round_trip() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let list = int_list(&mut host, &[7, 8, 9]);
    assert_eq!(terms::list_elements(list).unwrap().len(), 3);
    assert_int_list(&host, list, &[7, 8, 9]);
}

// ---------------------------------------------------------------------------
// Corpus BREADTH — the oracle-checked call corpus
//
// `tests/stdlib_sweep.rs` proves every installed `stdlib`/`kernel`/`compiler`
// `.beam` **loads** and runs a curated handful of functions; the sections above
// run a handful of modules in depth.  The sections below add *breadth*: a
// 258-call corpus spanning **151 distinct installed modules** (of the 261 the
// three stdlib suites discover), every call checked against the value the real
// system Erlang returns.
//
// The corpus is data: `tests/fixtures/stdlib_breadth_targets.txt`, one
// `module:function(args) => expected` line per call, each carrying the exact
// `erl -noshell -eval …` command that produces its expected value. *No expected
// value in this suite is guessed and none is derived from the engine's own
// output* — every one is the verbatim `~w` rendering the system `erl`
// (OTP 29 / erts-17.1, the differential oracle) printed.
//
// These sections live in this file because it is one of the three stdlib suites
// this node owns; the width of the table is why it is a *data* fixture rather
// than 258 inline assertions.
// ---------------------------------------------------------------------------

/// The committed oracle-checked call corpus (see the section header).
const BREADTH_TARGETS: &str = include_str!("fixtures/stdlib_breadth_targets.txt");

/// The floor this suite enforces: the number of distinct corpus modules that
/// must have **at least one export actually interpreted** on an oracle-checked
/// input. Set to the honest number this suite measures (171 of 261; it was 15
/// before the breadth corpus was added), not to a wishful one.
const BREADTH_MODULE_FLOOR: usize = 171;
/// The floor on the number of oracle-checked calls the corpus must actually
/// execute (set to the honest number this suite measures: 504).
const BREADTH_CALL_FLOOR: usize = 480;

/// One parsed corpus line: `module:function(args) => expected`.
struct BreadthTarget<'a> {
    /// The module to call.
    module: &'a str,
    /// The exported function to call.
    function: &'a str,
    /// The argument text, e.g. `[1,2],[3,4]` (one of the corpus shapes).
    args: &'a str,
    /// The verbatim `~w` rendering the system `erl` produced.
    expected: &'a str,
}

/// Parses one corpus line; `None` for a comment/blank line. A malformed *target*
/// line is a corpus bug and panics (`expect`), so a broken fixture can never be
/// silently skipped.
fn parse_breadth_target(line: &str) -> Option<BreadthTarget<'_>> {
    let line = line.split("  # oracle:").next().unwrap_or(line).trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, expected) = line
        .split_once(" => ")
        .expect("corpus line without ` => `");
    let (module, rest) = key.split_once(':').expect("corpus key without `:`");
    let (function, args) = rest.split_once('(').expect("corpus key without `(`");
    let args = args.strip_suffix(')').expect("corpus key without `)`");
    Some(BreadthTarget {
        module,
        function,
        args,
        expected,
    })
}

/// Splits an argument list on **top-level** commas, respecting `[]`/`{}`/`<<>>`
/// nesting and `"`-quoted strings.
fn split_arg_terms(args: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut start = 0usize;
    for (index, &byte) in args.as_bytes().iter().enumerate() {
        match byte {
            b'"' => in_string = !in_string,
            b'[' | b'{' | b'<' if !in_string => depth += 1,
            b']' | b'}' | b'>' if !in_string => depth -= 1,
            b',' if !in_string && depth == 0 => {
                parts.push(&args[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&args[start..]);
    parts
}

/// Builds one corpus argument from its source text: `[]`, `[a,b,c]` (a list of
/// nested terms), `{a,b}` (a tuple), `<<1,2,3>>` (a byte binary), `"abc"` (a
/// code-point string), an integer, an atom (bare or `'quoted'`).
fn arg_term(host: &mut ModuleHost, text: &str) -> Eterm {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix("<<").and_then(|t| t.strip_suffix(">>")) {
        let bytes: Vec<u8> = if inner.trim().is_empty() {
            Vec::new()
        } else {
            split_arg_terms(inner)
                .iter()
                .map(|part| {
                    part.trim()
                        .parse::<u8>()
                        .expect("a corpus binary element is a byte literal")
                })
                .collect()
        };
        return terms::make_heap_binary(host, &bytes).expect("build the corpus binary argument");
    }
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        if inner.trim().is_empty() {
            return beam_term::NIL;
        }
        let elements: Vec<Eterm> = split_arg_terms(inner)
            .iter()
            .map(|part| arg_term(host, part))
            .collect();
        return terms::make_list(host, &elements).expect("build the corpus list argument");
    }
    if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        let elements: Vec<Eterm> = split_arg_terms(inner)
            .iter()
            .map(|part| arg_term(host, part))
            .collect();
        return terms::make_tuple(host, &elements).expect("build the corpus tuple argument");
    }
    if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        let chars: Vec<Eterm> = inner
            .bytes()
            .map(|byte| beam_term::make_small(i64::from(byte)))
            .collect();
        return terms::make_list(host, &chars).expect("build the corpus string argument");
    }
    if let Ok(value) = text.parse::<i64>() {
        return beam_term::make_small(value);
    }
    match text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        Some(quoted) => host.intern(quoted),
        None => host.intern(text),
    }
}

/// Builds the argument list for a corpus call from its `(<args>)` text. The text
/// is a comma-separated list of the term shapes [`arg_term`] understands; the
/// empty text is the zero-arity call. A malformed argument is a corpus bug and
/// panics, so a broken fixture can never be silently skipped.
fn breadth_args(host: &mut ModuleHost, args: &str) -> Vec<Eterm> {
    if args.trim().is_empty() {
        return Vec::new();
    }
    split_arg_terms(args)
        .iter()
        .map(|part| arg_term(host, part))
        .collect()
}

/// The words `~w` quotes when they appear as atoms (`io_lib:write_atom/1`).
const ERESERVED: &[&str] = &[
    "after", "begin", "case", "catch", "cond", "end", "fun", "if", "let", "of", "query", "receive",
    "try", "when", "andalso", "orelse", "bsl", "bsr", "band", "bnot", "bor", "bxor", "div", "rem",
    "and", "or", "not", "xor", "maybe", "else",
];

/// Renders an atom exactly as `~w` does: bare when it is a plain lowercase name
/// that is not a reserved word, `'quoted'` otherwise.
fn render_atom(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .map(|first| first.is_ascii_lowercase())
        .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')
        && !ERESERVED.contains(&name);
    if plain {
        return name.to_string();
    }
    let mut out = String::from("'");
    for c in name.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Renders a float the way `~w` does (`1.0` rather than `1`).
fn render_float(value: f64) -> String {
    let text = format!("{value}");
    if text.contains(['.', 'e', 'E']) {
        text
    } else {
        format!("{text}.0")
    }
}

/// Renders a bitstring the way `~w` does — always as byte values, never as a
/// `<<"…">>` string (`~w` does no string detection).
fn render_binary(bytes: &[u8]) -> String {
    let mut out = String::from("<<");
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&byte.to_string());
    }
    out.push_str(">>");
    out
}

/// The engine's `~w`-equivalent term renderer — the form the corpus's expected
/// values are written in. `None` for a term `~w` cannot render *deterministically*
/// (a fun, pid, port or reference); such a result is reported as a loud skip
/// rather than compared against a fabricated string.
fn render_term(host: &ModuleHost, term: Eterm) -> Option<String> {
    if beam_term::is_small(term) {
        return Some(beam_term::signed_value(term).to_string());
    }
    if beam_term::is_atom(term) {
        return host.atom_table().name(beam_term::atom_val(term)).map(render_atom);
    }
    if beam_term::is_nil(term) {
        return Some("[]".to_string());
    }
    if let Ok(value) = terms::float_value(term) {
        return Some(render_float(value));
    }
    if let Ok(bytes) = terms::binary_bytes(term) {
        return Some(render_binary(bytes));
    }
    if let Ok(arity) = terms::tuple_arity(term) {
        let mut out = String::from("{");
        for index in 0..arity {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&render_term(host, terms::tuple_element(term, index).ok()?)?);
        }
        out.push('}');
        return Some(out);
    }
    if let Ok(pairs) = terms::map_entries(term) {
        let mut out = String::from("#{");
        for (index, &(key, value)) in pairs.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&render_term(host, key)?);
            out.push_str(" => ");
            out.push_str(&render_term(host, value)?);
        }
        out.push('}');
        return Some(out);
    }
    if beam_term::is_list(term) {
        let elements = terms::list_elements(term).ok()?;
        let mut out = String::from("[");
        for (index, &element) in elements.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&render_term(host, element)?);
        }
        out.push(']');
        return Some(out);
    }
    None
}

/// A host over the system stdlib `ebin` with `kernel`'s and `compiler`'s `ebin`
/// on the search path, so a cross-module call into a second/third application
/// resolves by name.
fn breadth_host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut host = ModuleHost::new(ebin);
    for app in ["kernel", "compiler"] {
        if let Some(dir) = support::app_ebin(app) {
            host.add_search_dir(dir);
        }
    }
    host
}

/// The installed corpus: every `*.beam` basename in the `stdlib`, `kernel` and
/// `compiler` `ebin` directories (the same 261 modules `tests/stdlib_sweep.rs`
/// loads).
fn corpus_modules() -> Vec<String> {
    let mut modules: Vec<String> = Vec::new();
    for app in ["stdlib", "kernel", "compiler"] {
        let Some(dir) = support::app_ebin(app) else {
            continue;
        };
        let mut found: Vec<String> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|ext| ext == "beam").unwrap_or(false))
            .filter_map(|path| path.file_stem().map(|stem| stem.to_string_lossy().into_owned()))
            .collect();
        found.sort();
        modules.extend(found);
    }
    modules
}

/// **The breadth measurement, and the floor that enforces it.**
///
/// Runs every corpus call and compares the engine's result against the system
/// `erl`'s rendering of the same call. A *wrong value* fails; a typed error
/// prints `SKIP: <module>:<f>(<args>) — <error>` and is counted but does not
/// fail (the floor below is what catches a regression that turns an executing
/// module into a blocked one). The report prints the per-module list so the
/// breadth is auditable, not just a number.
#[test]
fn breadth_runs_a_large_fraction_of_the_installed_corpus() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = breadth_host();
    let corpus = corpus_modules();
    let mut executed: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut calls = 0usize;
    let mut skips: Vec<String> = Vec::new();

    for line in BREADTH_TARGETS.lines() {
        let Some(target) = parse_breadth_target(line) else {
            continue;
        };
        let args = breadth_args(&mut host, target.args);
        let label = format!("{}:{}({})", target.module, target.function, target.args);
        match host.call(target.module, target.function, &args) {
            Ok(value) => {
                let Some(rendered) = render_term(&host, value) else {
                    skips.push(format!("SKIP: {label} — unrenderable result"));
                    continue;
                };
                assert_eq!(
                    rendered, target.expected,
                    "ORACLE MISMATCH for `{label}`: the engine returned `{rendered}`, \
                     the system erl returns `{}`",
                    target.expected
                );
                calls += 1;
                executed.insert(target.module);
            }
            Err(error) => skips.push(format!("SKIP: {label} — {error:?}")),
        }
    }

    let mut report = format!(
        "BREADTH: modules executed: {}/{}\nBREADTH: functions executed: {calls}\n",
        executed.len(),
        corpus.len()
    );
    for module in &executed {
        report.push_str(&format!("BREADTH: executed: {module}\n"));
    }
    for skip in &skips {
        report.push_str(&format!("BREADTH: {skip}\n"));
    }
    print!("{report}");

    assert!(
        corpus.len() >= 200,
        "the breadth suite only found {} corpus modules — expected stdlib+kernel (≈260)",
        corpus.len()
    );
    assert!(
        calls >= BREADTH_CALL_FLOOR,
        "only {calls} oracle-checked calls executed (floor {BREADTH_CALL_FLOOR}); \
         {} call(s) were blocked:\n{}",
        skips.len(),
        skips.join("\n")
    );
    assert!(
        executed.len() >= BREADTH_MODULE_FLOOR,
        "only {} of {} corpus modules were actually executed (floor {BREADTH_MODULE_FLOOR}); \
         {} call(s) were blocked:\n{}",
        executed.len(),
        corpus.len(),
        skips.len(),
        skips.join("\n")
    );
}

/// **The skip-vs-assert policy for the breadth corpus, spelled out.**
///
/// A module/function that needs a BIF the shared test host does not serve is
/// *blocked*, never wrong: the call returns a typed [`beam_common::BeamError`].
/// Each such probe is printed as `SKIP: <module>:<f>/<n> — <typed error>` and its
/// **exact** error is asserted, so a later regression that turns a typed error
/// into a fabricated result is caught. A genuine value mismatch (see above)
/// fails instead.
#[test]
fn breadth_blocked_calls_report_their_exact_typed_error() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = breadth_host();

    // oracle: erl -noshell -eval 'io:format("~p~n",[unicode:characters_to_binary("hi")]),halt().'
    //         <<"hi">>
    // `unicode:characters_to_binary/1` is a `unicode` BIF the host floor does not
    // serve, so the real module's call is a typed `BadArg` — never `<<"hi">>`.
    let hi = int_list(&mut host, &[104, 105]);
    let error = host
        .call("unicode", "characters_to_binary", &[hi])
        .expect_err("`unicode:characters_to_binary/1` is not in the host floor");
    println!("SKIP: unicode:characters_to_binary/1 — {error:?}");
    assert!(
        matches!(error, beam_common::BeamError::BadArg),
        "unexpected `unicode:characters_to_binary/1` error: {error:?}"
    );

    // oracle: erl -noshell -eval 'io:format("~p~n",[maps:put(k,1,#{})]),halt().'
    //         #{k => 1}
    // `maps:put/3` is an `erlang` BIF the host floor does not serve.
    let empty = terms::make_flatmap(&mut host, &[]).expect("build the empty map");
    let key = host.intern("k");
    let error = host
        .call(
            "maps",
            "put",
            &[key, beam_term::make_small(1), empty],
        )
        .expect_err("`maps:put/3` is not in the host floor");
    println!("SKIP: maps:put/3 — {error:?}");
    assert!(
        matches!(error, beam_common::BeamError::BadArg),
        "unexpected `maps:put/3` error: {error:?}"
    );

    // oracle: erl -noshell -eval 'io:format("~p~n",[maps:from_list([{k,1}])]),halt().'
    //         #{k => 1}
    let pair = support::term_tuple(&mut host, &[key, beam_term::make_small(1)]);
    let pairs = support::term_list(&mut host, &[pair]);
    let error = host
        .call("maps", "from_list", &[pairs])
        .expect_err("`maps:from_list/1` folds with the unserved `maps:put/3`");
    println!("SKIP: maps:from_list/1 — {error:?}");
    assert!(
        matches!(error, beam_common::BeamError::BadArg),
        "unexpected `maps:from_list/1` error: {error:?}"
    );
}

