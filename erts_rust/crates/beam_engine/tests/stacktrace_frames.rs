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
//! **DEFECT-2 regression: the cooked catch-stacktrace names each frame EXACTLY
//! ONCE.**
//!
//! A field report claimed `Machine::throw_with_trace`
//! (`src/machine/catch.rs`) duplicates the innermost stacktrace frame by pushing
//! **both** `self.current_code()` and every `self.resumption_contexts()`. The
//! claim is **false**: C's `gather_stacktrace`
//! (`../../erts/emulator/beam/beam_common.c`) records `[current frame CP] ++ caller
//! CPs` — a sequence of **unique** continuation pointers — so
//! `raise_snapshot = [current_code] ++ resumption_contexts()` is exactly right, and
//! removing the `current_code()` push would *drop* the raising frame (the
//! `function_clause`/`badmatch` localization this derivation exists for).
//!
//! This suite is the **regression gate** that stops the claim from resurfacing: it
//! drives the three `try ... catch _C:_R:S -> S end` shapes the report used and
//! pins, per shape, the **cooked** `S` — the frames the handler's `build_stacktrace`
//! carves from the machine's raise-time snapshot — against the real system ERTS.
//!
//! The oracle shapes (verified against `erl`, OTP 29 / `erts-17.1`, by rebuilding
//! `tests/fixtures/stacktrace_frames.erl`):
//!
//! | entry | Erlang | oracle `S` (leading `stacktrace_frames` frames) |
//! |---|---|---|
//! | `t0/0` | `try f() catch _C:_R:S -> S end`, `f() -> erlang:error(boom).` | `[{stacktrace_frames,f,0,{line,4}},{stacktrace_frames,t0,0,{line,3}}]` |
//! | `t2/0` | `try hh() catch _C:_R:S -> S end`, `hh() -> erlang:error(boom).` | `[{stacktrace_frames,hh,0,{line,7}},{stacktrace_frames,t2,0,{line,8}}]` |
//! | `t3/0` | `try g2() catch _C:_R:S -> S end`, `g2() -> hh2() + 1.`, `hh2() -> erlang:error(boom).` | `[{stacktrace_frames,hh2,0,{line,11}},{stacktrace_frames,t3,0,{line,9}}]` |
//!
//! Each frame appears **once**. `t0`/`t2` are two identical chains (the control
//! that a second chain does not accumulate a doubled frame); `t3` additionally pins
//! the oracle's **tail-call elision** — `t3`'s try body tail-calls `g2/0`, which
//! (`sys_core_fold` proving `hh2/0` never returns) is itself a tail call to `hh2/0`,
//! so `g2/0`'s frame is reused and it is absent from the cooked `S`.
//!
//! The engine's cooked trace is a strictly-shorter **prefix** of the oracle's — the
//! trailing `erl_eval`/`init` host frames live outside this run — so every test
//! compares the engine's frames against the oracle's own `stacktrace_frames` frames
//! (never a fabricated host frame). The oracle is re-derived live from the system
//! `erl` and each test **skips loudly** when no runnable `erl` is on `PATH` (a green
//! run without `erl` proves nothing about corpus parity).
//!
//! # The second family of rows: a consumed continuation slot is cleared
//!
//! A frame's continuation-pointer slot means exactly one thing — *this frame is
//! suspended inside a call and resumes at that address when the call returns* — so
//! whoever follows the address clears it in the same step (C's `RETURN()` does
//! `SET_I(cp_val(*E)); *E = NIL`, `erts/emulator/beam/emu/macros.tab:241-247`), and a
//! raise that **lands** in a frame's catch handler clears it too (the handler runs in
//! that frame). A frame that already returned from its call, or that a raise landed
//! in, must therefore never be named a second time. Those rows pin, per shape, the
//! cooked trace (or the raise's own frames, or `process_info/2`'s
//! `current_stacktrace`):
//!
//! | entry | Erlang | pinned frames |
//! |---|---|---|
//! | `withcall/0` | `parent(ok)`, then `try 1 / 0 catch _C:_R:S -> S end` | `[withcall/0 @16]` |
//! | `inhandler/0` | `try g(0) catch _:_ -> 1 / 0 end` | `[inhandler/0 @19]` |
//! | `park/0` | `parent(ok)`, then `receive _ -> ok end` | `[park/0 @21]` (`current_stacktrace`) |
//! | `folded/0` | `try 1 / 0 catch C:R:S -> erlang:raise(C,R,S) end` (folded to `raw_raise`) | `[folded/0 @24]` |
//! | `refold/0` | `try g(0) catch C:R:S -> erlang:raise(C,R,S) end` | `[g/1 @17, refold/0 @26]` |
//!
//! `folded/0`/`refold/0` pin the supplied-vs-derived trace rule: `sys_core_fold`
//! rewrites `catch C:R:S -> erlang:raise(C,R,S)` into the `raw_raise` instruction
//! (so the `erlang:raise/3` BIF never runs), and C's `raw_raise` keeps the trace the
//! re-raised exception already carried (`beam_common.c:1148-1179`), so no
//! re-raise-site frame is ever added — `refold/0` is the row whose frame *identity*
//! moves when a trace is derived at the re-raise site instead.
//!
//! Every row above runs on **both** execution tiers — the interpreter and the native
//! x86-64 baseline tier (whose `return`/`deallocate_return` bodies are the
//! interpreter's JIT twins) — and the two must derive byte-identical frames before
//! either is compared with the oracle.

