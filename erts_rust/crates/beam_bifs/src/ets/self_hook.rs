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

//! The match-spec `self/0` [`SelfHook`] this crate installs over `beam_ets`, plus
//! [`SpecEval`] — the one aggregate scoped guard every match-spec-evaluating BIF
//! in this crate opens.
//!
//! `beam_ets` evaluates a match spec's `self/0` through a **thread-local** seam
//! ([`beam_ets::select::expr::set_self_hook`], mirroring its `ArithHook`); with
//! none installed the seam's default answers the typed `badarg`, which turns a
//! `self/0` **guard** into a silently skipped clause and a `self/0` **body** into
//! the `EXIT` sentinel — both a silently **wrong result** (the oracle answers the
//! calling pid in either position: `ets:select(T,[{'$1',[],[{self}]}])` ⇒
//! `[<0.10.0>,<0.10.0>]` and `ets:select(T,[{'$1',[{'=:=',{self},{self}}],[ok]}])`
//! ⇒ `[ok,ok]` on OTP 29.1.1 / erts-17.1).
//!
//! A BIF owns the calling pid ([`BifContext::self_pid`]) but neither the runtime
//! thread nor a "install once" entry point for the seam, so the hook is installed
//! **per match-spec evaluation** with [`SpecEval::install`] and each seam's own
//! default is restored when the outermost guard drops.
//!
//! # Why the guard also opens `PrecisionArith`
//!
//! [`SpecEval`] is the one place a spec-running BIF installs *both* seams
//! `beam_ets` exposes: the `self/0` hook here and the arbitrary-precision
//! arithmetic hook ([`super::arith_hook::PrecisionArith`]). A site that installed
//! only one of them would be a silent defect of exactly the class above, so the
//! two are inseparable — the aggregate guard holds the arithmetic guard, so
//! dropping it can only ever restore both.
//!
//! # Nesting restores the OUTER pid
//!
//! A spec may be evaluated **inside** another spec's evaluation: `ets:foldl/3`'s
//! fun is an Erlang fun that runs `ets:select/2` of its own (through the
//! [`crate::context::FunCaller`] seam), and a `self/0`-carrying spec can equally
//! be reached from a nested `beam_ets` run. The installed pids are therefore kept
//! on a thread-local **stack**: [`SpecEval::install`] pushes the pid and installs
//! a hook for it, and its `Drop` pops and re-installs the hook for the new top (or
//! restores the seam's default only when the stack empties). Restoring blindly to
//! the default would silently re-break an outer evaluation still in progress — the
//! very defect this module fixes.
//!
//! # Honesty
//!
//! The hook never fabricates a value: it answers the calling process's pid as the
//! term the runtime's own [`ProcessId::to_eterm`] builds (an internal-pid
//! immediate — nothing is allocated, so there is no heap or GC-root concern).

use std::cell::RefCell;
use std::rc::Rc;

use beam_common::Result;
use beam_ets::select::expr::{set_self_hook, SelfHook};
use beam_process::ProcessId;
use beam_term::Eterm;

use super::arith_hook::PrecisionArith;

/// The `self/0` hook: the calling process's pid, rendered with the runtime's own
/// `ProcessId` term, so a spec's `self/0` and a caller's `erlang:self/0` are the
/// same term. All-immediate, hence `Copy`.
#[derive(Debug, Clone, Copy)]
struct PidHook(ProcessId);

impl SelfHook for PidHook {
    fn self_pid(&self) -> Result<Eterm> {
        Ok(self.0.to_eterm())
    }
}

thread_local! {
    /// The pids whose [`PidHook`] is installed on this thread, outermost first: the
    /// last element is the innermost live evaluation and the installed hook. Empty
    /// means the seam's own default (the typed `badarg`).
    static INSTALLED: RefCell<Vec<ProcessId>> = const { RefCell::new(Vec::new()) };
}

/// Makes `pid`'s [`PidHook`] the seam's installed hook, or restores the seam's own
/// default when `pid` is `None`.
fn install_top(pid: Option<ProcessId>) {
    match pid {
        Some(pid) => set_self_hook(Some(Rc::new(PidHook(pid)))),
        None => set_self_hook(None),
    }
}

