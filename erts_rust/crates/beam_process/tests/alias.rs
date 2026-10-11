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

//! Integration evidence for **reference aliases** — `erlang:alias/1`, `erlang:unalias/1` and the
//! `erlang:monitor/3` `{alias, Mode}` option.
//!
//! An **alias** is a reference a process may be **sent to**: `Alias ! Msg` is delivered to the
//! process that created the alias, and the alias **expires** according to its [`UnaliasMode`].
//! `ProcessTable::create_alias` mints a **standalone** alias (the `erlang:alias/1` operation, with
//! no monitor behind it); `MonitorOptions::alias = Some(mode)` makes a monitor's own reference
//! double as one. `ProcessTable::resolve_alias` is the send path's lookup (its `None` is the "not
//! a live alias" answer); `ProcessTable::unalias`, the DOWN site and
//! `ProcessTable::alias_reply_delivered` are the three expiry sites.
//!
//! # Why the oracle cross-check is meaningful here
//!
//! The whole feature is a **mode-dependent expiry rule**, and the oracle is where those rules are
//! defined. The crate keeps the alias's owner and mode in a **plain-value side table**
//! (`ProcessTable::aliases`) that has no one-to-one C counterpart to copy — the C packs the creator
//! pid into the reference itself (`erts_make_pid_ref`) and the mode into the monitor entry's flags
//! — so every `*_with_options` rule is pinned here against what `erl` really does:
//! `explicit_unalias` survives both the DOWN and `demonitor/1`; `demonitor` expires at the DOWN;
//! `reply_demonitor` expires at the DOWN **and** at the reply.
//!
//! # Two oracle renditions this file is careful about
//!
//! * **`erlang:demonitor/1` renders the internal `am_false` as `true`.** The C's `demonitor_1`
//!   (`erts/emulator/beam/bif.c:598-611`) maps *both* `am_false` and `am_true` onto `am_true`; only
//!   `demonitor(Ref, [info])` (`bif.c:642-645`) passes the internal verdict through. So the
//!   discriminating probe for "was there a monitor to remove?" is `demonitor(Ref, [info])`, and
//!   that is what the assertions below compare against `ProcessTable::demonitor`'s `bool` (which
//!   *is* the internal verdict, not the BIF's rendering).
//! * **The send path itself is not this crate's.** `ProcessTable::resolve_alias` is the seam
//!   `beam_rt`'s `!` uses; `beam_process` implements no send. The oracle drops a `Ref ! Msg` whose
//!   reference is not a live alias silently (it answers the message, delivering nothing), so the
//!   crate's observable for "the alias is gone" is `resolve_alias` returning `None`, and the
//!   oracle's is "a message through it no longer arrives".
//!
//! # Absence of the oracle is not a failure
//!
//! If `erl` is not on `PATH` (or does not answer within [`common::ERL_TIMEOUT`]) every `oracle_*`
//! check **skips loudly** (a clear `eprintln!`) and returns, never failing the suite.

mod common;

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{
    external_pid_size_words, make_atom, make_header, make_small, tuple_element, tuple_size,
    write_external_pid, Atom, AtomTable, Eterm, REF_SUBTAG,
};

use common::expect_oracle;

/// The one `unsupported:` reason naming an `{alias, Mode}` option on a **port** monitor.
const UNSUPPORTED_PORT_ALIAS: &str =
    "unsupported: monitor/3: {alias, Mode} on a port monitor (the DOWN and reply paths hold no port table)";

/// The one `unsupported:` reason naming an `{alias, Mode}` option on a **remote** target.
const UNSUPPORTED_REMOTE_ALIAS: &str =
    "unsupported: monitor/3: {alias, Mode} on a remote target (DOP_MONITOR_P carries no alias)";

// The oracle driver (`ERL_TIMEOUT`, `erl_stdout`, `expect_oracle` and the stdout normalisation) is
// shared with the crate's other `oracle_*` files; it lives in `tests/common/mod.rs` so the
// logger-suppression fix exists once. `erl` runs with `-kernel logger_level none` and its stdout is
// normalised, so a crashed probe helper's run-time error report can never make a comparison flaky.

