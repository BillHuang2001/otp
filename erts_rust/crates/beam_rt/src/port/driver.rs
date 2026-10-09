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
//! The queue is per port and shared by handle between the host (which pushes, from inside a
//! callback) and the runtime (which takes the handle before the port is registered and drains it).
//! Its one invariant is a lifetime one: a term's words live on [`DriverPort`]'s arena, so they may
//! only be read while the driver is alive. [`DriverPortAdapter`] therefore **empties** the queue
//! when it tears the driver down (from [`beam_io::Driver::stop`], where the C's own
//! `ERTS_PORT_SFLG_CLOSING` check refuses a delivery — `deliver_term_check_port`, `io.c:6213` — and
//! from its `Drop`, for a port whose `stop` never ran), so a late drain observes an empty queue
//! rather than a term built on a heap that has gone away.
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
//! * **`driver_control`** (the driver's `control` callback behind `port_control/3`). It is
//!   reachable only from the port lifecycle, and its reply protocol (`rbuf`/`rlen`, plus the
//!   `driver_free_buf` rule for a driver-allocated reply) is not modelled here.
//! * **A queued term carries no sending port.** The C stamps the port into the message header
//!   (`from = prt->common.id`, `io.c:6172`), which is what send-tracing shows; a [`TermQueue`]
//!   entry is `(destination, term)` only, so the runtime that drains it delivers the term itself
//!   (`erl_drv_output_term` sends the decoded term, never a `{Port, Term}` wrapper) with no
//!   sending-port identity.
//! * **`ERL_DRV_USE` / `stop_select`** is accepted but never fires: a deselected event is
//!   deregistered from the poll set, yet the driver's `stop_select` callback is not run.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use libc::{c_int, c_ulong};

use beam_abi::driver::loader::{self, DriverLibrary, DriverLoadError};
use beam_abi::driver::types::{
    ErlDrvBinary, ErlDrvEntry, ErlDrvEvent, ErlDrvPort, ErlIOVec, ERTS_INVALID_ERL_DRV_PORT,
    ERL_DRV_READ, ERL_DRV_WRITE,
};
use beam_abi::{DriverEnv, DriverHost, SysIOVec};
use beam_common::BeamError;
use beam_alloc::Heap;
use beam_bifs::AtomInterner;
use beam_io::{DriverContext, Interest, Token};
use beam_process::ProcessId;
use beam_term::{make_atom, AtomTable, Eterm};

use crate::context::PendingInterner;

/// One `driver_select` request a driver callback made through [`DriverHost::select`].
///
/// The C acts on it immediately; here it is *recorded* and applied by the port lifecycle as soon
/// as the callback returns (see the module docs). `fd` is the `ErlDrvEvent` the C passes — the
/// descriptor itself, exactly as `erl_check_io.c` reads it back out of the event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SelectRequest {
    /// The descriptor the driver selected (or deselected).
    fd: c_int,
    /// The `ERL_DRV_READ`/`ERL_DRV_WRITE` (and, unhandled, `ERL_DRV_USE`) mode bits.
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

/// A driver host for one port: the runtime services the `driver_*` / `erl_drv_*` functions need.
///
/// It owns a private term arena (the heap the term-data decoder builds its terms on), holds a
/// handle to the runtime's **one shared** [`AtomTable`] plus the deferred-commit [`PendingInterner`]
/// a *new* atom is recorded on, and answers the port queries with the owning [`ProcessId`].
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
    /// The deferred-commit interner a **new** atom is recorded on (see
    /// [`DriverPort::commit_atoms`]).
    interner: PendingInterner,
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
}

