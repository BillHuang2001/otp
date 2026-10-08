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

//! Integration evidence for the **`erlang:monitor/3` `{tag, Tag}` user tag**: the notification a
//! tagged monitor delivers is the oracle's `{Tag, Ref, Kind, Target, Reason}` in place of the
//! ordinary `{'DOWN', Ref, Kind, Target, Reason}`, for a **live** target, a target that
//! terminates with a reason, a target that is already dead (`noproc`), a **registered name**
//! target and a **port** target.
//!
//! # Where the tag lives
//!
//! Not on the monitor records: `Monitor`/`MonitoredBy`/`PortWatch` are built by struct literal
//! from other crates, so their shape is fixed (adding a field would break them). The tag is kept
//! in a **side table on `ProcessTable`**, keyed by the monitor reference — exactly like the
//! `{Name, node}` descriptor of a named monitor (see `monitor.rs` and `ptab.rs`, Design
//! Decisions). A tag must be a heap-independent immediate (an atom, small integer, pid/port or
//! `nil`); a term resident in a per-process heap cannot be stored safely, because the side table
//! is not a collector rootset and a collection would relocate the word underneath it. A
//! non-immediate tag is therefore refused with the typed `unsupported` rather than stored — a
//! **documented deviation** from the oracle (which accepts any term), pinned by
//! [`a_non_immediate_tag_is_a_typed_unsupported`].
//!
//! # Oracle protocol (the same text protocol as `tests/registry_monitors.rs`)
//!
//! Every `oracle_*` check runs an equivalent Erlang expression under the system `erl`
//! (`-noshell -eval '<expr>, halt(0).'`) and asserts its printed answer. The expressions are
//! written to print only **stable** parts of the `{'DOWN', …}` tuple — the head, whether element
//! 1 is a reference, the kind atom, whether the target is a pid/port, and the reason — because
//! the reference, the pid and the node name are not reproducible from a Rust test. Where the node
//! name is involved the expression compares against `node()` instead of spelling a literal.
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

/// The one `unsupported:` reason naming a tag that is not a heap-independent immediate.
const UNSUPPORTED_TAG: &str =
    "unsupported: monitor/3: tag must be an immediately-representable term (atom/integer/pid/port)";

// ---------------------------------------------------------------------------
// The oracle driver (text protocol, shared with `tests/registry_monitors.rs`)
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
// Fixture
// ---------------------------------------------------------------------------

/// A process table plus the atom table its [`SignalAtoms`] were interned from, so a test can
/// resolve the well-known message atoms (`'DOWN'`, `process`, `port`, `noproc`, ...) back to
/// terms.
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

    /// Builds a tuple on `pid`'s own heap (a *structured*, non-immediate term).
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

    /// Removes and discards the head of `pid`'s mailbox, so a multi-phase test can again assert
    /// "exactly one message is queued".
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

/// Asserts `payload` is the 5-tuple `{head, <a ref>, kind, target, reason}` and returns its
/// **target** element (element 3).
///
/// This is the one shape every DOWN builder produces — `{'DOWN', …}` when `head` is the interned
/// `'DOWN'` atom, `{Tag, …}` when it is the monitor's user tag. The target is returned rather
/// than compared, because a caller's *structured* target (`{Name, Node}`, built by
/// [`Fixture::tuple_in`]) is a structurally equal but distinct boxed term and must be compared
/// with `beam_term::compare::eq`.
fn assert_down(payload: Eterm, head: Atom, kind: Atom, reason: Eterm) -> Eterm {
    // SAFETY: `head`/`kind` come from the same table that interned the message's atoms, and
    // `payload` is the live 5-tuple the table built on the recipient's heap; the five element
    // slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5, "a DOWN message is a 5-tuple");
        assert_eq!(tuple_element(payload, 0), make_atom(head));
        assert_eq!(tuple_element(payload, 2), make_atom(kind));
        assert_eq!(tuple_element(payload, 4), reason);
        tuple_element(payload, 3)
    }
}