// ---------------------------------------------------------------------------
// Fixture
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
        Fixture { table: ProcessTable::new(atoms), names }
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

    /// A **process** monitor from `watcher` on `target` carrying only the `{alias, Mode}` option —
    /// `ProcessTable::monitor_with_options` with no `{tag, Tag}` (that half has its own file,
    /// `tests/monitor_tag.rs`).
    fn monitor_with_alias(
        &mut self,
        watcher: ProcessId,
        target: ProcessId,
        mode: UnaliasMode,
    ) -> MonitorRef {
        self.table
            .monitor_with_options(
                watcher,
                target,
                MonitorKind::Process,
                MonitorOptions { tag: None, alias: Some(mode) },
            )
            .expect("both pids are live")
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
        let drained = self.table.get_mut(pid).expect("the recipient is live").mailbox_mut().dequeue();
        assert!(drained.is_some(), "a message was queued");
    }

    /// Spawns a process and drops it again, so its pid is a **non-live** target (the `noproc` case
    /// every born-down path is about).
    fn dead_pid(&mut self) -> ProcessId {
        let dying = self.table.spawn_default().expect("a process about to die");
        self.table.remove(dying);
        dying
    }
}

/// Asserts `payload` is the 5-tuple `{head, <a ref>, kind, target, reason}` and returns its
/// **target** element (element 3).
///
/// This is the one shape every DOWN builder produces — `{'DOWN', …}` when `head` is the interned
/// `'DOWN'` atom, `{Tag, …}` when it is the monitor's user tag. The target is returned rather than
/// compared, because a caller's *structured* target (`{Name, Node}`, built by [`Fixture::tuple_in`])
/// is a structurally equal but distinct boxed term and must be compared with
/// `beam_term::compare::eq`.
fn assert_down(payload: Eterm, head: Atom, kind: Atom, reason: Eterm) -> Eterm {
    // SAFETY: `head`/`kind` come from the same table that interned the message's atoms, and
    // `payload` is the live 5-tuple the table built on the recipient's heap; the five element slots
    // are in bounds.
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

/// Builds an external pid term on `heap` (the frozen 4-word `[header, node, creation, data]` thing)
/// — the term `erlang:monitor(process, RemotePid)` receives.
fn external_pid(heap: &mut Heap, node: Eterm) -> Eterm {
    // SAFETY: `heap` hands out `external_pid_size_words()` (4) exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(external_pid_size_words()).expect("the heap has room") };
    // SAFETY: `p` addresses the four words just handed out and `write_external_pid` writes all of
    // them before the pointer escapes; `node` is a live atom term.
    unsafe { write_external_pid(p, node, 11, 22, 33) }
}

// ---------------------------------------------------------------------------
// 1. The mint/resolve/unalias round trip
// ---------------------------------------------------------------------------

#[test]
fn oracle_an_alias_resolves_to_its_owner_and_unalias_reports_true_then_false() {
    // One probe covers three facts: a message sent to an `alias()` reaches the process that created
    // it, and `unalias/1` answers `true` exactly once.
    let expr = r#"
        A = alias(),
        A ! hello,
        M = receive X -> X end,
        T1 = unalias(A),
        T2 = unalias(A),
        io:format("~p|~p|~p~n", [T1, T2, M])
    "#;
    if !expect_oracle(expr, "true|false|hello") {
        return;
    }

    let mut f = Fixture::new();
    let owner = f.table.spawn_default().expect("an alias owner");
    let reference =
        f.table.create_alias(owner, UnaliasMode::ExplicitUnalias).expect("a live owner mints one");

    // `resolve_alias` is the send path's lookup: the alias delivers to its creator.
    assert_eq!(f.table.resolve_alias(reference), Some(owner), "an alias resolves to its creator");

    // `unalias/1` answers `true` exactly once. `create_alias` returns a **fresh** reference (the
    // `RefAllocator` never reuses an id), so two aliases can never collide.
    assert!(f.table.unalias(owner, reference).expect("a live owner"));
    assert_eq!(f.table.resolve_alias(reference), None, "a cleared alias is no destination");
    assert!(!f.table.unalias(owner, reference).expect("a live owner is not an error"));

    // A second alias from the same owner gets its own reference.
    let other = f.table.create_alias(owner, UnaliasMode::ExplicitUnalias).expect("a live owner");
    assert_ne!(other, reference, "each alias gets its own reference");
    assert_eq!(f.table.resolve_alias(other), Some(owner));

    // `unalias/1` of a reference belonging to **another** process is `false`, not an error — the
    // mirror of the oracle's `unalias(RefFromAnotherProcess)`.
    let stranger = f.table.spawn_default().expect("a stranger");
    assert!(!f.table.unalias(stranger, other).expect("a live stranger is not an error"));
    assert_eq!(f.table.resolve_alias(other), Some(owner), "the refused unalias left it alone");

    // A dead owner cannot mint one (`TermNotFound`).
    let corpse = f.dead_pid();
    assert!(matches!(
        f.table.create_alias(corpse, UnaliasMode::ExplicitUnalias),
        Err(BeamError::TermNotFound)
    ));
}

// ---------------------------------------------------------------------------
// 2. The DOWN site
// ---------------------------------------------------------------------------

#[test]
fn oracle_a_monitored_targets_down_expires_every_alias_mode_but_explicit_unalias() {
    // One probe, three modes: what a message sent to the monitor's reference does **after** the
    // target's `normal` DOWN has been delivered, and what `unalias/1` says then.
    let expr = r#"
        F = fun(Mode) ->
              P = spawn(fun() -> receive go -> ok end end),
              R = erlang:monitor(process, P, [{alias, Mode}]),
              P ! go,
              receive {H, R, process, P, normal} when is_atom(H) -> ok
                  after 2000 -> timeout end,
              R ! after_down,
              M = receive X -> X after 400 -> none end,
              {M, unalias(R), unalias(R)}
            end,
        io:format("~p|~p|~p~n", [F(explicit_unalias), F(demonitor), F(reply_demonitor)])
    "#;
    if !expect_oracle(expr, "{after_down,true,false}|{none,false,false}|{none,false,false}") {
        return;
    }

    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let normal = f.atom_term("normal");

    // `ExplicitUnalias` survives its target's death and lives on as a **standalone** alias.
    let watcher = f.table.spawn_default().expect("a watcher");
    let target = f.table.spawn_default().expect("a target");
    let reference = f
        .table
        .monitor_with_options(
            watcher,
            target,
            MonitorKind::Process,
            MonitorOptions { tag: None, alias: Some(UnaliasMode::ExplicitUnalias) },
        )
        .expect("both pids are live");
    assert_eq!(f.table.resolve_alias(reference), Some(watcher));
    f.table.terminate(target, normal).expect("the target is live");

    // The DOWN still arrives, and the alias outlives it.
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, down, process, normal), target.to_eterm());
    f.drain_one(watcher);
    assert_eq!(f.table.resolve_alias(reference), Some(watcher), "explicit_unalias survives");
    // ... and is removed only by an explicit `unalias/1` — after which the monitor is already
    // resolved, so `demonitor` finds nothing either.
    assert!(f.table.unalias(watcher, reference).expect("a live watcher"));
    assert_eq!(f.table.resolve_alias(reference), None);
    assert!(!f.table.demonitor(watcher, reference).expect("a live watcher"));

    // `Demonitor` and `ReplyDemonitor` die with the monitor.
    for mode in [UnaliasMode::Demonitor, UnaliasMode::ReplyDemonitor] {
        let watcher = f.table.spawn_default().expect("a watcher");
        let target = f.table.spawn_default().expect("a target");
        let reference = f.monitor_with_alias(watcher, target, mode);
        assert_eq!(f.table.resolve_alias(reference), Some(watcher));
        f.table.terminate(target, normal).expect("the target is live");
        let payload = f.sole_payload(watcher);
        assert_immediate_target(assert_down(payload, down, process, normal), target.to_eterm());
        f.drain_one(watcher);
        assert_eq!(f.table.resolve_alias(reference), None, "{mode:?} expires with its DOWN");
        assert!(!f.table.unalias(watcher, reference).expect("a live watcher"));
    }
}

