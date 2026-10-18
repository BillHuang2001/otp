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
//! a `binary` for a `binary`-stream port (a char list otherwise), **at whatever width the
//! child wrote** (see [`RuntimeContext::port_data_message`]).
//!
//! A **built-in driver port**'s replies (`{spawn_driver, "ram_file_drv"}` and friends) are shaped
//! by the C's driver-output rule instead: a driver output call's **header** bytes are list
//! elements on either stream, so a `[binary]` port's `{data, D}` is a list of byte integers, not a
//! binary ([`StreamShape::DriverOutput`]). That is exact for a header-only reply — every
//! `ram_file_drv` `OK`/`NUMBER`/`ERROR` — and is the documented residual for a reply that carried
//! a data part, whose binary tail the `beam_io` seam does not carry.
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

use std::borrow::Cow;
use std::cell::{Cell, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::os::fd::RawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub mod driver;
pub(crate) mod exit;
pub(crate) mod info_extras;
pub(crate) mod ops;
pub(crate) mod packet;
pub(crate) mod tty;
pub(crate) mod watch;
use beam_alloc::Heap;
use beam_abi::driver::types::PORT_CONTROL_FLAG_BINARY;
use beam_bifs::PortHook;
use beam_common::{BeamError, Result};
use beam_engine::terms;
use libc::c_int;
use beam_io::{
    Driver, DriverRegistry, Event, Poller, PortCommandOptions, PortExitReason, PortNotification,
    PortRegistry as IoPortRegistry, SpawnDriver, SpawnOptions, SpawnSpec, SpawnStream,
    StreamOptions, PORT_REDS_EXECUTE,
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
    /// through the driver's poller-registered descriptor on the port-task round, so
    /// [`PortRegistry::take_driver_outputs`] must not drain it and [`PortRegistry::write_command`]
    /// takes the ordinary `port_command` path.
    SpawnDriverBuiltin,
    /// `{fd, N}`: a descriptor the Erlang side already owns.
    Fd,
}

impl PortName {
    /// Whether the port's bytes come from a C driver's **inline `output` callback** — only a
    /// `dlopen`ed `{spawn_driver, Name}` port ([`PortName::SpawnDriver`]).
    ///
    /// A built-in driver port ([`PortName::SpawnDriverBuiltin`]) — and every `spawn`/`fd` port —
    /// delivers through a poller-registered descriptor on the port-task round instead, so its
    /// bytes are not harvested from the driver's input queue.
    fn bytes_from_driver_output(self) -> bool {
        match self {
            PortName::SpawnDriver => true,
            PortName::SpawnDriverBuiltin
            | PortName::Spawn
            | PortName::SpawnExecutable
            | PortName::Fd => false,
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

/// How one driver output call splits the bytes it delivered into the legacy C's `hbuf`/`buf`
/// halves.
///
/// The C has four output entry points, and its port-message builders
/// (`deliver_read_message`, `erts/emulator/beam/io.c:3363`, and `deliver_vec_message`, `:3501`)
/// always render the **header** bytes as list *elements* while the **data** bytes follow the
/// port's stream bit (`ERTS_PORT_SFLG_BINARY_IO`), a `binary` for a `binary` port and further
/// list elements for the default `list` one:
///
/// | the driver called | header | data | a `[binary]` port's `{data, D}` |
/// |---|---|---|---|
/// | `driver_output(buf, len)` | none | `buf` | `<<Data>>` |
/// | `driver_output2(hbuf, hlen, NULL, 0)` | `hbuf` | none | `[H1..Hh]` — a **proper** list |
/// | `driver_output2(hbuf, hlen, buf, len)` | `hbuf` | `buf` | `[H1..Hh\|<<Data>>]` |
/// | `driver_output_binary(hbuf, hlen, bin, off, len)` | `hbuf` | `bin` | `[H1..Hh\|<<Data>>]` |
///
/// So a `binary` port does **not** always deliver a binary: the shape is a function of
/// `(stream, split)`, never of the bytes themselves. The `list` stream is insensitive to the
/// split — the header and the data are both list elements there.
///
/// A driver's reply is the interesting case in practice (`rfile_command`'s `numeric_reply` goes
/// through `driver_output2` and its `DATA` reply through `driver_output_binary`,
/// `erts/emulator/drivers/common/ram_file_drv.c:197-228,448-489`), while a byte-stream *read* —
/// a child's stdout, an `fd` port — is header-less and therefore a binary.
///
/// [`beam_io`] folds each of those calls into one flat byte queue
/// ([`beam_io::DriverContext::input`]), so a payload arrives here as bytes with **no split
/// attached**. Exactly one shape says so: [`StreamShape::DriverOutput`], which a **built-in
/// driver port** reports, and which renders every delivered byte as list elements — the C's header
/// rendering. Every other producer in this runtime (a child's stdout on the port-task round, an
/// `fd` port, and a `dlopen`ed driver's `driver_output` on the driver's own input queue) reports
/// [`StreamShape::Data`] and keeps the stream option's own shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum StreamShape {
    /// `driver_output`: no header — every delivered byte is the driver's data.
    #[default]
    Data,
    /// A **built-in driver port**'s payload: an in-tree driver ([`PortName::SpawnDriverBuiltin`] —
    /// `ram_file_drv`, `tcp_inet`, `udp_inet`) answered through one of the four `driver_output*`
    /// entry points, and [`beam_io`]'s flat byte queue does not say which.
    ///
    /// It renders as **list elements on either stream** — what the C gives the header half of
    /// every output call (`deliver_read_message`, `erts/emulator/beam/io.c:3404-3435`), so a
    /// `[binary]` port's `{data, D}` is a list of byte integers rather than a binary.
    ///
    /// That rendering is **exact** when the driver answered header-only — `driver_output2(hbuf,
    /// hlen, NULL, 0)`, which is every `ram_file_drv` `OK`/`NUMBER`/`ERROR`
    /// (`ram_file_drv.c:181-228`) — and is the documented residual otherwise: a call that carried
    /// a **data part** (a `DATA` reply, `driver_output_binary`, `:448`) has its binary tail
    /// rendered as further list elements (`[2,0,0,0,2,97,98]` where the oracle delivers
    /// `[2,0,0,0,2|<<97,98>>]`), and a header-less `driver_output` (an `inet` socket read) is
    /// rendered as list elements where the oracle delivers a binary. Both residuals are the
    /// producer's, not the renderer's: distinguishing them needs the header/data split across the
    /// seam, which is the `beam_io` handoff in this node's `CONTEXT.md`.
    DriverOutput,
    // The two header shapes have no producer in this runtime: [`beam_io`] folds every output
    // call into one flat byte queue (`beam_io::DriverContext::input`), so the port layer cannot
    // tell which call delivered a payload. They are kept — and pinned by `stream_shape_tests` —
    // because they are what the message builder must render the moment the input channel carries
    // the split (the handoff is recorded in this node's `CONTEXT.md`).
    #[allow(dead_code)]
    /// `driver_output2(hbuf, hlen, NULL, 0)`: the **whole** delivered payload is the header, so a
    /// `binary` port renders a proper list of byte integers.
    Header,
    #[allow(dead_code)]
    /// A header followed by a data part — `driver_output2(hbuf, hlen, buf, len)` or
    /// `driver_output_binary(...)`. The first `len` bytes are the header, the rest the data; the
    /// data part is **present even when it is empty**, which is why a zero-length `DATA` reply
    /// renders `[H1..Hh|<<>>]` rather than a bare list.
    HeaderThenData {
        /// How many leading bytes of the payload are the header.
        len: usize,
    },
}

impl StreamShape {
    /// The `(header, data)` halves of a delivered payload, `data` being `Some` exactly when the
    /// driver's call carried a data part.
    ///
    /// `len` is clamped to the payload: a shape naming a longer header than was delivered would
    /// otherwise *drop* bytes, and dropping bytes is a wrong result. No bytes are invented
    /// either — the clamp only ever widens the header to the whole payload.
    fn split(self, bytes: &[u8]) -> (&[u8], Option<&[u8]>) {
        match self {
            StreamShape::Data => (&[], Some(bytes)),
            // [`StreamShape::DriverOutput`] never reaches here ([`RuntimeContext::make_stream_payload`]
            // renders it as list elements before the split is asked for); were it reached, every
            // byte is the header — the same answer as [`StreamShape::Header`].
            StreamShape::DriverOutput | StreamShape::Header => (bytes, None),
            StreamShape::HeaderThenData { len } => {
                let len = len.min(bytes.len());
                (&bytes[..len], Some(&bytes[len..]))
            }
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
    /// The two message producers route on it (see [`PortName::bytes_from_driver_output`], and
    /// [`PortMeta::has_driver_control`] for the `port_control` gate's `Name`-form half): a
    /// `dlopen`ed `{spawn_driver, Name}` port's bytes
    /// come from the C driver's `output` callback (queued on the port's input queue by
    /// [`driver::DriverPortAdapter`]) and are drained by [`PortRegistry::take_driver_outputs`],
    /// while every other port's — including a **built-in** driver port's — come from a
    /// poller-registered descriptor through the port-task round.
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
    /// `io.c:4133-4175`). Seized from the host with [`driver::DriverPort::control_flags`] before
    /// the host is moved into the port lifecycle (the boxed driver exposes no host), the same way
    /// [`PortMeta::driver_terms`] and [`PortMeta::driver_atoms`] are.
    ///
    /// `None` for every port without a C driver (a `spawn`/`fd` port) — one whose `control` reply
    /// is the oracle's own `badarg`.
    driver_control_flags: Option<Rc<Cell<c_int>>>,
    /// The **busy state** of a `{spawn_driver, Name}` port's C driver — the C's
    /// `prt->sched.flags & ERTS_PTS_FLG_BUSY_PORT`, as the driver's own `set_busy_port` sets it.
    ///
    /// `erlang:port_command/2,3` consults it exactly as `erts_port_output` does (`io.c:1749-1764` and
    /// `erl_bif_port.c:192-220`): `nosuspend` on a busy port answers `false`, `force` bypasses the
    /// busy check, and a plain command on a busy port suspends the caller. The handle also carries
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
    /// `stream` **and** by `shape` — the driver output call that produced the bytes (see
    /// [`StreamShape`]).
    Data {
        /// The port the bytes came from.
        port: PortId,
        /// The bytes one driver read produced.
        bytes: Vec<u8>,
        /// The port's `binary`/`list` option.
        stream: StreamType,
        /// How the output call that produced `bytes` split them into the C's `hbuf`/`buf` halves:
        /// [`StreamShape::DriverOutput`] for a built-in driver port's payload (rendered as list
        /// elements on either stream), [`StreamShape::Data`] for every other producer here (a
        /// child's stream bytes, an `fd` port, a `dlopen`ed driver's `driver_output`).
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
    /// the trailing partial line released at end-of-file. `Data` is shaped by `stream`
    /// exactly as the raw [`PortEvent::Data`] arm shapes its payload.
    ///
    /// Unlike [`PortEvent::Data`] — whose empty payload is dropped, because a driver read of
    /// nothing is no message at all — an **empty `bytes` here is a real frame**: a blank line
    /// is the oracle's `{Port, {data, {eol, []}}}`. The line channel
    /// ([`beam_io::PortRegistry::take_line_notifications`]) is what feeds this variant; a
    /// line-mode port never reports the raw [`PortNotification::Data`] channel.
    Line {
        /// The port the frame came from.
        port: PortId,
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
    /// A `prim_tty:tty_select/2` read interest became ready — the delivery half of the
    /// `enif_select` a `prim_tty` reader arms before it announces itself.
    ///
    /// Like [`PortEvent::Signal`] this variant has **no port**: the descriptor (`fd 0`) is
    /// registered with the poller directly, not carried by any port, and the message's
    /// recipient is the process that armed the select (recorded here as `owner`), not a
    /// port-derived one. The message is the 4-tuple `{select, Handle, Reference, ready_input}`
    /// with the emulator itself (`system`) as the sender, exactly the shape
    /// `prepare_select_msg` (`erts/emulator/sys/common/erl_check_io.c:731-761`) builds and
    /// `send_select_msg` (`:836-858`) queues — see [`tty`] for the C ground truth and the
    /// one-shot rule.
    Select {
        /// The process the select message belongs to — `enif_select`'s implicit recipient (the
        /// calling process).
        owner: ProcessId,
        /// The opaque tty handle (`argv[0]`), the tuple's second element.
        handle: Eterm,
        /// `prim_tty:reader/1`'s `ReaderRef` (`argv[1]`), the tuple's third element.
        reference: Eterm,
    },
}

impl PortEvent {
    /// The port this event is **about**, or `None` for the two node-global / owner-keyed kinds
    /// that no port carries (a handled signal and a `tty_select/2` report).
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
            PortEvent::Signal { .. } | PortEvent::Select { .. } => None,
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
    /// The `prim_tty:tty_select/2` read-select registrations with [`PortRegistry::poller`] — the
    /// delivery half of the `enif_select` a `prim_tty` reader arms (see [`tty`]).
    tty: tty::TtyWatch,
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
    /// and a **built-in** driver (`ram_file_drv`, `efile`) writes its reply from that very
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
            tty: tty::TtyWatch::new(),
            liveness: ProcessLiveness::new(),
            extras: ops::PortExtras::default(),
            pending_events: Vec::new(),
            pending_port_signals: exit::PendingPortSignals::default(),
        })
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

    /// Arms a **one-shot** read interest on the tty descriptor (fd 0) for `owner` — the
    /// `enif_select` half of `prim_tty:tty_select/2` (see [`tty`]).
    ///
    /// `handle` and `reference` must already be **stabilized** terms (copied onto the runtime's
    /// never-reset arena by [`Runtime::stabilize_term`]): a raw `Eterm` held across a scheduler
    /// pass belongs to a heap a collection may relocate, and this registration outlives the call
    /// that armed it. The caller — [`crate::native::prim_tty`] — owns that stabilisation, because
    /// it holds the arguments and the runtime's arena.
    ///
    /// # Errors
    ///
    /// The poller's own registration failure, or the typed exhaustion of the key reservation.
    /// The C **discards** `enif_select`'s answer, so the NIF answers `ok` either way — but the
    /// failure is still returned here rather than swallowed, and the NIF maps it to "nothing
    /// armed".
    pub(crate) fn arm_tty_select(
        &mut self,
        owner: ProcessId,
        handle: Eterm,
        reference: Eterm,
    ) -> Result<()> {
        self.tty
            .arm(&mut self.poller, owner, handle, reference)
    }

    /// Drops (and de-registers) every `tty_select/2` registration whose owner `live` denies —
    /// the C's `tty_monitor_down` `ERL_NIF_SELECT_STOP` arm (see [`tty`]).
    ///
    /// Called from both port pumps, immediately after [`PortRegistry::refresh_liveness`], so a
    /// reader that died without disarming its select cannot keep the runtime polling for ever.
    ///
    /// # Errors
    ///
    /// The poller's own de-registration failure.
    pub(crate) fn prune_dead_tty(
        &mut self,
        live: impl FnMut(ProcessId) -> bool,
    ) -> Result<()> {
        self.tty.prune_dead(&mut self.poller, live)
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
    /// # The driver's reply is retained, not discarded
    ///
    /// That task round is the round a **built-in** driver answers from: `ram_file_drv`/`efile`
    /// write their reply into [`beam_io::DriverContext::input`] during the `OUTPUT` task, and
    /// the round then harvests it as a [`beam_io::PortNotification::Data`] — draining the port's
    /// input queue, so no later round can see it. The outcome is therefore **folded into**
    /// [`PortRegistry::pending_events`] (through the same
    /// [`PortRegistry::fold_notifications`] the pump uses) rather than dropped, and the next
    /// [`PortRegistry::pump`] reports it as the owner's `{Port, {data, Data}}`. Dropping it was
    /// the defect: the reply never reached the owner and `port_info(Port, input)` stayed `0`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port this registry does not hold (closed, or never
    /// opened) — the oracle's own reason for `port_command` on a dead port;
    /// [`BeamError::Io`] when the write itself fails (a child that closed its stdin yields
    /// `EPIPE`).
    pub fn write_command(&mut self, id: PortId, bytes: &[u8]) -> Result<()> {
        let Some(meta) = self.meta.get_mut(&id.number()) else {
            return Err(BeamError::BadArg);
        };
        // Every byte handed to a port counts as **output** for `erlang:statistics(io)`,
        // whichever stream it goes to (the counters are bumped before the branch so a
        // driver port and a child/`{fd,N}` port are counted alike). The port's **own**
        // `port_info(Port, output)` total is bumped at the same instant, so the two agree by
        // construction.
        self.io_counters.note_output(bytes.len());
        meta.output.set(meta.output.get().saturating_add(bytes.len() as u64));
        // A **C driver** port has no child pipe: `port_command` *is* the driver's own `output`
        // callback. `beam_io::PortRegistry::output` runs it inline, and the bytes the driver
        // answers with through `driver_output` are queued as this port's input for the next
        // [`PortRegistry::pump`] — see [`PortRegistry::take_driver_outputs`].
        if self
            .meta
            .get(&id.number())
            .map(|meta| meta.kind.bytes_from_driver_output())
            .unwrap_or(false)
        {
            self.registry.output(&mut self.poller, id, bytes)?;
            return Ok(());
        }
        // A `{packet, N}` port's stream is framed by the emulator: the length header is
        // prepended to this payload here, **before** the bytes reach `beam_io`, exactly as the
        // reference C's sys driver does in its `output`/`outputv` callbacks
        // (`sys_drivers.c:1144-1160`, `:1062-1077`). The counters above were bumped with the
        // **payload** length, which is what the oracle's own `port_info(Port, output)` reports
        // for a framed port (measured: 3, not 7, for a 3-byte payload on `{packet,4}`).
        let wire: Cow<'_, [u8]> = match self.meta.get(&id.number()).and_then(|meta| meta.packet.as_ref())
        {
            Some(framing) => Cow::Owned(framing.frame_output(bytes)?),
            None => Cow::Borrowed(bytes),
        };
        self.registry
            .port_command(&mut self.poller, id, &wire, PortCommandOptions::EMPTY)?;
        // `port_command` only queues the bytes and schedules the task; run it now so the
        // write really happened.
        let outcome = self
            .registry
            .execute_port_tasks(&mut self.poller, PORT_REDS_EXECUTE)?;
        // The round harvested the replies the drivers just wrote (and every other live port's
        // buffered input along with them — a round drains them all), so its notifications are
        // **retained** here for the next pump: the queue they came from is already empty.
        let mut events = Vec::new();
        self.fold_notifications(outcome.notifications, &mut events)?;
        self.pending_events.extend(events);
        Ok(())
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
        self.registry.port_control(&mut self.poller, id, op, arg)
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
    /// The C reads the port's `control_flags` **after** the driver's `control` callback and renders
    /// the reply as a binary when that bit is set, as a list of byte integers otherwise
    /// (`write_port_control_result`, `io.c:4133-4175`). Call this after
    /// [`PortRegistry::port_control`], which is when the driver has set its flags.
    ///
    /// `false` for an unknown port and for every port without a C driver (a `spawn`/`fd` port),
    /// whose `control` reply is the oracle's `badarg` in the first place.
    pub fn control_binary(&self, id: PortId) -> bool {
        self.meta
            .get(&id.number())
            .and_then(|meta| meta.driver_control_flags.as_ref())
            .is_some_and(|flags| flags.get() & PORT_CONTROL_FLAG_BINARY != 0)
    }

    /// Whether `id`'s port is currently **busy** — the state the driver last reported through
    /// `set_busy_port` (the C's `prt->sched.flags & ERTS_PTS_FLG_BUSY_PORT`, `io.c:5208-5264`).
    ///
    /// `erlang:port_command/2,3` consults it exactly as `erts_port_output` does: a busy port answers
    /// `false` for `nosuspend`, suspends the caller for a plain command, and lets `force` through —
    /// when the driver entry advertises `ERL_DRV_FLAG_SOFT_BUSY`
    /// ([`PortRegistry::soft_busy`]).
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
    /// question — a signal self-pipe or an armed `tty_select/2` interest can be waited on with
    /// zero ports — so a caller deciding whether a blocked process can still be woken asks
    /// [`PortRegistry::has_nothing_to_poll`].
    pub fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }

    /// Whether there is nothing a poll round must **wait on**, and nothing left that could wake
    /// a blocked process: no live port, no armed `tty_select/2` interest, and no **fired**
    /// `os:set_signal/2` self-pipe (see [`watch`]).
    ///
    /// This is the "nothing pending" predicate. It is what tells a caller that a blocked process
    /// can no longer be woken and that a poll round is pure sleep: a live port can deliver
    /// `{Port, {data, …}}`, an armed `tty_select/2` can deliver
    /// `{select, Handle, Ref, ready_input}`, and a self-pipe **holding bytes** can deliver
    /// `{notify, Signal}` — any one of them keeps the runtime awake.
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
            && !self.tty.is_armed()
            && !self.watch.has_pending_signal()
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
    /// seen before the status of the child that wrote it.    ///
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
        // The driver ports' bytes may already be queued (a `port_command` on a driver port ran
        // the driver's `output` callback inline), so they are harvested before any wait too —
        // together with the terms a driver's callback emitted (`erl_drv_output_term`).
        events.extend(self.take_driver_outputs());
        events.extend(self.take_driver_terms());
        // A signal handled before this round — or while the runtime was not polling — is
        // recorded in the facility's own bitmask, so it is drained unconditionally rather than
        // keyed to the readiness event.
        events.extend(self.take_signals());
        // Only a *live* wake source is worth blocking on. The signal self-pipe is deliberately
        // excluded even though it is registered: `take_signals` above has already harvested any
        // signal that was caught, so a *pending* one must return at once rather than poll a
        // now-drained pipe, and an *idle* one can only become readable when a signal arrives —
        // blocking on it would be waiting for nothing. A live port or an armed `tty_select/2`
        // interest is real async I/O and is still waited on.
        if self.registry.is_empty() && !self.tty.is_armed() {
            // A close `beam_io` has not reported yet still owes its `'EXIT'`/`'DOWN'`: minting
            // the report is what retires the port's number and releases them, and with no
            // live port left nothing else would ever run the round that mints it.
            self.report_pending_closes(&mut events)?;
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
            // A `tty_select/2` read interest is the other non-port registration: its readiness
            // is a one-shot `enif_select` report, so each ready key yields exactly one
            // `{select, …}` event and is de-registered as it is reported (see [`tty`]). The
            // reported handle/reference were stabilised when the select was armed, so they are
            // valid across this pass.
            let tty_keys: Vec<u64> = polled
                .iter()
                .map(|event| event.key())
                .filter(|key| self.tty.owns(*key))
                .collect();
            for (owner, handle, reference) in self.tty.take_ready(&mut self.poller, &tty_keys)? {
                events.push(PortEvent::Select {
                    owner,
                    handle,
                    reference,
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
                    .filter(|event| event.key() != watch::SIGNAL_KEY && !self.tty.owns(event.key()))
                    .collect();
                if !port_events.is_empty() {
                    self.registry.schedule_events(&port_events);
                }
            }
            let outcome = self
                .registry
                .execute_port_tasks(&mut self.poller, PORT_REDS_EXECUTE)?;
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
            // appending the status after the frames this very round just flushed — the two
            // orderings a naive round would race between, depending on whether the child happened
            // to be reaped before the poll or during it.
            events.extend(self.take_due_exit_statuses()?);
            // The end-of-stream flush releases a trailing partial line as a `noeol` frame (the
            // C's `driver_eof` → `flush_linebuf_messages`).
            self.flush_line_stream_ends()?;
            for frame in self.registry.take_line_notifications() {
                // A frame for a port whose metadata is gone (closed while the task was
                // queued) has no recipient and is dropped — the rule the raw `Data` arm
                // below follows too.
                if let Some(meta) = self.meta.get(&frame.port.number()) {
                    // Framed bytes are **input** for `erlang:statistics(io)`, counted where
                    // they really arrive; a blank line contributes zero bytes, exactly as
                    // the C's per-read accounting does.
                    self.io_counters.note_input(frame.bytes.len());
                    // ...and for this port's own `port_info(Port, input)`.
                    meta.input
                        .set(meta.input.get().saturating_add(frame.bytes.len() as u64));
                    events.push(PortEvent::Line {
                        port: frame.port,
                        bytes: frame.bytes,
                        eol: frame.eol,
                        stream: meta.stream,
                        owner: meta.owner,
                    });
                }
            }
            self.fold_notifications(outcome.notifications, &mut events)?;
            events.extend(self.take_driver_outputs());
            events.extend(self.take_driver_terms());
            events.extend(self.take_due_exit_statuses()?);
            if !events.is_empty() || Instant::now() >= deadline {
                return Ok(events);
            }
        }
    }

    /// The `{Port, {data, Data}}` events the **`dlopen`ed C driver** ports have produced.
    ///
    /// A `{spawn_driver, Name}` port backed by a loaded `.so` gets its bytes from the driver's
    /// `output` callback answering through `driver_output`; [`driver::DriverPortAdapter`] queues
    /// them on the port's input queue, and this drains it. A `spawn`/`fd` port's input queue, and a
    /// **built-in** driver port's ([`PortName::SpawnDriverBuiltin`], whose bytes come from its
    /// poller-registered descriptor), are drained by the port-task round instead
    /// ([`beam_io::Port::take_input_notification`], whose `INPUT` task their registration makes
    /// run), so the two never report the same bytes.
    ///
    /// # Errors
    ///
    /// None: a port that has gone away (closed while the bytes were queued) has no recipient and
    /// its bytes are dropped, exactly as the task round drops a notification for a closed port.
    fn take_driver_outputs(&mut self) -> Vec<PortEvent> {
        let numbers: Vec<u64> = self
            .meta
            .iter()
            .filter(|(_, meta)| meta.kind.bytes_from_driver_output())
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
                // ...and for this port's own `port_info(Port, input)`.
                meta.input.set(meta.input.get().saturating_add(bytes.len() as u64));
                events.push(PortEvent::Data {
                    port,
                    bytes,
                    stream: meta.stream,
                    // A `dlopen`ed driver's bytes come from its own `output` callback through the
                    // driver's input queue, and the `beam_abi` host seam does not record **which**
                    // `driver_output*` entry point it called — so a `.so` driver's payload is
                    // indistinguishable from `driver_output`'s header-less form, which is a binary
                    // on a `binary` port. Deliberately unchanged: it is what its corpus cases pin.
                    shape: StreamShape::Data,
                    owner: meta.owner,
                });
            }
        }
        events
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
                    let bytes = read.flattened();
                    if bytes.is_empty() {
                        continue;
                    }
                    // A `{packet, N}` port reassembles the child's stream into whole packets: the
                    // bytes are fed to the port's own framer and **one** message is produced per
                    // completed packet (see [`packet`]). This is the C's `ready_input` packet arm
                    // (`sys_drivers.c:1393-1447`), including its two edge cases: a packet split
                    // across several reads is buffered until it completes, and a **zero-length**
                    // packet is a real `{Port,{data,<<>>}}` message (which is why the empty-payload
                    // drop above is checked before this arm and not inside it).
                    let framed = {
                        let mut framed = None;
                        if let Some(meta) = self.meta.get_mut(&port.number()) {
                            if let Some(framing) = meta.packet.as_mut() {
                                // The framer's refusal (a buffer that cannot grow) is the C's
                                // `ENOMEM` port death; it is reported, never papered over.
                                framed = Some((framing.push(&bytes)?, meta.stream, meta.owner));
                            }
                        }
                        framed
                    };
                    if let Some((packets, stream, owner)) = framed {
                        for payload in packets {
                            // A framed payload is **input** for `erlang:statistics(io)` and for
                            // this port's own `port_info(Port, input)`, and the header is not
                            // counted — the oracle reports the delivered bytes (measured: 3 for a
                            // 7-byte `{packet,4}` wire packet, and 0 for an empty one).
                            self.io_counters.note_input(payload.len());
                            if let Some(meta) = self.meta.get(&port.number()) {
                                meta.input
                                    .set(meta.input.get().saturating_add(payload.len() as u64));
                            }
                            events.push(PortEvent::Data {
                                port,
                                bytes: payload,
                                stream,
                                // A framed payload is the child's own byte stream (the framing is
                                // the emulator's, [`packet`]), never a driver reply.
                                shape: StreamShape::Data,
                                owner,
                            });
                        }
                        continue;
                    }
                    if let Some(meta) = self.meta.get(&port.number()) {
                        // Bytes a port produced are **input** for `erlang:statistics(io)`,
                        // counted where they really arrive.
                        self.io_counters.note_input(bytes.len());
                        // ...and for this port's own `port_info(Port, input)`.
                        meta.input.set(meta.input.get().saturating_add(bytes.len() as u64));
                        // A **built-in** driver port's bytes are an in-tree driver's reply, and the
                        // C renders a driver output call's header as list elements on either
                        // stream — see [`StreamShape::DriverOutput`]. Everything else on this
                        // round is a byte stream's own payload (a child's stdout, an `fd` port),
                        // which keeps the stream option's shape.
                        let shape = match meta.kind {
                            PortName::SpawnDriverBuiltin => StreamShape::DriverOutput,
                            PortName::SpawnDriver | PortName::Spawn | PortName::SpawnExecutable
                            | PortName::Fd => StreamShape::Data,
                        };
                        events.push(PortEvent::Data {
                            port,
                            bytes,
                            stream: meta.stream,
                            shape,
                            owner: meta.owner,
                        });
                    }
                }
                PortNotification::Closed { port } => {
                    // The port has genuinely left the runtime, so the `'EXIT'`/`'DOWN'`
                    // messages its close owed its links and monitors are released for the
                    // delivery routes to take (see [`exit`]).
                    self.pending_port_signals.report(port);
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
    /// Nothing is re-delivered: the bytes left [`beam_io`]'s input queue when they were harvested,
    /// so this is the **only** report they can still have.
    fn drain_pending_events(&mut self) -> Vec<PortEvent> {
        std::mem::take(&mut self.pending_events)
            .into_iter()
            .filter(|event| match event.port() {
                Some(port) => self.meta.contains_key(&port.number()),
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
            // [`driver_lookup`] applies to build the `.so` candidates.
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
            // [`PortName::SpawnDriverBuiltin`] — a driver port for `port_control`, but not for
            // [`PortRegistry::take_driver_outputs`].
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
                    driver_monitors: None,
                    input: Cell::new(0),
                    output: Cell::new(0),
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
            .with_liveness(registry.liveness());
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
                driver_monitors: Some(driver_monitors),
                input: Cell::new(0),
                output: Cell::new(0),
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
            driver_monitors: None,
            input: Cell::new(0),
            output: Cell::new(0),
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
            // A `prim_tty` reader that died without disarming its `tty_select/2` interest would
            // otherwise keep the runtime polling for ever (see [`PortRegistry::prune_dead_tty`]).
            registry.prune_dead_tty(|pid| table.contains(pid))?;
            registry.pump(timeout)?
        };
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
                        shape,
                        owner,
                    } => (
                        *owner,
                        port.to_eterm(),
                        ctx.port_data_message_shaped(*port, bytes, *stream, *shape)?,
                    ),
                    // A line-framed read is the same `{Port, {data, …}}` delivery with the
                    // `{eol|noeol, Data}` body the `line` option selects.
                    PortEvent::Line {
                        port,
                        bytes,
                        eol,
                        stream,
                        owner,
                    } => (
                        *owner,
                        port.to_eterm(),
                        ctx.port_line_message(*port, bytes, *eol, *stream)?,
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
                    // A ready `tty_select/2` interest is addressed directly to the process that
                    // armed it (`enif_select`'s implicit recipient, recorded as `owner`), with the
                    // emulator (`system`) as the sender — the ordinary one-shot select delivery.
                    PortEvent::Select {
                        owner,
                        handle,
                        reference,
                    } => {
                        let (from, message) = ctx.tty_select_message(*handle, *reference)?;
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
    /// capability gap rather than a panic; the message builders' allocation failure when a
    /// term does not fit.
    pub(crate) fn poll_port_messages(&mut self, timeout: Duration) -> Result<Vec<Eterm>> {
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
            // Same as the scheduler path's round: a dead `prim_tty` reader's armed select is
            // dropped before the poll (see [`PortRegistry::prune_dead_tty`]).
            registry.prune_dead_tty(|pid| table.contains(pid))?;
            registry.pump(timeout)?
        };
        let mut messages = Vec::with_capacity(events.len());
        for event in events {
            match event {
                PortEvent::Data {
                    port,
                    bytes,
                    stream,
                    shape,
                    ..
                } => messages.push(self.port_data_message_shaped(port, &bytes, stream, shape)?),
                // The `line` option's framed shape (see [`PortEvent::Line`]); the evaluator is
                // the port's owner on this path, so the message is its own.
                PortEvent::Line {
                    port,
                    bytes,
                    eol,
                    stream,
                    ..
                } => messages.push(self.port_line_message(port, &bytes, eol, stream)?),
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
                // A handled signal is addressed to the `erl_signal_server` process, and the
                // evaluator is not it: a `RuntimeContext` can only observe its **own** mailbox, so
                // the notification is not returned here — the scheduler path
                // ([`Runtime::deliver_port_messages`]) delivers it into that process's mailbox
                // instead — rather than being matched as if it were the evaluator's own. Same
                // reasoning as the driver-term arm above.
                PortEvent::Signal { .. } => continue,
                // A ready `tty_select/2` interest belongs in the **owning reader's** mailbox, which
                // the evaluator is not: it can only observe its own mailbox, so — like the signal
                // arm — the event is not returned here and the scheduler path delivers it.
                PortEvent::Select { .. } => continue,
            }
        }
        // A port close owes `'EXIT'`/`'DOWN'` messages to its links and monitors; the ones
        // addressed to the evaluator itself are returned for its `receive` to match, and every
        // other recipient's stay queued for the scheduler route that owns a process table (see
        // [`exit`]).
        messages.extend(self.poll_port_signals()?);
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
    /// `Data` is an Erlang **binary** for a header-less read on a `binary` stream and a char list
    /// otherwise (the `list` default, `erl`'s own). A driver output call that carried a header
    /// renders the C's `[H1..Hh | <<Data>>]` shapes instead, which is what
    /// [`Self::port_data_message_shaped`] is for; this entry point is the split-less form
    /// ([`StreamShape::Data`]). The `data` atom is interned into the runtime's one shared table,
    /// so the message is nameable by the printer and matchable by a `receive` clause.
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
    // [`Self::port_data_message_shaped`] with the event's own shape; this stays as the split-less
    // convenience the provider's own suite builds plain byte-stream messages with.
    #[allow(dead_code)]
    pub(crate) fn port_data_message(
        &mut self,
        port: PortId,
        bytes: &[u8],
        stream: StreamType,
    ) -> Result<Eterm> {
        self.port_data_message_shaped(port, bytes, stream, StreamShape::Data)
    }

    /// [`Self::port_data_message`] for a driver output call whose **split** is known (or known to
    /// be unknown).
    ///
    /// `shape` is the [`StreamShape`] of the call that produced `bytes`; see that type for the
    /// C's four output entry points and the term each one renders. The `list` stream ignores it
    /// (header and data are both list elements there), so this only changes a `binary` port. Both
    /// delivery routes render a [`PortEvent::Data`] through this entry point with the event's own
    /// `shape`, so a built-in driver port's replies ([`StreamShape::DriverOutput`]) and a child's
    /// byte stream ([`StreamShape::Data`]) reach an owner as the C's own terms.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the term does not fit.
    pub(crate) fn port_data_message_shaped(
        &mut self,
        port: PortId,
        bytes: &[u8],
        stream: StreamType,
        shape: StreamShape,
    ) -> Result<Eterm> {
        let data = make_atom(self.runtime.code_server.atoms_mut().intern("data"));
        let payload = self.make_stream_payload(bytes, stream, shape)?;
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
    /// partial line released at end-of-stream. `Data` is shaped by `stream` exactly as
    /// [`Self::port_data_message`] shapes an unframed read — the split-less [`StreamShape::Data`]
    /// form, because a frame's bytes come from [`beam_io`]'s line accumulator rather than from one
    /// driver output call (see [`StreamShape`]).
    ///
    /// Unlike the unframed builder this one is reachable with an **empty** `bytes`: a blank
    /// line is the oracle's `{Port, {data, {eol, []}}}` (and an empty `noeol` frame is
    /// possible when a flush releases nothing), so the payload is built even for zero bytes.
    /// The `data`/`eol`/`noeol` atoms are interned into the runtime's **one shared** table, so
    /// the message is nameable by the printer and matchable by a `receive` clause, and it is
    /// built on the same pointer-stable `-eval` arena as the unframed builder.
    pub(crate) fn port_line_message(
        &mut self,
        port: PortId,
        bytes: &[u8],
        eol: bool,
        stream: StreamType,
    ) -> Result<Eterm> {
        let tag = if eol { "eol" } else { "noeol" };
        let (data, tag) = {
            let atoms = self.runtime.code_server.atoms_mut();
            (make_atom(atoms.intern("data")), make_atom(atoms.intern(tag)))
        };
        let payload = self.make_stream_payload(bytes, stream, StreamShape::Data)?;
        let framed = terms::make_tuple(self, &[tag, payload])?;
        let inner = terms::make_tuple(self, &[data, framed])?;
        terms::make_tuple(self, &[port.to_eterm(), inner])
    }

    /// The `Data` **payload** of a port-stream message, shaped by the port's stream option and by
    /// the driver output call that produced the bytes.
    ///
    /// Shared by [`Self::port_data_message_shaped`] and [`Self::port_line_message`] so the message
    /// shapes can never drift. The legacy C renders every delivered chunk in two parts — the
    /// driver's **header** bytes as list elements and its **data** bytes as the stream asks
    /// (`deliver_read_message`, `erts/emulator/beam/io.c:3404-3435`, and `deliver_vec_message`,
    /// `:3559-3617`) — so:
    ///
    /// * a `list` stream is a char list of **all** the bytes, whatever the split;
    /// * a `binary` stream is a bare binary for a header-less call ([`StreamShape::Data`]) — built
    ///   by [`beam_codec::term::make_binary`], see [`Self::port_data_message`] on the width;
    /// * a `binary` stream is a **proper list** of the byte integers for a header-only call
    ///   ([`StreamShape::Header`]: `driver_output2(hbuf, hlen, NULL, 0)`, what a `ram_file_drv`
    ///   `OK`/`NUMBER`/`ERROR` reply makes) and for a **built-in driver port**'s payload
    ///   ([`StreamShape::DriverOutput`], whose split the seam does not carry — see that variant);
    /// * a `binary` stream is the **improper** list `[H1..Hh | <<Data>>]` for a header-plus-data
    ///   call ([`StreamShape::HeaderThenData`]), an empty data part rendering `[...|<<>>]` (a
    ///   zero-length `DATA` reply, i.e. a read at end-of-file).
    ///
    /// Every block is allocated on the runtime's pointer-stable `-eval` arena.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the term does not fit.
    fn make_stream_payload(
        &mut self,
        bytes: &[u8],
        stream: StreamType,
        shape: StreamShape,
    ) -> Result<Eterm> {
        if stream == StreamType::List || shape == StreamShape::DriverOutput {
            // The header is list elements too, so the split is invisible on this stream — and a
            // built-in driver port's payload is rendered as the C's header half on **either**
            // stream, its split being unknown (see [`StreamShape::DriverOutput`]).
            return self.bytes_to_list(bytes, NIL);
        }
        let (header, data) = shape.split(bytes);
        match data {
            // No data part: the whole reply is the (already-complete) header list.
            None => self.bytes_to_list(header, NIL),
            // Otherwise the data part is the term's tail — a binary, empty or not — and the
            // header bytes are consed in front of it (an empty header leaves the binary bare,
            // the header-less `driver_output` shape).
            Some(data) => {
                let tail = beam_codec::term::make_binary(self.eval_heap(), data)?;
                self.bytes_to_list(header, tail)
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

    /// The `{select, Handle, Reference, ready_input}` message a ready `tty_select/2` interest
    /// produces, and its sender — the delivery half of the `enif_select` a `prim_tty` reader
    /// arms.
    ///
    /// ERTS queues exactly this 4-tuple to the **calling** process, with `am_system` as the
    /// sender: `prepare_select_msg` (`erts/emulator/sys/common/erl_check_io.c:731-761`) builds
    /// it from the select's resource term and reference term and always closes with the
    /// `ready_input` event atom, and `send_select_msg` (`:836-858`) queues it with `system` as
    /// the sender. `handle` is the opaque tty term ([`crate::native::prim_tty`]'s reference) and
    /// `reference` is `prim_tty:reader/1`'s `ReaderRef`; `prim_tty:reader_loop/2`
    /// (`lib/kernel/src/prim_tty.erl:454-482`) matches the message on both.
    ///
    /// Both terms are already **stabilized** (see [`PortRegistry::arm_tty_select`]), so this
    /// builder copies them into the tuple without re-homing them; the `select`/`ready_input`/
    /// `system` atoms are interned in the runtime's **one shared** table.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the tuple does not fit.
    pub(crate) fn tty_select_message(
        &mut self,
        handle: Eterm,
        reference: Eterm,
    ) -> Result<(Eterm, Eterm)> {
        let atoms = self.runtime.code_server.atoms_mut();
        let select = make_atom(atoms.intern("select"));
        let ready_input = make_atom(atoms.intern("ready_input"));
        let sender = make_atom(atoms.intern("system"));
        let message = terms::make_tuple(self, &[select, handle, reference, ready_input])?;
        Ok((sender, message))
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
    let path = driver_path_name(name);
    let mut candidates = vec![path.to_string(), format!("{path}.so")];
    let driver_name = if path.contains('/') {
        let base = std::path::Path::new(path)            .file_name()
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
