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
//! Tier-equivalence, refusal, deopt-idempotency and heap-exhaustion pins for the
//! native `put_list` arm (`super::cons` + `emit/cons.rs`). A `#[path]` child of
//! `super::tests`, so it shares that module's harness (`with_both_tiers`,
//! `assert_same_outcome`, `body_module`, `run_counting`, `error_reason`).
//!
//! Tier agreement alone cannot prove the arm native — a deopted instruction is executed
//! by the interpreter on **both** sides — so every structural row reads the emitter's own
//! `#[cfg(test)]` counter (`emit::lowered`/`emit::declined`) over a cache-free
//! `emit::compile_function`, exactly as `call_tests.rs` does for the local `call`.
//!
//! The rows, one concern each:
//!
//! * the arm really lowers `put_list` (three conses, one counter tick each) and records
//!   no static deopt for the variant;
//! * a built chain is the list Erlang denotes — the interpreter, the baseline tier and
//!   the live `erl` all agree on `[1,2,3]`;
//! * a `Y` destination and `Y` head/tail operands take the frame-window guard path;
//! * an operand the arm cannot guard (`Literal`, `FloatReg`, an out-of-range `X`) is
//!   declined at compile time and answered by the family, identically on both tiers;
//! * a run-time-rejected arm takes **exactly one** interpreter step (the deopt is
//!   re-execution idempotent: no double allocation, no half-built cell);
//! * a refusing heap reaches the family's exact reason through the block-ABI `ERROR`
//!   word, with no partial state.
use super::*;
use crate::context::{ExecutionContext, RootSet};
use crate::jit::Tier;
use crate::machine::RunOutcome;

/// `f/0` building `[1,2,3]` from three constants, the second and third conses reading
/// their tail out of an x-register — the shape `build(N, Acc) -> build(N-1, [N|Acc])`
/// lowers to once the recursion is unrolled.
fn cons_chain_module() -> (Module, Atom) {
    body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::PutList {
            head: Operand::Integer(3),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        Instr::PutList {
            head: Operand::Integer(2),
            tail: Operand::X(0),
            dst: Operand::X(1),
        },
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ])
}

/// The lowered/declined counter pair over a **cache-free** compile of `module`'s `f/0`,
/// so the emitter's own decision is observable (`CodeCache` would re-emit nothing).
fn lowered_for(module: &Module) -> (u64, u64) {
    counters::reset();
    counters::set_enabled(true);
    super::super::emit::reset_counters();
    let labels = crate::interpreter::build_label_map(module);
    super::super::emit::compile_function(module, &labels, 0).expect("f/0 compiles");
    counters::set_enabled(false);
    (
        super::super::emit::lowered(),
        super::super::emit::declined(),
    )
}

/// Decode a proper list of the small ints `1..=3` into a `Vec`, purely through the
/// frozen term API — an anchor independent of the arm under test.
fn decode_small_chain(term: Eterm) -> Option<Vec<i64>> {
    let mut out = Vec::new();
    let mut cur = term;
    while cur != NIL {
        if !beam_term::is_list(cur) {
            return None;
        }
        let head = crate::terms::cons_head(cur).ok()?;
        let found = (1..=3i64).find(|k| head == make_small(*k))?;
        out.push(found);
        cur = crate::terms::cons_tail(cur).ok()?;
    }
    Some(out)
}

