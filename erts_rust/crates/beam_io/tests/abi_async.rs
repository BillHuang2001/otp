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

//! The `driver_async` **completion arms** and the `ErlDrvPort` **stability** contract, over the real
//! compiled C driver fixture (`tests/fixtures/host_driver.c` and its `ready_async`-less variant
//! `tests/fixtures/host_driver_no_ready_async.c`).
//!
//! `driver_async` runs a driver's `invoke` on a worker thread and later hands its completion to
//! **one** arm: the entry's `ready_async` when it has one (which then owns `async_data` — the C's
//! `need_free = 0`), or the driver's own `async_free` otherwise (`io.c:5399-5406`,
//! `erl_async.c:324-338`). The driver's `async_data` must be released **exactly once** whether or
//! not its port is still alive when the job finishes — that is the guarantee the crate's
//! `AsyncCompletion` makes a value (`beam_io/src/abi/job.rs`), and these tests pin every arm of it:
//!
//! * the `ready_async` arm (a completion handed to the entry's callback, `async_free` **not** run);
//! * the `async_free` arm of an entry **without** `ready_async` (the variant fixture);
//! * the fire-and-forget form (`driver_async` with a `NULL` invoke), released through `async_free`
//!   right away;
//! * a port **dropped while a job is in flight** (the worker releases the data itself, the C's
//!   `!p` branch);
//! * a port dropped **after** a job completed but **before** its completion was drained (`AsyncJobs`
//!   drains and releases it).
//!
//! They also pin the other half of the "M1 limitation" the crate `CONTEXT.md` records: the
//! `ErlDrvPort` a driver stashes in `start` is the **same pointer** for the port's whole life, so
//! using it in a later callback (here `output`) is not just accepted but names the identical handle.
//!
//! # Non-flakiness
//!
//! Every wait is bounded. Where a test must know the worker finished first, it polls the fixture's
//! callback log for the line the worker writes (`async_invoke`) rather than sleeping a fixed time —
//! see [`await_log_line`]. The "exactly once" assertions then make both the drain and the
//! port-gone paths pass: the completion is a single value, so whichever owner releases it, the
//! driver's `async_free` runs precisely once.
//!
//! # Skip-vs-fail policy
//!
//! As in `tests/abi_driver.rs`: a missing toolchain (`cc` / `erl_driver.h`) prints a loud `SKIP:`
//! and returns; a `dlopen` failure is a wiring bug and panics.

mod cdriver;

use beam_abi::driver::host::DriverEnv;
use beam_abi::driver::types::ErlDrvPort;
use beam_io::prelude::{AbiHost, LocalAtomPortHost};

use cdriver::*;

/// The `handle=<%p>` lines of the fixture's callback log (the `hdl` token makes one per callback).
fn handle_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| line.starts_with("handle="))
        .cloned()
        .collect()
}

/// The fixture's callback log reduced to its *shape*: every `handle=<%p>` line becomes `"handle"`,
/// every other line stays as it is.
///
/// The `hdl` token makes the fixture append a `handle=` line **inside** each callback that runs
/// (`start`/`output`/`stop`), so the shape shows which callback each logged handle came from — the
/// property that makes the pointer-identity assertion below a genuine stash-in-one-callback /
/// read-back-in-a-later-one comparison rather than two captures inside a single callback.
fn log_shape(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            if line.starts_with("handle=") {
                "handle".to_string()
            } else {
                line.clone()
            }
        })
        .collect()
}
// ---------------------------------------------------------------------------------------------
// The `ready_async` arm (the default fixture)
// ---------------------------------------------------------------------------------------------

