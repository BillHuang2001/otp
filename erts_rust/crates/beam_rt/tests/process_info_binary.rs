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

//! `erlang:process_info(Pid, binary)` for the **executing** process (contract A.8).
//!
//! The `binary` item lists a process's off-heap reference-counted binaries as
//! `[{Ptr, Size, Refc}, …]`, `[]` when it owns none. Its roster is the union of the
//! process's engine **reservoir** (`Machine::refc_binaries`, the one holder of the payloads
//! its own compiled bitstring code built) and the PCB's off-heap table (empty in
//! production).
//!
//! A process asking about **itself** is the hard shape: a scheduler pass takes the running
//! process's engine *out* of the runner state for the whole slice, so the roster a second
//! process reads is not reachable for the caller. `beam_engine`'s in-machine BIF door hands
//! the live `Machine` to `beam_rt` ([`beam_rt::context::position`]) on every BIF dispatch,
//! and that published window now carries the machine's reservoir too, so
//! `erlang:process_info(self(), binary)` lists the payload the process built earlier in the
//! very slice that asks — the oracle's answer.
//!
//! # What this suite pins
//!
//! 1. [`a_self_query_lists_the_off_heap_binary_it_just_built`] — inside one scheduled
//!    process: build a 40 000-byte binary with compiled bitstring code (`bs_create_bin`,
//!    above `ERL_ONHEAP_BINARY_LIMIT`), then read our own roster. It must list exactly one
//!    payload of 40 000 bytes with refcount 1, and the list form of the same query
//!    (`process_info(self(), [status, binary])`) must answer identically.
//! 2. [`a_self_query_with_no_off_heap_binary_answers_the_empty_list`] — the same query from a
//!    process that built nothing: `[]`, the oracle's answer.
//! 3. [`another_pids_query_keeps_the_runner_state_answer`] — a **second** process probing a
//!    parked one still goes through `RunnerState::reservoir_at` and answers the same roster.
//!
//! Every case is compared against the live system `erl` oracle as a **canonical flat integer
//! list** — `[Count, IsPositiveInt?, Size, Refc, …]` — because a payload pointer is an
//! address: the two runtimes can agree on the shape, the byte length and the refcount, never
//! on the number. The Rust side also asserts the raw term's shape (the item is the 2-tuple
//! `{binary, …}`, each element a 3-tuple of integers, the pointer positive) before reducing
//! it, so a wrong shape cannot slip through as an empty canonical list.
//!
//! Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`; recorded verbatim):
//!
//! ```none
//! $ erl -noshell -pa <scratch> -eval '<probe>.'
//! payload: [1,1,40000,1,40000]     %% self, one 40 000-byte off-heap binary
//! none:    [0]                     %% self, nothing built
//! other:   [1,1,40000,1]           %% another pid, parked holding one
//! ```
//!
//! The fixture (`tests/fixtures/pi_binary.erl`) is compiled **at test time** with the system
//! `erlc` into a scratch directory that holds only that module (a safe single `-pa` for the
//! oracle); a missing `erl`/`erlc` is a **loud** skip, never a silent pass.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_engine::terms;
use beam_rt::Runtime;
use beam_term::{make_small, signed_value, Eterm};

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "process_info_binary";

/// The fixture source (compiled at test time — never committed as a `.beam`).
const FIXTURE_ERL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/pi_binary.erl"
);