// ---------------------------------------------------------------------------
// 3. The reply site
// ---------------------------------------------------------------------------

#[test]
fn oracle_a_reply_through_a_reply_demonitor_alias_consumes_the_alias_and_its_monitor() {
    // The reply site's own probe: one message through the alias, then what `unalias/1` and
    // `demonitor(Ref, [info])` say. Only `reply_demonitor` loses its monitor; the other two modes
    // keep both (an `unalias` would clear the alias but not the monitor, so the monitor still
    // answers `true`).
    let expr = r#"
        F = fun(Mode) ->
              P = spawn(fun() -> receive go -> ok end end),
              R = erlang:monitor(process, P, [{alias, Mode}]),
              R ! reply_msg,
              M = receive X -> X end,
              {M, unalias(R), erlang:demonitor(R, [info])}
            end,
        io:format("~p|~p|~p~n", [F(reply_demonitor), F(demonitor), F(explicit_unalias)])
    "#;
    let expected = "{reply_msg,false,false}|{reply_msg,true,true}|{reply_msg,true,true}";
    if !expect_oracle(expr, expected) {
        return;
    }

    let mut f = Fixture::new();

    // `ReplyDemonitor`: the reply removes the alias **and** the sharing monitor — so nothing is left
    // to watch the target, and no DOWN can ever arrive.
    let watcher = f.table.spawn_default().expect("a watcher");
    let target = f.table.spawn_default().expect("a target");
    let reference = f.monitor_with_alias(watcher, target, UnaliasMode::ReplyDemonitor);
    f.table.alias_reply_delivered(reference).expect("a live owner consumes the alias");
    assert_eq!(f.table.resolve_alias(reference), None, "the reply consumed the alias");
    // The monitor half is gone on **both** sides — the `process_info`-equivalent state and a later
    // `demonitor`, which now reports `false`: the internal verdict. (`erlang:demonitor/1` would
    // render that as `true`, which is exactly why the probe above uses the `[info]` form.)
    assert!(f.table.get(watcher).expect("live").monitors().find(reference).is_none());
    assert!(f
        .table
        .get(target)
        .expect("live")
        .monitored_by()
        .iter()
        .all(|entry| entry.reference != reference));
    assert!(!f.table.demonitor(watcher, reference).expect("the watcher is live"));
    // Nothing watches the target any more, so its death produces no DOWN.
    let normal = f.atom_term("normal");
    f.table.terminate(target, normal).expect("the target is live");
    assert!(f.table.get(watcher).expect("live").mailbox().is_empty());

    // `Demonitor` and `ExplicitUnalias` are **not** consumed by a delivery: alias and monitor both
    // survive, so the monitor is still there to remove.
    for mode in [UnaliasMode::Demonitor, UnaliasMode::ExplicitUnalias] {
        let watcher = f.table.spawn_default().expect("a watcher");
        let target = f.table.spawn_default().expect("a target");
        let reference = f.monitor_with_alias(watcher, target, mode);
        f.table.alias_reply_delivered(reference).expect("a no-op for this mode");
        assert_eq!(f.table.resolve_alias(reference), Some(watcher), "{mode:?} survives a reply");
        assert!(f.table.get(watcher).expect("live").monitors().find(reference).is_some());
        assert!(f.table.demonitor(watcher, reference).expect("the watcher is live"));
    }

    // A delivery through a reference the table never minted is a no-op, not an error.
    f.table.alias_reply_delivered(RefAllocator::new().next()).expect("an unknown ref is a no-op");
}

