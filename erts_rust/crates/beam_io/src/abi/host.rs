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

//! The driver host seam: [`PortHost`] (the runtime hook) and [`AbiHost`] (the
//! [`DriverHost`](beam_abi::driver::host::DriverHost) implementation).
//!
//! [`beam_abi::driver::host::DriverHost`] is the runtime side of the driver C ABI: the
//! `driver_select`/`driver_set_timer`/`driver_output`/`driver_async` functions a driver calls
//! resolve, through [`beam_abi::driver::drv`], to a method on a `DriverHost`. `beam_abi` cannot
//! implement it — registering with a poller, queueing bytes for a port owner and arming a port timer
//! are all *this crate's* business — so [`AbiHost`] is the implementation, living here next to the
//! [`Driver`](crate::driver::Driver) adapter that owns it.
//!
//! # Reaching the live port
//!
//! A `DriverHost` method is entered from **inside** a driver callback (`start`, `output`,
//! `ready_input`, …), at which point the port exists only as a
//! [`DriverContext`](crate::driver::DriverContext) borrowed by the
//! [`DriverPort`](super::port::DriverPort) method that made the call. [`AbiHost`] therefore holds a
//! `Cell<*mut c_void>` slot: [`AbiHost::install`] stores the callback's context pointer and
//! [`AbiHost::clear`] nulls it again, and every port-touching host method goes through
//! [`AbiHost::with_ctx`]. With no context installed they fail **gracefully** (`-1` / `None` /
//! nothing) rather than dereferencing a null pointer — a driver that calls `driver_output` outside
//! a callback (e.g. from its own worker thread) gets the C's failure code.
//!
//! # What lives in the host
//!
//! The port's *persistent* C-ABI state is [`DriverState`]: the `driver_select` registration map
//! (fd → token + interest + the two `ERL_DRV_USE` release bits, a `BTreeMap` so
//! `ready_input`/`ready_output` pick a stable fd), the `driver_set_timer` deadline, and the
//! `driver_async` job set. It survives individual callbacks, exactly as the C's per-port
//! `ErlDrvEvent` list and port timer do. A released `ERL_DRV_USE` registration leaves its event in
//! the state's [`DriverState::queued_stop_select`] queue, which the
//! [`DriverPort`](super::port::DriverPort) drains into the driver's `stop_select` callback after
//! the callback that made the `driver_select` has returned.
//!
//! A host may additionally carry its port's **registered driver identity** (the driver name and
//! whether the port was built from a `dlopen`ed object). It is what `driver_lock_driver` needs:
//! the C consults the port's *own* driver (`prt->drv_ptr->handle`), not a name lookup. A host
//! built with [`AbiHost::new`] has no identity and answers the C's statically-linked case.
//!
//! # The process-global driver registry
//!
//! Four of the frozen `ErlDrvApi` slots act on structure **outside** one port — the emulator's
//! process-global `driver_list` and the creation of a brand-new port. [`AbiHost`] is the host
//! those slots delegate to: it forwards `add_driver_entry`/`remove_driver_entry`/
//! `driver_lock_driver` to the process-global [`DriverTable`](crate::abi::DriverTable), and
//! answers `driver_create_port` with the C's invalid-port sentinel (a driver callback here already
//! holds a `&mut` into the port registry, so re-entering it would alias — see the method docs).
//!
//! # `unsafe`
//!
//! This is an explicitly-marked `unsafe` module (workspace `../CONTEXT.md` §3): it re-enters a
//! borrowed [`DriverContext`](crate::driver::DriverContext) from a raw pointer, records a driver's
//! own static entry in the global table and calls a driver-supplied `async_free`. Every block
//! carries a `// SAFETY:` rationale.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use libc::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

use beam_abi::driver::host::DriverHost;
use beam_abi::driver::types::{
    ErlDrvData, ErlDrvEntry, ErlDrvEvent, ErlDrvPort, ErlDrvTermData, ERTS_INVALID_ERL_DRV_PORT,
    ERL_DRV_READ, ERL_DRV_USE, ERL_DRV_USE_NO_CALLBACK, ERL_DRV_WRITE,
};
use beam_alloc::Heap;
use beam_common::Result;
use beam_term::atom::AtomTable;
use beam_term::prelude::{make_atom, Eterm};

use crate::abi::job::AsyncJobs;
use crate::abi::DriverTable;
use crate::driver::DriverContext;
use crate::poller::{Interest, Token};

/// The word capacity of the term heap handed to a driver through
/// [`DriverHost::heap`](beam_abi::driver::host::DriverHost::heap).
///
/// A driver builds terms with `erl_drv_output_term`/`driver_mk_term_nil`, which allocate on a host
/// heap ([`beam_alloc::Heap`]) for the duration of the call. Milestone-1 hands every port its own
/// small fixed arena; a driver with a large term-building need is a follow-up (the heap is
/// **replaced**, not grown, if this ever needs to be dynamic — see `beam_alloc`'s contract that
/// handed-out pointers stay valid for the heap's lifetime).
const DRIVER_HEAP_WORDS: usize = 1024;

/// The services a driver port needs from the **runtime**, which `beam_io` cannot supply.
///
/// There is exactly one thing here that is not optional: [`PortHost::intern_atom`]. Atom identity
/// is global state owned by the runtime's atom table (`beam_term::atom::AtomTable` seeded from
/// `atom.names`, and in `beam_rt` shared with the engine and the BIF layer), and a term carrying an
/// atom from *any other* table would compare unequal to the same atom built by running code.
///
/// The remaining methods are optional (they default to "the runtime does not support this"), so a
/// test-only host only has to intern atoms.
pub trait PortHost {
    /// Intern `name` in the **runtime's** atom table and return the atom term.
    ///
    /// Returns `None` when the runtime cannot intern it (no table, the table is full, …); the
    /// caller then reports `erl_drv_output_term`'s failure code rather than inventing a term.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm>;

    /// The pid the port is connected to (`driver_connected`), or `None` when the runtime does not
    /// model it (default).
    fn connected_pid(&mut self) -> Option<Eterm> {
        None
    }

    /// The pid that opened the port (`driver_caller`), or `None` when the runtime does not model it
    /// (default).
    fn caller_pid(&mut self) -> Option<Eterm> {
        None
    }

