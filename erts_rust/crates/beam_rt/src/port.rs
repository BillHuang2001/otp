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
//! * the **`-eval` `receive … after T`** path ([`RuntimeContext::poll_port_messages_into_mailbox`])
//!   delivers them into the evaluating process's **mailbox** too, through the same delivery
//!   path — `-eval` runs outside a scheduler slice, so it is the port *poll* it drives itself
//!   and not the delivery rule that differs. A message no clause matches therefore stays
//!   queued for a later `receive`, exactly as it does on the scheduler route.
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
//! a `binary` for a `binary`-stream port (a char list otherwise), **at whatever width the
//! child wrote** (see [`RuntimeContext::port_data_message`]).
//!
//! The payload's shape is the C's `driver_output*` **split**, not the bytes: a read carries its
//! header (`hbuf`) and body (`buf`) apart ([`ReadMessage`]), so a `[binary]` port delivers the bare
//! binary for a header-less read, the improper `[H1..Hh|<<Data>>]` when the call carried both
//! halves, the proper `[H1..Hh]` when it carried no body at all, and `[]` for a read of nothing
//! (`deliver_read_message`, `erts/emulator/beam/io.c:3363-3442`); a `list` stream is every byte as
//! a char list either way.
//!
//! An **in-tree driver**'s reply carries its own split: `ram_file_drv` hands every reply up as one
//! [`ReadMessage`] ([`beam_io::DriverContext::deliver_read`],
//! `beam_io/src/drivers/ram_file.rs:477`), so a header-only `OK`/`NUMBER`/`ERROR` is the proper list
//! (`[3,0,0,0,0]` for a fresh `open`) and a `DATA` reply the improper `[2,0,0,0,2|<<97,98>>]` on a
//! `[binary]` port — the oracle's own terms, on either stream.
//!
//! A **header-less** read of a built-in driver port is the one shape the [`StreamShape`] decides
//! instead: it renders as list elements on either stream — the C's header rendering
//! ([`StreamShape::DriverOutput`], `erts/emulator/beam/io.c:3404-3435`) — which is the answer for the
//! two in-tree producers still on [`beam_io::DriverContext::input`], `tcp_inet`'s and `udp_inet`'s
//! passive `recv` verb (`beam_io/src/drivers/inet/{async,udp}.rs`), where the C answers a binary on a
//! `[binary]` port.
//!
//! The child's **stderr** follows the oracle exactly: it is *inherited* from the emulator
//! by default (so it appears on `beam_rt`'s own stderr and is never port data), and the
//! `stderr_to_stdout` option makes the child's descriptor 2 an exact duplicate of its
//! descriptor 1, so the diagnostics arrive as port data interleaved with stdout in true
//! write order (`spawn_start`'s `stderrfd = opts->redir_stderr ? ifd[1] : dup(2)`,
//! `erts/emulator/sys/unix/sys_drivers.c:500`).
//!
//! `port_command/2,3` writes to the child's **stdin** (an iolist, a binary or a char list),
//! which is what makes `open_port({spawn,"cat"},[])` echo its input back: the bytes are
//! queued through [`beam_io::PortRegistry::port_command`] and the scheduled `OUTPUT` task is
//! run immediately so they reach the child before the call returns. A `{fd, N}` port owns no
//! child pipe, but its adopted descriptor is **duplex** (`beam_io`'s `SpawnDriver` registers
//! it once with `Interest::BOTH`), so the same call writes out through that descriptor —
//! exactly the oracle's behaviour.
//!
//! **Not yet:** `connected` only flips to `false` at `port_close/1` rather than when the
//! child exits by itself; the `eof` option is accepted but inert (its `{Port, eof}` message
//! is not synthesized); and `nouse_stdio` is accepted without effect (the child always gets
//! the stdio pipes, never the `fd 3`/`fd 4` pair).
//!
//! # `port_info` items
//!
//! The pair list [`PortRegistry::info`] returns **is** the oracle's answer for both
//! arities: `beam_bifs`'s `erlang:port_info/1` filters it down to the C's `/1` roster
//! (`ERTS_PORT_INFO_1_ITEMS`, `erts/emulator/beam/erl_port.h:96-105`) and
//! `erlang:port_info/2` searches it whole. In order:
//!
//! `{name, …}`, `{links, [Owner]}`, `{id, N}`, `{connected, Owner}`, `{input, 0}`,
//! `{output, 0}`, `{os_pid, N|undefined}`, then the `/2`-only items `{monitors, []}`,
//! `{queue_size, N}`, `{memory, N}`, `{monitored_by, []}`, `{locking, Atom}` and
//! `{parallelism, false}` — plus a registered port's `{registered_name, Name}`, which the
//! oracle reports **first** (it is appended here and moved to the front by the native floor;
//! see `crate::native::port`).
//!
//! `links` is the owner process as a one-element list and `connected` is that owner pid,
//! matching erts-17.1 (nothing else links or monitors a port until `port_connect/2` or
//! `monitor(port, …)`). `input`/`output` are this port's cumulative byte counters. The
//! derived accounting items — `queue_size`, `memory`, `monitored_by`, `locking`,
//! `parallelism` — are [`info_extras`]'s, each carrying the reference C's derivation. The
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

use core::ffi::c_void;
use std::borrow::Cow;
use std::cell::{Cell, RefCell, RefMut};
use std::collections::{HashMap, HashSet, VecDeque};
use std::os::fd::RawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub mod driver;
pub(crate) mod async_ready;
use async_ready::AsyncReady;
// The C's "the caller suspends until the port is idle, then the command is re-issued"
// (`erts_port_output`'s `ERTS_PORT_OP_BUSY` arm), shared by `port_command/2,3` and `send/2,3`.
pub(crate) mod busy_wait;
pub(crate) mod completion;
pub(crate) mod exit;pub(crate) mod info_extras;
pub(crate) mod ops;
pub(crate) mod packet;
// The general select seam: `enif_select` (A.3) reached through `crate::nif_select`, plus
// `prim_tty:tty_select/2` (A.7), which is the read-only special case of it.
pub(crate) mod select;
pub(crate) mod watch;
use beam_alloc::Heap;
use beam_abi::driver::types::PORT_CONTROL_FLAG_BINARY;
use beam_bifs::PortHook;
use beam_common::{BeamError, ExitReason, Raise, Result};
use beam_engine::terms;
use libc::c_int;
use beam_io::{
    Driver, DriverRegistry, Event, Interest, Poller, PortCommandOptions, PortCommandOutcome,
    PortExitReason, PortNotification, PortRegistry as IoPortRegistry, PortTaskOutcome, ReadMessage,
    SpawnDriver, SpawnOptions, SpawnSpec, SpawnStream, StreamOptions, PORT_REDS_EXECUTE,
};
use beam_process::{PortId, PortMonitor, PortTable, ProcessId, ProcessTable};
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

/// How long a **nested** run waits for a live port to answer an asynchronous operation.
///
/// A passive `gen_tcp:recv/2,3`, a `gen_tcp:accept/1,2` and a `gen_udp:recv/2,3` all end in a
/// plain `receive` of the socket driver's own `{inet_async, Port, Id, Result}`
/// (`prim_inet.erl`'s `recv0/3`/`accept0/3`), which only a later port round can produce — so a
/// caller that is *not* a scheduled process (the nested `-eval`/`-s`/`-run`/boot route) has to
/// sit the wait out itself ([`crate::context::RuntimeContext::pump_once`]). The C has no bound
/// there (a suspended process waits for the driver, however long that takes); a single-threaded
/// runtime that owns its own pass cannot hang the whole executable, so the wait is bounded and
/// an expired deadline is the typed capability gap [`PORT_EVENT_DEADLINE_REASON`].
///
/// This is the read's analogue of `port::busy_wait::BUSY_PORT_DEADLINE` — the same five
/// seconds, the same "far longer than any real driver takes, far shorter than a user would
/// tolerate" reasoning — and it bounds **one round of no progress**: every delivered event or
/// executed slice restarts it, so a workload whose answers keep arriving is never cut short,
/// while a port that never answers reports the diagnostic instead of looping (the outer
/// `BLOCKING_PUMP_DEADLINE` still caps a peer that stays busy forever).
pub(crate) const PORT_EVENT_DEADLINE: Duration = Duration::from_secs(5);

/// Re-arms [`PORT_EVENT_DEADLINE`]'s no-progress window at **now** — the restart rule every nested
/// port wait applies to a round that made progress.
///
/// The bound measures *consecutive* rounds without progress, so a round that delivered a port
/// message or executed a slice moves the window forward: a workload whose completions keep arriving
/// is then measured from its last progress, never from the instant its wait began (which is what
/// computing the deadline once, at entry, would mean). A round that made **no** progress never
/// restarts it, so a port that never answers still reports [`PORT_EVENT_DEADLINE_REASON`] instead
/// of looping for ever.
///
/// Both nested waits apply this one rule — [`crate::context::RuntimeContext`]'s blocking port wait
/// (a passive `gen_tcp:recv`/`accept` under the `-eval`/`-s`/`-run`/boot route) and the `-eval`
/// bare `receive`'s port branch — so the windows cannot drift apart. The window is taken by `&mut`
/// because the progress path resumes the machine and returns: the write *states* the rule at the
/// point progress was established.
pub(crate) fn restart_port_event_window(window: &mut Instant) {
    *window = Instant::now() + PORT_EVENT_DEADLINE;
}
/// The capability-gap reason for a nested wait whose [`PORT_EVENT_DEADLINE`] expired with the
/// port still holding its operation.
pub(crate) const PORT_EVENT_DEADLINE_REASON: &str =
    "port event: no live port delivered the {inet_async, …} completion the caller waits for \
     within the 5 s bound (the C waits for ever)";
/// How long one poll round may block while the async-job pool has work in flight.
///
/// The pool's completions arrive on **worker** threads, which the poller knows nothing about, so a
/// round that blocked for a whole [`PORT_POLL_SLICE`] would report a finished `driver_async` job up
/// to 25 ms late — squarely inside the owner's `receive` latency. This bound shortens the wait to a
/// couple of milliseconds while
/// [`AsyncReady::pool_is_busy`](async_ready::AsyncReady::pool_is_busy) is true, which is the
/// polling substitute for the C's `erts_notify_check_async_ready_queue` wake-up
/// (`erl_async.c:174`). It is a bound, never a spin: an idle round still blocks for the whole
/// slice, and a quiet pool costs the clamp nothing.
const ASYNC_READY_POLL_BOUND: Duration = Duration::from_millis(2);

/// How long a child port's **line frames** may be held back while the port still owes its
/// `{Port, {exit_status, Status}}` (see [`PortRegistry::hold_or_report_line`]).
///
/// The C does not flush a child's trailing partial line while the child is still alive and an
/// exit status is still owed: `port_inp_failure` sees `dd->alive == 1` at the end of the
/// stream and returns **before** `driver_failure_eof`, and the flush happens later from the
/// `ready_input` call the child's exit triggers
/// (`erts/emulator/sys/unix/sys_drivers.c:1232-1252`), i.e. after `driver_report_exit`. That
/// deferral is what makes the status the first of the two messages.
///
/// It is unbounded in the C because the C is guaranteed to end it: a port whose input stream
/// ends is terminated (`driver_failure_eof` → `erts_deliver_port_exit`), which stops the child
/// and so always produces the status. This runtime keeps a child's port open until an explicit
/// close, so a child that closes its own stdout and keeps running would strand its trailing
/// frame — hence the bound: a frame held this long is delivered anyway (and the status, if it
/// ever comes, after it). Every real child reaps within one [`PORT_POLL_SLICE`], so the bound
/// is only reachable by a child that never exits.
pub(crate) const EXIT_STATUS_FLUSH_GRACE: Duration = Duration::from_secs(1);
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
    // The `/2`-only **derived** items (see [`info_extras`]).
    "queue_size",
    "memory",
    "monitored_by",
    "locking",
    "parallelism",
    // … and the two atoms `locking` answers with.
    "driver_level",
    "port_level",
];

/// The option **atoms** an `open_port/2` call accepts — the oracle's strict whitelist
/// (`erts/emulator/beam/erl_bif_port.c`).
///
/// Anything outside this list — including `list` (a list stream is already the default) — is
/// [`BeamError::BadArg`], never silently ignored. `binary` (the stream shape),
/// `line` (line framing at the default length), `use_stdio`/`nouse_stdio`, `stream` (the
/// packet-framing cancellation), `stderr_to_stdout` and `exit_status` all change the port;
/// `in`, `out`, `eof`, `hide` and `overlapped_io` are recognized and accepted without effect.
/// The last section of [`RuntimePorts::port_options`] documents what each one does.
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
/// `cd`, `env` and `args` are honored in the child, `line` sets the line framing and
/// `packet` sets the [`packet`] framing width (which the `line`/`packet` mutual-exclusion
/// check reads); the remaining tags
/// (`arg0`, `parallelism`, `busy_limits_port`, `busy_limits_msgq`) are recognized and
/// accepted without effect. A tag outside this list is [`BeamError::BadArg`].
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
    /// `{spawn_driver, Name}`: a **C driver** (contract A.4) `dlopen`ed from `Name` — no OS child.
    SpawnDriver,
    /// `{spawn_driver, Name}` where `Name` is a **built-in** driver this runtime links in
    /// (`fd`/`efile`/`tcp_inet` — the Rust analogue of the emulator's statically linked drivers,
    /// e.g. `tcp_inet` in `erts/emulator/drivers/common/inet_drv.c:1348`), resolved against
    /// [`beam_io::DriverRegistry`] rather than `dlopen`ed.
    ///
    /// Like [`PortName::SpawnDriver`] it is a **driver** port — so [`PortMeta::has_driver_control`]
    /// answers from the driver's own `control` capability, captured when the port is opened — but
    /// **unlike** it its bytes do not come from a C driver's inline `output` callback: they arrive
    /// through the driver's poller-registered descriptor on the port-task round, and
    /// [`PortRegistry::write_command`] takes the ordinary `port_command` path instead of running
    /// the driver's `output` callback inline.
    SpawnDriverBuiltin,
    /// `{fd, N}`: a descriptor the Erlang side already owns.
    Fd,
}

impl PortName {
    /// The [`StreamShape`] this `Name` form's reads carry — the producer fact a read cannot say
    /// about itself.
    ///
    /// A **built-in** driver port renders through [`StreamShape::DriverOutput`], because its
    /// header-less reads are the ones [`beam_io::DriverContext::input`] — the only sink with no
    /// split attached — still feeds. Every other producer states its own header/body split in the
    /// read ([`StreamShape::Data`]): a child's stdout, an `fd` port, a `dlopen`ed driver's
    /// `driver_output*`.
    fn stream_shape(self) -> StreamShape {
        match self {
            PortName::SpawnDriverBuiltin => StreamShape::DriverOutput,
            PortName::SpawnDriver | PortName::Spawn | PortName::SpawnExecutable | PortName::Fd => {
                StreamShape::Data
            }
        }
    }

    /// Whether the **emulator** owns this port's `{packet, N}` framing — the C's
    /// `prt->opts.packet_bytes`, which only the sys drivers read.
    ///
    /// `true` for the spawn/fd family (`io.c`'s `erts_sys_spawn`/`fd` drivers, whose
    /// `output`/`ready_input` callbacks prepend and strip the header:
    /// `erts/emulator/sys/unix/sys_drivers.c:1144-1160`, `:1268-1450`) and `false` for both
    /// driver forms, whose `packet` option belongs to the **driver** (an inet driver's
    /// `packet_inet_*`). Measured on the oracle: `open_port({spawn_driver,"ram_file_drv"},
    /// [{packet,4}])` answers a bare `SIZE` byte exactly as the unframed port does, so no
    /// header was added by the emulator — see [`packet`].
    fn frames_packets(self) -> bool {
        match self {
            PortName::Spawn | PortName::SpawnExecutable | PortName::Fd => true,
            PortName::SpawnDriver | PortName::SpawnDriverBuiltin => false,
        }
    }
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

/// How a delivered read is rendered **beyond** the header/body split it carries itself.
///
/// The split is the read's: [`ReadMessage`] carries the legacy C's `hbuf`/`hlen` and `buf`/`len`
/// halves apart (`driver_output2`/`driver_output_binary`/`driver_outputv`), so the payload builder
/// renders the C's shapes straight from it — the C's port-message builders
/// (`deliver_read_message`, `erts/emulator/beam/io.c:3363-3442`, and `deliver_vec_message`,
/// `:3501-3617`) always render the **header** bytes as list *elements* while the **data** bytes
/// follow the port's stream bit (`ERTS_PORT_SFLG_BINARY_IO`):
///
/// | the driver called | `[binary]` port's `{data, D}` | `[list]` port |
/// |---|---|---|
/// | `driver_output(buf, len)` (no header, body present) | `<<Data>>` | `[D1..Dn]` |
/// | `driver_output2(hbuf, hlen, buf, len)` | `[H1..Hh\|<<Data>>]` | `[H1..Hh,D1..Dn]` |
/// | `driver_output2(hbuf, hlen, buf, 0)` | `[H1..Hh\|<<>>]` | `[H1..Hh]` |
/// | `driver_output2(hbuf, hlen, NULL, 0)` (no body) | `[H1..Hh]` — a **proper** list | `[H1..Hh]` |
/// | `driver_output2(NULL, 0, NULL, 0)` | `[]` | `[]` |
///
/// A driver's reply is the interesting case in practice (`rfile_command`'s `numeric_reply` goes
/// through `driver_output2` and its `DATA` reply through `driver_output_binary`,
/// `erts/emulator/drivers/common/ram_file_drv.c:197-228,448-489`), while a byte-stream *read* —
/// a child's stdout, an `fd` port — is header-less and therefore a binary.
///
/// What a read cannot say is whether its producer stated a header: a header-less read is one body and
/// no header whether it came from a plain `driver_output` or from [`beam_io`]'s flat
/// [`beam_io::DriverContext::input`] sink, which carries no split at all.
/// [`StreamShape::DriverOutput`] supplies exactly that one producer fact, and only for a header-less
/// read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum StreamShape {
    /// The read's own header/body split decides the term (the table above): the header is a cons
    /// prefix, the body — present, empty or absent — the tail, and a header-less read with a body
    /// the bare binary (a char list on the `list` stream).
    ///
    /// This is every producer in this runtime except a built-in driver port: a child's stdout on
    /// the port-task round, an `fd` port, and a `dlopen`ed driver's `driver_output` on the driver's
    /// own input queue — and every reassembled `{packet, N}` packet, whose framing is the
    /// emulator's.
    #[default]
    Data,
    /// A **built-in driver port**'s **header-less** read: an in-tree driver
    /// ([`PortName::SpawnDriverBuiltin`] — `ram_file_drv`, `tcp_inet`, `udp_inet`) produced a read
    /// with no header, the shape every push through [`beam_io`]'s flat
    /// [`beam_io::DriverContext::input`] sink takes.
    ///
    /// Such a read renders as **list elements on either stream** — what the C gives the header half
    /// of every output call (`deliver_read_message`, `erts/emulator/beam/io.c:3404-3435`), so a
    /// `[binary]` port's `{data, D}` is a list of byte integers rather than a binary.
    ///
    /// That rendering is the oracle's **exact** proper list for a header-only reply, and diverges
    /// where the C answers a binary on a `[binary]` port: the producers still on that flat sink are
    /// `tcp_inet`'s and `udp_inet`'s passive `recv` verb
    /// (`beam_io/src/drivers/inet/{async,udp}.rs`), whose payload is one body-only read.
    ///
    /// A read that **does** carry a header keeps its own split (see [`StreamShape::Data`]), whatever
    /// the port's `Name` form: `ram_file_drv` hands its `OK`/`NUMBER`/`ERROR` up header-only and its
    /// `DATA` reply with both halves, so both render the oracle's own terms.
    DriverOutput,
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
    /// The resolved line-framing option — the C's `linebuf` ([`PortOptions::line`]):
    /// `Some(max_len)` puts the port in line mode (`beam_io`'s
    /// [`PortRegistry::set_line_mode`](beam_io::PortRegistry::set_line_mode)), `None` (the
    /// default) leaves it in raw mode.
    ///
    /// It is carried on the metadata (rather than merged into [`PortMeta::stream`]) because the
    /// two are **independent** port state bits (`[binary, line]` sets both; see
    /// [`StreamType::port_flags`]), and [`PortRegistry::register`] arms the framing from it.
    line: Option<usize>,
    /// The port's `{packet, N}` framing — the C's `prt->opts.packet_bytes`, or `None` when the
    /// option is off **or** the port is driver-backed ([`PortName::frames_packets`]).
    ///
    /// The framing is the emulator's own for the spawn/fd family:
    /// [`PortRegistry::write_command`] prepends the length header through
    /// [`packet::PacketFraming::frame_output`] and [`PortRegistry::fold_notifications`]
    /// reassembles the child's bytes into whole packets through
    /// [`packet::PacketFraming::push`]. A driver port carries `None` because its `packet`
    /// option belongs to the driver (see [`packet`]).
    ///
    /// The reassembler's partial packet lives **on the port**, so it dies with this metadata at
    /// [`PortRegistry::close`] — which is what keeps a reused port number from inheriting a
    /// dead child's half packet, and what makes an end-of-stream partial packet vanish exactly
    /// as the C's `port_inp_failure` discards it.
    packet: Option<packet::PacketFraming>,
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
    /// The two message producers route on it (see [`PortName::stream_shape`] and
    /// [`PortMeta::has_driver_control`] for the `port_control` gate's `Name`-form half): a built-in
    /// driver port's **header-less** reads render as list elements
    /// ([`StreamShape::DriverOutput`]), while a `dlopen`ed `{spawn_driver, Name}` port's reads carry
    /// the C's header/body split.
    kind: PortName,
    /// Whether this port's driver declares a **`control` callback** — the C's
    /// `prt->drv_ptr->control != NULL` test (`../../erts/emulator/beam/io.c:4035`), captured from
    /// the driver object **when the port is opened** because that object is moved into
    /// [`PortRegistry::register`] and is unreachable afterwards.
    ///
    /// The answer comes from the driver itself, never from a name: a static driver's
    /// [`beam_io::Driver::has_control`] (false for `ram_file_drv`, whose C entry leaves `control`
    /// `NULL`, `../../erts/emulator/drivers/common/ram_file_drv.c:116`), and a `dlopen`ed
    /// `{spawn_driver, Name}` port's `ErlDrvEntry::control` slot through
    /// [`driver::DriverPortAdapter::has_control`]. `false` for the `spawn`/`spawn_executable`/`fd`
    /// family, which is backed by [`beam_io::SpawnDriver`] — a driver with no `control` callback at
    /// all.
    ///
    /// Only [`PortMeta::has_driver_control`] reads it; that method is the gate's whole answer for a
    /// driver-backed port.
    driver_has_control: bool,
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
    /// The names a `{spawn_driver, Name}` port's C driver first interned inside a callback,
    /// awaiting a commit into the runtime's **live** atom table.
    ///
    /// A driver callback cannot mint an atom — the live table is borrowed for the whole callback
    /// and this crate is `#![deny(unsafe_code)]` — so a *new* name is handed a **predicted** index
    /// and recorded here, exactly like the BIF/NIF hosts' `PendingInterner`. Seized with
    /// [`driver::DriverPort::pending_atoms`] **before** the host is moved into the port lifecycle,
    /// for the same reason as [`PortMeta::driver_terms`] (the boxed driver exposes no host), and
    /// landed by [`PortRegistry::commit_driver_atoms`].
    ///
    /// `None` for every port without a C driver (a `spawn`/`fd` port).
    driver_atoms: Option<driver::DriverAtoms>,
    /// The control flags a `{spawn_driver, Name}` port's C driver last set through
    /// `set_port_control_flags` — the C's `prt->control_flags` (`io.c:5318`).
    ///
    /// Read **after** a `control` callback to pick the reply's Erlang form: a list of byte
    /// integers, or a binary when `PORT_CONTROL_FLAG_BINARY` is set (`write_port_control_result`,
    /// `io.c:4133-4175`). It is the **second** discriminator — the driver's reply pointer is tested
    /// first, and a `NULL` pointer is the empty list `[]` for **every** flag word (`io.c:4134`),
    /// which is [`PortRegistry::control_reply_shape`]'s fact. Seized from the host with
    /// [`driver::DriverPort::control_flags`] before the host is moved into the port lifecycle (the
    /// boxed driver exposes no host), the same way [`PortMeta::driver_terms`] and
    /// [`PortMeta::driver_atoms`] are.
    ///
    /// `None` for every port without a C driver (a `spawn`/`fd` port) — one whose `control` reply
    /// is the oracle's own `badarg`.
    driver_control_flags: Option<Rc<Cell<c_int>>>,
    /// The **busy state** of a `{spawn_driver, Name}` port's C driver — the C's
    /// `prt->sched.flags & ERTS_PTS_FLG_BUSY_PORT`, as the driver's own `set_busy_port` sets it.
    ///
    /// `erlang:port_command/2,3` consults it exactly as `erts_port_output` does (`io.c:1749-1764` and
    /// `erl_bif_port.c:192-220`): `nosuspend` on a busy port answers `false`, `force` bypasses the
    /// busy check, and a plain command on a busy port waits for the idle state and is then
    /// re-issued ([`busy_wait`]). The handle also carries
    /// whether the loaded entry advertises `ERL_DRV_FLAG_SOFT_BUSY`, which decides the `force` arm.
    /// Seized from the host with [`driver::DriverPort::busy_flag`] before the host is moved into the
    /// port lifecycle (the boxed driver exposes no host), the same way [`PortMeta::driver_terms`],
    /// [`PortMeta::driver_atoms`] and [`PortMeta::driver_control_flags`] are.
    ///
    /// `None` for every port without a C driver (a `spawn`/`fd` port) — and for a **built-in**
    /// driver port, whose entry flags are not exposed by the built-in registry. Both are the C's
    /// "never busy, no `SOFT_BUSY`" case, i.e. `force` ⇒ `notsup`, matching the oracle's own answers
    /// for `{spawn, Cmd}` and `{fd, N}` ports.
    driver_busy: Option<driver::BusyFlag>,
    /// The **OS child pid** a `{spawn_driver, Name}` port's C driver recorded through
    /// `erl_drv_set_os_pid` — the C's `prt->os_pid` (`io.c:2971`), which
    /// `port_info(Port, os_pid)` reads back (`erl_bif_info.c:3814-3817`).
    ///
    /// The C starts the field at `-1` (`io.c:386`) and the BIF renders a negative value as the atom
    /// `undefined`, so **unset is `None`** ([`driver::OsPidFlag`]): a driver that never called the
    /// hook answers `undefined`, exactly like the oracle
    /// (`open_port({spawn_driver,"ram_file_drv"},[])`).
    ///
    /// It is a shared handle, not a value, because the driver records the pid from inside its
    /// `start` callback — which runs **after** the port is registered — so the reader has to see a
    /// later write. Seized from the host with [`driver::DriverPort::os_pid_flag`] **before**
    /// [`driver::DriverPortAdapter::load`] consumes it (the boxed driver exposes no host), the same
    /// way [`PortMeta::driver_busy`], [`PortMeta::driver_terms`] and
    /// [`PortMeta::driver_control_flags`] are.
    ///
    /// `None` for every port without a C driver (a `spawn`/`fd` port, and a **built-in** driver
    /// port, whose Rust driver declares no OS child of its own) — those answer from
    /// [`PortMeta::os_pid`], the pid `beam_io` reports for the child it really forked.
    driver_os_pid: Option<driver::OsPidFlag>,
    /// The monitor tree a `{spawn_driver, Name}` port's C driver holds on Erlang processes
    /// (`driver_monitor_process`) — the C's per-port monitor list.
    ///
    /// Seized from the host with [`driver::DriverPort::monitors`] **before** the host is moved into
    /// the port lifecycle (the boxed driver exposes no host), the same way
    /// [`PortMeta::driver_terms`], [`PortMeta::driver_atoms`] and
    /// [`PortMeta::driver_control_flags`] are. The runtime owns the *observing* half the hook seam
    /// cannot reach — it holds the live process table — so it sweeps every monitored pid at the
    /// scheduler-pass boundary ([`PortRegistry::note_driver_monitor_exits`]) and marks the ones the
    /// table no longer holds, which is what makes the port's later drive point fire the driver's
    /// `ErlDrvEntry::process_exit` callback.
    ///
    /// `None` for every port without a C driver (a `spawn`/`fd` port) and for a built-in driver
    /// port, which holds no such tree.
    driver_monitors: Option<Rc<driver::DriverMonitors>>,
    /// Cumulative bytes this port has delivered **into** Erlang — `port_info(Port, input)`.
    ///
    /// Bumped exactly where the node-global [`IoCounters::input`] is (the bytes of a
    /// `{Port, {data, …}}` message), so the per-port figure is the port's own share of the same
    /// events rather than a second, independently-maintained count. A `Cell` because `info` reads
    /// it through the immutable `&PortMeta` the registry hands out.
    input: Cell<u64>,
    /// Cumulative bytes this runtime has written **to** this port — `port_info(Port, output)`.
    ///
    /// Bumped at the single write path ([`PortRegistry::write_command`]), alongside the
    /// node-global [`IoCounters::output`], so it counts what `port_command/2,3` handed the port
    /// exactly as the oracle's own `output` item does.
    output: Cell<u64>,
    /// Whether the child's `{Port, {exit_status, Status}}` has already been reported.
    ///
    /// [`beam_io::PortRegistry::take_exit_status`] hands a status out **once** and answers
    /// `Ok(None)` forever after, so "does this port still owe a status" cannot be asked of
    /// `beam_io`: it needs this latch. It is what
    /// [`PortMeta::owes_exit_status`] reads, and it is set at the one place a status is taken
    /// ([`PortRegistry::take_due_exit_statuses`]).
    exit_status_taken: Cell<bool>,
    /// The **line frames held back** because this port still owed its `exit_status` when they
    /// were harvested, in arrival order — the deliverable half of the C's `dd->alive == 1`
    /// deferral (`erts/emulator/sys/unix/sys_drivers.c:1232-1240`; see
    /// [`EXIT_STATUS_FLUSH_GRACE`] and [`PortMeta::owes_exit_status`]).
    ///
    /// [`PortRegistry::release_held_lines`] empties it — after the status has been reported, or
    /// once the frame has been held for the whole grace — into the round's events, so a held
    /// frame is delivered exactly once and the port's own frame order (arrival order) is kept.
    held_lines: VecDeque<PortEvent>,
    /// When the **first** frame currently in [`PortMeta::held_lines`] was held back — the grace's
    /// clock, cleared when the frames are released.
    held_since: Cell<Option<Instant>>,
}
impl PortMeta {
    /// Whether this port's driver declares a **`control` callback** — the C's
    /// `prt->drv_ptr->control != NULL` test (`../../erts/emulator/beam/io.c:4035`), which is what
    /// [`PortRegistry::port_control`] gates its `badarg` on.
    ///
    /// The answer is the driver's own, captured when the port was opened
    /// ([`PortMeta::driver_has_control`]): a `{spawn_driver, Name}` port — `dlopen`ed or
    /// **built in** — carries whatever its driver entry declares, so `{spawn_driver,
    /// "ram_file_drv"}` (no `control` slot) and `{spawn_driver, "udp_inet"}` (an `inet_control`
    /// callback) differ on the same `Name` form.
    ///
    /// The `Name` form is still the authority for the rest: every other form —
    /// `spawn`/`spawn_executable`/`fd`, all backed by [`beam_io::SpawnDriver`], which has no
    /// `control` callback at all — can never claim the capability, so `port_control/3` is the
    /// oracle's `badarg` for **every** verb on such a port.
    fn has_driver_control(&self) -> bool {
        match self.kind {
            PortName::SpawnDriver | PortName::SpawnDriverBuiltin => self.driver_has_control,
            PortName::Spawn | PortName::SpawnExecutable | PortName::Fd => false,
        }
    }

