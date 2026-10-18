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
//!   configured byte reply per verb, exactly as a real C driver's `control` slot would. It is the
//!   safe-Rust stand-in for the C fixtures `src/port/driver/control_tests.rs` compiles, for tests
//!   that must reach [`PortRegistry::port_control`] through a **registered** port rather than
//!   through `beam_io`'s own registry.
//! * [`register_control_port`] — registers such a port into a [`Runtime`]'s port table (the
//!   provider's private `register`, reachable because this module is a descendant of the module
//!   that defines it) and hands back the id and the port's shared control-flag cell, so a test can
//!   drive `port_control` and observe [`PortRegistry::control_binary`] without an OS child, a poll
//!   round or a C compiler.
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
pub(crate) struct ControlVerb {
    /// The verb (`command`) the driver recognises, as the decimal `c_uint` the runtime parses.
    pub(crate) command: u32,
    /// The bytes the driver replies with (an empty vector models a driver that wrote nothing).
    pub(crate) reply: Vec<u8>,
    /// The bits to record in the port's control-flag cell as this callback runs, if any — the
    /// C's `set_port_control_flags(port, flags)`.
    pub(crate) set_flags: Option<libc::c_int>,
}

impl ControlVerb {
    /// A verb answering `reply` and leaving the port's control flags alone.
    pub(crate) fn replying(command: u32, reply: &[u8]) -> ControlVerb {
        ControlVerb {
            command,
            reply: reply.to_vec(),
            set_flags: None,
        }
    }

    /// A verb answering `reply` **and** setting `flags` on the port, as a C driver's `control`
    /// slot does through `set_port_control_flags`.
    pub(crate) fn setting(command: u32, reply: &[u8], flags: libc::c_int) -> ControlVerb {
        ControlVerb {
            command,
            reply: reply.to_vec(),
            set_flags: Some(flags),
        }
    }
}

/// A minimal, always-ready driver whose `control` callback answers one of `verbs`.
///
/// An **unknown** verb is [`BeamError::BadArg`], the C's own answer for a command the driver
/// rejects (a negative `cres`, `io.c:4074`), so a test can tell "the driver saw the verb" from
/// "the runtime substituted one".
pub(crate) struct ControlDriver {
    /// The registered driver name (the trait requires one; nothing looks it up here).
    name: &'static str,
    /// The verbs this driver recognises.
    verbs: Vec<ControlVerb>,
    /// The port's control-flag cell, shared with the [`PortMeta`] the runtime reads after the
    /// callback — the C's `prt->control_flags`.
    flags: Rc<Cell<libc::c_int>>,
}

impl Driver for ControlDriver {
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
                Ok(entry.reply.clone())
            }
            None => Err(BeamError::BadArg),
        }
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
    register_driver_port(runtime, name, verbs, None, None)
}

/// Registers a [`ControlDriver`] as a live `{spawn_driver, _}`-kind port that reports the **busy**
/// state `busy`, and returns its id together with the shared [`BusyFlag`] handle so a test can
/// toggle the state (`set_busy_port`) the way a C driver does from inside a callback.
///
/// The handle is shared by `Rc` with the registered port's metadata — exactly as
/// [`super::driver::DriverPort::busy_flag`] is with a real C host — so
/// [`PortRegistry::is_busy`](super::PortRegistry::is_busy) sees every change, and a test can drive
/// the `erlang:port_command/2,3` busy arms without a C compiler.
pub(crate) fn register_busy_port(
    runtime: &mut Runtime,
    name: &'static str,
    busy: bool,
    soft_busy: bool,
) -> (PortId, BusyFlag) {
    let flag = BusyFlag::new();
    flag.set_busy(busy);
    flag.set_soft_busy(soft_busy);
    let (id, _flags) = register_driver_port(runtime, name, Vec::new(), None, Some(flag.clone()));
    (id, flag)
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
    let (id, _flags) = register_driver_port(runtime, name, Vec::new(), Some(monitors), None);
    id
}

/// The one body behind both fixtures above: a [`ControlDriver`] with `verbs` registered under
/// `name`, carrying `driver_monitors` when the caller asked for a monitor tree.
fn register_driver_port(
    runtime: &mut Runtime,
    name: &'static str,
    verbs: Vec<ControlVerb>,
    driver_monitors: Option<Rc<super::driver::DriverMonitors>>,
    driver_busy: Option<BusyFlag>,
) -> (PortId, Rc<Cell<libc::c_int>>) {
    let flags = Rc::new(Cell::new(0));
    let driver = ControlDriver {
        name,
        verbs,
        flags: Rc::clone(&flags),
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
    (id, flags)
}