/// With a `ready_async` slot the finished job goes **there**, on the port thread, and no
/// `async_free` runs: `ready_async` owns `async_data` from that point (the C's `need_free = 0`).
///
/// The fixture's `ready_async` logs `ready_async h=same` only when the `ErlDrvThreadData` the host
/// passed it (the job's `async_data`) is the very same pointer it was handed as `drv_data` — so the
/// line is simultaneously the "the arm ran" and the "the data identity survived the worker round
/// trip" assertion.
#[test]
fn a_ready_async_entry_hands_the_completion_to_ready_async_and_never_runs_async_free() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("async-ready-arm");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    // Control verb 14 is `driver_async(port, NULL, host_async_invoke, data, host_async_free)`.
    registry
        .port_control(&mut poller, id, "14", &[])
        .expect("control verb 14 schedules the job");

    // The worker logs `async_invoke` when the job body finished; `ready_async` has *not* run yet —
    // it only runs on the port thread, at the drive point below.
    let lines = await_log_line(&probe.log, "async_invoke", ASYNC_TIMEOUT);
    assert_eq!(
        count_line(&lines, "ready_async h=same"),
        0,
        "ready_async must not run on the worker thread, only at the port's drive point"
    );
    assert_eq!(
        count_line(&lines, "async_free"),
        0,
        "the data must not be released before the completion is handed to an arm"
    );

    let processed = drive_until_drained(&mut poller, &mut registry, id, ASYNC_TIMEOUT);
    assert!(processed >= 1, "the completion was drained");

    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "ready_async h=same"),
        1,
        "the entry's ready_async ran once, with the job's own async_data as its thread data"
    );
    assert_eq!(
        count_line(&lines, "async_free"),
        0,
        "ready_async owns async_data, so the driver's async_free must not run"
    );
    assert!(
        !registry
            .get(id)
            .expect("a live port")
            .has_pending_async(),
        "the drained job leaves no pending async work"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

// ---------------------------------------------------------------------------------------------
// The `async_free` arm (the entry with no `ready_async`)
// ---------------------------------------------------------------------------------------------

/// An entry **without** a `ready_async` has no callback to hand a finished job to, so the host
/// releases the job's `async_data` through the driver's own `async_free` (the C's `need_free`
/// path). The `host_driver_no_ready_async.c` variant build is exactly that entry.
#[test]
fn an_entry_without_ready_async_releases_the_completion_through_async_free() {
    let Some(so) = variant_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("async-free-arm");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    registry
        .port_control(&mut poller, id, "14", &[])
        .expect("control verb 14 schedules the job");

    // The job ran (its worker logged), but the release is deferred to the drive point.
    let lines = await_log_line(&probe.log, "async_invoke", ASYNC_TIMEOUT);
    assert_eq!(
        count_line(&lines, "async_free"),
        0,
        "the release happens at the drive point, not on the worker"
    );

    let processed = drive_until_drained(&mut poller, &mut registry, id, ASYNC_TIMEOUT);
    assert!(processed >= 1, "the completion was drained");

    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "async_free"),
        1,
        "an entry with no ready_async has its data released through async_free, exactly once"
    );
    assert_eq!(
        count_line(&lines, "ready_async h=same"),
        0,
        "the variant entry has no ready_async slot to run"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

// ---------------------------------------------------------------------------------------------
// The fire-and-forget (`NULL` invoke) form
// ---------------------------------------------------------------------------------------------

/// `driver_async` with a `NULL` `invoke` schedules no work, so no `ready_async` can ever run for it
/// and the host must release the data through `async_free` **right away** (the crate's documented
/// deviation from the C, which dereferences the null `invoke` and faults — refusing to crash must
/// not leak).
#[test]
fn a_null_invoke_job_is_released_through_async_free_right_away() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("async-null-invoke");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    // Control verb 15 is `driver_async(port, NULL, NULL, data, host_async_free)`; the release runs
    // inside the call, so no drive point is needed (or possible — nothing is in flight).
    registry
        .port_control(&mut poller, id, "15", &[])
        .expect("control verb 15 schedules the fire-and-forget form");

    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "async_free"),
        1,
        "the fire-and-forget form is released through async_free exactly once"
    );
    assert_eq!(
        count_line(&lines, "async_invoke"),
        0,
        "a NULL invoke runs no job body"
    );
    assert_eq!(
        count_line(&lines, "ready_async h=same"),
        0,
        "nothing was scheduled, so no ready_async can run"
    );
    assert!(
        !registry
            .get(id)
            .expect("a live port")
            .has_pending_async(),
        "the fire-and-forget form leaves nothing in flight"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

// ---------------------------------------------------------------------------------------------
// Dropping a port around a job's completion
// ---------------------------------------------------------------------------------------------

/// A port dropped **while its job is still running** still gets the job's `async_data` released
/// exactly once: the worker's completion hand-off fails and the worker releases it itself — the
/// C's port-gone branch of `call_async_ready` (`erl_async.c:324-330`).
///
/// The job is the deliberately slow one (control verb 16 sleeps ~500 ms) so the drop really lands
/// mid-flight; the release is then observed by polling the fixture's log from the worker's own
/// thread. The port is dropped (not closed), so the fixture's `stop` never frees the state the
/// `async_free` logs through.
#[test]
fn dropping_a_port_while_a_job_is_in_flight_releases_it_exactly_once() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("async-drop-in-flight");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    // Control verb 16 is the slow-invoke form with an `async_free`.
    registry
        .port_control(&mut poller, id, "16", &[])
        .expect("control verb 16 schedules the slow job");

    // Drop the whole registry: every live `Port` — and so the port's `DriverPort` — is dropped
    // while the job sleeps on its worker.
    drop(registry);

    // The slow worker finishes and releases the data on *its* thread; poll the log for it.
    let lines = await_log_line(&probe.log, "async_free", ASYNC_TIMEOUT);
    assert_eq!(
        count_line(&lines, "async_free"),
        1,
        "the job whose port vanished is released through async_free exactly once; log = {lines:?}"
    );
    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "async_free"),
        1,
        "the release happens exactly once — never twice; log = {lines:?}"
    );
    assert_eq!(
        count_line(&lines, "stop"),
        0,
        "a port dropped without a close never runs the driver's stop"
    );
    drop(poller);
}

