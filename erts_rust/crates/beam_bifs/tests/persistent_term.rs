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

//! Oracle-pinned integration tests for the `persistent_term` BIFs
//! (`src/persistent_term.rs`): `get/0`, `get/1`, `get/2`, `put/2`, `put_new/2`,
//! `erase/1` and `info/0`.
//!
//! They are driven through the **public** API only (`default_registry()` +
//! `BifRegistry::call`), in three layers:
//!
//! * **(a)** with **no** `PersistentTerm` hook installed, every key is the typed
//!   `unsupported_bif` capability gap — never `[]`, never `undefined`, never `ok`,
//!   never a made-up count (workspace `CONTEXT.md` §3);
//! * **(b)** with a mock `PersistentTerm` hook, the term shapes and the two
//!   `badarg` outcomes each key produces (an absent key for `get/1`, a `put_new/2`
//!   onto a *different* value), plus the bignum promotion of the `info/0` figures;
//! * **(c)** a live cross-check against the system `erl`, which **skips loudly**
//!   when the oracle is unavailable (the `tests/os_bif.rs` / `tests/proc_reg.rs`
//!   pattern).
//!
//! Every (b) golden carries the system-OTP probe that pinned it (OTP 29.1.1 /
//! erts-17.1).
//!
//! Structural comparisons use `beam_term::compare::eq`; small integers and atoms
//! are read with `beam_term::{signed_value, atom_val}`.

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{make_atom, make_small, signed_value, AtomTable, Eterm};
/// A generous heap: these BIFs build short lists, tuples and a two-pair map.
fn heap() -> Heap {
    Heap::new(4096).expect("test heap")
}

/// The atoms these tests resolve by name (`count`/`memory` are the `info/0` map
/// keys, which the BIF interns through the shared table).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "count", "memory"]);
    atoms
}

/// Calls `module:function/arity` through the full public registry.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm> {
    reg.call(module, function, arity, ctx, args)
}

/// The atom term for the seeded name `name`.
fn atom_term(atoms: &AtomTable, name: &str) -> Eterm {
    make_atom(atoms.lookup(name).expect("a seeded atom"))
}

/// Asserts `result` is the typed `unsupported_bif` capability gap.
fn assert_unsupported(result: Result<Eterm>) {
    match result {
        Err(err) => assert_eq!(
            err.as_atom(),
            Some("unsupported_bif"),
            "expected the typed unsupported_bif gap, got {err:?}"
        ),
        Ok(_) => panic!("expected the typed unsupported_bif gap, got a value"),
    }
}

/// Asserts `result` is `badarg` (the plain [`BeamError::BadArg`] the BIF layer
/// raises for the invalid cases it decides from the arguments/store outcome).
fn assert_badarg(result: Result<Eterm>) {
    match result {
        Err(err) => assert_eq!(
            err.as_atom(),
            Some("badarg"),
            "expected badarg, got {err:?}"
        ),
        Ok(_) => panic!("expected badarg, got a value"),
    }
}

/// The exact `u128` value of a term — a small integer or a heap bignum (the
/// `info/0` figures are promoted past the 60-bit range, `crate::arith::int_from_mag`).
fn as_u128(term: Eterm) -> u128 {
    match beam_codec::term::big_parts(term) {
        Ok((negative, digits)) => {
            assert!(!negative, "an info figure is never negative");
            let mut magnitude: u128 = 0;
            for (i, limb) in digits.iter().enumerate() {
                let shift = 64 * i as u32;
                if shift >= 128 {
                    break;
                }
                magnitude |= u128::from(*limb) << shift;
            }
            magnitude
        }
        Err(_) => {
            let small = signed_value(term);
            assert!(small >= 0, "an info figure is never negative");
            u128::try_from(small).expect("a small integer is non-negative here")
        }
    }
}

/// A `PersistentTerm` test double over an ordered pair list, matching the
/// `put_common` semantics (`erl_bif_persistent.c:642-784`): a key that already
/// holds the same value is a no-op `ok` and one holding a different value makes
/// `put_new` answer `false` (the oracle's `badarg`). It records the calls so the
/// tests can assert the BIF really routed through the seam.
#[derive(Default)]
struct MockPersistentTerm {
    entries: Vec<(Eterm, Eterm)>,
    /// The byte total `info/0` reports (runtime bookkeeping the hook owns).
    memory: u64,
    puts: Vec<(Eterm, Eterm, bool)>,
    erases: Vec<Eterm>,
}

