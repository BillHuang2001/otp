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

//! The **driver C ABI host** (contract **A.4**, phase 1): load a real C `.so` driver and drive it.
//!
//! `beam_abi::driver` freezes the *shape* of the driver ABI — the `ErlDrvEntry` struct, the
//! `driver_*` / `erl_drv_*` function table ([`beam_abi::driver::api::ErlDrvApi`]) and the
//! `#[no_mangle]` host functions ([`beam_abi::driver::drv`]) — and owns the `dlopen` loader
//! ([`beam_abi::driver::loader`]). Those host functions cannot do their job alone:
//! `driver_output` must hand its bytes to *this* runtime, `erl_drv_output_term` must build its
//! term on *this* runtime's heap and intern its atoms in *this* runtime's table, and
//! `driver_connected` must answer with *this* runtime's pid. [`DriverPort`] is that host — the
//! concrete [`beam_abi::DriverHost`] behind the opaque `*mut ErlDrvPort_` a driver callback
//! receives.
//!
//! # The port handle is the environment pointer
//!
//! [`beam_abi::DriverEnv`] is the per-port environment; its `as_port` pointer **is** the
//! `ErlDrvPort` a driver callback gets, and `beam_abi`'s host functions recover it with
//! `DriverEnv::from_port`. `DriverEnv::new` erases the borrow of the host into a raw pointer (a
//! documented `mem::transmute` inside `beam_abi`), so the caller must keep the host alive **and in
//! place** for as long as the environment (and every port handle derived from it) is used.
//! [`DriverPortAdapter::start`] is the one place that invariant is stated for this crate: the host
//! lives in a `Box` that never moves, and the environment is itself `Box`ed **before** its handle is
//! taken, because the handle *is* the environment's own address — a driver may keep it in its
//! private state for the port's whole life (the A.4 fixture does exactly that).
//!
//! # What phase 1 proves
//!
//! A real C driver `.so`, compiled against the system `erl_driver.h`, is `dlopen`ed, validated and
//! driven: the driver's `start` callback allocates its private state through the runtime's
//! `driver_alloc`, its `output` callback echoes through `driver_output`, and the bytes land in
//! [`DriverPort::outputs`]. [`DriverPortAdapter`] then routes those bytes end to end: a
//! `{spawn_driver, Name}` port queued from Erlang reaches the driver's `output` callback through
//! `port_command`, and the echo comes back to the owner as `{Port, {data, Data}}`. The driver's
//! `driver_*` imports are left **undefined** by the linker and
//! bind at `dlopen(RTLD_NOW)` out of the loading process's **dynamic symbol table**
//! (`build.rs`'s `-Wl,--export-dynamic`, anchored by
//! [`PortRegistry::new`](crate::port::PortRegistry::new)'s `black_box` reference to
//! [`beam_abi::driver::drv::driver_api_table`]). `tests/driver.rs` pins the mechanism with
//! `nm -D` / `readelf --dyn-syms` and the end-to-end delivery.
//!
//! # The event and timer surface (phase 2)
//!
//! A driver's own event and timer requests are **real**, not captured:
//!
//! * `driver_select(port, (ErlDrvEvent)fd, mode, on)` registers `fd` with the runtime's **one**
//!   [`beam_io::Poller`] — the same poll set every port's descriptors live in — through the port's
//!   [`beam_io::DriverContext`]. Readiness then schedules the port's `INPUT`/`OUTPUT` task, which
//!   runs the driver's own `ready_input`/`ready_output` callback with the very descriptor the C
//!   passes there (`erl_check_io.c:1886`: `(ErlDrvEvent) state->fd`).
//! * `driver_set_timer(port, ms)` arms the port's `TIMEOUT` task by declaring a deadline through
//!   [`beam_io::Driver::next_timeout`], which the port mirrors after **every** driver interaction
//!   (`beam_io::Port::refresh_timeout`). The runtime's poll round is clamped by that deadline
//!   (`beam_io::PortRegistry::next_deadline`), so the driver's `timeout` callback fires **at** its
//!   deadline. `driver_read_timer` reports the remaining milliseconds and `driver_cancel_timer`
//!   drops the deadline — with the C's own return codes. The timer is armed for **every** live port:
//!   an entry with no `timeout` callback gets the C's silent `no_timeout_callback`
//!   (`io.c:7601`), so `driver_set_timer` answers `0` there too, and the fired deadline is a no-op.
//!
//! The host function a C driver calls has no way to reach the port lifecycle that owns the poller
//! (the `DriverHost` seam is frozen and the opaque `ErlDrvPort` is the *only* thing a callback
//! receives), so the two share one [`DriverControl`]: a callback records what it asked for and the
//! adapter applies it as soon as the callback returns — exactly the pattern
//! [`DriverPort::take_outputs`] already uses for `driver_output` bytes. A host that no port
//! lifecycle has claimed (a bare [`Driver`] driven by a test) therefore answers the C's failure
//! code rather than recording a request nobody would ever apply.
//!
//! # Driver-emitted terms (`erl_drv_output_term` / `erl_drv_send_term`)
//!
//! A driver hands a term to the runtime through the term-data stack machine
//! (`ERL_DRV_ATOM`/`ERL_DRV_TUPLE`/…), and `beam_abi` decodes that spec into a real term on this
//! host's arena before calling [`DriverHost::send_term`] with the destination (`None` = the port's
//! connected process, `erl_drv_output_term`, `io.c:6254`; `Some(pid)` = `erl_drv_send_term`,
//! `io.c:6289`). The C *delivers* it there and then: it builds the term straight on the receiving
//! process's heap and queues the message from inside the callback (`driver_deliver_term`,
//! `io.c:5664`). The frozen [`DriverHost`] seam gives a host no way to reach a receiver's mailbox
//! (it has the owning pid and nothing else), so the term is **queued** instead:
//! [`DriverHost::send_term`] pushes `(destination, term)` onto the port's [`TermQueue`], and
//! whoever holds that queue — the runtime's port pump, which owns the process table — drains it
//! and delivers each entry's term to its destination.
//!
//! One spec operand is not self-contained: `ERL_DRV_EXT2TERM` carries an external-term *image* the
//! runtime has to decode. `beam_abi` owns no ETF decoder, so it calls
//! [`DriverHost::binary_to_term`], which [`DriverPort`] implements over
//! [`beam_codec::decode_term`] — the same acceptance rule the C's `binary2term_prepare`
//! (`external.c:1745`) applies (a leading `VERSION_MAGIC`, plus the `COMPRESSED` wrapper). A
//! malformed image aborts the whole decode (`ERTS_DDT_FAIL`), never a partial term.//!
//! The queue is per port and shared by handle between the host (which pushes, from inside a
//! callback) and the runtime (which takes the handle before the port is registered and drains it).
//! Its one invariant is a lifetime one: a term's words live on [`DriverPort`]'s arena, so they may
//! only be read while the driver is alive. [`DriverPortAdapter`] therefore **empties** the queue
//! when it tears the driver down (from [`beam_io::Driver::stop`], where the C's own
//! `ERTS_PORT_SFLG_CLOSING` check refuses a delivery — `deliver_term_check_port`, `io.c:6213` — and
//! from its `Drop`, for a port whose `stop` never ran), so a late drain observes an empty queue
//! rather than a term built on a heap that has gone away.
//!
//! # Firing `process_exit` (the driver-monitor drive point)
//!
//! A driver's watches on Erlang processes are installed through the three [`DriverHost`] monitor
//! hooks and kept in a per-port tree ([`DriverMonitors`]) the port lifecycle seizes from the host at
//! [`DriverPortAdapter::load`]. `ErlDrvEntry::process_exit` has **no** port-task kind in this
//! runtime, so it rides the per-round drive point `beam_io`'s port-task round already runs for a
//! driver whose `has_pending_async` answers `true`: [`DriverPortAdapter::has_pending_async`] reports
//! the tree, and [`DriverPortAdapter::drive_async`] fires one callback per monitor taken from it
//! (taking it removes it, as the C does when a monitor fires). That hook is named for `driver_async`
//! — which stays a typed gap, see below — but it is the one per-round driver entry point this
//! runtime has, and both are "work that must be handed to the driver between rounds". The teardown
//! guards are [`beam_io::Driver::stop`] and the `Drop` impl, each of which empties the tree before
//! the library is unloaded. What marks a monitor pending is the *runtime's* observation of a process
//! exit (a higher scope's obligation): the tree never fires anything on its own.
//!
//! # The liveness view behind `driver_monitor_process` (the C's `1`)
//!
//! The C's `driver_monitor_process` has three outcomes, and the **positive** one — `1`,
//! `ERL_DRV_MONITOR_NOT_ALIVE` — is reached when the target process is gone
//! (`do_driver_monitor_process`, `io.c:7012-7038`). This host holds no live process table (it is
//! built before its port is registered and is reached only through an opaque `ErlDrvPort`), so the
//! runtime **publishes** its most recent observation into a shared [`ProcessLiveness`] handle that
//! [`RuntimePorts::open_port`](crate::port::RuntimePorts::open_port) installs on the host
//! ([`DriverPort::with_liveness`]) and refreshes at the start of every port round. The hook
//! [`DriverPort::monitor_process_result`] reads it, and the observation's freshness rule — never
//! older than the last round, `None` before the first — is spelled out there.
//!
//! # What is NOT wired yet (the typed gaps)
//!
//! * **`driver_async` / `driver_async_cancel`.** `Runtime` is `!Send` (it owns `Rc`/`RefCell`
//!   tables, a `dlopen`ed library registry and pointer-stable term arenas), so a worker thread
//!   cannot call back into it and there is no async pool to schedule on. The C's *in-band* path
//!   (no pool ⇒ invoke in the caller, then `ready_async`, then `async_free` unless `ready_async`
//!   is present — `erl_async.c:604-613`) needs the driver's private state and entry, which the
//!   frozen [`DriverHost`] seam does not give the host. It is reported as the C's own failure code
//!   (`-1`) rather than faked.
//! * **`driver_control`'s reply *source*.** The entry's `control` callback behind `port_control/3`
//!   **is** routed ([`DriverPortAdapter::control`]), but the C lets a driver answer from a buffer it
//!   allocated itself (`*resp_bufp = driver_alloc(…)`, `io.c:4141-4168`), and reading that raw
//!   pointer is precisely the raw-memory access this crate's `#![deny(unsafe_code)]` forbids. A
//!   reply the driver writes into the buffer it was handed (and the C's `NIL`, a `NULL` reply
//!   pointer) is served; a reply from the driver's own memory is the typed *unsupported* gap — never
//!   a fabricated or truncated reply. Closing it needs a **safe** copy helper on `beam_abi`'s
//!   [`DriverLibrary`] (recorded in `port/CONTEXT.md`'s Handoffs).
//! * **A queued term carries no sending port.** The C stamps the port into the message header
//!   (`from = prt->common.id`, `io.c:6172`), which is what send-tracing shows; a [`TermQueue`]
//!   entry is `(destination, term)` only, so the runtime that drains it delivers the term itself
//!   (`erl_drv_output_term` sends the decoded term, never a `{Port, Term}` wrapper) with no
//!   sending-port identity.
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use libc::{c_int, c_uint, c_ulong};

use beam_abi::driver::loader::{self, DriverLibrary, DriverLoadError};
use beam_abi::driver::types::{
    ErlDrvBinary, ErlDrvEntry, ErlDrvEvent, ErlDrvPort, ErlDrvSizeT, ErlIOVec,
    ERTS_INVALID_ERL_DRV_PORT, ERL_DRV_READ, ERL_DRV_USE, ERL_DRV_USE_NO_CALLBACK, ERL_DRV_WRITE,
};
use beam_abi::driver::types::ERL_DRV_FLAG_SOFT_BUSY;
use beam_abi::driver::types::ErlDrvPDL;
use beam_abi::{
    monitor_from_identity, monitor_identity, DriverEnv, DriverHost, ErlDrvMonitor, MonitorResult,
    SysIOVec,
};
/// The per-port driver-private state the host owns: the C's driver-private byte queue
/// (`prt->ioq`), the port **state** stream flags `driver_get_port_flags` reports, and the port's
/// one-PDL rule (`driver_pdl_create`).
mod state;

/// The per-port **driver monitor tree** — the state behind the three [`DriverHost`] monitor hooks
/// (`driver_monitor_process`/`driver_demonitor_process`/`driver_get_monitored_process`, contract
/// A.4). A private module like [`state`], re-exported because the runtime seizes the shared handle
/// from the host before it is boxed into the port lifecycle, for its later `process_exit` drain.
mod monitor;

use state::DriverQueue;
pub use monitor::DriverMonitors;
use beam_common::BeamError;
use beam_alloc::Heap;
use beam_io::{DriverContext, Interest, Token};
use beam_process::ProcessId;
use beam_term::{make_atom, refc::ERL_ONHEAP_BINARY_LIMIT, Atom, AtomTable, Eterm};

use super::ProcessLiveness;
/// The atoms a driver first interned inside a callback, awaiting the port layer's commit.
///
/// An [`AtomTable`] is **append-only** and its indices never renumber, so a name the live table
/// already holds keeps its index — but a *new* name can only be **predicted** while a driver
/// callback runs: the runtime's one live table is mutably borrowed at the commit site, and this
/// crate is `#![deny(unsafe_code)]`, so a host cannot mint an atom inline. This is the same
/// deferred-commit rule the BIF/NIF hosts use (`crate::context::PendingInterner`), with the two
/// things a **long-lived** host needs and a per-call one does not:
///
/// * **re-rooting** ([`DriverAtoms::rebase`]) — a driver host outlives the BIF call that opened
///   its port, so other code may have interned names since; the port layer re-bases the
///   prediction on the live table's length (`DriverAtoms::len`) at a step boundary it controls
///   (see [`crate::port::PortRegistry::rebase_driver_atoms`]);
/// * a **verified** commit ([`DriverAtoms::commit`]) — a name is interned only when it lands at
///   exactly the index the driver was handed. A prediction the table has moved past (something
///   else interned in between, or the name already exists elsewhere) is *dropped and returned to
///   the caller*, never interned at a different index — which would make the driver's already
///   built term name the **wrong** atom.
///
/// The handle is shared by `Rc<RefCell<..>>` between the host (which predicts, from inside a
/// callback) and the port layer (which re-roots and commits), exactly like [`TermQueue`].
#[derive(Clone, Debug)]
pub struct DriverAtoms {
    /// The shared prediction state: the live table's length when the window was opened, and the
    /// names recorded since, in order.
    inner: Rc<RefCell<DriverAtomsState>>,
}

/// The mutable half of a [`DriverAtoms`].
#[derive(Debug)]
struct DriverAtomsState {
    /// The index the *first* recorded name was predicted at (the live table's length when the
    /// window was opened).
    base: usize,
    /// The names recorded in this window, in order (deduplicated).
    names: Vec<String>,
}

impl DriverAtoms {
    /// A pending set rooted at `base` (the live table's current length).
    pub fn new(base: usize) -> DriverAtoms {
        DriverAtoms {
            inner: Rc::new(RefCell::new(DriverAtomsState {
                base,
                names: Vec::new(),
            })),
        }
    }

    /// Whether no name is awaiting a commit.
    ///
    /// Load-bearing as a **guard**: committing nothing must not even *reach* the shared table
    /// through `Rc::make_mut`, because that would deep-clone the whole table (thousands of
    /// entries after boot) for nothing.
    pub fn is_empty(&self) -> bool {
        self.names().is_empty()
    }

    /// The names recorded since the last commit, in order — the caller's report of what a commit
    /// dropped.
    fn names(&self) -> Vec<String> {
        self.inner.borrow().names.clone()
    }

    /// The index `name` will be interned at: the live table's index when it already holds the
    /// name, the index it was already predicted at, or the next free predicted index.
    fn intern(&self, name: &str) -> Atom {
        let mut state = self.inner.borrow_mut();
        if let Some(offset) = state.names.iter().position(|pending| pending == name) {
            // A duplicate within this window reuses the predicted index.
            return Atom::from_raw((state.base + offset) as u32);
        }
        let index = state.base + state.names.len();
        state.names.push(name.to_string());
        Atom::from_raw(index as u32)
    }

    /// Re-roots the prediction on the live table's length `len` — the port layer's window opening.
    ///
    /// A no-op while names are still pending: the already handed-out indices are anchored at the
    /// current base, so moving it would invalidate them (the caller commits first — see
    /// [`crate::port::PortRegistry::rebase_driver_atoms`]).
    pub fn rebase(&self, len: usize) {
        let mut state = self.inner.borrow_mut();
        if state.names.is_empty() {
            state.base = len;
        }
    }

    /// Interns the recorded names into `atoms`, in prediction order, and returns the names it had
    /// to **drop** — the ones whose predicted index the live table has already moved past, so a
    /// different name sits where the index the driver was handed points.
    ///
    /// A drop is never silent: the caller reports it (see
    /// [`crate::port::PortRegistry::dropped_driver_atoms`]). Interning such a name anyway would
    /// append it at a *different* index than the driver's term carries, which is a wrong atom
    /// behind a valid-looking index — strictly worse than a prediction that names nothing
    /// (`beam_abi`'s term-data machine reports the failure to the driver).
    ///
    /// The base is re-rooted on `atoms.len()` afterwards, so the next window predicts from the
    /// table's real end.
    pub fn commit(&self, atoms: &mut AtomTable) -> Vec<String> {
        let names = {
            let mut state = self.inner.borrow_mut();
            std::mem::take(&mut state.names)
        };
        let base = self.inner.borrow().base;
        let mut dropped = Vec::new();
        for (offset, name) in names.into_iter().enumerate() {
            let predicted = base + offset;
            if atoms.len() != predicted {
                dropped.push(name);
                continue;
            }
            atoms.intern(&name);
        }
        self.inner.borrow_mut().base = atoms.len();
        dropped
    }
}

