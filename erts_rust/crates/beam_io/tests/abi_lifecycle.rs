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

//! End-to-end coverage for the loaded-driver **port-lifecycle adapters** of `src/abi/port.rs` over
//! the real, compiled C driver fixture `tests/fixtures/host_driver.c` (workspace contract **A.4**,
//! the frozen driver C ABI).
//!
//! Two `DriverPort` methods are exercised here through a real `.so` rather than only through unit
//! tests:
//!
//! * [`DriverPort::port_exit`](beam_io::DriverPort) — the adapter that maps the [`Driver::port_exit`]
//!   seam onto the closest slot a loaded `ErlDrvEntry` has: `process_exit` when the entry declares
//!   it, else `emergency_close` (the C has no port-exit callback). Both **branches** are covered:
//!   the fixture's default entry declares `process_exit`, and the
//!   `host_driver_no_process_exit.c` variant forces that slot to `NULL` so the `emergency_close`
//!   fallback runs instead. The notification is delivered through the real once-only advisory seam
//!   [`PortRegistry::notify_port_exit`] — the port stays live and `port_exit` runs **before** any
//!   `stop`, so the per-port state is still valid inside the callback.
//! * [`DriverPort::take_exit_status`](beam_io::DriverPort) — a C driver owns no OS child this crate
//!   can reap, so the adapter always reports "no status"; the test pins that through the public
//!   [`PortRegistry::take_exit_status`] and [`Port::take_exit_status`] seams (never a fabricated
//!   `Some(0)`).
//!
//! # The observables
//!
//! Both port-exit callbacks are observable two ways, so the test drives the whole path: each appends
//! its own line (`process_exit` / `emergency_close`) to the fixture's per-port `log=<path>` file, and
//! each `driver_output`s a marker (`"P"` / `"E"`) that the next port-task round reports as the
//! `PortNotification::Data` a runtime turns into `{Port, {data, Data}}`.
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixtures needs a C compiler and the system OTP include directory (`erl_driver.h`);
//! when either is missing every test prints a **loud** `SKIP:` banner and returns (the
//! [`cdriver`](crate) helpers). A load failure is **never** a skip — it means the `driver_*` host
//! symbols are not in this test binary's dynamic symbol table, which is a wiring bug.

mod cdriver;

use beam_common::BeamError;
use beam_io::prelude::*;
use beam_process::PortId;

use cdriver::*;

// ---------------------------------------------------------------------------------------------
// `DriverPort::port_exit` — the `process_exit` branch
// ---------------------------------------------------------------------------------------------

