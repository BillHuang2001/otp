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

//! Receive/message instruction-family tests.
//!
//! Drives the committed oracle fixture `tests/fixtures/recv.beam` (built with the
//! system `erlc`; regeneration recipe in `tests/fixtures/README.md`) through a
//! [`MinimalContext`] whose **mailbox** is a tiny in-memory implementation, so the
//! whole `send` / `loop_rec` / `loop_rec_end` / `wait` / `wait_timeout` / `timeout`
//! / `remove_message` / `recv_marker_*` family runs end to end.
//!
//! Every expected value is **oracle-derived** from the real system `erl`, run in a
//! neutral directory with the fixture on the code path:
//!
//! ```sh
//! erl -noshell -pa "$TMPDIR/fixgen" -eval '<expr>, halt().'
//! ```
//!
//! | export | oracle expression | value |
//! |---|---|---|
//! | `wait/0` | `Self=self(), Self ! hello, recv:wait()` | `hello` |
//! | `bomb/1` | `Self=self(), recv:bomb(Self), receive M -> M end` | `42` |
//! | `sel/0` | `Self=self(), Self ! 99, recv:sel()` | `99` |
//! | `sel/0` | `Self=self(), Self ! foo, recv:sel()` | `a` |
//! | `afterzero/0` | `recv:afterzero()` (empty mailbox) | `empty` |
//! | `scan/0` | `Self=self(), Self ! 42, Self ! {bar,7}, recv:scan()` | `7` |
//! | `scan/0` | `Self=self(), Self ! foo, recv:scan()` | `foo` |
//! | `aftervar/1` | `recv:aftervar(0)` (empty mailbox) | `empty` |
//! | `aftervar/1` | `Self=self(), Self ! foo, recv:aftervar(0)` | `foo` |
//! | `aftervar/1` | `recv:aftervar(5)` (empty mailbox) | `empty` (times out) |
//! | `marked/0` | `recv:marked()` | `7` |
//!
//! `bomb/1`'s return value is `ok` (the oracle expression above drains the sent
//! `42` out of the process mailbox, which is the message the engine must deliver).
//!
//! A `receive` that finds no message **suspends**: [`Machine::run_export_outcome`]
//! reports [`RunOutcome::Wait`] and [`Machine::resume_outcome`] re-enters at the
//! reported address once a message arrives. The plain [`Machine::run_export`] is
//! non-blocking and folds that into the typed
//! `unsupported_opcode: run: process blocked (no message available)` error.
//!
//! The one deliberate divergence is a **non-zero `after` timeout**: Milestone 1 has
//! no host timer, so `aftervar(5)` on an empty mailbox reports `Wait` where the
//! oracle returns `empty` after 5 ms (tested below and documented, never a wrong
//! result).

use std::cell::RefCell;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::{Machine, Mailbox, MinimalContext, RunOutcome, Tier};
use beam_loader::{load_file, Module};
use beam_term::{is_small, make_atom, make_small, Atom, Eterm};

/// The committed receive fixture (`erlc recv.erl`).
const RECV_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/recv.beam");

// ---------------------------------------------------------------------------
// A tiny in-memory mailbox implementing the engine's `Mailbox` seam
// ---------------------------------------------------------------------------

/// One recorded receive marker: the opaque marker term, the reference it was bound
/// to (if any) and the scan position it recorded.
struct Marker {
    term: Eterm,
    reference: Option<Eterm>,
    position: usize,
}

/// The mailbox's shared state (shared with the test so it can inspect the queue
/// after a run has consumed the `Box<dyn Mailbox>`).
#[derive(Default)]
struct MailboxState {
    /// The queued messages, in arrival order.
    messages: Vec<Eterm>,
    /// The current scan position (`loop_rec` peeks here, `loop_rec_end` advances).
    scan: usize,
    /// Reserved receive markers.
    markers: Vec<Marker>,
    /// Distinguishes consecutive reserved markers.
    next_marker: i64,
}