    /// Deliver a decoded term built by `erl_drv_output_term`/`erl_drv_send_term`.
    ///
    /// `to == None` means the **port owner**; `Some(pid)` means that process. Returns whether the
    /// term was handed to the runtime (the default is `false`: no term sink is wired up).
    fn send_term(&mut self, to: Option<Eterm>, term: Eterm) -> bool {
        let _ = (to, term);
        false
    }
}

/// A [`PortHost`] that interns atoms into a private table.
///
/// # This is a test/standalone host — do NOT use it for a live runtime
///
/// The table is **local to this struct**, so the atoms it produces do **not** match the runtime's:
/// `make_atom(LocalAtomPortHost::intern_atom("ok"))` is some unrelated atom index as far as the
/// engine, the BIF layer and `term_to_binary` are concerned. A term built with such an atom would
/// compare unequal to the atom `ok` produced by running code — a silently wrong result, not an
/// error.
///
/// `beam_rt` therefore **must** inject its own [`PortHost`] over the runtime's shared atom table
/// (the same one `beam_engine`'s `ExecutionContext` and `beam_bifs`' `AtomInterner` use). This type
/// exists so `beam_io`'s own tests (and a standalone driver smoke test) can exercise the bridge
/// without a runtime.
pub struct LocalAtomPortHost {
    /// The private atom table (indices assigned in insertion order).
    atoms: AtomTable,
}

impl LocalAtomPortHost {
    /// An empty host.
    pub fn new() -> LocalAtomPortHost {
        LocalAtomPortHost {
            atoms: AtomTable::new(),
        }
    }

    /// A host seeded with `names`, in order.
    pub fn with_names(names: &[&str]) -> LocalAtomPortHost {
        let mut host = LocalAtomPortHost::new();
        for name in names {
            host.atoms.intern(name);
        }
        host
    }
}

impl Default for LocalAtomPortHost {
    fn default() -> LocalAtomPortHost {
        LocalAtomPortHost::new()
    }
}

impl PortHost for LocalAtomPortHost {
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }
}

/// One `driver_select` registration: the poller token and the interest it was made with.
///
/// The `interest` is re-read when the port needs to name the fd a `ready_input`/`ready_output`
/// callback should receive (see [`DriverState::fd_armed`]), so a `driver_select` that flips
/// direction updates it in place rather than adding an entry.
///
/// The `ERL_DRV_USE` release obligation is the registration's own (see
/// [`SelectedFd::wants_stop_select`]); it lives here, next to the fd, because the fd is what the
/// release callback names.
pub(crate) struct SelectedFd {
    /// The poller token `DriverContext::register` returned.
    pub(crate) token: Token,
    /// The interest the registration currently expresses.
    pub(crate) interest: Interest,
    /// The registration still owes its owner a `stop_select` callback: a select carried
    /// `ERL_DRV_USE` (`ERL_NIF_SELECT_STOP`) without the `_NO_CALLBACK` bit, and no later select
    /// has declined it.
    pub(crate) owes_stop_select: bool,
}

impl SelectedFd {
    /// Whether releasing this registration owes the driver a `stop_select` callback.
    ///
    /// The obligation is **sticky across re-selection**, like the C's `ERTS_EV_FLAG_USED`, which a
    /// select sets (`if (mode & ERL_DRV_USE) state->flags |= ERTS_EV_FLAG_USED;`,
    /// `erl_check_io.c:1154-1155`) and a later select on the same fd does **not** clear. Only
    /// `ERL_DRV_USE_NO_CALLBACK` declines the callback — the C's `no_stop_select_callback` decision
    /// (`(mode & ERL_DRV_USE_NO_CALLBACK) == ERL_DRV_USE`, `erl_check_io.c:1033`, `erl_check_io.c:1179`).
    ///
    /// Fidelity note: the C reads that bit off the **releasing call's** `mode`, so a driver that
    /// never selected with `ERL_DRV_USE` but de-selects with it still gets the callback there. This
    /// floor instead treats the request as a property of the registration (a select with `READ`
    /// after `READ | ERL_DRV_USE` keeps the obligation, which the C's sticky flag achieves too),
    /// and therefore *declines* that nonsensical case. Erring toward the driver's expressed intent
    /// is the safe direction: it never leaks a handle the driver expected to be told about.
    pub(crate) fn wants_stop_select(&self) -> bool {
        self.owes_stop_select
    }
}

/// The port's persistent C-ABI state, mutated by [`AbiHost`].
///
/// It outlives any single driver callback: a `driver_select` made in `start` is still in force when
/// `ready_input` runs much later, and a `driver_set_timer` deadline set in `output` is what
/// [`DriverPort::next_timeout`](super::port::DriverPort) reports to the port-task queue.
pub struct DriverState {
    /// The live `driver_select` registrations, keyed by fd.
    ///
    /// A `BTreeMap` rather than a `HashMap` so iteration order is deterministic: `ready_input`/
    /// `ready_output` must pick the *same* fd for the same state, or a test would be flaky.
    pub(crate) selected: BTreeMap<RawFd, SelectedFd>,
    /// The `driver_set_timer` deadline, or `None` when no port timer is armed.
    pub(crate) deadline: Option<Instant>,
    /// The `driver_async` jobs scheduled but not yet handed to `ready_async`/`async_free`.
    pub(crate) jobs: AsyncJobs,
    /// The `ErlDrvEvent` values whose `ERL_DRV_USE` registration a `driver_select(..., 0)` released
    /// and whose owner is therefore owed a `stop_select` callback.
    ///
    /// The callback cannot run from `AbiHost::select` itself — it is the loaded driver's own
    /// `stop_select`, which the host does not own — so the events are queued here and fired by
    /// [`DriverPort`](super::port::DriverPort) once the driver callback that made the
    /// `driver_select` has returned. That is also the C's ordering: `stop_select` runs after the
    /// fd lock is released (`erl_check_io.c:1210-1216`), never re-entrantly inside the driver's
    /// own `driver_select`.
    pub(crate) queued_stop_select: Vec<ErlDrvEvent>,
}

impl DriverState {
    /// An empty state: nothing selected, no timer, no jobs.
    pub(crate) fn new() -> DriverState {
        DriverState {
            selected: BTreeMap::new(),
            deadline: None,
            jobs: AsyncJobs::new(),
            queued_stop_select: Vec::new(),
        }
    }

    /// The port timer deadline, or `None` when none is armed.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// How many fds the driver currently has selected (`driver_select` on, not yet off).
    pub fn selected_len(&self) -> usize {
        self.selected.len()
    }

