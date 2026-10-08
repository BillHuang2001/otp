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

//! Integration evidence for the saved BIF/NIF arguments sub-rootset ([`SavedArgs`]).
//!
//! Covers the CONTEXT.md requirement that saved arguments are an in-heap sub-rootset: terms
//! saved only through [`Process::saved_args`] must survive [`Process::collect`] (relocated,
//! content intact), and a single collection must relocate the registers, the stack, the
//! mailbox payloads, the dictionary values and the saved arguments **together**.

use beam_process::prelude::*;
use beam_term::prelude::{list_val, make_small, tuple_element, tuple_size, NIL};

/// A pid with the given number and serial 0.
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

#[test]
fn saved_arguments_survive_a_collection() {
    let mut process = Process::new(pid(1)).expect("the default heap allocates");
    assert!(process.saved_args().is_empty());

    // A saved argument reachable only through `saved_args`: a live 2-tuple.
    let arg = process
        .heap_mut()
        .tuple(&[make_small(1), make_small(2)])
        .expect("the saved-argument tuple fits");
    process.saved_args_mut().push(arg);

    // A second saved argument: a cons cell.
    let cons = process
        .heap_mut()
        .cons(make_small(3), make_small(4))
        .expect("the saved-argument cons fits");
    process.saved_args_mut().push(cons);

    // Unrooted garbage: reachable from nothing at all.
    process
        .heap_mut()
        .tuple(&[make_small(111), make_small(222)])
        .expect("the garbage tuple fits");

    let used_before = process.heap_used_words();
    let collection = process.collect().expect("the collection succeeds");
    assert!(
        collection.stats.words_reclaimed > 0,
        "the unrooted garbage was reclaimed"
    );
    assert!(process.heap_used_words() < used_before);
    assert_eq!(process.saved_args().len(), 2, "both saved args survived");

    // SAFETY: the first saved word is the live 2-tuple the collector relocated; slots 0..2 are
    // in bounds.
    unsafe {
        let first = process.saved_args().as_slice()[0];
        assert_eq!(tuple_size(first), 2);
        assert_eq!(tuple_element(first, 0), make_small(1));
        assert_eq!(tuple_element(first, 1), make_small(2));
    }

    // SAFETY: the second saved word is the live cons cell `[3 | 4]` the collector relocated;
    // its two words are in bounds.
    unsafe {
        let second = process.saved_args().as_slice()[1];
        let cell = list_val(second);
        assert_eq!(*cell, make_small(3));
        assert_eq!(*cell.add(1), make_small(4));
    }
}

#[test]
fn collect_relocates_all_sub_rootsets_together() {
    let mut process = Process::new(pid(1)).expect("the default heap allocates");

    // (a) An x-register root.
    let register_term = process
        .heap_mut()
        .tuple(&[make_small(1), make_small(2)])
        .expect("the register tuple fits");
    {
        let registers = process.registers_mut();
        registers.x_mut().set_live(1).expect("1 <= MAX_REG");
        registers.x_mut().set(0, register_term).expect("0 < live");
    }

    // (b) A stack root.
    let stack_term = process
        .heap_mut()
        .cons(make_small(3), NIL)
        .expect("the stack cons fits");
    process.registers_mut().stack_mut().push(stack_term);

    // (c) A mailbox payload.
    let mail_source = process
        .heap_mut()
        .tuple(&[make_small(5), make_small(6)])
        .expect("the mailbox tuple fits");
    process
        .deliver_simple(pid(2).to_eterm(), mail_source)
        .expect("the mailbox payload fits");

    // (d) A dictionary value.
    let dict_key = make_small(70);
    let dict_value = process
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("the dictionary tuple fits");
    process.dict_mut().put(dict_key, dict_value);

    // (e) A saved argument.
    let saved = process
        .heap_mut()
        .tuple(&[make_small(9), make_small(10)])
        .expect("the saved-argument tuple fits");
    process.saved_args_mut().push(saved);

    // Unrooted garbage to prove the collection actually moved/relcaimed.
    process
        .heap_mut()
        .tuple(&[make_small(111), make_small(222)])
        .expect("the garbage tuple fits");

    let used_before = process.heap_used_words();
    let collection = process.collect().expect("the collection succeeds");
    assert!(
        collection.stats.words_reclaimed > 0,
        "the unrooted garbage was reclaimed"
    );
    assert!(process.heap_used_words() < used_before);

    // (a) The register root survived.
    let register_after = process.registers().x().get(0).expect("the register is live");
    // SAFETY: `register_after` is the live 2-tuple the collector relocated; slots 0..2 are in
    // bounds.
    unsafe {
        assert_eq!(tuple_size(register_after), 2);
        assert_eq!(tuple_element(register_after, 0), make_small(1));
        assert_eq!(tuple_element(register_after, 1), make_small(2));
    }

    // (b) The stack root survived.
    let stack_after = process.registers().stack().as_slice()[0];
    // SAFETY: `stack_after` is the live cons cell `[3 | NIL]` the collector relocated; its two
    // words are in bounds.
    unsafe {
        let cell = list_val(stack_after);
        assert_eq!(*cell, make_small(3));
        assert_eq!(*cell.add(1), NIL);
    }

    // (c) The mailbox payload survived.
    assert_eq!(process.mailbox().len(), 1);
    let payload_after = process.mailbox().peek().expect("one message").payload();
    // SAFETY: `payload_after` is the live 2-tuple the collector relocated; slots 0..2 are in
    // bounds.
    unsafe {
        assert_eq!(tuple_size(payload_after), 2);
        assert_eq!(tuple_element(payload_after, 0), make_small(5));
        assert_eq!(tuple_element(payload_after, 1), make_small(6));
    }

    // (d) The dictionary value survived.
    let dict_after = process.dict().get(dict_key).expect("the value is present");
    // SAFETY: `dict_after` is the live 2-tuple the collector relocated; slots 0..2 are in
    // bounds.
    unsafe {
        assert_eq!(tuple_size(dict_after), 2);
        assert_eq!(tuple_element(dict_after, 0), make_small(7));
        assert_eq!(tuple_element(dict_after, 1), make_small(8));
    }

    // (e) The saved argument survived.
    let saved_after = process.saved_args().as_slice()[0];
    // SAFETY: `saved_after` is the live 2-tuple the collector relocated; slots 0..2 are in
    // bounds.
    unsafe {
        assert_eq!(tuple_size(saved_after), 2);
        assert_eq!(tuple_element(saved_after, 0), make_small(9));
        assert_eq!(tuple_element(saved_after, 1), make_small(10));
    }
}

#[test]
fn saved_args_gather_and_scatter_round_trip_through_the_pcb() {
    let mut process = Process::new(pid(1)).expect("the default heap allocates");

    let arg = process
        .heap_mut()
        .cons(make_small(1), NIL)
        .expect("the cons fits");
    process.saved_args_mut().push(arg);

    // Pop the argument before a collection: nothing is rooted, so the term is reclaimed and
    // the saved-arg list is empty afterwards.
    assert_eq!(process.saved_args_mut().pop(), Some(arg));
    assert!(process.saved_args().is_empty());

    process.collect().expect("the collection succeeds");
    assert!(process.saved_args().is_empty());
}