/// A tiny in-memory mailbox: a `Vec` of messages, a scan position and a marker
/// table. It reproduces the C message-queue contract the receive family drives
/// (`../../erts/emulator/beam/erl_proc_sig_queue.h:2225-2394`): `peek` at the save
/// position, `set_save_next` advances it, `unlink_msg_set_save_first` removes the
/// peeked message and resets the scan, `set_save_first` resets the scan.
#[derive(Clone)]
struct TestMailbox {
    state: Rc<RefCell<MailboxState>>,
    /// The term `recv_marker_reserve` returns when the queue is empty (the C
    /// `am_undefined`); a marker request with nothing to record is a no-op marker.
    undefined: Eterm,
}

impl TestMailbox {
    /// A mailbox whose empty-queue marker request yields `undefined`.
    fn new(undefined: Eterm) -> TestMailbox {
        TestMailbox {
            state: Rc::new(RefCell::new(MailboxState::default())),
            undefined,
        }
    }

    /// Queue `message` as if it had been sent to this mailbox.
    fn push(&self, message: Eterm) {
        self.state.borrow_mut().messages.push(message);
    }

    /// The queued messages, in arrival order.
    fn messages(&self) -> Vec<Eterm> {
        self.state.borrow().messages.clone()
    }
}

impl Mailbox for TestMailbox {
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        // The engine's receive tests always send to this very mailbox, so the
        // destination is not routed (a real host resolves `to` to a process).
        let _ = to;
        self.push(message);
        Ok(())
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        let state = self.state.borrow();
        Ok(state.messages.get(state.scan).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.state.borrow_mut().scan += 1;
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        let mut state = self.state.borrow_mut();
        let scan = state.scan;
        if scan < state.messages.len() {
            state.messages.remove(scan);
        }
        state.scan = 0;
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.state.borrow_mut().scan = 0;
        Ok(())
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        let undefined = self.undefined;
        let mut state = self.state.borrow_mut();
        if state.messages.is_empty() {
            // C: an empty queue has no position to record.
            return Ok(undefined);
        }
        let n = state.next_marker;
        state.next_marker += 1;
        let term = make_small(9000 + n);
        let position = state.scan;
        state.markers.push(Marker {
            term,
            reference: None,
            position,
        });
        Ok(term)
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        // The C only binds a live (small/big) marker id; `undefined` and any other
        // term is a silent no-op (`erl_proc_sig_queue.h:2238-2263`).
        if !is_small(marker) {
            return Ok(());
        }
        let mut state = self.state.borrow_mut();
        if let Some(entry) = state.markers.iter_mut().find(|m| m.term == marker) {
            entry.reference = Some(reference);
        }
        Ok(())
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        let mut state = self.state.borrow_mut();
        let position = state
            .markers
            .iter()
            .find(|m| m.reference == Some(reference))
            .map(|m| m.position);
        if let Some(position) = position {
            state.scan = position;
        }
        Ok(())
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if let Some(index) = state
            .markers
            .iter()
            .position(|m| m.reference == Some(reference))
        {
            state.markers.remove(index);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fixture + context helpers
// ---------------------------------------------------------------------------

/// Load the fixture and intern the `undefined` atom (the fixtures do not name it,
/// but `MinimalContext`'s mailbox needs a canonical "no marker" term).
fn load() -> (Module, Atom) {
    let mut module = load_file(RECV_BEAM).unwrap_or_else(|e| panic!("load recv.beam: {e:?}"));
    let undefined = module.atom_table.intern("undefined");
    (module, undefined)
}

/// `module:function`'s name atom.
fn name(module: &Module, function: &str) -> Atom {
    module
        .atom_table
        .lookup(function)
        .unwrap_or_else(|| panic!("atom {function:?} not interned"))
}

/// A context with `mailbox` installed and the two BIFs `marked/0` needs.
///
/// `erlang:make_ref/0` returns a fresh small integer (a stand-in for a reference —
/// the fixture only ever compares it for equality) and `erlang:self/0` the atom
/// `self`; the test mailbox ignores the destination. Every other BIF is `BadArg`.
fn context(module: &Module, mailbox: TestMailbox) -> MinimalContext {
    let self_atom = module
        .atom_table
        .lookup("self")
        .expect("the fixture imports `self/0`, so the atom is interned");
    let mut next_ref: i64 = 100;
    let mut ctx = MinimalContext::with_bif_hook(Box::new(move |m, f, a, _args| {
        if m == "erlang" && a == 0 {
            if f == "make_ref" {
                let value = make_small(next_ref);
                next_ref += 1;
                return Ok(value);
            }
            if f == "self" {
                return Ok(make_atom(self_atom));
            }
        }
        Err(BeamError::BadArg)
    }));
    ctx.set_mailbox(Box::new(mailbox));
    ctx
}

/// Run an export to completion (the non-blocking entry), over `ctx`.
fn run(module: &Module, function: &str, args: &[Eterm], ctx: &mut MinimalContext) -> Result<Eterm> {
    let mut machine = Machine::new();
    machine.run_export(module, name(module, function), args.len() as u32, args, ctx)
}

/// Run an export and report whether it returned or blocked.
fn run_outcome(
    module: &Module,
    function: &str,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<RunOutcome> {
    let mut machine = Machine::new();
    machine.run_export_outcome(module, name(module, function), args.len() as u32, args, ctx)
}

/// The reason atom of a typed engine raise.
fn reason(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .expect("the engine's unsupported error carries an atom reason")
            .to_string(),
        other => panic!("expected a raise, got {other:?}"),
    }
}

/// The fixture's exports and their oracle values, for the smoke check below.
const EXPORTS: [(&str, u32); 7] = [
    ("wait", 0),
    ("bomb", 1),
    ("sel", 0),
    ("afterzero", 0),
    ("scan", 0),
    ("aftervar", 1),
    ("marked", 0),
];

#[test]
fn the_fixture_has_every_receive_export() {
    let (module, _) = load();
    for (function, arity) in EXPORTS {
        assert!(
            module.export(name(&module, function), arity).is_some(),
            "{function}/{arity} must be exported"
        );
    }
}

// ---------------------------------------------------------------------------
// send + the plain `receive`
// ---------------------------------------------------------------------------

#[test]
fn bomb_delivers_the_message_and_returns_ok() {
    // Oracle: `Self=self(), recv:bomb(Self), receive M -> M end` → 42.
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());

    let result = run(&module, "bomb", &[make_small(0)], &mut ctx).expect("bomb/1");

    assert_eq!(result, make_atom(name(&module, "ok")), "`Pid ! 42, ok` returns ok");
    assert_eq!(mailbox.messages(), vec![make_small(42)], "the message was delivered");
}

#[test]
fn wait_returns_the_queued_message_and_removes_it() {
    // Oracle: `Self=self(), Self ! hello, recv:wait()` → hello.
    let (mut module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    // `hello` is not an atom the fixture names, so intern it in the *same* table the
    // run uses (an atom index from a foreign table would mean a different atom).
    let hello = make_atom(module.atom_table.intern("hello"));
    mailbox.push(hello);
    let mut ctx = context(&module, mailbox.clone());

    let result = run(&module, "wait", &[], &mut ctx).expect("wait/0");

    assert_eq!(result, hello, "the first message is the receive result");
    assert!(mailbox.messages().is_empty(), "remove_message unlinked it");
}

#[test]
fn sel_binds_the_catch_all_clause() {
    // Oracle: `Self=self(), Self ! 99, recv:sel()` → 99.
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    mailbox.push(make_small(99));
    let mut ctx = context(&module, mailbox.clone());

    let result = run(&module, "sel", &[], &mut ctx).expect("sel/0");

    assert_eq!(result, make_small(99));
    assert!(mailbox.messages().is_empty());
}

#[test]
fn sel_matches_the_literal_clause() {
    // Oracle: `Self=self(), Self ! foo, recv:sel()` → a.
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    mailbox.push(make_atom(name(&module, "foo")));
    let mut ctx = context(&module, mailbox.clone());

    let result = run(&module, "sel", &[], &mut ctx).expect("sel/0");

    assert_eq!(result, make_atom(name(&module, "a")));
    assert!(mailbox.messages().is_empty());
}

#[test]
fn scan_skips_a_non_matching_head_before_matching() {
    // Oracle: `Self=self(), Self ! 42, Self ! {bar,7}, recv:scan()` → 7.
    // `42` fails both clauses and reaches `loop_rec_end`; `{bar,7}` then matches.
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());
    // Build `{bar, 7}` in the **run's own** context: the term must point into the
    // heap (arena) that outlives the run, never a scratch context's.
    let bar = make_atom(name(&module, "bar"));
    let pair = beam_engine::terms::make_tuple(&mut ctx, &[bar, make_small(7)]).expect("tuple");
    mailbox.push(make_small(42));
    mailbox.push(pair);

    let result = run(&module, "scan", &[], &mut ctx).expect("scan/0");

    assert_eq!(result, make_small(7), "the second message matched");
    assert_eq!(
        mailbox.messages(),
        vec![make_small(42)],
        "only the matched message was removed; the scanned-past head remains"
    );
}

#[test]
fn scan_matches_the_literal_head() {
    // Oracle: `Self=self(), Self ! foo, recv:scan()` → foo.
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    mailbox.push(make_atom(name(&module, "foo")));
    let mut ctx = context(&module, mailbox.clone());

    let result = run(&module, "scan", &[], &mut ctx).expect("scan/0");

    assert_eq!(result, make_atom(name(&module, "foo")));
    assert!(mailbox.messages().is_empty());
}

// ---------------------------------------------------------------------------
// `after` handling
// ---------------------------------------------------------------------------

#[test]
fn afterzero_returns_empty_on_an_empty_mailbox() {
    // Oracle: `recv:afterzero()` (empty mailbox) → empty.
    // `after 0` compiles to a direct jump to the `timeout` opcode, which resets the
    // scan position.
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());

    let result = run(&module, "afterzero", &[], &mut ctx).expect("afterzero/0");

    assert_eq!(result, make_atom(name(&module, "empty")));
}

#[test]
fn aftervar_with_a_runtime_zero_timeout_does_not_suspend() {
    // Oracle: `recv:aftervar(0)` (empty mailbox) → empty.
    // The `0` is a *runtime* value here, so it reaches the `wait_timeout` opcode,
    // which must fall through to `timeout` instead of suspending.
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());

