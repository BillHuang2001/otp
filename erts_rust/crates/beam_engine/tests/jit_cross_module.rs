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

//! **The native tiers' in-machine module-context handling, pinned
//! interpreter-identical on REAL `erlc`-produced modules.**
//!
//! `tests/external_call.rs` proves the cross-module seam (`Step::Enter`, the
//! driver-mediated module-context switch) on hand-built synthetic bytecode and
//! `tests/jit_real_module.rs` proves the tiers are observationally the interpreter
//! over the whole installed OTP corpus -- but neither pins a **native tier's
//! module-context machinery itself** on a real compiled module: the driver
//! round-trip that resolves a `call_ext*` through
//! [`ExecutionContext::resolve_external_call`], registers the callee as a module
//! context, and re-enters the callee's own compiled blocks
//! (`NativeCode::module_blocks` -> the per-thread `CodeCache`).
//!
//! This suite is that independent engine-level proof: every fixture is a tiny
//! `.beam` compiled from source by the **system `erlc`** at test time (the
//! differential gate's own toolchain), and every case is driven through the tiers
//! this build compiles in over the *same* [`ExecutionContext`] -- always
//! [`Tier::Interpreter`] and [`Tier::BaselineJit`], plus the optional
//! [`Tier::Cranelift`] hot tier when the **`jit-cranelift`** feature is enabled
//! (the roster is [`NATIVE_TIERS`], the same feature-gated shape
//! `tests/jit_coverage.rs` drives). Every native tier must agree with the
//! interpreter on the [`RunOutcome`] -- the **same value** (compared with
//! `beam_term::cmp`, the module's exact `=:=` ordering, so boxed
//! tuples/lists compare by value) or the **same** suspension address and the same
//! typed error shape. A value on one tier and an error on another **fails**, as
//! do two *different* errors or two different suspension addresses.
//!
//! # Tier coverage
//!
//! The roster is a build-time property, not a per-case choice:
//!
//! - [`Tier::Interpreter`] and [`Tier::BaselineJit`] in **every** build. The
//!   baseline tier is the one that makes the cross-module seam *native*: a resolved
//!   `call_ext*` leaves its compiled block as a driver-mediated module-context
//!   transfer and the callee's own blocks are compiled on first sight
//!   (`NativeCode::module_blocks` -> the per-thread `CodeCache`).
//! - [`Tier::Cranelift`] additionally with the **`jit-cranelift`** Cargo feature
//!   (default OFF), on every case below **except** the parked-suspension row. That
//!   tier does **not** accelerate a cross-module callee: it runs each `call_ext*`
//!   through the same shared interpreter-backed per-instruction helper, so the
//!   claim it carries here is **correctness** (the seam's callee resolution, the
//!   recursive module-context re-entry and the dynamic `apply` decode all produce
//!   the interpreter's answer), never acceleration.
//! - The parked-suspension row (`a_slice_parked_in_a_cross_module_callee_resumes_on_the_same_tier`)
//!   deliberately stays on the interpreter and the baseline tier: Cranelift cannot
//!   resume at a mid-function `pc` and answers the honest typed `unsupported` from
//!   [`Machine::resume_outcome_with_tier`], so a *same-tier resume* is only
//!   expressible on the two tiers that support it (see `beam_engine/CONTEXT.md`,
//!   "The JIT tier seam").
//!
//! # The cases
//!
//! 1. **A native block makes a cross-module call.** `crosscall_a:run/1` compiles to
//!    a `gc_bif '*'/2` (inlined natively) followed by a `call_ext_only` to
//!    `crosscall_b:add_one/1` -- a *resolved* cross-module call, so the transfer is
//!    the driver-mediated module-context switch into the callee's compiled blocks.
//! 2. **A nested A -> B -> C chain with a module entered recursively.**
//!    `chain_a:run/1` -> `chain_b:walk/1` (which recurses into **itself** while its
//!    own context is live, each level calling `chain_c:inc_twice/1`) ->
//!    `chain_c:inc_twice/1` -> `chain_c:inc/1` (a second, **recursive** entry into
//!    the `chain_c` context). Both `chain_b` and `chain_c` are therefore entered
//!    again while already on the module-context stack.
//! 3. **A slice parked inside a cross-module callee, resumed on the SAME tier.**
//!    `park_a:run/0` tail-calls `park_b:await/0`, whose `receive` finds the mailbox
//!    empty and reports [`RunOutcome::Wait`] with the callee's own address. The
//!    suspension is asserted to be in a **non-entry** module context
//!    ([`Machine::current_module`] is `park_b`); the process is then resumed **on the
//!    same tier** through `Machine::resume_outcome_with_tier` after a message is
//!    delivered, and both tiers must yield the delivered term. This is the one row
//!    restricted to the interpreter and the baseline tier (see *Tier coverage*).
//! 4. **A dynamic `apply/3` on registers.** `apply_a:run/3` tail-calls
//!    `erlang:apply/3` with `M`/`F`/`A` read out of `x(0)`/`x(1)`/`x(2)`; the engine's
//!    `fixed_apply` special case decodes them and resolves the *computed* target
//!    through the same seam, so the applied callee runs in this machine.
//!
//! # The host
//!
//! [`SeamHost`] wraps the shared `tests/support` [`ModuleHost`] (one term arena,
//! one shared atom universe, the small-arithmetic BIF floor) and adds the two things
//! this suite needs: a `resolve_external_call` implementation over the loaded
//! fixture handles (so a `call_ext*` runs its callee **in the calling machine**) and
//! a mailbox the test can enqueue into from the outside (for case 3).
//!
//! # Skipping
//!
//! Every fixture is compiled by the system `erlc` at test time. When `erlc` cannot be
//! **spawned** at all (absent toolchain) the suite **skips loudly** -- it never
//! silently passes -- but because every expected value below is cross-checked against
//! the real `erl` at authoring time, a green run in that environment proves nothing
//! and says so on stderr.

