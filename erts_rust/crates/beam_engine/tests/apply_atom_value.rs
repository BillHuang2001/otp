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
//! The runtime-atom-**value** resolution seam,
//! [`ExecutionContext::atom_name`].
//!
//! # The bug this pins
//!
//! [`Module::atom_name`](beam_loader::Module::atom_name) indexes the module's
//! **owned load-time snapshot** of the shared atom table, so a module loaded early
//! cannot name an atom interned into the shared namespace *afterwards*: that
//! atom's global id is beyond the snapshot's length. At the A.9 boot path the
//! engine evaluates `{ok, Mod:init(Args)}` in `gen_server:init_it/6` with
//! `Mod = supervisor_bridge` — an atom whose global id exceeds `gen_server`'s
//! snapshot — and a bare module-local lookup there dies with `error:bad argument`,
//! tearing the `permanent` `kernel` application down.
//!
//! # What this suite pins
//!
//! 1. **The default path is byte-identical.** A context that does **not** override
//!    [`atom_name`](ExecutionContext::atom_name) (here a [`MinimalContext`] whose
//!    cross-module resolver *would* answer the callee) resolves the runtime atom
//!    value through the module's own snapshot, which cannot name it, so the apply
//!    fails with the typed [`BeamError::BadArg`] — the documented `badarg` — and
//!    the resolver is **never consulted**.
//! 2. **The live-namespace path resolves it.** A host that owns the live shared
//!    namespace answers from it, the cross-module seam resolves the callee, and the
//!    engine **enters** the callee's code in this machine, reproducing the oracle's
//!    value `42`.
//! 3. **The oracle agrees.** The committed fixtures
//!    (`tests/fixtures/atomapply.{erl,beam}` / `atomtarget.{erl,beam}`, compiled
//!    with the system `erlc +deterministic`) are re-run under the live system `erl`
//!    and it prints `42` for `atomapply:run(atomtarget, target_fun, [])`. That step
//!    **skips loudly** when there is no runnable `erl`/`erlc` toolchain.
//!
//! The fixture `atomapply.beam`'s own atom table never interns `atomtarget`/
//! `target_fun`, so a **clone** of that table with the two names appended gives
//! exactly the bug's shape: a snapshot strictly shorter than the live namespace.

use std::cell::RefCell;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::{ExecutionContext, Machine, MinimalContext};
use beam_loader::{load_file, Label, Module};
use beam_term::{make_atom, make_small, Atom, AtomTable, Eterm, NIL};

/// The committed fixture whose `run/3` is `erlang:apply(M, F, A)` over runtime
/// atoms (`erlc +deterministic atomapply.erl`).
const ATOMAPPLY_BEAM: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/atomapply.beam");
/// The committed callee fixture (`erlc +deterministic atomtarget.erl`).
const ATOMTARGET_BEAM: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/atomtarget.beam");
/// The fixture directory (appended with `-pz`, never `-pa`).
const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

// ---------------------------------------------------------------------------
// The cross-module resolver both contexts share
// ---------------------------------------------------------------------------

/// Resolve `(module, function, arity)` to the callee fixture, keyed by name —
/// the body of [`ExecutionContext::resolve_external_call`] in both contexts, so
/// the only difference between them is [`ExecutionContext::atom_name`].
fn resolve(
    callees: &[(String, Arc<Module>)],
    module: &str,
    function: &str,
    arity: u32,
) -> Result<Option<(Arc<Module>, Label)>> {
    for (name, callee) in callees {
        if name != module {
            continue;
        }
        let Some(function) = callee.atom_table.lookup(function) else {
            continue;
        };
        if let Some(label) = callee.export(function, arity) {
            return Ok(Some((Arc::clone(callee), label)));
        }
    }
    Ok(None)
}

/// The callee fixture as the host's own module handle, plus its `target_fun/0`
/// export entry label.
fn target_callee() -> Arc<Module> {
    Arc::new(load_file(ATOMTARGET_BEAM).expect("load atomtarget.beam"))
}

// ---------------------------------------------------------------------------
// (1) The default path: the module's own snapshot, and the resolver is never asked
// ---------------------------------------------------------------------------

#[test]
fn the_default_seam_cannot_name_the_runtime_atom_and_never_consults_the_resolver() {
    let caller = load_file(ATOMAPPLY_BEAM).expect("load atomapply.beam");
    let callee = target_callee();
    let callees = vec![("atomtarget".to_string(), Arc::clone(&callee))];

    // The live namespace: a clone of the caller's own snapshot plus the two names
    // `atomapply` never mentions, so their global ids are `>= the snapshot length`.
    let mut live = caller.atom_table.clone();
    let target_mod = live.intern("atomtarget");
    let target_fun = live.intern("target_fun");
    // This is the bug's exact shape: unnameable through the module's snapshot,
    // nameable through the live namespace.
    assert!(
        caller.atom_name(target_mod).is_none(),
        "`atomtarget` must be beyond atomapply's load-time snapshot"
    );
    assert_eq!(live.name(target_mod), Some("atomtarget"));
    assert!(caller.atom_name(target_fun).is_none());
    assert_eq!(live.name(target_fun), Some("target_fun"));

    // A recording resolver that WOULD answer the callee — proving the failure is
    // `atom_name`'s, not the resolver's.
    let log: Rc<RefCell<Vec<(String, String, u32)>>> = Rc::new(RefCell::new(Vec::new()));
    let resolver = {
        let log = Rc::clone(&log);
        let callees = callees.clone();
        Box::new(move |m: &str, f: &str, a: u32| {
            log.borrow_mut().push((m.to_string(), f.to_string(), a));
            resolve(&callees, m, f, a)
        })
    };
    // `MinimalContext` does NOT override `atom_name`, so this is the trait's own
    // default body: the running module's snapshot only.
    let mut ctx = MinimalContext::with_external_resolver(resolver);
    let mut machine = Machine::new();

    let run = caller
        .atom_table
        .lookup("run")
        .expect("atomapply exports run/3");
    let error = machine
        .run_export(
            &caller,
            run,
            3,
            &[make_atom(target_mod), make_atom(target_fun), NIL],
            &mut ctx,
        )
        .expect_err("the unnamed runtime atom is a badarg");

    assert!(
        matches!(error, BeamError::BadArg),
        "the unnamed runtime atom must stay the typed badarg, got {error:?}"
    );
    assert!(
        log.borrow().is_empty(),
        "the resolver is never consulted once `atom_name` declines: {:?}",
        log.borrow()
    );
}