    let result = run(&module, "aftervar", &[make_small(0)], &mut ctx).expect("aftervar(0)");

    assert_eq!(result, make_atom(name(&module, "empty")));
}

#[test]
fn aftervar_matches_a_queued_message_regardless_of_the_timeout() {
    // Oracle: `Self=self(), Self ! foo, recv:aftervar(0)` → foo.
    // (The message path does not depend on the timeout at all.)
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    mailbox.push(make_atom(name(&module, "foo")));
    let mut ctx = context(&module, mailbox.clone());

    let result = run(&module, "aftervar", &[make_small(0)], &mut ctx).expect("aftervar(0)");

    assert_eq!(result, make_atom(name(&module, "foo")));
}

#[test]
fn aftervar_with_a_nonzero_timeout_suspends() {
    // DOCUMENTED GAP: the oracle returns `empty` after 5 ms
    // (`erl -noshell -eval 'recv:aftervar(5)'` → empty), but Milestone 1 has no host
    // timer, so a non-zero timeout suspends until a message arrives. Reporting
    // `RunOutcome::Wait` is honest (no wrong result); the timer is the host's job.
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());

    let outcome = run_outcome(&module, "aftervar", &[make_small(5)], &mut ctx).expect("aftervar(5)");

    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "a non-zero timeout with no message blocks, got {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// blocking + resumption
