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

//! The runtime's port provider: the concrete [`PortHook`] behind `erlang:open_port/2`.
//!
//! `beam_bifs` owns the *port BIFs* (`open_port/2`, `port_close/1`, `port_info/1,2`) but
//! deliberately owns none of the port *machinery*: a port is an OS process plus a driver
//! plus an entry in the port table, which is runtime state (see
//! `../../beam_bifs/src/port.rs` and the `PortHook` seam in
//! `../../beam_bifs/src/context.rs`). This module is that provider — the counterpart of
//! the legacy C's `spawn` driver (`../../erts/emulator/beam/erl_bif_port.c` and
//! `../../erts/emulator/drivers/unix/spawn_driver.c`).
//!
//! # One OS-child implementation: `beam_io`
//!
//! The child process, its argv vector, its descriptors and its teardown all belong to
//! [`beam_io`] — this module owns **none** of them. [`beam_io::SpawnDriver`] really starts
//! the child (or adopts `{fd, N}`) in its constructor, [`beam_io::PortRegistry`] holds the
//! live ports and allocates their ids from `beam_process`'s port table, and
//! [`beam_io::Poller`] is the readiness reactor the drivers register their descriptors
//! with. This crate keeps exactly two things `beam_io` does not:
//!
//! * the **Erlang term decoding** of `open_port/2`'s `Name`/`Options` (which produces a
//!   term-free [`beam_io::SpawnSpec`]/[`beam_io::SpawnOptions`] pair), and
//! * the **`port_info/1` metadata** `beam_io` deliberately does not expose
//!   ([`PortMeta`]: the command name, the OS pid, the stream type, `connected`, the exit
//!   notification flag, links/monitors and the **owning pid**).
//!
//! Both live in [`PortRegistry`], which wraps `{ poller, registry, meta, heap }`: **one**
//! `beam_io::Poller` created once and kept for the runtime's lifetime (registrations live
//! in the poll set, so a per-call poller would drop them), **one** `beam_io::PortRegistry`,
//! the `port_info` metadata by port number, and the term arena a `port_info/1` result is
//! built on.
//!
//! # The two message producers
//!
//! A port produces two kinds of message: the child's **stream** (`{Port, {data, Data}}`)
//! and its **exit status** (`{Port, {exit_status, Status}}`, only for the `exit_status`
//! option). [`PortRegistry::pump`] is the single place both are harvested: it polls the
//! persistent poller in bounded slices, turns readiness into scheduled port tasks
//! ([`beam_io::PortRegistry::schedule_events`] + `execute_port_tasks`), and reports the
//! [`PortNotification`]s as [`PortEvent`]s.
//!
//! Two callers drive it, and each owns a different delivery point:
//!
//! * the **scheduler path** ([`Runtime::deliver_port_messages`], called from the scheduler
//!   pass) delivers every event into the **owning process's mailbox** with
//!   `beam_process`'s own deliver path, so a process blocked in a compiled `receive` is
//!   woken by the existing resume-on-send machinery;
//! * the **`-eval` `receive … after T`** path ([`RuntimeContext::poll_port_messages`])
//!   returns the built messages inline, because `-eval` runs outside a scheduler slice and
//!   matches them against the clause patterns itself.
//!
//! # What is real, and what is not (yet)
//!
//! **Real:** `open_port({spawn, Cmd}, _)` flattens `Cmd` to a command string and runs it
//! through `/bin/sh -c` with an `exec` prefix (`/bin/sh -c "exec Cmd"` — the shell replaces
//! itself with the command, exactly as the oracle does, so `{spawn,"echo a; echo b"}`
//! produces only `"a\n"`); `{spawn_executable, Path}` runs `Path` directly with
//! the `{args, …}` option; `{fd, N}` registers a descriptor port; the port gets a real
//! [`PortId`] out of `beam_io`'s registry; `port_close/1` kills and reaps the child and
//! returns the id to the table; `port_info/1,2` answers real items; and the child's stdout
//! is polled — a `receive {Port, {data, Data}}` reads the driver's stream back, delivering
//! a `binary` for a `binary`-stream port (a char list otherwise).
//!
//! `port_command/2,3` writes to the child's **stdin** (an iolist, a binary or a char list),
//! which is what makes `open_port({spawn,"cat"},[])` echo its input back: the bytes are
//! queued through [`beam_io::PortRegistry::port_command`] and the scheduled `OUTPUT` task is
//! run immediately so they reach the child before the call returns. A `{fd, N}` port owns no
//! child pipe, but its adopted descriptor is **duplex** (`beam_io`'s `SpawnDriver` registers
//! it once with `Interest::BOTH`), so the same call writes out through that descriptor —
//! exactly the oracle's behaviour.
//!
//! **Not yet:** only **stdout** is polled (the child's stderr is discarded by the driver);
//! `connected` only flips to `false` at `port_close/1` rather than when the child exits by
//! itself; a stream read of more than `ERL_ONHEAP_BINARY_LIMIT` (64) bytes on a `binary`
//! port is a typed *unsupported* gap (there is no off-heap ref-counted binary build path
//! here yet); and the `eof` option is accepted but inert (its `{Port, eof}` message is not
//! synthesized).
//!
//! # `port_info` items
//!
//! The pair list [`PortRegistry::info`] returns **is** the oracle's answer for both
//! arities: `beam_bifs`'s `erlang:port_info/1` filters it down to the seven `/1` items and
//! `erlang:port_info/2` searches it whole. In order:
//!
//! `{name, …}`, `{links, [Owner]}`, `{id, N}`, `{connected, Owner}`, `{input, 0}`,
//! `{output, 0}`, `{os_pid, N|undefined}` and — the `/2`-only extra — `{monitors, []}`.
//!
//! `links` is the owner process as a one-element list and `connected` is that owner pid,
//! matching erts-17.1 (nothing else links or monitors a port in Milestone-1). `input`/
//! `output` are byte counters — always `0` (the runtime keeps no per-port totals). The
//! oracle's own `stream` is **not** an item at all: `erlang:port_info(P, stream)` is
//! `error:badarg`.
//!
//! A port the runtime no longer knows (closed, or never opened) answers with an **empty**
//! pair list — which both `erlang:port_info/1` and `erlang:port_info/2` render as the atom
//! `undefined`, exactly the oracle's answer (`erlang.erl`'s own wrapper maps erts's empty
//! list to `undefined`; the native floor does the same here, see
//! `RuntimeContext::port_info_closed_port`).
//!
//! # The runtime's *own* port table
//!
//! `erlang:ports/0` and `erlang:system_info(port_count)` read **this** runtime's table by
//! design: a fresh runtime has opened no port, so it answers `[]` / `0` where the oracle
//! reports its standard-I/O ports. Those two answers are not meant to (and must not) match
//! the oracle's — see the same note in [`crate::info`].
//!
//! # The shared-registry shape (why the hook cannot borrow the runtime)
//!
//! [`crate::context::RuntimeContext::call_registered`] builds its `BifContext` from cheap
//! clones that do **not** borrow the [`Runtime`](crate::Runtime), because the installed
//! fun-caller bridge needs `&mut Runtime` at the same time. The port state therefore
//! travels the same way the ETS tables do: [`Runtime::ports`](crate::Runtime) is an
//! `Rc<RefCell<PortRegistry>>`, [`RuntimePorts`] holds a cheap clone of that `Rc` plus a
//! clone of the shared atom-table handle and the **owning pid** an `open_port/2` records,
//! and a nested borrow is reported as a typed capability gap instead of panicking (the
//! crate is `#![deny(unsafe_code)]` and never panics on runtime data).

use std::cell::{Cell, RefCell, RefMut};
use std::collections::HashMap;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub mod driver;
use beam_alloc::Heap;
use beam_bifs::PortHook;
use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_io::{
    Driver, Poller, PortCommandOptions, PortExitReason, PortNotification,
    PortRegistry as IoPortRegistry, SpawnDriver, SpawnOptions, SpawnSpec, SpawnStream,
    PORT_REDS_EXECUTE,
};
use beam_process::{PortId, PortTable, ProcessId, ProcessTable};
use beam_term::{
    atom_val, is_atom, is_small, make_atom, make_small, signed_value, try_make_small, AtomTable,
    Eterm, NIL,
};

use crate::context::RuntimeContext;
use crate::error;
use crate::Runtime;

/// The size in term words of the arena a port's `port_info/1` result is built on.
///
/// The arena is **never reset** (see [`PortRegistry::heap`]): a `port_info/1` result is a
/// term, and a term is a pointer, so its cells must outlive the call that produced it.
const PORT_HEAP_WORDS: usize = 4_096;

/// How long one poll round may block before a driver's second kind of message — the
/// `{Port, {exit_status, Status}}` of a reaped child — is re-checked.
///
/// An exit status is *not* a stream event (a child that wrote nothing still reports its
/// status), so the poller cannot rely on readiness to notice a child that was reaped during
/// the wait. Slicing a wait by this bounds how late that message can be, and a round that
/// *did* see readiness never waits at all (so the slice costs a data-producing port
/// nothing).
///
/// The scheduler path uses the very same slice as its per-round port-poll budget: one round
/// of [`Runtime::deliver_port_messages`] blocks for at most this long, so a runtime whose
/// run queue is drained and whose ports are quiet returns promptly instead of spinning. A
/// slice is an **upper bound only** — [`PortRegistry::pump`] shortens it further when a live
/// driver declares an earlier deadline (see [`beam_io::Driver::next_timeout`]), so a
/// driver-declared timer fires at its deadline rather than up to a slice late.
pub(crate) const PORT_POLL_SLICE: Duration = Duration::from_millis(25);

/// The atom vocabulary `port_info/1,2` can emit.
///
/// [`PortRegistry::info`] resolves each of these through the runtime's **one shared**
/// [`AtomTable`] and reports [`BeamError::TermNotFound`] rather than inventing an index —
/// so the runtime's boot seed ([`crate::boot::seed_core_atoms`]) must contain them. This
/// is the single source of truth for that list, exactly like [`crate::info::INFO_ATOMS`]
/// for the introspection surface (`true`/`false`/`undefined`/`process` are already part of
/// [`crate::boot::CORE_ATOMS`], and interning is idempotent).
///
/// The names are resolved **through** the hook's [`Rc`] handle but never interned
/// *through* it: the handle is a clone of the shared table, so `Rc::make_mut` would clone
/// it and mint an index the runtime's own table does not have — the term would then not
/// resolve (see [`RuntimePorts::atoms`]).
pub const PORT_INFO_ATOMS: &[&str] = &[
    // The item names.
    "name",
    "id",
    "connected",
    "os_pid",
    "links",
    "input",
    "output",
    "monitors",
    // A `/2`-only item whose unregistered value (`[]`) the native floor answers itself.
    "registered_name",
    // The shape of a `monitors` entry.
    "process",
];

/// The option **atoms** an `open_port/2` call accepts — the oracle's strict whitelist
/// (`erts/emulator/beam/erl_bif_port.c`).
///
/// Anything outside this list — including `list` (a list stream is already the default) — is
/// [`BeamError::BadArg`], never silently ignored. Only `binary` and `exit_status` change the
/// port here; the rest are recognized and accepted without effect.
const PORT_OPTION_ATOMS: &[&str] = &[
    "stream",
    "use_stdio",
    "stderr_to_stdout",
    "line",
    "nouse_stdio",
    "binary",
    "in",
    "out",
    "eof",
    "hide",
    "exit_status",
    "overlapped_io",
];

/// The option **tuple tags** an `open_port/2` call accepts — the oracle's strict whitelist
/// (`erts/emulator/beam/erl_bif_port.c`).
///
/// `cd`, `env` and `args` are honored; the rest are recognized and accepted without effect.
/// A tag outside this list is [`BeamError::BadArg`].
const PORT_OPTION_TUPLES: &[&str] = &[
    "packet",
    "line",
    "env",
    "args",
    "arg0",
    "cd",
    "parallelism",
    "busy_limits_port",
    "busy_limits_msgq",
];

/// Which `Name` the `open_port/2` call used — the driver that backs the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortName {
    /// `{spawn, Cmd}`: flatten `Cmd` and run it through `/bin/sh -c "exec Cmd"`.
    Spawn,
    /// `{spawn_executable, Path}`: run `Path` directly with the `args` option.
    SpawnExecutable,
    /// `{spawn_driver, Name}`: a **C driver** (contract A.4) loaded from `Name` — no OS child.
    SpawnDriver,
    /// `{fd, N}`: a descriptor the Erlang side already owns.
    Fd,
}

/// The Erlang-side representation of a port's stream: the `binary`/`list` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    /// `binary` — the driver would deliver `{Port, {data, Bin}}`.
    Binary,
    /// `list` — the driver would deliver `{Port, {data, Str}}` (the `erl` default).
    List,
}

impl From<StreamType> for SpawnStream {
    /// The `beam_io` stream option a runtime-side [`StreamType`] names.
    fn from(stream: StreamType) -> SpawnStream {
        match stream {
            StreamType::Binary => SpawnStream::Binary,
            StreamType::List => SpawnStream::List,
        }
    }
}

/// The `port_info/1,2` metadata of one live port — what [`beam_io`] deliberately does not
/// expose.
///
/// `beam_io`'s port owns the child, the descriptors and the lifecycle; these are the Erlang
/// side's facts: the command the port was opened with, the OS pid the driver reported, the
/// stream option, the `exit_status` flag and the process the port belongs to (which is where
/// its stream/exit messages are delivered, and which `port_info(Port, connected)`/`links`
/// report).
#[derive(Debug)]
struct PortMeta {
    /// The raw command line (a flattened shell command, a path, or `fd:<N>`) — the
    /// `port_info(Port, name)` value. The `exec` prefix the shell form runs with is **not**
    /// part of it. A `{fd, N}` port's adopted descriptor is duplex, so the write path does
    /// not need the `Name` form either (see [`PortRegistry::write_command`]).
    command: String,
    /// The OS process id the driver reported **before** the port was registered; absent for
    /// a `{fd, N}` port.
    os_pid: Option<u32>,
    /// The `binary`/`list` option (shapes the `{Port, {data, …}}` messages; no longer a
    /// `port_info` item).
    stream: StreamType,
    /// Whether the `exit_status` option was asked for.
    ///
    /// `beam_io`'s driver already hands the status out exactly once
    /// ([`beam_io::PortRegistry::take_exit_status`]), so this flag is not a once-only
    /// guard — it is what [`PortRegistry::pump`] uses to skip the status check for a port
    /// that never asked for it.
    notify_exit: bool,
    /// The processes monitoring this port. Empty in Milestone-1: nothing monitors a port
    /// yet, and `beam_term` has no reference term to identify a monitor by.
    monitors: Vec<ProcessId>,
    /// The process the port belongs to — the recipient of its `{Port, {data, …}}` and
    /// `{Port, {exit_status, …}}` messages, and the value of its `connected`/`links` items.
    owner: ProcessId,
    /// Which `Name` form opened the port.
    ///
    /// The two message producers route on it: a `{spawn_driver, Name}` port's bytes come from the
    /// C driver's `output` callback (queued on the port's input queue by
    /// [`driver::DriverPortAdapter`]) and are drained by [`PortRegistry::take_driver_outputs`],
    /// while every other port's come from its registered descriptor through the port-task round.
    kind: PortName,
    /// The term queue of a `{spawn_driver, Name}` port: the terms its C driver emitted through
    /// `erl_drv_output_term`/`erl_drv_send_term`, drained by
    /// [`PortRegistry::take_driver_terms`].
    ///
    /// `None` for every port without a C driver (a `spawn`/`fd` port). It is the port host's own
    /// [`driver::TermQueue`], seized with [`driver::DriverPort::term_queue`] **before** the host is
    /// moved into the port lifecycle (the boxed driver exposes no host). The host empties the queue
    /// when the driver is unloaded, so a term is only ever drained while the arena it was built on
    /// is alive (see the queue's lifetime contract).
    driver_terms: Option<driver::TermQueue>,
}

