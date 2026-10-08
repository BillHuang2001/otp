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

//! Integration tests for `erlang:load_nif/2`, driven through
//! [`BifRegistry::call`] over a mock [`NifLoader`].
//!
//! The crate owns only the seam: `dlopen`-ing the shared object and binding its
//! NIF/driver entry points is `beam_rt`/`beam_abi`'s job. So the tests install a
//! mock loader that records `(module, path, load_info)` and returns a scripted
//! result, and pin four contracts:
//!
//! 1. a successful load returns the atom `ok` and forwards the calling module, the
//!    decoded path and the untouched `LoadInfo`;
//! 2. each [`NifLoadError`] variant renders the oracle's `{error, {Reason, Text}}`
//!    term (the `Text` an Erlang string) as a **returned** value — the oracle
//!    returns it, it does not raise;
//! 3. with no hook, or an unknown calling module, the BIF raises `unsupported_bif`
//!    (never a fabricated `ok`, never a silently dropped load);
//! 4. a non-string `Path` is `badarg` *before* the seam is consulted;
//! 5. the failure term's `error` tag and reason atoms are **interned on demand**
//!    (the oracle's `mkatom`/`atom_put`, `erl_nif.c:4479-4482`): a reason atom
//!    absent from the context table is created through the [`AtomInterner`] seam
//!    rather than reported as a `TermNotFound`.
//!
//! Structural comparisons use `beam_term::compare`-free readers
//! (`beam_codec::term::tuple_*`/`cons_*`), the same public API the sibling suites
//! use.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::{default_registry, AtomInterner, BifContext, NifLoadError, NifLoader};
use beam_codec::term::{cons_head, cons_tail, tuple_arity, tuple_element};
use beam_common::{BeamError, ExceptionClass, ExitReason};
use beam_process::ProcessId;
use beam_term::{atom_val, is_list, make_small, signed_value, Atom, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "error",
        "load_failed",
        "bad_lib",
        "reload",
        "load",
        "upgrade",
        "old_code",
    ]);
    atoms
}

/// A heap that outlives the terms the tests inspect (the results point into it).
fn leaked_heap() -> &'static mut Heap {
    Box::leak(Box::new(Heap::new(4096).expect("test heap")))
}

/// A `NifLoader` test double: it records every call and replays a scripted result
/// list in order (the last entry repeats once the script is exhausted; `Ok(())` when
/// the script is empty).
struct MockNifLoader {
    results: Vec<std::result::Result<(), NifLoadError>>,
    calls: Vec<(String, String, Eterm)>,
}

impl MockNifLoader {
    fn new(results: Vec<std::result::Result<(), NifLoadError>>) -> MockNifLoader {
        MockNifLoader {
            results,
            calls: Vec::new(),
        }
    }
}

impl NifLoader for MockNifLoader {
    fn load_nif(
        &mut self,
        module: &str,
        path: &str,
        load_info: Eterm,
    ) -> std::result::Result<(), NifLoadError> {
        self.calls
            .push((module.to_string(), path.to_string(), load_info));
        if self.results.is_empty() {
            return Ok(());
        }
        let index = (self.calls.len() - 1).min(self.results.len() - 1);
        self.results[index].clone()
    }
}

/// The proper list of the bytes `bytes`, built on `ctx`'s heap.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let elems: Vec<Eterm> = bytes
        .iter()
        .map(|b| ctx.int(i64::from(*b)).expect("a byte"))
        .collect();
    ctx.list(&elems).expect("the list fits")
}

/// A proper list of byte-sized small ints read back as a `String`.
fn string_of(term: Eterm) -> String {
    let mut out = String::new();
    let mut cur = term;
    while is_list(cur) {
        let head = cons_head(cur).expect("a cons head");
        out.push(char::from(
            u8::try_from(signed_value(head)).expect("a byte"),
        ));
        cur = cons_tail(cur).expect("a cons tail");
    }
    out
}

/// An `unsupported_bif` error raise.
fn assert_unsupported(result: beam_common::Result<Eterm>) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, ExceptionClass::Error);
            assert_eq!(raise.reason, ExitReason::atom("unsupported_bif"));
        }
        other => panic!("expected an unsupported_bif raise, got {other:?}"),
    }
}