/// A port-exit notification reaches a loaded driver through its **`process_exit`** slot.
///
/// The fixture's default entry declares `process_exit` (and `emergency_close`), so the adapter must
/// pick `process_exit` and never the fallback. The test asserts the callback ran exactly once (the
/// log line and the `"P"` marker as port data), that the port stays live across it (the
/// notification is not the teardown `stop`), and that a repeated notification never re-runs it.
#[test]
fn a_port_exit_notification_reaches_the_drivers_process_exit_slot() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("port-exit-process-exit");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    // Only `start` has run so far (the log path comes from the command string's `log=` token).
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string()],
        "the driver's start callback ran once and nothing else"
    );

    registry
        .notify_port_exit(&mut poller, id, PortExitReason::OwnerExited)
        .expect("the exit notification reaches the loaded driver");

    // The adapter reached `process_exit` — the entry declares it, so the `emergency_close` fallback
    // must not be taken.
    let lines = log_lines(&probe.log);
    assert_eq!(
        count_line(&lines, "process_exit"),
        1,
        "the entry's process_exit slot ran; log = {lines:?}"
    );
    assert_eq!(
        count_line(&lines, "emergency_close"),
        0,
        "the emergency_close fallback is unreachable while process_exit is declared; log = {lines:?}"
    );

    // The callback's `driver_output` marker reaches the port owner as port data.
    assert_eq!(
        round_data(&mut poller, &mut registry, id)
            .expect("the process_exit callback emitted its marker"),
        b"P".to_vec(),
        "process_exit driver_output reached the port owner"
    );

    // The notification is *not* the teardown `stop`: the port (and its state) stays live.
    assert!(registry.contains(id), "notify_port_exit leaves the port live");
    assert_eq!(
        count_line(&lines, "stop"),
        0,
        "the port-exit notification must not run the driver's stop; log = {lines:?}"
    );

    // At most once: a repeat never reaches the driver again, whatever the reason.
    registry
        .notify_port_exit(&mut poller, id, PortExitReason::Closed)
        .expect("a repeat notification is a no-op");
    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "process_exit"),
        1,
        "the port-exit notification is delivered exactly once; log = {lines:?}"
    );
    assert!(
        round_data(&mut poller, &mut registry, id).is_none(),
        "a repeat notification emits no second marker"
    );

    // A later close still runs the teardown `stop`, so the two are independent.
    registry.close(&mut poller, id).expect("the port closes");
    let lines = log_lines(&probe.log);
    assert_eq!(
        count_line(&lines, "stop"),
        1,
        "close runs the driver's stop exactly once; log = {lines:?}"
    );
    assert_eq!(
        count_line(&lines, "process_exit"),
        1,
        "teardown does not re-run the port-exit notification; log = {lines:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// `DriverPort::port_exit` — the `emergency_close` fallback
// ---------------------------------------------------------------------------------------------

/// A port-exit notification reaches a loaded driver whose **only** exit slot is `emergency_close`.
///
/// `host_driver_no_process_exit.c` is the same fixture with the entry's `process_exit` slot forced
/// to `NULL`, so the adapter must take its `emergency_close` fallback: the notification still
/// arrives (the log line and the `"E"` marker), and `process_exit` is never reached.
#[test]
fn a_port_exit_notification_falls_back_to_emergency_close() {
    let Some(so) = no_process_exit_variant_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("port-exit-emergency-close");
    let (mut poller, mut registry, id) =
        open_named_port(&so, "host_driver", &probe.command, host);

    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string()],
        "the variant entry's start callback ran once and nothing else"
    );

    registry
        .notify_port_exit(&mut poller, id, PortExitReason::Normal)
        .expect("the exit notification reaches the emergency_close-only driver");

    let lines = log_lines(&probe.log);
    assert_eq!(
        count_line(&lines, "emergency_close"),
        1,
        "an entry with no process_exit observes the notification through emergency_close; \
         log = {lines:?}"
    );
    assert_eq!(
        count_line(&lines, "process_exit"),
        0,
        "the variant entry has no process_exit slot to run; log = {lines:?}"
    );

    assert_eq!(
        round_data(&mut poller, &mut registry, id)
            .expect("the emergency_close callback emitted its marker"),
        b"E".to_vec(),
        "emergency_close driver_output reached the port owner"
    );

    // The notification is still advisory: the port is live until its teardown.
    assert!(registry.contains(id));
    registry.close(&mut poller, id).expect("the port closes");
    let lines = log_lines(&probe.log);
    assert_eq!(
        count_line(&lines, "stop"),
        1,
        "the variant entry's stop ran once at teardown; log = {lines:?}"
    );
    assert_eq!(
        count_line(&lines, "emergency_close"),
        1,
        "teardown does not re-run the fallback; log = {lines:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// `DriverPort::take_exit_status` — a loaded C driver reports no status
// ---------------------------------------------------------------------------------------------

/// A loaded C driver reports "no exit status" through the port/registry seam, never a fabricated
/// `Some(0)`.
///
/// `DriverPort::take_exit_status` is the adapter's inherited default: a driver `.so` owns no OS
/// child this crate can reap, so the answer is always `Ok(None)` — repeatably (it is not a
/// take-once status that a second call could turn into a different value) and distinct from the
/// sibling `take_eof` seam. The port stays fully functional across it.
#[test]
fn a_loaded_c_driver_reports_no_exit_status() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    assert_eq!(
        registry
            .take_exit_status(id)
            .expect("the registry's exit-status seam succeeds"),
        None,
        "a loaded C driver has no reaped child to report"
    );
    assert_eq!(
        registry
            .get_mut(id)
            .expect("a live port")
            .take_exit_status()
            .expect("the port's exit-status seam succeeds"),
        None,
        "the port-level seam agrees with the registry's (no fabricated status)"
    );
    assert!(
        !registry
            .take_eof(id)
            .expect("the sibling eof seam succeeds"),
        "a driver that streams no input never reports end-of-file"
    );

    // The port is unaffected: its data path still round-trips.
    registry
        .output(&mut poller, id, b"still-live")
        .expect("the driver's output callback runs");
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the echo reaches the port owner"),
        b"still-live".to_vec(),
        "taking the exit status leaves the port fully functional"
    );

    // An unknown id is an argument fault, not a silent `None`.
    assert!(matches!(
        registry.take_exit_status(PortId::new(4242)),
        Err(BeamError::BadArg)
    ));

    registry.close(&mut poller, id).expect("the port closes");
}
