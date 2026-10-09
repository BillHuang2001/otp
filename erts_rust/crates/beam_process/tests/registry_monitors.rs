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

//! Integration evidence for the **process-control mechanisms** the `spawn*`/`link`/`monitor`/
//! `register`/`process_info` BIF families are built on: the local name registry, liveness and
//! exit-reason observability, a monitor of a dead/never-existing target, a monitor of a
//! registered **name**, and the flushed demonitor.
//!
//! Everything here is stated against the real system Erlang's observable behaviour (OTP 29.1.1 /
//! erts-17.1), and the `oracle_*` tests additionally drive the oracle itself (see below), because
//! the crate-level test module cannot cross-check the `erl` runtime.
//!
//! # Protocol (copied from `tests/dict_oracle.rs`)
//!
//! Each `oracle_*` check runs the equivalent Erlang expression under the system `erl`
//! (`-noshell -eval '<expr>, halt(0).'`, printing its observables with `io:format`) and asserts
//! the printed result. An expression is written to be **self-consistent** where the host's node
//! name is involved (it prints a comparison against `node()` rather than a literal node name).
//!
//! # Absence of the oracle is not a failure
//!
//! If `erl` is not on `PATH` (or does not answer within [`ERL_TIMEOUT`]) every oracle check
//! **skips loudly** (a clear `eprintln!`) and returns, never failing the suite.

use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_common::BeamError;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{
    is_immediate, make_atom, make_small, tuple_element, tuple_size, Atom, AtomTable, Eterm,
};

/// Wall-clock budget for a single `erl` invocation before the oracle is declared unusable and
/// the affected check skips.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// The oracle driver (text protocol)
// ---------------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout.
///
/// Returns `None` — after printing a clear skip message — when `erl` cannot be spawned or does
/// not answer within [`ERL_TIMEOUT`]. A *spawned* `erl` that exits non-zero is a bug in the
/// embedded expression, so that is asserted rather than skipped.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    let code_for_thread = code.clone();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code_for_thread)
            .output();
        // The receiver may have timed out and gone away; that is fine.
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({e}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "`erl` exited with {} for expression:\n{code}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// Runs `expr` under the oracle and asserts the printed result is `expected`.
///
/// Returns `false` (after the driver's loud skip message) when the oracle is unusable, so a
/// caller can `return` early; an oracle that *answers with something else* is a failure.
fn expect_oracle(expr: &str, expected: &str) -> bool {
    let Some(out) = erl_stdout(expr) else {
        return false;
    };
    assert_eq!(
        out.trim(),
        expected,
        "the oracle disagrees with the expected result for:\n{expr}"
    );
    true
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A process table plus the atom table its [`SignalAtoms`] were interned from, so a test can
/// resolve the well-known message atoms (`'DOWN'`, `process`, `noproc`, ...) back to terms.
struct Fixture {
    table: ProcessTable,
    names: AtomTable,
}

impl Fixture {
    /// An empty table whose atoms come from a fresh table the fixture keeps.
    fn new() -> Fixture {
        let mut names = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut names);
        Fixture {
            table: ProcessTable::new(atoms),
            names,
        }
    }

    /// The interned atom `name` (the same index space the table's signal atoms use).
    fn atom(&mut self, name: &str) -> Atom {
        self.names.intern(name)
    }

    /// The atom `name` as a term.
    fn atom_term(&mut self, name: &str) -> Eterm {
        make_atom(self.atom(name))
    }

    /// Builds a tuple on `pid`'s own heap (a *structured* term for the process that owns it).
    fn tuple_in(&mut self, pid: ProcessId, elems: &[Eterm]) -> Eterm {
        self.table
            .get_mut(pid)
            .expect("the holder is live")
            .heap_mut()
            .tuple(elems)
            .expect("the tuple fits the holder's young heap")
    }

    /// The payload of `pid`'s single queued message (panics unless exactly one is queued).
    fn sole_payload(&self, pid: ProcessId) -> Eterm {
        let mailbox = self.table.get(pid).expect("the recipient is live").mailbox();
        assert_eq!(mailbox.len(), 1, "exactly one message is queued");
        mailbox.peek().expect("one message is queued").payload()
    }

    /// Removes and discards the head of `pid`'s mailbox (the tests that queue several messages
    /// consume them one at a time).
    fn drain_one(&mut self, pid: ProcessId) {
        let drained = self
            .table
            .get_mut(pid)
            .expect("the recipient is live")
            .mailbox_mut()
            .dequeue();
        assert!(drained.is_some(), "a message was queued");
    }
}

/// A monitor reference the fixture's table has never handed out.
///
/// [`RefAllocator`] is public but its ids are private to it, so a caller can only *skip* past the
/// ids a table could have used: the allocator starts at `0`, and the caller is expected to pass a
/// table that has created no monitors (then no id is in use).
fn reference_beyond_local_table() -> MonitorRef {
    let mut refs = RefAllocator::new();
    let mut reference = refs.next();
    for _ in 0..100 {
        reference = refs.next();
    }
    reference
}

/// Asserts `payload` is the 5-tuple `{'DOWN', <a ref>, tag, target, reason}` and returns the
/// reason element (element 4).
///
/// `down_atom`/`tag_atom` must be the interned `'DOWN'`/`process` atoms of the fixture.
fn assert_pid_down(
    payload: Eterm,
    down_atom: Atom,
    tag_atom: Atom,
    target: Eterm,
    reason: Eterm,
) {
    // SAFETY: `down_atom`/`tag_atom` come from the same table that interned the message's atoms,
    // and `payload` is the live tuple the table built on the recipient's heap; the five element
    // slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5, "a DOWN message is a 5-tuple");
        assert_eq!(tuple_element(payload, 0), make_atom(down_atom));
        assert_eq!(tuple_element(payload, 2), make_atom(tag_atom));
        assert_eq!(tuple_element(payload, 3), target);
        assert_eq!(tuple_element(payload, 4), reason);
    }
}

