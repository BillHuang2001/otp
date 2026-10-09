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

//! End-to-end integration test for the driver **C-ABI host layer** (`src/abi/`), over the real,
//! compiled C driver fixture `tests/fixtures/host_driver.c`.
//!
//! This is the proof that a **real** driver shared object — one that leaves every `driver_*` /
//! `erl_drv_*` symbol *undefined* and resolves them out of the loading process, exactly like an OTP
//! driver — can be `dlopen`ed and driven end to end by this crate:
//!
//! * `driver_init`'s `ErlDrvEntry` is loaded through
//!   [`beam_abi::driver::loader::load`](beam_abi) and wrapped in a [`DriverPort`] (contract **A.4**,
//!   the frozen driver C ABI);
//! * the [`DriverPort`] is opened as a **real port** in a [`PortRegistry`] over a real [`Poller`];
//! * data is driven in and out through the port's own seams — the driver's `output` callback, the
//!   `port_command` write-readiness task, a `driver_select`ed descriptor routed back through
//!   `ready_input`, a `driver_set_timer` deadline, `erl_drv_output_term`'s term sink and a
//!   `driver_async` completion — and every observable is asserted on the Rust side.
//!
//! # The fixture's behaviour (see `tests/fixtures/host_driver.c`)
//!
//! `driver_name = "host_driver"`; `start` parses `fd=<n>` (the descriptor it will `driver_select`
//! on), `log=<path>` (the file its callbacks append an event line to) and optional bare tokens
//! `upper` (its `output` upper-cases the bytes it echoes) and `hdl` (every callback additionally
//! logs its stashed `ErlDrvPort`). Its integer `control` verbs are `1`
//! echo into `rbuf`, `2` `driver_set_timer(ms)`, `3` `driver_cancel_timer`, `4`
//! `driver_select(READ|USE)`, `5` `erl_drv_output_term({host_driver, 1})`, `6` `driver_async`,
//! `7` `driver_select(READ, 0)`, `8` `driver_read_timer`, `9` `driver_select(READ|USE)`, `10`
//! `driver_select(READ|USE_NO_CALLBACK)`, `11` `driver_select(READ)`, `12`
//! `driver_select(READ, 0)`, `13` a fixed reply re-pointed at the driver's own buffer, `14`–`16`
//! the `driver_async` completions owned by a `ready_async`/`async_free` (the latter the subject of
//! `tests/abi_async.rs`) and `17`/`18` the sticky `ERL_DRV_USE` re-selections; any
//! other verb answers the negative `cres` that `port_control/3` reports as `badarg`. Its `output`
//! echoes via `driver_output`, its `ready_input` reads the selected fd and `driver_output`s the
//! bytes, its `ready_output` emits `"W"`, its `timeout` emits `"T"`, its `ready_async`/`async_free`
//! log their arm and its `stop_select` appends
//! `stop_select` to the log — so every callback's firing is observable as port data or as a log
//! line.
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixture needs host facilities a bare container may lack: a C compiler and the
//! system OTP include directory (`erl_driver.h`). When either is missing every test prints a **loud**
//! `SKIP:` banner and returns. A load failure is **never** a skip: it means the `driver_*` host
//! symbols are not in this test binary's dynamic symbol table (see
//! [`force_driver_symbols_link`]), which is a wiring bug, so the `dlopen` diagnostic text is
//! panicked with.
//!
//! # The descriptor must be non-blocking
//!
//! The fixture's `ready_input` reads the selected descriptor in a plain `read(2)` loop down to
//! `EAGAIN`/EOF. This crate's reactor is edge-triggered, so the descriptor handed to `fd=<n>` must
//! be non-blocking or that loop blocks forever (exactly why `SpawnDriver` switches an adopted
//! `{fd, N}` descriptor to `O_NONBLOCK`); the socket-pair test below sets it explicitly.

mod cdriver;

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::prelude::*;
use beam_process::PortId;
use beam_term::immediate::{is_atom, is_small, signed_value};