    /// Whether the owner is still **owed** this port's `{Port, {exit_status, Status}}`.
    ///
    /// This is the C's `dd->alive == 1` state — `exit_status` requested, a child that exists, and
    /// no status reported yet (`erts/emulator/sys/unix/sys_drivers.c:371` sets `alive` from the
    /// option, `:744` clears it when the child is reaped). It gates two things that must agree:
    /// the deferral of this port's line frames ([`PortMeta::held_lines`]) and, through it, the
    /// order in which the two messages of one child are delivered.
    ///
    /// A port with **no child** (a driver port, an `{fd, N}` port) never owes a status, so its
    /// frames are never held: `os_pid` is the truthful "this port owns an OS child" fact.
    fn owes_exit_status(&self) -> bool {
        self.notify_exit && self.os_pid.is_some() && !self.exit_status_taken.get()
    }
}

/// The recipient facts a surviving read's `{Port, {data, …}}` message needs, captured when a
/// **driver closed its own port** (`beam_io`'s `driver_failure*`/`driver_exit` family).
///
/// A driver delivers `driver_output*` **before** it fails, and `beam_io` re-prepends those bytes
/// for the port it then reaps ([`beam_io::PortRegistry`]'s `take_reaped_notifications`), so the
/// read really is reported — after [`PortRegistry::fail_port`] has already removed the port's
/// [`PortMeta`], which is where a read's recipient normally comes from. The oracle delivers
/// `{Port, {data, <<"hi">>}}` **then** `{'EXIT', Port, Reason}`, so the recipient is snapshotted
/// here, before the metadata goes, exactly as the completion channel snapshots its own
/// ([`completion::ClosedPortSnapshot`]).
///
/// Keyed by port **number** in [`PortRegistry::failed_read_recipients`] and consulted only when
/// the live metadata no longer holds that number, so the read renders exactly as a live port's
/// would. The entry is dropped when it is used, when the port's `Closed` report is folded (the
/// same round that retires the number), and when a new port registers under that number — so a
/// **recycled** number inherits nothing.
#[derive(Clone, Copy, Debug)]
struct FailedReadRecipient {
    /// The process the port belonged to — the recipient of the surviving read.
    owner: ProcessId,
    /// Its `binary`/`list` option.
    stream: StreamType,
    /// Its producer rendering rule (see [`StreamShape`]).
    shape: StreamShape,
}

/// A message a live port has produced: the child's stream bytes, or its reaped exit status.
///
/// This is [`PortRegistry::pump`]'s report, and the seam between the port poller and the
/// two things that turn it into an Erlang term: the scheduler path
/// ([`Runtime::deliver_port_messages`], which delivers it to `owner`'s mailbox) and the
/// `-eval` `receive` path ([`RuntimeContext::poll_port_messages_into_mailbox`], which builds
/// it and delivers it into the evaluating process's mailbox).
#[derive(Debug)]
enum PortEvent {
    /// One driver read, owed to the owner as `{Port, {data, Data}}`: `Data` is shaped by `stream`
    /// **and** by the read's own `hbuf`/`buf` split (the C's four shapes, [`ReadMessage`]), with
    /// `shape` naming the one producer fact a read cannot carry (see [`StreamShape`]).
    Data {
        /// The port the read came from.
        port: PortId,
        /// One `driver_output*` call's bytes with the C's header/body split intact — `header` is
        /// `hbuf`/`hlen` and `body` is `buf`/`len`, `None` being a `NULL` body
        /// (`erts/emulator/beam/io.c:3363-3442`).
        ///
        /// One read is **one** message, and its `len()` (header included) is what
        /// `erlang:statistics(io)` and `port_info(Port, input)` charge for it.
        read: ReadMessage,
        /// The port's `binary`/`list` option.
        stream: StreamType,
        /// The producer-specific rendering rule: [`StreamShape::DriverOutput`] for a built-in
        /// driver port, which decides only a **header-less** read (rendered as list elements on
        /// either stream), [`StreamShape::Data`] for every other producer here (a child's stream
        /// bytes, an `fd` port, a `dlopen`ed driver's `driver_output`) and for every reassembled
        /// `{packet, N}` packet.
        shape: StreamShape,
        /// The process the message belongs to.
        owner: ProcessId,
    },
    /// A **line-framed** read: one `{Port, {data, {eol|noeol, Data}}}` message.
    ///
    /// This is the `line`/`{line, N}` option's message shape — the C's
    /// `deliver_linebuf_message` (`erts/emulator/beam/io.c:3454`), whose wrapper
    /// `{Port, {data, {eol|noeol, Data}}}` is built at `io.c:3430-3435`. `eol` means the frame
    /// was terminated by a newline; `noeol` means it was a length-limited `{line, N}` chunk or
    /// the trailing partial line released at end-of-file. `Data` is shaped by `stream` exactly
    /// as a raw [`PortEvent::Data`] payload is: the frame's bytes are the **body** and `header`
    /// is consed in front of them (`deliver_linebuf_message` hands both to
    /// `deliver_read_message`, `io.c:3464`).
    ///
    /// Unlike [`PortEvent::Data`] — whose empty read is dropped, because a driver read of
    /// nothing is no message at all — an **empty `bytes` here is a real frame**: a blank line
    /// is the oracle's `{Port, {data, {eol, []}}}`. The line channel
    /// ([`beam_io::PortRegistry::take_line_notifications`]) is what feeds this variant; a
    /// line-mode port never reports the raw [`PortNotification::Data`] channel.
    Line {
        /// The port the frame came from.
        port: PortId,
        /// The frame's **header prefix**: the [`ReadMessage::header`] of the read that produced
        /// this frame, which the C conses in front of every frame that call completed
        /// (`deliver_read_message`, `erts/emulator/beam/io.c:3416-3428`) — so a
        /// `driver_output2("HDR",3,"a\nb\n",4)` yields two frames, each carrying `"HDR"`. Empty
        /// for a plain (header-less) read and for the end-of-file flush.
        header: Vec<u8>,
        /// The frame's content bytes (its terminator is not included; empty for a blank line).
        bytes: Vec<u8>,
        /// Whether the frame was newline-terminated (`eol`) or not (`noeol`).
        eol: bool,
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
    /// A **driver→owner completion**: the `{inet_reply, …}` answer to a `port_command/2` and the
    /// `{inet_async, …}` completion of a queued socket operation (see [`completion`]).
    ///
    /// Unlike every port-derived variant this one carries its **recipient already resolved**
    /// ([`completion::CompletionPacket::owner`]): a completion staged around a port's teardown
    /// outlives the port's [`PortMeta`], so the owner and the stream option are snapshotted at
    /// drain time. Nothing about it is recomputed at delivery, which is why
    /// [`PortEvent::port`] answers `None` for it.
    Completion {
        /// The drained completion, its recipient resolved.
        packet: completion::CompletionPacket,
    },
    /// An **`{active, …}` input delivery**: the `{tcp, S, Data}` / `{udp, S, IP, Port, Data}` /
    /// `{tcp_passive, S}` / `{udp_passive, S}` message a socket port's driver hands up while the
    /// port is in an active mode (see [`ops::active_input`]).
    ///
    /// Like [`PortEvent::Completion`] it carries its **recipient already resolved**
    /// ([`ops::active_input::ActiveInputPacket::owner`], together with the stream mode and the
    /// socket kind bound at drain time), so nothing is read from a port table the delivering
    /// route may no longer hold — which is why [`PortEvent::port`] answers `None` for it.
    ActiveInput {
        /// The drained delivery, its recipient resolved.
        packet: ops::active_input::ActiveInputPacket,
    },
    /// A **handled signal** arrived (`os:set_signal(Sig, handle)`) — the delivery half of the
    /// A.4/A.8 signal surface, drained from the self-pipe [`watch::SignalWatch`] registers.
    ///
    /// Unlike every other variant this one has **no port and no pid owner**: it is a
    /// node-global notification that ERTS addresses to the process registered as
    /// `erl_signal_server` (`signal_notify_requested`, `sys.c:589-604`), so the recipient is
    /// resolved by name in the delivering pump, not carried here. The message is the 2-tuple
    /// `{notify, Name}` with the emulator itself (`system`) as the sender.
    Signal {
        /// The signal's BEAM atom name — [`beam_io::signal::Signal::name`] (`"sigusr1"`, …).
        name: &'static str,
    },
    /// A registered descriptor became ready — the delivery half of **both** `enif_select`
    /// (contract A.3) and `prim_tty:tty_select/2` (A.7), which share one mechanism (see
    /// [`select`]).
    ///
    /// Like [`PortEvent::Signal`] this variant has **no port**: the descriptor is registered with
    /// the poller directly, not carried by any port, and the message's recipient is the process
    /// that armed the select (recorded here as `owner`), not a port-derived one.
    ///
    /// * A [`select::SelectPayload::Plain`] registration delivers the 4-tuple
    ///   `{select, Handle, Reference, EventAtom}` with the emulator itself (`system`) as the
    ///   sender, exactly the shape `prepare_select_msg`
    ///   (`erts/emulator/sys/common/erl_check_io.c:699-800`) builds and `send_select_msg`
    ///   (`:836-858`) queues.
    /// * A [`select::SelectPayload::Custom`] registration (`ERL_NIF_SELECT_CUSTOM_MSG`) delivers
    ///   the caller's own message term and nothing else — no tuple, no event atom (`:716-731`).
    ///
    /// Either way the report is **one-shot**: the registration is de-registered once it has
    /// reported, and **one readiness can produce several of these events** — one per observed
    /// direction the registration asked for, in the C's `input` → `output` → `error` order
    /// (`erl_check_io.c:2126-2175`; see [`select::SelectWatch::take_ready`]).
    Select {
        /// The process the select message belongs to — `enif_select`'s implicit recipient (the
        /// calling process).
        owner: ProcessId,
        /// What a readiness report delivers — the plain shape's reference, or the caller's own
        /// message.
        payload: select::SelectPayload,
        /// The direction actually observed, as the C's event atom.
        event: select::SelectEvent,
        /// The registration's opaque handle term — the plain shape's second element until the
        /// resource term is minted in its place.
        handle: Eterm,
        /// The resource object a NIF's notification term is minted from, or null.
        ///
        /// Moved out of the registration and never dereferenced: it only ever flows into
        /// [`beam_abi`]'s resource-term minting. A `prim_tty:tty_select/2` arming carries none.
        obj: *mut c_void,
        /// The interest's **own reference** on [`PortEvent::Select::obj`], or `None`.
        ///
        /// It travels with the delivery so the object outlives the disarm that produced this event,
        /// and it is released only **after** the notification's resource term is minted (the mint
        /// takes its own reference): releasing it earlier could destroy the object between the
        /// disarm and the mint. See [`select::SelectHold`].
        hold: Option<select::SelectHold>,
    },
}

impl PortEvent {
    /// The port this event is **about**, or `None` for the events that already carry their
    /// recipient resolved (a completion, an `{active, …}` delivery) and for the two node-global
    /// kinds that no port carries (a handled signal and a ready select registration).
    ///
    /// Used where an event has to be checked against the registry's live ports before it is
    /// reported — [`PortRegistry::drain_pending_events`], which drops a retained event whose port
    /// has since been closed — and by the test suites' drain helpers, so the roster of port-bearing
    /// variants lives in exactly one place.
    fn port(&self) -> Option<PortId> {
        match self {
            PortEvent::Data { port, .. }
            | PortEvent::Line { port, .. }
            | PortEvent::Exited { port, .. }
            | PortEvent::Term { port, .. } => Some(*port),
            // A completion's recipient is already resolved, so there is nothing to re-derive
            // from a live port: filtering it against [`PortRegistry::meta`] would drop exactly
            // the completions a port's teardown produced (`async_error_am_all`), which are the
            // ones its owner is still owed. It carries its own owner, so a recycled port number
            // cannot mis-deliver it either.
            PortEvent::Completion { .. } => None,
            // An `{active, …}` delivery is bound to its owner, mode and kind at drain time (see
            // [`ops::active_input`]), so — like a completion — filtering it against
            // [`PortRegistry::meta`] would drop exactly the deliveries a closing port's owner is
            // still owed, and a recycled port number could not mis-deliver it either.
            PortEvent::ActiveInput { .. } => None,
            PortEvent::Signal { .. } | PortEvent::Select { .. } => None,
        }
    }

    /// Visits the terms this event carries, mutably, so a **moving** collector can rewrite the
    /// words in place — the per-variant half of
    /// [`PortRegistry::for_each_pending_event_root_mut`].
    ///
    /// Only two variants carry a raw `Eterm`: [`PortEvent::Term`] (a C driver's emitted term) and
    /// [`PortEvent::Select`] (a reported readiness's payload and its opaque handle). Every other
    /// variant holds bytes, ids, atoms or pids — or a `Completion`/`ActiveInput` packet, which is
    /// rendered into a term from its own bytes at delivery time and stores none.
    ///
    /// # Errors
    ///
    /// Whatever `f` answers; nothing is written back beyond what `f` itself wrote, because each
    /// slice is a direct view of the stored word.
    fn for_each_root_mut(
        &mut self,
        f: &mut impl FnMut(&mut [Eterm]) -> Result<()>,
    ) -> Result<()> {
        match self {
            PortEvent::Term { term, .. } => f(core::slice::from_mut(term)),
            PortEvent::Select { payload, handle, .. } => {
                f(core::slice::from_mut(handle))?;
                payload.for_each_payload_root_mut(f)
            }
            PortEvent::Data { .. }
            | PortEvent::Line { .. }
            | PortEvent::Exited { .. }
            | PortEvent::Completion { .. }
            | PortEvent::ActiveInput { .. }
            | PortEvent::Signal { .. } => Ok(()),
        }
    }
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

/// The runtime's most recent **process-liveness observation**, published for the per-port driver
/// hosts to read.
///
/// # Why this exists
///
/// A C driver's `driver_monitor_process` has three outcomes, not two: the C answers `0` (armed),
/// `1` (`ERL_DRV_MONITOR_NOT_ALIVE`, the advertised process is gone) or `-1` (no `process_exit`
/// slot / invalid port), and `do_driver_monitor_process`
/// (`erts/emulator/beam/io.c:7012-7038`) reaches the positive one exactly when the monitor cannot
/// be armed because the target is no longer alive. A per-port [`driver::DriverPort`] host is built
/// **before** its port is registered and holds no process table — only its owning [`ProcessId`] —
/// so it cannot ask that question itself. The runtime therefore **publishes** what it last observed
/// into this shared view, and the host queries it from inside the callback.
///
/// # A snapshot, not a live query
///
/// The view is a *snapshot* of the live pids taken at a moment the runtime chose
/// ([`PortRegistry::refresh_liveness`]), refreshed at the **start of every port round** — the one
/// place driver callbacks run — so a callback reached from inside a round consults an observation
/// at most one round old.
///
/// A callback that runs **outside** a round (the driver's own `start` during `open_port/2`, or a
/// `control` callback during `erlang:port_control/3`) sees the view from the last round; with no
/// round yet the view is `None`, which the host answers by installing the monitor anyway — the
/// C's own "monitor the target" behaviour for a process it believes alive. That fallback is
/// explicit and documented, never a fabricated `NotAlive`: a process nobody has observed is not
/// reported dead.
///
/// # Borrowing
///
/// [`ProcessLiveness::observe`] / [`clear`](ProcessLiveness::clear) run from the runtime, never
/// from inside a driver callback, and never re-enter, so they may use `borrow_mut`.
/// [`is_alive`](ProcessLiveness::is_alive) is called *from* a driver callback — which may itself
/// hold unrelated runtime state borrowed — so it is `try_borrow`-based and answers `None` ("no
/// observation available") rather than panicking.
#[derive(Debug)]
pub(crate) struct ProcessLiveness {
    /// The observed live pids, or `None` when no observation is being kept (nothing can ask for
    /// one yet — see [`PortRegistry::refresh_liveness`]).
    live: RefCell<Option<HashSet<ProcessId>>>,
}

impl ProcessLiveness {
    /// A viewless observation — the state a port host starts in.
    pub(crate) fn new() -> Rc<ProcessLiveness> {
        Rc::new(ProcessLiveness {
            live: RefCell::new(None),
        })
    }

    /// Replaces the view with `pids` — the runtime's most recent observation of who is alive.
    pub(crate) fn observe(&self, pids: impl Iterator<Item = ProcessId>) {
        *self.live.borrow_mut() = Some(pids.collect());
    }

    /// Drops the view back to "no observation" (nothing can ask for one — see
    /// [`PortRegistry::refresh_liveness`]).
    pub(crate) fn clear(&self) {
        *self.live.borrow_mut() = None;
    }

    /// Whether the last observation saw `pid`, or `None` when there is no observation to consult
    /// (or the cell is unreadable).
    pub(crate) fn is_alive(&self, pid: ProcessId) -> Option<bool> {
        let live = self.live.try_borrow().ok()?;
        live.as_ref().map(|live| live.contains(&pid))
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
    /// The **statically linked emulator** driver factories a `{spawn_driver, Name}` port is
    /// resolved against **before** any `dlopen` — the Rust analogue of the C `driver_tab.c` table
    /// ([`beam_io::DriverRegistry::with_static_spawn_drivers`], exactly
    /// [`beam_io::STATIC_SPAWN_DRIVERS`]).
    ///
    /// Built **once**, when this registry is constructed, so an `open_port/2` only asks for a
    /// factory here. The membership gate is [`beam_io::is_static_spawn_driver`], **not** this
    /// table: `beam_io`'s internal `fd`/`efile` drivers are emulator-internal and the oracle refuses
    /// `open_port({spawn_driver,"fd"}, …)` with `badarg`, so a non-static name never reaches this
    /// table — it falls through to the `.so` loader exactly as before.
    static_spawn_drivers: DriverRegistry,
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
    /// The signal self-pipe's registration with [`PortRegistry::poller`] — the delivery half of
    /// `os:set_signal/2` (see [`watch`]).
    watch: watch::SignalWatch,
    /// The runtime's registrations of `enif_select`/`prim_tty:tty_select/2` interests with
    /// [`PortRegistry::poller`] — the delivery half of both (see [`select`]).
    select: select::SelectWatch,
    /// The runtime's most recent process-liveness observation, shared with every
    /// `{spawn_driver, Name}` port's [`driver::DriverPort`] host — the state behind
    /// `driver_monitor_process`'s positive `ERL_DRV_MONITOR_NOT_ALIVE` (see
    /// [`ProcessLiveness`]).
    liveness: Rc<ProcessLiveness>,
    /// The state `port_set_data/2`, `erlang:register/2` and `port_connect/2` establish
    /// **after** the port is open — the data term, the registered name and the link set (see
    /// [`ops::PortExtras`]).
    extras: ops::PortExtras,
    /// Events a port-task round has already **harvested** but no [`PortRegistry::pump`] has
    /// reported yet.
    ///
    /// A round is destructive: [`beam_io::PortRegistry::execute_port_tasks`] drains every live
    /// port's buffered input into [`beam_io::PortNotification::Data`]
    /// ([`beam_io::Port::take_input_notifications`]), so a round run *outside* a pump would
    /// otherwise throw its bytes away. The one such round is
    /// [`PortRegistry::write_command`]'s — it runs the `OUTPUT` task that carried the command,
    /// and a **built-in** driver (`ram_file_drv`) writes its reply from that very
    /// `output` call, so the reply is in that round's outcome. Folding it here retains it until
    /// the next pump reports it.
    ///
    /// This is still **exactly once**: the bytes left `beam_io`'s input queue when they were
    /// harvested, so no later round can produce them again, and
    /// [`PortRegistry::drain_pending_events`] takes the buffer as it reports it.
    pending_events: Vec<PortEvent>,
    /// The `'EXIT'`/`'DOWN'` messages a closing port owes its links and monitors — snapshotted
    /// at close and released when the close report is folded (see [`exit`]).
    pending_port_signals: exit::PendingPortSignals,
    /// Recipient facts for the bytes a **driver-failed** port had already delivered, keyed by
    /// port number (see [`FailedReadRecipient`]).
    ///
    /// [`beam_io`] preserves the reads of a port a driver closed itself and reports them ahead of
    /// its close report, but [`PortRegistry::fail_port`] removes the port's [`PortMeta`] first —
    /// so the read would have no recipient. [`PortRegistry::fail_port`] records one here, and
    /// [`PortRegistry::fold_notifications`]' `Data` arm falls back to it. A port number never
    /// inherits an entry: it is consumed by the read it was recorded for, dropped when the port's
    /// `Closed` report is folded, and cleared when a new port registers under that number.
    failed_read_recipients: HashMap<u64, FailedReadRecipient>,
    /// Every live port's incoming monitor set, read from the authoritative
    /// [`beam_process::PortTable`] **immediately before** each port-task round — the reading a
    /// **driver-failed** port's `'DOWN'` messages are built from (see
    /// [`PortRegistry::fail_port`](super::PortRegistry)).
    ///
    /// The failure channel needs it because a round that reaps a driver-failed port mints that
    /// port's [`PortNotification::Closed`] report in the very same call, and minting is what
    /// retires its **number** ([`beam_io::PortRegistry::execute_port_tasks`] →
    /// `take_closed_notifications` → [`PortTable::release`]) — which drops the number's incoming
    /// monitors along with its owner and links. [`PortRegistry::drain_failures`] therefore runs
    /// strictly *after* the port has left the table, so this
    /// [`PortRegistry::capture_round_monitors`] reading is the last truthful statement of who was
    /// watching it.
    ///
    /// Replaced (never accumulated) by every round, so an entry belongs to the round that is
    /// about to run and is consumed by the failure that round reaps; a port that survives the
    /// round is simply re-read next time.
    pre_round_monitors: HashMap<u64, Vec<PortMonitor>>,
    /// The runtime's **async-job delivery handle** — the C async pool behind `driver_async` and the
    /// per-port completion queues (see [`AsyncReady`] and [`driver`]).
    ///
    /// This registry is the party that **drains the pool** (a port round has no driver, and the
    /// pool's ready queue is global), while each port's own drive point calls the driver. The handle
    /// is shared by `Rc` with every driver host and every port adapter, so the pool is built once for
    /// the whole runtime.
    async_ready: AsyncReady,
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
        Ok(PortRegistry {
            poller: Poller::new()?,
            registry: IoPortRegistry::new(),
            meta: HashMap::new(),
            static_spawn_drivers: DriverRegistry::with_static_spawn_drivers(),
            heap: Heap::new(PORT_HEAP_WORDS)?,
            io_counters: Rc::new(IoCounters::default()),
            watch: watch::SignalWatch::new(),
            select: select::SelectWatch::new(),
            liveness: ProcessLiveness::new(),
            extras: ops::PortExtras::default(),
            pending_events: Vec::new(),
            pending_port_signals: exit::PendingPortSignals::default(),
            failed_read_recipients: HashMap::new(),
            pre_round_monitors: HashMap::new(),
            // The pool itself is built lazily, on the first `driver_async` (`AsyncReady::ensure_pool`),
            // but the worker count is stamped now: `+A`'s default is one thread, and `+A 0` — an
            // explicit zero — must stay distinguishable from "not configured yet".
            async_ready: {
                let async_ready = AsyncReady::new();
                async_ready.set_threads(driver::DEFAULT_ASYNC_THREADS);
                async_ready
            },
        })
    }
    /// Sets the **async-job worker count** — the emulator's `+A N` (`erl_async.c`'s
    /// `ERTS_ASYNC_THREADS_DEFAULT` is [`driver::DEFAULT_ASYNC_THREADS`]).
    ///
    /// `0` is `+A 0` and is honoured as a genuine zero-worker pool: every job then runs **inline on
    /// the submitting thread** (`AsyncPool::new(0)`), which is exactly the C's `+A 0` behaviour, and
    /// `driver_async` answers `0`. It is deliberately **not** "no pool": that would send
    /// `driver_async` down the frozen seam's legacy hook and answer `-1`.
    ///
    /// The runtime calls this once, at startup, from `RuntimeConfig`'s preserved `+A` flag (see
    /// `crate::runtime`'s registry construction). A pool that already exists is rebuilt, so this
    /// belongs **before** the first driver port is opened.
    pub fn set_async_threads(&mut self, threads: usize) {
        self.async_ready.set_threads(threads);
    }
    /// The runtime's async-job delivery handle (see [`AsyncReady`]) — what a port's host and adapter
    /// are stamped with at `open_port/2`.
    pub fn async_ready(&self) -> AsyncReady {
        self.async_ready.clone()
    }
    /// Moves every completed `driver_async` job from the pool into the per-port delivery queues
    /// (see [`AsyncReady::offer`]) and releases any library whose port's work is gone
    /// ([`AsyncReady::reap_unloads`]).
    ///
    /// Called once per port round, **before** the round's drive points, so a job that finished while
    /// the runtime was busy is delivered by the very round that notices it. A job whose port has
    /// closed is released here — the C's `!p` branch — so its `async_free` runs even when no port is
    /// left to drive.
    fn drain_async_pool(&mut self) {
        let Some(pool) = self.async_ready.pool() else {
            return;
        };
        let drained = pool.drain();
        if !drained.is_empty() {
            self.async_ready.offer(drained);
        }
        // Order matters: the completions are offered first, so a job that just became terminal no
        // longer counts against its port's deferred unload in this round.
        self.async_ready.reap_unloads();
    }
    /// Whether the async-job pool has anything in flight — what shortens a poll round to
    /// [`ASYNC_READY_POLL_BOUND`] and keeps a blocked owner wakeable (see [`AsyncReady::pool_is_busy`]).
    fn async_pool_is_busy(&self) -> bool {
        self.async_ready
            .pool()
            .is_some_and(|pool| self.async_ready.pool_is_busy(&pool))
    }

