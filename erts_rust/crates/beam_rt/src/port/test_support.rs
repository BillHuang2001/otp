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

//! Test-only helpers for the port provider's own suite and for the native floor's port tests.
//!
//! Two things a port unit test cannot otherwise build are provided here once rather than
//! re-derived per test module (Prevent-Duplication):
//!
//! * [`ControlDriver`] — a minimal, always-ready [`Driver`] whose `control` callback answers a
//!   configured byte reply per verb, exactly as a real C driver's `control` slot would, **and**
//!   reports the reply-buffer shape that verb declares — [`ControlVerb::null`] is a `NULL` reply
//!   pointer (`beam_io::ControlReplyShape::Null`), the case a zero-byte buffer cannot stand in for.
//!   It is the safe-Rust stand-in for the C fixtures `src/port/driver/control_tests.rs` compiles,
//!   for tests that must reach [`PortRegistry::port_control`] through a **registered** port rather
//!   than through `beam_io`'s own registry.
//! * [`register_control_port`] — registers such a port into a [`Runtime`]'s port table (the
//!   provider's private `register`, reachable because this module is a descendant of the module
//!   that defines it) and hands back the id and the port's shared control-flag cell, so a test can
//!   drive `port_control` and observe [`PortRegistry::control_binary`] **and**
//!   [`PortRegistry::control_reply_shape`] without an OS child, a poll round or a C compiler.
//!
//! Nothing here fabricates a port lifecycle: `start`/`output`/`ready_input` are the trait's own
//! no-ops, so a registered port is genuinely live, registered with the poller and torn down by
//! [`PortRegistry::close`] like any other.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_io::{Driver, DriverContext};
use beam_process::{PortId, ProcessId};

use super::driver::BusyFlag;
use super::{PortMeta, PortName, StreamType};
use crate::Runtime;

/// The pid ports registered by the tests belong to (the runtime's root process shape).
const OWNER: ProcessId = ProcessId::new(1, 0);

/// One verb [`ControlDriver`] answers: the reply bytes and, optionally, the control-flag bits the
/// driver asks for **from inside the callback** — the C's `set_port_control_flags`
/// (`io.c:5318`), recorded in the port's shared cell.
///
/// It also carries how the driver describes its **reply buffer**
/// ([`beam_io::ControlReplyShape`]), which a C driver controls by leaving `*rbuf` `NULL` or not.
/// That is a second fact beside the bytes: `write_port_control_result` tests the pointer *before*
/// the port's `PORT_CONTROL_FLAG_BINARY` word (`io.c:4134`), so a `NULL` reply is the empty list
/// for every flag setting while a zero-byte buffer under that flag is `<<>>`.
pub(crate) struct ControlVerb {
    /// The verb (`command`) the driver recognises, as the decimal `c_uint` the runtime parses.
    pub(crate) command: u32,
    /// The bytes the driver replies with (an empty vector models a driver that wrote nothing).
    pub(crate) reply: Vec<u8>,
    /// The bits to record in the port's control-flag cell as this callback runs, if any — the
    /// C's `set_port_control_flags(port, flags)`.
    pub(crate) set_flags: Option<libc::c_int>,
    /// How this verb describes its reply buffer: `Null` models a driver that leaves `*rbuf` NULL
    /// (`io.c:4134`, the empty list for every flag word), `Buffer` a driver that answers from a
    /// real buffer (the shape every [`ControlVerb::replying`]/[`ControlVerb::setting`] reply has).
    pub(crate) reply_shape: beam_io::ControlReplyShape,
    /// Whether serving this verb **clears the port's busy bit** — the C fixture
    /// `tests/fixtures/busy_driver.c`'s `$N` verb, which calls `set_busy_port(port, 0)` from inside
    /// its `control` callback (`io.c:5261-5262`, the `erts_port_resume_procs` side of the clear).
    /// A real driver is the only thing that can clear the bit a `set_busy_port(port, 1)` set, so a
    /// test that wants a busy port to go idle *from Erlang code* (`port_control(P, $N, <<>>)`) needs
    /// this side effect on the verb (see [`ControlVerb::clearing`]).
    pub(crate) clears_busy: bool,
}

impl ControlVerb {
    /// A verb answering `reply` and leaving the port's control flags alone.
    pub(crate) fn replying(command: u32, reply: &[u8]) -> ControlVerb {
        ControlVerb {
            command,
            reply: reply.to_vec(),
            set_flags: None,
            reply_shape: beam_io::ControlReplyShape::Buffer,
            clears_busy: false,
        }
    }

    /// A verb answering `reply` **and** setting `flags` on the port, as a C driver's `control`
    /// slot does through `set_port_control_flags`.
    pub(crate) fn setting(command: u32, reply: &[u8], flags: libc::c_int) -> ControlVerb {
        ControlVerb {
            command,
            reply: reply.to_vec(),
            set_flags: Some(flags),
            reply_shape: beam_io::ControlReplyShape::Buffer,
            clears_busy: false,
        }
    }

