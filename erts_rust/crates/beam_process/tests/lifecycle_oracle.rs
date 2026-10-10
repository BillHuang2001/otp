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

//! `erl`-cross-checked evidence for the **lifecycle / process-control semantics** this crate owns:
//! how a process's death reaches its links and monitors, the exit-reason *shapes* a trapping peer
//! observes, `erlang:exit/2` reason propagation, the local name registry's round trip, and the
//! `erlang:demonitor/1,2` return-value semantics.
//!
//! # What is pinned here, and what is not
//!
//! Every test runs the **same scenario twice** — under the real system `erl`
//! (OTP 29.1.1 / erts-17.1, the source of truth for the expected value) and through this crate's
//! [`ProcessTable`] — and asserts the crate's observable end state equals the oracle's. The cases
//! are the ones the existing `oracle_*` suites leave **unpinned**:
//!
//! * link exit propagation from a **live** linked origin (a non-trapping peer dies, a trapping
//!   peer survives with `{'EXIT', A, Reason}`) — [`oracle_link_exit_propagation_terminates_untrapping_and_traps_for_trapping`];
//! * **`kill` under `trap_exit`** — `exit(P, kill)` kills even a trapping `P` with reason `killed`
//!   (untrappable), and a trapping *link* peer of `P` receives `{'EXIT', P, killed}`
//!   ([`oracle_kill_under_trap_exit_is_untrappable_and_its_link_peer_sees_killed`]);
//! * the `noproc` / `normal` / `killed` exit-reason **shapes** at a trapping peer
//!   ([`oracle_exit_reason_shapes_at_a_trapping_peer`]) — including that an *explicit* `killed`
//!   reason is **trappable**, unlike the untrappable `kill` sentinel;
//! * `erlang:exit/2` propagating a **structured** reason verbatim, with `From` being the signaller
//!   ([`oracle_exit_2_propagates_a_structured_reason_to_a_trapping_peer`]);
//! * the name registry's `register`/`whereis`/`registered`/`unregister` round trip
//!   ([`oracle_register_whereis_registered_unregister_round_trip`]);
//! * the `erlang:demonitor/1,2` return value, incl. the `[info]` form, and — documented, not
//!   asserted for parity — the born-down `[info]` divergence
//!   ([`oracle_demonitor_info_bool_semantics`]);
//! * `demonitor(Ref, [flush])` removing an already-queued `{'DOWN', …}` for a **pid** target
//!   ([`oracle_demonitor_flush_removes_a_queued_down_for_a_pid_target`]).
//!
//! The cases already cross-checked elsewhere are deliberately **not** repeated here; a test cites
//! the existing pin rather than duplicating it (e.g. `link(DeadPid)` → `{'EXIT', Target, noproc}`
//! and the `register`/`unregister` `badarg` forms are pinned by
//! `tests/registry_monitors.rs::oracle_link_to_a_non_live_pid_signals_noproc_from_it` and
//! `::oracle_register_and_unregister_raise_badarg`).
//!
//! # Determinism (no sleeps, anywhere)
//!
//! Every oracle expression synchronizes on a message (`Parent ! {w, self()}` / `receive …`)
//! rather than a delay, so the interleaving is forced and not raced. A `receive … after T` guard
//! appears only as a *negative* backstop on a path the positive assertion already forces; every
//! Rust-side assertion is a plain deterministic table operation with no wall-clock component.
//! Variable names are kept distinct per receive in the `-eval` text so no earlier binding is
//! silently re-used as a pattern (a real hazard single-expression probes hit).
//!
//! # Absence of the oracle is not a failure
//!
//! If `erl` is not on `PATH` (or does not answer within [`common::ERL_TIMEOUT`]) every oracle
//! check **skips loudly** (a clear `eprintln!` from the shared driver) and returns, never failing
//! the suite.

mod common;

use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{
    make_atom, make_small, tuple_element, tuple_size, Atom, AtomTable, Eterm,
};

use common::expect_oracle;