/// A message a live port has produced: the child's stream bytes, or its reaped exit status.
///
/// This is [`PortRegistry::pump`]'s report, and the seam between the port poller and the
/// two things that turn it into an Erlang term: the scheduler path
/// ([`Runtime::deliver_port_messages`], which delivers it to `owner`'s mailbox) and the
/// `-eval` `receive` path ([`RuntimeContext::poll_port_messages`], which builds it inline).
#[derive(Debug)]
enum PortEvent {
    /// The child wrote `bytes`; the message is `{Port, {data, Data}}` with `Data` shaped by
    /// `stream`.
    Data {
        /// The port the bytes came from.
        port: PortId,
        /// The bytes one driver read produced.
        bytes: Vec<u8>,
        /// The port's `binary`/`list` option.
        stream: StreamType,
        /// The process the message belongs to.
        owner: ProcessId,
    },
    /// The child was reaped and its `exit_status` option asked for the status.
    Exited {
        /// The port whose child went away.
        port: PortId,
        /// The child's status (its own exit code, or `128 + signal`).
        status: i32,
        /// The process the message belongs to.
        owner: ProcessId,
    },
    /// A C driver emitted a term (`erl_drv_output_term`/`erl_drv_send_term`, `io.c:5664`).
    ///
    /// The delivered message **is** the term itself — never a `{Port, Term}` wrapper: the C hands
    /// the decoded term straight to the destination's mailbox. The term's words live on the driver
    /// host's arena, which dies with the driver, so the consumer copies it onto a pointer-stable
    /// heap before it is used ([`RuntimeContext::stabilize_driver_term`]) — exactly as the
    /// `{Port, {data, …}}` builders already allocate their message on the runtime's arena.
    Term {
        /// The port whose driver emitted the term (the sender).
        port: PortId,
        /// The destination: `None` = the port's connected process (`erl_drv_output_term`),
        /// `Some(pid)` = the explicit destination (`erl_drv_send_term`).
        to: Option<ProcessId>,
        /// The decoded term, built on the driver host's arena.
        term: Eterm,
        /// The process the port belongs to (used when `to` is `None`).
        owner: ProcessId,
    },
}

/// The node-global **I/O byte counters** behind `erlang:statistics(io)`.
///
/// Shared as an [`Rc`] between the [`Runtime`](crate::Runtime) (which answers
/// `statistics(io)` from them) and its [`PortRegistry`] (which bumps them exactly where port
/// bytes move), so reading them never needs a borrow of the port table — and a `statistics`
/// call can never be turned into a capability gap by an unrelated live borrow.
///
/// `input` counts the bytes a port delivered to the runtime (the payload of a
/// `{Port, {data, …}}` message); `output` counts the bytes the runtime wrote to a port
/// (`erlang:port_command/2,3`). Both are cumulative for the runtime's lifetime and are never
/// reset, exactly the oracle's own semantics for `statistics(io)`
/// (`erts/emulator/beam/erl_bif_info.c`'s `am_io` arm).
///
/// A `Runtime` counts only the port traffic **it** performed, so a runtime that never opened
/// a port reports `{{input,0},{output,0}}` where the oracle reports its own standard-I/O
/// traffic — the same "an answer that reads *this* runtime's own state" class as
/// `erlang:ports/0`, never a fabricated figure.
#[derive(Debug, Default)]
pub struct IoCounters {
    /// Bytes read *from* ports (delivered as `{Port, {data, …}}`).
    input: Cell<u64>,
    /// Bytes written *to* ports (`erlang:port_command/2,3`).
    output: Cell<u64>,
}

impl IoCounters {
    /// The `(bytes in, bytes out)` pair `erlang:statistics(io)` reports.
    pub fn bytes(&self) -> (u64, u64) {
        (self.input.get(), self.output.get())
    }

    /// Credits `n` bytes a port delivered to the runtime.
    fn note_input(&self, n: usize) {
        self.input.set(self.input.get().saturating_add(n as u64));
    }

    /// Credits `n` bytes the runtime wrote to a port.
    fn note_output(&self, n: usize) {
        self.output.set(self.output.get().saturating_add(n as u64));
    }
}

/// The runtime's port subsystem state: one poller, one `beam_io` port registry, the
/// `port_info` metadata and the term arena.
///
/// Shared as `Rc<RefCell<PortRegistry>>` (see the module docs). The [`Poller`] is created
/// **once**, here, and kept for the runtime's lifetime: a port's descriptors are registered
/// with the poll set when its driver starts, so a poller re-created per call would lose
/// every registration.
pub struct PortRegistry {
    /// The readiness reactor every live port's descriptors are registered with.
    poller: Poller,
    /// The `beam_io` port registry: the id authority and the live ports with their drivers.
    registry: IoPortRegistry,
    /// The `port_info/1` metadata, keyed by port number.
    meta: HashMap<u64, PortMeta>,
    /// The arena a `port_info/1` result is built on.
    ///
    /// `port_info/1` hands its `{Item, Value}` pairs to `beam_bifs`, which copies them into
    /// the BIF's **result term** — and a list term is a *pointer*, so the cells must stay
    /// valid after the call returns. The arena is therefore runtime-lifetime and **never
    /// reset** (the same rule as [`crate::Runtime::eval_heap`]); a pathologically long
    /// sequence of `port_info/1` calls on long command names would exhaust it, and that
    /// exhaustion surfaces as the arena's typed allocation error rather than as a dangling
    /// term.
    heap: Heap,
    /// The shared byte counters behind `erlang:statistics(io)` (see [`IoCounters`]).
    ///
    /// The runtime keeps a clone of this same [`Rc`], so the `statistics(io)` arm reads the
    /// counters without borrowing this registry.
    io_counters: Rc<IoCounters>,
}

impl PortRegistry {
    /// An empty registry with its poller and term arena.
    ///
    /// # Errors
    ///
    /// Propagates the poll set's and the arena's allocation failures.
    pub fn new() -> Result<PortRegistry> {
        // The driver C ABI (contract A.4) is a **link-time** seam, exactly like the NIF ABI: a
        // loaded driver `.so` leaves its `driver_*`/`erl_drv_*` imports undefined and resolves them
        // out of the loading process's dynamic symbol table. `-Wl,--export-dynamic` (see
        // `build.rs`) publishes the symbols; this `black_box` reference is what anchors
        // `beam_abi`'s driver codegen unit in the link so there *are* symbols to publish. Without
        // it, a `dlopen(RTLD_NOW)` of a real driver would fail on an unresolved symbol.
        let _ = std::hint::black_box(beam_abi::driver::drv::driver_api_table());
        Ok(PortRegistry {            poller: Poller::new()?,
            registry: IoPortRegistry::new(),
            meta: HashMap::new(),
            heap: Heap::new(PORT_HEAP_WORDS)?,
            io_counters: Rc::new(IoCounters::default()),
        })
    }

    /// The shared I/O byte counters behind `erlang:statistics(io)` (see [`IoCounters`]).
    ///
    /// The runtime keeps a clone so its `statistics(io)` arm reads the counters without
    /// borrowing this registry.
    pub fn io_counters(&self) -> Rc<IoCounters> {
        Rc::clone(&self.io_counters)
    }

    /// Registers `driver` as a new port and records its `port_info` `meta`.
    ///
    /// The id is allocated by [`beam_io::PortRegistry::open`], which also starts the driver
    /// and tears it down (and releases the number) when the start fails — so a failed start
    /// leaks neither the poll set nor the id space, and the metadata is only recorded once
    /// the port is genuinely live.
    ///
    /// # Errors
    ///
    /// [`BeamError::SystemLimit`] when the id space is exhausted, or whatever the driver's
    /// `start` returns (the driver having been torn down).
    fn register(&mut self, driver: Box<dyn Driver>, meta: PortMeta) -> Result<PortId> {
        let id = self.registry.open(&mut self.poller, driver)?;
        self.meta.insert(id.number(), meta);
        Ok(id)
    }

    /// Whether `id` names a port the runtime currently **holds**.
    ///
    /// The native `port_info/1,2` floor uses it to tell "a port I know" (delegate to
    /// `beam_bifs`) from "a port I do not" (the oracle's `undefined`).
    pub fn is_live(&self, id: PortId) -> bool {
        self.meta.contains_key(&id.number())
    }

    /// Whether `id` names a port the runtime currently holds.
    ///
    /// [`PortRegistry::close`] removes the metadata and the `beam_io` entry together, so
    /// this and [`PortRegistry::is_live`] always agree.
    pub fn contains(&self, id: PortId) -> bool {
        self.meta.contains_key(&id.number())
    }

    /// Writes `bytes` to the port's stream — `erlang:port_command/2,3`.
    ///
    /// The bytes are queued through [`beam_io::PortRegistry::port_command`] and the
    /// `OUTPUT` task it scheduled is run immediately, so the stream has been handed the data
    /// before this call returns (the BIF's `true` answer means exactly that). For a child port
    /// that stream is the child's stdin; for a `{fd, N}` port it is the **adopted, duplex**
    /// descriptor — writing is the oracle's own behaviour for a descriptor port, so the write
    /// takes the same path rather than being a capability gap.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port this registry does not hold (closed, or never
    /// opened) — the oracle's own reason for `port_command` on a dead port;
    /// [`BeamError::Io`] when the write itself fails (a child that closed its stdin yields
    /// `EPIPE`).
    pub fn write_command(&mut self, id: PortId, bytes: &[u8]) -> Result<()> {
        if !self.meta.contains_key(&id.number()) {
            return Err(BeamError::BadArg);
        }
        // Every byte handed to a port counts as **output** for `erlang:statistics(io)`,
        // whichever stream it goes to (the counters are bumped before the branch so a
        // driver port and a child/`{fd,N}` port are counted alike).
        self.io_counters.note_output(bytes.len());
        // A **C driver** port has no child pipe: `port_command` *is* the driver's own `output`
        // callback. `beam_io::PortRegistry::output` runs it inline, and the bytes the driver
        // answers with through `driver_output` are queued as this port's input for the next
        // [`PortRegistry::pump`] — see [`PortRegistry::take_driver_outputs`].
        if self
            .meta
            .get(&id.number())
            .map(|meta| meta.kind == PortName::SpawnDriver)
            .unwrap_or(false)
        {
            self.registry.output(&mut self.poller, id, bytes)?;
            return Ok(());
        }
        self.registry
            .port_command(&mut self.poller, id, bytes, PortCommandOptions::EMPTY)?;
        // `port_command` only queues the bytes and schedules the task; run it now so the
        // write really happened.
        let _outcome = self
            .registry
            .execute_port_tasks(&mut self.poller, PORT_REDS_EXECUTE)?;
        Ok(())
    }

    /// The registry's port-number table, mutably — the seam the **port-monitor** BIFs drive.
    ///
    /// This is the **same** [`beam_process::PortTable`] instance
    /// [`beam_io::PortRegistry::open`] allocates from and [`PortRegistry::close`] releases
    /// through (see [`beam_io::PortRegistry::port_table_mut`], of which this is a forwarder), so
    /// a monitor recorded here is the one `open`/`close` and the port's own lifecycle observe.
    /// It is therefore the **authoritative** port table for port monitors.
    ///
    /// # Borrowing rule
    ///
    /// The returned `&mut PortTable` borrows this registry, which lives behind the runtime's
    /// `Rc<RefCell<PortRegistry>>`: scope the borrow so it is dropped before any nested borrow
    /// of the same cell (acquire the `RefMut`, take the table, run the single monitor/demonitor
    /// call, drop the whole chain). A nested borrow stays the typed capability gap
    /// ([`crate::port::RuntimePorts::registry_mut`]'s `error::unsupported`), never a panic.
    pub fn port_table_mut(&mut self) -> &mut PortTable {
        self.registry.port_table_mut()
    }

    /// The registry's port-number table, **read-only** — the inspecting counterpart of
    /// [`PortRegistry::port_table_mut`].
    ///
    /// For observers that only look at a port's relationships (its incoming port-monitor set,
    /// say) without changing them, so they need no `&mut` borrow of the registry. It forwards
    /// [`beam_io::PortRegistry::port_table`], the same table `port_table_mut` hands out.
    pub fn port_table(&self) -> &PortTable {
        self.registry.port_table()
    }
    /// The number of **live** ports — the value `erlang:system_info(port_count)` reports.
    pub fn len(&self) -> usize {
        self.meta.len()
    }

