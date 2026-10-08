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

//! Integration evidence for the multi-scheduler (SMP) [`SchedulerPool`].
//!
//! Pins the concurrency the Stage-5 SMP pool adds on top of the frozen single-threaded slice
//! logic: `N` scheduler threads sharing one process table and one queue array, **work stealing**,
//! the pool-owned timer service and dirty pool, and the lifecycle/observable surface
//! (`stats` / `scheduler_stats` / `run_queue_len` / `wait_idle` / `take_error` / `shutdown` /
//! `join`).
//!
//! # Anti-flakiness
//!
//! Every assertion is forced by **synchronization**, never by `std::thread::sleep`:
//!
//! * [`SchedulerPool::wait_idle`] is the pool's own converging wait ("every queue empty **and** no
//!   pid in flight"), awaited with a generous timeout — never a wait on a fixed wall-clock delay.
//! * `mpsc` channels plus [`std::sync::mpsc::Receiver::recv_timeout`] carry the deterministic
//!   completion signals (a runner's *N*th invocation, a forwarded mailbox payload, the
//!   rescheduled process' second run, a worker entering a gated slice). A wedged pool fails the
//!   test with a clear timeout instead of hanging it.
//! * Where a runner **yields** — so the pool has to re-queue it, and a just-finished pid briefly
//!   lives in a worker-local flush buffer — the test additionally waits on an explicit "every
//!   process reached its blocking run" channel *before* reading counters. `wait_idle` is a
//!   *queue* barrier; the channel makes the per-pid run totals independent of the instant a
//!   yielded pid is re-enqueued, so the totals are exact rather than probabilistically-close.
//! * The work-stealing test (test 2) keeps its victims yielding until a **converging wait** on
//!   `SchedulerPool::stats().steals` has observed the steal, then releases them. This deliberately
//!   does *not* depend on a peer scheduler thread having been scheduled before the scenario starts
//!   — a worker that is still starting up would otherwise do all the work alone (there would be no
//!   parked peer to steal), which is a property of OS scheduling, not of the pool. It then awaits a
//!   second explicit channel: each victim blocks exactly once, so the channel fires only when the
//!   whole scenario is over. Because the pool is quiescent and no queue holds stealable work at
//!   that point, the steal counters read afterwards are frozen and the pool total and the
//!   per-scheduler sum are compared at the same (final) value.
//! * A gated runner (test 6c) pins a worker **inside** a slice, so the queueing observable is
//!   sampled while one pid is provably in flight and three are provably still queued.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{make_small, AtomTable, Eterm};
use parking_lot::Mutex;

/// A generous upper bound for a wait that a healthy pool satisfies in microseconds; a wedged pool
/// fails the test rather than hanging it.
const TIMEOUT: Duration = Duration::from_secs(5);

/// An empty process table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

// ---------------------------------------------------------------------------------------------
// A shared "block immediately" runner (used by the lifecycle/observable tests).
// ---------------------------------------------------------------------------------------------

/// A runner that blocks the process it is given (one slice, then leaves the run queue) and counts
/// its invocations in shared state.
struct WaitRunner {
    counter: Arc<AtomicUsize>,
}

impl ProcessRunner for WaitRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        // A blocked process leaves the run queue, so the pool empties once every pid has run.
        if let Some(process) = table.get_mut(pid) {
            process.set_state(ProcessState::Waiting);
        }
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(RunOutcome::Wait)
    }
}

/// A `make_runner` factory for [`WaitRunner`] sharing one invocation counter across schedulers.
fn wait_runner(counter: &Arc<AtomicUsize>) -> impl Fn(usize) -> WaitRunner + Send + Sync + 'static {
    let counter = Arc::clone(counter);
    move |_index: usize| WaitRunner {
        counter: Arc::clone(&counter),
    }
}

// ---------------------------------------------------------------------------------------------
// Test 1 — N processes make progress on M schedulers.
// ---------------------------------------------------------------------------------------------