use cdriver::*;

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------
/// The driver's `output` callback echoes bytes back to the port owner.
///
/// This is the driver→owner data path: `watch the port's output queue, the driver reads it in its
/// `output` callback and calls `driver_output`, and the port-task round reports the bytes as the
/// `PortNotification::Data` a runtime turns into `{Port, {data, Data}}`.
#[test]
fn the_drivers_output_callback_echoes_bytes_back_to_the_owner() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    assert_eq!(
        registry.get(id).expect("a live port").driver_name(),
        "host_driver",
        "the port reports the loaded driver's name"
    );

    // The port's inline output path hands the bytes to the driver's `output` callback, which
    // echoes them with `driver_output`; the callback reports the whole buffer consumed (the C
    // `output` callback returns `void`).
    let written = registry
        .output(&mut poller, id, b"hello")
        .expect("the driver's output callback runs");
    assert_eq!(written, 5, "the driver consumes the whole output buffer");

    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the echo reaches the port owner"),
        b"hello".to_vec(),
        "the driver's output callback echoed the bytes back through driver_output"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// A `port_command` write-readiness task delivers the command bytes through the driver's `output`
/// callback **and** runs its `ready_output` notification callback.
///
/// `port_command`/`PortRegistry::port_command` only queues the bytes and schedules an `OUTPUT`
/// task (the `erlang:port_command/2` contract); running that task reproduces the C's combined
/// effect: the queued bytes go to the entry's `output`/`outputv` slot (the C's `port_command` →
/// `output`/`outputv`, `io.c:1782`/`io.c:1517`) and, when the entry also declares `ready_output`,
/// that slot then runs as the C's write-readiness notification (the `ERTS_PORT_TASK_OUTPUT` arm,
/// `erl_port_task.c:1793-1797`). The fixture echoes the bytes its `output` receives and emits `"W"`
/// from `ready_output`, both through `driver_output`, so the owner sees `"hello"` then `"W"` — the
/// order in which the C performs them, coalesced by the port's single input queue.
#[test]
fn a_port_command_write_readiness_task_reaches_ready_output() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    registry
        .port_command(&mut poller, id, b"hello", PortCommandOptions::EMPTY)
        .expect("the command is queued");
    let outcome = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the OUTPUT task runs");
    assert_eq!(outcome.ran, 1, "the queued command schedules one task");
    // One round both runs the task and harvests what it produced, so both echoes are in this
    // outcome's notifications (a second round would find the port's input queue already drained).
    assert_eq!(
        data_of(&outcome, id),
        vec![b"helloW".to_vec()],
        "the OUTPUT task writes the queued bytes through the driver's output callback, then runs \
         its ready_output notification — both echo through driver_output into the port's input \
         queue, so the owner sees them coalesced in that order"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// The fixture's `start(port, command)` really parses its command string: the `upper` token makes
/// the driver's `output` callback upper-case what it echoes.
///
/// This is the evidence that the command string built by `open_port/2` reached the loaded driver's
/// `start` callback intact (through `DriverPort`'s C string) and was honoured per port instance.
#[test]
fn the_start_command_reaches_the_driver() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "upper", host);

    registry
        .output(&mut poller, id, b"hello")
        .expect("the driver's output callback runs");
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the upper-cased echo"),
        b"HELLO".to_vec(),
        "the `upper` start token reached the driver and changed its output"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// `erl_drv_output_term` builds the term on the host heap and hands it to the [`PortHost`] sink:
