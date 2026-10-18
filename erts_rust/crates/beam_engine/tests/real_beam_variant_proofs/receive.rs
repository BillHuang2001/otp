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

//! **Receive/message variants on real compiler output**: `Send`, `LoopRec`,
//! `RemoveMessage`, `Wait`, `Timeout` (+ `WaitTimeout`), `RecvMarkerReserve`,
//! `RecvMarkerBind`, `RecvMarkerUse`, `RecvMarkerClear`.
//!
//! # The source module
//!
//! Every row drives the committed `erlc` fixture `tests/fixtures/recv.beam`
//! (`recv.erl`, built with the system `erlc +deterministic`; regeneration recipe
//! in `tests/fixtures/README.md`). `beam_disasm:file/1` on the committed binary
//! shows the OTP compiler emitting exactly the shapes this file proves:
//!
//! ```text
//! bomb/1      send                                                     (Send)
//! wait/0      loop_rec / remove_message / … label: wait                (LoopRec, RemoveMessage, Wait)
//! afterzero/0 loop_rec → label: timeout                               (Timeout)
//! aftervar/1  loop_rec / loop_rec_end / wait_timeout / timeout        (WaitTimeout, Timeout)
//! marked/0    recv_marker_reserve … recv_marker_bind … recv_marker_use
//!             … recv_marker_clear                                     (all four RecvMarker*)
//! ```
//!
//! # Rows
//!
//! | export | variants proven | oracle expression | value |
//! |---|---|---|---|
//! | `bomb/1` | `Send` | `begin Self=self(), recv:bomb(Self), receive M -> M end end` | `42` (plus `ok` returned) |
//! | `wait/0`, message queued | `LoopRec`, `RemoveMessage` | `begin Self=self(), Self ! hello, recv:wait() end` | `hello` |
//! | `wait/0`, empty queue | `LoopRec`, `Wait` (then `LoopRec`, `RemoveMessage` on the resume) | `begin Self=self(), Self ! hello, recv:wait() end` | `hello` |
//! | `afterzero/0`, empty queue | `Timeout` | `recv:afterzero()` | `empty` |
//! | `aftervar/1` = `0`, empty queue | `WaitTimeout`, `Timeout` | `recv:aftervar(0)` | `empty` |
//! | `marked/0`, one junk message queued | `RecvMarkerReserve`, `RecvMarkerBind`, `RecvMarkerUse`, `RecvMarkerClear` | `begin Self=self(), Self ! junk, recv:marked() end` | `7` |
//!
//! # Why these rows carry their own harness plumbing
//!
//! The receive family runs only against a **mailbox**, and the shared
//! [`ModuleHost`](crate::support::ModuleHost) (whose `call`/`run` the other family
//! files use) carries none — and `tests/support` is read-only for this suite. So
//! each row here wraps its own run in [`harness::measure`] (the raw census fence:
//! reset → enable → run → snapshot → disable) and builds the
//! [`Proof`] from the returned `(value, census)` pair, which keeps
//! [`Proof::require`]/[`Proof::report`]/[`Proof::value`] available unchanged.
//!
//! The context is a [`MinimalContext`] over the fixture's own heap, with two things
//! installed — exactly the seam `tests/receive.rs` drives:
//!
//! * the shared [`ProbeMailbox`] (the `support` harness's `Mailbox`
//!   implementation — reused rather than copied), and
//! * a BIF hook serving the only two `erlang` BIFs the fixture calls: `self/0`
//!   (the destination of `marked/0`'s send) and `make_ref/0` (the reference
//!   `marked/0` matches on; a fresh small integer stands in, exactly as
//!   `tests/receive.rs` does — the fixture only ever compares it for equality).
//!
//! Nothing about the *module* is synthetic: `recv.beam` is the system compiler's
//! output and is loaded with [`load_file`](beam_loader::load_file), so the census
//! counts belong to real compiled bytes. Only the host around it (mailbox + two
//! BIFs) is test plumbing, which no engine contract forbids: the mailbox *is* the
//! `Mailbox` seam's host side.
//!
//! # Honest notes (not divergent results)
//!
//! * A `receive` that finds no message **suspends**: `run_export_outcome` reports
//!   [`RunOutcome::Wait`] and `resume_outcome` re-enters at the reported address,
//!   which is what the `Wait` row proves (with the message delivered at the park
//!   point and the resumed run removing it). The plain `run_export` folds that
//!   suspension into the typed `blocked` error, which is why only the suspending row
//!   uses the outcome-aware entry.
//! * A **non-zero** `after` on an empty mailbox also suspends (`aftervar(5)`); the
//!   engine publishes the deadline through `Machine::wait_deadline` and the host
//!   owns arming the timer. The mock arms none, so this file proves the timeout
//!   *opcodes* on the zero-delay routes (`afterzero/0`, `aftervar(0)`) whose oracle
//!   values a mock can reproduce exactly, rather than pretending to fire a timer.
//! * `aftervar(0)` is the interesting `Timeout` route: the compiled code reaches
//!   `wait_timeout` with a **runtime** zero, which falls through to the `timeout`
//!   continuation instead of suspending — so that row proves `WaitTimeout` and
//!   `Timeout` together.
//!
//! `Send` is proven twice over: `bomb/1` returns `ok` like the oracle's
//! `recv:bomb(self())`, *and* the `42` it sent is observable in the mailbox exactly
//! as the oracle's `receive M -> M end` observes it.