    /// How many async jobs have been scheduled but not yet drained by
    /// [`AbiHost`]'s [`DriverPort::poll_async`](super::port::DriverPort::poll_async).
    pub fn in_flight(&self) -> usize {
        self.jobs.in_flight()
    }

    /// The first selected fd whose interest covers `want_read` (readability when `true`,
    /// writability when `false`), in fd order.
    ///
    /// The poller keys a registration by *port number*, so a readiness event names the port but not
    /// which of its fds fired. The C carries that per-`ErlDrvEvent` (each has its own task handle);
    /// here the driver's own `driver_select` bookkeeping is the only record, and the C's
    /// `ready_input`/`ready_output` callbacks are per-event too. Milestone-1 therefore passes the
    /// matching fd (the stable first one) and documents the floor: a port with several selected fds
    /// in the same direction gets them in fd order, one callback per port-task round.
    pub(crate) fn fd_armed(&self, want_read: bool) -> Option<RawFd> {
        self.selected
            .iter()
            .find(|(_, selected)| {
                if want_read {
                    selected.interest.is_readable()
                } else {
                    selected.interest.is_writable()
                }
            })
            .map(|(fd, _)| *fd)
    }

    /// Takes the `stop_select` events a `driver_select(..., 0)` queued (see
    /// [`DriverState::queued_stop_select`]), leaving the queue empty.
    pub(crate) fn take_queued_stop_select(&mut self) -> Vec<ErlDrvEvent> {
        core::mem::take(&mut self.queued_stop_select)
    }

    /// Takes **every** `driver_select` registration still in force, as
    /// `(fd, token, owes stop_select)`, leaving none.
    ///
    /// This is the port-teardown walk: each entry's poller registration must be withdrawn (the
    /// caller does that through its [`DriverContext`](crate::driver::DriverContext)) and each
    /// `ERL_DRV_USE` registration still owes its owner a release callback. The C's equivalent is
    /// `erts_check_io_cleanup`/`deselect`, which fires `(*stop_select)((ErlDrvEvent) fd, NULL)`
    /// before the fd is closed (`erl_check_io.c:2232-2238`) — the **fd cast to a pointer**, not
    /// necessarily the operand the driver originally selected with.
    pub(crate) fn take_all_selected(&mut self) -> Vec<(RawFd, Token, bool)> {
        core::mem::take(&mut self.selected)
            .into_iter()
            .map(|(fd, selected)| (fd, selected.token, selected.wants_stop_select()))
            .collect()
    }
}

/// The registered driver identity of the port an [`AbiHost`] belongs to.
///
/// The C reaches the same two facts as `prt->drv_ptr->name` and `prt->drv_ptr->handle != NULL`;
/// they are what `driver_lock_driver` (which consults the port's *own* driver) needs. A host built
/// without one models a port whose driver has no `dlopen` handle — the C's statically-linked case.
struct DriverIdentity {
    /// The name the driver's `ErlDrvEntry` declares (`dp->name`).
    name: String,
    /// Whether the port was built from a **dynamically loaded** object (`dp->handle != NULL`),
    /// i.e. from an [`Arc<DriverLibrary>`](beam_abi::driver::loader::DriverLibrary).
    loaded: bool,
}

/// The [`DriverHost`](beam_abi::driver::host::DriverHost) implementation for one driver port.
///
/// Owns everything the C-facing surface needs — the [`PortHost`], the driver's term heap, the
/// persistent [`DriverState`] and the installed-context slot — so it can be `Box`ed and pointed at
/// by a stable [`DriverEnv`](beam_abi::driver::host::DriverEnv) for the port's whole life.
pub struct AbiHost {
    /// The runtime hook supplying atom interning (and the optional pid/term sinks).
    port_host: Box<dyn PortHost>,
    /// The term arena drivers allocate on through
    /// [`DriverHost::heap`](beam_abi::driver::host::DriverHost::heap).
    heap: Heap,
    /// The port's persistent C-ABI state (see [`DriverState`]).
    state: DriverState,
    /// The port's registered driver identity, or `None` for a bare host (see [`DriverIdentity`]).
    driver: Option<DriverIdentity>,
    /// The [`DriverContext`](crate::driver::DriverContext) installed for the duration of one
    /// driver callback, or null outside a callback. See [`AbiHost::with_ctx`].
    ctx: Cell<*mut c_void>,
}

impl AbiHost {
    /// Build a host over `port_host` with **no** driver identity, and a fresh
    /// [`DRIVER_HEAP_WORDS`]-word term heap.
    ///
    /// With no identity the host answers `driver_lock_driver` with the C's statically-linked `-1`
    /// (see [`DriverHost::lock_driver`]): it is the right host for a driver whose `.so` handle is
    /// unknown. [`AbiHost::for_driver`] supplies one.
    ///
    /// # Errors
    ///
    /// Returns the allocator's error when the term heap cannot be backed (the only fallible step).
    pub fn new(port_host: Box<dyn PortHost>) -> Result<AbiHost> {
        Ok(AbiHost {
            port_host,
            heap: Heap::new(DRIVER_HEAP_WORDS)?,
            state: DriverState::new(),
            driver: None,
            ctx: Cell::new(core::ptr::null_mut()),
        })
    }

    /// Build a host over `port_host` that carries the port's registered driver `name` and whether
    /// the port was built from a **dynamically loaded** object.
    ///
    /// This is what a port opened from a `dlopen`ed driver uses (see
    /// [`DriverPort::from_library`](super::port::DriverPort::from_library)), so
    /// `driver_lock_driver` answers the C's `0` for it.
    ///
    /// # Errors
    ///
    /// As [`AbiHost::new`].
    pub fn for_driver(
        port_host: Box<dyn PortHost>,
        name: impl Into<String>,
        loaded: bool,
    ) -> Result<AbiHost> {
        Ok(AbiHost::new(port_host)?.with_driver(name, loaded))
    }

