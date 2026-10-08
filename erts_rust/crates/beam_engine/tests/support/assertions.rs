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

//! **Shared test helpers** — term builders and asserters used across the
//! integration suites.
//!
//! Split out of the former single-file `support/mod.rs`; every `pub` item here is
//! re-exported unchanged by the [`support`](super) facade.
//!
//! The builders (`int_list`, `chars`, `keyed_tuple`, `term_tuple`, `term_list`,
//! `term_atom`) construct terms through the host's shared arena; the asserters
//! (`assert_str`, `assert_int_list`, `assert_atom`, `assert_term`, `assert_small`,
//! `assert_keyed_list`, `assert_int_pair_list`, `assert_int_queue`) compare with
//! `beam_term::cmp` (the module's exact `=:=` ordering) through
//! [`ModuleHost::atom_table`](super::ModuleHost::atom_table).

use beam_engine::terms;
use beam_term::{cmp, make_atom, make_small, Eterm};

use super::host::ModuleHost;

// ---------------------------------------------------------------------------
// Small list helpers shared by the tests
// ---------------------------------------------------------------------------

/// Builds a proper list of small integers.
pub fn int_list(host: &mut ModuleHost, values: &[i64]) -> Eterm {
    let elements: Vec<Eterm> = values.iter().map(|&v| make_small(v)).collect();
    terms::make_list(host, &elements).expect("build an int list")
}

/// An Erlang string (a list of character code points) as a term.
pub fn chars(host: &mut ModuleHost, text: &str) -> Eterm {
    let codes: Vec<i64> = text.chars().map(|c| i64::from(c as u32)).collect();
    int_list(host, &codes)
}

/// Asserts `got` is the Erlang string `want` (a list of character code points).
pub fn assert_str(host: &ModuleHost, got: Eterm, want: &str) {
    let codes: Vec<i64> = want.chars().map(|c| i64::from(c as u32)).collect();
    assert_int_list(host, got, &codes);
}

/// Asserts `got` is the proper list of the small integers `want`, comparing with
/// `beam_term::cmp` (the module's exact `=:=` ordering).
pub fn assert_int_list(host: &ModuleHost, got: Eterm, want: &[i64]) {
    let elements = terms::list_elements(got).expect("a proper list result");
    assert_eq!(
        elements.len(),
        want.len(),
        "list length: got {elements:?}, want {want:?}"
    );
    for (index, (&got, &want)) in elements.iter().zip(want.iter()).enumerate() {
        let expected = make_small(want);
        assert!(
            cmp(got, expected, host.atom_table()).is_eq(),
            "element {index}: got {got:?}, want {want}"
        );
    }
}

/// Asserts `got` is the shared atom `name` (resolved by name through the shared
/// table, so a term interned by any module compares equal).
pub fn assert_atom(host: &ModuleHost, got: Eterm, name: &str) {
    let atom = host
        .atom_table()
        .lookup(name)
        .unwrap_or_else(|| panic!("the shared atom table does not name `{name}`"));
    let expected = make_atom(atom);
    assert!(
        cmp(got, expected, host.atom_table()).is_eq(),
        "got {got:?}, want the atom `{name}`"
    );
}

/// The two-element tuple `{Key, Value}` where `Key` is the shared atom `key` and
/// `Value` the small integer `value` — the shape `proplists` operates on.
pub fn keyed_tuple(host: &mut ModuleHost, key: &str, value: i64) -> Eterm {
    let key = host.intern(key);
    terms::make_tuple(host, &[key, make_small(value)]).expect("build a 2-tuple")
}

/// Asserts `got` is the list of `{atom, small integer}` tuples `want`.
pub fn assert_keyed_list(host: &ModuleHost, got: Eterm, want: &[(&str, i64)]) {
    let elements = terms::list_elements(got).expect("a proper list result");
    assert_eq!(
        elements.len(),
        want.len(),
        "list length: got {elements:?}, want {want:?}"
    );
    for (index, (&got, &(key, value))) in elements.iter().zip(want.iter()).enumerate() {
        assert_eq!(
            terms::tuple_arity(got).ok(),
            Some(2),
            "element {index}: not a 2-tuple: {got:?}"
        );
        let got_key = terms::tuple_element(got, 0).expect("tuple key");
        let got_value = terms::tuple_element(got, 1).expect("tuple value");
        assert_atom(host, got_key, key);
        assert!(
            cmp(got_value, make_small(value), host.atom_table()).is_eq(),
            "element {index}: got {got_value:?}, want {value}"
        );
    }
}

/// Builds a tuple from the given terms (the test-side `put_tuple2`).
pub fn term_tuple(host: &mut ModuleHost, elems: &[Eterm]) -> Eterm {
    terms::make_tuple(host, elems).expect("build a tuple")
}

/// Builds a proper list from the given terms.
pub fn term_list(host: &mut ModuleHost, elems: &[Eterm]) -> Eterm {
    terms::make_list(host, elems).expect("build a list")
}

/// The shared atom `name` as a term (interning it if no module has yet).
pub fn term_atom(host: &mut ModuleHost, name: &str) -> Eterm {
    host.intern(name)
}

/// Asserts `got` and `want` are the same term under the module's exact (`=:=`)
/// ordering.
pub fn assert_term(host: &ModuleHost, got: Eterm, want: Eterm) {
    assert!(
        cmp(got, want, host.atom_table()).is_eq(),
        "got {got:?}, want {want:?}"
    );
}

/// Asserts `got` is the small integer `want`.
pub fn assert_small(host: &ModuleHost, got: Eterm, want: i64) {
    assert_term(host, got, make_small(want));
}

/// Asserts `got` is the proper list of `{integer, integer}` 2-tuples `want`.
pub fn assert_int_pair_list(host: &ModuleHost, got: Eterm, want: &[(i64, i64)]) {
    let elements = terms::list_elements(got).expect("a proper list result");
    assert_eq!(
        elements.len(),
        want.len(),
        "list length: got {elements:?}, want {want:?}"
    );
    for (index, (&got, &(first, second))) in elements.iter().zip(want.iter()).enumerate() {
        assert_eq!(
            terms::tuple_arity(got).ok(),
            Some(2),
            "element {index}: not a 2-tuple: {got:?}"
        );
        assert_small(host, terms::tuple_element(got, 0).expect("pair key"), first);
        assert_small(host, terms::tuple_element(got, 1).expect("pair value"), second);
    }
}

/// Asserts `got` is the 2-tuple `{Rear, Front}` of the two integer lists — the
/// internal shape of a `queue:queue/0` as the real `queue` module represents it.
pub fn assert_int_queue(host: &ModuleHost, got: Eterm, rear: &[i64], front: &[i64]) {
    assert_eq!(
        terms::tuple_arity(got).ok(),
        Some(2),
        "a queue is a 2-tuple: {got:?}"
    );
    assert_int_list(host, terms::tuple_element(got, 0).expect("rear"), rear);
    assert_int_list(host, terms::tuple_element(got, 1).expect("front"), front);
}
