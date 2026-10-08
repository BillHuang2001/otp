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

//! Oracle cross-check: the process dictionary's **key and value equality** against the real
//! system Erlang (`erl`, OTP 29.1.1 / erts-17.1).
//!
//! This is the crate-local slice of the workspace *compatibility gate* (`../tools/differential`
//! owns the full harness). `beam_process::Dict` compares keys — and, for `get_keys/1`, values —
//! with [`beam_term::compare::eq`], which reproduces the C's `EQ`
//! (`erts/emulator/beam/erl_utils.h:113`), so a *boxed* key must match a structurally equal but
//! **distinct** term: `erlang:put({a},1), erlang:get({a})` is `1`, not `undefined`. A raw-word
//! comparison (what `Dict` used to do) reports `undefined` for every boxed key.
//!
//! # Protocol
//!
//! Each check runs the equivalent Erlang expression under the system `erl`
//! (`-noshell -eval '<expr>, halt(0).'`, printing its observables with `io:format`) and asserts
//! the printed result, then asserts the same outcome through a Rust [`Dict`] built from the
//! same input terms. The expectation is pinned on **both** sides, so agreeing is a genuine
//! oracle agreement rather than a tautology.
//!
//! # Absence of the oracle is not a failure
//!
//! If `erl` is not on `PATH` (or does not answer within [`ERL_TIMEOUT`]) every check **skips
//! loudly** (a clear `eprintln!`) and returns, never failing the suite — `cargo test -p
//! beam_process` therefore stays green on a host without a system Erlang.
//!
//! # Iteration order
//!
//! `get/0`, `get_keys/0` and `erase/0` are compared as **sets** (the oracle is asked for
//! `lists:sort/1`): the oracle's order follows its hash array (see the `Dict` module docs for
//! the documented deviation), which is an orthogonal concern to term equality.

use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_process::prelude::Dict;
use beam_term::prelude::{
    heap_bits_size_words, make_atom, make_small, map_words_capacity, write_binary, write_float,
    write_map, AtomTable, Eterm, Heap, FLOAT_SIZE_WORDS, NIL,
};

