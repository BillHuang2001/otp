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

//! Integration tests for the two additive `beam_io` **driver seams**:
//!
//! * the **timer-arming seam** — [`Driver::next_timeout`] plus the port/registry machinery
//!   that arms (or clears) the port's `TIMEOUT` task around every driver interaction, and the
//!   explicit [`PortRegistry::schedule_timeout`] / [`PortRegistry::cancel_timeout`] entry
//!   points; and
//! * the **port-exit notification channel** — [`Driver::port_exit`] plus
//!   [`PortRegistry::notify_port_exit`].
//!
//! Each test drives a small in-test driver through the real [`PortRegistry`] and the real port
//! task round, so the seams are exercised end to end (arming, gating, firing, teardown) rather
//! than in isolation.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_common::{BeamError, Result};
use beam_io::prelude::*;
use beam_process::PortId;

/// A driver that overrides only the required methods. It declares no timer and ignores the
/// port-exit notification, so it pins the *additive defaults* of the new seams.
struct NopDriver;

impl Driver for NopDriver {
    fn name(&self) -> &str {
        "nop"
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
        Ok(())
    }
}

/// A driver that exercises the timer seam: it declares a deadline through
/// [`Driver::next_timeout`], counts the `timeout` callbacks it observes, clears its deadline
/// when the timer fires, and changes its deadline on the `arm`/`disarm` control commands.
struct TimeoutDriver {
    deadline: Option<Instant>,
    /// How many times [`Driver::timeout`] fired.
    fires: Rc<Cell<usize>>,
    /// How many times the port consulted [`Driver::next_timeout`] (the refresh).
    polls: Rc<Cell<usize>>,
}

impl TimeoutDriver {
    fn new(
        deadline: Option<Instant>,
        fires: Rc<Cell<usize>>,
        polls: Rc<Cell<usize>>,
    ) -> TimeoutDriver {
        TimeoutDriver {
            deadline,
            fires,
            polls,
        }
    }
}

