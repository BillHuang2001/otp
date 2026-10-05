//! Integration evidence for the copy-on-send messaging path and selective receive.
//!
//! Covers CONTEXT.md Test Strategy item 2: a term built in a sender's heap is copied into a
//! receiver's mailbox, [`Process::receive`] returns exactly the one matching message and
//! leaves the non-matching messages queued in FIFO order.

use beam_process::prelude::*;
use beam_term::prelude::{boxed_val, make_small, tuple_element, tuple_size};

/// A pid with serial 0 (the table is not involved in these tests).
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

#[test]
fn selective_receive_returns_one_match_and_keeps_the_rest_fifo() {
    let sender = Process::new(pid(1)).expect("the default heap allocates");
    let mut receiver = Process::new(pid(2)).expect("the default heap allocates");
    let from = sender.pid().to_eterm();

    for value in [1i64, 2, 3] {
        receiver
            .deliver(from, make_small(value), sender.off_heap())
            .expect("an immediate always fits");
    }
    assert_eq!(receiver.mailbox().len(), 3);

    let matched = receiver
        .receive(|message| message.payload() == make_small(2))
        .expect("the middle message matches");
    assert_eq!(matched.payload(), make_small(2));
    assert_eq!(matched.from(), from);
    assert_eq!(
        receiver.mailbox().len(),
        2,
        "the non-matching messages stay queued"
    );

    // The survivors keep their FIFO order: 1 then 3.
    assert_eq!(
        receiver.receive(|_| true).expect("1 is queued").payload(),
        make_small(1)
    );
    assert_eq!(
        receiver.receive(|_| true).expect("3 is queued").payload(),
        make_small(3)
    );
    assert!(receiver.receive(|_| true).is_none(), "the mailbox is empty");
}

#[test]
fn a_heap_term_is_copied_into_the_receiver_heap() {
    let mut sender = Process::new(pid(3)).expect("the default heap allocates");
    let mut receiver = Process::new(pid(4)).expect("the default heap allocates");
    let from = sender.pid().to_eterm();

    let original = sender
        .heap_mut()
        .tuple(&[make_small(7), make_small(8), make_small(9)])
        .expect("the tuple fits");
    assert!(sender.heap_used_words() > 0);

    receiver
        .deliver(from, original, sender.off_heap())
        .expect("the copy fits the receiver heap");

    assert_eq!(receiver.mailbox().len(), 1);
    assert!(
        receiver.heap_used_words() > 0,
        "the payload lives in the receiver's heap"
    );

    let message = receiver.receive(|_| true).expect("one message");
    let payload = message.payload();
    // SAFETY: `payload` is the live 3-tuple the copy engine wrote into the receiver's heap;
    // element indices 0..3 are in bounds.
    unsafe {
        assert_eq!(tuple_size(payload), 3);
        assert_eq!(tuple_element(payload, 0), make_small(7));
        assert_eq!(tuple_element(payload, 1), make_small(8));
        assert_eq!(tuple_element(payload, 2), make_small(9));
    }
    // The copy is a distinct object, not an alias of the sender's term.
    assert_ne!(boxed_val(payload), boxed_val(original));
}
