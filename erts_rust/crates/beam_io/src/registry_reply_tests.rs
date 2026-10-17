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

//! What one port-task round makes of a driver's write path — the reply, and the refusal.
//!
//! Both pins here are **hermetic and oracle-independent**: they drive the real port layer
//! with a descriptor-less built-in driver, so no `erl`, no child process, no `sleep` and no
//! poller wait is involved (`ram_file_drv`'s `start` registers nothing — a `ram_file` port
//! owns no descriptor at all).
//!
//! * [`a_built_in_drivers_reply_reaches_the_owner_as_one_data_notification`] follows one
//!   `port_command` from the queue, through a single
//!   [`PortRegistry::execute_port_tasks`](crate::PortRegistry::execute_port_tasks) round, to
//!   the [`PortNotification::Data`] the owner receives, and pins the reply bytes and the
//!   write bookkeeping exactly.
//! * [`RefusingWriteDriver`] pins how a driver that **refuses** a command surfaces: the port
//!   layer propagates the typed error out of the round instead of reporting an outcome, and
//!   the command's bytes stay queued (see the `// A refused command` section of
//!   [`Port::output`](crate::port::Port::output)'s rustdoc for why that is deliberate).
//!
//! # Verdict on the "a built-in driver's reply bytes never reach its owner" divergence
//!
//! The crate's half is **correct and is now pinned**: `port_command` queues the bytes and
//! schedules one `Output` task (`PortRegistry::port_command`) → `run_scheduled_tasks` pops it
//! and runs `Port::run_task(Output)` → `Port::ready_output`, whose [`Driver`] default
//! delegates to the driver's `output` → `RamFileDriver::output` consumes the whole payload
//! and pushes its `NUMBER 0` reply onto `ctx.input()` (`drivers/ram_file.rs`) → the round
//! harvests `take_input_notifications()` for **every** live port and reports one
//! [`PortNotification::Data`]. A missing reply term in the running runtime therefore cannot
//! originate in this layer: it is above the crate (the runtime must turn that `Data` into
//! `{Port, {data, Bytes}}` and deliver it to the port's owner).

use beam_common::{BeamError, Result};
use beam_process::PortId;

use crate::driver::{Driver, DriverContext, DriverRegistry};
use crate::poller::Poller;
use crate::port::{Port, PortCommandOptions, DEFAULT_READINESS_RED_BUDGET};
use crate::port_task::{PortNotification, PortTaskKind};
use crate::registry::PortRegistry;

/// The exact `port_command` payload the divergence record measured: `ram_file_drv`'s `open`
/// command word (`1`), the access mode `3` (`RAM_FILE_MODE_READ | RAM_FILE_MODE_WRITE`) as a
/// 4-byte big-endian `i32`, then the initial file content `"abc"`.
const OPEN_READ_WRITE_ABC: [u8; 8] = [1, 0, 0, 0, 3, 97, 98, 99];

/// `ram_file_drv`'s reply to [`OPEN_READ_WRITE_ABC`]: the `NUMBER` reply code (`3`) plus a
/// 4-byte big-endian `i32` — the `open` result slot, always `0`. Five bytes, i.e. neither the
/// command's length nor its bytes: the reply is what the driver pushed onto its **input**
/// queue, never an echo of what the owner wrote.
const OPEN_READ_WRITE_ABC_REPLY: [u8; 5] = [3, 0, 0, 0, 0];

/// A fresh poll set. The `ram_file_drv` port registers no descriptor, so nothing is ever
/// polled here — the poller only has to exist for the port layer's signatures.
fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// A registry holding one open `ram_file_drv` port, reached through the built-in
/// [`DriverRegistry`] under its registered name — i.e. exactly the driver lookup a
/// `{spawn_driver, "ram_file_drv"}` port performs.
fn ram_file_registry(poller: &mut Poller) -> (PortRegistry, PortId) {
    let mut registry = PortRegistry::new();
    let drivers = DriverRegistry::with_builtins();
    let id = registry
        .open_named(poller, &drivers, "ram_file_drv")
        .expect("the built-in ram_file_drv opens");
    (registry, id)
}

