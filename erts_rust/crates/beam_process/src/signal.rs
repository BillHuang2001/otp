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

//! Exit signals: the well-known atoms, the propagation rule and the signal-message builders.
//!
//! The exit semantics of Erlang are a pure function of the reason, the target's `trap_exit` flag
//! **and the signal's family** — which is exactly what [`exit_policy`] computes. The family is the
//! C's `ERTS_PROC_SIG_OP(tag)` on a `ERTS_SIG_Q_TYPE_GEN_EXIT` signal
//! (`erts/emulator/beam/erl_proc_sig_queue.h:176-177`, `erl_proc_sig_queue.c:4740-4770`):
//!
//! * [`ExitFamily::Explicit`] — `ERTS_SIG_Q_OP_EXIT` (0), an explicit `erlang:exit/2` signal:
//!   * reason `kill` ⇒ [`ExitPolicy::Terminate`] with reason `killed` — **untrappable**;
//!   * otherwise trapping ⇒ [`ExitPolicy::Trap`] — the reason is delivered as an
//!     `{'EXIT', From, Reason}` message;
//!   * otherwise reason `normal` ⇒ [`ExitPolicy::Ignore`];
//!   * otherwise ⇒ [`ExitPolicy::Terminate`] with the reason unchanged.
//! * [`ExitFamily::Linked`] — `ERTS_SIG_Q_OP_EXIT_LINKED` (1), a link exit (a linked peer's
//!   termination, a node-down or a port-close cascade). Here `kill` is **trappable**, and the
//!   reason is never rewritten:
//!   * trapping ⇒ [`ExitPolicy::Trap`] (the peer receives `{'EXIT', From, kill}` and survives);
//!   * otherwise reason `normal` ⇒ [`ExitPolicy::Ignore`];
//!   * otherwise ⇒ [`ExitPolicy::Terminate`] with the reason **unchanged** — a `kill` link exit
//!     terminates a non-trapping process with `kill`, never `killed`.
//!
//! The C's `ERTS_SIG_LNK_X_FLAG_NORMAL_KILLS` escape (`erl_proc_sig_queue.c:4758-4759`,
//! `:1809`), which makes a `normal` link exit terminate anyway, is **not modelled** (a documented
//! gap).
//!
//! The **third** signal category — a monitor `{'DOWN', …}`
//! (`ERTS_SIG_Q_OP_MONITOR_DOWN`, `erl_proc_sig_queue.h:178`) — never consults this policy at all:
//! the C handles it in its own arm (`erl_proc_sig_queue.c:6355`) and this crate in its own
//! ([`crate::ptab::ProcessTable`]'s `deliver_down_message`), so a DOWN is delivered
//! **unconditionally**, whatever the receiver's `trap_exit` flag says.
//!
//! # The receive-time reading of the policy
//!
//! The same decision is read differently at two points. At a **schedule-in** (or on an eager
//! signal to a non-executing receiver) the whole queue is applied and a terminating verdict kills
//! the receiver on the spot. During a **receive scan** the receiver is executing, so a terminating
//! verdict must only *stop the scan* and leave the signal queued for the schedule-out net — the C's
//! `i_loop_rec` → `do_schedule` ([`ReceiveApplicability`],
//! [`receive_applicability`]). Both readings are derived from this one [`exit_policy`]; neither
//! re-implements it.
//!
//! The message builders allocate the message terms on the *receiving* process's heap
//! (through `beam_alloc::Heap`), so a built `{'EXIT', …}`/`{'DOWN', …}` term is already
//! owned by its recipient.
//!
//! A `{'DOWN', …}` builder takes an optional **user tag** — the `erlang:monitor/3`
//! `{tag, Tag}` option. `None` reproduces the default `'DOWN'` head atom (a byte-identical
//! `{'DOWN', Reference, Kind, Target, Reason}`), `Some(tag)` puts `tag` there instead
//! (`{Tag, Reference, Kind, Target, Reason}`), exactly as the oracle delivers it.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_monitor_link.c`,
//! `erl_proc_sig_queue.{c,h}` (the `ERTS_SIG_Q_OP_EXIT` / `ERTS_SIG_Q_OP_EXIT_LINKED` families and
//! `handle_exit_signal`) and the `am_exit`/`am_down`/`am_kill`/`am_killed` atoms of `atom.names`.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{atom_val, is_atom, make_atom, Atom, AtomTable, Eterm};

use crate::pid::{PortId, ProcessId};
use crate::remote::RemotePid;
use crate::signal_queue::PendingSignal;

