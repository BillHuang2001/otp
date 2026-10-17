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

//! The **failed clause's argument list** in the cooked `function_clause`
//! stacktrace item.
//!
//! C's `save_stacktrace` (`../../erts/emulator/beam/beam_common.c`) makes the
//! innermost saved entry of a `function_clause` the **function itself**, carrying
//! `args = make_arglist(c_p, reg, a)` — the registers the failed clause was entered
//! with. Cooking that entry with `erts_build_mfa_item`
//! (`../../erts/emulator/beam/utils.c:3430-3457`) emits `args` in the item's `A` slot
//! whenever it is a list (or `nil`), so the oracle reports
//! `{Mod, Fun, [Arg, ...], Location}` for the function whose clause failed while
//! **every outer frame keeps its declared arity** (`{Mod, Fun, Arity, Location}`).
//!
//! The engine reproduces that through two latches
//! ([`Machine::pending_function_clause_args`] — armed by the raise site,
//! [`Machine::raise_function_clause_args`] — promoted by
//! [`Machine::throw_with_trace`]) and two consuming cooked-trace builders: the
//! caught path's [`Instr::BuildStacktrace`] and the uncaught path's
//! [`Machine::with_uncaught_frames`]. This module pins every observable shape:
//!
//! 1. a failed **2**-arity clause (`f(1, 2)`) — frame 0's `A` slot is the argument
//!    **list** `[1, 2]`, never the small integer `2`;
//! 2. a failed **0**-arity clause (`h()`) — frame 0's `A` slot is the empty list
//!    `[]`, never `0`;
//! 3. a failed 2-arity clause with **distinct** arguments (`h2(1, 2)`) —
//!    `[1, 2]`;
//! 4. **every outer frame keeps its declared arity** — a caller `run/0` frame stays
//!    `{fc, run, 0, Location}`, never an argument list;
//! 5. both catch surfaces: the caught `try ... catch C:R:S -> S` (the
//!    `build_stacktrace` cooked trace) **and** the uncaught raise's frames as
//!    [`Machine::with_uncaught_frames`] derives them;
//! 6. the regression guard — **absent** the latch the `A` slot is the declared
//!    arity again, pinned from a hand-built module and driven through both routes.
//!
//! # Oracle discipline
//!
//! Shapes 1–5 are cross-checked against the running Erlang system: the fixture is
//! compiled by the system `erlc` and the same call is asked of the system `erl`,
//! printing each cooked item's four fields with `~p` so the comparison is textual
//! and exact (never a hand-written expectation that could drift). Every test
//! **skips loudly** (`eprintln!`) when `erlc`/`erl` are absent; a spawned
//! `erlc`/`erl` that ran and exited non-zero is a harness failure and panics with
//! its stderr, so a broken oracle can never masquerade as a silent skip.
//!
//! **The engine's cooked list is a strictly shorter PREFIX of the oracle's** (the
//! oracle appends its own `erl_eval`/`init` caller frames, which live *outside* this
//! engine's run), so the shared assertion is: the class/reason line equals the
//! oracle's, the engine cooked a non-empty list, and that list equals the oracle's
//! first `n` items.
//!
//! # The uncaught path's documented limit
//!
//! [`Machine::raise_stacktrace`] hands the failed clause's argument list to
//! [`push_stack_frame`], but a [`beam_common::StackFrame`] carries only an
//! **arity** — the list cannot be represented there. It does not have to be: the
//! list is built from exactly the function's `arity` argument registers, so its
//! length *is* that arity and the two render identically (C's own
//! `erts_build_mfa_item` fallback). The uncaught test therefore pins the frame
//! chain's module/function/**arity**/location, and the argument-list-vs-arity
//! distinction is pinned where it is observable: on the cooked path.
use super::*;

use std::path::PathBuf;

use beam_common::Raise;
use beam_term::{is_list, is_small};

use crate::jit::Tier;