    /// A verb that answers **no bytes** and clears the port's busy bit — the C fixture
    /// `tests/fixtures/busy_driver.c`'s `$N` (`'N'`, 78) verb, which is `set_busy_port(port, 0)`
    /// behind a `0` reply (`io.c:5261-5262`). This is what lets Erlang code
    /// (`erlang:port_control(P, $N, <<>>)`) make a busy port go idle, which is exactly the
    /// mechanism the C's suspended caller waits for.
    pub(crate) fn clearing(command: u32) -> ControlVerb {
        ControlVerb {
            command,
            reply: Vec::new(),
            set_flags: None,
            reply_shape: beam_io::ControlReplyShape::Buffer,
            clears_busy: true,
        }
    }

    /// A verb that answers a **`NULL` reply pointer** — the C's `NIL` (`io.c:4126`), which
    /// `write_port_control_result` renders as the bare empty list `[]` for **every**
    /// `PORT_CONTROL_FLAG_BINARY` setting (`io.c:4134`). No bytes are returned and no flag is set:
    /// a `NULL` reply carries neither, and the port's flags are per-port state a previous verb may
    /// already have set.
    pub(crate) fn null(command: u32) -> ControlVerb {
        ControlVerb {
            command,
            reply: Vec::new(),
            set_flags: None,
            reply_shape: beam_io::ControlReplyShape::Null,
            clears_busy: false,
        }
    }
}

/// A minimal, always-ready driver whose `control` callback answers one of `verbs`.
///
/// An **unknown** verb is [`BeamError::BadArg`], the C's own answer for a command the driver
/// rejects (a negative `cres`, `io.c:4074`), so a test can tell "the driver saw the verb" from
/// "the runtime substituted one".
///
/// It also carries the two facts `beam_io`'s `port_command` decision reads off the **driver**
/// ([`Driver::is_busy`] and [`Driver::flags`]) when the caller supplied them, forwarded from the
/// very [`BusyFlag`] the port's metadata holds — exactly as
/// [`DriverPortAdapter`](super::driver::DriverPortAdapter) forwards its own
/// (`super::driver::DriverPortAdapter::is_busy`/`flags`). Without that, a fixture port could look
/// busy to the runtime's metadata and idle to the port layer's decision table.
pub(crate) struct ControlDriver {
    /// The registered driver name (the trait requires one; nothing looks it up here).
    name: &'static str,
    /// The verbs this driver recognises.
    verbs: Vec<ControlVerb>,
    /// The port's control-flag cell, shared with the [`PortMeta`] the runtime reads after the
    /// callback — the C's `prt->control_flags`.
    flags: Rc<Cell<libc::c_int>>,
    /// The port's busy state and its entry's `ERL_DRV_FLAG_SOFT_BUSY` claim, or `None` for a
    /// fixture that is never busy (the state every port no driver has reported on is in).
    busy: Option<BusyFlag>,
    /// How many times the driver's `output` callback ran — the fixture's evidence that a refused
    /// or dropped command reached **no write** at all.
    writes: Rc<Cell<usize>>,
    /// How this fixture described the reply buffer of its last **successful** `control` call — the
    /// C's `*rbuf` pointer, reported through [`Driver::control_reply_shape`] so the runtime can
    /// render a `NULL` reply as `[]` even on a `PORT_CONTROL_FLAG_BINARY` port (`io.c:4134`).
    ///
    /// A plain [`Cell`] because it is the driver's own private state: the runtime reads it back
    /// through the very `beam_io::Driver` it registered, exactly as it reads the C driver's
    /// pointer from the loaded adapter. An unknown (rejected) verb leaves it alone — it belongs to
    /// the last successful call.
    reply_shape: Cell<beam_io::ControlReplyShape>,
}

impl Driver for ControlDriver {
    fn name(&self) -> &str {
        self.name
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.writes.set(self.writes.get() + 1);
        Ok(0)
    }

    /// The port's busy state as its driver last reported it (`set_busy_port`) — the C's
    /// `ERTS_PTS_FLG_BUSY_PORT`, which `beam_io::Port::command` reads to answer
    /// `ERTS_PORT_OP_BUSY` (`io.c:1749-1769`).
    fn is_busy(&self) -> bool {
        self.busy.as_ref().is_some_and(BusyFlag::is_busy)
    }

