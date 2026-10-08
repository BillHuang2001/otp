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

//! The engine's **execution-trace seam** in the runtime (contract A.8, item 1): the
//! `call` / `return_from` / `return_to` events.
//!
//! The Stage-1 engine reports every call/return it executes to its host through
//! [`ExecutionContext::trace_enabled`](beam_engine::ExecutionContext::trace_enabled) (a single
//! cheap gate, latched once per run into `Machine::trace_on`) and
//! [`ExecutionContext::trace_event`](beam_engine::ExecutionContext::trace_event) (one call per
//! event). Those two trait methods are implemented in the parent module (Rust allows only
//! **one** `impl ExecutionContext for RuntimeContext` block) and delegate here to
//! [`RuntimeContext::execution_trace_enabled`] / [`RuntimeContext::execution_trace_event`],
//! which decide *matching* (the installed `erlang:trace_pattern/3` table) and **build +
//! deliver** the `{trace, Tracee, Event, …}` message through the runtime's single emit point
//! ([`RuntimeContext::emit_trace_event`](crate::context::RuntimeContext::emit_trace_event)).
//! Nothing else produces these three events.
//!
//! # The oracle semantics this reproduces
//!
//! * **The process `call` flag is required for the whole family.** It gates `call` and
//!   `return_from`, and it plus the `return_to` flag arms `return_to`. A `[return_to]`-only
//!   process observes nothing (verified against the live oracle; the C clears the local/global
//!   session breakpoint when `!IS_SESSION_TRACED_FL(ref, F_TRACE_CALLS)`,
//!   `beam_bp.c`'s `do_session_breakpoint`).
//! * **`local` vs `global`** are mutually exclusive `trace_pattern` flags (`[local,global]`
//!   is `badarg`). A `[local]` pattern fires for every entry to the matched function (a local
//!   `call` and a `call_ext` alike, from any module); a `[global]` pattern fires only for an
//!   entry that goes through the **export** (an external dispatch).
//! * **`return_from` is armed per function, not by a flag.** It takes a `{return_trace}` (or
//!   `{exception_trace}`) match-spec **action** in the last element of a 2- or 3-tuple match
//!   function; `true` (`MatchAll`) never arms it, and neither does a bare `return_trace` atom.
//! * **`return_to` is armed by the *returning* function's `[local]` pattern** plus the process
//!   `call` + `return_to` flags. A `[global]` pattern never arms it.
//! * **The `arity` process flag** switches the `call` event's third element from the argument
//!   list to the bare arity. `return_from`/`return_to` always carry the arity, and
//!   `return_from` additionally carries the returned value.
//! * **The pattern-level `silent` flag** suppresses the event (on top of the process-level
//!   `silent` flag [`RuntimeContext::emit_trace_event`] already honours).
//!
//! Exact message shapes: `{trace, Pid, call, {M,F,Args|A}}`,
//! `{trace, Pid, return_from, {M,F,A}, Value}`, `{trace, Pid, return_to, {M,F,A}}`.
//!
//! # The `local`/`global` heuristic — and its one documented divergence
//!
//! [`beam_engine::TraceEvent::Call`] does **not** carry the call kind (a local `call` vs an
//! external `call_ext`), and `beam_engine` is not this crate's to change, so "an entry that
//! goes through the export" is approximated as **`callee_module != current_module`** — the
//! module whose code is executing (the private
//! [`RuntimeContext::current_module`](crate::context::RuntimeContext'), set by `run_export`
//! and `enter_module`; `None` — the `-eval` evaluator — counts as "differs").
//!
//! **Residual divergence:** a `[global]`-only pattern matching a call written
//! `?MODULE:F(...)` — a `call_ext` whose callee module *is* the current module — is
//! **under-reported** by this heuristic where the oracle reports it. Everything else agrees:
//! the whole `[local]` surface, every cross-module `[global]` entry, and the
//! no-pattern case (nothing is reported).
//!
//! # Infallibility
//!
//! `trace_event` returns `()`, so every failure here is **swallowed**: an unresolvable atom, a
//! full heap or an undeliverable message skips (or drops) the event, and the traced program's
//! result, raise or suspension is exactly what an untraced run would have produced.

use std::cell::Cell;

use beam_common::Result;
use beam_engine::{terms, TraceEvent};
use beam_term::{make_small, Eterm};

