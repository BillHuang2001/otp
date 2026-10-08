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

//! Integration evidence for the safepoint collection over the process root set.
//!
//! Covers CONTEXT.md Test Strategy item 4: live terms sitting in the young heap and reachable
//! only through an x-register, a stack word and a mailbox payload must all survive
//! [`Process::collect`] (relocated, content intact) while unrooted garbage is reclaimed.

use beam_process::prelude::*;
use beam_term::prelude::{make_small, tuple_element, tuple_size, NIL};

/// A pid with the given number and serial 0.
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

#[test]
fn collect_keeps_the_register_stack_and_mailbox_roots() {
    let mut process = Process::new(pid(1)).expect("the default heap allocates");

    // A live term reachable only through an x-register.
    let register_term = process
        .heap_mut()
        .tuple(&[make_small(1), make_small(2)])
        .expect("the register tuple fits");
    {
        let registers = process.registers_mut();
        // The live prefix is widened first; registers can only be written inside it.
        registers
            .x_mut()
            .set_live(1)
            .expect("1 <= MAX_REG, so the register is live");
        registers.x_mut().set(0, register_term).expect("0 < live");
    }

    // A live term reachable only through the Erlang stack.
    let stack_term = process
        .heap_mut()
        .cons(make_small(3), NIL)
        .expect("the stack cons fits");
    process.registers_mut().stack_mut().push(stack_term);

    // A live term reachable only through the mailbox: build it, then copy it in (the copy
    // lands in the same young heap and is what the message actually references).
    let mail_source = process
        .heap_mut()
        .tuple(&[make_small(5), make_small(6)])
        .expect("the mailbox tuple fits");
    let from = pid(2).to_eterm();
    process
        .deliver_simple(from, mail_source)
        .expect("the mailbox payload fits");

    // Unrooted garbage: reachable from nothing at all.
    process
        .heap_mut()
        .tuple(&[make_small(111), make_small(222)])
        .expect("the garbage tuple fits");

    let used_before = process.heap_used_words();
    assert!(used_before > 0);

    let collection = process.collect().expect("the collection succeeds");
    assert!(
        collection.stats.words_reclaimed > 0,
        "the unrooted garbage was reclaimed"
    );
    assert!(
        process.heap_used_words() < used_before,
        "the nursery shrank after the collection"
    );

    // The register root survived (relocated into the mature space).
    let register_after = process
        .registers()
        .x()
        .get(0)
        .expect("the register is still live");
    // SAFETY: `register_after` is the live 2-tuple the collector relocated; slots 0..2 are
    // in bounds.
    unsafe {
        assert_eq!(tuple_size(register_after), 2);
        assert_eq!(tuple_element(register_after, 0), make_small(1));
        assert_eq!(tuple_element(register_after, 1), make_small(2));
    }

    // The stack root survived (relocated).
    let stack_after = process
        .registers()
        .stack()
        .as_slice()
        .first()
        .copied()
        .expect("the stack word is still there");
    // SAFETY: `stack_after` is the live cons cell `[3 | NIL]` the collector relocated; its
    // two words are in bounds.
    unsafe {
        let cell = beam_term::prelude::list_val(stack_after);
        assert_eq!(*cell, make_small(3));
        assert_eq!(*cell.add(1), NIL);
    }

    // The mailbox payload survived (relocated) and is still queued.
    assert_eq!(process.mailbox().len(), 1, "the message survived the collection");
    let payload_after = process
        .mailbox()
        .peek()
        .expect("one message is queued")
        .payload();
    // SAFETY: `payload_after` is the live 2-tuple the collector relocated; slots 0..2 are in
    // bounds.
    unsafe {
        assert_eq!(tuple_size(payload_after), 2);
        assert_eq!(tuple_element(payload_after, 0), make_small(5));
        assert_eq!(tuple_element(payload_after, 1), make_small(6));
    }
}