mod support;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use beam_common::Result;
use beam_engine::jit::cache::baseline_code_map;
use beam_engine::{terms, ExecutionContext, Machine, RunOutcome, Tier};
use beam_loader::{Label, Module};
use beam_term::{cmp, make_small, Eterm};

use support::ModuleHost;

// ---------------------------------------------------------------------------
// The fixture sources, compiled at test time by the system `erlc`
// ---------------------------------------------------------------------------

/// `crosscall_a:run/1` -- a native `gc_bif '*'/2` then a resolved cross-module tail
/// call (`call_ext_only`). `run0/0` is the same cross-module call with an
/// immediate argument (so the *caller* has no arithmetic, only the `call_ext_only`).
const CROSSCALL_A: &str = "\
-module(crosscall_a).
-export([run/1, run0/0]).
run(N) ->
    X = N * 2,
    crosscall_b:add_one(X).
run0() ->
    crosscall_b:add_one(41).
";

/// The cross-module callee: `gc_bif '+'/2` then a tuple build (`test_heap` +
/// `put_tuple2`) -- a value the caller could only obtain by actually entering this
/// module's code.
const CROSSCALL_B: &str = "\
-module(crosscall_b).
-export([add_one/1]).
add_one(N) ->
    {ok, N + 1}.
";

/// The head of the A -> B -> C chain.
const CHAIN_A: &str = "\
-module(chain_a).
-export([run/1]).
run(N) ->
    chain_b:walk(N).
";

/// The middle of the chain: `walk/1` recurses into **itself** (a local,
/// non-tail `call`, so `chain_b` stays a live module context across its own
/// re-entry) and each level cross-module-calls `chain_c:inc_twice/1`.
const CHAIN_B: &str = "\
-module(chain_b).
-export([walk/1]).
walk(0) ->
    0;
walk(N) ->
    chain_c:inc_twice(walk(N - 1)).
";