use super::oracle::{
    assert_matches_the_oracle, atom, compile_module, engine_trace, load_module, oracle_trace,
    render_atom, render_flat, render_location, run_fixture,
};

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// The fixture module, compiled by the system `erlc`.
///
/// Three caught entry points — one per failing-clause shape — and one uncaught
/// entry point with a **live non-tail caller** (`uncaught/1` calls `f/2` and then
/// uses the result), so the uncaught raise's frame chain holds two frames.
///
/// Every failing clause reaches its function's `func_info` instruction (the label
/// *after* the callable entry, reachable only from a failed head/guard test), which
/// is the site that publishes the failed clause's argument list:
///
/// * `f/2`   — a guard `A =:= B` that fails, so `func_info` is reached with the two
///   real arguments in `x(0)`/`x(1)`;
/// * `h/0`   — a constant-false guard, so the entry jumps straight to `func_info`
///   with a **zero**-arity register state;
/// * `h2/2`  — two head tests that both fail for `h2(1, 2)`.
const FIXTURE_SRC: &str = r#"-module(fc).

-export([run/0,
         run_h/0,
         run_h2/0,
         uncaught/1,
         f/2,
         h/0,
         h2/2]).

%% A failed 2-arity clause: `f(1, 2)` has no matching clause.
run() -> try f(1, 2) catch C:R:S -> {C,R,S} end.
f(A, B) when A =:= B -> ok.

%% A failed 0-arity clause: `h()`'s guard is never true.
run_h() -> try h() catch C:R:S -> {C,R,S} end.
h() when 1 > 2 -> ok.

%% A failed 2-arity clause with two distinct arguments: `h2(1, 2)`'s head
%% pattern is `h2(1, 1)`.
run_h2() -> try h2(1, 2) catch C:R:S -> {C,R,S} end.
h2(1, 1) -> ok.

%% An UNCAUGHT failed clause whose caller frame is still live (the call is not a
%% tail call, so the raise has a caller to name).
uncaught(X) -> R = f(X, 2), {R, X}.
"#;

/// Compile the fixture and load it, interning the `function_clause` atom the raise
/// resolves its reason from.
///
/// The OTP compiler emits `func_info`, not `erlang:error(function_clause)`, so the
/// atom is **not** in the module's atom chunk — a real runtime keeps it interned
/// globally for every module (the production counterpart is
/// `beam_rt::boot::CORE_ATOMS`, exactly as `super::oracle` documents for its own
/// `SEED_ATOMS`), and the engine's lookup policy is unchanged: it still resolves
/// the atom by name from the running module's table and never substitutes a
/// different one.
///
/// `None` means no runnable `erlc` is on `PATH` at all (the caller skips loudly).
fn fixture(tag: &str) -> Option<(PathBuf, Module)> {
    let dir = compile_module(tag, "fc", FIXTURE_SRC)?;
    let module = load_module(&dir, "fc", &["function_clause"]);
    Some((dir, module))
}

/// The `A` slot (the item's third field) of cooked item `index` of `cooked`.
fn a_slot(cooked: Eterm, index: usize) -> Eterm {
    let items = crate::terms::list_elements(cooked).expect("the cooked stacktrace is a list");
    crate::terms::tuple_element(items[index], 2).expect("the item's third field")
}

/// The `Location` field of cooked item `index`, rendered as the oracle prints it.
fn location(cooked: Eterm, index: usize) -> String {
    let items = crate::terms::list_elements(cooked).expect("the cooked stacktrace is a list");
    render_location(crate::terms::tuple_element(items[index], 3).expect("the item's location"))
}

/// The function name of cooked item `index` in `module`.
fn function_of(cooked: Eterm, index: usize, module: &Module) -> String {
    let items = crate::terms::list_elements(cooked).expect("the cooked stacktrace is a list");
    render_atom(
        crate::terms::tuple_element(items[index], 1).expect("the item's function"),
        module,
    )
}

/// Field `index` (`0` = module, `1` = function, `2` = the `A` slot, `3` = the
/// location) of one oracle item line (`M|F|A|Location`) — the location is the last
/// field because the fixtures' locations contain no `|`.
fn oracle_field(line: &str, index: usize) -> &str {
    line.splitn(4, '|')
        .nth(index)
        .unwrap_or_else(|| panic!("the oracle item `{line}` has no field {index}"))
}

/// The `M|F` prefix (fields 0 and 1) of a rendered frame line.
fn function_of_frame(items: &[String], index: usize) -> String {
    items[index]
        .splitn(4, '|')
        .take(2)
        .collect::<Vec<_>>()
        .join("|")
}