/// A port dropped **after** a job's worker finished but **before** its completion was drained still
/// releases the job's `async_data` exactly once: the undelivered completion is drained and dropped
/// with the port (`AsyncJobs`'s `Drop`), which runs the driver's `async_free`.
///
/// This is the counterpart of the in-flight case above: there the worker released the data, here
/// the port's own teardown does — and neither double-releases.
#[test]
fn dropping_a_port_after_a_job_completed_but_before_draining_releases_it_exactly_once() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("async-drop-after-complete");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    registry
        .port_control(&mut poller, id, "14", &[])
        .expect("control verb 14 schedules the fast job");

    // Wait for the worker's body to finish: the completion is now queued on the port, undrained.
    let lines = await_log_line(&probe.log, "async_invoke", ASYNC_TIMEOUT);
    assert_eq!(
        count_line(&lines, "async_free"),
        0,
        "nothing has been released while the completion sits undrained"
    );
    assert!(
        registry.get(id).expect("a live port").has_pending_async(),
        "the completed-but-undrained job is still pending on the port"
    );

    // Drop the port WITHOUT running a port-task round or an async drive point.
    drop(registry);

    let lines = await_log_line(&probe.log, "async_free", ASYNC_TIMEOUT);
    assert_eq!(
        count_line(&lines, "async_free"),
        1,
        "the undrained completion is released through async_free exactly once; log = {lines:?}"
    );
    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "async_free"),
        1,
        "no second release: the completion had exactly one owner; log = {lines:?}"
    );
    drop(poller);
}

// ---------------------------------------------------------------------------------------------
// The `ErlDrvPort` a driver sees never moves
// ---------------------------------------------------------------------------------------------

