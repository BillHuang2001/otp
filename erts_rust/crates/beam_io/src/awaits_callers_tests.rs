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
//! The **wait-side** async predicate: [`Driver::awaits_callers_async_op`] and its two forwarding
//! surfaces ([`Port::awaits_callers_async_op`], [`PortRegistry::awaits_callers_async_op`]).
//!
//! Everything here runs over a plain-Rust fake driver — no socket, no `cc`, no `erl` — the same
//! shape as `active_mode_tests.rs`/`completion_tests.rs`: the seam is *driver-agnostic*, so the
//! concrete `tcp_inet`/`udp_inet` wiring (accept-inclusive) and the loaded-`DriverPort` wiring
//! (`false`) are those nodes' own suites, not this one's. The `DriverPort` leg is unreachable from
//! a source-module unit test: it needs a real `cc`-built `.so` behind a
//! `beam_abi::driver::loader` handle, which lives in the integration-test node.
use std::cell::Cell;
use beam_common::Result;
use beam_process::PortId;
use crate::driver::{Driver, DriverContext};
use crate::poller::Poller;
use crate::port::Port;
use crate::registry::PortRegistry;
/// A driver that overrides **only** the drive predicate — the shape of a driver that cannot tell
/// the two questions apart, so the wait-side answer must be the identity.
struct DriveOnlyDriver {
    pending: Cell<bool>,
}
impl Driver for DriveOnlyDriver {
    fn name(&self) -> &str {
        "drive-only-test"
    }
    fn has_pending_async(&self) -> bool {
        self.pending.get()
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
/// A driver that knows the two answers separately — the shape a listener (drive *and* wait) or a
/// loaded C driver (drive only) has.
struct SplitDriver {
    name: &'static str,
    drive: bool,
    wait: bool,
}
impl Driver for SplitDriver {
    fn name(&self) -> &str {
        self.name
    }
    fn has_pending_async(&self) -> bool {
        self.drive
    }
    fn awaits_callers_async_op(&self) -> bool {
        self.wait
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
/// Opens one drive-only port and hands back the flag the test drives.
fn registry_with_drive_only_port(poller: &mut Poller) -> (PortRegistry, PortId) {
    let mut registry = PortRegistry::new();
    let id = registry
        .open(
            poller,
            Box::new(DriveOnlyDriver {
                pending: Cell::new(false),
            }),
        )
        .expect("the port opens");
    (registry, id)
}
/// The default is the identity: a driver that cannot tell the two questions apart reports the
/// drive answer for both, so a wait can never give up on a still-pending reply.
#[test]
fn the_default_follows_the_drive_predicate() {
    let mut poller = Poller::new().expect("a poll set");
    let (mut registry, id) = registry_with_drive_only_port(&mut poller);
    assert!(!registry.awaits_callers_async_op(), "nothing is pending yet");
    assert!(!registry.get(id).expect("live").has_pending_async());
    assert!(!registry.get(id).expect("live").awaits_callers_async_op());
    // Flip the driver's own pending bit: both accessors follow it, unchanged.
    let port = registry.get_mut(id).expect("live");
    port.driver_as::<DriveOnlyDriver>()
        .expect("the port's own driver type")
        .pending
        .set(true);
    assert!(registry.get(id).expect("live").has_pending_async());
    assert!(
        registry.get(id).expect("live").awaits_callers_async_op(),
        "the default is `self.has_pending_async()`, so a driver that cannot tell the two apart \
         reports the drive answer rather than claiming nobody waits"
    );
    assert!(
        registry.awaits_callers_async_op(),
        "and the registry-wide probe agrees"
    );
}
/// A driver that overrides **both** members is the authority for each: the drive question and the
/// wait question are answered independently, through the port and through the registry.
#[test]
fn each_accessor_reports_its_own_answer() {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    // Port one: a listener's shape — a caller is parked on a queued accept, so both answer `true`.
    let listener = registry
        .open(
            &mut poller,
            Box::new(SplitDriver {
                name: "listener-test",
                drive: true,
                wait: true,
            }),
        )
        .expect("the port opens");
    // Port two: a loaded C driver's shape — a `driver_async` job is in flight, so the round must
    // drive it, but its completion goes to the driver's own `ready_async`, so nobody is waiting.
    let loaded = registry
        .open(
            &mut poller,
            Box::new(SplitDriver {
                name: "loaded-test",
                drive: true,
                wait: false,
            }),
        )
        .expect("the port opens");
    assert!(registry.get(listener).expect("live").has_pending_async());
    assert!(
        registry
            .get(listener)
            .expect("live")
            .awaits_callers_async_op(),
        "the listener's queued accept is an answer a caller is parked on"
    );
    assert!(registry.get(loaded).expect("live").has_pending_async());
    assert!(
        !registry.get(loaded).expect("live").awaits_callers_async_op(),
        "an in-flight `driver_async` job is work to drive, not an answer owed to a waiter"
    );
    assert!(
        registry.awaits_callers_async_op(),
        "one port owes a reply, so the registry answers `true`"
    );
    // With only the loaded port left, the registry-wide probe answers `false` even though that
    // port still has work to drive — the split the two predicates exist for.
    registry.close(&mut poller, listener).expect("the port closes");
    assert!(registry.get(loaded).expect("live").has_pending_async());
    assert!(
        !registry.awaits_callers_async_op(),
        "no live port owes a reply any more, so a caller-blocking wait must not be kept alive"
    );
}
/// The registry-wide probe is live-port only: a closed port contributes nothing, exactly like every
/// other registry drain (`close` removes the port from the live map immediately).
#[test]
fn the_registry_probe_is_live_port_only() {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    assert!(
        !registry.awaits_callers_async_op(),
        "an empty registry owes nobody an answer"
    );
    let waiting = registry
        .open(
            &mut poller,
            Box::new(SplitDriver {
                name: "waiting-test",
                drive: true,
                wait: true,
            }),
        )
        .expect("the port opens");
    assert!(registry.awaits_callers_async_op());
    registry.close(&mut poller, waiting).expect("the port closes");
    assert!(
        !registry.awaits_callers_async_op(),
        "the port is gone from the live map, so its outstanding op no longer counts"
    );
}
/// Both accessors are pure probes: they take `&self`, never touch the driver context, and are safe
/// to call while iterating a registry (the round's own probe style).
#[test]
fn the_probes_take_a_shared_borrow_and_do_not_consume_anything() {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let id = registry
        .open(
            &mut poller,
            Box::new(SplitDriver {
                name: "borrow-test",
                drive: true,
                wait: true,
            }),
        )
        .expect("the port opens");
    let port: &Port = registry.get(id).expect("live");
    assert!(port.has_pending_async());
    assert!(port.awaits_callers_async_op());
    // Repeated calls answer the same thing: a probe is not a drain.
    assert!(registry.awaits_callers_async_op());
    assert!(registry.awaits_callers_async_op());
}