// The oracle driver (`ERL_TIMEOUT`, `erl_stdout`, `expect_oracle` and the stdout normalisation) is
// shared with the crate's other `oracle_*` files; it lives in `tests/common/mod.rs` so the
// logger-suppression fix exists once. `erl` runs with `-kernel logger_level none` and its stdout is
// normalised, so a crashed probe helper's run-time error report can never make a comparison flaky.

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A process table plus the atom table its [`SignalAtoms`] were interned from, so a test can
/// resolve the well-known message atoms (`'EXIT'`, `'DOWN'`, `process`, `normal`, …) back to terms
/// in the same index space the table's builders use.
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

    /// Spawns a process and turns on `trap_exit` for it, so an exit signal is delivered as a
    /// message rather than terminating it.
    fn spawn_trapping(&mut self) -> ProcessId {
        let pid = self.table.spawn_default().expect("a process is available");
        self.table
            .get_mut(pid)
            .expect("the fresh pid is live")
            .set_trap_exit(true);
        pid
    }

    /// Builds a tuple on `pid`'s own heap (a *structured*, non-immediate exit reason/expectation).
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

    /// The mailbox length of `pid`.
    fn mailbox_len(&self, pid: ProcessId) -> usize {
        self.table.get(pid).expect("the recipient is live").mailbox().len()
    }
}

/// Asserts `payload` is the 3-tuple `{'EXIT', from, reason}` and structurally equal to `expected`
/// (a distinct, equivalent term the caller built — a copied reason is never the source word).
fn assert_exit_message(payload: Eterm, exit_atom: Atom, from: Eterm, expected_reason: Eterm, names: &AtomTable) {
    // SAFETY: `exit_atom` comes from the same table that interned the message's atoms, and
    // `payload` is the live 3-tuple the table built on the recipient's heap; the three element
    // slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3, "an EXIT message is a 3-tuple");
        assert_eq!(tuple_element(payload, 0), make_atom(exit_atom));
        assert_eq!(tuple_element(payload, 1), from, "the signal came *from* the expected pid");
        let delivered = tuple_element(payload, 2);
        assert!(
            compare::eq(delivered, expected_reason, names),
            "the delivered reason is structurally the expected one"
        );
    }
}

/// Asserts `payload` is the 5-tuple `{'DOWN', <ref>, process, target, reason}` with the reason
/// **structurally** equal to `expected`.
///
/// `compare::eq` (not `==`) is used for the reason so a caller may pass either an immediate atom
/// (`normal`/`noproc`/`killed`) or a distinct but structurally equal *structured* term — a copied
/// reason is never the source word, so raw word equality would be wrong for the structured case.
fn assert_pid_down(
    payload: Eterm,
    down_atom: Atom,
    tag_atom: Atom,
    target: Eterm,
    expected_reason: Eterm,
    names: &AtomTable,
) {
    // SAFETY: `down_atom`/`tag_atom` come from the same table that interned the message's atoms,
    // and `payload` is the live 5-tuple the table built on the recipient's heap; the five element
    // slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5, "a DOWN message is a 5-tuple");
        assert_eq!(tuple_element(payload, 0), make_atom(down_atom));
        assert_eq!(tuple_element(payload, 2), make_atom(tag_atom));
        assert_eq!(tuple_element(payload, 3), target);
        let delivered = tuple_element(payload, 4);
        assert!(
            compare::eq(delivered, expected_reason, names),
            "the delivered DOWN reason is structurally the expected one"
        );
    }
}

// ---------------------------------------------------------------------------
// Link exit propagation (live linked origin)
// ---------------------------------------------------------------------------