impl beam_bifs::AtomInterner for DriverAtoms {
    /// Records `name` (or resolves the index it was already predicted at), for
    /// [`DriverAtoms::commit`] to land.
    fn intern(&mut self, name: &str) -> Atom {
        DriverAtoms::intern(self, name)
    }
}
/// One `driver_select` request a driver callback made through [`DriverHost::select`].
///
/// The C acts on it immediately; here it is *recorded* and applied by the port lifecycle as soon
/// as the callback returns (see the module docs). `fd` is the `ErlDrvEvent` the C passes — the
/// descriptor itself, exactly as `erl_check_io.c` reads it back out of the event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SelectRequest {
    /// The descriptor the driver selected (or deselected).
    fd: c_int,
    /// The `ERL_DRV_READ`/`ERL_DRV_WRITE`/`ERL_DRV_USE` mode bits, verbatim.
    mode: c_int,
    /// Non-zero to select, zero to deselect (`driver_select`'s `on` argument).
    on: c_int,
}

/// The armed readiness directions of one registration, per descriptor.
///
/// [`Interest`] has no "nothing armed" value, so the two directions are tracked as plain booleans
/// and converted to an [`Interest`] only when something is (still) armed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Armed {
    /// Whether the descriptor is armed for readability.
    read: bool,
    /// Whether the descriptor is armed for writability.
    write: bool,
}

impl Armed {
    /// The `Interest` mask for the armed directions, or `None` when both were deselected.
    fn interest(self) -> Option<Interest> {
        match (self.read, self.write) {
            (true, true) => Some(Interest::BOTH),
            (true, false) => Some(Interest::READABLE),
            (false, true) => Some(Interest::WRITABLE),
            (false, false) => None,
        }
    }

    /// This set with `mode`'s directions turned on (`on`) or off.
    fn with(mut self, mode: c_int, on: bool) -> Armed {
        for (bit, side) in [
            (ERL_DRV_READ, &mut self.read),
            (ERL_DRV_WRITE, &mut self.write),
        ] {
            if mode & bit != 0 {
                *side = on;
            }
        }
        self
    }
}

/// The per-port state a [`DriverPort`] host and its [`DriverPortAdapter`] lifecycle share.
///
/// One host is one port, so there is exactly one of these per port and no identity matching is
/// needed: a request recorded here belongs to this port by construction. The `Rc<RefCell<..>>` is
/// the channel (the host function has no other way to reach the lifecycle), which is why neither
/// side may hold a borrow **across** a driver callback — the callback is what calls back in.
#[derive(Debug, Default)]
struct DriverControl {
    /// Whether a port lifecycle claimed this host ([`DriverPortAdapter::load`]). Until then a
    /// select/timer request has nobody to apply it and is refused with the C's failure code.
    attached: bool,
    /// Whether the port's driver was `dlopen`ed — the C's `prt->drv_ptr->handle != NULL`
    /// (`io.c:7337`), which is what decides `driver_lock_driver`.
    ///
    /// A bare host nobody claimed ([`DriverPort::new`]) and a host over a **statically linked**
    /// entry both leave this `false`, so `driver_lock_driver` answers the C's `-1` for exactly the
    /// statically linked case.
    driver_loaded: bool,
    /// The deadline the port's timer is currently armed at (`None` = no timer pending).
    deadline: Option<Instant>,
    /// The `driver_select` requests recorded since the lifecycle last applied them.
    selects: Vec<SelectRequest>,
}

/// One loaded driver and the host its callbacks run against.
///
/// The field order is deliberate, mirroring `crate::nif`'s `NifEntry`: the loaded library drops
/// **before** the host, so the driver's `finish` callback (run from [`DriverLibrary`]'s `Drop`)
/// still sees a live host if it calls a `driver_*` function.
pub struct Driver {
    /// The `dlopen`ed library (its `Drop` runs `finish` then `dlclose`s).
    library: DriverLibrary,
    /// The host every callback of this driver draws its runtime services from.
    ///
    /// Boxed so the [`DriverEnv`] pointer handed to the driver stays valid: the host must not move
    /// while a callback holds a port handle derived from it.
    host: Box<DriverPort>,
}

impl Driver {
    /// Pairs `library` with the `host` its callbacks will run against.
    pub fn new(library: DriverLibrary, host: DriverPort) -> Driver {
        Driver {
            library,
            host: Box::new(host),
        }
    }

    /// The loaded library (the `ErlDrvEntry` plus its `start`/`output`/`control`/`stop` wrappers).
    pub fn library(&self) -> &DriverLibrary {
        &self.library
    }

    /// The host shared by every callback of this driver, for the driver's whole lifetime.
    pub fn host(&self) -> &DriverPort {
        &self.host
    }

    /// The host, mutably (the caller building the environment).
    pub fn host_mut(&mut self) -> &mut DriverPort {
        &mut self.host
    }

    /// The driver name the entry declares (`ErlDrvEntry::driver_name`).
    pub fn name(&self) -> String {
        self.library.name()
    }
}

/// The terms a driver emitted through `erl_drv_output_term` / `erl_drv_send_term`, awaiting
/// delivery to a process mailbox.
///
/// One queue belongs to one port and is shared by handle between the port's [`DriverPort`] host
/// (which pushes, from inside a driver callback) and its reader: the runtime's port pump, which
/// holds the process table and can turn an entry into a message. The C instead delivers the term
/// synchronously from inside the callback (`driver_deliver_term`, `io.c:5664`), which a
/// [`DriverHost`] cannot do — the frozen seam hands it the owning pid and nothing else (see the
/// module docs).
///
/// The entry shape is the C's: `destination` is `None` for `erl_drv_output_term` (the port's
/// connected process) and `Some(pid)` for `erl_drv_send_term`'s explicit destination. Terms are
/// **not** wrapped in `{Port, Term}` — the decoded term itself is what the C queues
/// (`io.c:6174`).
///
/// # Lifetime contract
///
/// A queued term's words live on the port host's arena ([`DriverPort::heap`]), so an entry may only
/// be read while the driver is alive. [`DriverPortAdapter`] empties the queue when it unloads the
/// driver, so a reader that drains after the port went away observes an empty queue, never a
/// dangling term.
#[derive(Clone, Debug, Default)]
pub struct TermQueue {
    /// The queued `(destination, term)` pairs, in emission order.
    pending: Rc<RefCell<Vec<(Option<Eterm>, Eterm)>>>,
}

impl TermQueue {
    /// An empty queue.
    pub fn new() -> TermQueue {
        TermQueue::default()
    }

    /// Takes every queued term out, in emission order — the delivery side's drain.
    ///
    /// # Panics
    ///
    /// Never: a driver callback does not hold a borrow of this cell across a call (the pushing
    /// side borrows it only for the length of one `push`).
    pub fn take(&self) -> Vec<(Option<Eterm>, Eterm)> {
        std::mem::take(&mut *self.pending.borrow_mut())
    }

    /// Whether nothing is awaiting delivery.
    pub fn is_empty(&self) -> bool {
        self.pending.borrow().is_empty()
    }

    /// Queues `term` for `destination` (the host's push, from inside a driver callback).
    ///
    /// A re-entrant push while the cell is already borrowed cannot happen (a driver callback never
    /// holds the queue's borrow) and, if it ever did, dropping this one term is the honest outcome
    /// — never a panic and never a term delivered twice.
    fn push(&self, destination: Option<Eterm>, term: Eterm) {
        if let Ok(mut pending) = self.pending.try_borrow_mut() {
            pending.push((destination, term));
        }
    }

    /// Drops everything queued: the driver that built these terms is being unloaded, so its arena
    /// is about to disappear (see the lifetime contract above).
    fn clear(&self) {
        if let Ok(mut pending) = self.pending.try_borrow_mut() {
            pending.clear();
        }
    }
}

/// The **busy state** of one driver port — the C's `prt->sched.flags & ERTS_PTS_FLG_BUSY_PORT`,
/// as the driver's own `set_busy_port` sets it (`io.c:5208-5264`).
///
/// Shared by [`Rc`] between the [`DriverPort`] host (which [`DriverHost::set_busy_port`] writes,
/// from inside a driver callback) and the runtime's port metadata, which
/// [`crate::port::PortRegistry::write_command`] and `erlang:port_command/2,3` read back. Seized
/// with [`DriverPort::busy_flag`] **before** the host is boxed into the port lifecycle (the boxed
/// driver exposes no host), exactly like [`DriverPort::control_flags`].
///
/// The handle also carries the *other* half of the C's `port_command` decision: whether the driver
/// **entry** advertises `ERL_DRV_FLAG_SOFT_BUSY`. The C tests `prt->drv_ptr->flags` for it, which
/// is a property of the loaded `ErlDrvEntry`, not of the per-port state — but the entry is only
/// known *after* the `dlopen` that happens inside [`DriverPortAdapter::load`], and the host is
/// consumed by that same call. Recording it here (a [`Cell`] written by `load` through the very
/// handle the runtime already holds) is what lets the runtime consult it later without re-reading
/// the library.
#[derive(Clone, Debug)]
pub struct BusyFlag {
    /// Whether the port is currently busy — what `set_busy_port` toggles.
    busy: Rc<Cell<bool>>,
    /// Whether the loaded driver entry advertises `ERL_DRV_FLAG_SOFT_BUSY`.
    soft_busy: Rc<Cell<bool>>,
}

impl BusyFlag {
    /// A fresh, **idle**, non-`soft_busy` flag — the state of a port no driver has reported on.
    pub fn new() -> BusyFlag {
        BusyFlag {
            busy: Rc::new(Cell::new(false)),
            soft_busy: Rc::new(Cell::new(false)),
        }
    }

    /// Whether the port is busy (`set_busy_port(port, 1)` was the last request).
    pub fn is_busy(&self) -> bool {
        self.busy.get()
    }

    /// Marks the port busy or idle — the write half of `set_busy_port`.
    pub fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
    }

    /// Whether the loaded driver entry advertises `ERL_DRV_FLAG_SOFT_BUSY`.
    pub fn soft_busy(&self) -> bool {
        self.soft_busy.get()
    }

    /// Records whether the loaded entry advertises `ERL_DRV_FLAG_SOFT_BUSY`.
    pub fn set_soft_busy(&self, soft_busy: bool) {
        self.soft_busy.set(soft_busy);
    }
}

/// A driver host for one port: the runtime services the `driver_*` / `erl_drv_*` functions need.
///
/// It owns a private term arena (the heap the term-data decoder builds its terms on), holds a
/// handle to the runtime's **one shared** [`AtomTable`] plus the deferred-commit [`DriverAtoms`]
/// record a *new* atom is predicted on, and answers the port queries with the owning [`ProcessId`].
///
/// See the module docs for the current scope: the output / term-data / pid services and the
/// recorded `driver_select` / timer requests are implemented; `driver_async` and the custom
/// `control` keep the seam's typed defaults.
pub struct DriverPort {
    /// The term arena the term-data decoder allocates on.
    ///
    /// Runtime-lifetime and **never reset**: an `erl_drv_output_term` result is a term — a pointer —
    /// and the driver may hold it after the callback returns (the same rule as
    /// [`crate::port::PortRegistry`]'s `port_info` arena).
    heap: Heap,
    /// A handle to the runtime's shared atom table, used to resolve (never to mint *through*) an
    /// existing atom.
    atoms: Rc<AtomTable>,
    /// The deferred-commit record a **new** atom is predicted on (see
    /// [`DriverPort::commit_atoms`]).
    interner: DriverAtoms,
    /// The process the port belongs to — what `driver_connected` / `driver_caller` report.
    owner: ProcessId,
    /// Every `driver_output` / `erl_drv_output` payload, in call order.
    outputs: Vec<Vec<u8>>,
    /// Every decoded term handed to `send_term` (`erl_drv_output_term` / `erl_drv_send_term`),
    /// with the destination (`None` = the port owner).
    ///
    /// The accumulate-only view the A.4 integration test reads — the same split as
    /// [`DriverPort::outputs`] / [`DriverPort::take_outputs`]. Delivery goes through
    /// [`DriverPort::terms`] instead (see [`TermQueue`]).
    sent: Vec<(Option<Eterm>, Eterm)>,
    /// The port's delivery queue for those terms, shared with whoever drains it (see
    /// [`TermQueue`]).
    terms: TermQueue,
    /// The event/timer state shared with the port lifecycle that owns this host (see
    /// [`DriverControl`]).
    control: Rc<RefCell<DriverControl>>,
    /// The port's `PORT_CONTROL_FLAG_*` bit set, as the driver last set it through
    /// `set_port_control_flags` — the C's `prt->control_flags` (`io.c:5318`).
    ///
    /// Shared with the runtime's `PortMeta`, which reads it **after** a `control` callback to pick
    /// the reply's Erlang form (`write_port_control_result`, `io.c:4133-4175`): a list of byte
    /// integers normally, a **binary** when `PORT_CONTROL_FLAG_BINARY` is set. Seized with
    /// [`DriverPort::control_flags`] before the host is boxed into the port lifecycle.
    control_flags: Rc<Cell<c_int>>,
    /// The port's **busy state** — the C's `prt->sched.flags & ERTS_PTS_FLG_BUSY_PORT`, as the
    /// driver's own `set_busy_port` sets it (`io.c:5208-5264`).
    ///
    /// Shared with the runtime's `PortMeta`, which reads it (and the driver entry's
    /// `ERL_DRV_FLAG_SOFT_BUSY` bit, recorded on the very same handle) to answer
    /// `erlang:port_command/2,3` the way the C does: `nosuspend` on a busy port ⇒ `false`, `force`
    /// ⇒ `notsup` unless the entry advertises `SOFT_BUSY`, and a plain command on a busy port
    /// suspends the caller. Seized with [`DriverPort::busy_flag`] before the host is boxed into
    /// the port lifecycle, exactly like [`DriverPort::control_flags`].
    busy: BusyFlag,
    /// The driver's **private** byte queue — the C's `prt->ioq` (`io.c:6790-6919`), which
    /// `driver_enq`/`driver_pushq`/`driver_deq`/… reach and nothing else does. Distinct from the
    /// port's *input* queue (the bytes `driver_output` answers with) and from the Erlang→driver
    /// output queue `beam_io` owns (see [`state::DriverQueue`]).
    queue: DriverQueue,
    /// The port's **state** stream flags as `driver_get_port_flags` reports them — the C's
    /// `ERTS_PORT_SFLG_BINARY_IO`/`ERTS_PORT_SFLG_LINEBUF_IO` (`io.c:5325-5344`), derived from the
    /// `open_port/2` stream option (`binary` ⇒ `PORT_FLAG_BINARY`, otherwise `0`).
    ///
    /// **Not** the same thing as [`DriverPort::control_flags`]: the C reads `prt->control_flags`
    /// only for the `port_control` reply form (`io.c:5318`), and `driver_get_port_flags` reads the
    /// port *state* flags instead (`io.c:5338-5341`).
    port_flags: c_int,
    /// Whether this port already handed out its one PDL (`driver_pdl_create`) — the C's
    /// "a port admits exactly one PDL" rule (`io.c:6716-6787`), which `beam_abi`'s PDL registry
    /// enforces by handle address.
    pdl_created: bool,
    /// The monitors this port's driver holds on processes — the tree behind
    /// `driver_monitor_process`/`driver_demonitor_process`/`driver_get_monitored_process`
    /// (see [`DriverMonitors`]).
    ///
    /// Shared with the runtime's `PortMeta`, which seizes it with [`DriverPort::monitors`] before
    /// the host is boxed into the port lifecycle (the boxed driver exposes no host), exactly like
    /// [`DriverPort::term_queue`].
    monitors: Rc<DriverMonitors>,
    /// Whether the **loaded driver entry** declares an `ErlDrvEntry::process_exit` slot — the C's
    /// `dp->entry->process_exit != NULL` test, which gates `driver_monitor_process` (`io.c:6998`).
    ///
    /// It cannot be derived inside the host hook: the host is built before the library is loaded and
    /// the frozen [`DriverHost`] hook receives no entry, so the port lifecycle stamps it once the
    /// two first coexist ([`DriverPortAdapter::load`], the same place
    /// `beam_io`'s own host reads this field). A host nobody claimed keeps the C's "no slot" answer,
    /// which is what a bare host in a unit test gets.
    has_process_exit: bool,
    /// The runtime's most recent **process-liveness observation** — the view
    /// [`DriverHost::monitor_process_result`] answers the C's positive
    /// `ERL_DRV_MONITOR_NOT_ALIVE` from (see [`ProcessLiveness`]).
    ///
    /// This host holds no live `ProcessTable`: it is built before its port is registered and is
    /// reached only through an opaque `ErlDrvPort`, so the runtime *publishes* what it last
    /// observed and the hook reads that. A host nobody published a view to — a bare host in a unit
    /// test, or a port opened before any port round ran — keeps the viewless
    /// [`ProcessLiveness::new`], whose `is_alive` answers `None`.
    liveness: Rc<ProcessLiveness>,
}