/// The oracle's `~w` rendering of an expression, or `None` when the oracle cannot
/// answer — no `erl` on `PATH`, a non-zero exit, or no answer within
/// [`ORACLE_TIMEOUT`]. The caller skips loudly: a green run without the oracle proves
/// no parity, and an oracle row must never block the suite.
///
/// `halt(0)` is part of the evaluation, not an afterthought: `erl -noshell -eval` runs
/// the expression and then **keeps running**, so a `Command::output()` on that shape
/// waits forever. The bounded wait below is the second line of defence (a loaded host
/// can make even a halting `erl` miss a generous deadline); the child is killed on
/// expiry and the row reports the skip instead of hanging.
fn erl_w(eval: &str) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    const ORACLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    let mut child = Command::new("erl")
        .args([
            "-noshell",
            "-env",
            "ERL_CRASH_DUMP",
            "/dev/null",
            "-eval",
            &format!("{eval}, halt(0)"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + ORACLE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out.trim().to_string());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!(
                        "skipping the erl row: no answer within {ORACLE_TIMEOUT:?} for `{eval}`"
                    );
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// **The arm lowers `put_list` and the variant records no static deopt.**
///
/// Three conses, three counter ticks: `lowered()` counts the arm's own decision (zero
/// bytes are emitted when the shape declines), and `instr_deopts["PutList"]` staying
/// empty is the static half of the same claim — the emitter never routed the variant to
/// `emit_deopt`. A tier-vs-tier comparison cannot show either.
#[test]
fn the_put_list_arm_is_lowered_and_records_no_static_deopt() {
    let (module, _) = cons_chain_module();
    assert_eq!(
        lowered_for(&module),
        (3, 0),
        "all three conses must be lowered in-block, none declined"
    );
    let snapshot = counters::snapshot();
    assert!(
        snapshot
            .instr_instances
            .get("PutList")
            .copied()
            .unwrap_or(0)
            >= 3,
        "the variant's instances must be counted: {:?}",
        snapshot.instr_instances
    );
    assert_eq!(
        snapshot.instr_deopts.get("PutList").copied().unwrap_or(0),
        0,
        "a native arm must record no static deopt: {:?}",
        snapshot.instr_deopts
    );
}

/// **A built chain is the list Erlang denotes, on both tiers and against the oracle.**
///
/// The fixture's `[1|[2|[3|[]]]]` is what `put_list` denotes; the oracle row pins the
/// rendering (`io:format("~w", [[1,2,3]])` ⇒ `[1,2,3]`) so the expectation is not just
/// this runtime's own opinion, and it is skipped loudly where no `erl` exists.
#[test]
fn a_put_list_chain_builds_the_list_erl_denotes() {
    let (module, name) = cons_chain_module();
    let oracle = erl_w("io:format(\"~w\", [[1,2,3]])");
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome("put_list chain", interpreter, baseline, &module.atom_table);
        for (label, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
            let RunOutcome::Returned(value) = outcome.as_ref().expect("the run completes") else {
                panic!("{label}: unexpected outcome {outcome:?}");
            };
            let decoded = decode_small_chain(*value)
                .unwrap_or_else(|| panic!("{label}: expected a proper list of small ints"));
            assert_eq!(decoded, vec![1, 2, 3], "{label}: the chain's element order");
            match &oracle {
                Some(text) => assert_eq!(
                    *text,
                    format!(
                        "[{}]",
                        decoded
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                    "{label}: the built list must be the one Erlang denotes"
                ),
                None => eprintln!("skipping the erl row: no system `erl` on PATH"),
            }
        }
    });
}

/// **A `Y` destination and `Y` head/tail operands take the frame-window guard path.**
///
/// The block pushes a frame (`allocate`, one slot) and the arm then stores into `Y(0)`
/// and reads `Y(0)` as both a head and a tail — three `Loc::Y` operands, each preceded by
/// the emitted guard. The arm still lowers (compile-time `Y` addressability is all the
/// shape needs) and both tiers agree on the result, which is the run-time half.
#[test]
fn put_list_y_operands_take_the_frame_window_guard_path() {
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(1),
            live: 0,
        },
        Instr::PutList {
            head: Operand::Integer(7),
            tail: Operand::Nil,
            dst: Operand::Y(0),
        },
        Instr::PutList {
            head: Operand::Y(0),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        Instr::PutList {
            head: Operand::Integer(9),
            tail: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]);
    assert_eq!(
        lowered_for(&module),
        (3, 0),
        "an addressable Y operand must not decline the shape"
    );
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome("put_list on Y", interpreter, baseline, &module.atom_table);
        for (label, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
            let RunOutcome::Returned(value) = outcome.as_ref().expect("the run completes") else {
                panic!("{label}: unexpected outcome {outcome:?}");
            };
            let tail = crate::terms::cons_tail(*value).expect("a cons cell");
            assert_eq!(
                crate::terms::cons_head(*value).expect("a cons cell"),
                make_small(9),
                "{label}: the head must be the `9` operand"
            );
            assert_eq!(
                crate::terms::cons_head(tail).expect("the Y(0) list's head"),
                make_small(7),
                "{label}: `Y(0)` must hold the `[7]` the first cons built"
            );
        }
    });
}