impl MockPersistentTerm {
    /// Seeds one pair directly (bypassing the BIF), for a pre-populated store.
    fn seed(&mut self, key: Eterm, value: Eterm) {
        self.entries.push((key, value));
    }
}

impl PersistentTerm for MockPersistentTerm {
    fn get(&mut self, key: Eterm) -> Result<Option<Eterm>> {
        Ok(self
            .entries
            .iter()
            .find(|&&(stored, _)| stored == key)
            .map(|&(_, value)| value))
    }

    fn put(&mut self, key: Eterm, value: Eterm, only_if_new: bool) -> Result<bool> {
        self.puts.push((key, value, only_if_new));
        match self.entries.iter().position(|&(stored, _)| stored == key) {
            Some(index) if self.entries[index].1 == value => Ok(true),
            Some(_) if only_if_new => Ok(false),
            Some(index) => {
                self.entries[index].1 = value;
                Ok(true)
            }
            None => {
                self.entries.push((key, value));
                Ok(true)
            }
        }
    }

    fn erase(&mut self, key: Eterm) -> Result<bool> {
        self.erases.push(key);
        match self.entries.iter().position(|&(stored, _)| stored == key) {
            Some(index) => {
                self.entries.remove(index);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn get_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
        Ok(self.entries.clone())
    }

    fn info(&mut self) -> Result<(u64, u64)> {
        Ok((self.entries.len() as u64, self.memory))
    }
}

/// A `PersistentTerm` whose every operation fails, to prove the hook's own error
/// is propagated verbatim (never swallowed, never rewritten as the typed gap).
struct FailingPersistentTerm;

impl PersistentTerm for FailingPersistentTerm {
    fn get(&mut self, _key: Eterm) -> Result<Option<Eterm>> {
        Err(BeamError::SystemLimit)
    }

    fn put(&mut self, _key: Eterm, _value: Eterm, _only_if_new: bool) -> Result<bool> {
        Err(BeamError::SystemLimit)
    }

    fn erase(&mut self, _key: Eterm) -> Result<bool> {
        Err(BeamError::SystemLimit)
    }

    fn get_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
        Err(BeamError::SystemLimit)
    }

    fn info(&mut self) -> Result<(u64, u64)> {
        Err(BeamError::SystemLimit)
    }
}

// ---------------------------------------------------------------------------
// (a) no hook: every key is the typed capability gap
// ---------------------------------------------------------------------------

#[test]
fn every_persistent_term_bif_is_unsupported_without_a_hook() {
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    assert_unsupported(call(&reg, &mut ctx, "persistent_term", "get", 0, &[]));
    assert_unsupported(call(
        &reg,
        &mut ctx,
        "persistent_term",
        "get",
        1,
        &[make_small(1)],
    ));
    assert_unsupported(call(
        &reg,
        &mut ctx,
        "persistent_term",
        "get",
        2,
        &[make_small(1), make_small(2)],
    ));
    assert_unsupported(call(
        &reg,
        &mut ctx,
        "persistent_term",
        "put",
        2,
        &[make_small(1), make_small(2)],
    ));
    assert_unsupported(call(
        &reg,
        &mut ctx,
        "persistent_term",
        "put_new",
        2,
        &[make_small(1), make_small(2)],
    ));
    assert_unsupported(call(
        &reg,
        &mut ctx,
        "persistent_term",
        "erase",
        1,
        &[make_small(1)],
    ));
    assert_unsupported(call(&reg, &mut ctx, "persistent_term", "info", 0, &[]));
}

#[test]
fn the_seams_own_failures_are_propagated_not_swallowed() {
    // The three-layer (a) test above proves the *missing* seam is honest; this
    // proves a *present* seam's failure surfaces as its own error (here the
    // distinctive `system_limit`), never as the typed gap and never as a value.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = FailingPersistentTerm;
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_persistent_term(&mut hook);

    for (name, arity, args) in [
        ("get", 0u32, vec![]),
        ("get", 1, vec![make_small(1)]),
        ("get", 2, vec![make_small(1), make_small(2)]),
        ("put", 2, vec![make_small(1), make_small(2)]),
        ("put_new", 2, vec![make_small(1), make_small(2)]),
        ("erase", 1, vec![make_small(1)]),
        ("info", 0, vec![]),
    ] {
        let err = call(&reg, &mut ctx, "persistent_term", name, arity, &args)
            .expect_err("the hook fails");
        assert_eq!(
            err.as_atom(),
            Some("system_limit"),
            "persistent_term:{name}/{arity} must propagate the hook's failure, got {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// (b) a mock store: term shapes and the two `badarg` outcomes
// ---------------------------------------------------------------------------

#[test]
fn put_returns_ok_and_get_round_trips_the_value() {
    // Probed: `persistent_term:put(k,1)` ⇒ `ok`, `persistent_term:get(k)` ⇒ `1`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();

    let (ok, value) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_persistent_term(&mut hook);
        let ok = call(
            &reg,
            &mut ctx,
            "persistent_term",
            "put",
            2,
            &[make_small(7), make_small(11)],
        )
        .expect("the hook answers");
        let value = call(
            &reg,
            &mut ctx,
            "persistent_term",
            "get",
            1,
            &[make_small(7)],
        )
        .expect("the hook answers");
        (ok, value)
    };

    assert_eq!(ok, atom_term(&atoms, "ok"));
    assert_eq!(value, make_small(11));
    // The seam really carried `only_if_new = false` (the `put/2` arity).
    assert_eq!(hook.puts, vec![(make_small(7), make_small(11), false)]);
}

#[test]
fn get_of_an_absent_key_is_badarg_while_get_2_yields_the_default() {
    // Probed: `catch persistent_term:get(nope)` ⇒ `EXIT`; `get(nope, default)` ⇒
    // `default`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();

    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_persistent_term(&mut hook);
    assert_badarg(call(
        &reg,
        &mut ctx,
        "persistent_term",
        "get",
        1,
        &[make_small(9)],
    ));

    let default = make_small(42);
    let value = call(
        &reg,
        &mut ctx,
        "persistent_term",
        "get",
        2,
        &[make_small(9), default],
    )
    .expect("the hook answers");
    assert_eq!(value, default, "an absent key yields the caller's default");
}

#[test]
fn get_2_prefers_the_stored_value_over_the_default() {
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();
    hook.seed(make_small(1), make_small(99));

    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_persistent_term(&mut hook);
    let value = call(
        &reg,
        &mut ctx,
        "persistent_term",
        "get",
        2,
        &[make_small(1), make_small(42)],
    )
    .expect("the hook answers");
    assert_eq!(value, make_small(99));
}

#[test]
fn put_new_is_ok_for_a_new_key_and_badarg_for_a_different_value() {
    // Probed: `put_new(n, a)` ⇒ `ok`; a second `put_new(n, a)` ⇒ `ok` (the same
    // value is a no-op); `put_new(n, b)` ⇒ `EXIT badarg`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();

    let (ok, same, collision) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_persistent_term(&mut hook);

        let ok = call(
            &reg,
            &mut ctx,
            "persistent_term",
            "put_new",
            2,
            &[make_small(1), make_small(10)],
        )
        .expect("the hook answers");

        // The same value again is the oracle's no-op `ok`.
        let same = call(
            &reg,
            &mut ctx,
            "persistent_term",
            "put_new",
            2,
            &[make_small(1), make_small(10)],
        )
        .expect("the hook answers");

        // A different value raises `badarg`.
        let collision = call(
            &reg,
            &mut ctx,
            "persistent_term",
            "put_new",
            2,
            &[make_small(1), make_small(11)],
        );
        (ok, same, collision)
    };

    assert_eq!(ok, atom_term(&atoms, "ok"));
    assert_eq!(same, atom_term(&atoms, "ok"));
    assert_badarg(collision);

    // The seam received `only_if_new = true` for each of the three calls and the
    // store was only ever populated once.
    assert_eq!(
        hook.puts,
        vec![
            (make_small(1), make_small(10), true),
            (make_small(1), make_small(10), true),
            (make_small(1), make_small(11), true),
        ]
    );
    assert_eq!(hook.entries, vec![(make_small(1), make_small(10))]);
}
#[test]
fn put_overwrites_an_existing_key_without_raising() {
    // Probed: `put(k,1), put(k,2)` ⇒ `ok, ok` — only `put_new` rejects a
    // collision (`put_common:730-733`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();
    hook.seed(make_small(1), make_small(10));

    let (ok, value) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_persistent_term(&mut hook);
        let ok = call(
            &reg,
            &mut ctx,
            "persistent_term",
            "put",
            2,
            &[make_small(1), make_small(20)],
        )
        .expect("the hook answers");
        let value = call(
            &reg,
            &mut ctx,
            "persistent_term",
            "get",
            1,
            &[make_small(1)],
        )
        .expect("the hook answers");
        (ok, value)
    };
    let flag = hook.puts[0].2;

    assert_eq!(ok, atom_term(&atoms, "ok"));
    assert_eq!(value, make_small(20));
    assert!(!flag, "put/2 carries only_if_new = false");
}
#[test]
fn erase_reports_presence_as_the_atoms_true_and_false() {
    // Probed: `persistent_term:erase(k)` ⇒ `true` when present, then `false` —
    // the return is **not** `ok` (`erl_bif_persistent.c:539,548`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();
    hook.seed(make_small(1), make_small(10));

    let (first, second) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_persistent_term(&mut hook);
        let first = call(
            &reg,
            &mut ctx,
            "persistent_term",
            "erase",
            1,
            &[make_small(1)],
        )
        .expect("the hook answers");
        let second = call(
            &reg,
            &mut ctx,
            "persistent_term",
            "erase",
            1,
            &[make_small(1)],
        )
        .expect("the hook answers");
        (first, second)
    };

