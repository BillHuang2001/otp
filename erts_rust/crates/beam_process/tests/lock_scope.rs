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

//! Evidence for the **bounded-quantum process-table lock** of the SMP pool.
//!
//! A scheduler used to hold the shared table lock for a *whole* slice, so an independent process —
//! or a cross-thread `deliver_simple` / timer / dirty completion — could not make progress until
//! that slice's full reduction budget was spent. The pool now services a slice as
//! `ceil(reductions_per_slice / lock_quantum)` **quanta**, releasing the table lock between them
//! (see `beam_process::scheduler::DEFAULT_LOCK_QUANTUM` and `SchedulerPool::lock_quantum`).
//!
//! Three tests pin that:
//!
//! 1. [`a_slice_is_serviced_as_bounded_quanta`] — *structural*: one slice consumes
//!    `ceil(budget / quantum)` runner invocations (i.e. the lock was released inside the slice).
//!    The old coarse behavior is exactly reproducible by configuring `lock_quantum == budget`.
//! 2. [`an_independent_process_and_a_cross_thread_send_progress_inside_a_slice`] — *interleaving*:
//!    while a long slice is in progress, a cross-thread send is serviced promptly and the woken
//!    process runs **inside** that slice.
//! 3. [`per_sender_message_and_exit_order_is_identical_on_one_and_four_schedulers`] —
//!    *determinism*: the same program (4 senders → 1 trapping receiver, then each sender exits)
//!    produces the same per-sender message order and the same per-sender `{'EXIT', …}` placement
//!    on 1 and on 4 schedulers.
//!
//! # Anti-flakiness
//!
//! Every wait is bounded and every assertion is forced by synchronization or by an exact pool
//! counter, never by a bare sleep:
//!
//! * The structural test reads the pool's own `stats().slices` after `wait_idle`, so the counts are
//!   exact (no racing observer).
//! * The interleaving test learns "the long slice is in progress" from a channel the runner signals
//!   on its first invocation, and "which quanta had run when the peer first ran" from an atomic the
//!   *runners* read — not from wall-clock comparisons between threads.
//! * The only wall-clock assertions are deliberately loose bounds whose violation is exactly the
//!   coarse-lock behavior: the measured table-lock wait must stay far below the slice's duration,
//!   and the slice's duration must stay far above that bound (the anti-vacuity guard).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{is_immediate, make_small, signed_value, tuple_element, AtomTable};
use parking_lot::Mutex;

/// A generous upper bound for a wait a healthy pool satisfies in microseconds; a wedged pool fails
/// the test rather than hanging it.
const TIMEOUT: Duration = Duration::from_secs(10);

/// An empty process table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

// ---------------------------------------------------------------------------------------------
// Test 1 — one slice is serviced as `ceil(budget / quantum)` bounded lock hold periods.
// ---------------------------------------------------------------------------------------------

/// The per-slice reduction budget used by the structural probe.
const SLICE_BUDGET: u32 = 4096;
/// The narrowed per-lock-acquisition quantum used by the structural probe.
const QUANTUM: u32 = 128;

/// A runner that yields `stop_after - 1` times and then blocks, counting its invocations.
///
/// Yielding is not what a real engine does for a *partial* quantum, but it is the worst case for
/// the pool: the runner gives up the CPU on every invocation, so the pool decides how much of the
/// slice's budget to keep running it for — and that is exactly what this probe measures.
struct YieldProbe {
    /// Invocations so far (shared across scheduler threads).
    calls: Arc<AtomicUsize>,
    /// The invocation on which the process blocks (its slice ends there).
    stop_after: usize,
}

impl ProcessRunner for YieldProbe {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if calls >= self.stop_after {
            // The blocking run: leave the run queue, so the pool becomes idle.
            if let Some(process) = table.get_mut(pid) {
                process.set_state(ProcessState::Waiting);
            }
            return Ok(RunOutcome::Wait);
        }
        Ok(RunOutcome::Yield)
    }
}