    /// Records the port's registered driver `name` and whether it was dynamically loaded.
    pub fn with_driver(mut self, name: impl Into<String>, loaded: bool) -> AbiHost {
        self.driver = Some(DriverIdentity {
            name: name.into(),
            loaded,
        });
        self
    }
    /// Records `ctx` as the live port context for the driver callback about to run.
    ///
    /// The caller **must** pair this with [`AbiHost::clear`] before the context goes out of scope —
    /// [`DriverPort`](super::port::DriverPort) does, in the same method that made the call.
    pub(crate) fn install(&self, ctx: &mut DriverContext<'_>) {
        self.ctx
            .set(ctx as *mut DriverContext<'_> as *mut c_void);
    }

    /// Forgets the installed context (see [`AbiHost::install`]).
    pub(crate) fn clear(&self) {
        self.ctx.set(core::ptr::null_mut());
    }

    /// The port's persistent state, read-only.
    pub(crate) fn state(&self) -> &DriverState {
        &self.state
    }

    /// Whether the port's `driver_async` job set has outstanding jobs.
    ///
    /// `true` while jobs are running or completed-but-undelivered, i.e. exactly when a
    /// [`Driver::drive_async`](crate::driver::Driver::drive_async) call would run at least one
    /// `ready_async`/`async_free`. This is the probe the port-task round uses to decide whether the
    /// port needs an async drive point (see
    /// [`crate::PortRegistry::execute_port_tasks`](crate::PortRegistry::execute_port_tasks)).
    pub fn has_pending_async(&self) -> bool {
        self.state.jobs.in_flight() > 0
    }

    /// The port's persistent state, mutably.
    pub(crate) fn state_mut(&mut self) -> &mut DriverState {
        &mut self.state
    }

    /// Runs `f` against the installed port context, or returns `None` when none is installed.
    fn with_ctx<R>(&self, f: impl FnOnce(&mut DriverContext<'_>) -> R) -> Option<R> {
        let ptr = self.ctx.get();
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `ptr` was stored by `install` from a `&mut DriverContext<'_>` for a callback that
        // is still running (nothing clears it in between), and `DriverContext` is a plain struct of
        // `&mut` borrows whose referents the port keeps alive for the whole callback — so the
        // reborrow reproduces exactly the borrow `install` was handed. `f` cannot let the reference
        // escape: it is `FnOnce` over `&mut DriverContext<'_>` and returns `R`.
        let ctx: &mut DriverContext<'_> = unsafe { &mut *(ptr as *mut DriverContext<'_>) };
        Some(f(ctx))
    }
}

/// The interest mask an `int mode` argument to `driver_select` expresses.
///
/// `mode` is a bitwise or of [`ERL_DRV_READ`] and [`ERL_DRV_WRITE`], optionally with
/// `ERL_DRV_USE`/`ERL_DRV_USE_NO_CALLBACK` — the two release bits, which ask for (or decline) the
/// driver's `stop_select` callback and are recorded on the [`SelectedFd`] instead of being mapped
/// to an interest (see `AbiHost::select`). Neither direction set is mapped to
/// [`Interest::READABLE`] rather than to "not registered": the fd must still be watched so the
/// platform reports its close/error state (the C's `driver_select` always registers the event).
pub(crate) fn interest_from_mode(mode: c_int) -> Interest {
    match (mode & ERL_DRV_READ != 0, mode & ERL_DRV_WRITE != 0) {
        (true, true) => Interest::BOTH,
        (true, false) => Interest::READABLE,
        (false, true) => Interest::WRITABLE,
        (false, false) => Interest::READABLE,
    }
}