/// The tail of the chain: `inc_twice/1` calls `inc/1` **twice** (a local `call`
/// then a local tail `call_last`), so the `chain_c` module context is re-entered
/// while its own frame is still live.
const CHAIN_C: &str = "\
-module(chain_c).
-export([inc_twice/1]).
inc_twice(X) ->
    inc(inc(X)).
inc(X) ->
    X + 1.
";

/// The entry module of the parked case: a bare cross-module tail call.
const PARK_A: &str = "\
-module(park_a).
-export([run/0]).
run() ->
    park_b:await().
";

/// The callee that parks: a `receive` with an empty mailbox (so the calling process
/// suspends inside *this* module's context).
const PARK_B: &str = "\
-module(park_b).
-export([await/0]).
await() ->
    receive
        X -> X
    end.
";

/// The dynamic-apply entry: `apply/3` on the `M`/`F`/`A` registers.
const APPLY_A: &str = "\
-module(apply_a).
-export([run/3]).
run(M, F, A) ->
    apply(M, F, A).
";

/// Every fixture: `(module name, source)`, compiled together into one directory.
const FIXTURES: &[(&str, &str)] = &[
    ("crosscall_a", CROSSCALL_A),
    ("crosscall_b", CROSSCALL_B),
    ("chain_a", CHAIN_A),
    ("chain_b", CHAIN_B),
    ("chain_c", CHAIN_C),
    ("park_a", PARK_A),
    ("park_b", PARK_B),
    ("apply_a", APPLY_A),
];

/// Compiles every fixture into a fresh temp directory (once per test binary) and
/// returns it; `None` when no runnable `erlc` is on `PATH` at all (the caller skips
/// loudly).
///
/// As in `tests/trace.rs`: `None` means `erlc` could not be **spawned**. An `erlc`
/// that *ran* and exited non-zero is a **harness** failure (the sources are valid
/// Erlang), so it panics with the captured stderr rather than masquerading as an
/// absent toolchain.
fn fixture_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join("beam_engine_jit_cross_module");
        std::fs::create_dir_all(&dir).ok()?;
        let mut sources = Vec::new();
        for (name, source) in FIXTURES {
            let file = dir.join(format!("{name}.erl"));
            std::fs::write(&file, source)
                .unwrap_or_else(|error| panic!("write {}: {error}", file.display()));
            sources.push(format!("{name}.erl"));
        }
        let output = Command::new("erlc")
            .arg("+deterministic")
            .args(&sources)
            .current_dir(&dir)
            .output()
            .ok()?;
        assert!(
            output.status.success(),
            "the system `erlc` exited {} compiling the jit_cross_module fixtures\n  stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Some(dir)
    })
    .as_ref()
}

/// Prints the loud skip and returns `true` when the oracle toolchain is unavailable.
fn skip_if_unavailable() -> bool {
    match fixture_dir() {
        Some(_) => false,
        None => {
            eprintln!(
                "SKIP: no usable `erlc` on PATH -- the jit_cross_module fixtures could not be \
                 compiled, so the baseline tier's module-context handling is NOT exercised by \
                 this run"
            );
            true
        }
    }
}

// ---------------------------------------------------------------------------
// The host: ModuleHost + the cross-module seam + an externally-fed mailbox
// ---------------------------------------------------------------------------

/// The mailbox: a FIFO of messages plus the scan position the receive family drives
/// (`receive_next`/`receive_skip`/`receive_remove`/`receive_reset`).
///
/// The model is the one the receive seam's ordering invariant requires: ordering is
/// FIFO and the scan index only ever saturates, so a suspended receive re-reads from
/// the position it was exhausted at (`tests/receive.rs` pins the same shape on the
/// production `beam_message` queue).
struct MailboxState {
    messages: VecDeque<Eterm>,
    scan: usize,
}

impl MailboxState {
    fn new() -> MailboxState {
        MailboxState {
            messages: VecDeque::new(),
            scan: 0,
        }
    }
}

