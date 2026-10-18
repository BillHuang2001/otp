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
//! The **resource** half of the busy-port / peer-close surfaces: the registry seams a runtime
//! calls — [`PortRegistry::port_command_with`], [`PortRegistry::is_busy`] and
//! [`PortRegistry::port_closed`] — resolved over the id table.
//!
//! The decision table itself is [`Port::command`]'s and is pinned in the sibling
//! `port_command_tests.rs` (over a plain Rust driver) and in `tests/abi_busy.rs` (over a real
//! compiled C driver). What is asserted here is only what the *registry* adds: the unknown-id
//! `BadArg` every other seam answers, the live-port answer, and that the old
//! [`PortRegistry::port_command`] entry point keeps its `Result<()>` shape while routing through
//! the same table.

use std::cell::Cell;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_process::PortId;

use crate::driver::{Driver, DriverContext, DriverFlags};
use crate::poller::Poller;
use crate::port::{PortCommandOptions, PortCommandOutcome};
use crate::port_task::{PortTaskKind, PORT_REDS_EXECUTE};
use crate::registry::PortRegistry;

/// The state a test flips while the driver is owned by a port.
#[derive(Default)]
struct Flags {
    busy: Cell<bool>,
    closed: Cell<bool>,
}

/// A driver that reports the flags a test sets and consumes whatever it is handed.
struct TestDriver {
    flags: Rc<Flags>,
}

impl Driver for TestDriver {
    fn name(&self) -> &str {
        "registry-busy-test"
    }

    fn flags(&self) -> DriverFlags {
        DriverFlags::empty()
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
        let len = ctx.output().front().len();
        ctx.output().consume(len);
        Ok(len)
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// A registry holding one open port over a [`TestDriver`], plus the flag handle.
fn registry_with_test_port(poller: &mut Poller) -> (PortRegistry, PortId, Rc<Flags>) {
    let flags = Rc::new(Flags::default());
    let driver = Box::new(TestDriver {
        flags: Rc::clone(&flags),
    });
    let mut registry = PortRegistry::new();
    let id = registry.open(poller, driver).expect("the port opens");
    (registry, id, flags)
}

/// An id no port was ever opened under.
fn unknown() -> PortId {
    PortId::new(9999)
}

/// Every one of the three seams answers the same `badarg` the rest of the registry does.
#[test]
fn the_unknown_id_is_bad_arg_for_all_three_seams() {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    assert!(matches!(registry.is_busy(unknown()), Err(BeamError::BadArg)));
    assert!(matches!(
        registry.port_closed(unknown()),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        registry.port_command_with(&mut poller, unknown(), b"x", PortCommandOptions::EMPTY),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        registry.port_command(&mut poller, unknown(), b"x", PortCommandOptions::EMPTY),
        Err(BeamError::BadArg)
    ));
}

/// A live, idle port: not busy, not closed, and it takes the command.
#[test]
fn a_live_idle_port_reports_idle_and_takes_the_command() {
    let mut poller = Poller::new().expect("a poll set");
    let (mut registry, id, _flags) = registry_with_test_port(&mut poller);
    assert_eq!(registry.is_busy(id).expect("a live port"), false);
    assert_eq!(registry.port_closed(id).expect("a live port"), false);

    assert_eq!(
        registry
            .port_command_with(&mut poller, id, b"hi", PortCommandOptions::EMPTY)
            .expect("an idle port takes the command"),
        PortCommandOutcome::Done
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 2);
    let outcome = registry
        .execute_port_tasks(&mut poller, PORT_REDS_EXECUTE)
        .expect("the round runs");
    assert!(outcome.ran >= 1, "the write ran");
    assert_eq!(registry.get(id).expect("live").output_len(), 0);
}

/// The busy state is the driver's, and the registry reads it back unchanged; `port_command`'s
/// `Result<()>` folds a `Busy` outcome into `Ok(())` **without** queueing, which is the shape its
/// existing callers rely on.
#[test]
fn a_busy_port_is_reported_busy_and_the_legacy_entry_point_queues_nothing() {
    let mut poller = Poller::new().expect("a poll set");
    let (mut registry, id, flags) = registry_with_test_port(&mut poller);
    flags.busy.set(true);
    assert!(
        registry.is_busy(id).expect("a live port"),
        "the driver's state, read back"
    );

    registry
        .port_command(&mut poller, id, b"hi", PortCommandOptions::EMPTY)
        .expect("the legacy entry point keeps its Result<()> shape");
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        0,
        "a busy port's command is never queued"
    );
    assert!(
        !registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output),
        "nor is a write scheduled"
    );

    // `[force]` is refused by the driver's flag word, through the same entry point.
    let err = registry
        .port_command(
            &mut poller,
            id,
            b"hi",
            PortCommandOptions::EMPTY.with_force(true),
        )
        .expect_err("a driver without SOFT_BUSY refuses a forced command");
    assert_eq!(err.as_atom(), Some("notsup"));
}

/// The peer-close probe is the driver's latch, and it is not affected by the busy state.
#[test]
fn the_peer_close_probe_reports_the_drivers_latch() {
    let mut poller = Poller::new().expect("a poll set");
    let (registry, id, flags) = registry_with_test_port(&mut poller);
    assert!(!registry.port_closed(id).expect("a live port"));
    flags.closed.set(true);
    assert!(
        registry.port_closed(id).expect("a live port"),
        "the latch is read back"
    );
    assert!(
        !registry.is_busy(id).expect("a live port"),
        "the two are independent"
    );
}