// ---------------------------------------------------------------------------
// The name registry
// ---------------------------------------------------------------------------

#[test]
fn register_rejects_a_dead_pid_and_a_taken_name() {
    let mut f = Fixture::new();
    let name = f.atom("server");
    let other = f.atom("other");
    let pid = f.table.spawn_default().expect("a live process");
    let pid2 = f.table.spawn_default().expect("another live process");

    f.table.register(name, pid).expect("the name is free");
    assert!(
        matches!(f.table.register(name, pid2), Err(BeamError::BadArg)),
        "a taken name is badarg"
    );
    assert!(matches!(
        f.table.register(other, ProcessId::new(999, 0)),
        Err(BeamError::BadArg)
    ));

    // A pid that has died cannot be registered either: `erlang:register/2` raises badarg.
    let dead = f.table.spawn_default().expect("a process about to die");
    f.table.remove(dead).expect("it was live");
    assert!(!f.table.contains(dead));
    assert!(matches!(
        f.table.register(other, dead),
        Err(BeamError::BadArg)
    ));
    assert_eq!(f.table.whereis(other), None, "nothing was registered");
}

#[test]
fn unregister_of_an_unknown_name_is_badarg() {
    let mut f = Fixture::new();
    let name = f.atom("server");
    let pid = f.table.spawn_default().expect("a live process");
    f.table.register(name, pid).expect("the name is free");

    f.table.unregister(name).expect("it was registered");
    assert!(matches!(
        f.table.unregister(name),
        Err(BeamError::BadArg)
    ));
    let never = f.atom("never_registered");
    assert!(matches!(
        f.table.unregister(never),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn name_of_tracks_registration_and_liveness() {
    let mut f = Fixture::new();
    let name = f.atom("server");
    let pid = f.table.spawn_default().expect("a live process");

    assert_eq!(f.table.name_of(pid), None, "not registered yet");
    f.table.register(name, pid).expect("the name is free");
    assert_eq!(f.table.name_of(pid), Some(name));
    assert_eq!(f.table.name_of(ProcessId::new(999, 0)), None, "unknown pid");

    f.table.unregister(name).expect("it was registered");
    assert_eq!(f.table.name_of(pid), None, "an unregistered pid names nothing");

    // A name whose process died stops resolving in both directions.
    f.table.register(name, pid).expect("free again");
    assert_eq!(f.table.whereis(name), Some(pid));
    f.table.terminate(pid, make_small(1)).expect("pid is live");
    assert_eq!(f.table.whereis(name), None);
    assert_eq!(f.table.name_of(pid), None);
}

#[test]
fn is_process_alive_reflects_the_table() {
    let mut f = Fixture::new();
    let pid = f.table.spawn_default().expect("a live process");
    assert!(f.table.is_process_alive(pid));
    assert!(!f.table.is_process_alive(ProcessId::new(999, 0)));

    assert_eq!(
        f.table.exit_reason(pid).expect("the pid is live"),
        None,
        "a live process has no exit reason"
    );
    f.table.terminate(pid, make_small(3)).expect("the pid is live");
    assert!(!f.table.is_process_alive(pid), "a terminated pid is gone");
    assert!(matches!(
        f.table.exit_reason(pid),
        Err(BeamError::TermNotFound)
    ));
}

// ---------------------------------------------------------------------------
// Monitors on a pid target
// ---------------------------------------------------------------------------

#[test]
fn monitor_of_a_live_target_delivers_the_pid_down() {
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let holder = f.table.spawn_default().expect("a holder process");
    let watcher = f.table.spawn_default().expect("watcher");

    // 1. A `normal` reason: the watcher is not linked, so it survives and receives the DOWN.
    let target = f.table.spawn_default().expect("target");
    f.table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");
    let normal = f.atom_term("normal");
    f.table.terminate(target, normal).expect("target is live");
    assert!(!f.table.contains(target));
    assert!(f.table.contains(watcher));
    let payload = f.sole_payload(watcher);
    assert_pid_down(payload, down, process, target.to_eterm(), normal);
    // Consume it, so the second phase again has exactly one queued message.
    f.drain_one(watcher);
    assert_eq!(f.table.get(watcher).expect("watcher").mailbox().len(), 0);

    // 2. A *structured* reason built in the dying process's own heap is delivered as a copy.
    let target2 = f.table.spawn_default().expect("target2");
    let reason = f.tuple_in(target2, &[make_small(3), make_small(4)]);
    let expected = f.tuple_in(holder, &[make_small(3), make_small(4)]);
    f.table
        .monitor(watcher, target2, MonitorKind::Process)
        .expect("both pids are live");
    f.table.terminate(target2, reason).expect("target2 is live");
    let payload = f.sole_payload(watcher);
    // SAFETY: the payload is the live 5-tuple table built on the watcher's heap.
    let delivered = unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(process));
        assert_eq!(tuple_element(payload, 3), target2.to_eterm());
        tuple_element(payload, 4)
    };
    assert!(
        !is_immediate(delivered),
        "the structured reason was copied, not shared"
    );
    assert!(compare::eq(delivered, expected, &f.names));
}

#[test]
fn monitor_of_a_dead_pid_is_born_down() {
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let noproc = f.atom_term("noproc");
    let watcher = f.table.spawn_default().expect("watcher");

    // A pid that was live and has died.
    let dead = f.table.spawn_default().expect("a process about to die");
    f.table.remove(dead).expect("it was live");
    assert!(!f.table.contains(dead));

    assert_eq!(f.table.get(watcher).expect("watcher").mailbox().len(), 0);
    let reference = f
        .table
        .monitor(watcher, dead, MonitorKind::Process)
        .expect("a non-live target is not an error");

    let watcher_process = f.table.get(watcher).expect("watcher");
    assert_eq!(
        watcher_process.mailbox().len(),
        1,
        "the immediate noproc DOWN is queued"
    );
    assert!(
        watcher_process.monitors().is_empty(),
        "a non-live target records no outgoing monitor"
    );
    assert!(watcher_process.monitored_by().is_empty());
    let payload = f.sole_payload(watcher);
    assert!(
        payload_is_down_for(payload, reference, down),
        "the DOWN carries the fresh reference"
    );
    assert_pid_down(payload, down, process, dead.to_eterm(), noproc);

    // A pid that never existed behaves identically.
    let never = ProcessId::new(999, 0);
    let reference = f
        .table
        .monitor(watcher, never, MonitorKind::Process)
        .expect("a non-live target is not an error");
    assert_eq!(f.table.get(watcher).expect("watcher").mailbox().len(), 2);
    let payload = f
        .table
        .get(watcher)
        .expect("watcher")
        .mailbox()
        .iter()
        .last()
        .expect("two messages")
        .payload();
    assert!(payload_is_down_for(payload, reference, down));
    assert_pid_down(payload, down, process, never.to_eterm(), noproc);
}

// ---------------------------------------------------------------------------
// demonitor / demonitor with flush
// ---------------------------------------------------------------------------

#[test]
fn demonitor_flush_removes_only_the_matching_down() {
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let watcher = f.table.spawn_default().expect("watcher");

    // Two monitors on two non-live pids: each queues its own immediate noproc DOWN.
    let first = f
        .table
        .monitor(watcher, ProcessId::new(999, 0), MonitorKind::Process)
        .expect("a non-live target is not an error");
    let second = f
        .table
        .monitor(watcher, ProcessId::new(998, 0), MonitorKind::Process)
        .expect("a non-live target is not an error");
    assert_eq!(f.table.get(watcher).expect("watcher").mailbox().len(), 2);

    // Flushing the *second* reference removes exactly its own DOWN.
    assert!(f
        .table
        .demonitor_flush(watcher, second)
        .expect("the watcher is live"));
    assert_eq!(
        f.table.get(watcher).expect("watcher").mailbox().len(),
        1,
        "only the second DOWN was flushed"
    );
    let remaining = f.sole_payload(watcher);
    assert!(
        payload_is_down_for(remaining, first, down),
        "the surviving DOWN is the first monitor's"
    );
    assert!(
        !payload_is_down_for(remaining, second, down),
        "the flushed DOWN is gone"
    );

    // Then the first one; a second flush of the same reference changes nothing.
    assert!(f
        .table
        .demonitor_flush(watcher, first)
        .expect("the watcher is live"));
    assert_eq!(f.table.get(watcher).expect("watcher").mailbox().len(), 0);
    assert!(!f
        .table
        .demonitor_flush(watcher, first)
        .expect("the watcher is live"));
}

#[test]
fn plain_demonitor_leaves_a_queued_down_while_flush_removes_it() {
    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("watcher");
    let target = f.table.spawn_default().expect("target");

    let reference = f
        .table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");
    f.table.terminate(target, make_small(5)).expect("target is live");
    assert_eq!(
        f.table.get(watcher).expect("watcher").mailbox().len(),
        1,
        "the eagerly applied DOWN is queued"
    );

    // `demonitor/1` (no `[flush]`) drops the monitor half but leaves the DOWN queued.
    f.table
        .demonitor(watcher, reference)
        .expect("the watcher is live");
    assert_eq!(f.table.get(watcher).expect("watcher").mailbox().len(), 1);

    // `demonitor/2` with `[flush]` removes it.
    assert!(f
        .table
        .demonitor_flush(watcher, reference)
        .expect("the watcher is live"));
    assert_eq!(f.table.get(watcher).expect("watcher").mailbox().len(), 0);
}

#[test]
fn demonitor_of_a_bogus_reference_is_false() {
    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("watcher");
    // A reference the table never handed out: this test creates no monitor, so every id is free.
    let bogus = reference_beyond_local_table();

    assert!(!f.table.demonitor(watcher, bogus).expect("the watcher is live"));
    assert!(!f
        .table
        .demonitor_flush(watcher, bogus)
        .expect("the watcher is live"));
    assert_eq!(f.table.get(watcher).expect("watcher").mailbox().len(), 0);

    // An unknown watcher is still an error.
    assert!(matches!(
        f.table.demonitor_flush(ProcessId::new(999, 0), bogus),
        Err(BeamError::TermNotFound)
    ));
}

// ---------------------------------------------------------------------------
// Named monitors (`erlang:monitor(process, Name)`)
// ---------------------------------------------------------------------------

#[test]
fn monitor_named_reports_the_name_node_target_on_death() {
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let name = f.atom("server");
    let node = f.atom("nonode@nohost");
    let watcher = f.table.spawn_default().expect("watcher");
    let target = f.table.spawn_default().expect("target");
    f.table.register(name, target).expect("the name is free");

    let reference = f
        .table
        .monitor_named(watcher, name, node, MonitorKind::Process)
        .expect("the name resolves to a live process");
    // The name resolved to a pid, so the ordinary monitor halves were recorded.
    assert_eq!(
        f.table
            .get(watcher)
            .expect("watcher")
            .monitors()
            .find(reference)
            .map(|m| m.target),
        Some(target)
    );
    assert_eq!(f.table.get(target).expect("target").monitored_by().len(), 1);

    let reason = make_small(7);
    f.table.terminate(target, reason).expect("target is live");
    let payload = f.sole_payload(watcher);
    // SAFETY: the payload is the live 5-tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(process));
        let target_term = tuple_element(payload, 3);
        assert_eq!(
            tuple_size(target_term),
            2,
            "the target is the `{{Name, Node}}` pair, not a pid"
        );
        assert_eq!(tuple_element(target_term, 0), make_atom(name));
        assert_eq!(tuple_element(target_term, 1), make_atom(node));
        assert_eq!(tuple_element(payload, 4), reason);
    }
    // The outgoing record is resolved by the death like any other monitor — the `{Name, node}`
    // descriptor is held separately (a `ProcessTable` side table) precisely so this DOWN could
    // still carry the name.
    assert!(
        f.table.get(watcher).expect("watcher").monitors().is_empty(),
        "the named monitor's outgoing half is resolved too"
    );
}