/// the fixture's control verb `5` sends the 2-tuple `{host_driver, 1}` to the port owner.
///
/// This is the driver→owner **term** path (`DriverHost::send_term`), the counterpart of
/// `driver_output`'s byte path; the assertion inspects the term with `beam_term`, so the atom's
/// *identity* (its name in the host's table) and the integer's value are both pinned.
#[test]
fn erl_drv_output_term_delivers_the_expected_tuple() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    let reply = registry
        .port_control(&mut poller, id, "5", &[])
        .expect("control verb 5 runs the driver's erl_drv_output_term");
    assert!(reply.is_empty(), "the fixture's verb 5 answers no reply bytes");

    assert_eq!(
        state.borrow().sent.len(),
        1,
        "the driver sent exactly one term to the owner"
    );
    let (to, term) = state.borrow().sent[0];
    assert!(
        to.is_none(),
        "erl_drv_output_term sends to the port owner (no explicit pid)"
    );

    let (arity, elements) = tuple_parts(term);
    assert_eq!(arity, 2, "the fixture builds the 2-tuple {{host_driver, 1}}");
    assert!(is_atom(elements[0]), "the first element is an atom");
    assert_eq!(
        atom_name(&state, elements[0]),
        "host_driver",
        "the atom is host_driver"
    );
    assert!(is_small(elements[1]), "the second element is a small int");
    assert_eq!(signed_value(elements[1]), 1, "the second element is 1");

    registry.close(&mut poller, id).expect("the port closes");
}

