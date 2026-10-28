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

//! Integration evidence for **`erlang:demonitor(Ref, [flush])`**: the head-agnostic pattern, the
//! found-monitor short-circuit, and the one-shot removal — the three halves of ERTS's rule.
//!
//! # The two DOWN shapes
//!
//! An `erlang:monitor/2` monitor delivers `{'DOWN', Ref, Kind, Target, Reason}`; an
//! `erlang:monitor/3` `{tag, Tag}` monitor delivers `{Tag, Ref, Kind, Target, Reason}` — the same
//! DOWN with the **user's** term in the head slot (see `monitor.rs`'s "User tags" section and
//! `tests/monitor_tag.rs`). ERTS's flush is the pattern `{_, Ref, _, _, _}`
//! (`erts_internal:flush_monitor_messages/3`, `erts/preloaded/src/erts_internal.erl:523-534`),
//! which `erlang:demonitor/2`'s `[flush]` traps to, so a head test on the `'DOWN'` atom would leak
//! a tagged DOWN into the caller's mailbox. That is the shape `gen_server`'s `mc_recv/5` leaves
//! behind: it calls `demonitor(Mon, [flush])` right after the reply.
//!
//! # … plus the two rules around it
//!
//! The trap is reached **only** for the C `demonitor()`'s `am_false` arm (nothing on record under
//! that reference) or an `am_true` with `Multi == am_true` — a class (time-offset / spawn-pending
//! monitors) this crate cannot create. So:
//!
//! 1. **A found monitor flushes nothing** (`erts/emulator/beam/bif.c:630-645`: the `am_true` +
//!    `Multi == am_false` arm returns without trapping). Every DOWN site in this crate drops the
//!    monitor record when it creates the DOWN, so a DOWN for a *recorded* reference cannot be
//!    queued anyway; scanning regardless would remove a program-constructed 5-tuple that the
//!    oracle leaves alone (`oracle_a_found_monitor_flushes_nothing`).
//! 2. **At most one message is removed**: `flush_monitor_messages/3` with `Multi = false` returns
//!    after the first `{_, Ref, _, _, _}` it takes
//!    (`oracle_flush_removes_at_most_one_matching_message`).
//!
//! # Identity is the sole discriminator
//!
//! A **different** monitor's DOWN stays, whatever its head, as does a 5-tuple carrying any other
//! term at element 1 and a message that is not a 5-tuple at all.
//!
//! # Oracle protocol
//!
//! Every `oracle_*` check runs an equivalent Erlang expression under the system `erl` (the shared
//! driver in `tests/common/mod.rs`: `erl -noshell -kernel logger_level none -eval '<expr>,
//! halt(0).'`, captured streams, normalised stdout) and asserts its printed answer. The
//! expressions print only **stable** observables — `process_info(self(), message_queue_len)` and
//! the atom heads of the queued messages — because the reference, the pid and the node name are
//! not reproducible from a Rust test.
//!
//! If `erl` is not on `PATH` (or does not answer within [`common::ERL_TIMEOUT`]) every oracle check
//! **skips loudly** (a clear `eprintln!`) and returns, never failing the suite.
mod common;

use beam_alloc::Heap;
use beam_process::prelude::*;
use beam_term::prelude::{
    is_literal, make_arityval, make_atom, make_list, make_small, make_tuple, set_literal_tag,
    tuple_element, tuple_size, Atom, AtomTable, Eterm, LiteralArea, NIL, MAX_SMALL,
};
use common::expect_oracle;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------
/// A process table plus the atom table its [`SignalAtoms`] were interned from, so a test can
/// resolve the well-known message atoms back to terms.
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

    /// How many messages `pid`'s mailbox holds.
    fn mailbox_len(&self, pid: ProcessId) -> usize {
        self.table.get(pid).expect("the recipient is live").mailbox().len()
    }

    /// The payload of `pid`'s single queued message (panics unless exactly one is queued).
    fn sole_payload(&self, pid: ProcessId) -> Eterm {
        let mailbox = self.table.get(pid).expect("the recipient is live").mailbox();
        assert_eq!(mailbox.len(), 1, "exactly one message is queued");
        mailbox.peek().expect("one message is queued").payload()
    }

    /// The **head** element (element 0) of every message in `pid`'s mailbox, in queue order.
    ///
    /// Queue order is what makes "the flushed message is gone and the others stayed" observable
    /// without depending on a reference being printable.
    fn queued_heads(&self, pid: ProcessId) -> Vec<Eterm> {
        let mailbox = self.table.get(pid).expect("the recipient is live").mailbox();
        mailbox
            .iter()
            .map(|message| {
                let payload = message.payload();
                // SAFETY: every message these tests queue is a live 5-tuple (a DOWN builder's
                // output, or a tuple this file built), so element 0 is in bounds.
                unsafe {
                    assert_eq!(tuple_size(payload), 5, "the tests queue 5-tuples only");
                    tuple_element(payload, 0)
                }
            })
            .collect()
    }
}

