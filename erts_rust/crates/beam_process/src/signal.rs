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
//! The exit semantics of Erlang are a pure function of the reason and the target's
//! `trap_exit` flag, which is exactly what [`exit_policy`] computes:
//!
//! * reason `kill` ⇒ [`ExitPolicy::Terminate`] with reason `killed` — **untrappable**;
//! * otherwise trapping ⇒ [`ExitPolicy::Trap`] — the reason is delivered as an
//!   `{'EXIT', From, Reason}` message;
//! * otherwise reason `normal` ⇒ [`ExitPolicy::Ignore`];
//! * otherwise ⇒ [`ExitPolicy::Terminate`] with the reason unchanged.
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
//! `erl_proc_sig_queue.c` and the `am_exit`/`am_down`/`am_kill`/`am_killed` atoms of
//! `atom.names`.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{atom_val, is_atom, make_atom, Atom, AtomTable, Eterm};

use crate::pid::{PortId, ProcessId};
use crate::remote::RemotePid;

/// The well-known atom indices used to build signal messages.
///
/// Interned once from the module's [`AtomTable`] so the signal machinery never re-interns
/// a name on a hot path; the crate's other units read them through the accessors.
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
    /// The `normal` reason: silently ignored by a non-trapping target.
    Normal,
    /// The `kill` reason: terminates any target, `trap_exit` or not.
    Kill,
    /// Every other reason.
    Other,
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
/// `trapping` (see the module docs for the table).
pub fn exit_policy(reason: Eterm, trapping: bool, atoms: &SignalAtoms) -> ExitPolicy {
    match atoms.classify(reason) {
        // `kill` is untrappable: it terminates with `killed` whatever the flag says.
        ReasonClass::Kill => ExitPolicy::Terminate(atoms.killed_term()),
        // Every other reason is trapped as a message when the target traps exits ...
        _ if trapping => ExitPolicy::Trap,
        // ... a `normal` reason is otherwise ignored ...
        ReasonClass::Normal => ExitPolicy::Ignore,
        // ... and any other reason terminates the target unchanged.
        ReasonClass::Other => ExitPolicy::Terminate(reason),
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
        let mut f = fixture();
        let atoms = &f.atoms;
        let boom = make_atom(f.table.intern("boom"));

        // `kill` is untrappable: `Terminate(killed)` in both trapping states.
        assert_eq!(
            exit_policy(atoms.kill_term(), false, atoms),
            ExitPolicy::Terminate(atoms.killed_term())
        );
        assert_eq!(
            exit_policy(atoms.kill_term(), true, atoms),
            ExitPolicy::Terminate(atoms.killed_term())
        );

        // `normal` without trapping is ignored; with trapping it is delivered.
        assert_eq!(
            exit_policy(atoms.normal_term(), false, atoms),
            ExitPolicy::Ignore
        );
        assert_eq!(
            exit_policy(atoms.normal_term(), true, atoms),
            ExitPolicy::Trap
        );

        // Any other reason terminates unchanged, or traps when trapping.
        assert_eq!(
            exit_policy(boom, false, atoms),
            ExitPolicy::Terminate(boom)
        );
        assert_eq!(exit_policy(boom, true, atoms), ExitPolicy::Trap);

        // A non-atom reason is "other" too.
        let weird = make_small(3);
        assert_eq!(
            exit_policy(weird, false, atoms),
            ExitPolicy::Terminate(weird)
        );
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