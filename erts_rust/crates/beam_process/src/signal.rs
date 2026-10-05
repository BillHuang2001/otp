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
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_monitor_link.c`,
//! `erl_proc_sig_queue.c` and the `am_exit`/`am_down`/`am_kill`/`am_killed` atoms of
//! `atom.names`.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{make_atom, make_header, Atom, AtomTable, Eterm, NIL, REF_SUBTAG};

use crate::pid::ProcessId;

/// The well-known atom indices used to build signal messages.
///
/// Interned once from the module's [`AtomTable`] so the signal machinery never re-interns
/// a name on a hot path; the crate's other units read them through the accessors.
//
// TODO(impl): implement `SignalAtoms` (all bodies are stubs).
pub struct SignalAtoms {
    exit: Atom,
    down: Atom,
    process: Atom,
    normal: Atom,
    kill: Atom,
    killed: Atom,
}

impl SignalAtoms {
    /// Interns `'EXIT'`, `'DOWN'`, `process`, `normal`, `kill` and `killed` in `t`.
    pub fn from_table(t: &mut AtomTable) -> SignalAtoms {
        unimplemented!("stub — to be implemented")
    }

    /// The `'EXIT'` atom.
    pub fn exit(&self) -> Atom {
        unimplemented!("stub — to be implemented")
    }

    /// The `'DOWN'` atom.
    pub fn down(&self) -> Atom {
        unimplemented!("stub — to be implemented")
    }

    /// The `process` atom (the tag of a `{'DOWN', …}` message).
    pub fn process(&self) -> Atom {
        unimplemented!("stub — to be implemented")
    }

    /// The `normal` atom.
    pub fn normal(&self) -> Atom {
        unimplemented!("stub — to be implemented")
    }

    /// The `kill` atom.
    pub fn kill(&self) -> Atom {
        unimplemented!("stub — to be implemented")
    }

    /// The `killed` atom.
    pub fn killed(&self) -> Atom {
        unimplemented!("stub — to be implemented")
    }

    /// The `normal` atom as a term.
    pub fn normal_term(&self) -> Eterm {
        unimplemented!("stub — to be implemented")
    }

    /// The `kill` atom as a term.
    pub fn kill_term(&self) -> Eterm {
        unimplemented!("stub — to be implemented")
    }

    /// The `killed` atom as a term.
    pub fn killed_term(&self) -> Eterm {
        unimplemented!("stub — to be implemented")
    }

    /// Classifies an exit reason against the well-known atoms.
    pub fn classify(&self, reason: Eterm) -> ReasonClass {
        unimplemented!("stub — to be implemented")
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
//
// TODO(impl): implement `ExitSignal` (all bodies are stubs).
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
    unimplemented!("stub — to be implemented")
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
    unimplemented!("stub — to be implemented")
}

/// Builds `{'DOWN', Reference, process, Pid, Reason}` on `heap`.
///
/// # Errors
///
/// Propagates `BeamError::Alloc` when `heap` cannot fit the tuple.
pub fn build_down_message(
    heap: &mut Heap,
    atoms: &SignalAtoms,
    reference: Eterm,
    pid: ProcessId,
    reason: Eterm,
) -> Result<Eterm> {
    unimplemented!("stub — to be implemented")
}
