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
//! **A process that dies while its engine sits parked gives its off-heap payloads back** — the
//! regression suite for the one engine-table reclamation (`RunnerState::prune_dead`, called at
//! the scheduler-pass boundary).
//!
//! # The defect this file pins
//!
//! `RunnerState::engines` (`src/runner/entry.rs`) is the pid → engine table, and an engine is the
//! sole holder of a process's [`Machine`], which is the sole holder of that process's **off-heap
//! refc-binary reservoir** (`Machine::refc_binaries` — [*one* holder, deliberately not mirrored
//! onto the PCB](src/runner/run.rs)'s off-heap note) and of its binary virtual-heap budget
//! (`ProcessEngine::off_heap`). The map was only ever emptied by the per-slice take-out in
//! `runner/run.rs`, which puts the engine **back** at every park arm, so a process that died while
//! parked — `exit(Pid, kill)` from another process, an exit signal serviced at schedule-in, a
//! raise while blocked — left its machine in the map for the runtime's life, holding every
//! payload that process had built off-heap.
//!
//! # The witness: `binary_alloc`'s live bytes
//!
//! `beam_alloc`'s `binary_alloc` allocator owns every refc payload (`beam_alloc::RefcBinary`'s
//! payload is the registry's `BINARY` type, whose allocator is `AllocatorId::Binary`), and its
//! summed `live_bytes` is **exactly** the figure `src/context/memory_hook.rs` reports as
//! `erlang:memory(binary)`. So the leak is read here as a number: the victim builds
//! [`PAYLOAD_COUNT`] × [`PAYLOAD_BYTES`] of payload, parks, and is killed — and the figure must
//! fall back to the pre-spawn baseline instead of staying 16 MiB up.
//!
//! Every assertion is a **relation or an exact return to baseline**: no wall-clock figure, no
//! load-dependent number, and the drain loop is bounded by counted passes (with the cap itself
//! asserted), never by a sleep. This host is shared.
//!
//! # The two death paths, in one test
//!
//! Both variants live in a **single** `#[test]` on purpose: `beam_alloc`'s allocator registry is
//! process-global, so two tests running in parallel threads of the same binary would each see the
//! other's payloads in the figure they are asserting on.
//!
//! 1. [`DEATH_KILL`] — an untrappable `exit(Pid, kill)` while the victim is parked. This is the
//!    leak: the pid leaves the process table (the signal is serviced eagerly, in the killing
//!    process's own `exit/2`), and **the engine is the only thing left**.
//! 2. [`DEATH_STOP`] — an ordinary message the parked `receive` matches, so the process resumes,
//!    returns and dies through the scheduler's own `RunOutcome::Exit(normal)`. This one is the
//!    **control**: it was already correct (the returning arm never stores the engine back), and
//!    it is what would go red if the prune ever touched a **live** parked engine — the park
//!    must still be woken, resumed and run.
//!
//! # Fixture
//!
//! `tests/fixtures/pk.beam` is a committed `.beam` image produced by the system `erlc`
//! (`+deterministic`; the recipe is in `tests/fixtures/README.md`), so the suite needs no
//! compiler at run time — only the system Erlang installation the boot sequence resolves. The
//! source (`tests/fixtures/pk.erl`) is committed beside it: `hold/2` builds `N` off-heap binaries
//! of a **runtime** size (`<<0:(Bytes*8)>>`, so `bs_create_bin` really allocates a payload — a
//! literal size would be folded away), keeps all of them live in a list and parks in an untimed
//! `receive`, and `kill/1`/`stop/1` are the two endings.
//!
//! A missing system Erlang installation is a **loud skip** ([`support::require_oracle`]), never a
//! silent pass.
// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::time::{Duration, Instant};

use beam_alloc::registry::all_stats;
use beam_alloc::AllocType;
use beam_engine::terms;
use beam_process::{ProcessId, ProcessState};
use beam_rt::Runtime;
use beam_term::{is_atom, is_small, make_small, signed_value};

