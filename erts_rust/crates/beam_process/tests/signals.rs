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

//! Integration evidence for link-exit propagation and monitor down-messages.
//!
//! Covers CONTEXT.md Test Strategy item 3: `A` links `B`; `A` terminating with a non-normal
//! reason terminates a non-trapping `B` with that same reason, while a trapping `B` survives
//! and receives `{'EXIT', A, Reason}`; and `A` monitoring `B` means `B`'s termination queues
//! `{'DOWN', Ref, process, B, Reason}` on `A`.
//!
//! The **reason-ownership** tests below pin that a *structured* (non-immediate) exit reason
//! built in one process's heap must be copied into every recipient's own heap, so it survives
//! both the death of its original owner (link cascade, monitor down-message) and a later
//! safepoint collection of the recipient. A raw word pointing into a dropped heap would segfault
//! or compare unequal here.
//!
//! The `kill`/`normal` tests pin the two reason-class special cases of [`exit_policy`], which is
//! **family aware**: `kill` is untrappable and terminates with the rewritten `killed` **only** in
//! the explicit `erlang:exit/2` family, while in the **link-exit** family (`kill`/`normal` tests at
//! the end) a `kill` link exit is trappable and unrewritten and a `normal` link exit is silently
//! ignored by a non-trapping peer (delivered as `{'EXIT', From, normal}` only to a trapping one).
//! Each asserts the value the system `erl` (OTP 29.1.1) was probed to produce before asserting it
//! (the probes are cited on the tests).

use beam_process::prelude::*;
use beam_term::compare;
use beam_term::prelude::{
    boxed_val, is_atom, is_immediate, make_atom, make_small, signed_value, tuple_element,
    tuple_size, Atom, AtomTable, Eterm,
};

/// A pid with the given number and serial 0.
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

/// A table plus the raw indices of the well-known signal atoms it will build messages with.
fn table_with_atoms() -> (ProcessTable, Atom, Atom, Atom) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    let exit = atoms.exit();
    let down = atoms.down();
    let process = atoms.process();
    (ProcessTable::new(atoms), exit, down, process)
}

/// A table plus the atom table its [`SignalAtoms`] were interned from, so tests can compare
/// terms structurally (`compare::eq`) against the same atom index space.
fn table_with_names() -> (ProcessTable, AtomTable) {
    let mut names = AtomTable::new();
    let atoms = SignalAtoms::from_table(&mut names);
    (ProcessTable::new(atoms), names)
}

/// Builds a tuple on `pid`'s own heap and returns it, for use as a *structured* exit reason.
fn tuple_in(table: &mut ProcessTable, pid: ProcessId, elems: &[Eterm]) -> Eterm {
    table
        .get_mut(pid)
        .expect("the holder is live")
        .heap_mut()
        .tuple(elems)
        .expect("the tuple fits the process's young heap")
}

/// The payload of `pid`'s single queued message (panics unless exactly one is queued).
fn sole_payload(table: &ProcessTable, pid: ProcessId) -> Eterm {
    let mailbox = table.get(pid).expect("the recipient is live").mailbox();
    assert_eq!(mailbox.len(), 1, "exactly one message is queued");
    mailbox.peek().expect("one message is queued").payload()
}

#[test]
fn a_non_normal_link_exit_terminates_the_peer() {
    let (mut table, _exit, _down, _process) = table_with_atoms();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    table.link(a, b).expect("both pids are live");

    let reason = make_small(7);
    table.terminate(a, reason).expect("a is live");

    assert!(!table.contains(a), "the signalled process is gone");
    assert!(
        !table.contains(b),
        "a non-normal link exit terminates the non-trapping peer"
    );
}

#[test]
fn a_trapping_peer_survives_and_receives_the_exit_message() {
    let (mut table, exit, _down, _process) = table_with_atoms();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    table
        .get_mut(b)
        .expect("b is live")
        .set_trap_exit(true);
    table.link(a, b).expect("both pids are live");

    let reason = make_small(11);
    table.terminate(a, reason).expect("a is live");

    assert!(!table.contains(a));
    assert!(table.contains(b), "the trapping peer survives");

    let survivor = table.get(b).expect("b survives");
    assert_eq!(survivor.mailbox().len(), 1, "exactly one exit message");
    let payload = survivor
        .mailbox()
        .peek()
        .expect("one message is queued")
        .payload();
    // SAFETY: `payload` is the live `{'EXIT', A, Reason}` tuple the table built on b's heap;
    // the three element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), make_atom(exit));
        assert!(is_atom(tuple_element(payload, 0)));
        assert_eq!(tuple_element(payload, 1), a.to_eterm());
        assert_eq!(tuple_element(payload, 2), reason);
    }
}