    assert_eq!(first, atom_term(&atoms, "true"));
    assert_eq!(second, atom_term(&atoms, "false"));
    assert_eq!(hook.erases, vec![make_small(1), make_small(1)]);
}

#[test]
fn get_0_is_a_list_of_key_value_tuples_in_the_hooks_order() {
    // `do_get_all:830` builds `[{Key, Value}]`; the order is a hash-table walk
    // and is not observable, so the hook's order is preserved verbatim.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();
    hook.seed(make_small(2), make_small(200));
    hook.seed(make_small(1), make_small(100));

    let list = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_persistent_term(&mut hook);
        call(&reg, &mut ctx, "persistent_term", "get", 0, &[]).expect("the hook answers")
    };

    let mut scratch = Heap::new(256).expect("scratch heap");
    let expected = {
        let mut plain = BifContext::new(&mut scratch, &atoms, ProcessId::new(1, 0));
        let first = plain
            .tuple(&[make_small(2), make_small(200)])
            .expect("the tuple fits");
        let second = plain
            .tuple(&[make_small(1), make_small(100)])
            .expect("the tuple fits");
        plain.list(&[first, second]).expect("the list fits")
    };
    assert!(
        beam_term::compare::eq(list, expected, &atoms),
        "get/0 must be the hook's pairs as {{Key, Value}} tuples"
    );
}

