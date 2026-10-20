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

//! A `ram_file_drv` command queued **before** a close must execute, not vanish — pinned here.
//!
//! The C's `ram_file_driver_entry` has `flush = NULL` (`erts/emulator/drivers/common/ram_file_drv.c:120`),
//! so `flush_port` has nothing to call (`erts/emulator/beam/io.c:3659-3700`); the command is run
//! by the driver's **`output`** slot, and the port's own drain is what reaches it. This floor keeps
//! exactly that shape: [`crate::Driver::flush`]'s default discards nothing and
//! [`RamFileDriver::output`](super::RamFileDriver::output) consumes the whole queued payload and
//! delivers its reply, so [`crate::Port::close`]'s begin half — `run_task(Output)` → `ready_output`
//! (the `Driver` default, i.e. `output`) → `Port::check_write_accounting` — is what makes a
//! command that no round ever drove still execute, with its reply handed up as a completed read.
//!
//! This driver consumes **everything** it is given, so the queue is empty after the drain and the
//! close finishes inside the `close` call: the parked half of the flush-then-close shape is
//! unreachable here (pinned below through [`crate::PortRegistry::is_closing`], which is a `badarg`
//! once the port is gone).
//!
//! # One command per `output` call — and why the queue cannot split them
//!
//! The C runs `rfile_command` once per `erlang:port_command/2`, so `buf`/`len` *is* one command.
//! This crate's output queue is one byte stream with no per-command boundary (the C's `ioq` is
//! the same), and this driver cannot invent one: a `write`'s operand is "everything after the
//! command word", delimited only by the `port_command` call. Two commands queued before any round
//! therefore arrive as **one** payload and run as one command — pinned by
//! [`several_commands_queued_without_a_round_arrive_as_one_payload`] — which is why a caller
//! issuing a command-driven protocol drives a round (or `PortRegistry::output`, which writes
//! inline) between commands. The close drain is the one place several commands *would* run back to
//! back, and it can only ever see the single merged payload.
//!
//! Hermetic: the driver owns no descriptor and registers nothing with the poller.
//!
//! The tests are the driver's own in-module suite, split into a sibling like the two existing
//! suites (`super` is the `ram_file` module).

use super::{
    RamFileDriver, RAM_FILE_GET, RAM_FILE_MODE_READ_WRITE, RAM_FILE_OPEN, RAM_FILE_RESP_DATA,
    RAM_FILE_RESP_NUMBER, RAM_FILE_SIZE, RAM_FILE_WRITE,
};
use crate::poller::Poller;
use crate::port::{Port, PortCommandOptions, PortCommandOutcome, PortRegistry, PortState};
use crate::port_task::PortNotification;
use beam_common::BeamError;
use beam_process::PortId;

/// The reduction budget a round runs with (the registry's own default).
const ROUND_BUDGET: usize = 1000;

// -----------------------------------------------------------------------
// Command builders (the wire form, byte for byte — the C's `rfile_command` payload)
// -----------------------------------------------------------------------

fn c_open(mode: i32, content: &[u8]) -> Vec<u8> {
    let mut v = vec![RAM_FILE_OPEN];
    v.extend_from_slice(&mode.to_be_bytes());
    v.extend_from_slice(content);
    v
}

fn c_write(data: &[u8]) -> Vec<u8> {
    let mut v = vec![RAM_FILE_WRITE];
    v.extend_from_slice(data);
    v
}

fn c_get() -> Vec<u8> {
    vec![RAM_FILE_GET]
}

fn c_size() -> Vec<u8> {
    vec![RAM_FILE_SIZE]
}

// -----------------------------------------------------------------------
// Reply readers
// -----------------------------------------------------------------------

/// A fresh poll set.
fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// An **open** port over a fresh [`RamFileDriver`], kept by the caller so the driver survives the
/// close (a registry removes — and drops — the port instead).
fn open_port() -> (Poller, Port) {
    let mut poller = poller();
    let mut port = Port::new(PortId::new(1), Box::new(RamFileDriver::new()));
    port.open(&mut poller).expect("the driver starts");
    assert_eq!(port.state(), PortState::Open);
    (poller, port)
}

/// The `NUMBER` reply `header` for `value`: `[RAM_FILE_RESP_NUMBER, value:i32be]`.
fn number(value: i32) -> Vec<u8> {
    let mut v = vec![RAM_FILE_RESP_NUMBER];
    v.extend_from_slice(&value.to_be_bytes());
    v
}