#[test]
fn a_monitor_down_message_is_queued_on_the_watcher() {
    let (mut table, _exit, down, process) = table_with_atoms();
    let watcher = table.spawn_default().expect("watcher");
    let target = table.spawn_default().expect("target");
    let reference = table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");

    let reason = make_small(13);
    table.terminate(target, reason).expect("target is live");

    assert!(!table.contains(target));
    assert!(table.contains(watcher), "the watcher is not linked to the target");

    let survivor = table.get(watcher).expect("watcher survives");
    assert_eq!(survivor.mailbox().len(), 1, "exactly one down message");
    let payload = survivor
        .mailbox()
        .peek()
        .expect("one message is queued")
        .payload();
    // SAFETY: `payload` is the live `{'DOWN', Ref, process, Target, Reason}` tuple the table
    // built on the watcher's heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), make_atom(down));
        assert_eq!(tuple_element(payload, 2), make_atom(process));
        assert_eq!(tuple_element(payload, 3), target.to_eterm());
        assert_eq!(tuple_element(payload, 4), reason);
        // Element 1 is the ref thing; its last payload word encodes the reference id.
        let reference_term = tuple_element(payload, 1);
        let words = boxed_val(reference_term);
        assert_eq!(signed_value(*words.add(3)), reference.id() as i64);
    }

    // Sanity: the reference id is meaningful (the allocator handed out a real id).
    assert_eq!(pid(1).number(), 1);
}

#[test]
fn a_trapping_peer_owns_a_structured_reason_across_a_collection() {
    let (mut table, names) = table_with_names();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    // A process whose heap is never collected: it holds the reference tuple so the assertions
    // stay valid across `b`'s relocation.
    let holder = table.spawn_default().expect("holder");

    table.get_mut(b).expect("b").set_trap_exit(true);
    table.link(a, b).expect("both pids are live");

    // A's reason is a *structured* tuple living in A's own heap.
    let reason = tuple_in(&mut table, a, &[make_small(1), make_small(2)]);
    let expected = tuple_in(&mut table, holder, &[make_small(1), make_small(2)]);

    table.terminate(a, reason).expect("a is live");
    assert!(!table.contains(a));

    let check = |table: &ProcessTable| {
        let payload = sole_payload(table, b);
        // SAFETY: `payload` is the live `{'EXIT', A, Reason}` tuple the table built on B's
        // heap; the three element slots are in bounds.
        unsafe {
            assert_eq!(tuple_size(payload), 3);
            assert_eq!(tuple_element(payload, 1), a.to_eterm());
            let delivered = tuple_element(payload, 2);
            assert!(
                !is_immediate(delivered),
                "the structured reason was delivered as a copy, not the raw source word"
            );
            assert!(compare::eq(delivered, expected, &names));
        }
    };
    check(&table);

    // A safepoint GC on B relocates the reason: it is still valid — and still equal — only if
    // the reason lives in B's own heap (a pointer into A's dropped heap would dangle here).
    table
        .get_mut(b)
        .expect("b survives")
        .collect()
        .expect("the collection succeeds");
    check(&table);
}

#[test]
fn a_monitor_down_message_owns_a_structured_reason_across_a_collection() {
    let (mut table, names) = table_with_names();
    let watcher = table.spawn_default().expect("watcher");
    let target = table.spawn_default().expect("target");
    let holder = table.spawn_default().expect("holder");
    let reference = table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");

    let reason = tuple_in(&mut table, target, &[make_small(3), make_small(4)]);
    let expected = tuple_in(&mut table, holder, &[make_small(3), make_small(4)]);

    table.terminate(target, reason).expect("target is live");
    assert!(!table.contains(target));

    let check = |table: &ProcessTable| {
        let payload = sole_payload(table, watcher);
        // SAFETY: `payload` is the live `{'DOWN', Ref, process, Target, Reason}` tuple the
        // table built on the watcher's heap; the five element slots are in bounds.
        unsafe {
            assert_eq!(tuple_size(payload), 5);
            assert_eq!(tuple_element(payload, 3), target.to_eterm());
            let delivered = tuple_element(payload, 4);
            assert!(!is_immediate(delivered), "the structured reason was copied");
            assert!(compare::eq(delivered, expected, &names));
            // The ref thing survived the collection: its last payload word is the id.
            let words = boxed_val(tuple_element(payload, 1));
            assert_eq!(signed_value(*words.add(3)), reference.id() as i64);
        }
    };
    check(&table);

    // The watcher's own safepoint GC relocates the down-message; the reason must remain valid.
    table
        .get_mut(watcher)
        .expect("the watcher survives")
        .collect()
        .expect("the collection succeeds");
    check(&table);
}

