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
//! The **atom index space** (`../src/atom_seed.rs`): the runtime interns
//! `erts/emulator/beam/atom.names` in file order before anything else, so an atom's index is the
//! one real ERTS gives it.
//!
//! Atom *comparison* is by name (since OTP 26, so `lists:sort/1` is alphabetical), but the
//! *storage* order of a flat map's key tuple — hence `maps:keys/1`, a `~p`-rendered map and
//! `term_to_binary/1`'s `MAP_EXT` key sequence — follows the atom **index**. That is why the rows
//! below are map/printing/ETF shapes and not "sorted" ones, and why they diverged before the seed
//! (the table then started with `src/boot.rs`'s hand-ordered `CORE_ATOMS`, i.e. `true` before
//! `false`).
//!
//! The goldens are measured against the live oracle, which is compared directly when `erl` is
//! available and skipped **loudly** when it is not (the shared [`support`] module resolves the
//! oracle from `$ERTS_ORACLE`/`$PATH` and bounds every probe). Each row also pins the recorded
//! oracle value unconditionally, so the ordering is checked even without a toolchain installed.
//!
//! # Was the tree's `atom.names` order verified against the oracle's?
//!
//! Yes, before the seed landed. A 2-key map literal stores its keys in atom-index order
//! (`#{infinity=>1,error=>2}` ⇒ `[infinity,error]`, which distinguishes the file order from
//! alphabetical order), so 37 such pairs — every one of them a pair whose file order differs from
//! its alphabetical order, spread across the whole 763-name file — were compiled with the system
//! `erlc` and run under the oracle: **all 37 agreed with this tree's file order**. The four
//! verification rows below need no caveat (all eight golden names sit in `atom.names` lines
//! 44-543, long before any name an OTP-30-rc0-vs-OTP-29 file difference could insert); a name
//! *inserted* between two releases would shift only the indices after it, which is the residual
//! `crates/beam_rt/CONTEXT.md` states.
// The shared helper module is compiled into every integration-test binary, so this binary would
// otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;
use std::path::Path;
/// The banner for the loud skip (see [`support::require_oracle`]).
const BANNER: &str = "atom seed order";
/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
// ---------------------------------------------------------------------------------------
// The verification rows
// ---------------------------------------------------------------------------------------
/// Oracle: `[false,true,infinity,timeout,error,undefined,nil,ok]` — atom.names file order for
/// these eight names (neither alphabetical nor `CORE_ATOMS` order).
const MAPS_KEYS_ROW: &str = "io:format(\"~p~n\",[maps:keys(#{ok=>1,error=>2,undefined=>3,\
     true=>4,false=>5,nil=>6,infinity=>7,timeout=>8})]), halt(0).";
/// The recorded oracle rendering of the same eight-atom map's three-name subset.
const MAP_RENDER_ROW: &str = "io:format(\"~p~n\",[#{ok=>1,error=>2,undefined=>3}]), halt(0).";
/// Oracle: `#{cause => x,module => y}` — `cause` (atom.names line ~150) precedes `module`
/// (~line 440), which is also alphabetical here, so the row pins `~p`'s space around `=>`.
const MAP_RENDER_ROW_2: &str = "io:format(\"~p~n\",[#{cause=>x,module=>y}]), halt(0).";
/// `MAP_EXT` writes its keys in **atom-index** order: `timeout` (atom.names index 7) before
/// `error` (index 12), which is the opposite of their alphabetical order — so a writer that sorted
/// keys by name would flip this row. Both names are inside the `atom.names` prefix, so the pair's
/// relative index is fixed by the file rather than by this invocation's interning history.
const ETF_MAP_ROW: &str =
    "io:format(\"~p~n\",[term_to_binary(#{error=>2,timeout=>7})]), halt(0).";
