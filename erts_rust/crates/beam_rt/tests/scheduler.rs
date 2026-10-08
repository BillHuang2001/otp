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

//! The **scheduler vertical slice**: several processes run, one blocks in a `receive`,
//! and a **send resumes it** — end to end, through the real engine and the real mailbox.
//!
//! `tests/slice.rs` proves the runtime can run *one* function; `tests/mailbox.rs` proves a
//! `receive` finds a message that is already queued. This test proves the piece that ties
//! them together: [`Runtime::spawn_process`] +
//! [`Runtime::run_scheduler`](beam_rt::Runtime::run_scheduler) drive a
//! [`beam_process::Scheduler`] over [`Runtime`]'s real `ProcessRunner`, so a process that
//! blocks on an empty mailbox is left `Waiting`, a message delivered **by another
//! process** wakes it, and the scheduler absorbs that wake and resumes it exactly where it
//! stopped.
//!
//! The images are the committed oracle fixtures, reused by **relative path** (never
//! copied, never rebuilt — a single source of truth for each of them):
//!
//! * `recv.beam` — `../beam_engine/tests/fixtures/recv.beam` (built with the system
//!   `erlc`; regeneration recipe in that directory's `README.md`), and
//! * `mailbox.beam` — `tests/fixtures/mailbox.beam` (recipe in `tests/fixtures/README.md`).
//!
//! Every expectation below is the value the **system `erl`** returns for the same
//! computation (OTP 29 / `erts-17.1`, the differential oracle). The oracle expressions and
//! values are quoted next to each assertion; `recv.beam` must be reachable on its own
//! (`erl -noshell -pa <dir-with-rec-v.beam> -eval '…'`), because the whole fixture
//! directory cannot go on `-pa` (it contains a `lists.beam` that would shadow stdlib).

use beam_process::ProcessState;
use beam_rt::Runtime;
use beam_term::{make_small, signed_value};

/// The committed receive fixture (`../beam_engine/tests/fixtures/recv.beam`).
const RECV_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_engine/tests/fixtures/recv.beam"
);

/// This crate's own receive fixture (`tests/fixtures/mailbox.beam`).
const MAILBOX_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mailbox.beam");

/// This crate's sender fixture (`tests/fixtures/sched.beam`), which can send a matching
/// message *after* a non-matching head (something `recv:bomb/1` cannot do).
const SCHED_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sched.beam");

/// A runtime with `fixture` loaded explicitly (so the module is resolved from the code
/// server's cache, exactly as a `-pa` directory would resolve it).
fn runtime_with(fixture: &str) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(fixture).expect("the committed fixture loads");
    rt
}

/// Oracle: `R = spawn(recv, wait, []), S = spawn(recv, bomb, [R])` ⇒ the receiver
/// delivers `42` and the sender returns `ok`; both terminate.
///
/// ```erl
/// Self = self(),
/// R = spawn(fun() -> Self ! recv:wait() end),
/// S = spawn(fun() -> Self ! recv:bomb(R) end).   %% 42 and ok
/// ```
#[test]
fn a_sender_wakes_a_blocked_receiver_and_its_message_becomes_the_result() {
    let mut rt = runtime_with(RECV_BEAM);
    let receiver = rt
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");
    let sender = rt
        .spawn_process("recv", "bomb", vec![receiver.to_eterm()])
        .expect("spawn recv:bomb/1");
    assert_eq!(rt.run_queue_len(), 2, "both processes are runnable");

    let slices = rt.run_scheduler().expect("the pass runs to completion");
    assert!(slices > 0, "the pass ran at least one process");

    let result = rt.process_result(receiver).expect("the receiver returned");
    assert_eq!(
        signed_value(result),
        42,
        "the resumed receive took the message the sender sent"
    );
    let sender_result = rt.process_result(sender).expect("the sender returned");
    assert_eq!(rt.atom_name_of(sender_result), Some("ok"));

    // A process that returns exits with `normal` — the value is *not* its exit reason —
    // so no abnormal reason is recorded for either of them.
    assert!(rt.process_exit(receiver).is_none());
    assert!(rt.process_exit(sender).is_none());
    // Both terminated, so the process table no longer holds them and the queue is empty.
    assert!(rt.processes().get(receiver).is_none());
    assert!(rt.processes().get(sender).is_none());
    assert_eq!(rt.run_queue_len(), 0);
}