/// A live linked origin `A` dying **abnormally** with a structured reason `{bad,1}` terminates a
/// non-trapping linked peer `N` with that same reason (observable as `N`'s DOWN reason) and is
/// delivered to a trapping linked peer `W` as `{'EXIT', A, {bad,1}}`.
///
/// This is the *live* target the existing `oracle_link_to_a_non_live_pid_signals_noproc_from_it`
/// pin does not cover (it is about `link` to an already-dead pid).
#[test]
fn oracle_link_exit_propagation_terminates_untrapping_and_traps_for_trapping() {
    let expr = r#"
        Parent = self(),
        A = spawn(fun() -> receive go -> exit({bad, 1}) end end),
        W = spawn(fun() -> process_flag(trap_exit, true), link(A), Parent ! {w, self()},
                          receive X -> Parent ! {got, X} after 2000 -> Parent ! {got, timeout} end end),
        N = spawn(fun() -> link(A), Parent ! {n, self()}, receive never -> ok end end),
        receive {w, _} -> ok end,
        receive {n, _} -> ok end,
        NM = erlang:monitor(process, N),
        A ! go,
        Got = receive {got, M} -> M end,
        NReason = receive {'DOWN', NM, process, N, Rn} -> Rn after 2000 -> timeout end,
        io:format("~p|~p~n", [Got =:= {'EXIT', A, {bad,1}}, NReason])
    "#;
    if !expect_oracle(expr, "true|{bad,1}") {
        return;
    }

    // The Rust side of the same statement: `W` traps and survives with the EXIT, `N` does not and
    // dies with the origin's reason, and the watcher's DOWN carries that same reason.
    let mut f = Fixture::new();
    let exit_atom = f.atom("EXIT");
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let bad = f.atom_term("bad");

    let a = f.table.spawn_default().expect("the origin");
    let w = f.spawn_trapping();
    let n = f.table.spawn_default().expect("the non-trapping peer");
    let watcher = f.table.spawn_default().expect("the watcher");
    let holder = f.table.spawn_default().expect("holds the expected reason");
    f.table.link(w, a).expect("both pids are live");
    f.table.link(n, a).expect("both pids are live");
    // The watcher is not linked to the origin, so only the DOWN (not an EXIT) reaches it.
    f.table
        .monitor(watcher, n, MonitorKind::Process)
        .expect("both pids are live");

    // The reason is a structured tuple living in the dying process's own heap; `expected` is a
    // distinct but structurally equal tuple, so an accidental raw-word share would be caught.
    let reason = f.tuple_in(a, &[bad, make_small(1)]);
    let expected = f.tuple_in(holder, &[bad, make_small(1)]);

    f.table.terminate(a, reason).expect("the origin is live");

    assert!(!f.table.contains(a), "the origin is gone");
    assert!(
        !f.table.contains(n),
        "a non-trapping linked peer follows the abnormal origin"
    );
    assert!(f.table.contains(w), "the trapping peer survives");

    let payload = f.sole_payload(w);
    assert_exit_message(payload, exit_atom, a.to_eterm(), expected, &f.names);

    let payload = f.sole_payload(watcher);
    assert_pid_down(payload, down, process, n.to_eterm(), expected, &f.names);
}

// ---------------------------------------------------------------------------
// `kill` under `trap_exit`
// ---------------------------------------------------------------------------

/// `exit(P, kill)` is **untrappable**: even a `trap_exit` process `P` dies, with reason `killed`
/// (never the sent `kill`), and a trapping *link* peer of `P` receives `{'EXIT', P, killed}`.
///
/// This extends the existing DOWN-reason pin (`registry_monitors::oracle_down_reason_fidelity`
/// covers a `killed` DOWN) with the two observables it does not: `P`'s death despite `trap_exit`,
/// and the **link** peer's EXIT shape.
#[test]
fn oracle_kill_under_trap_exit_is_untrappable_and_its_link_peer_sees_killed() {
    let expr = r#"
        Parent = self(),
        P = spawn(fun() -> process_flag(trap_exit, true), receive never -> ok end end),
        W = spawn(fun() -> process_flag(trap_exit, true), link(P), Parent ! {w, self()},
                          receive X -> Parent ! {got, X} after 2000 -> Parent ! {got, timeout} end end),
        receive {w, _} -> ok end,
        M = erlang:monitor(process, P),
        exit(P, kill),
        Alive = is_process_alive(P),
        PM = receive {'DOWN', M, process, P, Rn} -> Rn after 2000 -> timeout end,
        WM = receive {got, X} -> X after 2000 -> timeout end,
        io:format("~p|~p|~p~n", [Alive, PM, WM =:= {'EXIT', P, killed}])
    "#;
    if !expect_oracle(expr, "false|killed|true") {
        return;
    }

    let mut f = Fixture::new();
    let exit_atom = f.atom("EXIT");
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let kill = f.atom_term("kill");
    let killed = f.atom_term("killed");

    let killer = f.table.spawn_default().expect("the signaller");
    let p = f.spawn_trapping();
    let w = f.spawn_trapping();
    let watcher = f.table.spawn_default().expect("the watcher");
    f.table.link(w, p).expect("both pids are live");
    f.table
        .monitor(watcher, p, MonitorKind::Process)
        .expect("both pids are live");

    f.table.send_exit(killer, p, kill).expect("the target is live");

    assert!(
        !f.table.contains(p),
        "`kill` terminates even a trapping target (untrappable)"
    );
    assert!(f.table.contains(w), "the trapping link peer survives");

    let payload = f.sole_payload(w);
    assert_exit_message(payload, exit_atom, p.to_eterm(), killed, &f.names);

    let payload = f.sole_payload(watcher);
    assert_pid_down(payload, down, process, p.to_eterm(), killed, &f.names);
}