/// The well-known atom indices used to build signal messages.
///
/// Interned once from the module's [`AtomTable`] so the signal machinery never re-interns
/// a name on a hot path; the crate's other units read them through the accessors.
///
/// Every field is an [`Atom`] — a `Copy` immediate — so the whole set is `Copy`: a signal builder
/// can **capture it by value** instead of borrowing `ProcessTable::atoms`, which is what lets a
/// build closure be handed to a `&mut self` method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalAtoms {
    exit: Atom,
    down: Atom,
    process: Atom,
    port: Atom,
    normal: Atom,
    kill: Atom,
    killed: Atom,
    noproc: Atom,
    system: Atom,
}

impl SignalAtoms {
    /// Interns `'EXIT'`, `'DOWN'`, `process`, `port`, `normal`, `kill`, `killed`, `noproc` and
    /// `system` in `t`.
    ///
    /// `system` is the `'DOWN'` **sender** (`ERL_MESSAGE_FROM`) of a monitor that failed **by
    /// name** — an `erlang:monitor(process, Name)` whose name is not registered
    /// (`erl_proc_sig_queue.c:4991`).
    pub fn from_table(t: &mut AtomTable) -> SignalAtoms {
        SignalAtoms {
            exit: t.intern("EXIT"),
            down: t.intern("DOWN"),
            process: t.intern("process"),
            port: t.intern("port"),
            normal: t.intern("normal"),
            kill: t.intern("kill"),
            killed: t.intern("killed"),
            noproc: t.intern("noproc"),
            system: t.intern("system"),
        }
    }
    /// The `'EXIT'` atom.
    pub fn exit(&self) -> Atom {
        self.exit
    }

    /// The `'DOWN'` atom.
    pub fn down(&self) -> Atom {
        self.down
    }

    /// The `process` atom (the tag of a `{'DOWN', …}` message).
    pub fn process(&self) -> Atom {
        self.process
    }

    /// The `port` atom (the tag of a `{'DOWN', …}` message about a port).
    pub fn port(&self) -> Atom {
        self.port
    }

    /// The `normal` atom.
    pub fn normal(&self) -> Atom {
        self.normal
    }
    /// The `kill` atom.
    pub fn kill(&self) -> Atom {
        self.kill
    }

    /// The `killed` atom.
    pub fn killed(&self) -> Atom {
        self.killed
    }

    /// The `noproc` atom — the reason of the immediate `{'DOWN', …}` a monitor on a
    /// dead/unknown target yields.
    pub fn noproc(&self) -> Atom {
        self.noproc
    }

    /// The `system` atom — the `'DOWN'` **sender** of a monitor that failed **by name**, i.e. an
    /// `erlang:monitor(process, Name)` whose name was not registered.
    pub fn system(&self) -> Atom {
        self.system
    }

    /// The `normal` atom as a term.
    pub fn normal_term(&self) -> Eterm {
        make_atom(self.normal)
    }

    /// The `kill` atom as a term.
    pub fn kill_term(&self) -> Eterm {
        make_atom(self.kill)
    }

    /// The `killed` atom as a term.
    pub fn killed_term(&self) -> Eterm {
        make_atom(self.killed)
    }

    /// The `noproc` atom as a term.
    pub fn noproc_term(&self) -> Eterm {
        make_atom(self.noproc)
    }

    /// The `system` atom as a term — an **immediate**, so it is safe to store in a message's
    /// `from` slot (a non-root the collector never relocates).
    pub fn system_term(&self) -> Eterm {
        make_atom(self.system)
    }
    /// Classifies an exit reason against the well-known atoms.
    pub fn classify(&self, reason: Eterm) -> ReasonClass {
        if !is_atom(reason) {
            return ReasonClass::Other;
        }
        let atom = atom_val(reason);
        if atom == self.normal {
            ReasonClass::Normal
        } else if atom == self.kill {
            ReasonClass::Kill
        } else {
            ReasonClass::Other
        }
    }
}

/// The class of an exit reason, as far as propagation is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasonClass {
    /// The `normal` reason: silently ignored by a non-trapping target, in **both** families.
    Normal,
    /// The `kill` reason: **untrappable only in the [`ExitFamily::Explicit`] family**, where it
    /// also terminates with the rewritten `killed`; a [`ExitFamily::Linked`] `kill` is an ordinary
    /// trappable reason (see [`exit_policy`]).
    Kill,
    /// Every other reason.
    Other,
}

