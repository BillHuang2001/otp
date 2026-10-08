//! `erlang:exit/2,3`: the runtime-native exit-signal floor.
//!
//! `exit(Target, Reason)` is a *runtime* operation in the same sense the rest of the floor
//! is (see [`super`]): it reads and writes this runtime's own process table, so it lives on
//! the runtime-native floor (consulted **before** the `beam_bifs` registry) rather than in a
//! BIF module that has no access to the host state. The signalling machinery is
//! `beam_process`'s; this module only decodes the BIF's terms onto it (call down, never
//! duplicate):
//!
//! * a **local pid** other than the caller goes through
//!   [`ProcessTable::send_exit`](beam_process::ProcessTable::send_exit), which — via
//!   `apply_exit_signal` — delivers `{'EXIT', From, Reason}` to a trapping target and
//!   terminates a non-trapping one, ignoring `normal` towards a non-trapping one. That is
//!   exactly the signal path the oracle's own `exit/2` uses;
//! * the **caller itself** is the one case that must *not* go through that path: the
//!   scheduler defers a self-directed signal while the pid is `Running` (which it is, inside
//!   a slice), so `exit(self(), Reason)` is served by the ordinary exception mechanism
//!   instead — a trapping caller gets the `{'EXIT', Caller, Reason}` message in its own
//!   mailbox ([`RuntimeContext::deliver_message`]) and answers `true`, and every other caller
//!   is terminated by a raise (`beam_common::Raise`), with `kill` always raising the
//!   untrappable `killed`;
//! * an **atom** target is a registered name ([`ProcessTable::whereis`](beam_process::ProcessTable::whereis));
//!   an unknown name is the oracle's `badarg`;
//! * a **port** or a **reference** target accepts only the reason `normal` and then answers
//!   `true`: a signal has nowhere to go, and the oracle accepts the sentinel target for
//!   exactly that reason while rejecting every other reason with `badarg`.
//!
//! # Reference targets are classified from the runtime's own registries
//!
//! Reading a boxed reference thing's header needs `unsafe`, and this crate is
//! `#![deny(unsafe_code)]`; the safe `beam_term`/`beam_bifs` reference predicates are
//! crate-private. A reference is therefore recognised by **the registries this runtime minted
//! it in** — [`Runtime::references`](crate::Runtime::references) (`make_ref/0`, `monitor/2`)
//! plus the timer heap's reference ids. Every reference this runtime can mint is covered; one
//! that arrived from elsewhere (decoded from ETF, or an ETS table id) is not, so
//! `exit(<such a ref>, normal)` is `badarg` here where the oracle answers `true`.

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Raise, Result};
use beam_engine::terms;
use beam_process::{PidTarget, PortId, Process, ProcessId};
use beam_term::{atom_val, is_atom, Eterm};

use crate::context::RuntimeContext;

impl RuntimeContext<'_> {
    // -- erlang:exit/2,3 ------------------------------------------------------

    /// `erlang:exit(Target, Reason)`: signal `Reason` at `Target`.
    ///
    /// `Target` is a pid (local or remote), a registered name, or a port/reference
    /// sentinel; anything else is the oracle's `badarg`. A signal that has nowhere to go —
    /// an unknown pid, a pid on another node, a port, a reference — still answers `true`,
    /// exactly as the oracle does.
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
        if is_atom(target) {
            let Some(pid) = self.process_table().whereis(atom_val(target)) else {
                // An unregistered name is the oracle's `badarg` (the name *is* the target
                // here, so a name nothing answers to cannot be silently ignored).
                return Err(BeamError::BadArg);
            };
            return self.exit_local(pid, reason);
        }
        if PortId::from_eterm(target).is_ok() || self.is_minted_reference(target) {
            return self.exit_sentinel(reason);
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

    /// `exit(Target, Reason)` for a target that holds no signal: a port or a reference.
    ///
    /// Only the reason `normal` is accepted (the oracle's sentinel form); every other reason
    /// is `badarg`.
    fn exit_sentinel(&mut self, reason: Eterm) -> Result<Eterm> {
        if self.atom_name(reason).as_deref() != Some("normal") {
            return Err(BeamError::BadArg);
        }
        self.atom("true")
    }

    /// Whether `term` is a reference this runtime minted (see the module docs).
    fn is_minted_reference(&self, term: Eterm) -> bool {
        self.runtime.references.contains_key(&term) || self.runtime.timers.is_timer_ref(term)
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
    fn a_non_pid_non_name_target_is_badarg() {
        let mut runtime = runtime();
        let normal = atom(&mut runtime, "normal");
        let tuple = make_tuple(&mut runtime, &[normal, normal]);

        for argument in [make_small(42), NIL, tuple] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "exit", &[argument, normal]),
                    Err(BeamError::BadArg)
                ),
                "exit({argument:?}, normal) is badarg"
            );
        }
        // A float is not a pid either.
        assert!(matches!(
            call(&mut runtime, "erlang", "exit", &[normal, normal]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn an_unregistered_name_target_is_badarg() {
        let mut runtime = runtime();
        let missing = atom(&mut runtime, "no_such_registered_name");
        let normal = atom(&mut runtime, "normal");
        assert!(matches!(
            call(&mut runtime, "erlang", "exit", &[missing, normal]),
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
    fn a_port_or_reference_target_accepts_only_the_normal_reason() {
        let mut runtime = runtime();
        let normal = atom(&mut runtime, "normal");
        let bye = atom(&mut runtime, "bye");
        let port = PortId::new(7).to_eterm();
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");

        for target in [port, reference] {
            let answer = call(&mut runtime, "erlang", "exit", &[target, normal])
                .expect("exit(port_or_ref, normal)");
            assert_eq!(atom_name(&runtime, answer), Some("true"));
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "exit", &[target, bye]),
                    Err(BeamError::BadArg)
                ),
                "a non-normal reason towards a port/reference is badarg"
            );
            // A *structured* reason is rejected the same way (only `normal` is accepted).
            let structured = make_small(1);
            assert!(matches!(
                call(&mut runtime, "erlang", "exit", &[target, structured]),
                Err(BeamError::BadArg)
            ));
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