/// Asserts the target element of a DOWN is the pid/port immediate `expected`.
fn assert_immediate_target(target: Eterm, expected: Eterm) {
    assert_eq!(target, expected, "the DOWN target is the expected immediate");
}

/// Asserts `err` is the typed `unsupported: <expected>` raise.
fn assert_typed_unsupported(err: BeamError, expected: &str) {
    match err {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some(expected),
            "the typed unsupported raise must name the missing capability exactly"
        ),
        other => panic!("expected the typed `unsupported` raise, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Process targets
// ---------------------------------------------------------------------------

#[test]
fn a_tagged_live_monitor_delivers_the_tagged_down_and_the_untagged_one_is_unchanged() {
    // The two baseline shapes, cross-checked against the oracle in one run: the tagged monitor's
    // DOWN is `{mytag, Ref, process, Pid, Reason}` and the untagged monitor's is byte-identical to
    // the ordinary `{'DOWN', Ref, process, Pid, Reason}`.
    let expr = r#"
        F = fun(Opts) ->
              P = spawn(fun() -> receive go -> ok end end),
              _ = erlang:monitor(process, P, Opts),
              P ! go,
              receive {H, R, K, T, A} -> {H, is_reference(R), K, is_pid(T), A}
              after 2000 -> timeout end
            end,
        io:format("~p|~p~n", [F([{tag, mytag}]), F([])])
    "#;
    if !expect_oracle(
        expr,
        "{mytag,true,process,true,normal}|{'DOWN',true,process,true,normal}",
    ) {
        return;
    }

    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");

    // 1. Tagged through the pid entry point: the head is the tag, the rest is the ordinary DOWN.
    let target = f.table.spawn_default().expect("a target process");
    let tagged_ref = f
        .table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("both pids are live");
    f.table.terminate(target, normal).expect("the target is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, mytag, process, normal),
        target.to_eterm(),
    );
    // SAFETY: the live DOWN tuple on the watcher's heap.
    unsafe {
        assert_ne!(
            tuple_element(payload, 0),
            make_atom(down),
            "a tagged DOWN never carries the default head atom"
        );
    }
    f.drain_one(watcher);

    // 2. The same tag through the arbitrary-term entry point, which routes a local pid identically
    //    (and allocates a fresh reference — the two monitors are really separate).
    let target = f.table.spawn_default().expect("a second target");
    let term_ref = f
        .table
        .monitor_term_with_tag(
            watcher,
            target.to_eterm(),
            MonitorKind::Process,
            Some(make_atom(mytag)),
        )
        .expect("a local pid term dispatches to the pid path");
    assert_ne!(term_ref, tagged_ref, "each monitor gets its own reference");
    f.table.terminate(target, normal).expect("the target is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, mytag, process, normal),
        target.to_eterm(),
    );
    f.drain_one(watcher);

    // 3. Untagged: `monitor/2` draws the default `'DOWN'` head — unchanged by this feature.
    let untagged = f.table.spawn_default().expect("an untagged target");
    f.table
        .monitor(watcher, untagged, MonitorKind::Process)
        .expect("both pids are live");
    f.table
        .terminate(untagged, normal)
        .expect("the untagged target is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, down, process, normal),
        untagged.to_eterm(),
    );
}

#[test]
fn a_tagged_monitor_of_an_exiting_target_carries_the_death_reason() {
    let expr = r#"
        P = spawn(fun() -> receive go -> exit(boom) end end),
        _ = erlang:monitor(process, P, [{tag, mytag}]),
        P ! go,
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, is_pid(T), A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,process,true,boom}") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let boom = f.atom_term("boom");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let target = f.table.spawn_default().expect("a target process");

    f.table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("both pids are live");
    f.table.terminate(target, boom).expect("the target is live");
    assert!(!f.table.contains(target));

    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, mytag, process, boom),
        target.to_eterm(),
    );
}

