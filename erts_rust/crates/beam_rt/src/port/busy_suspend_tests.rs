// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All rights reserved.
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

//! The **in-process** busy-port park → pump → resume probe (contract A.4).
//!
//! `src/port/busy_wait.rs` answers [`PortWrite::Parked`](super::busy_wait::PortWrite) for a caller
//! that may not pump the scheduler — a compiled process inside a scheduler slice — and
//! `RuntimeContext::request_suspend` turns that into
//! [`BifOutcome::Suspend`](beam_engine::BifOutcome), which [`crate::runner`] folds into a park on
//! the **instruction** (`RunOutcome::Yield`: the process is re-enqueued and the BIF re-executed
//! once the port is idle). Three modules cooperate, so the loop needs a pin no single one of them
//! owns. `src/port/busy_wait_tests.rs` pins the *verdict* (`Park`, nothing written, nothing
//! converted) and `tests/port_busy.rs` pins the CLI round trip through a **real C driver**; this
//! suite pins the loop **in process**, over
//! [`register_busy_control_port`](crate::port::test_support::register_busy_control_port)'s
//! registered driver, with the driver's own **write counter** as the observable: the suspending
//! attempt must consume nothing, so a completed command has written exactly **once**.
//!
//! # The two shapes
//!
//! | test | clear driven by | what it proves |
//! |---|---|---|
//! | [`a_parked_caller_is_resumed_and_writes_exactly_once`] | the harness, between two slices | the park itself: no result, `0` writes, still busy, one suspend — then the resume writes `1` |
//! | [`a_deadline_driven_clear_resumes_a_parked_caller_in_one_run`] | a helper's `receive … after T` → `erlang:port_control/3` | one `run_scheduled` drives the whole round trip: park, pump (ports → run queue → timers), re-issue |
//!
//! Neither asserts a **wall clock**: the fixture's only timing is a delay long enough that the
//! first attempt certainly meets a busy port, and every count is structural — the runner's
//! suspend counter (`crate::runner::run_slice_counters`) and the driver's write counter.
//!
//! Oracle (erts-17.1, OTP 29.1.1): the same call answers `true` and the driver's `output` callback
//! runs once — `tests/port_busy.rs`'s `busy_pc` row, the CLI-side twin of this probe.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use beam_term::{make_small, Eterm};

use crate::port::test_support::{register_busy_control_port, ControlVerb};
use crate::runtime::Runtime;

/// The committed fixture (see `tests/fixtures/port_suspend.erl` for why it exists and
/// `tests/fixtures/README.md` for the recipe).
const PORT_SUSPEND_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/port_suspend.beam"
);

/// The verb the fixture's `command_after/2` sends: `$N` in the C fixture is the clear
/// (`tests/fixtures/busy_driver.c` — clearing a busy bit is a **driver's** job, so a registered
/// fixture driver that recognises the verb is what makes `erlang:port_control/3` reach it).
const CLEAR: u32 = 78;

/// A **booted** runtime with the committed fixture module loaded, so an entry can be spawned by
/// name (`Runtime::spawn_process` resolves a module the code server already holds).
///
/// The boot is the preloaded modules plus the code path — enough for `spawn_process` and the port
/// table. No `timer_server` exists on this route (`Runtime::boot` loads the preloaded modules
/// only), which is why the fixture's clear is armed by a helper's `receive … after T` rather than
/// by `timer:apply_after/4`: both are runtime-native heap timers the pass fires.
fn runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the runtime boots");
    rt.code_server
        .load_path(PathBuf::from(PORT_SUSPEND_BEAM))
        .expect("the committed port_suspend fixture loads");
    rt
}

/// `true` — the atom the oracle's `port_command/2` answers for a command that was written, named
/// through the runtime's **shared** atom table (the one the compiled module compares against).
fn is_true(runtime: &mut Runtime, value: Eterm) -> bool {
    runtime.atom_name_of(value) == Some("true")
}