use beam_common::Result;
use beam_engine::{Machine, MinimalContext, RunOutcome};
use beam_loader::{load_file, Module};
use beam_term::{
    atom_val, is_atom, is_nil, is_small, make_atom, make_small, signed_value, Atom, Eterm,
};

use crate::harness::{fixtures_dir, measure, oracle_call, oracle_w, skip_if_unavailable, Proof};
use crate::support::ProbeMailbox;

// ---------------------------------------------------------------------------
// The fixture + the mailbox-bearing context
// ---------------------------------------------------------------------------

/// Loads the committed receive fixture and interns the `undefined` atom the
/// mailbox needs as its "no marker to record" term.
///
/// The atom is interned in the **module's own** table (the table the run itself
/// resolves names through), so the marker term is a canonical one.
fn load() -> (Module, Atom) {
    let path = fixtures_dir().join("recv.beam");
    let mut module = load_file(&path).unwrap_or_else(|e| panic!("load {path:?}: {e:?}"));
    let undefined = module.atom_table.intern("undefined");
    (module, undefined)
}

/// `module:function`'s name atom.
fn atom(module: &Module, function: &str) -> Atom {
    module
        .atom_table
        .lookup(function)
        .unwrap_or_else(|| panic!("atom {function:?} not interned in recv.beam"))
}

/// A [`MinimalContext`] with `mailbox` installed and the fixture's two `erlang`
/// BIFs (`self/0`, `make_ref/0`) served — see the module docs.
///
/// The two BIFs are the whole surface `recv.beam` imports: `self/0` is
/// `marked/0`'s send destination and `make_ref/0` the reference it matches on.
/// Every other BIF is the typed `BadArg`, so an unexpected call fails loudly
/// instead of silently succeeding.
fn context(module: &Module, mailbox: ProbeMailbox) -> MinimalContext {
    let self_atom = atom(module, "self");
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
        Err(beam_common::BeamError::BadArg)
    }));
    ctx.set_mailbox(Box::new(mailbox));
    ctx
}

// ---------------------------------------------------------------------------
// The proof helpers (the census fence around our own context)
// ---------------------------------------------------------------------------

/// Runs `function/args.len()` of the real compiled module to a value on `ctx`,
/// with the execution census enabled over exactly that run.
///
/// This is [`harness::run`](crate::harness::run)'s proof, minus the
/// `ModuleHost` — the raw [`measure`] fence plus a `MinimalContext` that has a
/// mailbox (`harness::run` cannot be used: `ModuleHost` carries no mailbox).
fn prove(module: &Module, function: &str, args: &[Eterm], ctx: &mut MinimalContext) -> Proof {
    let function = atom(module, function);
    let (value, census) = measure(|| {
        let mut machine = Machine::new();
        machine.run_export(module, function, args.len() as u32, args, ctx)
    });
    Proof { value, census }
}

