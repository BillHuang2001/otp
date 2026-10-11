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

//! The A.4 **driver-monitor marking** suite for the runtime's pass-boundary surveillance:
//! [`Runtime::drain_driver_monitor_exits`] → [`PortRegistry::note_driver_monitor_exits`].
//!
//! The firing half (a marked monitor reaches `ErlDrvEntry::process_exit`) is pinned against a real
//! C driver in `src/port/driver/fire_tests.rs`, and the tree's own units live in
//! `src/port/driver/monitor_tests.rs`; what is pinned **here** is the production marking route, with
//! no C compiler needed: the sweep is given the live [`ProcessTable`] the per-port hook seam cannot
//! see, and it must mark exactly the monitored pids that table no longer holds.
//!
//! The unit under test is the sweep, not the queue: a port registers through the **real**
//! [`PortRegistry`] with a real id (the shape
//! [`crate::port::test_support::register_control_port`] established) and carries a real
//! [`DriverMonitors`] handle shared by `Rc`, exactly as a `{spawn_driver, Name}` port's metadata
//! shares the tree it seized from its C host.
//!
//! Cases: a dead watched pid is marked while a live one is left alone; the mark is idempotent (a
//! second sweep marks nothing new and nothing was fired or cancelled in between); every live port's
//! tree is swept, not just the first; a port without a C driver has no tree and contributes nothing;
//! and a still-live port-registry borrow **defers** the sweep (answering `Ok(0)`, leaving the tree
//! untouched and losing nothing — the next sweep marks it).

use std::rc::Rc;

use beam_abi::{monitor_identity, ErlDrvMonitor};
use beam_process::{ProcessId, ProcessTable, SignalAtoms};

use super::driver::DriverMonitors;
use super::test_support::{register_control_port, register_monitored_port};
use crate::config::RuntimeConfig;
use crate::Runtime;

/// The 24-byte identity of the holder an install returned — the same bytes the driver receives.
fn identity_of(monitor: &ErlDrvMonitor) -> [u8; 24] {
    monitor_identity(monitor as *const ErlDrvMonitor).expect("a monitor this fixture built")
}

/// A runtime and a process table sharing the **runtime's own** atom table — the shape
/// [`crate::port::tests`] uses for the port-message cases, and the same table the sweep reads.
fn runtime_and_table() -> (Runtime, ProcessTable) {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let table = ProcessTable::new(SignalAtoms::from_table(rt.code_server_atoms_mut()));
    (rt, table)
}

/// A run of pids no test process can be: the sweep must mark them and the live table must not hold
/// them.
fn absent(number: u64) -> ProcessId {
    ProcessId::new(9000 + number, 0)
}

/// The port owner the fixture trees are minted for (identities only mix the owner's number in, so
/// this never has to match the port's own owner).
fn monitor_owner() -> ProcessId {
    ProcessId::new(1, 0)
}

/// The sweep marks a monitor whose watched process is absent from the live table, and leaves a
/// monitor on a **live** process pending-free.
///
/// Discriminates: a sweep that marked every monitor (or one that marked none) would fail the two
/// halves; the two-monitors-one-tree shape is what makes "left alone" observable at all.
#[test]
fn the_sweep_marks_an_absent_watched_process_and_leaves_a_live_one_alone() {
    let (mut rt, mut table) = runtime_and_table();
    let live = table.spawn_default().expect("a live process");
    let dead = absent(1);
    assert!(
        !table.contains(dead),
        "the fixture pid must not be a live process"
    );
    let monitors = DriverMonitors::new(monitor_owner());
    let _port = register_monitored_port(&mut rt, "sweep_marks", Rc::clone(&monitors));
    let dead_holder = monitors.install(dead);
    let live_holder = monitors.install(live);

    assert_eq!(
        rt.drain_driver_monitor_exits(&table)
            .expect("the sweep cannot fail"),
        1,
        "exactly the watched pid the table does not hold is newly marked"
    );
    assert!(
        monitors.has_pending(),
        "the dead pid's monitor awaits its firing"
    );
    assert!(
        monitors.monitors(dead),
        "the mark keeps the monitor in the tree until it fires or is cancelled"
    );
    // The mark is observable exactly as the firing half sees it: **one** pending identity, and it is
    // the dead pid's — the live watch is untouched, so it is neither pending nor removed.
    let pending = monitors.take_pending();
    assert_eq!(
        pending,
        vec![identity_of(&dead_holder)],
        "only the dead pid's monitor is pending"
    );
    assert_eq!(
        monitors.find(identity_of(&live_holder)),
        Some(live),
        "a monitor on a live process belongs to the tree until the process dies"
    );
    assert!(!monitors.has_pending());
}