/// A parked caller is re-enqueued with the instruction **not applied**: nothing written, no
/// result, the port still busy — and the runner's suspend counter says the slice really ended at
/// the BIF park rather than at a reduction pre-emption. The clear (a driver's `$N`, which is what
/// another process's `erlang:port_control/3` runs) then lets the next slice re-issue the command,
/// which writes **exactly once**.
#[test]
fn a_parked_caller_is_resumed_and_writes_exactly_once() {
    let mut rt = runtime();
    let (port, flag, writes) = register_busy_control_port(
        &mut rt,
        "suspend_probe",
        vec![ControlVerb::clearing(CLEAR)],
        // Busy from the start: the first attempt must meet the C's `ERTS_PORT_OP_BUSY` arm.
        true,
        false,
    );
    let pid = rt
        .spawn_process("port_suspend", "command", vec![port.to_eterm()])
        .expect("port_suspend:command/1 spawns");

    crate::runner::reset_run_slice_counters();
    assert_eq!(rt.run_one_slice().expect("one slice runs"), 1);

    // The park, in the three facts that make it observable: no value, no write, and a slice that
    // ended at the suspend arm.
    assert!(
        rt.process_result(pid).is_none(),
        "a suspended caller has not returned"
    );
    assert!(
        rt.processes().get(pid).is_some(),
        "the parked caller is still a live process"
    );
    assert_eq!(writes.get(), 0, "the suspending attempt wrote nothing");
    assert!(
        flag.is_busy(),
        "the port is still busy: only the driver clears it"
    );
    assert!(
        crate::runner::run_slice_counters().suspends >= 1,
        "the slice must end at the BIF-suspend arm, not at a pre-emption"
    );

    // The clear — the C's `$N` `port_control` from another process (here: the harness itself,
    // between two slices, which is the same state change a port round or a second process makes).
    flag.set_busy(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "the resumed caller must re-issue the command and return"
        );
        rt.run_one_slice().expect("a slice runs");
    }

    let value = rt.process_result(pid).expect("the caller returned");
    assert!(
        is_true(&mut rt, value),
        "a resumed command answers the oracle's `true`"
    );
    assert_eq!(
        writes.get(),
        1,
        "the suspending attempt consumed nothing, so the command wrote exactly once"
    );
    assert!(
        rt.busy_port_waits.is_empty(),
        "a completed command removes its wait entry: {:?}",
        rt.busy_port_waits
    );
}

/// The same round trip inside **one** `Runtime::run_scheduled` call: the caller parks, the pass
/// pumps (ports → run queue → timers), the helper's `receive … after T` deadline fires its
/// `erlang:port_control/3` clear, and the re-issued command writes.
///
/// This is the in-process twin of the CLI's scheduled row (`tests/port_busy.rs`) and of the
/// `-eval` row that has always worked; the only difference between them is which route owns the
/// pass.
#[test]
fn a_deadline_driven_clear_resumes_a_parked_caller_in_one_run() {
    // `run_scheduled` boots the runtime itself (preloaded modules + code path), so the fixture
    // module is resolved from the path it is given — no `spawn_process` by name is needed.
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let (port, _flag, writes) = register_busy_control_port(
        &mut rt,
        "suspend_timer",
        vec![ControlVerb::clearing(CLEAR)],
        true,
        false,
    );

    crate::runner::reset_run_slice_counters();
    let value = rt
        .run_scheduled(
            PORT_SUSPEND_BEAM,
            "command_after",
            2,
            // 50 ms: far longer than the first attempt's slice, so that attempt certainly meets a
            // busy port. The delay gates nothing else — no assertion reads a clock.
            &[port.to_eterm(), make_small(50)],
        )
        .expect("the parked caller is resumed by the deadline-driven clear");
    assert!(
        is_true(&mut rt, value),
        "the resumed command answers the oracle's `true`"
    );
    assert_eq!(
        writes.get(),
        1,
        "the suspending attempt issued no write, so the re-issued command wrote exactly once"
    );
    assert!(
        crate::runner::run_slice_counters().suspends >= 1,
        "the run must have parked at the BIF at least once"
    );
    assert!(
        rt.busy_port_waits.is_empty(),
        "a completed command removes its wait entry: {:?}",
        rt.busy_port_waits
    );
}
