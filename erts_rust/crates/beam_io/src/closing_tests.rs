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
//! The **flush-then-close** shape of a port, pinned structurally.
//!
//! A caller that sends and immediately closes must not lose the buffered output: the oracle
//! delivers all of it (`erlang:port_command(P, Data)` then `erlang:port_close(P)` — measured, a
//! `tee` child received 500 000/500 000 bytes while `port_close/1` returned `true` with 454 656
//! bytes still queued, so the drain is asynchronous, not part of the close's return). The C's
//! shape is `erts_deliver_port_exit` (`erts/emulator/beam/io.c:3936-3942`): a close with a
//! non-empty ioq sets `ERTS_PORT_SFLG_CLOSING` and calls `flush_port`, which runs the driver's
//! `flush` (only when the entry has one) and terminates the port **only if the ioq became empty**
//! (`io.c:3659-3700`); `terminate_port` is the sole caller of `drv->stop` (`io.c:3733-3752`), and
//! a CLOSING port is "in Limbo, waiting to pass on … as soon as the port queue gets empty"
//! (`io.c:896-900`, deferred half `erl_port.h:797-813`).
//!
//! Every case here runs over a plain-Rust [`ClosingDriver`] — no socket, no `cc` fixture, no
//! `erl`, no wall-clock sleeping — whose ordered call log, accepted bytes and drain policy are all
//! observable, so "the flush ran before the stop" is a fact about the driver, not about timing.

use std::cell::RefCell;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_process::PortId;

use crate::driver::{Driver, DriverContext};
use crate::poller::Poller;
use crate::port::{
    PortCommandOptions, PortCommandOutcome, CLOSING_DRAIN_STALL_LIMIT,
    DEFAULT_READINESS_RED_BUDGET as ROUND_BUDGET,
};
use crate::port_task::PortNotification;
use crate::PortRegistry;

/// How much of the output queue a [`ClosingDriver`] accepts per `output` call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drain {
    /// Everything the queue holds (a device that takes the write synchronously).
    All,
    /// At most one byte per call — a peer that reads far slower than it is written to.
    OneByte,
    /// Nothing, ever — a full socket nobody drains, or a driver that cannot make progress.
    Never,
}

/// A test driver: a drain policy, an optional discarding `flush`, an ordered call log and the
/// bytes it really accepted.
///
/// The log records `"flush"`, `"output"` and `"stop"` **in call order**, which is what pins the
/// C's ordering — the flush is the driver's one chance to discard before the drain, and `stop`
/// runs only once the queue is empty.
struct ClosingDriver {
    drain: Drain,
    flush_discards: bool,
    calls: Rc<RefCell<Vec<&'static str>>>,
    written: Rc<RefCell<Vec<u8>>>,
}

impl ClosingDriver {
    /// A driver plus the handles a test observes it through.
    fn new(drain: Drain, flush_discards: bool) -> (ClosingDriver, Handles) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let written = Rc::new(RefCell::new(Vec::new()));
        (
            ClosingDriver {
                drain,
                flush_discards,
                calls: Rc::clone(&calls),
                written: Rc::clone(&written),
            },
            Handles { calls, written },
        )
    }
}

/// The observation handles of a [`ClosingDriver`] that a test keeps after the port owns it.
struct Handles {
    calls: Rc<RefCell<Vec<&'static str>>>,
    written: Rc<RefCell<Vec<u8>>>,
}

impl Handles {
    /// The call log, in order.
    fn calls(&self) -> Vec<&'static str> {
        self.calls.borrow().clone()
    }

    /// How many times `call` appears in the log.
    fn count(&self, call: &str) -> usize {
        self.calls.borrow().iter().filter(|seen| **seen == call).count()
    }

    /// The bytes the driver really accepted (in order).
    fn written(&self) -> Vec<u8> {
        self.written.borrow().clone()
    }
}