#[test]
fn a_tagged_monitor_of_a_dead_pid_is_born_down_tagged() {
    let expr = r#"
        {P, R0} = spawn_monitor(fun() -> ok end),
        receive {DOWN, R0, _, _, _} when is_atom(DOWN) -> ok end,
        _ = erlang:monitor(process, P, [{tag, mytag}]),
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, is_pid(T), A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,process,true,noproc}") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let noproc = f.atom_term("noproc");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let dead = {
        let dying = f.table.spawn_default().expect("a process about to die");
        f.table.remove(dying);
        dying
    };

    let reference = f
        .table
        .monitor_with_tag(watcher, dead, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("a non-live target is not an error");
    // Nothing is left to watch, so no monitor half is recorded — only the immediate DOWN.
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .monitors()
        .find(reference)
        .is_none());
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, mytag, process, noproc),
        dead.to_eterm(),
    );
}

// ---------------------------------------------------------------------------
// Named targets
// ---------------------------------------------------------------------------

#[test]
fn a_tagged_named_monitor_reports_the_name_node_target() {
    // A named monitor's DOWN target is `{Name, Node}` rather than the pid the name resolved to;
    // a tag replaces only the head atom.
    let expr = r#"
        P = spawn(fun() -> receive go -> ok end end),
        true = register(my_tag_server, P),
        _ = erlang:monitor(process, my_tag_server, [{tag, mytag}]),
        P ! go,
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, T =:= {my_tag_server, node()}, A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,process,true,normal}") {
        return;
    }

    let mut f = Fixture::new();
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let name = f.atom("my_tag_server");
    let node = f.atom("nonode@nohost");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let target = f.table.spawn_default().expect("a target process");
    f.table.register(name, target).expect("the name is free");

    f.table
        .monitor_named_with_tag(
            watcher,
            name,
            node,
            MonitorKind::Process,
            Some(make_atom(mytag)),
        )
        .expect("the name resolves to a live process");
    f.table.terminate(target, normal).expect("the target is live");

    // The DOWN target is the `{Name, Node}` pair materialised on the watcher's heap.
    let expected_target = f.tuple_in(watcher, &[make_atom(name), make_atom(node)]);
    let payload = f.sole_payload(watcher);
    let delivered = assert_down(payload, mytag, process, normal);
    assert!(
        compare::eq(delivered, expected_target, &f.names),
        "the DOWN target is the `{{Name, Node}}` pair"
    );
    f.drain_one(watcher);

    // The unregistered-name form is likewise tagged.
    let nobody = f.atom("nobody_here");
    f.table
        .monitor_named_with_tag(
            watcher,
            nobody,
            node,
            MonitorKind::Process,
            Some(make_atom(mytag)),
        )
        .expect("an unregistered name is not an error");
    let expected_target = f.tuple_in(watcher, &[make_atom(nobody), make_atom(node)]);
    let payload = f.sole_payload(watcher);
    let delivered = assert_down(payload, mytag, process, f.atom_term("noproc"));
    assert!(
        compare::eq(delivered, expected_target, &f.names),
        "the DOWN target is the `{{Name, Node}}` pair"
    );
}

#[test]
fn the_oracle_tags_a_noproc_named_down() {
    let expr = r#"
        _ = erlang:monitor(process, nobody_here, [{tag, mytag}]),
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, T =:= {nobody_here, node()}, A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,process,true,noproc}") {
        return;
    }

    // The same observable through the table: the unregistered name is born down, tagged.
    let mut f = Fixture::new();
    let process = f.atom("process");
    let mytag = f.atom("mytag");
    let nobody = f.atom("nobody_here");
    let node = f.atom("nonode@nohost");
    let watcher = f.table.spawn_default().expect("a watcher process");

    f.table
        .monitor_named_with_tag(
            watcher,
            nobody,
            node,
            MonitorKind::Process,
            Some(make_atom(mytag)),
        )
        .expect("an unregistered name is not an error");
    let expected_target = f.tuple_in(watcher, &[make_atom(nobody), make_atom(node)]);
    let payload = f.sole_payload(watcher);
    let delivered = assert_down(payload, mytag, process, f.atom_term("noproc"));
    assert!(
        compare::eq(delivered, expected_target, &f.names),
        "the DOWN target is the `{{Name, Node}}` pair"
    );
}

