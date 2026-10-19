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

//! The **driver-downcast seam**: [`Port::driver_as`] and [`PortRegistry::driver_as`], the
//! read-only route from a port id to the *concrete* driver behind its `Box<dyn Driver>`.
//!
//! The seam exists for one consumer, the C's `tcp_inet_copy`
//! (`erts/emulator/drivers/common/inet_drv.c:11795`): accepting a connection must build the
//! second, connected port's driver from the **listener's** option set, so
//! [`TcpInetDriver::from_accepted`] needs the listener's `&TcpInetDriver` — which the port layer
//! otherwise erases (it publishes only `driver_name()`).
//!
//! What is asserted here is that the downcast returns the **live driver instance the port owns**,
//! not a copy and not merely *a* driver of the right type:
//!
//! * a marker the constructor set **before boxing** is visible through the reference, so the value
//!   really travelled through `Box<dyn Driver>`;
//! * a field `start` flipped **after boxing** ([`MarkerDriver::started`]) is visible too, so the
//!   reference is that same boxed object;
//! * a counter the port's own write path moves ([`PortRegistry::output`] → `Driver::output`)
//!   reads back through the downcast — one instance, two handles.
//!
//! The refusals are pinned as well: another concrete type, an id that names no port, and a port
//! that has already been closed all answer `None`. Nothing here panics, and none of it touches
//! the existing suites' assertions.

use std::cell::Cell;

use beam_common::Result;
use beam_process::PortId;

use crate::driver::{Driver, DriverContext};
use crate::drivers::{make_tcp_inet, InetState, TcpInetDriver, UdpInetDriver};
use crate::poller::Poller;
use crate::registry::PortRegistry;

/// The value the driver's constructor seeds, before the driver is boxed.
///
/// Deliberately distinctive: it is not produced anywhere else in this suite, so seeing it through
/// a downcast reference is proof the pre-boxing field survived the type erasure.
const MARKER: u64 = 0x5EED_C0DE_1234_5678;

/// A plain Rust driver whose state is fully observable, so a downcast can be checked for
/// **identity** rather than for type alone.
struct MarkerDriver {
    /// Set by the constructor, i.e. before the driver is boxed into the port.
    marker: u64,
    /// Set by [`Driver::start`], i.e. **after** the driver was boxed and owned by the port.
    started: bool,
    /// Moved by [`Driver::output`], which only the port's own write path calls.
    writes: Cell<usize>,
}

impl MarkerDriver {
    /// A fresh driver with `marker` and no writes.
    fn new(marker: u64) -> MarkerDriver {
        MarkerDriver {
            marker,
            started: false,
            writes: Cell::new(0),
        }
    }
}

