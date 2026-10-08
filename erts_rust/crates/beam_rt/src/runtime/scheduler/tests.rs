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

//! Unit tests for the [`super`] scheduler family: the bounded and unbounded `run_*` entry
//! points, the round-robin progress guarantee, the `erlang:send_after/3` and finite
//! `receive … after T` timer halves, and the CLI's blocking `run` action.
//!
//! Every fixture is a committed `.beam`: `slice.beam` is shared with the façade suite (reused
//! by relative path, never copied) and `recv.beam`/`recv_after.beam` are this crate's own. Every
//! oracle value quoted below was verified against the system `erl`.

use std::time::{Duration, Instant};

use beam_engine::ExecutionContext;
use beam_process::{Priority, ProcessId, ProcessState};
use beam_term::{make_atom, make_small, signed_value, Eterm};

use crate::context::RuntimeContext;
use crate::info::InfoValue;
use crate::runtime::{PendingSpawn, Runtime};

use super::super::tests::SLICE_BEAM;
use super::*;

/// The default runtime the scheduler tests run against, **booted up front**.
///
/// [`Runtime`]'s boot is lazy: the first scheduler pass calls `boot_if_needed`, which adds
/// the system code path and loads the 22 preloaded modules. That first pass therefore pays a
/// one-time cost: with `$ERTS_STDLIB_DIR` unset it spawns the system `erl` once per OS process
/// (memoized in a `OnceLock`, `codeserver.rs::resolve_system_ebins`). Several tests below
/// bound a timed window with that **first** pass inside it (`< 200 ms`,
/// [`a_bounded_step_does_not_wait_for_a_pending_deadline`], and `< 1000 ms`,
/// [`a_message_already_in_the_mailbox_is_taken_before_the_timeout`]), so the spawn could land
/// inside the window and flake the suite. Warming the boot here moves the cost before every
/// window; the spawn is memoized for the whole test binary, so only the first call pays it.
fn runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the runtime boots");
    rt
}

// -- scheduler count & multi-process progress -----------------------------

/// `+S N` is the effective scheduler count the runtime **reports** through
/// `system_info(schedulers)` — the whole chain from the A.6 command line to the A.8
/// observable, pinned at the runtime boundary. (`-smp enable|disable|auto` is parsed and
/// stored by the A.6 parser; the reported count is the `+S`/online-processor figure.)
#[test]
fn a_configured_scheduler_count_is_observable_through_system_info() {
    let config = crate::cli::parse(&["+S".to_string(), "3".to_string()]).expect("+S 3 parses");
    let rt = Runtime::new(config).expect("a runtime with +S 3");
    assert_eq!(
        rt.system_info_value("schedulers").expect("schedulers"),
        InfoValue::Int(3)
    );
}

/// Iterative Fibonacci; the oracle value of `slice:fib/1`.
fn fib(n: i64) -> i64 {
    let (mut a, mut b) = (0i64, 1i64);
    for _ in 0..n {
        (a, b) = (b, a + b);
    }
    a
}

/// **N** spawned processes each make progress to completion under a **bounded
/// round-robin**: every `run_one_slice` step runs exactly one process once, so the number
/// of completed processes rises by one per step until the run queue drains.
///
/// This is the behavioural guarantee an SMP `SchedulerPool` adoption must preserve (the
/// Milestone-1 runtime drives a single-threaded scheduler, so several runnable processes
/// are interleaved slice-by-slice and none is starved). The multi-pass counterpart — a
/// receiver blocked in `receive` woken by a **timer** rather than the run queue — is
/// pinned by [`a_timer_wakes_a_receiver_that_blocked_before_it_fired`].
#[test]
fn every_spawned_process_makes_progress_to_completion_round_robin() {
    const N: i64 = 6;
    let mut rt = runtime();
    rt.load_module(SLICE_BEAM).expect("load slice.beam");

    let pids: Vec<ProcessId> = (1..=N)
        .map(|n| {
            rt.spawn_process("slice", "fib", vec![make_small(n)])
                .expect("slice:fib/1 is exported")
        })
        .collect();
    assert_eq!(rt.run_queue_len(), N as usize, "every spawn enqueued one process");

    let completed =
        |rt: &Runtime| pids.iter().filter(|pid| rt.process_result(**pid).is_some()).count();
    assert_eq!(completed(&rt), 0, "nothing has run yet");

    for step in 1..=N as usize {
        let ran = rt.run_one_slice().expect("a bounded step");
        assert_eq!(ran, 1, "step {step} runs exactly one process");
        assert_eq!(completed(&rt), step, "one more process completed per step");
    }

    for (pid, n) in pids.iter().zip(1..=N) {
        assert_eq!(
            signed_value(rt.process_result(*pid).expect("the process returned")),
            fib(n),
            "every process produced its own result"
        );
        assert!(rt.processes().get(*pid).is_none(), "it terminated normally");
    }
    assert_eq!(rt.run_queue_len(), 0, "the run queue drained");
    assert_eq!(rt.run_one_slice().expect("nothing left to run"), 0);
}

