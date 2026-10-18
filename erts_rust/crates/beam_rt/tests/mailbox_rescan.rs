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
//! The **mailbox scan position across a suspend/resume**: a receiver parked on an empty
//! mailbox takes the message that arrives later — including the `recv_marker_*`
//! (selective-receive skip) shape — and a finite `receive … after T` still fires exactly
//! when it should.
//!
//! `src/mailbox.rs`'s unit tests pin the scan state machine ([`MailboxState`]); this file
//! pins the same properties **end to end**, through real compiled code, the engine's
//! receive family and the scheduled path. The image is the committed oracle fixture
//! `tests/fixtures/rescan.beam` (`erlc +deterministic rescan.erl`, recipe in
//! `tests/fixtures/README.md`), which is reused by relative path — never copied into the
//! crate, and only ever copied into a *private* scratch dir for the oracle probe (the
//! fixture directory holds a `lists.beam` that would shadow stdlib).
//!
//! Every expectation is the value the system `erl` returns for the same call, and the last
//! test asserts exactly that (`beam_rt run …` vs `erl -noshell -eval …`) for all six
//! exports:
//!
//! | export | oracle expression | value |
//! |---|---|---|
//! | `rescan:parked/0` | `rescan:parked()` | `42` |
//! | `rescan:marked/0` | `rescan:marked()` | `done` |
//! | `rescan:junk_then_ref/0` | `rescan:junk_then_ref()` | `7` |
//! | `rescan:two_receives/0` | `rescan:two_receives()` | `3` |
//! | `rescan:timed/0` | `rescan:timed()` | `timeout` (after ≈ 20 ms) |
//! | `rescan:timedhit/0` | `rescan:timedhit()` | `done` (the deadline is cancelled) |
//!
//! `marked/0`, `junk_then_ref/0`, `two_receives/0`, `timed/0` and `timedhit/0` all match on
//! a **reference created just before the receive**, which is the shape `beam_ssa_recv`
//! rewrites with `recv_marker_reserve`/`_bind`/`_use` (see `rescan.S`); `parked/0` is the
//! plain `loop_rec`/`wait` shape, so both receive families are covered.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use beam_process::ProcessState;
use beam_rt::Runtime;
use beam_term::{signed_value, Eterm};
#[allow(dead_code)]
mod support;
/// The executable under test (the `run` route's oracle comparison drives it).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
/// The committed fixture (`erlc +deterministic rescan.erl`).
const RESCAN_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rescan.beam");
/// The fixture directory the oracle probe copies *from* (never handed to `-pa` directly).
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
/// The `erl`-facing banner for the loud skip.
const BANNER: &str = "mailbox re-scan";
/// Every export, with its oracle value (the table in the module docs).
const EXPORTS: [(&str, &str); 6] = [
    ("parked", "42"),
    ("marked", "done"),
    ("junk_then_ref", "7"),
    ("two_receives", "3"),
    ("timed", "timeout"),
    ("timedhit", "done"),
];
/// A runtime with the fixture loaded explicitly (so `rescan` resolves from the code
/// server's cache, exactly as a `-pa` directory would resolve it).
fn runtime() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(RESCAN_BEAM).expect("the committed fixture loads");
    rt
}
/// Spawns `rescan:<function>/0` on a fresh runtime, returning the runtime and the pid.
fn spawn(function: &str) -> (Runtime, beam_process::ProcessId) {
    let mut rt = runtime();
    let pid = rt
        .spawn_process("rescan", function, vec![])
        .expect("the fixture exports it");
    (rt, pid)
}
/// The atom name of a returned value (a loud failure when it is not an atom).
fn atom_name(rt: &Runtime, value: Eterm) -> String {
    rt.atom_name_of(value)
        .unwrap_or_else(|| panic!("expected an atom, got {value:?}"))
        .to_string()
}
// ---------------------------------------------------------------------------------------
// (i) A receiver parked on an EMPTY mailbox takes the very next message sent to it
// ---------------------------------------------------------------------------------------
/// Oracle: `rescan:parked()` ⇒ `42`.
///
/// The receiver finds an **empty** mailbox, `loop_rec` reports no candidate and `wait`
/// suspends it; the message that wakes it is the one the spawned sender sends *after*
/// the suspend, so the scan position has to survive the park.
#[test]
fn a_receiver_parked_on_an_empty_mailbox_takes_the_next_message() {
    let (mut rt, receiver) = spawn("parked");
    // One slice: the receiver spawns its sender, finds no candidate and parks.
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    let parked = rt.processes().get(receiver).expect("it stays in the table");
    assert_eq!(parked.state(), ProcessState::Waiting, "the receive suspended");
    assert!(parked.mailbox().is_empty(), "it parked on an EMPTY mailbox");
    assert!(rt.process_result(receiver).is_none(), "it has not returned");
    // Nothing else to run but the spawned sender: it sends, the receiver is woken and
    // takes the message.
    let started = Instant::now();
    rt.run_scheduler().expect("the sender wakes the receiver");
    let result = rt.process_result(receiver).expect("the receiver returned");
    assert_eq!(signed_value(result), 42, "the next message was taken");
    assert!(rt.process_exit(receiver).is_none(), "it terminated normally");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the parked receive was resumed, not timed out (took {:?})",
        started.elapsed()
    );
}
/// Oracle: `rescan:marked()` ⇒ `done`.
///
/// The **marker'd** shape parked on an **empty** mailbox: `recv_marker_reserve` runs when
/// the queue is empty, so the marker is the empty anchor — it must still re-position the
/// scan onto the message the sender delivers while the receiver is suspended.
#[test]
fn a_marker_reserved_on_an_empty_mailbox_still_resumes_onto_the_next_message() {
    let (mut rt, receiver) = spawn("marked");
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    let parked = rt.processes().get(receiver).expect("it stays in the table");
    assert_eq!(parked.state(), ProcessState::Waiting, "the receive suspended");
    assert!(parked.mailbox().is_empty(), "the marker was reserved on an empty queue");
    rt.run_scheduler().expect("the sender wakes the receiver");
    let result = rt.process_result(receiver).expect("the receiver returned");
    assert_eq!(atom_name(&rt, result), "done", "the marker'd receive matched");
}
// ---------------------------------------------------------------------------------------
// The marker anchors behind the messages queued before the reference
// ---------------------------------------------------------------------------------------
/// Oracle: `rescan:junk_then_ref()` ⇒ `7`.
///
/// `junk` is queued *before* `make_ref/0`, so the marker anchors after it and the
/// selective receive must skip it and take the message queued afterwards. (The compiler
/// proves a pre-anchor message cannot match — `beam_ssa_recv.erl:26-33` — so this is a
/// *scan cost* property, and the value must be the same either way.)
#[test]
fn a_marker_skips_the_messages_queued_before_its_reference() {
    let (mut rt, receiver) = spawn("junk_then_ref");
    rt.run_scheduler().expect("the receive drains");
    let result = rt.process_result(receiver).expect("the receiver returned");
    assert_eq!(signed_value(result), 7, "the post-reference message was taken");
}
/// Oracle: `rescan:two_receives()` ⇒ `3`.
///
/// Two receives share one reference, so each starts with `recv_marker_use`; the first one
/// **removes** the message it took, which shifts the queue under the marker. The second
/// receive must still find `{Ref, 2}` — the anchor is length-stable, a raw index is not.
#[test]
fn a_marker_survives_the_removal_the_first_receive_made() {
    let (mut rt, receiver) = spawn("two_receives");
    rt.run_scheduler().expect("both receives drain");
    let result = rt.process_result(receiver).expect("the receiver returned");
    assert_eq!(signed_value(result), 3, "1 + 2 — both messages were taken");
}
// ---------------------------------------------------------------------------------------
// (ii) `receive … after T`
// ---------------------------------------------------------------------------------------
/// Oracle: `rescan:timed()` ⇒ `timeout` (after ≈ 20 ms).
///
/// An empty mailbox, nothing that can match, and a finite deadline: the scan runs out, the
/// receiver parks, and the `after` clause fires. The elapsed lower bound is the only timing
/// assertion — it rules out the deadline being skipped, and nothing else depends on load.
#[test]
fn an_empty_mailbox_with_no_match_fires_the_timeout() {
    let (mut rt, receiver) = spawn("timed");
    let started = Instant::now();
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    assert_eq!(
        rt.processes().get(receiver).expect("in the table").state(),
        ProcessState::Waiting,
        "the receive parked instead of returning at once"
    );
    rt.run_scheduler().expect("the pass waits the deadline out");
    let elapsed = started.elapsed();
    let result = rt.process_result(receiver).expect("the receiver returned");
    assert_eq!(atom_name(&rt, result), "timeout", "the after clause ran");
    assert!(
        elapsed >= Duration::from_millis(10),
        "the 20 ms deadline was waited out, not skipped (took {elapsed:?})"
    );
    assert!(rt.processes().get(receiver).is_none(), "it terminated normally");
}
/// Oracle: `rescan:timedhit()` ⇒ `done`.
///
/// The same finite deadline, but a matching message arrives while the receiver is parked:
/// the timeout must be **cancelled**, never raced — so the answer is the message, and the
/// pass does not then wait the deadline out.
#[test]
fn a_message_arriving_in_time_cancels_the_timeout() {
    let (mut rt, receiver) = spawn("timedhit");
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    assert_eq!(
        rt.processes().get(receiver).expect("in the table").state(),
        ProcessState::Waiting,
        "the receiver parked on its empty mailbox"
    );
    let started = Instant::now();
    rt.run_scheduler().expect("the sender delivers before the deadline");
    let elapsed = started.elapsed();
    let result = rt.process_result(receiver).expect("the receiver returned");
    assert_eq!(atom_name(&rt, result), "done", "the message beat the deadline");
    assert!(
        elapsed < Duration::from_secs(5),
        "the receive deadline was cancelled, not waited out (took {elapsed:?})"
    );
}
// ---------------------------------------------------------------------------------------
// The live-oracle cross-check (the `run` route vs `erl -noshell`)
// ---------------------------------------------------------------------------------------
/// A private scratch directory holding a copy of the committed fixture.
///
/// The oracle gets a single `-pa`, and the fixture directory itself must never be one (it
/// contains a `lists.beam` that shadows stdlib), so the `.beam` is copied out first.
struct Scratch {
    /// The scratch directory (removed on drop).
    dir: PathBuf,
}
impl Scratch {
    /// Copies `rescan.beam` into a fresh private directory.
    fn new() -> Scratch {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "beam_rt_mailbox_rescan_{}_{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|error| panic!("cannot prepare {}: {error}", dir.display()));
        let from = Path::new(FIXTURE_DIR).join("rescan.beam");
        std::fs::copy(&from, dir.join("rescan.beam")).unwrap_or_else(|error| {
            panic!("cannot copy the committed fixture {}: {error}", from.display())
        });
        Scratch { dir }
    }
    /// The copied `rescan.beam`.
    fn beam(&self) -> PathBuf {
        self.dir.join("rescan.beam")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the scratch dir {}: {error}",
                self.dir.display()
            );
        }
    }
}
/// Every export's value agrees with the live `erl` oracle, on the scheduled `run` route.
///
/// A missing `erl` is a **loud skip** naming the banner (never a silent pass); a probe that
/// cannot answer is a loud failure.
#[test]
fn every_export_agrees_with_the_live_oracle() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let scratch = Scratch::new();
    let beam = scratch.beam();
    let beam = beam.to_str().expect("the scratch path is UTF-8");
    for (function, expected) in EXPORTS {
        let module_function = format!("rescan:{function}");
        let spec = format!("{module_function}/0");
        let ours = support::run_bounded(Path::new(BINARY), &["run", beam, &spec])
            .unwrap_or_else(|| panic!("beam_rt run {spec} could not run to completion"));
        assert_eq!(
            ours.status.code(),
            Some(0),
            "beam_rt run {spec} must exit 0: {}",
            String::from_utf8_lossy(&ours.stderr)
        );
        let ours = String::from_utf8_lossy(&ours.stdout).trim().to_string();
        let expr = format!("io:format(\"~w~n\", [{module_function}()]), halt(0).");
        let theirs = support::oracle_eval(&oracle, Some(&scratch.dir), &expr)
            .unwrap_or_else(|| panic!("the live oracle could not answer {spec}"))
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        assert_eq!(theirs, expected, "the recorded oracle value for {spec} is current");
        assert_eq!(ours, theirs, "{spec} diverges from the oracle");
    }
}