// ---------------------------------------------------------------------------

#[test]
fn wait_on_an_empty_mailbox_reports_wait_and_the_plain_entry_is_a_typed_error() {
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());

    let outcome = run_outcome(&module, "wait", &[], &mut ctx).expect("wait/0");
    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "an empty mailbox suspends the receive, got {outcome:?}"
    );

    // The non-blocking entry folds the suspension into the documented typed error.
    let mut ctx = context(&module, TestMailbox::new(make_atom(undefined)));
    let error = run(&module, "wait", &[], &mut ctx).expect_err("run_export is non-blocking");
    assert_eq!(
        reason(error),
        "unsupported_opcode: run: process blocked (no message available)"
    );
}

#[test]
fn a_suspended_receive_resumes_when_a_message_arrives() {
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&module, name(&module, "wait"), 0, &[], &mut ctx)
        .expect("wait/0");
    let (func, pc) = match outcome {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("expected a wait, got {other:?}"),
    };
    // The suspension must leave the whole call-frame stack intact: the bottom frame
    // (`run_label_outcome` pushes it) plus `wait/0`'s own `allocate 0` frame, which
    // the `deallocate 0` after `remove_message` would pop on the matching path.
    // A depth of 1 here would mean the blocked receive had lost its frame.
    assert_eq!(machine.frame_depth(), 2, "the suspended receive keeps its frame");

    mailbox.push(make_small(42));
    let resumed = machine
        .resume_outcome(&module, func, pc, &mut ctx)
        .expect("resume at the wait target");

    assert_eq!(resumed, RunOutcome::Returned(make_small(42)));
    assert!(
        mailbox.messages().is_empty(),
        "the resumed receive removed the matched message"
    );
}