/// The `DATA` reply `header` for `len` bytes: `[RAM_FILE_RESP_DATA, len:i32be]`.
fn data_header(len: usize) -> Vec<u8> {
    let mut v = vec![RAM_FILE_RESP_DATA];
    v.extend_from_slice(&(len as i32).to_be_bytes());
    v
}

/// Runs one port-task round, answering the `Closed` reports it carried.
fn round(poller: &mut Poller, registry: &mut PortRegistry) -> Vec<PortId> {
    registry
        .execute_port_tasks(poller, ROUND_BUDGET)
        .expect("the round runs")
        .notifications
        .into_iter()
        .filter_map(|notification| match notification {
            PortNotification::Closed { port } => Some(port),
            PortNotification::Data { .. } => None,
        })
        .collect()
}

// -----------------------------------------------------------------------
// The close drain
// -----------------------------------------------------------------------

/// **The queued command executes in the close.** A `size` queued with no round run answers
/// `NUMBER 5` once the close drains it — which proves two things at once: the command ran (an
/// unexecuted one would answer nothing at all), and the driver's file really holds the five bytes
/// the earlier `open` loaded, so the reply reflects live driver state rather than a fabricated one.
///
/// The port finishes inside the `close` call (this driver consumes its whole queue), and the reply
/// is staged as one completed read — the C's single `driver_output2` call
/// (`ram_file_drv.c:182`-`:230`).
#[test]
fn a_command_queued_before_a_close_executes_in_the_close_drain() {
    let (mut poller, mut port) = open_port();

    // Setup, driven inline: `open` loads the file and is answered.
    let written = port
        .output(&mut poller, &c_open(RAM_FILE_MODE_READ_WRITE, b"hello"))
        .expect("`open` is accepted");
    assert_eq!(written, c_open(RAM_FILE_MODE_READ_WRITE, b"hello").len());
    assert_eq!(
        port.take_input_notifications().len(),
        1,
        "`open` answers `NUMBER 0`"
    );

    // The command under test: queued the way `erlang:port_command/2` queues, with **no** round.
    assert_eq!(
        port.command(&c_size(), PortCommandOptions::EMPTY)
            .expect("an idle port takes the command"),
        PortCommandOutcome::Done
    );
    assert_eq!(port.output_len(), 1, "the command is queued, not run");
    // The close drains it: `run_task(Output)` → `ready_output` → `output`.
    port.close(&mut poller).expect("the close runs");

    assert_eq!(
        port.state(),
        PortState::Closed,
        "a driver that consumes everything leaves nothing to park on"
    );
    assert_eq!(
        port.output_len(),
        0,
        "the command was consumed by the drain"
    );

    let notifications = port.take_input_notifications();
    assert_eq!(
        notifications.len(),
        1,
        "the close drain staged exactly one reply: {notifications:?}"
    );
    match &notifications[0] {
        PortNotification::Data {
            port: stamped,
            read,
        } => {
            assert_eq!(*stamped, port.id(), "the reply names this port");
            assert_eq!(
                read.header,
                number(5),
                "`size` reports the five bytes `open` loaded — the queued command really ran"
            );
            assert_eq!(read.body, None, "`NUMBER` is a header-only reply");
        }
        other => panic!("expected the `size` reply, got {other:?}"),
    }

    // `stop` ran (the reset is what a second observation would see) and a repeated close is a
    // no-op: no second flush, no second drain, no second stop.
    port.close(&mut poller)
        .expect("a repeated close is a no-op");
    assert_eq!(port.state(), PortState::Closed);
}

/// **A queued `write` executes in the close too** — its reply is the byte count the driver really
/// copied into its buffer (`NUMBER 5`), and the `DATA` path stays untouched.
#[test]
fn a_write_queued_before_a_close_replies_from_the_close_drain() {
    let (mut poller, mut port) = open_port();

    // Setup: an empty writable file.
    port.output(&mut poller, &c_open(RAM_FILE_MODE_READ_WRITE, b""))
        .expect("`open` is accepted");
    let _ = port.take_input_notifications();

    assert_eq!(
        port.command(&c_write(b"world"), PortCommandOptions::EMPTY)
            .expect("an idle port takes the command"),
        PortCommandOutcome::Done
    );
    assert_eq!(port.output_len(), c_write(b"world").len());

    port.close(&mut poller).expect("the close runs");

    assert_eq!(port.state(), PortState::Closed);
    assert_eq!(port.output_len(), 0);
    let notifications = port.take_input_notifications();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    match &notifications[0] {
        PortNotification::Data { read, .. } => {
            assert_eq!(
                read.header,
                number(5),
                "the queued `write` really ran and wrote five bytes"
            );
            assert_eq!(read.body, None);
        }
        other => panic!("expected the `write` reply, got {other:?}"),
    }
}