#[test]
fn get_0_of_an_empty_store_is_the_empty_list() {
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();

    let list = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_persistent_term(&mut hook);
        call(&reg, &mut ctx, "persistent_term", "get", 0, &[]).expect("the hook answers")
    };
    assert_eq!(list, beam_term::NIL);
}

#[test]
fn info_0_is_the_count_memory_map() {
    // Probed: `persistent_term:info()` ⇒ `#{count => 28, memory => 33392}` — a
    // **flat map**, not a proplist (`do_info:977`, `MAP2(hp, am_count, …,
    // am_memory, …)`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();
    hook.seed(make_small(1), make_small(10));
    hook.seed(make_small(2), make_small(20));
    hook.memory = 33_392;

    let count_key = atom_term(&atoms, "count");
    let memory_key = atom_term(&atoms, "memory");

    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_persistent_term(&mut hook);

    let map = call(&reg, &mut ctx, "persistent_term", "info", 0, &[]).expect("the hook answers");
    // `info/0` is a map, not a proplist: the registered `erlang:is_map/1` says so.
    let is_map = call(&reg, &mut ctx, "erlang", "is_map", 1, &[map]).expect("is_map");
    assert_eq!(is_map, atom_term(&atoms, "true"), "info/0 must be a map");
    // Exactly two pairs: `count` and `memory`.
    let size = call(&reg, &mut ctx, "maps", "size", 1, &[map]).expect("maps:size");
    assert_eq!(signed_value(size), 2);

    let count = call(&reg, &mut ctx, "maps", "get", 2, &[count_key, map]).expect("maps:get");
    assert_eq!(as_u128(count), 2, "count is the store's entry count");

    let memory = call(&reg, &mut ctx, "maps", "get", 2, &[memory_key, map]).expect("maps:get");
    assert_eq!(as_u128(memory), 33_392, "memory is the hook's byte total");
}