/// Runs one process that yields `stop_after - 1` times and then blocks, returning
/// `(runner invocations, completed slices)` once the pool is idle.
fn run_yield_probe(config: SchedulerConfig, stop_after: usize) -> (usize, u64) {
    let calls = Arc::new(AtomicUsize::new(0));
    let make = {
        let calls = Arc::clone(&calls);
        move |_index: usize| YieldProbe {
            calls: Arc::clone(&calls),
            stop_after,
        }
    };
    let pool = SchedulerPool::start(config, table(), make).expect("a fresh pool starts");
    let pid = pool.spawn_default().expect("room in the pid space");
    pool.enqueue(pid);

    assert!(
        pool.wait_idle(Some(TIMEOUT)),
        "the single yielding process blocks within the timeout"
    );
    let invocations = calls.load(Ordering::SeqCst);
    let slices = pool.stats().slices;
    assert!(pool.take_error().is_none(), "no worker error");
    pool.shutdown();
    pool.join();
    (invocations, slices)
}

#[test]
fn a_slice_is_serviced_as_bounded_quanta() {
    let quanta_per_slice = SLICE_BUDGET.div_ceil(QUANTUM) as usize;
    assert!(quanta_per_slice > 1, "the quantum really narrows the lock");

    // Narrowed: one slice's budget is spent as `quanta_per_slice` invocations, each under its own
    // lock acquisition, so the whole (one-slice) work completes as **one** completed slice.
    let mut config = SchedulerConfig::with_schedulers(1);
    config
        .set_reductions_per_slice(SLICE_BUDGET)
        .set_lock_quantum(QUANTUM);
    let (invocations, slices) = run_yield_probe(config, quanta_per_slice);
    assert_eq!(
        invocations, quanta_per_slice,
        "the slice's budget is handed out as {quanta_per_slice} quanta"
    );
    assert_eq!(
        slices, 1,
        "the {quanta_per_slice} invocations belong to ONE slice (the lock was released inside it)"
    );

    // Coarse: `lock_quantum == budget` is exactly the old one-lock-hold-per-slice behavior — every
    // runner invocation is its own slice.
    let mut config = SchedulerConfig::with_schedulers(1);
    config
        .set_reductions_per_slice(SLICE_BUDGET)
        .set_lock_quantum(SLICE_BUDGET);
    let (invocations, slices) = run_yield_probe(config, 4);
    assert_eq!(
        (invocations, slices),
        (4, 4),
        "with a whole-slice lock quantum every invocation is requeued as its own slice"
    );
}

// ---------------------------------------------------------------------------------------------
// Test 2 — an independent process and a cross-thread send progress INSIDE a slice.
// ---------------------------------------------------------------------------------------------

/// The narrowed quantum used by the interleaving probe.
const INTERLEAVE_QUANTUM: u32 = 128;
/// Modelled CPU cost of one reduction (so one quantum is ~10 ms and one slice ~300 ms — far longer
/// than any thread-scheduling delay the assertions below have to tolerate).
const NANOS_PER_REDUCTION: u64 = 75_000;
/// The cross-thread send must be serviced well inside the slice (the coarse lock would block it for
/// the whole ~300 ms slice).
const DELIVERY_BOUND: Duration = Duration::from_millis(100);
/// Anti-vacuity floor: the slice must really be much longer than [`DELIVERY_BOUND`], otherwise the
/// delivery assertion above would pass trivially even with a whole-slice lock.
const SLICE_FLOOR: Duration = Duration::from_millis(150);

/// Shared state of the interleaving probe: which pid is the long-running process and how many of its
/// quanta have run.
#[derive(Default)]
struct InterleaveShared {
    /// The long-running process (its slices are long).
    target: Mutex<Option<ProcessId>>,
    /// How many quanta of the long-running process have been entered.
    quanta: AtomicUsize,
    /// Signals the first quantum of the long-running process.
    first: Mutex<Option<Sender<()>>>,
    /// Signals the first run of the peer, carrying how many quanta had run at that moment.
    peer: Mutex<Option<Sender<usize>>>,
}