impl Driver for TimeoutDriver {
    fn name(&self) -> &str {
        "timeout-test"
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

    fn next_timeout(&self) -> Option<Instant> {
        self.polls.set(self.polls.get() + 1);
        self.deadline
    }

    fn timeout(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        self.fires.set(self.fires.get() + 1);
        // One-shot: clear the deadline so the port drops the timer.
        self.deadline = None;
        Ok(())
    }

    fn control(
        &mut self,
        _ctx: &mut DriverContext<'_>,
        command: &str,
        _arg: &[u8],
    ) -> Result<Vec<u8>> {
        match command {
            "arm" => self.deadline = Some(Instant::now() + Duration::from_secs(60)),
            "disarm" => self.deadline = None,
            _ => {}
        }
        Ok(Vec::new())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// A driver that records every [`PortExitReason`] it is told and counts its `stop` calls, so the
/// exit-notification channel and its separation from teardown can be observed.
struct ExitRecordingDriver {
    reasons: Rc<RefCell<Vec<PortExitReason>>>,
    stops: Rc<Cell<usize>>,
}

impl ExitRecordingDriver {
    fn new(
        reasons: Rc<RefCell<Vec<PortExitReason>>>,
        stops: Rc<Cell<usize>>,
    ) -> ExitRecordingDriver {
        ExitRecordingDriver { reasons, stops }
    }
}

impl Driver for ExitRecordingDriver {
    fn name(&self) -> &str {
        "exit-test"
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

    fn port_exit(&mut self, _ctx: &mut DriverContext<'_>, reason: PortExitReason) -> Result<()> {
        self.reasons.borrow_mut().push(reason);
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        self.stops.set(self.stops.get() + 1);
        Ok(())
    }
}

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// A driver that **reports** a write count it never consumed from its output queue — the contract
/// violation the port guards against, because the unconsumed bytes would otherwise be written
/// again by the next drive (duplicated output).
struct LyingWriteDriver;

impl Driver for LyingWriteDriver {
    fn name(&self) -> &str {
        "lying-write"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        // Counts every queued byte as written without a single `consume`.
        Ok(ctx.output().len())
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// A driver that writes exactly one byte per call and **never** arms write readiness: a short
/// write, which leaves the rest of the queue for the port to re-drive.
struct OneByteWriterDriver;

impl Driver for OneByteWriterDriver {
    fn name(&self) -> &str {
        "one-byte"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        if ctx.output().is_empty() {
            return Ok(0);
        }
        ctx.output().consume(1);
        Ok(1)
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

// --- GAP 1: the driver timer-arming seam ------------------------------------

#[test]
fn a_driver_declared_deadline_fires_only_after_it_passes() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let fires = Rc::new(Cell::new(0usize));
    let polls = Rc::new(Cell::new(0usize));
    let deadline = Instant::now() + Duration::from_millis(200);

    let id = registry
        .open(
            &mut poller,
            Box::new(TimeoutDriver::new(
                Some(deadline),
                fires.clone(),
                polls.clone(),
            )),
        )
        .expect("the port opens");

    // `start` is followed by the automatic refresh, so the declared deadline is armed.
    assert!(
        polls.get() >= 1,
        "the port consulted the driver's declared deadline"
    );
    assert_eq!(
        registry.get(id).expect("live").next_timeout(),
        Some(deadline),
        "the port mirrors the driver's declared deadline"
    );
    assert!(registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Timeout));

    // Immediately: the timer is not due, so the round runs nothing and the driver stays silent.
    let early = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(early.ran, 0, "a timer must not run before its deadline");
    assert_eq!(fires.get(), 0, "the driver's timeout did not fire early");
    assert!(
        registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Timeout),
        "the not-yet-due timer stays queued"
    );

    // After the deadline: the timer runs exactly once and the driver observes it.
    std::thread::sleep(Duration::from_millis(300));
    let late = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(late.ran, 1, "the due timer ran");
    assert_eq!(fires.get(), 1, "the driver's timeout fired once");
    assert_eq!(
        registry.get(id).expect("live").next_timeout(),
        None,
        "the driver cleared its deadline when the timer fired"
    );
    assert!(
        !registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Timeout),
        "a cleared deadline leaves no timer queued"
    );

    registry.close(&mut poller, id).expect("closes");
}

#[test]
fn a_control_command_re_arms_or_clears_the_port_timer() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let fires = Rc::new(Cell::new(0usize));
    let polls = Rc::new(Cell::new(0usize));

    let id = registry
        .open(
            &mut poller,
            Box::new(TimeoutDriver::new(None, fires.clone(), polls.clone())),
        )
        .expect("the port opens");

    // No declared deadline ⇒ no timer, and the registry reports no deadline to wait for.
    assert!(!registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Timeout));
    assert_eq!(registry.next_deadline(), None);

    // A control command that changes the driver's deadline is honoured: the port re-arms from
    // `next_timeout` after the interaction.
    registry
        .port_control(&mut poller, id, "arm", b"")
        .expect("arm reaches the driver");
    assert!(registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Timeout));
    assert!(registry.get(id).expect("live").next_timeout().is_some());
    assert!(registry.next_deadline().is_some());

    // And clearing it through the same interaction drops the timer again.
    registry
        .port_control(&mut poller, id, "disarm", b"")
        .expect("disarm reaches the driver");
    assert!(!registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Timeout));
    assert_eq!(registry.next_deadline(), None);

    registry.close(&mut poller, id).expect("closes");
}

#[test]
fn the_registry_arms_and_cancels_a_port_timer() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(NopDriver))
        .expect("the port opens");

    // A driver that declares nothing leaves no timer after the opening refresh.
    assert!(!registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Timeout));

    // Arm a timer that is not yet due: the round runs nothing.
    let not_yet = Instant::now() + Duration::from_millis(100);
    registry
        .schedule_timeout(id, not_yet)
        .expect("the timer arms");
    assert!(registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Timeout));
    let early = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(early.ran, 0, "the timer is not due yet");

    // `schedule_timeout` re-points the pending timer (unlike the dedupe-by-kind queue enqueue):
    // pushing it far into the future means the original deadline passing must not fire it.
    registry
        .schedule_timeout(id, Instant::now() + Duration::from_secs(60))
        .expect("the timer re-points");
    std::thread::sleep(Duration::from_millis(150));
    let still_early = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(
        still_early.ran, 0,
        "the re-pointed timer is not fired by the superseded deadline"
    );

    // Cancel drops it, and reports whether one was pending.
    assert!(registry.cancel_timeout(id).expect("cancel succeeds"));
    assert!(!registry.cancel_timeout(id).expect("cancel succeeds"));
    assert!(!registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Timeout));

    // Unknown ids are argument faults, never silent no-ops.
    let missing = PortId::new(9999);
    assert!(matches!(
        registry.schedule_timeout(missing, Instant::now()),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        registry.cancel_timeout(missing),
        Err(BeamError::BadArg)
    ));

    registry.close(&mut poller, id).expect("closes");
}

// --- GAP 2: the port-exit notification channel ------------------------------