// ---------------------------------------------------------------------------
// 4. The explicit `demonitor/1` site
// ---------------------------------------------------------------------------

#[test]
fn oracle_demonitor_keeps_only_an_explicit_unalias_alias() {
    // The oracle-verified **deviation** from a naive reading of the rule: `demonitor(Ref)` does
    // **not** unconditionally drop the alias. The C's `demonitor` replaces an
    // `ERTS_ML_STATE_ALIAS_UNALIAS` monitor entry with a standalone `ERTS_MON_TYPE_ALIAS` one
    // (`bif.c:470-484`), so only `demonitor`/`reply_demonitor` expire here. The `[info]` form is
    // used because `erlang:demonitor/1` renders the internal `false` as `true`.
    let expr = r#"
        F = fun(Mode) ->
              P = spawn(fun() -> receive go -> ok end end),
              R = erlang:monitor(process, P, [{alias, Mode}]),
              D = erlang:demonitor(R, [info]),
              T = unalias(R),
              {D, T}
            end,
        io:format("~p|~p|~p~n", [F(explicit_unalias), F(demonitor), F(reply_demonitor)])
    "#;
    if !expect_oracle(expr, "{true,true}|{true,false}|{true,false}") {
        return;
    }

    let mut f = Fixture::new();
    for (mode, survives) in [
        (UnaliasMode::ExplicitUnalias, true),
        (UnaliasMode::Demonitor, false),
        (UnaliasMode::ReplyDemonitor, false),
    ] {
        let watcher = f.table.spawn_default().expect("a watcher");
        let target = f.table.spawn_default().expect("a target");
        let reference = f.monitor_with_alias(watcher, target, mode);

        // The monitor itself existed, so `demonitor` reports `true` ...
        assert!(f.table.demonitor(watcher, reference).expect("the watcher is live"));
        // ... and only `explicit_unalias` leaves the alias behind.
        assert_eq!(f.table.resolve_alias(reference), survives.then_some(watcher), "{mode:?}");
        assert_eq!(f.table.unalias(watcher, reference).expect("a live watcher"), survives);
        assert_eq!(f.table.resolve_alias(reference), None);
    }
}