/// The scoped installation of the two match-spec seams this crate serves over
/// `beam_ets` — the `self/0` [`SelfHook`] (for the calling process's pid) and the
/// arbitrary-precision [`ArithHook`](beam_ets::select::expr::ArithHook). Both are
/// live for the returned guard's lifetime; the outermost drop restores each seam's
/// own default. See the module docs for the install point and the nesting rule.
#[derive(Debug)]
pub(crate) struct SpecEval {
    /// The arithmetic hook's own depth-counted guard; holding it here is what makes
    /// the two seams inseparable (dropping this guard drops both).
    _arith: PrecisionArith,
}

impl SpecEval {
    /// Installs [`PidHook`] for `pid` and the arbitrary-precision `ArithHook` on
    /// the current thread for the returned guard's lifetime.
    ///
    /// Every `ets:*` BIF that evaluates a match spec — `ets:select/1,2,3`,
    /// `ets:select_reverse/1,2,3`, `ets:select_count/2`, `ets:select_delete/2`,
    /// `ets:select_replace/2`, `ets:match_spec_run/2`, `ets:match_spec_run_r/3`,
    /// `ets:internal_select_delete/2` and the continuation resumes they share — and
    /// `erlang:match_spec_test/3`'s `table` dialect opens it, so a resumed
    /// traversal keeps both seams.
    pub(crate) fn install(pid: ProcessId) -> SpecEval {
        INSTALLED.with(|stack| {
            let mut stack = stack.borrow_mut();
            stack.push(pid);
            // Re-install on every push: the seam exposes no getter, so the hook is
            // always rebuilt from the innermost evaluation's pid.
            install_top(Some(pid));
        });
        SpecEval {
            _arith: PrecisionArith::install(),
        }
    }
}