// -- erlang:send_after/3 on the scheduler path ----------------------------

/// The committed receive fixture (`../beam_engine/tests/fixtures/recv.beam`), reused by
/// relative path: `recv:wait/0` is the bare `receive X -> X end`.
const RECV_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_engine/tests/fixtures/recv.beam"
);

/// Schedules `message` for `target` in `millis` through the runtime's own dispatch (the
/// `erlang:send_after/3` a running process would call).
fn send_after(rt: &mut Runtime, millis: i64, target: Eterm, message: Eterm) -> Eterm {
    let mut ctx = RuntimeContext::new(rt);
    ctx.call_bif("erlang", "send_after", 3, &[make_small(millis), target, message])
        .expect("send_after/3")
}

/// A receiver blocks **before** the timer fires, and the timer's message is what wakes
/// it: the run queue is empty and the mailbox is empty when the deadline is scheduled,
/// so only the timer half of the scheduler pass can resume it.
///
/// Oracle: `R = spawn(recv, wait, []), erlang:send_after(5, R, 42)` ⇒ `R` returns `42`.
#[test]
fn a_timer_wakes_a_receiver_that_blocked_before_it_fired() {
    let mut rt = runtime();
    rt.load_module(RECV_BEAM).expect("load recv.beam");
    let receiver = rt
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");

    // Slice 1: the receiver runs and blocks on an empty mailbox — *before* any timer
    // exists, so nothing but the receiver can have been running.
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    let blocked = rt.processes().get(receiver).expect("it stays in the table");
    assert_eq!(blocked.state(), ProcessState::Waiting);
    assert!(blocked.mailbox().is_empty());
    assert!(rt.process_result(receiver).is_none(), "it has not returned");
    assert_eq!(rt.run_queue_len(), 0, "nothing is runnable");

    let reference = send_after(&mut rt, 5, receiver.to_eterm(), make_small(42));
    assert!(rt.timers.is_timer_ref(reference), "the handle is a timer reference");
    assert_eq!(rt.timers.pending_count(), 1);

    // The pass has nothing to run, so it waits for the deadline, delivers the message
    // and resumes the receiver through the ordinary deliver+wake path.
    assert!(
        rt.run_scheduler().expect("the pass waits for the deadline") >= 1,
        "the resumed receiver ran"
    );
    assert_eq!(
        signed_value(rt.process_result(receiver).expect("the receiver returned")),
        42,
        "the timer's message became the receive's value"
    );
    assert!(rt.processes().get(receiver).is_none(), "it terminated normally");
    assert_eq!(rt.timers.pending_count(), 0, "the timer fired exactly once");
}

/// A **bounded** step never sleeps on a pending deadline — `run_one_slice` is a step, not
/// a wait — while the unbounded pass does wait, and the message is then in the target's
/// mailbox without any process having run.
#[test]
fn a_bounded_step_does_not_wait_for_a_pending_deadline() {
    let mut rt = runtime();
    let me = rt.self_pid();
    let reference = send_after(&mut rt, 200, me.to_eterm(), make_small(7));

    let started = Instant::now();
    assert_eq!(rt.run_one_slice().expect("nothing to run"), 0);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "a bounded step must not sleep on a deadline (took {:?})",
        started.elapsed()
    );
    assert_eq!(rt.timers.pending_count(), 1, "the timer is still pending");

    rt.run_scheduler().expect("the unbounded pass waits for the deadline");
    assert_eq!(rt.timers.pending_count(), 0);
    assert!(rt.timers.is_timer_ref(reference), "it stays a timer reference");
    let mailbox = rt.processes().get(me).expect("the runtime's own process").mailbox();
    assert_eq!(mailbox.len(), 1, "the message was delivered to its mailbox");
    assert_eq!(signed_value(mailbox.iter().next().expect("the message").payload()), 7);
}