    /// The driver entry's capability flags in `beam_io`'s vocabulary: this fixture advertises
    /// `ERL_DRV_FLAG_SOFT_BUSY` when the caller said so, and nothing otherwise — the same one-bit
    /// mapping `DriverPortAdapter::flags` performs from a loaded entry's `driver_flags`.
    fn flags(&self) -> beam_io::DriverFlags {
        let flags = beam_io::DriverFlags::empty();
        if self.busy.as_ref().is_some_and(BusyFlag::soft_busy) {
            flags.union(beam_io::DriverFlags::SOFT_BUSY)
        } else {
            flags
        }
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        // No resources to release: the fixture owns no descriptor, no child and no timer.
        Ok(())
    }
    fn control(
        &mut self,
        _ctx: &mut DriverContext<'_>,
        command: &str,
        _arg: &[u8],
    ) -> Result<Vec<u8>> {
        let Ok(verb) = command.parse::<u32>() else {
            // The C's `call_driver_control` parses the verb as a `c_uint` before the callback; a
            // verb that is not one never reaches the driver. Here the runtime has already parsed
            // it, so a non-numeric command means the wiring is wrong — still `badarg`, never a
            // substituted command.
            return Err(BeamError::BadArg);
        };
        match self.verbs.iter().find(|entry| entry.command == verb) {
            Some(entry) => {
                if let Some(bits) = entry.set_flags {
                    self.flags.set(bits);
                }
                // The C's `$N`: a driver clears the port's own busy bit from inside its `control`
                // callback (`set_busy_port(port, 0)`, `io.c:5261-5262`). The handle is the same
                // one the port's metadata reads (`PortMeta::driver_busy`), so the clear is visible
                // to `PortRegistry::is_busy` — and therefore to the next command attempt.
                if entry.clears_busy {
                    if let Some(busy) = &self.busy {
                        busy.set_busy(false);
                    }
                }
                // The reply buffer this verb answers with — the fact the bytes cannot carry
                // (a `NULL` reply and an empty buffer are both zero bytes, but the C renders the
                // former as `[]` whatever the port's binary flag says, `io.c:4134`).
                self.reply_shape.set(entry.reply_shape);
                Ok(entry.reply.clone())
            }
            None => Err(BeamError::BadArg),
        }
    }

    /// How the last successful `control` call described its reply buffer — what a C driver's
    /// `*rbuf` pointer says through [`DriverPortAdapter`](super::driver::DriverPortAdapter), and
    /// the fact the runtime's `erlang:port_control/3` renders `[]` from when it is
    /// [`Null`](beam_io::ControlReplyShape::Null) (`io.c:4134`).
    ///
    /// The trait default ([`Buffer`](beam_io::ControlReplyShape::Buffer)) is the starting point:
    /// no verb has run yet.
    fn control_reply_shape(&self) -> beam_io::ControlReplyShape {
        self.reply_shape.get()
    }
}

/// Registers a [`ControlDriver`] as a live `{spawn_driver, _}`-kind port on `runtime`'s port
/// table and returns its id together with the port's shared control-flag cell.
///
/// The id is a real one: [`super::PortRegistry::register`] allocates it from `beam_io`'s registry
/// (so `is_live`, `close` and the metadata all agree), which is what makes the returned port
/// indistinguishable from one an `open_port/2` produced as far as
/// [`RuntimePorts::port_control`](crate::port::RuntimePorts::port_control) is concerned.
///
/// # Panics
///
/// The runtime's port table is the test's own, unshared state: a borrow that fails there is a
/// broken fixture, and a registration that fails means the id space is exhausted — neither is a
/// runtime condition a test should paper over.
pub(crate) fn register_control_port(
    runtime: &mut Runtime,
    name: &'static str,
    verbs: Vec<ControlVerb>,
) -> (PortId, Rc<Cell<libc::c_int>>) {
    let (id, flags, _writes) = register_driver_port(runtime, name, verbs, None, None);
    (id, flags)
}

/// Registers a [`ControlDriver`] as a live `{spawn_driver, _}`-kind port that reports the **busy**
/// state `busy`, and returns its id together with the shared [`BusyFlag`] handle — so a test can
/// toggle the state (`set_busy_port`) the way a C driver does from inside a callback — and the
/// driver's **write counter**.
///
/// The flag handle is shared by `Rc` with the registered port's metadata **and** with the driver
/// itself, exactly as [`super::driver::DriverPort::busy_flag`] is with a real C host: the runtime's
/// [`PortRegistry::is_busy`](super::PortRegistry::is_busy) reads the metadata and `beam_io`'s
/// `port_command` decision reads [`Driver::is_busy`]/[`Driver::flags`], so both see every change
/// and a test can drive the `erlang:port_command/2,3` busy arms without a C compiler.
///
/// The counter is the fixture's own proof that a refused or dropped command reached **no write**:
/// it increments once per `output` callback the port layer actually ran.
pub(crate) fn register_busy_port(
    runtime: &mut Runtime,
    name: &'static str,
    busy: bool,
    soft_busy: bool,
) -> (PortId, BusyFlag, Rc<Cell<usize>>) {
    register_busy_control_port(runtime, name, Vec::new(), busy, soft_busy)
}