#[test]
fn a_port_exit_notification_reaches_the_driver_once() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let reasons = Rc::new(RefCell::new(Vec::new()));
    let stops = Rc::new(Cell::new(0usize));

    let id = registry
        .open(
            &mut poller,
            Box::new(ExitRecordingDriver::new(reasons.clone(), stops.clone())),
        )
        .expect("the port opens");

    registry
        .notify_port_exit(&mut poller, id, PortExitReason::OwnerExited)
        .expect("the exit notification is delivered");
    assert_eq!(*reasons.borrow(), vec![PortExitReason::OwnerExited]);
    assert_eq!(stops.get(), 0, "notify_port_exit is not stop");

    // At most once: a second notification never reaches the driver again, whatever the reason.
    registry
        .notify_port_exit(&mut poller, id, PortExitReason::Closed)
        .expect("a repeat is a no-op");
    assert_eq!(
        *reasons.borrow(),
        vec![PortExitReason::OwnerExited],
        "the notification is delivered exactly once"
    );

    // The port stays live; a later close still runs the teardown `stop`, so the two are
    // independent and a `port_exit` before a `stop` is well-defined.
    assert!(registry.contains(id));
    registry.close(&mut poller, id).expect("closes");
    assert_eq!(stops.get(), 1, "close runs the driver's stop");
    assert!(!registry.contains(id));

    // A released id is no longer notifiable.
    assert!(matches!(
        registry.notify_port_exit(&mut poller, id, PortExitReason::Normal),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn a_driver_without_the_port_exit_hook_still_accepts_the_notification() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(NopDriver))
        .expect("the port opens");

    // The default `port_exit` is a no-op, so the notification succeeds and the port stays live.
    registry
        .notify_port_exit(&mut poller, id, PortExitReason::Normal)
        .expect("the default hook accepts every reason");
    assert!(registry.contains(id));

    // A close with no preceding notification is equally well-defined.
    registry.close(&mut poller, id).expect("closes");
    assert!(!registry.contains(id));

    // An unknown id is an argument fault.
    assert!(matches!(
        registry.notify_port_exit(&mut poller, PortId::new(4242), PortExitReason::Closed),
        Err(BeamError::BadArg)
    ));
}

// --- the write-count contract and the short-write re-drive -------------------

/// A driver that reports a byte count it did not consume is caught, on **both** write paths, by
/// the typed accounting guard instead of silently leaving the bytes to be written again.
#[test]
fn a_driver_reporting_a_count_it_did_not_consume_is_caught() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(LyingWriteDriver))
        .expect("the port opens");

    // The inline write path (`PortRegistry::output` -> `Port::output`): the driver claims both
    // bytes are written while the queue still holds them.
    let inline = registry.output(&mut poller, id, b"hi");
    assert!(
        inline.is_err(),
        "a count the driver did not consume is a typed error, not a silent success"
    );
    assert!(
        matches!(&inline, Err(BeamError::Raise(_))),
        "the mismatch is the crate's typed raise diagnostic: {inline:?}"
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        2,
        "the unconsumed bytes are still queued (they were not silently dropped)"
    );

    // The scheduled-task write path (`PortTaskKind::Output` -> `Port::ready_output`): the same
    // guard fires when the round runs the task.
    registry
        .port_command(&mut poller, id, b"xy", PortCommandOptions::new())
        .expect("the command is queued");
    let round = registry.execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET);
    assert!(
        matches!(&round, Err(BeamError::Raise(_))),
        "the task round surfaces the same accounting error: {round:?}"
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        4,
        "nothing was written by the lying driver"
    );

    registry.close(&mut poller, id).expect("closes");
}

/// A short inline write schedules an [`PortTaskKind::Output`] task, so the remainder is re-driven
/// by the next port-task round even though the driver never armed write readiness.
#[test]
fn a_short_inline_write_schedules_an_output_task_for_the_remainder() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(OneByteWriterDriver))
        .expect("the port opens");

    // The driver takes one byte of the four and arms nothing.
    assert_eq!(
        registry.output(&mut poller, id, b"abcd").expect("writes"),
        1,
        "the driver wrote one byte"
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        3,
        "the remainder stays queued"
    );
    assert!(
        registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output),
        "the unfinished write schedules an OUTPUT task without any readiness arming"
    );

    // The scheduled task runs the driver again, making progress on the remainder.
    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the OUTPUT task runs");
    assert_eq!(round.ran, 1, "the scheduled OUTPUT task ran");
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        2,
        "the round drained one more byte"
    );

    registry.close(&mut poller, id).expect("closes");
}