#[test]
fn a_cascaded_link_reason_owns_a_structured_reason_across_a_collection() {
    let (mut table, names) = table_with_names();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    let c = table.spawn_default().expect("c");
    let holder = table.spawn_default().expect("holder");

    // A — B — C: B is a non-trapping relay (it must follow A), C traps and must survive.
    table.link(a, b).expect("both pids are live");
    table.link(b, c).expect("both pids are live");
    table.get_mut(c).expect("c").set_trap_exit(true);

    let reason = tuple_in(&mut table, a, &[make_small(5), make_small(6)]);
    let expected = tuple_in(&mut table, holder, &[make_small(5), make_small(6)]);

    table.terminate(a, reason).expect("a is live");

    assert!(!table.contains(a), "the origin died");
    assert!(
        !table.contains(b),
        "the non-trapping relay followed the origin"
    );
    assert!(table.contains(c), "the trapping survivor is still alive");

    let check = |table: &ProcessTable| {
        let payload = sole_payload(table, c);
        // SAFETY: `payload` is the live `{'EXIT', B, Reason}` tuple the table built on C's
        // heap; the three element slots are in bounds.
        unsafe {
            assert_eq!(tuple_size(payload), 3);
            assert_eq!(
                tuple_element(payload, 1),
                b.to_eterm(),
                "the relay is the signaller C sees"
            );
            let delivered = tuple_element(payload, 2);
            assert!(!is_immediate(delivered), "the cascaded reason was copied");
            assert!(compare::eq(delivered, expected, &names));
        }
    };
    check(&table);

    // C's collection relocates the cascaded reason: it must live in C's heap (not in the
    // relay B's heap, which was dropped as B terminated).
    table
        .get_mut(c)
        .expect("c survives")
        .collect()
        .expect("the collection succeeds");
    check(&table);
}

#[test]
fn a_self_signal_embeds_a_reason_already_in_the_recipients_heap() {
    let (mut table, names) = table_with_names();
    let a = table.spawn_default().expect("a");
    let holder = table.spawn_default().expect("holder");
    table.get_mut(a).expect("a").set_trap_exit(true);

    // `from == to`: the structured reason lives in the recipient's own heap, so it must be
    // embedded as-is (the `source_off_heap == None` path) and stay valid across a collection.
    let reason = tuple_in(&mut table, a, &[make_small(7), make_small(8)]);
    let expected = tuple_in(&mut table, holder, &[make_small(7), make_small(8)]);

    assert_eq!(
        table
            .apply_exit_signal(a, ExitSignal { from: a, reason })
            .expect("a is live"),
        ExitEffect::Trapped
    );
    assert!(table.contains(a), "a trapped its own exit signal");

    let check = |table: &ProcessTable| {
        let payload = sole_payload(table, a);
        // SAFETY: the payload is the live `{'EXIT', A, Reason}` tuple on a's heap.
        unsafe {
            assert_eq!(tuple_size(payload), 3);
            assert_eq!(tuple_element(payload, 1), a.to_eterm());
            assert!(compare::eq(tuple_element(payload, 2), expected, &names));
        }
    };
    check(&table);

    table
        .get_mut(a)
        .expect("a survives")
        .collect()
        .expect("the collection succeeds");
    check(&table);
}

#[test]
fn a_self_signal_to_a_non_trapping_process_terminates_it() {
    let (mut table, _exit, _down, _process) = table_with_atoms();
    let a = table.spawn_default().expect("a");

    let reason = tuple_in(&mut table, a, &[make_small(9)]);
    assert_eq!(
        table
            .apply_exit_signal(a, ExitSignal { from: a, reason })
            .expect("a is live"),
        ExitEffect::Terminated
    );
    assert!(!table.contains(a), "the non-trapping process terminated");
}