/// `driver_set_timer` arms the port's `TIMEOUT` task, the timer fires the driver's `timeout`
/// callback (which emits `"T"`), and `driver_cancel_timer` drops the deadline again.
///
/// The evidence chain: control verb `2` (with a 100 ms `int` argument) makes
/// [`PortRegistry::next_deadline`] answer `Some` (the driver's declaration is mirrored into the
/// port's task); after the deadline passes the next round runs the `TIMEOUT` task, whose callback
/// reports `"T"` and consumes the deadline; control verb `3` clears it, so `next_deadline` is
/// `None`.
#[test]
fn a_driver_timer_is_armed_fires_and_cancels() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    // Arm a 100 ms timer: `driver_set_timer(port, 100)` (the `int` rides in the control buffer).
    registry
        .port_control(&mut poller, id, "2", &100i32.to_le_bytes())
        .expect("control verb 2 arms the driver's timer");
    let deadline = registry
        .next_deadline()
        .expect("the driver's driver_set_timer deadline is mirrored into the port");
    assert!(
        deadline > Instant::now(),
        "the deadline is in the future, so the callback cannot have run yet"
    );
    assert!(
        !await_data_ready(&mut poller, &mut registry, id),
        "the timeout callback must not run before its deadline"
    );

    // Let the deadline pass; the next round runs the TIMEOUT task.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the timeout callback emitted its marker"),
        b"T".to_vec(),
        "the driver's timeout callback ran and emitted `T`"
    );
    assert!(
        registry.next_deadline().is_none(),
        "a fired driver timer leaves no deadline behind"
    );

    // Re-arm, then cancel: the driver's declaration is what `next_deadline` reports.
    registry
        .port_control(&mut poller, id, "2", &1_000i32.to_le_bytes())
        .expect("control verb 2 re-arms the timer");
    assert!(registry.next_deadline().is_some());
    registry
        .port_control(&mut poller, id, "3", &[])
        .expect("control verb 3 cancels the timer");
    assert!(
        registry.next_deadline().is_none(),
        "driver_cancel_timer clears the driver's deadline and the port's task"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// Whether one non-blocking port-task round reports data for `id` (used to prove a not-yet-due
/// timer does **not** fire early).
fn await_data_ready(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> bool {
    let events = poller
        .poll(Some(Duration::from_millis(10)))
        .expect("the poll succeeds");
    registry.schedule_events(&events);
    round_data(poller, registry, id).is_some()
}

/// A `driver_select`ed descriptor routes readiness back through the driver's `ready_input`
/// callback and out as port data.
///
/// This is the full reactor path over a real OS descriptor: control verb `4` makes the driver call
/// `driver_select(port, fd, READ)` (registered with the poller under the port number), the
/// descriptor becomes readable, the poller's readiness event schedules an `INPUT` task, and the
/// task runs the driver's `ready_input`, which `read(2)`s the fd and `driver_output`s the bytes.
/// Control verb `7` deselects, which deregisters the fd from the poller.
#[test]
fn a_selected_descriptor_routes_input_through_ready_input() {
    let Some(so) = fixture_so() else {
        return;
    };

    // A socket pair stands in for the descriptor a driver would select on. The read end must be
    // non-blocking: the fixture's `ready_input` loops `read(2)` down to EAGAIN, and the reactor is
    // edge-triggered (see the module docs).
    let (read_end, mut write_end) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
    read_end
        .set_nonblocking(true)
        .expect("the selected descriptor must be non-blocking");
    let read_fd = read_end.as_raw_fd();

    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, &format!("fd={read_fd}"), host);

    // Select the read fd: the driver registers it with the poller through this port's context.
    registry
        .port_control(&mut poller, id, "4", &[])
        .expect("control verb 4 selects the descriptor");
    assert_eq!(
        poller.len(),
        1,
        "driver_select registered the descriptor with the poller"
    );

    // Write to the peer: the readiness event schedules the INPUT task, which reads it back.
    write_end.write_all(b"from-the-socket").expect("the write succeeds");
    assert_eq!(
        await_data(&mut poller, &mut registry, id, Duration::from_secs(2)),
        b"from-the-socket".to_vec(),
        "the driver's ready_input read the selected descriptor and reported the bytes"
    );

    // Deselect: the registration is gone.
    registry
        .port_control(&mut poller, id, "7", &[])
        .expect("control verb 7 deselects the descriptor");
    assert!(
        poller.is_empty(),
        "driver_select(.., 0) deregistered the descriptor from the poller"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// A `driver_async` job is scheduled and its completion is driven through the async drive point.
///
/// `driver_async` runs the driver's invoke callback on a worker thread; there is **no port-task
/// kind** for the async hand-off in Milestone-1, so the runtime must call the drive point
/// [`PortRegistry::drive_async`] to drain the finished job (the C runs it from `call_async_ready`
/// on the port's scheduler). This test pins that: the completion is not delivered until
/// `drive_async` is called, and once the completion is observed the worker has finished, so the
/// port keeps working.
///
/// The fixture's entry leaves `ready_async` **and** `async_free` `NULL`, so the observable result
/// of the drive point is that the job is drained and reported as processed rather than that a
/// callback runs.
#[test]
fn an_async_job_is_scheduled_and_its_completion_is_driven() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    // Schedule the async job (control verb 6). `driver_async` answers a queue position, which the
    // adapter returns as the control result — a non-negative position means the job was accepted.
    registry
        .port_control(&mut poller, id, "6", &[])
        .expect("control verb 6 schedules the async job");
    assert!(
        registry.next_deadline().is_none(),
        "the async job declares no port timer"
    );

    // Wait for the worker to finish and the completion to be drained by the drive point.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut processed = 0usize;
    while processed == 0 && Instant::now() < deadline {
        processed = registry
            .drive_async(&mut poller, id)
            .expect("the async drive point runs");
        if processed == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(
        processed >= 1,
        "the driver_async job completed but its completion was never drained by `drive_async`"
    );

    // The completion proved the worker finished; drop whatever the worker's driver_output left
    // (Milestone-1 has no installed port context on a worker thread, so it may be nothing at all),
    // then prove the port still moves data.
    let _drained = round_data(&mut poller, &mut registry, id);
    registry
        .output(&mut poller, id, b"still-alive")
        .expect("the driver's output callback still runs");
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("the port still moves data"),
        b"still-alive".to_vec(),
        "the port keeps working after an async job"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// Closing the port runs the loaded driver's `stop` callback and releases its id, without crashing.
///
/// The fixture's `stop` frees the per-port state it allocated in `start` — the evidence that the
/// callback really ran is that the free is safe on that state, plus the port notification and the
/// empty reactor the close leaves behind.
#[test]
fn closing_a_loaded_driver_port_runs_stop_and_releases_the_port() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    registry
        .output(&mut poller, id, b"before-close")
        .expect("the port works before the close");
    assert_eq!(
        round_data(&mut poller, &mut registry, id).expect("pre-close data"),
        b"before-close".to_vec()
    );

    registry.close(&mut poller, id).expect("the driver's stop runs");
    assert!(!registry.contains(id), "the closed port is gone");
    assert!(poller.is_empty(), "the closed port holds no poller registration");

    let outcome = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the close round runs");
    assert!(
        outcome
            .notifications
            .iter()
            .any(|notification| matches!(notification, PortNotification::Closed { port } if *port == id)),
        "the round after a close reports the closed port"
    );
}

// ---------------------------------------------------------------------------------------------
// `ERL_DRV_USE` / `stop_select`: the driver's release callback
// ---------------------------------------------------------------------------------------------
//
// `driver_select` with `ERL_DRV_USE` in `mode` tells the runtime the driver wants
// `stop_select(event, NULL)` when the selection is really gone — the last moment at which it may
// close or recycle the handle. `ERL_DRV_USE_NO_CALLBACK` keeps the bookkeeping but declines the
// callback. The fixture's `stop_select` appends `stop_select` to its `log=<path>` file (it gets no
// `ErlDrvData`, so it logs through the path the most recent `start` published), which is how these
// tests observe it. The C fires it on the deselecting `driver_select(..., 0)`
// (`erl_check_io.c:1210-1216`) and at port teardown for a registration the driver left selected
// (`erl_check_io.c:2232-2238`).

/// `stop_select` fires when an `ERL_DRV_USE` selection is de-selected, and it fires for the
/// *deselecting* `driver_select`'s event (the operand the C hands back).
#[test]
fn stop_select_fires_when_a_use_selection_is_deselected() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("stop-select-deselect");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    // Control verb 9 is `driver_select(port, fd, ERL_DRV_READ | ERL_DRV_USE, 1)`.
    assert_eq!(
        registry
            .port_control(&mut poller, id, "9", &[])
            .expect("control verb 9 selects READ|USE"),
        Vec::<u8>::new(),
        "the fixture's select verbs answer no reply bytes"
    );
    assert_eq!(poller.len(), 1, "the selection registered with the reactor");
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string()],
        "a live ERL_DRV_USE selection owes nothing yet"
    );

    // Control verb 12 de-selects; the release callback runs once the driver's own call returns.
    registry
        .port_control(&mut poller, id, "12", &[])
        .expect("control verb 12 de-selects");
    assert!(poller.is_empty(), "the de-select withdrew the registration");
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string(), "stop_select".to_string()],
        "the ERL_DRV_USE release fired the driver's stop_select"
    );

    registry.close(&mut poller, id).expect("the port closes");
    assert_eq!(
        log_lines(&probe.log),
        vec![
            "start".to_string(),
            "stop_select".to_string(),
            "stop".to_string()
        ],
        "nothing was left selected, so teardown fires no further stop_select"
    );
}

