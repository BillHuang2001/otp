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

//! Does [`FdDriver::stop`](super::FdDriver) lose queued output? **No** — pinned here.
//!
//! `FdDriver::stop` releases the text-phase input remainder, deregisters both descriptors and
//! closes them. It never writes to the output queue, so a byte still queued when it runs would be
//! lost — which is exactly why the port only calls it once that queue is **empty**:
//!
//! * the C's `terminate_port` — the sole caller of `drv->stop` — is reached only when
//!   `is_port_ioq_empty(p)` (`erts/emulator/beam/io.c:3697-3700`, `:3733-3752`), and a close with a
//!   non-empty ioq parks the port in `ERTS_PORT_SFLG_CLOSING` instead
//!   (`io.c:896-900`, `:3936-3942`);
//! * this crate's [`crate::Port::close`] mirrors that split: the driver's `stop` runs from
//!   [`crate::Port::finish_close`], which a close reaches only when the drain emptied the queue —
//!   otherwise the port stays live in [`crate::PortState::Closing`] and
//!   [`crate::PortRegistry::finish_closing_ports`] keeps draining it.
//!
//! So a short write (`EAGAIN` on a full pipe/socket) parks the close **before** `stop`, and the
//! rounds that follow deliver the remainder. Both halves are pinned below over a real socket pair:
//! the *parked* port still owns its write descriptor (`FdDriver::has_write_fd` is `true`, and
//! `stop` is what takes it), and after the reader drains every byte arrives — byte for byte, no
//! truncation and no duplication — before the port reports `Closed`.
//!
//! # Why no `flush` overrider
//!
//! The C `fd` driver's `fd_flush` only latches a flag; it writes nothing
//! (`erts/emulator/sys/unix/sys_drivers.c:1029-1034`, the guarantee documented at `:900-907`). The
//! writing is done by its `ready_output` (`sys_drivers.c:1448-1471`), which keeps running while the
//! port is `CLOSING` and calls `driver_failure_atom(port, "normal")` once the queue emptied and the
//! latch is set — i.e. the C's `terminating` flag is just "when the queue empties, terminate".
//! This port layer already *is* that loop ([`crate::Port::drain_output_during_close`] runs the
//! ordinary `OUTPUT` task), and its `Closing` state is that latch, so an `fd` `flush` override
//! could only discard bytes the C keeps. `Driver::flush`'s default (discard nothing) is therefore
//! the faithful answer, and `a_command_queued_before_a_close_is_written_by_the_close` pins the
//! writing it must leave alone.
//!
//! Hermetic: a `UnixStream::pair` and a dup of its write end — no `cc`, no `erl`, no child.
//!
//! The tests are the driver's own in-module suite, split into a sibling only because `fd.rs` is
//! past its size target (`super` is the `fd` module).

use super::FdDriver;
use crate::poller::Poller;
use crate::port::{PortCommandOptions, PortRegistry};
use crate::port_task::{PortNotification, PortTaskKind};
use beam_common::BeamError;
use beam_process::PortId;
use std::io::Read;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The payload the parked-close tests use: deliberately far larger than any socket send buffer
/// (the default is a couple of hundred kilobytes, and `/proc/sys/net/core/wmem_max` bounds it), so
/// the very first write cannot complete.
const PAYLOAD: usize = 1 << 20;

/// The reduction budget a round runs with (the registry's own default).
const ROUND_BUDGET: usize = 1000;

/// A fresh poll set.
fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// A socket pair plus a **dup** of its write end: the test keeps the reader, the port gets the
/// writer (and owns it — [`FdDriver::stop`] closes it).
fn write_end() -> (UnixStream, RawFd) {
    let (peer, write_end) = UnixStream::pair().expect("socketpair");
    // SAFETY: `write_end.as_raw_fd()` names an open descriptor of a live stream, and `dup(2)`
    // returns a fresh descriptor for the same socket. The dup is handed to the driver, which owns
    // it and closes it exactly once (`FdDriver::close_fds`), so `write_end` dropping its own
    // descriptor at the end of this helper cannot invalidate it.
    let fd = unsafe { libc::dup(write_end.as_raw_fd()) };
    assert!(fd >= 0, "dup(2) succeeded");
    (peer, fd)
}