/// The number of arguments the oracle's `A` field describes: the element count of
/// an argument list (`[]` ⇒ `0`), or the integer C emits when there is no list.
///
/// The engine's uncaught frames carry only the **arity**, so this is how a
/// [`beam_common::StackFrame`] is checked against the oracle's argument list.
fn oracle_argument_count(a: &str) -> u64 {
    match a {
        "[]" => 0,
        _ if a.starts_with('[') => a.matches(',').count() as u64 + 1,
        _ => a
            .parse()
            .unwrap_or_else(|_| panic!("the oracle's A field `{a}` is neither a list nor an integer")),
    }
}

// ---------------------------------------------------------------------------
// Shape 1 — a failed 2-arity clause carries `[A1, A2]`
// ---------------------------------------------------------------------------

/// `fc:run/0` is `try f(1, 2) catch C:R:S -> {C,R,S} end`; the guard in `f/2`
/// fails, so `func_info` raises `function_clause` with `x(0)=1`, `x(1)=2`.
///
/// The oracle's innermost item is `{fc,f,[1,2],[{file,"fc.erl"},{line,6}]}` — the
/// `A` slot is the **argument list**, and it is that list (not `2`, the declared
/// arity) that distinguishes the fixed shape. The caller `run/0` keeps its
/// declared arity `0`.
#[test]
fn a_failed_two_arity_clause_cooks_the_argument_list_in_the_a_slot() {
    let Some((dir, module)) = fixture("two_arity") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the failed-clause test is skipped");
        return;
    };
    let name = atom(&module, "run");
    let mut ctx = MinimalContext::new();
    let (class, reason, cooked) = run_fixture(&module, &mut ctx, name, 0, &[], Tier::Interpreter);
    let (head, items) = engine_trace(class, reason, cooked, &module);

    let Some(oracle) = oracle_trace(&dir, "fc:run()") else {
        eprintln!("SKIP: no system `erl` on PATH -- the failed-clause oracle check is skipped");
        return;
    };
    assert_matches_the_oracle("fc:run/0", &head, &items, &oracle);

    assert_eq!(items.len(), 2, "the failed function then its caller");
    assert_eq!(function_of(cooked, 0, &module), "f");
    let args = a_slot(cooked, 0);
    assert!(
        is_list(args),
        "the failed clause's A slot is the argument LIST, not the declared arity: {args:?}"
    );
    assert_eq!(
        render_flat(args, &module),
        "[1,2]",
        "the two registers the failed clause was entered with"
    );
    // The location is the oracle's own, field by field (the whole item — location
    // included — is already compared by `assert_matches_the_oracle`; restating it
    // here makes a regression name the location rather than dump two strings).
    assert_eq!(
        location(cooked, 0),
        oracle_field(&oracle.items[0], 3),
        "the failing clause's own source line"
    );

    // Shape 4: the OUTER frame keeps its declared arity — a plain `0`, never a list.
    assert_eq!(function_of(cooked, 1, &module), "run");
    let outer = a_slot(cooked, 1);
    assert!(
        is_small(outer),
        "an outer frame's A slot is its declared arity, never an argument list"
    );
    assert_eq!(render_flat(outer, &module), "0");
    assert_eq!(location(cooked, 1), oracle_field(&oracle.items[1], 3));
}

// ---------------------------------------------------------------------------
// Shape 2 — a failed 0-arity clause carries `[]`
// ---------------------------------------------------------------------------