impl DriverPort {
    /// A host over `heap`, resolving/interning atoms against `atoms`, running as `owner`.
    pub fn new(heap: Heap, atoms: Rc<AtomTable>, owner: ProcessId) -> DriverPort {
        let interner = DriverAtoms::new(atoms.len());
        DriverPort {
            heap,
            atoms,
            interner,
            owner,
            outputs: Vec::new(),
            sent: Vec::new(),
            terms: TermQueue::new(),
            control: Rc::new(RefCell::new(DriverControl::default())),
            control_flags: Rc::new(Cell::new(0)),
            // A port opened without a driver request is idle and its entry's `SOFT_BUSY` is
            // unknown until the library is loaded (`with_soft_busy`, from `DriverPortAdapter::load`).
            busy: BusyFlag::new(),
            queue: DriverQueue::new(),
            // A port opened without a stream option is the C's default `list` stream (`io.c:5338`).
            port_flags: 0,
            pdl_created: false,
            monitors: DriverMonitors::new(owner),
            has_process_exit: false,
            // No observation has been published yet: a host nobody claims answers "monitor anyway"
            // rather than fabricating a `NotAlive` (see [`ProcessLiveness`]).
            liveness: ProcessLiveness::new(),
        }
    }
    /// Records whether the loaded driver entry declares an [`ErlDrvEntry::process_exit`] slot (see
    /// [`DriverPort::has_process_exit`]).
    ///
    /// A builder step like [`DriverPort::with_port_flags`]: [`DriverPortAdapter::load`] is the one
    /// place the host and the loaded `ErlDrvEntry` first coexist, so it is the only caller. A host
    /// nobody claims keeps the C's "no `process_exit` slot" answer (`false`).
    pub fn with_has_process_exit(mut self, has_process_exit: bool) -> DriverPort {
        self.has_process_exit = has_process_exit;
        self
    }

    /// Installs the runtime's process-liveness view (see [`ProcessLiveness`]).
    ///
    /// A builder step like [`DriverPort::with_has_process_exit`] /
    /// [`DriverPort::with_port_flags`], because the handle can only be taken once the runtime that
    /// publishes it exists: [`RuntimePorts::open_port`](crate::port::RuntimePorts::open_port) is the
    /// one caller, and it takes the shared handle from the same
    /// [`PortRegistry`](crate::port::PortRegistry) the port is about to be registered in. A host
    /// nobody claims keeps the viewless default, which answers `None` — "no observation" — never a
    /// fabricated dead process.
    pub(crate) fn with_liveness(mut self, liveness: Rc<ProcessLiveness>) -> DriverPort {
        self.liveness = liveness;
        self
    }
    /// The port's driver-monitor tree — the handle the runtime seizes before the host is boxed into
    /// the port lifecycle and later drains to fire `ErlDrvEntry::process_exit` (see
    /// [`DriverMonitors`]).
    pub fn monitors(&self) -> Rc<DriverMonitors> {
        Rc::clone(&self.monitors)
    }

    /// Sets the port's **state** stream flags (see [`DriverPort::get_port_flags`]).
    ///
    /// The `open_port/2` provider computes them from the resolved stream option
    /// ([`crate::port::StreamType`]) right before the host is moved into the port lifecycle, which
    /// is why this is a builder step rather than a constructor argument: a host nobody claims keeps
    /// the C's `list` default (`0`).
    pub fn with_port_flags(mut self, port_flags: c_int) -> DriverPort {
        self.port_flags = port_flags;
        self
    }

    /// Drops the per-port driver-private state: the port (and the driver) is going away, so its
    /// queue must not outlive it.
    pub fn clear_driver_state(&mut self) {
        self.queue.clear();
    }

    /// The port's [`TermQueue`] — the handle a driver callback's `erl_drv_output_term` /
    /// `erl_drv_send_term` pushes onto and the one the port pump drains.
    ///
    /// Taken **before** the host is boxed into a port (the boxed driver is reached through
    /// [`beam_io::Driver`], which exposes no host), which is why the queue is a shared handle
    /// rather than a field the reader can borrow.
    pub fn term_queue(&self) -> TermQueue {
        self.terms.clone()
    }

    /// The predicted-name handle of this host — the one the port layer commits into the runtime's
    /// **live** atom table (see [`DriverPort::commit_atoms`]).
    ///
    /// Taken **before** the host is boxed into a port, for the same reason as
    /// [`DriverPort::term_queue`]: the boxed driver exposes no host, so without this handle a name
    /// a driver interned inside a callback would stay *predicted* forever — its index would never
    /// reach the live table (see [`DriverAtoms`]).
    pub fn pending_atoms(&self) -> DriverAtoms {
        self.interner.clone()
    }
    /// The event/timer state this host shares with its port lifecycle.
    ///
    /// [`DriverPortAdapter::load`] takes this handle so the driver's `driver_select` and
    /// `driver_set_timer` requests reach the poller and the port's `TIMEOUT` task (see the module
    /// docs). A host nobody claims keeps answering the C's failure code for both.
    fn control(&self) -> Rc<RefCell<DriverControl>> {
        Rc::clone(&self.control)
    }

    /// The port's control-flags cell, shared with the runtime — the handle `PortMeta` holds to
    /// render a `port_control` reply (see [`DriverPort::set_control_flags`]).
    ///
    /// Taken **before** the host is boxed into a port (the boxed driver exposes no host), exactly
    /// like [`DriverPort::term_queue`].
    pub fn control_flags(&self) -> Rc<Cell<c_int>> {
        Rc::clone(&self.control_flags)
    }
    /// The port's **busy state** handle, shared with the runtime — the C's
    /// `prt->sched.flags & ERTS_PTS_FLG_BUSY_PORT` (see [`BusyFlag`]).
    ///
    /// Taken **before** the host is boxed into a port (the boxed driver exposes no host), exactly
    /// like [`DriverPort::control_flags`]. The driver's `set_busy_port` writes it from inside a
    /// callback; the runtime reads it back to answer `erlang:port_command/2,3`.
    pub fn busy_flag(&self) -> BusyFlag {
        self.busy.clone()
    }
    /// Records whether the loaded driver entry advertises `ERL_DRV_FLAG_SOFT_BUSY` (see
    /// [`BusyFlag`]).
    ///
    /// A builder step like [`DriverPort::with_has_process_exit`]: [`DriverPortAdapter::load`] is the
    /// one place the host and the loaded `ErlDrvEntry` first coexist, so it is the only caller. The
    /// write lands on the handle the runtime already seized with [`DriverPort::busy_flag`] (shared
    /// by `Rc`), so the caller observes it without re-reading the library. A host nobody claims
    /// keeps the default (`false`): a driver that advertises nothing is not `SOFT_BUSY`.
    pub fn with_soft_busy(self, soft_busy: bool) -> DriverPort {
        self.busy.set_soft_busy(soft_busy);
        self
    }
    /// The process the port belongs to.
    pub fn owner(&self) -> ProcessId {
        self.owner
    }

    /// Every captured `driver_output` payload, in call order.
    pub fn outputs(&self) -> &[Vec<u8>] {
        &self.outputs
    }

    /// Every decoded term a `*_output_term` / `*_send_term` call delivered: `(destination, term)`.
    pub fn sent(&self) -> &[(Option<Eterm>, Eterm)] {
        &self.sent
    }

    /// Takes every captured `driver_output` / `erl_drv_output` payload out, in call order.
    ///
    /// The port lifecycle drains them after each driver interaction
    /// ([`DriverPortAdapter::output`]) and queues them as the port's input; the
    /// [`DriverPort::outputs`] accessor is the accumulate-only view the unit tests read.
    pub fn take_outputs(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outputs)
    }

    /// The name of `term` when it is an atom the shared table **already** holds.
    ///
    /// A name recorded on the pending interner is not resolvable until
    /// [`DriverPort::commit_atoms`] lands it, so this answers `None` for it — the honest answer,
    /// never a guessed name.
    pub fn atom_name(&self, term: Eterm) -> Option<&str> {
        if !beam_term::is_atom(term) {
            return None;
        }
        self.atoms.name(beam_term::atom_val(term))
    }

    /// Interns the names recorded by [`DriverHost::intern_atom`] into `atoms`, in order, and
    /// returns the names it had to **drop** (see [`DriverAtoms::commit`]).
    ///
    /// The same deferred-commit rule as the NIF host (`crate::nif`'s `NifHostImpl`): the runtime's
    /// atom table is held immutably while a driver callback runs, so a *new* name is given its
    /// predicted index straight away and only materialised **after** the borrow is released. The
    /// caller must pass the **same** table the host's `atoms` handle refers to.
    pub fn commit_atoms(&self, atoms: &mut AtomTable) -> Vec<String> {
        self.interner.commit(atoms)
    }

    /// Builds the one per-port environment for this driver.
    ///
    /// The returned [`DriverEnv`] must stay alive — and must not be moved — for as long as any
    /// `ErlDrvPort` derived from it is used (the `ErlDrvPort` **is** the environment's own address;
    /// see the module docs). One host is one port and the host is not shared, so exactly one
    /// environment is meaningful; nothing stops a second call, but the two would define the same
    /// port.
    pub fn environment(&mut self) -> DriverEnv {
        DriverEnv::new(self)
    }

    /// Takes the term arena back out (a host is consumed once its driver is unloaded).
    pub fn into_heap(self) -> Heap {
        self.heap
    }
}

/// One entry this runtime's driver-entry list knows about (the C's `erts_driver_t` node, reduced to
/// what the two entry-list slots observe).
#[derive(Clone, Copy)]
struct DriverEntryRecord {
    /// The `ErlDrvEntry` a C driver registered (compared by address only, exactly like the C's
    /// `dp->entry == drv`).
    entry: *mut ErlDrvEntry,
    /// Whether the entry belongs to a `dlopen`ed object (the C's `dp->handle != NULL`): the C
    /// refuses to remove such an entry (`io.c:7709-7713`).
    dynamic: bool,
}

thread_local! {
    /// This runtime's driver-entry list — the C's process-global `driver_list` as the two
    /// port-less `driver_*` slots observe it (`add_driver_entry`/`remove_driver_entry`,
    /// `io.c:7640`/`7698`).
    ///
    /// Thread-local rather than a process-global `static`: the records hold raw `*mut ErlDrvEntry`
    /// (not `Send`), and this runtime drives every port on one thread, so a thread-local is the
    /// correct scope — and it needs no `unsafe impl Send` (the crate is `#![deny(unsafe_code)]`),
    /// matching the thread-local publication `beam_abi` already uses for the current driver host.
    static DRIVER_ENTRIES: RefCell<Vec<DriverEntryRecord>> = const { RefCell::new(Vec::new()) };
}

/// One `dlopen`ed C driver this runtime knows about, keyed by its driver **name** — the runtime's
/// loaded-driver list.
///
/// Unlike [`DRIVER_ENTRIES`] (keyed by the `ErlDrvEntry` **address**, modelling only the two
/// port-less `driver_*` slots), this list is keyed by the driver *name*, because that is how the C
/// resolves an `open_port({spawn_driver, Name}, _)` request against its process-global driver list
/// (`io.c:599-628`) and how `erl_ddll:loaded_drivers/0` reports it. Recording every `dlopen` here
/// is what makes those two views **one** view: a driver is listed however it was opened.
#[derive(Debug)]
pub(crate) struct LoadedDriver {
    /// The driver's registered name (the entry's validated `driver_name`).
    pub(crate) name: String,
    /// The object path the driver was loaded from — the candidate a later `open_port` re-uses.
    pub(crate) path: String,
    /// Whether the `kill_ports` load option was given (`erl_ddll:try_load/3`); `false` for a
    /// driver reached through `open_port({spawn_driver, …})`, which has no such option.
    pub(crate) kill_ports: bool,
    /// The `dlopen` handle when **`erl_ddll:try_load/3`** performed the load — the record owns
    /// that handle (so dropping the record runs the driver's `finish` and `dlclose`s it, exactly
    /// like `beam_bifs`'s registry).
    ///
    /// `None` for a driver opened by `open_port({spawn_driver, …})`: its handle lives in the
    /// **port's** adapter, so forgetting the record must never unload it.
    // Held for its `Drop`: dropping the record runs the driver's `finish` and `dlclose`s the
    // object. The field is deliberately never *read* (the C's handle is opaque to the
    // runtime), which is exactly why the dead-code lint must not fire on it.
    #[allow(dead_code)]
    pub(crate) library: Option<DriverLibrary>,
}

thread_local! {
    /// The runtime's loaded-driver list, keyed by driver name, in load order (a `Vec`, so the
    /// names report load order like the C's list walk). Thread-local for the same reason as
    /// [`DRIVER_ENTRIES`]: the records hold the `!Send` `dlopen` handle and this crate is
    /// `#![deny(unsafe_code)]`, and the runtime drives every driver on one thread.
    static LOADED_DRIVERS: RefCell<Vec<LoadedDriver>> = const { RefCell::new(Vec::new()) };
}

/// Records `name` as loaded by **`erl_ddll:try_load/3`**, owning `library` (the load's `dlopen`
/// handle) unless an entry of the same name is already present.
///
/// Replacing an existing entry only happens when a caller deliberately re-registers; the loading
/// BIF checks [`driver_is_loaded`] first and answers `already_loaded`.
pub(crate) fn record_driver_loaded(name: &str, path: &str, kill_ports: bool, library: DriverLibrary) {
    LOADED_DRIVERS.with(|drivers| {
        let mut drivers = drivers.borrow_mut();
        let record = LoadedDriver {
            name: name.to_string(),
            path: path.to_string(),
            kill_ports,
            library: Some(library),
        };
        match drivers.iter_mut().find(|entry| entry.name == name) {
            Some(slot) => *slot = record,
            None => drivers.push(record),
        }
    });
}

/// Notes that `name` is open through **`open_port({spawn_driver, …})`** at `path`.
///
/// The handle belongs to the port, so the record stores no library; a `kill_ports` flag already
/// recorded by an earlier `try_load` is preserved.
pub(crate) fn note_driver_opened(name: &str, path: &str) {
    let kill_ports = loaded_driver_kill_ports(name).unwrap_or(false);
    LOADED_DRIVERS.with(|drivers| {
        let mut drivers = drivers.borrow_mut();
        let record = LoadedDriver {
            name: name.to_string(),
            path: path.to_string(),
            kill_ports,
            library: None,
        };
        match drivers.iter_mut().find(|entry| entry.name == name) {
            Some(slot) => *slot = record,
            None => drivers.push(record),
        }
    });
}

/// Whether `name` is currently loaded (whatever loaded it).
pub(crate) fn driver_is_loaded(name: &str) -> bool {
    LOADED_DRIVERS.with(|drivers| drivers.borrow().iter().any(|entry| entry.name == name))
}

/// The object path `name` was loaded from, or `None` when it is not loaded.
pub(crate) fn loaded_driver_path(name: &str) -> Option<String> {
    LOADED_DRIVERS.with(|drivers| {
        drivers
            .borrow()
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.path.clone())
    })
}

/// Whether `name` was loaded with the `kill_ports` option, or `None` when it is not loaded.
pub(crate) fn loaded_driver_kill_ports(name: &str) -> Option<bool> {
    LOADED_DRIVERS.with(|drivers| {
        drivers
            .borrow()
            .iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.kill_ports)
    })
}

/// Every loaded driver's name, in load order.
pub(crate) fn loaded_driver_names() -> Vec<String> {
    LOADED_DRIVERS.with(|drivers| {
        drivers
            .borrow()
            .iter()
            .map(|entry| entry.name.clone())
            .collect()
    })
}

/// Forgets `name`, answering whether it was loaded (`false` ⇒ the caller reports `not_loaded`).
///
/// A record that owns a `dlopen` handle drops it here (running the driver's `finish` and
/// `dlclose`ing the object); a record for an `open_port`-opened driver owns no handle, so its port
/// keeps working — "unload" means "no longer listed", matching the C's list removal.
pub(crate) fn forget_driver(name: &str) -> bool {
    LOADED_DRIVERS.with(|drivers| {
        let mut drivers = drivers.borrow_mut();
        match drivers.iter().position(|entry| entry.name == name) {
            Some(index) => {
                drivers.remove(index);
                true
            }
            None => false,
        }
    })
}

/// Records `entry` as a **statically linked** driver entry, the C's `add_driver_entry` →
/// `erts_add_driver_entry(de, NULL, …)` (`io.c:7640-7695`).
///
/// `0` = recorded, `1` = refused (a null entry, which the C would fault on reading `de->driver_name`
/// — a refusal is the honest answer, never a crash). The C additionally runs the entry's `init`
/// callback (`init_driver`, `io.c:7610-7620`) and drops the entry again when it returns non-zero.
/// That callback is an `unsafe extern "C" fn` this crate cannot call under its `#![deny(unsafe_code)]`
/// rule, so it is not run — and the C's own `void` wrapper discards the result anyway ("the init is
/// not allowed to fail when drivers are added by drivers", `io.c:7642-7645`), so the only observable
/// difference is an `init` that *vetoes* an entry a driver added.
fn registry_add_entry(entry: *mut ErlDrvEntry) -> c_int {
    if entry.is_null() {
        return 1;
    }
    DRIVER_ENTRIES.with(|entries| {
        entries.borrow_mut().push(DriverEntryRecord {
            entry,
            dynamic: false,
        })
    });
    0
}