#[test]
fn info_0_promotes_a_large_byte_total_to_a_bignum() {
    // The byte total is runtime bookkeeping with no 60-bit bound; the exact
    // integer promotion (`crate::arith::int_from_mag`) must produce a bignum the
    // oracle would (`2^62` is past the small-integer range).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = MockPersistentTerm::default();
    hook.memory = 1 << 62;

    let memory_key = atom_term(&atoms, "memory");
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_persistent_term(&mut hook);

    let map = call(&reg, &mut ctx, "persistent_term", "info", 0, &[]).expect("the hook answers");
    let memory = call(&reg, &mut ctx, "maps", "get", 2, &[memory_key, map]).expect("maps:get");
    assert_eq!(as_u128(memory), 1 << 62);
}

#[test]
fn the_registry_registers_every_persistent_term_bif() {
    let reg = default_registry();
    for (name, arity) in [
        ("get", 0u32),
        ("get", 1),
        ("get", 2),
        ("put", 2),
        ("put_new", 2),
        ("erase", 1),
        ("info", 0),
    ] {
        assert!(
            reg.contains("persistent_term", name, arity),
            "missing persistent_term:{name}/{arity}"
        );
        assert!(
            reg.lookup("persistent_term", name, arity).is_some(),
            "no impl for persistent_term:{name}/{arity}"
        );
    }

    // Exactly seven keys for the module (a duplicate would silently replace).
    let keys: Vec<_> = reg
        .names()
        .into_iter()
        .filter(|(module, _, _)| module == "persistent_term")
        .collect();
    assert_eq!(keys.len(), 7, "got {keys:?}");
}

// ---------------------------------------------------------------------------
// (c) the live oracle
// ---------------------------------------------------------------------------

/// Runs `erl -noshell -eval <statement>` and returns its stdout, or `None` (after
/// a loud note) when the oracle is unavailable — the `tests/os_bif.rs` pattern.
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

#[test]
fn live_oracle_agrees_on_the_persistent_term_semantics() {
    // One live cross-check against the system ERTS (OTP 29.1.1 / erts-17.1): the
    // `put`/`get`/`get-2` round trip, the `erase` `true`/`false` return, the
    // `put_new` same-value no-op vs different-value `badarg`, the absent-key
    // `badarg` for `get/1`, and the `get/0` list / `info/0` map shapes (with
    // `count` matching the list length). Skips loudly when the oracle is
    // unavailable.
    let Some(output) = oracle_eval(
        "P1 = persistent_term:put(pt_oracle_k, 1), \
         G1 = persistent_term:get(pt_oracle_k), \
         D1 = persistent_term:get(pt_oracle_missing, default), \
         E1 = persistent_term:erase(pt_oracle_k), \
         E2 = persistent_term:erase(pt_oracle_k), \
         N1 = persistent_term:put_new(pt_oracle_n, a), \
         N2 = persistent_term:put_new(pt_oracle_n, a), \
         X1 = (catch persistent_term:put_new(pt_oracle_n, b)), \
         X2 = (catch persistent_term:get(pt_oracle_missing)), \
         L = persistent_term:get(), \
         I = persistent_term:info(), \
         io:format(\"~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p~n\",[\
             P1 =:= ok, G1 =:= 1, D1 =:= default, E1 =:= true, E2 =:= false,\
             N1 =:= ok, N2 =:= ok,\
             element(1,X1) =:= 'EXIT', element(1,X2) =:= 'EXIT',\
             lists:member({pt_oracle_n, a}, L), is_map(I),\
             maps:get(count, I) =:= length(L)])",
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "true|true|true|true|true|true|true|true|true|true|true|true",
        "the live oracle disagrees with the pinned persistent_term semantics"
    );
}