    /// Constructs the **statically linked emulator** driver named `name`, or `None` when `name` is
    /// not one of them (so the caller falls through to the `.so` loader).
    ///
    /// The membership test is [`driver::is_static_driver`] — `beam_io`'s emulator `driver_tab.c`
    /// set — **not** "is a factory registered here": that distinction is the whole point. A name
    /// outside the set is the oracle's `badarg` whatever this runtime can build, so `"fd"`/`"efile"`
    /// (this crate's internal drivers, reachable through their own `{fd, N}`/`spawn` forms) and an
    /// invented name take the `dlopen` route and fail there, exactly as the system runtime fails
    /// them. [`beam_io::DriverRegistry::create_static_spawn`] applies that gate and then the factory
    /// lookup.
    ///
    /// The `Some` wraps the factory's own `Result`: a **static** driver whose construction fails
    /// (say a descriptor the OS refuses) is reported as that typed failure — never a fabricated
    /// port — and is distinct from `None`, meaning "no such emulator driver at all".
    fn create_static_spawn_driver(&self, name: &str) -> Option<Result<Box<dyn Driver>>> {
        driver::is_static_driver(name).then(|| self.static_spawn_drivers.create_static_spawn(name))
    }

    /// The shared I/O byte counters behind `erlang:statistics(io)` (see [`IoCounters`]).
    ///
    /// The runtime keeps a clone so its `statistics(io)` arm reads the counters without
    /// borrowing this registry.
    pub fn io_counters(&self) -> Rc<IoCounters> {
        Rc::clone(&self.io_counters)
    }

    /// The shared process-liveness view a `{spawn_driver, Name}` port's C-driver host answers
    /// `driver_monitor_process` from (see [`ProcessLiveness`]).
    ///
    /// A cheap [`Rc`] clone, taken while the driver host is built in
    /// [`RuntimePorts::open_port`] — the host outlives every later borrow, so it must hold the
    /// handle rather than ask the registry.
    pub(crate) fn liveness(&self) -> Rc<ProcessLiveness> {
        Rc::clone(&self.liveness)
    }

    /// Republishes the runtime's liveness observation for the per-port driver hosts.
    ///
    /// **Gated on a live `{spawn_driver, Name}` port.** Only that kind of port has a C driver that
    /// can call `driver_monitor_process` (`meta.driver_monitors` is `Some` exactly then), so with
    /// none of them the view is *cleared* and no snapshot is ever built — a runtime that never
    /// opened such a port pays nothing for this. With one, `table.pids()` is collected, which is
    /// the whole cost of the observation.
    ///
    /// Called at the **start of every port round** — the one place driver callbacks normally run
    /// — so the view a callback consults is at most one round old. A callback reached outside a
    /// round (`start`, `control`) sees the last round's view, or `None` when no round has run yet;
    /// the host's fallback for `None` is to install the monitor (see [`ProcessLiveness`]).
    fn refresh_liveness(&self, table: &ProcessTable) {
        if self
            .meta
            .values()
            .any(|meta| meta.driver_monitors.is_some())
        {
            self.liveness.observe(table.pids());
        } else {
            self.liveness.clear();
        }
    }

    /// Reads every live port's incoming monitor set into
    /// [`PortRegistry::pre_round_monitors`] — **before** the round that may reap one of them.
    ///
    /// This is the failure channel's only chance: [`beam_io::PortRegistry::execute_port_tasks`]
    /// reaps a driver-failed port and then mints its close report in the same call, and minting
    /// retires the port's number, which is what drops its monitors from the
    /// [`PortTable`](beam_process::PortTable). The drain that renders the failure's `'DOWN'`
    /// messages runs after the round, so it must find them here.
    ///
    /// Only a port that actually has watchers takes an entry, so a runtime whose ports are
    /// unmonitored pays one hash probe per live port and allocates nothing. (A round already
    /// collects and sorts the whole port-number list twice, so the probe is not the cost.)
    fn capture_round_monitors(&mut self) {
        // Field-level destructuring keeps the three borrows disjoint: `PortTable` is reached
        // through `registry`, the port list from `meta`, and the snapshot written here.
        let PortRegistry {
            registry,
            meta,
            pre_round_monitors,
            ..
        } = self;
        pre_round_monitors.clear();
        for number in meta.keys() {
            let monitors = registry.port_table().monitors(PortId::new(*number));
            if !monitors.is_empty() {
                pre_round_monitors.insert(*number, monitors.to_vec());
            }
        }
    }

    /// Runs one `beam_io` port-task round — the crate's single entry to
    /// [`beam_io::PortRegistry::execute_port_tasks`].
    ///
    /// The round is where a driver's own `driver_failure*`/`driver_exit` is reaped *and* where
    /// the reaped port's close report retires its number, so the monitor snapshot that teardown
    /// needs is taken first (see [`PortRegistry::capture_round_monitors`]). Every other step is
    /// `beam_io`'s, unchanged.
    ///
    /// # Errors
    ///
    /// As for [`beam_io::PortRegistry::execute_port_tasks`] — a driver error raised while a task
    /// runs; the round's closes then stay queued for the next one.
    fn run_port_tasks(&mut self, reds_budget: usize) -> Result<PortTaskOutcome> {
        self.capture_round_monitors();
        let outcome = self.registry.execute_port_tasks(&mut self.poller, reds_budget)?;
        if crate::debug_trace::enabled() {
            crate::debug_trace::trace(format_args!(
                "port tasks: ran={} reds={} notif={}",
                outcome.ran,
                outcome.reds_used,
                outcome.notifications.len()
            ));
        }
        Ok(outcome)
    }

    /// Runs one `beam_io` readiness round — [`beam_io::PortRegistry::handle_events`], the round
    /// that exists only to mint outstanding close reports.
    ///
    /// It reaps a driver-failed port exactly as [`PortRegistry::run_port_tasks`] does, so it
    /// takes the same monitor snapshot first.
    ///
    /// # Errors
    ///
    /// As for [`beam_io::PortRegistry::handle_events`].
    fn handle_port_events(&mut self, events: &[Event]) -> Result<PortTaskOutcome> {
        self.capture_round_monitors();
        self.registry.handle_events(&mut self.poller, events)
    }
    /// Arms a **one-shot** read interest on the tty descriptor (fd 0) for `owner` — the
    /// `enif_select` half of `prim_tty:tty_select/2` (see [`tty`]).
    /// Arms a **one-shot** interest on `fd` for `owner` — the registration half of `enif_select`
    /// (contract A.3) and of `prim_tty:tty_select/2`, which is a special case of it (see
    /// [`select`]).
    ///
    /// `payload`'s terms and `handle` must already be **stabilized** terms (copied onto the
    /// runtime's never-reset arena by [`Runtime::stabilize_term`]): a raw `Eterm` held across a
    /// scheduler pass belongs to a heap a collection may relocate, and this registration outlives
    /// the call that armed it. The caller — [`crate::native::prim_tty`] for the tty half, the NIF
    /// dispatch for the `enif_select` half — owns that stabilisation, because it holds the
    /// arguments and the runtime's arena.
    ///
    /// `obj` is the NIF's `void*` resource operand (null when the producer has none, as
    /// `prim_tty:tty_select/2` has). It is **moved** into the registration and the eventual
    /// [`PortEvent::Select`] and never dereferenced — this crate is `#![deny(unsafe_code)]`.
    ///
    /// # Errors
    ///
    /// The poller's own registration failure (a closed descriptor, say), or the typed exhaustion
    /// of the key reservation. The C **discards** `enif_select`'s answer, so `tty_select/2`
    /// answers `ok` either way — but the failure is still returned here rather than swallowed.
    pub(crate) fn arm_nif_select(
        &mut self,
        owner: ProcessId,
        fd: RawFd,
        interest: Interest,
        payload: select::SelectPayload,
        handle: Eterm,
        obj: *mut c_void,
    ) -> Result<()> {
        self.select
            .arm(&mut self.poller, fd, interest, owner, payload, handle, obj)
    }

    /// De-registers the interest armed on `fd`, answering which directions it had armed — the
    /// C's `ERL_NIF_SELECT_STOP` (`erl_check_io.c:1272-1284`, see [`select`]).
    ///
    /// The C decides a `STOP` before it decodes the mask, rewrites the mode to
    /// `ERL_DRV_READ | ERL_DRV_WRITE | ERL_DRV_USE` (`:1274-1276`) and runs its "off" path, so the
    /// answer is the armed interest of the registration the stop removed: one
    /// `ERL_NIF_SELECT_*_CANCELLED` bit per direction really cleared (`:1470-1482`). An empty
    /// [`select::Cancelled`] is a descriptor that was never armed — the C's own fast track
    /// (`:1276-1280`).
    ///
    /// # Errors
    ///
    /// The poller's own de-registration failure.
    pub(crate) fn stop_nif_select(
        &mut self,
        fd: RawFd,
        handle: Eterm,
    ) -> Result<select::Cancelled> {
        self.select.stop(&mut self.poller, fd, handle)
    }

    /// Clears the `interest` directions armed on `fd`, answering which of them really were armed —
    /// the C's `ERL_NIF_SELECT_CANCEL` and its `ERL_NIF_SELECT_*_CANCELLED` bits
    /// (`erl_check_io.c:1292`, `:1470-1482`; see [`select`]).
    ///
    /// # Errors
    ///
    /// The poller's own re-registration or de-registration failure.
    pub(crate) fn cancel_nif_select(
        &mut self,
        fd: RawFd,
        handle: Eterm,
        interest: Interest,
    ) -> Result<select::Cancelled> {
        self.select.cancel(&mut self.poller, fd, handle, interest)
    }

    /// Arms a **one-shot** read interest on the tty descriptor for `owner` — the `enif_select`
    /// half of `prim_tty:tty_select/2` (see [`select`]).
    ///
    /// It is [`PortRegistry::arm_nif_select`] with the C's own `tty_select_nif` arguments: the
    /// hardcoded descriptor [`select::TTY_IFD`], a read interest, the caller's reference as the
    /// plain payload's third tuple element and no resource object.
    ///
    /// # Errors
    ///
    /// As [`PortRegistry::arm_nif_select`]; the C discards the answer, so the NIF answers `ok`
    /// either way and only logs the failure.
    pub(crate) fn arm_tty_select(
        &mut self,
        owner: ProcessId,
        handle: Eterm,
        reference: Eterm,
    ) -> Result<()> {
        self.arm_nif_select(
            owner,
            select::TTY_IFD,
            Interest::READABLE,
            select::SelectPayload::Plain { reference },
            handle,
            core::ptr::null_mut(),
        )
    }

    /// Drops (and de-registers) every select registration whose owner `live` denies — the C's
    /// `tty_monitor_down` `ERL_NIF_SELECT_STOP` arm (see [`select`]).
    ///
    /// Called from both port pumps, immediately after [`PortRegistry::refresh_liveness`], so a
    /// process that died without disarming its select cannot keep the runtime polling for ever.
    ///
    /// # Errors
    ///
    /// The poller's own de-registration failure.
    pub(crate) fn prune_dead_select(
        &mut self,
        live: impl FnMut(ProcessId) -> bool,
    ) -> Result<()> {
        self.select.prune_dead(&mut self.poller, live)
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
        // Arm line framing on the freshly-registered port when `open_port/2` resolved a
        // `line`/`{line, N}` option (`Some`), so `beam_io`'s line machinery — which is off
        // otherwise — becomes reachable. This is the single funnel every `open_port/2` Name
        // form registers through, so it covers all of them. `set_line_mode` only refuses an
        // unknown port, which `open` has just made live.
        if let Some(max_len) = meta.line {
            self.registry.set_line_mode(id, Some(max_len))?;
        }
        // A number the arena has handed out again starts clean: a surviving read's snapshot from
        // a previous port under this number is not this port's (see [`FailedReadRecipient`]).
        self.failed_read_recipients.remove(&id.number());
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

    /// Hands a command to the port the way `erlang:port_command/2,3` does, taking the command's
    /// bytes from `make_data` **only once the port has accepted the command** — the C's own order.
    ///
    /// The C resolves the port and tests the driver's flag word **before** `erts_port_output` runs
    /// (`erts_internal_port_command_3`, `erts/emulator/beam/erl_bif_port.c:190-198`), and
    /// `erts_port_output` answers `ERTS_PORT_OP_BUSY` from the port's scheduler flags **before** it
    /// converts `Data` (`io.c:1749-1754`, against the `erts_ioq_iodata_vec_len`/
    /// `io_list_to_buf` conversion at `io.c:1790`/`:1866`). A `[nosuspend]` command on a busy port
    /// therefore answers the atom `false` even when `Data` is **not iodata at all** — the command is
    /// dropped, never `badarg`. A bytes-first seam cannot reproduce that (the caller converts first,
    /// so the conversion failure wins), which is why this variant takes a producer and calls it
    /// only on the accept path.
    ///
    /// The pre-conversion gate is [`PortRegistry::decide_before_data`], the C's two checks that
    /// answer without looking at `Data`; a command that passes it is handed to
    /// [`PortRegistry::command`], where [`beam_io`]'s decision table re-decides on the converted
    /// bytes and the accepted command's round and byte accounting run. The two halves read the same
    /// facts, so a command the gate accepts cannot then be refused as busy.
    ///
    /// # Errors
    ///
    /// As for [`PortRegistry::command`], plus whatever `make_data` returns (the caller's own iodata
    /// conversion — `badarg` for a term that is not iodata).
    pub fn command_with_data<F>(
        &mut self,
        id: PortId,
        make_data: F,
        options: PortCommandOptions,
    ) -> Result<PortCommandOutcome>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        if let Some(outcome) = self.decide_before_data(id, options)? {
            return Ok(outcome);
        }
        let bytes = make_data()?;
        self.command(id, &bytes, options)
    }
    /// The C's **pre-conversion** half of the `port_command` decision: the checks that answer
    /// without ever looking at the data, in the C's order.
    ///
    /// * the port must be live — `sig_lookup_port`'s `badarg` (`erl_bif_port.c:190-193`), which
    ///   [`beam_io::PortRegistry::is_busy`] reports as its own `badarg` for an unknown port;
    /// * `[force]` is refused `error:notsup` unless the driver **entry** advertises
    ///   `ERL_DRV_FLAG_SOFT_BUSY` (`erl_bif_port.c:195-198`) — a fact about the entry, so it holds
    ///   on an idle port too;
    /// * otherwise a **busy** port is `ERTS_PORT_OP_BUSY` (`io.c:1749-1754`): nothing is queued,
    ///   and the caller answers `false` (`[nosuspend]`) or hands itself to
    ///   [`busy_wait::issue`]'s wait-and-re-issue for the idle state.
    ///
    /// `Some(outcome)` is that answer; `None` means the command is accepted so far and its bytes
    /// may be converted.
    ///
    /// Both facts are the ones the port layer's own decision reads — the state through
    /// [`beam_io::Driver::is_busy`] (whom [`beam_io::PortRegistry::is_busy`] forwards to) and the
    /// entry flag through [`PortRegistry::soft_busy`], the runtime's mirror of the
    /// `ERL_DRV_FLAG_SOFT_BUSY` bit a loaded driver entry reported (the very handle
    /// [`driver::DriverPortAdapter::flags`] answers from) — so the gate can never disagree with the
    /// decision that follows it.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port this registry does not hold (closed, or never opened), and
    /// the `notsup` raise for `[force]` on a driver without `ERL_DRV_FLAG_SOFT_BUSY`.
    fn decide_before_data(
        &self,
        id: PortId,
        options: PortCommandOptions,
    ) -> Result<Option<PortCommandOutcome>> {
        let busy = self.registry.is_busy(id)?;
        if options.is_force() {
            if !self.soft_busy(id) {
                return Err(notsup());
            }
            return Ok(None);
        }
        Ok(busy.then_some(PortCommandOutcome::Busy))
    }
    /// Hands `bytes` to the port the way `erlang:port_command/2,3` does, and reports what the
    /// port's **busy state** and the command options made of it.
    ///
    /// This is [`PortRegistry::command_with_data`]'s **eager** form: the bytes are already
    /// converted, so the C's order (busy before conversion) is not observable and a caller holding
    /// a *term* must use `command_with_data` instead.
    ///
    /// The **decision** is [`beam_io`]'s — [`beam_io::PortRegistry::port_command_with`], whose
    /// [`beam_io::Port::command`] is the crate's one implementation of the C's decision table
    /// (`erts/emulator/beam/erl_bif_port.c:192-235`, `io.c:1731`, `:1763-1769`) — never re-derived
    /// here. This method owns the two halves `beam_io` cannot: the **round** that makes an accepted
    /// write really happen before the answer, and the byte accounting that must follow the accept.
    ///
    /// # The accepted command
    ///
    /// `Done` ⇒ the `OUTPUT` task the command scheduled is run **now**
    /// ([`beam_io::PortRegistry::execute_port_tasks`]), so the stream has been handed the data
    /// before this returns (the BIF's `true` answer means exactly that). For a child port that
    /// stream is the child's stdin; for a `{fd, N}` port it is the **adopted, duplex** descriptor;
    /// for a `{spawn_driver, Name}` port that task *is* the driver's own `output` callback
    /// ([`beam_io::Driver::ready_output`]'s default calls `output`), which is what the C's
    /// `port_command` on a loaded driver does.
    ///
    /// That round is also what a **built-in** driver answers from: `ram_file_drv` hands its reply up
    /// as one completed read ([`beam_io::DriverContext::deliver_read`]) during the `OUTPUT` task, and
    /// the round harvests it as a [`beam_io::PortNotification::Data`] — draining the port's read
    /// queue, so no later round can see it. The outcome is therefore **folded into**
    /// [`PortRegistry::pending_events`] (through the same [`PortRegistry::fold_notifications`] the
    /// pump uses), and the next [`PortRegistry::pump`] reports it as the owner's
    /// `{Port, {data, Data}}` with both byte counters advanced.
    ///
    /// # The byte accounting follows the accept
    ///
    /// `erlang:statistics(io)`'s output total and the port's own `port_info(Port, output)` figure
    /// are bumped **only for a command that was accepted**: a command the driver entry refuses
    /// (`notsup`) and a `Busy` one that was never queued both write nothing and so count nothing.
    /// Both are bumped with the **payload** length — a `{packet, N}` port's emulator-side length
    /// header is not part of what the oracle reports (measured: 3, not 7, for a 3-byte payload on
    /// `{packet,4}`).
    ///
    /// A `{packet, N}` port's header is prepended **before** the command reaches `beam_io`,
    /// exactly as the reference C's sys driver does in its own `output`/`outputv` callbacks
    /// (`sys_drivers.c:1144-1160`, `:1062-1077`). Only the spawn/`{fd,N}` family is framed by the
    /// emulator ([`PortName::frames_packets`]), and such a port can never report busy, so the
    /// header is prepended before the decision without that being observable — the decision itself
    /// stays `beam_io`'s alone.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port this registry does not hold (closed, or never opened) —
    /// the oracle's own reason for `port_command` on a dead port; the `notsup` raise for `[force]`
    /// on a driver that does not advertise `ERL_DRV_FLAG_SOFT_BUSY`, **idle or busy**, decided
    /// before any output is attempted; the framer's growth refusal for an oversized `{packet, N}`
    /// payload; and whatever the write itself failed with (a child that closed its stdin yields
    /// `EPIPE`).
    pub fn command(
        &mut self,
        id: PortId,
        bytes: &[u8],
        options: PortCommandOptions,
    ) -> Result<PortCommandOutcome> {
        let wire: Cow<'_, [u8]> = match self.meta.get(&id.number()) {
            Some(meta) if meta.kind.frames_packets() => match meta.packet.as_ref() {
                Some(framing) => Cow::Owned(framing.frame_output(bytes)?),
                None => Cow::Borrowed(bytes),
            },
            _ => Cow::Borrowed(bytes),
        };
        let outcome = self
            .registry
            .port_command_with(&mut self.poller, id, &wire, options)?;
        if outcome == PortCommandOutcome::Busy {
            // Nothing was queued, so nothing is written and nothing is counted: the caller owns
            // the bytes and the options and re-issues the command once the driver is idle.
            return Ok(outcome);
        }
        // Accepted — only now do the bytes count as output. `erlang:statistics(io)`'s total and
        // the port's own `port_info(Port, output)` figure are bumped at the same instant, so the
        // two agree by construction.
        self.io_counters.note_output(bytes.len());
        if let Some(meta) = self.meta.get_mut(&id.number()) {
            meta.output
                .set(meta.output.get().saturating_add(bytes.len() as u64));
        }
        // The command is only queued and its task scheduled; run the round now so the write really
        // happened (and so a driver port's `output` callback has answered) before the caller is
        // told `true`. A driver that closes its own port from that very callback is reaped and
        // retired by this round, which is why the round is entered through
        // [`PortRegistry::run_port_tasks`] (it snapshots the monitors the teardown owes).
        let round = self.run_port_tasks(PORT_REDS_EXECUTE);
        let (notifications, refused) = match round {
            Ok(outcome) => (outcome.notifications, None),
            // A driver that refused the command may still owe its owner a reply, a completion or
            // its `{'EXIT', …}`: the channels below are drained before the refusal is reported
            // (the round's own `Closed` report stays queued for the next one).
            Err(error) => (Vec::new(), Some(error)),
        };
        // The `OUTPUT` task that carried this command is where a socket driver stages its
        // `{inet_reply, …}` (see [`completion`]) and its `{active, …}` delivery (see
        // [`ops::active_input`]), and where a driver that closes its **own** port latches the
        // failure — so both channels are drained and held as pending work until a pump reports
        // them.
        //
        // The drain comes **before** the round's notifications are folded, the order
        // [`PortRegistry::pump`] uses and the only order that keeps a failing callback's own read:
        // [`PortRegistry::drain_failures`] is what finishes the teardown
        // ([`PortRegistry::fail_port`]) — removing the port's metadata and purging the events
        // retained for it — so a fold that ran first would bind the read to metadata the teardown
        // is about to discard and then lose the event to that purge. Folded after, the read is
        // rendered from the recipient the teardown snapshotted ([`FailedReadRecipient`]), which is
        // the snapshot the oracle's `{data,<<"hi">>}` **then** `{'EXIT',P,9}` row needs.
        // The `active` grants this round's driver callbacks spent are harvested **before** anything
        // is folded or drained, so the mirror the peer-close gate reads (`ops::inet_opts`) is never
        // a grant behind the driver (see [`PortRegistry::drain_active_spends`]).
        self.drain_active_spends();
        let completions = self.drain_io_completions(None);
        // The round harvested the replies the drivers just wrote (and every other live port's
        // buffered input along with them — a round drains them all), so its notifications are
        // **retained** here for the next pump: the queue they came from is already empty.
        let mut events = Vec::new();
        self.fold_notifications(notifications, &mut events)?;
        self.pending_events.extend(events);
        self.pending_events.extend(completions);
        let active = self.drain_active_inputs();
        self.pending_events
            .extend(active.into_iter().map(|packet| PortEvent::ActiveInput { packet }));
        match refused {
            Some(error) => Err(error),
            None => Ok(PortCommandOutcome::Done),
        }
    }