/// The same shape with two names **outside** `atom.names` (`a` and `b`), pinned as this
/// runtime's own invariant only: the oracle's relative index for a name the file does not list
/// depends on what its boot interned first, and it really does answer both ways — measured on this
/// host, `erl -noshell -eval '<row>'` gives `a` first while `… -s init stop` gives `b` first. This
/// runtime interns them in the literal's source order (`b`, then `a`), which is the ERTS rule for
/// an `AtU8`/parse order and the shape the objective records.
const ETF_PINNED_ROW: &str = "io:format(\"~p~n\",[term_to_binary(#{b=>1,a=>2})]), halt(0).";
/// `erlang:system_info(atom_count)` — **own state**: this runtime's table length, never a pinned
/// oracle figure (the oracle answers 10851 on OTP 29.1.2, which is not this runtime's number).
const ATOM_COUNT_EXPR: &str = "erlang:system_info(atom_count)";
/// The recorded oracle `maps:keys/1` answer for [`MAPS_KEYS_ROW`].
const GOLDEN_KEYS: &str = "[false,true,infinity,timeout,error,undefined,nil,ok]";
/// The recorded oracle `~p` answers.
const GOLDEN_RENDER: &str = "#{error => 2,undefined => 3,ok => 1}";
const GOLDEN_RENDER_2: &str = "#{cause => x,module => y}";
/// The oracle's `term_to_binary(#{error=>2,timeout=>7})` bytes as `~p` renders them, with every
/// space/newline removed (the oracle's printer breaks this line at 80 columns; the breaks carry no
/// meaning, so both sides are compared whitespace-free — see [`check_row_normalized`]).
const GOLDEN_ETF: &str =
    "<<131,116,0,0,0,2,119,7,116,105,109,101,111,117,116,97,7,119,5,101,114,114,111,114,97,2>>";