#[test]
fn monitor_named_of_an_unregistered_name_is_born_down() {
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let noproc = f.atom_term("noproc");
    let name = f.atom("nobody");
    let node = f.atom("nonode@nohost");
    let watcher = f.table.spawn_default().expect("watcher");

    assert_eq!(f.table.get(watcher).expect("watcher").mailbox().len(), 0);
    let reference = f
        .table
        .monitor_named(watcher, name, node, MonitorKind::Process)
        .expect("an unregistered name is not an error");
    let watcher_process = f.table.get(watcher).expect("watcher");
    assert_eq!(watcher_process.mailbox().len(), 1);
    assert!(watcher_process.monitors().is_empty(), "nothing was recorded");
    let payload = f.sole_payload(watcher);
    assert!(payload_is_down_for(payload, reference, down));
    // SAFETY: the payload is the live 5-tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(process));
        let target_term = tuple_element(payload, 3);
        assert_eq!(tuple_size(target_term), 2);
        assert_eq!(tuple_element(target_term, 0), make_atom(name));
        assert_eq!(tuple_element(target_term, 1), make_atom(node));
        assert_eq!(tuple_element(payload, 4), noproc);
    }
}

#[test]
fn set_local_node_and_local_node_round_trip() {
    let mut f = Fixture::new();
    assert_eq!(f.table.local_node(), None, "not installed yet");
    let node = f.atom("nonode@nohost");
    f.table.set_local_node(node);
    assert_eq!(f.table.local_node(), Some(node));
}