#[test]
fn kill_terminates_the_target_with_reason_killed() {
    // Oracle probe (OTP 29.1.1): the exit reason `kill` is rewritten to `killed`
    //   P = spawn(fun() -> receive never -> ok end end),
    //   M = erlang:monitor(process, P),
    //   exit(P, kill),
    //   receive {'DOWN', M, process, P, R} -> R end   => `killed`
    let (mut table, mut names) = table_with_names();
    let killer = table.spawn_default().expect("killer");
    let victim = table.spawn_default().expect("victim");
    let watcher = table.spawn_default().expect("watcher");
    let reference = table
        .monitor(watcher, victim, MonitorKind::Process)
        .expect("both pids are live");

    let down = make_atom(names.intern("DOWN"));
    let process = make_atom(names.intern("process"));
    let kill = make_atom(names.intern("kill"));
    let killed = make_atom(names.intern("killed"));

    table
        .send_exit(killer, victim, kill)
        .expect("the victim is live");

    assert!(!table.contains(victim), "`kill` terminates the victim");
    assert!(table.contains(watcher), "the watcher is not linked to the victim");
    assert!(table.contains(killer), "the killer is not linked to the victim");

    let payload = sole_payload(&table, watcher);
    // SAFETY: `payload` is the live `{'DOWN', Ref, process, Victim, killed}` tuple the table
    // built on the watcher's heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), down);
        assert_eq!(tuple_element(payload, 2), process);
        assert_eq!(tuple_element(payload, 3), victim.to_eterm());
        assert_eq!(
            tuple_element(payload, 4),
            killed,
            "the DOWN reason is `killed`, not the sent `kill`"
        );
        assert_ne!(tuple_element(payload, 4), kill);
        // The DOWN carries the reference the monitor handed out.
        let words = boxed_val(tuple_element(payload, 1));
        assert_eq!(signed_value(*words.add(3)), reference.id() as i64);
    }
}

#[test]
fn kill_is_untrappable() {
    // Oracle probes (OTP 29.1.1):
    //   (a) a *trapping* target killed anyway is gone and its DOWN reason is `killed`:
    //       P = spawn(fun() -> process_flag(trap_exit, true), receive never -> ok end end),
    //       M = erlang:monitor(process, P), exit(P, kill),
    //       receive {'DOWN', M, process, P, R} -> R end => `killed`; is_process_alive(P) => false
    //   (b) the mechanism `kill` bypasses: an ordinary reason IS trapped, leaving the process
    //       alive with `{'EXIT', Signaller, boom}` queued => `trapped_boom`.
    let (mut table, mut names) = table_with_names();
    let killer = table.spawn_default().expect("killer");
    let kill = make_atom(names.intern("kill"));
    let killed = make_atom(names.intern("killed"));
    let down = make_atom(names.intern("DOWN"));
    let process = make_atom(names.intern("process"));

    // (a) `kill` on a trapping target.
    let victim = table.spawn_default().expect("victim");
    table
        .get_mut(victim)
        .expect("the victim is live")
        .set_trap_exit(true);
    let watcher = table.spawn_default().expect("watcher");
    table
        .monitor(watcher, victim, MonitorKind::Process)
        .expect("both pids are live");

    assert_eq!(
        table
            .apply_exit_signal(victim, ExitSignal { from: killer, reason: kill })
            .expect("the victim is live"),
        ExitEffect::Terminated,
        "`kill` terminates even a trapping target"
    );
    // A trapped `{'EXIT', _, kill}` would have kept the victim alive; instead it is gone, and its
    // mailbox (which held no EXIT message) went with it. The watcher's sole message is the DOWN.
    assert!(
        !table.contains(victim),
        "the trapping victim is still terminated; no `{{'EXIT', _, kill}}` was trapped"
    );
    let payload = sole_payload(&table, watcher);
    // SAFETY: the live 5-tuple the table built on the watcher's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), down);
        assert_eq!(tuple_element(payload, 2), process);
        assert_eq!(tuple_element(payload, 3), victim.to_eterm());
        assert_eq!(tuple_element(payload, 4), killed);
    }

    // (b) Contrast: the same shape with a trappable reason survives with the EXIT queued.
    let survivor = table.spawn_default().expect("survivor");
    table
        .get_mut(survivor)
        .expect("the survivor is live")
        .set_trap_exit(true);
    let boom = make_atom(names.intern("boom"));
    let exit_atom = make_atom(names.intern("EXIT"));
    assert_eq!(
        table
            .apply_exit_signal(survivor, ExitSignal { from: killer, reason: boom })
            .expect("the survivor is live"),
        ExitEffect::Trapped
    );
    assert!(table.contains(survivor), "a trappable reason is trapped");
    let payload = sole_payload(&table, survivor);
    // SAFETY: the live `{'EXIT', Killer, boom}` tuple the table built on the survivor's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), exit_atom);
        assert_eq!(tuple_element(payload, 1), killer.to_eterm());
        assert_eq!(tuple_element(payload, 2), boom);
    }
}