/// `stop_select` fires at port teardown for a registration the driver left selected — after the
/// driver's `stop` callback, and with the poller registration withdrawn.
#[test]
fn stop_select_fires_on_port_stop_for_a_still_selected_use_fd() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("stop-select-teardown");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    registry
        .port_control(&mut poller, id, "9", &[])
        .expect("control verb 9 selects READ|USE");
    assert_eq!(poller.len(), 1, "the selection registered with the reactor");

    registry.close(&mut poller, id).expect("the port closes");
    assert_eq!(
        log_lines(&probe.log),
        vec![
            "start".to_string(),
            "stop".to_string(),
            "stop_select".to_string()
        ],
        "teardown runs the driver's stop first, then releases the still-selected ERL_DRV_USE"
    );
    assert!(
        poller.is_empty(),
        "the teardown withdrew the poller registration of the selected fd"
    );
}

/// A plain select and an `ERL_DRV_USE_NO_CALLBACK` select owe no `stop_select` — neither on
/// de-select nor at teardown.
#[test]
fn stop_select_does_not_fire_for_a_plain_or_no_callback_selection() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("stop-select-none");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    // Control verb 11 is a plain `driver_select(..., ERL_DRV_READ, 1)`.
    registry
        .port_control(&mut poller, id, "11", &[])
        .expect("control verb 11 selects READ");
    registry
        .port_control(&mut poller, id, "12", &[])
        .expect("control verb 12 de-selects");
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string()],
        "a plain select owes no release callback"
    );

    // Control verb 10 is `driver_select(..., ERL_DRV_READ | ERL_DRV_USE_NO_CALLBACK, 1)`.
    registry
        .port_control(&mut poller, id, "10", &[])
        .expect("control verb 10 selects READ|USE_NO_CALLBACK");
    registry
        .port_control(&mut poller, id, "12", &[])
        .expect("control verb 12 de-selects");
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string()],
        "ERL_DRV_USE_NO_CALLBACK declines the release callback"
    );

    // The same at teardown: the still-selected registration owes nothing.
    registry
        .port_control(&mut poller, id, "10", &[])
        .expect("control verb 10 selects READ|USE_NO_CALLBACK");
    registry.close(&mut poller, id).expect("the port closes");
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string(), "stop".to_string()],
        "a `_NO_CALLBACK` registration still owes nothing when the port goes away"
    );
}

