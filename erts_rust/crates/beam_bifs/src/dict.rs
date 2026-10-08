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

//! Process-dictionary BIFs for `erlang`: `put/2`, `get/0`, `get/1`, `erase/0`,
//! `erase/1`, `get_keys/0` and `get_keys/1`.
//!
//! Ground truth (read-only): `bif erlang:put/2`, `erlang:get/0`, `erlang:get/1`,
//! `erlang:erase/0`, `erlang:erase/1`, `erlang:get_keys/0` and `erlang:get_keys/1`
//! in `../../erts/emulator/beam/bif.tab` (lines 63–64, 75–77, 134, 653) and
//! `../../erts/emulator/beam/erl_process_dict.c` (`get_0:315`, `get_1:324`,
//! `get_keys_0:333`, `get_keys_1:343`, `put_2:353`, `erase_0:363`, `erase_1:373`).
//! There is **no** `erlang:erase/2` in OTP — the dictionary offers only `erase/1`
//! (one key) and `erase/0` (the whole store).
//!
//! # `get/0` and `erase/0` render the store's own order
//!
//! The C walks an internal hash table (`pd_hash_get_all`,
//! `erl_process_dict.c:579`), prepending each pair, so the returned list's order
//! is an artefact of the hash layout — **implementation-defined**, not insertion
//! order. Oracle (erts-17.1): a fresh process gives `[]`, `put(a,1), put(b,2)`
//! gives `[{a,1},{b,2}]`, but twelve keys come back in descending-key order.
//! The [`DictHook`](crate::context::DictHook) therefore hands the pairs over in
//! *its* order and this layer renders that order **verbatim**, never sorting or
//! otherwise reordering (ordering is the store's concern).
//!
//! # The store is a runtime seam
//!
//! The dictionary is a per-process GC sub-rootset owned by
//! [`beam_process::Dict`]; a BIF cannot reach into it directly. Every operation
//! therefore routes through the [`DictHook`](crate::context::DictHook) seam
//! supplied by `beam_rt`. With no hook installed these BIFs report
//! `unsupported_bif` rather than inventing a value (workspace `CONTEXT.md` §3).
//!
//! # `undefined`, not the sentinel
//!
//! The C signals "no such key" with the sentinel `THE_NON_VALUE`/`am_undefined`
//! (`erl_process_dict.c`). The [`DictHook`](crate::context::DictHook) surfaces that
//! as `None`, which the BIF layer maps to the atom `undefined` — `put/2` and
//! `erase/1` return the *previous* value or `undefined`, and `get/1` returns the
//! stored value or `undefined`.

use beam_common::Result;
use beam_term::Eterm;

use crate::context::BifContext;
use crate::registry::BifRegistry;

/// The atom `undefined` — the value returned for an absent key.
fn undefined(ctx: &BifContext<'_>) -> Result<Eterm> {
    ctx.atom("undefined")
}

/// `erlang:put(Key, Value)` — stores `value` under `key` and returns the
/// **previous** value, or `undefined` when the key was absent.
fn put(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let previous = ctx.dict_put(args[0], args[1])?;
    match previous {
        Some(value) => Ok(value),
        None => undefined(ctx),
    }
}

/// `erlang:get(Key)` — the value stored under `key`, or `undefined` when absent.
fn get(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match ctx.dict_get(args[0])? {
        Some(value) => Ok(value),
        None => undefined(ctx),
    }
}

/// `erlang:erase(Key)` — removes `key` and returns the value that was stored
/// under it, or `undefined` when the key was absent.
fn erase(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match ctx.dict_erase(args[0])? {
        Some(value) => Ok(value),
        None => undefined(ctx),
    }
}

/// Renders `{Key, Value}` pairs as a list of 2-tuples, preserving the store's
/// order verbatim (see the module docs).
fn pairs_to_list(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let mut tuples = Vec::with_capacity(pairs.len());
    for &(key, value) in pairs {
        tuples.push(ctx.tuple(&[key, value])?);
    }
    ctx.list(&tuples)
}

/// `erlang:get/0` — the whole process dictionary as a list of `{Key, Value}`
/// 2-tuples, in the store's own order (`get_0`, `erl_process_dict.c:315`). A
/// fresh process gives `[]`.
fn get_all(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let pairs = ctx.dict_get_all()?;
    pairs_to_list(ctx, &pairs)
}

/// `erlang:erase/0` — empties the process dictionary and returns the pairs it
/// held, in the store's own order (`erase_0`, `erl_process_dict.c:363`). An
/// empty dictionary gives `[]` and leaves it empty.
fn erase_all(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let pairs = ctx.dict_erase_all()?;
    pairs_to_list(ctx, &pairs)
}

/// `erlang:get_keys/0` — the list of keys in the process dictionary.
fn get_keys0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let keys = ctx.dict_get_keys()?;
    ctx.list(&keys)
}

/// `erlang:get_keys(Value)` — the keys whose value raw-equals `value` (the
/// [`beam_process::Dict`] equality contract).
fn get_keys1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let keys = ctx.dict_get_keys_for_value(args[0])?;
    ctx.list(&keys)
}