use crate::trace::{MatchSpec, PatternEntry, PatternFlags};

use super::RuntimeContext;

thread_local! {
    /// Whether the `return_from` event the engine just reported armed a `return_to`.
    ///
    /// The engine reports one `return` instruction as **two** events — `ReturnFrom` (the
    /// returning function) immediately followed by `ReturnTo` (the function control returns
    /// to) — but the `return_to` gate belongs to the *returning* function, so the two must be
    /// correlated across the two `trace_event` calls.
    ///
    /// A `thread_local!` slot is that correlation channel and is legitimate here: `Runtime` is
    /// `!Send` (it owns `Rc`/`RefCell` tables and raw `*mut Eterm` heaps) and single-threaded,
    /// so at most one trace can be in flight per OS thread. Every arm that is not `ReturnFrom`
    /// **clears** the slot, so a stale arm can never leak into an unrelated event.
    static RETURN_TO_ARMED: Cell<Option<bool>> = const { Cell::new(None) };
}

/// What the installed `{M,F,A}` patterns say about one call/return site.
///
/// Owned (`PatternFlags` is `Copy`) so the immutable borrow of the runtime's trace state is
/// released before the event's terms are built on `&mut self`.
#[derive(Clone, Copy, Debug)]
struct PatternHit {
    /// The matched pattern's `trace_pattern` flags.
    flags: PatternFlags,
    /// Whether the entry is reported for this run's call kind — the `local`/`global` gate.
    fires: bool,
    /// Whether the matched match spec arms a `return_from` (a `{return_trace}` /
    /// `{exception_trace}` action in a match function's body).
    arms_return: bool,
}