use std::process::Command;

use beam_common::Raise;
use beam_engine::{terms, Machine, RunOutcome, Tier};
use beam_term::{atom_val, is_atom, is_nil, is_small, signed_value, Eterm};

mod support;
use support::ModuleHost;
// ---------------------------------------------------------------------------
// The committed fixture (produced by the system `erlc +deterministic`)
// ---------------------------------------------------------------------------

/// The module name under test (the fixture's `-module`).
const MODULE: &str = "stacktrace_frames";

/// The committed oracle fixture (`erlc +deterministic stacktrace_frames.erl`).
const FRAMES_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/stacktrace_frames.beam"
);

/// The two execution tiers every row must agree on: the direct-threaded
/// interpreter and the native x86-64 baseline tier (whose `return` /
/// `deallocate_return` bodies are the JIT twins of the interpreter's).
const TIERS: [Tier; 2] = [Tier::Interpreter, Tier::BaselineJit];
/// The fixture's source file name — the `{file,Name}` element of every frame's
/// location (a list of code points, exactly the oracle's shape).
const FRAMES_SOURCE: &str = "stacktrace_frames.erl";

/// A [`ModuleHost`] with `stacktrace_frames` registered. The shared-atom host
/// seeds the well-known atoms — `file`/`line` included, which `build_stacktrace`
/// resolves by name from the running module's own table.
fn frames_host() -> ModuleHost {
    let mut host = ModuleHost::new(".");
    host.register(MODULE, FRAMES_BEAM);
    host
}

// ---------------------------------------------------------------------------
// Reading a cooked stacktrace term back into frames
// ---------------------------------------------------------------------------

/// One cooked frame: `(module, function, arity, file, line)`.
type Frame = (String, String, i64, String, i64);

/// The `frame(function, arity, line)` expectation for this fixture — the module
/// and file are fixed, so only the varying fields are spelled out per case.
fn frame(function: &str, arity: i64, line: i64) -> Frame {
    (
        MODULE.to_string(),
        function.to_string(),
        arity,
        FRAMES_SOURCE.to_string(),
        line,
    )
}

/// The name of the atom `term`, resolved through the host's shared atom table.
fn atom_name(host: &ModuleHost, term: Eterm) -> String {
    assert!(
        is_atom(term),
        "a stacktrace field must be an atom, got {term:?}"
    );
    host.atom_table()
        .name(atom_val(term))
        .unwrap_or_else(|| panic!("the shared atom table does not name {term:?}"))
        .to_string()
}

