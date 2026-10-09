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

//! The **`outputv`-only** port-command callback and the **async-free** shape, over the real
//! compiled C driver fixture `tests/fixtures/vector_driver.c`.
//!
//! The C prefers the *vector* callback on every write path: `if (drv->outputv) { … } else { …
//! (*drv->output)(…) }` (`erts/emulator/beam/io.c:1782`). The sibling fixture `host_driver.c`
//! declares the opposite shape (`.output = host_output, .outputv = NULL`), so it can only prove the
//! fallback; this fixture is `.output = NULL, .outputv = vector_outputv`, so it is the only one that
//! can prove an entry whose **sole** port-command callback is `outputv` still receives its
//! `port_command` bytes — the divergence `tools/differential/GAPS.md` §"A.4" records ("an entry
//! whose only port-command callback is `outputv` never receives `port_command/2`").
//!
//! Both of the adapter's write paths are exercised, because the C reaches `outputv` from both:
//!
//! * `Driver::output` — the registry-level direct write ([`PortRegistry::output`]); and
//! * the **absent-`ready_output` fallback** of `Driver::ready_output` — what an `OUTPUT` port task
//!   runs ([`PortRegistry::port_command`] + [`PortRegistry::execute_port_tasks`]), matching the C's
//!   `no_ready_output_callback` default (`io.c:7524-7533`).
//!
//! The exact vector the adapter builds is pinned too (`io.c:1842-1864`): `iov[0]` is the empty
//! **header** block, `iov[1]` the data, `binv` runs parallel to `iov` with every entry NULL,
//! `vsize` counts the header block plus the data and `size` is the data's byte count. The fixture
//! logs that shape and echoes the data block back through `driver_output`, so the bytes the callback
//! received are observable as port data as well — and, in `upper` mode, so is the fact that the
//! callback mutates the caller's own buffer in place.
//!
//! The fixture's entry also has **no `ready_async`** (`.ready_async = NULL`), so it is the
//! `outputv` fixture *and* the async-free one: a finished `driver_async` job is released through the
//! driver's own `async_free` (the C's `need_free` path, `io.c:5399-5406`), **exactly once**, whether
//! or not the port is still alive when the job finishes.
//!
//! # Skip-vs-fail policy
//!
//! As in `tests/abi_driver.rs` and `tests/abi_async.rs`: a missing toolchain (`cc` /
//! `erl_driver.h`) prints a loud `SKIP:` and returns; a `dlopen` failure is a wiring bug and panics.

mod cdriver;

use std::path::Path;

use beam_io::prelude::*;
use beam_process::PortId;

use cdriver::*;

/// Opens a port around the `outputv`-only fixture, under the driver name its `driver_init` entry
/// carries (`vector_driver`).
fn open_vector(so: &Path, command: &str, host: TestPortHost) -> (Poller, PortRegistry, PortId) {
    open_named_port(so, "vector_driver", command, host)
}

// ---------------------------------------------------------------------------------------------
// The `outputv`-only port-command callback
// ---------------------------------------------------------------------------------------------