/// Records `entry` as belonging to a **dynamically loaded** object — the C's *load path*
/// (`erts_add_driver_entry(dp, dh, …)`, `io.c:7650`, where `dp->handle != NULL`).
///
/// This runtime `dlopen`s every driver it runs, so the port's own entry is recorded here at load
/// ([`DriverPortAdapter::load`]); that is what makes `remove_driver_entry` answer the C's `-1` for a
/// port's own driver instead of the misleading `0` ("no such entry").
fn registry_note_loaded_entry(entry: *mut ErlDrvEntry) {
    DRIVER_ENTRIES.with(|entries| {
        entries.borrow_mut().push(DriverEntryRecord {
            entry,
            dynamic: true,
        })
    });
}

/// The C's `remove_driver_entry(de)` (`io.c:7698-7735`): `-1` for a dynamically loaded driver entry
/// (`dp->handle != NULL`, "Not allowed for dynamic drivers"), `1` when a matching statically linked
/// entry was found and removed, `0` when no listed entry matches.
///
/// The newest matching registration is removed first, mirroring the C's head-inserted list scan.
/// `entry` is only ever compared by address, so every value (including null) is a safe operand.
fn registry_remove_entry(entry: *mut ErlDrvEntry) -> c_int {
    DRIVER_ENTRIES.with(|entries| {
        let mut entries = entries.borrow_mut();
        let Some(index) = entries.iter().rposition(|record| record.entry == entry) else {
            return 0;
        };
        if entries[index].dynamic {
            return -1;
        }
        entries.remove(index);
        1
    })
}

/// Runs a driver entry-point call with the driver's host **published** as this thread's current
/// driver host ([`beam_abi::with_current_host`]).
///
/// Two of the frozen `driver_*` slots carry no port operand — `add_driver_entry`/`remove_driver_entry`
/// (`io.c:7640`/`7698`) — because the C functions they back act on the emulator's process-global
/// `driver_list`; their slot bodies read the published host back and delegate to it, and
/// [`DriverPort`] is that host here. A C driver may call them from **any** of its callbacks, so every
/// entry point that runs driver code is wrapped in this call.
///
/// The host is lent exclusively (`&mut dyn DriverHost`) for the dynamic extent of `f` only, so no
/// second `&mut` into it can coexist; the loaded library is handed to `f` as the shared borrow the
/// callbacks take (the two are disjoint fields of [`Driver`]).
fn publishing<R>(driver: &mut Driver, f: impl FnOnce(&DriverLibrary) -> R) -> R {
    let Driver { library, host } = driver;
    beam_abi::with_current_host(&mut **host, || f(library))
}

impl DriverHost for DriverPort {
    /// The term arena the term-data decoder allocates on.
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// An existing atom keeps the shared table's index; a new one is recorded on the pending
    /// interner at its predicted index (committed later by [`DriverPort::commit_atoms`]).
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        if let Some(atom) = self.atoms.lookup(name) {
            return Some(make_atom(atom));
        }
        Some(make_atom(self.interner.intern(name)))
    }

    /// Decodes the external-term image behind the term-data machine's `ERL_DRV_EXT2TERM` operand
    /// (`io.c:5824`).
    ///
    /// The C requires the image's leading `VERSION_MAGIC` and, when the byte after it is
    /// `COMPRESSED`, inflates the inner body first (`binary2term_prepare`, `external.c:1745-1760`);
    /// anything malformed is the driver's own "Invalid data" failure (`ERTS_DDT_FAIL`, `io.c:5842`).
    /// [`beam_codec::decode_term`] implements exactly that acceptance rule for this runtime — it
    /// owns both the version-byte check and the `COMPRESSED` wrapper, and it refuses trailing
    /// garbage for the plain form — so this hook is a straight delegation. An invalid image answers
    /// `None` (the whole term-data decode aborts, exactly like the C), never a partial or wrong
    /// term.
    ///
    /// The term is built on **this host's own arena** ([`DriverHost::heap`]), which lives as long as
    /// the driver does: the C builds the term on the *receiving process's* heap and delivers it from
    /// inside the callback, but the frozen seam gives a host no receiver mailbox, so the decoded term
    /// is queued instead (see the module docs) and the driver may keep reading it until the port is
    /// torn down.
    ///
    /// Atom handling follows [`DriverHost::intern_atom`] exactly: a name the shared table already
    /// holds keeps its index, and a name it does not is recorded on the pending interner at its
    /// predicted index for [`DriverPort::commit_atoms`] to materialise once the caller's borrow of
    /// the table is released. The decoder needs `&mut AtomTable`, so the shared table is **cloned**
    /// (the [`beam_bifs::binconv`] precedent); nothing is ever minted *through* that clone — a fresh
    /// name goes onto the interner — so this host can never hand out an index the runtime's own
    /// table cannot see.
    fn binary_to_term(&mut self, bytes: &[u8]) -> Option<Eterm> {
        let mut local = (*self.atoms).clone();
        let known = local.len();
        let term = beam_codec::decode_term(bytes, &mut self.heap, &mut local).ok()?;
        // Record the names the decoder appended, in order, so the interner predicts the very
        // indices the cloned table assigned them. `intern` (not `intern_atom`) is load-bearing: the
        // interner's `base` was fixed when this host was built, so appending mid-sequence would
        // shift the prediction of every name recorded after it.
        for index in known..local.len() {
            let Some(name) = local.name(beam_term::Atom::from_raw(index as u32)) else {
                continue;
            };
            self.interner.intern(name);
        }
        Some(term)
    }
    /// Records the port's request to watch `event` (`driver_select`); the port lifecycle applies
    /// it as soon as this callback returns (see the module docs).
    ///
    /// `0` means "recorded", `-1` the C's failure code: a host no port lifecycle has claimed, and
    /// an `event` that is not a descriptor at all (a 64-bit handle cannot be an fd). The
    /// registration itself happens right after the callback, so a poller-level refusal cannot be
    /// reported here — it surfaces as the port operation's own error instead (never silently).
    fn select(&mut self, _port: ErlDrvPort, event: ErlDrvEvent, mode: c_int, on: c_int) -> c_int {
        let Ok(fd) = c_int::try_from(event as isize) else {
            return -1;
        };
        let Ok(mut control) = self.control.try_borrow_mut() else {
            return -1;
        };
        if !control.attached {
            return -1;
        }
        control.selects.push(SelectRequest { fd, mode, on });
        0
    }

    /// Arms the port's timer at `ms` from now (`driver_set_timer`).
    ///
    /// `0` is the C's answer for **any** live port, including one whose entry declares no `timeout`
    /// callback: the C never leaves `drv->timeout` null, it substitutes its own `no_timeout_callback`
    /// (`io.c:7601`: `drv->timeout = de->timeout ? de->timeout : no_timeout_callback`), so the `-1`
    /// at `io.c:6918` stands only for the *invalid port* case and the timer is armed either way. A
    /// host no lifecycle has claimed is that invalid-port case here, so it keeps the C's `-1`; the
    /// call *is* synchronous: the deadline is stored here and the port picks it up as this callback
    /// returns. When such a deadline fires with no `timeout` callback, the C's `no_timeout_callback`
    /// is a silent no-op — which is what [`DriverPortAdapter::timeout`] reproduces for an entry
    /// without the slot.
    fn set_timer(&mut self, _port: ErlDrvPort, ms: c_ulong) -> c_int {
        let Some(deadline) = Instant::now().checked_add(Duration::from_millis(ms)) else {
            // A delay that does not fit an `Instant` cannot be a deadline: the C's failure code is
            // the honest answer (never a silently dropped timer).
            return -1;
        };
        let Ok(mut control) = self.control.try_borrow_mut() else {
            return -1;
        };
        if !control.attached {
            return -1;
        }
        control.deadline = Some(deadline);
        0
    }

    /// Records the control flags the driver asked for (`set_port_control_flags` — the C's
    /// `prt->control_flags`, `io.c:5318`).
    ///
    /// The C returns `void`, so the seam's `0` is always the answer. The recorded bit set is what
    /// the runtime reads back (`set_port_control_flags` → `get_port_flags` round-trips through
    /// this seam, `beam_abi`'s `driver/drv/lifecycle.rs`) to render a `port_control` reply as a
    /// **binary** when `PORT_CONTROL_FLAG_BINARY` is set and as a list of byte integers otherwise —
    /// the C's own rule (`write_port_control_result`, `io.c:4133-4175`).
    fn set_control_flags(&mut self, _mode: c_int, flags: c_int) -> c_int {
        self.control_flags.set(flags);
        0
    }

    /// Records the port's **busy state** (`set_busy_port` — the C's
    /// `prt->sched.flags & ERTS_PTS_FLG_BUSY_PORT`, `io.c:5208-5264`).
    ///
    /// Marking a port busy is a request to the *emulator*, not to the driver: the C stores the bit
    /// on the port and `erlang:port_command/2,3` consults it (`erts_port_output`, `io.c:1749-1764`).
    /// `0` is the C's answer (the `void set_busy_port(ErlDrvPort, int)` wrapper discards it) and the
    /// flag reads back through the shared [`BusyFlag`] handle the runtime seized at `open_port`.
    fn set_busy_port(&mut self, busy: bool) -> c_int {
        self.busy.set_busy(busy);
        0
    }

    /// The port's **state** stream flags, as `driver_get_port_flags` reports them.
    ///
    /// The C reads the *port state* bits (`ERTS_PORT_SFLG_BINARY_IO` ⇒ `PORT_FLAG_BINARY` (1), else
    /// `ERTS_PORT_SFLG_LINEBUF_IO` ⇒ `PORT_FLAG_LINE` (2), else `0`, `io.c:5338-5341`), **not**
    /// `prt->control_flags` — which only decides the `port_control` reply form (`io.c:5318`,
    /// `write_port_control_result`, `io.c:4133-4175`) and is what
    /// [`DriverPort::set_control_flags`]/[`DriverPort::control_flags`] carry.
    fn get_port_flags(&mut self) -> Option<c_int> {
        Some(self.port_flags)
    }

    /// Drops the port's pending timer (`driver_cancel_timer`) — `0`, or the C's `-1` for a host no
    /// lifecycle has claimed.
    fn cancel_timer(&mut self, _port: ErlDrvPort) -> c_int {
        let Ok(mut control) = self.control.try_borrow_mut() else {
            return -1;
        };
        if !control.attached {
            return -1;
        }
        control.deadline = None;
        0
    }

    /// The remaining milliseconds of the port's timer (`driver_read_timer`).
    ///
    /// The C answers `0` with the remaining time for *any* live port (`io.c:6930-6946`, its
    /// `erts_read_port_timer` is `0` when nothing is pending), so `None` — the seam's `-1` — is
    /// reserved for a host no lifecycle has claimed.
    fn read_timer(&mut self, _port: ErlDrvPort) -> Option<c_ulong> {
        let control = self.control.try_borrow().ok()?;
        if !control.attached {
            return None;
        }
        let remaining = match control.deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()).as_millis(),
            None => 0,
        };
        Some(c_ulong::try_from(remaining).unwrap_or(c_ulong::MAX))
    }

    /// Captures the port's output bytes (`driver_output` / `erl_drv_output`). Always succeeds: the
    /// bytes are the driver's own and a capture cannot fail.
    fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
        self.outputs.push(data.to_vec());
        0
    }
    /// Captures the decoded term (`erl_drv_output_term` / `erl_drv_send_term`).
    ///
    /// The C delivers it synchronously into the destination's mailbox (`io.c:5664`); here the term
    /// goes onto this port's [`TermQueue`], which the runtime's port pump drains and delivers (see
    /// the module docs). `0` is the frozen [`DriverHost`] seam's "accepted" code — the same code
    /// `beam_abi`'s own hosts answer.
    fn send_term(&mut self, _port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int {
        self.sent.push((to, term));
        self.terms.push(to, term);
        0
    }
    /// `driver_connected`: the port's owning process (this runtime's own pid term).
    fn connected_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        Some(self.owner.to_eterm())
    }

    /// `driver_caller`: the process that opened the port. One [`DriverPort`] is one port opened by
    /// one process, so this is the same pid [`DriverPort::connected_pid`] answers with.
    fn caller_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        Some(self.owner.to_eterm())
    }

    // -----------------------------------------------------------------------------------------
    // The driver-private queue (`prt->ioq`) and the one-PDL rule
    // -----------------------------------------------------------------------------------------

    /// `driver_enq` / `driver_enq_bin`: append `data` at the driver-private queue's **tail**.
    ///
    /// The queue is the C's `prt->ioq` (`io.c:6790-6919`) — the driver's **own** queue, reached only
    /// by `driver_enq`/`driver_pushq`/`driver_deq`/…, and distinct from both the Erlang→driver
    /// output queue `beam_io` owns (`prt->queue`) and the port's input queue (what `driver_output`
    /// answers with). `0` is the C's success code; nothing here can fail to accept the bytes (the
    /// C's `-1` stands for an allocation failure).
    fn enq(&mut self, data: &[u8], _is_binary: bool) -> c_int {
        self.queue.enq(data)
    }

    /// `driver_pushq` / `driver_pushq_bin`: put `data` at the queue's **head** (the C's
    /// `driver_pushq*`, the `enq_front` counterpart).
    fn pushq(&mut self, data: &[u8], _is_binary: bool) -> c_int {
        self.queue.pushq(data)
    }

    /// `driver_deq`: drop `len` bytes from the queue's head, answering the number dropped, or `-1`
    /// for a request larger than the queue (the C's failure code, which `beam_abi`'s wrapper turns
    /// into the `NOT_A_SIZE` result and which leaves the queue untouched).
    fn deq(&mut self, len: c_int) -> c_int {
        self.queue.deq(len)
    }

    /// `driver_sizeq`: the driver-private queue's byte count — `0` for a **fresh** port
    /// (`io.c:6827-6834`), and never negative for a live one.
    fn sizeq(&mut self) -> c_int {
        self.queue.sizeq()
    }

    /// `driver_peekq`: one iovec entry per queued chunk, `None` for an empty queue (the C's `NULL`
    /// with `*vlenp = 0`, `io.c:6861-6880`).
    fn peekq(&mut self) -> Option<Vec<SysIOVec>> {
        self.queue.peekq()
    }

    /// `driver_pdl_create`: this port's **one** port-data lock (`io.c:6716-6787`).
    ///
    /// The C's handle is an opaque pointer the driver keeps for the port's life; the properties that
    /// matter are that it is non-null, stable and **distinct per port**, which the host's own
    /// address is: a [`DriverPort`] lives inside a `Box` that never moves (the invariant
    /// [`DriverPortAdapter::start`] states), so this is a stable, per-port unique handle and a plain
    /// pointer cast — no `unsafe` needed.
    ///
    /// A second call on the same port answers `None` (the C's "a port admits exactly one PDL"), and
    /// `beam_abi`'s PDL registry rejects a duplicate handle address independently — so both the
    /// no-op repeated call and a port whose host was rebuilt answer the C's `NULL`.
    fn pdl_create(&mut self) -> Option<ErlDrvPDL> {
        if self.pdl_created {
            return None;
        }
        self.pdl_created = true;
        let handle: *const DriverPort = &*self;
        Some(handle as ErlDrvPDL)
    }

    // -----------------------------------------------------------------------------------------
    // The driver-monitor family — `driver_monitor_process` / `driver_demonitor_process` /
    // `driver_get_monitored_process`
    // -----------------------------------------------------------------------------------------

    /// `driver_monitor_process`: install a monitor on `pid` and hand back the holder
    /// (`io.c:6998-7015`).
    ///
    /// This is the frozen seam's **two-way** subset: it can only say "armed" (`Some`) or "could not
    /// arm" (`None`, which the wrapper maps to the C's `-1`). The C's third outcome — the **positive**
    /// `ERL_DRV_MONITOR_NOT_ALIVE` (`1`) for a process that is not alive — is carried by
    /// [`DriverHost::monitor_process_result`], which this host overrides
    /// ([`DriverPort::monitor_process_result`]); `beam_abi`'s wrapper prefers that hook, so the
    /// `1` **is** reachable end to end. Kept as the two-way answer for an implementor that only
    /// needs the `Option` shape, so a `{spawn_driver, Name}` port's C driver gets the same
    /// install-side behaviour either way:
    ///
    /// * an entry with **no** `process_exit` slot (the C's `-1`) is `None`;
    /// * a `pid` that is not a local process id — a non-pid term, or an external/wire pid — is
    ///   `None` as well on this subset (the full hook reports the C's `-1` for it too, while the C
    ///   entry point itself pre-rejects such an operand with `1`, `io.c:7025-7028`);
    /// * otherwise `Some(monitor)`, which the wrapper writes into the driver's holder.
    ///
    /// Liveness of a **local** pid is deliberately not checked here (this hook has no view to
    /// consult): [`DriverPort::monitor_process_result`] is the one that consults the published
    /// observation and answers `NotAlive`.
    fn monitor_process(&mut self, pid: Eterm) -> Option<ErlDrvMonitor> {
        if !self.has_process_exit {
            return None;
        }
        let pid = ProcessId::from_eterm(pid).ok()?;
        Some(self.monitors.install(pid))
    }

    /// `driver_monitor_process`: the full three-way answer (`io.c:7012-7038`,
    /// `do_driver_monitor_process`) — the C's `0` (`ERL_DRV_MONITOR_OK`), `1`
    /// (`ERL_DRV_MONITOR_NOT_ALIVE`) or `-1` (`ERL_DRV_MONITOR_ERROR`).
    ///
    /// The split, in the C's own order:
    ///
    /// 1. the loaded entry declares **no** `ErlDrvEntry::process_exit` slot ⇒ [`MonitorResult::Error`]
    ///    (the C's `-1`, `io.c:7020-7021`);
    /// 2. an operand that is not a **local** process id — a non-pid term, or an external/wire pid —
    ///    ⇒ [`MonitorResult::Error`]: no monitor can be armed on it, and this runtime cannot tell a
    ///    *dead* remote process from an unknown one, so it reports the honest "cannot monitor"
    ///    rather than the positive code the C's `names_a_process` guard uses (`io.c:7025-7028`);
    /// 3. a local pid the published observation says **is alive** ⇒
    ///    [`MonitorResult::Ok`](MonitorResult::Ok) with the freshly minted holder — the C arms the
    ///    monitor and lets a later `process_exit` fire it;
    /// 4. a local pid the observation says is **dead** ⇒ [`MonitorResult::NotAlive`] — the C's `1`,
    ///    reached when its monitor signal cannot be sent (`io.c:7031`). Nothing is installed;
    /// 5. **no observation** (or an unreadable view) ⇒ [`MonitorResult::Ok`](MonitorResult::Ok): the
    ///    documented non-regression fallback, "monitor it anyway". An unobserved process is not
    ///    reported dead — a fabricated `NotAlive` would silently drop a live driver watch, and
    ///    dropping a monitor is the one failure a driver cannot detect.
    ///
    /// # Freshness
    ///
    /// The view is the runtime's **snapshot** taken at the start of the last port round
    /// ([`ProcessLiveness`]; published by
    /// [`PortRegistry::refresh_liveness`](crate::port::PortRegistry)), so this hook consults an
    /// observation at most one round old. A callback reached outside a round — the driver's own
    /// `start`, or a `control` callback — sees the last round's view, and one reached before any
    /// round sees `None` (outcome 5).
    fn monitor_process_result(&mut self, pid: Eterm) -> MonitorResult {
        if !self.has_process_exit {
            return MonitorResult::Error;
        }
        let Ok(pid) = ProcessId::from_eterm(pid) else {
            return MonitorResult::Error;
        };
        match self.liveness.is_alive(pid) {
            Some(false) => MonitorResult::NotAlive,
            // Alive, or nothing observed to contradict it: arm the monitor, exactly as the C does
            // for a live target (outcome 3/5).
            Some(true) | None => MonitorResult::Ok(self.monitors.install(pid)),
        }
    }
    /// `driver_demonitor_process`: cancel the monitor `mon` (`io.c:7065-7081`) — the C's `0` when a
    /// monitor was removed, `1` when it no longer exists.
    ///
    /// `-1` (invalid port) is unreachable here: the frozen wrapper already answered it for a null
    /// monitor or a port with no environment, so this hook only ever sees a live port.
    fn demonitor_process(&mut self, mon: *const ErlDrvMonitor) -> c_int {
        let Some(identity) = monitor_identity(mon) else {
            return 1;
        };
        if self.monitors.remove(identity) {
            0
        } else {
            1
        }
    }

    /// `driver_get_monitored_process`: the pid behind `mon` (`io.c:7104`), or `None` — the wrapper's
    /// `driver_term_nil` — for a monitor this port does not hold.
    fn get_monitored_process(&mut self, mon: *const ErlDrvMonitor) -> Option<Eterm> {
        let identity = monitor_identity(mon)?;
        self.monitors.find(identity).map(ProcessId::to_eterm)
    }

    // -----------------------------------------------------------------------------------------
    // The driver-registry family (the four slots with no term services of their own)
    // -----------------------------------------------------------------------------------------

    /// `add_driver_entry`: record the statically linked entry `de` in this runtime's driver-entry
    /// list (`io.c:7640`), answering `0` recorded / `1` refused.
    ///
    /// Reached through the thread-local publication [`publishing`] installs around every driver
    /// callback, since the slot has no port operand to recover a host from.
    fn add_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        registry_add_entry(de)
    }

    /// `remove_driver_entry`: remove `de` from this runtime's driver-entry list (`io.c:7698`),
    /// answering the C's `1` removed / `0` no such entry / `-1` dynamically loaded.
    fn remove_driver_entry(&mut self, de: *mut ErlDrvEntry) -> c_int {
        registry_remove_entry(de)
    }

    /// `driver_lock_driver`: lock the driver behind `port`'s **own** loaded shared object
    /// (`io.c:7324`).
    ///
    /// The C reads `prt->drv_ptr->handle` — the *port's own* driver, never a lookup by name — and
    /// answers `-1` when that handle is null (`io.c:7337`, a statically linked driver) and `0`
    /// otherwise. This runtime `dlopen`s every driver it runs, so a host a port lifecycle claimed
    /// answers `0`; a bare host nobody claimed (a statically linked entry) answers `-1`, the C's own
    /// value for exactly that case. The port operand names the port the code already answers for, so
    /// it is not consulted.
    fn lock_driver(&mut self, _port: ErlDrvPort) -> c_int {
        match self.control.try_borrow() {
            Ok(control) if control.driver_loaded => 0,
            // A nested borrow cannot happen here (no borrow is held across a callback) and means a
            // host whose load state cannot be read; the C's "no dlopen handle" value is the honest
            // answer — never a fabricated lock.
            _ => -1,
        }
    }

    /// `driver_create_port`: the invalid-port sentinel [`ERTS_INVALID_ERL_DRV_PORT`] (`erl_port.h:67`).
    ///
    /// The C's single failure answer, kept for a genuinely unsupported state: creating a port needs
    /// a live `&mut PortRegistry`/`&mut Poller` **and** a `Box<dyn PortHost>`, neither of which is
    /// reachable from a per-port host hook (the port registry is owned elsewhere and a re-entrant
    /// open would alias the borrow the running callback was dispatched through). A fabricated port
    /// here would be a wrong handle, so the failure sentinel is the honest answer.
    fn create_port(
        &mut self,
        _creator: ErlDrvPort,
        _connected: beam_abi::driver::types::ErlDrvTermData,
        _name: *mut std::os::raw::c_char,
        _drv_data: beam_abi::driver::types::ErlDrvData,
    ) -> ErlDrvPort {
        ERTS_INVALID_ERL_DRV_PORT
    }
}

