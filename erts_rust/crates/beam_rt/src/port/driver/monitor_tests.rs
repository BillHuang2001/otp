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

//! The A.4 **driver monitor regression suite** for [`DriverPort`]'s three
//! [`beam_abi::DriverHost`] monitor hooks (`driver_monitor_process`,
//! `driver_demonitor_process`, `driver_get_monitored_process`) and the
//! [`DriverMonitors`] tree behind them.
//!
//! # Which level this pins, and why
//!
//! The preference would be to drive the real `#[no_mangle]` C entry points. They are **not
//! nameable** from `beam_rt`: `beam_abi::driver::drv` re-exports only its `mem` and `port`
//! families, while the monitor family lives in the private `pdl` module — so
//! `driver_monitor_process`/`driver_demonitor_process`/`driver_get_monitored_process` are out of
//! reach, and even the re-exported ones are `unsafe extern "C" fn`, which this `#![deny(unsafe_code)]`
//! crate may not call. What **is** reachable — and is exactly the seam those entry points delegate
//! to — is the frozen [`beam_abi::DriverHost`] method set on [`DriverPort`], so that is the level
//! pinned here. `beam_abi`'s own suite (`driver/drv/pdl.rs`) pins the `Option`→result-code
//! translation *onto* this seam (its `ERL_DRV_MONITOR_OK`/`_NOT_ALIVE`/`_ERROR` assertions), and
//! `beam_abi::monitor_identity`/`monitor_from_identity` pin the byte round-trip.
//!
//! # The observable anchors
//!
//! Every case asserts something a host could get **wrong** and the oracle would show:
//!
//! * the identity round-trip — a monitor the hook handed out resolves back to the very pid term it
//!   was installed for (the C's `driver_get_monitored_process`);
//! * the C's result split for a cancel — `0` the first time, `1` the second, and the monitor is gone
//!   afterwards (`io.c:7065-7081`);
//! * that a monitor this port never issued is neither resolvable nor removable, so two ports cannot
//!   see each other's monitors;
//! * that a host whose driver entry declares **no** `process_exit` slot installs nothing, and a
//!   non-pid term is refused — the two `None` answers of the hook;
//! * the pending/drain contract the runtime's later `process_exit` firing depends on:
//!   `note_process_exit` marks exactly one pid's monitors, is idempotent, `take_pending` yields each
//!   identity exactly once and removes it, while a different pid's monitor is left untouched.

use std::rc::Rc;

use beam_abi::{monitor_from_identity, monitor_identity, DriverHost, ErlDrvMonitor};
use beam_alloc::Heap;
use beam_process::ProcessId;
use beam_term::{make_small, AtomTable, NIL};

use super::{DriverMonitors, DriverPort};

/// A host that **declares** a `process_exit` slot, running as pid 1 — the shape the `hooks_tests`
/// suite builds, plus the builder step that makes the monitor hooks live.
fn host() -> DriverPort {
    DriverPort::new(
        Heap::new(256).expect("a test heap"),
        Rc::new(AtomTable::new()),
        ProcessId::new(1, 0),
    )
    .with_has_process_exit(true)
}

/// The 24-byte identity of the holder returned by an install.
fn identity_of(monitor: &ErlDrvMonitor) -> [u8; 24] {
    monitor_identity(monitor as *const ErlDrvMonitor)
        .expect("a monitor this suite built is non-null")
}

/// `driver_monitor_process` installs a monitor that genuinely resolves back to the pid it was
/// installed for, and `driver_get_monitored_process` answers the **same** pid term
/// (`io.c:6998`/`7104`).
#[test]
fn monitor_process_installs_a_monitor_that_resolves_to_its_pid() {
    let mut port = host();
    let pid = ProcessId::new(77, 3);
    let term = pid.to_eterm();

    let monitor = port
        .monitor_process(term)
        .expect("the entry declares a process_exit slot");
    let mon = &monitor as *const ErlDrvMonitor;

    assert_eq!(port.get_monitored_process(mon), Some(term));
    assert_eq!(port.monitors().find(identity_of(&monitor)), Some(pid));
}

/// `driver_demonitor_process` answers the C's `0` for the call that really removes the monitor and
/// `1` for the next one, and the monitor is unresolvable afterwards (`io.c:7065-7081`).
#[test]
fn demonitor_process_removes_the_monitor_exactly_once() {
    let mut port = host();
    let monitor = port
        .monitor_process(ProcessId::new(5, 1).to_eterm())
        .expect("monitored");
    let mon = &monitor as *const ErlDrvMonitor;

    assert_eq!(port.demonitor_process(mon), 0, "a monitor was removed");
    assert_eq!(port.demonitor_process(mon), 1, "it no longer exists");
    assert_eq!(port.get_monitored_process(mon), None);
}