impl RuntimeContext<'_> {
    /// Whether the **running** process asks for the `call`/`return_from`/`return_to` family
    /// — the body of [`ExecutionContext::trace_enabled`] (the engine latches the answer once
    /// per run into `Machine::trace_on`).
    ///
    /// One hash lookup on the trace state: `true` iff the process has a trace entry whose
    /// flags include `call` **or** `return_to`. `return_to` alone is enough to arm the gate —
    /// it reports nothing without `call`, but the engine cannot know that and asking costs one
    /// bit.
    pub(super) fn execution_trace_enabled(&self) -> bool {
        let pid = self.pid();
        self.runtime
            .trace()
            .get(pid)
            .is_some_and(|entry| entry.flags.has_calls() || entry.flags.has_return_to())
    }

    /// Report one execution-trace event — the body of [`ExecutionContext::trace_event`],
    /// which the engine hands each event to **verbatim** and whose result it ignores.
    ///
    /// Every failure is therefore swallowed: an unresolvable atom, a full heap or an
    /// undeliverable event drops the event, and tracing never changes a run's outcome.
    pub(super) fn execution_trace_event(&mut self, event: TraceEvent<'_>) {
        let _ = self.dispatch_trace_event(event);
    }

    /// Build and deliver the `{trace, …}` message one engine event produces, if the process's
    /// flags and the installed function patterns ask for one.
    fn dispatch_trace_event(&mut self, event: TraceEvent<'_>) -> Result<()> {
        let traced = self.pid();
        let Some(flags) = self.runtime.trace().get(traced).map(|entry| entry.flags) else {
            // Untraced — the gate was latched for a process whose tracing has since been
            // cleared. Nothing is reported, and no stale `return_to` arm may survive.
            RETURN_TO_ARMED.with(|slot| slot.set(None));
            return Ok(());
        };
        match event {
            TraceEvent::Call {
                module,
                function,
                arity,
                args,
            } => {
                // A call opens a new frame: any arm left by an earlier return is stale.
                RETURN_TO_ARMED.with(|slot| slot.set(None));
                if !flags.has_calls() {
                    return Ok(());
                }
                let Some(hit) = self.pattern_hit(module, function, arity) else {
                    return Ok(());
                };
                if hit.flags.is_silent() || !hit.fires {
                    return Ok(());
                }
                // The `arity` process flag replaces the argument list with the bare arity.
                let third = if flags.has_arity() {
                    make_small(i64::from(arity))
                } else {
                    terms::make_list(self, args)?
                };
                let mfa = self.mfa_tuple(module, function, third)?;
                self.emit_trace_event(traced, "call", &[mfa])
            }
            TraceEvent::ReturnFrom {
                module,
                function,
                arity,
                value,
            } => {
                let hit = self.pattern_hit(module, function, arity);
                // The `return_to` arm is the *returning* function's own decision — a firing
                // `[local]` pattern plus the process `call`/`return_to` flags — recorded here for
                // the `ReturnTo` event the engine reports in this same step. It is stored
                // **always** (so a stale arm can never leak), and independently of whether this
                // very `ReturnFrom` is emitted: a `MatchAll` pattern arms the `return_to` without
                // arming a `return_from` (probed).
                let armed = flags.has_calls()
                    && flags.has_return_to()
                    && hit.is_some_and(|hit| hit.flags.is_local() && !hit.flags.is_silent());
                RETURN_TO_ARMED.with(|slot| slot.set(Some(armed)));
                if !flags.has_calls() {
                    return Ok(());
                }
                let Some(hit) = hit else {
                    return Ok(());
                };
                if hit.flags.is_silent() || !hit.fires || !hit.arms_return {
                    return Ok(());
                }
                let mfa = self.mfa_tuple(module, function, make_small(i64::from(arity)))?;
                self.emit_trace_event(traced, "return_from", &[mfa, value])
            }
            TraceEvent::ReturnTo {
                module,
                function,
                arity,
            } => {
                // The gate is the *returning* function's decision, recorded by the `ReturnFrom`
                // arm the engine reported in this same step; the function control returns **to**
                // is not pattern-matched. That is the oracle's behaviour: the arm is planted at
                // the traced **call** and fires unconditionally when the callee returns, so
                // `{trace, Pid, return_to, {Caller, F, A}}` names a caller with no pattern of its
                // own (probed: a `[local]` pattern on `tcall:a2/2` yields
                // `return_to {tcall,run,0}`).
                let armed = RETURN_TO_ARMED
                    .with(|slot| slot.replace(None))
                    .unwrap_or(false);
                if !armed {
                    return Ok(());
                }
                let mfa = self.mfa_tuple(module, function, make_small(i64::from(arity)))?;
                self.emit_trace_event(traced, "return_to", &[mfa])
            }
        }
    }

    /// The pattern decision for the concrete `module:function/arity`: the **most specific**
    /// installed `erlang:trace_pattern/3` entry that matches it, together with the two
    /// derived gates (see [`PatternHit`]). `None` when no pattern matches — nothing is then
    /// reported.
    fn pattern_hit(&self, module: &str, function: &str, arity: u32) -> Option<PatternHit> {
        let entry = self
            .runtime
            .trace()
            .patterns()
            .matches(module, function, arity)?;
        Some(PatternHit {
            flags: entry.flags,
            // A `[local]` pattern fires on every entry; a `[global]` one only when the callee
            // is not the module whose code is executing (an external dispatch). No calling
            // module (`None`, the `-eval` evaluator) counts as "differs".
            fires: entry.flags.is_local()
                || (entry.flags.is_global() && self.current_module.as_deref() != Some(module)),
            arms_return: self.arms_return_trace(entry),
        })
    }

    /// Whether `entry`'s match spec arms a `return_from` for the function it matches.
    ///
    /// `return_from` is not a settable `erlang:trace/3` flag in the oracle: a match function
    /// arms it with a **1-tuple action** — `{return_trace}` or `{exception_trace}` — in its
    /// **body**, i.e. the **last** element of a 2- or 3-tuple match function
    /// ([`MatchSpec::Functions`] holds those tuples verbatim). `MatchAll` (`true`) never
    /// arms it, and a bare `return_trace` *atom* is accepted at install but arms nothing —
    /// both verified against the live oracle.
    fn arms_return_trace(&self, entry: &PatternEntry) -> bool {
        let MatchSpec::Functions(functions) = &entry.match_spec else {
            return false;
        };
        functions.iter().any(|function| {
            let Ok(arity) = terms::tuple_arity(*function) else {
                return false;
            };
            if arity != 2 && arity != 3 {
                return false;
            }
            let Ok(body) = terms::tuple_element(*function, arity - 1) else {
                return false;
            };
            terms::list_elements(body)
                .is_ok_and(|actions| actions.iter().any(|action| self.is_trace_action(*action)))
        })
    }

    /// Whether `term` is a 1-tuple trace **action** this runtime recognises: `{return_trace}`
    /// or `{exception_trace}`.
    fn is_trace_action(&self, term: Eterm) -> bool {
        if terms::tuple_arity(term).ok() != Some(1) {
            return false;
        }
        let Ok(action) = terms::tuple_element(term, 0) else {
            return false;
        };
        matches!(
            self.runtime.atom_name_of(action),
            Some("return_trace" | "exception_trace")
        )
    }

    /// The `{Module, Function, Third}` tuple of an event, with every name interned in the
    /// runtime's **one** shared atom table ([`RuntimeContext::atom`](crate::context::RuntimeContext)).
    fn mfa_tuple(&mut self, module: &str, function: &str, third: Eterm) -> Result<Eterm> {
        let module_atom = self.atom(module)?;
        let function_atom = self.atom(function)?;
        terms::make_tuple(self, &[module_atom, function_atom, third])
    }
}