/// A runner with two roles: the [`InterleaveShared::target`] process burns
/// `budget * NANOS_PER_REDUCTION` per invocation (a long slice); every other process records when it
/// first ran, relative to the target's quanta, and then blocks.
struct InterleaveRunner {
    /// Shared probe state.
    shared: Arc<InterleaveShared>,
    /// How many quanta the target yields for before it blocks (i.e. its whole slice).
    quanta: usize,
    /// Modelled cost of one reduction.
    nanos_per_reduction: u64,
}

impl ProcessRunner for InterleaveRunner {
    fn run(&mut self, pid: ProcessId, table: &mut ProcessTable, budget: u32) -> Result<RunOutcome> {
        let is_target = *self.shared.target.lock() == Some(pid);
        if !is_target {
            // The peer: report how much of the target's slice had already elapsed.
            if let Some(sender) = self.shared.peer.lock().take() {
                let _ = sender.send(self.shared.quanta.load(Ordering::SeqCst));
            }
            if let Some(process) = table.get_mut(pid) {
                process.set_state(ProcessState::Waiting);
            }
            return Ok(RunOutcome::Wait);
        }

        let quanta = self.shared.quanta.fetch_add(1, Ordering::SeqCst) + 1;
        if quanta == 1 {
            if let Some(sender) = self.shared.first.lock().take() {
                let _ = sender.send(());
            }
        }
        // One quantum of modelled CPU work.
        thread::sleep(Duration::from_nanos(u64::from(budget) * self.nanos_per_reduction));

        if quanta >= self.quanta {
            if let Some(process) = table.get_mut(pid) {
                process.set_state(ProcessState::Waiting);
            }
            return Ok(RunOutcome::Wait);
        }
        Ok(RunOutcome::Yield)
    }
}

/// Receives `value` or fails the test with a clear message (a wedged pool never hangs the suite).
fn expect<T>(rx: &Receiver<T>, what: &str) -> T {
    rx.recv_timeout(TIMEOUT)
        .unwrap_or_else(|error| panic!("{what} within {TIMEOUT:?}: {error}"))
}

#[test]
fn an_independent_process_and_a_cross_thread_send_progress_inside_a_slice() {
    const SLICE: u32 = 4096;
    let quanta_per_slice = SLICE.div_ceil(INTERLEAVE_QUANTUM) as usize;

    let (first_tx, first_rx) = mpsc::channel::<()>();
    let (peer_tx, peer_rx) = mpsc::channel::<usize>();
    let shared = Arc::new(InterleaveShared::default());
    *shared.first.lock() = Some(first_tx);
    *shared.peer.lock() = Some(peer_tx);

    let make = {
        let shared = Arc::clone(&shared);
        move |_index: usize| InterleaveRunner {
            shared: Arc::clone(&shared),
            // Exactly one full slice: the last invocation blocks, so all of the target's work is
            // one popped-pid slice.
            quanta: quanta_per_slice,
            nanos_per_reduction: NANOS_PER_REDUCTION,
        }
    };

    let mut config = SchedulerConfig::with_schedulers(4);
    config
        .set_reductions_per_slice(SLICE)
        .set_lock_quantum(INTERLEAVE_QUANTUM);
    let pool = SchedulerPool::start(config, table(), make).expect("a fresh pool starts");
    assert_eq!(pool.lock_quantum(), INTERLEAVE_QUANTUM);

    let target = pool.spawn_default().expect("room in the pid space");
    let peer = pool.spawn_default().expect("room in the pid space");
    *shared.target.lock() = Some(target);

    // The peer waits in its mailbox: only the cross-thread send can wake it.
    pool.table()
        .lock()
        .get_mut(peer)
        .expect("the peer is live")
        .set_state(ProcessState::Waiting);

    pool.enqueue(target);
    expect(&first_rx, "the long-running process reaches its first quantum");
    assert_eq!(
        pool.stats().slices,
        0,
        "the target is inside its first slice while we send"
    );

    // The cross-thread send: it needs the process-table lock, which the target holds only for one
    // quantum at a time.
    let started = Instant::now();
    let outcome = pool
        .deliver_simple(peer, target.to_eterm(), make_small(7))
        .expect("the peer is live and a small int always fits");
    let delivery_wait = started.elapsed();
    assert_eq!(
        outcome,
        DeliverOutcome::Woke,
        "the send wakes the blocked peer"
    );

    let peer_at_quanta = expect(&peer_rx, "the woken peer runs");
    assert!(
        pool.wait_idle(Some(TIMEOUT)),
        "both processes block, so the pool drains"
    );
    let slice_span = started.elapsed();
    let stats = pool.stats();
    let error = pool.take_error();
    pool.shutdown();
    pool.join();

    assert!(error.is_none(), "no worker error: {error:?}");
    assert_eq!(
        shared.quanta.load(Ordering::SeqCst),
        quanta_per_slice,
        "the target ran exactly one slice's worth of quanta"
    );
    assert_eq!(stats.slices, 2, "one slice for the target, one for the peer");

    // The interleaving guarantee: the independent process ran *inside* the target's slice —
    // strictly after its first quantum and strictly before its last one. With a whole-slice lock
    // the peer could not have run until the slice was over (`peer_at_quanta == quanta_per_slice`).
    assert!(
        peer_at_quanta >= 1,
        "the peer ran after the target's first quantum (it was woken by our send)"
    );
    assert!(
        peer_at_quanta < quanta_per_slice,
        "the peer ran inside the target's slice (quanta at that point: {peer_at_quanta} of \
         {quanta_per_slice})"
    );

    // The measured effect: the cross-thread send was serviced within one quantum, not after a whole
    // slice (anti-vacuity: the slice really did last much longer than the bound).
    assert!(
        delivery_wait < DELIVERY_BOUND,
        "the table lock is released between quanta: the send waited {delivery_wait:?}"
    );
    assert!(
        slice_span >= SLICE_FLOOR,
        "the slice under test really is long ({slice_span:?}), so the bound above is meaningful"
    );
}

