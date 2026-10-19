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
//! The **busy-port backpressure decision table** at the port layer, pinned structurally.
//!
//! [`Port::command`] is the one place the C's `erts_port_output` busy arm lives, so this suite
//! drives it directly over a [`BusyDriver`] — a Rust [`Driver`] whose busy state, flag word and
//! written bytes are all observable — with no descriptor, no `cc` fixture and no `erl` oracle:
//!
//! | row | C |
//! |---|---|
//! | idle ⇒ `Done` (queued + an `OUTPUT` task) | `io.c:1763` (`ERTS_PORT_OP_DONE`) |
//! | busy + plain ⇒ `Busy`, nothing queued | `io.c:1763-1769` (`ERTS_PORT_OP_BUSY`) |
//! | busy + `[nosuspend]` ⇒ `Busy` (the runtime answers `false`) | `erl_bif_port.c:213-214` |
//! | `[force]` without `SOFT_BUSY` ⇒ the `notsup` raise, busy or not | `erl_bif_port.c:195-198` |
//! | `[force]` with `SOFT_BUSY` ⇒ `Done` (the busy state is bypassed) | `io.c:1731` |
//! | busy cleared ⇒ `Done` again | `io.c:5242-5264` (`set_busy_port(port, 0)`) |
//!
//! The C-driver half of the same table (a real `set_busy_port` through a loaded `.so`) is
//! `tests/abi_busy.rs`; the driver-ABI state's own suite is [`crate::abi::host_tests`].

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use beam_common::{BeamError, Result};
use beam_process::PortId;

use crate::driver::{Driver, DriverContext, DriverFlags};
use crate::poller::Poller;
use crate::port::{Port, PortCommandOptions, PortCommandOutcome, PortState};
use crate::port_task::PortTaskKind;

/// The state a test flips while the driver is already owned by a port.
#[derive(Default)]
struct Flags {
    busy: Cell<bool>,
    closed: Cell<bool>,
}

/// A driver whose busy state and flag word a test controls, and whose writes it can count.
///
/// `output` drains the port's output queue into `written`, exactly as a real driver consumes what
/// it reports (the accounting contract [`Port::output`] verifies), so "the command really reached
/// the driver" is an assertion about bytes, not about a task having been scheduled.
struct BusyDriver {
    flags: Rc<Flags>,
    soft_busy: bool,
    written: Vec<u8>,
}

impl BusyDriver {
    /// An idle, non-`SOFT_BUSY` driver plus the handle a test flips its state through.
    fn new() -> (BusyDriver, Rc<Flags>) {
        let flags = Rc::new(Flags::default());
        (
            BusyDriver {
                flags: Rc::clone(&flags),
                soft_busy: false,
                written: Vec::new(),
            },
            flags,
        )
    }
}

impl Driver for BusyDriver {
    fn name(&self) -> &str {
        "busy-test"
    }

    fn flags(&self) -> DriverFlags {
        if self.soft_busy {
            DriverFlags::SOFT_BUSY
        } else {
            DriverFlags::empty()
        }
    }

    fn is_busy(&self) -> bool {
        self.flags.busy.get()
    }

