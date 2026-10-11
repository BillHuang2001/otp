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

use beam_abi::{monitor_from_identity, monitor_identity, DriverHost, ErlDrvMonitor, MonitorResult};
use beam_alloc::Heap;
use beam_process::ProcessId;
use beam_term::{make_small, AtomTable, NIL};

use super::{DriverMonitors, DriverPort};
use crate::port::ProcessLiveness;

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

/// The same host, wired to a **published** liveness view — the one builder step
/// [`RuntimePorts::open_port`](crate::port::RuntimePorts::open_port) adds in production (see
/// [`ProcessLiveness`]).
fn host_with_liveness(liveness: Rc<ProcessLiveness>) -> DriverPort {
    host().with_liveness(liveness)
}

/// What `monitor_process_result` answered, in the shape a test asserts on: `MonitorResult` has no
/// `PartialEq` (its `Ok` payload, an opaque 32-byte holder, cannot implement one), so the three
/// variants are matched into this comparable projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The C's `0` — armed.
    Ok,
    /// The C's positive `1` (`ERL_DRV_MONITOR_NOT_ALIVE`).
    NotAlive,
    /// The C's `-1`.
    Error,
}

/// The three-way answer, projected for comparison.
fn outcome(result: MonitorResult) -> Outcome {
    match result {
        MonitorResult::Ok(_) => Outcome::Ok,
        MonitorResult::NotAlive => Outcome::NotAlive,
        MonitorResult::Error => Outcome::Error,
    }
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

/// A monitor that has **fired** is gone from the host's point of view: the drain removed it, so
/// `driver_get_monitored_process` answers the C's `NULL` (the hook's `None`) and a later
/// `driver_demonitor_process` reports "no longer exists" — the C unlinks a monitor as it fires
/// (`io.c:7140-7160`). This is the state the port's `process_exit` drive point leaves behind.
#[test]
fn a_fired_monitor_is_no_longer_resolvable() {
    let mut port = host();
    let pid = ProcessId::new(10, 0);
    let monitor = port
        .monitor_process(pid.to_eterm())
        .expect("the entry declares a process_exit slot");
    let mon = &monitor as *const ErlDrvMonitor;

    assert_eq!(port.monitors().note_process_exit(pid), 1);
    assert_eq!(port.monitors().take_pending(), vec![identity_of(&monitor)]);

    assert_eq!(port.get_monitored_process(mon), None);
    assert_eq!(port.demonitor_process(mon), 1, "it no longer exists");
    assert!(!port.monitors().monitors(pid));
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

// ---------------------------------------------------------------------------------------------
// The three-way `driver_monitor_process` split (`monitor_process_result`)
// ---------------------------------------------------------------------------------------------
//
// The C's `do_driver_monitor_process` (`io.c:7012-7038`) answers `0` (armed), the **positive** `1`
// (`ERL_DRV_MONITOR_NOT_ALIVE`, the advertised process is gone) or `-1` (no `process_exit` slot).
// The frozen `Option` hook above can only express the first and the third, so the three-way hook
// [`beam_abi::DriverHost::monitor_process_result`] is what the real entry point prefers; these
// cases pin the five-way split `DriverPort` implements for it, including the positive path that
// makes the C's `1` reachable at all.
//
// The liveness input is the runtime's **published** observation ([`ProcessLiveness`]): a snapshot
// refreshed at each port round, because the host holds no process table. What each case varies is
// exactly that view.

/// A pid the published observation saw alive is monitored normally: the C's `0`, with a holder the
/// driver's later `driver_get_monitored_process`/`driver_demonitor_process` can use (`io.c:7012`).
#[test]
fn a_pid_the_view_saw_alive_is_armed() {
    let view = ProcessLiveness::new();
    let alive = ProcessId::new(77, 3);
    let other = ProcessId::new(78, 0);
    view.observe([alive, other].into_iter());

    let mut port = host_with_liveness(Rc::clone(&view));
    let result = port.monitor_process_result(alive.to_eterm());
    let mon = match result {
        MonitorResult::Ok(mon) => mon,
        other => panic!("a pid the view saw alive must arm a monitor, got {other:?}"),
    };

    // The holder is the very monitor this port installed for that pid.
    assert_eq!(
        port.get_monitored_process(&mon as *const ErlDrvMonitor),
        Some(alive.to_eterm())
    );
    assert_eq!(port.monitors().find(identity_of(&mon)), Some(alive));
    assert!(port.monitors().monitors(alive));
    assert!(
        !port.monitors().has_pending(),
        "arming a monitor does not fire it"
    );
}
/// A local pid the observation **did not** see is the C's positive `1`
/// (`ERL_DRV_MONITOR_NOT_ALIVE`, `io.c:7031`) — and nothing is installed: a monitor on a dead
/// process must not sit in the tree waiting for a `process_exit` that can never come.
#[test]
fn a_pid_absent_from_the_view_is_not_alive() {
    let view = ProcessLiveness::new();
    let recorded = ProcessId::new(11, 0);
    let gone = ProcessId::new(12, 0);
    view.observe([recorded].into_iter());

    let mut port = host_with_liveness(Rc::clone(&view));
    assert_eq!(
        outcome(port.monitor_process_result(gone.to_eterm())),
        Outcome::NotAlive,
        "the C's positive 1, not the -1 the two-way subset would report"
    );
    assert!(
        !port.monitors().monitors(gone),
        "a not-alive answer installs nothing"
    );
    assert!(!port.monitors().has_pending());

    // ...while the pid the view did record is still armable, so the case discriminates on the view
    // rather than on some other state.
    assert_eq!(
        outcome(port.monitor_process_result(recorded.to_eterm())),
        Outcome::Ok
    );
    assert!(port.monitors().monitors(recorded));
}

/// A view with **no** observation (and one cleared after an observation) answers the fallback: the
/// monitor is installed. A `NotAlive` here would silently drop a live driver's watch, which is the
/// one failure a driver cannot detect.
#[test]
fn an_unobserved_pid_is_monitored_anyway() {
    let pid = ProcessId::new(21, 0);

    // Never observed.
    let view = ProcessLiveness::new();
    let mut port = host_with_liveness(Rc::clone(&view));
    assert_eq!(outcome(port.monitor_process_result(pid.to_eterm())), Outcome::Ok);
    assert!(port.monitors().monitors(pid));

    // Observed, then cleared (the state after `open_port` drops the last C-driver port).
    let cleared = ProcessLiveness::new();
    cleared.observe([pid].into_iter());
    cleared.clear();
    let mut port = host_with_liveness(Rc::clone(&cleared));
    assert_eq!(outcome(port.monitor_process_result(pid.to_eterm())), Outcome::Ok);
    assert!(port.monitors().monitors(pid));

    // A host with no view at all (a bare host in a test) behaves identically.
    assert_eq!(outcome(host().monitor_process_result(pid.to_eterm())), Outcome::Ok);
}

/// A term that is not a local process id cannot be monitored: the C's `-1` on this runtime's side
/// (it cannot tell a dead remote process from an unknown one, so it reports the honest "cannot
/// monitor" rather than the positive code).
#[test]
fn a_non_pid_term_is_an_error() {
    let view = ProcessLiveness::new();
    view.observe([ProcessId::new(1, 0)].into_iter());
    let mut port = host_with_liveness(Rc::clone(&view));

    assert_eq!(outcome(port.monitor_process_result(make_small(7))), Outcome::Error);
    assert_eq!(outcome(port.monitor_process_result(NIL)), Outcome::Error);
    assert!(!port.monitors().has_pending());
    assert_eq!(port.monitors().monitored_pids(), Vec::new());
}

/// An **external** (wire) pid is refused with the same `-1`: it names no process this runtime can
/// arm a monitor on.
///
/// The term is built without `unsafe` — an ETF `NEW_PID_EXT` (88) image is decoded onto a test heap
/// — and the case asserts first that it really is *not* an internal pid, so it discriminates the
/// local/external split rather than restating the non-pid case.
#[test]
fn an_external_pid_term_is_an_error() {
    // [VERSION_MAGIC][NEW_PID_EXT][SMALL_ATOM_UTF8_EXT "foo@bar"]
    // [number:u32][serial:u32][creation:u32]
    let image: [u8; 23] = [
        131, 88, 119, 7, b'f', b'o', b'o', b'@', b'b', b'a', b'r', 0, 0, 0, 10, 0, 0, 0, 0, 0, 0,
        0, 0,
    ];
    let mut heap = Heap::new(256).expect("a test heap");
    let mut atoms = AtomTable::new();
    let external = beam_codec::decode_term(&image, &mut heap, &mut atoms)
        .expect("the NEW_PID_EXT image decodes");
    assert!(
        !ProcessId::is_process(external),
        "the decoded wire pid must not be an internal pid"
    );

    let view = ProcessLiveness::new();
    view.observe([ProcessId::new(1, 0)].into_iter());
    let mut port = host_with_liveness(Rc::clone(&view));
    assert_eq!(
        outcome(port.monitor_process_result(external)),
        Outcome::Error
    );
    assert!(!port.monitors().has_pending());
}

/// A host whose loaded entry declares **no** `process_exit` slot refuses the operand whatever it
/// names — the C's `-1` (`dp->entry->process_exit == NULL`, `io.c:7020-7021`), which outranks the
/// liveness question: an entry that cannot be told about an exit must not be handed a monitor.
#[test]
fn a_port_without_a_process_exit_slot_is_an_error() {
    let view = ProcessLiveness::new();
    let pid = ProcessId::new(31, 0);
    view.observe([pid].into_iter());

    let mut port = DriverPort::new(
        Heap::new(256).expect("a test heap"),
        Rc::new(AtomTable::new()),
        ProcessId::new(1, 0),
    )
    .with_liveness(Rc::clone(&view));

    assert_eq!(outcome(port.monitor_process_result(pid.to_eterm())), Outcome::Error);
    assert_eq!(outcome(port.monitor_process_result(NIL)), Outcome::Error);
    assert!(!port.monitors().monitors(pid));
}

/// The two-way `Option` hook keeps its own contract: it still installs for a local pid and still
/// refuses a non-pid term or a host with no `process_exit` slot. This is the shape
/// `beam_abi`'s default three-way hook is built on, so it must not change.
#[test]
fn the_two_way_hook_still_installs_for_a_local_pid() {
    let view = ProcessLiveness::new();
    let pid = ProcessId::new(41, 0);
    view.observe([pid].into_iter());

    let mut port = host_with_liveness(Rc::clone(&view));
    assert!(port.monitor_process(pid.to_eterm()).is_some());
    assert!(port.monitors().monitors(pid));
    // It consults no view: a pid the observation did not see is still installed by this hook (the
    // three-way hook is the one that reports `1`).
    let unobserved = ProcessId::new(42, 0);
    assert!(port.monitor_process(unobserved.to_eterm()).is_some());
    assert!(port.monitors().monitors(unobserved));

    assert!(port.monitor_process(make_small(7)).is_none());
    assert!(port.monitor_process(NIL).is_none());
}