/// `fc:run_h/0` is `try h() catch C:R:S -> {C,R,S} end`; `h/0`'s guard is
/// constant-false, so its entry jumps straight to `func_info` with a zero-arity
/// register state.
///
/// The oracle's innermost item is `{fc,h,[],[{file,"fc.erl"},{line,9}]}` — the
/// **empty list**, which is what C's `erts_build_mfa_item` emits for `nil`. The
/// small integer `0` (the declared arity) would be the pre-fix shape.
#[test]
fn a_failed_zero_arity_clause_cooks_the_empty_argument_list_in_the_a_slot() {
    let Some((dir, module)) = fixture("zero_arity") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the 0-arity failed-clause test is skipped");
        return;
    };
    let name = atom(&module, "run_h");
    let mut ctx = MinimalContext::new();
    let (class, reason, cooked) = run_fixture(&module, &mut ctx, name, 0, &[], Tier::Interpreter);
    let (head, items) = engine_trace(class, reason, cooked, &module);

    let Some(oracle) = oracle_trace(&dir, "fc:run_h()") else {
        eprintln!("SKIP: no system `erl` on PATH -- the 0-arity oracle check is skipped");
        return;
    };
    assert_matches_the_oracle("fc:run_h/0", &head, &items, &oracle);

    assert_eq!(items.len(), 2, "the failed function then its caller");
    assert_eq!(function_of(cooked, 0, &module), "h");
    let args = a_slot(cooked, 0);
    assert_eq!(render_flat(args, &module), "[]", "the empty argument list, not `0`");
    assert!(
        is_list(args) || is_nil(args),
        "the A slot of a failed zero-arity clause is the empty list, never the integer 0"
    );
    assert_eq!(location(cooked, 0), oracle_field(&oracle.items[0], 3));

    assert_eq!(function_of(cooked, 1, &module), "run_h");
    assert_eq!(render_flat(a_slot(cooked, 1), &module), "0");
    assert_eq!(location(cooked, 1), oracle_field(&oracle.items[1], 3));
}

// ---------------------------------------------------------------------------
// Shape 3 — a failed 2-arity clause with two distinct arguments
// ---------------------------------------------------------------------------

/// `fc:run_h2/0` is `try h2(1, 2) catch C:R:S -> {C,R,S} end`; both head tests of
/// `h2(1, 1)` fail, so `func_info` raises with `x(0)=1`, `x(1)=2`.
///
/// The oracle's innermost item is `{fc,h2,[1,2],[{file,"fc.erl"},{line,12}]}`.
#[test]
fn a_failed_head_pattern_cooks_the_arguments_it_was_called_with() {
    let Some((dir, module)) = fixture("head_pattern") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the head-pattern test is skipped");
        return;
    };
    let name = atom(&module, "run_h2");
    let mut ctx = MinimalContext::new();
    let (class, reason, cooked) = run_fixture(&module, &mut ctx, name, 0, &[], Tier::Interpreter);
    let (head, items) = engine_trace(class, reason, cooked, &module);

    let Some(oracle) = oracle_trace(&dir, "fc:run_h2()") else {
        eprintln!("SKIP: no system `erl` on PATH -- the head-pattern oracle check is skipped");
        return;
    };
    assert_matches_the_oracle("fc:run_h2/0", &head, &items, &oracle);

    assert_eq!(items.len(), 2, "the failed function then its caller");
    assert_eq!(function_of(cooked, 0, &module), "h2");
    assert_eq!(
        render_flat(a_slot(cooked, 0), &module),
        "[1,2]",
        "the failed head pattern's real arguments"
    );
    assert_eq!(location(cooked, 0), oracle_field(&oracle.items[0], 3));

    assert_eq!(function_of(cooked, 1, &module), "run_h2");
    assert_eq!(render_flat(a_slot(cooked, 1), &module), "0");
    assert_eq!(location(cooked, 1), oracle_field(&oracle.items[1], 3));
}

// ---------------------------------------------------------------------------
// Shape 4 — the outer frames keep their DECLARED arity, for every shape
// ---------------------------------------------------------------------------

/// The argument list belongs to the frame whose function contains the failed
/// clause and to **no other** frame.
///
/// Every one of the three caught entry points is a `run*/0`, so each caller frame's
/// `A` slot must be the plain small integer `0` — C's
/// `erts_build_mfa_item` answer for a frame with no saved `args` — while frame 0 is
/// an argument list. This is the rule that a "carry the arity list everywhere"
/// regression would break.
#[test]
fn every_outer_frame_keeps_its_declared_arity() {
    let Some((_dir, module)) = fixture("outer_frames") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the outer-frame test is skipped");
        return;
    };
    for entry in ["run", "run_h", "run_h2"] {
        let name = atom(&module, entry);
        let mut ctx = MinimalContext::new();
        let (class, reason, cooked) =
            run_fixture(&module, &mut ctx, name, 0, &[], Tier::Interpreter);
        let (head, items) = engine_trace(class, reason, cooked, &module);
        assert_eq!(head, "error|function_clause", "{entry}: the class/reason");
        assert_eq!(items.len(), 2, "{entry}: the failed function then its caller");

        let inner = a_slot(cooked, 0);
        assert!(
            is_list(inner) || is_nil(inner),
            "{entry}: frame 0 is the failed clause's own function"
        );
        assert_eq!(
            function_of(cooked, 1, &module),
            entry,
            "frame 1 is the caller that made the non-tail call"
        );
        let outer = a_slot(cooked, 1);
        assert!(
            is_small(outer),
            "{entry}: an outer frame carries its declared arity, never an argument list"
        );
        assert_eq!(render_flat(outer, &module), "0", "{entry}: the caller's declared arity");
    }
}