/// The shared `tests/support` [`ModuleHost`], plus the two seams this suite needs:
/// `resolve_external_call` over the loaded fixture handles (so a `call_ext*` the host
/// can serve runs its callee **in the calling machine**) and a mailbox the test can
/// feed from the outside.
struct SeamHost {
    /// The arena/BIF-floor/shared-atom loader. Every accessor below forwards to it.
    inner: ModuleHost,
    /// Every loaded fixture, keyed by module name -- the resolver's table.
    modules: Vec<(String, Arc<Module>)>,
    /// The mailbox, shared with the test (the test enqueues; the machine consumes).
    mailbox: Rc<RefCell<MailboxState>>,
}

impl SeamHost {
    /// A host that has loaded every fixture in `names` from `dir`.
    fn new(dir: &Path, names: &[&str]) -> SeamHost {
        let mut inner = ModuleHost::new(dir.to_path_buf());
        for name in names {
            inner.register(name, dir.join(format!("{name}.beam")));
        }
        let modules = names
            .iter()
            .map(|name| {
                let module = inner
                    .load(name)
                    .unwrap_or_else(|error| panic!("load the `{name}` fixture: {error:?}"));
                ((*name).to_string(), module)
            })
            .collect();
        SeamHost {
            inner,
            modules,
            mailbox: Rc::new(RefCell::new(MailboxState::new())),
        }
    }

    /// The shared handle of the loaded fixture `name` (the exact `Arc<Module>` the
    /// resolver answers with, so a machine's module context can be identified by
    /// `Arc::ptr_eq`).
    fn module(&self, name: &str) -> Arc<Module> {
        Arc::clone(
            &self
                .modules
                .iter()
                .find(|(module, _)| module == name)
                .unwrap_or_else(|| panic!("no `{name}` fixture was loaded"))
                .1,
        )
    }

    /// Interns `name` in the host's one shared atom table and returns its atom term.
    fn intern(&mut self, name: &str) -> Eterm {
        self.inner.intern(name)
    }

    /// The one shared atom table (what term comparison resolves names through).
    fn atom_table(&self) -> &beam_term::AtomTable {
        self.inner.atom_table()
    }

    /// Delivers `message` to the mailbox (the test-side `!`).
    fn enqueue(&self, message: Eterm) {
        self.mailbox.borrow_mut().messages.push_back(message);
    }

    /// Empties the mailbox and resets the scan position -- so the two tiers of the
    /// parked case (case 3) each start from a genuinely empty mailbox.
    fn clear_mailbox(&self) {
        let mut mailbox = self.mailbox.borrow_mut();
        mailbox.messages.clear();
        mailbox.scan = 0;
    }
}

impl ExecutionContext for SeamHost {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }

    /// The cross-module-call seam: answer a `call_ext*` (or a dynamic `apply/3`
    /// target) naming one of the loaded fixtures with that module's **shared**
    /// `Arc<Module>` and its export entry label, so the engine enters the callee in
    /// this machine. Anything else (`erlang:*` BIFs, an unknown module) keeps the
    /// synchronous `call_bif` path.
    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        let Some((_, callee)) = self.modules.iter().find(|(name, _)| name == module) else {
            return Ok(None);
        };
        let callee = Arc::clone(callee);
        let Some(function_atom) = callee.atom_table.lookup(function) else {
            return Ok(None);
        };
        Ok(callee.export(function_atom, arity).map(|label| (callee, label)))
    }

    fn send_message(&mut self, _to: Eterm, message: Eterm) -> Result<()> {
        self.mailbox.borrow_mut().messages.push_back(message);
        Ok(())
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        let mailbox = self.mailbox.borrow();
        Ok(mailbox.messages.get(mailbox.scan).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        let mut mailbox = self.mailbox.borrow_mut();
        if mailbox.scan < mailbox.messages.len() {
            mailbox.scan += 1;
        }
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        let mut mailbox = self.mailbox.borrow_mut();
        let scan = mailbox.scan;
        if scan < mailbox.messages.len() {
            mailbox.messages.remove(scan);
        }
        mailbox.scan = 0;
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.mailbox.borrow_mut().scan = 0;
        Ok(())
    }

    // A single-clause `receive` compiles no `recv_marker_*` instruction (see the
    // `park_b.await/0` disassembly), so the marker seam is a no-op here.
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        Ok(beam_term::NIL)
    }

    fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> Result<()> {
        Ok(())
    }

    fn receive_marker_use(&mut self, _reference: Eterm) -> Result<()> {
        Ok(())
    }

    fn receive_marker_clear(&mut self, _reference: Eterm) -> Result<()> {
        Ok(())
    }
}

