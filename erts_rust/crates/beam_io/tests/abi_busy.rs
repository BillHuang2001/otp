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

//! The busy-port state (`set_busy_port` → `Driver::is_busy`) and the port layer's **backpressure
//! decision table** (`Port::command` / `PortRegistry::port_command_with`), pinned end to end over the
//! real, compiled C driver fixtures `tests/fixtures/host_driver.c` and
//! `tests/fixtures/host_driver_soft_busy.c`.
//!
//! The C's `void set_busy_port(ErlDrvPort port, int on)` (`io.c:5208-5264`) moves the port's busy
//! state and always answers `0` for a live port. `beam_abi` discards that code — the entry callback
//! is `void` and the `driver_*` C function ignores what the hook returns — so the observable is the
//! **flag**: [`AbiHost`] records it per port, [`AbiHost::is_busy`] reads it back (the same "safe
//! companion of a raw hook" shape `DriverState::queue_len` has for `DriverHost::sizeq`), and
//! [`Driver::is_busy`]/[`Port::is_busy`]/[`PortRegistry::is_busy`] forward it to the decision table.
//!
//! # What is pinned
//!
//! | path | assertion |
//! |---|---|
//! | the raw hook, `AbiHost::set_busy_port` | answers the C's `0`; the flag reads back set, cleared, and idempotently |
//! | the fixture's `busy` start token (`start` runs `set_busy_port(port, 1)`) | the port is busy before it is handed any command |
//! | the fixture's control verbs `25`/`26` (`set_busy_port(port, 1)` / `(port, 0)`) | the flag follows the driver's own requests |
//!
//! and, through a `PortRegistry`/`Port::command` — the C's `erts_port_output` busy arm
//! (`io.c:1763-1769`) and `erl_bif_port.c`'s two option arms:
//!
//! | state | options | pinned answer |
//! |---|---|---|
//! | idle | — | [`PortCommandOutcome::Done`]: queued, written by the driver, echoed to the owner |
//! | busy | — | [`PortCommandOutcome::Busy`]: **nothing** queued, no `OUTPUT` task, nothing reaches the driver |
//! | busy | `[nosuspend]` | [`PortCommandOutcome::Busy`] (the runtime answers `false`) |
//! | idle **or** busy | `[force]`, driver without `ERL_DRV_FLAG_SOFT_BUSY` | `Err` = `error:notsup`, nothing attempted |
//! | busy | `[force]`, driver **with** `ERL_DRV_FLAG_SOFT_BUSY` | [`PortCommandOutcome::Done`]: the state is bypassed and the driver writes |
//! | busy → control verb 26 | — | back to [`PortCommandOutcome::Done`] |
//!
//! # Why the two host-level tests build their own host rather than using a live port
//!
//! This crate exposes no accessor for the interior `ErlDrvPort` of an already-opened port (`Port`
//! hands out its `driver_name`, never its driver/environment), so — exactly like
//! `tests/driver_registry_slots.rs` — those two tests build an [`AbiHost`] the way
//! `DriverPort::from_library` does (`AbiHost::for_driver` over the loaded fixture's name, `loaded =
//! true`), wrap it in a [`DriverEnv`] and give the fixture **that** handle. The C `driver_*`
//! functions, the fixture's `start`/`control` callbacks and the `AbiHost` hook are all the real
//! ones; only the host *instance* is the test's, built from identical inputs.
//!
//! # The caller-blocking half is the runtime's; the decision is not
//!
//! The port layer reports the state and what a command may do with it: [`PortCommandOutcome::Busy`]
//! (nothing queued), or the `notsup` raise for a `[force]` on a driver without
//! `ERL_DRV_FLAG_SOFT_BUSY`. What only a runtime can do is the part that needs the **process**:
//! answer `false` for `[nosuspend]`, suspend a caller that did not ask for it, and re-issue the
//! command once the driver clears the state (`erts_port_resume_procs`, `io.c:5265`).
//!
//! # Skip-vs-fail policy
//!
//! Compiling the fixtures needs a C compiler and the system OTP include directory
//! (`erl_driver.h`); when either is missing the test prints a loud `SKIP:` banner and returns. A
//! load failure is **never** a skip — it means the `driver_*` host symbols are not in this test
//! binary's dynamic symbol table, which is a wiring bug, so the `dlopen` diagnostic is panicked.

mod cdriver;

use std::ffi::c_char;

use beam_abi::driver::host::DriverEnv;
use beam_abi::driver::types::{ErlDrvData, ErlDrvSizeT};

use beam_io::prelude::*;

use cdriver::{
    data_of, fixture_so, force_driver_symbols_link, load_path, open_port, soft_busy_fixture_so,
    TestPortHost,
};