// ---------------------------------------------------------------------------
// Shape 5b — the UNCAUGHT surface
// ---------------------------------------------------------------------------

/// Recover the raise's **live reason term** — what a live catch frame lands in `x(1)`.
///
/// The engine's uncaught raise is `ExitReason::Structured` with the reason term in
/// the `live_reason` slot, so [`Raise::reason::as_atom`] is `None` here; the atom
/// name is recovered by resolving the term against the module that raised it (the
/// runtime does the same through its shared atom table).
fn live_reason_term(raise: &Raise) -> Option<Eterm> {
    raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
}

/// Resolve the raise's reason term to its atom name in `module`'s own atom table.
fn reason_name(raise: &Raise, module: &Module) -> Option<String> {
    let reason = live_reason_term(raise)?;
    if !beam_term::is_atom(reason) {
        return None;
    }
    module
        .atom_table
        .name(beam_term::atom_val(reason))
        .map(str::to_owned)
}

/// Render the uncaught raise's derived frames exactly as [`oracle_trace`] renders a
/// cooked item, so the two can be compared as strings.
///
/// A [`beam_common::StackFrame`] carries an **arity**, not an argument list, so the
/// third field here is always the integer — which is why the oracle's item for the
/// *same* frame (whose `A` slot is the failed clause's list) matches only in the
/// first two fields plus the location; the arity equals the list's length by
/// construction (see the module docs).
fn render_uncaught_frames(raise: &Raise) -> Vec<String> {
    raise
        .stacktrace
        .frames
        .iter()
        .map(|frame| {
            let location = match &frame.location {
                Some(location) => format!(
                    "[{{file,\"{}\"}},{{line,{}}}]",
                    location.file, location.line
                ),
                None => "[]".to_string(),
            };
            format!(
                "{}|{}|{}|{location}",
                frame.module, frame.function, frame.arity
            )
        })
        .collect()
}

