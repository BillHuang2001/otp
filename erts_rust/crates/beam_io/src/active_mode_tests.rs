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
//! The **driver-agnostic** `{active, …}` seam: the value types ([`ActiveMode`], [`ActiveSpend`]),
//! the two defaulted [`Driver`] members that publish them (`active_mode`/`take_active_spends`) and
//! the two registry seams over them ([`PortRegistry::active_mode`] /
//! [`PortRegistry::take_active_spends`]).
//!
//! Everything here runs over a plain-Rust fake driver — no socket, no `cc`, no `erl` — exactly like
//! `registry_busy_tests.rs`: the point of the seam is that it is *driver-agnostic*, so the concrete
//! `tcp_inet`/`udp_inet` wiring is the drivers node's own suite, not this one's.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_process::PortId;

use crate::driver::{Driver, DriverContext};
use crate::poller::Poller;
use crate::port_task::{ActiveMode, ActiveSpend};
use crate::registry::PortRegistry;

/// The `{active, …}` state a test driver reports, plus the spends it has staged for its owner.
///
/// The two integers are the driver's raw store — `active` (the C's `inet.active`, an `i32`) and
/// `active_count` (`i16`) — so a test can drive every mapping case through
/// [`ActiveMode::from_parts`] without the driver hard-coding a mode.
#[derive(Default)]
struct ActiveState {
    active: Cell<i32>,
    count: Cell<i16>,
    spends: RefCell<Vec<ActiveSpend>>,
}

impl ActiveState {
    /// Stages one spend, as a delivery that exhausted a grant would.
    fn spend(&self, spend: ActiveSpend) {
        self.spends.borrow_mut().push(spend);
    }
}

/// A driver that publishes the [`ActiveState`] a test sets — the *overriding* half of the seam.
struct ActiveDriver {
    state: Rc<ActiveState>,
}

impl Driver for ActiveDriver {
    fn name(&self) -> &str {
        "active-mode-test"
    }

    fn active_mode(&self) -> ActiveMode {
        ActiveMode::from_parts(self.state.active.get(), self.state.count.get())
    }