/// A port dropped **without** being closed still releases its `ERL_DRV_USE` selections: that is
/// [`DriverPort`]'s `Drop`, the teardown floor for a port whose `stop` never ran.
#[test]
fn stop_select_fires_when_a_port_is_dropped_without_being_closed() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("stop-select-drop");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    registry
        .port_control(&mut poller, id, "9", &[])
        .expect("control verb 9 selects READ|USE");
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string()],
        "nothing has been released yet"
    );

    // Drop the whole registry: every live `Port` — and so every `DriverPort` — is dropped.
    drop(registry);
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string(), "stop_select".to_string()],
        "dropping a port without `stop` still releases the still-selected ERL_DRV_USE"
    );
    drop(poller);
}

/// A re-selection that omits `ERL_DRV_USE` keeps the release obligation an earlier select took on
/// (the C's sticky `ERTS_EV_FLAG_USED`), so a later deselect still fires `stop_select`.
///
/// Control verb 17 is `driver_select(READ | ERL_DRV_USE, 1)` followed by
/// `driver_select(READ, 1)`: the second call says nothing about `ERL_DRV_USE`, and the obligation
/// must survive it.
#[test]
fn a_reselection_without_use_keeps_the_stop_select_obligation() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("stop-select-sticky-keep");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    registry
        .port_control(&mut poller, id, "17", &[])
        .expect("control verb 17 selects READ|USE and then re-selects READ");
    assert_eq!(
        poller.len(),
        1,
        "the USE-less re-selection re-registers the same fd, not a second one"
    );

    registry
        .port_control(&mut poller, id, "12", &[])
        .expect("control verb 12 de-selects");
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string(), "stop_select".to_string()],
        "the obligation survived the USE-less re-selection, so the deselect fired stop_select"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// A re-selection that carries `ERL_DRV_USE_NO_CALLBACK` clears the obligation a select took on, so
/// a later deselect fires no `stop_select` (the C's `no_stop_select_callback`).
///
/// Control verb 18 is `driver_select(READ | ERL_DRV_USE, 1)` followed by
/// `driver_select(READ | ERL_DRV_USE_NO_CALLBACK, 1)`: the second call declines the callback.
#[test]
fn a_reselection_declining_the_callback_clears_the_stop_select_obligation() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("stop-select-sticky-decline");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    registry
        .port_control(&mut poller, id, "18", &[])
        .expect("control verb 18 selects READ|USE and then re-selects READ|USE_NO_CALLBACK");
    assert_eq!(
        poller.len(),
        1,
        "the declining re-selection re-registers the same fd, not a second one"
    );

    registry
        .port_control(&mut poller, id, "12", &[])
        .expect("control verb 12 de-selects");
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string()],
        "the _NO_CALLBACK re-selection cleared the obligation, so the deselect fires nothing"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// `stop_select` is owed **exactly once per release**: re-selecting an `ERL_DRV_USE` fd (with or