/// Reads an Erlang string (a proper list of code points) into a Rust `String`.
fn erl_string(term: Eterm) -> String {
    let codes: Vec<u32> = terms::list_elements(term)
        .expect("an Erlang string is a proper list")
        .into_iter()
        .map(|code| {
            assert!(
                is_small(code),
                "a string code point must be a small integer, got {code:?}"
            );
            signed_value(code) as u32
        })
        .collect();
    codes
        .iter()
        .map(|&code| char::from_u32(code).expect("a string code point is a valid char"))
        .collect()
}

/// The `(Module, Function, Arity, File, Line)` view of one cooked stacktrace item.
///
/// A frame with no location (the oracle's trailing `{init,…}` items) yields an
/// empty file and line `0` — the fixture never produces one, but the reader stays
/// total so a shape change is a typed assert, never a panic on unexpected input.
fn frame_fields(host: &ModuleHost, item: Eterm) -> Frame {
    assert_eq!(
        terms::tuple_arity(item).ok(),
        Some(4),
        "a cooked stacktrace item is the 4-tuple {{M,F,A,Location}}: {item:?}"
    );
    let module = atom_name(host, terms::tuple_element(item, 0).expect("module"));
    let function = atom_name(host, terms::tuple_element(item, 1).expect("function"));
    let arity_term = terms::tuple_element(item, 2).expect("arity");
    assert!(
        is_small(arity_term),
        "a frame arity must be a small integer, got {arity_term:?}"
    );
    let arity = signed_value(arity_term);
    let location = terms::tuple_element(item, 3).expect("location");
    if is_nil(location) {
        return (module, function, arity, String::new(), 0);
    }
    let pairs = terms::list_elements(location).expect("the location is a proper list");
    assert_eq!(
        pairs.len(),
        2,
        "the location must be [{{file,Name}},{{line,N}}], got {pairs:?}"
    );
    assert_eq!(
        atom_name(
            host,
            terms::tuple_element(pairs[0], 0).expect("the file key")
        ),
        "file",
        "the location's first pair must be {{file,Name}}"
    );
    let file = erl_string(terms::tuple_element(pairs[0], 1).expect("the file name"));
    assert_eq!(
        atom_name(
            host,
            terms::tuple_element(pairs[1], 0).expect("the line key")
        ),
        "line",
        "the location's second pair must be {{line,N}}"
    );
    let line_term = terms::tuple_element(pairs[1], 1).expect("the line number");
    assert!(
        is_small(line_term),
        "a frame line must be a small integer, got {line_term:?}"
    );
    (module, function, arity, file, signed_value(line_term))
}

/// Reads a cooked stacktrace term into [`Frame`]s (innermost first).
fn read_frames(host: &ModuleHost, trace: Eterm) -> Vec<Frame> {
    terms::list_elements(trace)
        .unwrap_or_else(|err| panic!("a cooked stacktrace must be a proper list: {err:?}"))
        .iter()
        .map(|&item| frame_fields(host, item))
        .collect()
}

/// Runs `MODULE:function/0` and returns the frames of its cooked `S`.
fn cooked_frames(host: &mut ModuleHost, function: &str) -> Vec<Frame> {
    let s = host
        .call(MODULE, function, &[])
        .unwrap_or_else(|err| panic!("{MODULE}:{function}/0 must return its caught S, got {err:?}"));
    read_frames(host, s)
}

// ---------------------------------------------------------------------------
// Running a row on an explicit tier (the tier-parity half of every row)
// ---------------------------------------------------------------------------

/// Runs `MODULE:function/0` on a fresh machine through `tier`, returning that
/// machine (so a parked row can read its state) and the run's outcome.
///
/// The machine is handed back rather than retained by the host: a row's cooked `S`
/// holds atoms, small integers and character lists only — no off-heap
/// reference-counted binary — so its terms stay valid while the machine is alive,
/// which it is for the whole of the caller's read.
fn run_on_tier(
    host: &mut ModuleHost,
    function: &str,
    tier: Tier,
) -> (Machine, beam_common::Result<RunOutcome>) {
    let module = host.load(MODULE).expect("the fixture loads");
    let function_atom = module
        .atom_table
        .lookup(function)
        .unwrap_or_else(|| panic!("{function} is interned in {MODULE}"));
    let mut machine = Machine::new();
    let outcome = machine.run_export_outcome_with_tier(&module, function_atom, 0, &[], host, tier);
    (machine, outcome)
}