/// The raw hook answers the C's `0` and the flag it records is readable through the accessor the
/// port layer consults: set, cleared, and idempotent in both directions.
///
/// This drives `beam_abi`'s `set_busy_port` slot (`driver_api_table().set_busy_port`, the C function
/// a loaded driver resolves out of the loading process) against a live `DriverEnv`, so the whole
/// chain — the frozen API-table slot, the `env_for` host recovery, the `DriverHost` hook and the
/// accessor — is exercised, not just the hook.
#[test]
fn the_set_busy_port_hook_records_and_clears_the_flag_and_answers_zero() {
    let Some(so) = fixture_so() else {
        return;
    };
    force_driver_symbols_link();

    let name = beam_abi::driver::loader::load(load_path(&so), "host_driver")
        .expect("the fixture loads")
        .name();
    assert_eq!(name, "host_driver");

    let slot = beam_abi::driver::drv::driver_api_table()
        .set_busy_port
        .expect("set_busy_port is populated in the frozen API table");

    let mut host = AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), name, true)
        .expect("a driver heap");
    assert!(!host.is_busy(), "a fresh port is idle");

    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    // `void set_busy_port(ErlDrvPort, int)` — the callback is `void`, so the hook's `0` is the only
    // code there is (asserted on the host in-memory suite); the observable here is the flag.
    // SAFETY: `port` came from `DriverEnv::as_port` on the live `env` above.
    unsafe { slot(port, 1) };
    assert!(
        host.is_busy(),
        "the hook recorded the busy state on the port's host"
    );

    // Idempotent: a driver may re-assert the state it already asked for.
    // SAFETY: as above.
    unsafe { slot(port, 1) };
    assert!(host.is_busy(), "re-asserting busy keeps the port busy");

    // SAFETY: as above.
    unsafe { slot(port, 0) };
    assert!(!host.is_busy(), "clearing the flag reads back idle");
}

/// The fixture's own `set_busy_port` calls: the `busy` start token (busy **before** any command, the
/// state a `port_command/2,3` on a freshly opened port must consult) and the control verbs `25`/`26`
/// that flip it from a later callback.
///
/// The three fixture paths run through the real compiled `.so`, so this is the end-to-end half of the
/// hook: `driver_mk_port`-style handle identity (the `ErlDrvPort` the driver stashed in `start` is
/// still the one the C `set_busy_port` resolves a host from), the fixture's command parsing and the
/// `AbiHost` flag.
#[test]
fn the_fixture_sets_the_busy_state_at_start_and_through_its_control_verbs() {
    let Some(so) = fixture_so() else {
        return;
    };
    force_driver_symbols_link();

    let library =
        beam_abi::driver::loader::load(load_path(&so), "host_driver").expect("the fixture loads");

    let mut host = AbiHost::for_driver(Box::new(LocalAtomPortHost::new()), "host_driver", true)
        .expect("a driver heap");

    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    // The `busy` start token: `host_start` runs `set_busy_port(port, 1)` itself, so the port is busy
    // **before** it is handed any command.
    let command = c"busy";
    let data: ErlDrvData = library
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture's start callback runs");
    assert!(
        host.is_busy(),
        "the fixture's `busy` start token marked the port busy at start time"
    );

    // Control verb `26` clears it through the handle the fixture stashed at `start`.
    let mut reply: *mut c_char = core::ptr::null_mut();
    let cleared = library
        .control(
            data,
            26,
            core::ptr::null_mut(),
            0,
            &mut reply,
            0 as ErlDrvSizeT,
        )
        .expect("the fixture's control callback runs");
    assert_eq!(cleared, 0, "the C's `set_busy_port` answers no reply");
    assert!(
        !host.is_busy(),
        "the fixture's control verb 26 cleared the busy flag"
    );

    // Control verb `25` sets it again.
    let mut reply: *mut c_char = core::ptr::null_mut();
    let set = library
        .control(
            data,
            25,
            core::ptr::null_mut(),
            0,
            &mut reply,
            0 as ErlDrvSizeT,
        )
        .expect("the fixture's control callback runs");
    assert_eq!(set, 0, "the C's `set_busy_port` answers no reply");
    assert!(
        host.is_busy(),
        "the fixture's control verb 25 set the busy flag"
    );

    // Release the fixture's per-port state (it frees through `driver_alloc`/`driver_free`).
    library.stop(data).expect("the fixture's stop callback runs");
}

// ---------------------------------------------------------------------------------------------
// The decision table, over a real port in a real `PortRegistry`
// ---------------------------------------------------------------------------------------------