/// Slices each process yields for before it blocks (test 1).
///
/// A `Yield` ends a **quantum**, not the slice: this runner consumes only a fraction of each
/// quantum, so all `YIELDS + 1` invocations happen inside **one** slice (the pool would requeue it
/// only after its 4000-reduction slice budget were spent). The per-pid invocation count therefore
/// still pins the requeue/continue path, while `stats().slices` pins one popped-pid iteration per
/// process.
const YIELDS: u32 = 3;
/// Processes spawned in test 1.
const PROGRESS_PIDS: usize = 200;

/// A runner that yields [`YIELDS`] times and then blocks, recording a per-pid run count so the
/// pool-wide total is deterministic. On its blocking (final) run each process joins an explicit
/// "every process has blocked" barrier (see the module docs).
struct ProgressRunner {
    counts: Arc<Mutex<HashMap<ProcessId, u32>>>,
    finished: Arc<AtomicUsize>,
    done: Arc<Mutex<Sender<()>>>,
    target: usize,
    yields: u32,
}

impl ProcessRunner for ProgressRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        let count = {
            let mut counts = self.counts.lock();
            let entry = counts.entry(pid).or_insert(0);
            *entry += 1;
            *entry
        };
        if count > self.yields {
            // The blocking run: leave the run queue (empty requeue buffer), then report.
            if let Some(process) = table.get_mut(pid) {
                process.set_state(ProcessState::Waiting);
            }
            let finished = self.finished.fetch_add(1, Ordering::SeqCst) + 1;
            if finished == self.target {
                // Exactly one thread observes the final blocker; the receiver may already be gone.
                let _ = self.done.lock().send(());
            }
            Ok(RunOutcome::Wait)
        } else {
            Ok(RunOutcome::Yield)
        }
    }
}

/// Drives `PROGRESS_PIDS` processes — each running `YIELDS + 1` slices — on `schedulers` threads
/// and returns `(total runs, recorded slices)`.
///
/// Synchronization: an `mpsc` channel is signalled when every process has reached its blocking
/// run (so the per-pid totals are final), then `wait_idle` confirms the pool quiesced.
fn run_progress(schedulers: usize) -> (u64, u64) {
    let counts: Arc<Mutex<HashMap<ProcessId, u32>>> = Arc::new(Mutex::new(HashMap::new()));
    let finished = Arc::new(AtomicUsize::new(0));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let done = Arc::new(Mutex::new(done_tx));

    let make = {
        let counts = Arc::clone(&counts);
        let finished = Arc::clone(&finished);
        let done = Arc::clone(&done);
        move |_index: usize| ProgressRunner {
            counts: Arc::clone(&counts),
            finished: Arc::clone(&finished),
            done: Arc::clone(&done),
            target: PROGRESS_PIDS,
            yields: YIELDS,
        }
    };

    let pool = SchedulerPool::start(SchedulerConfig::with_schedulers(schedulers), table(), make)
        .expect("a fresh pool starts");

    for _ in 0..PROGRESS_PIDS {
        let pid = pool.spawn_default().expect("room in the pid space");
        pool.enqueue(pid);
    }

    done_rx
        .recv_timeout(TIMEOUT)
        .expect("every process reaches its blocking run within the timeout");
    assert!(
        pool.wait_idle(Some(TIMEOUT)),
        "the pool is idle once every process has blocked"
    );

    let total: u64 = counts.lock().values().map(|runs| u64::from(*runs)).sum();
    let slices = pool.stats().slices;
    let error = pool.take_error();

    pool.shutdown();
    pool.join();

    assert!(
        error.is_none(),
        "the pool recorded no worker error: {error:?}"
    );
    (total, slices)
}

#[test]
fn two_hundred_processes_progress_on_one_scheduler() {
    let expected = PROGRESS_PIDS as u64 * (YIELDS as u64 + 1);
    let (total, slices) = run_progress(1);
    assert_eq!(
        total, expected,
        "each of the {PROGRESS_PIDS} processes ran yields+1 times"
    );
    assert_eq!(
        slices, PROGRESS_PIDS as u64,
        "each process's whole (short) work fits in one slice: the yields are quanta of it"
    );
}

