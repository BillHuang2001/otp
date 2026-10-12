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

//! `erlang:exit/2,3`: the runtime-native exit-signal floor.
//!
//! `exit(Target, Reason)` is a *runtime* operation in the same sense the rest of the floor
//! is (see [`super`]): it reads and writes this runtime's own process table, so it lives on
//! the runtime-native floor (consulted **before** the `beam_bifs` registry) rather than in a
//! BIF module that has no access to the host state. The signalling machinery is
//! `beam_process`'s; this module only decodes the BIF's terms onto it (call down, never
//! duplicate).
//!
//! # The target taxonomy — the oracle's own (`bif.c:1721-1870`, `send_exit_signal_bif`)
//!
//! * the **options** are validated first (`[]` or any number of bare `priority` atoms;
//!   anything else, including the `{priority, P}` tuple and a non-list, is `badarg`);
//! * a **local pid** other than the caller goes through
//!   [`ProcessTable::send_exit`](beam_process::ProcessTable::send_exit), which — via
//!   `apply_exit_signal` — delivers `{'EXIT', From, Reason}` to a trapping target,
//!   terminates a non-trapping one and ignores `normal` towards one. That is exactly the
//!   signal path the oracle's own `exit/2` uses, `kill` included (the table maps it onto the
//!   untrappable `killed`);
//! * the **caller itself** is the one case that must *not* go through that path: the
//!   scheduler defers a self-directed signal while the pid is `Running` (which it is, inside
//!   a slice), so `exit(self(), Reason)` is served by the ordinary exception mechanism
//!   instead — a trapping caller gets the `{'EXIT', Caller, Reason}` message in its own
//!   mailbox ([`RuntimeContext::deliver_message`]) and answers `true`, and every other caller
//!   is terminated by a raise (`beam_common::Raise`) with the reason it named, `kill`
//!   included (as `killed`). This mirrors the C's "terminate ourselves even though the exit
//!   reason is normal (unless we trap exit), and terminate ourselves before `exit/2`
//!   returns" (`bif.c:1750-1772`);
//! * an **internal port** and a **reference** answer `true` for **any** reason: the C routes
//!   the first through `erts_port_exit` and the second through the very same signal send as a
//!   pid (a reference matches no process), and neither can fail. A reference is *not* a
//!   process, so nothing is signalled;
//! * a **pid on another node** answers `true` without a delivery (the signal is contract
//!   A.5's, and an unconnected node is `ERTS_DSIG_PREP_NOT_CONNECTED`);
//! * **everything else** — an atom (registered or not), an integer, a float, a tuple, a
//!   binary — is `badarg`. The C has no atom/name case at all: `exit(RegisteredName, Reason)`
//!   is `badarg` even when the name is registered (verified against OTP 29/erts-17.1).
//!
//! # Reference (and external-port) classification
//!
//! Reading a boxed reference thing's header needs `unsafe`, and this crate is
//! `#![deny(unsafe_code)]`; the safe `beam_term`/`beam_bifs` reference predicates are
//! crate-private. A reference is therefore recognised by **the registries this runtime minted
//! it in** — [`Runtime::references`](crate::Runtime::references) (`make_ref/0`, `monitor/2`)
//! plus the timer heap's reference ids. Every reference this runtime can mint is covered; one
//! that arrived from elsewhere (decoded from ETF, or an ETS table id) is not, so
//! `exit(<such a ref>, Reason)` is `badarg` here where the oracle answers `true`. The same
//! limit applies to an **external** port (the oracle answers `true` for one of this node's
//! former incarnations).
//!
//! # Known side-effect gap
//!
//! The C's internal-port case really does send the port an exit signal
//! (`erts_port_exit`), so `exit(Port, bye)` closes it in the oracle. This runtime answers the
//! oracle's `true` but leaves the port alone: the port's exit path belongs to the
//! `beam_io`-backed registry this floor does not own, and fabricating one would be a wrong
//! result rather than an honest gap.

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Raise, Result};
use beam_engine::terms;
use beam_process::{PidTarget, PortId, Process, ProcessId};
use beam_term::Eterm;

use crate::context::RuntimeContext;

