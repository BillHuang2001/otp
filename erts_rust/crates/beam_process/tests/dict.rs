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

//! Integration evidence for the process dictionary [`Dict`] and its role as an in-heap
//! sub-rootset.
//!
//! Covers the `erlang:put/2`/`get/1`/`erase/1`/`get_keys/0`/`get_keys/1` semantics on the
//! [`Process`] PCB, and the CONTEXT.md requirement that a dictionary value that is a
//! **structured heap term** survives [`Process::collect`] (i.e. the dictionary is scanned as
//! a root — otherwise the copying GC would leave a dangling reference).

use beam_process::prelude::*;
use beam_term::prelude::{make_small, tuple_element, tuple_size, NIL};

/// A pid with the given number and serial 0.
fn pid(number: u64) -> ProcessId {
    ProcessId::new(number, 0)
}

#[test]
fn put_get_erase_round_trip_on_the_pcb() {
    let mut process = Process::new(pid(1)).expect("the default heap allocates");
    assert!(process.dict().is_empty());

    // First put is absent (the BIF layer surfaces this as the `undefined` atom).
    assert_eq!(process.dict_mut().put(make_small(1), make_small(10)), None);
    assert_eq!(process.dict().get(make_small(1)), Some(make_small(10)));

    // Replacing a key returns the OLD value; the key set is unchanged.
    assert_eq!(process.dict_mut().put(make_small(1), make_small(11)), Some(make_small(10)));
    assert_eq!(process.dict().get(make_small(1)), Some(make_small(11)));
    assert_eq!(process.dict().len(), 1);

    // Erase returns the stored value; a second erase and a get of an absent key are absent.
    assert_eq!(process.dict_mut().erase(make_small(1)), Some(make_small(11)));
    assert_eq!(process.dict().get(make_small(1)), None);
    assert_eq!(process.dict_mut().erase(make_small(1)), None);
    assert!(process.dict().is_empty());
}

#[test]
fn get_keys_and_get_keys_for_value_on_the_pcb() {
    let mut process = Process::new(pid(1)).expect("the default heap allocates");
    process.dict_mut().put(make_small(1), make_small(100));
    process.dict_mut().put(make_small(2), make_small(100));
    process.dict_mut().put(make_small(3), make_small(200));

    assert_eq!(
        process.dict().get_keys(),
        vec![make_small(1), make_small(2), make_small(3)]
    );
    assert_eq!(
        process.dict().get_keys_for_value(make_small(100)),
        vec![make_small(1), make_small(2)]
    );
    assert!(process.dict().get_keys_for_value(make_small(7)).is_empty());

    let removed = process.dict_mut().erase_all();
    assert_eq!(removed.len(), 3);
    assert!(process.dict().is_empty());
}

#[test]
fn a_structured_dict_value_survives_a_collection() {
    let mut process = Process::new(pid(1)).expect("the default heap allocates");

    // A dictionary value reachable only through the dictionary: a live 2-tuple.
    let key = make_small(7);
    let value = process
        .heap_mut()
        .tuple(&[make_small(1), make_small(2)])
        .expect("the dictionary tuple fits");
    process.dict_mut().put(key, value);

    // A dictionary *key* that is a structured heap term (a cons cell) must also be scanned.
    let tuple_key = process
        .heap_mut()
        .cons(make_small(3), make_small(4))
        .expect("the cons key fits");
    let indexed_value = process
        .heap_mut()
        .tuple(&[make_small(5), make_small(6)])
        .expect("the second tuple fits");
    process.dict_mut().put(tuple_key, indexed_value);

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

    // The immediate-keyed value survived (relocated) with its contents intact.
    let value_after = process.dict().get(key).expect("the value is still present");
    // SAFETY: `value_after` is the live 2-tuple the collector relocated; slots 0..2 are in
    // bounds.
    unsafe {
        assert_eq!(tuple_size(value_after), 2);
        assert_eq!(tuple_element(value_after, 0), make_small(1));
        assert_eq!(tuple_element(value_after, 1), make_small(2));
    }

    // The structured key survived too: the pairs keep insertion order, so the second entry
    // is the cons-keyed pair, reachable only through the (relocated) key/value words.
    let pairs: Vec<_> = process.dict().entries().collect();
    assert_eq!(pairs.len(), 2, "both dictionary entries survived");
    let (rebuilt_key, rebuilt_value) = pairs[1];
    // SAFETY: `rebuilt_key` is the live cons cell `[3 | 4]` the collector relocated; its two
    // words are in bounds.
    unsafe {
        let cell = beam_term::prelude::list_val(rebuilt_key);
        assert_eq!(*cell, make_small(3));
        assert_eq!(*cell.add(1), make_small(4));
    }
    // SAFETY: `rebuilt_value` is the live 2-tuple the collector relocated; slots 0..2 are in
    // bounds.
    unsafe {
        assert_eq!(tuple_size(rebuilt_value), 2);
        assert_eq!(tuple_element(rebuilt_value, 0), make_small(5));
        assert_eq!(tuple_element(rebuilt_value, 1), make_small(6));
    }
}

#[test]
fn a_dictionary_value_that_is_a_list_survives_a_collection() {
    let mut process = Process::new(pid(1)).expect("the default heap allocates");

    let key = make_small(42);
    let head = process
        .heap_mut()
        .cons(make_small(1), NIL)
        .expect("the list head fits");
    process.dict_mut().put(key, head);

    process.collect().expect("the collection succeeds");

    let head_after = process.dict().get(key).expect("the list is still present");
    // SAFETY: `head_after` is the live `[1 | NIL]` cons cell the collector relocated; its two
    // words are in bounds.
    unsafe {
        let cell = beam_term::prelude::list_val(head_after);
        assert_eq!(*cell, make_small(1));
        assert_eq!(*cell.add(1), NIL);
    }
}