/// Loads the driver shared object for `name`, trying each of `candidates` in order.
///
/// The candidate spellings are the caller's (typically the driver name as written in
/// `open_port({spawn_driver, Name}, …)`, then the explicit `<Name>.so`); the `dlopen` loader appends
/// the platform's shared-object suffix when a candidate does not already carry it
/// (`beam_abi::driver::loader::load`), so `["t", "t.so"]` resolves `<dir>/t.so` on the first and the
/// second attempt alike, and `["/abs/t"]` resolves `/abs/t.so`.
///
/// A failure is the loader's own typed [`DriverLoadError`] (its [`DriverLoadError::reason_atom`] is
/// the oracle's `{error, Reason}` atom) — never a fabricated success and never a panic. The **last**
/// candidate's error is the one reported, since it is the one that names the path actually meant.
pub fn load_driver(
    candidates: &[String],
    name: &str,
) -> std::result::Result<DriverLibrary, DriverLoadError> {
    load_driver_at(candidates, name).map(|(library, _path)| library)
}

/// [`load_driver`], also reporting **which** candidate resolved — the object path the loader was
/// handed, recorded in the runtime's loaded-driver list so the same object is re-used later.
///
/// # Errors
///
/// The last candidate's typed [`DriverLoadError`] (see [`load_driver`]).
pub(crate) fn load_driver_at(
    candidates: &[String],
    name: &str,
) -> std::result::Result<(DriverLibrary, String), DriverLoadError> {
    let mut last: Option<DriverLoadError> = None;
    for candidate in candidates {
        match loader::load(candidate, name) {
            Ok(library) => return Ok((library, candidate.clone())),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        DriverLoadError::NoDriverInit(format!("no candidate paths to load driver '{name}' from"))
    }))
}

/// The **port lifecycle** the A.4 host alone lacks: a [`beam_io::Driver`] over a loaded C driver.
///
/// [`DriverPort`] is the host behind the `ErlDrvPort` a driver callback receives; this type owns
/// the driver itself and drives its `ErlDrvEntry` callbacks the way the runtime's port machinery
/// asks for them ([`beam_io::Driver`]). It is what `open_port({spawn_driver, Name}, _)` registers:
/// `beam_io::PortRegistry::open` boxes it, calls [`beam_io::Driver::start`] (⇒ the driver's `start`
/// callback) and, at close, [`beam_io::Driver::stop`] (⇒ the driver's `stop` callback) followed by
/// the `Driver` drop (`finish` + `dlclose`).
///
/// # Output is the driver's own callback, not a pipe
///
/// A `spawn`-driver port writes to the child's stdin; a *C driver* port has no child, so
/// [`beam_io::Driver::output`] hands the bytes to the driver's `outputv` callback when the entry
/// declares one, else to `output`. The bytes the driver answers with through `driver_output` are
/// captured by [`DriverPort`] and are then queued on the port's **input** queue — which is exactly
/// what the port hands up to Erlang as `{Port, {data, Data}}` (see `crate::port::PortRegistry::pump`).
/// Nothing here builds a term: the message is the runtime's, this type only moves bytes.
///
/// A driver-emitted *term* (`erl_drv_output_term`/`erl_drv_send_term`) does not go through that byte
/// queue: the host queues it on the port's [`TermQueue`], which a reader holding the queue handle
/// (the runtime's port pump) drains. This adapter's part in it is the teardown: it empties the queue
/// when the driver is unloaded, so no queued term survives its arena (see [`TermQueue`]).
///
/// # Typed gaps (documented, never a fabricated success)
///
/// * `driver_async` keeps the [`beam_abi::DriverHost`] default (`-1`): see the module docs — there
///   is no async pool, and the C's in-band path needs the entry and the driver's private state,
///   which the frozen host seam does not hand the host function.
/// * `driver_control` (the entry's `control` callback behind `port_control/3`) is routed by
///   [`DriverPortAdapter::control`]; its only gap is a reply the driver answers from memory of its
///   own rather than from the buffer it was handed (a typed *unsupported*, see the module docs).
/// * A queued term is delivered without a sending-port identity (the C stamps `from = prt`).
/// * `take_exit_status` is always `Ok(None)`: a driver port owns no child to reap, so no
///   `{Port, {exit_status, …}}` message is ever synthesized for it.
pub struct DriverPortAdapter {
    /// The loaded library **and** the host its callbacks run against (one `Box`, so the host never
    /// moves while an `ErlDrvPort` derived from its environment is in use).
    ///
    /// `None` after [`DriverPortAdapter::stop`]: the entry's `stop` runs at most once.
    driver: Option<Driver>,
    /// The per-port environment the `ErlDrvPort` handed to the driver **is** (see the module docs).
    ///
    /// **Boxed, and that is load-bearing:** [`DriverEnv::as_port`] hands the driver the
    /// environment's *own address*, so the environment must not move while the driver holds the
    /// port handle (which it may, for as long as the port lives — the fixture even stores it in its
    /// private state). A `Box` is allocated before `as_port` runs and only ever moves as a pointer,
    /// so the handle stays valid for the port's whole life.
    env: Option<Box<DriverEnv>>,
    /// The private state the driver's `start` returned — what every later callback receives.
    state: beam_abi::driver::types::ErlDrvData,
    /// The driver name this port was loaded as.
    name: String,
    /// The command string handed to the driver's `start` callback (the `open_port/2` `Name`).
    command: String,
    /// The event/timer state shared with the host (see [`DriverControl`]).
    control: Rc<RefCell<DriverControl>>,
    /// The port's [`TermQueue`], shared with the host (which pushes from inside a callback).
    ///
    /// Held here for one purpose: emptying it when the driver is unloaded, so a queued term can
    /// never outlive the arena it was built on (see [`TermQueue`]'s lifetime contract).
    terms: TermQueue,
    /// The port's driver-monitor tree, seized from the host at [`DriverPortAdapter::load`] (see
    /// [`DriverMonitors`]).
    ///
    /// Held for the whole port life for two reasons: the per-round drive point fires each pending
    /// monitor's `ErlDrvEntry::process_exit` through it (see
    /// [`DriverPortAdapter::drive_async`]), and teardown must empty it **before** the library is
    /// unloaded — a callback fired against a `dlclose`d driver would be undefined behaviour.
    monitors: Rc<DriverMonitors>,
    /// The descriptors this port has registered with the poller, in selection order.
    ///
    /// One port shares a single poller key, so the event identity the C carries per registration
    /// (`erl_check_io.c`'s `ErtsDrvEventState`) is not available from a `beam_io` port task. It is
    /// rebuilt here from the driver's own selections, which is exact while a port has **one**
    /// registration per direction — the single-fd case every `driver_select` user in this runtime
    /// has today (see [`DriverPortAdapter::armed_event`]).
    selected: Vec<Selection>,
}

/// One descriptor a driver selected, with the directions it armed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Selection {
    /// The descriptor the driver selected.
    fd: c_int,
    /// The directions armed for it.
    armed: Armed,
    /// The poller token, needed to re-register or deregister it.
    token: Token,
}

impl DriverPortAdapter {
    /// Loads the driver shared object for `name` from `candidates` and pairs it with `host`.
    ///
    /// The driver's `start` callback is **not** run here: [`beam_io::Driver::start`] runs it once
    /// the port has an id, which is where the `ErlDrvPort` comes from. The host is claimed here,
    /// which is what makes its `driver_select`/`driver_set_timer` requests applyable at all.
    ///
    /// # Errors
    ///
    /// The loader's own typed [`DriverLoadError`] (its [`DriverLoadError::reason_atom`] is the
    /// oracle's `{error, Reason}` atom) — never a fabricated library and never a panic.
    pub fn load(
        candidates: &[String],
        name: &str,
        host: DriverPort,
        command: String,
    ) -> std::result::Result<DriverPortAdapter, DriverLoadError> {
        let (library, path) = load_driver_at(candidates, name)?;
        // The host and the loaded entry first coexist here, so this is where the one fact the host
        // cannot observe for itself is stamped: whether the entry declares a `process_exit` slot,
        // which gates `driver_monitor_process` (`io.c:6998`). `beam_io`'s own host reads the very
        // same field at its own load site.
        let host = host.with_has_process_exit(library.entry().process_exit.is_some());
        // The same load site stamps the entry's `ERL_DRV_FLAG_SOFT_BUSY` bit onto the host's
        // shared busy handle: `erlang:port_command/2,3`'s `force` arm consults it
        // (`erl_bif_port.c:195-198`), and the runtime already holds the handle (it seized it with
        // `DriverPort::busy_flag` before this call, and the write lands on the same `Rc`).
        let host = host.with_soft_busy(library.entry().driver_flags & ERL_DRV_FLAG_SOFT_BUSY != 0);
        // The C's load path records the entry as a **dynamically loaded** driver
        // (`erts_add_driver_entry(dp, dh, …)`, `io.c:7650`, `dp->handle != NULL`), which is why
        // `remove_driver_entry` refuses it. Recording the same entry here lets the frozen
        // `remove_driver_entry` slot answer the C's `-1` for a port's own driver.
        registry_note_loaded_entry(library.entry() as *const ErlDrvEntry as *mut ErlDrvEntry);
        // The runtime's **loaded-driver list** ([`LOADED_DRIVERS`]), keyed by driver name: the
        // same view `erl_ddll:loaded_drivers/0` reports and the one a later
        // `open_port({spawn_driver, Name}, …)` matches the name against (`io.c:599-628`). The
        // `dlopen` handle stays with this port — the record stores none, so forgetting the entry
        // never unloads a live port (see [`note_driver_opened`]).
        note_driver_opened(name, &path);
        let control = host.control();
        let terms = host.term_queue();
        // The driver-monitor tree the host's monitor hooks install into. Seizing it here is what
        // lets the port **fire** their `process_exit` callbacks later — the boxed driver exposes no
        // host (see [`DriverMonitors`]). The handle is shared by `Rc`, so taking it before or after
        // the host moves into the `Box` is the same tree.
        let monitors = host.monitors();
        {
            let mut state = control.borrow_mut();
            state.attached = true;
            // The port's driver was `dlopen`ed, so `driver_lock_driver` answers the C's `0`.
            state.driver_loaded = true;
        }
        Ok(DriverPortAdapter {
            driver: Some(Driver::new(library, host)),
            env: None,
            state: std::ptr::null_mut(),
            name: name.to_string(),
            command,
            control,
            terms,
            monitors,
            selected: Vec::new(),
        })
    }
    /// The driver name this port was loaded as.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The private state the driver's `start` returned (null before `start`).
    pub fn state(&self) -> beam_abi::driver::types::ErlDrvData {
        self.state
    }