/// The sweep is **idempotent**: a second sweep over the same table marks nothing new, and the
/// already-pending monitor is still pending (nothing fired or cancelled it in between).
///
/// Discriminates: a sweep that re-marked (answering `1` again) would report work every pass and
/// would break the caller's "newly marked" accounting; a sweep that emptied the tree would lose the
/// monitor before the port round fires it.
#[test]
fn a_second_sweep_marks_nothing_new() {
    let (mut rt, table) = runtime_and_table();
    let dead = absent(2);
    let monitors = DriverMonitors::new(monitor_owner());
    let _port = register_monitored_port(&mut rt, "sweep_idempotent", Rc::clone(&monitors));
    monitors.install(dead);

    assert_eq!(
        rt.drain_driver_monitor_exits(&table)
            .expect("the sweep cannot fail"),
        1
    );
    assert!(monitors.has_pending());
    assert_eq!(
        rt.drain_driver_monitor_exits(&table)
            .expect("the sweep cannot fail"),
        0,
        "an already-marked monitor is not marked again"
    );
    assert!(
        monitors.has_pending(),
        "the monitor is still awaiting its firing, not taken by the sweep"
    );
    assert!(monitors.monitors(dead));
}

/// **Every** live port's tree is swept, and a port without a C driver contributes nothing — its
/// [`PortMeta`](super::PortMeta) has no monitor tree at all.
///
/// Discriminates: a sweep that stopped at the first monitored port (or one that expected every port
/// to carry a tree) would report `1` rather than `2`; the unmonitored port is the control case for
/// the `None` field the other construction sites set.
#[test]
fn every_monitored_port_is_swept_and_an_unmonitored_one_contributes_nothing() {
    let (mut rt, table) = runtime_and_table();
    let first = DriverMonitors::new(monitor_owner());
    let second = DriverMonitors::new(ProcessId::new(2, 0));
    let _control = register_control_port(&mut rt, "sweep_unmonitored", Vec::new());
    let _first = register_monitored_port(&mut rt, "sweep_first", Rc::clone(&first));
    let _second = register_monitored_port(&mut rt, "sweep_second", Rc::clone(&second));
    first.install(absent(3));
    second.install(absent(4));

    assert_eq!(
        rt.drain_driver_monitor_exits(&table)
            .expect("the sweep cannot fail"),
        2,
        "both monitored ports' dead pids are marked, and the unmonitored port adds nothing"
    );
    assert!(first.has_pending(), "the first port's tree was swept");
    assert!(second.has_pending(), "the second port's tree was swept too");
}

/// A **live** borrow of the port registry defers the sweep: it answers `Ok(0)`, leaves every tree
/// exactly as it was, and loses nothing — the next sweep marks the same dead pid.
///
/// Discriminates: an implementation that panicked (or propagated a typed error) on a nested borrow
/// would turn an in-flight port BIF into a runtime failure; one that dropped the obligation would
/// lose the mark. The follow-up sweep is the proof that the deferral kept the obligation.
#[test]
fn a_live_registry_borrow_defers_the_sweep_and_loses_nothing() {
    let (mut rt, table) = runtime_and_table();
    let dead = absent(5);
    let monitors = DriverMonitors::new(monitor_owner());
    let _port = register_monitored_port(&mut rt, "sweep_deferred", Rc::clone(&monitors));
    monitors.install(dead);

    // The `Rc` is cloned **before** the borrow so the guard borrows the clone rather than the
    // runtime's own field — the same shape the production drain uses.
    let cell = Rc::clone(&rt.ports);
    let guard = cell.try_borrow_mut().expect("the port registry is free");
    assert_eq!(
        rt.drain_driver_monitor_exits(&table)
            .expect("a deferred sweep is not an error"),
        0,
        "a live borrow defers the sweep instead of panicking or failing"
    );
    assert!(
        !monitors.has_pending(),
        "a deferred sweep must not touch any tree"
    );
    drop(guard);

    assert_eq!(
        rt.drain_driver_monitor_exits(&table)
            .expect("the sweep cannot fail"),
        1,
        "nothing was lost by the deferral: the next sweep still marks the dead pid"
    );
    assert!(monitors.has_pending());
}