    /// Whether no port is open.
    pub fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }

    /// The ids of the **live** ports, in port-number order.
    ///
    /// The metadata map holds exactly the ports `open_port/2` recorded, so it is the direct
    /// source of truth for `erlang:ports/0` — reported here rather than recovered by
    /// scanning the table upward from zero, which a slot released *below* a still-live port
    /// would cut short. The numbers are sorted so the answer does not depend on the map's
    /// iteration order.
    pub fn ids(&self) -> Vec<PortId> {
        let mut numbers: Vec<u64> = self.meta.keys().copied().collect();
        numbers.sort_unstable();
        numbers.into_iter().map(PortId::new).collect()
    }

    /// Closes the port `id`: notifies its driver of the exit, kills and reaps its child,
    /// releases its id and drops its metadata.
    ///
    /// The driver is told the port is exiting ([`PortExitReason::Closed`]) **before** the
    /// teardown runs — `beam_io`'s [`beam_io::PortRegistry::notify_port_exit`] invokes
    /// [`Driver::port_exit`] at most once per port and leaves the port live, so a driver can
    /// react to its port exiting without owning the lifecycle; the teardown `stop` then
    /// follows (see [`beam_io::Driver::port_exit`]).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port the runtime does not hold — never a silently
    /// accepted close of a port that does not exist. The port is removed from this registry
    /// either way, and both outcomes are reported: the teardown's failure is returned first,
    /// and a failed exit notification is surfaced when the teardown succeeded — neither is
    /// silently swallowed.
    pub fn close(&mut self, id: PortId) -> Result<()> {
        if !self.meta.contains_key(&id.number()) {
            return Err(BeamError::BadArg);
        }
        // The port-exit notification first: the driver sees `port_exit` before the teardown
        // `stop`, and the port is still live for it.
        let notified = self
            .registry
            .notify_port_exit(&mut self.poller, id, PortExitReason::Closed);
        // The port is leaving this registry either way: a close the runtime has begun is
        // never abandoned halfway, so a failed notification cannot leave the table stale.
        self.meta.remove(&id.number());
        let closed = self.registry.close(&mut self.poller, id);
        // `and` returns the teardown's error when it failed, and the notification's
        // otherwise — so a failure on either path reaches the caller.
        closed.and(notified)
    }

    /// Polls the live ports for up to `timeout`, runs the port tasks the readiness implies,
    /// and reports the messages they produced.
    ///
    /// The wait is spent in [`PORT_POLL_SLICE`] slices so a driver's second kind of message
    /// is picked up as well: a port opened with the `exit_status` option reports
    /// [`PortEvent::Exited`] once its child has been reaped, which is **not** tied to stream
    /// readiness. A due exit status is checked **before** the wait too, so a child reaped
    /// before this call is reported without any polling. The method returns as soon as any
    /// event is available, and with an empty report once `timeout` elapses.
    ///
    /// Each slice is further clamped by the earliest deadline a live driver declares
    /// ([`beam_io::PortRegistry::next_deadline`], the minimum over the drivers'
    /// [`beam_io::Driver::next_timeout`]): a driver-declared timer must fire **at** its
    /// deadline, so the poll may never block past it. The clamp is never negative (a due
    /// deadline waits `0`), and because the very next thing the loop does is run the port
    /// tasks, a due `TIMEOUT` task is serviced immediately — no busy loop, and the slice's
    /// other work is unchanged.
    ///
    /// Nothing at all is polled when no port is live, so a runtime that never opened a port
    /// never sleeps here.
    ///
    /// # Errors
    ///
    /// The poller's own failures ([`BeamError::Io`] when the poll syscall fails) and any
    /// error a port's driver raises while a scheduled task runs.
    fn pump(&mut self, timeout: Duration) -> Result<Vec<PortEvent>> {
        let mut events = self.take_due_exit_statuses()?;
        // The driver ports' bytes may already be queued (a `port_command` on a driver port ran
        // the driver's `output` callback inline), so they are harvested before any wait too —
        // together with the terms a driver's callback emitted (`erl_drv_output_term`).
        events.extend(self.take_driver_outputs());
        events.extend(self.take_driver_terms());
        if self.registry.is_empty() {
            return Ok(events);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            let mut wait = remaining.min(PORT_POLL_SLICE);
            // A driver-declared deadline (a `Driver::next_timeout` mirrored into the port's
            // `TIMEOUT` task) bounds the wait too: without the clamp a timer that is due in a
            // few milliseconds would sit unserviced until the whole slice elapsed. A deadline
            // already past yields a zero wait, and the task round below then runs it.
            if let Some(next) = self.registry.next_deadline() {
                wait = wait.min(next.saturating_duration_since(now));
            }
            let polled = self.poller.poll(Some(wait))?;
            if !polled.is_empty() {
                // Readiness becomes *scheduled tasks* (the ERTS port-task model) — the
                // driver is called when the task runs, never from the event loop.
                self.registry.schedule_events(&polled);
            }
            let outcome = self
                .registry
                .execute_port_tasks(&mut self.poller, PORT_REDS_EXECUTE)?;
            for notification in outcome.notifications {
                match notification {
                    PortNotification::Data { port, bytes } => {
                        if bytes.is_empty() {
                            continue;
                        }
                        // A notification for a port whose metadata is gone (closed while the
                        // task was queued) has no recipient and is dropped.
                        if let Some(meta) = self.meta.get(&port.number()) {
                            // Bytes a port produced are **input** for
                            // `erlang:statistics(io)`, counted where they really arrive.
                            self.io_counters.note_input(bytes.len());
                            events.push(PortEvent::Data {
                                port,
                                bytes,
                                stream: meta.stream,
                                owner: meta.owner,
                            });
                        }
                    }
                    // A close is the caller's own `port_close/1` (which has already dropped
                    // the port's metadata), so there is nobody to notify: an explicit close
                    // produces no message in ERTS either.
                    PortNotification::Closed { .. } => {}
                }
            }
            events.extend(self.take_driver_outputs());
            events.extend(self.take_driver_terms());
            events.extend(self.take_due_exit_statuses()?);
            if !events.is_empty() || Instant::now() >= deadline {
                return Ok(events);
            }
        }
    }

    /// The `{Port, {data, Data}}` events the **C driver** ports have produced.
    ///
    /// A driver port's bytes are what the driver's `output` callback answered with through
    /// `driver_output`; [`driver::DriverPortAdapter`] queues them on the port's input queue, and
    /// this drains it. A `spawn`/`fd` port's input queue is drained by the port-task round instead
    /// ([`beam_io::Port::take_input_notification`], whose `INPUT` task its registered descriptor
    /// makes run), so the two never report the same bytes.
    ///
    /// # Errors
    ///
    /// None: a port that has gone away (closed while the bytes were queued) has no recipient and
    /// its bytes are dropped, exactly as the task round drops a notification for a closed port.
    fn take_driver_outputs(&mut self) -> Vec<PortEvent> {
        let numbers: Vec<u64> = self
            .meta
            .iter()
            .filter(|(_, meta)| meta.kind == PortName::SpawnDriver)
            .map(|(number, _)| *number)
            .collect();
        let mut events = Vec::new();
        for number in numbers {
            let port = PortId::new(number);
            let bytes = match self.registry.get_mut(port) {
                Some(live) => live.drain_input(),
                None => Vec::new(),
            };
            if bytes.is_empty() {
                continue;
            }
            if let Some(meta) = self.meta.get(&number) {
                // Bytes a driver answered with are **input** for
                // `erlang:statistics(io)`, counted where they are harvested.
                self.io_counters.note_input(bytes.len());
                events.push(PortEvent::Data {
                    port,
                    bytes,
                    stream: meta.stream,
                    owner: meta.owner,
                });
            }
        }
        events
    }

    /// The terms the **C driver** ports have emitted (`erl_drv_output_term`/`erl_drv_send_term`).
    ///
    /// A driver's callback decodes its term-data spec on the host's arena and queues the resulting
    /// term on the port's [`driver::TermQueue`] ([`driver::DriverPort::send_term`]); this drains
    /// every live driver port's queue, in emission order. The delivered message **is** the term —
    /// never a `{Port, Term}` wrapper (`io.c:6174`) — and its destination is the port's connected
    /// process for `erl_drv_output_term` (`None`) or the pid `erl_drv_send_term` named. The C
    /// answers `0` (not delivered) for a destination it cannot resolve, so a `to` that is not one
    /// of this node's pids is dropped rather than misdelivered to the owner.
    ///
    /// A port that has gone away (closed while the term was queued) has no metadata and no
    /// recipient, so its terms are dropped exactly as [`PortRegistry::take_driver_outputs`] drops
    /// the bytes of a closed port. A driver's queue is emptied when it is unloaded, so this never
    /// reports a term whose arena is gone (see [`driver::TermQueue`]'s lifetime contract).
    fn take_driver_terms(&mut self) -> Vec<PortEvent> {
        let numbers: Vec<u64> = self
            .meta
            .iter()
            .filter(|(_, meta)| meta.driver_terms.is_some())
            .map(|(number, _)| *number)
            .collect();
        let mut events = Vec::new();
        for number in numbers {
            let Some(meta) = self.meta.get(&number) else {
                continue;
            };
            let Some(queue) = meta.driver_terms.as_ref() else {
                continue;
            };
            let owner = meta.owner;
            for (destination, term) in queue.take() {
                // `None` = the port's connected process (`erl_drv_output_term`); `Some(pid_term)`
                // = the destination `erl_drv_send_term` named. A destination this node does not
                // hold is the C's own "not delivered" (`io.c:5664`), never a misdelivery.
                let to = match destination {
                    None => None,
                    Some(pid_term) => match ProcessId::from_eterm(pid_term) {
                        Ok(pid) => Some(pid),
                        Err(_) => continue,
                    },
                };
                events.push(PortEvent::Term {
                    port: PortId::new(number),
                    to,
                    term,
                    owner,
                });
            }
        }
        events
    }

    /// The `{Port, {exit_status, Status}}` events that are due now.
    ///
    /// Only a port opened with the `exit_status` option is consulted, and
    /// [`beam_io::PortRegistry::take_exit_status`] hands each status out exactly once, so a
    /// second round cannot report the same exit twice.
    ///
    /// # Errors
    ///
    /// [`BeamError::Io`] when the wait itself fails.
    fn take_due_exit_statuses(&mut self) -> Result<Vec<PortEvent>> {
        let numbers: Vec<u64> = self
            .meta
            .iter()
            .filter(|(_, meta)| meta.notify_exit)
            .map(|(number, _)| *number)
            .collect();
        let mut events = Vec::new();
        for number in numbers {
            let port = PortId::new(number);
            if let Some(status) = self.registry.take_exit_status(port)? {
                if let Some(meta) = self.meta.get(&number) {
                    events.push(PortEvent::Exited {
                        port,
                        status,
                        owner: meta.owner,
                    });
                }
            }
        }
        Ok(events)
    }

    /// The `{Item, Value}` pairs of `port` — the `port_info/1,2` answer.
    ///
    /// Every item atom is resolved through `atoms` (the runtime's one shared table). A port
    /// the registry does not hold answers with an **empty** list — never a fabricated pair,
    /// and never `badarg`: the oracle's `erlang:port_info/1,2` answer `undefined` for a
    /// closed port, which is exactly what an empty list renders as through the wrapper
    /// (see the module docs and `RuntimeContext::port_info_closed_port`).
    ///
    /// The list is the oracle's answer for **both** arities: `beam_bifs`'s
    /// `erlang:port_info/1` keeps the seven `/1` items and drops the final `monitors`
    /// pair, while `erlang:port_info/2` searches them all. The order and the value shapes
    /// are the ones erts-17.1 reports (see the module docs).
    ///
    /// # Errors
    ///
    /// [`BeamError::TermNotFound`] for an item atom missing from the seed (a bug, not a
    /// runtime condition — the list is [`PORT_INFO_ATOMS`]); the arena's allocation failure
    /// when a value does not fit.
    fn info(&mut self, port: PortId, atoms: &AtomTable) -> Result<Vec<(Eterm, Eterm)>> {
        let Some(meta) = self.meta.get(&port.number()) else {
            return Ok(Vec::new());
        };
        let item = |name: &str| -> Result<Eterm> {
            atoms
                .lookup(name)
                .map(make_atom)
                .ok_or(BeamError::TermNotFound)
        };
        // `self.meta` and `self.heap` are disjoint fields, so the immutable borrow of the
        // port and the mutable borrow of the arena coexist.
        let name = charlist(&mut self.heap, &meta.command)?;
        // The owning process is the port's only link (`[]`-shaped as `[Owner]`) and its
        // `connected` value — both the oracle's answer while the port is live.
        let owner = meta.owner.to_eterm();
        let links = pid_list(&mut self.heap, &[meta.owner])?;
        let monitors = monitor_list(&mut self.heap, atoms, &meta.monitors)?;
        // `id` is a **bare small integer** (the oracle's shape), not the port term.
        let id = try_make_small(i64::try_from(port.number()).map_err(|_| BeamError::SystemLimit)?)
            .map_err(|_| BeamError::SystemLimit)?;
        // `input`/`output` are byte counters; the runtime keeps no per-port totals, so both
        // are `0` — which is what the oracle reports right after `open_port`.
        let input = make_small(0);
        let output = make_small(0);
        let os_pid = match meta.os_pid {
            Some(pid) => try_make_small(i64::from(pid)).map_err(|_| BeamError::SystemLimit)?,
            // `port_info(Port, os_pid)` is `undefined` for a port without an OS process —
            // the oracle's own answer.
            None => item("undefined")?,
        };
        Ok(vec![
            (item("name")?, name),
            (item("links")?, links),
            (item("id")?, id),
            (item("connected")?, owner),
            (item("input")?, input),
            (item("output")?, output),
            (item("os_pid")?, os_pid),
            // The `/2`-only extra item; `beam_bifs`'s `port_info/1` drops it.
            (item("monitors")?, monitors),
        ])
    }
}

/// The validated `Name` of an `open_port/2` call.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NameSpec {
    /// Which driver backs the port.
    kind: PortName,
    /// The command line (or path) `port_info(Port, name)` reports.
    command: String,
    /// The adopted descriptor of a `{fd, N}` name.
    fd: Option<RawFd>,
    /// The inline option list of `{spawn_executable, Path, Opts}`, if given.
    inline_options: Option<Eterm>,
}

impl NameSpec {
    /// The term-free [`beam_io::SpawnSpec`] this validated name describes.
    ///
    /// A `{spawn, Cmd}` port runs through the shell with an `exec` prefix (`/bin/sh -c
    /// "exec Cmd"`): the oracle's `spawn` driver replaces the shell with the command, so
    /// `{spawn,"echo a; echo b"}` produces only `"a\n"` and `{spawn,"exit 3"}` reports
    /// `127`. `PortMeta::command` keeps the **raw** flattened command so
    /// `port_info(Port, name)` still reports `"true"`/`"cat"`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a `{fd, N}` name without a validated descriptor (a
    /// programming error — [`RuntimePorts::name_spec`] always records one).
    fn spawn_spec(&self) -> Result<SpawnSpec> {
        match self.kind {
            PortName::Spawn => Ok(SpawnSpec::Spawn {
                command: format!("exec {}", self.command),
                argv: None,
            }),
            PortName::SpawnExecutable => Ok(SpawnSpec::SpawnExecutable {
                path: self.command.clone(),
            }),
            PortName::Fd => Ok(SpawnSpec::Fd {
                fd: self.fd.ok_or(BeamError::BadArg)?,
            }),
            // A `{spawn_driver, Name}` port is backed by a **C driver**, not by the spawn
            // driver: `open_port` registers a `driver::DriverPortAdapter` for it and never asks
            // for a spawn spec, so reaching this is a programming error.
            PortName::SpawnDriver => Err(BeamError::BadArg),
        }
    }
}

/// The validated option list of an `open_port/2` call.
///
/// `stream` is an `Option` so that overlaying the inline options of a
/// `{spawn_executable, Path, Opts}` name with the call's own `Options` can tell "no stream
/// asked for" from "`list` asked for".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PortOptions {
    /// The `binary`/`list` stream choice.
    stream: Option<StreamType>,
    /// The `{cd, Dir}` working directory.
    cd: Option<String>,
    /// The `{env, [{Name, Value}]}` environment additions.
    env: Vec<(String, String)>,
    /// The `{args, [String]}` arguments of a `spawn_executable` port.
    args: Vec<String>,
    /// The `exit_status` flag: the driver queues a `{Port, {exit_status, Status}}` message
    /// once the child has been reaped.
    exit_status: bool,
}

impl PortOptions {
    /// The effective stream type (`list` unless `binary` was asked for — the oracle's
    /// default).
    fn stream(&self) -> StreamType {
        self.stream.unwrap_or(StreamType::List)
    }

    /// Applies `other` on top of `self`: whatever `other` explicitly set wins, and the
    /// flags accumulate. This is how the call's `Options` overlay the inline options of a
    /// `{spawn_executable, Path, Opts}` name.
    fn overlay(&mut self, other: PortOptions) {
        if other.stream.is_some() {
            self.stream = other.stream;
        }
        if other.cd.is_some() {
            self.cd = other.cd;
        }
        if !other.args.is_empty() {
            self.args = other.args;
        }
        self.env.extend(other.env);
        // A flag: asking anywhere in either option list asks for it.
        self.exit_status |= other.exit_status;
    }
}

/// The concrete [`PortHook`]: validates `open_port/2` arguments, hands the term-free
/// result to `beam_io`'s `spawn` driver (which really starts the OS process), and answers
/// the port BIFs from the runtime's [`PortRegistry`].
///
/// One is built per BIF call (see the module docs). It owns nothing but two cheap `Rc`
/// clones and the owning pid, so it never borrows the [`Runtime`](crate::Runtime).
pub struct RuntimePorts {
    /// The shared port state.
    registry: Rc<RefCell<PortRegistry>>,
    /// A handle to the runtime's **one shared** atom table, used to resolve (never to
    /// mint) the `port_info/1` item atoms.
    ///
    /// Interning *through* this handle would be wrong: it is an `Rc` clone of the shared
    /// table, so `Rc::make_mut` would clone it and mint an index the runtime's own table
    /// does not have, and the handle is dropped when the call ends. The names are instead
    /// part of the boot seed ([`PORT_INFO_ATOMS`]) and only ever looked up here.
    atoms: Rc<AtomTable>,
    /// The process the port is opened **for** — what `PortMeta::owner` records, so the
    /// port's `{Port, {data, …}}` and `{Port, {exit_status, …}}` messages can be delivered
    /// to the process that opened it.
    owner: ProcessId,
}

impl RuntimePorts {
    /// A provider over `registry` that resolves its item atoms in `atoms` and records
    /// `owner` as the process every port it opens belongs to.
    pub fn new(
        registry: Rc<RefCell<PortRegistry>>,
        atoms: Rc<AtomTable>,
        owner: ProcessId,
    ) -> RuntimePorts {
        RuntimePorts {
            registry,
            atoms,
            owner,
        }
    }