// ---------------------------------------------------------------------------
// Port targets
// ---------------------------------------------------------------------------

#[test]
fn a_tagged_port_monitor_delivers_the_tagged_port_down_and_the_untagged_one_is_unchanged() {
    // The port analogue: the kind slot carries the `port` atom and the target is the port
    // immediate. `port_close/1` from the owning process queues an `{'EXIT', Port, normal}` first
    // (the owner link), so the expressions pattern-match the 5-tuple DOWN specifically.
    let expr = r#"
        process_flag(trap_exit, true),
        F = fun(Opts) ->
              Port = open_port({spawn, "cat"}, []),
              _ = erlang:monitor(port, Port, Opts),
              port_close(Port),
              receive {H, R, K, T, A} -> {H, is_reference(R), K, is_port(T), A}
              after 2000 -> timeout end
            end,
        io:format("~p|~p~n", [F([{tag, mytag}]), F([])])
    "#;
    if !expect_oracle(
        expr,
        "{mytag,true,port,true,normal}|{'DOWN',true,port,true,normal}",
    ) {
        return;
    }

    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let port_atom = f.atom("port");
    let mytag = f.atom("mytag");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut ports = PortTable::new();

    // 1. Tagged, through the typed `PortId` entry point.
    let port = ports.alloc().expect("room in the number space");
    let reference = f
        .table
        .monitor_port_with_tag(watcher, port, &mut ports, Some(make_atom(mytag)))
        .expect("a live port monitors");
    assert_eq!(
        f.table.get(watcher).expect("watcher").port_monitors(),
        &[PortWatch { reference, port }],
        "the outgoing half is recorded on the watcher"
    );
    f.table
        .port_closed(&mut ports, port, normal)
        .expect("the port is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, mytag, port_atom, normal),
        port.to_eterm(),
    );
    assert!(
        f.table
            .get(watcher)
            .expect("watcher")
            .port_monitors()
            .is_empty(),
        "the port's death resolved the outgoing half"
    );
    f.drain_one(watcher);

    // 2. Untagged, through the term entry point: the default `'DOWN'` head, unchanged.
    let port = ports.alloc().expect("room in the number space");
    f.table
        .monitor_port_term(watcher, port.to_eterm(), &mut ports)
        .expect("a live port monitors");
    f.table
        .port_closed(&mut ports, port, normal)
        .expect("the port is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, down, port_atom, normal),
        port.to_eterm(),
    );
    f.drain_one(watcher);

    // 3. Tagged, through the term entry point.
    let port = ports.alloc().expect("room in the number space");
    f.table
        .monitor_port_term_with_tag(watcher, port.to_eterm(), &mut ports, Some(make_atom(mytag)))
        .expect("a live port monitors");
    f.table
        .port_closed(&mut ports, port, normal)
        .expect("the port is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, mytag, port_atom, normal),
        port.to_eterm(),
    );
}

#[test]
fn a_tagged_monitor_of_a_closed_port_is_born_down_tagged() {
    let expr = r#"
        process_flag(trap_exit, true),
        Port = open_port({spawn, "cat"}, []),
        true = port_close(Port),
        receive _ -> ok end,
        _ = erlang:monitor(port, Port, [{tag, mytag}]),
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, is_port(T), A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{mytag,true,port,true,noproc}") {
        return;
    }

    let mut f = Fixture::new();
    let port_atom = f.atom("port");
    let mytag = f.atom("mytag");
    let noproc = f.atom_term("noproc");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut ports = PortTable::new();
    let port = ports.alloc().expect("room in the number space");
    // The port is already gone: the number was handed back, so nothing is watching it.
    ports.release(port);

    let reference = f
        .table
        .monitor_port_with_tag(watcher, port, &mut ports, Some(make_atom(mytag)))
        .expect("a non-live port is not an error");
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .port_monitors()
        .is_empty());
    // Not a recorded monitor, so it is not removable as one (`false`, not an error).
    assert!(!f
        .table
        .demonitor_port(watcher, reference, &mut ports)
        .expect("the watcher is live"));

    let payload = f.sole_payload(watcher);
    assert_immediate_target(
        assert_down(payload, mytag, port_atom, noproc),
        port.to_eterm(),
    );
}