/// A host over every fixture in [`FIXTURES`].
fn fixture_host() -> SeamHost {
    let dir = fixture_dir().expect("the caller guarded on `skip_if_unavailable`");
    let names: Vec<&str> = FIXTURES.iter().map(|(name, _)| *name).collect();
    SeamHost::new(dir, &names)
}

// ---------------------------------------------------------------------------
// The multi-tier driver
// ---------------------------------------------------------------------------

/// The native tiers this build has to check the interpreter against.
///
/// The baseline tier is in every build; the Cranelift hot tier joins the roster
/// with the **`jit-cranelift`** Cargo feature (the same feature-gated roster
/// `tests/jit_coverage.rs` drives). Both run the *same* shared per-instruction
/// interpreter-backed helper for everything they do not inline, so every case below
/// that uses [`run_both`] asserts tier-vs-interpreter equivalence for whichever
/// tiers are compiled in.
///
/// The one row that does **not** use this roster is the parked-suspension case,
/// because Cranelift has no mid-function resume (see the module docs).
#[cfg(feature = "jit-cranelift")]
const NATIVE_TIERS: [Tier; 2] = [Tier::BaselineJit, Tier::Cranelift];

/// The native tiers this build has to check the interpreter against (baseline only).
#[cfg(not(feature = "jit-cranelift"))]
const NATIVE_TIERS: [Tier; 1] = [Tier::BaselineJit];

/// One target's run through one tier: the outcome plus the machine that produced it.
///
/// The machine is kept so the parked case can inspect its module context and resume
/// **it** (the resume must not reset).
struct TierRun {
    outcome: Result<RunOutcome>,
    machine: Machine,
}

/// Runs `function/arity` on `module` through `tier`, on a fresh machine, over `host`.
fn run_tier(
    host: &mut SeamHost,
    module: &Arc<Module>,
    function: &str,
    args: &[Eterm],
    tier: Tier,
) -> TierRun {
    let function_atom = module.atom_table.lookup(function).unwrap_or_else(|| {
        panic!(
            "module `{}` does not intern `{function}`",
            module.atom_name(module.name).unwrap_or("?")
        )
    });
    let mut machine = Machine::new();
    let outcome = machine.run_export_outcome_with_tier(
        &**module,
        function_atom,
        args.len() as u32,
        args,
        host,
        tier,
    );
    TierRun { outcome, machine }
}