/// Asserts `payload` is the 5-tuple `{head, <a ref>, kind, target, reason}` and returns its
/// **reference** element (element 1).
fn assert_down(payload: Eterm, head: Eterm, kind: Atom, reason: Eterm) -> MonitorRef {
    // SAFETY: `payload` is the live 5-tuple a DOWN builder wrote on the recipient's heap; all five
    // element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5, "a DOWN message is a 5-tuple");
        assert_eq!(
            tuple_element(payload, 0),
            head,
            "the head is the tag (or the `'DOWN'` atom for an untagged monitor)"
        );
        assert_eq!(tuple_element(payload, 2), make_atom(kind));
        assert_eq!(tuple_element(payload, 4), reason);
        MonitorRef::from_eterm(tuple_element(payload, 1)).expect("element 1 is a reference")
    }
}

/// Builds a 5-tuple on the scratch arena `heap` and delivers it to `to` from the live process
/// `from`, i.e. a message that carries no relation to any monitor of the recipient.
fn deliver_tuple(f: &mut Fixture, heap: &mut Heap, to: ProcessId, from: ProcessId, elems: &[Eterm]) {
    let payload = heap.tuple(elems).expect("the scratch arena holds the tuple");
    f.table
        .deliver_simple(to, from.to_eterm(), payload)
        .expect("the recipient is live");
}

/// A literal-area 1-tuple, i.e. a **boxed** tag that is heap-independent (see `tests/monitor_tag.rs`).
fn literal_tuple(area: &mut LiteralArea, value: Eterm) -> Eterm {
    // SAFETY: two uninitialised words are requested and both are written immediately below,
    // before anything is read.
    let words = unsafe { area.alloc_words(2) }.expect("the literal area has room");
    // SAFETY: `words[0..2]` were just handed out exclusively by `alloc_words` and both are written
    // here — an arityval header followed by the single element — so nothing is read uninitialised,
    // and the words live as long as `area`.
    unsafe {
        words.write(make_arityval(1));
        words.add(1).write(value);
    }
    let mut term = make_tuple(words);
    // SAFETY: `words` addresses the two live, initialised words just written inside `area`, and
    // `&mut term` is a live, aligned, writable root word; the walk only ORs `TAG_LITERAL_PTR` into
    // BOXED/LIST words, leaving the transparent arityval and the immediate element intact.
    unsafe { set_literal_tag(&mut term, words, 2) };
    term
}

/// A literal-area cons cell — the `LIST` half of the same heap-independence rule.
fn literal_cons(area: &mut LiteralArea, value: Eterm) -> Eterm {
    // SAFETY: two uninitialised words are requested and both are written immediately below,
    // before anything is read.
    let words = unsafe { area.alloc_words(2) }.expect("the literal area has room");
    // SAFETY: the two words were just handed out exclusively and are both written here (the CAR
    // and the NIL CDR), and they live as long as `area`.
    unsafe {
        words.write(value);
        words.add(1).write(NIL);
    }
    let mut term = make_list(words);
    // SAFETY: `words` addresses the two live, initialised cons-cell words inside `area` and
    // `&mut term` is a live, aligned, writable root word, so the walk may tag both.
    unsafe { set_literal_tag(&mut term, words, 2) };
    term
}

