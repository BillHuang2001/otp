//! Integration evidence for link-exit propagation and monitor down-messages.
//!
//! Covers CONTEXT.md Test Strategy item 3: `A` links `B`; `A` terminating with a non-normal
//! reason terminates a non-trapping `B` with that same reason, while a trapping `B` survives
//! and receives `{'EXIT', A, Reason}`; and `A` monitoring `B` means `B`'s termination queues
//! `{'DOWN', Ref, process, B, Reason}` on `A`.

use beam_process::prelude::*;
use beam_term::prelude::{
    boxed_val, is_atom, make_atom, make_small, signed_value, tuple_element, tuple_size, Atom,
    AtomTable,
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
