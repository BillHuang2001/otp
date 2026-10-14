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

use std::process::Command;

use beam_engine::terms;
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

/// Runs `MODULE:function/0` and returns the frames of its cooked `S`.
fn cooked_frames(host: &mut ModuleHost, function: &str) -> Vec<Frame> {
    let s = host
        .call(MODULE, function, &[])
        .unwrap_or_else(|err| panic!("{MODULE}:{function}/0 must return its caught S, got {err:?}"));
    terms::list_elements(s)
        .unwrap_or_else(|err| panic!("{function}/0's caught S must be a proper list: {err:?}"))
        .iter()
        .map(|&item| frame_fields(host, item))
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
    let eval = format!(
        "Pick = fun(L) -> [X || {{{MODULE},_,_,_}} = X <- L] end, \
         Show = fun(L) -> lists:flatten(lists:join(\";\", \
         [io_lib:format(\"~s|~s|~w|~s|~w\", [M, atom_to_list(FN), A, \
         proplists:get_value(file, Loc), proplists:get_value(line, Loc)]) \
         || {{M,FN,A,Loc}} <- L])) end, \
         io:format(\"~s\", [Show(Pick({MODULE}:{function}()))]), halt()."
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

/// Cross-checks the engine's frames for `function/0` against the live oracle and
/// **skips loudly** when `erl` is absent. `test_name` prefixes the skip notice;
/// every field (module, function, arity, file, line) is compared, because the
/// oracle prints them flat. Equality of the whole rendered list also pins the frame
/// **count**: a duplicated (or dropped) frame would make the lists differ in length
/// as well as content.
fn assert_matches_oracle(test_name: &str, function: &str, engine_frames: &[Frame]) {
    match oracle_frames(function) {
        Some(oracle) => assert_eq!(
            render_frames(engine_frames),
            oracle,
            "{test_name}: the engine's cooked S for {MODULE}:{function}/0 must equal \
             the oracle's leading `{MODULE}` frames"
        ),
        None => eprintln!(
            "SKIP stacktrace_frames::{test_name} oracle half: no `erl` oracle on PATH \
             — the engine-only assertions above still ran"
        ),
    }
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