/// `fc:uncaught(7)` is `R = f(7, 2), {R, 7}` — a **non-tail** call into the same
/// failing clause, so the raise is uncaught and has a live caller frame.
///
/// Both surfaces must agree on the frame chain: the uncaught raise's derived
/// frames name the failed function first (arity `2`), then its caller
/// (`uncaught/1`, arity `1`), with the failing clause's own source line — the same
/// chain, in the same order, that the oracle's cooked items show for the identical
/// exception. The `A` slot's list-vs-arity distinction is invisible here (a
/// `StackFrame` has no `A` slot); it is pinned on the cooked path above.
#[test]
fn an_uncaught_failed_clause_names_the_function_then_its_caller() {
    let Some((dir, module)) = fixture("uncaught") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the uncaught test is skipped");
        return;
    };
    let name = atom(&module, "uncaught");
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::new();
    let err = machine
        .run_export_outcome(&module, name, 1, &[make_small(7)], &mut ctx)
        .expect_err("f(7, 2) matches no clause and must raise");
    let raise = err.into_raise().expect("a raise, not a capability gap");

    assert_eq!(raise.class, ExceptionClass::Error);
    assert_eq!(
        reason_name(&raise, &module).as_deref(),
        Some("function_clause"),
        "the reason is the bare atom the failed-clause raise resolves"
    );
    let head = format!("{}|function_clause", raise.class.as_str());
    let items = render_uncaught_frames(&raise);
    assert_eq!(items.len(), 2, "the failed function then its live caller, innermost first");
    assert_eq!(function_of_frame(&items, 0), "fc|f");
    assert_eq!(function_of_frame(&items, 1), "fc|uncaught");

    // The oracle's items for the same exception, caught at the `erl` top level so
    // its `S` is observable; the engine's two frames must be its first two items —
    // module, function and location field by field, and the engine's arity must be
    // the length of the oracle's argument list (the only field the two surfaces
    // render differently, because a `StackFrame` carries an arity, not a list).
    let call = "(try fc:uncaught(7) catch C2:R2:S2 -> {C2,R2,S2} end)";
    let Some(oracle) = oracle_trace(&dir, call) else {
        eprintln!("SKIP: no system `erl` on PATH -- the uncaught oracle check is skipped");
        return;
    };
    assert_eq!(head, oracle.head, "fc:uncaught/1: the class/reason");
    assert!(
        oracle.items.len() >= items.len(),
        "the oracle cooked {} item(s), the engine {}",
        oracle.items.len(),
        items.len()
    );
    for (index, engine_line) in items.iter().enumerate() {
        let oracle_line = &oracle.items[index];
        assert_eq!(
            engine_line.splitn(4, '|').take(2).collect::<Vec<_>>(),
            oracle_line.splitn(4, '|').take(2).collect::<Vec<_>>(),
            "frame {index}: module|function"
        );
        assert_eq!(
            oracle_field(engine_line, 3),
            oracle_field(oracle_line, 3),
            "frame {index}: the derived source location"
        );
        assert_eq!(
            oracle_field(engine_line, 2).parse::<u64>().expect("an arity"),
            oracle_argument_count(oracle_field(oracle_line, 2)),
            "frame {index}: the arity is the failed clause's argument-list length"
        );
    }
    // ... and the oracle's innermost `A` slot really is the argument list, so the
    // engine's frame-0 arity of `2` is that list's length (not the declared arity
    // reached by accident).
    assert_eq!(oracle_field(&oracle.items[0], 2), "[7,2]");
}

// ---------------------------------------------------------------------------
// Shape 6 — the regression guard: ABSENT the latch the A slot is the arity
// ---------------------------------------------------------------------------

/// A `m` module whose `f/2` is the raising function and whose `run/0` holds the
/// catch frame the raise lands in.
///
/// The engine looks the `function_clause` reason atom up **only** in the running
/// module's own atom table, so a module that can raise it carries it — as does the
/// `error` class atom the landing installs.
fn clause_module() -> Module {
    let mut fixture = Fixture::new(&["function_clause", "error"]);
    fixture.add_function("f", 2, vec![Instr::Label(Label(1)), Instr::Return]);
    fixture.add_function("run", 0, vec![Instr::Label(Label(2)), Instr::Return]);
    fixture.finish()
}

/// A machine positioned **inside** `f/2` with a live catch frame, `f/2`'s two
/// argument registers seeded.
fn machine_in_failing_clause() -> Machine {
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    // The handler lives in `run` (function index 1) at pc 0; the bottom frame owns it.
    machine.push_catch_frame(CatchFrame::new(1, 0, 0));
    machine.set_x(0, make_small(1)).unwrap();
    machine.set_x(1, make_small(2)).unwrap();
    // `f/2` executes at (function index 0, pc 0).
    machine.set_current_code(0, 0);
    machine
}

/// Cook the raise that just landed: the handler's `build_stacktrace` over a `NIL`
/// raw trace with a live raise snapshot.
fn cook_the_landed_raise(machine: &mut Machine, module: &Module) -> Eterm {
    machine.set_x(0, NIL).unwrap();
    let mut ctx = MinimalContext::new();
    let step = exec_one(module, &Instr::BuildStacktrace, machine, &mut ctx).unwrap();
    assert_eq!(step, Some(Step::Next));
    machine.x(0).unwrap()
}