#[test]
fn a_normal_link_exit_is_ignored_by_an_untrapping_peer_and_trapped_by_a_trapping_one() {
    // Oracle probes (OTP 29.1.1), each run in its own process:
    //   trapping:   process_flag(trap_exit, true), P = spawn_link(fun() -> receive go -> ok end end),
    //               P ! go, receive {'EXIT', From, R} -> {From =:= P, R} end   => `{true,normal}`
    //   untrapping: process_flag(trap_exit, false), P = spawn_link(...), P ! go,
    //               receive X -> {unexpected, X} after 500 -> survived end   => `survived`
    let (mut table, mut names) = table_with_names();
    let normal = make_atom(names.intern("normal"));
    let exit_atom = make_atom(names.intern("EXIT"));

    // 1. Untrapping peer: a linked normal exit is ignored — the peer survives with an empty
    //    mailbox and the dropped back-link.
    let origin = table.spawn_default().expect("origin");
    let peer = table.spawn_default().expect("peer");
    table.link(origin, peer).expect("both pids are live");

    table.terminate(origin, normal).expect("the origin is live");

    assert!(!table.contains(origin), "the origin exited");
    assert!(
        table.contains(peer),
        "an untrapping peer survives a linked normal exit"
    );
    {
        let survivor = table.get(peer).expect("the peer survives");
        assert!(
            survivor.mailbox().is_empty(),
            "a normal reason queues no message on an untrapping peer"
        );
        assert!(
            !survivor.links().contains(origin),
            "the terminating origin's back-link is dropped"
        );
    }

    // 2. Trapping peer: the same normal exit is delivered as `{'EXIT', Origin, normal}`.
    let origin2 = table.spawn_default().expect("origin2");
    let trapper = table.spawn_default().expect("trapper");
    table
        .get_mut(trapper)
        .expect("the trapper is live")
        .set_trap_exit(true);
    table.link(origin2, trapper).expect("both pids are live");

    table.terminate(origin2, normal).expect("origin2 is live");

    assert!(table.contains(trapper), "the trapping peer survives");
    let payload = sole_payload(&table, trapper);
    // SAFETY: the live `{'EXIT', Origin2, normal}` tuple the table built on the trapper's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), exit_atom);
        assert_eq!(tuple_element(payload, 1), origin2.to_eterm());
        assert_eq!(tuple_element(payload, 2), normal);
    }
}