/// A monitor identity this port never issued is neither resolvable nor removable — the C's `NULL`
/// term and its "no longer exists" code — so two ports can never see each other's monitors.
#[test]
fn an_identity_this_port_never_issued_is_unknown() {
    let mut port = host();
    let stranger = monitor_from_identity([0xa5; 24]);
    let mon = &stranger as *const ErlDrvMonitor;

    assert_eq!(port.get_monitored_process(mon), None);
    assert_eq!(port.demonitor_process(mon), 1);
}

/// A host whose loaded driver entry declares **no** `process_exit` slot installs nothing: the C
/// answers `-1` there (`io.c:6998`), which the frozen `Option` hook spells `None`.
#[test]
fn a_port_without_a_process_exit_slot_monitors_nothing() {
    let mut port = DriverPort::new(
        Heap::new(256).expect("a test heap"),
        Rc::new(AtomTable::new()),
        ProcessId::new(1, 0),
    );
    assert!(port.monitor_process(ProcessId::new(2, 0).to_eterm()).is_none());
}

/// A term that is not a local process id cannot be monitored (the C's `names_a_process` guard): the
/// hook answers `None` rather than fabricating a monitor, and nothing was recorded.
#[test]
fn a_non_local_pid_term_is_not_monitored() {
    let mut port = host();
    assert!(port.monitor_process(make_small(42)).is_none());
    assert!(port.monitor_process(NIL).is_none());
    assert!(!port.monitors().has_pending());
}

/// `note_process_exit` marks exactly the exiting pid's monitors, is idempotent, and the drain yields
/// each identity once (removing it, as the C does when a monitor fires) while another pid's monitor
/// is left alone.
#[test]
fn note_process_exit_marks_and_drains_only_that_pids_monitors() {
    let exiting = ProcessId::new(10, 0);
    let survivor = ProcessId::new(11, 0);
    let tree = DriverMonitors::new(ProcessId::new(1, 0));

    let gone = tree.install(exiting);
    let gone_id = identity_of(&gone);
    let alive = tree.install(survivor);
    let alive_id = identity_of(&alive);

    assert!(!tree.has_pending(), "nothing has exited yet");
    assert_eq!(tree.note_process_exit(exiting), 1, "only the exiting pid");
    assert!(tree.has_pending());
    assert!(tree.monitors(exiting), "the entry stays until it is fired");
    assert!(tree.monitors(survivor));
    assert_eq!(
        tree.note_process_exit(exiting),
        0,
        "a pid already marked contributes nothing new"
    );

    assert_eq!(tree.take_pending(), vec![gone_id], "drained in order");
    assert!(!tree.has_pending());
    assert!(
        !tree.monitors(exiting),
        "firing the monitor removed it, as the C does"
    );
    assert!(tree.monitors(survivor), "the survivor is untouched");
    assert!(tree.take_pending().is_empty(), "yielded exactly once");
    assert_eq!(tree.find(gone_id), None);
    assert_eq!(tree.find(alive_id), Some(survivor));

    assert!(tree.remove(alive_id), "the survivor is still cancellable");
    assert!(!tree.remove(alive_id));
}

/// `clear` empties the tree — no monitor survives, nothing is pending, and no minted identity
/// resolves.
#[test]
fn clear_empties_the_tree() {
    let pid = ProcessId::new(10, 0);
    let tree = DriverMonitors::new(ProcessId::new(1, 0));
    let id = identity_of(&tree.install(pid));
    assert!(tree.monitors(pid));

    tree.clear();

    assert!(!tree.monitors(pid));
    assert!(tree.find(id).is_none());
    assert!(!tree.has_pending());
    assert!(tree.take_pending().is_empty());
}

/// Two ports never mint the same identity, even though each starts its counter at zero — the
/// owning process's number is mixed in.
#[test]
fn two_ports_never_mint_the_same_identity() {
    let first = DriverMonitors::new(ProcessId::new(1, 0));
    let second = DriverMonitors::new(ProcessId::new(2, 0));
    let pid = ProcessId::new(10, 0);

    let a = identity_of(&first.install(pid));
    let b = identity_of(&second.install(pid));
    assert_ne!(a, b);

    // And a port does not resolve the other's identity.
    assert_eq!(first.find(b), None);
    assert_eq!(second.find(a), None);
}