/// Row **idle ⇒ `Done`**: the first command of a fresh port is queued, an `OUTPUT` task is
/// scheduled, and running the round really delivers the bytes to the driver — whose `output`
/// callback echoes them — plus the `ready_output` marker the fixture's entry also declares.
///
/// This is the baseline every other row is a deviation from: the state probes read the driver's own
/// answer, and `port_closed` is `false` because only a driver with a latch (the `tcp_inet` one)
/// reports a peer close.
#[test]
fn an_idle_loaded_port_takes_the_command_and_the_driver_really_writes_it() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);

    assert!(
        !registry.is_busy(id).expect("a live port"),
        "the driver reports no busy state"
    );
    assert!(
        !registry.port_closed(id).expect("a live port"),
        "the C-ABI port reports no peer close (that latch is the inet driver's)"
    );

    let outcome = registry
        .port_command_with(&mut poller, id, b"hi", PortCommandOptions::EMPTY)
        .expect("an idle port takes the command");
    assert_eq!(
        outcome,
        PortCommandOutcome::Done,
        "the C's ERTS_PORT_OP_DONE — the runtime answers `true` for it"
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 2);
    assert!(
        registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output),
        "the write is scheduled"
    );

    // The queued bytes reach the driver's `output` callback (the echo) and its `ready_output`
    // notification runs after it — one `driver_output` call each, so two messages, in this order.
    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the OUTPUT task runs");
    assert_eq!(
        data_of(&round, id),
        vec![b"hi".to_vec(), b"W".to_vec()],
        "the driver consumed the command and echoed it"
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        0,
        "the driver reports the whole command written"
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// Row **busy ⇒ `Busy`**: the `busy` start token marks the port at `start` time, so the very first
/// command on a freshly opened port is refused with **nothing** queued — no bytes in the output
/// queue, no `OUTPUT` task, and a full port-task round that reaches the driver with nothing.
///
/// Queueing here would deliver the command twice (or out of order) once the runtime re-issues it
/// after the driver clears the state, which is what the C's suspend-and-retry does.
#[test]
fn the_busy_start_token_makes_the_first_command_busy_with_nothing_queued() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "busy", host);

    assert!(
        registry.is_busy(id).expect("a live port"),
        "the fixture marked the port busy in its `start` callback"
    );

    assert_eq!(
        registry
            .port_command_with(&mut poller, id, b"hi", PortCommandOptions::EMPTY)
            .expect("a busy port is not an error"),
        PortCommandOutcome::Busy,
        "the C's ERTS_PORT_OP_BUSY — the runtime suspends the caller and retries"
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        0,
        "a busy port queues nothing"
    );
    assert!(
        !registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output),
        "a busy port schedules no write"
    );

    // The proof that nothing was queued: a full round runs no task and the driver reports nothing.
    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(round.ran, 0, "there is no task to run");
    assert!(
        data_of(&round, id).is_empty(),
        "the driver was handed nothing, so it echoed nothing"
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 0);

    registry.close(&mut poller, id).expect("the port closes");
}

/// Row **busy + `[nosuspend]` ⇒ `Busy`**: the option changes nothing at this layer — the state is
/// the driver's — and the runtime is what turns the outcome into the oracle's `false`
/// (`erl_bif_port.c:213-214`). What must not happen here is a queue-and-write.
#[test]
fn a_nosuspend_command_on_a_busy_port_is_the_busy_outcome() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "busy", host);

    assert_eq!(
        registry
            .port_command_with(
                &mut poller,
                id,
                b"hi",
                PortCommandOptions::EMPTY.with_nosuspend(true)
            )
            .expect("a busy port is not an error"),
        PortCommandOutcome::Busy,
        "the runtime answers `false` for this, not the port"
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 0);
    assert!(
        !registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output)
    );

    registry.close(&mut poller, id).expect("the port closes");
}

/// Rows **busy (control verb 25) ⇒ `Busy`** and **busy → control verb 26 ⇒ `Done`**: the driver
/// flips its own state from a later callback, which is the point the runtime's suspended caller is
/// resumed at (`set_busy_port(port, 0)` → `erts_port_resume_procs`).
#[test]
fn control_verb_25_marks_the_port_busy_and_verb_26_lets_the_command_through() {
    let Some(so) = fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    // An idle start: the port is only marked busy by the verb, so the transition is the driver's own.
    let (mut poller, mut registry, id) = open_port(&so, "", host);
    assert!(!registry.is_busy(id).expect("a live port"));

    // Verb 25: `set_busy_port(port, 1)`.
    let reply = registry
        .port_control(&mut poller, id, "25", &[])
        .expect("control verb 25 runs");
    assert!(reply.is_empty(), "the fixture's busy verbs answer no bytes");
    assert!(
        registry.is_busy(id).expect("a live port"),
        "the driver's own request marked the port busy"
    );
    assert_eq!(
        registry
            .port_command_with(&mut poller, id, b"hi", PortCommandOptions::EMPTY)
            .expect("a busy port is not an error"),
        PortCommandOutcome::Busy
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 0);

    // Verb 26: `set_busy_port(port, 0)` — the port takes commands again.
    let reply = registry
        .port_control(&mut poller, id, "26", &[])
        .expect("control verb 26 runs");
    assert!(reply.is_empty());
    assert!(!registry.is_busy(id).expect("a live port"));

    assert_eq!(
        registry
            .port_command_with(&mut poller, id, b"hi", PortCommandOptions::EMPTY)
            .expect("the cleared port takes the command"),
        PortCommandOutcome::Done
    );
    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the OUTPUT task runs");
    assert_eq!(
        data_of(&round, id),
        vec![b"hi".to_vec(), b"W".to_vec()],
        "the command really reached the driver once the state was cleared"
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 0);

    registry.close(&mut poller, id).expect("the port closes");
}