// ---------------------------------------------------------------------------
// 5. `unalias/1` only clears the alias
// ---------------------------------------------------------------------------

#[test]
fn oracle_unalias_clears_the_alias_and_keeps_the_monitor() {
    // `unalias/1` clears the `ERTS_ML_STATE_ALIAS_MASK` bits only (`bif.c:5963`); the monitor record
    // survives, so the DOWN still arrives and a later `unalias` finds nothing.
    let expr = r#"
        P = spawn(fun() -> receive go -> ok end end),
        R = erlang:monitor(process, P, [{alias, explicit_unalias}]),
        T = unalias(R),
        P ! go,
        D = receive {H, R, process, P, normal} when is_atom(H) -> yes
            after 2000 -> no end,
        T2 = unalias(R),
        io:format("~p|~p|~p~n", [T, D, T2])
    "#;
    if !expect_oracle(expr, "true|yes|false") {
        return;
    }

    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher");
    let target = f.table.spawn_default().expect("a target");
    let reference = f.monitor_with_alias(watcher, target, UnaliasMode::ExplicitUnalias);

    assert!(f.table.unalias(watcher, reference).expect("a live watcher"));
    assert_eq!(f.table.resolve_alias(reference), None);
    // The monitor is untouched on both sides.
    assert!(f.table.get(watcher).expect("live").monitors().find(reference).is_some());
    assert!(f
        .table
        .get(target)
        .expect("live")
        .monitored_by()
        .iter()
        .any(|entry| entry.reference == reference));

    // So the DOWN still arrives ...
    f.table.terminate(target, normal).expect("the target is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, down, process, normal), target.to_eterm());
    // ... and the alias is still gone.
    assert!(!f.table.unalias(watcher, reference).expect("a live watcher"));
}

// ---------------------------------------------------------------------------
// 6. A dying owner takes every alias with it
// ---------------------------------------------------------------------------

#[test]
fn oracle_a_dying_owners_aliases_all_disappear() {
    // What the oracle can show is the *consequence*: once the alias's creator is gone, a message
    // through the alias is no longer delivered. It cannot distinguish "the alias entry is gone"
    // from "the destination is unreachable" — both look like a silently dropped send — so the
    // crate's stronger, directly observable guarantee (`resolve_alias` → `None`, for a **standalone**
    // alias and a monitor-with-alias alias alike) is asserted below it.
    let expr = r#"
        Parent = self(),
        P = spawn(fun() -> A = alias(), Parent ! {a, A}, receive die -> ok end end),
        {a, A} = receive X -> X end,
        Ref = erlang:monitor(process, P),
        P ! die,
        receive {D, Ref, process, P, _} when is_atom(D) -> ok after 2000 -> timeout end,
        A ! after_death,
        M = receive X -> X after 400 -> none end,
        io:format("~p~n", [M])
    "#;
    if !expect_oracle(expr, "none") {
        return;
    }

    let mut f = Fixture::new();
    let owner = f.table.spawn_default().expect("an alias owner");
    let target = f.table.spawn_default().expect("a target");

    // A standalone `alias/1` alias ...
    let standalone =
        f.table.create_alias(owner, UnaliasMode::ExplicitUnalias).expect("a live owner");
    // ... and a monitor-with-alias alias, whose `explicit_unalias` mode would normally outlive even
    // the target's DOWN.
    let monitored = f.monitor_with_alias(owner, target, UnaliasMode::ExplicitUnalias);
    assert_eq!(f.table.resolve_alias(standalone), Some(owner));
    assert_eq!(f.table.resolve_alias(monitored), Some(owner));

    let normal = f.atom_term("normal");
    f.table.terminate(owner, normal).expect("the owner is live");
    assert!(!f.table.contains(owner));

    // Both aliases are swept — the standalone one is not a monitor record, so nothing else could
    // have collected it.
    assert_eq!(f.table.resolve_alias(standalone), None, "the standalone alias is swept");
    assert_eq!(f.table.resolve_alias(monitored), None, "the monitor-with-alias alias is swept");
    // The dying owner's outgoing monitor went with it too.
    assert!(f.table.get(target).expect("live").monitored_by().is_empty());
    // A surviving process can neither clear it nor act against the dead owner.
    let witness = f.table.spawn_default().expect("a witness");
    assert!(!f.table.unalias(witness, standalone).expect("the witness is live"));
    assert!(matches!(f.table.unalias(owner, standalone), Err(BeamError::TermNotFound)));
}

// ---------------------------------------------------------------------------
// 7. A standalone alias is not a monitor
// ---------------------------------------------------------------------------

#[test]
fn oracle_a_standalone_alias_is_not_a_monitor() {
    // `demonitor(Ref)` renders the internal `am_false` as `true` (`bif.c:602-604`), so the probe
    // uses the `[info]` form — the one that passes the verdict through — and shows the alias still
    // working afterwards.
    let expr = r#"
        A = alias(),
        D1 = erlang:demonitor(A),
        D2 = erlang:demonitor(A, [info]),
        A ! still_here,
        M = receive X -> X after 400 -> none end,
        io:format("~p|~p|~p~n", [D1, D2, M])
    "#;
    if !expect_oracle(expr, "true|false|still_here") {
        return;
    }

    let mut f = Fixture::new();
    let owner = f.table.spawn_default().expect("an alias owner");
    let reference =
        f.table.create_alias(owner, UnaliasMode::ExplicitUnalias).expect("a live owner mints one");

    // `demonitor` reports "no monitor under this reference" — the internal `false`, which the
    // `erlang:demonitor/1` BIF layer would render as `true`.
    assert!(!f.table.demonitor(owner, reference).expect("the owner is live"), "not a monitor");
    // ... and leaves the alias alone.
    assert_eq!(f.table.resolve_alias(reference), Some(owner), "demonitor must not clear it");
    assert!(f.table.unalias(owner, reference).expect("a live owner"));
}

// ---------------------------------------------------------------------------
// 8. The reference ⇄ term seam
// ---------------------------------------------------------------------------

#[test]
fn a_monitor_reference_round_trips_through_its_term_and_rejects_every_non_ref() {
    let mut heap = Heap::new(16).expect("a test heap");
    let mut refs = RefAllocator::new();
    let reference = refs.next();
    let _ = refs.next();
    let second = refs.next();

    let term = reference.to_eterm(&mut heap).expect("the ref thing fits");
    assert_eq!(
        MonitorRef::from_eterm(term).expect("an internal ref thing decodes"),
        reference,
        "the destination-to-key seam round-trips the reference a message carries"
    );
    let other = second.to_eterm(&mut heap).expect("the ref thing fits");
    assert_eq!(MonitorRef::from_eterm(other).expect("decodes"), second);

    // Non-references are the oracle's `badarg` (`erlang:unalias/1` of a non-reference), never a
    // wrong id: a small integer, an atom, a pid immediate and a bare header word (the ABI's
    // `THE_NON_VALUE`) — the four the objective names.
    let mut names = AtomTable::new();
    let not_a_ref = make_atom(names.intern("not_a_ref"));
    for bad in [make_small(7), not_a_ref, ProcessId::new(3, 0).to_eterm(), make_header(3, REF_SUBTAG)]
    {
        assert!(
            matches!(MonitorRef::from_eterm(bad), Err(BeamError::BadArg)),
            "a non-reference must be badarg: {bad:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 9. Born-down: a monitor-with-alias on an already-dead target
// ---------------------------------------------------------------------------

#[test]
fn oracle_a_monitor_with_an_alias_on_an_already_dead_target_births_down() {
    // The alias is registered **before** the target's liveness is checked, so the mode's rule still
    // applies at the immediate `noproc` DOWN — `explicit_unalias` outlives even that.
    let expr = r#"
        Dead = spawn(fun() -> ok end),
        Ref = erlang:monitor(process, Dead),
        receive {D, Ref, _, _, _} when is_atom(D) -> ok after 2000 -> timeout end,
        G = fun(Mode) ->
              R = erlang:monitor(process, Dead, [{alias, Mode}]),
              D2 = receive {H, R, _, _, _} when is_atom(H) -> yes
                  after 2000 -> no end,
              R ! x,
              M = receive X -> X after 300 -> none end,
              T = unalias(R),
              {D2, M, T}
            end,
        io:format("~p|~p|~p~n", [G(explicit_unalias), G(demonitor), G(reply_demonitor)])
    "#;
    if !expect_oracle(expr, "{yes,x,true}|{yes,none,false}|{yes,none,false}") {
        return;
    }

    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let noproc = f.atom_term("noproc");

    for (mode, survives) in [
        (UnaliasMode::ExplicitUnalias, true),
        (UnaliasMode::Demonitor, false),
        (UnaliasMode::ReplyDemonitor, false),
    ] {
        let watcher = f.table.spawn_default().expect("a watcher");
        let dead = f.dead_pid();

        let reference = f
            .table
            .monitor_with_options(
                watcher,
                dead,
                MonitorKind::Process,
                MonitorOptions { tag: None, alias: Some(mode) },
            )
            .expect("a non-live target is not an error");
        // Nothing is left to watch, so no monitor half is recorded on either side.
        assert!(f.table.get(watcher).expect("live").monitors().find(reference).is_none());
        // The mode's rule has already fired: only `explicit_unalias` is still a destination.
        assert_eq!(f.table.resolve_alias(reference), survives.then_some(watcher), "{mode:?}");
        let payload = f.sole_payload(watcher);
        assert_immediate_target(assert_down(payload, down, process, noproc), dead.to_eterm());
        f.drain_one(watcher);
        assert_eq!(f.table.unalias(watcher, reference).expect("a live watcher"), survives);
    }
}

// ---------------------------------------------------------------------------
// 10. `monitor_term_with_options` routes every local dispatch
// ---------------------------------------------------------------------------

#[test]
fn monitor_term_with_options_routes_every_local_dispatch_through_the_alias_path() {
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher");

    // 1. A local **pid term** reaches `monitor_with_options` (and its `{alias, Mode}` handling).
    let target = f.table.spawn_default().expect("a target");
    let reference = f
        .table
        .monitor_term_with_options(
            watcher,
            target.to_eterm(),
            MonitorKind::Process,
            MonitorOptions { tag: None, alias: Some(UnaliasMode::ExplicitUnalias) },
        )
        .expect("a local pid term dispatches to the alias-capable path");
    assert_eq!(f.table.resolve_alias(reference), Some(watcher));
    // The alias outlives the target's DOWN here too — the routing really did reach the
    // option-aware entry point rather than the option-less `monitor_term`.
    f.table.terminate(target, normal).expect("the target is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, down, process, normal), target.to_eterm());
    f.drain_one(watcher);
    assert_eq!(f.table.resolve_alias(reference), Some(watcher));

    // 2. A registered **name atom** reaches `monitor_named_with_options`, whose DOWN target is the
    //    `{Name, Node}` pair while carrying the alias the very same way.
    let name = f.atom("alias_name_server");
    let node = f.atom("nonode@nohost");
    f.table.set_local_node(node);
    let named_target = f.table.spawn_default().expect("a named target");
    f.table.register(name, named_target).expect("the name is free");
    let named_ref = f
        .table
        .monitor_term_with_options(
            watcher,
            make_atom(name),
            MonitorKind::Process,
            MonitorOptions { tag: None, alias: Some(UnaliasMode::ExplicitUnalias) },
        )
        .expect("the name resolves to a live process");
    assert_eq!(f.table.resolve_alias(named_ref), Some(watcher), "a named monitor aliases too");
    f.table.terminate(named_target, normal).expect("the named target is live");
    let expected_target = f.tuple_in(watcher, &[make_atom(name), make_atom(node)]);
    let payload = f.sole_payload(watcher);
    let delivered = assert_down(payload, down, process, normal);
    assert!(
        compare::eq(delivered, expected_target, &f.names),
        "the DOWN target is the `{{Name, Node}}` pair"
    );
    assert_eq!(f.table.resolve_alias(named_ref), Some(watcher));

    // 3. A **remote** (external pid) target can carry neither option: `DOP_MONITOR_P` has no alias
    //    field, so the remote node could not know when to expire it.
    let mut heap = Heap::new(16).expect("a test heap");
    let remote_node = f.atom_term("remote@host");
    let remote = external_pid(&mut heap, remote_node);
    let remote_watcher = f.table.spawn_default().expect("a second watcher");
    assert_typed_unsupported(
        f.table
            .monitor_term_with_options(
                remote_watcher,
                remote,
                MonitorKind::Process,
                MonitorOptions { tag: None, alias: Some(UnaliasMode::Demonitor) },
            )
            .expect_err("a remote target cannot carry an alias"),
        UNSUPPORTED_REMOTE_ALIAS,
    );
    // A typed error, not a partial relationship: nothing was recorded or routed.
    assert!(f.table.remote_monitors(remote_watcher).is_empty());
    assert!(f.table.get(remote_watcher).expect("live").monitors().is_empty());
    assert!(f.table.get(remote_watcher).expect("live").mailbox().is_empty());
}

// ---------------------------------------------------------------------------
// 11. Port monitors cannot carry an alias
// ---------------------------------------------------------------------------

#[test]
fn oracle_a_port_monitor_alias_is_the_typed_unsupported_and_leaks_nothing() {
    // The oracle **does** support the combination, so this probe is the honest record of a
    // capability gap rather than an agreement check: the reply is delivered through the port
    // monitor's alias.
    let expr = r#"
        process_flag(trap_exit, true),
        Port = open_port({spawn, "cat"}, []),
        R = erlang:monitor(port, Port, [{alias, reply_demonitor}]),
        R ! reply,
        M = receive X -> X after 400 -> none end,
        io:format("~p|~p~n", [is_reference(R), M])
    "#;
    if !expect_oracle(expr, "true|reply") {
        return;
    }

    // A `reply_demonitor` alias must be consumed where a message is delivered *through* it, and
    // that site (`alias_reply_delivered`) holds no `PortTable` — so the port's incoming
    // `PortMonitor` could not be removed with the alias, and a later `port_closed` would deliver a
    // `{'DOWN', …}` the oracle never delivers. The typed diagnostic is the honest answer.
    let mut f = Fixture::new();
    let watcher = f.table.spawn_default().expect("a watcher");
    let mut ports = PortTable::new();
    let port = ports.alloc().expect("room in the number space");
    let alias_option = MonitorOptions { tag: None, alias: Some(UnaliasMode::ReplyDemonitor) };

    // Both port entry points refuse the option with the one shared reason ...
    assert_typed_unsupported(
        f.table
            .monitor_port_with_options(watcher, port, &mut ports, alias_option)
            .expect_err("a port monitor cannot carry an alias"),
        UNSUPPORTED_PORT_ALIAS,
    );
    assert_typed_unsupported(
        f.table
            .monitor_port_term_with_options(watcher, port.to_eterm(), &mut ports, alias_option)
            .expect_err("a port monitor cannot carry an alias"),
        UNSUPPORTED_PORT_ALIAS,
    );
    // ... even for the mode whose whole point is to outlive everything.
    assert_typed_unsupported(
        f.table
            .monitor_port_with_options(
                watcher,
                port,
                &mut ports,
                MonitorOptions { tag: None, alias: Some(UnaliasMode::ExplicitUnalias) },
            )
            .expect_err("a port monitor cannot carry an alias"),
        UNSUPPORTED_PORT_ALIAS,
    );

    // Nothing leaked out of the refused calls: no incoming port monitor, no outgoing watch and no
    // DOWN in the mailbox.
    assert!(ports.monitors(port).is_empty());
    assert!(f.table.get(watcher).expect("live").port_monitors().is_empty());
    assert!(f.table.get(watcher).expect("live").mailbox().is_empty());

    // The `{tag, Tag}` half of the option set is unaffected: the port entry point still accepts it
    // (its own file pins the delivered shape).
    let tag = f.atom_term("port_tag");
    f.table
        .monitor_port_with_tag(watcher, port, &mut ports, Some(tag))
        .expect("a tag alone is still supported on a port monitor");
    assert!(!ports.monitors(port).is_empty());
}

// ---------------------------------------------------------------------------
// 12. A plain monitor reference is not an alias
// ---------------------------------------------------------------------------

#[test]
fn a_plain_monitor_reference_is_not_an_alias_so_the_alias_api_is_a_no_op() {
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let process = f.atom("process");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher");
    let target = f.table.spawn_default().expect("a target");

    // An `erlang:monitor/2` (no options at all) reference never enters the alias table, so a send
    // through it has no destination.
    let reference = f.table.monitor(watcher, target, MonitorKind::Process).expect("both are live");
    assert_eq!(f.table.resolve_alias(reference), None, "a monitor ref is not an alias");

    // The reply site is a no-op for it (the message was still delivered by whoever resolved it, but
    // there is no alias to consume) ...
    f.table.alias_reply_delivered(reference).expect("a non-alias reference is not an error");
    // ... and so is `unalias/1`, which must not disturb the monitor.
    assert!(!f.table.unalias(watcher, reference).expect("the watcher is live"));
    assert!(f.table.get(watcher).expect("live").monitors().find(reference).is_some());
    assert!(f.table.demonitor(watcher, reference).expect("the watcher is live"));

    // The DOWN of an untouched plain monitor is unaffected. (The `demonitor` above removed it, so
    // re-monitor to observe the notification.)
    let reference = f.table.monitor(watcher, target, MonitorKind::Process).expect("both are live");
    f.table.alias_reply_delivered(reference).expect("a no-op");
    f.table.terminate(target, normal).expect("the target is live");
    let payload = f.sole_payload(watcher);
    assert_immediate_target(assert_down(payload, down, process, normal), target.to_eterm());
    assert_eq!(f.table.resolve_alias(reference), None);
}
