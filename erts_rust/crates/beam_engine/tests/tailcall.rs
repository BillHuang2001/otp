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

//! Tail-call frame discipline: `call_ext_last` (and its `*Last` siblings) must
//! deallocate **after** the callee, never before it.
//!
//! # The bug this suite pins
//!
//! `call_ext_last Arity Fun Deallocate` is "deallocate, then tail-call" only from
//! the *compiler's* point of view. In C it expands to two instructions —
//! `call_light_bif Bif Bif | deallocate_return D` (`emu/ops.tab:964`) — and
//! `call_light_bif` (`emu/bif_instrs.tab:262`) asserts `E == c_p->stop` across the
//! call: the frame pointer is **untouched while the BIF runs**. A raising callee
//! therefore unwinds from the live frame and `handle_error` → `next_catch`
//! (`:312`) reports the callee's own reason. Popping first (what the interpreter
//! used to do) both corrupts that unwind and rejects a `Deallocate` operand the
//! compiler legitimately left **stale**: `trim N` (`emu/instrs.tab:593`, `E += N`)
//! shrinks a frame, and the operand of a following never-returning tail call (e.g.
//! `erlang:error/1`) is then dead code C never applies.
//!
//! `tailcall:gap/1` (fixture `tests/fixtures/tailcall.erl`, compiled with the
//! system `erlc`) is exactly that shape. Before the fix it reported the frame
//! artifact —
//! `unsupported_opcode: deallocate: frame size mismatch` — instead of the BIF's
//! own reason. After the fix it reports the BIF's reason, which is also the system
//! `erl`'s.
//!
//! # Fixture
//!
//! `tests/fixtures/tailcall.beam`, produced by
//! `env -C "$TMPDIR/fixgen" erlc +deterministic tailcall.erl`. Verified generated
//! code for the two interesting functions:
//!
//! ```text
//! gap/1:   allocate 1 1 … call tailcall:hold/1 … select_val … {label,3}
//!          trim 1 0 ; move [6] x0 ; call_ext lists:reverse/1 ; … test_nil -> f=4
//!          {label,4} move unsupported_gap x0
//!                    call_ext_last 1 erlang:error/1 1   %% operand 1 > frame 0
//! tailrev/1: allocate_heap 1 2 1 … call_ext lists:reverse/1 (x2) …
//!            call_ext_last 1 lists:reverse/1 1           %% the non-raising tail call
//! loop/2:  no frame at all — call_only 2 tailcall:loop/2  (tail recursion)
//! even/1/odd/1: call_only (mutual tail recursion)
//! ```
//!
//! Every expected value is cross-checked against the system `erl` when it is
//! available on `PATH` (the tests skip that check **loudly** when it is not).

use std::process::Command;

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{make_atom, make_small, Eterm};

/// The committed oracle fixture (produced by `erlc tailcall.erl`).
const TAILCALL_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/tailcall.beam"
);

// ---------------------------------------------------------------------------
// The test context
// ---------------------------------------------------------------------------

/// A context serving exactly the three BIFs the fixture reaches, over a
/// `MinimalContext` arena.
///
/// * `erlang:error/1` — raises class `error` with the argument as a **live reason
///   term**, the shape a real BIF raise has (`beam_bifs`/`beam_rt`). This is what
///   lets the test observe *which* reason survived the unwind: the fixture passes
///   the literal atom `unsupported_gap`, so the observable reason names the
///   fixture's own failure rather than a frame-discipline artifact.
/// * `lists:reverse/1` — a real reverse over the list element reader/builder.
/// * `erlang:'-'/2` — small-int subtraction (`gc_bif '-'/2`, one per tail-recursion
///   step), counted so the tests can assert the recursion actually made progress.
struct TailCtx {
    arena: MinimalContext,
    /// How many subtraction steps the run executed.
    subs: usize,
}

impl TailCtx {
    fn new() -> Self {
        TailCtx {
            arena: MinimalContext::new(),
            subs: 0,
        }
    }
}