/// Asserts one native tier's outcome is **the same observation** as the
/// interpreter's -- the same value (under `beam_term::cmp`), the same suspension
/// address, or the same typed error shape -- panicking loudly on any divergence.
///
/// Every message names the tier under test (`interpreter vs <tier name>`) so a
/// divergence says *which* native tier it was.
fn assert_tiers_agree(
    host: &SeamHost,
    label: &str,
    interp: &TierRun,
    native: &TierRun,
    tier: Tier,
) {
    let name = tier.name();
    match (&interp.outcome, &native.outcome) {
        (Ok(RunOutcome::Returned(expected)), Ok(RunOutcome::Returned(got))) => assert!(
            cmp(*got, *expected, host.atom_table()).is_eq(),
            "TIER DIVERGENCE for `{label}` (interpreter vs {name}): the tiers returned \
             DIFFERENT terms:\n  interpreter: {expected:?}\n  {name}: {got:?}"
        ),
        (
            Ok(RunOutcome::Wait {
                func: interp_func,
                pc: interp_pc,
            }),
            Ok(RunOutcome::Wait {
                func: native_func,
                pc: native_pc,
            }),
        ) => assert_eq!(
            (interp_func, interp_pc),
            (native_func, native_pc),
            "TIER DIVERGENCE for `{label}` (interpreter vs {name}): the tiers suspended at \
             DIFFERENT addresses"
        ),
        (Ok(got), Ok(other)) => panic!(
            "TIER DIVERGENCE for `{label}` (interpreter vs {name}): the tiers agreed on an \
             outcome -- one returned, the other suspended:\n  interpreter: {got:?}\n  {name}: \
             {other:?}"
        ),
        (Ok(got), Err(error)) => panic!(
            "TIER DIVERGENCE for `{label}` (interpreter vs {name}): the interpreter reported \
             {got:?} while this tier errored: {error:?}"
        ),
        (Err(error), Ok(got)) => panic!(
            "TIER DIVERGENCE for `{label}` (interpreter vs {name}): the tier reported {got:?} \
             while the interpreter errored: {error:?}"
        ),
        (Err(expected), Err(got)) => assert_eq!(
            format!("{got:?}"),
            format!("{expected:?}"),
            "TIER DIVERGENCE for `{label}` (interpreter vs {name}): both tiers errored, but \
             with DIFFERENT typed errors (this compares the full error shape, not \
             `is_err()`):\n  interpreter: {expected:?}\n  {name}: {got:?}"
        ),
    }
}

/// Drives one `module:function(args)` through the interpreter and through **every**
/// native tier in [`NATIVE_TIERS`] over one host, asserting each native run agrees
/// with the interpreter, and returns `(interpreter, baseline)`.
///
/// The second element is always the [`Tier::BaselineJit`] run -- the first entry of
/// [`NATIVE_TIERS`] in every build -- so a call site that cares about the
/// *accelerated* cross-module path keeps its handle while the Cranelift hot tier is
/// checked purely for equivalence.
fn run_both(
    host: &mut SeamHost,
    module: &Arc<Module>,
    function: &str,
    args: &[Eterm],
    label: &str,
) -> (TierRun, TierRun) {
    let interp = run_tier(host, module, function, args, Tier::Interpreter);
    let mut baseline = None;
    for tier in NATIVE_TIERS {
        let native = run_tier(host, module, function, args, tier);
        assert_tiers_agree(host, label, &interp, &native, tier);
        if baseline.is_none() {
            baseline = Some(native);
        }
    }
    let baseline = baseline.expect("`NATIVE_TIERS` always starts with the baseline tier");
    (interp, baseline)
}

/// The returned value of an outcome the tiers already agree on.
fn value_of(run: &TierRun, label: &str) -> Eterm {
    match &run.outcome {
        Ok(RunOutcome::Returned(value)) => *value,
        other => panic!("`{label}` was expected to return a value, got {other:?}"),
    }
}
/// Asserts `value` is the term `expected` (under the host's exact ordering).
fn assert_value(host: &SeamHost, value: Eterm, expected: Eterm, label: &str) {
    assert!(
        cmp(value, expected, host.atom_table()).is_eq(),
        "`{label}` computed the wrong term: got {value:?}, expected {expected:?}"
    );
}

/// Builds the `{ok, N}` term on the host's arena (the shape `crosscall_b` returns).
fn expected_ok(host: &mut SeamHost, n: i64) -> Eterm {
    let ok = host.intern("ok");
    terms::make_tuple(host, &[ok, make_small(n)]).expect("build the expected `{ok,N}` term")
}

// ---------------------------------------------------------------------------
// (1) a native block making a cross-module call
// ---------------------------------------------------------------------------

