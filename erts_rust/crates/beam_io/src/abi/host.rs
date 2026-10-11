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

use beam_abi::common::SysIOVec;
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
use crate::queue::ByteQueue;

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
    /// The port's **driver queue** — the C's `prt->ioq`, the byte queue the `driver_enq*` family
    /// fills and `driver_peekq`/`driver_deq` drain.
    ///
    /// It is deliberately **not** the port's `DriverContext::output()` queue. The C keeps the two
    /// apart: `erts_port_output` (the Erlang→driver direction) builds its vector from the Erlang
    /// iodata term on the fly and never reads `prt->ioq` (`io.c:1609-1646`, `io.c:1795-1846`),
    /// whereas every `driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq` is
    /// defined straight in terms of `drvport2ioq(ix)` (`io.c:6790-6919`) — a buffer only the driver
    /// touches, drained by the driver's own `ready_output` (the reference user is `inet_drv`'s
    /// `tcp_sendv`/`tcp_inet_output`, `inet_drv.c:13771-13842`, `:14093-14104`). Merging them would
    /// re-deliver a driver's own queued bytes to its `outputv` as if they were new Erlang commands;
    /// [`AbiHost::enq`] records the concrete consequence.
    pub(crate) ioq: ByteQueue,
}

impl DriverState {
    /// An empty state: nothing selected, no timer, no jobs, an empty driver queue.
    pub(crate) fn new() -> DriverState {
        DriverState {
            selected: BTreeMap::new(),
            deadline: None,
            jobs: AsyncJobs::new(),
            queued_stop_select: Vec::new(),
            ioq: ByteQueue::new(),
        }
    }