impl Driver for ClosingDriver {
    fn name(&self) -> &str {
        "closing-test"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.calls.borrow_mut().push("output");
        let available = ctx.output().front().len();
        let take = match self.drain {
            Drain::All => available,
            Drain::OneByte => available.min(1),
            Drain::Never => 0,
        };
        if take == 0 {
            return Ok(0);
        }
        let bytes = ctx.output().front()[..take].to_vec();
        ctx.output().consume(take);
        self.written.borrow_mut().extend_from_slice(&bytes);
        Ok(take)
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn flush(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.calls.borrow_mut().push("flush");
        if self.flush_discards {
            // The C's discarding flush takes the bytes off the queue itself; the port verifies no
            // byte count for it (`flush` is `void` in the C), so consuming is the whole rule.
            let queued = ctx.output().front().len();
            ctx.output().consume(queued);
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        self.calls.borrow_mut().push("stop");
        Ok(())
    }
}

/// A fresh poll set.
fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// Opens a port over `driver` and queues `payload` on it the way `erlang:port_command/2` does.
fn open_and_command(
    poller: &mut Poller,
    driver: ClosingDriver,
    payload: &[u8],
) -> (PortRegistry, PortId) {
    let mut registry = PortRegistry::new();
    let id = registry.open(poller, Box::new(driver)).expect("the port opens");
    assert_eq!(
        registry
            .port_command_with(poller, id, payload, PortCommandOptions::EMPTY)
            .expect("an idle port takes the command"),
        PortCommandOutcome::Done
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        payload.len(),
        "`port_command` queues the bytes and returns; nothing is written yet"
    );
    (registry, id)
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

/// **The C's ordering**: `flush` (the driver's one chance to discard) runs **before** the drain,
/// and `stop` runs **after** it, once the queue is empty.
///
/// `flush_port` calls `drv->flush` and only then terminates a now-empty port
/// (`erts/emulator/beam/io.c:3659-3700`), and `terminate_port` is the sole caller of `drv->stop`
/// (`io.c:3733-3752`) — so a driver may rely on `flush` → (drain) → `stop`.
#[test]
fn a_close_flushes_before_it_drains_and_stops_last() {
    let mut poller = poller();
    let (driver, seen) = ClosingDriver::new(Drain::All, false);
    let (mut registry, id) = open_and_command(&mut poller, driver, b"hello");

    registry.close(&mut poller, id).expect("the close runs");

    assert_eq!(
        seen.calls(),
        vec!["flush", "output", "stop"],
        "flush precedes the drain, and stop follows an emptied queue"
    );
    assert_eq!(seen.written(), b"hello", "the driver really wrote the bytes");
    assert!(
        !registry.contains(id),
        "an emptied queue terminates the port inside the close (the C's `terminate_port`, \
         io.c:3733-3752) — it leaves the live map"
    );
    assert_eq!(
        round(&mut poller, &mut registry),
        vec![id],
        "the close removed the port, so the next round reports its `Closed`"
    );
    assert_eq!(round(&mut poller, &mut registry), Vec::new(), "exactly once");
}

/// A driver that accepts everything synchronously closes **inside** the `close` call: the port is
/// `Closed`, all bytes are written, `stop` ran — and the `Closed` notification still rides the
/// next port-task round (minting it is what retires the number, `io.c:3877-3880`'s deferred
/// report).
#[test]
fn a_fully_written_command_closes_inside_the_close_call() {
    let mut poller = poller();
    let (driver, seen) = ClosingDriver::new(Drain::All, false);
    let (mut registry, id) = open_and_command(&mut poller, driver, b"0123456789");

    registry.close(&mut poller, id).expect("the close runs");
    assert!(
        !registry.contains(id),
        "a fully written command finishes the close inside the call"
    );
    assert_eq!(seen.written(), b"0123456789");
    assert_eq!(seen.count("stop"), 1, "the teardown ran exactly once");
    assert!(
        matches!(registry.is_closing(id), Err(BeamError::BadArg)),
        "a drained close never parks — and a finished port is already out of the live map"
    );

    let closed = round(&mut poller, &mut registry);
    assert_eq!(
        closed,
        vec![id],
        "one round reports the close — the same round that finishes any park, not a second one"
    );
    assert_eq!(round(&mut poller, &mut registry), Vec::new(), "exactly once");
}

/// **The park**: a slow driver cannot write the whole command synchronously, so the port stays
/// live in [`crate::PortState::Closing`] — exactly the C's Limbo state, where the port "receives
/// in/out-put events so as soon as the port queue gets empty `terminate_port()` is called"
/// (`ertes/emulator/beam/io.c:896-900`) — refuses further commands (`io.c:1440` answers `badarg`
/// for `ERTS_PORT_SFLGS_INVALID_LOOKUP`, which includes `CLOSING`, `erl_port.h:351-355`), and is
/// drained by the rounds that follow. Only when the queue empties does `stop` run and its `Closed`
/// arrive — with every byte delivered, never truncated.
#[test]
fn a_slow_drain_parks_the_port_in_closing_and_the_rounds_finish_it() {
    let mut poller = poller();
    let (driver, seen) = ClosingDriver::new(Drain::OneByte, false);
    let payload = vec![b'x'; 200];
    let (mut registry, id) = open_and_command(&mut poller, driver, &payload);

    registry.close(&mut poller, id).expect("a park is not an error");
    assert!(
        registry.get(id).expect("live").is_closing(),
        "the port parks in `Closing` instead of dropping the queue"
    );
    assert!(registry.is_closing(id).expect("the port is live"));
    assert!(registry.contains(id), "a parked port is still a live port");
    let remaining = registry.get(id).expect("live").output_len();
    assert!(remaining > 0, "the remainder is still queued: {remaining}");
    assert!(
        registry
            .get(id)
            .expect("live")
            .has_pending_task(crate::PortTaskKind::Output),
        "an OUTPUT task stays pending so write readiness has something to run"
    );
    assert_eq!(
        seen.count("stop"),
        0,
        "`stop` waits for the queue, it does not pre-empt it"
    );

    // A command on a CLOSING port is the C's `badarg`.
    assert!(matches!(
        registry.port_command_with(&mut poller, id, b"late", PortCommandOptions::EMPTY),
        Err(BeamError::BadArg)
    ));

    // A second close is a no-op: no second flush, no second drain, no second stop.
    let flushes = seen.count("flush");
    registry.close(&mut poller, id).expect("a repeated close is a no-op");
    assert_eq!(seen.count("flush"), flushes, "the flush runs once per close");
    assert!(registry.get(id).expect("live").is_closing(), "still parked");

    // Successive rounds drain it; every one of them reports progress, so the stall bound never
    // fires. The round that reports `Closed` has written the whole payload.
    let mut rounds = 0;
    let mut last_remaining = remaining;
    loop {
        rounds += 1;
        assert!(rounds < 100, "the drain must terminate");
        let closed = round(&mut poller, &mut registry);
        match registry.get(id) {
            Some(port) => {
                assert!(
                    port.output_len() < last_remaining,
                    "every round makes progress ({} → {})",
                    last_remaining,
                    port.output_len()
                );
                last_remaining = port.output_len();
                assert!(
                    closed.is_empty(),
                    "a `Closed` is never minted while output is still queued"
                );
            }
            None => {
                assert_eq!(closed, vec![id], "the finishing round reports the close");
                break;
            }
        }
    }
    assert!(rounds > 1, "the drain really took several rounds");
    assert_eq!(seen.written(), payload, "every byte reached the driver");
    assert_eq!(seen.count("stop"), 1, "the teardown ran exactly once");
    assert_eq!(seen.count("flush"), 1, "and so did the flush");
    assert_eq!(round(&mut poller, &mut registry), Vec::new(), "exactly once");
}

/// A driver that never makes progress cannot hold the port for ever: after
/// [`CLOSING_DRAIN_STALL_LIMIT`] consecutive no-progress rounds the port is torn down anyway, the
/// remainder is dropped and the close is reported **exactly once**.
///
/// The C has no such bound (a CLOSING port waits for as long as the driver keeps it alive), so this
/// is the crate's own valve: holding the port number, its descriptors and its poller registrations
/// for ever would also mean never telling the owner the port closed.
#[test]
fn a_driver_that_never_drains_is_torn_down_after_the_stall_limit() {
    let mut poller = poller();
    let (driver, seen) = ClosingDriver::new(Drain::Never, false);
    let (mut registry, id) = open_and_command(&mut poller, driver, b"stuck");

    registry.close(&mut poller, id).expect("a park is not an error");
    assert!(registry.get(id).expect("live").is_closing());
    assert_eq!(seen.count("stop"), 0, "nothing is torn down yet");

    for attempt in 1..CLOSING_DRAIN_STALL_LIMIT {
        let closed = round(&mut poller, &mut registry);
        assert!(
            closed.is_empty(),
            "round {attempt} must still be waiting: a `Closed` never precedes the empty queue"
        );
        assert_eq!(
            registry.get(id).expect("live").output_len(),
            5,
            "the remainder is still queued, waiting for a driver that cannot write"
        );
        assert_eq!(seen.count("stop"), 0, "and `stop` still has not run");
    }

    // The limit-th no-progress round gives up.
    let closed = round(&mut poller, &mut registry);
    assert_eq!(closed, vec![id], "the limit round reports the close");
    assert!(!registry.contains(id), "the port is gone");
    assert_eq!(seen.count("stop"), 1, "`stop` ran once, on the give-up path");
    assert_eq!(seen.written(), Vec::new(), "the remainder was dropped");
    assert_eq!(round(&mut poller, &mut registry), Vec::new(), "exactly once");
}

/// A `flush` that discards the queue closes the port **inside** the `close` call: nothing is
/// written, the queue is empty afterwards and `stop` runs at once — the C's
/// `if (… && is_port_ioq_empty(p)) terminate_port(p);` (`io.c:3697-3700`).
#[test]
fn a_flush_that_discards_the_queue_closes_the_port_inside_the_close_call() {
    let mut poller = poller();
    let (driver, seen) = ClosingDriver::new(Drain::All, true);
    let (mut registry, id) = open_and_command(&mut poller, driver, b"obsolete");

    registry.close(&mut poller, id).expect("the close runs");

    assert_eq!(
        seen.calls(),
        vec!["flush", "stop"],
        "a discarding flush leaves nothing to drain"
    );
    assert_eq!(seen.written(), Vec::new(), "the bytes were dropped, not written");
    assert!(
        !registry.contains(id),
        "an emptied queue terminates the port inside the close, so it leaves the live map"
    );
    assert_eq!(round(&mut poller, &mut registry), vec![id]);
}

/// A `Closed` retires the port's **number** only when it is reported, so a `close` + `open` before
/// the reporting round cannot make the report name a reused, live port (`io.c:3877-3880`'s
/// deferred report; [`crate::PortRegistry::close`] states the rule).
#[test]
fn a_close_and_open_before_the_reporting_round_cannot_shadow_the_reported_number() {
    let mut poller = poller();
    let (driver, _seen) = ClosingDriver::new(Drain::All, false);
    let (mut registry, closed) = open_and_command(&mut poller, driver, b"bye");
    registry.close(&mut poller, closed).expect("the close runs");

    let (driver, seen) = ClosingDriver::new(Drain::All, false);
    let reopened = registry
        .open(&mut poller, Box::new(driver))
        .expect("a fresh port opens");
    assert_ne!(
        reopened, closed,
        "the pending report still owns the closed number"
    );

    assert_eq!(
        round(&mut poller, &mut registry),
        vec![closed],
        "the report names the port that closed"
    );
    assert!(
        registry.contains(reopened),
        "and it did not disturb the port opened meanwhile"
    );
    assert!(
        !registry.port_table().contains(closed),
        "the report is what retires the number"
    );
    assert_eq!(seen.count("stop"), 0, "the live port was never touched");
}

/// A port that leaves the registry closes exactly once, whichever path parked it, and its staged
/// completion/side-channel data is harvested by the same helper — so a `flush` that hands a final
/// delivery up is not lost with the port.
#[test]
fn the_close_path_reports_a_closed_exactly_once_per_port() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let (driver, fast) = ClosingDriver::new(Drain::All, false);
    let fast_id = registry.open(&mut poller, Box::new(driver)).expect("opens");
    registry
        .port_command_with(&mut poller, fast_id, b"a", PortCommandOptions::EMPTY)
        .expect("queued");
    registry.close(&mut poller, fast_id).expect("closes");

    let (driver, slow) = ClosingDriver::new(Drain::Never, false);
    let slow_id = registry.open(&mut poller, Box::new(driver)).expect("opens");
    registry
        .port_command_with(&mut poller, slow_id, b"b", PortCommandOptions::EMPTY)
        .expect("queued");
    registry.close(&mut poller, slow_id).expect("parks");

    let closed = round(&mut poller, &mut registry);
    assert_eq!(closed, vec![fast_id], "only the finished port is reported");
    assert!(registry.contains(slow_id), "the parked one is still live");

    assert_eq!(fast.count("stop"), 1);
    assert_eq!(slow.count("stop"), 0);
    // The unknown-id fault is unchanged.
    assert!(matches!(
        registry.close(&mut poller, PortId::new(9999)),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        registry.is_closing(PortId::new(9999)),
        Err(BeamError::BadArg)
    ));
}