    fn port_closed(&self) -> bool {
        self.flags.closed.get()
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let bytes = ctx.output().front().to_vec();
        ctx.output().consume(bytes.len());
        self.written.extend_from_slice(&bytes);
        Ok(bytes.len())
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// A poller plus an **open** port over `driver` — `Port::command` refuses a port that is not
/// [`PortState::Open`], which is the C's `badarg` for a dead port.
fn open_port(driver: BusyDriver) -> (Poller, Port) {
    let mut poller = Poller::new().expect("a poll set");
    let mut port = Port::new(PortId::new(1), Box::new(driver));
    port.open(&mut poller).expect("the driver starts");
    (poller, port)
}

/// Runs the port's queued `OUTPUT` task, so a `Done` command really reaches the driver.
fn run_output_task(poller: &mut Poller, port: &mut Port) {
    let task = port
        .pop_ready_task(Instant::now())
        .expect("the command scheduled an OUTPUT task");
    assert_eq!(task.kind(), PortTaskKind::Output);
    port.run_task(poller, &task).expect("the write runs");
}

#[test]
fn an_idle_port_takes_the_command_and_schedules_the_write() {
    let (driver, _flags) = BusyDriver::new();
    let (mut poller, mut port) = open_port(driver);
    assert!(!port.is_busy(), "a fresh port is idle");

    let outcome = port
        .command(b"hi", PortCommandOptions::EMPTY)
        .expect("an idle port takes the command");
    assert_eq!(outcome, PortCommandOutcome::Done);
    assert_eq!(port.output_len(), 2, "the bytes are queued for the driver");
    assert!(
        port.has_pending_task(PortTaskKind::Output),
        "the write is scheduled"
    );

    run_output_task(&mut poller, &mut port);
    assert_eq!(port.output_len(), 0, "the driver consumed the command");
}

/// The busy row: **nothing** is queued and no write is scheduled, because the oracle suspends the
/// caller and re-issues the command once the driver clears the state — queueing now would deliver
/// it twice or out of order.
#[test]
fn a_busy_port_answers_busy_and_queues_nothing() {
    let (driver, flags) = BusyDriver::new();
    flags.busy.set(true);
    let (_poller, mut port) = open_port(driver);
    assert!(port.is_busy());

    assert_eq!(
        port.command(b"hi", PortCommandOptions::EMPTY)
            .expect("a busy port is not an error"),
        PortCommandOutcome::Busy
    );
    assert_eq!(port.output_len(), 0, "a refused command is never queued");
    assert!(
        !port.has_pending_task(PortTaskKind::Output),
        "a refused command schedules no write"
    );
}

/// `[nosuspend]` changes the outcome **above** this layer only: `Busy` is still the port's answer,
/// and the runtime turns it into the oracle's `false` (`erl_bif_port.c:213-214`). What must not
/// happen here is a queue-and-write.
#[test]
fn nosuspend_on_a_busy_port_answers_busy_like_a_plain_command() {
    let (driver, flags) = BusyDriver::new();
    flags.busy.set(true);
    let (_poller, mut port) = open_port(driver);

    assert_eq!(
        port.command(b"hi", PortCommandOptions::EMPTY.with_nosuspend(true))
            .expect("a busy port is not an error"),
        PortCommandOutcome::Busy,
        "the runtime answers `false` for this, not the port"
    );
    assert_eq!(port.output_len(), 0);
}

/// `[force]` is decided from the **driver's flag word**, never from the busy state
/// (`erl_bif_port.c:195-198`), and it is refused *before* anything is queued — so an idle
/// non-`SOFT_BUSY` driver is refused exactly like a busy one.
#[test]
fn force_without_soft_busy_is_notsup_busy_or_not() {
    // Idle: the flag, not the state, is what the C tests.
    let (driver, _flags) = BusyDriver::new();
    let (_poller, mut idle) = open_port(driver);
    let err = idle
        .command(b"hi", PortCommandOptions::EMPTY.with_force(true))
        .expect_err("a driver without SOFT_BUSY refuses a forced command");
    assert_eq!(
        err.as_atom(),
        Some("notsup"),
        "the oracle raises `error:notsup` for `[force]` on a non-soft-busy driver"
    );
    assert_eq!(idle.output_len(), 0, "nothing is attempted");

    // Busy: the same answer, because the flag is checked first and independently.
    let (driver, flags) = BusyDriver::new();
    flags.busy.set(true);
    let (_poller, mut busy) = open_port(driver);
    let err = busy
        .command(
            b"hi",
            PortCommandOptions::EMPTY.with_force(true).with_nosuspend(true),
        )
        .expect_err("the flag decides, not the state");
    assert_eq!(err.as_atom(), Some("notsup"));
    assert_eq!(busy.output_len(), 0);
}

/// The one row where `[force]` **succeeds on a busy port**: the driver advertises
/// `ERL_DRV_FLAG_SOFT_BUSY`, so the C clears its busy mask (`busy_flgs = 0` under `FORCE`,
/// `io.c:1731`) and the command is queued and written.
#[test]
fn force_with_soft_busy_bypasses_the_busy_state_and_really_writes() {
    let (mut driver, flags) = BusyDriver::new();
    driver.soft_busy = true;
    flags.busy.set(true);
    let (mut poller, mut port) = open_port(driver);
    assert!(port.is_busy(), "the port is busy, and force must ignore that");

    assert_eq!(
        port.command(b"hi", PortCommandOptions::EMPTY.with_force(true))
            .expect("a soft-busy driver takes a forced command"),
        PortCommandOutcome::Done
    );
    assert_eq!(port.output_len(), 2);
    run_output_task(&mut poller, &mut port);
    assert_eq!(port.output_len(), 0, "the driver really wrote the command");
}

/// Clearing the busy state — the driver's `set_busy_port(port, 0)` — is what makes the port accept
/// commands again, which is the point the runtime retries a suspended caller at.
#[test]
fn clearing_the_busy_state_lets_the_command_through() {
    let (driver, flags) = BusyDriver::new();
    flags.busy.set(true);
    let (mut poller, mut port) = open_port(driver);

    assert_eq!(
        port.command(b"hi", PortCommandOptions::EMPTY)
            .expect("not an error"),
        PortCommandOutcome::Busy
    );

    // The driver clears its own state (`set_busy_port(port, 0)`), which the port reads back
    // through `Driver::is_busy` on the next command.
    flags.busy.set(false);
    assert!(!port.is_busy());
    assert_eq!(
        port.command(b"hi", PortCommandOptions::EMPTY)
            .expect("an idle port takes the command"),
        PortCommandOutcome::Done
    );
    run_output_task(&mut poller, &mut port);
    assert_eq!(port.output_len(), 0);
}

#[test]
fn a_port_that_is_not_open_is_bad_arg() {
    let mut poller = Poller::new().expect("a poll set");
    // `Port::new` leaves the port in `PortState::Opening`: the driver has not started, which is the
    // C's `ERTS_PORT_OP_BADARG` for a port that cannot take output.
    let (driver, _flags) = BusyDriver::new();
    let mut port = Port::new(PortId::new(1), Box::new(driver));
    assert_eq!(port.state(), PortState::Opening);
    assert!(matches!(
        port.command(b"hi", PortCommandOptions::EMPTY),
        Err(BeamError::BadArg)
    ));
    port.close(&mut poller).expect("the teardown succeeds");
    assert!(matches!(
        port.command(b"hi", PortCommandOptions::EMPTY),
        Err(BeamError::BadArg)
    ));
}

/// The two state probes forward to the driver and nothing more: neither is inferred from the byte
/// queues or from a task having run.
#[test]
fn the_state_probes_forward_to_the_driver() {
    let (driver, flags) = BusyDriver::new();
    flags.busy.set(true);
    flags.closed.set(true);
    let (_poller, port) = open_port(driver);
    assert!(port.is_busy());
    assert!(port.port_closed());

    // A driver that reports neither state answers `false` for both.
    let (driver, _flags) = BusyDriver::new();
    let (_poller, idle) = open_port(driver);
    assert!(!idle.is_busy());
    assert!(!idle.port_closed());
}