/// Row **[force] without `ERL_DRV_FLAG_SOFT_BUSY` ⇒ `error:notsup`**, decided from the entry's
/// **flag word**, never from the busy state (`erl_bif_port.c:195-198`) — so an idle port is refused
/// exactly like a busy one, and nothing is ever queued.
#[test]
fn force_on_a_driver_without_soft_busy_is_the_notsup_raise_busy_or_idle() {
    let Some(so) = fixture_so() else {
        return;
    };

    // Idle: the flag, not the state, is what the C tests.
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "", host);
    assert!(!registry.is_busy(id).expect("a live port"));
    let err = registry
        .port_command_with(
            &mut poller,
            id,
            b"hi",
            PortCommandOptions::EMPTY.with_force(true),
        )
        .expect_err("a driver without SOFT_BUSY refuses a forced command");
    assert_eq!(
        err.as_atom(),
        Some("notsup"),
        "the oracle raises `error:notsup` for `[force]` on a non-soft-busy driver"
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 0);
    registry.close(&mut poller, id).expect("the port closes");

    // Busy: the same answer, because the flag is checked first and independently of the state.
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "busy", host);
    assert!(registry.is_busy(id).expect("a live port"));
    let err = registry
        .port_command_with(
            &mut poller,
            id,
            b"hi",
            PortCommandOptions::EMPTY.with_force(true).with_nosuspend(true),
        )
        .expect_err("the flag decides, not the state");
    assert_eq!(err.as_atom(), Some("notsup"));
    assert_eq!(registry.get(id).expect("live").output_len(), 0);
    registry.close(&mut poller, id).expect("the port closes");
}

/// Row **busy + `[force]` on a driver that advertises `ERL_DRV_FLAG_SOFT_BUSY` ⇒ `Done`**: the C
/// clears its busy mask under `ERTS_PORT_SIG_FLG_FORCE` (`busy_flgs = 0`, `io.c:1731`), so the
/// command is queued and the driver really consumes it, on a port that stays busy.
///
/// The two sides of this C branch are the two fixture builds: `host_driver.c` (`driver_flags = 0`,
/// the `notsup` row above) and `host_driver_soft_busy.c` (the flag set), which differ in nothing but
/// that word.
#[test]
fn force_on_a_soft_busy_driver_bypasses_the_busy_state_and_the_driver_writes() {
    let Some(so) = soft_busy_fixture_so() else {
        return;
    };
    let (host, _state) = TestPortHost::new();
    let (mut poller, mut registry, id) = open_port(&so, "busy", host);

    assert!(
        registry.is_busy(id).expect("a live port"),
        "the port is busy, and force must ignore that"
    );
    assert_eq!(
        registry
            .port_command_with(
                &mut poller,
                id,
                b"hi",
                PortCommandOptions::EMPTY.with_force(true)
            )
            .expect("a soft-busy driver takes a forced command"),
        PortCommandOutcome::Done,
        "the busy state is bypassed, so the command is queued"
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 2);

    assert_eq!(
        data_of(
            &registry
                .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
                .expect("the OUTPUT task runs"),
            id
        ),
        vec![b"hi".to_vec(), b"W".to_vec()],
        "the bytes really reached the driver's output callback"
    );
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        0,
        "the driver consumed the whole command"
    );
    assert!(
        registry.is_busy(id).expect("a live port"),
        "`[force]` bypasses the state for one command; it does not clear it"
    );

    // A plain command on the still-busy port is refused again: the bypass was the option's, not a
    // state change.
    assert_eq!(
        registry
            .port_command_with(&mut poller, id, b"hi", PortCommandOptions::EMPTY)
            .expect("a busy port is not an error"),
        PortCommandOutcome::Busy
    );

    registry.close(&mut poller, id).expect("the port closes");
}