// ---------------------------------------------------------------------------------------------
// Test 3 — the same program produces the same per-sender message and exit order on 1 and N
// schedulers.
// ---------------------------------------------------------------------------------------------

/// Messages each sender sends before exiting.
const MESSAGES: u32 = 16;
/// Sender processes in the ordering program.
const SENDERS: usize = 4;

/// One observable event of the ordering program, derived inside the receiver's heap guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    /// A data message, carrying its sequence number (the sender is the map key).
    Data(u32),
    /// The sender's `{'EXIT', From, Reason}` signal, carrying the reason.
    Exit(i64),
}

/// Shared state of the ordering program.
#[derive(Default)]
struct OrderShared {
    /// The sender pids, in index order.
    senders: Mutex<Vec<ProcessId>>,
    /// The receiver.
    receiver: Mutex<Option<ProcessId>>,
    /// Per-sender message sequence numbers.
    seqs: Mutex<HashMap<ProcessId, u32>>,
}

impl OrderShared {
    /// The index of `pid` among the senders, if it is one.
    fn index_of(&self, pid: ProcessId) -> Option<usize> {
        self.senders.lock().iter().position(|&sender| sender == pid)
    }
}

/// Each sender sends one tagged message per invocation and then exits with its index as the reason;
/// the receiver is linked + `trap_exit` and never runs.
struct OrderRunner {
    /// Shared program state.
    shared: Arc<OrderShared>,
}

impl ProcessRunner for OrderRunner {
    fn run(&mut self, pid: ProcessId, table: &mut ProcessTable, _budget: u32) -> Result<RunOutcome> {
        let Some(index) = self.shared.index_of(pid) else {
            // Not a sender: nothing to do (awaiting a place in the program).
            return Ok(RunOutcome::Wait);
        };
        let Some(receiver) = *self.shared.receiver.lock() else {
            return Ok(RunOutcome::Wait);
        };

        let seq = {
            let mut seqs = self.shared.seqs.lock();
            let entry = seqs.entry(pid).or_insert(0);
            *entry += 1;
            *entry
        };
        if seq > MESSAGES {
            // Every message is sent: terminate with the sender's index as the exit reason.
            return Ok(RunOutcome::Exit(make_small(index as i64)));
        }

        let payload = make_small(index as i64 * 1000 + i64::from(seq));
        table.deliver_simple(receiver, pid.to_eterm(), payload)?;
        Ok(RunOutcome::Yield)
    }
}