/// A timer whose target has died (or whose name nobody holds) is consumed without
/// delivering anything: the pass ends instead of hanging, and the message is dropped —
/// the oracle's behaviour for a dead receiver.
#[test]
fn a_timer_to_a_dead_receiver_is_dropped() {
    let mut rt = runtime();
    let dead = beam_process::ProcessId::new(999_999, 0);
    let reference = send_after(&mut rt, 0, dead.to_eterm(), make_small(1));
    assert!(rt.timers.is_timer_ref(reference));

    assert_eq!(rt.run_scheduler().expect("the pass drains the deadline"), 0);
    assert_eq!(rt.timers.pending_count(), 0, "the timer was consumed");
    assert!(rt.processes().get(rt.self_pid()).expect("self").mailbox().is_empty());
}

/// Oracle: `erlang:send_after(5, self(), hello), receive X -> X end` ⇒ `hello` — the
/// `-eval` receive's pump waits for the deadline and rescans the mailbox.
#[test]
fn a_timer_message_reaches_a_blocking_eval_receive() {
    let mut rt = runtime();
    let value = rt
        .eval("erlang:send_after(5, self(), hello), receive X -> X end")
        .expect("the receive takes the timer's message");
    assert_eq!(rt.atom_name_of(value).as_deref(), Some("hello"));
    assert_eq!(rt.timers.pending_count(), 0);
}

/// Oracle: `erlang:send_after(5, self(), hello), receive hello -> got after 1000 ->
/// timeout end` ⇒ `got`.
#[test]
fn a_timer_message_is_taken_in_preference_to_a_timeout() {
    let mut rt = runtime();
    let value = rt
        .eval(
            "erlang:send_after(5, self(), hello), \
             receive hello -> got after 1000 -> timeout end",
        )
        .expect("the receive takes the timer's message");
    assert_eq!(rt.atom_name_of(value).as_deref(), Some("got"));
}

// -- receive … after T on the scheduler path ------------------------------

/// The committed `receive … after T` fixture, reused by relative path: every export
/// pins one literal-timeout opcode shape (see `tests/fixtures/recv_after.erl`).
const RECV_AFTER_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/recv_after.beam"
);

/// Spawns `recv_after:function/0` in `rt`.
fn spawn_recv_after(rt: &mut Runtime, function: &str) -> ProcessId {
    rt.load_module(RECV_AFTER_BEAM).expect("load recv_after.beam");
    rt.spawn_process("recv_after", function, vec![])
        .expect("the fixture exports it")
}

/// Oracle: `recv_after:after20()` ⇒ `timeout` — the receive parks on an empty mailbox,
/// its 20 ms deadline fires, and the process resumes at the **timed-out** path (the
/// `timeout` instruction) with **no** message delivered.
#[test]
fn a_literal_receive_timeout_fires_on_the_scheduler_path() {
    let mut rt = runtime();
    let receiver = spawn_recv_after(&mut rt, "after20");

    // Slice 1: the receiver finds an empty mailbox and parks, arming its deadline.
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    let parked = rt.processes().get(receiver).expect("it stays in the table");
    assert_eq!(parked.state(), ProcessState::Waiting);
    assert!(parked.mailbox().is_empty());
    assert!(rt.process_result(receiver).is_none(), "it has not returned");
    assert_eq!(rt.timers.pending_count(), 1, "the receive armed a deadline");

    // Nothing can run, so the pass waits for the deadline and resumes the receiver on
    // the timed-out path; had it delivered anything, the mailbox clause would have taken
    // it instead of running the `after` body.
    rt.run_scheduler().expect("the pass waits for the deadline");
    assert_eq!(
        rt.atom_name_of(rt.process_result(receiver).expect("the receiver returned"))
            .as_deref(),
        Some("timeout"),
        "the after clause ran"
    );
    assert_eq!(rt.timers.pending_count(), 0, "the deadline fired exactly once");
    assert!(rt.processes().get(receiver).is_none(), "it terminated normally");
}

/// Oracle: `recv_after:afterzero()` ⇒ `timeout` — an `after 0` literal is folded into a
/// direct `timeout` by the compiler, so the receive never suspends and arms no deadline.
#[test]
fn a_receive_after_zero_does_not_suspend() {
    let mut rt = runtime();
    let receiver = spawn_recv_after(&mut rt, "afterzero");

    assert_eq!(rt.run_scheduler().expect("the pass drains the run queue"), 1);
    assert_eq!(
        rt.atom_name_of(rt.process_result(receiver).expect("the receiver returned"))
            .as_deref(),
        Some("timeout")
    );
    assert_eq!(rt.timers.pending_count(), 0, "after 0 arms no deadline");
}