/// Registers `erlang:put/2`, `erlang:get/0`, `erlang:get/1`, `erlang:erase/0`,
/// `erlang:erase/1`, `erlang:get_keys/0` and `erlang:get_keys/1`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "put", 2, put);
    reg.register("erlang", "get", 0, get_all);
    reg.register("erlang", "get", 1, get);
    reg.register("erlang", "erase", 0, erase_all);
    reg.register("erlang", "erase", 1, erase);
    reg.register("erlang", "get_keys", 0, get_keys0);
    reg.register("erlang", "get_keys", 1, get_keys1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::DictHook;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{atom_val, is_list, make_small, AtomTable, NIL};

    /// A `DictHook` that records the calls and stores pairs by raw `Eterm`
    /// equality, mirroring [`beam_process::Dict`]. Its `get_all`/`erase_all`
    /// return the pairs in stored order (the C's hash order is the store's
    /// concern, see the module docs).
    #[derive(Default)]
    struct StubDict {
        entries: Vec<(Eterm, Eterm)>,
        puts: Vec<(Eterm, Eterm)>,
        erases: Vec<Eterm>,
        erase_alls: usize,
    }

    impl DictHook for StubDict {
        fn put(&mut self, key: Eterm, value: Eterm) -> Result<Option<Eterm>> {
            self.puts.push((key, value));
            match self.entries.iter().position(|&(stored, _)| stored == key) {
                Some(index) => Ok(Some(core::mem::replace(&mut self.entries[index].1, value))),
                None => {
                    self.entries.push((key, value));
                    Ok(None)
                }
            }
        }

        fn get(&mut self, key: Eterm) -> Result<Option<Eterm>> {
            Ok(self
                .entries
                .iter()
                .find(|&&(stored, _)| stored == key)
                .map(|&(_, value)| value))
        }

        fn erase(&mut self, key: Eterm) -> Result<Option<Eterm>> {
            self.erases.push(key);
            match self.entries.iter().position(|&(stored, _)| stored == key) {
                Some(index) => Ok(Some(self.entries.remove(index).1)),
                None => Ok(None),
            }
        }

        fn get_keys(&mut self) -> Result<Vec<Eterm>> {
            Ok(self.entries.iter().map(|&(key, _)| key).collect())
        }

        fn get_keys_for_value(&mut self, value: Eterm) -> Result<Vec<Eterm>> {
            Ok(self
                .entries
                .iter()
                .filter(|&&(_, stored)| stored == value)
                .map(|&(key, _)| key)
                .collect())
        }

        fn get_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
            Ok(self.entries.clone())
        }

        fn erase_all(&mut self) -> Result<Vec<(Eterm, Eterm)>> {
            self.erase_alls += 1;
            Ok(core::mem::take(&mut self.entries))
        }
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "undefined"]);
        atoms
    }

    /// Whether `term` is the atom `undefined`.
    fn is_undefined(atoms: &AtomTable, term: Eterm) -> bool {
        atoms.name(atom_val(term)) == Some("undefined")
    }

    /// An `unsupported_bif` error raise.
    fn assert_unsupported(result: Result<Eterm>) {
        match result {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected an unsupported_bif raise, got {other:?}"),
        }
    }

    #[test]
    fn put_returns_the_previous_value_or_undefined() {
        let mut heap = Heap::new(32).expect("test heap");
        let atoms = atoms();
        let mut hook = StubDict::default();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);

        // First put of a fresh key: absent, so `undefined`.
        let first = put(&mut ctx, &[make_small(1), make_small(10)]).expect("the hook stores");
        assert!(is_undefined(ctx.atoms(), first));

        // Overwriting returns the prior value.
        let replaced = put(&mut ctx, &[make_small(1), make_small(11)]).expect("the hook stores");
        assert_eq!(replaced, make_small(10));

        assert_eq!(
            hook.puts,
            vec![(make_small(1), make_small(10)), (make_small(1), make_small(11))]
        );
    }

    #[test]
    fn get_returns_the_value_or_undefined() {
        let mut heap = Heap::new(32).expect("test heap");
        let atoms = atoms();
        let mut hook = StubDict::default();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);

        put(&mut ctx, &[make_small(1), make_small(42)]).expect("the hook stores");

        let found = get(&mut ctx, &[make_small(1)]).expect("the hook answers");
        assert_eq!(found, make_small(42));

        let missing = get(&mut ctx, &[make_small(9)]).expect("the hook answers");
        assert!(is_undefined(ctx.atoms(), missing));
    }

    #[test]
    fn erase_returns_the_previous_value_or_undefined() {
        let mut heap = Heap::new(32).expect("test heap");
        let atoms = atoms();
        let mut hook = StubDict::default();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);

        put(&mut ctx, &[make_small(1), make_small(7)]).expect("the hook stores");

        let removed = erase(&mut ctx, &[make_small(1)]).expect("the hook erases");
        assert_eq!(removed, make_small(7));
        // The value is really gone.
        assert!(ctx.dict_get(make_small(1)).expect("the hook answers").is_none());

        let again = erase(&mut ctx, &[make_small(1)]).expect("the hook erases");
        assert!(is_undefined(ctx.atoms(), again));
    }

    #[test]
    fn get_keys_0_builds_the_key_list() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let mut hook = StubDict::default();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);

        // An empty dictionary is the empty list `[]` (i.e. `NIL`).
        assert_eq!(get_keys0(&mut ctx, &[]).expect("the hook answers"), NIL);

        put(&mut ctx, &[make_small(1), make_small(10)]).expect("the hook stores");
        put(&mut ctx, &[make_small(2), make_small(20)]).expect("the hook stores");

        let listed = get_keys0(&mut ctx, &[]).expect("the hook answers");
        assert!(is_list(listed));
        let expected = ctx.list(&[make_small(1), make_small(2)]).expect("fits");
        assert!(beam_term::compare::eq(listed, expected, ctx.atoms()));
    }

    #[test]
    fn get_keys_1_selects_by_value() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let mut hook = StubDict::default();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);

        put(&mut ctx, &[make_small(1), make_small(100)]).expect("the hook stores");
        put(&mut ctx, &[make_small(2), make_small(100)]).expect("the hook stores");
        put(&mut ctx, &[make_small(3), make_small(200)]).expect("the hook stores");

        let matching = get_keys1(&mut ctx, &[make_small(100)]).expect("the hook answers");
        let expected = ctx.list(&[make_small(1), make_small(2)]).expect("fits");
        assert!(beam_term::compare::eq(matching, expected, ctx.atoms()));

        // No key holds the value: the empty list.
        let none = get_keys1(&mut ctx, &[make_small(7)]).expect("the hook answers");
        assert_eq!(none, NIL);
    }

    #[test]
    fn get_0_renders_the_whole_dictionary() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let mut hook = StubDict::default();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);

        // A fresh process gives `[]`.
        assert_eq!(get_all(&mut ctx, &[]).expect("the hook answers"), NIL);

        put(&mut ctx, &[make_small(1), make_small(10)]).expect("the hook stores");
        put(&mut ctx, &[make_small(2), make_small(20)]).expect("the hook stores");

        let listed = get_all(&mut ctx, &[]).expect("the hook answers");
        assert!(is_list(listed));
        let t1 = ctx.tuple(&[make_small(1), make_small(10)]).expect("fits");
        let t2 = ctx.tuple(&[make_small(2), make_small(20)]).expect("fits");
        let expected = ctx.list(&[t1, t2]).expect("fits");
        assert!(beam_term::compare::eq(listed, expected, ctx.atoms()));
        // `get/0` does not erase: the dictionary is still there.
        assert!(ctx.dict_get(make_small(1)).expect("the hook answers").is_some());
    }

    #[test]
    fn erase_0_empties_the_dictionary_and_returns_the_pairs() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let mut hook = StubDict::default();

        {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_dict_hook(&mut hook);

            // An empty dictionary: `[]`, and still empty.
            assert_eq!(erase_all(&mut ctx, &[]).expect("the hook answers"), NIL);

            put(&mut ctx, &[make_small(1), make_small(10)]).expect("the hook stores");
            put(&mut ctx, &[make_small(2), make_small(20)]).expect("the hook stores");

            let erased = erase_all(&mut ctx, &[]).expect("the hook answers");
            let t1 = ctx.tuple(&[make_small(1), make_small(10)]).expect("fits");
            let t2 = ctx.tuple(&[make_small(2), make_small(20)]).expect("fits");
            let expected = ctx.list(&[t1, t2]).expect("fits");
            assert!(is_list(erased));
            assert!(beam_term::compare::eq(erased, expected, ctx.atoms()));
            // The store really is empty now.
            assert_eq!(get_keys0(&mut ctx, &[]).expect("the hook answers"), NIL);
        }

        assert_eq!(hook.erase_alls, 2);
    }

    #[test]
    fn every_dict_bif_is_unsupported_without_a_hook() {
        let mut heap = Heap::new(32).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        assert_unsupported(put(&mut ctx, &[make_small(1), make_small(2)]));
        assert_unsupported(get_all(&mut ctx, &[]));
        assert_unsupported(get(&mut ctx, &[make_small(1)]));
        assert_unsupported(erase_all(&mut ctx, &[]));
        assert_unsupported(erase(&mut ctx, &[make_small(1)]));
        assert_unsupported(get_keys0(&mut ctx, &[]));
        assert_unsupported(get_keys1(&mut ctx, &[make_small(2)]));
    }

    #[test]
    fn register_installs_the_seven_dict_bifs() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert!(reg.contains("erlang", "put", 2));
        assert!(reg.contains("erlang", "get", 0));
        assert!(reg.contains("erlang", "get", 1));
        assert!(reg.contains("erlang", "erase", 0));
        assert!(reg.contains("erlang", "erase", 1));
        assert!(reg.contains("erlang", "get_keys", 0));
        assert!(reg.contains("erlang", "get_keys", 1));
        assert_eq!(reg.len(), 7);
    }
}