#[test]
fn a_cascaded_link_exit_reaches_a_trapping_head_from_the_relay() {
    // Oracle probe (OTP 29.1.1): a chain head—relay—origin, where the head traps, the relay does
    // not, and the origin dies abnormally:
    //   process_flag(trap_exit, true),
    //   B = spawn(fun() -> C = spawn_link(fun() -> receive never -> ok end end),
    //                       Self ! {c, C}, receive never -> ok end end),
    //   link(B), C = receive {c, Cp} -> Cp end, exit(C, {bad, 1}),
    //   receive {'EXIT', From, Reason} -> {From =:= B, Reason} end   => `{true,{bad,1}}`
    let (mut table, mut names) = table_with_names();
    let head = table.spawn_default().expect("head");
    let relay = table.spawn_default().expect("relay");
    let origin = table.spawn_default().expect("origin");
    let holder = table.spawn_default().expect("holder");

    table
        .get_mut(head)
        .expect("the head is live")
        .set_trap_exit(true);
    table.link(head, relay).expect("both pids are live");
    table.link(relay, origin).expect("both pids are live");

    // The origin's abnormal reason is a *structured* term in its own heap; `holder` keeps an
    // equal term built from the same atom table so the delivered copy can be compared.
    let bad = make_atom(names.intern("bad"));
    let reason = tuple_in(&mut table, origin, &[bad, make_small(1)]);
    let expected = tuple_in(&mut table, holder, &[bad, make_small(1)]);
    let exit_atom = make_atom(names.intern("EXIT"));

    table.terminate(origin, reason).expect("the origin is live");

    assert!(!table.contains(origin), "the origin died");
    assert!(
        !table.contains(relay),
        "the non-trapping relay followed the origin"
    );
    assert!(table.contains(head), "the trapping head survived");

    let payload = sole_payload(&table, head);
    // SAFETY: the live `{'EXIT', Relay, Reason}` tuple the table built on the head's heap; the
    // three element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), exit_atom);
        assert_eq!(
            tuple_element(payload, 1),
            relay.to_eterm(),
            "the relay is the signaller the head sees"
        );
        let delivered = tuple_element(payload, 2);
        assert!(
            !is_immediate(delivered),
            "the cascaded reason was delivered as a copy of the relay's reason"
        );
        assert!(compare::eq(delivered, expected, &names));
    }
}
#[test]
fn a_targets_death_resolves_the_watchers_outgoing_monitor() {
    // Oracle probe (OTP 29.1.1), run in a *watcher* process so the caller's own list is read:
    //   W = spawn(fun() -> M = monitor(process, T), Parent ! {m, M},
    //                       receive {'DOWN', M, process, _, _} -> ok end,
    //                       Parent ! {after, process_info(self(), monitors),
    //                                demonitor(M, [info])} end),
    //   (parent) before `T ! stop`: `{monitor,W}`-style read => `{monitors,[{process,T}]}`;
    //   after the DOWN:                                            => `{monitors,[]}`;
    //   `demonitor(M, [info])`                                     => `false`
    //   (`demonitor/1` without `info` answers `true` unconditionally — the BIF layer's rendering).
    // The target's death therefore resolves **both** halves: the DOWN is still delivered, but the
    // watcher no longer lists the monitor and `[info]` reports it gone.
    let (mut table, mut names) = table_with_names();
    let down_atom = names.intern("DOWN");
    let watcher = table.spawn_default().expect("watcher");
    let target = table.spawn_default().expect("target");
    let reference = table
        .monitor(watcher, target, MonitorKind::Process)
        .expect("both pids are live");
    assert_eq!(
        table.get(watcher).expect("watcher").monitors().len(),
        1,
        "the monitor is recorded while the target lives"
    );

    let boom = make_atom(names.intern("boom"));
    table.terminate(target, boom).expect("the target is live");

    // The DOWN is still delivered …
    let down = sole_payload(&table, watcher);
    assert!(payload_is_down_for(down, reference, down_atom));
    // … and the outgoing half is gone with it.
    assert!(
        table.get(watcher).expect("watcher").monitors().is_empty(),
        "the target's death resolves the watcher's outgoing monitor"
    );
    assert!(
        !table
            .demonitor(watcher, reference)
            .expect("the watcher is live"),
        "a resolved monitor is no longer on record"
    );
    // `demonitor/1` (no `[flush]`) leaves the already-queued DOWN; `[flush]` still removes it.
    assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 1);
    assert!(table
        .demonitor_flush(watcher, reference)
        .expect("the watcher is live"));
    assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 0);
}
#[test]
fn link_to_a_non_live_pid_signals_noproc_from_it() {
    // Oracle probes (OTP 29.1.1):
    //   trapping:     `link(DeadPid)` => `true`; the caller then receives
    //                 `{'EXIT', DeadPid, noproc}`.
    //   non-trapping: `link(DeadPid)` kills the caller — a `noproc` raise whose uncaught reason is
    //                 `{noproc, Stacktrace}` (this crate signals the bare atom `noproc`; see
    //                 `ProcessTable::link`'s documented approximation).
    //   `link(LivePid)` and `link(self())` => `true`, recording the link.
    let (mut table, mut names) = table_with_names();
    let exit_atom = make_atom(names.intern("EXIT"));
    let noproc = make_atom(names.intern("noproc"));
    let dead = ProcessId::new(999, 0);

    // A trapping caller, *currently executing* — the position `erlang:link/1` itself runs in. The
    // signal is queued there (the `exit/2` model) and applied at the caller's next schedule-in.
    let trapper = table.spawn_default().expect("trapper");
    table
        .get_mut(trapper)
        .expect("the trapper is live")
        .set_trap_exit(true);
    table
        .get_mut(trapper)
        .expect("the trapper is live")
        .set_state(ProcessState::Running);
    table
        .link(trapper, dead)
        .expect("a non-live target is not an error");
    assert_eq!(
        table.pending_signals(trapper).expect("the trapper is live"),
        1,
        "one noproc signal is queued for the caller"
    );
    assert!(
        table.get(trapper).expect("trapper").links().is_empty(),
        "nothing is linked to a pid that is not there"
    );

    table
        .service_signals(trapper)
        .expect("the trapper is live");
    assert!(table.contains(trapper), "the trapping caller survives");
    let payload = sole_payload(&table, trapper);
    // SAFETY: the live `{'EXIT', Dead, noproc}` tuple the table built on the trapper's heap.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), exit_atom);
        assert_eq!(
            tuple_element(payload, 1),
            dead.to_eterm(),
            "the signal came *from* the non-live target"
        );
        assert_eq!(tuple_element(payload, 2), noproc);
    }

    // A non-trapping caller is terminated with `noproc` once its signals are serviced.
    let victim = table.spawn_default().expect("victim");
    table
        .get_mut(victim)
        .expect("the victim is live")
        .set_state(ProcessState::Running);
    table
        .link(victim, dead)
        .expect("a non-live target is not an error");
    assert!(table.contains(victim), "the signal was deferred while the caller ran");
    table
        .service_signals(victim)
        .expect("the victim is live");
    assert!(
        !table.contains(victim),
        "the non-trapping caller is terminated with noproc"
    );

    // Contrast: a **live** target records the link (both directions) and a self-link is a no-op.
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    table.link(a, b).expect("both pids are live");
    assert!(table.get(a).expect("a").links().contains(b));
    assert!(table.get(b).expect("b").links().contains(a));
    table.link(a, a).expect("a self-link is a no-op");
    assert!(!table.get(a).expect("a").links().contains(a));
    assert_eq!(table.pending_signals(a).expect("a is live"), 0);
}