    /// The descriptors this port has currently selected, as `(fd, read, write)` triples.
    ///
    /// Diagnostic: the driver's own `driver_select` state, as the poll set holds it.
    pub fn selected(&self) -> Vec<(c_int, bool, bool)> {
        self.selected
            .iter()
            .map(|selection| (selection.fd, selection.armed.read, selection.armed.write))
            .collect()
    }
    /// Moves whatever the driver's just-finished callback asked of its port into the port itself:
    /// the `driver_select` requests are applied to the poll set and the `driver_output` payloads
    /// queued as the port's input (which is what reaches Erlang as `{Port, {data, Data}}`).
    ///
    /// Called right after **every** driver interaction, so a request is never left unapplied.
    fn settle(&mut self, ctx: &mut DriverContext<'_>) -> beam_common::Result<()> {
        self.collect_outputs(ctx);
        self.apply_selects(ctx)
    }

    /// Queues every captured `driver_output` / `erl_drv_output` payload as this port's input.
    fn collect_outputs(&mut self, ctx: &mut DriverContext<'_>) {
        let Some(driver) = self.driver.as_mut() else {
            return;
        };
        for chunk in driver.host_mut().take_outputs() {
            ctx.input().push(&chunk);
        }
    }

    /// Applies every `driver_select` request recorded during the last callback.
    fn apply_selects(&mut self, ctx: &mut DriverContext<'_>) -> beam_common::Result<()> {
        let requests = match self.control.try_borrow_mut() {
            Ok(mut control) => std::mem::take(&mut control.selects),
            // A nested borrow means a driver callback is still between the two of us; that cannot
            // happen here (no borrow is held across a callback) so it is a capability gap, never a
            // panic, and the requests stay recorded for the next round.
            Err(_) => return Err(crate::error::unsupported("the driver port state is in use")),
        };
        for request in requests {
            self.apply_select(ctx, request)?;
        }
        Ok(())
    }

    /// Applies one `driver_select` request to the poll set.
    ///
    /// The C's rule is per `(event, direction)`: `on` arms the `ERL_DRV_READ`/`ERL_DRV_WRITE`
    /// directions named by `mode`, `!on` clears them, and the event is deregistered once no
    /// direction is left (`erl_check_io.c:987`).
    ///
    /// A **deselect** whose `mode` carries `ERL_DRV_USE` *without* the cancel bit — the C's
    /// `(mode & ERL_DRV_USE_NO_CALLBACK) == ERL_DRV_USE` test (`erl_check_io.c:1177`) — is the
    /// "I am done with this event" form: it also clears **both** directions
    /// (`mode |= ERL_DRV_READ | ERL_DRV_WRITE`, `erl_check_io.c:1044`) and, once the event has no
    /// armed direction left, runs the driver's `stop_select` callback. The same form fires it on an
    /// event this port never armed, which is the C's "unknown fd" fast track
    /// (`erl_check_io.c:1030-1040`). The cancel bit (`ERL_DRV_USE_NO_CALLBACK`) suppresses the
    /// callback, which is why the C tests the pair rather than `ERL_DRV_USE` alone — and why the
    /// substituted `no_ready_*_callback` (which deselects with `ERL_DRV_USE_NO_CALLBACK`,
    /// `io.c:7522`/`7534`) never re-enters `stop_select`.
    ///
    /// The C stores the same obligation on its per-fd event state (`ERTS_EV_FLAG_USED`,
    /// `erl_check_io.c:86`) and keeps a plain deselect from removing the fd from the poll set while
    /// that flag is set (`erl_check_io.c:1111`/`1123`/`1170`). The flag is read off the request that
    /// carries it here instead, because that is the very input the C's callback branch tests and
    /// `beam_io`'s `DriverContext` has no "registered with no armed direction" state
    /// ([`Interest`] has no empty value): the difference is poll-set bookkeeping only — never an
    /// observable change through the driver ABI, and never a wrong `stop_select` decision.
    fn apply_select(
        &mut self,
        ctx: &mut DriverContext<'_>,
        request: SelectRequest,
    ) -> beam_common::Result<()> {
        // `driver_select(p, e, ERL_DRV_USE, 0)` — the "done with this event" form.
        let stop_use = request.on == 0
            && (request.mode & ERL_DRV_USE_NO_CALLBACK) == ERL_DRV_USE;
        let mode = if stop_use {
            request.mode | ERL_DRV_READ | ERL_DRV_WRITE
        } else {
            request.mode
        };
        let index = self.selected.iter().position(|s| s.fd == request.fd);
        let current = index.map_or_else(Armed::default, |i| self.selected[i].armed);
        let next = current.with(mode, request.on != 0);
        match (index, next.interest()) {
            // Nothing was armed and nothing becomes armed: a no-op (`ERL_DRV_USE` on its own, or a
            // deselect of a direction this port never held). The C's fast path still runs
            // `stop_select` for the "done with this event" form.
            (None, None) => {
                if stop_use {
                    self.fire_stop_select(request.fd)?;
                }
                Ok(())
            }
            // Still armed: re-register with the new direction mask. A direction is left, so the C
            // never reaches its `ASSERT(state->events == 0)` branch and no `stop_select` runs.
            (Some(i), Some(interest)) => {
                let token = self.selected[i].token;
                ctx.reregister(request.fd, token, interest)?;
                self.selected[i].armed = next;
                Ok(())
            }
            // The last direction went away: drop the registration, and run the driver's
            // `stop_select` when this call was the "done with this event" form.
            (Some(i), None) => {
                let token = self.selected[i].token;
                ctx.deregister(request.fd, token)?;
                self.selected.remove(i);
                if stop_use {
                    self.fire_stop_select(request.fd)?;
                }
                Ok(())
            }
            // A new descriptor.
            (None, Some(interest)) => {
                let token = ctx.register(request.fd, interest)?;
                self.selected.push(Selection {
                    fd: request.fd,
                    armed: next,
                    token,
                });
                Ok(())
            }
        }
    }

    /// Runs the driver's `stop_select` for `fd` — the C's `(*drv_ptr->stop_select)(e, NULL)`
    /// (`erl_check_io.c:1212-1216`), the driver's licence to release the event.
    ///
    /// The event handed back is `fd as ErlDrvEvent`, which **is** the value the C passes on: it
    /// forwards the very `ErlDrvEvent` the driver gave `driver_select`, and this runtime only ever
    /// accepted one that reduces to an fd ([`DriverPort`]'s `select` refuses anything else with the
    /// C's `-1`), so rebuilding the pointer from the descriptor is the identical handle — a driver
    /// that compares the two sees the same event.
    ///
    /// An entry with no `stop_select` slot is the C's `no_stop_select_callback`
    /// (`io.c:7546-7553`, a log line) — a no-op here, the same rule every other absent callback
    /// follows.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when the driver has already been torn down, and [`BeamError::Io`] when
    /// the callback reports a failure.
    fn fire_stop_select(&mut self, fd: c_int) -> beam_common::Result<()> {
        let Some(driver) = self.driver.as_mut() else {
            return Err(BeamError::BadArg);
        };
        let event = fd as isize as ErlDrvEvent;
        let called = publishing(driver, |library| {
            library.stop_select(event, std::ptr::null_mut())
        });
        match called {
            Ok(()) => Ok(()),
            Err(error) if error.is_absent() => Ok(()),
            Err(error) => Err(driver_call_error(error)),
        }
    }

    /// The descriptor to hand the driver's `ready_input`/`ready_output` for `read`.
    ///
    /// The C passes the **descriptor** the poller reported (`(ErlDrvEvent) state->fd`,
    /// `erl_check_io.c:1886`). A `beam_io` port task carries no such identity — every registration
    /// of a port shares the port's poller key — so the descriptor is recovered from the driver's
    /// own selections: exact for the one-registration-per-direction case, and a typed capability
    /// gap (never a wrong descriptor) when a driver has armed several.
    fn armed_event(&self, read: bool) -> beam_common::Result<Option<c_int>> {
        let mut armed = self
            .selected
            .iter()
            .filter(|selection| {
                if read {
                    selection.armed.read
                } else {
                    selection.armed.write
                }
            })
            .map(|selection| selection.fd);
        let first = armed.next();
        if armed.next().is_some() {
            return Err(crate::error::unsupported(
                "a driver that selected more than one event per direction",
            ));
        }
        Ok(first)
    }

    /// Runs the driver's `ready_input`/`ready_output` for the descriptor the poller reported.
    ///
    /// The descriptor is resolved here ([`DriverPortAdapter::armed_event`]) and handed to
    /// [`DriverPortAdapter::notify_ready`]; a caller that must read it *before* another driver
    /// interaction resolves it itself and calls `notify_ready` directly (see
    /// [`DriverPortAdapter::ready_output`]). With no registration for the direction the task is
    /// spurious (a `port_command`-driven OUTPUT task, say) and there is nothing to dispatch.
    ///
    /// # Errors
    ///
    /// The typed capability gap for a driver that armed more than one descriptor per direction, and
    /// whatever [`DriverPortAdapter::notify_ready`] returns.
    fn dispatch_ready(
        &mut self,
        ctx: &mut DriverContext<'_>,
        read: bool,
    ) -> beam_common::Result<()> {
        match self.armed_event(read)? {
            None => Ok(()),
            Some(fd) => self.notify_ready(ctx, fd, read),
        }
    }

    /// Runs the entry's `ready_input`/`ready_output` for one already-resolved descriptor.
    ///
    /// `fd` is the descriptor the C passes the callback (`(ErlDrvEvent) state->fd`,
    /// `erl_check_io.c:1886`); it is passed in rather than looked up here so that a caller which had
    /// to capture it earlier — [`DriverPortAdapter::ready_output`] reads it *before* the write,
    /// because `output`/`outputv` are free to re-arm with `driver_select` — notifies with the
    /// readiness the task was actually scheduled for.
    ///
    /// An entry that declares no callback for the direction is the C's `no_ready_*_callback`, which
    /// the C substitutes for a `NULL` slot (`io.c:7518-7534`): it logs and **deselects** that
    /// direction — with `ERL_DRV_USE_NO_CALLBACK`, so the substituted callback never re-enters
    /// `stop_select` — rather than spinning on a readiness nothing consumes.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when the driver has been torn down, [`BeamError::Io`] when the callback
    /// reports a failure, and whatever applying its `driver_select`/`driver_output` requests
    /// returns.
    fn notify_ready(
        &mut self,
        ctx: &mut DriverContext<'_>,
        fd: c_int,
        read: bool,
    ) -> beam_common::Result<()> {
        let Some(driver) = self.driver.as_mut() else {
            return Err(BeamError::BadArg);
        };
        let event = fd as isize as ErlDrvEvent;
        let state = self.state;
        let called = publishing(driver, |library| {
            if read {
                library.ready_input(state, event)
            } else {
                library.ready_output(state, event)
            }
        });
        match called {
            Ok(()) => {}
            // The entry has no callback for this direction: the C's `no_ready_input_callback`
            // (`io.c:7518-7523`) / `no_ready_output_callback` (`io.c:7529-7534`), which deselect
            // **exactly the direction the readiness was for** with the cancel bit
            // (`ERL_DRV_READ|ERL_DRV_USE_NO_CALLBACK`, resp. the `ERL_DRV_WRITE` form). The cancel
            // bit is load-bearing: it is what keeps the substituted callback from re-entering
            // `stop_select` through `apply_select`'s "done with this event" form.
            Err(error) if error.is_absent() => {
                let direction = if read { ERL_DRV_READ } else { ERL_DRV_WRITE };
                self.apply_select(
                    ctx,
                    SelectRequest {
                        fd,
                        mode: direction | ERL_DRV_USE_NO_CALLBACK,
                        on: 0,
                    },
                )?;
            }
            Err(error) => return Err(driver_call_error(error)),
        }
        self.settle(ctx)
    }

    /// Clears the port's pending timer.
    ///
    /// It is cleared **before** the driver's `timeout` callback runs, the way the C does it
    /// (`erts_port_task_handle_timeout` clears the port's timer state and only then invokes
    /// `entry->timeout`): a `timeout` callback that re-arms with `driver_set_timer` therefore
    /// really wins, which is the only way a port timer can repeat itself.
    fn clear_timeout(&self) {
        if let Ok(mut control) = self.control.try_borrow_mut() {
            control.deadline = None;
        }
    }

    /// Deregisters every descriptor this port selected, so a stopped port leaves nothing behind in
    /// the poll set.
    fn release_selections(&mut self, ctx: &mut DriverContext<'_>) -> beam_common::Result<()> {
        let selections = std::mem::take(&mut self.selected);
        let mut first_error = None;
        for selection in selections {
            if let Err(error) = ctx.deregister(selection.fd, selection.token) {
                first_error = first_error.or(Some(error));
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The bytes a `control` call answered with, read from whichever buffer the driver left its
    /// reply pointer in.
    ///
    /// The C reads `cres` bytes from `*rbuf` unconditionally, because it is C: the pointer the
    /// driver leaves is either the pre-allocated buffer it was handed or memory the driver owns.
    /// Both are the *same bytes*; the difference is only whether this crate may read them. It is
    /// `#![deny(unsafe_code)]` (workspace `../CONTEXT.md` §3), so a raw pointer may not be
    /// dereferenced here — and the driver's own allocation (`driver_alloc` is a plain `malloc`,
    /// `beam_abi`'s `driver/drv/mem.rs:62`) is exactly such a pointer.
    ///
    /// The reply is therefore sliced out of the buffers this call **owns** whenever the driver's
    /// pointer addresses them — the reply buffer it was handed (the buffer-filling driver, and the
    /// `ctl_reply` shape of `inet_drv.c`, which re-points `*rbuf` only when its answer does not fit
    /// the 64 bytes it was given) or the request buffer it was passed. A pointer that addresses
    /// neither is reported as the typed *unsupported* capability gap: the runtime cannot read the
    /// driver's memory without the raw access its crate attributes forbid, and a truncated or empty
    /// reply would be a fabricated one.
    ///
    /// A `NULL` reply pointer is the C's `NIL` (`io.c:4126`) — an empty reply, which is what this
    /// returns.
    ///
    /// # Errors
    ///
    /// The typed capability gap for a reply from the driver's own memory.
    fn control_reply(
        reply: &[u8],
        request: &[u8],
        rbuf: *const std::os::raw::c_char,
        len: usize,
    ) -> beam_common::Result<Vec<u8>> {
        if rbuf.is_null() {
            return Ok(Vec::new());
        }
        let start = rbuf as usize;
        for source in [reply, request] {
            let base = source.as_ptr() as usize;
            if start < base {
                continue;
            }
            let offset = start - base;
            // `offset + len <= source.len()`, written without an addition that could overflow.
            if offset <= source.len() && len <= source.len() - offset {
                return Ok(source[offset..offset + len].to_vec());
            }
        }
        Err(crate::error::unsupported(
            "a driver control reply from the driver's own memory",
        ))
    }
}
/// Maps a driver-callback failure onto the runtime's error type.
fn driver_call_error(error: loader::DriverCallError) -> BeamError {
    BeamError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        format!("driver callback failed: {error}"),
    ))
}

impl beam_io::Driver for DriverPortAdapter {
    /// The driver's own `ErlDrvEntry::driver_name`.
    fn name(&self) -> &str {
        &self.name
    }

    /// Runs the driver's `start` callback with this port's handle.
    ///
    /// The environment is built here and kept for the port's life (the handle **is** the
    /// environment's address). The returned [`ErlDrvData`](beam_abi::driver::types::ErlDrvData) is
    /// the driver's private state; the C failure signals (`ERL_DRV_ERROR_GENERAL` −1,
    /// `…_ERRNO` −2, `…_BADARG` −3) are reported as a typed failure rather than stored as a state.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when the driver has already been torn down or the command carries an
    /// interior NUL byte, and [`BeamError::Io`] when the driver's `start` reports a failure code.
    fn start(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> beam_common::Result<()> {
        let Some(driver) = self.driver.as_mut() else {
            return Err(BeamError::BadArg);
        };
        // `DriverEnv::as_port` returns the environment's own address, so the environment must be
        // allocated **before** the handle is taken and must not move afterwards: a `Box` is stable,
        // and the driver's `start` may keep the handle in its private state (the A.4 fixture does).
        // `DriverEnv` carries no lifetime, so the `&mut` borrow of the host ends with this call;
        // the environment (and so the handle) stays valid because the host lives in a `Box` that
        // never moves.
        let mut env = Box::new(driver.host_mut().environment());
        let port = env.as_port();
        self.env = Some(env);
        let command = std::ffi::CString::new(self.command.as_str()).map_err(|_| BeamError::BadArg)?;
        let data = publishing(driver, |library| {
            library.start(port, command.as_ptr() as *mut std::os::raw::c_char)
        })
        .map_err(driver_call_error)?;
        if (data as isize) < 0 {
            // The C's failure codes are negative; a driver that refuses the port must not become a
            // port with a bogus state pointer.
            return Err(BeamError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("driver '{}' refused the port (start returned {})", self.name, data as isize),
            )));
        }
        self.state = data;
        // Anything the driver's `start` asked of its port (a `driver_select`, a
        // `driver_set_timer`) is applied now: the registration reaches the poll set and the
        // deadline reaches the port's `TIMEOUT` task (`Port::open` re-arms the timer from
        // `Driver::next_timeout` right after this returns).
        self.settle(ctx)
    }