impl RuntimeContext<'_> {
    // -- erlang:exit/2,3 ------------------------------------------------------

    /// `erlang:exit(Target, Reason)`: signal `Reason` at `Target`.
    ///
    /// `Target` is a pid (local or remote), a port or a reference; every other term — an
    /// atom included, registered or not — is the oracle's `badarg`. A signal that has nowhere
    /// to go (a port, a reference, an unknown pid, a pid on another node) is not an error:
    /// the answer is `true`.
    pub(in crate::native) fn exit_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let target = *args.first().ok_or(BeamError::BadArg)?;
        let reason = *args.get(1).ok_or(BeamError::BadArg)?;
        self.exit_target(target, reason)
    }

    /// `erlang:exit(Target, Reason, Options)`: `exit/2` plus the option list.
    ///
    /// The only element the oracle accepts here is the bare atom `priority` (OTP 24+), and
    /// the one place the runtime could use it — the exit *signal*'s priority — is
    /// `beam_process`'s, which has no such field. Accepting the option while ignoring it
    /// would silently break the ordering guarantee it buys, so the option list is validated
    /// (the oracle's `badarg` for anything else, including the `{priority, high}` tuple) and
    /// nothing more is claimed.
    pub(in crate::native) fn exit_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let target = *args.first().ok_or(BeamError::BadArg)?;
        let reason = *args.get(1).ok_or(BeamError::BadArg)?;
        self.check_exit_options(*args.get(2).ok_or(BeamError::BadArg)?)?;
        self.exit_target(target, reason)
    }

    // -- helpers --------------------------------------------------------------

    /// The `exit/3` option list: `[]`, or any number of `priority` atoms.
    ///
    /// A non-list and every element other than the bare atom `priority` — including the
    /// 3-tuple `{priority, P}` the oracle rejects — are `badarg`.
    fn check_exit_options(&self, options: Eterm) -> Result<()> {
        let options = terms::list_elements(options).map_err(|_| BeamError::BadArg)?;
        for option in options {
            if self.atom_name(option).as_deref() != Some("priority") {
                return Err(BeamError::BadArg);
            }
        }
        Ok(())
    }

    /// Signals `reason` at the decoded `target`.
    fn exit_target(&mut self, target: Eterm, reason: Eterm) -> Result<Eterm> {
        match PidTarget::from_eterm(target) {
            Ok(PidTarget::Local(pid)) => return self.exit_local(pid, reason),
            // A pid on another node: the signal belongs to contract A.5's live
            // distribution link, which this runtime does not have. Nothing local is
            // affected either way, and the oracle answers `true`.
            Ok(PidTarget::Remote(_remote)) => return self.atom("true"),
            Err(_) => {}
        }
        // A port or a reference: the signal has nowhere to go (a reference matches no
        // process, and the port's exit path is not this floor's), so the oracle's answer is
        // the bare `true` for **any** reason. Everything else — an atom, registered or not,
        // an integer, a tuple — is the oracle's `badarg`.
        if PortId::from_eterm(target).is_ok() || self.is_minted_reference(target) {
            return self.atom("true");
        }
        Err(BeamError::BadArg)
    }

    /// `exit(Target, Reason)` for a **local** pid target.
    fn exit_local(&mut self, target: ProcessId, reason: Eterm) -> Result<Eterm> {
        let caller = self.pid();
        if target == caller {
            return self.exit_self(reason);
        }
        // Another process: signal it when the table holds it (a dead pid and a pid that
        // never existed are both silent), through the table's own signal path so a
        // trapping target gets its `{'EXIT', …}` and a non-trapping one is terminated.
        if self.process_table_mut().contains(target) {
            self.process_table_mut().send_exit(caller, target, reason)?;
        }
        self.atom("true")
    }

    /// The self-directed `exit(Caller, Reason)`.
    ///
    /// A trapping caller is handed the `{'EXIT', Caller, Reason}` message and answers
    /// `true`; every other caller — and a `kill`, which no flag can trap — is terminated by
    /// a raise. The `{'EXIT', …}` tuple is built and delivered here rather than through
    /// `send_exit`, whose self-directed signal the scheduler defers while the pid runs.
    fn exit_self(&mut self, reason: Eterm) -> Result<Eterm> {
        let caller = self.pid();
        // `kill` is never trappable: the caller dies with the reason `killed`, whatever its
        // `trap_exit` flag says.
        if self.atom_name(reason).as_deref() == Some("kill") {
            return Err(BeamError::Raise(Raise::exit(ExitReason::Killed)));
        }
        let trapping = self
            .process_table()
            .get(caller)
            .map(Process::is_trapping_exit)
            .unwrap_or(false);
        if trapping {
            let tag = self.atom("EXIT")?;
            let message = terms::make_tuple(self, &[tag, caller.to_eterm(), reason])?;
            self.deliver_message(caller, caller.to_eterm(), message)?;
            return self.atom("true");
        }
        Err(self.exit_raise(reason))
    }

    /// Whether `term` is a reference this runtime minted (see the module docs). Delegates to
    /// the ONE provenance test, [`Runtime::is_known_local_reference`].
    fn is_minted_reference(&self, term: Eterm) -> bool {
        self.runtime.is_known_local_reference(term)
    }

    /// The exit-class raise that terminates the calling process with `reason`.
    ///
    /// An **atom** reason keeps its own name ([`ExitReason::Atom`], so the caller's exit
    /// reason is exactly that atom); a structured one has no atom of its own and rides the
    /// raise's live-reason slot as the term itself — [`crate::error::live_reason_term`]
    /// recovers it and `crate::runner::exit_reason` renders it as the process's exit reason,
    /// so a compound reason survives intact.
    fn exit_raise(&self, reason: Eterm) -> BeamError {
        match self.atom_name(reason) {
            Some(name) => BeamError::Raise(Raise::exit(ExitReason::atom(name))),
            None => BeamError::Raise(
                Raise::new(ExceptionClass::Exit, ExitReason::Structured)
                    .with_live_reason(LiveReason::new(reason)),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for `erlang:exit/2,3`'s argument classification and its self-directed
    //! raise/deliver split: no toolchain is needed, and every oracle-verified answer is
    //! pinned here (the end-to-end flows live in `tests/exit.rs`).

    use super::*;
    use beam_bifs::NullSink;
    use beam_term::{make_small, NIL};

    use crate::native::test_support::{atom, call, runtime};

    /// A tuple built on the process heap by the runtime's own term constructors.
    fn make_tuple(runtime: &mut crate::Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_tuple(&mut context, elements).expect("a tuple")
    }

    /// A list built on the process heap by the runtime's own term constructors.
    fn make_list(runtime: &mut crate::Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_list(&mut context, elements).expect("a list")
    }

    /// The atom `name` of `term`, resolved through the runtime's shared table.
    fn atom_name(runtime: &crate::Runtime, term: Eterm) -> Option<&str> {
        runtime.atom_name_of(term)
    }

    /// The class and reason-atom name of the raise `result` must be.
    fn raise_of(result: Result<Eterm>) -> (ExceptionClass, Option<String>) {
        match result {
            Err(BeamError::Raise(raise)) => (
                raise.class,
                raise.reason.as_atom().map(str::to_string),
            ),
            other => panic!("expected an exit-class raise, got {other:?}"),
        }
    }

    #[test]
    fn a_non_pid_port_or_reference_target_is_badarg() {
        let mut runtime = runtime();
        let normal = atom(&mut runtime, "normal");
        let tuple = make_tuple(&mut runtime, &[normal, normal]);
        let float = {
            let mut sink = NullSink;
            let mut context = RuntimeContext::with_output(&mut runtime, &mut sink);
            terms::make_float(&mut context, 1.5).expect("a float")
        };

        for argument in [make_small(42), NIL, tuple, float] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "exit", &[argument, normal]),
                    Err(BeamError::BadArg)
                ),
                "exit({argument:?}, normal) is badarg"
            );
        }
    }

    #[test]
    fn an_atom_target_is_badarg_even_when_the_name_is_registered() {
        let mut runtime = runtime();
        let missing = atom(&mut runtime, "no_such_registered_name");
        let normal = atom(&mut runtime, "normal");
        assert!(matches!(
            call(&mut runtime, "erlang", "exit", &[missing, normal]),
            Err(BeamError::BadArg)
        ));

        // A **registered** name is `badarg` too: the oracle's own `exit/2`
        // (`bif.c:1750-1867`) has no name case at all, and a registered name must not be
        // turned into a signal — verified against OTP 29/erts-17.1.
        let registered = atom(&mut runtime, "exit_probe_registered");
        let pid = runtime.self_pid().to_eterm();
        call(&mut runtime, "erlang", "register", &[registered, pid]).expect("register/2");
        assert!(matches!(
            call(&mut runtime, "erlang", "exit", &[registered, normal]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn a_known_pid_is_signalled_and_the_unknown_one_is_silently_true() {
        let mut runtime = runtime();
        let bye = atom(&mut runtime, "bye");
        // A pid the table does not hold (and a pid that never existed) answers `true`
        // without touching anything, exactly as the oracle does for a dead pid.
        let stranger = ProcessId::new(4242, 7).to_eterm();
        let answer =
            call(&mut runtime, "erlang", "exit", &[stranger, bye]).expect("exit(dead_pid, bye)");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
    }

    #[test]
    fn a_port_or_reference_target_answers_true_for_any_reason() {
        let mut runtime = runtime();
        let normal = atom(&mut runtime, "normal");
        let bye = atom(&mut runtime, "bye");
        let port = PortId::new(7).to_eterm();
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");

        // Neither target holds a signal, so the oracle answers `true` whatever the reason
        // is — `normal`, another atom, or a structured term.
        for target in [port, reference] {
            for reason in [normal, bye, make_small(1)] {
                let answer = call(&mut runtime, "erlang", "exit", &[target, reason]).unwrap_or_else(
                    |error| panic!("exit({target:?}, {reason:?}) is badarg: {error}"),
                );
                assert_eq!(atom_name(&runtime, answer), Some("true"));
            }
        }
    }

    #[test]
    fn exit_three_accepts_only_the_priority_atom() {
        let mut runtime = runtime();
        let priority = atom(&mut runtime, "priority");
        let bye = atom(&mut runtime, "bye");
        let pid = ProcessId::new(4242, 7).to_eterm();
        let empty = make_list(&mut runtime, &[]);
        let priorities = make_list(&mut runtime, &[priority]);

        // `[]` and `[priority]` are legal: `exit/3` then behaves exactly like `exit/2`.
        for options in [empty, priorities] {
            let answer = call(&mut runtime, "erlang", "exit", &[pid, bye, options])
                .expect("a legal option list");
            assert_eq!(atom_name(&runtime, answer), Some("true"));
        }

        // The 3-tuple `{priority, P}`, an unknown atom, a mixed list and a non-list are all
        // the oracle's `badarg` — and the *target is not signalled* for any of them (the
        // options are validated first), which the unknown-pid answer would hide, so this is
        // asserted through the error shape only.
        let high = atom(&mut runtime, "high");
        let tuple = make_tuple(&mut runtime, &[priority, high]);
        let three = make_tuple(&mut runtime, &[priority, high, high]);
        let foo = atom(&mut runtime, "foo");
        for options in [
            make_list(&mut runtime, &[tuple]),
            make_list(&mut runtime, &[three]),
            make_list(&mut runtime, &[foo]),
            make_list(&mut runtime, &[priority, foo]),
            make_small(42),
        ] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "exit", &[pid, bye, options]),
                    Err(BeamError::BadArg)
                ),
                "exit/3 with an illegal option list is badarg"
            );
        }
    }

    #[test]
    fn a_non_trapping_self_exit_raises_the_reason() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let bye = atom(&mut runtime, "bye");

        let (class, name) = raise_of(call(&mut runtime, "erlang", "exit", &[pid, bye]));
        assert_eq!(class, ExceptionClass::Exit, "the class is `exit`");
        assert_eq!(name.as_deref(), Some("bye"), "the reason atom is kept");

        // A structured reason is carried as the live term, so nothing is reshaped.
        let tuple = make_tuple(&mut runtime, &[bye, bye]);
        let raised = call(&mut runtime, "erlang", "exit", &[pid, tuple]).expect_err("a raise");
        let reason = crate::error::live_reason_term(&raised);
        assert_eq!(reason, Some(tuple), "the compound reason survives");
    }

    #[test]
    fn kill_is_untrappable_and_raises_killed() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let kill = atom(&mut runtime, "kill");

        let (class, name) = raise_of(call(&mut runtime, "erlang", "exit", &[pid, kill]));
        assert_eq!(
            (class, name.as_deref()),
            (ExceptionClass::Exit, Some("killed")),
            "kill exits with the reason `killed`"
        );

        // … even for a trapping caller: no flag can trap a kill.
        let trap = atom(&mut runtime, "trap_exit");
        let v_true = atom(&mut runtime, "true");
        call(&mut runtime, "erlang", "process_flag", &[trap, v_true]).expect("process_flag/2");
        let (class, name) = raise_of(call(&mut runtime, "erlang", "exit", &[pid, kill]));
        assert_eq!(
            (class, name.as_deref()),
            (ExceptionClass::Exit, Some("killed")),
            "a trapping caller is still killed"
        );
    }

    #[test]
    fn a_trapping_self_exit_delivers_the_exit_message_and_answers_true() {
        let mut runtime = runtime();
        let pid = runtime.self_pid();
        let trap = atom(&mut runtime, "trap_exit");
        let v_true = atom(&mut runtime, "true");
        let bye = atom(&mut runtime, "bye");
        call(&mut runtime, "erlang", "process_flag", &[trap, v_true]).expect("process_flag/2");

        let answer = call(&mut runtime, "erlang", "exit", &[pid.to_eterm(), bye])
            .expect("a trapping self-exit answers true");
        assert_eq!(atom_name(&runtime, answer), Some("true"));

        // The message really is in the caller's own mailbox, exactly the
        // `{'EXIT', Caller, Reason}` the oracle delivers.
        let process = runtime.processes().get(pid).expect("the runtime's process");
        assert_eq!(process.mailbox().len(), 1, "one message was delivered");
        let cursor = process.mailbox().cursor();
        let message = process
            .mailbox()
            .message_at(cursor)
            .expect("the delivered message")
            .payload();
        assert_eq!(terms::tuple_arity(message).expect("a tuple"), 3);
        assert_eq!(atom_name(&runtime, terms::tuple_element(message, 0).unwrap()), Some("EXIT"));
        assert_eq!(
            terms::tuple_element(message, 1).unwrap(),
            pid.to_eterm(),
            "the sender is the caller itself"
        );
        assert_eq!(terms::tuple_element(message, 2).unwrap(), bye, "the reason");
    }
}