// ---------------------------------------------------------------------------
// The capability gap: a non-immediate tag
// ---------------------------------------------------------------------------

#[test]
fn a_non_immediate_tag_is_a_typed_unsupported() {
    // The oracle accepts ANY term as a tag ...
    let expr = r#"
        T = {a},
        P = spawn(fun() -> receive go -> ok end end),
        _ = erlang:monitor(process, P, [{tag, T}]),
        P ! go,
        M = receive {H, R, K, Tt, A} -> {H =:= T, is_reference(R), K, is_pid(Tt), A}
            after 2000 -> timeout end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "{true,true,process,true,normal}") {
        return;
    }

    // ... but a monitor's tag is stored in a **plain-value side table** (`ProcessTable::tags`)
    // that the collector never scans, so a term resident in a per-process heap cannot be kept
    // valid across a collection. Refusing it with the typed `unsupported` is the honest answer:
    // a stored heap word would dangle and deliver a wrong `{Tag, …}` after a GC.
    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("a watcher process");

    // A boxed term on the watcher's own heap is the reproducer: it is not an immediate.
    let boxed_tag = f.tuple_in(watcher, &[make_small(1)]);
    assert!(!is_immediate(boxed_tag));
    let target = f.table.spawn_default().expect("a target process");

    assert_typed_unsupported(
        f.table
            .monitor_with_tag(watcher, target, MonitorKind::Process, Some(boxed_tag))
            .expect_err("a non-immediate tag cannot be stored"),
        UNSUPPORTED_TAG,
    );
    // A typed error, not a partial monitor: nothing was recorded on either side.
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .monitors()
        .is_empty());
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .mailbox()
        .is_empty());
    assert!(f
        .table
        .get(target)
        .expect("the target is live")
        .monitored_by()
        .is_empty());

    // The same rule on every tagged entry point.
    let name = f.atom("my_tag_server");
    f.table.register(name, target).expect("the name is free");
    let node = f.atom("nonode@nohost");
    assert_typed_unsupported(
        f.table
            .monitor_named_with_tag(
                watcher,
                name,
                node,
                MonitorKind::Process,
                Some(boxed_tag),
            )
            .expect_err("a non-immediate tag cannot be stored"),
        UNSUPPORTED_TAG,
    );
    let mut ports = PortTable::new();
    let port = ports.alloc().expect("room in the number space");
    assert_typed_unsupported(
        f.table
            .monitor_port_with_tag(watcher, port, &mut ports, Some(boxed_tag))
            .expect_err("a non-immediate tag cannot be stored"),
        UNSUPPORTED_TAG,
    );
    assert_typed_unsupported(
        f.table
            .monitor_port_term_with_tag(watcher, port.to_eterm(), &mut ports, Some(boxed_tag))
            .expect_err("a non-immediate tag cannot be stored"),
        UNSUPPORTED_TAG,
    );
    assert_typed_unsupported(
        f.table
            .monitor_term_with_tag(
                watcher,
                target.to_eterm(),
                MonitorKind::Process,
                Some(boxed_tag),
            )
            .expect_err("a non-immediate tag cannot be stored"),
        UNSUPPORTED_TAG,
    );
    // Nothing leaked out of the refused calls.
    assert!(ports.monitors(port).is_empty());
    assert!(f
        .table
        .get(watcher)
        .expect("the watcher is live")
        .port_monitors()
        .is_empty());
    assert!(f.table.get(watcher).expect("the watcher is live").mailbox().is_empty());
}