// ---------------------------------------------------------------------------
// 1 + 2: an untagged and a tagged DOWN are both flushed
// ---------------------------------------------------------------------------
#[test]
fn oracle_a_tagged_down_is_flushed_like_an_untagged_one() {
    // Oracle probe (OTP 29.1.2 / erts-17.1.1): one target per shape, so the tagged DOWN and the
    // untagged one are queued separately and each `demonitor(Ref, [flush])` is measured on its
    // own. `message_queue_len` is 1 before and 0 after in **both** cases, and `demonitor/2`
    // answers `true` in both — the flush drops the message whatever its head is.
    //
    // The flush is reached in these shapes because the target's death **resolved** the monitor
    // record: `demonitor/1` of it answers `false` afterwards (`tests/signals.rs` pins that), so the
    // C takes its `am_false` arm and traps to `erts_internal:flush_monitor_messages/3`. A monitor
    // that is *still* on record flushes nothing — see `oracle_a_found_monitor_flushes_nothing`.
    let expr = r#"
        Q = fun() -> element(2, erlang:process_info(self(), message_queue_len)) end,
        P1 = spawn(fun() -> receive go -> ok end end),
        RT = erlang:monitor(process, P1, [{tag, mytag}]),
        P1 ! go, timer:sleep(150),
        B1 = Q(), DT = erlang:demonitor(RT, [flush]), A1 = Q(),
        P2 = spawn(fun() -> receive go -> ok end end),
        RU = erlang:monitor(process, P2),
        P2 ! go, timer:sleep(150),
        B2 = Q(), DU = erlang:demonitor(RU, [flush]), A2 = Q(),
        io:format("tagged=~p/~p/~p untagged=~p/~p/~p~n", [B1, DT, A1, B2, DU, A2])
    "#;
    if !expect_oracle(expr, "tagged=1/true/0 untagged=1/true/0") {
        return;
    }
    let mut f = Fixture::new();
    let down = f.atom("DOWN");
    let mytag = f.atom("mytag");
    let process = f.atom("process");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");

    // 1. The unchanged case: the ordinary `{'DOWN', …}` is flushed.
    let target = f.table.spawn_default().expect("a target process");
    let reference = f
        .table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");
    f.table.terminate(target, normal).expect("the target is live");
    assert_eq!(assert_down(f.sole_payload(watcher), make_atom(down), process, normal), reference);
    assert!(f
        .table
        .demonitor_flush(watcher, reference)
        .expect("the watcher is live"));
    assert_eq!(
        f.mailbox_len(watcher),
        0,
        "the untagged DOWN was flushed"
    );

    // 2. The case this file exists for: a **tagged** DOWN (head = the user's tag) goes too.
    let target = f.table.spawn_default().expect("a second target");
    let reference = f
        .table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("both pids are live");
    f.table.terminate(target, normal).expect("the target is live");
    assert_eq!(f.mailbox_len(watcher), 1, "the tagged DOWN is queued");
    assert_eq!(
        assert_down(f.sole_payload(watcher), make_atom(mytag), process, normal),
        reference
    );
    assert!(f
        .table
        .demonitor_flush(watcher, reference)
        .expect("the watcher is live"));
    assert_eq!(
        f.mailbox_len(watcher),
        0,
        "a tagged DOWN is this monitor's DOWN, so it is flushed too"
    );
}