/// The committed fixture (`hold/2`, `kill/1`, `stop/1`).
const PK_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pk.beam");

/// The suite's name in the loud-skip banner.
const SUITE: &str = "parked_exit_reclaim";

/// The byte size of **one** payload the victim keeps live: far above
/// `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` (64 bytes), so every one of them is a genuine
/// off-heap refc payload held by the machine's reservoir.
const PAYLOAD_BYTES: u64 = 8 * 1024;

/// How many payloads the victim builds and keeps live before it parks — 2 048 × 8 KiB =
/// **16 MiB** of `binary_alloc` live bytes, i.e. large enough that a residue is unmistakable
/// against the boot sequence's own few hundred kilobytes.
const PAYLOAD_COUNT: i64 = 2_048;

/// The one-payload allowance the post-drain figure is given over the baseline: a refc payload's
/// block is `PAYLOAD_BYTES` plus its header, and `beam_alloc`'s carrier bookkeeping may keep the
/// two live blocks it just released. Anything more than one payload still held means an engine
/// outlived its process.
const ONE_PAYLOAD_BYTES: u64 = PAYLOAD_BYTES + 4 * 1024;

/// What the pre-kill reading must show above the baseline: 20 payloads. A reservoir that is
/// there at all holds 2 048 of them, so this can never pass vacuously on a run whose victim never
/// built (or never retained) the payloads.
const MIN_RESERVOIR_BYTES: u64 = 20 * PAYLOAD_BYTES;

/// The bound on the passes any one wait may take. Every wait below asserts the cap, so a
/// regression can only fail, never hang.
const MAX_PASSES: u32 = 4_000;

/// How many passes the post-death drain may take to reach the baseline.
///
/// One pass is enough in practice (the death and the prune both happen inside the pass that
/// observes it); the allowance exists so the assertion is about the *figure returning*, not about
/// which pass it happened in — and it is small enough that a leak (which never returns) fails.
const DRAIN_PASSES: u32 = 64;

/// A generous wall-clock safety net for one measured phase of the run; it bounds a pathological
/// hang only, and nothing is asserted against it.
const PHASE_DEADLINE: Duration = Duration::from_secs(180);

/// Which ending the victim gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// `exit(Victim, kill)` from a helper process — the untrappable kill, and the leak.
    Kill,
    /// `Victim ! stop` from a helper process — the parked process resumes, returns and exits
    /// normally (the control).
    Stop,
}

impl Ending {
    /// The fixture function the helper process runs (`pk:kill/1` / `pk:stop/1`).
    fn entry(self) -> &'static str {
        match self {
            Ending::Kill => "kill",
            Ending::Stop => "stop",
        }
    }

    /// The label used in the assertion messages.
    fn label(self) -> &'static str {
        match self {
            Ending::Kill => "exit(Pid, kill)",
            Ending::Stop => "Pid ! stop",
        }
    }
}

/// The `binary_alloc` live-bytes figure — literally what [`erlang:memory(binary)`] reports
/// (`src/context/memory_hook.rs` sums the same `beam_alloc::registry::all_stats()` rows), read
/// directly so the test needs no BIF call and no term round trip.
fn binary_live_bytes() -> u64 {
    all_stats()
        .into_iter()
        .filter(|(ty, _)| *ty == AllocType::Binary)
        .map(|(_, stats)| stats.live_bytes as u64)
        .sum()
}

/// A runtime with the fixture loaded and the boot sequence already driven to quiescence.
///
/// The drain matters: `Runtime::spawn_process` boots on demand, and the boot script's own
/// processes would otherwise contend with the measured processes for slices (and their own
/// allocations would land in the `binary_alloc` figure the assertions read).
fn runtime_with_fixture() -> Option<Runtime> {
    support::require_oracle(SUITE)?;
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    let mut guard = 0u32;
    while rt.run_one_slice().expect("a boot-drain slice runs") > 0 {
        guard += 1;
        assert!(guard < 100_000, "the boot process tree never quiesced");
    }
    rt.load_module(PK_BEAM).expect("the pk fixture loads");
    Some(rt)
}