    /// Writes `bytes` to the port's stream — `erlang:port_command/2`, i.e. [`PortRegistry::command`]
    /// with **no** options.
    ///
    /// A busy port is reported as the typed capability gap [`busy_wait::NO_PUMP_REASON`] raises
    /// for a plain command on one: this seam holds no [`Runtime`](crate::runtime::Runtime) and so
    /// has no authority to pump the scheduler, which is the wait the C's `erts_suspend` +
    /// `ERTS_BIF_YIELD3` re-run needs. Every other outcome is [`Ok`] — the bytes really were handed
    /// to the stream, or the driver really did answer them. The pumping callers (`port_command/2,3`
    /// and `send/2,3`) go through [`busy_wait::issue`] instead.
    ///
    /// # Errors
    ///
    /// As for [`PortRegistry::command`], plus the typed capability gap for a busy port.
    pub fn write_command(&mut self, id: PortId, bytes: &[u8]) -> Result<()> {
        match self.command(id, bytes, PortCommandOptions::EMPTY)? {
            PortCommandOutcome::Done => Ok(()),
            PortCommandOutcome::Busy => Err(error::unsupported(busy_wait::NO_PUMP_REASON)),
        }
    }

    /// Delivers a named port-control operation to the port's driver — the runtime side of
    /// `erts_internal:port_control/3`.
    ///
    /// The operation is forwarded **verbatim** to [`beam_io::PortRegistry::port_control`], which
    /// hands it to the port's driver: for a [`PortName::SpawnDriver`] port that is
    /// [`DriverPortAdapter::control`](driver::DriverPortAdapter::control), i.e. the C driver's own
    /// `control` callback (`op` is then the verb as a decimal `c_uint` string —
    /// `erts_internal:port_control/3` reads its `Command` with `term_to_Uint`,
    /// `../../erts/emulator/beam/erl_bif_port.c:322-340`, the same convention `beam_io` uses,
    /// `beam_io/src/abi/port.rs:716`), and for a [`PortName::SpawnDriverBuiltin`] port the built-in
    /// driver's own string verbs.
    ///
    /// The answer is the driver's reply **bytes only**: rendering them as an Erlang term needs the
    /// two facts the oracle reads after the callback — the reply buffer's shape
    /// ([`PortRegistry::control_reply_shape`], whose `NULL` case is `[]` for every flag word,
    /// `io.c:4134`) and the port's binary flag ([`PortRegistry::control_binary`]). Both are read
    /// after this call, which is when the driver has had its say.
    ///
    /// [`beam_io::PortControl::CLOSE`] (`"close"`) is **reserved** by that forwardee: it closes the
    /// port instead of reaching a driver. A C driver's verbs are integers, so the reserved verb can
    /// only arrive from a caller that meant the built-in drivers' close — and it closes the port
    /// through `beam_io`, which is why a caller that must keep this registry's own metadata in step
    /// uses [`PortRegistry::close`] instead.
    ///
    /// # A port whose driver has no `control` callback is `badarg` for every verb
    ///
    /// The C's `call_driver_control` answers `ERTS_PORT_OP_BADARG` **before** any callback for a
    /// driver whose `control` slot is `NULL` (`io.c:4035`), and `erts_internal_port_control_3`
    /// turns that into the atom `badarg` (`erl_bif_port.c:340-343`). So a port whose driver
    /// declares no `control` callback answers the oracle's `badarg` for **every** verb (measured),
    /// never an empty `Ok(vec![])`. Two such ports exist:
    ///
    /// * every **internal** port — `{spawn, Cmd}` / `{spawn_executable, Path}` / `{fd, N}`, all
    ///   backed by [`beam_io::SpawnDriver`], which has no `control` callback at all;
    /// * a `{spawn_driver, Name}` port whose **driver entry** declares none — measured on
    ///   `{spawn_driver,"ram_file_drv"}` (`ram_file_drv.c:116`'s `NULL, /* control */`), where
    ///   `port_control(P, 1, <<0,0,0,3,97,98,99>>)` — a verb the driver *does* answer through
    ///   `port_command` — is still `badarg`.
    ///
    /// [`PortRegistry::has_driver_control`] is that capability question; it deliberately precedes
    /// the reserved-`"close"` forward, because the oracle's command is a `c_uint`, so a `"close"`
    /// string cannot arise from a real `port_control` call and rejecting the driverless port first
    /// is what matches the C. A `{spawn_driver, Name}` port whose driver *does* declare `control`
    /// — a loaded `.so` such as the `busy_driver` fixture, or the built-in `tcp_inet`/`udp_inet` —
    /// reaches its callback.
    ///
    /// Note that the driver's **own** rejection of an operand is `badarg` too (a C driver's
    /// negative `cres`, `io.c:4074`; a built-in's malformed operand). The two converge on the same
    /// exception, so they must be kept distinct by *cause* when reasoning about a reply: the gate
    /// here never consults the verb, while the driver's rejection is per-verb.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port this registry does not hold (closed, or never opened), for
    /// a port whose driver declares no `control` callback (above), and for the driver's own
    /// rejection (a negative result); the typed capability gap for a reply the driver answers from
    /// memory of its own (see [`driver::DriverPortAdapter::control_reply`]).
    pub fn port_control(&mut self, id: PortId, op: &str, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.has_driver_control(id) {
            // A port whose driver declares no `control` callback — the oracle's `badarg` for every
            // verb (`call_driver_control`, `io.c:4035`) — or a port this registry does not hold,
            // which is `badarg` too. Both are the atom `badarg`, never a fabricated reply.
            return Err(BeamError::BadArg);
        }
        let reply = self.registry.port_control(&mut self.poller, id, op, arg);
        // A control op can in principle stage a completion (`prim_inet`'s `recv`/`accept` arms
        // queue an operation that completes later), so the channel is drained here too — the
        // drain is unconditional, because a failing control call must not also lose the reply
        // it staged.
        self.drain_active_spends();
        let completions = self.drain_io_completions(None);
        self.pending_events.extend(completions);
        // The same callback is where a socket driver stages an `{active, …}` delivery (a
        // `setopts` that reads inline does), so that channel is drained here too.
        let active = self.drain_active_inputs();
        self.pending_events
            .extend(active.into_iter().map(|packet| PortEvent::ActiveInput { packet }));
        // A `setopts` control op is also where a socket port's option block becomes part of its
        // mirror: record the argument the driver **accepted** — its reply's leading
        // `INET_REP_OK` tag (`inet_drv.c:870`) — which is what maintains the port's own
        // `mode`/`active`/`exit_on_close` latches: the *data* mode every inet payload renderer
        // resolves through [`PortRegistry::data_mode`], and the two latches the `peer_closed`
        // gate reads. An accepted socket's block is inherited inside its driver
        // (`TcpInetDriver::from_accepted`), so nothing here is retained for a replay.
        // The reply is read, never consumed: a refused `setopts` is the
        // `Ok([0]einval)` reply and records nothing, and a failing call keeps its own path.
        if ops::inet_opts::is_setopts_control(op) {
            if let Ok(bytes) = &reply {
                let _ = self.note_inet_setopts(id, arg, ops::inet_opts::setopts_succeeded(bytes));
            }
        }
        reply
    }

    /// Whether `id`'s driver declares a **`control` callback** — the C's
    /// `prt->drv_ptr->control != NULL` test (`io.c:4035`), and therefore whether
    /// `erlang:port_control/3` reaches a callback at all.
    ///
    /// The capability is a fact about the **driver**, not about the port's `Name` form: a
    /// `{spawn_driver, Name}` port carries whatever its driver entry declares, which is why
    /// `{spawn_driver,"ram_file_drv"}` (no `control` slot) is `false` while
    /// `{spawn_driver,"udp_inet"}` — same `Name` form — is `true`. The per-`Name`-form answer is
    /// [`PortMeta::has_driver_control`].
    ///
    /// It is what [`PortRegistry::port_control`] gates its `badarg` on. `false` for a port this
    /// registry does not hold.
    pub fn has_driver_control(&self, id: PortId) -> bool {
        self.meta
            .get(&id.number())
            .is_some_and(PortMeta::has_driver_control)
    }
    /// Whether `id`'s C driver asked for a **binary** `erlang:port_control/3` reply
    /// (`PORT_CONTROL_FLAG_BINARY`).
    ///
    /// This is the **second** of the two facts the oracle's reply rendering decides on
    /// (`write_port_control_result`, `io.c:4133-4175`). The first is the driver's reply pointer,
    /// which it tests **before** this flag word: a `NULL` pointer is the empty **list** `[]` for
    /// every flag setting (`io.c:4134`), while a real buffer splits on the bit — a binary when it
    /// is set, a list of byte integers when it is clear (`io.c:4136-4166`). A renderer therefore
    /// asks [`PortRegistry::control_reply_shape`] first and only then this.
    ///
    /// Call this after [`PortRegistry::port_control`], which is when the driver has set its flags.
    ///
    /// `false` for an unknown port and for every port without a C driver (a `spawn`/`fd` port),
    /// whose `control` reply is the oracle's `badarg` in the first place.
    pub fn control_binary(&self, id: PortId) -> bool {
        self.meta
            .get(&id.number())
            .and_then(|meta| meta.driver_control_flags.as_ref())
            .is_some_and(|flags| flags.get() & PORT_CONTROL_FLAG_BINARY != 0)
    }
    /// How `id`'s C driver described the **reply buffer** of its most recent `control` call — the
    /// first of the two facts the oracle's reply rendering decides on.
    ///
    /// The C tests the driver's reply pointer before the port's `PORT_CONTROL_FLAG_BINARY` word:
    /// `write_port_control_result` opens with `if (!resp_bufp) return NIL;` (`io.c:4134`), so a
    /// `NULL` reply is the empty **list** `[]` regardless of the flag, while a zero-byte *buffer*
    /// under that flag is `<<>>` (`io.c:4136-4162`). [`beam_io::ControlReplyShape`] names the two
    /// cases; [`beam_io::PortRegistry::control_reply_shape`] answers from the port's own driver,
    /// which is where the pointer's `NULL`-ness survives (the reply bytes cannot carry it — both
    /// cases are zero bytes).
    ///
    /// Call this after [`PortRegistry::port_control`], beside the flag
    /// ([`PortRegistry::control_binary`]): it reads the **last** reply, so anything that drove the
    /// port's `control` in between would describe that call instead.
    ///
    /// [`ControlReplyShape::Buffer`](beam_io::ControlReplyShape::Buffer) for an unknown port and for
    /// every port without a C driver (a `spawn`/`fd` port, and a **built-in** driver port, whose
    /// Rust driver answers a real `Vec`) — the honest default, and in the first two cases a port
    /// whose `port_control` is the oracle's `badarg` anyway. The one residual is a port a driver
    /// **reaped from inside its own `control` callback**: [`PortRegistry::port_control`] drains the
    /// completion channel after the call, so the port is gone by the time this reads it and the
    /// answer is that same default. `beam_io`'s own
    /// [`port_control_reply`](beam_io::PortRegistry::port_control_reply) is the combined call, and
    /// the shape it answers for that case is correct — a caller that must not lose it can use that
    /// one directly.
    pub fn control_reply_shape(&self, id: PortId) -> beam_io::ControlReplyShape {
        self.registry
            .control_reply_shape(id)
            .unwrap_or(beam_io::ControlReplyShape::Buffer)
    }

    /// Whether `id`'s port is currently **busy** — the state the driver last reported through
    /// `set_busy_port` (the C's `prt->sched.flags & ERTS_PTS_FLG_BUSY_PORT`, `io.c:5208-5264`).
    ///
    /// `erlang:port_command/2,3` consults it exactly as `erts_port_output` does: a busy port answers
    /// `false` for `nosuspend`, waits for the idle state before a plain command is re-issued
    /// ([`busy_wait`]), and lets `force` through — when the driver entry advertises
    /// `ERL_DRV_FLAG_SOFT_BUSY` ([`PortRegistry::soft_busy`]).
    ///
    /// `false` for an unknown port and for every port without a C driver (a `spawn`/`fd` port and a
    /// built-in driver port), none of which can ever report busy — the oracle's own answer for a
    /// `{spawn, Cmd}`/`{fd, N}` port.
    pub fn is_busy(&self, id: PortId) -> bool {
        self.meta
            .get(&id.number())
            .and_then(|meta| meta.driver_busy.as_ref())
            .is_some_and(driver::BusyFlag::is_busy)
    }

    /// Whether `id`'s C driver **entry** advertises `ERL_DRV_FLAG_SOFT_BUSY` — the bit
    /// `erlang:port_command/2,3`'s `force` arm tests (`erl_bif_port.c:195-198`).
    ///
    /// `true` only for a `{spawn_driver, Name}` port whose loaded `ErlDrvEntry::driver_flags`
    /// carries the bit; `false` for an unknown port, for every `spawn`/`fd` port, for a built-in
    /// driver port and for a `{spawn_driver, Name}` port whose entry does not advertise it — in
    /// every one of which `[force]` is the oracle's `error:notsup`.
    pub fn soft_busy(&self, id: PortId) -> bool {
        self.meta
            .get(&id.number())
            .and_then(|meta| meta.driver_busy.as_ref())
            .is_some_and(driver::BusyFlag::soft_busy)
    }

    /// The process-monitor tree a `{spawn_driver, Name}` port's C driver holds — the handle the
    /// runtime **marks** a dead monitored process through, so the port's later drive point fires
    /// `ErlDrvEntry::process_exit`.
    ///
    /// Taken before the driver host is boxed into the port lifecycle and stored in the port's
    /// [`PortMeta`] ([`PortMeta::driver_monitors`]); shared by `Rc`, so this is the very tree the
    /// driver's own `driver_monitor_process`/`driver_demonitor_process` hooks touch.
    ///
    /// `None` for an unknown port, for a `spawn`/`fd` port and for a built-in driver port — none of
    /// which has a C driver that could hold a monitor.
    pub(crate) fn driver_monitors(&self, id: PortId) -> Option<Rc<driver::DriverMonitors>> {
        self.meta
            .get(&id.number())
            .and_then(|meta| meta.driver_monitors.as_ref())
            .map(Rc::clone)
    }

    /// Marks every monitor a port's C driver holds on a process the live `table` **no longer
    /// holds**, answering how many monitors were newly marked.
    ///
    /// This is the runtime's **observing** half of the A.4 monitor family: the driver hook seam
    /// (`driver_monitor_process`) lets a C driver install a watch, but the per-port host holds only
    /// its owning pid — not the process table — so *this* sweep is where a monitored process's death
    /// is noticed. Every live port whose C driver holds monitors contributes its distinct watched
    /// pids ([`driver::DriverMonitors::monitored_pids`]); each one the table does not hold is marked
    /// through [`driver::DriverMonitors::note_process_exit`]. Both a monitor installed on a pid that
    /// was already dead and a cascade of deaths since the last sweep are therefore marked, and
    /// marking is **idempotent** — a second sweep marks nothing new, so a deferred one costs nothing
    /// but a repeat.
    ///
    /// The mark is a fact about the port's own metadata, not a message: nothing is delivered, so the
    /// caller (a scheduler pass, immediately before its port round) simply proceeds and the
    /// **same** round's port tasks fire the driver's `process_exit` for the newly pending monitors
    /// (the port asks for a drive point as soon as one is pending,
    /// [`driver::DriverPortAdapter::has_pending_async`]).
    pub(crate) fn note_driver_monitor_exits(&self, table: &ProcessTable) -> usize {
        let mut marked = 0usize;
        for number in self.meta.keys().copied() {
            // Through the accessor, so "which ports even have a monitor tree" and "how one is read"
            // stay one answer (a port without a C driver contributes nothing).
            let Some(monitors) = self.driver_monitors(PortId::new(number)) else {
                continue;
            };
            for pid in monitors.monitored_pids() {
                if !table.contains(pid) {
                    marked += monitors.note_process_exit(pid);
                }
            }
        }
        marked
    }    /// The registry's port-number table, mutably — the seam the **port-monitor** BIFs drive.
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
    ///
    /// This counts **ports only**. It is deliberately *not* the "is there anything to poll?"
    /// question — a signal self-pipe or an armed select registration can be waited on with
    /// zero ports — so a caller deciding whether a blocked process can still be woken asks
    /// [`PortRegistry::has_nothing_to_poll`].
    pub fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }

    /// Whether there is nothing a poll round must **wait on**, and nothing left that could wake
    /// a blocked process: no live port, no armed select registration (`enif_select`'s or
    /// `tty_select/2`'s — see [`select`]), and no **fired** `os:set_signal/2` self-pipe (see
    /// [`watch`]).
    ///
    /// This is the "nothing pending" predicate. It is what tells a caller that a blocked process
    /// can no longer be woken and that a poll round is pure sleep: a live port can deliver
    /// `{Port, {data, …}}`, an armed select can deliver `{select, Handle, Ref, EventAtom}`, and a
    /// self-pipe **holding bytes** can deliver `{notify, Signal}` — any one of them keeps the
    /// runtime awake.
    ///
    /// An **armed but idle** `handle` disposition is deliberately **not** counted
    /// ([`watch::SignalWatch::has_pending_signal`], not `is_pollable`): its descriptor is idle
    /// and can only become readable when a signal actually arrives, so a round that blocked on
    /// it would be waiting for nothing. A signal that *has* been caught makes the pipe readable
    /// and so is pending work — and it is delivered either way, because
    /// [`PortRegistry::pump`] drains the facility unconditionally at the top of every round.
    ///
    /// An event a port-task round already harvested ([`PortRegistry::pending_events`]) is pending
    /// work too — it is a message the owner is still owed — so it keeps the runtime awake until a
    /// pump has reported it. The same holds for the `'EXIT'`/`'DOWN'` messages of a port that has
    /// closed or is still to be reported as closed ([`PortRegistry::pending_port_signals`]): they
    /// are messages a process is owed, and a route that skipped the poll because "nothing is
    /// live" would strand them (see [`exit`]).
    pub fn has_nothing_to_poll(&self) -> bool {
        self.meta.is_empty()
            && self.pending_events.is_empty()
            && self.pending_port_signals.is_empty()
            && !self.select.is_armed()
            && !self.watch.has_pending_signal()
            // A `driver_async` job in flight is a `{Port, {data, …}}` (or a freed job's obligation)
            // the owner is still owed, and it is finished by a **worker thread** the poller knows
            // nothing about — so a route that skipped the round because "nothing is live" would
            // never see its completion (see [`AsyncReady`]).
            && !self.async_pool_is_busy()
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
        // The `'EXIT'`/`'DOWN'` messages this close owes its links and monitors are
        // snapshotted **here**, while the port's metadata, link set and incoming monitors are
        // still readable; they become deliverable once `beam_io` reports the close (see
        // [`exit`]).
        // Everything a completion this close produces still needs must be read **here**: it
        // outlives the metadata removed below (see [`completion::ClosedPortSnapshot`]).
        let snapshot = self.closed_snapshot(id);
        self.capture_port_exit(id);
        // The port-exit notification first: the driver sees `port_exit` before the teardown
        // `stop`, and the port is still live for it.
        let notified = self
            .registry
            .notify_port_exit(&mut self.poller, id, PortExitReason::Closed);
        // The port is leaving this registry either way: a close the runtime has begun is
        // never abandoned halfway, so a failed notification cannot leave the table stale.
        self.meta.remove(&id.number());
        // The port's name and data term go with it: a closed port is `undefined` for
        // `whereis/1` and `badarg` for `port_get_data/1`, and a recycled port number starts
        // clean (see `ops::PortExtras`).
        self.extras.forget_port(id.number());
        // Bytes a round harvested for this port but no pump has reported yet are dropped: a closed
        // port has no recipient. Purging them here (rather than only at the drain) is what keeps a
        // **reused** port number — `beam_io` retires a number only once its close has been
        // reported — from inheriting the dead port's messages.
        self.pending_events
            .retain(|event| event.port() != Some(id));
        let closed = self.registry.close(&mut self.poller, id);
        // The teardown is where a driver answers its queued operations with `{error, closed}`
        // (the C's `async_error_am_all`), and `beam_io` harvests those into a registry-owned
        // queue precisely so they are still reported exactly once. Draining here — with the
        // snapshot, because the port's metadata is already gone — is what keeps that reply.
        // The `{active, …}` spends the driver staged before (and during) the teardown are taken in
        // the same breath; a spend for this port lands on a mirror `extras.forget_port` has already
        // dropped, which is the intended no-op (a dead socket has no mode).
        self.drain_active_spends();
        let completions = self.drain_io_completions(snapshot);
        self.pending_events.extend(completions);
        // The teardown's round is a drive point for the `{active, …}` channel as well. A delivery
        // staged for **this** port has no metadata any more, so the drain drops it — the
        // completion channel's rule — while a delivery staged for another live port is held like
        // any other pending work (see [`ops::active_input`]).
        let active = self.drain_active_inputs();
        self.pending_events
            .extend(active.into_iter().map(|packet| PortEvent::ActiveInput { packet }));
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
    /// A round reports a reaped child's `exit_status` **before** the round's own end-of-stream
    /// line flush, which is the order the oracle delivers in (see the ordering note at the
    /// round's line drain): a partial trailing line released at end-of-stream is therefore never
    /// seen before the status of the child that wrote it.
    ///
    /// A round never blocks when there is nothing that could become ready — no live port and no
    /// armed `tty_select/2` interest (see [`PortRegistry::has_nothing_to_poll`]): an armed but
    /// **idle** signal self-pipe is not a wake source, so a runtime that only handles a signal
    /// and never opened a port never sleeps here. Any signal that *has* been caught was already
    /// harvested into the returned events above this guard, so it is never stranded by the
    /// early return.
    ///
    /// # Errors
    ///
    /// The poller's own failures ([`BeamError::Io`] when the poll syscall fails, and whatever it
    /// reports when the signal self-pipe cannot be registered) and any error a port's driver
    /// raises while a scheduled task runs.
    fn pump(&mut self, timeout: Duration) -> Result<Vec<PortEvent>> {
        // The signal self-pipe is considered first: the facility that owns it is created by
        // `os:set_signal/2`, so the descriptor to register only exists after that call — the
        // watch catches up here, on the first round after the first `handle` disposition.
        self.watch.sync(&mut self.poller)?;
        // Events an earlier round harvested **outside** a pump are reported first: they are older
        // than anything this round can produce, and the queue they were taken from is already
        // empty, so reporting them here neither loses nor duplicates them (see
        // [`PortRegistry::pending_events`]).
        let mut events = self.drain_pending_events();
        events.extend(self.take_due_exit_statuses()?);
        // The terms a driver's callback emitted (`erl_drv_output_term`/`erl_drv_send_term`) are
        // queued on the port and harvested before any wait.
        //
        // A `dlopen`ed `{spawn_driver, Name}` port's `driver_output*` **bytes** are deliberately
        // NOT harvested here: they are the port's buffered *reads*, and `beam_io`'s flat
        // `Port::drain_input` view joins a `driver_output2` call's header and body into one byte
        // string — reporting `<<h…tail>>` for the oracle's `[h…|<<tail>>]`. The port-task round
        // below harvests them instead (`execute_port_tasks` → `Port::take_input_notifications`):
        // one `PortNotification::Data` per `driver_output*` call with the C's header/body split
        // intact, which `fold_notifications` renders (`StreamShape::Data`). They cannot be reported
        // twice — a round **drains** the reads — and nothing is lost: every path that can run a
        // driver callback (`output`, `control`, `ready_input`, `timeout`, `ready_async`) either
        // runs inside a round or leaves its read for the next one.
        events.extend(self.take_driver_terms());
        // A signal handled before this round — or while the runtime was not polling — is
        // recorded in the facility's own bitmask, so it is drained unconditionally rather than
        // keyed to the readiness event.
        events.extend(self.take_signals());
        // Only a *live* wake source is worth blocking on. The signal self-pipe is deliberately
        // excluded even though it is registered: `take_signals` above has already harvested any
        // signal that was caught, so a *pending* one must return at once rather than poll a
        // now-drained pipe, and an *idle* one can only become readable when a signal arrives —
        // blocking on it would be waiting for nothing. A live port or an armed select
        // registration is real async I/O and is still waited on.
        // The async-job pool is drained before anything else: a completion finished by a worker
        // thread is this round's work, and a job whose port has closed is released right here (the
        // C's `!p` branch) so its `async_free` runs even with no port left to drive.
        self.drain_async_pool();
        if self.registry.is_empty() && !self.select.is_armed() {
            // A close `beam_io` has not reported yet still owes its `'EXIT'`/`'DOWN'`: minting
            // the report is what retires the port's number and releases them, and with no
            // live port left nothing else would ever run the round that mints it.
            //
            // The report round is a drive point for the `{active, …}` spend channel, so the spend
            // drain comes first — a spend harvested after the closes it precedes would apply to a
            // mirror the next round already replaced (see [`PortRegistry::drain_active_spends`]).
            self.drain_active_spends();
            self.report_pending_closes(&mut events)?;
            // That round is also a drive point for the completion channel (it is what mints a
            // close report, and a close is where a driver answers its queued operations), so it
            // is drained here — otherwise a completion awaiting the last port's teardown would
            // be stranded for ever.
            events.extend(self.drain_io_completions(None));
            // That round minted the last port's close report, which is also a drive point for the
            // `{active, …}` channel (see [`ops::active_input`]).
            events.extend(
                self.drain_active_inputs()
                    .into_iter()
                    .map(|packet| PortEvent::ActiveInput { packet }),
            );
            // No port is left to poll, but the async-job pool may still owe work (a job a worker is
            // running, or a library it keeps mapped — see [`AsyncReady::pool_is_busy`]). Waiting one
            // bounded tick keeps the caller from spinning while it drains, and the second drain turns
            // a completion into its release; a `+A N` job is then never stranded by the last port's
            // close.
            if self.async_pool_is_busy() {
                let _ = self.poller.poll(Some(ASYNC_READY_POLL_BOUND))?;
                self.drain_async_pool();
            }
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
            // With async jobs in flight the round must come back soon: their completions arrive on
            // worker threads, which the poller cannot report (see [`ASYNC_READY_POLL_BOUND`]).
            if self.async_pool_is_busy() {
                wait = wait.min(ASYNC_READY_POLL_BOUND);
            }
            let polled = self.poller.poll(Some(wait))?;
            // Whatever finished during the wait is routed to its port **before** the tasks run, so
            // this very round's drive point delivers it (`beam_io` drives `drive_async` ahead of
            // harvesting input).
            self.drain_async_pool();
            // The signal self-pipe is **not** a port: its readiness means "the dispatcher caught
            // something", so it is read here — the C's own model — and the descriptor's event is
            // filtered out of the batch before the port scheduler sees it, so nothing but
            // [`watch::SignalWatch`] can ever act on the sentinel key. (What *arrived* is the
            // facility's bitmask; the pipe bytes are discarded — `beam_io::signal`'s coalescing
            // policy. A signal caught while the runtime was not polling is caught by the
            // unconditional drain above, so this readiness-driven read is the fast path, not the
            // only one.)
            if self.watch.ready(&polled) {
                events.extend(self.take_signals());
            }
            // A select registration is the other non-port wake source: its readiness is a
            // one-shot `enif_select` report, so each ready registration yields exactly one
            // [`PortEvent::Select`] and is de-registered as it is reported (see [`select`]). The
            // payload terms and the handle were stabilised when the select was armed, so they are
            // valid across this pass.
            //
            // Ownership is snapshotted **before** the drain: `take_ready` removes the
            // registrations it reports, so asking the watch afterwards would say the key belongs
            // to no one and leak it into the port batch below.
            let select_keys: Vec<u64> = polled
                .iter()
                .map(|event| event.key())
                .filter(|key| self.select.owns(*key))
                .collect();
            for delivery in self.select.take_ready(&mut self.poller, &polled)? {
                events.push(PortEvent::Select {
                    owner: delivery.owner,
                    payload: delivery.payload,
                    event: delivery.event,
                    handle: delivery.handle,
                    obj: delivery.obj,
                    hold: delivery.hold,
                });
            }
            if !polled.is_empty() {
                // Readiness becomes *scheduled tasks* (the ERTS port-task model) — the
                // driver is called when the task runs, never from the event loop. The signal
                // sentinel and this watch's own keys are **not** ports, so they are filtered
                // out of the batch before the port scheduler sees them.
                let port_events: Vec<Event> = polled
                    .iter()
                    .copied()
                    .filter(|event| {
                        event.key() != watch::SIGNAL_KEY && !select_keys.contains(&event.key())
                    })
                    .collect();
                if !port_events.is_empty() {
                    self.registry.schedule_events(&port_events);
                }
            }
            let outcome = self.run_port_tasks(PORT_REDS_EXECUTE)?;
            // The `{active, …}` grants those tasks spent are taken **before** the channels below,
            // so a spend and the delivery it produced are never split across the mirror's view:
            // the mode the driver now holds is the one a later peer-close gate must read (see
            // [`PortRegistry::drain_active_spends`]).
            self.drain_active_spends();
            // The **completion channel** is drained on *every* round, immediately after the
            // tasks: a driver stages an `{inet_reply, …}`/`{inet_async, …}` from inside a task
            // (or from `output`/`control`), and the port keeps it until a drain takes it, so
            // deferring the drain until something else looked non-empty would starve a caller
            // blocked on that very message (see [`completion`]).
            events.extend(self.drain_io_completions(None));
            // The `{active, …}` channel is drained here too: a driver stages a delivery from the
            // same `ready_input` callback a completion comes from (see [`ops::active_input`]).
            events.extend(
                self.drain_active_inputs()
                    .into_iter()
                    .map(|packet| PortEvent::ActiveInput { packet }),
            );
            // The **line channel** is drained on *every* round, immediately after the tasks:
            // `take_line_notifications` empties every line-mode port's staged frames, so
            // deferring it until something else looked non-empty would lose them (see
            // [`PortEvent::Line`]).
            //
            // A due `exit_status` is taken **here**, between the tasks and the end-of-stream
            // flush, because that is the order the oracle delivers in: the C reports a reaped
            // child's status (`driver_report_exit`, the `{exit_status, Status}` message) and only
            // then releases the child's trailing partial line (`driver_failure_eof` →
            // `flush_linebuf_messages`, `erts/emulator/sys/unix/sys_drivers.c:1250-1252`), both
            // from the one `ready_input` call the child's exit triggers. Taking the status before
            // the flush is what keeps the two messages of one round in that order instead of
            // appending the status after the frames this very round just flushed.
            //
            // The mid-round check alone is **not** enough: the kernel can surface the child's
            // read-EOF and make it reapable in either order (the pipe's write end closes earlier
            // in `do_exit` than the zombie transition that `waitpid` reports), so a round can
            // observe the end of the stream while the status is still `None`. That is why the
            // frames a round harvests are **held** while their port still owes an `exit_status`
            // (see [`PortRegistry::hold_or_report_line`]): the status is then the first of the
            // two messages whether the two observations land in this round or in adjacent ones.
            events.extend(self.take_due_exit_statuses()?);
            // A port that stopped owing its status since the last round (either check) releases
            // its held frames here — before this round's own frames, so a port's frames stay in
            // arrival order.
            self.release_held_lines(&mut events);
            // The end-of-stream flush releases a trailing partial line as a `noeol` frame (the
            // C's `driver_eof` → `flush_linebuf_messages`).
            self.flush_line_stream_ends()?;
            for frame in self.registry.take_line_notifications() {
                // A frame for a port whose metadata is gone (closed while the task was
                // queued) has no recipient and is dropped — the rule the raw `Data` arm
                // below follows too.
                if let Some(meta) = self.meta.get(&frame.port.number()) {
                    // A frame's **content** bytes are **input** for
                    // `erlang:statistics(io)`, counted where they really arrive; a blank line
                    // contributes zero bytes, exactly as the C's per-read accounting does.
                    //
                    // The frame's derived **header prefix** is not charged here: the C counts
                    // `hlen` once per `driver_output*` call, while [`beam_io`] stamps that call's
                    // header on **every** frame it produced, so charging it per frame would
                    // multiply it by the frame count.
                    self.io_counters.note_input(frame.bytes.len());
                    // ...and for this port's own `port_info(Port, input)`.
                    meta.input
                        .set(meta.input.get().saturating_add(frame.bytes.len() as u64));
                    let event = PortEvent::Line {
                        port: frame.port,
                        header: frame.header,
                        bytes: frame.bytes,
                        eol: frame.eol,
                        stream: meta.stream,
                        owner: meta.owner,
                    };
                    self.hold_or_report_line(frame.port.number(), event, &mut events);
                }
            }
            self.fold_notifications(outcome.notifications, &mut events)?;
            events.extend(self.take_driver_terms());
            events.extend(self.take_due_exit_statuses()?);
            // A status taken by that last check releases the frames this very round harvested
            // and held: the status still comes first, in this same round.
            self.release_held_lines(&mut events);
            if !events.is_empty() || Instant::now() >= deadline {
                return Ok(events);
            }
        }
    }

    /// Folds one port-task round's notifications into `events`, crediting every payload where it
    /// is **harvested**.
    ///
    /// This is the **single** place a [`beam_io::PortNotification`] becomes a [`PortEvent`]: both
    /// rounds that can produce one use it — [`PortRegistry::pump`], which runs its own round, and
    /// [`PortRegistry::write_command`], whose round runs outside any pump and retains the result in
    /// [`PortRegistry::pending_events`] — so the two cannot drift.
    ///
    /// A payload is counted in **both** places here, at this one instant: the node-global
    /// [`IoCounters::note_input`] behind `erlang:statistics(io)`, and the port's own
    /// [`PortMeta::input`] behind `port_info(Port, input)`.
    ///
    /// A notification whose port's metadata is gone (closed while the task was queued) has no
    /// recipient and is dropped; a notification for an **empty** payload is no message at all
    /// (a driver read of nothing), unlike an empty *line* frame. A `{packet, N}` port's read is
    /// a different matter: it is fed to the port's framer ([`packet`]), which answers **one
    /// message per completed packet** — including a legitimately empty one, which the C
    /// delivers as `{Port,{data,<<>>}}`. A close is the caller's own
    /// `port_close/1` — whose metadata is already gone, so no `{Port, {data, …}}` is produced,
    /// in ERTS either — but it is precisely the point at which the port leaves the runtime, so
    /// the signals [`PortRegistry::capture_port_exit`](super::PortRegistry) snapshotted at the
    /// close become deliverable here (see [`exit`]).
    ///
    /// # Errors
    ///
    /// The framer's growth refusal (the C's `ENOMEM` port death) for a `{packet, N}` port.
    fn fold_notifications(
        &mut self,
        notifications: Vec<PortNotification>,
        events: &mut Vec<PortEvent>,
    ) -> Result<()> {
        for notification in notifications {
            match notification {
                PortNotification::Data { port, read } => {
                    // A read of **nothing** is no message at all (a driver read that returned no
                    // header and no byte), unlike an empty *line* frame. `ReadMessage::is_empty`
                    // is the C's `hlen == 0 && len == 0`, and it keeps `body: None` (a `NULL`
                    // body, which renders the proper header list) distinct from a present-but-
                    // empty one (the `[H1..Hh|<<>>]` binary tail).
                    if read.is_empty() {
                        continue;
                    }
                    // A `{packet, N}` port reassembles the child's stream into whole packets: the
                    // read's flat byte sequence is fed to the port's own framer and **one**
                    // message is produced per completed packet (see [`packet`]). This is the C's
                    // `ready_input` packet arm (`sys_drivers.c:1393-1447`), including its two edge
                    // cases: a packet split across several reads is buffered until it completes,
                    // and a **zero-length** packet is a real `{Port,{data,<<>>}}` message (which is
                    // why the empty-read drop above is checked before this arm and not inside it).
                    //
                    // The framer is a **byte stream**, so it consumes `flattened()` — for this
                    // family the read is body-only anyway, because a `{packet, N}` port is opened
                    // on a child (the spawn/spawn_executable/fd family), never on a driver.
                    let framed = {
                        let mut framed = None;
                        if let Some(meta) = self.meta.get_mut(&port.number()) {
                            if let Some(framing) = meta.packet.as_mut() {
                                // The framer's refusal (a buffer that cannot grow) is the C's
                                // `ENOMEM` port death; it is reported, never papered over.
                                let bytes = read.flattened();
                                framed = Some((framing.push(&bytes)?, meta.stream, meta.owner));
                            }
                        }
                        framed
                    };
                    if let Some((packets, stream, owner)) = framed {
                        for payload in packets {
                            // A framed payload is **input** for `erlang:statistics(io)` and for
                            // this port's own `port_info(Port, input)`, and the packet's length
                            // header is not counted — the oracle reports the delivered bytes
                            // (measured: 3 for a 7-byte `{packet,4}` wire packet, and 0 for an
                            // empty one).
                            self.io_counters.note_input(payload.len());
                            if let Some(meta) = self.meta.get(&port.number()) {
                                meta.input
                                    .set(meta.input.get().saturating_add(payload.len() as u64));
                            }
                            events.push(PortEvent::Data {
                                port,
                                // A reassembled packet is a bare payload: the framing is the
                                // emulator's ([`packet`]), never a driver's header/body pair.
                                read: ReadMessage::body_only(payload),
                                stream,
                                shape: StreamShape::Data,
                                owner,
                            });
                        }
                        continue;
                    }
                    // The read's recipient: a **live** port's metadata, else the snapshot a
                    // driver failure left ([`FailedReadRecipient`]) — the read a failing callback
                    // delivered before it closed its own port still reaches the owner, ahead of
                    // the `'EXIT'`/`'DOWN'` the same failure published (the oracle's
                    // `{data,<<"hi">>}` **then** `{'EXIT',P,9}`). An entry is consumed here: the
                    // bytes were harvested exactly once, so nothing else can still be owed them,
                    // and a port number that has no snapshot at all has no recoverable recipient —
                    // the read is dropped rather than delivered to a stale owner.
                    let recipient = match self.meta.get(&port.number()) {
                        Some(meta) => Some(FailedReadRecipient {
                            owner: meta.owner,
                            stream: meta.stream,
                            shape: meta.kind.stream_shape(),
                        }),
                        None => self.failed_read_recipients.remove(&port.number()),
                    };
                    if let Some(recipient) = recipient {
                        // The read's bytes are **input** for `erlang:statistics(io)`, counted
                        // where they really arrive — the header included, the C's
                        // `bytes_in += (hlen + len)` (`io.c:6400`). The node-global counter is
                        // credited for a failed port too: the bytes really did enter Erlang.
                        self.io_counters.note_input(read.len());
                        // ...and for this port's own `port_info(Port, input)`, which only a live
                        // port still has to report.
                        if let Some(meta) = self.meta.get(&port.number()) {
                            meta.input.set(meta.input.get().saturating_add(read.len() as u64));
                        }
                        events.push(PortEvent::Data {
                            port,
                            read,
                            stream: recipient.stream,
                            shape: recipient.shape,
                            owner: recipient.owner,
                        });
                    }
                }
                PortNotification::Closed { port } => {
                    // The port has genuinely left the runtime, so the `'EXIT'`/`'DOWN'`
                    // messages its close owed its links and monitors are released for the
                    // delivery routes to take (see [`exit`]).
                    self.pending_port_signals.report(port);
                    // ...and this is the round that retires its number (`beam_io` releases the
                    // id when it mints this report), so a surviving read's snapshot cannot
                    // outlive the number it names.
                    self.failed_read_recipients.remove(&port.number());
                }
            }
        }
        Ok(())
    }

    /// Takes the events an earlier port-task round harvested outside a pump (see
    /// [`PortRegistry::pending_events`]), dropping those whose port has since gone away.
    ///
    /// A port closed between the harvest and this drain has no recipient — the same rule
    /// [`PortRegistry::fold_notifications`] follows — so its bytes are dropped rather than
    /// delivered to a stale owner ([`PortRegistry::close`] purges them outright, which is what
    /// keeps a reused port number from inheriting them).
    ///
    /// A [`PortEvent::Data`] is the exception, and it is not a relaxation: its recipient was
    /// **already** resolved where the read was folded, from a live port's metadata or — for the
    /// read a failing driver callback wrote before it closed its own port — from the snapshot
    /// [`PortRegistry::fail_port`] took ([`FailedReadRecipient`]). That port is gone *by
    /// construction*, and the read is exactly what the owner is owed **ahead of** the `'EXIT'`
    /// the same failure published (the oracle's `{P,{data,<<"hi">>}}` then `{'EXIT',P,9}` row),
    /// so the liveness question here could only suppress it. There is no stale owner to reach: a
    /// read with no recoverable recipient is dropped by the fold itself, `close`'s and
    /// `fail_port`'s purges never leave another port's read behind, and a recycled number gets a
    /// live port whose own metadata resolves its own reads.
    ///
    /// Nothing is re-delivered: the bytes left [`beam_io`]'s input queue when they were harvested,
    /// so this is the **only** report they can still have.
    fn drain_pending_events(&mut self) -> Vec<PortEvent> {
        std::mem::take(&mut self.pending_events)
            .into_iter()
            .filter(|event| match event.port() {
                Some(port) => {
                    self.meta.contains_key(&port.number())
                        || matches!(event, PortEvent::Data { .. })
                }
                None => true,
            })
            .collect()
    }

    /// Runs the **end-of-stream line flush** for every `line`/`{line, N}` port.
    ///
    /// A `{line, N}` port holds back the bytes since the last `\n`, so a trailing partial line
    /// is never a complete frame on its own. The C releases it from `driver_eof` —
    /// `driver_failure_eof` → `flush_linebuf_messages` (`erts/emulator/beam/io.c:7210-7211`),
    /// the same observation point that also runs the derived `flush_linebuf` (`io.c:7221`) —
    /// which delivers the held bytes **once** as a `noeol` frame (the oracle's
    /// `printf 'a\nb' ⇒ {eol,"a"} {noeol,"b"}`).
    ///
    /// [`beam_io::PortRegistry::take_eof`] *is* that observation point: it forwards to the
    /// driver's `Driver::take_eof` and, when the stream really has ended, runs
    /// [`beam_io::Port::flush_linebuf`] as its side effect. The `bool` it answers is the `eof`
    /// **option**'s `{Port, eof}` message, which this runtime does not synthesize (the module
    /// documents the option as accepted but inert), so the value is deliberately discarded —
    /// it is the flush that is wanted here. A port whose stream has not ended answers `false`
    /// and stages nothing, and a repeat observation stages nothing either (the flush is
    /// idempotent), so calling this every round is safe.
    ///
    /// Only line-mode ports are visited: a raw port has no line buffer to release.
    /// [`PortOptions::stream_options`] asks the child port's driver to report end-of-stream for
    /// exactly those ports, so the observation is available where it is needed.
    ///
    /// # Errors
    ///
    /// Whatever the driver's `take_eof` returns. The port numbers are snapshotted first, and
    /// nothing between the snapshot and the call can close a port, so a live entry always
    /// resolves.
    fn flush_line_stream_ends(&mut self) -> Result<()> {
        let numbers: Vec<u64> = self
            .meta
            .iter()
            .filter(|(_, meta)| meta.line.is_some())
            .map(|(number, _)| *number)
            .collect();
        for number in numbers {
            // The boolean is the (inert) `eof` option's report; the flush it runs is the point.
            let _eof_option_report = self.registry.take_eof(PortId::new(number))?;
        }
        Ok(())
    }

    /// Commits every live driver port's pending interned names into `atoms` — the runtime's
    /// **one live** table — and returns the `(port, name)` pairs whose prediction had drifted.
    ///
    /// A [`driver::DriverPort`] host cannot mint an atom inline (the runtime's table is borrowed
    /// while a driver callback runs and this crate is `#![deny(unsafe_code)]`), so a *new* name a
    /// driver interned inside a callback is handed a **predicted** index and recorded on its
    /// [`driver::DriverAtoms`] handle. Landing it here — at a step boundary where no callback is
    /// running and no such table is borrowed — is what makes the name resolvable at all; without
    /// this call a driver-interned name would keep a predicted index that names nothing.
    ///
    /// The returned list is never a silent drop: a name whose predicted index the table has moved
    /// past sits behind a term the driver has already built, so the caller reports it (see
    /// [`Runtime::deliver_port_messages`]).
    ///
    /// # Errors
    ///
    /// None: the driver callback that interned the name has already returned, so there is nobody
    /// left to fail — a name that cannot land at its predicted index is reported, not raised.
    fn commit_driver_atoms(
        &mut self,
        atoms: &mut beam_term::AtomTable,
    ) -> Vec<(PortId, String)> {
        let mut dropped = Vec::new();
        for (number, meta) in self.meta.iter_mut() {
            let Some(pending) = meta.driver_atoms.as_ref() else {
                continue;
            };
            for name in pending.commit(atoms) {
                dropped.push((PortId::new(*number), name));
            }
        }
        dropped
    }

    /// Whether any live port's driver has a name awaiting a commit into the shared
    /// [`AtomTable`] — the cheap predicate a round consults **before** it borrows that table.
    ///
    /// [`DriverAtoms::is_empty`] is the guard `commit_driver_atoms`' own contract asks for, and
    /// it has to be applied at the **site that owns the table borrow**, not inside the commit
    /// loop: the table is reached through
    /// [`CodeServer::atoms_mut`](crate::codeserver::CodeServer::atoms_mut), which is
    /// `Rc::make_mut`. Merely *reaching* it while any other holder is alive — a
    /// `{spawn_driver, Name}` port's host (which holds one for the port's whole life), a loaded
    /// NIF library's stored host, a port open in flight — deep-clones the entire shared table
    /// (thousands of names, and their `String`s, plus the index map) for a round that interns
    /// nothing. A round with nothing pending must therefore not reach it at all.
    ///
    /// The scan is one pass over the live ports — orders of magnitude below the clone it
    /// avoids, and the same pass `commit_driver_atoms` would make anyway.
    pub fn has_pending_driver_atoms(&self) -> bool {
        self.meta.values().any(|meta| {
            meta.driver_atoms
                .as_ref()
                .is_some_and(|pending| !pending.is_empty())
        })
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
    /// recipient, so its terms are dropped exactly as [`PortRegistry::fold_notifications`] drops a
    /// read whose port has closed. A driver's queue is emptied when it is unloaded, so this never
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
    /// second round cannot report the same exit twice. Taking one is what settles
    /// [`PortMeta::owes_exit_status`] — the fact the round's frame deferral reads — so the latch
    /// is set here, at the **only** place a status is taken.
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
                    meta.exit_status_taken.set(true);
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

    /// Records one harvested **line frame**: delivered now, or held back while its port still
    /// owes an `exit_status` (the C's `dd->alive == 1` deferral, [`PortMeta::held_lines`]).
    ///
    /// The C's `port_inp_failure` does not release a child's trailing partial line while the
    /// child is still alive and its status is still owed: it returns **before**
    /// `driver_failure_eof` and flushes only on the later `ready_input` the child's exit
    /// triggers, so the status is the first of the two messages
    /// (`erts/emulator/sys/unix/sys_drivers.c:1230-1252`). Holding the frame is this runtime's
    /// equivalent, and it is what makes the order independent of whether the poller surfaced
    /// the child's read-EOF and its reap in the same round or in adjacent ones.
    ///
    /// Holding is bounded by [`EXIT_STATUS_FLUSH_GRACE`], so a child that closes its own stdout
    /// and never exits cannot strand its trailing frame.
    fn hold_or_report_line(&mut self, number: u64, frame: PortEvent, events: &mut Vec<PortEvent>) {
        let Some(meta) = self.meta.get_mut(&number) else {
            // The port's metadata is gone (it was closed while the frame was queued): the frame
            // has no recipient and is dropped, the rule every other arm follows.
            return;
        };
        if !meta.owes_exit_status() {
            events.push(frame);
            return;
        }
        if meta.held_since.get().is_none() {
            meta.held_since.set(Some(Instant::now()));
        }
        meta.held_lines.push_back(frame);
    }

    /// Moves every held line frame that may now be delivered into `events`, in arrival order.
    ///
    /// A frame becomes deliverable when its port **no longer owes** its `exit_status` (the status
    /// has been reported, so the round's frame drain may proceed — the C's later `ready_input`
    /// flush) or when it has been held for the whole [`EXIT_STATUS_FLUSH_GRACE`] (a child that
    /// closed its stdout and never exited, which the oracle's own driver cannot produce because
    /// it terminates the port at end-of-stream). Both halves keep the port's own frame order:
    /// `held_lines` is a FIFO and is drained whole.
    ///
    /// The ports are visited in **port-number order** so that a round holding frames for several
    /// children releases them deterministically.
    fn release_held_lines(&mut self, events: &mut Vec<PortEvent>) {
        let mut numbers: Vec<u64> = self
            .meta
            .iter()
            .filter(|(_, meta)| !meta.held_lines.is_empty())
            .map(|(number, _)| *number)
            .collect();
        if numbers.is_empty() {
            return;
        }
        numbers.sort_unstable();
        for number in numbers {
            let Some(meta) = self.meta.get_mut(&number) else {
                continue;
            };
            let expired = meta
                .held_since
                .get()
                .is_some_and(|since| since.elapsed() >= EXIT_STATUS_FLUSH_GRACE);
            if meta.owes_exit_status() && !expired {
                continue;
            }
            events.extend(meta.held_lines.drain(..));
            meta.held_since.set(None);
        }
    }
    /// The `{notify, Signal}` notifications of the **handled** signals caught since the last
    /// drain — the delivery half of `os:set_signal/2` (contract A.8).
    ///
    /// ERTS's emulator turns each handled signal into a message to the process registered as
    /// `erl_signal_server`; this is where such a signal enters the runtime (the recipient is
    /// resolved by the port layer's message pumps, because the notification is addressed by
    /// *name*, not to a pid — see [`PortEvent::Signal`]).
    ///
    /// Each *distinct* signal caught since the previous drain is reported exactly once
    /// (`beam_io::signal`'s coalescing policy), in [`beam_io::signal::Signal::ALL`] order, and an
    /// `ignore`/`default` disposition never produces one at all (the kernel does not run the
    /// facility's trampoline for it). A signal caught while the runtime was not polling is still
    /// reported, because the record is the facility's bitmask rather than the self-pipe's
    /// readiness.
    ///
    /// Nothing is drained when no `os:set_signal/2` ever ran (there is no facility, so no
    /// signal can have been handled by it) — never an error, and never an invented notification.
    fn take_signals(&mut self) -> Vec<PortEvent> {
        self.watch
            .drain()
            .into_iter()
            .map(|name| PortEvent::Signal { name })
            .collect()
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
    /// `erlang:port_info/1` keeps the seven `/1` items and drops every `/2`-only pair
    /// (`monitors`, `queue_size`, `memory`, `monitored_by`, `locking`, `parallelism` and
    /// `registered_name`), while `erlang:port_info/2` searches them all. The order and the
    /// value shapes are the ones erts-17.1 reports (see the module docs; the derived items are
    /// [`info_extras`]'s).
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
        // The owning process is the port's `connected` value; the **link set** is the owner
        // alone until `port_connect/2` adds one (the C's `set_port_connected` creates a link
        // without removing the old one, so the oracle's `links` grows — see
        // [`ops::PortExtras::links`]).
        let owner = meta.owner.to_eterm();
        // The link set is copied out first: `extra_links` borrows the whole registry, and the
        // arena below is a second (disjoint) field.
        let linked = self.extra_links(port.number()).map(<[_]>::to_vec);
        let links = match linked {
            Some(linked) => pid_list(&mut self.heap, &linked)?,
            None => pid_list(&mut self.heap, &[meta.owner])?,
        };
        // A registered port reports `{registered_name, Name}` **first** (the oracle's own
        // order); `beam_bifs`'s `port_info/1` keeps only its seven items, so the pair is
        // reachable through `port_info/2`.
        let registered = match self.port_name(port) {
            Some(name) => Some((item("registered_name")?, make_atom(name))),
            None => None,
        };
        let monitors = monitor_list(&mut self.heap, atoms, &meta.monitors)?;
        // `id` is a **bare small integer** (the oracle's shape), not the port term.
        let id = try_make_small(i64::try_from(port.number()).map_err(|_| BeamError::SystemLimit)?)
            .map_err(|_| BeamError::SystemLimit)?;
        // `input`/`output` are this port's cumulative byte totals: the bytes it delivered into
        // Erlang and the bytes the runtime wrote to it (bumped at the same sites as the
        // node-global `IoCounters` the `statistics(io)` arm reads). Both are `0` for a port that
        // has moved no bytes yet — the oracle's answer right after `open_port`.
        let input = byte_count(meta.input.get())?;
        let output = byte_count(meta.output.get())?;
        // The **derived** `/2`-only items are answered from state the pairs above do not touch
        // (see [`info_extras`]). Each is computed to an owned value *before* the arena is
        // touched, so the immutable borrow of `self` they need cannot overlap the mutable
        // borrow of `self.heap`.
        let queue_size = byte_count(self.info_queue_size(port) as u64)?;
        let memory = byte_count(self.info_memory(port) as u64)?;
        let watching = self.info_monitored_by(port);
        let monitored_by = pid_list(&mut self.heap, &watching)?;
        let locking = item(self.info_locking(port))?;
        // The C reports the `ERTS_PTS_FLG_PARALLELISM` bit, which `open_port/2`'s `parallelism`
        // tuple option would set on a driver that supports it — this runtime never grants it, so
        // the honest answer is `false` (the oracle's own for a `spawn` and a driver port alike).
        let parallelism = item("false")?;
        let os_pid = match meta.os_pid {
            Some(pid) => try_make_small(i64::from(pid)).map_err(|_| BeamError::SystemLimit)?,
            // A **driver** port owns no child the emulator forked: its pid is whatever the driver
            // recorded with `erl_drv_set_os_pid` (`io.c:2971`), and the C's `prt->os_pid` starts at
            // `-1` (`io.c:386`), which the BIF renders as `undefined`
            // (`erl_bif_info.c:3814-3817`) — the oracle's own answer for a port whose driver never
            // called the hook.
            None => match meta.driver_os_pid.as_ref().and_then(driver::OsPidFlag::get) {
                Some(pid) => try_make_small(i64::from(pid)).map_err(|_| BeamError::SystemLimit)?,
                // `port_info(Port, os_pid)` is `undefined` for a port without an OS process —
                // the oracle's own answer.
                None => item("undefined")?,
            },
        };
        Ok(vec![
            (item("name")?, name),
            (item("links")?, links),
            (item("id")?, id),
            (item("connected")?, owner),
            (item("input")?, input),
            (item("output")?, output),
            (item("os_pid")?, os_pid),
            // The `/2`-only extras; `beam_bifs`'s `port_info/1` drops them all (and the native
            // floor's registered-port list keeps the C's own `/1` roster).
            (item("monitors")?, monitors),
            (item("queue_size")?, queue_size),
            (item("memory")?, memory),
            (item("monitored_by")?, monitored_by),
            (item("locking")?, locking),
            (item("parallelism")?, parallelism),
        ]
        .into_iter()
        .chain(registered)
        .collect())
    }
}

/// The port subsystem's **term-carrying slots**, enumerated mutably — the relocation half of the
/// runtime's host-side stores (see `CONTEXT.md`'s "Host-side term stores").
///
/// A runtime-level `for_each_root_mut` door needs every word the port layer **stores across a
/// pass**, not just the words it derives from them at delivery time. The inventory is split into
/// three families, one method each (so no shared "family" type is needed):
///
/// | Method | The stored terms it visits |
/// |---|---|
/// | [`PortRegistry::for_each_pending_event_root_mut`] | [`PortRegistry::pending_events`]' [`PortEvent::Term`] term and [`PortEvent::Select`] handle + payload; every live port's held line frames ([`PortMeta::held_lines`]); and every live port's [`driver::TermQueue`] (the `erl_drv_output_term`/`erl_drv_send_term` entries [`PortRegistry::take_driver_terms`] turns into [`PortEvent::Term`]s) |
/// | [`PortRegistry::for_each_port_data_root_mut`] | the `erlang:port_set_data/2` payloads of [`ops::PortExtras::data`] |
/// | [`PortRegistry::for_each_select_root_mut`] | every armed [`select::SelectRegistration`]'s handle + payload (both `enif_select` and `prim_tty:tty_select/2`) |
///
/// Each visitor is a **rewriter**: it may write through the `&mut [Eterm]` it is handed, and the
/// slice's length never changes (one word per slot, always). An `Err` stops the walk and is
/// returned as-is.
///
/// None of this is a liveness statement: visiting a word retains nothing, and none of these methods
/// starts or stops delivering, queueing or collecting anything. The one term-keyed index here —
/// [`select::SelectWatch`]'s `by_fd_handle`, keyed by the address-derived `handle_key` — is
/// **recomputed** rather than rewritten by [`PortRegistry::rekey_select_handles`], which a store
/// move calls after its forwarding rewrote the handles (see
/// [`select::SelectWatch::for_each_root_mut`]).
impl PortRegistry {
    /// Visits every term the **queued/pending port event** family stores, mutably.
    ///
    /// Three stores, all behind this one door: the harvested-but-unreported
    /// [`PortRegistry::pending_events`] buffer, each live port's held line frames, and each live
    /// port's [`driver::TermQueue`]. The visit order is the registry's own and carries no meaning.
    ///
    /// [`driver::TermQueue`] is shared by `Rc<RefCell<..>>` with the port's driver host, so its own
    /// enumerator takes the cell's borrow per element and never holds it across `f` (a driver
    /// callback pushing during the walk must still land its term).
    ///
    /// # Errors
    ///
    /// Whatever `f` answers; the walk stops there.
    pub(crate) fn for_each_pending_event_root_mut(
        &mut self,
        f: &mut impl FnMut(&mut [Eterm]) -> Result<()>,
    ) -> Result<()> {
        for event in &mut self.pending_events {
            event.for_each_root_mut(f)?;
        }
        for meta in self.meta.values_mut() {
            // Only `PortEvent::Line` frames are held here today, but the walk is variant-generic so
            // no term-carrying event a later producer parks here can hide from it.
            for event in &mut meta.held_lines {
                event.for_each_root_mut(f)?;
            }
            if let Some(queue) = meta.driver_terms.as_ref() {
                queue.for_each_root_mut(f)?;
            }
        }
        Ok(())
    }

    /// Visits every stored `erlang:port_set_data/2` payload, mutably — see
    /// [`ops::PortExtras::for_each_data_root_mut`], which is the side table's whole term inventory.
    ///
    /// # Errors
    ///
    /// Whatever `f` answers; the walk stops there.
    pub(crate) fn for_each_port_data_root_mut(
        &mut self,
        f: &mut impl FnMut(&mut [Eterm]) -> Result<()>,
    ) -> Result<()> {
        self.extras.for_each_data_root_mut(f)
    }

    /// Visits every term an **armed select registration** stores, mutably — see
    /// [`select::SelectWatch::for_each_root_mut`].
    ///
    /// Readiness a registration has already reported is not here: it was moved into
    /// [`PortRegistry::pending_events`] as a [`PortEvent::Select`], which the pending-event family
    /// covers. A payload that is in both places is visited twice, which is exactly what it needs
    /// (the two stores are independent copies of the word).
    ///
    /// # Errors
    ///
    /// Whatever `f` answers; the walk stops there.
    pub(crate) fn for_each_select_root_mut(
        &mut self,
        f: &mut impl FnMut(&mut [Eterm]) -> Result<()>,
    ) -> Result<()> {
        self.select.for_each_root_mut(f)
    }

    /// Rebuilds every address-valued index over a term this layer stores — today exactly one: the
    /// armed select registrations' `(fd, handle)` lookup
    /// ([`select::SelectWatch::rekey_handles`]).
    ///
    /// Called by the store move ([`crate::runtime::reclaim::compact`]) at its commit, once the
    /// forwarding has rewritten the handles the index is keyed by. Nothing else needs it: a port's
    /// own descriptor map is keyed by an `fd`, never by a term.
    pub(crate) fn rekey_select_handles(&mut self) {
        self.select.rekey_handles()
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
            // A `{spawn_driver, Name}` port is backed by a driver (a loaded `.so` or a built-in),
            // not by the spawn driver: `open_port` registers a [`driver::DriverPortAdapter`] or a
            // built-in [`Driver`] for it and never asks for a spawn spec, so reaching this is a
            // programming error.
            PortName::SpawnDriver | PortName::SpawnDriverBuiltin => Err(BeamError::BadArg),
        }
    }
}

/// The validated option list of an `open_port/2` call.
///
/// `stream` and `use_stdio` are `Option`s so that overlaying the inline options of a
/// `{spawn_executable, Path, Opts}` name with the call's own `Options` can tell "not asked
/// for" from an explicit choice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PortOptions {
    /// The `binary`/`list` stream choice.
    stream: Option<StreamType>,
    /// The resolved **line framing** option — the C's `linebuf` (`erl_bif_port.c:806,862-869,
    /// 985-986`): `Some(max_len)` is line mode on (`line` ⇒ [`LINE_DEFAULT_MAX_LEN`],
    /// `{line, N}` with `N > 0` ⇒ `N`), `None` (the default) is off.
    ///
    /// This is deliberately a **separate field** rather than a [`StreamType`] variant: it is
    /// an *independent* port state bit (`ERTS_PORT_SFLG_LINEBUF_IO`) from the
    /// `binary`/`list` one, both may be set at once (`[binary, line]`), and keeping it out of
    /// [`StreamType`] leaves the read path's message shape ([`RuntimeContext::port_data_message`])
    /// untouched.
    line: Option<usize>,
    /// The `{packet, N}` framing width in bytes — the C's `opts.packet_bytes`
    /// (`erl_bif_port.c:813,849-861`): `0` (the default) is no framing, `1`/`2`/`4` are the
    /// accepted header widths. The `stream` atom resets it to `0` (the C clears
    /// `packet_bytes`), the `line` option is **mutually exclusive** with a non-zero width, and
    /// the framing itself is [`packet::PacketFraming`]'s — carried onto [`PortMeta::packet`]
    /// for the spawn/fd family, whose framing the emulator owns (see
    /// [`PortName::frames_packets`]).
    packet_bytes: usize,
    /// The `use_stdio`/`nouse_stdio` choice — `None` is the oracle's default (`true`;
    /// `erl_bif_port.c:814`). Only the mutual-exclusion check reads it
    /// (`stderr_to_stdout` without `use_stdio` ⇒ `badarg`).
    use_stdio: Option<bool>,
    /// The `{cd, Dir}` working directory.
    cd: Option<String>,
    /// The `{env, [{Name, Value}]}` environment additions.
    env: Vec<(String, String)>,
    /// The `{args, [String]}` arguments of a `spawn_executable` port.
    args: Vec<String>,
    /// The `exit_status` flag: the driver queues a `{Port, {exit_status, Status}}` message
    /// once the child has been reaped.
    exit_status: bool,
    /// The `stderr_to_stdout` flag: the child's descriptor 2 becomes an exact duplicate of
    /// its descriptor 1, so its stderr bytes arrive as port data (interleaved with stdout in
    /// true write order) instead of being inherited by the emulator.
    stderr_to_stdout: bool,
}