/// The receiver genuinely blocks **before** the sender runs: one slice suffices to leave
/// it `Waiting` with an empty mailbox, and the send in a later slice wakes it.
///
/// This is the ordering the oracle cannot show from outside: it distinguishes "the
/// receiver was resumed by the send" from "the message was already there".
#[test]
fn the_receiver_blocks_first_and_a_later_senders_wake_resumes_it() {
    let mut rt = runtime_with(RECV_BEAM);
    let receiver = rt
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");
    let sender = rt
        .spawn_process("recv", "bomb", vec![receiver.to_eterm()])
        .expect("spawn recv:bomb/1");

    // Slice 1: the receiver is first in the run queue, runs, and blocks on an empty
    // mailbox — before the sender has sent anything at all.
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    let blocked = rt
        .processes()
        .get(receiver)
        .expect("a blocked process stays in the table");
    assert_eq!(blocked.state(), ProcessState::Waiting);
    assert!(blocked.mailbox().is_empty(), "nothing has been sent yet");
    assert!(rt.process_result(receiver).is_none(), "it has not returned");
    assert_eq!(rt.run_queue_len(), 1, "only the sender is left to run");

    // The rest of the run: the sender sends, the wake is absorbed at the end of that
    // slice and the receiver resumes in the very machine it suspended in.
    assert!(rt.run_scheduler().expect("the rest of the pass") >= 1);
    assert_eq!(signed_value(rt.process_result(receiver).expect("42")), 42);
    let sender_result = rt.process_result(sender).expect("the sender returned");
    assert_eq!(rt.atom_name_of(sender_result), Some("ok"));
}

/// A non-matching head is **scanned past, not removed**: `recv:scan/0` matches only `foo`
/// / `{bar, X}`, so a lone `42` leaves it blocked with the message still queued.
///
/// Oracle: `R = spawn(recv, scan, []), R ! 42` ⇒ `is_process_alive(R)` is `true` and
/// `erlang:process_info(R, messages)` still lists `42`.
#[test]
fn a_non_matching_head_is_skipped_without_being_removed() {
    let mut rt = runtime_with(RECV_BEAM);
    let receiver = rt
        .spawn_process("recv", "scan", vec![])
        .expect("spawn recv:scan/0");
    let sender = rt
        .spawn_process("recv", "bomb", vec![receiver.to_eterm()])
        .expect("spawn recv:bomb/1");

    rt.run_scheduler().expect("the pass drains the queue");

    let waiting = rt
        .processes()
        .get(receiver)
        .expect("the receiver is still alive");
    assert_eq!(waiting.state(), ProcessState::Waiting);
    assert_eq!(
        waiting.mailbox().len(),
        1,
        "`loop_rec_end` advances the scan; the skipped head stays queued"
    );
    assert_eq!(
        signed_value(waiting.mailbox().iter().next().expect("the message").payload()),
        42
    );
    assert!(rt.process_result(receiver).is_none(), "it has not returned");
    assert!(
        rt.process_result(sender).is_some(),
        "the sender ran and returned"
    );
    assert_eq!(
        rt.run_queue_len(),
        0,
        "a process waiting for a message nobody will send does not hang the loop"
    );
}

/// A `receive` that scans past a non-matching head still takes the matching message:
/// `mailbox:scan/0` sends `42` then `{bar, 7}` to itself and receives `{bar, X}`.
///
/// Oracle: `mailbox:scan()` ⇒ `7`.
#[test]
fn a_receive_scans_past_a_non_matching_head_to_the_matching_message() {
    let mut rt = runtime_with(MAILBOX_BEAM);
    let pid = rt
        .spawn_process("mailbox", "scan", vec![])
        .expect("spawn mailbox:scan/0");

    rt.run_scheduler().expect("the pass drains the queue");

    assert_eq!(
        signed_value(rt.process_result(pid).expect("the process returned")),
        7
    );
    assert!(rt.processes().get(pid).is_none(), "it terminated normally");
}