impl DriverPort {
    /// A host over `heap`, resolving/interning atoms against `atoms`, running as `owner`.
    pub fn new(heap: Heap, atoms: Rc<AtomTable>, owner: ProcessId) -> DriverPort {
        let interner = PendingInterner::new(atoms.len());
        DriverPort {
            heap,
            atoms,
            interner,
            owner,
            outputs: Vec::new(),
            sent: Vec::new(),
            terms: TermQueue::new(),
            control: Rc::new(RefCell::new(DriverControl::default())),
        }
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
    /// The event/timer state this host shares with its port lifecycle.
    ///
    /// [`DriverPortAdapter::load`] takes this handle so the driver's `driver_select` and
    /// `driver_set_timer` requests reach the poller and the port's `TIMEOUT` task (see the module
    /// docs). A host nobody claims keeps answering the C's failure code for both.
    fn control(&self) -> Rc<RefCell<DriverControl>> {
        Rc::clone(&self.control)
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

    /// Interns the names recorded by [`DriverHost::intern_atom`] into `atoms`, in order.
    ///
    /// The same deferred-commit rule as the NIF host (`crate::nif`'s `NifHostImpl`): the runtime's
    /// atom table is held immutably while a driver callback runs, so a *new* name is given its
    /// predicted index straight away and only materialised **after** the borrow is released. The
    /// caller must pass the **same** table the host's `atoms` handle refers to.
    pub fn commit_atoms(&self, atoms: &mut AtomTable) {
        self.interner.commit(atoms);
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
    let mut last: Option<DriverLoadError> = None;
    for candidate in candidates {
        match loader::load(candidate, name) {
            Ok(library) => return Ok(library),
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
/// * `driver_control` (the entry's `control` callback behind `port_control/3`) is not routed here
///   yet, so [`beam_io::Driver::control`]'s default (no reply bytes) is used.
/// * A queued term is delivered without a sending-port identity (the C stamps `from = prt`).
/// * `ERL_DRV_USE` is accepted but its `stop_select` callback never fires.
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
        let library = load_driver(candidates, name)?;
        // The C's load path records the entry as a **dynamically loaded** driver
        // (`erts_add_driver_entry(dp, dh, …)`, `io.c:7650`, `dp->handle != NULL`), which is why
        // `remove_driver_entry` refuses it. Recording the same entry here lets the frozen
        // `remove_driver_entry` slot answer the C's `-1` for a port's own driver.
        registry_note_loaded_entry(library.entry() as *const ErlDrvEntry as *mut ErlDrvEntry);
        let control = host.control();
        let terms = host.term_queue();
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
    fn apply_select(
        &mut self,
        ctx: &mut DriverContext<'_>,
        request: SelectRequest,
    ) -> beam_common::Result<()> {
        let index = self.selected.iter().position(|s| s.fd == request.fd);
        let current = index.map_or_else(Armed::default, |i| self.selected[i].armed);
        let next = current.with(request.mode, request.on != 0);
        match (index, next.interest()) {
            // Nothing was armed and nothing becomes armed: a no-op (`ERL_DRV_USE` on its own, or a
            // deselect of a direction this port never held).
            (None, None) => Ok(()),
            // Still armed: re-register with the new direction mask.
            (Some(i), Some(interest)) => {
                let token = self.selected[i].token;
                ctx.reregister(request.fd, token, interest)?;
                self.selected[i].armed = next;
                Ok(())
            }
            // The last direction went away: drop the registration.
            (Some(i), None) => {
                let token = self.selected[i].token;
                ctx.deregister(request.fd, token)?;
                self.selected.remove(i);
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
    /// An entry that declares no callback for the direction is the C's own no-op *plus* deselect
    /// (`io.c:7523-7538`: `no_ready_input_callback` logs and deselects the event), so the
    /// registration is dropped rather than spinning on a readiness nothing consumes.
    fn dispatch_ready(
        &mut self,
        ctx: &mut DriverContext<'_>,
        read: bool,
    ) -> beam_common::Result<()> {
        let Some(fd) = self.armed_event(read)? else {
            // No registration for this direction: the task is spurious (a `port_command`-driven
            // OUTPUT task, say) and there is nothing to dispatch.
            return Ok(());
        };
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
            // The entry has no callback for this direction: the C's `no_ready_*_callback`.
            Err(error) if error.is_absent() => {
                self.apply_select(
                    ctx,
                    SelectRequest {
                        fd,
                        mode: ERL_DRV_READ | ERL_DRV_WRITE,
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

    /// Runs the driver's `ready_output` when it armed a writable descriptor, else drains the
    /// output queue through [`beam_io::Driver::output`].
    ///
    /// The trait default is `output`, which is what a `port_command`-driven `OUTPUT` task needs
    /// for a driver with no write registration. A driver that *did* arm a descriptor for
    /// writability wants its own `ready_output` instead (the C never calls `output` from the poll
    /// path), so that is what runs — and the queue is left for the driver to drain.
    fn ready_output(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> beam_common::Result<usize> {
        if self.armed_event(false)?.is_some() {
            self.dispatch_ready(ctx, false)?;
            return Ok(0);
        }
        self.output(ctx)
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
    /// answer (see [`TermQueue`]).
    fn drop(&mut self) {
        self.terms.clear();
    }
}

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