// ---------------------------------------------------------------------------
// (2) The live-namespace path: resolve through the shared namespace and ENTER
// ---------------------------------------------------------------------------

/// A context that owns the **live** shared atom namespace and answers
/// [`ExecutionContext::atom_name`] from it, delegating everything else to a
/// [`MinimalContext`] core (the reserved-heap allocator).
struct LiveCtx {
    core: MinimalContext,
    callees: Vec<(String, Arc<Module>)>,
    live: AtomTable,
    resolved: Rc<RefCell<Vec<(String, String, u32)>>>,
}

impl ExecutionContext for LiveCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(beam_engine::unsupported(&format!(
            "bif {module}:{function}/{arity}"
        )))
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.core.alloc_words(n)
    }

    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        self.resolved
            .borrow_mut()
            .push((module.to_string(), function.to_string(), arity));
        resolve(&self.callees, module, function, arity)
    }

    fn atom_name(&self, _module: &Module, atom: Atom) -> Option<String> {
        self.live.name(atom).map(str::to_owned)
    }
}

#[test]
fn the_live_namespace_seam_resolves_the_runtime_atom_and_enters_the_callee() {
    let caller = load_file(ATOMAPPLY_BEAM).expect("load atomapply.beam");
    let callee = target_callee();
    let callees = vec![("atomtarget".to_string(), Arc::clone(&callee))];

    let mut live = caller.atom_table.clone();
    let target_mod = live.intern("atomtarget");
    let target_fun = live.intern("target_fun");

    let resolved = Rc::new(RefCell::new(Vec::new()));
    let mut ctx = LiveCtx {
        core: MinimalContext::new(),
        callees,
        live,
        resolved: Rc::clone(&resolved),
    };
    let mut machine = Machine::new();

    let run = caller
        .atom_table
        .lookup("run")
        .expect("atomapply exports run/3");
    let value = machine
        .run_export(
            &caller,
            run,
            3,
            &[make_atom(target_mod), make_atom(target_fun), NIL],
            &mut ctx,
        )
        .expect("the live namespace names the runtime atoms");

    assert_eq!(
        value,
        make_small(42),
        "the engine entered the callee and reproduced the oracle's 42"
    );
    // The seam was consulted with the resolved names and answered the callee, so
    // the engine took the `ApplyTarget::Enter` route (the only route that consults
    // it and the only one that can run the callee's own code — `call_bif` here is a
    // typed `unsupported`).
    assert_eq!(
        resolved.borrow().as_slice(),
        &[("atomtarget".to_string(), "target_fun".to_string(), 0)]
    );
}

// ---------------------------------------------------------------------------
// (3) The oracle cross-check
// ---------------------------------------------------------------------------

/// Whether the system `erl`/`erlc` toolchain is runnable.
///
/// `erl -version` prints the version and exits; `erlc -help` prints its usage and
/// exits. Either way a successful **spawn** proves the tool is on `PATH`.
fn oracle_tools_present() -> bool {
    let probe = |tool: &str, arg: &str| {
        Command::new(tool)
            .arg(arg)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    };
    probe("erl", "-version") && probe("erlc", "-help")
}

/// Re-run the fixture scenario under the live system `erl`.
///
/// `None` means no runnable `erl`/`erlc` (the caller **skips loudly**). A spawned
/// `erl` that exits non-zero is a **harness** failure, not "no oracle" — it panics
/// with the captured streams so a broken oracle can never masquerade as a skip.
///
/// The fixture directory is **appended** (`-pz`, never `-pa`): it holds stand-in
/// modules (e.g. `tests/fixtures/lists.beam`) that would shadow the real stdlib
/// once prepended.
fn oracle_atomapply() -> Option<String> {
    if !oracle_tools_present() {
        return None;
    }
    let eval = concat!(
        "io:format(\"~w\", [atomapply:run(atomtarget, target_fun, [])]), halt()."
    );
    let output = Command::new("erl")
        .args(["-noshell", "-pz", FIXTURES_DIR, "-eval", eval])
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

#[test]
fn the_live_oracle_reproduces_the_entered_callees_value() {
    let Some(oracle) = oracle_atomapply() else {
        eprintln!(
            "SKIP apply_atom_value::the_live_oracle_reproduces_the_entered_callees_value: \
             no runnable `erl`/`erlc` toolchain on PATH"
        );
        return;
    };
    // The value the live-namespace test bakes in, re-derived from the oracle itself.
    assert_eq!(oracle, "42", "the oracle's atomapply transcript changed");
}