/// The payload the fixture builds, in bytes — well past
/// `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` (64), so it is a genuine off-heap refc payload.
const PAYLOAD_BYTES: i64 = 40_000;

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler: a sibling of the oracle first (the same installation), then `$PATH`.
fn resolve_erlc(oracle: &Path) -> Option<PathBuf> {
    if let Some(dir) = oracle.parent() {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding the compiled `pi_binary.beam`.
    dir: PathBuf,
    /// The compiled `pi_binary.beam`.
    beam: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test, yet a
        // cleanup failure is reported rather than swallowed.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Compiles the committed fixture into a scratch directory holding **only** that module, or
/// `None` when the system toolchain is unavailable (the caller then returns early —
/// **loudly**).
///
/// `erlc` runs with the scratch directory as cwd and the bare file name as its argument, so
/// the recorded source name is exactly `pi_binary.erl` (path-independent), and the directory
/// never holds a `lists.beam` that could shadow stdlib for the oracle.
fn build_fixture(label: &str) -> Option<(Fixture, PathBuf)> {
    let oracle = support::require_oracle(&format!("{SUITE}/{label}"))?;
    let Some(erlc) = resolve_erlc(&oracle) else {
        support::skip_loudly(
            &format!("{SUITE}/{label}"),
            "no `erlc` available (neither beside the oracle nor on $PATH)",
        );
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_{SUITE}_{label}_{}_{n}",
        std::process::id()
    ));
    let source = match std::fs::read(FIXTURE_ERL) {
        Ok(bytes) => bytes,
        Err(error) => {
            support::skip_loudly(
                &format!("{SUITE}/{label}"),
                &format!("cannot read {FIXTURE_ERL}: {error}"),
            );
            return None;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join("pi_binary.erl"), &source))
    {
        support::skip_loudly(
            &format!("{SUITE}/{label}"),
            &format!("cannot prepare {}: {error}", dir.display()),
        );
        return None;
    }
    let output = match Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg("pi_binary.erl")
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(
                &format!("{SUITE}/{label}"),
                &format!("cannot run `erlc`: {error}"),
            );
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            &format!("{SUITE}/{label}"),
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    let beam = dir.join("pi_binary.beam");
    if !beam.is_file() {
        support::skip_loudly(
            &format!("{SUITE}/{label}"),
            &format!("`erlc` produced no {}", beam.display()),
        );
        return None;
    }
    Some((Fixture { dir, beam }, oracle))
}

// ---------------------------------------------------------------------------------------
// The oracle probes
// ---------------------------------------------------------------------------------------

/// The `-eval` expression for one case: `spawn` a **fresh** process (the `-eval` process
/// itself has already built off-heap payloads of its own through `io:format`, so it is never
/// a clean subject), have it report the canonical roster back, print that one line.
///
/// Every report is a `{result, Canon}` message from a `pi_binary:report_*` wrapper, because a
/// spawned export process's return value is unobservable in Erlang.
fn oracle_probe(case: &str) -> String {
    let (spawn, wait) = match case {
        "payload" => (
            "_ = spawn(pi_binary, report_payload, [Self, 40000]),",
            "{result, R} = receive X1 -> X1 end,",
        ),
        "payload-list" => (
            "_ = spawn(pi_binary, report_payload_list, [Self, 40000]),",
            "{result, R} = receive X1 -> X1 end,",
        ),
        "none" => (
            "_ = spawn(pi_binary, report_none, [Self]),",
            "{result, R} = receive X1 -> X1 end,",
        ),
        // The holder is the subject: build one payload, wait until it is parked holding it,
        // then read its roster from a second fresh process.
        "other" => (
            "Holder = spawn(pi_binary, report_hold, [Self]),\n\
             _ = receive {ready, Holder, 40000} -> ok end,",
            "_ = spawn(pi_binary, report_other, [Self, Holder]),\n\
             {result, R} = receive X1 -> X1 end,",
        ),
        _ => unreachable!("unknown case {case}"),
    };
    format!("Self = self(),\n{spawn}\n{wait}\nio:format(\"~w~n\", [R]),\nhalt(0).")
}

/// Runs `case`'s probe under the live oracle and returns its one canonical line.
fn oracle_line(oracle: &Path, code_dir: &Path, case: &str) -> String {
    match support::oracle_eval(oracle, Some(code_dir), &oracle_probe(case)) {
        Some(stdout) => stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_else(|| panic!("the oracle printed nothing for case {case}"))
            .to_string(),
        None => panic!("the oracle probe for case {case} could not answer"),
    }
}

// ---------------------------------------------------------------------------------------
// Canonical form + the raw shape check
// ---------------------------------------------------------------------------------------

/// An integer term as an `f64` (a small directly, a bignum through the safe
/// [`beam_term::big_to_f64`] shim), or `None` when `term` is not an integer at all.
///
/// A payload pointer can sit above the 60-bit small range, so the reader is deliberately not
/// small-only: a bignum is read through the same accessor the arithmetic does.
fn integer_value(term: Eterm) -> Option<f64> {
    if beam_term::is_small(term) {
        return Some(signed_value(term) as f64);
    }
    beam_term::big_to_f64(term)
}

/// The `{binary, Roster}` item value's roster: asserts the item really is the 2-tuple whose
/// tag is the atom `binary`, and answers its second element.
///
/// This is the shape half of the atom form: an answer that lost the item tag, or that is not
/// a tuple at all, fails here instead of reducing to a misleading canonical list.
fn binary_item(runtime: &Runtime, item: Eterm) -> Eterm {
    assert_eq!(
        terms::tuple_arity(item).expect("the binary item is a 2-tuple"),
        2,
        "process_info(Pid, binary) answers {{binary, Roster}}: {item:?}"
    );
    let tag = terms::tuple_element(item, 0).expect("the item tag");
    assert_eq!(
        runtime.atom_name_of(tag),
        Some("binary"),
        "the item tag is the atom binary"
    );
    terms::tuple_element(item, 1).expect("the roster")
}

/// The **raw** roster reduced to the canonical flat list both sides compare —
/// `[Count, IsPositiveInt?, Size, Refc, …]` — after asserting its shape.
///
/// `roster` is the `[{Ptr, Size, Refc}, …]` value `process_info/2` answers (the second element
/// of the atom form, or the selected item of the list form). The assertions are the rest of
/// this suite's shape half: the value is a proper list, every element a 3-tuple of integers,
/// and the pointer positive (it is an address) — so an empty canonical list can only come
/// from an **empty** roster, never from a malformed one.
fn canon(_runtime: &Runtime, roster: Eterm) -> Vec<i64> {    let entries = terms::list_elements(roster).expect("the roster is a proper list");
    let mut flat = vec![entries.len() as i64];
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(
            terms::tuple_arity(*entry).expect("a roster entry is a 3-tuple"),
            3,
            "roster entry {index} is {{Ptr, Size, Refc}}: {entry:?}"
        );
        let field = |position: usize| {
            let term = terms::tuple_element(*entry, position)
                .unwrap_or_else(|error| panic!("roster entry {index} field {position}: {error}"));
            integer_value(term).unwrap_or_else(|| {
                panic!("roster entry {index} field {position} is not an integer: {term:?}")
            })
        };
        let (pointer, size, refcount) = (field(0), field(1), field(2));
        flat.push(if pointer > 0.0 { 1 } else { 0 });
        flat.push(size as i64);
        flat.push(refcount as i64);
    }
    flat
}
/// `values` as the text `~w` prints for the same flat integer list — `[1,1,40000,1]`.
fn render(values: &[i64]) -> String {
    let body: Vec<String> = values.iter().map(i64::to_string).collect();
    format!("[{}]", body.join(","))
}