/// Oracle: `recv_after:self_send()` ⇒ `hello` — the message is already in the mailbox
/// when the receive starts, so it is taken and the long `after` clause is never reached
/// (and no deadline is armed for the abandoned clause).
#[test]
fn a_message_already_in_the_mailbox_is_taken_before_the_timeout() {
    let mut rt = runtime();
    let receiver = spawn_recv_after(&mut rt, "self_send");

    let started = Instant::now();
    rt.run_scheduler().expect("the receiver takes its own message");
    assert_eq!(
        rt.atom_name_of(rt.process_result(receiver).expect("the receiver returned"))
            .as_deref(),
        Some("hello")
    );
    assert_eq!(rt.timers.pending_count(), 0, "no deadline was armed");
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "the receive did not wait for its 2000 ms timeout"
    );
}

/// Oracle: a receiver parked in `receive X -> X after 2000 -> timeout end` that **is**
/// sent a message resumes normally with that message — and its receive deadline is
/// cancelled, so the pass does not also wait the full timeout out.
#[test]
fn a_message_wakes_a_receiver_before_its_deadline_and_cancels_it() {
    let mut rt = runtime();
    let receiver = spawn_recv_after(&mut rt, "after2000");

    // Slice 1: the receiver parks with a 2000 ms deadline.
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    assert_eq!(
        rt.processes().get(receiver).expect("in the table").state(),
        ProcessState::Waiting
    );
    assert_eq!(rt.timers.pending_count(), 1, "the receive armed its deadline");

    // `send_after` fires at 5 ms — long before the receive deadline — and the delivered
    // message wakes the receiver through the ordinary deliver+wake path.
    let _ = send_after(&mut rt, 5, receiver.to_eterm(), make_small(42));
    let started = Instant::now();
    rt.run_scheduler().expect("the pass delivers the message and resumes the receiver");
    assert_eq!(signed_value(rt.process_result(receiver).expect("the receiver returned")), 42);
    assert_eq!(rt.timers.pending_count(), 0);
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "the receive deadline was cancelled, not waited out (took {:?})",
        started.elapsed()
    );
}

/// Oracle: `spawn(recv_after, wait_forever, [])` stays parked — `after infinity`
/// compiles to a bare `wait`, which arms **no** deadline and never times out.
#[test]
fn after_infinity_parks_forever() {
    let mut rt = runtime();
    let receiver = spawn_recv_after(&mut rt, "wait_forever");

    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    assert_eq!(
        rt.processes().get(receiver).expect("in the table").state(),
        ProcessState::Waiting
    );
    assert!(rt.process_result(receiver).is_none(), "it never returned");
    assert_eq!(rt.timers.pending_count(), 0, "after infinity arms no deadline");

    let started = Instant::now();
    assert_eq!(rt.run_scheduler().expect("nothing can run"), 0);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the pass did not wait on a deadline (took {:?})",
        started.elapsed()
    );
    assert!(rt.processes().get(receiver).is_some(), "it is still parked");
}

// -- the CLI `run` action (`run_scheduled`) -------------------------------

/// A non-blocking export returns exactly what the single-pass `run_function` returns:
/// the result is recorded by the runner and handed back unchanged, so the CLI prints it
/// byte-identically (`run slice.beam slice:add/2 2 3` → `5`).
#[test]
fn run_scheduled_returns_a_non_blocking_export_unchanged() {
    let mut rt = runtime();
    let sum = rt
        .run_scheduled(SLICE_BEAM, "add", 2, &[make_small(2), make_small(3)])
        .expect("add/2 through the scheduler");
    assert_eq!(signed_value(sum), 5);
    let fib = rt
        .run_scheduled(SLICE_BEAM, "fib", 1, &[make_small(10)])
        .expect("fib/1 through the scheduler");
    assert_eq!(signed_value(fib), 55);
}

/// A canonical `run` invocation — the file's own name is what the process is spawned as,
/// so the already-loaded module is reused (no code-path search) and the export is the one
/// the caller named.
#[test]
fn run_scheduled_spawns_the_file_s_own_module() {
    let mut rt = runtime();
    let module = rt.load_module(SLICE_BEAM).expect("load slice.beam");
    let a = make_atom(module.atom_table.lookup("a").expect("a is interned"));
    let picked = rt
        .run_scheduled(SLICE_BEAM, "pick", 1, &[a])
        .expect("pick(a) through the scheduler");
    assert_eq!(signed_value(picked), 1);
}