/// The **family** an exit signal belongs to — the C's `ERTS_PROC_SIG_OP(tag)` on an
/// `ERTS_SIG_Q_TYPE_GEN_EXIT` signal (`erts/emulator/beam/erl_proc_sig_queue.h:176-177`), which is
/// what decides whether `kill` is untrappable (`erl_proc_sig_queue.c:4740-4770`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitFamily {
    /// An explicit `erlang:exit/2` signal — the C's `ERTS_SIG_Q_OP_EXIT` (0). Here `kill` is
    /// **untrappable** and terminates with the rewritten `killed`.
    Explicit,
    /// A link exit — the C's `ERTS_SIG_Q_OP_EXIT_LINKED` (1): a linked peer's termination, or the
    /// node-down / port-close cascade. Here `kill` **is** trappable and the reason is never
    /// rewritten.
    Linked,
}

/// An exit signal from one process to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitSignal {
    /// The signalling process.
    pub from: ProcessId,
    /// The exit reason term.
    pub reason: Eterm,
}

/// What the target of an exit signal must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitPolicy {
    /// Terminate with this reason.
    Terminate(Eterm),
    /// Deliver `{'EXIT', From, Reason}` as a message and survive.
    Trap,
    /// Do nothing: the signal is discarded.
    Ignore,
}

/// Computes the propagation rule for `reason` against a target whose `trap_exit` flag is
/// `trapping`, for a signal of family `family` — the C's `handle_exit_signal`
/// (`erts/emulator/beam/erl_proc_sig_queue.c:4740-4770`):
///
/// ```text
/// untrappable = family == Explicit && classify(reason) == Kill
/// if !untrappable && trapping                             -> Trap
/// else if classify(reason) == Normal                      -> Ignore
/// else if untrappable (Explicit && Kill)                  -> Terminate(killed)
/// else                                                    -> Terminate(reason)
/// ```
///
/// In particular `kill` is untrappable **only** in the [`ExitFamily::Explicit`] family; a
/// [`ExitFamily::Linked`] `kill` is an ordinary trappable reason whose termination reason is
/// **unchanged** (`kill`, never `killed`). A `normal` reason is ignored by a non-trapping target
/// in **both** families. See the module docs for the full table.
pub fn exit_policy(
    reason: Eterm,
    trapping: bool,
    family: ExitFamily,
    atoms: &SignalAtoms,
) -> ExitPolicy {
    let class = atoms.classify(reason);
    // `erl_proc_sig_queue.c:4740`: `(op != ERTS_SIG_Q_OP_EXIT || reason != am_kill)` — the
    // untrappable sentinel is confined to the explicit `erlang:exit/2` family.
    let untrappable = family == ExitFamily::Explicit && class == ReasonClass::Kill;
    if !untrappable && trapping {
        // Everything but an explicit `kill` is trapped as `{'EXIT', From, Reason}`.
        ExitPolicy::Trap
    } else if class == ReasonClass::Normal {
        // A `normal` exit is ignored by a non-trapping target, in both families
        // (`erl_proc_sig_queue.c:4758`).
        ExitPolicy::Ignore
    } else if untrappable {
        // The `reason = am_killed` rewrite, guarded by `op == ERTS_SIG_Q_OP_EXIT`
        // (`erl_proc_sig_queue.c:4768`): only an explicit `kill` becomes `killed`.
        ExitPolicy::Terminate(atoms.killed_term())
    } else {
        // Every other reason terminates the target unchanged — including a linked-family `kill`.
        ExitPolicy::Terminate(reason)
    }
}