/// Assert that `term` is the oracle's `{error, {Reason, Text}}` term, with
/// `reason_atom` the reason atom and `text` the char-list message.
fn assert_error_term(atoms: &AtomTable, term: Eterm, reason_atom: &str, text: &str) {
    assert_eq!(tuple_arity(term).expect("a tuple"), 2);
    let tag = tuple_element(term, 0).expect("element 0");
    assert_eq!(atoms.name(atom_val(tag)), Some("error"));
    let inner = tuple_element(term, 1).expect("element 1");
    assert_eq!(tuple_arity(inner).expect("a tuple"), 2);
    let reason = tuple_element(inner, 0).expect("the reason");
    assert_eq!(atoms.name(atom_val(reason)), Some(reason_atom));
    let rendered = tuple_element(inner, 1).expect("the text");
    assert_eq!(string_of(rendered), text);
}

#[test]
fn success_returns_ok_and_forwards_module_path_and_load_info() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockNifLoader::new(vec![Ok(())]);
    let info = make_small(7);

    let result = {
        let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0))
            .with_nif_loader(&mut hook)
            .with_current_module("nif_mod");
        let path = byte_list(&mut ctx, b"nif.so");
        reg.call("erlang", "load_nif", 2, &mut ctx, &[path, info])
            .expect("a successful load returns ok")
    };

    assert_eq!(atoms.name(atom_val(result)), Some("ok"));
    assert_eq!(hook.calls.len(), 1);
    assert_eq!(hook.calls[0].0, "nif_mod");
    assert_eq!(hook.calls[0].1, "nif.so");
    assert_eq!(hook.calls[0].2, info);
}

#[test]
fn a_binary_path_is_accepted() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockNifLoader::new(vec![Ok(())]);

    let result = {
        let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0))
            .with_nif_loader(&mut hook)
            .with_current_module("m");
        let path = beam_codec::term::make_binary(ctx.heap(), b"priv/nif.so").expect("fits");
        reg.call("erlang", "load_nif", 2, &mut ctx, &[path, NIL])
            .expect("a binary Path loads")
    };

    assert_eq!(atoms.name(atom_val(result)), Some("ok"));
    assert_eq!(hook.calls[0].1, "priv/nif.so");
}

#[test]
fn each_variant_renders_the_oracle_error_term() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();

    let cases: [(NifLoadError, &str, &str); 6] = [
        (
            NifLoadError::LoadFailed("cannot open nif.so".to_string()),
            "load_failed",
            "cannot open nif.so",
        ),
        (
            NifLoadError::BadLib("not a nif library".to_string()),
            "bad_lib",
            "not a nif library",
        ),
        (
            NifLoadError::Reload("library already loaded".to_string()),
            "reload",
            "library already loaded",
        ),
        (
            NifLoadError::Load("bad area".to_string()),
            "load",
            "bad area",
        ),
        (
            NifLoadError::Upgrade("upgrade refused".to_string()),
            "upgrade",
            "upgrade refused",
        ),
        (
            NifLoadError::OldCode("called from old code".to_string()),
            "old_code",
            "called from old code",
        ),
    ];

    for (error, reason_atom, text) in cases {
        let mut hook = MockNifLoader::new(vec![Err(error)]);

        let term = {
            let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0))
                .with_nif_loader(&mut hook)
                .with_current_module("m");
            let path = byte_list(&mut ctx, b"nif.so");
            reg.call("erlang", "load_nif", 2, &mut ctx, &[path, NIL])
                .expect("a load failure is returned, not raised")
        };

        // `{error, {Reason, Text}}`.
        assert_error_term(&atoms, term, reason_atom, text);
    }
}

#[test]
fn the_hook_result_can_change_between_calls() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockNifLoader::new(vec![Ok(()), Err(NifLoadError::Reload("again".to_string()))]);

    let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0))
        .with_nif_loader(&mut hook)
        .with_current_module("m");

    let first_path = byte_list(&mut ctx, b"nif.so");
    let first = reg
        .call("erlang", "load_nif", 2, &mut ctx, &[first_path, NIL])
        .expect("the first load succeeds");
    assert_eq!(atoms.name(atom_val(first)), Some("ok"));

    let second_path = byte_list(&mut ctx, b"nif.so");
    let second = reg
        .call("erlang", "load_nif", 2, &mut ctx, &[second_path, NIL])
        .expect("a load failure is returned, not raised");
    assert_error_term(&atoms, second, "reload", "again");
    assert_eq!(hook.calls.len(), 2);
}