impl ExecutionContext for TailCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm, BeamError> {
        match (module, function, arity) {
            // A raising BIF whose reason is a term: class `error`, and the
            // argument itself as the live reason term.
            ("erlang", "error", 1) if args.len() == 1 => {
                let raise = beam_common::Raise::new(ExceptionClass::Error, ExitReason::Structured)
                    .with_live_reason(LiveReason::new(args[0]));
                Err(BeamError::Raise(raise))
            }
            ("lists", "reverse", 1) if args.len() == 1 => {
                let mut items = terms::list_elements(args[0])?;
                items.reverse();
                terms::make_list(self, &items)
            }
            ("erlang", "-", 2) if args.len() == 2 => {
                let (a, b) = (small(args[0]), small(args[1]));
                self.subs += 1;
                Ok(make_small(a - b))
            }
            _ => Err(BeamError::BadArg),
        }
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<(), BeamError> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm, BeamError> {
        self.arena.alloc_words(n)
    }
}

/// The small-integer value of a term (panics on anything else — the fixture only
/// ever subtracts small integers).
fn small(t: Eterm) -> i64 {
    beam_term::signed_value(t)
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn module() -> Module {
    load_file(TAILCALL_BEAM).expect("load tailcall.beam")
}

/// The atom term for a name interned in the module's atom table.
fn atom_term(module: &Module, name: &str) -> Eterm {
    let index = module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in the module"));
    make_atom(index)
}

/// Run `name(args.len())` in `ctx` and return the result plus the machine (kept
/// alive so boxed results stay valid).
///
/// The caller owns the context, so an **argument built through it** (the arena is
/// per-context) stays valid for the whole run — a throwaway context would free the
/// words the argument points at.
fn run_with(
    module: &Module,
    name: &str,
    args: &[Eterm],
    ctx: &mut TailCtx,
) -> (Result<Eterm, BeamError>, Machine) {
    let function = module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("function {name:?} not in the module"));
    let mut machine = Machine::new();
    let result = machine.run_export(module, function, args.len() as u32, args, ctx);
    (result, machine)
}

/// [`run_with`] over a fresh context (for arguments the fixture builds itself).
fn run(module: &Module, name: &str, args: &[Eterm]) -> (Result<Eterm, BeamError>, Machine, TailCtx) {
    let mut ctx = TailCtx::new();
    let result = run_with(module, name, args, &mut ctx);
    (result.0, result.1, ctx)
}

/// The reason term carried by a raise, if it has one.
fn live_reason(error: &BeamError) -> Option<Eterm> {
    match error {
        BeamError::Raise(raise) => raise
            .live_reason()
            .and_then(|live| live.downcast_ref::<Eterm>())
            .copied(),
        _ => None,
    }
}

/// The atom name of a raise's reason, when the raise carries no live reason term.
fn reason_atom(error: &BeamError) -> Option<&str> {
    match error {
        BeamError::Raise(raise) => raise.reason.as_atom(),
        _ => None,
    }
}