/// The suspending form of [`prove`]: runs an export that parks in a `receive`,
/// delivers `message` at the park point, and resumes the **same** machine to its
/// value — all inside one [`measure`] fence, so the census covers the suspend
/// (`wait`/`wait_timeout`) and the resume together.
///
/// Panics (loudly, with the outcome) if the run does not suspend: this helper is
/// only for the rows that prove `Wait`.
fn prove_after_suspension(
    module: &Module,
    function: &str,
    ctx: &mut MinimalContext,
    mailbox: &ProbeMailbox,
    message: Eterm,
) -> Proof {
    let function_atom = atom(module, function);
    let (outcome, census) = measure(|| -> Result<Eterm> {
        let mut machine = Machine::new();
        let outcome = machine.run_export_outcome(module, function_atom, 0, &[], ctx)?;
        let (func, pc) = match outcome {
            RunOutcome::Wait { func, pc } => (func, pc),
            other => panic!("expected a suspended receive from {function}, got {other:?}"),
        };
        // A suspension must leave the whole call-frame stack intact: the bottom
        // frame plus the export's own frame. A shallower stack would mean the
        // parked receive had lost its frame and could not resume correctly.
        assert_eq!(
            machine.frame_depth(),
            2,
            "the suspended receive keeps its frame"
        );
        mailbox.push(message);
        match machine.resume_outcome(module, func, pc, ctx)? {
            RunOutcome::Returned(value) => Ok(value),
            other => panic!("expected the resumed receive to return, got {other:?}"),
        }
    });
    Proof {
        value: outcome,
        census,
    }
}

// ---------------------------------------------------------------------------
// Rendering (the oracle's `~w` notation)
// ---------------------------------------------------------------------------

/// Renders a value in the oracle's `~w` notation, so a row can compare it with
/// [`oracle_w`] by string equality.
///
/// [`harness::render`](crate::harness::render) renders through a
/// [`ModuleHost`](crate::support::ModuleHost)'s shared atom table; these rows run
/// on a `MinimalContext` over the **fixture's own** atom table, so the same
/// rendering is done here for the shapes the receive rows observe — `[]`, small
/// integers and atoms (named through `module.atom_table`, the table the run
/// itself resolves names through). Anything else panics loudly: a row must assert
/// a committed oracle value rather than compare two placeholders.
fn show(module: &Module, term: Eterm) -> String {
    if is_nil(term) {
        return "[]".into();
    }
    if is_small(term) {
        return signed_value(term).to_string();
    }
    if is_atom(term) {
        let atom = atom_val(term);
        let name = module.atom_table.name(atom).unwrap_or_else(|| {
            panic!("the run returned atom {atom:?}, which recv.beam does not name")
        });
        return show_atom_name(&name);
    }
    panic!("this file's renderer does not cover {term:?}; assert a committed oracle value instead");
}

/// An atom name in `~w` notation: bare when it lexes as an unquoted atom, quoted
/// (with the embedded quote escaped) otherwise.
fn show_atom_name(name: &str) -> String {
    let plain = !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@');
    if plain {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\'', "\\'"))
    }
}

// ---------------------------------------------------------------------------
// `send`
// ---------------------------------------------------------------------------

/// `Send` — `Pid ! 42` in `bomb/1` (destination in `x(0)`, message in `x(1)`).
///
/// Oracle (two observables, both checked):
///
/// ```sh
/// erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[recv:bomb(self())]), halt(0).'
/// # -> ok
/// erl -noshell -pz tests/fixtures \
///   -eval 'io:format("~w~n",[begin Self=self(), recv:bomb(Self), receive M -> M end end]), halt(0).'
/// # -> 42
/// ```
///
/// The first is the export's own return value; the second is the message the send
/// delivered, which is exactly what this row reads out of the mailbox afterwards.
///
/// Observed census: `Send = 1` (`total_executed = 7`, `total_errors = 0`).
#[test]
fn send_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let (module, undefined) = load();
    let mailbox = ProbeMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());
    // The oracle's `recv:bomb(self())`: the destination is an immediate (the send
    // seam ignores it — the mailbox is this process's), the message is the `42`.
    let self_term = make_atom(atom(&module, "self"));

    let proof = prove(&module, "bomb", &[self_term], &mut ctx);

    proof.require(&["Send"]);
    assert_eq!(
        show(&module, proof.value()),
        oracle_call("recv", "bomb", "self()"),
        "`Pid ! 42, ok` returns ok"
    );
    let delivered = mailbox.messages();
    assert_eq!(
        delivered.len(),
        1,
        "the send delivered exactly one message\n{}",
        proof.report()
    );
    assert_eq!(
        show(&module, delivered[0]),
        oracle_w("begin Self=self(), recv:bomb(Self), receive M -> M end end"),
        "the delivered message is the one the oracle's receive observes"
    );
}