/// without repeating the bit) fires nothing extra, the deselect fires it once, and a *new*
/// selection after that release owes its own single callback.
///
/// This is the "exactly once per port" property of the sticky `ERTS_EV_FLAG_USED` bookkeeping: the
/// obligation lives with the registration, is merged (never multiplied) by a re-select, and is
/// consumed exactly once when the registration is released — whether that release is a deselect,
/// port teardown or a `Drop`. The fixture's `stop_select` appends one line per call, so the *count*
/// of those lines is the assertion.
#[test]
fn a_use_selection_owes_stop_select_exactly_once_per_release() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let probe = logged_port_command("stop-select-once-per-release");
    let (mut poller, mut registry, id) = open_port(&so, &probe.command, host);

    // Select USE, then re-select twice — once repeating USE, once plain (`READ`).
    registry
        .port_control(&mut poller, id, "9", &[])
        .expect("control verb 9 selects READ|USE");
    registry
        .port_control(&mut poller, id, "9", &[])
        .expect("control verb 9 re-selects READ|USE");
    registry
        .port_control(&mut poller, id, "11", &[])
        .expect("control verb 11 re-selects plain READ");
    assert_eq!(
        poller.len(),
        1,
        "the re-selections re-register the same fd rather than adding a second registration"
    );
    assert_eq!(
        log_lines(&probe.log),
        vec!["start".to_string()],
        "a live ERL_DRV_USE selection owes nothing yet — a re-selection fires no stop_select"
    );

    // The deselect releases the registration and fires the callback exactly once.
    registry
        .port_control(&mut poller, id, "12", &[])
        .expect("control verb 12 de-selects");
    assert_eq!(
        count_line(&log_lines(&probe.log), "stop_select"),
        1,
        "three selects and one deselect owe exactly one release callback"
    );

    // A fresh selection takes on a fresh obligation, released once more at teardown.
    registry
        .port_control(&mut poller, id, "9", &[])
        .expect("control verb 9 selects READ|USE again");
    registry.close(&mut poller, id).expect("the port closes");
    let lines = log_lines(&probe.log);
    assert_eq!(
        count_line(&lines, "stop_select"),
        2,
        "one callback per released selection: the deselect's and the teardown's; log = {lines:?}"
    );
    assert_eq!(
        count_line(&lines, "stop"),
        1,
        "teardown ran the driver's stop exactly once; log = {lines:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// `port_control/3` -> the C driver's `control` callback
// ---------------------------------------------------------------------------------------------

/// The `port_control/3` reply path: the driver's `control` callback writes through `rbuf`/`rlen`
/// (or re-points `*rbuf` at its own memory) and its `cres` is the reply length, and a `cres < 0`
/// answer surfaces as the typed `badarg` (`io.c:4023-4083`).
#[test]
fn port_control_returns_the_drivers_reply_bytes() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    // Control verb 13 answers from the driver's **own** buffer, re-pointing `*rbuf` (the C lets a
    // driver do that and keep the pointer valid until the next `control` call).
    assert_eq!(
        registry
            .port_control(&mut poller, id, "13", &[])
            .expect("control verb 13 answers a reply"),
        b"host-reply".to_vec(),
        "the driver's reply bytes are copied out of its own buffer"
    );

    // Control verb 1 echoes the argument into the caller's buffer, `min(len, rlen)` bytes long.
    assert_eq!(
        registry
            .port_control(&mut poller, id, "1", b"reply-bytes")
            .expect("control verb 1 echoes"),
        b"reply-bytes".to_vec()
    );
    assert_eq!(
        registry
            .port_control(&mut poller, id, "1", &[])
            .expect("an empty argument echoes nothing"),
        Vec::<u8>::new()
    );

    // An unrecognised verb is the fixture's negative `cres` ⇒ `BeamError::BadArg`.
    let err = registry
        .port_control(&mut poller, id, "9999", &[])
        .expect_err("an unknown verb must fail");
    assert!(
        matches!(err, BeamError::BadArg),
        "a negative `cres` is the C's `badarg` (`io.c:4074`), got {err:?}"
    );

    // A verb that is not a decimal `c_uint` never reaches the driver.
    let err = registry
        .port_control(&mut poller, id, "not-a-number", &[])
        .expect_err("a non-numeric verb must fail");
    assert_eq!(err.as_atom(), Some("badarg"));

    registry.close(&mut poller, id).expect("the port closes");
}