impl DriverHost for AbiHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        self.port_host.intern_atom(name)
    }

    /// `driver_output`: queue `data` for the port **owner**.
    ///
    /// The bytes go onto the port's **input** queue — the queue the reactor turns into a
    /// `{Port, {data, Data}}` message on the next port-task round — and never onto the output queue
    /// (which is Erlang→driver). Nothing here knows which callback it was called from, which is
    /// deliberate: the C's `driver_output` reaches the owner whatever made it, so the report is
    /// gated on the queue being non-empty and not on an `INPUT` task having run.
    fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
        match self.with_ctx(|ctx| ctx.input().push(data)) {
            Some(()) => 0,
            None => -1,
        }
    }

    fn send_term(&mut self, _port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int {
        if self.port_host.send_term(to, term) {
            0
        } else {
            -1
        }
    }

    /// `driver_select`: register, re-register or de-register interest in an OS handle.
    ///
    /// The handle is the C convention of a **fd cast to a pointer**
    /// (`ErtsSysFdType fd = (ErtsSysFdType) e;`, `erl_check_io.c`). A second `driver_select` on the
    /// same fd with a new `mode` **re-registers** (the interest is updated), matching the C's
    /// one-registration-per-event model; de-selecting an fd that was never selected succeeds (the
    /// C's `driver_select` is idempotent).
    ///
    /// The `ERL_DRV_USE` release obligation is recorded with the registration
    /// ([`SelectedFd::wants_stop_select`]) rather than folded into the interest: it only asks for
    /// the driver's `stop_select` release callback, and `interest_from_mode` therefore leaves it out
    /// (a `mode` of `ERL_DRV_USE` alone still registers `READABLE`, the "watch it for
    /// close/error" fallback). The obligation is merged on re-selection rather than overwritten (a
    /// select without the bit keeps a request an earlier select made); a release queues the event
    /// the *deselecting* call was given, which is the operand the C's `stop_select` receives
    /// (`erl_check_io.c:1215`).
    fn select(&mut self, _port: ErlDrvPort, event: ErlDrvEvent, mode: c_int, on: c_int) -> c_int {
        let fd = event as usize as RawFd;
        let interest = interest_from_mode(mode);

        if on == 0 {
            let Some(selected) = self.state.selected.remove(&fd) else {
                return 0; // De-selecting an unknown fd is a no-op, not a failure.
            };
            let owes_stop_select = selected.wants_stop_select();
            let result = self.with_ctx(|ctx| ctx.deregister(fd, selected.token));
            if owes_stop_select {
                // Queued *after* the context borrow ends; the port fires it once this callback has
                // returned (see `DriverState::queued_stop_select`).
                self.state.queued_stop_select.push(event);
            }
            return match result {
                Some(Ok(())) => 0,
                _ => -1,
            };
        }

        let existing = self.state.selected.get(&fd);
        let token = existing.map(|selected| selected.token);
        // A select that says nothing about `ERL_DRV_USE` leaves an earlier request in force (the
        // C's sticky `ERTS_EV_FLAG_USED`); one that carries the `_NO_CALLBACK` bit declines it.
        let owes_stop_select = match (mode & ERL_DRV_USE != 0, existing) {
            (false, Some(selected)) => selected.wants_stop_select(),
            (false, None) => false,
            (true, _) => (mode & ERL_DRV_USE_NO_CALLBACK) == ERL_DRV_USE,
        };
        let result = self.with_ctx(|ctx| match token {
            Some(token) => ctx.reregister(fd, token, interest).map(|()| token),
            None => ctx.register(fd, interest),
        });
        match result {
            Some(Ok(token)) => {
                self.state.selected.insert(
                    fd,
                    SelectedFd {
                        token,
                        interest,
                        owes_stop_select,
                    },
                );
                0
            }
            _ => -1,
        }
    }

    /// `driver_set_timer`: arm the port's `timeout` callback `ms` milliseconds from now.
    ///
    /// The deadline is mirrored into the port's single `TIMEOUT`
    /// [port task](crate::PortTaskKind) by
    /// [`DriverPort::next_timeout`](super::port::DriverPort), which the port re-reads after every
    /// driver interaction.
    ///
    /// **M1 always succeeds.** The C refuses when the port has no timer support
    /// (`if (prt->timeout == NULL) return -1;`, `io.c`); this port always has one, so the check has
    /// nothing to reject. An expired deadline reports `0` from
    /// [`DriverHost::read_timer`](beam_abi::driver::host::DriverHost::read_timer), as in the C.
    fn set_timer(&mut self, _port: ErlDrvPort, ms: c_ulong) -> c_int {
        self.state.deadline = Some(Instant::now() + Duration::from_millis(ms as u64));
        0
    }

    fn cancel_timer(&mut self, _port: ErlDrvPort) -> c_int {
        self.state.deadline = None;
        0
    }

    /// `driver_read_timer`: the remaining milliseconds, or `None` when no timer is armed.
    ///
    /// An already-expired deadline reports `Some(0)`, matching the C (which writes `0`).
    /// `beam_abi`'s `driver_read_timer` maps `None` to `-1` — its own documented deviation from
    /// `io.c:6936`, which answers `0` with `0` for "no timer".
    fn read_timer(&mut self, _port: ErlDrvPort) -> Option<c_ulong> {
        self.state.deadline.map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            remaining.as_millis().min(c_ulong::MAX as u128) as c_ulong
        })
    }

    /// `driver_async`: run `invoke(data)` on a worker thread and hand the result to the port.
    ///
    /// The returned value is the job's queue position (`>= 0`), which the driver stores and compares
    /// against the completions it later sees in `ready_async` — see [`AsyncJobs`]. A `None`
    /// `invoke` is the fire-and-forget form (a position is still returned, and no completion is
    /// ever recorded).
    ///
    /// # Errors
    ///
    /// When no worker thread can be started, the C runs the driver's `async_free` and answers a
    /// negative error code; that is what happens here too (the `free` callback releases `data`,
    /// since no `ready_async` will ever run for it).
    fn r#async(
        &mut self,
        _port: ErlDrvPort,
        _key: *mut c_uint,
        invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_long {
        match self.state.jobs.schedule(invoke, data, free) {
            Ok(id) => id as c_long,
            Err(()) => {
                if let Some(free) = free {
                    // SAFETY: `free` is the driver's own `async_free` and `data` is the pointer it
                    // handed to `driver_async`; the driver contract is that `free` releases `data`,
                    // which is exactly what it is for when no `invoke`/`ready_async` will run.
                    unsafe { free(data) };
                }
                -1
            }
        }
    }

    fn connected_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        self.port_host.connected_pid()
    }

    fn caller_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        self.port_host.caller_pid()
    }

    /// `add_driver_entry`: record the statically linked driver entry `de` in the process-global
    /// [`DriverTable`] (the C's `driver_list`), running its `init` first (`io.c:7640-7645`).
    ///
    /// `0` = recorded, `1` = the entry's `init` refused it. The C's `void` wrapper **discards**
    /// the underlying `erts_add_driver_entry` result on purpose ("the init is not allowed to fail
    /// when drivers are added by drivers", `io.c:7642-7645`), so the refusal is a value here only
    /// so it stays distinguishable from the trait default's `-1` ("no current host").
    fn add_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        // SAFETY: `de` is the driver's own static `ErlDrvEntry`, live (its code mapped, its name
        // readable) for as long as it is registered — exactly the contract `driver_init` gives the
        // C and `DriverTable::add_entry` documents. The pointer is never dereferenced here.
        match unsafe { DriverTable::global().add_entry(de) } {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// `remove_driver_entry`: remove `de` from the process-global [`DriverTable`] (the C's
    /// `driver_list`), answering the C's `1` removed / `0` not listed / `-1` dynamic-driver case.
    ///
    /// The table's own answer is forwarded unchanged: it compares entries by address only, so it
    /// needs no `// SAFETY:` contract on `de`, and a dynamic registration's refusal is its business
    /// to report.
    fn remove_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        DriverTable::global().remove_entry(de)
    }

    /// `driver_lock_driver`: lock the port's **own** driver in the machine for good
    /// (`io.c:7324`).
    ///
    /// The C consults `prt->drv_ptr->handle` — the *port's own* driver, never a lookup by name —
    /// and answers `-1` when that handle is null (`io.c:7337`, a statically linked driver) and `0`
    /// otherwise. A [`DriverPort`](super::port::DriverPort) is **always** built from an
    /// [`Arc<DriverLibrary>`](beam_abi::driver::loader::DriverLibrary), i.e. dynamically loaded,
    /// so a host carrying such an identity answers `0`; a bare host (no identity) is the C's
    /// statically-linked case and answers `-1`.
    ///
    /// The process-global [`DriverTable`] is consulted only to honour and record its `locked` flag
    /// when a registration for the name exists; the answer does **not** depend on it. That matters
    /// because the runtime never registers its loaded drivers in the global table (the C likewise
    /// consults the port, not the list), so a table lookup must never turn a dynamically loaded
    /// port into `-1`.
    fn lock_driver(&mut self, _port: ErlDrvPort) -> c_int {
        match &self.driver {
            Some(identity) if identity.loaded => {
                let _ = DriverTable::global().lock_driver(&identity.name);
                0
            }
            _ => -1,
        }
    }

    /// `driver_create_port`: create a new port owned by the emitting driver (`io.c:786`).
    ///
    /// Always the invalid-port sentinel [`ERTS_INVALID_ERL_DRV_PORT`] — the C's single failure
    /// answer for a request that cannot be honoured (never a fabricated port). Two independent
    /// reasons, either of which alone forces it:
    ///
    /// 1. [`DriverTable::create_port`] needs a live `&mut PortRegistry` and `&mut Poller`, but
    ///    every driver callback here runs **inside** a [`PortRegistry`](crate::PortRegistry)
    ///    method that already holds a `&mut` into the registry's port map (the `&mut Port` the
    ///    callback was dispatched through). Re-entering `PortRegistry::open` with `&mut self`
    ///    would alias that borrow, and because `open` **inserts** into the map it can even
    ///    invalidate the live `&mut Port` — undefined behaviour, not merely a missing feature.
    /// 2. The new port also needs its own `Box<dyn PortHost>`, which only `beam_rt` can supply
    ///    (the real `PortHost` lives outside this crate) and which no defaulted hook hands over.
    fn create_port(
        &mut self,
        _creator: ErlDrvPort,
        _connected: ErlDrvTermData,
        _name: *mut c_char,
        _drv_data: ErlDrvData,
    ) -> ErlDrvPort {
        ERTS_INVALID_ERL_DRV_PORT
    }
}