// ---------------------------------------------------------------------------
// `loop_rec` + `remove_message`
// ---------------------------------------------------------------------------

/// `LoopRec` + `RemoveMessage` — the matching path of `wait/0`, whose compiled
/// body is `loop_rec {f,4},{x,0}` / `remove_message` / `deallocate` / `return`.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval
/// 'io:format("~w~n",[begin Self=self(), Self ! hello, recv:wait() end]), halt(0).'`
/// → `hello`.
///
/// `hello` is not an atom `recv.beam` names, so it is interned into the module's
/// own table first (an atom from a *different* table would be a different atom).
///
/// Observed census: `LoopRec = 1`, `RemoveMessage = 1`
/// (`total_executed = 6`, `total_errors = 0`).
#[test]
fn loop_rec_and_remove_message_execute_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let (mut module, undefined) = load();
    let hello = make_atom(module.atom_table.intern("hello"));
    let mailbox = ProbeMailbox::with_messages(make_atom(undefined), vec![hello]);
    let mut ctx = context(&module, mailbox.clone());

    let proof = prove(&module, "wait", &[], &mut ctx);

    proof.require(&["LoopRec", "RemoveMessage"]);
    assert_eq!(
        show(&module, proof.value()),
        oracle_w("begin Self=self(), Self ! hello, recv:wait() end"),
        "the queued message is the receive's result"
    );
    assert!(
        mailbox.messages().is_empty(),
        "remove_message unlinked the matched message\n{}",
        proof.report()
    );
}

// ---------------------------------------------------------------------------
// `wait` (suspend + resume)
// ---------------------------------------------------------------------------

/// `Wait` — the same `wait/0`, on an **empty** mailbox: `loop_rec` jumps to
/// `{wait,{f,3}}`, the run reports [`RunOutcome::Wait`], and the delivered message
/// wakes it so the resume re-enters at the compiled `loop_rec` and removes it.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval
/// 'io:format("~w~n",[begin Self=self(), Self ! hello, recv:wait() end]), halt(0).'`
/// → `hello`.
///
/// That one expression is the whole process: in the oracle the send and the
/// receive are the same process, so the message is already queued; here the
/// mailbox is empty when the run enters the receive (proving the suspension) and
/// the message is pushed at the park point (proving the resume). The observable —
/// the value the receive returns with the message removed — is identical.
///
/// Observed census (suspend + resume in one fence): `Wait = 1`, `LoopRec = 2`
/// (once before parking, once on the resume), `RemoveMessage = 1`
/// (`total_executed = 10`, `total_errors = 0`).
#[test]
fn wait_suspends_and_the_resumed_run_returns_the_message() {
    if skip_if_unavailable() {
        return;
    }
    let (mut module, undefined) = load();
    let hello = make_atom(module.atom_table.intern("hello"));
    let mailbox = ProbeMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());

    let proof = prove_after_suspension(&module, "wait", &mut ctx, &mailbox, hello);

    proof.require(&["LoopRec", "Wait", "RemoveMessage"]);
    assert_eq!(
        show(&module, proof.value()),
        oracle_w("begin Self=self(), Self ! hello, recv:wait() end"),
        "the resumed receive returns the delivered message"
    );
    assert!(
        mailbox.messages().is_empty(),
        "the resumed receive removed the matched message\n{}",
        proof.report()
    );
}

// ---------------------------------------------------------------------------
// `timeout` (+ the `wait_timeout` fall-through)
// ---------------------------------------------------------------------------