/// Runs slices until the victim **parks** (its PCB state is `Waiting`, which the runner sets in
/// the `receive` park arm), asserting the pass cap.
///
/// Reaching `Waiting` also means the whole build finished — the payloads are built *before* the
/// `receive` — so the reservoir is at its full size when this returns.
fn run_until_parked(rt: &mut Runtime, victim: ProcessId) {
    let deadline = Instant::now() + PHASE_DEADLINE;
    for _ in 0..MAX_PASSES {
        if rt.processes().get(victim).map(|p| p.state()) == Some(ProcessState::Waiting) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the victim never parked within {}s",
            PHASE_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
    }
    panic!("the victim never parked within {MAX_PASSES} scheduler passes");
}

/// Spawns the fixture helper (`pk:kill/1` or `pk:stop/1`) with the victim's pid as its argument,
/// so both endings go through real Erlang code (`exit/2` / `!`) rather than through an internal
/// runtime API.
fn spawn_helper(rt: &mut Runtime, ending: Ending, victim: ProcessId) -> ProcessId {
    rt.spawn_process("pk", ending.entry(), vec![victim.to_eterm()])
        .unwrap_or_else(|err| panic!("pk:{} must spawn: {err}", ending.entry()))
}

/// Runs slices until `done` (a *counted* wait, capped by [`MAX_PASSES`]), asserting the cap.
fn run_until(rt: &mut Runtime, what: &str, mut done: impl FnMut(&Runtime) -> bool) {
    let deadline = Instant::now() + PHASE_DEADLINE;
    for _ in 0..MAX_PASSES {
        if done(rt) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within {}s",
            PHASE_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
    }
    panic!("{what} did not happen within {MAX_PASSES} scheduler passes");
}

/// The `{stopped, N}` tuple `pk:hold/2` returns when the parked `receive` matches `stop`, decoded
/// through the frozen term accessors (a wrong shape is a fixture/engine bug, not an outcome).
///
/// The tag is asserted to be an **atom** and the count to be exactly [`PAYLOAD_COUNT`], which is
/// together enough to say the resumed process ran the `stop` clause of its `receive` with the
/// whole payload list in hand: a killed process returns nothing at all, and the `_Other` clause
/// would have to carry the same shape to be confused with it.
fn stopped_count(value: beam_term::Eterm) -> beam_term::Eterm {
    let arity = terms::tuple_arity(value).expect("the stopped result is a tuple");
    assert_eq!(arity, 2, "pk:hold/2 returns a {{stopped, Count}} pair");
    let tag = terms::tuple_element(value, 0).expect("the pair's first element");
    let count = terms::tuple_element(value, 1).expect("the pair's second element");
    assert!(is_atom(tag), "the pair's tag must be an atom (`stopped`)");
    assert!(is_small(count), "the pair's count must be a small integer");
    assert_eq!(signed_value(count), PAYLOAD_COUNT, "every payload is held");
    tag
}