#[test]
fn a_tagged_down_is_flushed_for_every_accepted_tag_kind() {
    // The head that must not defeat the flush is the *user's term*, so every tag kind the crate
    // accepts has to be covered: an atom, a small integer (both immediates) and a **boxed**
    // literal-area term. (An `erl` expression cannot tell a literal-area tag from a per-process
    // heap one, so this half is crate-local evidence of the *storage* rule, exactly like
    // `tests/monitor_tag.rs`'s literal-area check.)
    let mut f = Fixture::new();
    let mytag = f.atom("mytag");
    let process = f.atom("process");
    let normal = f.atom_term("normal");
    let noproc = f.atom_term("noproc");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let mut area = LiteralArea::new(16);

    // The tags, in the order they are exercised. Each is `Some(tag)` for `monitor_with_tag/4`.
    let boxed_tag = literal_tuple(&mut area, make_small(1234));
    let list_tag = literal_cons(&mut area, make_small(7));
    let tags = [
        make_atom(mytag),
        make_small(7),
        boxed_tag,
        list_tag,
    ];

    for tag in tags {
        // A **live** target that the test then terminates: the DOWN carries the death reason.
        let target = f.table.spawn_default().expect("a target process");
        let reference = f
            .table
            .monitor_with_tag(watcher, target, MonitorKind::Process, Some(tag))
            .expect("a heap-independent tag is accepted");
        f.table.terminate(target, normal).expect("the target is live");
        assert_eq!(f.mailbox_len(watcher), 1, "the tagged DOWN is queued");
        assert_eq!(
            assert_down(f.sole_payload(watcher), tag, process, normal),
            reference,
            "the notification head is the tag itself"
        );
        assert!(
            f.table
                .demonitor_flush(watcher, reference)
                .expect("the watcher is live"),
            "the flush reports the removal"
        );
        assert_eq!(f.mailbox_len(watcher), 0, "the tagged DOWN was flushed");

        // The same tag on a target that is **already dead**: the `noproc` DOWN is produced
        // eagerly, and still flushed.
        let dead = ProcessId::new(999, 0);
        let reference = f
            .table
            .monitor_with_tag(watcher, dead, MonitorKind::Process, Some(tag))
            .expect("a non-live target is not an error");
        assert_eq!(f.mailbox_len(watcher), 1, "the immediate noproc DOWN is queued");
        assert_eq!(
            assert_down(f.sole_payload(watcher), tag, process, noproc),
            reference
        );
        assert!(f
            .table
            .demonitor_flush(watcher, reference)
            .expect("the watcher is live"));
        assert_eq!(f.mailbox_len(watcher), 0, "the tagged noproc DOWN was flushed");
    }
    assert!(
        is_literal(boxed_tag) && is_literal(list_tag),
        "the fixture's boxed tags really are literal-area terms (never relocated)"
    );
}