#[cfg(test)]
mod tests {
    use super::{interest_from_mode, AbiHost, LocalAtomPortHost, PortHost};
    use beam_abi::driver::host::DriverHost;
    use beam_abi::driver::types::{
        ErlDrvEntry, ErlDrvEvent, ErlDrvEvent_, ERTS_INVALID_ERL_DRV_PORT, ERL_DRV_READ,
        ERL_DRV_USE, ERL_DRV_USE_NO_CALLBACK, ERL_DRV_WRITE,
    };
    use beam_process::PortId;
    use beam_term::prelude::{is_atom, make_atom, AtomTable};
    use libc::c_int;
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::sleep;
    use std::time::Duration;

    use crate::abi::DriverTable;
    use crate::driver::PortArms;
    use crate::poller::{Interest, Poller};
    use crate::queue::ByteQueue;
    use crate::DriverContext;

    /// A fresh host over a private atom table.
    fn new_host() -> AbiHost {
        AbiHost::new(Box::new(LocalAtomPortHost::new())).expect("a driver heap")
    }

    /// A `driver_select`-style event handle: the fd cast to a pointer (the C convention).
    fn event_for(fd: RawFd) -> ErlDrvEvent {
        fd as usize as *mut ErlDrvEvent_
    }

    /// A fresh poll set plus the two port queues and the arming record one context needs.
    fn new_context_parts() -> (Poller, ByteQueue, ByteQueue, PortArms) {
        (
            Poller::new().expect("a poll set"),
            ByteQueue::new(),
            ByteQueue::new(),
            PortArms::default(),
        )
    }

    #[test]
    fn the_local_atom_host_interns_into_its_own_table() {
        let mut table = AtomTable::new();
        let expected = make_atom(table.intern("ok"));
        let mut host = LocalAtomPortHost::with_names(&["ok", "error"]);
        assert_eq!(host.intern_atom("ok"), Some(expected));
        // A repeat is the same atom, and seeding order is insertion order.
        assert_eq!(host.intern_atom("ok"), Some(expected));
        assert_ne!(host.intern_atom("error"), Some(expected));
        assert_eq!(host.connected_pid(), None);
        assert_eq!(host.caller_pid(), None);
        assert!(!host.send_term(None, expected));
        assert_eq!(
            LocalAtomPortHost::default().intern_atom("x").map(is_atom),
            Some(true)
        );
    }

    #[test]
    fn the_new_host_starts_empty_and_has_a_term_heap() {
        let mut host = new_host();
        assert!(host.state().deadline().is_none());
        assert_eq!(host.state().selected_len(), 0);
        assert_eq!(host.state().in_flight(), 0);
        assert_eq!(host.heap().capacity(), super::DRIVER_HEAP_WORDS);
        assert_eq!(host.heap().used(), 0);
    }

    #[test]
    fn driver_output_lands_in_the_port_input_queue() {
        let mut host = new_host();
        let (mut poller, mut input, mut output, mut arms) = new_context_parts();
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        );
        host.install(&mut ctx);

        assert_eq!(
            DriverHost::output(&mut host, core::ptr::null_mut(), b"hi"),
            0
        );
        assert_eq!(ctx.input().front(), b"hi");
        assert_eq!(
            DriverHost::output(&mut host, core::ptr::null_mut(), b"!"),
            0
        );
        assert_eq!(ctx.input().front(), b"hi!");
        // Nothing was queued for the device: `driver_output` is owner-bound.
        assert_eq!(ctx.output().len(), 0);
        host.clear();