/// Runs the ordering program on `schedulers` threads and returns, per sender index, the events the
/// receiver observed **in order**.
fn run_order_program(schedulers: usize) -> BTreeMap<usize, Vec<Event>> {
    let shared = Arc::new(OrderShared::default());
    let make = {
        let shared = Arc::clone(&shared);
        move |_index: usize| OrderRunner {
            shared: Arc::clone(&shared),
        }
    };

    let pool = SchedulerPool::start(SchedulerConfig::with_schedulers(schedulers), table(), make)
        .expect("a fresh pool starts");
    let receiver = pool.spawn_default().expect("room in the pid space");
    let senders: Vec<ProcessId> = (0..SENDERS)
        .map(|_| pool.spawn_default().expect("room in the pid space"))
        .collect();
    *shared.receiver.lock() = Some(receiver);
    *shared.senders.lock() = senders.clone();

    // The receiver traps exits and is linked to every sender: each sender's termination must land
    // in its mailbox as `{'EXIT', From, Reason}`. Set up before the senders run, so the signal
    // policy is already in place when the first one exits.
    let shared_table = pool.table();
    {
        let mut table = shared_table.lock();
        table
            .get_mut(receiver)
            .expect("the receiver is live")
            .flags_mut()
            .set_trap_exit(true);
        for &sender in &senders {
            table.link(sender, receiver).expect("both are live");
        }
    }

    for &sender in &senders {
        pool.enqueue(sender);
    }
    assert!(
        pool.wait_idle(Some(TIMEOUT)),
        "every sender runs to its exit within the timeout"
    );
    let error = pool.take_error();

    // Drain the receiver's mailbox (it never ran, so no collection can move its payloads) and
    // derive plain values while the table guard is held.
    let mut observed: BTreeMap<usize, Vec<Event>> = (0..SENDERS).map(|i| (i, Vec::new())).collect();
    {
        let mut table = shared_table.lock();
        let process = table.get_mut(receiver).expect("the receiver is live");
        while let Some(message) = process.receive(|_| true) {
            let payload = message.payload();
            if is_immediate(payload) {
                let word = signed_value(payload);
                let index = (word / 1000) as usize;
                observed
                    .entry(index)
                    .or_default()
                    .push(Event::Data((word % 1000) as u32));
            } else {
                // SAFETY: the payload is the live `{'EXIT', From, Reason}` tuple the table built on
                // the receiver's heap (the same read `tests/signals.rs` performs).
                let (from, reason) = unsafe {
                    (
                        tuple_element(payload, 1),
                        signed_value(tuple_element(payload, 2)),
                    )
                };
                let index = senders
                    .iter()
                    .position(|&sender| sender.to_eterm() == from)
                    .expect("the exit message came from one of the senders");
                observed.entry(index).or_default().push(Event::Exit(reason));
            }
        }
    }

    pool.shutdown();
    pool.join();
    assert!(error.is_none(), "no worker error: {error:?}");
    observed
}

#[test]
fn per_sender_message_and_exit_order_is_identical_on_one_and_four_schedulers() {
    let one = run_order_program(1);
    let four = run_order_program(4);

    for index in 0..SENDERS {
        // Per-sender order: every message in sequence, then exactly one exit.
        let want: Vec<Event> = (1..=MESSAGES)
            .map(Event::Data)
            .chain(std::iter::once(Event::Exit(index as i64)))
            .collect();
        let events = one
            .get(&index)
            .expect("the one-scheduler run observed this sender");
        assert_eq!(
            events, &want,
            "sender {index} under one scheduler delivered its messages in order, then exited"
        );
        assert_eq!(
            four.get(&index)
                .expect("the four-scheduler run observed this sender"),
            events,
            "sender {index} kept the same message/exit order on four schedulers"
        );
    }
}
