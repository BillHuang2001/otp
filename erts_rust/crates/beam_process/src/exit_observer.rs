//! The process-death observer seam: [`ProcessExitObserver`] and its payload [`ProcessExit`].
//!
//! The runtime has to release per-process resources the moment a process dies — the **ETS
//! tables the pid owns** are the motivating case. This crate's *sole finalizer*,
//! [`ProcessTable::terminate_into`](crate::ptab::ProcessTable), reclaims the pid and propagates
//! the death to its links and monitors, but it cannot know what some other subsystem allocated
//! on the pid's behalf. [`ProcessExitObserver`] closes that gap: it is called **exactly once per
//! death**, from inside the finalizer, with an immutable [`ProcessExit`] describing the dead
//! process.
//!
//! # Shape — why a trait object
//!
//! The observer is installed on the [`ProcessTable`](crate::ptab::ProcessTable) as a
//! `Box<dyn ProcessExitObserver>` (see
//! [`set_process_exit_observer`](crate::ptab::ProcessTable::set_process_exit_observer)) and
//! mirrors the crate's other host seam, the distribution router
//! ([`DistSignalRouter`](crate::remote::DistSignalRouter)): the table stays free of any
//! dependency on the observing subsystem, and the runtime (`beam_rt`) installs its own
//! implementation. With **no** observer installed (`None`) the table's behaviour is unchanged.
//!
//! # Failure policy
//!
//! A registered observer is invoked exactly once per death, with the dead pid and its reason. A
//! **panicking** observer cannot corrupt the table: the callback is isolated with
//! [`std::panic::catch_unwind`], the panic is reported through [`beam_common`]'s logging
//! (`beam_error!`), and termination still completes (the pid is reclaimed, the death is
//! propagated, links/monitors stay consistent). The observer is wrapped in
//! [`std::panic::AssertUnwindSafe`] because it receives only an **immutable** [`ProcessExit`] and
//! holds no reference to the table, so a panic inside it cannot leave the table half-updated —
//! the only state it can corrupt is its own.
//!
//! # Retaining the payload
//!
//! [`ProcessExit::reason`] **may live in the dying process's heap**, which the finalizer releases
//! as soon as the observer returns: an observer that keeps the term beyond the call must **copy
//! it** into storage it owns. [`ProcessExit::name`] (an [`Atom`], an immediate) and
//! [`ProcessExit::port_monitors`] (a [`PortWatch`] is a pair of plain values) are heap-independent
//! and safe to retain as-is.

use beam_term::prelude::{Atom, Eterm};

use crate::monitor::PortWatch;
use crate::pid::ProcessId;

/// What a dying process exposed to the runtime — the payload of
/// [`ProcessExitObserver::process_exited`].
///
/// Field-by-field, and why each is here:
///
/// * `pid` — the identity of the dead process. It is the key an observer uses to release the
///   per-pid resources it owns (the ETS tables the pid created are the motivating case); a
///   [`ProcessId`] is an immediate, so it is safe to retain.
/// * `reason` — the process's exit reason term, exactly as the finalizer received it. **It may
///   live in the dying process's heap**, which is released when the observer returns, so an
///   observer that retains it beyond the call must copy it first.
/// * `name` — the process's registered name (`erlang:register/2`), captured **before** the
///   finalizer removes the registry entry. `Some(atom)` iff the process was registered at death.
///   An [`Atom`] is an immediate, so it is safe to retain, and without it the registration would
///   be unobservable — the finalizer drops it before any observer could look it up.
/// * `port_monitors` — the process's drained **outgoing port monitors**
///   (`erlang:monitor(port, Port)`), so an observer can see which ports the process was watching.
///   The finalizer also uses these to remove the matching *incoming* entries from the port table
///   (see [`ProcessTable::terminate_with_ports`](crate::ptab::ProcessTable::terminate_with_ports)).
///   A [`PortWatch`] is a pair of plain values (no heap term), so the slice is heap-independent
///   and safe to retain.
pub struct ProcessExit<'a> {
    /// The dead process's identity.
    pid: ProcessId,
    /// The exit reason term (may live in the dying heap — copy before retaining).
    reason: Eterm,
    /// The process's registered name, captured before the registry entry is removed.
    name: Option<Atom>,
    /// The process's drained outgoing port monitors.
    port_monitors: &'a [PortWatch],
}

impl<'a> ProcessExit<'a> {
    /// Builds a payload for the dead `pid` with its `reason`, registered `name` and drained
    /// `port_monitors`.
    ///
    /// Crate-internal: the payload is created only by the finalizer
    /// ([`ProcessTable::terminate_into`](crate::ptab::ProcessTable)).
    pub(crate) fn new(
        pid: ProcessId,
        reason: Eterm,
        name: Option<Atom>,
        port_monitors: &'a [PortWatch],
    ) -> ProcessExit<'a> {
        ProcessExit {
            pid,
            reason,
            name,
            port_monitors,
        }
    }

    /// The dead process's identity.
    pub fn pid(&self) -> ProcessId {
        self.pid
    }

    /// The process's exit reason term.
    ///
    /// The term may live in the dying process's heap, which is released when
    /// [`ProcessExitObserver::process_exited`] returns — copy it before retaining it.
    pub fn reason(&self) -> Eterm {
        self.reason
    }

    /// The process's registered name (`erlang:register/2`), `None` when it had none.
    pub fn name(&self) -> Option<Atom> {
        self.name
    }

    /// The process's drained outgoing port monitors (`erlang:monitor(port, Port)` watches).
    pub fn port_monitors(&self) -> &'a [PortWatch] {
        self.port_monitors
    }
}

/// The process-death observer seam: a host hook invoked **exactly once** for every process death,
/// from inside the crate's sole finalizer, **before** the dying heap is released.
///
/// Installed with
/// [`ProcessTable::set_process_exit_observer`](crate::ptab::ProcessTable::set_process_exit_observer)
/// and removed with
/// [`ProcessTable::take_process_exit_observer`](crate::ptab::ProcessTable::take_process_exit_observer);
/// with no observer installed the table behaves exactly as before the seam existed.
///
/// Implementations must not retain any borrowed part of the [`ProcessExit`] past the call without
/// copying it (see that type's docs), and should be cheap: they run on the finalizing scheduler
/// thread. A panicking implementation is contained (see the module docs) — termination still
/// completes — but the panic is logged, so an implementation should prefer returning over
/// panicking for expected conditions.
pub trait ProcessExitObserver {
    /// Reports that a process has died.
    ///
    /// Called once per death with the dead process's `pid`, exit `reason`, registered `name` and
    /// drained outgoing `port_monitors` (see [`ProcessExit`] for the exact payload). The callback
    /// runs after the dead process's links and monitors have been signalled and before its heap
    /// is released.
    fn process_exited(&mut self, exit: &ProcessExit<'_>);
}