/// A `DATA` command queued before the close comes back through the same drain with its
/// header/body split intact — the close path is the ordinary write path, not a second one.
#[test]
fn a_data_reply_queued_before_a_close_keeps_its_split() {
    let (mut poller, mut port) = open_port();

    port.output(&mut poller, &c_open(RAM_FILE_MODE_READ_WRITE, b"hello"))
        .expect("`open` is accepted");
    let _ = port.take_input_notifications();

    port.command(&c_get(), PortCommandOptions::EMPTY)
        .expect("an idle port takes the command");
    port.close(&mut poller).expect("the close runs");

    let notifications = port.take_input_notifications();
    assert_eq!(notifications.len(), 1, "{notifications:?}");
    match &notifications[0] {
        PortNotification::Data { read, .. } => {
            assert_eq!(read.header, data_header(5), "the C's 5-byte DATA header");
            assert_eq!(
                read.body.as_deref(),
                Some(b"hello".as_slice()),
                "the whole file, as the `get` it was queued as"
            );
        }
        other => panic!("expected the `get` reply, got {other:?}"),
    }
}

/// **The registry-level half.** A command queued through `port_command_with` and never driven by a
/// round is consumed by the close's own drain, so the queue is empty and the port finishes **inside
/// the `close` call**: it leaves the live map, [`crate::PortRegistry::is_closing`] is the `badarg`
/// of an unknown id, and the next round reports its `Closed` exactly once.
///
/// That the queue emptied *is* the evidence the command ran: a command the drain had not executed
/// would still be queued, and the port would park in [`crate::PortState::Closing`] instead
/// (`is_closing` ⇒ `Ok(true)`) — the state the [`FdDriver`](crate::FdDriver) suite pins for a
/// driver that cannot consume synchronously.
#[test]
fn a_queued_command_leaves_nothing_to_park_on_at_the_registry() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(RamFileDriver::new()))
        .expect("the port opens");

    let command = c_open(RAM_FILE_MODE_READ_WRITE, b"hello");
    assert_eq!(
        registry
            .port_command_with(&mut poller, id, &command, PortCommandOptions::EMPTY)
            .expect("an idle port takes the command"),
        PortCommandOutcome::Done
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        command.len(),
        "`port_command` queues the command and returns; no round has run"
    );

    registry.close(&mut poller, id).expect("the close runs");

    assert!(
        !registry.contains(id),
        "the drain consumed the queued command, so the close finished inside the call"
    );
    assert!(
        matches!(registry.is_closing(id), Err(BeamError::BadArg)),
        "the parked state is unreachable for this driver — a finished port is out of the map"
    );
    assert_eq!(
        round(&mut poller, &mut registry),
        vec![id],
        "the next round reports the close"
    );
    assert_eq!(
        round(&mut poller, &mut registry),
        Vec::new(),
        "exactly once"
    );
}

/// **The documented limitation, pinned**: two commands queued before a round arrive as one
/// payload, because the output queue is one byte stream and a `write`'s operand is delimited only
/// by the `port_command` call. Here `open` follows the flags with the `size` command byte, so it
/// loads a one-byte file — one command, one reply — and the `size` never runs separately.
///
/// This is why the close-drain tests above queue exactly one command, and why a caller issuing a
/// command-driven protocol drives a round (or `PortRegistry::output`) between commands.
#[test]
fn several_commands_queued_without_a_round_arrive_as_one_payload() {
    let (mut poller, mut port) = open_port();

    port.command(
        &c_open(RAM_FILE_MODE_READ_WRITE, b""),
        PortCommandOptions::EMPTY,
    )
    .expect("an idle port takes the command");
    port.command(&c_size(), PortCommandOptions::EMPTY)
        .expect("an idle port takes the command");
    let mut expected = c_open(RAM_FILE_MODE_READ_WRITE, b"");
    expected.extend_from_slice(&c_size());
    assert_eq!(
        port.output_len(),
        expected.len(),
        "both commands sit in one queue"
    );

    port.close(&mut poller).expect("the close runs");

    let notifications = port.take_input_notifications();
    assert_eq!(
        notifications.len(),
        1,
        "the merged payload is one command, so it is one reply: {notifications:?}"
    );
    match &notifications[0] {
        PortNotification::Data { read, .. } => assert_eq!(
            read.header,
            number(0),
            "the `open` reply: its trailing `size` byte became the initial content"
        ),
        other => panic!("expected one `open` reply, got {other:?}"),
    }
}