    /// Borrows the shared registry for one operation.
    ///
    /// A nested borrow (a port operation re-entered from inside a port operation) is
    /// impossible today — no port operation calls back into the runtime — but it is
    /// reported as a typed capability gap rather than panicking.
    fn registry_mut(&self) -> Result<RefMut<'_, PortRegistry>> {
        self.registry
            .try_borrow_mut()
            .map_err(|_| error::unsupported("the port table is already in use"))
    }

    /// The name of `term`, when `term` is an atom interned in the shared table.
    fn atom_name(&self, term: Eterm) -> Option<&str> {
        if !is_atom(term) {
            return None;
        }
        self.atoms.name(atom_val(term))
    }

    /// `term` as an Erlang string — a char list of bytes, a binary, or an atom's own name.
    ///
    /// Everything else is [`BeamError::BadArg`], and so is a char list that is not a byte
    /// string or whose bytes are not UTF-8 (a `String` cannot carry the latter, and
    /// silently mangling a command would be worse than rejecting it).
    fn string_value(&self, term: Eterm) -> Result<String> {
        if let Ok(bytes) = terms::binary_bytes(term) {
            return String::from_utf8(bytes.to_vec()).map_err(|_| BeamError::BadArg);
        }
        if let Ok(elements) = terms::list_elements(term) {
            let mut bytes = Vec::with_capacity(elements.len());
            for element in elements {
                if !is_small(element) {
                    return Err(BeamError::BadArg);
                }
                bytes.push(u8::try_from(signed_value(element)).map_err(|_| BeamError::BadArg)?);
            }
            return String::from_utf8(bytes).map_err(|_| BeamError::BadArg);
        }
        if let Some(name) = self.atom_name(term) {
            return Ok(name.to_string());
        }
        Err(BeamError::BadArg)
    }

    /// The `Name` of an `open_port/2` call: `{spawn, Cmd}`, `{spawn_executable, Path}`
    /// (optionally with an inline option list) or `{fd, N}`.
    ///
    /// Anything else — a bare atom, a tuple with another tag or arity, a `Cmd` that is not
    /// a string/atom/argv list, an **empty** command/path (the oracle's own name validation
    /// rejects it with `badarg`; see [`checked_name`]), a negative or non-integer descriptor
    /// — is [`BeamError::BadArg`].
    fn name_spec(&self, name: Eterm) -> Result<NameSpec> {
        let arity = terms::tuple_arity(name)?;
        let tag = terms::tuple_element(name, 0)?;
        let tag = self.atom_name(tag).ok_or(BeamError::BadArg)?;
        match (tag, arity) {
            ("spawn", 2) => {
                let command = self.command_spec(terms::tuple_element(name, 1)?)?;
                Ok(NameSpec {
                    kind: PortName::Spawn,
                    command: checked_name(command)?,
                    fd: None,
                    inline_options: None,
                })
            }
            ("spawn_executable", 2) => {
                let command = self.string_value(terms::tuple_element(name, 1)?)?;
                Ok(NameSpec {
                    kind: PortName::SpawnExecutable,
                    command: checked_name(command)?,
                    fd: None,
                    inline_options: None,
                })
            }
            ("spawn_executable", 3) => {
                let command = self.string_value(terms::tuple_element(name, 1)?)?;
                Ok(NameSpec {
                    kind: PortName::SpawnExecutable,
                    command: checked_name(command)?,
                    fd: None,
                    inline_options: Some(terms::tuple_element(name, 2)?),
                })
            }
            ("spawn_driver", 2) => {
                // The `Name` is flattened like `{spawn, …}`'s (an atom, a binary or a
                // (possibly nested) char list); anything else is `badarg`. The driver is
                // looked up under the prefix up to the first space — the C's own rule
                // (`io.c` walks the name to the first blank before matching `driver_name`),
                // which is also how a name can carry arguments for the driver.
                let command = self.command_spec(terms::tuple_element(name, 1)?)?;
                Ok(NameSpec {
                    kind: PortName::SpawnDriver,
                    command: checked_name(command)?,
                    fd: None,
                    inline_options: None,
                })
            }
            ("fd", 2) => {
                let descriptor = terms::tuple_element(name, 1)?;
                // A descriptor is a non-negative integer; anything else the driver cannot
                // accept.
                if !is_small(descriptor) || signed_value(descriptor) < 0 {
                    return Err(BeamError::BadArg);
                }
                let fd = i32::try_from(signed_value(descriptor)).map_err(|_| BeamError::BadArg)?;
                Ok(NameSpec {
                    kind: PortName::Fd,
                    command: format!("fd:{fd}"),
                    fd: Some(fd),
                    inline_options: None,
                })
            }
            _ => Err(BeamError::BadArg),
        }
    }

    /// The command of a `{spawn, Cmd}` name, **flattened** to a single shell command line.
    ///
    /// The oracle has no argv form for `{spawn, …}`: whatever the value's shape — an atom, a
    /// binary, a char list, or an iolist of those — it is flattened to one byte string and
    /// run through the shell (`{spawn,["/bin/echo","hi"]}` is the command `/bin/echohi`, no
    /// space inserted). The flattened command is what `port_info(Port, name)` reports.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a value that is not an atom/binary/char list/iolist, or
    /// whose bytes are not UTF-8 (the command must be a `String`).
    fn command_spec(&self, value: Eterm) -> Result<String> {
        let mut bytes = Vec::new();
        self.flatten_command(value, &mut bytes)?;
        String::from_utf8(bytes).map_err(|_| BeamError::BadArg)
    }

    /// Appends the bytes of `value` — an atom, a binary, or a (possibly nested) list of
    /// those — to `out`, recursively.
    fn flatten_command(&self, value: Eterm, out: &mut Vec<u8>) -> Result<()> {
        if let Some(name) = self.atom_name(value) {
            out.extend_from_slice(name.as_bytes());
            return Ok(());
        }
        if let Ok(bytes) = terms::binary_bytes(value) {
            out.extend_from_slice(bytes);
            return Ok(());
        }
        for element in terms::list_elements(value)? {
            if is_small(element) {
                out.push(u8::try_from(signed_value(element)).map_err(|_| BeamError::BadArg)?);
            } else {
                self.flatten_command(element, out)?;
            }
        }
        Ok(())
    }

    /// The validated option list of an `open_port/2` call.
    ///
    /// A **strict whitelist**, matching the oracle (`erts/emulator/beam/erl_bif_port.c`):
    /// any option the driver cannot accept is [`BeamError::BadArg`], never silently ignored.
    /// The accepted option **atoms** are [`PORT_OPTION_ATOMS`] and the accepted option
    /// **tuple tags** are [`PORT_OPTION_TUPLES`]; anything outside them — including the
    /// `list` atom (a list stream is already the default) — is `badarg`.
    ///
    /// `binary` (the stream shape), `exit_status`, `{cd, Dir}`, `{env, [{Name, Value}]}` and
    /// `{args, [String]}` are honored; every other whitelisted option is recognized and
    /// accepted **without effect** here (`eof`, `stream`, `use_stdio`, `in`, `out`, `hide`,
    /// `line`, `{packet, N}`, `{parallelism, Bool}`, …). A whitelisted tuple with the wrong
    /// arity, or a recognized option with a malformed value (`{cd, 5}`, `{args, foo}`), is
    /// [`BeamError::BadArg`] — the drivers cannot accept those.
    fn port_options(&self, options: Eterm) -> Result<PortOptions> {
        let mut parsed = PortOptions::default();
        for element in terms::list_elements(options)? {
            if is_atom(element) {
                let name = self.atom_name(element).ok_or(BeamError::BadArg)?;
                // `list` and every other non-whitelisted atom (a driver option this runtime
                // does not know) are rejected: the oracle's strict list has no tolerance.
                if !PORT_OPTION_ATOMS.contains(&name) {
                    return Err(BeamError::BadArg);
                }
                match name {
                    "binary" => parsed.stream = Some(StreamType::Binary),
                    // The driver queues a `{Port, {exit_status, Status}}` message once the
                    // child has been reaped — the pump reports it.
                    "exit_status" => parsed.exit_status = true,
                    // Whitelisted, accepted, without effect here (see the module docs).
                    _ => {}
                }
                continue;
            }
            let arity = terms::tuple_arity(element)?;
            let tag = self.atom_name(terms::tuple_element(element, 0)?).ok_or(BeamError::BadArg)?;
            // A tag outside the whitelist is not a usable option.
            if !PORT_OPTION_TUPLES.contains(&tag) {
                return Err(BeamError::BadArg);
            }
            // Every whitelisted tuple option is a 2-tuple (`{packet, N}`, `{cd, Dir}`, …); a
            // whitelisted tag with the wrong arity is not a usable option.
            if arity != 2 {
                return Err(BeamError::BadArg);
            }
            match tag {
                "cd" => {
                    parsed.cd = Some(self.string_value(terms::tuple_element(element, 1)?)?);
                }
                "env" => {
                    parsed.env = self.env_list(terms::tuple_element(element, 1)?)?;
                }
                "args" => {
                    parsed.args = self.arg_list(terms::tuple_element(element, 1)?)?;
                }
                // Whitelisted, accepted, without effect here.
                _ => {}
            }
        }
        Ok(parsed)
    }

    /// The `{env, [{Name, Value}]}` list: each name/value is a string or an atom.
    fn env_list(&self, term: Eterm) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for entry in terms::list_elements(term)? {
            if terms::tuple_arity(entry)? != 2 {
                return Err(BeamError::BadArg);
            }
            let name = self.string_value(terms::tuple_element(entry, 0)?)?;
            let value = self.string_value(terms::tuple_element(entry, 1)?)?;
            out.push((name, value));
        }
        Ok(out)
    }

    /// The `{args, [String]}` list: each element is a string or an atom.
    fn arg_list(&self, term: Eterm) -> Result<Vec<String>> {
        terms::list_elements(term)?
            .into_iter()
            .map(|element| self.string_value(element))
            .collect()
    }
}

impl PortHook for RuntimePorts {
    fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId> {
        // Validation first: a Name/Options the drivers cannot accept is `badarg` and must
        // not consume a port number.
        let spec = self.name_spec(name)?;
        // The call's own `Options` overlay the inline options of a
        // `{spawn_executable, Path, Opts}` name.
        let mut resolved = match spec.inline_options {
            Some(inline) => self.port_options(inline)?,
            None => PortOptions::default(),
        };
        resolved.overlay(self.port_options(options)?);

        // `beam_io`'s driver constructor really starts the child (or adopts `{fd, N}`)
        // before anything is registered anywhere, so a failure here cannot leak an id; the
        // OS pid the driver reports is what `port_info(Port, os_pid)` answers.
        let stream = resolved.stream();
        if spec.kind == PortName::SpawnDriver {
            // A **C driver** (contract A.4) port: the driver is `dlopen`ed and registered as the
            // port's driver, so `port_command` reaches its `output` callback and its
            // `driver_output` bytes reach the owner. An unknown driver name is the oracle's own
            // `error:badarg` (`io.c`'s `ERTS_OPEN_DRIVER_RET(NULL, -3, BADARG)`), never a
            // fabricated port and never a silent no-op.
            let (candidates, driver_name) = driver_lookup(&spec.command);
            let host = driver::DriverPort::new(
                Heap::new(PORT_HEAP_WORDS)?,
                Rc::clone(&self.atoms),
                self.owner,
            );
            // Seize the driver's term queue **before** the host is moved into the port lifecycle
            // (the boxed driver exposes no host): it is the handle a driver callback's
            // `erl_drv_output_term`/`erl_drv_send_term` pushes onto and
            // [`PortRegistry::take_driver_terms`] drains.
            let driver_terms = host.term_queue();
            let adapter =
                driver::DriverPortAdapter::load(&candidates, &driver_name, host, spec.command.clone())
                    .map_err(|_| BeamError::BadArg)?;
            let meta = PortMeta {
                command: spec.command,
                // A driver port owns no OS child; `port_info(Port, os_pid)` is `undefined`.
                os_pid: None,
                stream,
                // Nothing to reap, so no `{Port, {exit_status, …}}` is ever synthesized.
                notify_exit: false,
                monitors: Vec::new(),
                owner: self.owner,
                kind: PortName::SpawnDriver,
                driver_terms: Some(driver_terms),
            };
            let mut registry = self.registry_mut()?;
            return registry.register(Box::new(adapter), meta);
        }
        let spawn_options = SpawnOptions {
            stream: stream.into(),
            cd: resolved.cd,
            env: resolved.env,
            args: resolved.args,
            exit_status: resolved.exit_status,
        };
        let driver = SpawnDriver::new(&spec.spawn_spec()?, &spawn_options)?;
        let os_pid = driver.os_pid();
        let meta = PortMeta {
            command: spec.command,
            os_pid,
            stream,
            notify_exit: spawn_options.exit_status,
            monitors: Vec::new(),
            owner: self.owner,
            kind: spec.kind,
            // A `spawn`/`fd` port has no C driver, so no driver term queue exists.
            driver_terms: None,
        };
        // The registry stays borrowed across `register`: starting a port cannot re-enter a
        // port operation, and a nested borrow is a typed error rather than a panic.
        let mut registry = self.registry_mut()?;
        registry.register(Box::new(driver), meta)
    }

    fn close_port(&mut self, port: PortId) -> Result<()> {
        let mut registry = self.registry_mut()?;
        // An unknown port is `badarg` — never a silently dropped close.
        if !registry.is_live(port) {
            return Err(BeamError::BadArg);
        }
        // [`PortRegistry::close`] is also the port-exit path: it notifies the driver
        // ([`PortExitReason::Closed`], at most once) before the teardown, so this hook needs
        // no notification of its own.
        registry.close(port)
    }

    fn port_info(&mut self, port: PortId) -> Result<Vec<(Eterm, Eterm)>> {
        let atoms = Rc::clone(&self.atoms);
        let mut registry = self.registry_mut()?;
        registry.info(port, &atoms)
    }
}

impl Runtime {
    /// The watcher pids currently monitoring `port`, read from the runtime's **authoritative**
    /// port table ([`PortRegistry::port_table`] — the same `beam_process::PortTable`
    /// `open_port/2` allocates from, `erlang:monitor(port, Port)` records an incoming
    /// [`PortMonitor`](beam_process::PortMonitor) on, and a port close drains).
    ///
    /// This is the live **incoming** monitor set for the port: the entries
    /// [`ProcessTable::monitor_port`](beam_process::ProcessTable::monitor_port) adds for each
    /// `erlang:monitor(port, Port)` watch and the dead-watcher sweep
    /// ([`Runtime::drain_port_monitor_exits`](crate::runtime::ets_owner)) removes when the
    /// watching process dies first. It is the observable used by
    /// `tests/port_monitor_cleanup.rs`; the port table sits behind a private
    /// `Rc<RefCell<PortRegistry>>`, so there is no other public path to it.
    ///
    /// # Errors
    ///
    /// The typed `unsupported` capability gap when the port registry is already borrowed (a
    /// nested port operation) — the same rule every other port accessor of this crate
    /// follows, never a panic.
    pub fn port_monitors(&self, port: PortId) -> Result<Vec<ProcessId>> {
        // The `Rc` is cloned **first**, exactly like `crate::native::process::monitor`'s
        // `with_port_table`, so the borrow is of the clone and a nested borrow is a typed gap
        // rather than a panic.
        let cell = Rc::clone(&self.ports);
        let registry = cell
            .try_borrow()
            .map_err(|_| error::unsupported("the port table is already in use"))?;
        Ok(registry
            .port_table()
            .monitors(port)
            .iter()
            .map(|monitor| monitor.watcher)
            .collect())
    }