#[test]
fn without_a_module_or_a_hook_the_bif_is_unsupported() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();

    // A hook is installed, but the calling module is unknown: never guessed.
    let mut hook = MockNifLoader::new(vec![Ok(())]);
    {
        let mut ctx =
            BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0)).with_nif_loader(&mut hook);
        let path = byte_list(&mut ctx, b"nif.so");
        assert_unsupported(reg.call("erlang", "load_nif", 2, &mut ctx, &[path, NIL]));
    }
    assert!(
        hook.calls.is_empty(),
        "the hook is never consulted without a module"
    );

    // The module is known, but no hook is installed.
    {
        let mut ctx =
            BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0)).with_current_module("m");
        let path = byte_list(&mut ctx, b"nif.so");
        assert_unsupported(reg.call("erlang", "load_nif", 2, &mut ctx, &[path, NIL]));
    }
}

#[test]
fn a_non_string_path_is_badarg() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockNifLoader::new(vec![Ok(())]);
    let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0))
        .with_nif_loader(&mut hook)
        .with_current_module("m");

    let int = ctx.int(1).expect("fits");
    assert!(matches!(
        reg.call("erlang", "load_nif", 2, &mut ctx, &[int, NIL]),
        Err(BeamError::BadArg)
    ));

    let tuple = ctx.tuple(&[int]).expect("fits");
    assert!(matches!(
        reg.call("erlang", "load_nif", 2, &mut ctx, &[tuple, NIL]),
        Err(BeamError::BadArg)
    ));

    // An element outside the byte range is `badarg` too.
    let bad = byte_list_raw(&mut ctx, &[256]);
    assert!(matches!(
        reg.call("erlang", "load_nif", 2, &mut ctx, &[bad, NIL]),
        Err(BeamError::BadArg)
    ));

    assert!(hook.calls.is_empty(), "a bad Path never reaches the hook");
}

/// A list of the given small ints (`byte_list` refuses anything outside `0..=255`).
fn byte_list_raw(ctx: &mut BifContext<'_>, ints: &[i64]) -> Eterm {
    let elems: Vec<Eterm> = ints
        .iter()
        .map(|v| ctx.int(*v).expect("a small int"))
        .collect();
    ctx.list(&elems).expect("the list fits")
}

/// An [`AtomInterner`] over a **clone** of the table the context borrows.
///
/// The clone mirrors the context's indices for every already-interned name, so a
/// name the hook creates gets exactly the index the context's own table would have
/// assigned — the index coupling [`BifContext::intern_atom`] relies on.
struct Interner {
    table: Rc<RefCell<AtomTable>>,
}

impl AtomInterner for Interner {
    fn intern(&mut self, name: &str) -> Atom {
        self.table.borrow_mut().intern(name)
    }
}

#[test]
fn the_failure_atoms_are_interned_on_demand() {
    let reg = default_registry();
    let heap = leaked_heap();

    // Only the well-known atoms: `error` and the reason atoms are **deliberately
    // absent**, so the failure path must create them through the `AtomInterner`
    // seam — the oracle's `mkatom`/`atom_put` (`erl_nif.c:4479-4482`).
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok"]);
    let shared = Rc::new(RefCell::new(atoms.clone()));
    let mut hook = MockNifLoader::new(vec![Err(NifLoadError::LoadFailed(
        "cannot open nif.so".to_string(),
    ))]);

    let term = {
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0))
            .with_nif_loader(&mut hook)
            .with_atom_interner(&mut interner)
            .with_current_module("m");
        let path = byte_list(&mut ctx, b"nif.so");
        reg.call("erlang", "load_nif", 2, &mut ctx, &[path, NIL])
            .expect("a load failure is returned, not raised")
    };

    // The `error` tag and the reason atom were created on demand rather than
    // surfacing as a `TermNotFound`.
    let table = shared.borrow();
    assert!(
        table.lookup("error").is_some(),
        "the `error` tag atom was interned on demand"
    );
    assert!(
        table.lookup("load_failed").is_some(),
        "the reason atom was interned on demand"
    );
    assert_error_term(&table, term, "load_failed", "cannot open nif.so");
}

#[test]
fn the_registry_registers_load_nif_2() {
    let reg = default_registry();
    assert!(reg.contains("erlang", "load_nif", 2));
    assert!(reg.lookup("erlang", "load_nif", 2).is_some());
}