/// The built-in-driver reply round trip, pinned end to end on this side of the runtime.
///
/// Pins, in order: the command is **queued and not written** by `port_command`; a single
/// `execute_port_tasks` round reports **exactly one** [`PortNotification::Data`] naming that
/// port and carrying **exactly the driver's 5 reply bytes** (not the 8 command bytes — the
/// reply comes from the driver's input queue, never from the output bytes it consumed); the
/// round's write bookkeeping is exact (the command consumed once, the reply drained into the
/// notification, no `Output` task left pending); and a second round re-reports **nothing**
/// (no double delivery) and stays empty.
///
/// This is the crate-side evidence for the differential finding "a built-in driver's reply
/// bytes never reach its owner": the bytes *do* leave the driver and *do* become a
/// notification here, so the gap is above `beam_io`.
#[test]
fn a_built_in_drivers_reply_reaches_the_owner_as_one_data_notification() {
    let mut poller = poller();
    let (mut registry, id) = ram_file_registry(&mut poller);

    // 1./2. `port_command` enqueues the payload and schedules the write; it does not write.
    registry
        .port_command(
            &mut poller,
            id,
            &OPEN_READ_WRITE_ABC,
            PortCommandOptions::new(),
        )
        .expect("the command is queued");
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        OPEN_READ_WRITE_ABC.len(),
        "the whole command is queued"
    );
    assert!(
        registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output),
        "the write is scheduled, not performed"
    );
    assert_eq!(
        registry.get(id).expect("live").input_len(),
        0,
        "the driver has not run yet, so there is no reply"
    );

    // 3. One round runs the `Output` task and reports what the driver produced.
    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the output task runs");
    assert!(round.ran >= 1, "the round ran the queued write task");
    assert_eq!(
        round.notifications,
        vec![PortNotification::Data {
            port: id,
            bytes: OPEN_READ_WRITE_ABC_REPLY.to_vec(),
        }],
        "exactly one notification, naming that port, carrying the driver's reply bytes"
    );
    // Stated the other way round, so the assertion cannot be read as "the command echoed back":
    assert_ne!(
        OPEN_READ_WRITE_ABC_REPLY.as_slice(),
        &OPEN_READ_WRITE_ABC[..],
        "the reply is the driver's, not the queued command's bytes"
    );

    // 4. Both queues are balanced: the command was consumed exactly once and the reply was
    //    drained into the notification rather than left queued (which would report it twice).
    let port = registry.get(id).expect("live");
    assert_eq!(port.output_len(), 0, "the queued command was consumed once");
    assert_eq!(port.input_len(), 0, "the reply was drained, not left queued");
    assert!(
        !port.has_pending_task(PortTaskKind::Output),
        "a fully written command leaves no `Output` task pending"
    );

    // 5. Negative control: the reply is reported exactly once, so a second round has nothing
    //    to say about this port and re-drives nothing.
    let again = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("an empty round runs");
    assert!(
        again.notifications.is_empty(),
        "no double report of the same reply bytes"
    );
    assert_eq!(again.ran, 0, "nothing left to run");
    let port = registry.get(id).expect("live");
    assert_eq!(
        (port.output_len(), port.input_len()),
        (0, 0),
        "the empty round moved no bytes in either direction"
    );
}

/// A driver whose write path **refuses** every command, without touching the output queue.
///
/// This is the port layer's stand-in for a socket driver asked to write before its socket
/// exists: `tcp_inet`/`udp_inet` answer [`BeamError::BadArg`] then, which is the error the
/// oracle surfaces as `badarg` from `port_command/2` on a socket-less
/// `{spawn_driver, "tcp_inet"}` port (the C `!IS_OPEN` check).
///
/// It never calls `ctx.output().front()`/`consume()`, so a refusal can never be confused with
/// writing zero bytes, and both write entry points are implemented explicitly (rather than
/// relying on [`Driver::ready_output`]'s default) so the port layer's handling of each is
/// pinned independently.
struct RefusingWriteDriver {
    /// How often the driver's `output` was reached.
    outputs: usize,
    /// How often the driver's `ready_output` was reached.
    ready_outputs: usize,
}

impl RefusingWriteDriver {
    fn new() -> RefusingWriteDriver {
        RefusingWriteDriver {
            outputs: 0,
            ready_outputs: 0,
        }
    }
}