// ---------------------------------------------------------------------------
// The exit-reason shapes at a trapping peer
// ---------------------------------------------------------------------------

/// At a trapping peer, `erlang:exit(P, Reason)` is delivered as `{'EXIT', From, Reason}` for the
/// bare reasons `noproc`, `normal` and `killed`, with `From` being the signaller. The target
/// survives in all three cases — in particular an *explicit* `killed` reason is **trappable**,
/// unlike the `kill` sentinel of the previous test.
///
/// (The `{'EXIT', Target, noproc}` shape from `link/1` to a **non-live** pid is pinned by
/// `registry_monitors::oracle_link_to_a_non_live_pid_signals_noproc_from_it` and is not repeated.)
#[test]
fn oracle_exit_reason_shapes_at_a_trapping_peer() {
    let expr = r#"
        Parent = self(),
        Mk = fun() -> spawn(fun() -> process_flag(trap_exit, true), Parent ! {ready, self()},
                                  receive {'EXIT', _, _} = Ex -> Parent ! {got, Ex},
                                          receive stop -> ok end end end) end,
        P1 = Mk(), receive {ready, _} -> ok end, exit(P1, noproc),
        G1 = receive {got, Gx1} -> Gx1 end, A1 = is_process_alive(P1), P1 ! stop,
        P2 = Mk(), receive {ready, _} -> ok end, exit(P2, normal),
        G2 = receive {got, Gx2} -> Gx2 end, A2 = is_process_alive(P2), P2 ! stop,
        P3 = Mk(), receive {ready, _} -> ok end, exit(P3, killed),
        G3 = receive {got, Gx3} -> Gx3 end, A3 = is_process_alive(P3), P3 ! stop,
        io:format("~p|~p|~p|~p|~p|~p|~p~n",
                  [element(3, G1), element(2, G1) =:= self(), A1,
                   element(3, G2), A2, element(3, G3), A3])
    "#;
    if !expect_oracle(expr, "noproc|true|true|normal|true|killed|true") {
        return;
    }

    let mut f = Fixture::new();
    let exit_atom = f.atom("EXIT");
    let from = f.table.spawn_default().expect("the signaller");

    for reason in ["noproc", "normal", "killed"] {
        let target = f.spawn_trapping();
        let reason_term = f.atom_term(reason);
        let from_term = from.to_eterm();

        f.table
            .send_exit(from, target, reason_term)
            .expect("the target is live");

        assert!(
            f.table.contains(target),
            "`{reason}` is trapped: the target survives"
        );
        assert_eq!(
            f.table.pending_signals(target).expect("the target is live"),
            0,
            "the signal was serviced eagerly (the target is not the running process)"
        );
        let payload = f.sole_payload(target);
        assert_exit_message(payload, exit_atom, from_term, reason_term, &f.names);
    }
}

// ---------------------------------------------------------------------------
// `erlang:exit/2` reason propagation
// ---------------------------------------------------------------------------