#[test]
fn two_hundred_processes_progress_on_four_schedulers() {
    let expected = PROGRESS_PIDS as u64 * (YIELDS as u64 + 1);
    let (total, slices) = run_progress(4);
    assert_eq!(
        total, expected,
        "each of the {PROGRESS_PIDS} processes ran yields+1 times"
    );
    assert_eq!(
        slices, PROGRESS_PIDS as u64,
        "each process's whole (short) work fits in one slice: the yields are quanta of it"
    );
}

// ---------------------------------------------------------------------------------------------
// Test 2 — work stealing.
// ---------------------------------------------------------------------------------------------

/// Blocked receivers the driver wakes in the stealing test.
const STEAL_VICTIMS: usize = 128;
/// Upper bound on the converging wait for the peer scheduler to steal some of the backlog.
const STEAL_TIMEOUT: Duration = Duration::from_secs(10);

/// The shared identification/trace state the stealing runner reads and updates.
#[derive(Default)]
struct StealShared {
    /// The single driver pid (set before it is enqueued).
    driver: Option<ProcessId>,
    /// The blocked receivers the driver wakes.
    victims: Vec<ProcessId>,
    /// Per-pid run counts (so the test can assert every victim ran).
    runs: HashMap<ProcessId, u32>,
}

/// The stealing runner: the driver wakes every blocked victim (producing a large *local* queue on
/// the scheduler that ran the driver) and then blocks; every victim keeps yielding until the shared
/// `stop` flag is set and only then blocks, reporting its blocking run on `all_blocked`.
struct StealRunner {
    shared: Arc<Mutex<StealShared>>,
    stop: Arc<AtomicBool>,
    blocked: Arc<AtomicUsize>,
    all_blocked: Arc<Mutex<Sender<()>>>,
    total_victims: usize,
}

impl ProcessRunner for StealRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        let (is_driver, is_victim, victims) = {
            let mut shared = self.shared.lock();
            let is_driver = shared.driver == Some(pid);
            let is_victim = shared.victims.contains(&pid);
            let entry = shared.runs.entry(pid).or_insert(0);
            *entry += 1;
            let victims = if is_driver {
                shared.victims.clone()
            } else {
                Vec::new()
            };
            (is_driver, is_victim, victims)
        };

        if is_driver {
            // Wake every blocked receiver: the pool sweeps these wakes into the *running worker's
            // own local queue*, which is what gives a parked peer something to steal.
            let from = pid.to_eterm();
            for victim in victims {
                table
                    .deliver_simple(victim, from, make_small(1))
                    .expect("the victim is live and a small int always fits");
            }
        }

        if is_victim && !self.stop.load(Ordering::SeqCst) {
            // Keep yielding so the backlog never drains before the peer has had its chance.
            Ok(RunOutcome::Yield)
        } else {
            if let Some(process) = table.get_mut(pid) {
                process.set_state(ProcessState::Waiting);
            }
            if is_victim {
                // Each victim blocks exactly once — after blocking it is never requeued — so this
                // count reaches `total_victims` exactly: an exact "the whole scenario finished"
                // barrier that does not depend on the pool's own idle heuristic.
                let blocked = self.blocked.fetch_add(1, Ordering::SeqCst) + 1;
                if blocked == self.total_victims {
                    // Exactly one thread observes the final blocker; the receiver may be gone.
                    let _ = self.all_blocked.lock().send(());
                }
            }
            Ok(RunOutcome::Wait)
        }
    }
}