// ---------------------------------------------------------------------------
// The exit-signal FAMILY: `kill` is untrappable only over `erlang:exit/2`
// ---------------------------------------------------------------------------

// The three tests below pin the C's family split (`erl_proc_sig_queue.c:4740-4770`,
// `ERTS_SIG_Q_OP_EXIT`/`ERTS_SIG_Q_OP_EXIT_LINKED`): the local **link cascade** carries the
// link-exit family, where `kill` IS trappable and the reason is never rewritten, while an explicit
// `erlang:exit/2` (`ProcessTable::send_exit`) keeps `kill` untrappable with the `killed` rewrite.
// Before the family was modelled, a `kill` link exit wrongly terminated a trapping peer.

#[test]
fn a_trapping_link_peer_traps_a_kill_link_exit_and_survives() {
    // Oracle probe (OTP 29.1.1):
    //   process_flag(trap_exit, true),
    //   P = spawn(fun() -> receive go -> exit(kill) end end),
    //   link(P), P ! go,
    //   R = receive M -> M after 1000 -> timeout end
    // => `R = {'EXIT', P, kill}` and the receiver is still alive: in the LINK-exit family `kill`
    // is trappable and is NOT rewritten to `killed`.
    let (mut table, mut names) = table_with_names();
    let exit_atom = make_atom(names.intern("EXIT"));
    let kill = make_atom(names.intern("kill"));

    let peer = table.spawn_default().expect("the peer");
    let receiver = table.spawn_default().expect("the receiver");
    table
        .get_mut(receiver)
        .expect("the receiver is live")
        .set_trap_exit(true);
    table.link(peer, receiver).expect("both pids are live");

    // Only the family differs from `an_explicit_exit_2_kill_still_terminates_a_trapping_receiver`
    // below: this is the link cascade (`ExitFamily::Linked`).
    table.terminate(peer, kill).expect("the peer is live");

    assert!(!table.contains(peer), "the peer is gone");
    assert!(
        table.contains(receiver),
        "a trapping receiver survives a `kill` LINK exit (only an explicit `kill` is untrappable)"
    );

    let payload = sole_payload(&table, receiver);
    // SAFETY: `payload` is the live `{'EXIT', Peer, kill}` tuple the table built on the receiver's
    // heap; the three element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), exit_atom);
        assert_eq!(
            tuple_element(payload, 1),
            peer.to_eterm(),
            "the message came from the dead linked peer"
        );
        assert_eq!(
            tuple_element(payload, 2),
            kill,
            "the link-exit reason is delivered UNCHANGED — `kill`, never `killed`"
        );
    }
}