// ---------------------------------------------------------------------------------------
// Driving the runtime
// ---------------------------------------------------------------------------------------

/// A fresh runtime with the fixture module loaded.
fn runtime_with(beam: &Path) -> Runtime {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    runtime.load_module(beam).expect("pi_binary.beam loads");
    runtime
}

/// Drives the scheduler in bounded passes until `pid` returns, and answers its result.
///
/// A pid that **died** instead is a loud failure carrying its exit reason: the pre-fix shape
/// of this suite is a wrong answer rather than a crash, but a panic here pins the difference.
fn run_to_result(runtime: &mut Runtime, pid: beam_process::ProcessId) -> Eterm {
    for _ in 0..64 {
        runtime.run_scheduler().expect("the scheduler pass");
        if let Some(result) = runtime.process_result(pid) {
            return result;
        }
        if let Some(reason) = runtime.process_exit(pid) {
            panic!("the fixture process exited instead of returning: {reason:?}");
        }
    }
    panic!(
        "the fixture process never returned within 64 passes; state = {:?}",
        runtime.processes().get(pid).map(|process| process.state())
    );
}

/// Drives the scheduler in bounded passes until `pid` is **parked** (the holder in its
/// `receive`), so a second process can probe it.
fn run_until_parked(runtime: &mut Runtime, pid: beam_process::ProcessId) {
    for _ in 0..64 {
        runtime.run_scheduler().expect("the scheduler pass");
        let state = runtime.processes().get(pid).map(|process| process.state());
        if state == Some(beam_process::ProcessState::Waiting) {
            return;
        }
        if let Some(reason) = runtime.process_exit(pid) {
            panic!("the holder exited before parking: {reason:?}");
        }
    }
    panic!(
        "the holder never parked within 64 passes; state = {:?}",
        runtime.processes().get(pid).map(|process| process.state())
    );
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// The **executing** process's own `binary` item lists the off-heap payload it just built,
/// with the oracle's `Size`/`Refc` — on the atom form **and** the list form of the query.
///
/// Before the in-machine door carried the machine's reservoir this answered `[]` (the pass has
/// the caller's engine out of the runner state), against the oracle's `[{Ptr,40000,1}]`.
#[test]
fn a_self_query_lists_the_off_heap_binary_it_just_built() {
    let Some((fixture, oracle)) = build_fixture("self") else {
        return;
    };
    for (label, function) in [("atom form", "self_payload"), ("list form", "self_payload_list")] {
        let mut runtime = runtime_with(&fixture.beam);
        let pid = runtime
            .spawn_process("pi_binary", function, vec![make_small(PAYLOAD_BYTES)])
            .unwrap_or_else(|error| panic!("spawn pi_binary:{function}/1: {error:?}"));
        let result = run_to_result(&mut runtime, pid);
        let arity = terms::tuple_arity(result).expect("{Roster, Size}");
        assert_eq!(arity, 2, "{label}: the fixture answers {{Roster, Size}}");        let roster = terms::tuple_element(result, 0).expect("the roster");
        let size = terms::tuple_element(result, 1).expect("the built binary's size");
        assert_eq!(
            integer_value(size),
            Some(PAYLOAD_BYTES as f64),
            "{label}: the fixture really built a {PAYLOAD_BYTES}-byte binary"
        );
        // The atom form answers the `{binary, Roster}` item (asserted here); the list form
        // selects that item's value itself, so both reduce through the same builder.
        let roster = if function == "self_payload" {
            binary_item(&runtime, roster)
        } else {
            roster
        };
        let mut flat = canon(&runtime, roster);
        flat.push(PAYLOAD_BYTES);        assert_eq!(
            render(&flat),
            format!("[1,1,{PAYLOAD_BYTES},1,{PAYLOAD_BYTES}]"),
            "{label}: exactly one {PAYLOAD_BYTES}-byte payload at refcount 1"
        );
        let case = if function == "self_payload" {
            "payload"
        } else {
            "payload-list"
        };
        assert_eq!(
            render(&flat),
            oracle_line(&oracle, &fixture.dir, case),
            "{label}: the self roster must match the live oracle's Size/Refc and shape"
        );    }
}

/// The same self query from a process that built no off-heap binary answers `[]` — the
/// oracle's answer — so the door never fabricates a roster.
#[test]
fn a_self_query_with_no_off_heap_binary_answers_the_empty_list() {
    let Some((fixture, oracle)) = build_fixture("none") else {
        return;
    };
    let mut runtime = runtime_with(&fixture.beam);
    let pid = runtime
        .spawn_process("pi_binary", "self_none", vec![])
        .expect("spawn pi_binary:self_none/0");
    let roster = run_to_result(&mut runtime, pid);
    let canonical = canon(&runtime, binary_item(&runtime, roster));
    assert_eq!(
        render(&canonical),
        "[0]",
        "a process holding no off-heap binary answers []"
    );    assert_eq!(
        render(&canonical),
        oracle_line(&oracle, &fixture.dir, "none"),
        "the empty roster must match the live oracle"
    );
}

/// Another pid's `binary` item keeps the runner-state route (`RunnerState::reservoir_at`) and
/// answers the same roster as the self query — the door must not change it.
#[test]
fn another_pids_query_keeps_the_runner_state_answer() {
    let Some((fixture, oracle)) = build_fixture("other") else {
        return;
    };
    let mut runtime = runtime_with(&fixture.beam);
    // The holder parks holding its payload; the sink is the parent it reports readiness to.
    let sink = runtime
        .spawn_process("pi_binary", "sink", vec![])
        .expect("spawn pi_binary:sink/0");
    let holder = runtime
        .spawn_process("pi_binary", "hold", vec![sink.to_eterm()])
        .expect("spawn pi_binary:hold/1");
    run_until_parked(&mut runtime, holder);
    assert!(
        runtime.process_result(holder).is_none(),
        "the holder is still parked"
    );
    let prober = runtime
        .spawn_process("pi_binary", "probe", vec![holder.to_eterm()])
        .expect("spawn pi_binary:probe/1");
    let roster = run_to_result(&mut runtime, prober);
    let canonical = canon(&runtime, binary_item(&runtime, roster));    assert_eq!(
        render(&canonical),
        format!("[1,1,{PAYLOAD_BYTES},1]"),
        "another process's roster is the payload it holds"
    );
    assert_eq!(
        render(&canonical),
        oracle_line(&oracle, &fixture.dir, "other"),
        "another pid's roster must match the live oracle"
    );
}