/// The line-length limit a bare `line` option selects, in bytes — the oracle's default
/// (`linebuf = 512`, `erl_bif_port.c:986`), cross-checked live against erts-17.1.
const LINE_DEFAULT_MAX_LEN: usize = 512;

impl PortOptions {
    /// The effective stream type (`list` unless `binary` was asked for — the oracle's
    /// default).
    fn stream(&self) -> StreamType {
        self.stream.unwrap_or(StreamType::List)
    }

    /// The effective `use_stdio` choice (the oracle's default is `true`).
    fn use_stdio(&self) -> bool {
        self.use_stdio.unwrap_or(true)
    }

    /// The port's **state** stream flags, as `driver_get_port_flags` reports them from the
    /// resolved `binary`/`line` options (see [`StreamType::port_flags`]).
    fn port_flags(&self) -> i32 {
        self.stream().port_flags(self.line)
    }

    /// The child-stream options the `spawn` driver applies at `fork`/`exec` time
    /// ([`beam_io::StreamOptions`]).
    ///
    /// `eof` is asked for on exactly the ports that need the driver's **end-of-stream
    /// observation**: a `line`/`{line, N}` port, whose trailing partial line is released by
    /// the flush `take_eof` runs (see [`PortRegistry::flush_line_stream_ends`]). The option's
    /// own `{Port, eof}` message stays unsynthesized — the report is discarded — so asking the
    /// driver for it changes nothing an Erlang program can observe beyond the flush it enables.
    fn stream_options(&self) -> StreamOptions {
        StreamOptions {
            stderr_to_stdout: self.stderr_to_stdout,
            eof: self.line.is_some(),
        }
    }