/// `crosscall_a:run/1` (`N * 2` then `crosscall_b:add_one/1`) and `run0/0` (the bare
/// cross-module tail call), on the interpreter and on every native tier in
/// [`NATIVE_TIERS`].
///
/// The native tier executes the caller's `gc_bif '*'/2` inlined and hands the
/// `call_ext_only` to the driver as a module-context transfer into `crosscall_b`'s
/// compiled blocks; the interpreter resolves the same seam from its own dispatch
/// arm. All of them must observe `{ok, 15}` / `{ok, 42}`.
#[test]
fn a_native_block_makes_a_resolved_cross_module_call_across_tiers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    let caller = host.module("crosscall_a");

    let args = [make_small(7)];
    let (interp, jit) = run_both(&mut host, &caller, "run", &args, "crosscall_a:run/1");
    let expected = expected_ok(&mut host, 15);
    assert_value(&host, value_of(&interp, "interpreter"), expected, "crosscall_a:run(7)");
    assert_value(&host, value_of(&jit, "baseline"), expected, "crosscall_a:run(7)");

    let (interp, jit) = run_both(&mut host, &caller, "run0", &[], "crosscall_a:run0/0");
    let expected = expected_ok(&mut host, 42);
    assert_value(&host, value_of(&interp, "interpreter"), expected, "crosscall_a:run0()");
    assert_value(&host, value_of(&jit, "baseline"), expected, "crosscall_a:run0()");

    // The native tier has real compiled blocks for **both** modules: the callee's are
    // created on first sight through the driver's `NativeCode::module_blocks` when the
    // module-context transfer lands, so a non-empty map for `crosscall_b` is the
    // structural evidence that the cross-module call reached native code and did not
    // silently degrade the whole run to the interpreter step.
    for name in ["crosscall_a", "crosscall_b"] {
        let module = host.module(name);
        let regions = baseline_code_map(&module)
            .unwrap_or_else(|error| panic!("compile `{name}` on the baseline tier: {error:?}"));
        assert!(
            !regions.is_empty(),
            "the baseline tier compiled no block for `{name}`, so no native code executed it"
        );
    }
}

// ---------------------------------------------------------------------------
// (2) a nested A -> B -> C chain, with modules entered recursively
// ---------------------------------------------------------------------------

/// `chain_a:run/1` -> `chain_b:walk/1` (recursing into itself while its context is
/// live, each level cross-module-calling `chain_c:inc_twice/1`) ->
/// `chain_c:inc_twice/1` -> `chain_c:inc/1` (twice: a nested, recursive entry into
/// the same `chain_c` module context).
///
/// `walk(N)` is `2 * N` (each level adds two), so `run(5)` is `10` and `run(0)` is
/// `0` -- verified against the real `erl` at authoring time.
#[test]
fn a_nested_recursive_cross_module_chain_agrees_across_tiers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    let entry = host.module("chain_a");

    for (argument, expected) in [(5i64, 10i64), (0, 0)] {
        let label = format!("chain_a:run({argument})");
        let args = [make_small(argument)];
        let (interp, jit) = run_both(&mut host, &entry, "run", &args, &label);
        assert_value(&host, value_of(&interp, "interpreter"), make_small(expected), &label);
        assert_value(&host, value_of(&jit, "baseline"), make_small(expected), &label);
    }
}

// ---------------------------------------------------------------------------
// (3) a slice parked inside a cross-module callee, resumed on the SAME tier
// ---------------------------------------------------------------------------