#[cfg(test)]
mod tests {
    use beam_bifs::NullSink;
    use beam_engine::ExecutionContext;
    use beam_process::{ProcessId, ProcessTable, SignalAtoms};
    use beam_term::prelude::AtomTable;

    use super::*;
    use crate::runtime::Runtime;

    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    /// A **tracee** and a distinct **tracer** in a caller-owned table.
    ///
    /// ERTS discards an event whose tracer IS the tracee
    /// (`erts/emulator/nifs/common/erl_tracer_nif.c:143-146`), so the two must differ for any
    /// event to be delivered at all. `erts_internal:trace/3` always installs the **calling**
    /// process as the tracer, so the tracer half is the context that calls `erlang:trace/3`.
    fn two_processes(rt: &mut Runtime) -> (ProcessTable, ProcessId, ProcessId) {
        let _ = rt;
        let mut atoms = AtomTable::new();
        let mut table = ProcessTable::new(SignalAtoms::from_table(&mut atoms));
        let tracee = table.spawn_default().expect("spawn the tracee");
        let tracer = table.spawn_default().expect("spawn the tracer");
        (table, tracee, tracer)
    }

    /// Every message in `pid`'s mailbox, in order, drained.
    fn drain(rt: &mut Runtime, table: &mut ProcessTable, pid: ProcessId) -> Vec<Eterm> {
        let mut ctx = RuntimeContext::for_process(rt, table, pid);
        ctx.receive_reset().expect("timeout");
        let mut messages = Vec::new();
        while let Some(term) = ctx.receive_next().expect("receive_next") {
            messages.push(term);
            ctx.receive_remove().expect("remove_message");
            assert!(messages.len() < 16, "the mailbox keeps draining: {messages:?}");
        }
        messages
    }

    /// The element at `index` of the tuple `term`.
    fn element(term: Eterm, index: usize) -> Eterm {
        terms::tuple_element(term, index).expect("the tuple element")
    }

    /// A context running **as `tracee`**, with `erlang:trace(Tracee, true, Flags)` already
    /// installed by the tracer and `MFA`/`MatchSpec`/`PatternFlags` installed as a function
    /// trace pattern. Returns the runtime and the table, plus the two pids.
    struct Fixture {
        rt: Runtime,
        table: ProcessTable,
        tracee: ProcessId,
        tracer: ProcessId,
    }