#[test]
fn monitor_term_routes_an_atom_target_and_needs_the_local_node() {
    let mut f = Fixture::new();
    let name = f.atom("server");
    let node = f.atom("nonode@nohost");
    let watcher = f.table.spawn_default().expect("watcher");
    let target = f.table.spawn_default().expect("target");
    f.table.register(name, target).expect("the name is free");
    let name_term = make_atom(name);

    // Without a local node the name target is a typed unsupported, never a guessed target (and
    // nothing is recorded or queued).
    match f.table.monitor_term(watcher, name_term, MonitorKind::Process) {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason.as_atom(),
            Some("unsupported: monitor/2: name target requires the local node (set_local_node)")
        ),
        other => panic!("expected a typed unsupported raise, got {other:?}"),
    }
    assert!(f.table.get(watcher).expect("watcher").mailbox().is_empty());
    assert!(f.table.get(watcher).expect("watcher").monitors().is_empty());

    // With the node installed it routes to the named path.
    f.table.set_local_node(node);
    let reference = f
        .table
        .monitor_term(watcher, name_term, MonitorKind::Process)
        .expect("the name resolves");
    assert_eq!(
        f.table
            .get(watcher)
            .expect("watcher")
            .monitors()
            .find(reference)
            .map(|m| m.target),
        Some(target)
    );

    // A non-pid, non-atom target is still `badarg`.
    assert!(matches!(
        f.table.monitor_term(watcher, make_small(1), MonitorKind::Process),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// Oracle cross-checks (the real system ERTS)
// ---------------------------------------------------------------------------

/// `monitor/2` on a dead pid delivers `{'DOWN', Ref, process, Pid, noproc}`.
///
/// The check deliberately asserts the **received** message rather than `message_queue_len`: the
/// oracle hands a DOWN to a still-running receiver through its signal queue (it becomes a mailbox
/// message when the receiver next handles signals), so the queue length observed immediately after
/// `monitor/2` is not the observable the two runtimes share. The received tuple is.
#[test]
fn oracle_monitor_of_a_dead_pid_is_born_down_with_noproc() {
    let expr = r#"
        P0 = spawn(fun() -> ok end),
        M0 = erlang:monitor(process, P0),
        receive {'DOWN', M0, process, P0, _} -> ok end,
        M1 = erlang:monitor(process, P0),
        {Target, Reason} = receive {'DOWN', M1, process, T, Rn} -> {T, Rn} end,
        io:format("~p|~p~n", [Target =:= P0, Reason])
    "#;
    if !expect_oracle(expr, "true|noproc") {
        return;
    }

    // The Rust side of the same statement: a monitor on a pid that was live and has died queues
    // exactly one message, targeted at that pid, with reason `noproc`.
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let noproc = f.atom_term("noproc");
    let watcher = f.table.spawn_default().expect("watcher");
    let dead = f.table.spawn_default().expect("a process about to die");
    f.table.remove(dead).expect("it was live");
    let len_before = f.table.get(watcher).expect("watcher").mailbox().len();
    let reference = f
        .table
        .monitor(watcher, dead, MonitorKind::Process)
        .expect("a non-live target is not an error");
    let len_after = f.table.get(watcher).expect("watcher").mailbox().len();
    assert_eq!(len_after - len_before, 1, "exactly one message was queued");
    let payload = f.sole_payload(watcher);
    assert!(payload_is_down_for(payload, reference, down));
    assert_pid_down(payload, down, process, dead.to_eterm(), noproc);
}

/// `monitor(process, UnregisteredName)` targets the `{Name, Node}` pair, not a pid.
#[test]
fn oracle_monitor_of_an_unregistered_name_targets_the_name_node_pair() {
    // Printed as a *comparison* against `node()`, so the check does not hardcode the host's
    // node name.
    let expr = r#"
        M = erlang:monitor(process, nm1),
        {Target, Reason} = receive {'DOWN', M, process, T, Rn} -> {T, Rn} end,
        io:format("~p|~p~n", [Target =:= {nm1, node()}, Reason])
    "#;
    if !expect_oracle(expr, "true|noproc") {
        return;
    }

    // The Rust side: an unregistered name is born down with the `{Name, Node}` target.
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let name = f.atom("nm1");
    let node = f.atom("nonode@nohost");
    let noproc = f.atom_term("noproc");
    let watcher = f.table.spawn_default().expect("watcher");
    let reference = f
        .table
        .monitor_named(watcher, name, node, MonitorKind::Process)
        .expect("an unregistered name is not an error");
    let payload = f.sole_payload(watcher);
    assert!(payload_is_down_for(payload, reference, down));
    // SAFETY: the payload is the live 5-tuple the table built on the watcher's heap.
    unsafe {
        let target_term = tuple_element(payload, 3);
        assert_eq!(tuple_size(target_term), 2);
        assert_eq!(tuple_element(target_term, 0), make_atom(name));
        assert_eq!(tuple_element(target_term, 1), make_atom(node));
        assert_eq!(tuple_element(payload, 4), noproc);
    }
}

/// `demonitor(Ref, [flush])` drops the queued `{'DOWN', Ref, ...}`; `demonitor/1` does not.
///
/// The oracle hands a DOWN to a still-running receiver through its signal queue, so each check
/// first forces signal handling (`receive after 0`) to materialise it into the mailbox — which is
/// the state the Rust table is in right away.
#[test]
fn oracle_demonitor_flush_drops_a_queued_down() {
    let expr = r#"
        M = erlang:monitor(process, nm2),
        receive after 0 -> ok end,
        {message_queue_len, L0} = process_info(self(), message_queue_len),
        D = erlang:demonitor(M, [flush]),
        {message_queue_len, L1} = process_info(self(), message_queue_len),
        M2 = erlang:monitor(process, nm3),
        receive after 0 -> ok end,
        Dt = erlang:demonitor(M2),
        {message_queue_len, L2} = process_info(self(), message_queue_len),
        R = receive {'DOWN', M2, _, T, Rn} -> {T =:= {nm3, node()}, Rn} after 200 -> not_delivered end,
        io:format("~p|~p|~p|~p|~p~n", [L0, D, L1, L2, R])
    "#;
    // `L0` = 1 (the queued noproc DOWN), flushing drops it (`L1` = 0), a plain `demonitor/1`
    // leaves the next one queued (`L2` = 1) and it is received with the `{Name, Node}` target.
    if !expect_oracle(expr, "1|true|0|1|{true,noproc}") {
        return;
    }

    // The Rust side of the same statement. The table's `bool` means "something was removed"
    // (the BIF layer renders `erlang:demonitor/2` as unconditional `true`, the oracle's result),
    // so only the mailbox lengths are pinned here.
    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("watcher");
    let name = f.atom("nm2");
    let node = f.atom("nonode@nohost");

    let reference = f
        .table
        .monitor_named(watcher, name, node, MonitorKind::Process)
        .expect("an unregistered name is not an error");
    assert_eq!(
        f.table.get(watcher).expect("watcher").mailbox().len(),
        1,
        "the queued noproc DOWN"
    );
    assert!(f
        .table
        .demonitor_flush(watcher, reference)
        .expect("the watcher is live"));
    assert_eq!(
        f.table.get(watcher).expect("watcher").mailbox().len(),
        0,
        "the flush dropped the queued DOWN"
    );

    let second_name = f.atom("nm3");
    let second = f
        .table
        .monitor_named(watcher, second_name, node, MonitorKind::Process)
        .expect("an unregistered name is not an error");
    f.table
        .demonitor(watcher, second)
        .expect("the watcher is live");
    assert_eq!(
        f.table.get(watcher).expect("watcher").mailbox().len(),
        1,
        "a plain demonitor leaves the DOWN queued"
    );
}

/// `register/2` and `unregister/1` raise `badarg` for a dead pid and an unregistered name.
#[test]
fn oracle_register_and_unregister_raise_badarg() {
    let expr = r#"
        P = spawn(fun() -> ok end),
        M = erlang:monitor(process, P),
        receive {'DOWN', M, process, P, _} -> ok end,
        R1 = try register(rn2, P) catch error:badarg -> badarg end,
        R2 = try unregister(rn_never_registered) catch error:badarg -> badarg end,
        io:format("~p|~p~n", [R1, R2])
    "#;
    if !expect_oracle(expr, "badarg|badarg") {
        return;
    }

    let mut f = Fixture::new();
    let name = f.atom("rn2");
    let dead = f.table.spawn_default().expect("a process about to die");
    f.table.remove(dead).expect("it was live");
    assert!(matches!(f.table.register(name, dead), Err(BeamError::BadArg)));
    let never = f.atom("rn_never_registered");
    assert!(matches!(
        f.table.unregister(never),
        Err(BeamError::BadArg)
    ));
}

/// `process_info(Pid, registered_name)` reports the registered name (and `[]` when there is none).
#[test]
fn oracle_process_info_registered_name() {
    let expr = r#"
        P = spawn(fun() -> receive stop -> ok end end),
        R0 = process_info(P, registered_name),
        register(rn1, P),
        R1 = process_info(P, registered_name),
        io:format("~p|~p~n", [R0, R1])
    "#;
    if !expect_oracle(expr, "[]|{registered_name,rn1}") {
        return;
    }

    let mut f = Fixture::new();
    let name = f.atom("rn1");
    let pid = f.table.spawn_default().expect("a live process");
    assert_eq!(f.table.name_of(pid), None, "unregistered: `[]`");
    f.table.register(name, pid).expect("the name is free");
    assert_eq!(f.table.name_of(pid), Some(name), "`{{registered_name, rn1}}`");
}

/// The `{'DOWN', Ref, process, Pid, Reason}` a watcher receives carries the **exact** reason the
/// target died with: `normal` for a normal termination, the full *structured* reason for an
/// abnormal termination, and `killed` (the rewritten form of `kill`) for `exit(Pid, kill)`.
///
/// The oracle prints `normal|{bad,1}|killed` for the three receivers (each target is alive when
/// it is monitored — it waits for a `go` message — so none of them races a `noproc` DOWN).
#[test]
fn oracle_down_reason_fidelity() {
    let expr = r#"
        P1 = spawn(fun() -> receive go -> ok end end),
        M1 = erlang:monitor(process, P1),
        P1 ! go,
        R1 = receive {_, M1, _, _, A} -> A after 2000 -> t1 end,
        P2 = spawn(fun() -> receive go -> exit({bad, 1}) end end),
        M2 = erlang:monitor(process, P2),
        P2 ! go,
        R2 = receive {_, M2, _, _, B} -> B after 2000 -> t2 end,
        P3 = spawn(fun() -> receive never -> ok end end),
        M3 = erlang:monitor(process, P3),
        exit(P3, kill),
        R3 = receive {_, M3, _, _, C} -> C after 2000 -> t3 end,
        io:format("~p|~p|~p~n", [R1, R2, R3])
    "#;
    if !expect_oracle(expr, "normal|{bad,1}|killed") {
        return;
    }

    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let watcher = f.table.spawn_default().expect("watcher");

    // 1. A normal termination: the reason is the `normal` atom.
    let normal = f.atom_term("normal");
    let target1 = f.table.spawn_default().expect("target1");
    f.table
        .monitor(watcher, target1, MonitorKind::Process)
        .expect("both pids are live");
    f.table.terminate(target1, normal).expect("target1 is live");
    assert!(!f.table.contains(target1));
    let payload = f.sole_payload(watcher);
    assert_pid_down(payload, down, process, target1.to_eterm(), normal);
    f.drain_one(watcher);

    // 2. An abnormal termination with a *structured* reason built in the dying process's own heap.
    let bad = f.atom_term("bad");
    let holder = f.table.spawn_default().expect("holder");
    let target2 = f.table.spawn_default().expect("target2");
    let reason = f.tuple_in(target2, &[bad, make_small(1)]);
    let expected = f.tuple_in(holder, &[bad, make_small(1)]);
    f.table
        .monitor(watcher, target2, MonitorKind::Process)
        .expect("both pids are live");
    f.table.terminate(target2, reason).expect("target2 is live");
    let payload = f.sole_payload(watcher);
    // SAFETY: the payload is the live 5-tuple the table built on the watcher's heap.
    let delivered = unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(process));
        assert_eq!(tuple_element(payload, 3), target2.to_eterm());
        tuple_element(payload, 4)
    };
    assert!(
        !is_immediate(delivered),
        "the structured reason was copied, not shared"
    );
    assert!(compare::eq(delivered, expected, &f.names));
    f.drain_one(watcher);

    // 3. `exit(Pid, kill)`: the reason is rewritten to `killed`, never the sent `kill`.
    let kill = f.atom_term("kill");
    let killed = f.atom_term("killed");
    let killer = f.table.spawn_default().expect("killer");
    let target3 = f.table.spawn_default().expect("target3");
    f.table
        .monitor(watcher, target3, MonitorKind::Process)
        .expect("both pids are live");
    f.table
        .send_exit(killer, target3, kill)
        .expect("target3 is live");
    assert!(!f.table.contains(target3), "`kill` terminates the target");
    let payload = f.sole_payload(watcher);
    assert_pid_down(payload, down, process, target3.to_eterm(), killed);
    // SAFETY: the live DOWN tuple on the watcher's heap.
    unsafe {
        assert_ne!(
            tuple_element(payload, 4),
            kill,
            "the reason is the rewritten `killed`, not the sent `kill`"
        );
    }
}