/// `erlang:exit(P, {shutdown, 42})` reaching a trapping `P` hands it `{'EXIT', From, {shutdown,42}}`
/// with `From` the signaller, and `P` survives — the reason term is propagated verbatim, not
/// rewritten.
#[test]
fn oracle_exit_2_propagates_a_structured_reason_to_a_trapping_peer() {
    let expr = r#"
        Parent = self(),
        P = spawn(fun() -> process_flag(trap_exit, true), Parent ! {ready, self()},
                          receive {'EXIT', _, _} = Ex -> Parent ! {got, Ex},
                                  receive stop -> ok end end end),
        receive {ready, _} -> ok end,
        exit(P, {shutdown, 42}),
        Got = receive {got, Gx} -> Gx end,
        io:format("~p|~p~n", [Got =:= {'EXIT', self(), {shutdown, 42}}, is_process_alive(P)])
    "#;
    if !expect_oracle(expr, "true|true") {
        return;
    }

    let mut f = Fixture::new();
    let exit_atom = f.atom("EXIT");
    let shutdown = f.atom_term("shutdown");

    let from = f.table.spawn_default().expect("the signaller");
    let target = f.spawn_trapping();
    let holder = f.table.spawn_default().expect("holds the expected reason");

    // The reason is a *structured* tuple built in the signaller's own heap.
    let reason = f.tuple_in(from, &[shutdown, make_small(42)]);
    let expected = f.tuple_in(holder, &[shutdown, make_small(42)]);
    let from_term = from.to_eterm();

    f.table
        .send_exit(from, target, reason)
        .expect("the target is live");

    assert!(f.table.contains(target), "the trapping target survives");
    let payload = f.sole_payload(target);
    assert_exit_message(payload, exit_atom, from_term, expected, &f.names);
}

// ---------------------------------------------------------------------------
// The local name registry
// ---------------------------------------------------------------------------

/// `register/2` then `whereis/1`, `registered/0` and `unregister/1` form a round trip: the name
/// resolves to the registered pid, is a member of `registered()`, `unregister/1` answers `true`
/// and the name then resolves to `undefined`.
///
/// (The `badarg` forms — `register/2` of a dead pid, `unregister/1` of an unknown name — are
/// pinned by `registry_monitors::oracle_register_and_unregister_raise_badarg` and are not
/// repeated here.)
#[test]
fn oracle_register_whereis_registered_unregister_round_trip() {
    let expr = r#"
        register(rn_test, self()),
        W1 = whereis(rn_test),
        Member = lists:member(rn_test, registered()),
        U = unregister(rn_test),
        W2 = whereis(rn_test),
        io:format("~p|~p|~p|~p~n", [W1 =:= self(), Member, U, W2])
    "#;
    if !expect_oracle(expr, "true|true|true|undefined") {
        return;
    }

    let mut f = Fixture::new();
    let name = f.atom("rn_test");
    let pid = f.table.spawn_default().expect("a live process");

    f.table.register(name, pid).expect("the name is free");
    assert_eq!(f.table.whereis(name), Some(pid), "`whereis` resolves the name");
    assert!(
        f.table.registered().any(|(n, p)| n == name && p == pid),
        "`registered()` lists the `{{Name, Pid}}` pair"
    );

    f.table.unregister(name).expect("the name was registered");
    assert_eq!(f.table.whereis(name), None, "`whereis` of a released name is `undefined`");
}

// ---------------------------------------------------------------------------
// `erlang:demonitor/1,2` return values
// ---------------------------------------------------------------------------