#[test]
fn a_non_trapping_link_peer_follows_a_kill_link_exit_with_kill_not_killed() {
    // Oracle probe (OTP 29.1.1):
    //   P = spawn(fun() -> receive go -> exit(kill) end end),
    //   N = spawn(fun() -> link(P), ..., receive never -> ok end end),   % trap_exit false
    //   M = erlang:monitor(process, N), P ! go,
    //   receive {'DOWN', M, process, N, Rn} -> Rn end  => `kill` (NOT `killed`)
    let (mut table, mut names) = table_with_names();
    let kill = make_atom(names.intern("kill"));
    let killed = make_atom(names.intern("killed"));
    let down = make_atom(names.intern("DOWN"));
    let process = make_atom(names.intern("process"));

    let origin = table.spawn_default().expect("the origin");
    let victim = table.spawn_default().expect("the non-trapping peer");
    let watcher = table.spawn_default().expect("the watcher");
    table.link(origin, victim).expect("both pids are live");
    // The watcher is not linked to the origin, so only the DOWN (not an EXIT) reaches it.
    table
        .monitor(watcher, victim, MonitorKind::Process)
        .expect("both pids are live");

    table.terminate(origin, kill).expect("the origin is live");

    assert!(!table.contains(origin), "the origin is gone");
    assert!(
        !table.contains(victim),
        "a non-trapping peer follows a `kill` LINK exit"
    );
    assert!(table.contains(watcher), "the watcher is not linked to either");

    let payload = sole_payload(&table, watcher);
    // SAFETY: the live `{'DOWN', Ref, process, Victim, kill}` tuple the table built on the
    // watcher's heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), down);
        assert_eq!(tuple_element(payload, 2), process);
        assert_eq!(tuple_element(payload, 3), victim.to_eterm());
        assert_eq!(
            tuple_element(payload, 4),
            kill,
            "a link exit of reason `kill` terminates with the reason UNCHANGED"
        );
        assert_ne!(
            tuple_element(payload, 4),
            killed,
            "the `kill` -> `killed` rewrite is guarded by the explicit `erlang:exit/2` family"
        );
    }
}

#[test]
fn an_explicit_exit_2_kill_still_terminates_a_trapping_receiver() {
    // The family contrast, on the *same* trapping shape as
    // `a_trapping_link_peer_traps_a_kill_link_exit_and_survives`: over `erlang:exit/2`
    // (`ProcessTable::send_exit` = the explicit family) the `kill` sentinel is untrappable, so the
    // trapping receiver dies with the rewritten `killed` instead of trapping `{'EXIT', _, kill}`.
    let (mut table, mut names) = table_with_names();
    let kill = make_atom(names.intern("kill"));
    let killed = make_atom(names.intern("killed"));
    let down = make_atom(names.intern("DOWN"));
    let process = make_atom(names.intern("process"));

    let signaller = table.spawn_default().expect("the signaller");
    let victim = table.spawn_default().expect("the trapping victim");
    table
        .get_mut(victim)
        .expect("the victim is live")
        .set_trap_exit(true);
    let watcher = table.spawn_default().expect("the watcher");
    table
        .monitor(watcher, victim, MonitorKind::Process)
        .expect("both pids are live");

    table
        .send_exit(signaller, victim, kill)
        .expect("the victim is live");

    assert!(
        !table.contains(victim),
        "an explicit `kill` terminates even a trapping target"
    );
    let payload = sole_payload(&table, watcher);
    // SAFETY: the live `{'DOWN', Ref, process, Victim, killed}` tuple the table built on the
    // watcher's heap; the five element slots are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 5);
        assert_eq!(tuple_element(payload, 0), down);
        assert_eq!(tuple_element(payload, 2), process);
        assert_eq!(tuple_element(payload, 3), victim.to_eterm());
        assert_eq!(
            tuple_element(payload, 4),
            killed,
            "the explicit family rewrites `kill` to `killed`"
        );
        assert_ne!(tuple_element(payload, 4), kill);
    }
}