/// A blocked export with nothing left that could wake it (empty run queue, no live port,
/// no pending timer) stops **immediately** with the single-pass `beam_engine::blocked`
/// diagnostic — never spun out to `RUN_DEADLINE`, never a wrong value. This is what keeps
/// `recv:wait/0` fast.
#[test]
fn run_scheduled_reports_a_quiescent_stall_immediately() {
    let mut rt = runtime();
    let started = Instant::now();
    let error = rt
        .run_scheduled(RECV_BEAM, "wait", 0, &[])
        .expect_err("nothing can wake the receiver");
    assert_eq!(error.to_string(), beam_engine::blocked().to_string(), "{error}");
    assert!(
        started.elapsed() < RUN_DEADLINE,
        "a quiescent stall is reported at once, not after the deadline (took {:?})",
        started.elapsed()
    );
}

/// An argument count that disagrees with the named arity is a caller bug, reported rather
/// than silently running a different function.
#[test]
fn run_scheduled_rejects_an_arity_mismatch() {
    let mut rt = runtime();
    let error = rt
        .run_scheduled(SLICE_BEAM, "add", 3, &[make_small(1), make_small(2)])
        .expect_err("two arguments for add/3");
    assert!(error.to_string().contains("given 2 argument(s)"), "{error}");
}

// -- a spawn made from inside a slice is registered and enqueued ----------

/// Creates a process in the runtime's own table for `slice:fib/1` and records it as a
/// **pending spawn** — exactly what `src/native/process.rs`'s `spawn_in_pass` leaves behind
/// when an `erlang:spawn*` runs *inside* a scheduler slice.
///
/// The test writes the record directly instead of driving a BIF so the pass-side contract
/// ([`Runtime::absorb_pending_spawns`]) is pinned on its own, without a compiled fixture; the
/// BIF half is proved end to end by `tests/spawn_from_slice.rs`.
fn record_pending_fib(rt: &mut Runtime, n: i64, priority: Priority) -> ProcessId {
    let policy = rt.default_spawn_policy();
    let pid = rt.processes.spawn(policy).expect("a child in the process table");
    rt.processes
        .set_priority(pid, priority)
        .expect("the child's priority");
    let entry = rt
        .resolve_export_entry("slice", "fib", 1)
        .expect("slice:fib/1 resolves");
    rt.pending_spawns.push(PendingSpawn {
        pid,
        entry,
        args: vec![make_small(n)],
    });
    pid
}

/// A process recorded for the pass is **registered and run**: the pass installs its engine
/// entry in its runner state, puts it on its run queue, and the very next round carries it to
/// its `return` — the child is scheduled, not lost.
#[test]
fn a_recorded_spawn_is_registered_and_run_by_the_pass() {
    let mut rt = runtime();
    rt.load_module(SLICE_BEAM).expect("load slice.beam");
    let pid = record_pending_fib(&mut rt, 10, Priority::Normal);

    let ran = rt.run_scheduler().expect("a pass");
    assert_eq!(ran, 1, "the recorded child was the only runnable process");
    assert_eq!(
        signed_value(rt.process_result(pid).expect("the child ran to its return")),
        fib(10),
        "the child ran its own entry point"
    );
    assert!(rt.processes().get(pid).is_none(), "it terminated normally");
    assert_eq!(rt.run_queue_len(), 0, "the run queue drained");
}

/// The recorded child is enqueued **at its own priority**: with the slice's budget spent by
/// the process that was already runnable, the drain still runs (so the child is never left
/// unqueued) and the child waits where its PCB says it belongs.
#[test]
fn a_recorded_spawn_is_enqueued_at_its_own_priority() {
    let mut rt = runtime();
    rt.load_module(SLICE_BEAM).expect("load slice.beam");
    // A runnable process that consumes the single slice's budget.
    let busy = rt
        .spawn_process("slice", "fib", vec![make_small(10)])
        .expect("slice:fib/1 is exported");
    let child = record_pending_fib(&mut rt, 5, Priority::Low);

    let ran = rt.run_slices(1).expect("one bounded slice");
    assert_eq!(ran, 1, "the bounded pass ran exactly its budget");
    assert!(rt.process_result(busy).is_some(), "the queued process ran");

    // The child is on the run queue, in the **low** level, and not run yet.
    assert_eq!(
        rt.scheduler.run_queue().len_at(Priority::Low),
        1,
        "the recorded child was enqueued at its own priority"
    );

    assert_eq!(rt.run_scheduler().expect("the rest of the pass"), 1);
    assert_eq!(
        signed_value(rt.process_result(child).expect("the child ran")),
        fib(5)
    );
}