    impl Fixture {
        /// The fixture with the process flags `flags` (the `-eval` source of the flag list)
        /// and one installed function pattern, or none when `pattern` is `None`.
        fn new(
            flags: &str,
            pattern: Option<(&str, &str, u32, &str, &str)>,
        ) -> Fixture {
            let mut rt = runtime();
            let how_true = rt.intern("true");
            let flag_list = rt.eval(flags).unwrap_or_else(|e| panic!("{flags}: {e:?}"));
            let (mut table, tracee, tracer) = two_processes(&mut rt);
            let tracee_term = tracee.to_eterm();
            // The tracer installs the process flags on the tracee.
            {
                let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, tracer);
                let count = ctx
                    .call_bif("erlang", "trace", 3, &[tracee_term, how_true, flag_list])
                    .expect("trace/3");
                assert_eq!(beam_term::signed_value(count), 1);
            }
            if let Some((module, function, arity, match_spec, pattern_flags)) = pattern {
                let mfa = rt
                    .eval(&format!("{{{module}, {function}, {arity}}}"))
                    .expect("an MFA");
                let spec = rt.eval(match_spec).unwrap_or_else(|e| panic!("{match_spec}: {e:?}"));
                let list = rt
                    .eval(pattern_flags)
                    .unwrap_or_else(|e| panic!("{pattern_flags}: {e:?}"));
                let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, tracer);
                ctx.call_bif("erts_internal", "trace_pattern", 3, &[mfa, spec, list])
                    .expect("trace_pattern/3");
            }
            Fixture {
                rt,
                table,
                tracee,
                tracer,
            }
        }

        /// The events the **tracer** received, drained.
        fn events(&mut self) -> Vec<Eterm> {
            drain(&mut self.rt, &mut self.table, self.tracer)
        }

        /// Run `event` against a context executing **as the tracee** (the engine's seam).
        fn report(&mut self, event: TraceEvent<'_>) {
            let mut ctx = RuntimeContext::for_process(&mut self.rt, &mut self.table, self.tracee);
            ctx.execution_trace_event(event);
        }

        /// The single event the tracer received, or a panic naming what it got.
        fn one_event(&mut self) -> Eterm {
            let events = self.events();
            assert_eq!(events.len(), 1, "exactly one event: {events:?}");
            events[0]
        }

        /// Whether `execution_trace_enabled()` answers `true` for a context as the **tracee**.
        fn enabled(&mut self) -> bool {
            let ctx = RuntimeContext::for_process(&mut self.rt, &mut self.table, self.tracee);
            ctx.execution_trace_enabled()
        }
    }

    // -- the gate --------------------------------------------------------------

    /// `trace_enabled()` is `true` only for the `call`/`return_to` flags: no entry, a
    /// `[send]` entry and a cleared entry all answer `false`.
    #[test]
    fn trace_enabled_is_the_call_and_return_to_gate() {
        let mut none = Fixture::new("[]", None);
        assert!(!none.enabled(), "no trace flags at all");

        let mut send = Fixture::new("[send]", None);
        assert!(!send.enabled(), "the `send` flag is not the call family");

        let mut calls = Fixture::new("[call]", None);
        assert!(calls.enabled(), "the `call` flag arms the gate");

        let mut return_to = Fixture::new("[return_to]", None);
        assert!(return_to.enabled(), "the `return_to` flag arms the gate too");

        // `erlang:trace(Pid, false, [])` clears the entry again.
        let how_false = calls.rt.intern("false");
        let empty = calls.rt.eval("[]").expect("[]");
        let tracee_term = calls.tracee.to_eterm();
        let tracer = calls.tracer;
        {
            let mut ctx =
                RuntimeContext::for_process(&mut calls.rt, &mut calls.table, tracer);
            let count = ctx
                .call_bif("erlang", "trace", 3, &[tracee_term, how_false, empty])
                .expect("trace/3 off");
            assert_eq!(beam_term::signed_value(count), 1);
        }
        assert!(!calls.enabled(), "the cleared process is no longer traced");
    }

    // -- call ------------------------------------------------------------------

    /// A `[local]` pattern turns an engine `Call` into `{trace, Tracee, call, {M,F,Args}}`,
    /// with the **argument list** as the third element (`arity` unset).
    #[test]
    fn a_local_pattern_reports_a_call_with_the_argument_list() {
        let mut fixture = Fixture::new(
            "[call]",
            Some(("probe", "a2", 2, "true", "[local]")),
        );
        let tracee = fixture.tracee.to_eterm();
        fixture.report(TraceEvent::Call {
            module: "probe",
            function: "a2",
            arity: 2,
            args: &[make_small(1), make_small(2)],
        });
        let event = fixture.one_event();
        assert_eq!(terms::tuple_arity(event).expect("a tuple"), 4);
        assert_eq!(fixture.rt.atom_name_of(element(event, 0)), Some("trace"));
        assert_eq!(element(event, 1), tracee);
        assert_eq!(fixture.rt.atom_name_of(element(event, 2)), Some("call"));
        let mfa = element(event, 3);
        assert_eq!(fixture.rt.atom_name_of(element(mfa, 0)), Some("probe"));
        assert_eq!(fixture.rt.atom_name_of(element(mfa, 1)), Some("a2"));
        let args = terms::list_elements(element(mfa, 2)).expect("the argument list");
        assert_eq!(
            args.iter().map(|a| beam_term::signed_value(*a)).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// The `arity` process flag replaces the argument list with the bare arity.
    #[test]
    fn the_arity_flag_reports_the_bare_arity_instead_of_the_arguments() {
        let mut fixture = Fixture::new(
            "[call, arity]",
            Some(("probe", "a2", 2, "true", "[local]")),
        );
        fixture.report(TraceEvent::Call {
            module: "probe",
            function: "a2",
            arity: 2,
            args: &[make_small(1), make_small(2)],
        });
        let event = fixture.one_event();
        let mfa = element(event, 3);
        let third = element(mfa, 2);
        assert!(beam_term::is_small(third), "the bare arity, got {third:?}");
        assert_eq!(beam_term::signed_value(third), 2);
    }

    /// The `local`/`global` gate: a `[global]`-only pattern fires exactly when the callee
    /// module differs from the module whose code is executing (the external-dispatch
    /// approximation — see the module docs for the one residual divergence).
    #[test]
    fn a_global_pattern_fires_only_for_a_foreign_callee_module() {
        // No calling module (`None`, the `-eval` evaluator) counts as "differs".
        let mut foreign = Fixture::new(
            "[call]",
            Some(("probe", "a2", 2, "true", "[global]")),
        );
        foreign.report(TraceEvent::Call {
            module: "probe",
            function: "a2",
            arity: 2,
            args: &[make_small(1), make_small(2)],
        });
        assert_eq!(foreign.events().len(), 1, "an unknown caller fires");

        // A caller inside the callee's own module does NOT fire a `[global]` pattern.
        let mut own_module = Fixture::new(
            "[call]",
            Some(("probe", "a2", 2, "true", "[global]")),
        );
        {
            let mut ctx = RuntimeContext::for_process(
                &mut own_module.rt,
                &mut own_module.table,
                own_module.tracee,
            );
            // The module whose code is executing: `?MODULE:F(...)` — an external dispatch
            // whose callee module *is* the caller's.
            ctx.current_module = Some("probe".to_string());
            ctx.execution_trace_event(TraceEvent::Call {
                module: "probe",
                function: "a2",
                arity: 2,
                args: &[make_small(1), make_small(2)],
            });
        }
        assert!(
            own_module.events().is_empty(),
            "`?MODULE:F(...)` is under-reported by the heuristic"
        );

        // A different calling module DOES fire it.
        let mut other_caller = Fixture::new(
            "[call]",
            Some(("probe", "a2", 2, "true", "[global]")),
        );
        {
            let mut ctx = RuntimeContext::for_process(
                &mut other_caller.rt,
                &mut other_caller.table,
                other_caller.tracee,
            );
            ctx.current_module = Some("other".to_string());
            ctx.execution_trace_event(TraceEvent::Call {
                module: "probe",
                function: "a2",
                arity: 2,
                args: &[make_small(1), make_small(2)],
            });
        }
        assert_eq!(other_caller.events().len(), 1, "a foreign caller fires");
    }

    /// An unmatched MFA, an empty pattern table and a `call`-flag-less process all report
    /// nothing.
    #[test]
    fn an_unmatched_call_reports_nothing() {
        // The pattern selects another function.
        let mut other_function = Fixture::new(
            "[call]",
            Some(("probe", "b1", 1, "true", "[local]")),
        );
        other_function.report(TraceEvent::Call {
            module: "probe",
            function: "a2",
            arity: 2,
            args: &[make_small(1), make_small(2)],
        });
        assert!(other_function.events().is_empty());

        // No pattern installed at all.
        let mut no_pattern = Fixture::new("[call]", None);
        no_pattern.report(TraceEvent::Call {
            module: "probe",
            function: "a2",
            arity: 2,
            args: &[make_small(1), make_small(2)],
        });
        assert!(no_pattern.events().is_empty());

        // The process flag is `send`, not `call`.
        let mut not_calls = Fixture::new(
            "[send]",
            Some(("probe", "a2", 2, "true", "[local]")),
        );
        not_calls.report(TraceEvent::Call {
            module: "probe",
            function: "a2",
            arity: 2,
            args: &[make_small(1), make_small(2)],
        });
        assert!(not_calls.events().is_empty());
    }

    // -- return_from / return_to ----------------------------------------------

    /// `{return_trace}` in the match function's body arms `return_from`, which reports the
    /// returned value in a 5-tuple `{trace, Tracee, return_from, {M,F,A}, Value}`.
    #[test]
    fn a_return_trace_action_reports_return_from() {
        let mut fixture = Fixture::new(
            "[call]",
            Some(("probe", "a2", 2, "[{'_',[],[{return_trace}]}]", "[local]")),
        );
        let tracee = fixture.tracee.to_eterm();
        fixture.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        let event = fixture.one_event();
        assert_eq!(terms::tuple_arity(event).expect("a tuple"), 5);
        assert_eq!(element(event, 1), tracee);
        assert_eq!(
            fixture.rt.atom_name_of(element(event, 2)),
            Some("return_from")
        );
        let mfa = element(event, 3);
        assert_eq!(fixture.rt.atom_name_of(element(mfa, 0)), Some("probe"));
        assert_eq!(fixture.rt.atom_name_of(element(mfa, 1)), Some("a2"));
        assert_eq!(beam_term::signed_value(element(mfa, 2)), 2);
        assert_eq!(beam_term::signed_value(element(event, 4)), 3);
    }

    /// `MatchAll` (`true`) never arms `return_from`; neither does a bare `return_trace` atom
    /// body; neither does a process without the `call` flag.
    #[test]
    fn return_from_needs_a_return_trace_action_and_the_call_flag() {
        let mut match_all = Fixture::new("[call]", Some(("probe", "a2", 2, "true", "[local]")));
        match_all.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        assert!(match_all.events().is_empty(), "`true` arms nothing");

        let mut bare_atom = Fixture::new(
            "[call]",
            Some(("probe", "a2", 2, "[{'_',[],[return_trace]}]", "[local]")),
        );
        bare_atom.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        assert!(
            bare_atom.events().is_empty(),
            "a bare `return_trace` atom is not the `{{return_trace}}` action"
        );

        let mut no_call_flag = Fixture::new(
            "[send]",
            Some(("probe", "a2", 2, "[{'_',[],[{return_trace}]}]", "[local]")),
        );
        no_call_flag.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        assert!(
            no_call_flag.events().is_empty(),
            "`return_from` takes the process `call` flag"
        );
    }

    /// `return_from` fires even when the process lacks `return_to`, and arms nothing then.
    #[test]
    fn return_from_does_not_need_the_return_to_flag() {
        let mut fixture = Fixture::new(
            "[call]",
            Some(("probe", "a2", 2, "[{'_',[],[{return_trace}]}]", "[local]")),
        );
        fixture.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        fixture.report(TraceEvent::ReturnTo {
            module: "probe",
            function: "r2",
            arity: 0,
        });
        let events = fixture.events();
        assert_eq!(events.len(), 1, "only `return_from`: {events:?}");
        assert_eq!(
            fixture.rt.atom_name_of(element(events[0], 2)),
            Some("return_from")
        );
    }

    /// `return_to` is armed by the **returning** function's `[local]` pattern plus the
    /// process `call` + `return_to` flags, and reported for the function control returns
    /// **to**.
    #[test]
    fn an_armed_return_from_reports_return_to_for_the_caller() {
        let mut fixture = Fixture::new(
            "[call, return_to]",
            Some(("probe", "a2", 2, "true", "[local]")),
        );
        let tracee = fixture.tracee.to_eterm();
        fixture.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        fixture.report(TraceEvent::ReturnTo {
            module: "probe",
            function: "r2",
            arity: 0,
        });
        let events = fixture.events();
        assert_eq!(events.len(), 1, "`MatchAll` arms no `return_from`, but `return_to`: {events:?}");
        let event = events[0];
        assert_eq!(terms::tuple_arity(event).expect("a tuple"), 4);
        assert_eq!(element(event, 1), tracee);
        assert_eq!(fixture.rt.atom_name_of(element(event, 2)), Some("return_to"));
        let mfa = element(event, 3);
        assert_eq!(fixture.rt.atom_name_of(element(mfa, 0)), Some("probe"));
        assert_eq!(fixture.rt.atom_name_of(element(mfa, 1)), Some("r2"));
        assert_eq!(beam_term::signed_value(element(mfa, 2)), 0);
    }

    /// The three ways `return_to` stays silent: a `[global]`-only pattern never arms it, a
    /// process without the `return_to` flag never arms it, and a `ReturnTo` with no preceding
    /// `ReturnFrom` finds nothing armed.
    #[test]
    fn return_to_is_not_reported_without_its_preconditions() {
        // `[global]` only.
        let mut global = Fixture::new(
            "[call, return_to]",
            Some(("probe", "a2", 2, "true", "[global]")),
        );
        global.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        global.report(TraceEvent::ReturnTo {
            module: "probe",
            function: "r2",
            arity: 0,
        });
        assert!(
            global.events().is_empty(),
            "a `[global]` pattern never arms `return_to`"
        );

        // No `return_to` process flag.
        let mut no_flag = Fixture::new(
            "[call]",
            Some(("probe", "a2", 2, "true", "[local]")),
        );
        no_flag.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        no_flag.report(TraceEvent::ReturnTo {
            module: "probe",
            function: "r2",
            arity: 0,
        });
        assert!(no_flag.events().is_empty(), "`return_to` needs its own flag");

        // No preceding `ReturnFrom`: nothing is armed.
        let mut unarmed = Fixture::new(
            "[call, return_to]",
            Some(("probe", "a2", 2, "true", "[local]")),
        );
        unarmed.report(TraceEvent::ReturnTo {
            module: "probe",
            function: "r2",
            arity: 0,
        });
        assert!(unarmed.events().is_empty(), "nothing armed the return");
    }

    /// A `Call` clears a stale arm: a `ReturnFrom` that armed `return_to`, then a `Call`,
    /// then a bare `ReturnTo` reports nothing.
    #[test]
    fn a_call_clears_a_stale_return_to_arm() {
        let mut fixture = Fixture::new(
            "[call, return_to]",
            Some(("probe", "a2", 2, "true", "[local]")),
        );
        fixture.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        fixture.report(TraceEvent::Call {
            module: "probe",
            function: "a2",
            arity: 2,
            args: &[make_small(1), make_small(2)],
        });
        fixture.report(TraceEvent::ReturnTo {
            module: "probe",
            function: "r2",
            arity: 0,
        });
        let events = fixture.events();
        assert_eq!(events.len(), 1, "only the `call`: {events:?}");
        assert_eq!(fixture.rt.atom_name_of(element(events[0], 2)), Some("call"));
    }

    // -- the pattern-level `silent` flag --------------------------------------

    /// The `trace_pattern` `silent` flag suppresses the event entirely.
    #[test]
    fn a_silent_pattern_suppresses_the_event() {
        let mut call = Fixture::new(
            "[call]",
            Some(("probe", "a2", 2, "true", "[local, silent]")),
        );
        call.report(TraceEvent::Call {
            module: "probe",
            function: "a2",
            arity: 2,
            args: &[make_small(1), make_small(2)],
        });
        assert!(call.events().is_empty(), "a silent pattern reports nothing");

        let mut returned = Fixture::new(
            "[call, return_to]",
            Some(("probe", "a2", 2, "[{'_',[],[{return_trace}]}]", "[local, silent]")),
        );
        returned.report(TraceEvent::ReturnFrom {
            module: "probe",
            function: "a2",
            arity: 2,
            value: make_small(3),
        });
        returned.report(TraceEvent::ReturnTo {
            module: "probe",
            function: "r2",
            arity: 0,
        });
        assert!(returned.events().is_empty(), "both events are suppressed");
    }

    // -- the emit path ---------------------------------------------------------

    /// The events are ordinary messages of the tracee's **tracer**: a process with no trace
    /// entry reports nothing even when the engine calls the hook.
    #[test]
    fn an_untraced_process_reports_nothing() {
        let mut rt = runtime();
        let mut sink = NullSink;
        let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
        ctx.execution_trace_event(TraceEvent::Call {
            module: "probe",
            function: "a2",
            arity: 2,
            args: &[make_small(1), make_small(2)],
        });
        assert!(!ctx.execution_trace_enabled(), "no trace entry");
        ctx.receive_reset().expect("timeout");
        assert!(ctx.receive_next().expect("receive_next").is_none());
    }
}