/// What a **receive-scan** signal servicing must do with the head of a receiver's signal queue.
///
/// This is the *receive-time* reading of the propagation decision — the C's behaviour in
/// `erts_proc_sig_handle_incoming` (`erts/emulator/beam/erl_proc_sig_queue.c:6229`), which the
/// receive helper `erts_proc_sig_receive_helper` (`:7729`) calls when `i_loop_rec`
/// (`erts/emulator/beam/emu/msg_instrs.tab:60-140`) finds its message queue exhausted. That
/// handler walks the process's own incoming signals **in FIFO order** and:
///
/// * converts a signal it can deliver into a message — a trapped exit becomes `{'EXIT', …}`
///   (`ERTS_SIG_Q_OP_EXIT`/`_EXIT_LINKED` via `handle_exit_signal`, `:6389-6398`), a monitor-down
///   becomes `{'DOWN', …}` (`ERTS_SIG_Q_OP_MONITOR_DOWN` → `convert_to_down_message`, `:6400`);
/// * removes a signal it would drop — a `normal` exit reason to a non-trapping process
///   (`handle_exit_signal`'s ignore arm, `erl_proc_sig_queue.c:4758`), reported as "ignored or
///   converted to exit message";
/// * **stops** on a signal that would terminate the process: `handle_exit_signal` sets
///   `ERTS_PSFLG_EXITING`, the switch leaves with `goto stop`, and the receive helper reports
///   `get_out > 0` (`:7860-7872`) so the instruction jumps to `do_schedule` — the process is
///   **scheduled out to die**, never killed inside its own receive.
///
/// The distinction is the whole reason this type exists: a signal applied in the middle of an
/// instruction stream must not terminate the process executing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiveApplicability {
    /// The head signal becomes a mailbox message the receive scan can match immediately — a
    /// [`ExitPolicy::Trap`] exit (a `{'EXIT', From, Reason}` message) or a monitor-DOWN
    /// (a `{'DOWN', …}`/`{Tag, …}` message, which never consults the policy).
    Apply,
    /// The head signal is discarded without producing a message — a [`ExitPolicy::Ignore`] exit
    /// (a `normal` reason to a non-trapping receiver). The C removes it and keeps walking the
    /// queue.
    Discard,
    /// Stop the scan **before** this signal, leaving it — and every signal behind it — queued and
    /// untouched: applying it would [terminate](ExitPolicy::Terminate) the receiver, and a
    /// termination belongs to the schedule-out net (the runtime's park arm re-queues the process
    /// instead of parking it, and the next schedule-in's
    /// [`service_signals`](crate::ptab::ProcessTable::service_signals) applies the signal and
    /// cascades).
    Stop,
}

/// The receive-scan class of `signal` for a receiver whose `trap_exit` flag is `trapping` — the
/// FIFO walk `i_loop_rec` may make from the head of that receiver's queue.
///
/// A monitor-DOWN is [`ReceiveApplicability::Apply`] whatever `trapping` says: the C handles a
/// down notification in its own arm, never consulting the exit policy (see the module docs). An
/// exit signal is classified through [`exit_policy`] — the **single** source of the propagation
/// rule, so the receive scan and the schedule-in path can never disagree:
///
/// * [`ExitPolicy::Trap`] ⇒ [`ReceiveApplicability::Apply`];
/// * [`ExitPolicy::Ignore`] ⇒ [`ReceiveApplicability::Discard`];
/// * [`ExitPolicy::Terminate`] ⇒ [`ReceiveApplicability::Stop`].
pub fn receive_applicability(
    signal: PendingSignal,
    trapping: bool,
    atoms: &SignalAtoms,
) -> ReceiveApplicability {
    match signal {
        PendingSignal::Down { .. } => ReceiveApplicability::Apply,
        PendingSignal::Exit {
            reason, family, ..
        } => match exit_policy(reason, trapping, family, atoms) {
            ExitPolicy::Trap => ReceiveApplicability::Apply,
            ExitPolicy::Ignore => ReceiveApplicability::Discard,
            ExitPolicy::Terminate(_) => ReceiveApplicability::Stop,
        },
    }
}

/// Builds `{'EXIT', From, Reason}` on `heap`, with an arbitrary `from` term.
///
/// The general form of [`build_exit_message`]: link/exit propagation towards a **remote**
/// pid needs the `from` slot to hold an external pid term rather than a local
/// [`ProcessId`].
///
/// # Errors
///
/// Propagates `BeamError::Alloc` when `heap` cannot fit the tuple.
pub fn build_exit_message_term(
    heap: &mut Heap,
    atoms: &SignalAtoms,
    from: Eterm,
    reason: Eterm,
) -> Result<Eterm> {
    heap.tuple(&[make_atom(atoms.exit()), from, reason])
}

/// Builds `{'EXIT', From, Reason}` on `heap`.
///
/// # Errors
///
/// Propagates `BeamError::Alloc` when `heap` cannot fit the tuple.
pub fn build_exit_message(
    heap: &mut Heap,
    atoms: &SignalAtoms,
    from: ProcessId,
    reason: Eterm,
) -> Result<Eterm> {
    build_exit_message_term(heap, atoms, from.to_eterm(), reason)
}