/// One ending, driven end to end: build + park, observe the reservoir, end the process, and
/// assert the figure returns to the pre-spawn baseline.
///
/// Returns the baseline/peak/post figures so the caller's own assertions can be stated against
/// them (and printed on failure).
fn measure_ending(ending: Ending) -> (u64, u64, u64) {
    let mut rt = runtime_with_fixture().expect("the fixture runtime");
    // The baseline is taken **after** the boot drain and the fixture load, so it already carries
    // everything the runtime allocates for itself; nothing but the victim's payloads moves it
    // afterwards.
    let baseline = binary_live_bytes();
    let victim = rt
        .spawn_process(
            "pk",
            "hold",
            vec![make_small(PAYLOAD_BYTES as i64), make_small(PAYLOAD_COUNT)],
        )
        .expect("pk:hold/2 must spawn");
    run_until_parked(&mut rt, victim);
    let held = binary_live_bytes();
    assert!(
        held > baseline + MIN_RESERVOIR_BYTES,
        "the parked victim must hold its off-heap reservoir: baseline {baseline}, held {held}, \
         expected more than {} more bytes ({})",
        MIN_RESERVOIR_BYTES,
        ending.label()
    );

    // The ending, issued by a **real** Erlang process: `pk:kill/1` is `exit(Pid, kill)` and
    // `pk:stop/1` is `Pid ! stop`.
    let helper = spawn_helper(&mut rt, ending, victim);
    run_until(&mut rt, "the victim to leave the process table", |rt| {
        rt.processes().get(victim).is_none()
    });
    // The helper itself has returned by now (its body is one BIF call), so it is gone too.
    run_until(&mut rt, "the helper to finish", |rt| {
        rt.processes().get(helper).is_none()
    });

    // (a) The pid is gone from the table, and the *outcome* the runtime recorded matches the
    // ending. A kill by an exit signal terminates the pid **in the process table** (`exit/2` is
    // serviced eagerly for a non-running target), so the runner records no raise for it: the
    // reason travels to the victim's links and monitors, and there is nothing for it to answer
    // here. The control's `receive` matched, so the victim really ran to its `return` and its
    // `x(0)` — the `{stopped, Count}` pair — is the recorded result.
    assert!(rt.processes().get(victim).is_none(), "the victim is dead");
    match ending {
        Ending::Kill => {
            assert!(
                rt.process_exit(victim).is_none(),
                "a kill by an exit signal records no runner raise"
            );
            assert!(
                rt.process_result(victim).is_none(),
                "a killed process never returned a value"
            );
        }
        Ending::Stop => {
            let tag = stopped_count(
                rt.process_result(victim)
                    .expect("the resumed victim returned its value"),
            );
            assert!(
                is_atom(tag),
                "the resumed victim took its `stop` clause (an atom tag, not the `_Other` one)"
            );
            assert!(
                rt.process_exit(victim).is_none(),
                "a normally-returning process records no raise"
            );
        }
    }

    // (b) The reservoir comes back to the baseline within a **bounded** number of passes.
    let mut drained = None;
    for pass in 0..DRAIN_PASSES {
        let live = binary_live_bytes();
        if live <= baseline + ONE_PAYLOAD_BYTES {
            drained = Some((pass, live));
            break;
        }
        rt.run_one_slice().expect("a drain slice runs");
    }
    let (passes, after) = drained.unwrap_or_else(|| {
        panic!(
            "the dead process's off-heap reservoir was never released after {} passes \
             ({}): baseline {baseline}, held {held}, now {}",
            DRAIN_PASSES,
            ending.label(),
            binary_live_bytes()
        )
    });
    eprintln!(
        "parked_exit_reclaim: {} — baseline {baseline} B, held {held} B (+{} over baseline), \
         released after {passes} drain pass(es) at {after} B",
        ending.label(),
        held - baseline
    );
    (baseline, held, after)
}

/// **The regression**: a parked process that dies gives its off-heap payloads back — on both
/// death paths, in one test (the allocator registry is process-global, see the module docs).
#[test]
fn a_parked_process_that_dies_releases_its_off_heap_reservoir() {
    let (kill_baseline, kill_held, kill_after) = measure_ending(Ending::Kill);
    assert!(
        kill_after <= kill_baseline + ONE_PAYLOAD_BYTES,
        "after an untrappable kill the parked engine's payloads are released: baseline \
         {kill_baseline}, held {kill_held}, now {kill_after}"
    );

    let (stop_baseline, stop_held, stop_after) = measure_ending(Ending::Stop);
    assert!(
        stop_after <= stop_baseline + ONE_PAYLOAD_BYTES,
        "after a resume-and-return the payloads are released: baseline {stop_baseline}, held \
         {stop_held}, now {stop_after}"
    );
    assert!(
        stop_held > stop_baseline + MIN_RESERVOIR_BYTES,
        "the control's victim held its reservoir too: baseline {stop_baseline}, held {stop_held}"
    );
}