/// The list a term is, as its elements.
fn list(t: Eterm) -> Vec<Eterm> {
    terms::list_elements(t).expect("a proper list")
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

/// Run `expr` under the system `erl` with the fixture's directory on the code
/// path and return its stdout, or `None` when no `erl` is available.
fn oracle(expr: &str) -> Option<String> {
    let dir = std::path::Path::new(TAILCALL_BEAM).parent()?;
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(dir)
        .arg("-eval")
        .arg(expr)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Cross-check `expected` against the live oracle, or print a loud skip.
///
/// `expr` must print exactly `expected` (with `~p`) and then halt.
fn cross_check(expr: &str, expected: &str) {
    match oracle(expr) {
        Some(actual) => assert_eq!(
            actual, expected,
            "oracle disagrees with the pinned value for `{expr}`"
        ),
        None => eprintln!("SKIP: no system `erl` on PATH — oracle cross-check for `{expr}` skipped"),
    }
}

// ---------------------------------------------------------------------------
// (a) a raising tail call reports the BIF's own reason
// ---------------------------------------------------------------------------

#[test]
fn raising_tail_call_reports_the_bifs_own_reason() {
    // `gap(6)` trims its frame to 0 and then tail-calls `erlang:error/1` with a
    // `Deallocate` operand of 1 (stale after the trim). The BIF raises; the reason
    // the caller observes must be the BIF's own, never a frame-discipline artifact.
    let module = module();
    let (result, machine, _) = run(&module, "gap", &[make_small(6)]);

    let error = result.expect_err("gap(6) must raise");
    let reason = live_reason(&error).expect("the raise must carry the live reason term");
    assert_eq!(
        reason,
        atom_term(&module, "unsupported_gap"),
        "the raise must carry the BIF's own reason term"
    );
    match &error {
        BeamError::Raise(raise) => assert_eq!(raise.class, ExceptionClass::Error),
        other => panic!("expected a raise, got {other:?}"),
    }

    // The regression: the reason must NOT be the frame artifact the buggy ordering
    // produced. Before the fix this is what the engine reported.
    if let Some(name) = reason_atom(&error) {
        assert!(
            !name.contains("deallocate"),
            "the raise was reported as the frame-discipline artifact {name:?}"
        );
    }

    // The trim removed the only Y slot, and the raise popped nothing: the frame
    // discipline at the raise point is the fixture's (bottom frame + `gap/1`'s
    // trimmed frame), not `gap/1`'s frame already torn down.
    assert_eq!(machine.frame_depth(), 2, "gap/1's (trimmed) frame is still live");
    assert_eq!(machine.stack_depth(), 0, "the trimmed frame owns no Y slots");

    // The oracle agrees on the observable reason, and it is a bare atom.
    assert!(beam_term::is_atom(reason), "the reason is a bare atom");
    cross_check("io:format(\"~p\",[catch tailcall:gap(6)]),halt(0).", "unsupported_gap");
}

// ---------------------------------------------------------------------------
// (b) a non-raising tail call returns the value and leaves no extra frame
// ---------------------------------------------------------------------------

#[test]
fn non_raising_tail_call_returns_the_value_with_no_extra_frame() {
    // `tailrev/1` owns a 1-slot frame and ends in
    // `call_ext_last 1 lists:reverse/1 1`: the pop matches the live frame, so the
    // tail call is clean and the machine is left with only the bottom frame.
    let module = module();
    let mut ctx = TailCtx::new();
    let input = terms::make_list(
        &mut ctx,
        &[make_small(1), make_small(2), make_small(3)],
    )
    .expect("build [1,2,3]");

    let (result, machine) = run_with(&module, "tailrev", &[input], &mut ctx);
    let value = result.expect("tailrev/1 must not raise");

    // `[[1,2,3]]` — the harness's own `lists:reverse/1` result.
    let outer = list(value);
    assert_eq!(outer.len(), 1, "tailrev/1 returns a one-element list");
    let inner: Vec<i64> = list(outer[0]).into_iter().map(small).collect();
    assert_eq!(inner, vec![1, 2, 3]);

    // No leftover frame and no leftover stack words: the tail call popped exactly
    // `tailrev/1`'s frame, leaving the bottom frame.
    assert_eq!(machine.frame_depth(), 1, "only the bottom frame remains");
    assert_eq!(machine.stack_depth(), 0, "no stack words remain");

    // A second run over the same module gives the structurally identical value and
    // the identical frame discipline (nothing accumulates across runs). The
    // comparison must be structural (`list_elements`), never `Eterm` word equality:
    // each run materialises fresh cons cells in the arena, so two runs that produce
    // the *same* list from different allocations sit at different addresses and
    // their raw term words differ.
    let (again, machine2) = run_with(&module, "tailrev", &[input], &mut ctx);
    let again = again.expect("the second tailrev/1 run must not raise");
    let again_outer = list(again);
    assert_eq!(again_outer.len(), 1, "the second run returns a one-element list");
    let again_inner: Vec<i64> = list(again_outer[0]).into_iter().map(small).collect();
    assert_eq!(again_inner, inner, "the second run returns the same value");
    assert_eq!(machine2.frame_depth(), 1);
    assert_eq!(machine2.stack_depth(), 0);

    cross_check("io:format(\"~p\",[tailcall:tailrev([1,2,3])]),halt(0).", "[[1,2,3]]");
}

#[test]
fn non_raising_continuation_arm_is_unchanged() {
    // The `ok` arm of `gap/1` (N = 1, 3) reaches `deallocate 0; return` after the
    // trim — the ordinary, non-tail-call exit. It must still produce the value.
    let module = module();
    for (n, expected) in [(1i64, "one"), (3, "three"), (5, "five")] {
        let (result, machine, _) = run(&module, "gap", &[make_small(n)]);
        assert_eq!(result.expect("gap must not raise"), atom_term(&module, expected));
        assert_eq!(machine.frame_depth(), 1, "gap/{n} popped its own frame");
        assert_eq!(machine.stack_depth(), 0);
    }
    cross_check("io:format(\"~p\",[tailcall:gap(3)]),halt(0).", "three");
}

// ---------------------------------------------------------------------------
// (c) tail-recursive and mutually recursive chains run with constant stack
// ---------------------------------------------------------------------------

#[test]
fn tail_recursive_chain_runs_with_constant_stack() {
    // `loop/2` carries its accumulator in the arguments and `call_only`s itself:
    // the frame stack can never grow with the iteration count. A non-tail-call
    // rewrite would leave one frame and one stack word per iteration (an unbounded
    // `Vec` in M1), so a successful 100_000-iteration run is itself the evidence —
    // and the run leaves *nothing* behind.
    let module = module();
    let mut previous_depth = None;
    for n in [1_000i64, 100_000] {
        let (result, machine, ctx) = run(&module, "loop", &[make_small(n), beam_term::NIL]);
        let value = result.expect("loop/2 must not raise");
        let items: Vec<i64> = list(value).into_iter().map(small).collect();
        assert_eq!(items.len() as i64, n, "loop/2 built one element per iteration");
        assert_eq!(items.first().copied(), Some(1));
        assert_eq!(items.last().copied(), Some(n));
        // Progress: one `-` per iteration.
        assert_eq!(ctx.subs as i64, n, "every iteration took a subtraction step");
        // Constant stack: nothing accumulates in the machine.
        assert_eq!(machine.stack_depth(), 0, "loop/2 uses no stack words");
        assert_eq!(machine.frame_depth(), 1, "only the bottom frame remains");
        if let Some(previous) = previous_depth {
            assert_eq!(previous, machine.frame_depth(), "the depth must not grow with N");
        }
        previous_depth = Some(machine.frame_depth());
    }

    cross_check(
        "io:format(\"~p\",[length(tailcall:loop(1000,[]))]),halt(0).",
        "1000",
    );
}

#[test]
fn mutually_recursive_tail_calls_run_with_constant_stack() {
    // `even/1` ↔ `odd/1` bounce through `call_only`, the same constant-stack shape
    // across two functions.
    let module = module();
    for (function, n) in [("even", 200_000i64), ("odd", 199_999)] {
        let (result, machine, ctx) = run(&module, function, &[make_small(n)]);
        assert_eq!(
            result.expect("must not raise"),
            atom_term(&module, "true"),
            "{function}/{n}"
        );
        assert_eq!(ctx.subs as i64, n, "one step per decrement");
        assert_eq!(machine.stack_depth(), 0);
        assert_eq!(machine.frame_depth(), 1, "only the bottom frame remains");
    }
    cross_check("io:format(\"~p\",[tailcall:even(200000)]),halt(0).", "true");
}