/// Builds `{Tag, Reference, Kind, Target, Reason}` on `heap`, with an arbitrary monitor
/// **kind** (`process`/`port`), an arbitrary `target` term and an optional **user tag**.
///
/// The general form of [`build_down_message`]: a `{'DOWN', …}` about a **remote**
/// process/port carries an external pid/port term in the target slot.
///
/// `user_tag` is the `erlang:monitor/3` `{tag, Tag}` option. `None` puts the default
/// `'DOWN'` atom in the head position — the byte-identical
/// `{'DOWN', Reference, Kind, Target, Reason}` shape — while `Some(tag)` puts `tag` there
/// instead, the oracle's `{Tag, Reference, Kind, Target, Reason}`.
///
/// # Errors
///
/// Propagates `BeamError::Alloc` when `heap` cannot fit the tuple.
pub fn build_down_message_term(
    heap: &mut Heap,
    atoms: &SignalAtoms,
    user_tag: Option<Eterm>,
    reference: Eterm,
    kind: Atom,
    target: Eterm,
    reason: Eterm,
) -> Result<Eterm> {
    // The head is the user tag when one was supplied, the default `'DOWN'` atom otherwise.
    let head = match user_tag {
        Some(tag) => tag,
        None => make_atom(atoms.down()),
    };
    heap.tuple(&[head, reference, make_atom(kind), target, reason])
}

/// Builds `{'DOWN', Reference, process, Pid, Reason}` on `heap` — or
/// `{Tag, Reference, process, Pid, Reason}` when `user_tag` is supplied.
///
/// # Errors
///
/// Propagates `BeamError::Alloc` when `heap` cannot fit the tuple.
pub fn build_down_message(
    heap: &mut Heap,
    atoms: &SignalAtoms,
    user_tag: Option<Eterm>,
    reference: Eterm,
    pid: ProcessId,
    reason: Eterm,
) -> Result<Eterm> {
    build_down_message_term(
        heap,
        atoms,
        user_tag,
        reference,
        atoms.process(),
        pid.to_eterm(),
        reason,
    )
}

/// Builds `{'DOWN', Reference, process, RemotePid, Reason}` on `heap`, materialising the
/// remote target's external-pid term on that same heap — or the `{Tag, …}` form when
/// `user_tag` is supplied.
///
/// This is what a dropped distribution connection queues on each of the dead node's
/// monitors.
///
/// # Errors
///
/// Propagates `BeamError::Alloc` when `heap` cannot fit the external pid thing and the tuple.
pub fn build_down_message_remote(
    heap: &mut Heap,
    atoms: &SignalAtoms,
    user_tag: Option<Eterm>,
    reference: Eterm,
    remote: RemotePid,
    reason: Eterm,
) -> Result<Eterm> {
    let target = remote.to_eterm(heap)?;
    build_down_message_term(heap, atoms, user_tag, reference, atoms.process(), target, reason)
}

/// Builds `{'DOWN', Reference, port, Port, Reason}` on `heap` — or
/// `{Tag, Reference, port, Port, Reason}` when `user_tag` is supplied.
///
/// The port analogue of [`build_down_message`]: the monitor **kind** slot carries the `port`
/// atom rather than `process`, and the target is the port immediate (see
/// `erts/emulator/beam/erl_monitor_link.c`).
///
/// # Errors
///
/// Propagates `BeamError::Alloc` when `heap` cannot fit the tuple.
pub fn build_down_message_port(
    heap: &mut Heap,
    atoms: &SignalAtoms,
    user_tag: Option<Eterm>,
    reference: Eterm,
    port: PortId,
    reason: Eterm,
) -> Result<Eterm> {
    build_down_message_term(
        heap,
        atoms,
        user_tag,
        reference,
        atoms.port(),
        port.to_eterm(),
        reason,
    )
}

#[cfg(test)]
mod tests {
    //! Unit tests for the propagation table (`normal`/`kill`/other × trapping/not) and
    //! for the arity and head atom of the two message builders.

    use beam_term::prelude::{make_atom, make_small, tuple_element, tuple_size};

    use super::*;

    /// A shared atom table plus the signal atoms interned from it, so tests can intern
    /// extra names (`boom`, …) into the *same* index space.
    struct Fixture {
        table: AtomTable,
        atoms: SignalAtoms,
    }