    /// Drives the port poller for up to `timeout` and **delivers** every message the ports
    /// produced into its owning process's mailbox in `table`, returning how many were
    /// delivered.
    ///
    /// This is the scheduler path's port half: the runtime's main loop calls it once the
    /// run queue is drained, so a process blocked in a compiled `receive` waiting for
    /// `{Port, {data, …}}` is woken through `beam_process`'s own deliver path (the
    /// `ProcessTable` wake list, which `Scheduler::resume_woken` then resumes). The message
    /// is `copy_struct`-ed into the recipient's heap by `deliver_simple`, so the transient
    /// term built here on the runtime's arena is not shared.
    ///
    /// Nothing is polled when no port is live (so a runtime that never opened a port pays
    /// nothing), and a message whose owner is no longer in `table` is skipped rather than
    /// treated as an error.
    ///
    /// # Errors
    ///
    /// The poller's/driver's failures, a nested borrow of the port table (a typed
    /// capability gap), the term builders' allocation failures, and the deliver path's own
    /// errors.
    pub(crate) fn deliver_port_messages(
        &mut self,
        table: &mut ProcessTable,
        timeout: Duration,
    ) -> Result<usize> {
        // No live port ⇒ nothing to wait for: never sleep.
        {
            let registry = self
                .ports
                .try_borrow()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            if registry.is_empty() {
                return Ok(0);
            }
        }
        let events = {
            let mut registry = self
                .ports
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.pump(timeout)?
        };
        let mut delivered = 0;
        for event in events {
            // The term is built on the runtime's arena (the `RuntimeContext` the builders
            // need) *after* the registry borrow has been released — the same snapshot rule
            // the `-eval` poller follows. A driver-emitted term is stabilised the same way:
            // it was built on the *driver host's* arena, so it is copied onto the
            // pointer-stable `eval_heap` before the `deliver_simple` copy reads it, and can
            // never dangle when the driver is unloaded.
            let (recipient, from, message) = {
                let mut ctx = RuntimeContext::new(&mut *self);
                match &event {
                    PortEvent::Data {
                        port,
                        bytes,
                        stream,
                        owner,
                    } => (
                        *owner,
                        port.to_eterm(),
                        ctx.port_data_message(*port, bytes, *stream)?,
                    ),
                    PortEvent::Exited { port, status, owner } => (
                        *owner,
                        port.to_eterm(),
                        ctx.port_exit_message(*port, *status)?,
                    ),
                    PortEvent::Term {
                        port,
                        to,
                        term,
                        owner,
                    } => (
                        // `erl_drv_output_term` targets the port's owner; `erl_drv_send_term`
                        // names its destination explicitly (`io.c:5664`).
                        to.unwrap_or(*owner),
                        port.to_eterm(),
                        ctx.stabilize_driver_term(*term)?,
                    ),
                }
            };
            // A port whose recipient has since terminated has no recipient: the message is
            // dropped, not an error.
            if !table.contains(recipient) {
                continue;
            }
            table.deliver_simple(recipient, from, message)?;
            delivered += 1;
        }
        Ok(delivered)
    }
}

impl RuntimeContext<'_> {
    /// Reads any pending port-stream output as `{Port, {data, Data}}` messages.
    ///
    /// This is the `-eval` `receive … after T` path's port half: it waits up to `timeout`
    /// (in [`PORT_POLL_SLICE`] slices — see [`PortRegistry::pump`]) for the live ports to
    /// produce something and builds the messages **on the runtime's pointer-stable arena**,
    /// so the caller can match them against its `receive` clauses. The `data` atom is
    /// interned in the runtime's **one shared** table, so the message is nameable by the
    /// printer and comparable by the evaluator's own clauses.
    ///
    /// `Ok(vec![])` means nothing arrived within `timeout`; nothing is polled at all when no
    /// port is live (a receive with no port never sleeps here).
    ///
    /// The scheduler path uses [`Runtime::deliver_port_messages`] instead, because there the
    /// messages belong in a process's mailbox rather than in the caller's hands.
    ///
    /// # Errors
    ///
    /// The poller's/driver's failures; a nested borrow of the port table is a typed
    /// capability gap rather than a panic; the arena's allocation failure when a term does
    /// not fit (including the documented >`ERL_ONHEAP_BINARY_LIMIT` *unsupported* gap for a
    /// wide `binary`-stream read).
    pub(crate) fn poll_port_messages(&mut self, timeout: Duration) -> Result<Vec<Eterm>> {
        // The shared port state is cloned out so the *messages* — which need `&mut self` —
        // are never built while the registry is borrowed (the same snapshot rule
        // `native.rs` follows).
        let registry = Rc::clone(&self.runtime.ports);
        let events = {
            let mut registry = registry
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.pump(timeout)?
        };
        let mut messages = Vec::with_capacity(events.len());
        for event in events {
            match event {
                PortEvent::Data {
                    port, bytes, stream, ..
                } => messages.push(self.port_data_message(port, &bytes, stream)?),
                PortEvent::Exited { port, status, .. } => {
                    messages.push(self.port_exit_message(port, status)?)
                }
                // A driver-emitted term is the message itself, never a `{Port, Term}` wrapper
                // (`io.c:6174`). The evaluator can only observe its **own** process's mailbox, so
                // a term the driver addressed to a *different* process is not returned here — the
                // scheduler path ([`Runtime::deliver_port_messages`]) delivers it into that
                // process's mailbox instead — rather than being matched as if it were the
                // evaluator's own.
                PortEvent::Term { to, term, owner, .. } => {
                    if to.unwrap_or(owner) != self.pid() {
                        continue;
                    }
                    messages.push(self.stabilize_driver_term(term)?);
                }
            }
        }
        Ok(messages)
    }

    /// Copies a driver-emitted term onto the runtime's pointer-stable arena.
    ///
    /// A term a C driver emitted (`erl_drv_output_term`/`erl_drv_send_term`) is decoded on the
    /// **driver host's** arena, which is freed when the driver is unloaded. It is copied onto
    /// [`Runtime::eval_heap`](crate::runtime::Runtime) — runtime-lifetime and never reset — before
    /// it is delivered into a mailbox or returned to the `-eval` evaluator, exactly as the
    /// `{Port, {data, …}}` builders already allocate their message on the runtime's arena. An
    /// immediate is returned unchanged (the copier shares it).
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the term does not fit, translated to the emulator's
    /// documented `error:system_limit` — never a dangling term (see
    /// [`Runtime::stabilize_term`](crate::runtime::Runtime)).
    pub(crate) fn stabilize_driver_term(&mut self, term: Eterm) -> Result<Eterm> {
        self.runtime.stabilize_term(term)
    }

    /// The `{Port, {data, Data}}` message of one port-stream read.
    ///
    /// `Data` is an Erlang **binary** for a `binary` stream and a char list otherwise (the
    /// `list` default, `erl`'s own). The `data` atom is interned into the runtime's one
    /// shared table, so the message is nameable by the printer and matchable by a `receive`
    /// clause.
    pub(crate) fn port_data_message(
        &mut self,
        port: PortId,
        bytes: &[u8],
        stream: StreamType,
    ) -> Result<Eterm> {
        let data = make_atom(self.runtime.code_server.atoms_mut().intern("data"));
        let payload = match stream {
            StreamType::Binary => terms::make_heap_binary(self, bytes)?,
            StreamType::List => {
                let mut list = NIL;
                for byte in bytes.iter().rev() {
                    list = terms::make_cons(self, make_small(i64::from(*byte)), list)?;
                }
                list
            }
        };
        let inner = terms::make_tuple(self, &[data, payload])?;
        terms::make_tuple(self, &[port.to_eterm(), inner])
    }

    /// The `{Port, {exit_status, Status}}` message of a port whose child has been reaped,
    /// when that port's `exit_status` option asked for it.
    ///
    /// This is the `spawn` driver's exit notification (legacy C:
    /// `erts/emulator/drivers/unix/spawn_driver.c` queues `{exit_status, Status}` when
    /// `waitpid` reports the child gone). The status itself is handed out exactly once by
    /// [`beam_io::PortRegistry::take_exit_status`], so a second `receive` cannot see the same
    /// exit twice. The sibling of [`Self::port_data_message`], with the same
    /// interned-atom/pointer-stable-arena rule.
    pub(crate) fn port_exit_message(&mut self, port: PortId, status: i32) -> Result<Eterm> {
        let tag = make_atom(self.runtime.code_server.atoms_mut().intern("exit_status"));
        let inner = terms::make_tuple(self, &[tag, make_small(i64::from(status))])?;
        terms::make_tuple(self, &[port.to_eterm(), inner])
    }
}

/// The driver search a `{spawn_driver, Name}` name performs: the candidate object paths to try
/// and the driver name the loaded entry must declare.
///
/// The C looks the name's prefix up to the first space up in its **loaded-driver list**
/// (`io.c:599-622`: it walks the name to the first blank, then compares it against each
/// `ErlDrvEntry::driver_name`); the `Name` itself is never a path, so the file must already be
/// loaded by `erl_ddll:load_driver/3`/`erlang:load_driver/2`. This runtime has no such BIF yet, so
/// the name doubles as the *search* — a documented Milestone-1 extension:
///
/// * a name that contains `/` is taken as the object's path, and the driver name validated against
///   the entry is the path's file stem with the platform suffix removed (`…/t` and `…/t.so` both
///   name the driver `t`). This is what lets a driver outside the loader's search path be reached;
/// * a bare name is tried as itself, as `<name>.so` and as `./<name>` (the runtime's own
///   directory), with `<name>` as the driver name — the C's answer for a name in its search path.
///
/// `load_driver` appends the platform's shared-object suffix to a candidate that does not already
/// carry it, so the `<name>` candidate resolves `<name>.so`. A name that reaches no object, or an
/// object whose entry declares another name, is [`BeamError::BadArg`] at the call site — the oracle's
/// own reason for an unknown driver.
fn driver_lookup(name: &str) -> (Vec<String>, String) {
    // The C's own truncation: the driver is the name up to the first blank (`io.c`).
    let path = name.split(' ').next().unwrap_or(name);
    let mut candidates = vec![path.to_string(), format!("{path}.so")];
    let driver_name = if path.contains('/') {
        let base = std::path::Path::new(path)
            .file_name()
            .and_then(|stem| stem.to_str())
            .unwrap_or(path);
        base.strip_suffix(".so").unwrap_or(base).to_string()
    } else {
        // `dlopen` searches the loader's path for a name without a slash, never the current
        // directory, so the runtime's own directory is an explicit candidate.
        candidates.push(format!("./{path}"));
        path.to_string()
    };
    (candidates, driver_name)
}

/// The validated **non-empty** command/path a `{spawn, Cmd}`/`{spawn_executable, Path}` name
/// must carry.
///
/// The oracle rejects an empty name with `error:badarg` in its own validation of the
/// `open_port/2` `Name` (`erts_internal_open_port_2`), *before* any driver is reached:
/// `open_port({spawn,""},[])` and `open_port({spawn_executable,""},[])` are both
/// `error:badarg` (verified against erts-17.1), while `{spawn," "}` is accepted. Letting an
/// empty name through would reach `beam_io`'s spawn driver, whose `exec` of an empty program
/// maps to `enoent` — the wrong reason for the same call. Any non-empty name is passed
/// through unchanged.
///
/// # Errors
///
/// [`BeamError::BadArg`] for an empty name.
fn checked_name(command: String) -> Result<String> {
    if command.is_empty() {
        Err(BeamError::BadArg)
    } else {
        Ok(command)
    }
}

/// `text` as an Erlang string (a char list of bytes), built on `heap`.
///
/// # Errors
///
/// Propagates the arena's allocation failure.
fn charlist(heap: &mut Heap, text: &str) -> Result<Eterm> {
    let mut list = NIL;
    for byte in text.bytes().rev() {
        list = heap.cons(make_small(i64::from(byte)), list)?;
    }
    Ok(list)
}

/// `pids` as the Erlang list `[Pid]` — the `port_info(Port, links)` shape.
///
/// # Errors
///
/// Propagates the arena's allocation failure.
fn pid_list(heap: &mut Heap, pids: &[ProcessId]) -> Result<Eterm> {
    let mut list = NIL;
    for pid in pids.iter().rev() {
        list = heap.cons(pid.to_eterm(), list)?;
    }
    Ok(list)
}