// ---------------------------------------------------------------------------
// Oracle cross-checks: a target's death resolves the watcher's outgoing monitor
// ---------------------------------------------------------------------------

/// A target's death resolves **both** halves of the monitor: the DOWN is still delivered, and the
/// watcher's own outgoing entry disappears, so `process_info(W, monitors)` goes from
/// `[{process,T}]` to `[]` and `demonitor(Ref, [info])` then answers `false`
/// (`demonitor/1` without `info` answers `true` unconditionally — that is the BIF layer's
/// rendering, not this table's "was it on record" boolean).
///
/// The probe's observables are reported from **inside** the watcher process (the `-eval` process
/// cannot be the watcher and read its own list at the right moments), and every step is
/// synchronized by a message or a `receive`, so the probe is deterministic.
#[test]
fn oracle_a_targets_death_resolves_the_watchers_outgoing_monitor() {
    let expr = r#"
        Parent = self(),
        T = spawn(fun() -> receive stop -> ok end end),
        W = spawn(fun() ->
                M = erlang:monitor(process, T),
                Parent ! {m, M},
                receive {'DOWN', M, process, _, _} -> ok end,
                Parent ! {observed, erlang:process_info(self(), monitors),
                                    erlang:demonitor(M, [info])}
            end),
        _M = receive {m, Ref} -> Ref end,
        Before = erlang:process_info(W, monitors),
        T ! stop,
        {After, D} = receive {observed, A, R} -> {A, R} after 2000 -> {timeout, timeout} end,
        io:format("~p|~p|~p~n", [Before =:= {monitors, [{process, T}]}, After, D])
    "#;
    if !expect_oracle(expr, "true|{monitors,[]}|false") {
        return;
    }

    // The Rust side of the same statement: the outgoing `[{process,T}]` record is resolved when
    // the target dies, while the DOWN it produced is still queued.
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let watcher = f.table.spawn_default().expect("watcher");
    let target = f.table.spawn_default().expect("target");
    let reference = f
        .table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");
    assert_eq!(
        f.table.get(watcher).expect("watcher").monitors().len(),
        1,
        "`{{monitors, [{{process, T}}]}}` while the target lives"
    );

    let boom = f.atom_term("boom");
    f.table
        .terminate(target, boom)
        .expect("the target is live");

    let payload = f.sole_payload(watcher);
    assert!(
        payload_is_down_for(payload, reference, down),
        "the DOWN is still delivered"
    );
    assert!(
        f.table.get(watcher).expect("watcher").monitors().is_empty(),
        "`{{monitors, []}}` once the target is gone"
    );
    assert!(
        !f.table
            .demonitor(watcher, reference)
            .expect("the watcher is live"),
        "`demonitor(Ref, [info])` => false: the monitor is no longer on record"
    );
}