    fn fixture() -> Fixture {
        let mut table = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut table);
        Fixture { table, atoms }
    }

    #[test]
    fn from_table_interns_the_well_known_names() {
        let f = fixture();
        assert_eq!(f.table.name(f.atoms.exit()), Some("EXIT"));
        assert_eq!(f.table.name(f.atoms.down()), Some("DOWN"));
        assert_eq!(f.table.name(f.atoms.process()), Some("process"));
        assert_eq!(f.table.name(f.atoms.port()), Some("port"));
        assert_eq!(f.table.name(f.atoms.normal()), Some("normal"));
        assert_eq!(f.table.name(f.atoms.kill()), Some("kill"));
        assert_eq!(f.table.name(f.atoms.killed()), Some("killed"));
        assert_eq!(f.table.name(f.atoms.noproc()), Some("noproc"));
        assert_eq!(f.table.name(f.atoms.system()), Some("system"));
    }

    #[test]
    fn noproc_term_is_the_noproc_atom() {
        let f = fixture();
        assert_eq!(f.atoms.noproc_term(), make_atom(f.atoms.noproc()));
        assert_ne!(f.atoms.noproc_term(), f.atoms.normal_term());
    }

    #[test]
    fn system_term_is_the_immediate_system_atom() {
        let f = fixture();
        assert_eq!(f.atoms.system_term(), make_atom(f.atoms.system()));
        assert_ne!(f.atoms.system_term(), f.atoms.noproc_term());
        // The `from` slot must be an immediate so a collection can never relocate it.
        assert!(beam_term::prelude::is_immediate(f.atoms.system_term()));
    }
    #[test]
    fn classify_distinguishes_normal_kill_and_other() {
        let mut f = fixture();
        let boom = make_atom(f.table.intern("boom"));

        assert_eq!(f.atoms.classify(f.atoms.normal_term()), ReasonClass::Normal);
        assert_eq!(f.atoms.classify(f.atoms.kill_term()), ReasonClass::Kill);
        assert_eq!(f.atoms.classify(boom), ReasonClass::Other);
        assert_eq!(f.atoms.classify(make_small(1)), ReasonClass::Other);
    }

    #[test]
    fn exit_policy_table() {
        // The full C table (`erl_proc_sig_queue.c:4740-4770`): {normal, kill, other} ×
        // {Explicit, Linked} × trapping{false, true} = 12 cases.
        use ExitFamily::{Explicit, Linked};

        let mut f = fixture();
        let atoms = &f.atoms;
        let boom = make_atom(f.table.intern("boom"));
        let normal = atoms.normal_term();
        let kill = atoms.kill_term();
        let killed = atoms.killed_term();

        // --- The untrappable sentinel: `kill` in the EXPLICIT `erlang:exit/2` family. ---
        // It terminates with the rewritten `killed`, trapping or not.
        assert_eq!(
            exit_policy(kill, false, Explicit, atoms),
            ExitPolicy::Terminate(killed)
        );
        assert_eq!(
            exit_policy(kill, true, Explicit, atoms),
            ExitPolicy::Terminate(killed)
        );

        // --- A LINK exit of reason `kill` IS trappable and is NOT rewritten. ---
        assert_eq!(
            exit_policy(kill, true, Linked, atoms),
            ExitPolicy::Trap,
            "a trapping receiver traps a `kill` link exit"
        );
        assert_eq!(
            exit_policy(kill, false, Linked, atoms),
            ExitPolicy::Terminate(kill),
            "a non-trapping receiver terminates with `kill`, never `killed`"
        );

        // --- `normal` is ignored by a non-trapping target in BOTH families ... ---
        assert_eq!(exit_policy(normal, false, Explicit, atoms), ExitPolicy::Ignore);
        assert_eq!(exit_policy(normal, false, Linked, atoms), ExitPolicy::Ignore);
        // ... and trapped by a trapping one in both.
        assert_eq!(exit_policy(normal, true, Explicit, atoms), ExitPolicy::Trap);
        assert_eq!(exit_policy(normal, true, Linked, atoms), ExitPolicy::Trap);

        // --- Any other reason traps when trapping, terminates unchanged otherwise, in both. ---
        assert_eq!(exit_policy(boom, true, Explicit, atoms), ExitPolicy::Trap);
        assert_eq!(exit_policy(boom, true, Linked, atoms), ExitPolicy::Trap);
        assert_eq!(
            exit_policy(boom, false, Explicit, atoms),
            ExitPolicy::Terminate(boom)
        );
        assert_eq!(
            exit_policy(boom, false, Linked, atoms),
            ExitPolicy::Terminate(boom)
        );

        // A non-atom reason is "other" too, in both families.
        let weird = make_small(3);
        assert_eq!(
            exit_policy(weird, false, Explicit, atoms),
            ExitPolicy::Terminate(weird)
        );
        assert_eq!(
            exit_policy(weird, false, Linked, atoms),
            ExitPolicy::Terminate(weird)
        );
    }

    /// The receive-scan reading of the propagation table: every [`ExitPolicy`] arm maps onto its
    /// [`ReceiveApplicability`], over the full `{normal, kill, other} × {Explicit, Linked} ×
    /// trapping{false, true}` matrix.
    ///
    /// The two classifications are cross-checked here rather than asserted independently: each
    /// case's expected applicability is *derived* from [`exit_policy`]'s verdict, so the test
    /// pins the mapping (and the "no second copy of the rule" contract) instead of restating it.
    #[test]
    fn receive_applicability_is_the_receive_time_reading_of_the_policy() {
        use ExitFamily::{Explicit, Linked};

        let mut f = fixture();
        let atoms = &f.atoms;
        let boom = make_atom(f.table.intern("boom"));
        let normal = atoms.normal_term();
        let kill = atoms.kill_term();

        let expected = |policy: ExitPolicy| match policy {
            ExitPolicy::Trap => ReceiveApplicability::Apply,
            ExitPolicy::Ignore => ReceiveApplicability::Discard,
            ExitPolicy::Terminate(_) => ReceiveApplicability::Stop,
        };

        for reason in [normal, kill, boom, make_small(9)] {
            for family in [Explicit, Linked] {
                for trapping in [false, true] {
                    let policy = exit_policy(reason, trapping, family, atoms);
                    let signal = PendingSignal::exit(ProcessId::new(1, 0), reason, family);
                    assert_eq!(
                        receive_applicability(signal, trapping, atoms),
                        expected(policy),
                        "reason {:?} / {:?} / trapping {trapping}: the receive scan must read \
                         the schedule-in policy, not a second copy of it",
                        atoms.classify(reason),
                        family,
                    );
                }
            }
        }

        // The three arms are all actually reached (an anti-vacuity guard).
        let trapped = PendingSignal::exit(ProcessId::new(1, 0), boom, Linked);
        assert_eq!(
            receive_applicability(trapped, true, atoms),
            ReceiveApplicability::Apply
        );
        let ignored = PendingSignal::exit(ProcessId::new(1, 0), normal, Linked);
        assert_eq!(
            receive_applicability(ignored, false, atoms),
            ReceiveApplicability::Discard
        );
        let fatal = PendingSignal::exit(ProcessId::new(1, 0), boom, Linked);
        assert_eq!(
            receive_applicability(fatal, false, atoms),
            ReceiveApplicability::Stop
        );
        // An explicit `kill` is untrappable, so even a **trapping** receiver stops on it.
        let untrappable = PendingSignal::exit(ProcessId::new(1, 0), kill, Explicit);
        assert_eq!(
            receive_applicability(untrappable, true, atoms),
            ReceiveApplicability::Stop
        );
    }

    /// A monitor-DOWN is receive-applicable whatever the receiver's `trap_exit` flag says (and
    /// whatever user tag it carries): the C handles a down notification in its own arm, never
    /// consulting the exit policy.
    #[test]
    fn a_monitor_down_is_always_receive_applicable() {
        let mut f = fixture();
        let tag = make_atom(f.table.intern("mytag"));
        let atoms = &f.atoms;
        let reference = crate::monitor::RefAllocator::new().next();
        let target = ProcessId::new(4, 2);

        for trap in [false, true] {
            for user_tag in [None, Some(tag)] {
                let signal = PendingSignal::down(
                    reference,
                    target,
                    crate::monitor::MonitorKind::Process,
                    user_tag,
                    atoms.normal_term(),
                );
                assert_eq!(
                    receive_applicability(signal, trap, atoms),
                    ReceiveApplicability::Apply,
                    "a DOWN is delivered unconditionally (trapping {trap}, tag {user_tag:?})"
                );
            }
        }
    }

    #[test]
    fn build_exit_message_has_arity_three_and_the_exit_head() {
        let f = fixture();
        let atoms = &f.atoms;
        let mut heap = Heap::new(8).expect("test heap");
        let from = ProcessId::new(3, 1);
        let reason = atoms.kill_term();
        let message = build_exit_message(&mut heap, atoms, from, reason).expect("fits");

        // SAFETY: `message` is a live tuple written by `Heap::tuple`.
        unsafe {
            assert_eq!(tuple_size(message), 3);
            assert_eq!(tuple_element(message, 0), make_atom(atoms.exit()));
            assert_eq!(tuple_element(message, 1), from.to_eterm());
            assert_eq!(tuple_element(message, 2), reason);
        }
    }

    #[test]
    fn build_down_message_has_arity_five_and_the_expected_layout() {
        let f = fixture();
        let atoms = &f.atoms;
        let mut heap = Heap::new(16).expect("test heap");
        let mut ref_heap = Heap::new(8).expect("test heap");
        let reference = crate::monitor::RefAllocator::new()
            .next()
            .to_eterm(&mut ref_heap)
            .expect("the ref fits");
        let pid = ProcessId::new(7, 2);
        let reason = atoms.normal_term();
        let message =
            build_down_message(&mut heap, atoms, None, reference, pid, reason).expect("fits");

        // SAFETY: `message` is a live tuple written by `Heap::tuple`.
        unsafe {
            assert_eq!(tuple_size(message), 5);
            assert_eq!(tuple_element(message, 0), make_atom(atoms.down()));
            assert_eq!(tuple_element(message, 1), reference);
            assert_eq!(tuple_element(message, 2), make_atom(atoms.process()));
            assert_eq!(tuple_element(message, 3), pid.to_eterm());
            assert_eq!(tuple_element(message, 4), reason);
        }
    }

    #[test]
    fn build_down_message_with_a_user_tag_replaces_the_head_atom() {
        let mut f = fixture();
        let tag = make_atom(f.table.intern("mytag"));
        let atoms = &f.atoms;
        let mut heap = Heap::new(16).expect("test heap");
        let mut ref_heap = Heap::new(8).expect("test heap");
        let reference = crate::monitor::RefAllocator::new()
            .next()
            .to_eterm(&mut ref_heap)
            .expect("the ref fits");
        let pid = ProcessId::new(7, 2);
        let reason = atoms.normal_term();
        let message = build_down_message(&mut heap, atoms, Some(tag), reference, pid, reason)
            .expect("fits");

        // SAFETY: `message` is a live tuple written by `Heap::tuple`.
        unsafe {
            assert_eq!(tuple_size(message), 5);
            // The user tag sits where `'DOWN'` would be; `'DOWN'` itself is gone.
            assert_eq!(tuple_element(message, 0), tag);
            assert_ne!(tuple_element(message, 0), make_atom(atoms.down()));
            assert_eq!(tuple_element(message, 1), reference);
            assert_eq!(tuple_element(message, 2), make_atom(atoms.process()));
            assert_eq!(tuple_element(message, 3), pid.to_eterm());
            assert_eq!(tuple_element(message, 4), reason);
        }
    }

    #[test]
    fn build_down_message_port_has_arity_five_and_the_port_tag() {
        let f = fixture();
        let atoms = &f.atoms;
        let mut heap = Heap::new(16).expect("test heap");
        let mut ref_heap = Heap::new(8).expect("test heap");
        let reference = crate::monitor::RefAllocator::new()
            .next()
            .to_eterm(&mut ref_heap)
            .expect("the ref fits");
        let port = PortId::new(42);
        let reason = atoms.normal_term();
        let message = build_down_message_port(&mut heap, atoms, None, reference, port, reason)
            .expect("fits");

        // SAFETY: `message` is a live tuple written by `Heap::tuple`.
        unsafe {
            assert_eq!(tuple_size(message), 5);
            assert_eq!(tuple_element(message, 0), make_atom(atoms.down()));
            assert_eq!(tuple_element(message, 1), reference);
            assert_eq!(tuple_element(message, 2), make_atom(atoms.port()));
            assert_eq!(tuple_element(message, 3), port.to_eterm());
            assert_eq!(tuple_element(message, 4), reason);
        }
    }

    #[test]
    fn build_down_message_port_with_a_user_tag_replaces_the_head_atom() {
        let mut f = fixture();
        let tag = make_atom(f.table.intern("mytag"));
        let atoms = &f.atoms;
        let mut heap = Heap::new(16).expect("test heap");
        let mut ref_heap = Heap::new(8).expect("test heap");
        let reference = crate::monitor::RefAllocator::new()
            .next()
            .to_eterm(&mut ref_heap)
            .expect("the ref fits");
        let port = PortId::new(42);
        let reason = atoms.normal_term();
        let message = build_down_message_port(&mut heap, atoms, Some(tag), reference, port, reason)
            .expect("fits");

        // SAFETY: `message` is a live tuple written by `Heap::tuple`.
        unsafe {
            assert_eq!(tuple_size(message), 5);
            assert_eq!(tuple_element(message, 0), tag);
            assert_eq!(tuple_element(message, 1), reference);
            assert_eq!(tuple_element(message, 2), make_atom(atoms.port()));
            assert_eq!(tuple_element(message, 3), port.to_eterm());
            assert_eq!(tuple_element(message, 4), reason);
        }
    }

    #[test]
    fn builders_report_exhaustion() {
        let f = fixture();
        let atoms = &f.atoms;
        let mut tiny = Heap::new(1).expect("a one-word heap");
        assert!(build_exit_message(&mut tiny, atoms, ProcessId::new(1, 0), atoms.normal_term())
            .is_err());
    }
}