/// An `outputv`-only entry receives every byte written through the direct write path
/// (`Driver::output`), and the vector it is handed is exactly the shape the C builds.
///
/// Before the adapter preferred the `outputv` slot this was the recorded divergence: the entry's
/// absent `output` slot meant the write path had nothing to call, so the bytes were counted as
/// consumed and silently vanished.
#[test]
fn an_outputv_only_entry_receives_the_output_path_bytes() {
    let Some(so) = vector_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("vector-output");
    let (mut poller, mut registry, id) = open_vector(&so, &probe.command, host);

    assert_eq!(
        registry
            .output(&mut poller, id, b"hello")
            .expect("the entry's outputv callback runs"),
        5,
        "the whole drained buffer is counted as written (the C's output callback is void)"
    );

    // The fixture echoes `iov[1]` through `driver_output`, so the bytes the callback received come
    // back to the port owner as port data.
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the echo reaches the port owner"),
        b"hello".to_vec(),
        "the outputv callback received the data block and echoed it verbatim"
    );

    // The vector the adapter built: `iov[0]` the empty header block, `iov[1]` the data, `binv` all
    // NULL, `vsize` = 2 (header + data), `size` = the data's byte count.
    let shape = "outputv vsize=2 size=5 head=null head_len=0 binv0=null binv1=null";
    let lines = await_log_line(&probe.log, shape, ASYNC_TIMEOUT);
    assert_eq!(
        count_line(&lines, shape),
        1,
        "one write-path call is one outputv call; log = {lines:?}"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// The absent-`ready_output` fallback of `Driver::ready_output` runs the same `outputv` write path,
/// so a `port_command` on this entry lands with an `OUTPUT` port task as the caller.
///
/// This is the C's `no_ready_output_callback` default (`io.c:7524-7533`): an entry with no
/// `ready_output` slot is handed its queued output through the ordinary write path.
#[test]
fn the_absent_ready_output_fallback_also_runs_outputv() {
    let Some(so) = vector_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("vector-ready-output-fallback");
    let (mut poller, mut registry, id) = open_vector(&so, &probe.command, host);

    registry
        .port_command(&mut poller, id, b"abc", PortCommandOptions::EMPTY)
        .expect("port_command queues the bytes and schedules the OUTPUT task");
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the OUTPUT task wrote the bytes"),
        b"abc".to_vec(),
        "the OUTPUT task drained the port's queue through the outputv-only entry"
    );

    let shape = "outputv vsize=2 size=3 head=null head_len=0 binv0=null binv1=null";
    let lines = await_log_line(&probe.log, shape, ASYNC_TIMEOUT);
    assert_eq!(
        count_line(&lines, shape),
        1,
        "the queued bytes reached the vector callback exactly once; log = {lines:?}"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// The `start` command's own tokens reach the driver, and the vector's data block is the caller's
/// **own** buffer (which the C also hands over, and which a driver may rewrite in place).
///
/// The fixture's `upper` token makes `vector_outputv` upper-case the bytes it is given before
/// echoing them, so an upper-cased echo simultaneously proves the command string arrived at `start`
/// and that the bytes the driver touched are the ones the port sent.
#[test]
fn a_start_command_token_reaches_the_driver_and_mutates_the_bytes_in_place() {
    let Some(so) = vector_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("vector-upper");
    let command = format!("{} upper", probe.command);
    let (mut poller, mut registry, id) = open_vector(&so, &command, host);

    assert_eq!(
        registry
            .output(&mut poller, id, b"abc")
            .expect("the entry's outputv callback runs"),
        3,
        "the driver mutated the buffer in place; it still counts as fully consumed"
    );
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the echo reaches the port owner"),
        b"ABC".to_vec(),
        "the `upper` token reached start, so the callback upper-cased the very bytes it was handed"
    );

    // The logged vector still describes the untouched byte count (`size` is the length, not the
    // contents) — the mutation is in-place, so the shape is unchanged.
    let shape = "outputv vsize=2 size=3 head=null head_len=0 binv0=null binv1=null";
    let lines = await_log_line(&probe.log, shape, ASYNC_TIMEOUT);
    assert_eq!(
        count_line(&lines, shape),
        1,
        "the vector shape is unchanged by an in-place mutation; log = {lines:?}"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

// ---------------------------------------------------------------------------------------------
// The async-free shape (the entry has no `ready_async` slot)
// ---------------------------------------------------------------------------------------------

/// An `outputv`-only entry with no `ready_async` has no callback to hand a finished job to, so the
/// host releases the job's `async_data` through the driver's own `async_free` (the C's `need_free`
/// path, `io.c:5399-5406`) — **exactly once per job**, and a later drive point re-delivers nothing.
#[test]
fn an_entry_without_ready_async_releases_each_job_through_async_free_exactly_once() {
    let Some(so) = vector_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("vector-async-free");
    let (mut poller, mut registry, id) = open_vector(&so, &probe.command, host);

    // Control verb 1 is `driver_async(port, NULL, vector_async_invoke, st, vector_async_free)`.
    registry
        .port_control(&mut poller, id, "1", &[])
        .expect("control verb 1 schedules the fast job");

    // The worker logs when the job body finished; the release is deferred to the port's drive point.
    let lines = await_log_line(&probe.log, "async_invoke", ASYNC_TIMEOUT);
    assert_eq!(
        count_line(&lines, "async_free"),
        0,
        "the release happens at the drive point, never on the worker thread"
    );

    let processed = drive_until(&mut poller, &mut registry, id, 1, ASYNC_TIMEOUT);
    assert!(processed >= 1, "the completion was drained");

    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "async_free"),
        1,
        "an entry with no ready_async releases each job through async_free exactly once; \
         log = {lines:?}"
    );
    assert!(
        !registry
            .get(id)
            .expect("a live port")
            .has_pending_async(),
        "the drained job leaves nothing pending on the port"
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
        count_line(&lines, "async_free"),
        1,
        "the release was never repeated; log = {lines:?}"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// `driver_async` with a `NULL` `invoke` schedules no work, so no `ready_async` can ever run for it
/// and the host must release the data through `async_free` **right away** (the crate's documented
/// deviation from the C, which dereferences the null `invoke` and faults — refusing to crash must
/// not leak).
#[test]
fn a_fire_and_forget_job_is_released_through_async_free_right_away() {
    let Some(so) = vector_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("vector-async-null-invoke");
    let (mut poller, mut registry, id) = open_vector(&so, &probe.command, host);

    // Control verb 3 is `driver_async(port, NULL, NULL, st, vector_async_free)`: the release runs
    // inside the call, so no drive point is needed (or possible — nothing was scheduled).
    registry
        .port_control(&mut poller, id, "3", &[])
        .expect("control verb 3 schedules the fire-and-forget form");

    let lines = settle_log(&probe.log);
    assert_eq!(
        count_line(&lines, "async_free"),
        1,
        "the fire-and-forget form is released through async_free exactly once; log = {lines:?}"
    );
    assert_eq!(
        count_line(&lines, "async_invoke"),
        0,
        "a NULL invoke runs no job body"
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

/// A port dropped **while its job is still running** still gets the job's `async_data` released
/// exactly once: the worker's completion hand-off fails and the worker releases it itself — the
/// C's port-gone branch of `call_async_ready` (`erl_async.c:324-330`).
///
/// The job is the deliberately slow one (control verb 2 sleeps ~500 ms) so the drop really lands
/// mid-flight; the release is then observed by polling the fixture's log from the worker's own
/// thread. The port is dropped (not closed), so the fixture's `stop` never frees the state the
/// `async_free` logs through.
#[test]
fn dropping_the_port_while_a_job_is_in_flight_releases_it_exactly_once() {
    let Some(so) = vector_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("vector-async-drop-in-flight");
    let (mut poller, mut registry, id) = open_vector(&so, &probe.command, host);

    registry
        .port_control(&mut poller, id, "2", &[])
        .expect("control verb 2 schedules the slow job");

    // Drop the whole registry: every live `Port` — and so the port's `DriverPort` — goes away while
    // the job sleeps on its worker.
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
fn dropping_the_port_after_a_job_completed_but_before_draining_releases_it_exactly_once() {
    let Some(so) = vector_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("vector-async-drop-after-complete");
    let (mut poller, mut registry, id) = open_vector(&so, &probe.command, host);

    registry
        .port_control(&mut poller, id, "1", &[])
        .expect("control verb 1 schedules the fast job");

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
        "the teardown release happens exactly once — never twice; log = {lines:?}"
    );
    drop(poller);
}