/// `pids` as the Erlang list `[{process, Pid}]` — the oracle's `port_info(Port, monitors)`
/// shape.
///
/// # Errors
///
/// [`BeamError::TermNotFound`] when the `process` atom is missing from the seed (a bug);
/// the arena's allocation failure.
fn monitor_list(heap: &mut Heap, atoms: &AtomTable, pids: &[ProcessId]) -> Result<Eterm> {
    let process = atoms
        .lookup("process")
        .map(make_atom)
        .ok_or(BeamError::TermNotFound)?;
    let mut list = NIL;
    for pid in pids.iter().rev() {
        let entry = heap.tuple(&[process, pid.to_eterm()])?;
        list = heap.cons(entry, list)?;
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_io::DriverContext;
    use beam_process::SignalAtoms;

    use crate::boot;
    use crate::config::RuntimeConfig;

    /// The process every port the fixture's hook opens claims as its owner.
    const OWNER: ProcessId = ProcessId::new(7, 0);

    /// A test fixture: the shared registry, the atom table both the provider and the test
    /// resolve names through, the provider itself, and a term heap.
    ///
    /// The atom table is interned **before** the provider takes its handle: the handle is
    /// an `Rc` clone, so mutating the table afterwards would fork it (see the module docs).
    struct Fixture {
        registry: Rc<RefCell<PortRegistry>>,
        atoms: Rc<AtomTable>,
        hook: RuntimePorts,
        heap: Heap,
    }

    /// The vocabulary the tests build `Name`/`Options` terms out of, and the item atoms.
    ///
    /// The item atoms (`name`, `id`, `connected`, `os_pid`, `links`, `input`, `output`,
    /// `monitors`) come from the boot seed; the option atoms do not, so the whole
    /// `open_port/2` option whitelist is interned here too.
    const TEST_ATOMS: &[&str] = &[
        "spawn",
        "spawn_executable",
        "fd",
        "not_a_driver",
        // The honored option tags.
        "cd",
        "env",
        "args",
        // The full option-atom whitelist and its rejected neighbours.
        "stream",
        "use_stdio",
        "stderr_to_stdout",
        "line",
        "nouse_stdio",
        "binary",
        "list",
        "in",
        "out",
        "eof",
        "hide",
        "exit_status",
        "overlapped_io",
        // The option-tuple whitelist and its rejected neighbours.
        "packet",
        "arg0",
        "parallelism",
        "busy_limits_port",
        "busy_limits_msgq",
        "nope",
    ];

    fn fixture() -> Fixture {
        let mut atoms = AtomTable::new();
        boot::seed_core_atoms(&mut atoms);
        for name in TEST_ATOMS {
            atoms.intern(name);
        }
        let atoms = Rc::new(atoms);
        let registry = Rc::new(RefCell::new(PortRegistry::new().expect("a port registry")));
        let hook = RuntimePorts::new(Rc::clone(&registry), Rc::clone(&atoms), OWNER);
        Fixture {
            registry,
            atoms,
            hook,
            heap: Heap::new(512).expect("a test heap"),
        }
    }

    impl Fixture {
        /// The atom `name` as a term (it must be pre-interned: see [`Fixture`]).
        fn atom(&self, name: &str) -> Eterm {
            make_atom(self.atoms.lookup(name).expect("a pre-interned atom"))
        }

        /// `text` as an Erlang string on the fixture's heap.
        fn string(&mut self, text: &str) -> Eterm {
            charlist(&mut self.heap, text).expect("a string")
        }

        /// An Erlang list of `elements` on the fixture's heap.
        fn list(&mut self, elements: &[Eterm]) -> Eterm {
            let mut list = NIL;
            for element in elements.iter().rev() {
                list = heap_cons(&mut self.heap, *element, list);
            }
            list
        }

        /// The `{tag, value}` `Name` tuple `open_port/2` takes.
        fn name(&mut self, tag: &str, value: Eterm) -> Eterm {
            let tag = self.atom(tag);
            self.heap.tuple(&[tag, value]).expect("a name tuple")
        }

        /// The `{spawn, Cmd}` `Name` with `command` as the shell command line.
        fn spawn(&mut self, command: &str) -> Eterm {
            let command = self.string(command);
            self.name("spawn", command)
        }
        /// The value `port_info/1` answered for `name`.
        fn item(&mut self, port: PortId, name: &str) -> Eterm {
            let pairs = self
                .hook
                .port_info(port)
                .unwrap_or_else(|error| panic!("port_info({port:?}) failed: {error}"));
            let wanted = self.atom(name);
            pairs
                .iter()
                .find(|(key, _)| *key == wanted)
                .map(|(_, value)| *value)
                .unwrap_or_else(|| panic!("no {name:?} item in {pairs:?}"))
        }
    }

    /// A cons cell built directly on the fixture's heap (the `beam_alloc` interface, which
    /// is what the production `charlist`/`pid_list` helpers use).
    fn heap_cons(heap: &mut Heap, head: Eterm, tail: Eterm) -> Eterm {
        heap.cons(head, tail).expect("a cons cell")
    }

    /// `term` read as a byte string.
    fn text_of(term: Eterm) -> String {
        terms::list_elements(term)
            .expect("a proper list")
            .into_iter()
            .map(|element| char::from(u8::try_from(signed_value(element)).expect("a byte")))
            .collect()
    }

    /// Asserts `result` is exactly `Err(BeamError::BadArg)`.
    ///
    /// [`BeamError`] is not `PartialEq` (it carries an owned message and a live BEAM reason
    /// term), so the check is a `matches!` rather than an `assert_eq!`.
    fn assert_badarg<T: std::fmt::Debug>(result: Result<T>) {
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "expected badarg, got {result:?}"
        );
    }

    /// Drains the fixture's port poller until `port` produces stream data, returning it.
    ///
    /// The child's output is *not* read by the test any more: the runtime's poller owns the
    /// descriptors (the whole point of the `beam_io` adoption), so the bytes are recovered
    /// through the same `pump` the runtime itself uses.
    fn pump_data(registry: &mut PortRegistry, port: PortId) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            for event in registry.pump(Duration::from_millis(50)).expect("pump") {
                if let PortEvent::Data {
                    port: source,
                    bytes,
                    ..
                } = event
                {
                    if source == port {
                        return bytes;
                    }
                }
            }
        }
        panic!("no stream data for {port:?}");
    }

    /// The `port_info/1,2` metadata of a port a test registers **directly** (bypassing
    /// `open_port/2`), so a recorder driver can be driven without a real child.
    fn test_meta(command: &str) -> PortMeta {
        PortMeta {
            command: command.to_string(),
            kind: PortName::Spawn,
            os_pid: None,
            stream: StreamType::List,
            notify_exit: false,
            monitors: Vec::new(),
            owner: OWNER,
            driver_terms: None,
        }
    }

    /// A driver that records its lifecycle callbacks and can declare a timer.
    ///
    /// The built-in drivers (`fd`/`efile`/`spawn`) give no observation point for the two
    /// `beam_rt`-driven seams — the port-exit notification ([`Driver::port_exit`]) and the
    /// poll deadline ([`Driver::next_timeout`] + [`Driver::timeout`]) — so they are pinned
    /// against this recorder instead.
    struct RecorderDriver {
        /// The registered name (required by the trait; unused by the tests).
        name: &'static str,
        /// The lifecycle callbacks in call order: `"exit:<Reason>"` from `port_exit` and
        /// `"stop"` from the teardown.
        log: Rc<RefCell<Vec<String>>>,
        /// The deadline `next_timeout` declares, when this recorder carries a timer.
        deadline: Option<Instant>,
        /// Where the timer records the instant it actually fired (absent without a timer).
        fired_at: Option<Rc<RefCell<Option<Instant>>>>,
    }

    impl RecorderDriver {
        /// A recorder with no timer, appending its callbacks to `log`.
        fn new(name: &'static str, log: Rc<RefCell<Vec<String>>>) -> RecorderDriver {
            RecorderDriver {
                name,
                log,
                deadline: None,
                fired_at: None,
            }
        }

        /// A recorder declaring a timer at `deadline`, recording when it fires.
        fn with_timer(deadline: Instant, fired_at: Rc<RefCell<Option<Instant>>>) -> RecorderDriver {
            RecorderDriver {
                name: "recorder",
                log: Rc::new(RefCell::new(Vec::new())),
                deadline: Some(deadline),
                fired_at: Some(fired_at),
            }
        }
    }

    impl Driver for RecorderDriver {
        fn name(&self) -> &str {
            self.name
        }

        fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
            Ok(0)
        }

        fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            Ok(())
        }

        fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            self.log.borrow_mut().push("stop".to_string());
            Ok(())
        }

        fn port_exit(&mut self, _ctx: &mut DriverContext<'_>, reason: PortExitReason) -> Result<()> {
            self.log.borrow_mut().push(format!("exit:{reason:?}"));
            Ok(())
        }

        fn next_timeout(&self) -> Option<Instant> {
            // Declared until it has fired: the port re-arms its `TIMEOUT` task from this
            // after every interaction, so a still-declared deadline would fire every round.
            match &self.fired_at {
                Some(fired) if fired.borrow().is_some() => None,
                _ => self.deadline,
            }
        }

        fn timeout(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
            if let Some(fired) = &self.fired_at {
                *fired.borrow_mut() = Some(Instant::now());
            }
            Ok(())
        }
    }

    #[test]
    fn spawn_true_opens_a_real_registered_port() {
        let mut fx = fixture();
        let command = fx.string("true");
        let name = fx.name("spawn", command);

        let port = fx
            .hook
            .open_port(name, NIL)
            .expect("open_port({spawn, \"true\"}, [])");
        assert!(PortId::is_port(port.to_eterm()), "a real internal port");
        assert!(fx.registry.borrow().contains(port));

        // The child really started: `os_pid` is its process id, captured from the driver
        // before the port was registered.
        let os_pid = signed_value(fx.item(port, "os_pid"));
        assert!(os_pid > 0, "a spawned child has an os pid, got {os_pid}");

        // Every item atom resolves through the shared table (never a fabricated index), and
        // the pair list is exactly the oracle's `/1` roster followed by the `/2`-only
        // `monitors` — in the oracle's order.
        let pairs = fx.hook.port_info(port).expect("port_info");
        let keys: Vec<String> = pairs
            .iter()
            .map(|(key, _)| {
                fx.atoms
                    .name(atom_val(*key))
                    .expect("an item atom resolves")
                    .to_string()
            })
            .collect();
        assert_eq!(
            keys,
            [
                "name", "links", "id", "connected", "input", "output", "os_pid", "monitors"
            ]
        );
        assert_eq!(text_of(fx.item(port, "name")), "true");
        // `id` is a bare small integer (the port number), **not** the port term.
        assert_eq!(
            fx.item(port, "id"),
            make_small(i64::try_from(port.number()).expect("an id fits"))
        );
        // `links` is the owner as a one-element list and `connected` is that same owner pid.
        let owner = OWNER.to_eterm();
        assert_eq!(
            terms::list_elements(fx.item(port, "links")).expect("links is a list"),
            vec![owner]
        );
        assert_eq!(fx.item(port, "connected"), owner);
        assert_eq!(fx.item(port, "input"), make_small(0), "a byte counter, 0 at open");
        assert_eq!(fx.item(port, "output"), make_small(0), "a byte counter, 0 at open");
        assert_eq!(fx.item(port, "monitors"), NIL, "nothing monitors a port yet");
    }

    #[test]
    fn the_name_forms_and_their_options_are_accepted() {
        let mut fx = fixture();

        // `{spawn, Atom}` — an atom is a shell command line.
        let name = fx.name("spawn", fx.atom("not_a_driver"));
        let port = fx.hook.open_port(name, NIL).expect("{spawn, atom}");
        assert_eq!(text_of(fx.item(port, "name")), "not_a_driver");
        // `{spawn, [Prog, Arg…]}` is **flattened** (concatenation, no separator) into a shell
        // command line — the oracle has no argv form for `{spawn, …}`.
        let program = fx.string("/bin/sh");
        let dash_c = fx.string("-c");
        let command = fx.string("true");
        let argv = fx.list(&[program, dash_c, command]);
        let name = fx.name("spawn", argv);
        let port = fx.hook.open_port(name, NIL).expect("{spawn, argv}");
        assert_eq!(text_of(fx.item(port, "name")), "/bin/sh-ctrue");
        // `{spawn_executable, Path}` honouring `{args, […]}` and a `binary` stream.
        let path = fx.string("/bin/sh");
        let name = fx.name("spawn_executable", path);
        let dash_c = fx.string("-c");
        let true_cmd = fx.string("true");
        let args = fx.list(&[dash_c, true_cmd]);
        let args_pair = {
            let tag = fx.atom("args");
            fx.heap.tuple(&[tag, args]).expect("an args option")
        };
        let binary = fx.atom("binary");
        let options = fx.list(&[binary, args_pair]);
        let port = fx
            .hook
            .open_port(name, options)
            .expect("{spawn_executable, \"/bin/sh\"} with args");
        assert_eq!(text_of(fx.item(port, "name")), "/bin/sh");
        // `stream` is no longer a `port_info` item: the honored `binary` option is asserted
        // on the port's own metadata.
        let stream = fx
            .registry
            .borrow()
            .meta
            .get(&port.number())
            .expect("a live port")
            .stream;
        assert_eq!(stream, StreamType::Binary);

        // `{fd, N}` registers a descriptor port with no OS process. The descriptor is a
        // socketpair end the **test** owns and keeps open: `beam_io`'s driver adopts a
        // `{fd, N}` by putting it in non-blocking mode (`fcntl(F_SETFL, O_NONBLOCK)`), which
        // the sandbox refuses for an inherited descriptor (`EPERM`); a socketpair end always
        // accepts it, so the childless name form is exercised independently of the test
        // harness's fds.
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (local, _peer) = UnixStream::pair().expect("a socketpair");
        let fd = local.as_raw_fd();
        let name = fx.name("fd", make_small(i64::from(fd)));
        let port = fx.hook.open_port(name, NIL).expect("{fd, N}");
        // `os_pid` is `undefined` for a port without an OS process, while `connected` is
        // still the owner pid — both the oracle's answers.
        assert_eq!(fx.item(port, "os_pid"), fx.atom("undefined"));
        assert_eq!(fx.item(port, "connected"), OWNER.to_eterm());
    }

    #[test]
    fn the_working_directory_option_is_applied() {
        let mut fx = fixture();
        let name = fx.spawn("pwd");
        let dir = fx.string("/");
        let cd = {
            let tag = fx.atom("cd");
            fx.heap.tuple(&[tag, dir]).expect("a cd option")
        };
        let options = fx.list(&[cd]);
        // The option is applied to the child (`pwd` runs in `/`); the port itself only
        // exposes that it started.
        let port = fx.hook.open_port(name, options).expect("{cd, \"/\"}");
        assert!(signed_value(fx.item(port, "os_pid")) > 0);
    }

    #[test]
    fn invalid_names_and_options_are_badarg() {
        let mut fx = fixture();

        // A Name that is not a recognized `{tag, …}` tuple.
        assert_badarg(fx.hook.open_port(make_small(1), NIL));
        let command = fx.string("true");
        let unknown = fx.name("not_a_driver", command);
        assert_badarg(fx.hook.open_port(unknown, NIL));
        // A known tag whose `Cmd` the drivers cannot accept.
        let bad_command = fx.name("spawn", make_small(5));
        assert_badarg(fx.hook.open_port(bad_command, NIL));
        // A negative descriptor.
        let bad_fd = fx.name("fd", make_small(-1));
        assert_badarg(fx.hook.open_port(bad_fd, NIL));

        // An `Options` that is not a list at all.
        let spawn = fx.spawn("true");
        assert_badarg(fx.hook.open_port(spawn, make_small(5)));
        // A recognized option with a malformed value.
        let cd_pair = {
            let tag = fx.atom("cd");
            fx.heap.tuple(&[tag, make_small(5)]).expect("a cd option")
        };
        let options = fx.list(&[cd_pair]);
        assert_badarg(fx.hook.open_port(spawn, options));
        let bad_args = {
            let tag = fx.atom("args");
            let value = fx.atom("nope");
            fx.heap.tuple(&[tag, value]).expect("an args option")
        };
        let options = fx.list(&[bad_args]);
        assert_badarg(fx.hook.open_port(spawn, options));
        // A recognized option with the wrong arity.
        let cd_alone = {
            let tag = fx.atom("cd");
            fx.heap.tuple(&[tag]).expect("a cd option")
        };
        let options = fx.list(&[cd_alone]);
        assert_badarg(fx.hook.open_port(spawn, options));

        // Validation precedes allocation, so no rejected call consumed a port number: the
        // next successful `open_port` gets the very first id.
        let port = fx.hook.open_port(spawn, NIL).expect("a valid port");
        assert_eq!(port, PortId::new(0));
    }

    #[test]
    fn whitelisted_option_atoms_and_tuples_are_accepted() {
        let mut fx = fixture();
        // The whole accepted option-atom whitelist, every no-op here except `binary`.
        let atoms = [
            "stream",
            "use_stdio",
            "stderr_to_stdout",
            "line",
            "nouse_stdio",
            "binary",
            "in",
            "out",
            "eof",
            "hide",
            "exit_status",
            "overlapped_io",
        ];
        let mut elements: Vec<Eterm> = Vec::new();
        for name in atoms {
            let atom = fx.atom(name);
            elements.push(atom);
        }
        // A whitelisted tuple option is accepted too.
        let packet = {
            let tag = fx.atom("packet");
            fx.heap.tuple(&[tag, make_small(4)]).expect("a packet option")
        };
        elements.push(packet);
        let options = fx.list(&elements);
        let name = fx.spawn("true");
        let port = fx
            .hook
            .open_port(name, options)
            .expect("whitelisted options are accepted");
        // Accepting an option never changes what the port is (the `binary` atom is the one
        // that *does* change the stream, exercised in the name-forms test).
        assert_eq!(text_of(fx.item(port, "name")), "true");
    }

    #[test]
    fn options_outside_the_whitelist_are_badarg() {
        let mut fx = fixture();
        // `list` is not an option (a list stream is the default).
        let name = fx.spawn("true");
        let list = fx.atom("list");
        let options = fx.list(&[list]);
        assert_badarg(fx.hook.open_port(name, options));
        // An unknown option atom.
        let name = fx.spawn("true");
        let nope = fx.atom("nope");
        let options = fx.list(&[nope]);
        assert_badarg(fx.hook.open_port(name, options));
        // An unknown option tuple.
        let name = fx.spawn("true");
        let bad_tuple = {
            let tag = fx.atom("nope");
            fx.heap.tuple(&[tag, make_small(1)]).expect("a nope option")
        };
        let options = fx.list(&[bad_tuple]);
        assert_badarg(fx.hook.open_port(name, options));
        // A whitelisted tuple whose tag is fine but whose arity is wrong.
        let name = fx.spawn("true");
        let bad_arity = {
            let tag = fx.atom("packet");
            fx.heap.tuple(&[tag]).expect("a packet option")
        };
        let options = fx.list(&[bad_arity]);
        assert_badarg(fx.hook.open_port(name, options));
        // A non-atom, non-tuple option element.
        let name = fx.spawn("true");
        let options = fx.list(&[make_small(1)]);
        assert_badarg(fx.hook.open_port(name, options));
        // Validation precedes allocation: none of the rejected calls consumed a port number.
        let name = fx.spawn("true");
        let port = fx.hook.open_port(name, NIL).expect("a valid port");
        assert_eq!(port, PortId::new(0));
    }

    #[test]
    fn close_port_releases_the_slot_and_reaps_a_running_child() {
        let mut fx = fixture();
        // `cat` blocks on the port's stdin pipe (which the runtime holds open), so the
        // child is certainly still running when the port is closed.
        let name = fx.spawn("cat");
        let port = fx.hook.open_port(name, NIL).expect("open_port({spawn, \"cat\"}, [])");
        assert!(fx.registry.borrow().contains(port));

        fx.hook
            .close_port(port)
            .expect("port_close/1 kills and reaps the child");
        assert!(!fx.registry.borrow().contains(port));
        // A closed port is *not live*: `port_info/1` reports no items (the oracle's
        // `undefined`), while a close of it stays `badarg` — never a silently accepted
        // close.
        assert_badarg(fx.hook.close_port(port));
        assert!(fx
            .hook
            .port_info(port)
            .expect("a closed port has no items")
            .is_empty());
    }

    #[test]
    fn closing_a_port_that_already_exited_succeeds() {
        let mut fx = fixture();
        // `true` exits immediately; whether it has been reaped by the time it is closed
        // depends on scheduling, and both paths must succeed.
        let name = fx.spawn("true");
        let port = fx.hook.open_port(name, NIL).expect("open_port");
        fx.hook.close_port(port).expect("closing an exited child is fine");
        assert!(!fx.registry.borrow().contains(port));
    }

    #[test]
    fn an_unallocated_port_is_badarg() {
        let mut fx = fixture();
        let never_allocated = PortId::new(999);
        // `port_info/1` answers the oracle's "no items", i.e. the atom `undefined` once
        // `beam_bifs`/the native floor render it — never `badarg`, and never a fabricated
        // pair (see [`PortRegistry::info`]).
        assert!(fx
            .hook
            .port_info(never_allocated)
            .expect("an unallocated port has no items, not a badarg")
            .is_empty());
        // Closing a port the runtime does not hold *is* `badarg`: the close cannot be
        // performed, and a silently accepted no-op would hide a leaked port.
        assert_badarg(fx.hook.close_port(never_allocated));
    }

    #[test]
    fn write_command_really_writes_to_the_child_and_an_unknown_port_is_badarg() {
        let mut fx = fixture();
        // `cat` echoes exactly what it is given, so the write is observable through the
        // runtime's own poller (the same path that delivers `{Port, {data, …}}`).
        let name = fx.spawn("cat");
        let port = fx.hook.open_port(name, NIL).expect("open_port({spawn, \"cat\"}, [])");
        fx.registry
            .borrow_mut()
            .write_command(port, b"hello")
            .expect("port_command");
        let echo = pump_data(&mut fx.registry.borrow_mut(), port);
        assert_eq!(&echo, b"hello");

        // A port the registry does not hold is `badarg` (the oracle's reason for a closed
        // port), and so is a port id that was never allocated.
        assert_badarg(fx.registry.borrow_mut().write_command(PortId::new(9), b"x"));
        fx.hook.close_port(port).expect("port_close/1");
        assert_badarg(fx.registry.borrow_mut().write_command(port, b"x"));
    }

    /// `erlang:statistics(io)` reads the port subsystem's own byte counters: `output` is what
    /// `port_command/2,3` handed to a port's stream and `input` is what a port delivered back
    /// as `{Port, {data, …}}`. Both are bumped at the real read/write sites, so a byte count
    /// can never be a fabricated constant.
    #[test]
    fn the_io_counters_track_the_bytes_written_to_and_read_from_a_port() {
        let mut fx = fixture();
        // The runtime keeps a clone of the registry's counters; grab it before borrowing the
        // registry mutably below.
        let counters = fx.registry.borrow().io_counters();
        assert_eq!(counters.bytes(), (0, 0), "a fresh runtime has moved no port bytes");

        let name = fx.spawn("cat");
        let port = fx
            .hook
            .open_port(name, NIL)
            .expect("open_port({spawn, \"cat\"}, [])");
        // `cat` echoes exactly what it is given, so the write and the echo are both
        // observable through the runtime's own poller.
        fx.registry
            .borrow_mut()
            .write_command(port, b"hello")
            .expect("port_command");
        assert_eq!(
            counters.bytes(),
            (0, 5),
            "the write is `output`; nothing has been read back yet"
        );
        let echo = pump_data(&mut fx.registry.borrow_mut(), port);
        assert_eq!(&echo, b"hello");
        assert_eq!(
            counters.bytes(),
            (5, 5),
            "the echo `cat` produced is `input`"
        );

        // A rejected write moves nothing: the counters are bumped only after the port is
        // resolved.
        assert_badarg(fx.registry.borrow_mut().write_command(PortId::new(9), b"x"));
        assert_eq!(counters.bytes(), (5, 5));
    }

    #[test]
    fn a_fd_port_write_reaches_the_adopted_descriptor() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let mut fx = fixture();
        // A descriptor the **test** owns and keeps open: `beam_io`'s driver adopts a
        // `{fd, N}` by putting it in non-blocking mode (`fcntl(F_SETFL, O_NONBLOCK)`), which
        // the sandbox refuses for an inherited descriptor (`EPERM`); a socketpair end always
        // accepts it. The adoption is **duplex** — the one descriptor is registered with
        // `Interest::BOTH` — so `port_command` writes out through it (the oracle's own
        // behaviour for a `{fd, N}` port).
        let (local, mut peer) = UnixStream::pair().expect("a socketpair");
        let fd = local.as_raw_fd();
        let name = fx.name("fd", make_small(i64::from(fd)));
        let port = fx.hook.open_port(name, NIL).expect("{fd, N}");

        // The write takes the normal driver write path and the bytes come out of the peer.
        fx.registry
            .borrow_mut()
            .write_command(port, b"ping")
            .expect("the write reaches the adopted descriptor");
        let mut buf = [0u8; 4];
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("a read timeout");
        peer.read_exact(&mut buf)
            .expect("the peer reads what the port wrote");
        assert_eq!(&buf, b"ping");

        // A write never closes the port.
        assert!(fx.registry.borrow().is_live(port));
        fx.hook.close_port(port).expect("port_close/1");
    }

    #[test]
    fn an_empty_port_name_is_badarg() {
        let mut fx = fixture();
        // The oracle rejects an empty command/path in its own validation of the `Name`
        // (`open_port({spawn,""},[])` and `open_port({spawn_executable,""},[])` are both
        // `error:badarg` against erts-17.1) — before any driver is reached, whose `exec` of
        // an empty program would report `enoent` instead.
        let empty = fx.string("");
        let name = fx.name("spawn", empty);
        assert_badarg(fx.hook.open_port(name, NIL));
        let empty = fx.string("");
        let name = fx.name("spawn_executable", empty);
        assert_badarg(fx.hook.open_port(name, NIL));
        // The 3-tuple form carries the same name.
        let empty = fx.string("");
        let options = fx.list(&[]);
        let tag = fx.atom("spawn_executable");
        let name = fx.heap.tuple(&[tag, empty, options]).expect("a name tuple");
        assert_badarg(fx.hook.open_port(name, NIL));

        // Validation precedes allocation: none of the rejected calls consumed a port number.
        let name = fx.spawn("true");
        let port = fx.hook.open_port(name, NIL).expect("a valid port");
        assert_eq!(port, PortId::new(0));
    }

    #[test]
    fn closing_a_port_notifies_the_driver_of_the_exit_before_the_teardown() {
        let fx = fixture();
        let log = Rc::new(RefCell::new(Vec::new()));
        let driver = RecorderDriver::new("recorder", Rc::clone(&log));
        let port = fx
            .registry
            .borrow_mut()
            .register(Box::new(driver), test_meta("recorder"))
            .expect("a registered port");

        // The driver is told the port is exiting (`Closed`) **before** its teardown `stop`:
        // the two are distinct callbacks, and the order is what makes `port_exit` an
        // advisory notification rather than part of the teardown.
        fx.registry.borrow_mut().close(port).expect("close");
        assert_eq!(
            *log.borrow(),
            vec!["exit:Closed".to_string(), "stop".to_string()],
            "the port-exit notification precedes the teardown"
        );
        assert!(!fx.registry.borrow().contains(port));
        // A close the runtime has already performed is never silently accepted again.
        assert_badarg(fx.registry.borrow_mut().close(port));
    }

    #[test]
    fn a_port_exit_notification_is_delivered_once_and_leaves_the_port_live() {
        let fx = fixture();
        let log = Rc::new(RefCell::new(Vec::new()));
        let driver = RecorderDriver::new("recorder", Rc::clone(&log));
        let port = fx
            .registry
            .borrow_mut()
            .register(Box::new(driver), test_meta("recorder"))
            .expect("a registered port");

        // An isolated notification reaches the driver **once** and does not tear the port
        // down: a repeat never calls `port_exit` again, and the port is still live for the
        // later `stop`.
        let mut borrowed = fx.registry.borrow_mut();
        let ports = &mut *borrowed;
        ports
            .registry
            .notify_port_exit(&mut ports.poller, port, PortExitReason::Closed)
            .expect("the first notification reaches the driver");
        assert_eq!(*log.borrow(), vec!["exit:Closed".to_string()]);
        ports
            .registry
            .notify_port_exit(&mut ports.poller, port, PortExitReason::OwnerExited)
            .expect("a repeat is a no-op");
        assert_eq!(
            *log.borrow(),
            vec!["exit:Closed".to_string()],
            "the port-exit notification is delivered at most once"
        );
        assert!(
            ports.contains(port),
            "the notification leaves the port live"
        );
        ports.close(port).expect("the teardown still runs");
        assert_eq!(
            *log.borrow(),
            vec!["exit:Closed".to_string(), "stop".to_string()]
        );
    }

    #[test]
    fn the_pump_clamps_its_wait_to_a_driver_declared_deadline() {
        let fx = fixture();
        let fired_at = Rc::new(RefCell::new(None::<Instant>));
        // A deadline inside `PORT_POLL_SLICE`: without the clamp the first poll would spend
        // the whole slice before the port's `TIMEOUT` task could run at all.
        let declared = Instant::now() + Duration::from_millis(10);
        let driver = RecorderDriver::with_timer(declared, Rc::clone(&fired_at));
        fx.registry
            .borrow_mut()
            .register(Box::new(driver), test_meta("timer"))
            .expect("a registered port");

        // The declaration reaches the clamp: `beam_io`'s `next_deadline` is exactly the
        // driver's `next_timeout`, which the port's `TIMEOUT` task mirrors.
        assert_eq!(
            fx.registry.borrow().registry.next_deadline(),
            Some(declared),
            "the driver's declaration is the clamp's upper bound"
        );

        // A pump whose own timeout (60 ms) is far past the deadline still services the timer
        // **at** its deadline: the poll may not block past it, so the task round right after
        // the poll runs it. Unclamped, the first poll would block a whole `PORT_POLL_SLICE`.
        let started = Instant::now();
        let events = fx
            .registry
            .borrow_mut()
            .pump(Duration::from_millis(60))
            .expect("pump");
        assert!(events.is_empty(), "a timer is not a port message: {events:?}");

        let fired = fired_at.borrow().expect("the declared timer fired");
        let delay = fired.duration_since(started);
        assert!(
            delay >= Duration::from_millis(5),
            "the deadline is a lower bound — the timer must not fire early, fired after {delay:?}"
        );
        assert!(
            delay < PORT_POLL_SLICE,
            "the wait was clamped below the {PORT_POLL_SLICE:?} slice, fired after {delay:?}"
        );
        // The timer fired once and cleared its own declaration, so nothing is left armed.
        assert_eq!(*fired_at.borrow(), Some(fired), "the timer fired exactly once");
        assert_eq!(fx.registry.borrow().registry.next_deadline(), None);
    }
    #[test]
    fn a_fd_port_adopts_a_caller_owned_descriptor() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let mut fx = fixture();
        // A descriptor the **test** owns and keeps open for the whole test.
        // `beam_io`'s driver adopts a `{fd, N}` by putting it in non-blocking mode
        // (`fcntl(F_SETFL, O_NONBLOCK)`), which the sandbox refuses for an inherited
        // descriptor (fd 0 yields `EPERM`); a socketpair end always accepts it, so the
        // childless name form is exercised independently of the test harness's fds.
        let (mut local, _peer) = UnixStream::pair().expect("a socketpair");
        let fd = local.as_raw_fd();

        let name = fx.name("fd", make_small(i64::from(fd)));
        let port = fx
            .hook
            .open_port(name, NIL)
            .expect("{fd, N} adopts the descriptor");

        // A descriptor port has no OS process of its own, is connected to its owner, and
        // reports the `fd:<N>` rendering the reference implementation uses.
        assert_eq!(fx.item(port, "os_pid"), fx.atom("undefined"));
        assert_eq!(fx.item(port, "connected"), OWNER.to_eterm());
        assert_eq!(text_of(fx.item(port, "name")), format!("fd:{fd}"));
        assert!(fx.registry.borrow().contains(port));

        // Closing the port releases it but **never closes** the adopted descriptor: it
        // belongs to the Erlang side. The write below would fail with `EBADF` if the
        // driver had closed it.
        fx.hook.close_port(port).expect("port_close/1");
        assert!(!fx.registry.borrow().contains(port));
        local
            .write_all(b"x")
            .expect("the adopted descriptor is still open after port_close/1");
    }

    #[test]
    fn port_info_on_an_unknown_or_closed_port_is_an_empty_pair_list() {
        let mut fx = fixture();
        // Never opened: no pairs (the oracle's `undefined` through `beam_bifs`, and the
        // native floor's own `undefined` for `erlang:port_info/1,2`).
        assert!(fx.hook.port_info(PortId::new(9)).expect("no badarg").is_empty());
        assert!(!fx.registry.borrow().is_live(PortId::new(9)));

        // Closed: same answer, and `port_close/1` on it is still `badarg` — the two are
        // deliberately different questions (reports may be empty, but a close the runtime
        // cannot perform is never silently accepted).
        let name = fx.spawn("true");
        let port = fx.hook.open_port(name, NIL).expect("open_port");
        assert!(fx.registry.borrow().is_live(port));
        assert_eq!(
            fx.hook.port_info(port).expect("a live port").len(),
            8,
            "seven `/1` items plus the `/2`-only `monitors`"
        );
        fx.hook.close_port(port).expect("port_close/1");
        assert!(!fx.registry.borrow().is_live(port));
        assert!(fx.hook.port_info(port).expect("no badarg").is_empty());
        assert_badarg(fx.hook.close_port(port));
    }

    #[test]
    fn the_exit_status_option_is_remembered_on_the_port() {
        let mut fx = fixture();
        // Without the option the poller has nothing to report.
        let name = fx.spawn("true");
        let plain = fx.hook.open_port(name, NIL).expect("open_port");
        let exit_status = fx.atom("exit_status");
        let options = fx.list(&[exit_status]);
        let name = fx.spawn("true");
        let with_option = fx.hook.open_port(name, options).expect("open_port");
        let registry = fx.registry.borrow();
        assert!(
            !registry.meta.get(&plain.number()).expect("a port").notify_exit,
            "a port opened without exit_status must not report an exit message"
        );
        assert!(
            registry
                .meta
                .get(&with_option.number())
                .expect("a port")
                .notify_exit,
            "the exit_status option must be recorded"
        );
    }

    #[test]
    fn the_pump_reports_a_reaped_childs_exit_status_once() {
        let mut fx = fixture();
        let exit_status = fx.atom("exit_status");
        let options = fx.list(&[exit_status]);
        let name = fx.spawn("sh -c \"exit 3\"");
        let port = fx.hook.open_port(name, options).expect("open_port");

        // The status is not tied to stream readiness: a child that wrote nothing still
        // reports it, and it is reported exactly once.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut status = None;
        let mut reports = 0;
        while Instant::now() < deadline {
            for event in fx
                .registry
                .borrow_mut()
                .pump(Duration::from_millis(50))
                .expect("pump")
            {
                if let PortEvent::Exited {
                    port: source,
                    status: reported,
                    ..
                } = event
                {
                    assert_eq!(source, port);
                    status = Some(reported);
                    reports += 1;
                }
            }
            if status.is_some() {
                break;
            }
        }
        assert_eq!(status, Some(3), "the reaped child's exit code");
        assert_eq!(reports, 1);

        // A second round must not report it again.
        let quiet = fx
            .registry
            .borrow_mut()
            .pump(Duration::from_millis(50))
            .expect("pump");
        assert!(
            quiet.is_empty(),
            "an exit status is handed out exactly once: {quiet:?}"
        );

        // A port opened *without* the option never reports one.
        let name = fx.spawn("true");
        let plain = fx.hook.open_port(name, NIL).expect("open_port");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let events = fx
                .registry
                .borrow_mut()
                .pump(Duration::from_millis(50))
                .expect("pump");
            for event in events {
                assert!(
                    !matches!(event, PortEvent::Exited { port: p, .. } if p == plain),
                    "a port without the exit_status option must not report one"
                );
            }
            if fx.registry.borrow().is_live(plain) {
                // The child has certainly exited by now; nothing was reported for it.
                break;
            }
        }
    }

    #[test]
    fn the_port_info_atom_vocabulary_is_seeded() {
        let atoms = AtomTable::new();
        let mut seeded = atoms;
        boot::seed_core_atoms(&mut seeded);
        for name in PORT_INFO_ATOMS {
            assert!(seeded.lookup(name).is_some(), "{name} must be seeded");
        }
        // The provider resolves `true`/`false`/`undefined` too.
        for name in ["true", "false", "undefined"] {
            assert!(seeded.lookup(name).is_some(), "{name} must be seeded");
        }
    }

    #[test]
    fn an_empty_port_registry_has_no_ports() {
        let registry = PortRegistry::new().expect("a port registry");
        assert!(registry.is_empty());
        assert!(!registry.contains(PortId::new(0)));
        assert_eq!(registry.len(), 0);
        assert!(registry.ids().is_empty());
    }

    #[test]
    fn ids_reports_live_ports_across_a_released_lower_slot() {
        let mut fx = fixture();
        let first = {
            let name = fx.spawn("true");
            fx.hook.open_port(name, NIL).expect("first port")
        };
        let second = {
            let name = fx.spawn("true");
            fx.hook.open_port(name, NIL).expect("second port")
        };
        assert_eq!(
            fx.registry.borrow().ids(),
            vec![first, second],
            "the live ports are reported in port-number order"
        );

        // Closing the *lower* port leaves a gap below the still-live higher one. The
        // metadata map is the source of truth, so `ids` still reports the higher port (a
        // scan-from-zero recovery would have stopped at the gap and missed it).
        fx.hook.close_port(first).expect("closing the first port");
        assert!(!fx.registry.borrow().contains(first));
        assert_eq!(fx.registry.borrow().ids(), vec![second]);
    }

    // -----------------------------------------------------------------------------------------
    // The driver-emitted-term delivery handoff (`erl_drv_output_term`/`erl_drv_send_term`)
    // -----------------------------------------------------------------------------------------

    /// A `DriverPort` host plus the `port_info` metadata a `{spawn_driver, Name}` `open_port/2`
    /// builds for it: the two halves of the driver-term handoff (see
    /// [`RuntimePorts::open_port`]).
    ///
    /// The host's [`driver::TermQueue`] is the **same** handle stored on `meta.driver_terms`, so
    /// pushing through the host (what a driver callback does) is exactly what the port's drain
    /// observes.
    fn driver_term_host(atoms: &Rc<AtomTable>, owner: ProcessId) -> (driver::DriverPort, PortMeta) {
        let host = driver::DriverPort::new(
            Heap::new(64).expect("a driver heap"),
            Rc::clone(atoms),
            owner,
        );
        let queue = host.term_queue();
        let mut meta = test_meta("driver");
        meta.kind = PortName::SpawnDriver;
        meta.owner = owner;
        meta.driver_terms = Some(queue);
        (host, meta)
    }

    /// The same, registered in the fixture's registry (a port with no OS child, driven by a
    /// [`RecorderDriver`]).
    fn driver_term_fixture(
        fx: &Fixture,
        owner: ProcessId,
    ) -> (driver::DriverPort, PortId) {
        let (host, meta) = driver_term_host(&fx.atoms, owner);
        let port = fx
            .registry
            .borrow_mut()
            .register(
                Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
                meta,
            )
            .expect("a registered driver port");
        (host, port)
    }

    /// A term a C driver emitted is drained off the port's [`driver::TermQueue`] as a
    /// [`PortEvent::Term`] — the decoded term itself, with the destination `erl_drv_output_term`
    /// (`None` = the owner) or `erl_drv_send_term` (`Some(pid)`) named — and a destination this
    /// node cannot resolve is dropped (the C's own "not delivered", `io.c:5664`).
    #[test]
    fn a_driver_emitted_term_is_drained_as_a_term_event() {
        use beam_abi::DriverHost;

        let fx = fixture();
        let (mut host, port) = driver_term_fixture(&fx, OWNER);

        // Nothing queued yet.
        assert!(fx.registry.borrow_mut().take_driver_terms().is_empty());

        let recipient = ProcessId::new(9, 0);
        // `erl_drv_output_term`: the port's connected process.
        assert_eq!(host.send_term(std::ptr::null_mut(), None, make_small(1)), 0);
        // `erl_drv_send_term`: an explicit destination.
        assert_eq!(
            host.send_term(std::ptr::null_mut(), Some(recipient.to_eterm()), make_small(2)),
            0
        );
        // A destination that is not a pid of this node: the C's refusal, never a misdelivery.
        assert_eq!(
            host.send_term(std::ptr::null_mut(), Some(make_small(3)), make_small(4)),
            0
        );

        let events = fx.registry.borrow_mut().take_driver_terms();
        assert_eq!(
            events.len(),
            2,
            "the unresolvable destination is dropped: {events:?}"
        );
        match &events[0] {
            PortEvent::Term {
                port: source,
                to,
                term,
                owner,
            } => {
                assert_eq!(*source, port);
                assert_eq!(*to, None, "`None` = the port owner");
                assert_eq!(*term, make_small(1));
                assert_eq!(*owner, OWNER);
            }
            other => panic!("expected a term event, got {other:?}"),
        }
        match &events[1] {
            PortEvent::Term { to, term, .. } => {
                assert_eq!(*to, Some(recipient));
                assert_eq!(*term, make_small(2));
            }
            other => panic!("expected a term event, got {other:?}"),
        }
        // Draining empties the queue: a second round reports nothing.
        assert!(fx.registry.borrow_mut().take_driver_terms().is_empty());
    }

    /// The port pump surfaces a driver-emitted term as its own event (it is one of the two
    /// producers [`PortRegistry::pump`] harvests, next to `driver_output`'s bytes).
    #[test]
    fn the_pump_reports_a_driver_emitted_term() {
        use beam_abi::DriverHost;

        let fx = fixture();
        let (mut host, port) = driver_term_fixture(&fx, OWNER);
        assert_eq!(host.send_term(std::ptr::null_mut(), None, make_small(7)), 0);

        let events = fx
            .registry
            .borrow_mut()
            .pump(Duration::from_millis(1))
            .expect("pump");
        assert!(
            events.iter().any(|event| matches!(
                event,
                PortEvent::Term { port: p, term, .. } if *p == port && *term == make_small(7)
            )),
            "the pump must report the driver's term: {events:?}"
        );
        // Handed out exactly once.
        assert!(fx.registry.borrow_mut().take_driver_terms().is_empty());
    }

    /// A driver-emitted term is delivered into its recipient's mailbox by
    /// [`Runtime::deliver_port_messages`]: the term itself (never a `{Port, Term}` wrapper), to the
    /// port's owner for `erl_drv_output_term` and to the named pid for `erl_drv_send_term`.
    ///
    /// The term was built on the **driver host's** arena, so it is copied onto the runtime's
    /// pointer-stable heap before delivery — the mailbox payload is a fresh copy, not the driver's
    /// own words (see [`RuntimeContext::stabilize_driver_term`]).
    #[test]
    fn a_driver_emitted_term_is_delivered_into_the_recipients_mailbox() {
        use beam_abi::DriverHost;

        let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
        let owner = table.spawn_default().expect("the port's owner");
        let other = table.spawn_default().expect("an explicit destination");

        // A two-element tuple built on the *host's* arena, so the delivered copy proves the term
        // was stabilised off that heap rather than shared. A fresh `AtomTable` is fine: nothing in
        // this term is an atom.
        let atoms = Rc::new(AtomTable::new());
        let (mut host, meta) = driver_term_host(&atoms, owner);
        let mut scratch = Heap::new(16).expect("a scratch heap");
        let queued = scratch
            .tuple(&[make_small(1), make_small(2)])
            .expect("a small tuple");
        assert_eq!(host.send_term(std::ptr::null_mut(), None, queued), 0);
        assert_eq!(
            host.send_term(std::ptr::null_mut(), Some(other.to_eterm()), queued),
            0
        );

        let port = rt
            .ports
            .borrow_mut()
            .register(
                Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
                meta,
            )
            .expect("a registered driver port");

        let delivered = rt
            .deliver_port_messages(&mut table, Duration::from_millis(1))
            .expect("deliver");
        assert_eq!(delivered, 2, "both terms are delivered");

        // The owner got the `erl_drv_output_term` term; the explicit destination got its own.
        let in_owner = table
            .get(owner)
            .expect("the owner")
            .mailbox()
            .peek()
            .expect("a message")
            .payload();
        assert_eq!(
            terms::tuple_arity(in_owner).expect("the term itself is the message"),
            2,
            "in_owner={in_owner:?}"
        );
        assert_eq!(signed_value(terms::tuple_element(in_owner, 0).unwrap()), 1);
        assert_eq!(signed_value(terms::tuple_element(in_owner, 1).unwrap()), 2);
        assert_ne!(
            in_owner.raw(),
            queued.raw(),
            "the delivered term is a copy off the driver's arena"
        );

        let in_other = table
            .get(other)
            .expect("the explicit destination")
            .mailbox()
            .peek()
            .expect("a message")
            .payload();
        assert_eq!(terms::tuple_arity(in_other).unwrap(), 2, "{in_other:?}");

        // The `from` of both messages is the port itself (what send-tracing reports).
        assert_eq!(
            table.get(owner).unwrap().mailbox().peek().unwrap().from(),
            port.to_eterm()
        );
    }

    /// An unresolvable `erl_drv_send_term` destination is dropped at the drain, so
    /// [`Runtime::deliver_port_messages`] delivers nothing for it — the C's own "not delivered".
    #[test]
    fn a_driver_term_with_an_unresolvable_destination_is_not_delivered() {
        use beam_abi::DriverHost;

        let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
        let owner = table.spawn_default().expect("the port's owner");

        let atoms = Rc::new(AtomTable::new());
        let (mut host, meta) = driver_term_host(&atoms, owner);
        assert_eq!(
            host.send_term(std::ptr::null_mut(), Some(make_small(3)), make_small(4)),
            0
        );
        let _ = rt
            .ports
            .borrow_mut()
            .register(
                Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
                meta,
            )
            .expect("a registered driver port");

        let delivered = rt
            .deliver_port_messages(&mut table, Duration::from_millis(1))
            .expect("deliver");
        assert_eq!(delivered, 0, "the unresolvable destination is dropped");
        assert!(
            table.get(owner).expect("the owner").mailbox().is_empty(),
            "nothing reaches the owner"
        );
    }

    /// A term addressed to a process that is **not** the port's owner and not the evaluating
    /// process is delivered to the named process, not to the owner.
    #[test]
    fn an_explicit_destination_overrides_the_owner() {
        use beam_abi::DriverHost;

        let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
        let owner = table.spawn_default().expect("the port's owner");
        let other = table.spawn_default().expect("the destination");

        let atoms = Rc::new(AtomTable::new());
        let (mut host, meta) = driver_term_host(&atoms, owner);
        assert_eq!(
            host.send_term(std::ptr::null_mut(), Some(other.to_eterm()), make_small(42)),
            0
        );
        let _ = rt
            .ports
            .borrow_mut()
            .register(
                Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
                meta,
            )
            .expect("a registered driver port");

        let delivered = rt
            .deliver_port_messages(&mut table, Duration::from_millis(1))
            .expect("deliver");
        assert_eq!(delivered, 1);
        assert_eq!(
            table.get(other).unwrap().mailbox().peek().unwrap().payload(),
            make_small(42)
        );
        assert!(
            table.get(owner).unwrap().mailbox().is_empty(),
            "the owner receives nothing for an explicit destination"
        );
    }

    /// The `-eval` receive path returns a driver term addressed to the runtime's own process and
    /// skips one addressed elsewhere (it can only observe its own mailbox).
    #[test]
    fn the_eval_receive_path_returns_only_its_own_driver_terms() {
        use beam_abi::DriverHost;

        let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let me = rt.self_pid();
        let elsewhere = ProcessId::new(1234, 0);

        let atoms = Rc::new(AtomTable::new());
        let (mut host, meta) = driver_term_host(&atoms, me);
        // Addressed to the runtime's own process (the port owner).
        assert_eq!(host.send_term(std::ptr::null_mut(), None, make_small(11)), 0);
        // Addressed explicitly to the same process.
        assert_eq!(
            host.send_term(std::ptr::null_mut(), Some(me.to_eterm()), make_small(12)),
            0
        );
        // Addressed to another process: not observable here.
        assert_eq!(
            host.send_term(std::ptr::null_mut(), Some(elsewhere.to_eterm()), make_small(13)),
            0
        );
        let _ = rt
            .ports
            .borrow_mut()
            .register(
                Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
                meta,
            )
            .expect("a registered driver port");

        let messages = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.poll_port_messages(Duration::from_millis(1))
                .expect("poll")
        };
        let values: Vec<i64> = messages.iter().map(|term| signed_value(*term)).collect();
        assert_eq!(
            values,
            vec![11, 12],
            "only the terms addressed to the evaluating process are returned"
        );
    }

    /// [`RuntimeContext::stabilize_driver_term`] copies a heap term onto the runtime's
    /// pointer-stable arena: the copy is at a different address and is structurally the same.
    #[test]
    fn stabilize_driver_term_copies_a_heap_term() {
        let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let mut src = Heap::new(16).expect("a source heap");
        let term = src
            .tuple(&[make_small(5), make_small(6)])
            .expect("a small tuple");

        let stable = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.stabilize_driver_term(term).expect("stabilize")
        };
        assert_ne!(stable.raw(), term.raw(), "the term is copied off its source heap");
        assert_eq!(terms::tuple_arity(stable).unwrap(), 2);
        assert_eq!(signed_value(terms::tuple_element(stable, 0).unwrap()), 5);
        assert_eq!(signed_value(terms::tuple_element(stable, 1).unwrap()), 6);

        // An immediate is shared, not copied.
        let stable_immediate = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.stabilize_driver_term(make_small(9)).expect("stabilize")
        };
        assert_eq!(stable_immediate, make_small(9));
    }
}