/// A payload whose every byte is distinguishable, so a lost or duplicated chunk cannot hide.
fn payload() -> Vec<u8> {
    (0..PAYLOAD).map(|i| (i % 251) as u8).collect()
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

/// **The park**: a short write leaves bytes queued, so the close cannot finish synchronously and
/// the port stays live in [`crate::PortState::Closing`] with its write descriptor still owned —
/// i.e. [`FdDriver::stop`] has **not** run and therefore cannot have dropped the remainder.
///
/// The C agrees: a close with a non-empty ioq goes to `ERTS_PORT_SFLG_CLOSING`, where the port
/// "receives in/out-put events so as soon as the port queue gets empty `terminate_port()` is
/// called" (`erts/emulator/beam/io.c:896-900`, `erts_deliver_port_exit` `io.c:3936-3942`).
#[test]
fn a_short_write_parks_the_close_before_stop_runs() {
    let (peer, fd) = write_end();
    let payload = payload();
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(FdDriver::write_only(fd)))
        .expect("the port starts");

    // The write fills the socket buffer and reports the short write, leaving the remainder queued.
    let written = registry
        .output(&mut poller, id, &payload)
        .expect("the driver writes what it can");
    assert!(
        written < payload.len(),
        "a {PAYLOAD}-byte payload cannot all fit in a socket buffer (wrote {written})"
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        payload.len() - written,
        "the unwritten remainder stays queued"
    );

    registry
        .close(&mut poller, id)
        .expect("a park is not an error");

    assert!(
        registry.is_closing(id).expect("the port is live"),
        "a queue that cannot be written parks the port in `Closing`"
    );
    assert!(registry.contains(id), "a parked port is still a live port");
    let remaining = registry.get(id).expect("live").output_len();
    assert!(
        remaining > 0,
        "the remainder survives the close: {remaining} bytes queued"
    );
    assert!(
        registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output),
        "an OUTPUT task stays pending, so write readiness has something to run"
    );

    // The proof that `stop` has not run: `stop` is what closes the descriptors, so a port that
    // still owns its write fd is a port whose driver was never stopped — and therefore a port
    // whose queued bytes were never dropped with the descriptor.
    let driver = registry
        .driver_as::<FdDriver>(id)
        .expect("the port's driver is an FdDriver");
    assert!(
        driver.has_write_fd(),
        "`FdDriver::stop` closes the write fd, so an attached fd proves it has not run"
    );

    // A command on a closing port is the C's `badarg` (io.c:3877), not a second queue.
    assert!(matches!(
        registry.port_command_with(&mut poller, id, b"late", PortCommandOptions::EMPTY),
        Err(BeamError::BadArg)
    ));

    // Dropping the reader closes the socket; the parked port is dropped with the registry, and
    // its `Drop` releases the descriptor `stop` has not.
    drop(peer);
}