/// **The argument-free route.** `raise_function_clause_no_fail` — the forwarder
/// `raise_function_clause_with_args` degrades to — arms **no** failed-clause
/// argument list, so the cooked item for the raising function falls back to its
/// **declared arity**, exactly C's `erts_build_mfa_item` when `args` is neither a
/// list nor `nil`.
///
/// This is the delta the fix is measured against: the *same* module, the *same*
/// position inside `f/2` and the *same* argument registers `[1, 2]`, but no latch —
/// and the `A` slot is the small integer `2`.
#[test]
fn without_the_latch_the_a_slot_is_the_declared_arity() {
    let module = clause_module();
    let mut machine = machine_in_failing_clause();
    let mut ctx = MinimalContext::new();
    let step = crate::interpreter::raise_function_clause_no_fail(
        &mut machine,
        &module,
        "maps",
        &mut ctx,
    )
    .expect("a live catch frame catches the raise");    assert!(matches!(step, Step::Goto { .. }));

    let cooked = cook_the_landed_raise(&mut machine, &module);
    let items = crate::terms::list_elements(cooked).expect("a proper list");
    assert_eq!(items.len(), 1, "the raising frame is the only snapshot frame");
    assert_eq!(function_of(cooked, 0, &module), "f");
    let a = a_slot(cooked, 0);
    assert!(
        is_small(a),
        "without the latch the A slot is the declared arity, not an argument list"
    );
    assert_eq!(
        render_flat(a, &module),
        "2",
        "the declared arity, even though x(0)/x(1) hold [1,2]"
    );
    assert_eq!(location(cooked, 0), "[]", "no line marker is in effect at pc 0");
}

/// **The latch route.** `func_info` publishes the failed clause's **real call
/// argument list**; the cooked item for the raising function carries that list in
/// its `A` slot, while an outer frame (had there been one) keeps its declared arity.
///
/// The module and the call registers are identical to the test above — only the
/// raise site differs — so the pair pins the `[1,2]`-vs-`2` distinction to the
/// latch and nothing else.
#[test]
fn with_the_latch_the_a_slot_is_the_argument_list() {
    let mut fixture = Fixture::new(&["function_clause", "error"]);
    let module_atom = fixture.module.name;
    let function_atom = fixture.module.atom_table.intern("f");
    fixture.add_function(
        "f",
        2,
        vec![Instr::FuncInfo {
            module: module_atom,
            function: function_atom,
            arity: 2,
        }],
    );
    fixture.add_function("run", 0, vec![Instr::Label(Label(2)), Instr::Return]);
    let module = fixture.finish();

    let mut machine = machine_in_failing_clause();
    let mut ctx = MinimalContext::new();
    // `func_info` at (function index 0, pc 0): the failed-head raise site.
    let step = machine
        .step_instruction(&module, &no_labels(), 0, 0, &mut ctx)
        .expect("a live catch frame catches the raise");
    assert!(matches!(step, Step::Goto { .. }));

    let cooked = cook_the_landed_raise(&mut machine, &module);
    assert_eq!(function_of(cooked, 0, &module), "f");
    let a = a_slot(cooked, 0);
    assert!(
        is_list(a),
        "the latch makes the A slot the failed clause's argument list"
    );
    assert_eq!(render_flat(a, &module), "[1,2]");
}

/// The same latch route for a **zero**-arity failed clause: `x(0..0)` is the empty
/// register list, so the `A` slot is the empty list `[]` — never the small integer
/// `0` the pre-fix shape emitted.
#[test]
fn with_the_latch_a_zero_arity_clause_cooks_the_empty_list() {
    let mut fixture = Fixture::new(&["function_clause", "error"]);
    let module_atom = fixture.module.name;
    let function_atom = fixture.module.atom_table.intern("f");
    fixture.add_function(
        "f",
        0,
        vec![Instr::FuncInfo {
            module: module_atom,
            function: function_atom,
            arity: 0,
        }],
    );
    fixture.add_function("run", 0, vec![Instr::Label(Label(2)), Instr::Return]);
    let module = fixture.finish();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(1, 0, 0));
    machine.set_current_code(0, 0);

    let mut ctx = MinimalContext::new();
    let step = machine
        .step_instruction(&module, &no_labels(), 0, 0, &mut ctx)
        .expect("a live catch frame catches the raise");
    assert!(matches!(step, Step::Goto { .. }));

    let cooked = cook_the_landed_raise(&mut machine, &module);
    let a = a_slot(cooked, 0);
    assert_eq!(
        render_flat(a, &module),
        "[]",
        "the zero-arity argument list, not the integer 0"
    );
    assert!(is_nil(a) || is_list(a));
}
