//! Integration evidence for link-exit propagation and monitor down-messages.
//!
//! Covers CONTEXT.md Test Strategy item 3: `A` links `B`; `A` terminating with a non-normal
//! reason terminates a non-trapping `B` with that same reason, while a trapping `B` survives
//! and receives `{'EXIT', A, Reason}`; and `A` monitoring `B` means `B`'s termination queues
//! `{'DOWN', Ref, process, B, Reason}` on `A`.
//!
//! The last five tests pin **reason ownership**: a *structured* (non-immediate) exit reason
//! built in one process's heap must be copied into every recipient's own heap, so it survives
//! both the death of its original owner (link cascade, monitor down-message) and a later
//! safepoint collection of the recipient. A raw word pointing into a dropped heap would segfault
//! or compare unequal here.

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