// ---------------------------------------------------------------------------
// 3: everything that is NOT this monitor's DOWN stays
// ---------------------------------------------------------------------------
#[test]
fn oracle_flushing_one_monitor_leaves_a_different_down_and_an_unrelated_five_tuple() {
    // Oracle probe (OTP 29.1.2 / erts-17.1.1): three queued 5-tuples — a tagged DOWN, an untagged
    // DOWN and an unrelated `{unrelated, Ref, x, y, z}` — then each reference is flushed in turn.
    // The counts drop 3 → 2 → 1 and the surviving heads are printed, so the probe pins that only
    // *this* reference's DOWN is removed.
    let expr = r#"
        Q = fun() -> element(2, erlang:process_info(self(), message_queue_len)) end,
        Heads = fun() -> [element(1, M) || M <- element(2, erlang:process_info(self(), messages)),
                                        is_tuple(M) andalso tuple_size(M) =:= 5] end,
        P1 = spawn(fun() -> receive go -> ok end end),
        RA = erlang:monitor(process, P1, [{tag, mytag}]), P1 ! go, timer:sleep(150),
        P2 = spawn(fun() -> receive go -> ok end end),
        RB = erlang:monitor(process, P2), P2 ! go, timer:sleep(150),
        self() ! {unrelated, make_ref(), x, y, z},
        B = Q(), D = erlang:demonitor(RA, [flush]), A = Q(), H = Heads(),
        D2 = erlang:demonitor(RB, [flush]), A2 = Q(), H2 = Heads(),
        io:format("~p|~p|~p|~p~n~p|~p|~p~n", [B, D, A, H, D2, A2, H2])
    "#;
    if !expect_oracle(
        expr,
        "3|true|2|['DOWN',unrelated]\ntrue|1|[unrelated]",
    ) {
        return;
    }
    let mut f = Fixture::new();
    let mytag = f.atom("mytag");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let sender = f.table.spawn_default().expect("a sender process");
    let mut scratch = Heap::new(64).expect("a scratch arena");

    // A tagged DOWN and an untagged DOWN on two different targets, so neither head can be
    // mistaken for the other's (and the untagged one proves the head is not the discriminator).
    let tagged_target = f.table.spawn_default().expect("a target process");
    let tagged = f
        .table
        .monitor_with_tag(watcher, tagged_target, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("both pids are live");
    f.table
        .terminate(tagged_target, normal)
        .expect("the target is live");
    let plain_target = f.table.spawn_default().expect("a second target");
    let plain = f
        .table
        .monitor(watcher, plain_target, MonitorKind::Process)
        .expect("both pids are live");
    f.table
        .terminate(plain_target, normal)
        .expect("the target is live");

    // … plus two messages that carry **no** relation to either monitor: an `unrelated` 5-tuple
    // whose element 1 is a reference this table never minted, and a 5-tuple whose element 1 is a
    // small integer. Neither may be removed by a flush.
    let alien = MonitorRef::from_id(MAX_SMALL as u64);
    let alien_term = alien.to_eterm(&mut scratch).expect("the scratch arena holds a ref");
    let unrelated = f.atom_term("unrelated");
    let x = f.atom_term("x");
    let y = f.atom_term("y");
    let z = f.atom_term("z");
    let sender_term = sender.to_eterm();
    let normal_term = f.atom_term("normal");
    let process_term = f.atom_term("process");
    let down_head = f.atom_term("DOWN");
    deliver_tuple(
        &mut f,
        &mut scratch,
        watcher,
        sender,
        &[unrelated, alien_term, x, y, z],
    );
    deliver_tuple(
        &mut f,
        &mut scratch,
        watcher,
        sender,
        &[down_head, make_small(42), process_term, sender_term, normal_term],
    );
    assert_eq!(f.mailbox_len(watcher), 4, "two DOWNs and two unrelated 5-tuples");

    // Flushing the tagged monitor removes its tagged DOWN and nothing else.
    assert!(f
        .table
        .demonitor_flush(watcher, tagged)
        .expect("the watcher is live"));
    assert_eq!(f.mailbox_len(watcher), 3);
    let heads = f.queued_heads(watcher);
    let down_atom = f.atom_term("DOWN");
    assert_eq!(
        heads,
        vec![down_atom, unrelated, down_atom],
        "the untagged DOWN and both unrelated 5-tuples stayed, in queue order"
    );

    // Flushing the untagged monitor removes exactly its own DOWN.
    assert!(f
        .table
        .demonitor_flush(watcher, plain)
        .expect("the watcher is live"));
    assert_eq!(f.mailbox_len(watcher), 2);
    assert_eq!(
        f.queued_heads(watcher),
        vec![unrelated, down_atom],
        "only the two unrelated 5-tuples are left"
    );

    // A reference the table never handed out flushes nothing it does not carry: ERTS's rule is
    // `{_, Ref, _, _, _}`, so the crafted `{unrelated, Ref, x, y, z}` **is** that reference's DOWN
    // as far as the flush is concerned (oracle probe, OTP 29.1.2:
    //   `R = make_ref(), self() ! {weird, R, a, b, c}, self() ! {a, b, c},
    //    erlang:demonitor(R, [flush])` ⇒ `message_queue_len` 2 → 1, the survivor being `{a, b, c}`).
    // What must *not* go is a message whose element 1 is any other term.
    assert!(
        f.table
            .demonitor_flush(watcher, alien)
            .expect("the watcher is live"),
        "the message carrying this reference at element 1 was removed"
    );
    assert_eq!(
        f.mailbox_len(watcher),
        1,
        "only the alien-reference message was removed"
    );
    assert_eq!(
        f.queued_heads(watcher),
        vec![down_atom],
        "the `{{DOWN, 42, …}}` 5-tuple (element 1 is not a reference) stayed"
    );
    assert!(!f
        .table
        .demonitor_flush(watcher, alien)
        .expect("the watcher is live"));
    assert_eq!(f.mailbox_len(watcher), 1, "nothing left to remove");
}

// ---------------------------------------------------------------------------
// 4: the contrast — an unflushed tagged DOWN still arrives
// ---------------------------------------------------------------------------
#[test]
fn oracle_an_unflushed_tagged_down_still_arrives_and_flush_still_answers_true() {
    // Oracle probe (OTP 29.1.2 / erts-17.1.1): `demonitor/1` (no `[flush]`) answers `true`, leaves
    // the tagged DOWN queued (`message_queue_len` 1) and the message that is then received is
    // `{mytag, Ref, process, Pid, normal}`.
    let expr = r#"
        Q = fun() -> element(2, erlang:process_info(self(), message_queue_len)) end,
        P = spawn(fun() -> receive go -> ok end end),
        RT = erlang:monitor(process, P, [{tag, mytag}]), P ! go, timer:sleep(150),
        D = erlang:demonitor(RT),
        N = Q(),
        M = receive {H, R, K, T, A} -> {H, is_reference(R), K, is_pid(T), A}
            after 1000 -> timeout end,
        io:format("~p|~p|~p~n", [D, N, M])
    "#;
    if !expect_oracle(expr, "true|1|{mytag,true,process,true,normal}") {
        return;
    }
    let mut f = Fixture::new();
    let mytag = f.atom("mytag");
    let process = f.atom("process");
    let normal = f.atom_term("normal");
    let watcher = f.table.spawn_default().expect("a watcher process");

    // The contrast: `demonitor/1` drops the monitor half but leaves the tagged DOWN queued, and
    // the payload is still the oracle's `{mytag, Ref, process, Pid, normal}`.
    let target = f.table.spawn_default().expect("a target process");
    let reference = f
        .table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("both pids are live");
    f.table.terminate(target, normal).expect("the target is live");
    assert_eq!(f.mailbox_len(watcher), 1, "no flush: the tagged DOWN stays");
    // `demonitor/1` reports whether a monitor was still on record; the target's death already
    // resolved this one, so it is `false` (the DOWN it produced is untouched — that is the point).
    assert!(!f
        .table
        .demonitor(watcher, reference)
        .expect("the watcher is live"));
    assert_eq!(
        assert_down(f.sole_payload(watcher), make_atom(mytag), process, normal),
        reference
    );
    // A flush of the already-demonitored reference still removes the queued DOWN and answers
    // `true` (the monitor half is gone, a message was removed).
    assert!(f
        .table
        .demonitor_flush(watcher, reference)
        .expect("the watcher is live"));
    assert_eq!(f.mailbox_len(watcher), 0);
    // Re-flushing an empty mailbox changes nothing and is still `true`-shaped (nothing to remove).
    assert!(!f
        .table
        .demonitor_flush(watcher, reference)
        .expect("the watcher is live"));
    assert_eq!(f.mailbox_len(watcher), 0);
}

// ---------------------------------------------------------------------------
// 5: ERTS's found-monitor short-circuit — a live monitor flushes NOTHING
// ---------------------------------------------------------------------------
#[test]
fn oracle_a_found_monitor_flushes_nothing() {
    // Oracle probe (OTP 29.1.2 / erts-17.1.1):
    //   P = spawn(fun() -> receive go -> ok end end),
    //   R = erlang:monitor(process, P),
    //   self() ! {fake, R, a, b, c}, self() ! {fake2, R, a, b, c},
    //   `demonitor(R, [flush])` ⇒ `true`, `message_queue_len` **2 → 2**, heads `[fake, fake2]`
    //   — the monitor is on record, so the C returns from `demonitor_2` without trapping to
    //   `erts_internal:flush_monitor_messages/3` (`bif.c:640-645`) and the program-built 5-tuples
    //   stay.
    //   A **second** `demonitor(R, [flush])` (now the `am_false` arm: nothing on record) ⇒ `true`
    //   and exactly one message gone: 2 → 1, heads `[fake2]`.
    let expr = r#"
        Q = fun() -> element(2, erlang:process_info(self(), message_queue_len)) end,
        Heads = fun() -> [element(1, M) || M <- element(2, erlang:process_info(self(), messages))] end,
        P = spawn(fun() -> receive go -> ok end end),
        R = erlang:monitor(process, P),
        self() ! {fake, R, a, b, c},
        self() ! {fake2, R, a, b, c},
        B = Q(), D1 = erlang:demonitor(R, [flush]), A1 = Q(), H1 = Heads(),
        D2 = erlang:demonitor(R, [flush]), A2 = Q(), H2 = Heads(),
        io:format("~p|~p|~p|~p~n~p|~p|~p~n", [B, D1, A1, H1, D2, A2, H2])
    "#;
    if !expect_oracle(expr, "2|true|2|[fake,fake2]\ntrue|1|[fake2]") {
        return;
    }
    let mut f = Fixture::new();
    let fake = f.atom_term("fake");
    let fake2 = f.atom_term("fake2");
    let a = f.atom_term("a");
    let b = f.atom_term("b");
    let c = f.atom_term("c");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let sender = f.table.spawn_default().expect("a sender process");
    let mut scratch = Heap::new(64).expect("a scratch arena");

    // A monitor on a **live** target: the record exists, so the flush must not run at all.
    let target = f.table.spawn_default().expect("a live target process");
    let reference = f
        .table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");
    let reference_term = reference
        .to_eterm(&mut scratch)
        .expect("the scratch arena holds a ref");
    deliver_tuple(
        &mut f,
        &mut scratch,
        watcher,
        sender,
        &[fake, reference_term, a, b, c],
    );
    deliver_tuple(
        &mut f,
        &mut scratch,
        watcher,
        sender,
        &[fake2, reference_term, a, b, c],
    );
    assert_eq!(f.mailbox_len(watcher), 2, "the two program-built 5-tuples");

    // `demonitor(R, [flush])` on a recorded monitor: `true`, and **nothing** is flushed (the
    // oracle's 2 → 2). The monitor halves are dropped all the same.
    assert!(
        f.table
            .demonitor_flush(watcher, reference)
            .expect("the watcher is live"),
        "`demonitor(R, [flush])` => true"
    );
    assert_eq!(
        f.mailbox_len(watcher),
        2,
        "a found monitor flushes nothing: the oracle's message_queue_len is 2 before and after"
    );
    assert_eq!(
        f.queued_heads(watcher),
        vec![fake, fake2],
        "both program-built 5-tuples stayed, in queue order"
    );
    assert!(
        f.table
            .get(watcher)
            .expect("the watcher is live")
            .monitors()
            .is_empty(),
        "the monitor half was removed, so nothing is on record any more"
    );
    assert!(
        f.table
            .get(target)
            .expect("the target is still live")
            .monitored_by()
            .is_empty(),
        "and the target's incoming entry went with it"
    );

    // The second flush takes the `am_false` arm and removes exactly the **first** match (the
    // oracle's 2 → 1 with `[fake2]` left).
    assert!(
        f.table
            .demonitor_flush(watcher, reference)
            .expect("the watcher is live"),
        "a queued message was removed"
    );
    assert_eq!(f.mailbox_len(watcher), 1, "one removed, one left");
    assert_eq!(
        f.queued_heads(watcher),
        vec![fake2],
        "the FIRST match went; the later one stayed"
    );
}

// ---------------------------------------------------------------------------
// 6: a fake queued *before* the real DOWN is the one flushed
// ---------------------------------------------------------------------------
#[test]
fn oracle_a_fake_queued_before_a_tagged_down_is_the_one_flushed() {
    // Oracle probe (OTP 29.1.2 / erts-17.1.1):
    //   R = erlang:monitor(process, P, [{tag, mytag}]), self() ! {fake, R, a, b, c},
    //   P ! go, timer:sleep(120),
    //   `demonitor(R, [flush])` ⇒ `true`, `message_queue_len` 2 → 1, and the surviving message is
    //   the **real** tagged DOWN `{mytag, Ref, process, Pid, normal}` — the flush removes the first
    //   `{_, Ref, _, _, _}` (the fake) and stops.
    let expr = r#"
        Q = fun() -> element(2, erlang:process_info(self(), message_queue_len)) end,
        P = spawn(fun() -> receive go -> ok end end),
        R = erlang:monitor(process, P, [{tag, mytag}]),
        self() ! {fake, R, a, b, c},
        P ! go, timer:sleep(120),
        B = Q(), D = erlang:demonitor(R, [flush]), A = Q(),
        M = receive {H, RR, K, T, RS} -> {H, is_reference(RR), K, is_pid(T), RS}
            after 1000 -> timeout end,
        io:format("~p|~p|~p|~p~n", [B, D, A, M])
    "#;
    if !expect_oracle(expr, "2|true|1|{mytag,true,process,true,normal}") {
        return;
    }
    let mut f = Fixture::new();
    let mytag = f.atom("mytag");
    let process = f.atom("process");
    let normal = f.atom_term("normal");
    let fake = f.atom_term("fake");
    let a = f.atom_term("a");
    let b = f.atom_term("b");
    let c = f.atom_term("c");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let sender = f.table.spawn_default().expect("a sender process");
    let mut scratch = Heap::new(64).expect("a scratch arena");

    let target = f.table.spawn_default().expect("a target process");
    let reference = f
        .table
        .monitor_with_tag(watcher, target, MonitorKind::Process, Some(make_atom(mytag)))
        .expect("both pids are live");
    // The fake goes in **first**, so the real DOWN is queued behind it (the target's death is what
    // resolves the monitor record, which is why the flush runs at all).
    let reference_term = reference
        .to_eterm(&mut scratch)
        .expect("the scratch arena holds a ref");
    deliver_tuple(
        &mut f,
        &mut scratch,
        watcher,
        sender,
        &[fake, reference_term, a, b, c],
    );
    f.table.terminate(target, normal).expect("the target is live");
    assert_eq!(f.mailbox_len(watcher), 2, "the fake, then the real tagged DOWN");

    assert!(
        f.table
            .demonitor_flush(watcher, reference)
            .expect("the watcher is live"),
        "`demonitor(R, [flush])` => true"
    );
    assert_eq!(
        f.mailbox_len(watcher),
        1,
        "the flush removed ONE message (the oracle's 2 → 1)"
    );
    assert_eq!(
        assert_down(f.sole_payload(watcher), make_atom(mytag), process, normal),
        reference,
        "the survivor is the real tagged DOWN `{{mytag, Ref, process, Pid, normal}}`"
    );
}

// ---------------------------------------------------------------------------
// 7: the flush removes at most ONE matching message
// ---------------------------------------------------------------------------
#[test]
fn oracle_flush_removes_at_most_one_matching_message() {
    // Oracle probe (OTP 29.1.2 / erts-17.1.1): a **bare** `make_ref/0` reference (no monitor is
    // recorded, so the `am_false` arm is taken) with two program-built `{_, Ref, _, _, _}` shapes
    // ⇒ `demonitor(R, [flush])` answers `true`, `message_queue_len` 2 → 1 and the survivor's head
    // is `f2`. `erts_internal:flush_monitor_messages/3` returns after the first match when
    // `Multi = false` (`erts_internal.erl:523-534`).
    let expr = r#"
        Q = fun() -> element(2, erlang:process_info(self(), message_queue_len)) end,
        Heads = fun() -> [element(1, M) || M <- element(2, erlang:process_info(self(), messages))] end,
        R = make_ref(),
        self() ! {f1, R, a, b, c},
        self() ! {f2, R, a, b, c},
        B = Q(), D = erlang:demonitor(R, [flush]), A = Q(), H = Heads(),
        io:format("~p|~p|~p|~p~n", [B, D, A, H])
    "#;
    if !expect_oracle(expr, "2|true|1|[f2]") {
        return;
    }
    let mut f = Fixture::new();
    let f1 = f.atom_term("f1");
    let f2 = f.atom_term("f2");
    let a = f.atom_term("a");
    let b = f.atom_term("b");
    let c = f.atom_term("c");
    let watcher = f.table.spawn_default().expect("a watcher process");
    let sender = f.table.spawn_default().expect("a sender process");
    let mut scratch = Heap::new(64).expect("a scratch arena");

    // `make_ref/0`'s analogue: a reference minted from the table's shared space that backs no
    // monitor (`ProcessTable::next_reference`).
    let bare = f.table.next_reference();
    assert!(
        !f.table
            .demonitor(watcher, bare)
            .expect("the watcher is live"),
        "a bare reference backs no monitor, so the flush's `am_false` arm is the one taken"
    );
    let bare_term = bare
        .to_eterm(&mut scratch)
        .expect("the scratch arena holds a ref");
    deliver_tuple(
        &mut f,
        &mut scratch,
        watcher,
        sender,
        &[f1, bare_term, a, b, c],
    );
    deliver_tuple(
        &mut f,
        &mut scratch,
        watcher,
        sender,
        &[f2, bare_term, a, b, c],
    );
    assert_eq!(f.mailbox_len(watcher), 2);

    assert!(
        f.table
            .demonitor_flush(watcher, bare)
            .expect("the watcher is live"),
        "a matching message was removed"
    );
    assert_eq!(
        f.mailbox_len(watcher),
        1,
        "at most one message is flushed (the oracle's 2 → 1)"
    );
    assert_eq!(
        f.queued_heads(watcher),
        vec![f2],
        "the first match went and the scan stopped there"
    );
}