/// The `erlang:demonitor/1,2` return values: the `[info]` option answers `true` for an **active**
/// monitor and `false` for one that is already gone; the plain `demonitor(Ref)` form answers
/// `true` unconditionally (the BIF layer's rendering, *not* this table's "was it on record"
/// boolean).
///
/// [`ProcessTable::demonitor`]'s `bool` is exactly the `[info]` semantics ("an active local
/// monitor was found"); the plain form's unconditional `true` belongs to the BIF layer
/// (`beam_bifs`/`beam_rt`), which is why only the `[info]` column is asserted against the table.
///
/// # The born-down `[info]` divergence (documented, not asserted for parity)
///
/// For a monitor on a pid that is already dead the oracle answers `[info]` with `true` when the
/// `{'DOWN', …}` is still sitting in its **signal queue** (it has not been consumed yet). In this
/// crate a monitor on a non-live pid is **born down** — the DOWN is materialised immediately and
/// **no monitor entry is recorded** (there is nothing left to watch; see
/// [`ProcessTable::monitor`]), so [`ProcessTable::demonitor`] answers `false`. The divergence is
/// therefore a consequence of the crate's *eager* born-down delivery, not a wrong result, and is
/// asserted below as the crate's own deterministic value rather than against the oracle.
#[test]
fn oracle_demonitor_info_bool_semantics() {
    let expr = r#"
        P = spawn(fun() -> receive stop -> ok end end),
        M = erlang:monitor(process, P),
        D1 = erlang:demonitor(M, [info]),
        D2 = erlang:demonitor(M, [info]),
        M2 = erlang:monitor(process, P),
        D3 = erlang:demonitor(M2),
        D4 = erlang:demonitor(M2, [info]),
        io:format("~p|~p|~p|~p~n", [D1, D2, D3, D4])
    "#;
    if !expect_oracle(expr, "true|false|true|false") {
        return;
    }

    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("the watcher");
    let target = f.table.spawn_default().expect("the live target");

    // `[info]`: true for an active monitor, false once it is gone. This is the table's own bool.
    let reference = f
        .table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");
    assert!(
        f.table
            .demonitor(watcher, reference)
            .expect("the watcher is live"),
        "`demonitor(Ref, [info])` => true: an active monitor was on record"
    );
    assert!(
        !f.table
            .demonitor(watcher, reference)
            .expect("the watcher is live"),
        "`demonitor(Ref, [info])` => false: the monitor is no longer on record"
    );

    // The plain form maps to the same call here; its unconditional `true` is the BIF layer's.
    let second = f
        .table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");
    assert!(f
        .table
        .demonitor(watcher, second)
        .expect("the watcher is live"));
    assert!(!f
        .table
        .demonitor(watcher, second)
        .expect("the watcher is live"));

    // Born-down divergence (see the doc comment): the crate records no entry, so `demonitor`
    // answers `false` where the oracle's still-queued DOWN makes `[info]` answer `true`.
    let dead = f.table.spawn_default().expect("a process about to die");
    f.table.remove(dead).expect("it was live");
    let born = f
        .table
        .monitor(watcher, dead, MonitorKind::Process)
        .expect("a non-live target is not an error");
    assert_eq!(
        f.mailbox_len(watcher),
        1,
        "the born-down DOWN is already in the mailbox"
    );
    assert!(
        !f.table
            .demonitor(watcher, born)
            .expect("the watcher is live"),
        "no monitor entry was recorded for the born-down, so `demonitor` is false \
         (documented divergence from the oracle's still-queued `[info]` => true)"
    );
}

// ---------------------------------------------------------------------------
// `erlang:demonitor/2` with `[flush]`
// ---------------------------------------------------------------------------

/// `demonitor(Ref, [flush])` removes an already-queued `{'DOWN', Ref, …}` from the watcher's
/// mailbox (here for a **pid** target — the named-target flush is pinned by
/// `registry_monitors::oracle_demonitor_flush_drops_a_queued_down`), answering `true` and
/// dropping `message_queue_len` from 1 to 0.
#[test]
fn oracle_demonitor_flush_removes_a_queued_down_for_a_pid_target() {
    let expr = r#"
        P = spawn(fun() -> ok end),
        PM = erlang:monitor(process, P),
        receive {'DOWN', PM, _, _, _} -> ok end,
        M = erlang:monitor(process, P),
        receive after 0 -> ok end,
        {message_queue_len, L0} = process_info(self(), message_queue_len),
        D = erlang:demonitor(M, [flush]),
        {message_queue_len, L1} = process_info(self(), message_queue_len),
        io:format("~p|~p|~p~n", [L0, D, L1])
    "#;
    if !expect_oracle(expr, "1|true|0") {
        return;
    }

    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("the watcher");
    let dead = f.table.spawn_default().expect("a process about to die");
    f.table.remove(dead).expect("it was live");

    let reference = f
        .table
        .monitor(watcher, dead, MonitorKind::Process)
        .expect("a non-live target is not an error");
    assert_eq!(f.mailbox_len(watcher), 1, "`{{message_queue_len, 1}}`: the queued DOWN");

    assert!(
        f.table
            .demonitor_flush(watcher, reference)
            .expect("the watcher is live"),
        "`demonitor(Ref, [flush])` => true"
    );
    assert_eq!(
        f.mailbox_len(watcher),
        0,
        "`{{message_queue_len, 0}}`: the flush removed the queued DOWN"
    );
}