/// Wall-clock budget for a single `erl` invocation before the oracle is declared unusable and
/// the affected check skips.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// The oracle driver (text protocol)
// ---------------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout.
///
/// Returns `None` — after printing a clear skip message — when `erl` cannot be spawned or does
/// not answer within [`ERL_TIMEOUT`]. A *spawned* `erl` that exits non-zero is a bug in the
/// embedded expression, so that is asserted rather than skipped.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    let code_for_thread = code.clone();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code_for_thread)
            .output();
        // The receiver may have timed out and gone away; that is fine.
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({e}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "`erl` exited with {} for expression:\n{code}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// Runs `expr` under the oracle and asserts the printed result is `expected`.
///
/// Returns `false` (after the driver's loud skip message) when the oracle is unusable, so a
/// caller can `return` early; an oracle that *answers with something else* is a failure.
fn expect_oracle(expr: &str, expected: &str) -> bool {
    let Some(out) = erl_stdout(expr) else {
        return false;
    };
    assert_eq!(
        out.trim(),
        expected,
        "the oracle disagrees with the expected result for:\n{expr}"
    );
    true
}

// ---------------------------------------------------------------------------
// Term builders (the Rust side of every comparison)
// ---------------------------------------------------------------------------

/// The storage every built term points into, plus the atom table they were interned against.
///
/// A `Dict` records raw words, so the terms must outlive it and the pointers must stay valid:
/// the arena and the side buffers are owned here for the whole test. The dictionary compares
/// through a clone of the same atom table, so atom names resolve exactly as they do in the
/// runtime (one shared, append-only index space).
struct Terms {
    atoms: AtomTable,
    heap: Heap,
    bufs: Vec<Vec<Eterm>>,
}

impl Terms {
    /// An empty term fixture with a small arena.
    fn new() -> Terms {
        Terms {
            atoms: AtomTable::new(),
            heap: Heap::new(1024),
            bufs: Vec::new(),
        }
    }

    /// The atom `name`, interned in this fixture's table.
    fn atom(&mut self, name: &str) -> Eterm {
        make_atom(self.atoms.intern(name))
    }

    /// A dictionary whose comparisons resolve atom names through this fixture's table.
    fn dict(&self) -> Dict {
        Dict::with_atoms(Rc::new(self.atoms.clone()))
    }

    /// The tuple `{elems…}`.
    fn tuple(&mut self, elems: &[Eterm]) -> Eterm {
        self.heap.tuple(elems).expect("the test arena holds the tuple")
    }

    /// The list `[elems…]`.
    fn list(&mut self, elems: &[Eterm]) -> Eterm {
        let mut tail = NIL;
        for &elem in elems.iter().rev() {
            tail = self.heap.cons(elem, tail).expect("the test arena holds the list");
        }
        tail
    }

    /// The float `value` (`[HEADER_FLONUM, bits]`).
    fn float(&mut self, value: f64) -> Eterm {
        let mut buf = vec![Eterm::default(); FLOAT_SIZE_WORDS];
        // SAFETY: `buf` owns `FLOAT_SIZE_WORDS` writable words and is moved (never reallocated)
        // into `self.bufs`, where it outlives the returned term.
        let term = unsafe { write_float(buf.as_mut_ptr(), value) };
        self.bufs.push(buf);
        term
    }

    /// The binary `<<bytes…>>`.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        let words = heap_bits_size_words(bytes.len() * 8);
        let mut buf = vec![Eterm::default(); words];
        // SAFETY: `buf` owns `heap_bits_size_words(len * 8)` writable words and outlives the
        // returned term through `self.bufs`.
        let term = unsafe { write_binary(buf.as_mut_ptr(), bytes) }
            .expect("a byte-aligned binary is buildable");
        self.bufs.push(buf);
        term
    }

    /// The map `#{pairs…}`.
    fn map(&mut self, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let mut buf = vec![Eterm::default(); map_words_capacity(pairs.len())];
        // SAFETY: `buf` owns `map_words_capacity(pairs.len())` writable words and outlives the
        // returned term through `self.bufs`; the keys come from this fixture's atom table.
        let term = unsafe { write_map(buf.as_mut_ptr(), pairs, &self.atoms) }
            .expect("the map is buildable");
        self.bufs.push(buf);
        term
    }
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// The exact reproducer: a tuple key is matched by a distinct but equal tuple.
#[test]
fn a_tuple_key_matches_a_distinct_but_equal_tuple() {
    // Oracle: `erlang:put({a},1), erlang:get({a})` -> 1.
    if !expect_oracle(r#"put({a},1), io:format("~p~n",[get({a})])"#, "1") {
        return;
    }

    let mut terms = Terms::new();
    let a = terms.atom("a");
    let stored = terms.tuple(&[a]);
    let looked_up = terms.tuple(&[a]);
    assert_ne!(
        stored, looked_up,
        "the two `{{a}}` tuples are distinct heap words"
    );

    let mut dict = terms.dict();
    assert_eq!(
        dict.put(stored, make_small(1)),
        None,
        "the key was absent, so nothing is replaced"
    );
    assert_eq!(dict.get(looked_up), Some(make_small(1)));
    assert_eq!(dict.len(), 1);
}

/// A list key — two cons chains, distinct heap words, same content.
#[test]
fn a_list_key_matches_a_distinct_but_equal_list() {
    if !expect_oracle(r#"put([1,2],7), io:format("~p~n",[get([1,2])])"#, "7") {
        return;
    }

    let mut terms = Terms::new();
    let stored = terms.list(&[make_small(1), make_small(2)]);
    let looked_up = terms.list(&[make_small(1), make_small(2)]);
    assert_ne!(stored, looked_up);

    let mut dict = terms.dict();
    dict.put(stored, make_small(7));
    assert_eq!(dict.get(looked_up), Some(make_small(7)));
}

/// A map key — a flat map is a boxed term, so the key is a distinct word too.
#[test]
fn a_map_key_matches_a_distinct_but_equal_map() {
    if !expect_oracle(r#"put(#{a=>1},7), io:format("~p~n",[get(#{a=>1})])"#, "7") {
        return;
    }

    let mut terms = Terms::new();
    let a = terms.atom("a");
    let stored = terms.map(&[(a, make_small(1))]);
    let looked_up = terms.map(&[(a, make_small(1))]);
    assert_ne!(stored, looked_up);

    let mut dict = terms.dict();
    dict.put(stored, make_small(7));
    assert_eq!(dict.get(looked_up), Some(make_small(7)));
}

/// A binary key.
#[test]
fn a_binary_key_matches_a_distinct_but_equal_binary() {
    if !expect_oracle(r#"put(<<1,2,3>>,7), io:format("~p~n",[get(<<1,2,3>>)])"#, "7") {
        return;
    }

    let mut terms = Terms::new();
    let stored = terms.binary(&[1, 2, 3]);
    let looked_up = terms.binary(&[1, 2, 3]);
    assert_ne!(stored, looked_up);

    let mut dict = terms.dict();
    dict.put(stored, make_small(7));
    assert_eq!(dict.get(looked_up), Some(make_small(7)));
}

/// Immediate keys (atoms and small integers) still round-trip, and a *different* key is absent.
#[test]
fn immediate_keys_round_trip_and_a_different_key_is_absent() {
    let expr = r#"put(a,1), put(2,3), io:format("~p|~p|~p~n",[get(a),get(2),get(zzz)])"#;
    if !expect_oracle(expr, "1|3|undefined") {
        return;
    }

    let mut terms = Terms::new();
    let a = terms.atom("a");
    let other = terms.atom("zzz");
    let mut dict = terms.dict();
    assert_eq!(dict.put(a, make_small(1)), None);
    assert_eq!(dict.put(make_small(2), make_small(3)), None);
    assert_eq!(dict.get(a), Some(make_small(1)));
    assert_eq!(dict.get(make_small(2)), Some(make_small(3)));
    assert_eq!(dict.get(other), None, "an atom that was never put");
}

/// Floats compare by `=:=`: `1.5 =/= 1` and `1.5 =/= 1.0`, so only the exact float key matches.
#[test]
fn a_float_key_is_compared_exactly() {
    let expr = r#"put(1.5,7), put(1,9), io:format("~p|~p|~p|~p~n",[get(1.5),get(1),get(1.0),get(2)])"#;
    if !expect_oracle(expr, "7|9|undefined|undefined") {
        return;
    }

    let mut terms = Terms::new();
    let one_and_a_half = terms.float(1.5);
    let one_and_a_half_again = terms.float(1.5);
    let one_point_oh = terms.float(1.0);
    let mut dict = terms.dict();
    assert_eq!(dict.put(one_and_a_half, make_small(7)), None);
    assert_eq!(dict.put(make_small(1), make_small(9)), None);
    assert_eq!(dict.get(one_and_a_half_again), Some(make_small(7)));
    assert_eq!(dict.get(make_small(1)), Some(make_small(9)));
    assert_eq!(dict.get(one_point_oh), None, "1.0 is not the key 1.5");
    assert_eq!(dict.get(make_small(2)), None);
}

/// Repeated puts of the same structured key: last put wins, `put` returns the replaced value and
/// the dictionary keeps exactly one entry for the key.
#[test]
fn repeated_puts_of_a_structured_key_replace_in_place() {
    let expr = r#"put({a},1), R = put({a},2), io:format("~p|~p|~p~n",[R,get({a}),length(get())])"#;
    if !expect_oracle(expr, "1|2|1") {
        return;
    }

    let mut terms = Terms::new();
    let a = terms.atom("a");
    let stored = terms.tuple(&[a]);
    let looked_up = terms.tuple(&[a]);

    let mut dict = terms.dict();
    assert_eq!(dict.put(stored, make_small(1)), None);
    assert_eq!(
        dict.put(looked_up, make_small(2)),
        Some(make_small(1)),
        "the second put returns the replaced value"
    );
    assert_eq!(dict.get(stored), Some(make_small(2)), "last put wins");
    assert_eq!(dict.get(looked_up), Some(make_small(2)));
    assert_eq!(dict.len(), 1, "one entry per key");
}

/// `erase/1` finds the pair through the structural key and returns the erased value.
#[test]
fn erase_removes_a_structurally_equal_key() {
    let expr = r#"put({a},1), E = erase({a}), io:format("~p|~p|~p~n",[E,get({a}),length(get())])"#;
    if !expect_oracle(expr, "1|undefined|0") {
        return;
    }

    let mut terms = Terms::new();
    let a = terms.atom("a");
    let stored = terms.tuple(&[a]);
    let looked_up = terms.tuple(&[a]);

    let mut dict = terms.dict();
    dict.put(stored, make_small(1));
    assert_eq!(dict.erase(looked_up), Some(make_small(1)));
    assert_eq!(dict.erase(looked_up), None, "a second erase finds nothing");
    assert_eq!(dict.get(stored), None);
    assert!(dict.is_empty());
}

/// `get_keys/1` compares **values** structurally: a boxed value is matched by a distinct but
/// equal term.
#[test]
fn get_keys_for_value_matches_a_distinct_but_equal_value() {
    let expr = r#"put(ka,{v}), put(kb,{v}), put(kc,[v]), io:format("~p~n",[lists:sort(get_keys({v}))])"#;
    if !expect_oracle(expr, "[ka,kb]") {
        return;
    }

    let mut terms = Terms::new();
    let (ka, kb, kc) = (terms.atom("ka"), terms.atom("kb"), terms.atom("kc"));
    let v = terms.atom("v");
    let stored = terms.tuple(&[v]);
    let stored_again = terms.tuple(&[v]);
    let boxed = terms.tuple(&[v]);
    let other = terms.list(&[v]);

    let mut dict = terms.dict();
    dict.put(ka, stored);
    dict.put(kb, stored_again);
    dict.put(kc, other);
    assert_eq!(
        dict.get_keys_for_value(boxed),
        vec![ka, kb],
        "both `{{v}}` values match, the `[v]` value does not"
    );
    assert!(dict.get_keys_for_value(make_small(0)).is_empty());
}

/// A nested composite key (tuple of list, map and binary) matches a distinct but equal term.
#[test]
fn a_nested_composite_key_matches() {
    let expr = r#"put({[1],#{a=>1},<<1>>},9), io:format("~p~n",[get({[1],#{a=>1},<<1>>})])"#;
    if !expect_oracle(expr, "9") {
        return;
    }

    let mut terms = Terms::new();
    let a = terms.atom("a");
    let build = |terms: &mut Terms| {
        let list = terms.list(&[make_small(1)]);
        let map = terms.map(&[(a, make_small(1))]);
        let binary = terms.binary(&[1]);
        terms.tuple(&[list, map, binary])
    };
    let stored = build(&mut terms);
    let looked_up = build(&mut terms);
    assert_ne!(stored, looked_up);

    let mut dict = terms.dict();
    dict.put(stored, make_small(9));
    assert_eq!(dict.get(looked_up), Some(make_small(9)));
}

/// Distinct-but-similar keys must **not** match: the fix must never over-match.
#[test]
fn similar_but_unequal_keys_are_distinct() {
    let expr = r#"put({a,1},x), put({b,1},y), put({a,2},z),
                  io:format("~p|~p|~p|~p~n",[get({a,1}),get({b,1}),get({a,2}),get({a,3})])"#;
    if !expect_oracle(expr, "x|y|z|undefined") {
        return;
    }

    let mut terms = Terms::new();
    let (a, b) = (terms.atom("a"), terms.atom("b"));
    let a1 = terms.tuple(&[a, make_small(1)]);
    let b1 = terms.tuple(&[b, make_small(1)]);
    let a2 = terms.tuple(&[a, make_small(2)]);
    let a3 = terms.tuple(&[a, make_small(3)]);
    let (x, y, z) = (terms.atom("x"), terms.atom("y"), terms.atom("z"));

    let mut dict = terms.dict();
    dict.put(a1, x);
    dict.put(b1, y);
    dict.put(a2, z);
    assert_eq!(dict.get(a1), Some(x));
    assert_eq!(dict.get(b1), Some(y));
    assert_eq!(dict.get(a2), Some(z));
    assert_eq!(dict.get(a3), None, "`{{a,3}}` was never stored");
    assert_eq!(dict.len(), 3);
}

/// `get_keys/0` reports the same **key set** as the oracle (compared as a set — see the module
/// docs on the iteration order).
#[test]
fn get_keys_reports_the_same_key_set() {
    let expr = r#"put(a,1), put(b,2), io:format("~p~n",[lists:sort(get_keys())])"#;
    if !expect_oracle(expr, "[a,b]") {
        return;
    }

    let mut terms = Terms::new();
    let (a, b) = (terms.atom("a"), terms.atom("b"));
    let mut dict = terms.dict();
    dict.put(a, make_small(1));
    dict.put(b, make_small(2));
    assert_eq!(dict.get_keys(), vec![a, b]);
    assert_eq!(dict.len(), 2);
}