/// `park_a:run/0` tail-calls `park_b:await/0`, whose `receive` finds the mailbox
/// empty: both tiers must report the **same** [`RunOutcome::Wait`], and the machine
/// must be in the **callee's** (non-entry) module context. Delivering a message and
/// resuming **on the same tier** must yield the delivered term on both.
///
/// This is the one row that does **not** use [`run_both`] / [`NATIVE_TIERS`]: it is
/// the only case whose point is a **resume**, and the Cranelift hot tier has no
/// mid-function resume entry point. (Its *first* entry does report the same
/// [`RunOutcome::Wait`] address as the other tiers, so it is `run_both`-compatible
/// up to the park; the resume is what excludes it.) Asking that tier to resume is
/// the honest typed `Err(unsupported_opcode: cranelift jit: cannot resume at an
/// arbitrary pc (the cranelift tier has no resume entry point))`, so a same-tier
/// resume row is only meaningful on the interpreter and the baseline tier.
#[test]
fn a_slice_parked_in_a_cross_module_callee_resumes_on_the_same_tier() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    let entry = host.module("park_a");
    let callee = host.module("park_b");

    // The interpreter and the baseline tier only (see the doc comment above).
    host.clear_mailbox();
    let mut interp = run_tier(&mut host, &entry, "run", &[], Tier::Interpreter);
    host.clear_mailbox();
    let mut jit = run_tier(&mut host, &entry, "run", &[], Tier::BaselineJit);
    assert_tiers_agree(&host, "park_a:run/0", &interp, &jit, Tier::BaselineJit);
    // Both tiers suspended, at the same callee address.
    let wait = match (&interp.outcome, &jit.outcome) {
        (
            Ok(RunOutcome::Wait {
                func: interp_func,
                pc: interp_pc,
            }),
            Ok(RunOutcome::Wait {
                func: jit_func,
                pc: jit_pc,
            }),
        ) => {
            assert_eq!((interp_func, interp_pc), (jit_func, jit_pc));
            (*interp_func, *interp_pc)
        }
        other => panic!("`park_a:run/0` must park on both tiers, got {other:?}"),
    };

    // The suspension is inside the cross-module callee: the machine's module context
    // is `park_b`, not the run's entry module (context 0). This is what makes the
    // resume below a resume in a **non-entry** context.
    for (tier, run) in [("interpreter", &interp), ("baseline", &jit)] {
        let context_module = run.machine.current_module().unwrap_or_else(|| {
            panic!("the {tier} suspension must be inside the cross-module callee, not the entry module")
        });
        assert!(
            Arc::ptr_eq(&context_module, &callee),
            "the {tier} suspension must be in the `park_b` module context"
        );
    }

    // Deliver the message and resume **on the same tier** the slice parked on.
    let delivered = make_small(42);

    host.clear_mailbox();
    host.enqueue(delivered);
    let resumed_interp = interp
        .machine
        .resume_outcome_with_tier(&*entry, wait.0, wait.1, &mut host, Tier::Interpreter)
        .expect("the interpreter resume completes");

    host.clear_mailbox();
    host.enqueue(delivered);
    let resumed_jit = jit
        .machine
        .resume_outcome_with_tier(&*entry, wait.0, wait.1, &mut host, Tier::BaselineJit)
        .expect("the baseline-tier resume completes");

    assert_eq!(
        resumed_interp,
        RunOutcome::Returned(delivered),
        "the interpreter's resume must yield the delivered term"
    );
    assert_eq!(
        resumed_jit,
        RunOutcome::Returned(delivered),
        "the baseline tier's resume must yield the delivered term"
    );
}

// ---------------------------------------------------------------------------
// (4) a dynamic `apply/3` on registers
// ---------------------------------------------------------------------------

/// `apply_a:run/3` tail-calls `erlang:apply/3` with the target read out of the
/// registers; the engine's `fixed_apply` decode resolves the *computed* target
/// through the cross-module seam, so `crosscall_b:add_one/1` runs in this machine on
/// every tier in [`NATIVE_TIERS`] and all of them observe `{ok, 8}`.
#[test]
fn a_dynamic_apply_on_registers_agrees_across_tiers() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = fixture_host();
    let entry = host.module("apply_a");

    let module_arg = host.intern("crosscall_b");
    let function_arg = host.intern("add_one");
    let args_arg = terms::make_list(&mut host, &[make_small(7)])
        .expect("build the applied argument list `[7]`");
    let args = [module_arg, function_arg, args_arg];

    let (interp, jit) = run_both(
        &mut host,
        &entry,
        "run",
        &args,
        "apply_a:run(crosscall_b, add_one, [7])",
    );

    let expected = expected_ok(&mut host, 8);
    assert_value(&host, value_of(&interp, "interpreter"), expected, "the applied call");
    assert_value(&host, value_of(&jit, "baseline"), expected, "the applied call");
}