/// The cooked `S` frames `MODULE:function/0` returns on `tier`.
fn cooked_frames_on_tier(host: &mut ModuleHost, function: &str, tier: Tier) -> Vec<Frame> {
    let (machine, outcome) = run_on_tier(host, function, tier);
    let outcome = outcome.unwrap_or_else(|err| panic!("{MODULE}:{function}/0 on {tier:?}: {err:?}"));
    let RunOutcome::Returned(value) = outcome else {
        panic!("{function}/0 must return its caught S on {tier:?}, got {outcome:?}");
    };
    let frames = read_frames(host, value);
    drop(machine);
    frames
}

/// The frames of the uncaught raise `MODULE:function/0` propagates on `tier`.
fn raise_frames_on_tier(host: &mut ModuleHost, function: &str, tier: Tier) -> Vec<Frame> {
    let (machine, outcome) = run_on_tier(host, function, tier);
    let rendered = format!("{outcome:?}");
    let err = match outcome {
        Ok(outcome) => panic!("{MODULE}:{function}/0 must propagate its raise on {tier:?}, got {outcome:?}"),
        Err(err) => err,
    };
    drop(machine);
    let raise = err
        .into_raise()
        .unwrap_or_else(|| panic!("{function}/0 must raise, not fail its capability ({rendered})"));
    raise_frames(&raise)
}

/// The `(module, function, arity, file, line)` view of a raise's symbolic frames.
fn raise_frames(raise: &Raise) -> Vec<Frame> {
    raise
        .stacktrace
        .frames
        .iter()
        .map(|frame| match &frame.location {
            Some(location) => (
                frame.module.as_str().to_string(),
                frame.function.as_str().to_string(),
                i64::from(frame.arity),
                location.file.as_str().to_string(),
                i64::from(location.line),
            ),
            None => (
                frame.module.as_str().to_string(),
                frame.function.as_str().to_string(),
                i64::from(frame.arity),
                String::new(),
                0,
            ),
        })
        .collect()
}
/// Asserts `frames` contains no repeated frame — the exact defect the report
/// described (a duplicated top frame). The **whole** `(module, function, arity,
/// file, line)` tuple is compared, so a repeat with a drifted location is caught
/// too.
fn assert_no_duplicate_frames(frames: &[Frame], case: &str) {
    for (index, frame) in frames.iter().enumerate() {
        let earlier = frames[..index].iter().position(|other| other == frame);
        assert!(
            earlier.is_none(),
            "{case}: the cooked stacktrace repeats the frame {frame:?} \
             (positions {earlier:?} and {index}) — each frame must appear exactly once"
        );
    }
}

/// Renders frames in the flat `Module|Function|Arity|File|Line` form the oracle
/// helper below prints (frames joined by `;`), so the two lists compare as strings.
fn render_frames(frames: &[Frame]) -> String {
    frames
        .iter()
        .map(|(module, function, arity, file, line)| {
            format!("{module}|{function}|{arity}|{file}|{line}")
        })
        .collect::<Vec<_>>()
        .join(";")
}

// ---------------------------------------------------------------------------
// The oracle (real ERTS), skipped loudly without `erl`
// ---------------------------------------------------------------------------