    /// Applies `other` on top of `self`: whatever `other` explicitly set wins, and the
    /// flags accumulate. This is how the call's `Options` overlay the inline options of a
    /// `{spawn_executable, Path, Opts}` name.
    fn overlay(&mut self, other: PortOptions) {
        if other.stream.is_some() {
            self.stream = other.stream;
        }
        // The line option is a switch: asking for it in either list turns it on, and the
        // later (call) list's explicit length wins.
        if other.line.is_some() {
            self.line = other.line;
        }
        if other.use_stdio.is_some() {
            self.use_stdio = other.use_stdio;
        }
        // `{packet, N}` from either list is in effect unless the same list cancelled it with
        // `stream` — which `port_options` has already folded into `other.packet_bytes`.
        if other.packet_bytes != 0 {
            self.packet_bytes = other.packet_bytes;
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
        self.stderr_to_stdout |= other.stderr_to_stdout;
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

    /// Hands `bytes` to a port the way `erlang:port_command/2,3` does — the port-hook's own entry
    /// to the crate's one decision table.
    ///
    /// This is the seam `crate::native::port`'s `port_command` floor calls: the **decision**
    /// (`[force]` on a driver without `ERL_DRV_FLAG_SOFT_BUSY` ⇒ `notsup`, a busy port ⇒
    /// [`PortCommandOutcome::Busy`] with nothing queued, otherwise the command is queued) is
    /// [`beam_io`]'s, and the accepted command's round, notification folding and byte accounting
    /// are [`PortRegistry::command`]'s — see there for both halves. Nothing about the table is
    /// re-derived at this layer.
    ///
    /// # Errors
    ///
    /// As for [`PortRegistry::command`]; a nested borrow of the registry is the typed capability
    /// gap of [`RuntimePorts::registry_mut`], never a panic.
    pub fn command(
        &mut self,
        port: PortId,
        bytes: &[u8],
        options: PortCommandOptions,
    ) -> Result<PortCommandOutcome> {
        self.registry_mut()?.command(port, bytes, options)
    }
    /// Hands a command to a port the way `erlang:port_command/2,3` does, converting the data
    /// **only once the port has accepted the command** — [`PortRegistry::command_with_data`], the
    /// C's own order (the busy state and the driver's flag word are decided before `Data` is
    /// converted, `io.c:1749-1754` vs `:1790`).
    ///
    /// This is the seam `crate::native::port`'s `port_command` floor uses: it holds the **term**,
    /// not bytes, so a `[nosuspend]` command on a busy port must be able to answer `false` without
    /// ever converting it.
    ///
    /// # Errors
    ///
    /// As for [`PortRegistry::command_with_data`], plus whatever `make_data` returns; a nested
    /// borrow of the registry is the typed capability gap of [`RuntimePorts::registry_mut`], never a
    /// panic.
    pub fn command_with_data<F>(
        &mut self,
        port: PortId,
        make_data: F,
        options: PortCommandOptions,
    ) -> Result<PortCommandOutcome>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        self.registry_mut()?
            .command_with_data(port, make_data, options)
    }

    /// `erts_internal:port_control(Port, Op, Data)` — deliver a port-control operation to the
    /// port's driver and return its reply bytes.
    ///
    /// This is the runtime side of the `control` callback: it forwards to
    /// [`PortRegistry::port_control`] (see there for the verb convention, the reserved `"close"`
    /// verb and the full error set), which reaches the driver through `beam_io`'s port registry.
    /// The reply is the **driver's own bytes**: this method never invents one, so an unsupported
    /// driver state is a typed error rather than an empty answer.
    ///
    /// No `PortHook` method takes it yet — the reachable port-control entry is
    /// [`PortRegistry::port_control`], called directly by the native floor's
    /// `erlang:port_control/3` (`src/native/port.rs`) — so this method is retained for a
    /// `runtime.rs`-internal caller (see `src/port/CONTEXT.md`'s API Surface note).
    ///
    /// # Errors
    ///
    /// As for [`PortRegistry::port_control`]; a nested borrow of the registry is the typed
    /// capability gap of [`RuntimePorts::registry_mut`], never a panic.
    pub fn port_control(&mut self, port: PortId, op: &str, arg: &[u8]) -> Result<Vec<u8>> {
        self.registry_mut()?.port_control(port, op, arg)
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
    /// `binary` (the stream shape), `line`/`{line, N}` (line framing), `use_stdio`/
    /// `nouse_stdio`, `stream` (packet cancellation), `exit_status`, `{cd, Dir}`,
    /// `{env, [{Name, Value}]}` and `{args, [String]}` are honored; every other whitelisted
    /// option is recognized and accepted **without effect** here (`eof`, `in`, `out`, `hide`,
    /// `{parallelism, Bool}`, …).
    ///
    /// The oracle's two **mutual-exclusion** rules are enforced after the whole list is
    /// parsed, so they are order-independent (`erl_bif_port.c:1019-1022`): a line-buffered
    /// port cannot also be packet-framed (`line` + `{packet, N}` ⇒ `badarg`, unless a later
    /// `stream` cancelled the packet), and a redirected stderr requires `use_stdio`
    /// (`stderr_to_stdout` without `use_stdio` ⇒ `badarg`).
    ///
    /// A whitelisted tuple with the wrong arity, or a recognized option with a malformed
    /// value (`{cd, 5}`, `{args, foo}`, `{line, 0}`), is [`BeamError::BadArg`] — the drivers
    /// cannot accept those.
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
                    // The child's stderr is merged into its stdout, so its bytes arrive as
                    // port data (the `spawn` driver's `dup2(1, 2)`).
                    "stderr_to_stdout" => parsed.stderr_to_stdout = true,
                    // `line` is line framing at the oracle's default length (`linebuf = 512`).
                    "line" => parsed.line = Some(LINE_DEFAULT_MAX_LEN),
                    // The two stdio switches; the oracle's default is `use_stdio` (`true`).
                    "use_stdio" => parsed.use_stdio = Some(true),
                    "nouse_stdio" => parsed.use_stdio = Some(false),
                    // `stream` cancels any packet framing (`opts.packet_bytes = 0`,
                    // `erl_bif_port.c:979-980`), which is what makes `[line, {packet,2},
                    // stream]` legal where `[line, {packet,2}]` is not.
                    "stream" => parsed.packet_bytes = 0,
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
                // `{line, N}` is line framing at `N` bytes; the oracle requires a **positive**
                // integer (`if (linebuf <= 0) goto bad_settings`, `erl_bif_port.c:866-869`),
                // so `{line,0}`/`{line,-1}`/`{line,foo}` are `badarg`.
                "line" => {
                    let value = terms::tuple_element(element, 1)?;
                    if !is_small(value) {
                        return Err(BeamError::BadArg);
                    }
                    let length = signed_value(value);
                    if length <= 0 {
                        return Err(BeamError::BadArg);
                    }
                    parsed.line =
                        Some(usize::try_from(length).map_err(|_| BeamError::BadArg)?);
                }
                // `{packet, N}` is the framing width in bytes: the oracle accepts `1`, `2` and
                // `4` only (`switch (opts.packet_bytes)` → `default: goto bad_settings`,
                // `erl_bif_port.c:854-860`), so `{packet,0}`/`{packet,3}`/`{packet,foo}` are
                // `badarg` — a non-small first, exactly like `{line, N}`.
                "packet" => {
                    let value = terms::tuple_element(element, 1)?;
                    if !is_small(value) {
                        return Err(BeamError::BadArg);
                    }
                    let width = signed_value(value);
                    let Ok(width) = usize::try_from(width) else {
                        return Err(BeamError::BadArg);
                    };
                    if !packet::PACKET_WIDTHS.contains(&width) {
                        return Err(BeamError::BadArg);
                    }
                    parsed.packet_bytes = width;
                }
                // Whitelisted, accepted, without effect here.
                _ => {}
            }
        }
        // The oracle's **mutual-exclusion** rules, checked **after** the whole list is parsed
        // (so they are order-independent): a line-buffered port cannot also be packet-framed,
        // and a redirected stderr requires `use_stdio` (`erl_bif_port.c:1019-1022`).
        if (parsed.line.is_some() && parsed.packet_bytes != 0)
            || (parsed.stderr_to_stdout && !parsed.use_stdio())
        {
            return Err(BeamError::BadArg);
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

impl StreamType {
    /// The C's **port state** stream flags, as `driver_get_port_flags` reports them
    /// (`io.c:5325-5344`): the two state bits are **OR-ed** independently —
    /// `ERTS_PORT_SFLG_BINARY_IO` ⇒ `PORT_FLAG_BINARY` (1) and `ERTS_PORT_SFLG_LINEBUF_IO` ⇒
    /// `PORT_FLAG_LINE` (2), either, both or neither.
    ///
    /// Those state bits are set by the `open_port/2` stream options alone (`binary` ⇒ binary I/O
    /// at `erl_bif_port.c:989-990`, `line`/`{line,N}` ⇒ line-buffered at `:1139-1141`), so they
    /// are derived here from the resolved `binary`/`list` stream **and** the resolved line option.
    /// `[line]` is therefore `2`, `[binary]` is `1`, `[binary, line]` is `3` and no stream option
    /// is `0` — every one cross-checked live against erts-17.1 through a C driver's
    /// `get_port_flags`.
    pub(crate) fn port_flags(self, line: Option<usize>) -> i32 {
        let mut flags = match self {
            StreamType::Binary => PORT_FLAG_BINARY,
            StreamType::List => 0,
        };
        if line.is_some() {
            flags |= PORT_FLAG_LINE;
        }
        flags
    }
}

/// The C's `PORT_FLAG_BINARY`: the port state bit `driver_get_port_flags` maps
/// `ERTS_PORT_SFLG_BINARY_IO` onto (`erl_driver.h`'s `PORT_FLAG_BINARY`, `1 << 0`).
const PORT_FLAG_BINARY: i32 = 1;

/// The C's `PORT_FLAG_LINE`: the port state bit `driver_get_port_flags` maps
/// `ERTS_PORT_SFLG_LINEBUF_IO` onto (`erl_driver.h`'s `PORT_FLAG_LINE`, `1 << 1`).
const PORT_FLAG_LINE: i32 = 1 << 1;

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
        // `[exit_status]` is `badarg` for any port whose name resolves to a registered driver —
        // the C's `(driver != &spawn_driver && opts->exit_status)` (`io.c:623-626`), applied to
        // **both** spawn name forms because the `{spawn, Cmd}` lookup runs first. The refusal is the
        // whole truth, not a Milestone-1 shortcut: the only `{exit_status, N}` reporter is the
        // emulator-internal `driver_report_exit` (`io.c:5554`), which `erl_driver.h` does not
        // declare, so no Erlang-reachable driver can ever report one. Checked here, before any
        // registry call, so a refused port consumes no id.
        if resolved.exit_status && opens_a_driver_port(&spec) {
            return Err(BeamError::BadArg);
        }

        // `beam_io`'s driver constructor really starts the child (or adopts `{fd, N}`)
        // before anything is registered anywhere, so a failure here cannot leak an id; the
        // OS pid the driver reports is what `port_info(Port, os_pid)` answers.
        let stream = resolved.stream();
        // The resolved line framing, read before the honored fields below are moved out of
        // `resolved`; it is carried into every `PortMeta` so `PortRegistry::register` arms it.
        let line = resolved.line;
        // The resolved `{packet, N}` framing — the same read-before-the-move rule. Only the
        // spawn/fd family is framed by the emulator; a driver port's `packet` option is its
        // driver's (see [`PortName::frames_packets`] and [`packet`]).
        let packet = if spec.kind.frames_packets() {
            packet::PacketFraming::new(resolved.packet_bytes)?
        } else {
            None
        };
        if spec.kind == PortName::SpawnDriver {
            // The driver name is the prefix up to the first blank — the C's own rule (`io.c` walks
            // the name to the first blank before matching `driver_name`), the same rule
            // [`driver_lookup`] applies to build the driver-object candidates.
            let (candidates, driver_name) = driver_lookup(&spec.command);
            // The registry borrow is taken for the **whole** arm: neither branch re-enters a port
            // operation (`register` starting a driver cannot re-enter the port table, and the
            // `dlopen` path builds its host from `self.atoms`/`self.owner`, which are not the
            // registry), so one borrow serves both.
            let mut registry = self.registry_mut()?;
            // A **statically linked emulator** driver — such as `tcp_inet`
            // (`erts/emulator/drivers/common/inet_drv.c:1348`) — is resolved against
            // `beam_io`'s static set **before** any `dlopen`, exactly as the C resolves a name against
            // its loaded-driver list (`io.c:599-628`), so `open_port({spawn_driver,"tcp_inet"},_)`
            // opens a real port rather than failing to load a `.so` that does not exist. The gate is
            // `beam_io::is_static_spawn_driver`, so an emulator-internal name (`fd`, `efile`) or an
            // invented one is *not* a hit: it takes the `dlopen` route below and ends as the oracle's
            // `badarg`. Its bytes arrive through the poller-registered descriptor on the port-task
            // round (not a C driver's inline `output` callback), so it is recorded as
            // [`PortName::SpawnDriverBuiltin`] — a driver port for `port_control`, but not one
            // whose bytes come from an inline `output` callback.
            if let Some(created) = registry.create_static_spawn_driver(&driver_name) {
                let driver = created?;
                // The driver's own `control` capability, read **before** the boxed driver is moved
                // into `register` — `ram_file_drv`'s C entry leaves the slot `NULL`, the two socket
                // drivers declare `inet_control`.
                let driver_has_control = driver.has_control();
                let meta = PortMeta {
                    command: spec.command,
                    // A built-in driver port owns no OS child; `port_info(Port, os_pid)` is
                    // `undefined`.
                    os_pid: None,
                    stream,
                    line,
                    // A driver port's `{packet, N}` belongs to the driver, not to the emulator.
                    packet: None,
                    // Nothing to reap, so no `{Port, {exit_status, …}}` is ever synthesized.
                    notify_exit: false,
                    monitors: Vec::new(),
                    owner: self.owner,
                    kind: PortName::SpawnDriverBuiltin,
                    driver_has_control,
                    // A built-in driver has no C host, so it has no term queue, interns no name,
                    // sets no control flags and holds no process monitors.
                    driver_terms: None,
                    driver_atoms: None,
                    driver_control_flags: None,
                    driver_busy: None,
                    // A built-in driver is this crate's Rust driver, not a loaded `ErlDrvEntry`:
                    // it has no `erl_drv_set_os_pid` hook to record an OS child through.
                    driver_os_pid: None,
                    driver_monitors: None,
                    input: Cell::new(0),
                    output: Cell::new(0),
                    // Nothing to reap, so nothing is ever owed — see
                    // [`PortMeta::owes_exit_status`].
                    exit_status_taken: Cell::new(true),
                    held_lines: VecDeque::new(),
                    held_since: Cell::new(None),
                };
                return registry.register(driver, meta);
            }
            // A driver the runtime already **has loaded by name** is resolved against the
            // loaded-driver list first, exactly as the C matches the name against each loaded
            // entry's `driver_name` (`io.c:599-628`). The list records the object path the driver
            // really came from (`erl_ddll:try_load/3`, or the path form of an earlier
            // `open_port({spawn_driver, …})`), so the port re-uses **that** object instead of
            // searching the name afresh — which is what lets a driver loaded with
            // `erl_ddll:try_load(Dir, Name, [])` be opened as `{spawn_driver, Name}`. When the
            // name is not loaded the name doubles as the search ([`driver_lookup`], the documented
            // Milestone-1 extension).
            let (candidates, driver_name) = match driver::loaded_driver_path(&driver_name) {
                Some(path) => (vec![path], driver_name),
                None => (candidates, driver_name),
            };
            // A **C driver** (contract A.4) port: the driver is `dlopen`ed and registered as the
            // port's driver, so `port_command` reaches its `output` callback and its
            // `driver_output` bytes reach the owner. An unknown driver name is the oracle's own
            // `error:badarg` (`io.c`'s `ERTS_OPEN_DRIVER_RET(NULL, -3, BADARG)`), never a
            // fabricated port and never a silent no-op.
            let host = driver::DriverPort::new(
                Heap::new(PORT_HEAP_WORDS)?,
                Rc::clone(&self.atoms),
                self.owner,
            )
            // The port's **state** stream flags, which `driver_get_port_flags` reports: the C reads
            // `ERTS_PORT_SFLG_BINARY_IO`/`ERTS_PORT_SFLG_LINEBUF_IO` (`io.c:5338-5341`) set by the
            // very stream option the port was opened with (`erl_bif_port.c:985-990`).
            .with_port_flags(resolved.port_flags())
            // The liveness view this host answers `driver_monitor_process`'s positive
            // `ERL_DRV_MONITOR_NOT_ALIVE` from: the runtime's most recent observation, published
            // by [`PortRegistry::refresh_liveness`] at the start of each port round (see
            // [`ProcessLiveness`]).
            .with_liveness(registry.liveness())
            // The runtime's async-job delivery handle: the host answers `driver_async`'s pool hook
            // from it, and the port's adapter delivers the completions it queues (see
            // [`AsyncReady`]). One handle per runtime, so one pool.
            .with_async_ready(registry.async_ready());
            // Seize the driver's term queue **before** the host is moved into the port lifecycle
            // (the boxed driver exposes no host): it is the handle a driver callback's
            // `erl_drv_output_term`/`erl_drv_send_term` pushes onto and
            // [`PortRegistry::take_driver_terms`] drains.
            let driver_terms = host.term_queue();
            // The same seize for the names the driver may intern in a later callback: without this
            // handle a name it predicts would never reach the live atom table.
            let driver_atoms = host.pending_atoms();
            // And for the port's control flags: the driver sets them from inside a `control`
            // callback through `set_port_control_flags`, and the runtime reads them back to pick
            // the reply's Erlang form (a list, or a binary in `PORT_CONTROL_FLAG_BINARY` mode).
            let driver_control_flags = host.control_flags();
            // And for the port's **busy state**: the driver marks it from inside a callback through
            // `set_busy_port`, and `erlang:port_command/2,3` reads it back to answer the way the C
            // does (`io.c:1749-1764`). The handle also carries the entry's `ERL_DRV_FLAG_SOFT_BUSY`
            // bit, which `DriverPortAdapter::load` stamps onto this same handle once the library is
            // open — so the value is visible here without re-reading the entry.
            let driver_busy = host.busy_flag();
            // And for the port's **OS child pid**: the driver records it from inside a callback
            // through `erl_drv_set_os_pid` (`io.c:2971`), which `port_info(Port, os_pid)` reads
            // back. Seized here for the same reason as the busy handle — the host is consumed by
            // `load` below, and the write arrives *after* the port is registered.
            let driver_os_pid = host.os_pid_flag();
            // And for the monitors the driver may install through `driver_monitor_process`: the
            // runtime sweeps this handle at the scheduler-pass boundary to mark a monitored process
            // that has died, and the port's own drive point fires the driver's `process_exit`.
            let driver_monitors = host.monitors();
            let adapter =
                driver::DriverPortAdapter::load(&candidates, &driver_name, host, spec.command.clone())
                    .map_err(|_| BeamError::BadArg)?;
            // The loaded entry's own `control` slot, read **before** the adapter is moved into
            // `register`: an entry that leaves it `NULL` is the C's `badarg` for every verb
            // (`io.c:4035`), exactly as [`beam_io`]'s ABI adapter answers the same question.
            let driver_has_control = adapter.has_control();
            let meta = PortMeta {
                command: spec.command,
                // A driver port owns no OS child; `port_info(Port, os_pid)` is `undefined`.
                os_pid: None,
                stream,
                line,
                // A driver port's `{packet, N}` belongs to the driver, not to the emulator.
                packet: None,
                // Nothing to reap, so no `{Port, {exit_status, …}}` is ever synthesized.
                notify_exit: false,
                monitors: Vec::new(),
                owner: self.owner,
                kind: PortName::SpawnDriver,
                driver_has_control,
                driver_terms: Some(driver_terms),
                driver_atoms: Some(driver_atoms),
                driver_control_flags: Some(driver_control_flags),
                driver_busy: Some(driver_busy),
                driver_os_pid: Some(driver_os_pid),
                driver_monitors: Some(driver_monitors),
                input: Cell::new(0),
                output: Cell::new(0),
                // Nothing to reap, so nothing is ever owed — see
                // [`PortMeta::owes_exit_status`].
                exit_status_taken: Cell::new(true),
                held_lines: VecDeque::new(),
                held_since: Cell::new(None),
            };
            return registry.register(Box::new(adapter), meta);
        }
        // The child-stream options (`stderr_to_stdout`) are applied in the child, so they are
        // handed to the driver's constructor rather than carried in `SpawnOptions`. Read before
        // the honored fields below are moved out of `resolved`.
        let stream_options = resolved.stream_options();
        let spawn_options = SpawnOptions {
            stream: stream.into(),
            cd: resolved.cd,
            env: resolved.env,
            args: resolved.args,
            exit_status: resolved.exit_status,
        };
        let driver =
            SpawnDriver::with_stream_options(&spec.spawn_spec()?, &spawn_options, stream_options)?;
        let os_pid = driver.os_pid();
        let meta = PortMeta {
            command: spec.command,
            os_pid,
            stream,
            line,
            // The spawn/fd family is the one the emulator frames itself (see [`packet`]).
            packet,
            notify_exit: spawn_options.exit_status,
            monitors: Vec::new(),
            owner: self.owner,
            kind: spec.kind,
            // `beam_io::SpawnDriver` — this family's driver — has no `control` callback at all, so
            // `port_control/3` is `badarg` for every verb (the C's `!prt->drv_ptr->control`).
            driver_has_control: false,
            // A `spawn`/`fd` port has no C driver, so it has neither driver term queue nor names
            // a driver interned, nor control flags, nor a busy state, nor process monitors.
            driver_terms: None,
            driver_atoms: None,
            driver_control_flags: None,
            driver_busy: None,
            // A `spawn`/`fd` port has no C driver — its OS child pid is the one `beam_io` reports
            // for the process it forked, carried in [`PortMeta::os_pid`].
            driver_os_pid: None,
            driver_monitors: None,
            input: Cell::new(0),
            output: Cell::new(0),
            // A child port owes its status from the start and stops owing it when the status is
            // taken — even a port opened **without** `exit_status`, whose `notify_exit` is what
            // makes the debt exist in the first place (see [`PortMeta::owes_exit_status`]).
            exit_status_taken: Cell::new(false),
            held_lines: VecDeque::new(),
            held_since: Cell::new(None),
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

    /// Marks the driver monitors whose watched process has died, at a scheduler-pass boundary.
    ///
    /// This is the runtime's **observing** half of the A.4 monitor family (see
    /// [`PortRegistry::note_driver_monitor_exits`]): it sweeps every live port's C-driver monitor
    /// tree against the live `table` and marks each watched pid the table no longer holds, so the
    /// **same** pass's port round fires the driver's `ErlDrvEntry::process_exit` for it. It must run
    /// *before* [`Runtime::deliver_port_messages`] for that to happen in one round; nothing is
    /// delivered here, so the pass does not go round again for it.
    ///
    /// The registry is taken with `try_borrow` on a **cloned** [`Rc`] (so the returned borrow is of
    /// the clone, not of the runtime field) exactly like [`Runtime::port_monitors`]: a nested borrow
    /// — an in-flight port BIF — is **deferred**, answering `Ok(0)` and leaving every tree as it was.
    /// Nothing is lost by deferring: a mark is idempotent state in the port's own metadata, so the
    /// next pass re-discovers the same dead pids.
    ///
    /// # Errors
    ///
    /// Never today: a live borrow is deferred rather than reported (there is no obligation to
    /// discharge, only a repeated scan).
    pub(crate) fn drain_driver_monitor_exits(&mut self, table: &ProcessTable) -> Result<usize> {
        let cell = Rc::clone(&self.ports);
        let Ok(registry) = cell.try_borrow() else {
            return Ok(0);
        };
        Ok(registry.note_driver_monitor_exits(table))
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
    /// A [`PortEvent::Signal`] — `os:set_signal/2`'s delivery half — is addressed by **name**
    /// rather than to a pid: it is the 2-tuple `{notify, Name}` for the process
    /// [`whereis`](beam_process::ProcessTable::whereis) resolves for the interned
    /// `erl_signal_server` atom, sent by the emulator itself (`system`), exactly as
    /// `signal_notify_requested` (`erts/emulator/sys/unix/sys.c:589-604`) sends it. With no such
    /// process there is no recipient and the notification is dropped — the C's own behaviour,
    /// never an error and never a message to a name that has no live process.
    ///
    /// Nothing is polled when no port is live, no `tty_select/2` interest is armed **and** no
    /// signal self-pipe holds an undelivered notification (see
    /// [`PortRegistry::has_nothing_to_poll`]), so a runtime that never opened a port and never
    /// caught a signal pays nothing — an armed but idle `handle` disposition does not make this
    /// round sleep, which is what keeps a blocked `run`-route workload off a fixed 25 ms round;
    /// a message whose owner is no longer in `table` is skipped rather than treated as an error.
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
        // Nothing pending ⇒ nothing to wait for: never sleep. A *caught* signal keeps this false
        // even with zero ports, because it must still be delivered.
        {
            let registry = self
                .ports
                .try_borrow()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            if registry.has_nothing_to_poll() {
                return Ok(0);
            }
        }
        let events = {
            let mut registry = self
                .ports
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            // The round is where driver callbacks run, so the liveness view their hosts query is
            // refreshed from the live table first (see [`PortRegistry::refresh_liveness`]).
            registry.refresh_liveness(table);
            // A process that died without disarming its select interest would otherwise keep the
            // runtime polling for ever (see [`PortRegistry::prune_dead_select`]).
            registry.prune_dead_select(|pid| table.contains(pid))?;
            registry.pump(timeout)?
        };
        if crate::debug_trace::enabled() {
            crate::debug_trace::trace(format_args!(
                "deliver_port_messages: timeout={timeout:?} events={}",
                events.len()
            ));
        }
        // A driver callback cannot mint an atom (the live table is borrowed for the whole
        // callback), so a *new* name one interned was handed a predicted index for **this**
        // boundary to land. Committing before any message is built means the terms a driver
        // emitted through `erl_drv_output_term` resolve against a table that holds their names.
        {
            let mut registry = self
                .ports
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            // The `is_empty` guard, taken **before** the table borrow: `code_server.atoms_mut()`
            // is `Rc::make_mut`, so reaching it with another holder alive (a driver port's host,
            // a loaded NIF library) deep-clones the whole table for a round that interns
            // nothing. See [`PortRegistry::has_pending_driver_atoms`].
            if registry.has_pending_driver_atoms() {
                for (port, name) in registry.commit_driver_atoms(self.code_server.atoms_mut()) {
                    // Never silent: the term the driver built already carries an index that now
                    // names a different atom, which no later commit can repair.
                    eprintln!(
                        "beam_rt: driver port {} interned the atom `{name}` at an index the atom \
                         table had already moved past; the name is not resolvable",
                        port.number()
                    );
                }
            }
        }
        // The name `os:set_signal/2`'s notifications are addressed to, resolved **once** and by
        // `lookup` rather than `intern`: the shared table holds the atom only if some code
        // interned it, which is exactly when a process can be registered under it. `None` ⇒
        // there is no `erl_signal_server`, so (like the C) nothing is sent.
        let signal_server = self.code_server.atoms().lookup("erl_signal_server");
        let mut delivered = 0;
        for mut event in events {
            // An `{active, …}` delivery is rendered in the port's **effective** data mode — the
            // `{mode, …}` record its driver accepted (`inet_opts`), with the open-time stream as
            // the fallback — so a `{mode,list}` socket renders a char list as the oracle's does.
            // Resolved here, before `RuntimeContext` borrows `self`.
            let active_mode = match &event {
                PortEvent::ActiveInput { packet } => Some(
                    self.ports
                        .try_borrow()
                        .map(|registry| registry.data_mode(packet.port, packet.data_mode))
                        .unwrap_or(packet.data_mode),
                ),
                _ => None,
            };
            // The interest's **own reference** on the select's resource object is lifted out here,
            // before the arm borrows the event: it must be released only after that arm has minted
            // the notification's term (which takes a reference of its own), so releasing it any
            // earlier could destroy the object between the disarm and the mint.
            let select_hold = match &mut event {
                PortEvent::Select { hold, .. } => hold.take(),
                _ => None,
            };
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
                        read,
                        stream,
                        shape,
                        owner,
                    } => (
                        *owner,
                        port.to_eterm(),
                        ctx.port_data_message_shaped(*port, read, *stream, *shape)?,
                    ),
                    // A line-framed read is the same `{Port, {data, …}}` delivery with the
                    // `{eol|noeol, Data}` body the `line` option selects, and the producing read's
                    // header as its cons prefix.
                    PortEvent::Line {
                        port,
                        header,
                        bytes,
                        eol,
                        stream,
                        owner,
                    } => (
                        *owner,
                        port.to_eterm(),
                        ctx.port_line_message(*port, header, bytes, *eol, *stream)?,
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
                    // A driver→owner completion: the `Port` is the sender, exactly as the
                    // `{Port, {data, …}}` family has it, and the recipient is the owner the
                    // drain already resolved (see [`completion`]).
                    PortEvent::Completion { packet } => (
                        packet.owner,
                        packet.port.to_eterm(),
                        ctx.completion_message(packet)?,
                    ),
                    // An `{active, …}` delivery is rendered in the port's **effective** data mode —
                    // the `{mode, …}` record its driver accepted ([`ops::inet_opts`]), with the
                    // open-time stream as the fallback — so a `{mode,list}` socket renders a char
                    // list as the oracle's does (see [`ops::active_input`]).
                    PortEvent::ActiveInput { packet } => {
                        let mode = active_mode.unwrap_or(packet.data_mode);
                        let resolved = ops::active_input::ActiveInputPacket {
                            port: packet.port,
                            owner: packet.owner,
                            data_mode: mode,
                            kind: packet.kind,
                            event: packet.event.clone(),
                        };
                        (
                            packet.owner,
                            packet.port.to_eterm(),
                            ctx.active_input_message(&resolved)?,
                        )
                    }
                    // A handled signal goes to the `erl_signal_server` process by name, with the
                    // emulator (`system`) as the sender. An unregistered name resolves to no pid,
                    // in which case there is nobody to notify and the signal is dropped.
                    PortEvent::Signal { name } => {
                        let Some(server) = signal_server.and_then(|atom| table.whereis(atom)) else {
                            continue;
                        };
                        let (from, message) = ctx.signal_notify_message(name)?;
                        (server, from, message)
                    }
                    // A ready select registration is addressed directly to the process that
                    // armed it (`enif_select`'s implicit recipient, recorded as `owner`), with the
                    // emulator (`system`) as the sender — the ordinary one-shot select delivery.
                    // What it carries is the registration's own payload shape: the plain
                    // `{select, Handle, Ref, EventAtom}` 4-tuple, or the caller's custom message
                    // verbatim (`ERL_NIF_SELECT_CUSTOM_MSG`).
                    PortEvent::Select {
                        owner,
                        payload,
                        event,
                        handle,
                        obj,
                        ..
                    } => {
                        // The mint comes **first**: it takes its own reference on the object, so
                        // the interest's reference (dropped right after) can never be the one that
                        // destroys the object between the disarm and the mint.
                        let minted =
                            select_notification_handle(*handle, *obj, *owner, ctx.runtime)?;
                        drop(select_hold);
                        let Some(handle) = minted else {
                            // The resource object is gone (released between arm and readiness):
                            // nothing is delivered rather than a notification naming the wrong term.
                            continue;
                        };
                        let (from, message) = ctx.select_message(payload, *event, handle)?;
                        (*owner, from, message)
                    }
                }
            };
            // A port whose recipient has since terminated has no recipient: the message is
            // dropped, not an error.
            if !table.contains(recipient) {
                continue;
            }
            // The runtime's one **fragment-aware** delivery
            // (`RuntimeContext::deliver_in_table`): an owner that is **relocation-exempt** (a
            // process under the `-eval` evaluator or a nested machine) refuses the collection
            // an exhausted delivery would otherwise run, so the heap-fragment fallback is what
            // lets the port message land.
            RuntimeContext::deliver_in_table(self, table, recipient, from, message)?;
            delivered += 1;
        }
        // The `'EXIT'`/`'DOWN'` messages a port close owes its links and monitors take this
        // same route — the same `table`, the same delivery seam, so the same receiver is woken
        // through `beam_process`'s wake list (see [`exit`]).
        delivered += self.deliver_port_signals(table)?;
        // A driver that observed its **peer** closing owes its owner the oracle's own message
        // (`{tcp_closed, Port}`, and the port's retirement when `exit_on_close` is set). It takes
        // this same route — the same `table`, the same delivery seam — so a parked receiver is
        // woken through `beam_process`'s wake list (see [`ops::peer_closed`]).
        delivered += self.deliver_peer_closes(table)?;
        // The **progress signal** a nested port wait reads around its own round (see
        // [`Runtime::port_events_delivered`]): only a message really landed in a mailbox here, so
        // a round that produced nothing is not mistaken for progress. Bumped at this one site —
        // the round's own count, including the port's `'EXIT'`/`'DOWN'` and peer-close channels —
        // because a nested machine is not in the run queue and the pass's `ran` count cannot see a
        // delivery that happened in a round which executed no slice.
        self.note_port_events(delivered);
        Ok(delivered)
    }}

/// The second element of a [`select::SelectPayload::Plain`] notification: the registration's own
/// handle term, or — when the registration carries a NIF **resource object** — the term minted
/// from it.
///
/// The C mints the interest object's magic reference when the notification is queued
/// (`erts_mk_magic_ref`, `erts/emulator/sys/common/erl_check_io.c:746`). The mint is
/// [`crate::nif::resource_term_for`], which answers `None` for a null, foreign or already-dying
/// object instead of fabricating one — and a refusal delivers **nothing**: a notification whose
/// second element named the wrong thing would be a lie no caller can detect.
///
/// A `prim_tty:tty_select/2` registration carries no object: its stored handle term is exactly the
/// C's `argv[0]` resource term, so it is delivered as-is.
///
/// The minted block is built on a scratch host's heap, so the term is copied onto the runtime's
/// never-reset arena (`Runtime::stabilize_term`) before that host drops. The copy keeps the
/// reference's identity word, which is what the resource registry is keyed by, so
/// `enif_get_resource` still resolves it — and the reference the mint took is dropped by the
/// ordinary park-arm resource prune ([`crate::context::gc::resources::reclaim`]) once the delivered
/// message dies.
///
/// This crate is `#![deny(unsafe_code)]`, so `obj` is only ever **moved** — never dereferenced.
fn select_notification_handle(
    handle: Eterm,
    obj: *mut c_void,
    owner: beam_process::ProcessId,
    runtime: &mut Runtime,
) -> Result<Option<Eterm>> {
    if obj.is_null() {
        return Ok(Some(handle));
    }
    let Some(minted) = crate::nif::resource_term_for(runtime, owner, obj) else {
        return Ok(None);
    };
    Ok(Some(runtime.stabilize_term(minted)?))
}