/// [`register_busy_port`] **plus** the `control` verbs the driver recognises, so a test can make
/// the port go idle from **Erlang code** — `erlang:port_control(Port, $N, <<>>)` against a
/// [`ControlVerb::clearing`] verb — the way `tests/fixtures/busy_driver.c`'s `$N` does.
///
/// That is what a parked `port_command` caller waits for (`crate::port::busy_wait`): the bit is
/// cleared by *someone else* while the caller is suspended, so a test that drives the park →
/// pump → resume loop needs a clear it can reach from another process (or from a
/// `timer:apply_after/4`) rather than by touching the [`BusyFlag`] handle from Rust.
///
/// # Panics
///
/// As for [`register_control_port`].
pub(crate) fn register_busy_control_port(
    runtime: &mut Runtime,
    name: &'static str,
    verbs: Vec<ControlVerb>,
    busy: bool,
    soft_busy: bool,
) -> (PortId, BusyFlag, Rc<Cell<usize>>) {
    let flag = BusyFlag::new();
    flag.set_busy(busy);
    flag.set_soft_busy(soft_busy);
    let (id, _control_flags, writes) =
        register_driver_port(runtime, name, verbs, None, Some(flag.clone()));
    (id, flag, writes)
}

/// Registers a [`ControlDriver`] as a live `{spawn_driver, _}`-kind port that **holds the
/// driver-monitor tree** `monitors`, so a test can drive the runtime's dead-monitor sweep
/// ([`Runtime::drain_driver_monitor_exits`](crate::runtime::Runtime)) against a real port's
/// metadata without a C compiler.
///
/// The tree is the caller's own handle (shared by `Rc` with the registered port, exactly as
/// [`super::driver::DriverPort::monitors`] is with a real C host), so the caller can install
/// monitors on it and read the marks the sweep leaves.
///
/// # Panics
///
/// As for [`register_control_port`].
pub(crate) fn register_monitored_port(
    runtime: &mut Runtime,
    name: &'static str,
    monitors: Rc<super::driver::DriverMonitors>,
) -> PortId {
    let (id, _flags, _writes) =
        register_driver_port(runtime, name, Vec::new(), Some(monitors), None);
    id
}

/// The one body behind both fixtures above: a [`ControlDriver`] with `verbs` registered under
/// `name`, carrying `driver_monitors` when the caller asked for a monitor tree and `driver_busy`
/// when it asked for a controllable busy state.
///
/// A `driver_busy` handle is shared by `Rc` with **both** the driver (whose
/// `is_busy`/`flags` the port layer's decision reads) and the [`PortMeta`] (which the runtime's own
/// `is_busy`/`soft_busy` read), so the two views of one port can never disagree.
fn register_driver_port(
    runtime: &mut Runtime,
    name: &'static str,
    verbs: Vec<ControlVerb>,
    driver_monitors: Option<Rc<super::driver::DriverMonitors>>,
    driver_busy: Option<BusyFlag>,
) -> (PortId, Rc<Cell<libc::c_int>>, Rc<Cell<usize>>) {
    let flags = Rc::new(Cell::new(0));
    let writes = Rc::new(Cell::new(0));
    let driver = ControlDriver {
        name,
        verbs,
        flags: Rc::clone(&flags),
        busy: driver_busy.clone(),
        writes: Rc::clone(&writes),
        // No verb has answered yet, so there is no reply pointer to describe: the trait's own
        // `Buffer` default is the honest starting answer.
        reply_shape: Cell::new(beam_io::ControlReplyShape::Buffer),
    };
    let meta = PortMeta {
        command: name.to_string(),
        kind: PortName::SpawnDriver,
        os_pid: None,
        stream: StreamType::List,
        line: None,
        packet: None,
        notify_exit: false,
        monitors: Vec::new(),
        owner: OWNER,
        // This fixture's driver implements `control`, so the captured capability is the driver's
        // own answer — exactly what `open_port/2` captures from a real one.
        driver_has_control: true,
        // No driver interned a name and no driver host emitted a term: this fixture only drives
        // the control and monitor paths, so both handles are absent — but the control-flag cell
        // *is* shared, since that is exactly what `port_control` reads back.
        driver_terms: None,
        driver_atoms: None,
        driver_control_flags: Some(Rc::clone(&flags)),
        driver_busy,
        driver_os_pid: None,
        driver_monitors,
        input: Cell::new(0),
        output: Cell::new(0),
        exit_status_taken: Cell::new(false),
        held_lines: VecDeque::new(),
        held_since: Cell::new(None),
    };
    let mut registry = runtime
        .ports
        .try_borrow_mut()
        .expect("the port table is not borrowed");
    let id = registry
        .register(Box::new(driver), meta)
        .expect("the fixture port registers");
    (id, flags, writes)
}