/// **The delivery**: with the reader draining, the rounds deliver every queued byte — no
/// truncation, no duplication — and only then does the port finish and report `Closed`.
///
/// Every round makes progress, so [`crate::CLOSING_DRAIN_STALL_LIMIT`] never fires: the bound only
/// exists for a port that can never drain at all.
#[test]
fn the_parked_remainder_is_delivered_in_full_by_the_rounds() {
    let (mut peer, fd) = write_end();
    peer.set_read_timeout(Some(Duration::from_secs(30)))
        .expect("the reader gets a timeout, so a broken drain fails instead of hanging");
    let payload = payload();
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(FdDriver::write_only(fd)))
        .expect("the port starts");

    let written = registry
        .output(&mut poller, id, &payload)
        .expect("the driver writes what it can");
    assert!(
        written < payload.len(),
        "the write is short: {written} bytes"
    );
    registry
        .close(&mut poller, id)
        .expect("a park is not an error");
    assert!(registry.is_closing(id).expect("the port is live"));

    // The reader is what lets the writer move: read a chunk, then let a round write the next one.
    let mut received = Vec::with_capacity(payload.len());
    let mut chunk = vec![0u8; 64 * 1024];
    let mut closed_reports = Vec::new();
    let mut rounds = 0;
    let mut last_remaining = registry.get(id).expect("live").output_len();
    while registry.contains(id) || received.len() < payload.len() {
        rounds += 1;
        assert!(
            rounds < 100_000,
            "the drain must terminate (round {rounds})"
        );
        if received.len() < payload.len() {
            match peer.read(&mut chunk) {
                Ok(0) => panic!("the peer closed before the last byte was delivered"),
                Ok(n) => received.extend_from_slice(&chunk[..n]),
                // Nothing buffered right now: run another round and try again.
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(err) => panic!("the peer failed to read: {err}"),
            }
        }
        if registry.contains(id) {
            closed_reports.extend(round(&mut poller, &mut registry));
            if let Some(port) = registry.get(id) {
                let remaining = port.output_len();
                assert!(
                    remaining < last_remaining,
                    "every round makes progress ({last_remaining} → {remaining}), so the stall \
                     bound never fires"
                );
                last_remaining = remaining;
            }
        }
    }

    assert_eq!(
        received.len(),
        payload.len(),
        "every queued byte was delivered, none dropped"
    );
    assert!(
        received == payload,
        "the delivered bytes are the payload, byte for byte"
    );
    assert!(
        !registry.contains(id),
        "the port finished once its queue emptied"
    );
    assert_eq!(
        closed_reports,
        vec![id],
        "the finishing round reports the close, exactly once"
    );
    assert!(
        matches!(registry.is_closing(id), Err(BeamError::BadArg)),
        "a finished port is out of the live map"
    );
}

/// **A queue that fits is written by the close itself**: the drain is the ordinary `OUTPUT` task,
/// and with nothing left to do `stop` runs inside the same call.
///
/// This is the half an `fd` `flush` override must not touch: the C `fd` driver discards nothing at
/// close (`fd_flush` only latches `terminating`, `sys_drivers.c:1029-1034`), and the bytes are
/// written by its `ready_output` (`sys_drivers.c:1448-1471`). Here that loop is the port's drain,
/// so the command a caller queued (`port_command/2` queues and returns) is written before `stop`.
#[test]
fn a_command_queued_before_a_close_is_written_by_the_close() {
    let (mut peer, fd) = write_end();
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(FdDriver::write_only(fd)))
        .expect("the port starts");

    // `erlang:port_command/2` queues the bytes and returns; nothing is written yet.
    assert_eq!(
        registry
            .port_command_with(&mut poller, id, b"hello", PortCommandOptions::EMPTY)
            .expect("an idle port takes the command"),
        crate::port::PortCommandOutcome::Done
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        5,
        "the command is queued, not written"
    );

    registry.close(&mut poller, id).expect("the close runs");

    assert!(
        !registry.contains(id),
        "a queue that fits finishes the close inside the call"
    );
    assert!(
        matches!(registry.is_closing(id), Err(BeamError::BadArg)),
        "a drained close never parks"
    );
    assert_eq!(
        round(&mut poller, &mut registry),
        vec![id],
        "the next round reports the `Closed` (minting it retires the number)"
    );
    assert_eq!(
        round(&mut poller, &mut registry),
        Vec::new(),
        "exactly once"
    );

    // The driver really wrote the command before `stop` closed the descriptor.
    let mut received = [0u8; 8];
    let n = peer
        .read(&mut received)
        .expect("the peer reads the command");
    assert_eq!(&received[..n], b"hello", "the queued command was written");

    // …and `stop` closed the descriptor, so the peer now sees end-of-file: nothing was lost
    // between the write and the teardown.
    let mut rest = [0u8; 8];
    assert_eq!(
        peer.read(&mut rest).expect("the peer reads to end-of-file"),
        0,
        "the descriptor is closed after the queue emptied"
    );
}