        // With the context cleared the same call is refused, never a null dereference.
        assert_eq!(
            DriverHost::output(&mut host, core::ptr::null_mut(), b"x"),
            -1
        );
    }

    #[test]
    fn the_host_methods_fail_gracefully_without_an_installed_context() {
        let mut host = new_host();
        assert_eq!(
            DriverHost::select(
                &mut host,
                core::ptr::null_mut(),
                event_for(0),
                ERL_DRV_READ,
                1
            ),
            -1
        );
        assert_eq!(host.state().selected_len(), 0);
    }

    #[test]
    fn select_registers_reregisters_and_deselects_one_fd() {
        let mut host = new_host();
        let (mut poller, mut input, mut output, mut arms) = new_context_parts();
        let (_peer, local) = UnixStream::pair().expect("socketpair(2) succeeds");
        let fd = local.as_raw_fd();
        let event = event_for(fd);

        {
            let mut ctx = DriverContext::new(
                PortId::new(0),
                &mut poller,
                &mut input,
                &mut output,
                &mut arms,
            );
            host.install(&mut ctx);

            assert_eq!(
                DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_READ, 1),
                0
            );
            assert_eq!(host.state().selected_len(), 1);
            assert_eq!(host.state().fd_armed(true), Some(fd));
            assert_eq!(host.state().fd_armed(false), None);

            // A re-select with a new mode updates the interest instead of adding an entry.
            assert_eq!(
                DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_WRITE, 1),
                0
            );
            assert_eq!(host.state().selected_len(), 1);
            assert_eq!(host.state().fd_armed(true), None);
            assert_eq!(host.state().fd_armed(false), Some(fd));

            // De-selecting releases the poller registration.
            assert_eq!(
                DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_READ, 0),
                0
            );
            assert_eq!(host.state().selected_len(), 0);

            // De-selecting an fd that was never selected still succeeds.
            assert_eq!(
                DriverHost::select(
                    &mut host,
                    core::ptr::null_mut(),
                    event_for(4242),
                    ERL_DRV_READ,
                    0
                ),
                0
            );
            host.clear();
        }

        assert!(poller.is_empty(), "the poll set holds no registration");
    }

    /// The two `ERL_DRV_USE` bits are recorded with the registration (never folded into the
    /// interest), and a released `ERL_DRV_USE` registration queues its event for the port's
    /// `stop_select` (see `DriverState::queued_stop_select`).
    #[test]
    fn select_records_the_use_release_bits_and_queues_the_release() {
        let mut host = new_host();
        let (mut poller, mut input, mut output, mut arms) = new_context_parts();
        let (_peer, local) = UnixStream::pair().expect("socketpair(2) succeeds");
        let fd = local.as_raw_fd();
        let event = event_for(fd);
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut poller,
            &mut input,
            &mut output,
            &mut arms,
        );
        host.install(&mut ctx);

        // `ERL_DRV_READ | ERL_DRV_USE`: the release bit is not an interest, so the registration is
        // still read-only, and nothing is queued while the selection is live.
        assert_eq!(
            DriverHost::select(
                &mut host,
                core::ptr::null_mut(),
                event,
                ERL_DRV_READ | ERL_DRV_USE,
                1
            ),
            0
        );
        assert_eq!(host.state().fd_armed(true), Some(fd));
        assert_eq!(host.state().fd_armed(false), None);
        assert!(host.state_mut().take_queued_stop_select().is_empty());

        // De-selecting queues the event the deselecting call was handed (the C's operand).
        assert_eq!(
            DriverHost::select(&mut host, core::ptr::null_mut(), event, ERL_DRV_READ, 0),
            0
        );
        assert_eq!(host.state_mut().take_queued_stop_select(), vec![event]);
        assert!(
            host.state_mut().take_queued_stop_select().is_empty(),
            "taking the queue empties it"
        );

        // The release obligation each `mode` records: only `ERL_DRV_USE` without `_NO_CALLBACK`.
        for (mode, owes) in [
            (ERL_DRV_READ, false),
            (ERL_DRV_USE, true),
            (ERL_DRV_READ | ERL_DRV_USE, true),
            (ERL_DRV_WRITE | ERL_DRV_USE, true),
            (ERL_DRV_USE_NO_CALLBACK, false),
            (ERL_DRV_READ | ERL_DRV_USE_NO_CALLBACK, false),
        ] {
            assert_eq!(
                DriverHost::select(&mut host, core::ptr::null_mut(), event, mode, 1),
                0,
                "the selection registers"
            );
            let mut selected = host.state_mut().take_all_selected();
            assert_eq!(selected.len(), 1, "exactly one registration was live ({mode})");
            let (taken_fd, token, taken_owes) = selected.pop().expect("one entry was taken");
            assert_eq!(taken_fd, fd);
            assert_eq!(taken_owes, owes, "the release obligation recorded for mode {mode}");
            ctx.deregister(fd, token)
                .expect("the poller registration is withdrawn");
        }

        host.clear();
        assert!(poller.is_empty(), "every registration was released");
    }

    #[test]
    fn the_mode_bitfield_maps_onto_the_interest_mask() {
        assert_eq!(interest_from_mode(ERL_DRV_READ), Interest::READABLE);
        assert_eq!(interest_from_mode(ERL_DRV_WRITE), Interest::WRITABLE);
        assert_eq!(
            interest_from_mode(ERL_DRV_READ | ERL_DRV_WRITE),
            Interest::BOTH
        );
        // Neither direction set still watches the fd (see the function's docs).
        assert_eq!(interest_from_mode(4), Interest::READABLE);
        assert_eq!(interest_from_mode(0 as c_int), Interest::READABLE);
    }

    #[test]
    fn the_port_timer_arms_reports_and_cancels() {
        let mut host = new_host();

        assert_eq!(
            DriverHost::read_timer(&mut host, core::ptr::null_mut()),
            None,
            "no timer is armed yet"
        );
        assert_eq!(DriverHost::set_timer(&mut host, core::ptr::null_mut(), 50), 0);
        let deadline = host.state().deadline().expect("a deadline was armed");
        assert!(deadline > std::time::Instant::now());
        let remaining = DriverHost::read_timer(&mut host, core::ptr::null_mut())
            .expect("a remaining time is reported");
        assert!(remaining <= 50, "the remaining time never exceeds the request");
        assert!(remaining > 0, "a 50 ms timer has not expired yet: {remaining}");

        assert_eq!(DriverHost::cancel_timer(&mut host, core::ptr::null_mut()), 0);
        assert_eq!(host.state().deadline(), None);
        assert_eq!(DriverHost::read_timer(&mut host, core::ptr::null_mut()), None);

        // An immediate timer reports `Some(0)`, as the C writes `0`.
        assert_eq!(DriverHost::set_timer(&mut host, core::ptr::null_mut(), 0), 0);
        assert_eq!(
            DriverHost::read_timer(&mut host, core::ptr::null_mut()),
            Some(0)
        );
    }

    static INVOCATIONS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn count_invocation(_data: *mut libc::c_void) {
        INVOCATIONS.fetch_add(1, Ordering::SeqCst);
    }

    /// Blocks briefly until `done()` holds, so the async tests are not timing-flaky.
    fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..400 {
            if done() {
                return true;
            }
            sleep(Duration::from_millis(5));
        }
        done()
    }

    #[test]
    fn driver_async_runs_the_invoke_and_records_a_completion() {
        INVOCATIONS.store(0, Ordering::SeqCst);
        let mut host = new_host();

        let id = DriverHost::r#async(
            &mut host,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            Some(count_invocation),
            core::ptr::null_mut(),
            None,
        );
        assert!(id >= 0, "driver_async returns a queue position, got {id}");
        assert!(wait_for(|| INVOCATIONS.load(Ordering::SeqCst) == 1));
        assert_eq!(INVOCATIONS.load(Ordering::SeqCst), 1);
        assert_eq!(host.state().in_flight(), 1);

        // The worker reports its completion; the port thread drains it (what `poll_async` does).
        let mut jobs = 0;
        wait_for(|| {
            jobs = host.state_mut().jobs.drain().len();
            jobs > 0
        });
        assert_eq!(jobs, 1, "exactly one completion is recorded");
        assert_eq!(host.state().in_flight(), 0);
    }

    #[test]
    fn a_null_async_invoke_returns_a_position_and_records_nothing() {
        let mut host = new_host();
        let id = DriverHost::r#async(
            &mut host,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            None,
            core::ptr::null_mut(),
            None,
        );
        assert!(id >= 0, "the fire-and-forget form still returns a position");
        // Give a (non-existent) worker ample time; nothing must ever be reported.
        sleep(Duration::from_millis(30));
        assert_eq!(host.state_mut().jobs.drain().len(), 0);
        assert_eq!(host.state().in_flight(), 0);
    }

    #[test]
    fn the_port_host_defaults_decline_the_optional_hooks() {
        struct Bare;
        impl PortHost for Bare {
            fn intern_atom(&mut self, _name: &str) -> Option<beam_term::prelude::Eterm> {
                None
            }
        }

        let mut host = AbiHost::new(Box::new(Bare)).expect("a driver heap");
        assert_eq!(host.intern_atom("ok"), None);
        assert_eq!(host.connected_pid(core::ptr::null_mut()), None);
        assert_eq!(host.caller_pid(core::ptr::null_mut()), None);
        assert_eq!(
            host.send_term(core::ptr::null_mut(), None, beam_term::prelude::NIL),
            -1
        );
    }

    // -------------------------------------------------------------------------------------------
    // The process-global driver-registry hooks
    // -------------------------------------------------------------------------------------------

    /// A statically named `ErlDrvEntry` with no `init` — the shape `add_driver_entry` records.
    ///
    /// A distinct `'static` name per test keeps the shared process-global [`DriverTable`] from
    /// letting two parallel tests see each other's registrations (cargo runs them concurrently).
    fn named_entry(name: *const u8) -> ErlDrvEntry {
        ErlDrvEntry {
            driver_name: name as *mut libc::c_char,
            ..ErlDrvEntry::default()
        }
    }

    #[test]
    fn add_and_remove_driver_entry_reach_the_process_global_table() {
        const NAME: &[u8] = b"host_test_registry\0";
        let mut host = new_host();
        let mut entry = named_entry(NAME.as_ptr());

        // `remove_driver_entry` of an address the table has never seen is the C's `0`.
        assert_eq!(
            DriverHost::remove_driver_entry(&mut host, &mut entry),
            0,
            "an unregistered entry is the C's 0 (not found)"
        );

        // `add_driver_entry` records it in the process-global table and answers `0`.
        assert_eq!(
            DriverHost::add_driver_entry(&mut host, &mut entry),
            0,
            "recording the entry is the C's void-wrapper success"
        );
        assert_eq!(
            DriverTable::global()
                .lookup("host_test_registry")
                .map(|driver| driver.entry),
            Some(&mut entry as *mut ErlDrvEntry),
            "the entry reached the process-global table"
        );

        // Removing the same pointer answers the C's `1`, and a second removal reports `0`.
        assert_eq!(
            DriverHost::remove_driver_entry(&mut host, &mut entry),
            1,
            "removed"
        );
        assert!(DriverTable::global().lookup("host_test_registry").is_none());
        assert_eq!(
            DriverHost::remove_driver_entry(&mut host, &mut entry),
            0,
            "not found any more"
        );
    }

    #[test]
    fn lock_driver_reports_the_ports_own_driver_handle() {
        // A bare host carries no driver identity: the C's statically-linked case (`handle == NULL`).
        let mut bare = new_host();
        assert_eq!(
            DriverHost::lock_driver(&mut bare, core::ptr::null_mut()),
            -1,
            "a host with no driver identity answers the C's -1"
        );

        // A host carrying a dynamically loaded identity answers `0` even though the name is not
        // registered in the process-global table — the C consults the port's own driver, never
        // `driver_list`.
        let mut loaded =
            AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), "lock_driver_probe", true)
                .expect("a driver heap");
        assert_eq!(
            DriverHost::lock_driver(&mut loaded, core::ptr::null_mut()),
            0,
            "a loaded driver locks regardless of any global registration"
        );

        // An identity that is statically linked is still the C's `-1`.
        let mut statically =
            AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), "lock_driver_probe", false)
                .expect("a driver heap");
        assert_eq!(
            DriverHost::lock_driver(&mut statically, core::ptr::null_mut()),
            -1,
            "a statically linked driver has no handle to lock"
        );
    }

    #[test]
    fn create_port_is_the_invalid_port_sentinel() {
        let mut host = new_host();
        let name = c"host_driver";
        assert_eq!(
            DriverHost::create_port(
                &mut host,
                core::ptr::null_mut(),
                1,
                name.as_ptr() as *mut libc::c_char,
                core::ptr::null_mut(),
            ),
            ERTS_INVALID_ERL_DRV_PORT,
            "a request the host cannot honour answers the C's single failure value"
        );
    }

    /// The two **no-port** slots reach a host **only** inside a `beam_abi::with_current_host`
    /// scope — the thread-local publication the adapter installs around every driver callback.
    #[test]
    fn the_no_port_slots_reach_the_host_only_inside_a_publication() {
        const NAME: &[u8] = b"host_test_publication\0";
        let table = DriverTable::global();
        let api = beam_abi::driver::drv::driver_api_table();
        let add = api.add_driver_entry.expect("the slot is populated");
        let remove = api.remove_driver_entry.expect("the slot is populated");

        let mut host = new_host();
        let mut entry = named_entry(NAME.as_ptr());

        // Outside any publication there is no current host: the slots answer the C's failure
        // values without touching anything.
        // SAFETY: `entry` is a live `ErlDrvEntry` for this test's whole body; the pointer is only
        // forwarded to a host, and there is none.
        unsafe { add(&mut entry) };
        assert!(
            table.lookup("host_test_publication").is_none(),
            "with no published host nothing is recorded"
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { remove(&mut entry) },
            0,
            "with no published host the removal is the C's 0"
        );

        // Inside the publication the port's own host handles the request.
        beam_abi::with_current_host(&mut host, || {
            // SAFETY: `entry` is live; the published host records its address.
            unsafe { add(&mut entry) };
            assert!(
                table.lookup("host_test_publication").is_some(),
                "the published host recorded the entry"
            );
        });
        assert!(
            table.lookup("host_test_publication").is_some(),
            "the registration outlives the publication"
        );

        // The publication is gone again, so the entry can only be removed inside one.
        beam_abi::with_current_host(&mut host, || {
            // SAFETY: `entry` is live; the published host removes its address.
            assert_eq!(
                unsafe { remove(&mut entry) },
                1,
                "the published host removes the entry"
            );
        });
        assert!(table.lookup("host_test_publication").is_none());
    }
}