// ---------------------------------------------------------------------------
// Oracle cross-checks: `link/1` to a non-live pid
// ---------------------------------------------------------------------------

/// `link(DeadPid)` signals `noproc` **from that pid** at the linking caller: a **trapping** caller
/// answers `true` and is handed `{'EXIT', DeadPid, noproc}`, while a **non-trapping** caller dies
/// — a `noproc` raise whose uncaught reason is `{noproc, Stacktrace}` (this crate signals the bare
/// atom `noproc`; see `ProcessTable::link`'s documented approximation).
///
/// `D`'s death is established by monitoring it and waiting for the DOWN, so the pid is genuinely
/// non-live before either caller links to it.
#[test]
fn oracle_link_to_a_non_live_pid_signals_noproc_from_it() {
    let expr = r#"
        Parent = self(),
        D = spawn(fun() -> ok end),
        DM = erlang:monitor(process, D),
        receive {'DOWN', DM, process, D, _} -> ok end,
        W = spawn(fun() ->
                process_flag(trap_exit, true),
                R = link(D),
                Parent ! {ret, R},
                receive X -> Parent ! {msg, X} after 2000 -> Parent ! {msg, timeout} end
            end),
        {ret, Ret} = receive {ret, R0} -> {ret, R0} end,
        Msg = receive {msg, M} -> M end,
        V = spawn(fun() -> link(D) end),
        VM = erlang:monitor(process, V),
        Reason = receive {'DOWN', VM, process, V, Rn} -> Rn after 2000 -> timeout end,
        io:format("~p|~p|~p|~p~n",
                  [Ret, Msg =:= {'EXIT', D, noproc},
                   element(1, Reason) =:= noproc, is_list(element(2, Reason))])
    "#;
    if !expect_oracle(expr, "true|true|true|true") {
        return;
    }

    // The Rust side of the trapping half: `link/1` runs in the *executing* process, so its
    // `noproc` signal is queued (the `exit/2` model) and applied at the caller's schedule-in.
    let mut f = Fixture::new();
    let exit_atom = f.atom("EXIT");
    let noproc = f.atom_term("noproc");
    let dead = ProcessId::new(999, 0);

    let trapper = f.table.spawn_default().expect("trapper");
    f.table
        .get_mut(trapper)
        .expect("trapper")
        .set_trap_exit(true);
    f.table
        .get_mut(trapper)
        .expect("trapper")
        .set_state(ProcessState::Running);
    f.table
        .link(trapper, dead)
        .expect("a non-live target is not an error");
    assert_eq!(
        f.table.pending_signals(trapper).expect("trapper is live"),
        1,
        "one noproc signal is queued for the caller"
    );
    f.table
        .service_signals(trapper)
        .expect("trapper is live");
    assert!(f.table.contains(trapper), "the trapping caller survives");
    let payload = f.sole_payload(trapper);
    // SAFETY: the payload is the live 3-tuple the signal builder wrote on the trapper's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), make_atom(exit_atom));
        assert_eq!(tuple_element(payload, 1), dead.to_eterm());
        assert_eq!(tuple_element(payload, 2), noproc);
    }

    // The non-trapping half: the caller is terminated with `noproc`.
    let victim = f.table.spawn_default().expect("victim");
    f.table
        .get_mut(victim)
        .expect("victim")
        .set_state(ProcessState::Running);
    f.table
        .link(victim, dead)
        .expect("a non-live target is not an error");
    assert!(
        f.table.contains(victim),
        "the signal is deferred while the caller runs"
    );
    f.table
        .service_signals(victim)
        .expect("victim is live");
    assert!(
        !f.table.contains(victim),
        "the non-trapping caller is terminated with noproc"
    );
}