/// A peer scheduler steals half of the oversized local queue the driver created, and every woken
/// receiver runs.
#[test]
fn a_parked_scheduler_steals_half_of_a_busy_peers_queue() {
    let shared: Arc<Mutex<StealShared>> = Arc::new(Mutex::new(StealShared::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let blocked = Arc::new(AtomicUsize::new(0));
    let (all_blocked_tx, all_blocked_rx) = mpsc::channel::<()>();
    let all_blocked = Arc::new(Mutex::new(all_blocked_tx));
    let make = {
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        let blocked = Arc::clone(&blocked);
        let all_blocked = Arc::clone(&all_blocked);
        move |_index: usize| StealRunner {
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop),
            blocked: Arc::clone(&blocked),
            all_blocked: Arc::clone(&all_blocked),
            total_victims: STEAL_VICTIMS,
        }
    };
    let pool = SchedulerPool::start(SchedulerConfig::with_schedulers(2), table(), make)
        .expect("a fresh pool starts");
    assert_eq!(pool.schedulers(), 2);

    // Pre-create N blocked receivers (set `Waiting`, never enqueued) under one table lock.
    let mut victims = Vec::with_capacity(STEAL_VICTIMS);
    {
        let handle = pool.table();
        let mut table = handle.lock();
        for _ in 0..STEAL_VICTIMS {
            let pid = table.spawn_default().expect("room in the pid space");
            table
                .get_mut(pid)
                .expect("the freshly spawned process is live")
                .set_state(ProcessState::Waiting);
            victims.push(pid);
        }
    }

    // Publish the driver + victims, then enqueue only the driver.
    let driver = pool.spawn_default().expect("room in the pid space");
    {
        let mut state = shared.lock();
        state.driver = Some(driver);
        state.victims = victims;
    }
    let baseline_steals = pool.stats().steals;
    pool.enqueue(driver);

    // Converging wait: the victims keep yielding (so the backlog the driver created never drains)
    // until the parked peer has demonstrably stolen some of it. `yield_now` (never `sleep`) lets a
    // peer thread that is still starting up be scheduled; a healthy pool steals within
    // microseconds. The steal total observed on the iteration that ended the wait is retained, so
    // the assertion below compares against a value this wait actually proved.
    let deadline = Instant::now() + STEAL_TIMEOUT;
    let mut converged_steals = pool.stats().steals;
    while converged_steals <= baseline_steals && Instant::now() < deadline {
        std::thread::yield_now();
        converged_steals = pool.stats().steals;
    }
    // Release the victims, whether or not a steal was observed, so the pool can drain.
    stop.store(true, Ordering::SeqCst);

    // Exact completion barrier: each victim blocks exactly once, so this channel fires only when
    // the whole scenario is over — independent of the instant the pool looks idly empty.
    all_blocked_rx
        .recv_timeout(TIMEOUT)
        .expect("every woken receiver reaches its blocking run within the timeout");
    assert!(
        pool.wait_idle(Some(TIMEOUT)),
        "the pool drains to idle once the victims stop yielding"
    );

    // With no work left nowhere to steal, the counters below are frozen: read after `wait_idle`
    // they are exact, including the per-scheduler sum.
    let stats = pool.stats();
    let per_scheduler = pool.scheduler_stats();
    let victims_ran = {
        let state = shared.lock();
        state
            .victims
            .iter()
            .filter(|victim| state.runs.contains_key(victim))
            .count()
    };
    let error = pool.take_error();

    pool.shutdown();
    pool.join();

    // Diagnostic for the report: the observed steal total (visible with `--nocapture`).
    eprintln!(
        "smp: stealing test observed {} new steal(s) over baseline {baseline_steals} \
         (converged {converged_steals}, final {}) [{per_scheduler:?}]",
        stats.steals.saturating_sub(baseline_steals),
        stats.steals
    );

    assert_eq!(
        victims_ran, STEAL_VICTIMS,
        "every blocked receiver was woken and ran"
    );
    let per_steals: u64 = per_scheduler.iter().map(|stat| stat.steals).sum();
    assert!(
        stats.steals > 0,
        "a parked scheduler stole from the busy peer: {stats:?}"
    );
    assert!(
        converged_steals > baseline_steals,
        "the driver scenario produced the steal within {STEAL_TIMEOUT:?} \
         (baseline {baseline_steals}, converged {converged_steals}): {stats:?}"
    );
    assert!(
        stats.steals >= converged_steals,
        "the frozen total ({}) is at least the converged observation ({converged_steals})",
        stats.steals
    );
    assert_eq!(
        per_steals, stats.steals,
        "the per-scheduler steal counters sum to the pool total"
    );
    assert!(
        error.is_none(),
        "the pool recorded no worker error: {error:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Tests 3 & 4 — a message is delivered into a blocked receiver's heap and wakes it.
// ---------------------------------------------------------------------------------------------

/// A runner that pops the first mailbox message from the process it is given, forwards the payload
/// over a channel, then blocks the process again.
struct MailboxForwarder {
    tx: Arc<Mutex<Sender<Eterm>>>,
}

impl ProcessRunner for MailboxForwarder {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        let payload = table
            .get_mut(pid)
            .and_then(|process| process.receive(|_| true).map(|message| message.payload()));
        if let Some(payload) = payload {
            // The receiver may already be gone once a test has finished asserting.
            let _ = self.tx.lock().send(payload);
        }
        if let Some(process) = table.get_mut(pid) {
            process.set_state(ProcessState::Waiting);
        }
        Ok(RunOutcome::Wait)
    }
}

/// A timer fire wakes a blocked receiver into a run queue with the message in *its* heap.
#[test]
fn a_timer_fires_into_a_blocked_receiver() {
    let (tx, rx) = mpsc::channel::<Eterm>();
    let shared_tx = Arc::new(Mutex::new(tx));
    let make = {
        let shared_tx = Arc::clone(&shared_tx);
        move |_index: usize| MailboxForwarder {
            tx: Arc::clone(&shared_tx),
        }
    };
    // `with_schedulers` leaves timers on (the default), so the pool owns a live timer service.
    let pool = SchedulerPool::start(SchedulerConfig::with_schedulers(2), table(), make)
        .expect("a fresh pool starts");

    let receiver = pool.spawn_default().expect("room in the pid space");
    // Block the receiver *directly* (via the table) and never enqueue it, so only the timer can
    // make it runnable.
    pool.table()
        .lock()
        .get_mut(receiver)
        .expect("the receiver is live")
        .set_state(ProcessState::Waiting);

    let msg = make_small(4242);
    pool.send_after(Duration::from_millis(25), receiver, msg);

    let observed = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the timer sink delivers into the receiver's heap and wakes it");
    assert_eq!(observed, msg, "the exact timer message reaches the receiver");

    let error = pool.take_error();
    pool.shutdown();
    pool.join();
    assert!(
        error.is_none(),
        "the pool recorded no worker error: {error:?}"
    );
}

/// A `deliver_simple` send from outside a slice wakes a blocked receiver.
#[test]
fn deliver_simple_wakes_a_blocked_receiver() {
    let (tx, rx) = mpsc::channel::<Eterm>();
    let shared_tx = Arc::new(Mutex::new(tx));
    let make = {
        let shared_tx = Arc::clone(&shared_tx);
        move |_index: usize| MailboxForwarder {
            tx: Arc::clone(&shared_tx),
        }
    };
    let pool = SchedulerPool::start(SchedulerConfig::with_schedulers(2), table(), make)
        .expect("a fresh pool starts");

    let receiver = pool.spawn_default().expect("room in the pid space");
    let sender = pool.spawn_default().expect("room in the pid space");
    pool.table()
        .lock()
        .get_mut(receiver)
        .expect("the receiver is live")
        .set_state(ProcessState::Waiting);

    let msg = make_small(77);
    let outcome = pool
        .deliver_simple(receiver, sender.to_eterm(), msg)
        .expect("the receiver is live and a small int always fits");
    assert_eq!(
        outcome,
        DeliverOutcome::Woke,
        "delivering to a blocked receiver reports a wake"
    );

    let observed = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the send wakes the receiver onto a run queue");
    assert_eq!(observed, msg, "the exact sent term reaches the receiver");

    let error = pool.take_error();
    pool.shutdown();
    pool.join();
    assert!(
        error.is_none(),
        "the pool recorded no worker error: {error:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Test 5 — a dirty completion reschedules a blocked process.
// ---------------------------------------------------------------------------------------------

/// A runner that blocks the process on every run and signals a channel on its second invocation.
struct DirtyRunner {
    runs: Arc<AtomicUsize>,
    done: Arc<Mutex<Sender<()>>>,
}

impl ProcessRunner for DirtyRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        let runs = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(process) = table.get_mut(pid) {
            process.set_state(ProcessState::Waiting);
        }
        if runs >= 2 {
            let _ = self.done.lock().send(());
        }
        Ok(RunOutcome::Wait)
    }
}

/// A `DirtyOutcome::Reschedule(pid)` from a dirty worker re-enqueues the blocked process.
#[test]
fn a_dirty_completion_reschedules_a_blocked_process() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let done = Arc::new(Mutex::new(done_tx));
    let make = {
        let runs = Arc::clone(&runs);
        let done = Arc::clone(&done);
        move |_index: usize| DirtyRunner {
            runs: Arc::clone(&runs),
            done: Arc::clone(&done),
        }
    };

    let mut config = SchedulerConfig::with_schedulers(2);
    config.set_dirty_cpu(1).set_dirty_io(1);
    let pool = SchedulerPool::start(config, table(), make).expect("a fresh pool starts");

    let pid = pool.spawn_default().expect("room in the pid space");
    pool.enqueue(pid);
    assert!(
        pool.wait_idle(Some(TIMEOUT)),
        "the pid runs its first slice and blocks"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the process ran exactly once before the dirty completion"
    );

    // The dirty worker's completion routes back onto a normal scheduler and re-enqueues the pid.
    pool.submit_dirty(
        DirtyKind::Cpu,
        Box::new(move || DirtyOutcome::Reschedule(pid)),
    );

    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the rescheduled process runs a second time");
    assert!(
        runs.load(Ordering::SeqCst) >= 2,
        "the dirty completion re-enqueued the blocked process"
    );
    assert!(
        pool.wait_idle(Some(Duration::from_secs(2))),
        "the pool drains back to idle after the reschedule"
    );

    let error = pool.take_error();
    pool.shutdown();
    pool.join();
    assert!(
        error.is_none(),
        "the pool recorded no worker error: {error:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Test 6 — lifecycle and observables.
// ---------------------------------------------------------------------------------------------

/// Every per-scheduler observable has one entry per scheduler, indexed `0..N`.
#[test]
fn stats_vectors_have_one_entry_per_scheduler() {
    let counter = Arc::new(AtomicUsize::new(0));
    let pool = SchedulerPool::start(
        SchedulerConfig::with_schedulers(3),
        table(),
        wait_runner(&counter),
    )
    .expect("a fresh pool starts");

    assert_eq!(pool.schedulers(), 3);
    assert_eq!(pool.scheduler_stats().len(), 3);
    assert_eq!(pool.run_queue_lengths().len(), 3);
    let indices: Vec<usize> = pool
        .scheduler_stats()
        .iter()
        .map(|stat| stat.index)
        .collect();
    assert_eq!(indices, vec![0, 1, 2], "schedulers are indexed 0..N");

    pool.shutdown();
    pool.join();
}

/// Once idle the run queue is empty and the slice counter is the exact total.
#[test]
fn the_pool_drains_to_zero_and_counts_every_slice() {
    let counter = Arc::new(AtomicUsize::new(0));
    let pool = SchedulerPool::start(
        SchedulerConfig::with_schedulers(3),
        table(),
        wait_runner(&counter),
    )
    .expect("a fresh pool starts");

    let count: usize = 32;
    for _ in 0..count {
        let pid = pool.spawn_default().expect("room in the pid space");
        pool.enqueue(pid);
    }

    assert!(pool.wait_idle(Some(TIMEOUT)), "the pool drains to idle");
    assert_eq!(pool.run_queue_len(), 0, "no work is queued when idle");
    assert_eq!(pool.stats().slices, count as u64, "one slice per process");
    assert_eq!(counter.load(Ordering::SeqCst), count);

    let error = pool.take_error();
    pool.shutdown();
    pool.join();
    assert!(
        error.is_none(),
        "the pool recorded no worker error: {error:?}"
    );
}

/// A runner that, on its **first** invocation only, signals `entered` and then blocks on a gate
/// channel (with a bounded wait) before doing anything — pinning the single worker inside a slice.
struct GateRunner {
    entered: Sender<()>,
    gate: Arc<Mutex<Receiver<()>>>,
    gated: bool,
}

impl ProcessRunner for GateRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        if self.gated {
            self.gated = false;
            let _ = self.entered.send(());
            // Hold the slice open until the test releases the gate (bounded, so a bug fails rather
            // than hangs). While blocked the worker holds the table lock, so the test touches only
            // queue state (never the table) until after the release.
            let _ = self.gate.lock().recv_timeout(Duration::from_secs(10));
        }
        if let Some(process) = table.get_mut(pid) {
            process.set_state(ProcessState::Waiting);
        }
        Ok(RunOutcome::Wait)
    }
}

/// `run_queue_len` deterministically reflects queued work while the only worker is stuck in a
/// slice, and returns to zero once released.
#[test]
fn run_queue_len_reflects_work_while_the_only_worker_is_stuck() {
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(gate_rx));
    let make = {
        let gate = Arc::clone(&gate);
        move |_index: usize| GateRunner {
            entered: entered_tx.clone(),
            gate: Arc::clone(&gate),
            gated: true,
        }
    };
    let pool = SchedulerPool::start(SchedulerConfig::with_schedulers(1), table(), make)
        .expect("a fresh pool starts");
    assert_eq!(pool.schedulers(), 1);

    // Spawn the stuck process and the three that will queue *behind* it before running anything
    // (spawning needs the table lock, which the stuck worker will hold).
    let stuck = pool.spawn_default().expect("room in the pid space");
    let queued: Vec<ProcessId> = (0..3)
        .map(|_| pool.spawn_default().expect("room in the pid space"))
        .collect();

    pool.enqueue(stuck);
    entered_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the single worker enters the runner and blocks inside the slice");

    // The worker is pinned inside the runner, so these three stay queued (the injector is not
    // drained while the only worker is inside a slice).
    for &pid in &queued {
        pool.enqueue(pid);
    }
    assert_eq!(
        pool.run_queue_len(),
        3,
        "the three pids are visibly queued behind the stuck worker"
    );

    gate_tx.send(()).expect("the gate receiver is alive");
    assert!(
        pool.wait_idle(Some(TIMEOUT)),
        "the pool drains once the worker is released"
    );
    assert_eq!(pool.run_queue_len(), 0, "the queue is empty when idle");

    let error = pool.take_error();
    pool.shutdown();
    pool.join();
    assert!(
        error.is_none(),
        "the pool recorded no worker error: {error:?}"
    );
}