// ---------------------------------------------------------------------------
// the outcome-aware tier entries
// ---------------------------------------------------------------------------

#[test]
fn the_baseline_tier_reports_the_same_block_as_the_interpreter() {
    // The tier-selecting, blocking-aware entry over the committed real fixture: the
    // native baseline tier must report the interpreter's exact suspension (the `wait`
    // instruction is not one the emitter inlines, so it deopts to the shared
    // interpreter-backed helper, whose `WAIT` word the driver decodes back into a
    // `RunOutcome::Wait` with the interpreter's resume address).
    let (module, undefined) = load();
    let wait = name(&module, "wait");

    let mut interpreter = Machine::new();
    let mut ctx = context(&module, TestMailbox::new(make_atom(undefined)));
    let expected = interpreter
        .run_export_outcome_with_tier(&module, wait, 0, &[], &mut ctx, Tier::Interpreter)
        .expect("interpreter run of wait/0");
    assert!(matches!(expected, RunOutcome::Wait { .. }));

    let mailbox = TestMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());
    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome_with_tier(&module, wait, 0, &[], &mut ctx, Tier::BaselineJit)
        .expect("baseline run of wait/0");
    assert_eq!(
        outcome, expected,
        "the native tier must report the interpreter's exact suspension"
    );

    // The suspension left the machine resumable: a message arrives and the same
    // machine continues to a value.
    let (func, pc) = match outcome {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("expected a wait, got {other:?}"),
    };
    mailbox.push(make_small(42));
    assert_eq!(
        machine
            .resume_outcome(&module, func, pc, &mut ctx)
            .expect("resume the JIT suspension"),
        RunOutcome::Returned(make_small(42))
    );
}