    fn take_active_spends(&mut self) -> Vec<ActiveSpend> {
        // Taking is what consumes, exactly as the real drivers must: a second call answers `[]`.
        std::mem::take(&mut *self.state.spends.borrow_mut())
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

/// A driver that overrides **nothing** beyond the required members — the default half of the seam,
/// and the shape every existing driver in the workspace has.
struct PlainDriver;

impl Driver for PlainDriver {
    fn name(&self) -> &str {
        "plain-test"
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

/// Opens one [`ActiveDriver`] port and hands back the state handle a test drives.
fn registry_with_active_port(poller: &mut Poller) -> (PortRegistry, PortId, Rc<ActiveState>) {
    let state = Rc::new(ActiveState::default());
    let driver = Box::new(ActiveDriver {
        state: Rc::clone(&state),
    });
    let mut registry = PortRegistry::new();
    let id = registry.open(poller, driver).expect("the port opens");
    (registry, id, state)
}

/// An id no port was ever opened under.
fn unknown() -> PortId {
    PortId::new(9999)
}

/// The numeric mapping lives in exactly one place: `0` is passive (the default), `1` active, `2`
/// once, `3` an `{active, N}` carrying its counter, and any other value is passive.
#[test]
fn from_parts_maps_the_drivers_four_modes_and_treats_anything_else_as_passive() {
    assert_eq!(ActiveMode::from_parts(0, 0), ActiveMode::False);
    assert_eq!(ActiveMode::from_parts(1, 0), ActiveMode::True);
    assert_eq!(ActiveMode::from_parts(2, 0), ActiveMode::Once);
    assert_eq!(
        ActiveMode::from_parts(3, 5),
        ActiveMode::Multi { remaining: 5 }
    );

    // A value the driver never stores is not a fifth mode; passive is the only safe reading.
    for active in [-1, 4, 42] {
        assert_eq!(
            ActiveMode::from_parts(active, 7),
            ActiveMode::False,
            "an out-of-range `active` ({active}) is passive, not an invented mode"
        );
    }
}

/// `is_active` is the C's `active > 0` test on the **mode** field: everything but `False` is
/// active, including an `{active, N}` counter at zero (a store state a driver never holds).
#[test]
fn is_active_reads_the_mode_field_not_the_counter() {
    assert!(!ActiveMode::False.is_active());
    assert!(ActiveMode::True.is_active());
    assert!(ActiveMode::Once.is_active());
    assert!(ActiveMode::Multi { remaining: 3 }.is_active());
    assert!(
        ActiveMode::Multi { remaining: 0 }.is_active(),
        "the C tests `desc->inet.active` (still INET_MULTI), not the counter"
    );
    assert!(
        ActiveMode::Multi { remaining: -1 }.is_active(),
        "the C's counter is a signed i16 and is not consulted by the active test"
    );
}

/// `remaining` reports the **running** counter and only for `Multi` — the original `N` grant is not
/// recoverable from the driver's store.
#[test]
fn remaining_is_the_running_multi_counter_only() {
    assert_eq!(ActiveMode::False.remaining(), None);
    assert_eq!(ActiveMode::True.remaining(), None);
    assert_eq!(ActiveMode::Once.remaining(), None);
    assert_eq!(ActiveMode::Multi { remaining: 7 }.remaining(), Some(7));
    assert_eq!(
        ActiveMode::Multi { remaining: 0 }.remaining(),
        Some(0),
        "a zero counter is reported as-is rather than dropped"
    );
    assert_eq!(
        ActiveMode::from_parts(3, 0).remaining(),
        Some(0),
        "a `Multi` whose counter was never set reads back as zero, not as `None`"
    );
    assert_eq!(
        ActiveMode::default(),
        ActiveMode::False,
        "the default mode is the C's INET_PASSIVE"
    );
}

/// The defaults: a driver that overrides neither member answers passive mode and has spent nothing,
/// so adding the seam changed no existing driver's behaviour.
#[test]
fn the_trait_defaults_report_passive_and_no_spends() {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(PlainDriver))
        .expect("the port opens");

    assert_eq!(
        registry.active_mode(id).expect("a live port"),
        ActiveMode::False
    );
    assert_eq!(
        registry.active_mode(id).expect("a live port").remaining(),
        None
    );
    assert!(
        registry.take_active_spends().is_empty(),
        "a driver that overrides nothing has spent no grant"
    );
    assert!(
        registry
            .get_mut(id)
            .expect("live")
            .take_active_spends()
            .is_empty(),
        "and the port passthrough answers the same empty `Vec`"
    );

    // The overrides are additive: the plain driver still satisfies the rest of the trait.
    assert_eq!(
        registry.get(id).expect("live").active_mode(),
        ActiveMode::False
    );
    assert!(!registry.get(id).expect("live").port_closed());
}

/// The registry resolves the id, forwards the driver's answer unchanged, and refuses an id it does
/// not hold with the same `badarg` as its other seams.
#[test]
fn the_registry_forwards_the_mode_and_refuses_an_unknown_id() {
    let mut poller = Poller::new().expect("a poll set");
    let (registry, id, state) = registry_with_active_port(&mut poller);

    assert_eq!(
        registry.active_mode(id).expect("a live port"),
        ActiveMode::False
    );

    state.active.set(1);
    assert_eq!(
        registry.active_mode(id).expect("a live port"),
        ActiveMode::True
    );

    state.active.set(2);
    assert_eq!(
        registry.active_mode(id).expect("a live port"),
        ActiveMode::Once
    );

    state.active.set(3);
    state.count.set(4);
    assert_eq!(
        registry.active_mode(id).expect("a live port"),
        ActiveMode::Multi { remaining: 4 }
    );
    assert_eq!(
        registry.active_mode(id).expect("a live port").remaining(),
        Some(4)
    );

    // The driver's own store is the authority, and the running counter is what is left.
    state.count.set(1);
    assert_eq!(
        registry.active_mode(id).expect("a live port"),
        ActiveMode::Multi { remaining: 1 }
    );

    assert!(matches!(
        registry.active_mode(unknown()),
        Err(BeamError::BadArg)
    ));
}

/// A spend is reported **exactly once**, stamped with the port that spent it.
#[test]
fn a_spend_is_reported_once_and_the_second_drain_is_empty() {
    let mut poller = Poller::new().expect("a poll set");
    let (mut registry, id, state) = registry_with_active_port(&mut poller);

    assert!(
        registry.take_active_spends().is_empty(),
        "nothing spent yet"
    );

    // `{active, once}` was spent by a delivery: the driver clears the mode silently.
    state.spend(ActiveSpend::Once);
    state.active.set(0);
    assert_eq!(
        registry.take_active_spends(),
        vec![(id, ActiveSpend::Once)],
        "the spend is stamped with the port that made it"
    );
    assert_eq!(
        registry.active_mode(id).expect("a live port"),
        ActiveMode::False,
        "and the mode it left behind is readable"
    );
    assert!(
        registry.take_active_spends().is_empty(),
        "taking is what consumes — the second drain reports nothing"
    );

    // An `{active, N}` counter reaching zero reports its own variant, also once.
    state.spend(ActiveSpend::Multi);
    assert_eq!(
        registry.take_active_spends(),
        vec![(id, ActiveSpend::Multi)]
    );
    assert!(registry.take_active_spends().is_empty());
}

/// Several spends from one port keep the order the driver spent them in; two ports drain in
/// ascending port-number order, exactly like the other owner-facing channels.
#[test]
fn the_drain_keeps_the_drivers_order_and_visits_ports_by_number() {
    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let first_state = Rc::new(ActiveState::default());
    let first = registry
        .open(
            &mut poller,
            Box::new(ActiveDriver {
                state: Rc::clone(&first_state),
            }),
        )
        .expect("the first port opens");
    let second_state = Rc::new(ActiveState::default());
    let second = registry
        .open(
            &mut poller,
            Box::new(ActiveDriver {
                state: Rc::clone(&second_state),
            }),
        )
        .expect("the second port opens");
    assert!(
        first.number() < second.number(),
        "the allocator hands the ids out in ascending order"
    );

    // Two spends on the first port (in driver order), one on the second.
    first_state.spend(ActiveSpend::Multi);
    first_state.spend(ActiveSpend::Once);
    second_state.spend(ActiveSpend::Multi);

    assert_eq!(
        registry.take_active_spends(),
        vec![
            (first, ActiveSpend::Multi),
            (first, ActiveSpend::Once),
            (second, ActiveSpend::Multi),
        ],
        "port-number order across ports, spending order within one"
    );
    assert!(registry.take_active_spends().is_empty());
}

/// A spend recorded for a port that then **closes** before the drain is still reported, exactly
/// once — the closed half of the drain, mirroring `closed_active`/`closed_completions`.
///
/// A delivery staged for a port that closes is reported by [`PortRegistry::take_active_inputs`], so
/// the spend that produced it must be reportable too; otherwise the owner would see the delivery
/// without the mode change it caused.
#[test]
fn a_spend_recorded_before_a_close_is_reported_once_afterwards() {
    let mut poller = Poller::new().expect("a poll set");
    let (mut registry, id, state) = registry_with_active_port(&mut poller);

    state.spend(ActiveSpend::Once);
    registry.close(&mut poller, id).expect("the port closes");
    assert!(
        registry.get(id).is_none(),
        "the port is gone from the live map, so only the harvest can report it"
    );

    assert_eq!(
        registry.take_active_spends(),
        vec![(id, ActiveSpend::Once)],
        "the closed port's spend is harvested and reported once"
    );
    assert!(
        registry.take_active_spends().is_empty(),
        "and only once — the harvest queue was drained"
    );
    assert!(
        matches!(registry.active_mode(id), Err(BeamError::BadArg)),
        "a closed port's mode is no longer readable"
    );
}

/// A spend a driver records **during** its own teardown (`stop`) is harvested too, which is why the
/// harvest runs after the driver's `stop` rather than before it.
#[test]
fn a_spend_recorded_during_the_teardown_is_also_harvested() {
    /// A driver whose `stop` spends one last `{active, once}` grant.
    struct StoppingDriver {
        state: Rc<ActiveState>,
    }

    impl Driver for StoppingDriver {
        fn name(&self) -> &str {
            "stopping-test"
        }

        fn active_mode(&self) -> ActiveMode {
            ActiveMode::from_parts(self.state.active.get(), self.state.count.get())
        }

        fn take_active_spends(&mut self) -> Vec<ActiveSpend> {
            std::mem::take(&mut *self.state.spends.borrow_mut())
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
            self.state.spend(ActiveSpend::Multi);
            Ok(())
        }
    }

    let mut poller = Poller::new().expect("a poll set");
    let mut registry = PortRegistry::new();
    let state = Rc::new(ActiveState::default());
    let id = registry
        .open(
            &mut poller,
            Box::new(StoppingDriver {
                state: Rc::clone(&state),
            }),
        )
        .expect("the port opens");

    registry.close(&mut poller, id).expect("the port closes");
    assert_eq!(
        registry.take_active_spends(),
        vec![(id, ActiveSpend::Multi)],
        "the spend made from `stop` reaches the owner like any other"
    );
}