impl RuntimeContext<'_> {
    /// The `-eval` `receive … after T` path's port half: drains a port round and **delivers**
    /// every message it produced for this process into this process's **mailbox**.
    ///
    /// That is where a port message belongs (the C's `erts_queue_message(…, prt->common.id, …)`
    /// arms of `io.c`), and it is what makes the evaluator's `receive` *selective*: the message
    /// is queued before any clause is tried, so a message no clause matches stays queued for a
    /// later `receive` and a message that matched one does not swallow the ones drained with it.
    /// Delivery goes through the runtime's one delivery path
    /// ([`RuntimeContext::deliver_message`]), so an oversized payload takes the heap-fragment
    /// fallback and a parked receiver is woken exactly as on the scheduler route.
    ///
    /// The wait is bounded by `timeout` (in [`PORT_POLL_SLICE`] slices — see
    /// [`PortRegistry::pump`]); nothing is polled at all when no port is live. The returned
    /// count is what was **delivered**, so a caller can rescan the mailbox exactly when it
    /// changed.
    ///
    /// # Errors
    ///
    /// The poller's/driver's failures; a nested borrow of the port table is a typed
    /// capability gap rather than a panic; the message builders' allocation failure when a
    /// term does not fit; and the delivery's own failures
    /// ([`RuntimeContext::deliver_message`] — no mailbox for this pid, or `error:system_limit`
    /// for a payload the receiver's heap and a fragment both refuse).
    pub(crate) fn poll_port_messages_into_mailbox(&mut self, timeout: Duration) -> Result<usize> {
        let pid = self.pid();
        let messages = self.drain_port_messages(timeout)?;
        let mut delivered = 0;
        for (from, message) in messages {
            self.deliver_message(pid, from, message)?;
            delivered += 1;
        }
        Ok(delivered)
    }

    /// Drains a port round into `(sender, message)` pairs, addressed to this process.
    ///
    /// The sender is the one the producing event carries — the **port** for everything a port
    /// produces (`{Port, {data, …}}`, `{Port, {exit_status, _}}`, `{tcp_closed, Port}`,
    /// `{'EXIT', Port, _}`, `{'DOWN', _, port, Port, _}`, a driver-emitted term), and the
    /// `system` atom for the `enif_select`/`prim_tty` notification and the handled-signal
    /// 2-tuple — exactly the senders [`Runtime::deliver_port_messages`] enqueues with.
    ///
    /// Every message is built **on the runtime's pointer-stable arena**, so it stays valid for
    /// the runtime's lifetime (a delivery copies it into the receiver's own heap). The `data`
    /// atom is interned in the runtime's **one shared** table, so the message is nameable by
    /// the printer and matchable by the evaluator's own clauses.
    ///
    /// Nothing is returned for another process's event: this route can only observe its own
    /// mailbox, so such an event keeps its scheduler-route destination (see each arm). An empty
    /// vector means nothing arrived within `timeout`.
    ///
    /// The scheduler path uses [`Runtime::deliver_port_messages`] instead, because there the
    /// recipients come from a process table it holds.
    ///
    /// # Errors
    ///
    /// The poller's/driver's failures; a nested borrow of the port table is a typed
    /// capability gap rather than a panic; the message builders' allocation failure when a
    /// term does not fit.
    pub(crate) fn drain_port_messages(&mut self, timeout: Duration) -> Result<Vec<(Eterm, Eterm)>> {
        // The shared port state is cloned out so the *messages* — which need `&mut self` —
        // are never built while the registry is borrowed (the same snapshot rule
        // `native.rs` follows).
        let registry = Rc::clone(&self.runtime.ports);
        let events = {
            // Same rule as the scheduler path's round: the liveness view is refreshed from the
            // live table immediately before any driver callback can run (see
            // [`PortRegistry::refresh_liveness`]).
            let table = self.process_table();
            let mut registry = registry
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.refresh_liveness(table);
            // Same as the scheduler path's round: a dead process's armed select is
            // dropped before the poll (see [`PortRegistry::prune_dead_select`]).
            registry.prune_dead_select(|pid| table.contains(pid))?;
            registry.pump(timeout)?
        };
        let mut messages = Vec::with_capacity(events.len());
        for event in events {
            match event {
                // A port-produced message's sender is the **port**, the C's
                // `erts_queue_message(…, prt->common.id, …)` (`io.c:3443`/`:3627`/`:5570`).
                PortEvent::Data {
                    port,
                    read,
                    stream,
                    shape,
                    ..
                } => {
                    let message = self.port_data_message_shaped(port, &read, stream, shape)?;
                    messages.push((port.to_eterm(), message));
                }
                // The `line` option's framed shape (see [`PortEvent::Line`]); the evaluator is
                // the port's owner on this path, so the message is its own.
                PortEvent::Line {
                    port,
                    header,
                    bytes,
                    eol,
                    stream,
                    ..
                } => {
                    let message = self.port_line_message(port, &header, &bytes, eol, stream)?;
                    messages.push((port.to_eterm(), message));
                }
                PortEvent::Exited { port, status, .. } => {
                    let message = self.port_exit_message(port, status)?;
                    messages.push((port.to_eterm(), message));
                }
                // A driver-emitted term is the message itself, never a `{Port, Term}` wrapper
                // (`io.c:6174`). The evaluator can only observe its **own** process's mailbox, so
                // a term the driver addressed to a *different* process is not returned here — the
                // scheduler path ([`Runtime::deliver_port_messages`]) delivers it into that
                // process's mailbox instead — rather than being matched as if it were the
                // evaluator's own.
                PortEvent::Term {
                    port, to, term, owner, ..
                } => {
                    if to.unwrap_or(owner) != self.pid() {
                        continue;
                    }
                    let message = self.stabilize_driver_term(term)?;
                    messages.push((port.to_eterm(), message));
                }
                // A handled signal is addressed to the `erl_signal_server` process, and the
                // evaluator is not it: a `RuntimeContext` can only observe its **own** mailbox, so
                // the notification is not returned here — the scheduler path
                // ([`Runtime::deliver_port_messages`]) delivers it into that process's mailbox
                // instead — rather than being matched as if it were the evaluator's own. Same
                // reasoning as the driver-term arm above.
                // A driver→owner completion. The evaluator can only observe its **own**
                // mailbox, so one addressed to a different process is left to the scheduler
                // route — the same rule the driver-term and signal arms follow.
                PortEvent::Completion { packet } => {
                    if packet.owner != self.pid() {
                        continue;
                    }
                    let message = self.completion_message(&packet)?;
                    messages.push((packet.port.to_eterm(), message));
                }
                // An `{active, …}` input delivery. Same rule as the completion arm above: the
                // evaluator observes only its **own** mailbox, so one addressed to another
                // process is left to the scheduler route. The mode is resolved from the inet
                // mirror, exactly as the scheduler route resolves it (see [`ops::inet_opts`]).
                PortEvent::ActiveInput { packet } => {
                    if packet.owner != self.pid() {
                        continue;
                    }
                    let mode = self
                        .runtime
                        .ports
                        .try_borrow()
                        .map(|registry| registry.data_mode(packet.port, packet.data_mode))
                        .unwrap_or(packet.data_mode);
                    let resolved = ops::active_input::ActiveInputPacket {
                        port: packet.port,
                        owner: packet.owner,
                        data_mode: mode,
                        kind: packet.kind,
                        event: packet.event.clone(),
                    };
                    let message = self.active_input_message(&resolved)?;
                    messages.push((packet.port.to_eterm(), message));
                }
                PortEvent::Signal { .. } => continue,
                // A ready select registration. Same rule as the completion arm above: the evaluator
                // observes only its **own** mailbox, so one addressed to another process is left to
                // the scheduler route — but one the evaluator itself armed is its own notification
                // and is returned here, exactly as the oracle's reader would receive it.
                PortEvent::Select {
                    owner,
                    payload,
                    event,
                    handle,
                    obj,
                    hold,
                } => {
                    if owner != self.pid() {
                        continue;
                    }
                    // The mint first, the interest's reference released only after it (see the
                    // scheduler path's arm): the mint takes a reference of its own.
                    let minted = select_notification_handle(handle, obj, owner, self.runtime)?;
                    drop(hold);
                    let Some(handle) = minted else {
                        // The resource object is gone (released between arm and readiness):
                        // nothing is delivered rather than a notification naming the wrong term.
                        continue;
                    };
                    let pair = self.select_message(&payload, event, handle)?;
                    messages.push(pair);
                }
            }
        }
        // A port close owes `'EXIT'`/`'DOWN'` messages to its links and monitors; the ones
        // addressed to the evaluator itself are returned here (with the port as their sender,
        // like every other port-produced message), and every other recipient's stay queued for
        // the scheduler route that owns a process table (see [`exit`]).
        messages.extend(self.poll_port_signals()?);
        // A peer close owed to the evaluator itself rides the same route (see
        // [`ops::peer_closed`]); another process's report is left latched for the scheduler route.
        messages.extend(self.poll_peer_closes()?);
        Ok(messages)
    }

    /// The **test-only** view of [`Self::drain_port_messages`]: the messages without their
    /// senders.
    ///
    /// The port suite asserts the *term* the `-eval` route produces (the `{Port, {data, …}}`
    /// shape, the framed `{eol|noeol, _}` payload, the driver's own reply) on contexts whose
    /// port owner is a synthetic pid no process table holds, so it cannot deliver — and does not
    /// want to. The production route delivers instead
    /// ([`Self::poll_port_messages_into_mailbox`]); nothing outside a unit test may use this.
    #[cfg(test)]
    pub(crate) fn poll_port_messages(&mut self, timeout: Duration) -> Result<Vec<Eterm>> {
        Ok(self
            .drain_port_messages(timeout)?
            .into_iter()
            .map(|(_from, message)| message)
            .collect())
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
    /// `Data` is rendered from the read's own `hbuf`/`buf` split ([`ReadMessage`]'s table) and the
    /// port's stream option: a bare **binary** for a header-less read on a `binary` stream
    /// (anything else — the `list` default, `erl`'s own — is a char list), the improper
    /// `[H1..Hh|<<Data>>]` for a call that carried both halves, and the proper `[H1..Hh]` for one
    /// that carried no body. This entry point is the header-less convenience: a read of nothing but
    /// a body, which is what a child's byte stream is. [`Self::port_data_message_shaped`] is the
    /// full form — it also carries the producer rule ([`StreamShape`]) — and is what both delivery
    /// routes render a [`PortEvent::Data`] through. The `data` atom is interned into the runtime's
    /// one shared table, so the message is nameable by the printer and matchable by a `receive`
    /// clause.
    ///
    /// # The binary stream has no width limit
    ///
    /// A read is as wide as the child wrote: the driver hands over up to 64 KiB of one
    /// `read(2)` ([`beam_io`]'s `READ_CHUNK`), and the oracle delivers a port `binary` of
    /// exactly that width (`open_port({spawn,"…"},[binary])` on a 200-byte writer yields a
    /// 200-byte `binary()`). [`terms::make_heap_binary`] refuses anything past
    /// `ERL_ONHEAP_BINARY_LIMIT` (64) bytes because it only materializes the *on-heap*
    /// `ErlHeapBits` form, so the payload is built by
    /// [`beam_codec::term::make_binary`] instead — the same on-heap layout for every width,
    /// which is the interior-policy substitution `beam_codec` documents
    /// (`ERTS` would store a wider binary off-heap; `is_binary/1`, `byte_size/1`, the
    /// contents and the ETF image are identical either way).
    ///
    /// The block lives on the runtime's never-reset `-eval` arena
    /// ([`RuntimeContext::eval_heap`]) — the same pointer-stable home the evaluator gives a
    /// term the engine arena cannot express — so a delivered message stays valid for the
    /// runtime's lifetime.
    // Both delivery routes render every [`PortEvent::Data`] through
    // [`Self::port_data_message_shaped`] with the event's own read and shape; this stays as the
    // header-less convenience the provider's own suite builds plain byte-stream messages with.
    #[allow(dead_code)]
    pub(crate) fn port_data_message(
        &mut self,
        port: PortId,
        bytes: &[u8],
        stream: StreamType,
    ) -> Result<Eterm> {
        self.port_data_message_shaped(
            port,
            &ReadMessage::body_only(bytes.to_vec()),
            stream,
            StreamShape::Data,
        )
    }

    /// [`Self::port_data_message`] with the read's own split and its producer's rendering rule.
    ///
    /// `read` is one `driver_output*` call's bytes with the C's header/body split intact, and
    /// `shape` is the [`StreamShape`] of the producer that handed it up; see those two types for
    /// the C's terms and for the one fact a read cannot carry. The `list` stream ignores the split
    /// (header and data are both list elements there), so the two only ever change a `binary` port.
    /// Both delivery routes render a [`PortEvent::Data`] through this entry point with the event's
    /// own `read`/`shape`, so a driver's `[H1..Hh|<<Data>>]`, a built-in driver port's header list
    /// and a child's bare binary all reach an owner as the C's own terms.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the term does not fit.
    pub(crate) fn port_data_message_shaped(
        &mut self,
        port: PortId,
        read: &ReadMessage,
        stream: StreamType,
        shape: StreamShape,
    ) -> Result<Eterm> {
        let data = make_atom(self.runtime.code_server.atoms_mut().intern("data"));
        let payload = self.make_stream_payload(read, stream, shape)?;
        let inner = terms::make_tuple(self, &[data, payload])?;
        terms::make_tuple(self, &[port.to_eterm(), inner])
    }

    /// The `{Port, {data, {eol|noeol, Data}}}` message of one **line-framed** port read.
    ///
    /// This is the `line`/`{line, N}` option's shape, the [`PortEvent::Line`] variant's
    /// rendering: the C's `deliver_linebuf_message` (`erts/emulator/beam/io.c:3454`) builds the
    /// `{eol|noeol, Data}` wrapper (`io.c:3430-3435`) and hands it to the port's connected
    /// process as the `{Port, {data, …}}` body. `eol` (the atom `eol`) means the frame was
    /// newline-terminated; `noeol` means a length-limited `{line, N}` chunk or the trailing
    /// partial line released at end-of-stream.
    ///
    /// `Data` is built exactly as a raw read's payload is, because that is literally what the C
    /// does: `deliver_linebuf_message` calls `deliver_read_message` with the frame's bytes as the
    /// **body** (`io.c:3464`) — which it always is, present even when empty, so a blank line is the
    /// oracle's `{eol,<<>>}` on a `binary` port — and **conses `header`** in front of it
    /// (`io.c:3416-3428`), so `driver_output2("HDR",3,"a\nb\n",4)` yields two frames, each
    /// `{eol,[72,68,82|<<"a">>]}`. An end-of-file flush and a plain (header-less) read stamp an
    /// empty header.
    ///
    /// Unlike the unframed builder this one is reachable with an **empty** `bytes`: a blank
    /// line is the oracle's `{Port, {data, {eol, []}}}` (and an empty `noeol` frame is
    /// possible when a flush releases nothing), so the payload is built even for zero bytes.
    /// The `data`/`eol`/`noeol` atoms are interned into the runtime's **one shared** table, so
    /// the message is nameable by the printer and matchable by a `receive` clause, and it is
    /// built on the same pointer-stable `-eval` arena as the unframed builder.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the term does not fit.
    pub(crate) fn port_line_message(
        &mut self,
        port: PortId,
        header: &[u8],
        bytes: &[u8],
        eol: bool,
        stream: StreamType,
    ) -> Result<Eterm> {
        let tag = if eol { "eol" } else { "noeol" };
        let (data, tag) = {
            let atoms = self.runtime.code_server.atoms_mut();
            (make_atom(atoms.intern("data")), make_atom(atoms.intern(tag)))
        };
        // The frame is a read whose body is the frame's bytes and whose header is the producing
        // read's prefix — the C's own shape (`deliver_linebuf_message` → `deliver_read_message`).
        let frame = ReadMessage {
            header: header.to_vec(),
            body: Some(bytes.to_vec()),
        };
        let payload = self.make_stream_payload(&frame, stream, StreamShape::Data)?;
        let framed = terms::make_tuple(self, &[tag, payload])?;
        let inner = terms::make_tuple(self, &[data, framed])?;
        terms::make_tuple(self, &[port.to_eterm(), inner])
    }

    /// The `Data` **payload** of a port-stream message, shaped by the port's stream option, the
    /// read's own header/body split and the producer rule `shape`.
    ///
    /// Shared by [`Self::port_data_message_shaped`] and [`Self::port_line_message`] so the message
    /// shapes can never drift. The legacy C renders every delivered read in two parts — the
    /// driver's **header** bytes as list elements and its **data** bytes as the stream asks
    /// (`deliver_read_message`, `erts/emulator/beam/io.c:3404-3435`, and `deliver_vec_message`,
    /// `:3559-3617`) — so:
    ///
    /// * a `list` stream is a char list of **all** the bytes, whatever the split;
    /// * a `binary` stream is a bare binary when the read carried a body and no header
    ///   (`driver_output`, built by [`beam_codec::term::make_binary`] — see
    ///   [`Self::port_data_message`] on the width);
    /// * a `binary` stream is a **proper list** of the byte integers when the read carried a header
    ///   and **no** body ([`ReadMessage::body`] is `None`, the C's `NULL` `buf`:
    ///   `driver_output2(hbuf, hlen, NULL, 0)`, what a `ram_file_drv` `OK`/`NUMBER`/`ERROR` reply
    ///   makes);
    /// * a `binary` stream is the **improper** list `[H1..Hh | <<Data>>]` when the read carried
    ///   both halves, an empty body rendering `[...|<<>>]` (a zero-length `DATA` reply, i.e. a read
    ///   at end-of-file) — and a read of nothing at all the empty proper list `[]`;
    /// * a **built-in driver port**'s **header-less** read ([`StreamShape::DriverOutput`]) is list
    ///   elements on **either** stream — the C's header rendering (`io.c:3404-3435`). A read that
    ///   carries a header keeps its own split (see that variant), so `ram_file_drv`'s header-only
    ///   `OK`/`NUMBER`/`ERROR` and its two-halved `DATA` reply render the oracle's own terms.
    ///
    /// Every block is allocated on the runtime's pointer-stable `-eval` arena.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the term does not fit.
    fn make_stream_payload(
        &mut self,
        read: &ReadMessage,
        stream: StreamType,
        shape: StreamShape,
    ) -> Result<Eterm> {
        if stream == StreamType::List
            || (shape == StreamShape::DriverOutput && read.header.is_empty())
        {
            // The header is list elements too, so the split is invisible on this stream — and a
            // built-in driver port's flat, header-less payload is rendered as the C's header half
            // on **either** stream (see [`StreamShape::DriverOutput`]).
            return self.bytes_to_list(&read.flattened(), NIL);
        }
        match read.body.as_deref() {
            // No body part at all: the whole read is the (already-complete) header list.
            None => self.bytes_to_list(&read.header, NIL),
            // Otherwise the body is the term's tail — a binary, empty or not — and the header
            // bytes are consed in front of it (an empty header leaves the binary bare, the
            // header-less `driver_output` shape).
            Some(body) => {
                let tail = beam_codec::term::make_binary(self.eval_heap(), body)?;
                self.bytes_to_list(&read.header, tail)
            }
        }
    }

    /// `bytes` as a char list whose tail is `tail` — `[B1..Bn | Tail]`.
    ///
    /// `NIL` builds a proper list; any other `tail` makes the result improper, which is exactly
    /// the C's `buf_to_intlist` applied to a header list that already ends in a binary
    /// (`io.c:3426-3428`, `:3613-3617`). The cells live on the runtime's `-eval` arena.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the list does not fit.
    fn bytes_to_list(&mut self, bytes: &[u8], tail: Eterm) -> Result<Eterm> {
        let mut list = tail;
        for byte in bytes.iter().rev() {
            list = terms::make_cons(self, make_small(i64::from(*byte)), list)?;
        }
        Ok(list)
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

    /// The `{notify, SignalName}` message a **handled signal** produces, and its sender — the
    /// delivery half of `os:set_signal/2` (contract A.8).
    ///
    /// ERTS's signal dispatcher queues exactly this 2-tuple to the process registered as
    /// `erl_signal_server`, with `am_system` as the sender
    /// (`signal_notify_requested`, `erts/emulator/sys/unix/sys.c:589-604`) — so the sender this
    /// returns is the bare atom `system`, not a pid. `name` is the signal's own atom name
    /// ([`beam_io::signal::Signal::name`]), and both atoms are interned in the runtime's **one
    /// shared** table, so the message is nameable by the printer and matchable by a `receive`
    /// clause that spells `{notify, sigusr1}`.
    ///
    /// The caller resolves the recipient ([`beam_process::ProcessTable::whereis`]); this builder
    /// deliberately knows nothing about it, so the same helper serves any delivery path.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the tuple does not fit.
    pub(crate) fn signal_notify_message(&mut self, name: &str) -> Result<(Eterm, Eterm)> {
        let atoms = self.runtime.code_server.atoms_mut();
        let notify = make_atom(atoms.intern("notify"));
        let signal = make_atom(atoms.intern(name));
        let sender = make_atom(atoms.intern("system"));
        let message = terms::make_tuple(self, &[notify, signal])?;
        Ok((sender, message))
    }

    /// The message a ready select registration delivers, and its sender — the delivery half of
    /// both `enif_select` (A.3) and `prim_tty:tty_select/2` (A.7).
    ///
    /// ERTS queues the notification to the **selecting** process with `am_system` as the sender
    /// (`send_select_msg`, `erts/emulator/sys/common/erl_check_io.c:836-858`), in one of two
    /// shapes:
    ///
    /// * [`select::SelectPayload::Plain`] — the 4-tuple `{select, Handle, Reference, EventAtom}`
    ///   `prepare_select_msg` builds (`:699-800`): the registration's resource term, the caller's
    ///   reference, and the event atom of the direction actually observed
    ///   (`ready_input`/`ready_output`/`ready_error`, `:1451-1463`). For
    ///   `prim_tty:tty_select/2` the `Handle` is the opaque tty term
    ///   ([`crate::native::prim_tty`]'s reference) and the `Reference` is
    ///   `prim_tty:reader/1`'s `ReaderRef`, which `prim_tty:reader_loop/2`
    ///   (`lib/kernel/src/prim_tty.erl:454-482`) matches on both.
    /// * [`select::SelectPayload::Custom`] — the caller's own message term **verbatim**
    ///   (`ERL_NIF_SELECT_CUSTOM_MSG`, `:716-731`): no tuple and no event atom, exactly as the C
    ///   replaces the whole notification with `msg_env`'s term.
    ///
    /// The payload terms are already **stabilized** (see [`PortRegistry::arm_nif_select`]), so the
    /// builder copies them into the tuple without re-homing them; the `select`/event/`system`
    /// atoms are interned in the runtime's **one shared** table.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the tuple does not fit.
    pub(crate) fn select_message(
        &mut self,
        payload: &select::SelectPayload,
        event: select::SelectEvent,
        handle: Eterm,
    ) -> Result<(Eterm, Eterm)> {
        let atoms = self.runtime.code_server.atoms_mut();
        let sender = make_atom(atoms.intern("system"));
        match payload {
            select::SelectPayload::Custom { message } => Ok((sender, *message)),
            select::SelectPayload::Plain { reference } => {
                let select = make_atom(atoms.intern("select"));
                let event_atom = make_atom(atoms.intern(event.name()));
                let message =
                    terms::make_tuple(self, &[select, handle, *reference, event_atom])?;
                Ok((sender, message))
            }
        }
    }
}

/// The command a `{spawn_driver, Name}` `open_port/2` was given, truncated at the first blank —
/// the C's own rule (`io.c` walks the name to the first blank before matching
/// `ErlDrvEntry::driver_name`), so `"ram_file_drv extra args"` names the driver `ram_file_drv`.
///
/// This is the driver **search**'s rule ([`driver_lookup`]); whether the resolved driver declares a
/// `control` callback is the driver's own answer, captured at open time
/// ([`PortMeta::driver_has_control`]), not anything read from this name.
fn driver_path_name(command: &str) -> &str {
    command.split(' ').next().unwrap_or(command)
}

/// Whether `spec` names a port the C would open **through a driver** — the predicate behind the
/// `[exit_status]` refusal (`erts/emulator/beam/io.c:595-628`).
///
/// The C resolves the name against its loaded-driver list for **both** spawn name forms:
/// `{spawn, Cmd}` truncates the command at the first blank and searches for the token
/// (`io.c:599-628`), and `{spawn_driver, Name}` searches for the name itself. A hit means the port
/// is a *driver* port, for which `opts->exit_status` is `badarg` (`io.c:623-626`) — and an
/// unresolved `{spawn_driver, Name}` is `badarg` a few lines later for the same input, so the answer
/// does not depend on the lookup.
///
/// The membership gates are the emulator's own two: [`driver::is_static_driver`] (the `driver_tab.c`
/// statics — `tcp_inet`, `udp_inet`, `ram_file_drv`) and [`driver::driver_is_loaded`] (a driver an
/// `erl_ddll:load_driver/3` put on the loaded list). `fd`/`efile` are deliberately **not** members:
/// OTP 21+ made them ordinary modules, so the oracle runs `{spawn,"fd"}` as a shell command and
/// accepts `[exit_status]` for it (verified against erts-17.1).
///
/// The `{fd, N}` **name form** is a driver port, and a different driver from `spawn_driver`: it is
/// the emulator's own `fd_driver` (`erts/emulator/sys/unix/sys_drivers.c:257`, registered at
/// `io.c:86`), so the same test refuses `[exit_status]` for it — measured on erts-17.1,
/// `open_port({fd,1},[exit_status])` raises `error:badarg`. It is refused here for the same reason
/// the driver names are: no Erlang-reachable driver can report a status (see the `open_port` site).
/// `{spawn_executable, Path}` skips the lookup entirely in the C
/// (`spawn_type != ERTS_SPAWN_EXECUTABLE`), so it is never a driver port.
fn opens_a_driver_port(spec: &NameSpec) -> bool {
    match spec.kind {
        PortName::SpawnDriver | PortName::SpawnDriverBuiltin => true,
        PortName::Spawn => {
            let name = driver_path_name(&spec.command);
            driver::is_static_driver(name) || driver::driver_is_loaded(name)
        }
        PortName::SpawnExecutable => false,
        PortName::Fd => true,
    }
}

/// The `error:notsup` raise a `[force]` command answers on a driver whose entry does not
/// advertise `ERL_DRV_FLAG_SOFT_BUSY`
/// (`erts/emulator/beam/erl_bif_port.c:195-198`).
///
/// [`PortRegistry::decide_before_data`] needs it because it answers that arm **before** the command
/// reaches [`beam_io`], whose [`beam_io::Port::command`] raises the identical reason for the same
/// rule; both spell the bare atom the oracle raises, never a message the harness could not compare.
fn notsup() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("notsup")))
}/// The driver search a `{spawn_driver, Name}` name performs: the candidate object paths to try
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
/// * a bare name is tried as itself and as `./<name>` (the runtime's own directory), with `<name>`
///   as the driver name — the C's answer for a name in its search path.
///
/// Every candidate is a **suffix-free spelling**, because the `dlopen` loader appends the platform's
/// shared-object suffix unconditionally, exactly as the oracle does (`beam_abi::driver::loader::load`;
/// `erts_sys_ddll_open` copies `FILE_EXT` after the given path,
/// `erts/emulator/sys/unix/erl_unix_sys_ddll.c:104-118`). So `…/t` and `…/t.so` both yield the
/// candidate `…/t` (which the loader opens as `…/t.so`, never `…/t.so.so`), and a bare `foo.so` is
/// searched as `foo`/`./foo` instead of as `foo.so.so` (the bare-name driver name stays the name as
/// written). A name that reaches no object, or an object whose entry declares another name, is
/// [`BeamError::BadArg`] at the call site — the oracle's own reason for an unknown driver.
fn driver_lookup(name: &str) -> (Vec<String>, String) {
    let path = driver_path_name(name);
    // The suffix-free spelling the loader expects — it appends the suffix itself, so a candidate
    // that still carried one would be handed to `dlopen` as `<path>.so.so`.
    let object = strip_shared_object_suffix(path);
    let mut candidates = vec![object.to_string()];
    let driver_name = if path.contains('/') {
        let base = std::path::Path::new(path)
            .file_name()
            .and_then(|stem| stem.to_str())
            .unwrap_or(path);
        strip_shared_object_suffix(base).to_string()
    } else {
        // `dlopen` searches the loader's path for a name without a slash, never the current
        // directory, so the runtime's own directory is an explicit candidate.
        candidates.push(format!("./{object}"));
        path.to_string()
    };
    (candidates, driver_name)
}

/// The platform shared-object suffix, i.e. what [`beam_abi::driver::loader::load`] appends to every
/// path it is handed (`std::env::consts::DLL_SUFFIX`: `.so` on Unix, the oracle's `FILE_EXT`).
const SHARED_OBJECT_SUFFIX: &str = std::env::consts::DLL_SUFFIX;

/// `path` with one trailing shared-object suffix removed — the **suffix-free spelling** the driver
/// loader expects, since [`beam_abi::driver::loader::load`] appends the platform's suffix
/// unconditionally (`erts_sys_ddll_open`, `erl_unix_sys_ddll.c:104-118`). Only an *exact* trailing
/// suffix is removed, so a driver object legitimately named `foo.so.1` is left alone (the oracle
/// would open `foo.so.1.so`).
fn strip_shared_object_suffix(path: &str) -> &str {
    path.strip_suffix(SHARED_OBJECT_SUFFIX).unwrap_or(path)
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

/// `count` bytes as an Erlang integer for a `port_info(Port, input | output)` answer.
///
/// A byte total is always non-negative and, on any real host, far below `2^59`, so the small-int
/// encoding is the oracle's own shape. The `SystemLimit` arm is the honest answer for a count that
/// somehow did not fit a small integer rather than a truncated figure.
///
/// # Errors
///
/// [`BeamError::SystemLimit`] when `count` does not fit a small integer.
fn byte_count(count: u64) -> Result<Eterm> {
    let value = i64::try_from(count).map_err(|_| BeamError::SystemLimit)?;
    try_make_small(value).map_err(|_| BeamError::SystemLimit)
}

/// The provider's unit suite: `open_port/2` decoding, the `port_info/1,2` vocabulary,
/// the message pump and its driver interop (see `src/port/tests.rs`).
#[cfg(test)]
mod tests;

/// The `ram_file_drv` **command/reply matrix** against the live oracle
/// (see `src/port/ram_file_matrix_tests.rs`). A separate file so the matrix's session runner and
/// oracle-expression builder do not grow the provider's unit suite.
#[cfg(test)]
mod ram_file_matrix_tests;
/// The `[exit_status]` **name-form matrix** against the live oracle: which `open_port/2` `Name`
/// forms accept the option and which are `badarg`, plus the "a refusal consumes no port number"
/// property (see `src/port/exit_status_matrix_tests.rs`). A separate file so the matrix does not
/// grow this one.
#[cfg(test)]
mod exit_status_matrix_tests;
/// The `erlang:port_command/2,3` **row matrix** against the live oracle: the idle-port rows both
/// runtimes can reach, and the busy-port arms this runtime can only reach through its own
/// controllable driver fixture (see `src/port/command_matrix_tests.rs`). A separate file so the
/// matrix does not grow this one.
#[cfg(test)]
mod command_matrix_tests;
/// The **busy-port park → pump → resume** probe: a compiled process inside a scheduler slice that
/// is suspended on `erlang:port_command/2`, resumed once the port goes idle, and the driver's
/// write counter proving the suspending attempt consumed nothing (see
/// `src/port/busy_suspend_tests.rs`). A separate file so it does not grow this one.
#[cfg(test)]
mod busy_suspend_tests;
/// The A.4 **driver-monitor marking** suite: the runtime's pass-boundary sweep over every live
/// port's C-driver monitor tree (see `src/port/monitor_tests.rs`). A separate file so the suite
/// does not grow this one.
#[cfg(test)]
mod monitor_tests;
/// Test-only helpers for the provider's own suite and the native floor's port tests: a
/// minimal always-ready driver and the port-registration helper (see `src/port/test_support.rs`).
#[cfg(test)]
pub(crate) mod test_support;
/// The `{spawn_driver, Name}` **namespace** suite: the statically linked set, the oracle's
/// `badarg` for everything else, and the `erl_ddll:loaded_drivers/0` agreement
/// (see `src/port/spawn_driver_tests.rs`). A separate file so the namespace suite does not grow
/// this one.
#[cfg(test)]
mod spawn_driver_tests;
/// The **driver-capability** suite behind `erlang:port_control/3`: a driver with no `control`
/// callback is the oracle's `badarg`, a control-bearing one still answers
/// (see `src/port/control_capability_tests.rs`). A separate file so the capability matrix does not
/// grow this one.
#[cfg(test)]
mod control_capability_tests;
/// The **`inet` control-opcode matrix** through `erlang:port_control/3` against the live oracle
/// (see `src/port/inet_matrix_tests.rs`). A separate file so the matrix's probe expressions do not
/// grow this one.
#[cfg(test)]
mod inet_matrix_tests;
/// The `[binary]` **stream-shape** suite: the term shape a driver-produced reply takes under the
/// binary and default list streams (see `src/port/stream_shape_tests.rs`). A separate file so the
/// shape matrix does not grow this one.
#[cfg(test)]
mod stream_shape_tests;
/// The **end-to-end driver-output shape** suite: a built-in driver port's `{Port, {data, …}}` term
/// as the runtime really delivers it, whole-matrix against the live oracle (see
/// `src/port/driver_output_shape_tests.rs`). A separate file so the session runner and the oracle
/// expression do not grow this one.
#[cfg(test)]
mod driver_output_shape_tests;
/// The `{packet, N}` **framing** suite: the emulator-side length header on the way out and the
/// packet reassembly on the way in (see `src/port/packet_framing_tests.rs`). A separate file so the
/// framing matrix does not grow this one.
#[cfg(test)]
mod packet_framing_tests;
/// The driver→owner **completion** builder/render matrix: the `{inet_reply, …}` and
/// `{inet_async, …}` terms the runtime owes a port's owner, and the drain-time owner/stream
/// snapshot that keeps them deliverable across a close (see `src/port/completion_tests.rs`). A
/// separate file so the matrix does not grow this one.
#[cfg(test)]
mod completion_tests;
/// The **async-job pool** end-to-end suite (contract A.4, `erl_async.c`): `driver_async` through the
/// real port round — a positive id and one `ready_async` with the submitted `async_data`, the
/// port-exit path's single `async_free`, the `+A 0` inline configuration, and the entry with no
/// `ready_async` slot (see `src/port/async_ready_tests.rs`). A separate file so the C fixture text
/// and the measured oracle table do not grow this one.
#[cfg(test)]
mod async_ready_tests;