/// `Timeout` — `afterzero/0`'s `receive _ -> got after 0 -> empty end` on an empty
/// mailbox: `loop_rec` jumps to the `timeout` continuation, which cancels the
/// receive and falls through to `empty`.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval
/// 'io:format("~w~n",[recv:afterzero()]), halt(0).'` → `empty`.
///
/// Observed census: `Timeout = 1` (after `LoopRec = 1` jumped to the timeout
/// label; `total_executed = 7`, `total_errors = 0`).
#[test]
fn timeout_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let (module, undefined) = load();
    let mailbox = ProbeMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());

    let proof = prove(&module, "afterzero", &[], &mut ctx);

    proof.require(&["Timeout"]);
    assert_eq!(
        show(&module, proof.value()),
        oracle_call("recv", "afterzero", ""),
        "`after 0` with an empty mailbox takes the timeout branch"
    );
}

/// `WaitTimeout` → `Timeout` — `aftervar(0)`'s `after T ->` with a **runtime** `0`:
/// the compiled body reaches `wait_timeout {f,23},{y,0}`, which must fall through
/// to the `timeout` continuation instead of suspending.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval
/// 'io:format("~w~n",[recv:aftervar(0)]), halt(0).'` → `empty`.
///
/// A **non-zero** delay on an empty mailbox suspends instead (`aftervar(5)` parks
/// with a published deadline); that is the host-timer route this mock does not
/// arm, documented in the module docs rather than faked here.
///
/// Observed census: `WaitTimeout = 1`, `Timeout = 1` (`total_executed = 10`,
/// `total_errors = 0`).
#[test]
fn wait_timeout_falls_through_to_timeout_on_a_zero_delay() {
    if skip_if_unavailable() {
        return;
    }
    let (module, undefined) = load();
    let mailbox = ProbeMailbox::new(make_atom(undefined));
    let mut ctx = context(&module, mailbox.clone());

    let proof = prove(&module, "aftervar", &[make_small(0)], &mut ctx);

    proof.require(&["WaitTimeout", "Timeout"]);
    assert_eq!(
        show(&module, proof.value()),
        oracle_call("recv", "aftervar", "0"),
        "a runtime zero delay takes the timeout branch without suspending"
    );
}

// ---------------------------------------------------------------------------
// the `recv_marker_*` prologue
// ---------------------------------------------------------------------------

/// The four `recv_marker_*` instructions — `marked/0`'s alias-based receive:
/// `Ref = make_ref(), self() ! {Ref, 7}, receive {Ref, Reply} -> Reply end`
/// compiles to `recv_marker_reserve` / `recv_marker_bind` … `recv_marker_use`
/// … `recv_marker_clear` around the ordinary `send`/`loop_rec`/`remove_message`.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval
/// 'io:format("~w~n",[begin Self=self(), Self ! junk, recv:marked() end]), halt(0).'`
/// → `7`.
///
/// One `junk` message is queued **before** the run so `recv_marker_reserve` has a
/// live queue position to record: the marker is then a real marker id (the C's
/// non-`undefined` case) and `recv_marker_bind` takes its live branch. The run
/// scans past `junk` (`loop_rec_end`), matches `{Ref,7}`, clears the marker and
/// removes the matched message — leaving `junk` queued, exactly as the oracle's
/// receive does. (On an empty queue the same prologue runs with the C's
/// `undefined` no-op marker, which `tests/receive.rs` covers.)
///
/// Observed census: `RecvMarkerReserve = 1`, `RecvMarkerBind = 1`,
/// `RecvMarkerUse = 1`, `RecvMarkerClear = 1` (plus the surrounding
/// `Send = 1`, `LoopRec = 2`, `LoopRecEnd = 1`, `RemoveMessage = 1`;
/// `total_executed = 29`, `total_errors = 0`).
#[test]
fn recv_markers_execute_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let (mut module, undefined) = load();
    let junk = make_atom(module.atom_table.intern("junk"));
    let mailbox = ProbeMailbox::with_messages(make_atom(undefined), vec![junk]);
    let mut ctx = context(&module, mailbox.clone());

    let proof = prove(&module, "marked", &[], &mut ctx);

    proof.require(&[
        "RecvMarkerReserve",
        "RecvMarkerBind",
        "RecvMarkerUse",
        "RecvMarkerClear",
    ]);
    assert_eq!(
        show(&module, proof.value()),
        oracle_w("begin Self=self(), Self ! junk, recv:marked() end"),
        "the reference-matched message is the receive's result"
    );
    assert_eq!(
        mailbox.messages(),
        vec![junk],
        "only the matched message was removed; the scanned-past junk remains\n{}",
        proof.report()
    );
}