    /// Hands the port's queued bytes to the driver's `outputv` (when the entry declares one) or its
    /// `output` callback, and queues whatever the driver answers with through `driver_output` as
    /// this port's **input**.
    ///
    /// The C's own preference: `if (drv->outputv) …` else `… drv->output` (`io.c:1782` for the
    /// `port_command` path, `io.c:1597` for the async one) — an entry that declares **both** gets
    /// the vector callback, and an entry that declares only `outputv` is not left without any
    /// output callback at all.
    ///
    /// The vector mirrors the C's build: `iov[0]` is the empty **header** block a driver may fill
    /// in and `iov[1..]` the data, `binv` runs parallel to `iov` (all `NULL` here — these bytes are
    /// not backed by an `ErlDrvBinary`, the same convention `beam_abi`'s own queue views use), and
    /// `size` is the data's total byte count (`io.c:1842-1864`).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when the driver has been torn down, and [`BeamError::Io`] when the
    /// callback reports a failure.
    fn output(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> beam_common::Result<usize> {
        // Copy first: the queue is cleared below and the callback takes a raw pointer into the
        // buffer, so the slice must outlive the borrow of the context.
        let bytes = ctx.output().front().to_vec();
        if !bytes.is_empty() {
            ctx.output().clear();
            let Some(driver) = self.driver.as_mut() else {
                return Err(BeamError::BadArg);
            };
            let state = self.state;
            let result = if driver.library().entry().outputv.is_some() {
                // `iov`/`binv` are the driver's to read (and, for the header block, to write) for
                // the duration of the call only, which is exactly the C's contract: the vector is
                // built on the caller's stack (`ev.common.iov = iv`, `io.c:1806`).
                let mut iov = [
                    SysIOVec {
                        iov_base: std::ptr::null_mut(),
                        iov_len: 0,
                    },
                    SysIOVec {
                        iov_base: bytes.as_ptr() as *mut std::os::raw::c_void,
                        iov_len: bytes.len(),
                    },
                ];
                let mut binv: [*mut ErlDrvBinary; 2] = [std::ptr::null_mut(); 2];
                let mut ev = ErlIOVec {
                    vsize: iov.len() as c_int,
                    size: bytes.len(),
                    iov: iov.as_mut_ptr(),
                    binv: binv.as_mut_ptr(),
                };
                publishing(driver, |library| library.outputv(state, &mut ev))
            } else {
                let buf = bytes.as_ptr() as *mut std::os::raw::c_char;
                let len = bytes.len();
                publishing(driver, |library| library.output(state, buf, len))
            };
            match result {
                Ok(()) => {}
                Err(error) if error.is_absent() => {}
                Err(error) => return Err(driver_call_error(error)),
            }
        }
        // The driver may have answered with `driver_output` bytes (they are this port's *input*,
        // which the next task round hands up as `{Port, {data, Data}}`) and may have asked for a
        // new event or timer; both are applied here.
        self.settle(ctx)?;
        Ok(bytes.len())
    }
    /// Runs the driver's `ready_input` for the descriptor the poller found readable.
    ///
    /// Only reached when the driver itself armed that descriptor with `driver_select`; an entry
    /// without a `ready_input` callback is the C's `no_ready_input_callback` (a log *and* a
    /// deselect), which [`DriverPortAdapter::dispatch_ready`] reproduces.
    fn ready_input(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> beam_common::Result<()> {
        self.dispatch_ready(ctx, true)
    }

    /// Drains the port's queued output to the driver **and** runs its `ready_output` notification.
    ///
    /// The queued bytes always go down this port's single write path,
    /// [`DriverPortAdapter::output`] — the C's `port_command` → `output`/`outputv` delivery
    /// (`io.c:1782`, `io.c:1597`) — so a queued command reaches the driver whichever callback slots
    /// its entry declares. When the entry also declares a `ready_output` slot, that slot then runs
    /// with the write-armed descriptor as its event: it is the C's write-**readiness** notification
    /// (the `ERTS_PORT_TASK_OUTPUT` arm, `erl_port_task.c:1793-1797`, which calls
    /// `(*pp->drv_ptr->ready_output)(drv_data, ptp->u.alive.td.io.event)`), not a second write path,
    /// so an entry declaring both callbacks sees the C's combined effect — bytes via
    /// `output`/`outputv`, then the notification.
    ///
    /// The descriptor is read **before** the write: `output`/`outputv` are free to re-arm with
    /// `driver_select`, so the notification reports the readiness the task was scheduled for rather
    /// than whatever the write callback may have left armed.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when the driver has been torn down, [`BeamError::Io`] when either
    /// callback reports a failure, and the typed capability gap for a driver that armed more than
    /// one descriptor for writability.
    fn ready_output(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> beam_common::Result<usize> {
        let armed = self.armed_event(false)?;
        let written = self.output(ctx)?;
        if let Some(fd) = armed {
            self.notify_ready(ctx, fd, false)?;
        }
        Ok(written)
    }

    /// Whether any monitor this port's driver holds is awaiting its `process_exit` firing.
    ///
    /// This is the **cheap probe** `beam_io`'s port-task round consults
    /// ([`beam_io::Port::has_pending_async`], read by
    /// `beam_io::PortRegistry::execute_port_tasks`) to decide whether the port needs a
    /// [`beam_io::Driver::drive_async`] point this round. It answers `true` from the moment the
    /// runtime marks a monitored process dead through the shared [`DriverMonitors`] handle
    /// ([`DriverMonitors::note_process_exit`]) until [`DriverPortAdapter::drive_async`] has fired
    /// the callback and taken the entry, so the port is driven exactly while work is pending and a
    /// fresh port with no monitors never gets a spurious drive point.
    fn has_pending_async(&self) -> bool {
        self.monitors.has_pending()
    }

    /// Fires `ErlDrvEntry::process_exit` for every monitor whose process has exited.
    ///
    /// The C runs this callback from the signal/exit path; there is no port-task kind for it in this
    /// runtime, so it rides the **per-round drive point**
    /// `beam_io::PortRegistry::execute_port_tasks` already runs for a driver whose
    /// [`beam_io::Driver::has_pending_async`] answers `true` (the same point `driver_async`
    /// completions would use, and the reason this override is named for that hook). The runtime
    /// marks a monitor through the shared [`DriverMonitors`] handle when it observes a process exit;
    /// each marked identity is **taken** — and so **removed**, exactly as the C unlinks a monitor as
    /// it fires (`io.c:7140-7160`) — and handed to the callback through a local
    /// [`ErlDrvMonitor`] holder, whose address is the pointer the C passes into its own monitor
    /// table. A second round therefore fires nothing (nothing is left pending), and a later
    /// `driver_get_monitored_process` on the fired monitor is the C's "no longer exists".
    ///
    /// The callback is a `void` one and an entry that declares **no** `process_exit` slot is the C's
    /// silent no-op, so there is no failure to report; the `Result` is the frozen
    /// [`beam_io::Driver`] signature. If the driver has already been torn down
    /// ([`DriverPortAdapter::stop`], which empties the tree) the pending list can only be empty; a
    /// non-empty one is drained and answered `Ok(0)`, because there is no driver left to fire — the
    /// honest "nothing was fired", never a callback against a `dlclose`d library.
    ///
    /// # Errors
    ///
    /// Never today (see above).
    fn drive_async(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> beam_common::Result<usize> {
        let _ = ctx;
        let pending = self.monitors.take_pending();
        if pending.is_empty() {
            return Ok(0);
        }
        let Some(driver) = self.driver.as_mut() else {
            return Ok(0);
        };
        let data = self.state;
        for identity in &pending {
            let mut monitor = monitor_from_identity(*identity);
            // The pointer addresses a local that lives for the whole call, exactly as the C hands
            // the callback a pointer into its own monitor table.
            let ptr: *mut ErlDrvMonitor = &mut monitor;
            let _ = publishing(driver, |library| library.process_exit(data, ptr));
        }
        Ok(pending.len())
    }
    /// Runs the driver's `control` callback — the driver half of `erlang:port_control/3`
    /// (`call_driver_control`, `io.c:4024-4078`).
    ///
    /// `command` is a C driver's verb, an **integer** on the wire: `erts_internal:port_control/3`
    /// converts its `Command` with `term_to_Uint` (`erl_bif_port.c:322-340`, `> UINT_MAX` ⇒
    /// `badarg`), which is why [`beam_io`]'s built-in C-driver host parses the same `&str` as a
    /// decimal `c_uint` (`beam_io/src/abi/port.rs:716`). A verb that is not one is the caller's
    /// `badarg`, never a driver call with a substituted command.
    ///
    /// The reply protocol is the C's exactly. The caller hands the driver a pre-allocated
    /// [`ERL_ONHEAP_BINARY_LIMIT`]-byte buffer (`char resp_buf[…]`, `io.c:4352-4354`), `*rbuf`
    /// initially pointing at it; the driver either writes into that buffer, re-points `*rbuf` at
    /// memory of its own, or leaves it `NULL` — which the C renders as `NIL`
    /// (`write_port_control_result`, `io.c:4126`). The bytes it reports (`cres`) are the reply.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when the entry declares no `control` slot — the C's
    /// `if (!prt->drv_ptr->control) return ERTS_PORT_OP_BADARG;` (`io.c:4035`) — when `command` is
    /// not a decimal `c_uint`, or when the driver returned a negative result (`io.c:4074`), and the
    /// typed capability gap of [`DriverPortAdapter::control_reply`] for a reply the driver answers
    /// from memory of its own.
    fn control(
        &mut self,
        ctx: &mut beam_io::DriverContext<'_>,
        command: &str,
        arg: &[u8],
    ) -> beam_common::Result<Vec<u8>> {
        let Some(driver) = self.driver.as_mut() else {
            return Err(BeamError::BadArg);
        };
        let cmd: c_uint = command.parse().map_err(|_| BeamError::BadArg)?;
        let state = self.state;
        // The C passes the caller's `Data` as a writable buffer (`basep`, `io.c:4343-4348`) and the
        // driver may read it for the duration of the call only — an owned copy gives it exactly
        // that lifetime.
        let mut buf = arg.to_vec();
        // The C's reply buffer (`io.c:4352-4354`): `char resp_buf[ERL_ONHEAP_BINARY_LIMIT];
        // char *resp_bufp = &resp_buf[0]; ErlDrvSizeT resp_size = sizeof(resp_buf);`.
        let mut reply = vec![0u8; ERL_ONHEAP_BINARY_LIMIT];
        let mut rbuf = reply.as_mut_ptr() as *mut std::os::raw::c_char;
        let rlen = reply.len() as ErlDrvSizeT;
        let called = publishing(driver, |library| {
            library.control(
                state,
                cmd,
                buf.as_mut_ptr() as *mut std::os::raw::c_char,
                buf.len() as ErlDrvSizeT,
                &mut rbuf,
                rlen,
            )
        });
        let cres = match called {
            Ok(cres) => cres,
            // The C's absent-`control` badarg (`io.c:4035`).
            Err(error) if error.is_absent() => return Err(BeamError::BadArg),
            Err(error) => return Err(driver_call_error(error)),
        };
        if cres < 0 {
            // The C's `if (cres < 0) return ERTS_PORT_OP_BADARG;` (`io.c:4074`).
            return Err(BeamError::BadArg);
        }
        let bytes = Self::control_reply(&reply, &buf, rbuf, cres as usize)?;
        // A `control` callback is an ordinary driver callback: whatever it asked of its port (a
        // `driver_select`, a `driver_set_timer`, a `driver_output`) is applied now, exactly as for
        // every other callback (see [`DriverPortAdapter::settle`]).
        self.settle(ctx)?;
        Ok(bytes)
    }

    /// Runs the driver's `timeout` callback for a fired port timer.
    ///
    /// The pending deadline is cleared **first** (the C's own order), so a callback that re-arms
    /// with `driver_set_timer` decides the next deadline — and one that does not leaves the port
    /// with no timer at all (`Port::timeout` re-reads [`beam_io::Driver::next_timeout`] as this
    /// returns).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] when the driver has been torn down, and [`BeamError::Io`] when the
    /// `timeout` callback reports a failure.
    fn timeout(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> beam_common::Result<()> {
        self.clear_timeout();
        let state = self.state;
        let result = match self.driver.as_mut() {
            Some(driver) => match publishing(driver, |library| library.timeout(state)) {
                Ok(()) => Ok(()),
                Err(error) if error.is_absent() => Ok(()),
                Err(error) => Err(driver_call_error(error)),
            },
            None => Err(BeamError::BadArg),
        };
        self.settle(ctx)?;
        result
    }

    /// The deadline the driver last asked for with `driver_set_timer`, if any.
    ///
    /// This is the timer-arming seam: the port mirrors it into its `TIMEOUT` task after every
    /// driver interaction (`beam_io::Port::refresh_timeout`) and clamps its poll round by it
    /// (`beam_io::PortRegistry::next_deadline`), so a declared deadline really fires the driver's
    /// `timeout` callback at that instant.
    fn next_timeout(&self) -> Option<Instant> {
        self.control
            .try_borrow()
            .ok()
            .and_then(|control| control.deadline)
    }

    /// Runs the driver's `stop` callback and **unloads** the driver (its `finish` then `dlclose`).
    ///
    /// An entry without a `stop` callback is the C's own no-op (`io.c`), so its absence is not a
    /// failure. The environment (and so the port handle the driver may have kept in its private
    /// state) outlives the `stop` callback — the C's port struct is still live while `stop` runs —
    /// and is dropped only afterwards. Every descriptor the port selected is deregistered as it
    /// leaves, and a `driver_select`/`driver_set_timer` the teardown recorded is dropped with it.
    ///
    /// # Errors
    ///
    /// [`BeamError::Io`] when the driver's `stop` callback reports a failure; the library is
    /// unloaded either way, and a failed deregistration is surfaced when `stop` succeeded.
    fn stop(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> beam_common::Result<()> {
        let Some(mut driver) = self.driver.take() else {
            return Ok(());
        };
        let state = self.state;
        let result = match publishing(&mut driver, |library| library.stop(state)) {
            Ok(()) => Ok(()),
            Err(error) if error.is_absent() => Ok(()),
            Err(error) => Err(driver_call_error(error)),
        };
        if let Ok(mut control) = self.control.try_borrow_mut() {
            control.selects.clear();
            control.deadline = None;
        }
        // The port's driver-private state goes with the driver: the private queue (`prt->ioq`) dies
        // with the port struct.
        driver.host_mut().clear_driver_state();
        // The C detaches the port's driver monitors here — a dying port holds no more watches
        // (`erts_detach_processes`, `io.c:3846-3959`). It is also a **safety** obligation: the
        // library is unloaded below (`driver` drops → the entry's `finish` then `dlclose`), so a
        // monitor left behind could be fired by a later round against a `dlclose`d driver — see
        // [`DriverPortAdapter::drive_async`].
        self.monitors.clear();
        let released = self.release_selections(ctx);
        // The environment goes now: the port handle the driver held is dead the moment `stop`
        // returned, exactly as the C's `ErlDrvPort` is.
        self.env = None;
        // A term the driver queued is built on the host's arena, which this unload is about to
        // destroy, so the queue is emptied — the C's own outcome for a close in progress
        // (`deliver_term_check_port` refuses a delivery while `ERTS_PORT_SFLG_CLOSING`, `io.c:6213`).
        self.terms.clear();
        // `driver` drops here: `DriverLibrary::Drop` runs the entry's `finish` and `dlclose`s, and
        // the boxed host goes with it.
        result.and(released)
    }
}

impl Drop for DriverPortAdapter {
    /// Empties the port's [`TermQueue`] when the port goes away without [`beam_io::Driver::stop`]
    /// having run (a dropped registry, an unwound start).
    ///
    /// The queue's entries point into this adapter's own host arena, so leaving them for a reader
    /// to drain after the arena is freed would be a dangling read; an emptied queue is the honest
    /// answer (see [`TermQueue`]). The host's own per-port private state (its driver-private byte
    /// queue — see the `state` child module) is dropped with the host, and is cleared here too so a
    /// port that never reached [`beam_io::Driver::stop`] leaves exactly the same state behind.
    ///
    /// The driver-monitor tree is emptied **first**, and that ordering is the point: a `Drop` body
    /// runs before any field is dropped, so clearing here is the only reliable way to guarantee no
    /// pending `ErlDrvEntry::process_exit` monitor survives the `driver` field's own drop (the
    /// entry's `finish` then `dlclose`). A port whose `stop` never ran must therefore leave exactly
    /// the same "no watches" state behind — see [`DriverPortAdapter::drive_async`].
    fn drop(&mut self) {
        self.monitors.clear();
        self.terms.clear();
        if let Some(driver) = self.driver.as_mut() {
            driver.host_mut().clear_driver_state();
        }
    }
}

/// The A.4 **write-path regression test** for [`DriverPortAdapter::ready_output`], with the real C
/// driver fixture it compiles. A separate file so the fixture text (and the `erl_driver.h`
/// include-directory probe it needs) does not grow this one.
#[cfg(test)]
mod ready_output_tests;

/// The A.4 **`ERL_DRV_USE` / `stop_select` regression test** for
/// [`DriverPortAdapter::apply_select`], with the real C driver fixture it compiles: the C's
/// "done with this event" form, the plain deselect that must not fire it, and the cancel form.
#[cfg(test)]
mod stop_select_tests;

/// The A.4 **port-control regression suite** for [`DriverPortAdapter::control`] — the driver half of
/// `port_control/3` — with the real C driver fixture it compiles (five reply verbs, plus the
/// absent-slot build) and the reply-source rule of [`DriverPortAdapter::control_reply`].
#[cfg(test)]
mod control_tests;

/// The A.4 **driver-private queue / one-PDL / port-flags hook regression suite** for
/// [`DriverPort`]'s [`beam_abi::DriverHost`] implementation — the `prt->ioq` behind
/// `driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq`, the one-PDL rule behind
/// `driver_pdl_create`, and the port state flags `driver_get_port_flags` reports. A separate file
/// because the C entry points themselves are not nameable from this crate (see its module docs).
#[cfg(test)]
mod hooks_tests;

/// The A.4 **driver monitor regression suite** for [`DriverPort`]'s three
/// [`beam_abi::DriverHost`] monitor hooks — `driver_monitor_process`,
/// `driver_demonitor_process` and `driver_get_monitored_process` — over the [`DriverMonitors`]
/// tree behind them. A separate file so the identity/drain contract (which the runtime's later
/// `process_exit` firing depends on) does not grow this one.
#[cfg(test)]
mod monitor_tests;

/// The A.4 **`process_exit` firing regression suite** for
/// [`DriverPortAdapter::has_pending_async`] / [`DriverPortAdapter::drive_async`] — the per-round
/// drive point that hands a fired driver monitor to the entry's `ErlDrvEntry::process_exit`
/// callback — with the real C driver fixture it compiles. A separate file so the fixture text (and
/// the `erl_driver.h` include-directory probe it needs) does not grow this one.
#[cfg(test)]
mod fire_tests;

/// The A.4 **driver-monitor liveness wiring suite**: the runtime's published process-liveness view
/// ([`ProcessLiveness`]) reaching a real C driver's `driver_monitor_process` through the real
/// `#[no_mangle]` entry point — a live pid answers the C's `0`, and a pid the last port round no
/// longer observed answers its positive `1` (`ERL_DRV_MONITOR_NOT_ALIVE`, `io.c:7012-7038`). A
/// separate file so the C fixture it compiles does not grow this one.
#[cfg(test)]
mod liveness_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{atom_val, is_atom, Atom};

    /// A host over `atoms`, running as pid 1, with a small arena.
    fn host(atoms: &Rc<AtomTable>) -> DriverPort {
        DriverPort::new(
            Heap::new(256).expect("a test heap"),
            Rc::clone(atoms),
            ProcessId::new(1, 0),
        )
    }

    #[test]
    fn a_known_atom_keeps_the_shared_index_and_a_new_one_is_predicted() {
        let mut table = AtomTable::new();
        let known = table.intern("known");
        let atoms = Rc::new(table);
        let mut host = host(&atoms);

        // An existing atom keeps the shared table's index.
        assert_eq!(host.intern_atom("known"), Some(make_atom(known)));

        // A new name is predicted at `base + 0` (nothing else interning in between) and is not
        // resolvable until the caller commits it.
        let fresh = host.intern_atom("fresh").expect("interning always answers");
        assert!(is_atom(fresh));
        assert_eq!(atom_val(fresh), Atom::from_raw(atoms.len() as u32));
        assert_eq!(atoms.name(atom_val(fresh)), None);

        // A second call for the same name reuses the predicted index.
        assert_eq!(host.intern_atom("fresh"), Some(fresh));

        // Committing lands the name at exactly the predicted index.
        let mut committed = (*atoms).clone();
        host.commit_atoms(&mut committed);
        assert_eq!(committed.name(atom_val(fresh)), Some("fresh"));
        // ...and the handle's own table still does not see it: it is the caller's job to commit
        // into the *live* table (see `DriverPort::commit_atoms`).
        assert_eq!(atoms.name(atom_val(fresh)), None);
    }

    /// The external-term image behind `ERL_DRV_EXT2TERM` decodes onto the host's own arena.
    ///
    /// The image is produced with `beam_codec` (this runtime's `term_to_binary`) and decoded
    /// through the very hook `beam_abi`'s term-data machine calls. Comparing by **re-encoding** the
    /// decoded term avoids the unsafe term accessors a unit test in this crate may not use, and it
    /// still discriminates: a wrong term, a wrong atom index or a dropped element changes the
    /// bytes.
    #[test]
    fn an_ext2term_image_decodes_onto_the_hosts_arena() {
        let mut table = AtomTable::new();
        let known = table.intern("known");
        let atoms = Rc::new(table);
        let mut host = host(&atoms);

        // `{known, 42}` — both names already in the shared table.
        let source = host
            .heap()
            .tuple(&[make_atom(known), beam_term::make_small(42)])
            .expect("a two-tuple fits");
        let mut encoder = beam_codec::Encoder::new(&atoms);
        encoder.encode(source).expect("the source term encodes");
        let image = encoder.finish();

        let decoded = host
            .binary_to_term(&image)
            .expect("a valid external-term image decodes");
        let mut encoder = beam_codec::Encoder::new(&atoms);
        encoder.encode(decoded).expect("the decoded term re-encodes");
        assert_eq!(encoder.finish(), image, "the decoded term is the {{known, 42}} tuple");
    }

    /// A name the image carries but the shared table does not hold is recorded on the **pending
    /// interner** at the index the decoder's table clone assigned it — never minted through that
    /// clone — so `commit_atoms` lands it at exactly that index.
    #[test]
    fn an_ext2term_image_predicts_a_new_atoms_index() {
        // The shared table starts **empty**, so the image's atom is the first new name.
        let atoms = Rc::new(AtomTable::new());
        let mut host = host(&atoms);

        // `{a, 7}`, encoded against a table that holds `a` at index 0.
        let mut other = AtomTable::new();
        let a = other.intern("a");
        let mut encoder = beam_codec::Encoder::new(&other);
        encoder
            .encode(
                host.heap()
                    .tuple(&[make_atom(a), beam_term::make_small(7)])
                    .expect("a two-tuple fits"),
            )
            .expect("the source term encodes");
        let image = encoder.finish();

        let decoded = host
            .binary_to_term(&image)
            .expect("a valid external-term image decodes");
        // Not resolvable until committed: the hook never mints through the shared table.
        assert_eq!(atoms.len(), 0, "the shared table is never written through the clone");

        // Committing lands `a` at index 0, so re-encoding the decoded term against the committed
        // table reproduces the image byte-for-byte — which it could not if the prediction had
        // drifted.
        let mut committed = (*atoms).clone();
        host.commit_atoms(&mut committed);
        assert_eq!(committed.name(Atom::from_raw(0)), Some("a"));
        let mut encoder = beam_codec::Encoder::new(&committed);
        encoder.encode(decoded).expect("the decoded term re-encodes");
        assert_eq!(encoder.finish(), image, "the predicted atom index is the decoded one");
    }

    /// A malformed image is the C's `ERTS_DDT_FAIL` (`io.c:5842`) — `None`, never a partial term
    /// and never a panic. `binary2term_prepare` (`external.c:1745`) requires the leading
    /// `VERSION_MAGIC`, and `beam_codec::decode_term` enforces exactly that.
    #[test]
    fn an_invalid_ext2term_image_answers_none() {
        let atoms = Rc::new(AtomTable::new());
        let mut host = host(&atoms);
        for image in [
            &[][..],                      // no bytes at all
            &[131],                       // the version byte alone: truncated
            &[97, 1],                     // a term body with no version byte
            &[131, 97],                   // a truncated small int
            &[131, 104, 2, 97, 1],        // a 2-tuple with one element missing
            &[131, 116, 0, 0, 0, 1],      // a map header with no pairs
            &[131, 80, 0, 0, 0, 4, 1, 2], // a `COMPRESSED` wrapper with a bad stream
        ] {
            assert_eq!(host.binary_to_term(image), None, "image {image:?}");
        }
    }

    /// The C's `binary2term_prepare` also accepts a top-level `COMPRESSED` wrapper
    /// (`external.c:1754-1760`); the hook inherits `beam_codec::decode_term`'s handling of it.
    #[test]
    fn a_compressed_ext2term_image_decodes() {
        let mut table = AtomTable::new();
        let known = table.intern("known");
        let atoms = Rc::new(table);
        // A list of 400 cons cells needs more than the smoke-test arena.
        let mut host = DriverPort::new(
            Heap::new(4096).expect("a test heap"),
            Rc::clone(&atoms),
            ProcessId::new(1, 0),
        );
        let mut list = beam_term::NIL;
        // A long list of small ints: big enough that zlib's framing pays off, so the image really
        // carries the `COMPRESSED` wrapper (a small body reverts to the plain image).
        for value in (0..400_i64).rev() {
            list = host.heap().cons(beam_term::make_small(value), list).expect("fits");
        }
        let source = host
            .heap()
            .tuple(&[make_atom(known), list])
            .expect("a two-tuple fits");
        let mut encoder = beam_codec::Encoder::new(&atoms);
        encoder.encode(source).expect("the source term encodes");
        let plain = encoder.finish();
        let mut encoder = beam_codec::Encoder::new(&atoms);
        encoder.encode(source).expect("the source term encodes");
        let compressed = encoder.finish_compressed(6).expect("zlib is available");
        assert_eq!(
            (compressed[0], compressed[1]),
            (beam_codec::VERSION_MAGIC, 80),
            "the image must carry the COMPRESSED wrapper for this case to mean anything"
        );

        let decoded = host
            .binary_to_term(&compressed)
            .expect("a valid compressed image decodes");
        let mut encoder = beam_codec::Encoder::new(&atoms);
        encoder.encode(decoded).expect("the decoded term re-encodes");
        assert_eq!(encoder.finish(), plain, "the compressed image is the same term");
    }

    /// `erl_drv_output_term` / `erl_drv_send_term` queue their decoded term on the port's
    /// [`TermQueue`] — the delivery half — while [`DriverPort::sent`] keeps the accumulate-only
    /// view the A.4 integration test reads; draining the queue empties it.
    #[test]
    fn send_term_queues_the_decoded_term_and_draining_empties_the_queue() {
        let atoms = Rc::new(AtomTable::new());
        let mut host = host(&atoms);
        let queue = host.term_queue();
        assert!(queue.is_empty());
        let term = host.owner().to_eterm();
        // `None` = the port's connected process (`erl_drv_output_term`, `io.c:6254`).
        assert_eq!(host.send_term(std::ptr::null_mut(), None, term), 0);
        assert_eq!(host.sent(), &[(None, term)]);
        assert!(!queue.is_empty());
        // `Some(pid)` = an explicit destination (`erl_drv_send_term`, `io.c:6289`).
        assert_eq!(host.send_term(std::ptr::null_mut(), Some(term), term), 0);
        assert_eq!(queue.take(), vec![(None, term), (Some(term), term)]);
        // Draining empties it; the teardown path (`clear`) leaves it empty too.
        assert!(queue.is_empty());
        assert_eq!(queue.take(), Vec::new());
        host.terms.clear();
        assert!(queue.is_empty());
    }
    #[test]
    fn the_host_captures_output_and_answers_the_port_queries() {
        let atoms = Rc::new(AtomTable::new());
        let mut host = host(&atoms);
        assert_eq!(host.output(std::ptr::null_mut(), b"hi"), 0);
        assert_eq!(host.outputs(), &[b"hi".to_vec()]);
        assert!(host.sent().is_empty());

        // The owner answers both `driver_*` pid queries with this runtime's pid term.
        let owner = host.owner().to_eterm();
        assert_eq!(host.connected_pid(std::ptr::null_mut()), Some(owner));
        assert_eq!(host.caller_pid(std::ptr::null_mut()), Some(owner));

        // A host no port lifecycle claimed reports a timer request as unapplied (the C's `-1`);
        // `async` keeps the seam's typed default for the same reason.
        assert_eq!(host.set_timer(std::ptr::null_mut(), 100), -1);
        assert_eq!(host.cancel_timer(std::ptr::null_mut()), -1);
        assert_eq!(host.read_timer(std::ptr::null_mut()), None);
        assert_eq!(
            host.r#async(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                None,
                std::ptr::null_mut(),
                None
            ),
            -1
        );
    }

    /// A **claimed** host answers the timer family with the C's own codes for *every* driver,
    /// including one whose entry declares no `timeout` callback — the C substitutes a silent
    /// `no_timeout_callback` (`io.c:7601`) so `io.c:6918`'s `-1` stands only for an invalid port.
    ///
    /// Oracle ground truth (OTP 29 / erts-17.1): a real C `.so` whose entry leaves `.timeout` NULL
    /// and whose `output` reports `driver_set_timer(port, 1000)` prints `1` (the call returned `0`);
    /// `driver_read_timer`/`driver_cancel_timer` answer `0` there too.
    #[test]
    fn an_attached_host_arms_its_timer_without_a_timeout_callback() {
        let atoms = Rc::new(AtomTable::new());
        let mut host = host(&atoms);
        // Stand in for [`DriverPortAdapter::load`], which is the only thing that claims a host.
        host.control.borrow_mut().attached = true;

        // The C's `0`: armed, so the port's `TIMEOUT` task really fires.
        assert_eq!(host.set_timer(std::ptr::null_mut(), 100), 0);
        assert_eq!(
            host.control.borrow().deadline.is_some(),
            true,
            "a `0` answer must really arm the deadline"
        );
        // `io.c:6936`: a live port answers `0` with the remaining time (which is what `Some` means).
        assert!(host.read_timer(std::ptr::null_mut()).is_some());
        assert_eq!(host.cancel_timer(std::ptr::null_mut()), 0);
        assert!(host.control.borrow().deadline.is_none());
        // ...and `io.c:6925`: cancelling on a live port is `0` even with nothing pending.
        assert_eq!(host.cancel_timer(std::ptr::null_mut()), 0);
    }

    /// The four port-less `DriverHost` registry slots answer the C's own return codes, pinned
    /// directly against `io.c`'s `add_driver_entry` (`:7640`), `remove_driver_entry` (`:7698`),
    /// `driver_lock_driver` (`:7324`) and `driver_create_port` (`:787`).
    ///
    /// Oracle ground truth (OTP 29 / `erts-17.1`): a C fixture that calls all four hooks from a live
    /// `{spawn_driver, …}` port prints `0,1,0,-1` (see
    /// `tests/driver.rs::driver_lock_driver_and_the_entry_registry_agree_with_the_oracle`):
    /// `driver_lock_driver` answers `0` (the port's driver was `dlopen`ed), `add` then `remove` of a
    /// helper entry is `1` (recorded, then removed), a second `remove` is `0` (no such entry) and a
    /// `remove` of the port's **own** dynamically loaded entry is `-1` (`io.c:7709-7713`,
    /// "Not allowed for dynamic drivers").
    #[test]
    fn the_registry_hooks_answer_the_c_codes() {
        let atoms = Rc::new(AtomTable::new());
        let mut host = host(&atoms);

        // A bare host no port lifecycle claimed = the C's statically linked driver:
        // `driver_lock_driver` reads `prt->drv_ptr->handle == NULL` and answers `-1` (`io.c:7337`).
        assert_eq!(host.lock_driver(std::ptr::null_mut()), -1);

        // Two distinct entries, compared by address alone (the C's `dp->entry == drv`).
        let mut a = ErlDrvEntry::default();
        let mut b = ErlDrvEntry::default();

        // `add_driver_entry` records a statically linked entry: the C's `0`.
        assert_eq!(host.add_driver_entry(&mut a), 0);
        // ...and `remove_driver_entry` finds and removes it: the C's `1`.
        assert_eq!(host.remove_driver_entry(&mut a), 1);
        // A second removal finds nothing: the C's `0` ("no such entry").
        assert_eq!(host.remove_driver_entry(&mut a), 0);

        // A null entry is refused (`1`), never a read of `de->driver_name` the C would fault on.
        assert_eq!(host.add_driver_entry(std::ptr::null_mut()), 1);

        // An entry that was never added is `0` — never a fabricated removal.
        assert_eq!(host.remove_driver_entry(&mut b), 0);

        // `driver_create_port` has no live port registry reachable here: the C's single failure
        // sentinel (`erl_port.h:67`).
        assert_eq!(
            host.create_port(
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            ERTS_INVALID_ERL_DRV_PORT
        );
    }

    #[test]
    fn load_driver_reports_the_last_candidates_error() {
        let error = load_driver(&["/nonexistent/beam-rt-driver-test".to_string()], "t")
            .expect_err("a nonexistent object cannot load");
        assert_eq!(error.reason_atom(), "unspecified_error");
        assert!(error.text().contains("beam-rt-driver-test"), "{error:?}");
    }

    /// An empty candidate list is still a typed failure, never a panic.
    #[test]
    fn load_driver_rejects_an_empty_candidate_list() {
        let error = load_driver(&[], "t").expect_err("nothing to load");
        assert_eq!(error.reason_atom(), "no_driver_init");
    }

    #[test]
    fn the_host_hands_its_heap_back() {
        let atoms = Rc::new(AtomTable::new());
        let host = host(&atoms);
        assert!(host.into_heap().available() > 0, "the arena survives the host");
    }
}