impl Driver for MarkerDriver {
    fn name(&self) -> &str {
        "registry-downcast-marker"
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        self.started = true;
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let len = ctx.output().front().len();
        ctx.output().consume(len);
        self.writes.set(self.writes.get() + 1);
        Ok(len)
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// A second concrete driver type, so a mismatch is a *type* mismatch rather than "no driver".
struct OtherDriver;

impl Driver for OtherDriver {
    fn name(&self) -> &str {
        "registry-downcast-other"
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

/// An id no port was ever opened under.
fn unknown() -> PortId {
    PortId::new(9999)
}

/// Opens one [`MarkerDriver`] through the registry, returning the registry, the id and the poller.
fn registry_with_marker() -> (PortRegistry, PortId, Poller) {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(MarkerDriver::new(MARKER)))
        .expect("the port opens");
    (registry, id, poller)
}

/// The pre-boxing marker and the post-boxing `started` flag are both visible: the reference is the
/// driver the port owns, reached through the type erasure.
#[test]
fn the_downcast_reaches_the_driver_the_registry_boxed() {
    let (registry, id, _poller) = registry_with_marker();
    let driver = registry
        .driver_as::<MarkerDriver>(id)
        .expect("the live port's own driver");
    assert_eq!(
        driver.marker, MARKER,
        "the value the constructor set before boxing survived the erasure"
    );
    assert!(
        driver.started,
        "`start` ran on the port's own instance, so this is that instance and not a copy"
    );
}

/// One instance, two handles: a write driven through the registry moves the driver's own counter,
/// which the downcast reference (and [`Port::driver_as`]) reads straight back.
#[test]
fn the_downcast_shares_state_with_the_port_that_drives_it() {
    let (mut registry, id, mut poller) = registry_with_marker();
    assert_eq!(
        registry
            .driver_as::<MarkerDriver>(id)
            .expect("the live port's driver")
            .writes
            .get(),
        0
    );

    registry
        .output(&mut poller, id, b"abc")
        .expect("the write runs inline");
    assert_eq!(
        registry
            .driver_as::<MarkerDriver>(id)
            .expect("the live port's driver")
            .writes
            .get(),
        1,
        "the port's write path ran on this very driver"
    );

    // The port-side accessor is the same seam, and agrees with the registry's.
    let port = registry.get(id).expect("live");
    assert_eq!(
        port.driver_as::<MarkerDriver>()
            .expect("the same driver")
            .writes
            .get(),
        registry
            .driver_as::<MarkerDriver>(id)
            .expect("the same driver")
            .writes
            .get(),
        "`Port::driver_as` and `PortRegistry::driver_as` see one instance"
    );
}
/// The generic view is always available, and names the concrete driver — which is *why* a
/// downcast is needed at all: the name cannot be turned back into a type.
#[test]
fn the_generic_view_names_the_driver_the_downcast_reaches() {
    let (registry, id, _poller) = registry_with_marker();
    let port = registry.get(id).expect("live");
    assert_eq!(port.driver().name(), "registry-downcast-marker");
    assert_eq!(port.driver().name(), port.driver_name());
    assert_eq!(
        port.driver_as::<MarkerDriver>().expect("the concrete type").name(),
        port.driver_name()
    );
}

/// A driver of another concrete type answers `None` — from both seams, in both directions.
#[test]
fn a_driver_of_another_concrete_type_answers_none() {
    let (registry, marker_id, _poller) = registry_with_marker();
    assert!(
        registry.driver_as::<OtherDriver>(marker_id).is_none(),
        "a marker port is not an OtherDriver"
    );
    assert!(registry
        .get(marker_id)
        .expect("live")
        .driver_as::<OtherDriver>()
        .is_none());

    let mut poller = Poller::new().expect("a poll set");
    let mut registry = registry;
    let other_id = registry
        .open(&mut poller, Box::new(OtherDriver))
        .expect("the second port opens");
    assert!(
        registry.driver_as::<MarkerDriver>(other_id).is_none(),
        "an OtherDriver port is not a MarkerDriver"
    );
    assert!(registry.driver_as::<OtherDriver>(other_id).is_some());
}

/// An id that names no port answers `None` — the read-only seam's refusal, not a `badarg`.
#[test]
fn an_unknown_id_answers_none() {
    let (registry, _id, _poller) = registry_with_marker();
    assert!(registry.driver_as::<MarkerDriver>(unknown()).is_none());
    assert!(registry.driver_as::<TcpInetDriver>(unknown()).is_none());
}

/// A closed port is gone from the live map, so its driver is no longer reachable.
#[test]
fn a_closed_port_answers_none() {
    let (mut registry, id, mut poller) = registry_with_marker();
    assert!(registry.driver_as::<MarkerDriver>(id).is_some());

    registry.close(&mut poller, id).expect("the port closes");
    assert!(
        registry.driver_as::<MarkerDriver>(id).is_none(),
        "a closed port has no driver to downcast"
    );
    assert_eq!(registry.get(id).map(|port| port.driver_name()), None);
}

/// The seam over a **real** built-in driver: the registry opens the `"tcp_inet"` factory's
/// socket-less driver, and the downcast reaches `TcpInetDriver` and only that type.
///
/// This is exactly the lookup the accepted-socket adoption performs on a listener port before it
/// calls [`TcpInetDriver::from_accepted`]; the socket-less state keeps the case hermetic (no
/// descriptor, no `erl`, no wait).
#[test]
fn the_real_tcp_inet_driver_downcasts_and_its_datagram_sibling_does_not() {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, make_tcp_inet().expect("the tcp_inet factory"))
        .expect("the port opens");

    let driver = registry
        .driver_as::<TcpInetDriver>(id)
        .expect("the port's own tcp_inet driver");
    assert_eq!(driver.state(), InetState::Closed, "the factory is socket-less");
    assert_eq!(driver.socket(), None);
    assert!(!driver.peer_closed(), "nothing has closed yet");

    assert!(
        registry.driver_as::<UdpInetDriver>(id).is_none(),
        "the datagram driver is a different concrete type"
    );
    assert_eq!(registry.get(id).expect("live").driver().name(), "tcp_inet");
}