#[test]
fn the_label_entry_reports_the_same_block_on_both_tiers() {
    // The label-addressed tier entry, over the same fixture: the export's entry label
    // is resolved by both tiers to the same address, so the suspension agrees too.
    let (module, undefined) = load();
    let label = module
        .export(name(&module, "wait"), 0)
        .expect("the fixture exports wait/0");

    let mut interpreter = Machine::new();
    let mut ctx = context(&module, TestMailbox::new(make_atom(undefined)));
    let expected = interpreter
        .run_label_outcome_with_tier(&module, label, 0, &[], &mut ctx, Tier::Interpreter)
        .expect("interpreter run");

    let mut baseline = Machine::new();
    let mut ctx = context(&module, TestMailbox::new(make_atom(undefined)));
    let outcome = baseline
        .run_label_outcome_with_tier(&module, label, 0, &[], &mut ctx, Tier::BaselineJit)
        .expect("baseline run");

    assert_eq!(outcome, expected);
}

// ---------------------------------------------------------------------------
// the `recv_marker_*` prologue
// ---------------------------------------------------------------------------
#[test]
fn marked_runs_the_recv_marker_prologue() {
    // Oracle: `recv:marked()` → 7.
    // `marked/0` reserves a marker (with an empty queue, so C yields `undefined`),
    // binds it to a freshly created reference, uses and clears it, sends
    // `{Ref, 7}` to itself and matches it out of the mailbox.
    let (module, undefined) = load();
    let mailbox = TestMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());

    let result = run(&module, "marked", &[], &mut ctx).expect("marked/0");

    assert_eq!(result, make_small(7));
    assert!(mailbox.messages().is_empty());
}

#[test]
fn the_mailbox_repositions_the_scan_at_a_marker() {
    // The seam's contract, exercised directly: a marker reserved with a non-empty
    // queue records the scan position, and `receive_marker_use` jumps the scan back
    // to it. `recv_marker_reserve` on an empty queue yields the C `undefined`.
    let (_, undefined) = load();
    let undefined = make_atom(undefined);
    let mut mailbox = TestMailbox::new(undefined);

    assert_eq!(
        mailbox.receive_marker_reserve().unwrap(),
        undefined,
        "an empty queue has no position to record"
    );

    mailbox.push(make_small(1));
    mailbox.receive_skip().unwrap();
    let marker = mailbox.receive_marker_reserve().unwrap();
    mailbox.receive_marker_bind(marker, make_small(999)).unwrap();
    mailbox.push(make_small(2));

    mailbox.receive_reset().unwrap();
    assert_eq!(mailbox.receive_next().unwrap(), Some(make_small(1)));
    mailbox.receive_marker_use(make_small(999)).unwrap();
    assert_eq!(
        mailbox.receive_next().unwrap(),
        Some(make_small(2)),
        "the marker moved the scan past the first message"
    );

    mailbox.receive_marker_clear(make_small(999)).unwrap();
    mailbox.receive_reset().unwrap();
    mailbox.receive_marker_use(make_small(999)).unwrap();
    assert_eq!(
        mailbox.receive_next().unwrap(),
        Some(make_small(1)),
        "a cleared marker no longer repositions the scan"
    );
}

// ---------------------------------------------------------------------------
// the documented unsupported paths
// ---------------------------------------------------------------------------

#[test]
fn a_context_without_a_mailbox_reports_the_typed_unsupported() {
    // With no mailbox installed, every receive-seam call is the trait's typed
    // `unsupported` default — never a panic and never a wrong result.
    let (module, _) = load();
    let mut ctx = MinimalContext::new();

    let error = run(&module, "wait", &[], &mut ctx).expect_err("no mailbox installed");

    assert_eq!(reason(error), "unsupported_opcode: receive_next");
}

#[test]
fn a_missing_export_is_still_a_typed_error() {
    let (mut module, undefined) = load();
    // An atom the fixture does not export (interned so the lookup is well-defined).
    let nope = module.atom_table.intern("nope");
    let mut ctx = context(&module, TestMailbox::new(make_atom(undefined)));
    let mut machine = Machine::new();

    let error = machine
        .run_export_outcome(&module, nope, 0, &[], &mut ctx)
        .expect_err("no such export");

    assert_eq!(reason(error), "unsupported_opcode: run_export: export not found");
}