impl Drop for SpecEval {
    fn drop(&mut self) {
        INSTALLED.with(|stack| {
            let mut stack = stack.borrow_mut();
            stack.pop();
            // Restore the *outer* evaluation's pid, not the seam's default: a nested
            // run must not silently lose the caller identity of the scope it was
            // nested in.
            install_top(stack.last().copied());
        });
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the scoped `self/0` install, driven **directly** through
    //! `beam_ets`'s executor (so the guard's own effect is what is measured, with no
    //! BIF in between). The expected terms are the oracle's own (OTP 29.1.1 /
    //! erts-17.1): `ets:match_spec_run([7], ets:match_spec_compile([{'$1',[],[{self}]}]))`
    //! ⇒ `[<0.3.0>]` for the pid of the process making the call.

    use super::*;
    use beam_alloc::Heap;
    use beam_term::{make_atom, AtomTable};

    use crate::context::BifContext;

    /// A heap with room for the specs and results built below.
    fn heap() -> Heap {
        Heap::new(4096).expect("test heap")
    }

    /// The atoms these specs need: the match-spec variable, the `self` call atom,
    /// the `=:=` guard operator and the `EXIT` sentinel `beam_ets` substitutes for a
    /// raising body (plus the `ok` body the guard case yields).
    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["self", "$1", "=:=", "EXIT", "ok"]);
        atoms
    }

    /// `{self}` — the match-spec spelling of a `self/0` call (a 1-tuple holding the
    /// `self` atom), exactly what `ets:select/2`'s an `-eval`-free caller passes.
    fn self_call(ctx: &mut BifContext<'_>) -> Eterm {
        let s = ctx.atom("self").expect("seeded");
        ctx.tuple(&[s]).expect("fits")
    }

    /// `[{Head, Guards, Body}]` from the three element lists.
    fn one_clause(ctx: &mut BifContext<'_>, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Eterm {
        let guards = ctx.list(guards).expect("fits");
        let body = ctx.list(body).expect("fits");
        let clause = ctx.tuple(&[head, guards, body]).expect("fits");
        ctx.list(&[clause]).expect("fits")
    }

    /// The singleton list `[7]` the specs below run against.
    fn singleton(ctx: &mut BifContext<'_>) -> Eterm {
        let seven = ctx.int(7).expect("fits");
        ctx.list(&[seven]).expect("fits")
    }

    /// Runs `spec` over `list` straight through `beam_ets`'s executor — no BIF, no
    /// guard of its own, so the only hook in force is one an outer scope installed.
    fn run(heap: &mut Heap, atoms: &AtomTable, list: Eterm, spec: Eterm) -> Vec<Eterm> {
        beam_ets::match_spec_run(list, spec, atoms, heap).expect("match_spec_run")
    }

    /// `[{ '$1', [], [{self}] }]` plus its singleton list, built once per test.
    fn self_body_spec(heap: &mut Heap, atoms: &AtomTable, pid: ProcessId) -> (Eterm, Eterm) {
        let mut ctx = BifContext::new(heap, atoms, pid);
        let v1 = ctx.atom("$1").expect("seeded");
        let body = self_call(&mut ctx);
        let spec = one_clause(&mut ctx, v1, &[], &[body]);
        let list = singleton(&mut ctx);
        (list, spec)
    }

    /// `[{ '$1', [{'=:=',{self},{self}}], [ok] }]` plus its singleton list.
    fn self_guard_spec(heap: &mut Heap, atoms: &AtomTable, pid: ProcessId) -> (Eterm, Eterm) {
        let mut ctx = BifContext::new(heap, atoms, pid);
        let v1 = ctx.atom("$1").expect("seeded");
        let self_call = self_call(&mut ctx);
        let eq = ctx.atom("=:=").expect("fits");
        let guard = ctx.tuple(&[eq, self_call, self_call]).expect("fits");
        let ok = ctx.atom("ok").expect("seeded");
        let spec = one_clause(&mut ctx, v1, &[guard], &[ok]);
        let list = singleton(&mut ctx);
        (list, spec)
    }

    /// The atom term `name`, which the table must have seeded.
    fn atom_term(atoms: &AtomTable, name: &str) -> Eterm {
        make_atom(atoms.lookup(name).expect("seeded"))
    }

    #[test]
    fn a_self_body_answers_the_installed_pid_and_the_default_is_the_exit_sentinel() {
        let mut heap = heap();
        let atoms = atoms();
        let pid = ProcessId::new(3, 0);
        let (list, spec) = self_body_spec(&mut heap, &atoms, pid);

        // The control: with no guard installed the `self/0` body is `beam_ets`'s
        // `EXIT` sentinel (its documented typed gap), never the pid.
        assert_eq!(
            run(&mut heap, &atoms, list, spec),
            vec![atom_term(&atoms, "EXIT")],
            "without a hook a self/0 body must be the typed gap"
        );

        // Installed for the guard's lifetime: the calling pid.
        let guard = SpecEval::install(pid);
        assert_eq!(
            run(&mut heap, &atoms, list, spec),
            vec![pid.to_eterm()],
            "the installed hook must answer the calling pid"
        );
        drop(guard);

        // Dropped: back to the typed gap, so the seam does not leak.
        assert_eq!(
            run(&mut heap, &atoms, list, spec),
            vec![atom_term(&atoms, "EXIT")],
            "the outermost drop must restore the seam's default"
        );
    }

    #[test]
    fn a_self_guard_selects_under_a_guard_and_skips_its_clause_without_one() {
        let mut heap = heap();
        let atoms = atoms();
        let pid = ProcessId::new(3, 0);
        let (list, spec) = self_guard_spec(&mut heap, &atoms, pid);

        // No hook: `self/0` raises, so the guard is unmet and the clause is skipped —
        // the silently wrong result the aggregate guard exists to prevent.
        assert!(
            run(&mut heap, &atoms, list, spec).is_empty(),
            "without a hook the self/0 guard must skip its clause"
        );

        // The guard holds, so the clause selects.
        let _guard = SpecEval::install(pid);
        assert_eq!(
            run(&mut heap, &atoms, list, spec),
            vec![atom_term(&atoms, "ok")],
            "the self/0 guard must hold under the installed hook"
        );
    }

    #[test]
    fn nesting_restores_the_outer_pid_and_the_outermost_drop_restores_the_default() {
        let mut heap = heap();
        let atoms = atoms();
        let outer = ProcessId::new(3, 0);
        let inner = ProcessId::new(4, 0);
        let (list, spec) = self_body_spec(&mut heap, &atoms, outer);

        let outer_guard = SpecEval::install(outer);
        assert_eq!(run(&mut heap, &atoms, list, spec), vec![outer.to_eterm()]);

        {
            // The inner install names a *different* process, and answers with it.
            let _inner = SpecEval::install(inner);
            assert_eq!(run(&mut heap, &atoms, list, spec), vec![inner.to_eterm()]);
        }

        // The inner guard's drop restored the OUTER pid — not the seam's default,
        // which would silently re-break the evaluation still in progress.
        assert_eq!(
            run(&mut heap, &atoms, list, spec),
            vec![outer.to_eterm()],
            "an inner drop must restore the outer evaluation's pid"
        );

        drop(outer_guard);
        assert_eq!(
            run(&mut heap, &atoms, list, spec),
            vec![atom_term(&atoms, "EXIT")],
            "the outermost drop must restore the seam's default"
        );
    }
}