/// **An operand the arm cannot guard is declined at compile time and answered by the
/// family, identically on both tiers.**
///
/// `emit/cons.rs`'s shape is purely syntactic: every operand must be a `Loc` or an
/// immediate constant, so a `Literal`, a `FloatReg` and an out-of-range `X` each emit
/// **zero bytes**, fall through to `emit_family`, and reach the same family body the
/// interpreter runs. The counter row is the point — tier agreement alone would pass even
/// if the arm had silently stopped lowering.
#[test]
fn a_put_list_operand_the_arm_cannot_guard_is_declined_and_takes_the_family_path() {
    let cases = [
        (
            "literal head",
            Operand::Literal(0),
            Operand::Nil,
            Operand::X(0),
        ),
        (
            "float-register head",
            Operand::FloatReg(0),
            Operand::Nil,
            Operand::X(0),
        ),
        (
            "out-of-range head",
            Operand::X(100_000),
            Operand::Nil,
            Operand::X(0),
        ),
        (
            "out-of-range destination",
            Operand::Integer(1),
            Operand::Nil,
            Operand::X(100_000),
        ),
    ];
    for (label, head, tail, dst) in cases {
        let (module, name) = body_module(vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::PutList { head, tail, dst },
            Instr::Return,
        ]);
        assert_eq!(
            lowered_for(&module),
            (0, 1),
            "{label}: the shape must decline and emit zero bytes"
        );
        with_both_tiers(&module, name, |interpreter, baseline| {
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        });
    }
}

/// **A run-time-rejected arm takes exactly one interpreter step.**
///
/// The destination is an *addressable* `Y` slot (so the shape lowers) inside a block that
/// pushes no frame, so the emitted frame-window guard rejects at run time and the
/// `fallback` runs one `jit_step_one` — which re-executes the whole instruction and hits
/// the family's own `bad_register`, exactly as the interpreter does. `step_one == 1` is
/// the once-only pin: a second step would mean the instruction ran twice, i.e. an
/// already-committed (allocated) cons committed again.
#[test]
fn a_run_time_rejected_put_list_takes_exactly_one_interpreter_step() {
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::PutList {
            head: Operand::Integer(5),
            tail: Operand::Nil,
            dst: Operand::Y(0),
        },
        Instr::Return,
    ]);
    assert_eq!(
        lowered_for(&module),
        (1, 0),
        "`Y(0)` is addressable, so the shape lowers and the guard decides at run time"
    );
    let (interpreter, interpreter_counters) = run_counting(&module, name, Tier::Interpreter);
    let (baseline, baseline_counters) = run_counting(&module, name, Tier::BaselineJit);
    assert!(
        interpreter.is_err() && baseline.is_err(),
        "a destination outside the published frame must raise on both tiers"
    );
    assert_same_outcome(
        "rejected put_list",
        &interpreter,
        &baseline,
        &module.atom_table,
    );
    assert_eq!(
        baseline_counters.step_one, 1,
        "the rejected arm must run the interpreter step exactly once"
    );
    assert_eq!(
        interpreter_counters.step_one, 0,
        "the interpreter tier takes no deopt step"
    );
}

/// A host whose heap is permanently exhausted: every rooted allocation is the typed
/// refusal, everything else forwards to a real [`MinimalContext`].
struct RefusingHeap {
    inner: MinimalContext,
}

/// The refusal text the arm must surface unchanged through the block-ABI `ERROR` word.
const REFUSED: &str = "host: heap exhausted";

impl ExecutionContext for RefusingHeap {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> beam_common::Result<Eterm> {
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> beam_common::Result<()> {
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> beam_common::Result<*mut Eterm> {
        let _ = n;
        Err(crate::error::unsupported(REFUSED))
    }

    fn alloc_words_rooted(
        &mut self,
        n: usize,
        _roots: RootSet<'_>,
    ) -> beam_common::Result<*mut Eterm> {
        let _ = n;
        Err(crate::error::unsupported(REFUSED))
    }
}

/// **A refusing heap reaches the family's exact reason with no partial state.**
///
/// The cons allocation is the rooted door (`terms::make_cons_rooted` →
/// `alloc_words_rooted`), so an exhausted heap makes the family raise before any word is
/// written; the arm must carry that error out as the block-ABI `ERROR` word with the
/// host's own error in `JitRuntime::error`, and the interpreter must produce the same
/// reason. A destination written (or a half-built cell) would show up as a divergence
/// here, because `put_list` writes `dst` only after the build succeeds.
#[test]
fn a_refusing_heap_raises_the_interpreters_exact_reason() {
    fn reason(module: &Module, name: Atom, tier: Tier) -> String {
        let mut host = RefusingHeap {
            inner: MinimalContext::new(),
        };
        let mut machine = Machine::new();
        match machine.run_export_outcome_with_tier(module, name, 0, &[], &mut host, tier) {
            Ok(outcome) => panic!("expected a raise, got {outcome:?}"),
            Err(err) => error_reason(&err),
        }
    }
    let (module, name) = cons_chain_module();
    let interpreter = reason(&module, name, Tier::Interpreter);
    let baseline = reason(&module, name, Tier::BaselineJit);
    assert_eq!(
        interpreter, baseline,
        "the arm must surface the family's error unchanged"
    );
    assert!(
        interpreter.contains("heap exhausted"),
        "the host's own reason must survive: {interpreter}"
    );
}