/// Runs `expression` under the system `erl` with the fixture directory appended and
/// returns the fixture module's own frames from the stacktrace it evaluates to,
/// rendered in [`render_frames`]'s flat form.
///
/// `expression` must evaluate to a **stacktrace list** (the cooked `S` of a
/// `try … catch _C:_R:S`, or a `process_info(P, current_stacktrace)` value); the
/// trailing host frames (`erl_eval`/`init`) are selected away, exactly as in
/// [`oracle_frames`]. `None` when no runnable `erl` is on `PATH`.
fn oracle_select(expression: &str) -> Option<String> {
    let eval = format!(
        "Pick = fun(L) -> [X || {{{MODULE},_,_,_}} = X <- L] end, \
         Show = fun(L) -> lists:flatten(lists:join(\";\", \
         [io_lib:format(\"~s|~s|~w|~s|~w\", [M, atom_to_list(FN), A, \
         proplists:get_value(file, Loc), proplists:get_value(line, Loc)]) \
         || {{M,FN,A,Loc}} <- L])) end, \
         io:format(\"~s\", [Show(Pick({expression}))]), halt()."
    );
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let output = Command::new("erl")
        .args(["-noshell", "-pz", fixtures, "-eval", &eval])
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for `{eval}`\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The `stacktrace_frames`-module frames the system `erl` cooks for `function/0`,
/// rendered in [`render_frames`]'s flat form; `None` when no runnable `erl` is on
/// `PATH` (the caller skips loudly).
///
/// The oracle's cooked `S` is the engine's frames followed by host frames
/// (`erl_eval`/`init`) that live **outside** this engine's run, so the oracle
/// helper selects the leading `{stacktrace_frames,_,_,_}` items — exactly the
/// frames the engine is expected to produce. The engine's trace is therefore a
/// strictly-shorter **prefix** of the oracle's, and comparing against this
/// selection compares every field the engine can know (module, function, arity,
/// file, line) without fabricating a host frame.
///
/// `erl` is invoked with the fixtures directory **appended** (`-pz`, never `-pa`:
/// a fixture directory holds stand-in modules that would shadow the real stdlib if
/// prepended). A spawned `erl` that exits non-zero is a **harness** failure, not
/// "no oracle" — it panics with the captured stderr, so a broken oracle can never
/// masquerade as a silent skip (the same policy as `tests/trace.rs::oracle_eval`).
fn oracle_frames(function: &str) -> Option<String> {
    oracle_select(&format!("{MODULE}:{function}()"))
}

/// [`oracle_frames`] for a function whose raise is **uncaught**: the frames of the
/// stacktrace `try MODULE:function() catch _C:_R:S -> S end` binds.
fn oracle_raise_frames(function: &str) -> Option<String> {
    oracle_select(&format!(
        "try {MODULE}:{function}() catch _C:_R:S -> S end"
    ))
}

/// [`oracle_frames`] for `process_info(P, current_stacktrace)` on a process parked
/// inside `function/0` (the oracle's own `park/0` blocks in `receive`).
fn oracle_parked_frames(function: &str) -> Option<String> {
    oracle_select(&format!(
        "begin P = spawn(fun() -> {MODULE}:{function}() end), timer:sleep(100), \
         {{current_stacktrace, S}} = erlang:process_info(P, current_stacktrace), \
         exit(P, kill), S end"
    ))
}

/// Cross-checks `engine_frames` against a live-oracle rendering of the same
/// stacktrace and **skips loudly** when `erl` is absent. Equality of the whole
/// rendered list pins the frame **count** as well as every field: a duplicated (or
/// dropped) frame makes the lists differ in length as well as content.
fn assert_frames_eq_oracle(
    test_name: &str,
    oracle: Option<String>,
    engine_frames: &[Frame],
    what: &str,
) {
    match oracle {
        Some(oracle) => assert_eq!(
            render_frames(engine_frames),
            oracle,
            "{test_name}: the engine's frames for {what} must equal the oracle's leading \
             `{MODULE}` frames"
        ),
        None => eprintln!(
            "SKIP stacktrace_frames::{test_name} oracle half: no `erl` oracle on PATH \
             — the engine-only assertions above still ran"
        ),
    }
}

/// Cross-checks the engine's frames for `function/0` against the live oracle and
/// **skips loudly** when `erl` is absent. `test_name` prefixes the skip notice;
/// every field (module, function, arity, file, line) is compared, because the
/// oracle prints them flat. Equality of the whole rendered list also pins the frame
/// **count**: a duplicated (or dropped) frame would make the lists differ in length
/// as well as content.
fn assert_matches_oracle(test_name: &str, function: &str, engine_frames: &[Frame]) {
    assert_frames_eq_oracle(
        test_name,
        oracle_frames(function),
        engine_frames,
        &format!("{MODULE}:{function}/0"),
    );
}

/// Pins one **cooked** row: `function/0`'s caught `S` must be `expected` on **both**
/// tiers (a tier is only ever compared against the same pinned frame list, which
/// makes the two tiers agree byte-for-byte), no frame may repeat, and the result is
/// cross-checked against the live oracle.
fn assert_cooked_row(function: &str, expected: &[Frame]) {
    let mut host = frames_host();
    let mut per_tier: Vec<(Tier, Vec<Frame>)> = Vec::new();
    for tier in TIERS {
        per_tier.push((tier, cooked_frames_on_tier(&mut host, function, tier)));
    }
    for (tier, frames) in &per_tier {
        assert_eq!(
            frames, expected,
            "{MODULE}:{function}/0's cooked S on {tier:?} must be {expected:?}, got {frames:?}"
        );
        assert_no_duplicate_frames(frames, &format!("{MODULE}:{function}/0 on {tier:?}"));
    }
    assert_eq!(
        per_tier[0].1, per_tier[1].1,
        "the interpreter and the native baseline tier must derive byte-identical \
         frames for {MODULE}:{function}/0"
    );
    assert_frames_eq_oracle(
        function,
        oracle_frames(function),
        &per_tier[0].1,
        &format!("{MODULE}:{function}/0"),
    );
}

/// Pins one **uncaught-raise** row: the raise `function/0` propagates must carry
/// exactly `expected` on **both** tiers, and the result is cross-checked against the
/// live oracle.
fn assert_raise_row(function: &str, expected: &[Frame]) {
    let mut host = frames_host();
    let mut per_tier: Vec<(Tier, Vec<Frame>)> = Vec::new();
    for tier in TIERS {
        per_tier.push((tier, raise_frames_on_tier(&mut host, function, tier)));
    }
    for (tier, frames) in &per_tier {
        assert_eq!(
            frames, expected,
            "{MODULE}:{function}/0's raise on {tier:?} must carry {expected:?}, got {frames:?}"
        );
        assert_no_duplicate_frames(frames, &format!("{MODULE}:{function}/0 on {tier:?}"));
    }
    assert_eq!(
        per_tier[0].1, per_tier[1].1,
        "the interpreter and the native baseline tier must derive byte-identical \
         frames for the raise out of {MODULE}:{function}/0"
    );
    assert_frames_eq_oracle(
        function,
        oracle_raise_frames(function),
        &per_tier[0].1,
        &format!("the raise out of {MODULE}:{function}/0"),
    );
}

/// Pins one **parked** row: after `function/0` parks in a `receive`, the machine's
/// `process_info(P, current_stacktrace)` value must name exactly `expected` on
/// **both** tiers, and the result is cross-checked against the live oracle.
fn assert_parked_row(function: &str, expected: &[Frame]) {
    let mut host = frames_host();
    let module = host.load(MODULE).expect("the fixture loads");
    let mut per_tier: Vec<(Tier, Vec<Frame>)> = Vec::new();
    for tier in TIERS {
        let (machine, outcome) = run_on_tier(&mut host, function, tier);
        let outcome =
            outcome.unwrap_or_else(|err| panic!("{MODULE}:{function}/0 on {tier:?}: {err:?}"));
        assert!(
            matches!(outcome, RunOutcome::Wait { .. }),
            "{function}/0 must park in `receive` on {tier:?}, got {outcome:?}"
        );
        let trace = machine
            .current_stacktrace(&module, &mut host)
            .unwrap_or_else(|err| panic!("current_stacktrace on {tier:?}: {err:?}"));
        per_tier.push((tier, read_frames(&host, trace)));
    }
    for (tier, frames) in &per_tier {
        assert_eq!(
            frames, expected,
            "{MODULE}:{function}/0's current_stacktrace on {tier:?} must be \
             {expected:?}, got {frames:?}"
        );
        assert_no_duplicate_frames(frames, &format!("{MODULE}:{function}/0 on {tier:?}"));
    }
    assert_eq!(
        per_tier[0].1, per_tier[1].1,
        "the interpreter and the native baseline tier must report byte-identical \
         frames for a machine parked in {MODULE}:{function}/0"
    );
    assert_frames_eq_oracle(
        function,
        oracle_parked_frames(function),
        &per_tier[0].1,
        &format!("a machine parked in {MODULE}:{function}/0"),
    );
}
// ---------------------------------------------------------------------------
// t0 — `try f() catch _C:_R:S -> S end`; `f/0` raises directly
// ---------------------------------------------------------------------------

#[test]
fn t0_the_cooked_stacktrace_names_f_then_t0_exactly_once() {
    // `t0/0`'s try body makes a non-tail local call into `f/0`, which raises
    // `erlang:error(boom)`. The raise snapshot is the raising frame (`f/0`) then
    // its live caller (`t0/0`) — each ONCE. A duplicated top frame (the report's
    // claim) would show `f/0` twice.
    let mut host = frames_host();
    let frames = cooked_frames(&mut host, "t0");

    assert_eq!(
        frames,
        vec![frame("f", 0, 4), frame("t0", 0, 3)],
        "t0/0's cooked S must be [[f/0,line 4], [t0/0,line 3]] — the raising \
         function then its live caller, innermost-first"
    );
    assert_no_duplicate_frames(&frames, "t0/0");
    assert_matches_oracle("t0_the_cooked_stacktrace_names_f_then_t0_exactly_once", "t0", &frames);
}

// ---------------------------------------------------------------------------
// t2 — the same shape as t0 with different names (the anti-accumulation control)
// ---------------------------------------------------------------------------

#[test]
fn t2_the_cooked_stacktrace_names_hh_then_t2_exactly_once() {
    // A second, identical chain: `t2/0`'s try body non-tail calls `hh/0`, which
    // raises. The pinned shape proves the derivation is stateless — a second chain
    // cooks to its own two frames, never appending to (or doubling) a prior one.
    let mut host = frames_host();
    let frames = cooked_frames(&mut host, "t2");

    assert_eq!(
        frames,
        vec![frame("hh", 0, 7), frame("t2", 0, 8)],
        "t2/0's cooked S must be [[hh/0,line 7], [t2/0,line 8]]"
    );
    assert_no_duplicate_frames(&frames, "t2/0");
    assert_matches_oracle("t2_the_cooked_stacktrace_names_hh_then_t2_exactly_once", "t2", &frames);
}

// ---------------------------------------------------------------------------
// t3 — the oracle's tail-call elision (`g2/0` is reused, never named)
// ---------------------------------------------------------------------------

#[test]
fn t3_the_cooked_stacktrace_elides_the_tail_called_g2() {
    // `t3/0`'s try body calls `g2/0`, and `g2/0 -> hh2() + 1` — but `sys_core_fold`
    // proves `hh2/0` (a bare `erlang:error/1`) never returns, so the `+ 1` is dead
    // and `g2/0` compiles to a **tail** call into `hh2/0` (verified with
    // `erlc -S`: `g2/0`'s body is `call_only {f,hh2}`). The oracle therefore cooks
    // `[hh2/0, t3/0]` and ELIDES `g2/0` — the tail call reused the frame. The engine
    // must reproduce that: a `g2/0` frame (or a doubled `hh2/0`) is a divergence.
    let mut host = frames_host();
    let frames = cooked_frames(&mut host, "t3");

    assert_eq!(
        frames,
        vec![frame("hh2", 0, 11), frame("t3", 0, 9)],
        "t3/0's cooked S must be [[hh2/0,line 11], [t3/0,line 9]] — the tail-called \
         g2/0 is elided, exactly as the oracle elides it"
    );
    assert_no_duplicate_frames(&frames, "t3/0");
    assert!(
        !frames.iter().any(|(_, function, ..)| function == "g2"),
        "g2/0 was tail-called by t3/0 and must not appear in the cooked S: {frames:?}"
    );
    assert_matches_oracle("t3_the_cooked_stacktrace_elides_the_tail_called_g2", "t3", &frames);
}

// ---------------------------------------------------------------------------
// The property, across all three shapes at once
// ---------------------------------------------------------------------------

#[test]
fn every_shape_cooks_frames_that_are_pairwise_distinct_and_match_the_oracle() {
    // The defect is a *cross-cutting* property (no frame is ever repeated), so pin
    // it directly for all three shapes in one place: each cooked `S` is a proper
    // list whose entries are pairwise distinct, and each equals the oracle's own
    // frames for that entry point.
    let cases = [
        ("t0", vec![frame("f", 0, 4), frame("t0", 0, 3)]),
        ("t2", vec![frame("hh", 0, 7), frame("t2", 0, 8)]),
        ("t3", vec![frame("hh2", 0, 11), frame("t3", 0, 9)]),
    ];
    let mut host = frames_host();
    for (function, expected) in &cases {
        let frames = cooked_frames(&mut host, function);
        assert_eq!(
            &frames, expected,
            "{MODULE}:{function}/0's cooked S drifted from the pinned oracle frames"
        );
        assert_no_duplicate_frames(&frames, &format!("{function}/0"));
    }
}

// ---------------------------------------------------------------------------
// A frame's continuation-pointer slot: it means "suspended in a call", and every
// consumer that follows the address clears it in the same step (C's
// `RETURN()` does `SET_I(cp_val(*E)); *E = NIL`,
// `erts/emulator/beam/emu/macros.tab:241-247`). A frame that already RETURNED
// from its call — or that a raise LANDED in — is therefore never named again.
// ---------------------------------------------------------------------------

#[test]
fn withcall_names_the_returned_from_caller_once_at_the_raising_fdiv() {
    // `withcall/0` calls `parent(ok)` (which returns), then the try body raises.
    // The caller frame is named ONCE, at the raising `fdiv` (line 16): the call
    // that already returned left no address behind, so no second `withcall/0` item
    // appears at the returned-from call site.
    assert_cooked_row("withcall", &[frame("withcall", 0, 16)]);
}

#[test]
fn inhandler_names_the_frame_once_after_the_raise_landed_in_it() {
    // `inhandler/0`'s try body makes a **non-tail** `g(0)` call, whose `fdiv`
    // raises; the raise lands in `inhandler/0`'s own catch handler, which then
    // raises again. The landing consumes the frame's slot (the handler runs *in*
    // that frame), so the second raise names `inhandler/0` ONCE — a stale slot
    // would name it a second time at the call that raised.
    assert_raise_row("inhandler", &[frame("inhandler", 0, 19)]);
}

#[test]
fn park_reports_one_frame_in_process_info_current_stacktrace() {
    // `park/0` calls `parent(ok)` (which returns) and then blocks in `receive`.
    // `process_info(P, current_stacktrace)` derives through the same chain, so the
    // parked frame is named ONCE (line 21, the `wait`) — the returned-from call is
    // not a live caller.
    assert_parked_row("park", &[frame("park", 0, 21)]);
}

#[test]
fn folded_keeps_the_original_raise_chain_without_a_re_raise_site_frame() {
    // `folded/0` is `try 1 / 0 catch C:R:S -> erlang:raise(C, R, S) end`, which
    // `sys_core_fold` rewrites into the `raw_raise` instruction. C's `raw_raise`
    // stores the supplied trace and clears `EXF_SAVETRACE`, so `build_stacktrace`
    // returns the raise's own chain verbatim: exactly ONE `folded/0` frame, at the
    // original raise (line 24), with no re-raise-site frame added.
    assert_raise_row("folded", &[frame("folded", 0, 24)]);
}

#[test]
fn refold_keeps_the_two_frame_chain_of_the_raise_it_re_raises() {
    // `refold/0` re-raises out of a try whose body made a **non-tail** `g(0)` call,
    // so the exception it re-raises carries a two-frame chain: `g/1` (the raising
    // `fdiv`, line 17) then `refold/0` (line 26, the make-or-break question: the
    // caller of the raise, *not* the `raw_raise` site). Deriving the trace at the
    // re-raise site instead would substitute `refold/0` for `g/1` and name it twice.
    assert_raise_row("refold", &[frame("g", 1, 17), frame("refold", 0, 26)]);
}