impl Driver for RefusingWriteDriver {
    fn name(&self) -> &str {
        "refusing-write-test"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        // Registers no descriptor: the port needs no poller registration to be driven.
        Ok(())
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        // Registers no descriptor, so read readiness is never dispatched to this driver.
        Ok(())
    }

    fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.outputs += 1;
        Err(BeamError::BadArg)
    }

    fn ready_output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.ready_outputs += 1;
        Err(BeamError::BadArg)
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// An inline `Port::output` that the driver refuses: the typed error is propagated and the
/// command's bytes are **not** counted as written — they stay in the output queue.
///
/// The accounting check is short-circuited by the error (`result?` runs before the comparison),
/// so the driver's byte count is never inspected; that is the documented split (see
/// `Port::output`'s `# A refused command` section), and it is what keeps a refused command
/// from being silently reported as a successful short write.
#[test]
fn a_refused_write_keeps_the_command_bytes_queued() {
    let mut poller = poller();
    let mut port = Port::new(PortId::new(1), Box::new(RefusingWriteDriver::new()));
    port.open(&mut poller).expect("the stub opens");

    let result = port.output(&mut poller, b"abc");
    assert!(
        matches!(result, Err(BeamError::BadArg)),
        "the driver's refusal is propagated verbatim, got {result:?}"
    );
    assert_eq!(
        port.output_len(),
        3,
        "a refused command is not counted as written and stays queued"
    );
    assert!(
        !port.has_pending_task(PortTaskKind::Output),
        "a refused inline write schedules no retry task"
    );
}

/// The `ready_output` entry point (the one an `Output` port task runs) surfaces the same
/// refusal, leaving the queued bytes untouched.
#[test]
fn refused_write_readiness_propagates_and_keeps_the_queued_bytes() {
    let mut poller = poller();
    let mut port = Port::new(PortId::new(1), Box::new(RefusingWriteDriver::new()));
    port.open(&mut poller).expect("the stub opens");
    port.queue_output(b"cmd!");
    assert_eq!(port.output_len(), 4);

    let result = port.ready_output(&mut poller);
    assert!(
        matches!(result, Err(BeamError::BadArg)),
        "write readiness reports the driver's refusal, got {result:?}"
    );
    assert_eq!(
        port.output_len(),
        4,
        "the refused command's bytes are still queued"
    );
}

/// A refusal surfaces **out of the port-task round**, not as a `PortTaskOutcome`: the round
/// returns the driver's error, so a runtime can raise it (the oracle's `badarg`) instead of
/// believing the command was written.
///
/// Deliberate consequence, pinned here: the `Output` task was already popped when the driver
/// refused, so the failed command's bytes stay in the output queue with **no task pending** —
/// the round neither retries the refused command nor silently drops it. A caller must treat
/// the error as "the command was refused" and not re-drive the port; the bytes go away with
/// the port (a `PortRegistry::close` drops it, and both queues with it).
#[test]
fn a_refused_command_surfaces_out_of_the_port_task_round() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(RefusingWriteDriver::new()))
        .expect("the stub opens");
    registry
        .port_command(&mut poller, id, b"cmd!", PortCommandOptions::new())
        .expect("the command is queued");
    assert_eq!(registry.get(id).expect("live").output_len(), 4);
    assert!(
        registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output)
    );

    let outcome = registry.execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET);
    assert!(
        matches!(outcome, Err(BeamError::BadArg)),
        "the round propagates the driver's refusal instead of reporting an outcome"
    );

    // The port is still live, the refused bytes are still queued, and nothing is pending: the
    // round consumed its task and did not schedule another one.
    let port = registry.get(id).expect("the port survives a refused write");
    assert_eq!(port.output_len(), 4, "the refused bytes stay queued");
    assert_eq!(port.input_len(), 0, "a refused write yields no reply");
    assert_eq!(port.pending_tasks(), 0, "the failed task is not re-queued");

    // A later round therefore neither retries the command nor reports anything about it.
    let later = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("an empty round runs");
    assert_eq!(later.ran, 0, "the refused command is not retried");
    assert!(
        later.notifications.is_empty(),
        "a refused write reports no data"
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        4,
        "the bytes stay queued until the port is closed"
    );
}