/// This runtime's own `term_to_binary(#{b=>1,a=>2})` bytes (`b` before `a`: see [`ETF_PINNED_ROW`]).
const GOLDEN_ETF_PINNED: &str = "<<131,116,0,0,0,2,119,1,98,97,1,119,1,97,97,2>>";
// ---------------------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------------------
/// `beam_rt -noshell -eval <expr>` through the built binary, as the differential harness drives
/// it: the answer is the trimmed stdout of a run that must exit 0.
fn beam_rt_eval(expr: &str) -> String {
    let output = support::run_bounded(Path::new(BINARY), &["-noshell", "-eval", expr])
        .unwrap_or_else(|| panic!("`{BINARY} -eval` could not be run to completion"));
    assert!(
        output.status.success(),
        "`{BINARY} -noshell -eval` must exit 0, got {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
/// Every space, tab and newline removed — the oracle's term printer breaks long lines at 80
/// columns and indents the continuation, which is a rendering artifact, not a difference in the
/// printed term.
fn normalized(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}
/// One row: compare the runtime's own answer with the recorded golden, and — when the oracle is
/// installed — with the oracle's live answer as well.
fn check_row(expr: &str, golden: &str, what: &str) {
    check_row_normalized(expr, golden, what, false)
}
/// [`check_row`], optionally comparing whitespace-free (`normalize`), which is what a row whose
/// rendering the oracle wraps at 80 columns needs.
fn check_row_normalized(expr: &str, golden: &str, what: &str, normalize: bool) {
    let ours = beam_rt_eval(expr);
    let mine = if normalize { normalized(&ours) } else { ours.clone() };
    assert_eq!(
        mine,
        if normalize { normalized(golden) } else { golden.to_string() },
        "{what}: the runtime must answer the oracle's value"
    );
    if let Some(oracle) = support::require_oracle(BANNER) {
        let Some(expected) = support::oracle_eval(&oracle, None, expr) else {
            eprintln!("atom_seed_order: the oracle probe for {what} answered nothing — skipped");
            return;
        };
        eprintln!("atom_seed_order/{what}: beam_rt {ours:?}; oracle {expected:?}");
        if normalize {
            assert_eq!(
                normalized(&ours),
                normalized(&expected),
                "{what}: the runtime must agree with the live oracle"
            );
        } else {
            assert_eq!(ours, expected, "{what}: the runtime must agree with the live oracle");
        }
    }
}
// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------
/// `maps:keys/1` of an eight-atom map is atom.names **file order** — the whole reason the seed
/// exists, and the row that used to start `[true,false,ok,…]` (`CORE_ATOMS` order).
#[test]
fn maps_keys_follows_the_seeded_atom_index_order() {
    check_row(MAPS_KEYS_ROW, GOLDEN_KEYS, "maps:keys/1");
}
/// A multi-atom map renders in the same index order through the registered `~p` printer.
#[test]
fn a_rendered_map_follows_the_seeded_atom_index_order() {
    check_row(MAP_RENDER_ROW, GOLDEN_RENDER, "~p map");
    check_row(MAP_RENDER_ROW_2, GOLDEN_RENDER_2, "~p map (cause/module)");
}
/// `term_to_binary/1` of a map writes its keys in the same order, byte for byte.
#[test]
fn an_encoded_map_follows_the_seeded_atom_index_order() {
    check_row_normalized(ETF_MAP_ROW, GOLDEN_ETF, "term_to_binary map", true);
}
/// A map whose keys come from **outside** `atom.names` keeps the literal's own order (`b` then
/// `a`), pinned against this runtime's golden only (see [`ETF_PINNED_ROW`] for why the oracle's
/// own answer for that pair is invocation-dependent).
#[test]
fn an_encoded_map_of_unseeded_names_keeps_the_literal_order() {
    assert_eq!(beam_rt_eval(ETF_PINNED_ROW), GOLDEN_ETF_PINNED);
}
/// The ordering guard that needs **no** oracle: the seeded table's own indices are the golden
/// order, so dropping the seed fails here even on a host without `erl`.
#[test]
fn the_seeded_table_orders_the_golden_names_by_index() {
    let mut table = beam_term::AtomTable::new();
    beam_rt::atom_seed::seed(&mut table);
    let golden = [
        "false", "true", "infinity", "timeout", "error", "undefined", "nil", "ok",
    ];
    let indices: Vec<u32> = golden
        .iter()
        .map(|name| {
            table
                .lookup(name)
                .unwrap_or_else(|| panic!("{name} must be seeded from atom.names"))
                .raw()
        })
        .collect();
    assert!(
        indices.windows(2).all(|pair| pair[0] < pair[1]),
        "the golden order must be strictly ascending by atom index: {indices:?}"
    );
    // The seed is the *start* of the index space, not an addition to it.
    assert_eq!(
        table.lookup("false").map(|a| a.raw()),
        Some(0),
        "atom.names' first name must be index 0"
    );
    // …and it is the whole emulator table, not a handful of names.
    assert!(
        table.len() > 700,
        "the seed interns the emulator's whole C-side name table: {}",
        table.len()
    );
}
/// `erlang:system_info(atom_count)` is **this runtime's own table length** — an own-state answer,
/// measured in one VM (the same `Runtime` that evaluates the expression), never the oracle's.
#[test]
fn atom_count_is_this_runtimes_own_table_length() {
    let mut runtime = beam_rt::Runtime::with_defaults().expect("the default runtime");
    runtime.boot().expect("the boot script must boot");
    let value = runtime
        .eval(ATOM_COUNT_EXPR)
        .unwrap_or_else(|error| panic!("`{ATOM_COUNT_EXPR}` must answer: {error}"));
    assert!(
        beam_term::is_small(value),
        "atom_count must be a small int, not {:?}",
        value
    );
    let reported = beam_term::signed_value(value);
    assert_eq!(
        reported as usize,
        runtime.atoms().len(),
        "atom_count must be the runtime's own atom-table length"
    );
    // And that length carries the seed: the count can never be below atom.names' name count.
    let mut seeded = beam_term::AtomTable::new();
    beam_rt::atom_seed::seed(&mut seeded);
    assert!(
        reported as usize >= seeded.len(),
        "the table holds at least the atom.names run: {reported} < {}",
        seeded.len()
    );
}