/// The **cross-process** version of the same shape: one process sends the non-matching
/// head and then the match, and the receiver — which was already blocked — wakes up,
/// scans past the head and takes the match.
///
/// Oracle: `R = spawn(recv, scan, []), S = spawn(sched, send2, [R, 0])` ⇒ the receiver
/// delivers `7` and the sender returns `ok`; both terminate.
#[test]
fn a_sender_that_sends_a_head_then_a_match_completes_the_receive() {
    let mut rt = runtime_with(RECV_BEAM);
    rt.load_module(SCHED_BEAM).expect("the fixture loads");
    let receiver = rt
        .spawn_process("recv", "scan", vec![])
        .expect("spawn recv:scan/0");
    let sender = rt
        .spawn_process("sched", "send2", vec![receiver.to_eterm(), make_small(0)])
        .expect("spawn sched:send2/2");

    rt.run_scheduler().expect("the pass drains the queue");

    assert_eq!(
        signed_value(rt.process_result(receiver).expect("the receiver returned")),
        7,
        "the receive skipped the `42` and took the `{{bar, 7}}` that followed it"
    );
    let sender_result = rt.process_result(sender).expect("the sender returned");
    assert_eq!(rt.atom_name_of(sender_result), Some("ok"));
    assert!(rt.processes().get(receiver).is_none());
    assert!(rt.processes().get(sender).is_none());
}

/// A process waiting for a message nobody sends simply stays `Waiting`; the loop ends
/// instead of hanging, and a second pass has nothing to do.
#[test]
fn a_waiter_with_no_sender_never_hangs_the_pass() {
    let mut rt = runtime_with(RECV_BEAM);
    let pid = rt
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");

    assert_eq!(rt.run_scheduler().expect("one slice ran"), 1);
    assert_eq!(
        rt.processes().get(pid).expect("still alive").state(),
        ProcessState::Waiting
    );
    assert_eq!(rt.run_queue_len(), 0);
    // Nothing is runnable now, so another pass (and a single step) run nothing at all.
    assert_eq!(rt.run_scheduler().expect("nothing to do"), 0);
    assert_eq!(rt.run_one_slice().expect("nothing to do"), 0);
}

/// Spawning resolves `function/arity` honestly: an unexported function and a missing
/// module both raise the oracle's bare `error:undef` (reason atom `undef`; the
/// module/function/arity live in the stacktrace, not the reason), exactly as the run
/// path does — and neither leaves a half-spawned process behind.
#[test]
fn a_spawn_of_an_unexported_function_or_unknown_module_raises_undef() {
    let mut rt = runtime_with(RECV_BEAM);

    let error = rt
        .spawn_process("recv", "nope", vec![])
        .expect_err("recv:nope/0 is not exported");
    assert!(error.is_raise(), "{error}");
    assert_eq!(error.as_atom(), Some("undef"), "{error}");
    // The bare atom, not a message: it is a genuine runtime error (the CLI exits 1),
    // never a Milestone-1 capability gap (which would print `unsupported: …`).
    assert_eq!(error.to_string(), "error undef");

    let error = rt
        .spawn_process("recv", "bomb", vec![])
        .expect_err("recv:bomb/0 does not exist");
    assert_eq!(error.as_atom(), Some("undef"), "{error}");

    let error = rt
        .spawn_process("no_such_module_at_all", "f", vec![])
        .expect_err("the module cannot be loaded");
    // A missing module is the oracle's `exception error undef` (reason atom `undef`).
    assert_eq!(error.as_atom(), Some("undef"), "{error}");

    assert_eq!(rt.run_queue_len(), 0, "no failed spawn enqueued anything");
}

/// Only a process the runtime spawned has an engine to run, so enqueuing anything else
/// (a hand-built pid, the runtime's own process) is refused rather than run as nothing.
#[test]
fn enqueue_process_rejects_a_pid_without_an_entry_point() {
    let mut rt = runtime_with(RECV_BEAM);

    let error = rt
        .enqueue_process(beam_process::ProcessId::new(999_999, 0))
        .expect_err("no engine was registered for that pid");
    assert!(error.to_string().contains("no engine entry point"), "{error}");

    let self_pid = rt.self_pid();
    assert!(
        rt.enqueue_process(self_pid).is_err(),
        "the runtime's own process is not a spawned entry point"
    );
    assert_eq!(rt.run_queue_len(), 0);
}

/// Enqueuing a process that is already queued is a no-op, so a caller cannot accidentally
/// run one process twice per round.
#[test]
fn enqueuing_a_process_twice_is_idempotent() {
    let mut rt = runtime_with(RECV_BEAM);
    let pid = rt
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");
    rt.enqueue_process(pid).expect("already spawned ⇒ known");
    assert_eq!(rt.run_queue_len(), 1);
}
