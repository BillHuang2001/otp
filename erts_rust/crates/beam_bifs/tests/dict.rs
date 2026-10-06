//! Integration tests for the process-dictionary BIFs (`erlang:put/2`, `get/1`,
//! `erase/1`, `get_keys/0`, `get_keys/1`), driven through [`BifRegistry::call`]
//! over a mock [`DictHook`].
//!
//! The crate owns only the seam: the per-process store (`beam_process::Dict`) is a
//! GC sub-rootset of the calling process, so `beam_rt` supplies the concrete hook.
//! The tests install a mock hook over an in-memory pair list and pin the oracle
//! semantics (erts-17.1 `erl_bif_proc.c`/`erl_process_dict.c`):
//!
//! 1. `put/2` returns the **previous** value or `undefined`;
//! 2. `get/1` returns the stored value or `undefined`;
//! 3. `erase/1` returns the removed value or `undefined`;
//! 4. `get_keys/0` lists every key and `get_keys/1` only the keys whose value
//!    raw-equals the argument (empty list when none match);
//! 5. with **no** hook every one of those BIFs raises `unsupported_bif`.
//!
//! Structural comparisons use `beam_term::compare::eq`; the `undefined` atom is
//! resolved by looking the name up in the atom table.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, DictHook};
use beam_common::{BeamError, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, is_list, make_small, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "undefined", "alpha", "beta"]);
    atoms
}

/// A `DictHook` test double over an ordered pair list, matching
/// `beam_process::Dict`'s raw `Eterm`-equality semantics. It records the calls so
/// the tests can assert the BIF really routed through the seam.
#[derive(Default)]
struct MockDict {
    entries: Vec<(Eterm, Eterm)>,
    puts: Vec<(Eterm, Eterm)>,
    erases: Vec<Eterm>,
}

impl MockDict {
    /// Seeds one pair directly (bypassing the BIF), for the tests that need a
    /// pre-populated store.
    fn seed(&mut self, key: i64, value: i64) {
        self.entries.push((make_small(key), make_small(value)));
    }
}

impl DictHook for MockDict {
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
            assert_eq!(raise.reason, ExitReason::atom("unsupported_bif"));
        }
        other => panic!("expected an unsupported_bif raise, got {other:?}"),
    }
}

#[test]
fn put_returns_the_previous_value_or_undefined() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut hook = MockDict::default();
    let (first, replaced) = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);

        // First put of a fresh key: absent, so `undefined`.
        let (k1, v1) = (make_small(1), make_small(10));
        let first = reg
            .call("erlang", "put", 2, &mut ctx, &[k1, v1])
            .expect("the hook stores");

        // Overwriting returns the prior value.
        let (k1, v2) = (make_small(1), make_small(11));
        let replaced = reg
            .call("erlang", "put", 2, &mut ctx, &[k1, v2])
            .expect("the hook stores");
        (first, replaced)
    };

    assert!(atoms.name(atom_val(first)) == Some("undefined"));
    assert_eq!(replaced, make_small(10));
    assert_eq!(hook.puts, vec![(make_small(1), make_small(10)), (make_small(1), make_small(11))]);
}

#[test]
fn get_returns_the_value_or_undefined() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut hook = MockDict::default();
    hook.seed(1, 42);

    let (found, missing) = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);
        let found = reg
            .call("erlang", "get", 1, &mut ctx, &[make_small(1)])
            .expect("the hook answers");
        let missing = reg
            .call("erlang", "get", 1, &mut ctx, &[make_small(9)])
            .expect("the hook answers");
        (found, missing)
    };

    assert_eq!(found, make_small(42));
    assert!(is_undefined(&atoms, missing));
}

#[test]
fn erase_returns_the_previous_value_or_undefined() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut hook = MockDict::default();
    hook.seed(1, 7);

    let (removed, again) = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);
        let removed = reg
            .call("erlang", "erase", 1, &mut ctx, &[make_small(1)])
            .expect("the hook erases");
        let again = reg
            .call("erlang", "erase", 1, &mut ctx, &[make_small(1)])
            .expect("the hook erases");
        (removed, again)
    };

    assert_eq!(removed, make_small(7));
    assert!(is_undefined(&atoms, again));
    assert_eq!(hook.erases, vec![make_small(1), make_small(1)]);
}