/// `shutdown` then `join` returns without hanging, even with work still queued.
#[test]
fn shutdown_then_join_returns_without_hanging() {
    let counter = Arc::new(AtomicUsize::new(0));
    let pool = SchedulerPool::start(
        SchedulerConfig::with_schedulers(2),
        table(),
        wait_runner(&counter),
    )
    .expect("a fresh pool starts");

    for _ in 0..8 {
        let pid = pool.spawn_default().expect("room in the pid space");
        pool.enqueue(pid);
    }

    pool.shutdown();
    let error = pool.take_error();
    // `join` consumes the pool; if it hangs, the test harness times it out.
    pool.join();
    assert!(
        error.is_none(),
        "the pool recorded no worker error: {error:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Test 7 — the single-scheduler configuration (the pre-existing single-threaded contract).
// ---------------------------------------------------------------------------------------------

/// A one-scheduler pool runs a spawn/enqueue/run-to-completion cycle exactly like the
/// multi-scheduler case.
#[test]
fn a_single_scheduler_pool_runs_to_completion() {
    let counter = Arc::new(AtomicUsize::new(0));
    let pool = SchedulerPool::start(
        SchedulerConfig::with_schedulers(1),
        table(),
        wait_runner(&counter),
    )
    .expect("a one-scheduler pool starts");
    assert_eq!(pool.schedulers(), 1, "the configuration is honored");

    let count: usize = 50;
    for _ in 0..count {
        let pid = pool.spawn_default().expect("room in the pid space");
        pool.enqueue(pid);
    }

    assert!(pool.wait_idle(Some(TIMEOUT)), "the pool runs to idle");
    assert_eq!(counter.load(Ordering::SeqCst), count, "every pid ran");
    assert_eq!(pool.stats().slices, count as u64);

    let error = pool.take_error();
    pool.shutdown();
    pool.join();
    assert!(
        error.is_none(),
        "the pool recorded no worker error: {error:?}"
    );
}