    /// The port timer deadline, or `None` when none is armed.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// How many bytes the driver has queued on its own queue (`driver_sizeq`) — the observable
    /// behind the `driver_enq`/`driver_deq` hooks.
    pub fn queue_len(&self) -> usize {
        self.ioq.len()
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

    /// The head of the port's **driver queue** as a safe byte slice — the safe companion of
    /// [`DriverHost::peekq`], exactly as [`DriverState::queue_len`] is the safe companion of
    /// [`DriverHost::sizeq`].
    ///
    /// The raw hook [`DriverHost::peekq`] hands back `SysIOVec`s whose `iov_base` is a **raw
    /// pointer**, so a caller that wanted the queued bytes (what `driver_peekq`/`driver_peekqv`
    /// publish to a C driver) had to dereference it under `unsafe`. This accessor hands out the same
    /// bytes as a borrowed slice instead, and answers `None` for an empty queue (the raw hook's
    /// `NULL` view).
    ///
    /// The slice borrows the queue's own buffer, so it stays valid only until the next
    /// [`enq`](DriverHost::enq)/[`pushq`](DriverHost::pushq)/[`deq`](DriverHost::deq) — the lifetime
    /// the C gives the pointer it returns — which is what the `&self` borrow enforces: every hook
    /// that mutates the queue takes `&mut self`.
    ///
    /// Because this host's queue is one contiguous [`ByteQueue`], the slice is the queue's whole
    /// content — the raw view's single `vlen == 1` entry (see [`DriverHost::enq`] for that deviation
    /// from the C's per-enqueue chunking) — and [`DriverState::queue_len`] is its length.
    pub fn queue_head(&self) -> Option<&[u8]> {
        let front = self.state.ioq.front();
        (!front.is_empty()).then_some(front)
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

    // -------------------------------------------------------------------------------------------
    // Port queue — `driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq[v]`
    //
    // These read and write the port's **driver queue** ([`DriverState::ioq`], the C's `prt->ioq`),
    // which is a different object from the port's `DriverContext::output()` queue that
    // [`AbiHost::output`]'s counterpart feeds: the C never puts Erlang command bytes in
    // `prt->ioq` and never delivers `prt->ioq` to `outputv` (see [`DriverState::ioq`] and
    // [`AbiHost::enq`]).
    //
    // Unlike `driver_select`/`driver_output`, none of these needs an installed `DriverContext`:
    // the queue is this host's own state, so a driver reaches it wherever it holds a live
    // `ErlDrvPort`, exactly as the C's `drvport2ioq(ix)` does. The crate's single-thread invariant
    // still applies — drive them from the port's thread (the C lock-protects the ioq for driver
    // worker threads; this floor does not).
    // -------------------------------------------------------------------------------------------

    /// `driver_enq`/`driver_enq_bin`: append `data` to the port's driver queue.
    ///
    /// **Why not the port's `DriverContext::output()` queue** (the obvious-looking mapping): that
    /// queue is the Erlang→driver channel, and [`DriverPort::write_driver`] hands whatever it holds
    /// to the entry's `outputv`/`output` slot on the next `OUTPUT` task. A driver that enqueued
    /// there — which is exactly what the reference user does, `driver_enqv(ix, ev, n)` in
    /// `inet_drv`'s `tcp_sendv` (`inet_drv.c:13842`) and `driver_enq(ix, buf, h_len)` in its plain
    /// path (`:13881-13886`) — would get its own bytes back as a **new port command**, so
    /// `tcp_sendv` (which prepends the `{packet,N}` length header to every command it is handed,
    /// `:13756-13770`) would frame those bytes a second time: silently corrupt output. The C has
    /// no such path (an `OUTPUT` task runs only `ready_output`, `erl_port_task.c:1790-1797`), so
    /// the driver queue is kept separate here.
    ///
    /// **Deviation from the C:** the queue is one contiguous [`ByteQueue`], so the C's
    /// one-iovec-entry-per-enqueue chunking is not reproduced — [`DriverHost::peekq`] therefore
    /// always reports a single entry (`vlen == 1`) where the oracle counts the enqueues (two
    /// `driver_enq` calls → `*vlen == 2`, `beam_abi`'s `driver_peekq` docs). Byte **order** and
    /// byte **count** are identical either way, which is all the family's consumers rely on.
    ///
    /// `is_binary` is not consulted: the seam hands the bytes over as a slice, so the C's
    /// binary/plain distinction (its `ErlDrvBinary` refcount dance, `io.c:6860-6866`) has no
    /// analogue. `0` = queued (the C's `-1` is an allocation failure, which a `Vec` cannot report).
    fn enq(&mut self, data: &[u8], _is_binary: bool) -> c_int {
        self.state.ioq.push(data);
        0
    }

    /// `driver_pushq`/`driver_pushq_bin`: prepend `data` to the head of the port's driver queue.
    ///
    /// The C's `erts_ioq_pushqv` splices the new elements in front of the queue (`io.c:6800`).
    /// [`ByteQueue`] appends only, so the queued bytes are moved behind `data` (order preserved).
    /// `is_binary` as for [`DriverHost::enq`].
    fn pushq(&mut self, data: &[u8], _is_binary: bool) -> c_int {
        if data.is_empty() {
            return 0; // The C's `len == 0` early return (`io.c:6881`).
        }
        let tail = self.state.ioq.front().to_vec();
        self.state.ioq.clear();
        self.state.ioq.push(data);
        self.state.ioq.push(&tail);
        0
    }

    /// `driver_deq`: drop up to `len` bytes from the head of the port's driver queue.
    ///
    /// Returns how many bytes were **dropped**, which is the value `beam_abi`'s `driver_deq` needs:
    /// it re-reads [`DriverHost::sizeq`] afterwards and returns the C's *remaining* count
    /// (`queues.rs::driver_deq`, `io.c:6807-6813`). A negative `len` — impossible through the C API,
    /// whose operand is an `ErlDrvSizeT` — is refused with `-1`.
    ///
    /// **Deviation from the C:** `erts_ioq_deq` fails (`-1`) when `size > q->size` and leaves the
    /// queue untouched (`erl_io_queue.c:351`), whereas this clamps (`ByteQueue::consume`). The
    /// difference is unreachable through `driver_deq`, which takes that branch itself by comparing
    /// against [`DriverHost::sizeq`] first; clamping is the safe direction for a direct host call,
    /// because the queue is then left empty rather than silently holding bytes the caller believes
    /// it dropped.
    fn deq(&mut self, len: c_int) -> c_int {
        if len < 0 {
            return -1;
        }
        let dropped = self.state.ioq.consume(len as usize);
        dropped.min(c_int::MAX as usize) as c_int
    }

    /// `driver_sizeq`: how many bytes are on the port's driver queue.
    ///
    /// Never negative while the host is alive — the C's `(ErlDrvSizeT)-1` is its null-queue case
    /// (`io.c:6827-6834`), and `beam_abi` relies on a non-negative answer to let `driver_deq` and
    /// `driver_peekqv` take their normal path.
    fn sizeq(&mut self) -> c_int {
        self.state.ioq.len().min(c_int::MAX as usize) as c_int
    }

    /// `driver_peekq`/`driver_peekqv`: a view of the head of the port's driver queue.
    ///
    /// One entry over [`ByteQueue::front`] — the whole contiguous queue; see
    /// [`DriverHost::enq`] for the `vlen == 1` deviation from the C's per-enqueue chunking.
    ///
    /// `None` for an empty queue, which `beam_abi` renders as the C's `NULL` with `*vlen = 0`
    /// (it tells "empty" and "unsupported" apart through [`DriverHost::sizeq`], and this host
    /// always supports the queue).
    ///
    /// The `iov_base` addresses the queue's own buffer, so it stays valid until the next
    /// `driver_enq`/`driver_pushq`/`driver_deq` — the C's lifetime for the pointer it returns —
    /// while `beam_abi` copies the vector of iovecs itself into the driver's call for the callback's
    /// duration.
    fn peekq(&mut self) -> Option<Vec<SysIOVec>> {
        let front = self.state.ioq.front();
        if front.is_empty() {
            return None;
        }
        Some(vec![SysIOVec {
            iov_base: front.as_ptr() as *mut c_void,
            iov_len: front.len(),
        }])
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
    ///
    /// **The fd record is dropped only after the poller registration is really gone.** The record
    /// (and with it the `ERL_DRV_USE` release obligation) is what a later de-select or the port
    /// teardown walks, so withdrawing it on a **failed** `deregister` would lose the driver's
    /// `stop_select` for an fd the poller still holds — the release must stay queued, not vanish.
    /// A failed de-select answers `-1` and leaves the registration exactly as it was.
    fn select(&mut self, _port: ErlDrvPort, event: ErlDrvEvent, mode: c_int, on: c_int) -> c_int {
        let fd = event as usize as RawFd;
        let interest = interest_from_mode(mode);

        if on == 0 {
            let Some(selected) = self.state.selected.get(&fd) else {
                return 0; // De-selecting an unknown fd is a no-op, not a failure.
            };
            let token = selected.token;
            let owes_stop_select = selected.wants_stop_select();
            return match self.with_ctx(|ctx| ctx.deregister(fd, token)) {
                Some(Ok(())) => {
                    self.state.selected.remove(&fd);
                    if owes_stop_select {
                        // Queued *after* the context borrow ends; the port fires it once this
                        // callback has returned (see `DriverState::queued_stop_select`).
                        self.state.queued_stop_select.push(event);
                    }
                    0
                }
                // The poller still holds the registration (or no context is installed): keep the
                // record and its release obligation, so a later de-select or the teardown can
                // still withdraw it and fire the callback exactly once.
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
#[path = "host_tests.rs"]
mod tests;