/// The `ErlDrvPort` the driver stashes at `start` is the **same pointer** it reads back in every
/// later callback — the property the C gives a driver for free (`&prt->drv_port` never moves) and
/// the one that lets a driver keep the handle in its own data.
///
/// The fixture's `hdl` token makes every callback append `handle=<%p>` for the handle the driver
/// **stashed at `start`**; `start` logs one and each later `output` callback (driven through the
/// real port data path) logs its own. The assertion is only meaningful as a *cross-callback*
/// comparison, so the shape of the log is pinned first: a callback line, then that callback's own
/// `handle=` line, for `start` and for each `output` — proving the two handles come from two
/// different callbacks (never two captures inside one), and that the value read back later is
/// byte-identical to the one stashed.
///
/// The handle is also still *usable* later: the `output` callback sends its echo through the very
/// handle it stashed at `start` (`driver_output(st->port, …)`), and those bytes reach the port
/// owner — so the pointer is not merely unchanged but still names the live port.
#[test]
fn the_erl_drv_port_handle_a_driver_sees_is_the_same_pointer_through_the_ports_life() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("async-handle-stability");
    let command = format!("{} hdl", probe.command);
    let (mut poller, mut registry, id) = open_port(&so, &command, host);

    // `start` already logged its handle during the open; two *later* `output` callbacks log their own.
    for payload in [b"x".as_slice(), b"y".as_slice()] {
        assert_eq!(
            registry
                .output(&mut poller, id, payload)
                .expect("the driver's output callback runs"),
            payload.len(),
            "the driver consumes the whole output buffer"
        );
        assert_eq!(
            round_data(&mut poller, &mut registry, id).expect("the echo reaches the port owner"),
            payload.to_vec(),
            "the port moves data through the handle it stashed at start"
        );
    }

    let lines = log_lines(&probe.log);
    // The interleaving proves the handles below come from three *different* callbacks.
    assert_eq!(
        log_shape(&lines),
        vec![
            "start".to_string(),
            "handle".to_string(),
            "output".to_string(),
            "handle".to_string(),
            "output".to_string(),
            "handle".to_string(),
        ],
        "each callback appends its own event line and, under `hdl`, its own handle line; \
         log = {lines:?}"
    );

    let handles = handle_lines(&lines);
    assert_eq!(
        handles.len(),
        3,
        "`start` and the two later `output` callbacks each logged one handle; log = {lines:?}"
    );
    assert_eq!(
        handles[0], handles[1],
        "the ErlDrvPort a driver stashes at start is the same pointer a later callback reads back; \
         log = {lines:?}"
    );
    assert!(
        handles.iter().all(|handle| *handle == handles[0]),
        "every logged handle carries the same pointer; log = {lines:?}"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// The documented **counterpart** of the pointer-stability contract: the handle *pointer* is stable
/// for the port's whole life, but the *host context* that makes it usable is installed only for the
/// duration of a driver callback.
///
/// A `driver_output` on a plain `std::thread` — a driver's own worker, with no callback running — is
/// the C's `-1`, and its bytes are dropped; the same call inside a callback reaches the port owner
/// (the echo tests above prove the `0` arm). The handle here is a real one: a boxed [`AbiHost`]
/// behind a boxed [`DriverEnv`] — exactly the shape `DriverPort::from_library` builds (both boxed so
/// the pointer never moves) — so the pointer stays valid across the thread; only the callback-scoped
/// context is missing.
#[test]
fn a_driver_output_from_a_plain_thread_is_refused() {
    let mut host = Box::new(AbiHost::new(Box::new(LocalAtomPortHost::new())).expect("a driver heap"));
    let mut env = Box::new(DriverEnv::new(&mut *host));
    let port = env.as_port();

    // On this (non-callback) thread the handle is already refused, so the refusal is about the
    // missing callback context rather than about thread identity.
    let mut byte = *b"x";
    // SAFETY: `port` is the live handle for this test's body (the env/host outlive it), and `byte`
    // points at one readable byte.
    assert_eq!(
        unsafe {
            beam_abi::driver::drv::driver_output(port, &mut byte as *mut u8 as *mut libc::c_char, 1)
        },
        -1,
        "with no driver callback running the handle cannot reach the port's queue"
    );

    // And from a plain worker thread, as a driver's own `driver_async` worker would call it. Only
    // the handle's *address* crosses the thread: raw pointers are not `Send` (and must not be), so
    // the test round-trips the integer address, which keeps the pointer identity under test.
    let addr = port as usize;
    let code = std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let port = addr as ErlDrvPort;
                let mut byte = *b"x";
                // SAFETY: `port` is the live handle for this test's body (the env/host outlive the
                // scope), and `byte` points at one readable byte.
                unsafe {
                    beam_abi::driver::drv::driver_output(
                        port,
                        &mut byte as *mut u8 as *mut libc::c_char,
                        1,
                    )
                }
            })
            .join()
            .expect("the plain worker thread runs")
    });
    assert_eq!(
        code, -1,
        "a driver_output from a plain std::thread has no installed callback context — the C's -1"
    );

    // The handle is still the same, valid pointer after the round-trip: nothing moved.
    assert_eq!(env.as_port(), port, "the port handle never moves");
}

// ---------------------------------------------------------------------------------------------
// Exactly-once delivery of each completion
// ---------------------------------------------------------------------------------------------

/// `ready_async` runs **exactly once per `driver_async` job**, and a later drive point re-delivers
/// nothing.
///
/// A completion is a single value the drive point hands to exactly one arm: two jobs therefore
/// produce exactly two `ready_async` calls, and once they are drained the port reports no pending
/// async work — so a second drive point cannot hand the same completion over again. The count of the
/// fixture's `ready_async h=same` lines is the observable: a duplicate (or a missing) delivery moves
/// it.
#[test]
fn each_job_reaches_ready_async_exactly_once_and_is_never_re_delivered() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("async-exactly-once");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    for _ in 0..2 {
        registry
            .port_control(&mut poller, id, "14", &[])
            .expect("control verb 14 schedules a job");
    }

    let processed = drive_until(&mut poller, &mut registry, id, 2, ASYNC_TIMEOUT);
    assert_eq!(processed, 2, "each of the two jobs reported its own completion");

    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "ready_async h=same"),
        2,
        "one ready_async call per job — no more, no fewer; log = {lines:?}"
    );
    assert_eq!(
        count_line(&lines, "async_free"),
        0,
        "ready_async owned each job's async_data, so async_free never ran"
    );
    assert!(
        !registry
            .get(id)
            .expect("a live port")
            .has_pending_async(),
        "both drained jobs leave nothing pending on the port"
    );

    // A further drive point has nothing left to deliver: draining is not re-delivery.
    assert_eq!(
        registry
            .drive_async(&mut poller, id)
            .expect("the async drive point runs"),
        0,
        "a drained port re-delivers nothing"
    );
    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "ready_async h=same"),
        2,
        "the drained completion was never handed over a second time; log = {lines:?}"
    );

    registry.close(&mut poller, id).expect("the port closes");
}