#[test]
fn get_keys_0_lists_every_key_in_store_order() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut hook = MockDict::default();
    hook.seed(1, 10);
    hook.seed(2, 20);

    let keys = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);
        reg.call("erlang", "get_keys", 0, &mut ctx, &[])
            .expect("the hook answers")
    };

    assert!(is_list(keys));
    // `[1, 2]` built on a scratch heap: structural equality via `compare::eq`.
    let mut scratch = Heap::new(64).expect("scratch heap");
    let expected = {
        let mut plain = BifContext::new(&mut scratch, &atoms, ProcessId::new(1, 0));
        plain
            .list(&[make_small(1), make_small(2)])
            .expect("fits")
    };
    assert!(beam_term::compare::eq(keys, expected, &atoms));
}

#[test]
fn get_keys_0_of_an_empty_dictionary_is_the_empty_list() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut hook = MockDict::default();

    let keys = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);
        reg.call("erlang", "get_keys", 0, &mut ctx, &[])
            .expect("the hook answers")
    };
    assert_eq!(keys, NIL);
}

#[test]
fn get_keys_1_selects_keys_by_value() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut hook = MockDict::default();
    hook.seed(1, 100);
    hook.seed(2, 100);
    hook.seed(3, 200);

    let (matching, none) = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);
        let matching = reg
            .call("erlang", "get_keys", 1, &mut ctx, &[make_small(100)])
            .expect("the hook answers");
        // No key holds this value: the empty list.
        let none = reg
            .call("erlang", "get_keys", 1, &mut ctx, &[make_small(7)])
            .expect("the hook answers");
        (matching, none)
    };

    let mut scratch = Heap::new(64).expect("scratch heap");
    let expected = {
        let mut plain = BifContext::new(&mut scratch, &atoms, ProcessId::new(1, 0));
        plain
            .list(&[make_small(1), make_small(2)])
            .expect("fits")
    };
    assert!(beam_term::compare::eq(matching, expected, &atoms));
    assert_eq!(none, NIL);
}

#[test]
fn every_dict_bif_is_unsupported_without_a_hook() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    assert_unsupported(reg.call(
        "erlang",
        "put",
        2,
        &mut ctx,
        &[make_small(1), make_small(2)],
    ));
    assert_unsupported(reg.call("erlang", "get", 1, &mut ctx, &[make_small(1)]));
    assert_unsupported(reg.call("erlang", "erase", 1, &mut ctx, &[make_small(1)]));
    assert_unsupported(reg.call("erlang", "get_keys", 0, &mut ctx, &[]));
    assert_unsupported(reg.call("erlang", "get_keys", 1, &mut ctx, &[make_small(2)]));
}

#[test]
fn the_registry_registers_every_dict_bif() {
    let reg = default_registry();
    for (name, arity) in [
        ("put", 2),
        ("get", 1),
        ("erase", 1),
        ("get_keys", 0),
        ("get_keys", 1),
    ] {
        assert!(
            reg.contains("erlang", name, arity),
            "missing erlang:{name}/{arity}"
        );
        assert!(
            reg.lookup("erlang", name, arity).is_some(),
            "no impl for erlang:{name}/{arity}"
        );
    }

    // Every dictionary key is registered exactly once (a duplicate would silently
    // replace). `erase/2` does not exist in OTP and must not be present.
    let dict_keys: Vec<_> = reg
        .names()
        .into_iter()
        .filter(|(m, f, _)| {
            m == "erlang" && matches!(f.as_str(), "put" | "get" | "erase" | "get_keys")
        })
        .collect();
    assert_eq!(dict_keys.len(), 5, "got {dict_keys:?}");
    assert!(!BifRegistry::with_core_bifs().contains("erlang", "erase", 2));
}
