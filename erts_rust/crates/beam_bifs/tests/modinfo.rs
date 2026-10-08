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

//! Integration tests for `erlang:get_module_info/1,2`, driven through
//! [`BifRegistry::call`] over a mock [`ModuleInfo`].
//!
//! The crate owns only the seam: the introspection data (`exports`,
//! `attributes`, `compile`, `md5`, …) lives with the *loaded*
//! `beam_loader::Module` owned by the runtime's code server, so `beam_rt`
//! supplies the concrete hook. The tests install a mock hook that knows exactly
//! one module and renders a scripted item set, and pin the oracle semantics
//! (erts-17.1 `erl_bif_info.c`, `get_module_info:5927-5953` / `module_info_0:5955-5993`
//! / `module_info_1:5995-6017`):
//!
//! 1. `get_module_info/2` returns the item's value **directly** — the oracle's
//!    `get_module_info(lists, module)` is the atom `lists`, *not* `{module, lists}`;
//! 2. the second argument must be a single atom from the C's nine-item dispatch
//!    chain; an unknown atom, or a term that is not an atom (a list, integer, …), is
//!    `badarg` — validated *before* the seam is consulted;
//! 3. a non-atom or unknown *module* is `badarg` (the seam signals "unknown" with
//!    `Ok(None)`);
//! 4. with **no** hook installed both arities are the typed `unsupported_bif`;
//! 5. `get_module_info/1` is the fixed five-item list
//!    `[{module,…},{exports,…},{attributes,…},{compile,…},{md5,…}]` (the oracle's
//!    order), an unknown module is `badarg`, and a module the seam can name but not
//!    fully render is a typed `unsupported_bif` (never a partial list).
//!
//! Structural comparisons use `beam_term::compare`-free readers
//! (`beam_codec::term::{tuple_*, cons_*, bitstring_bytes}`), the same public API the
//! sibling suites use. The oracle probes in the comments were produced with the system
//! `erl` (OTP 29.1.1 / erts-17.1); one test re-runs them live and **skips loudly** when
//! `erl` is not on `PATH`.

use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, ModuleInfo};
use beam_codec::term::{bitstring_bytes, cons_head, cons_tail, tuple_arity, tuple_element};
use beam_common::{BeamError, ExceptionClass, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, make_atom, make_small, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "module",
        "md5",
        "exports",
        "functions",
        "nifs",
        "attributes",
        "compile",
        "native",
        "native_addresses",
        "no_such_item",
        "no_such_mod",
        "lists",
        "append",
        "reverse",
        "vsn",
        "version",
    ]);
    atoms
}

/// A heap that outlives the terms the tests inspect (the results point into it).
fn leaked_heap() -> &'static mut Heap {
    Box::leak(Box::new(Heap::new(4096).expect("test heap")))
}

/// A proper list of `elems` built on `heap`.
fn list_of(heap: &mut Heap, elems: &[Eterm]) -> Result<Eterm> {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc)?;
    }
    Ok(acc)
}

/// The name of the atom `term`, or `None` when it is not an atom / not interned.
fn name_of(atoms: &AtomTable, term: Eterm) -> Option<&str> {
    if !beam_term::is_atom(term) {
        return None;
    }
    atoms.name(atom_val(term))
}

/// A `ModuleInfo` test double: it knows one module and renders a scripted set of
/// items for it, answering `Ok(None)` (the seam's "unknown module/item" signal) for
/// everything else. It records every call so a test can assert the BIF really routed
/// through the seam — or did not, for the pre-seam `badarg` paths.
struct MockModuleInfo {
    /// The single module this seam knows.
    module: String,
    /// The items it can render for `self.module` (a subset of the nine).
    items: Vec<&'static str>,
    calls: Vec<(String, String)>,
}

impl MockModuleInfo {
    fn new(module: &str, items: &[&'static str]) -> MockModuleInfo {
        MockModuleInfo {
            module: module.to_string(),
            items: items.to_vec(),
            calls: Vec::new(),
        }
    }

    /// The full nine-item capability (a module the runtime fully knows).
    fn full(module: &str) -> MockModuleInfo {
        MockModuleInfo::new(
            module,
            &[
                "module",
                "md5",
                "exports",
                "functions",
                "nifs",
                "attributes",
                "compile",
                "native",
                "native_addresses",
            ],
        )
    }
}

/// The atom term for a seeded name (the mock's building block).
fn atom_term(atoms: &AtomTable, name: &str) -> Result<Eterm> {
    atoms
        .lookup(name)
        .map(make_atom)
        .ok_or(BeamError::TermNotFound)
}

impl ModuleInfo for MockModuleInfo {
    fn module_info(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        module: &str,
        item: &str,
    ) -> Result<Option<Eterm>> {
        self.calls.push((module.to_string(), item.to_string()));
        if module != self.module || !self.items.iter().any(|known| *known == item) {
            return Ok(None);
        }
        let value = match item {
            "module" => atom_term(atoms, module)?,
            "exports" => {
                let append = atom_term(atoms, "append")?;
                let reverse = atom_term(atoms, "reverse")?;
                let t1 = heap.tuple(&[append, make_small(2)])?;
                let t2 = heap.tuple(&[reverse, make_small(1)])?;
                list_of(heap, &[t1, t2])?
            }
            "functions" => {
                let append = atom_term(atoms, "append")?;
                let t = heap.tuple(&[append, make_small(2)])?;
                list_of(heap, &[t])?
            }
            "nifs" => NIL,
            "attributes" => {
                let vsn = atom_term(atoms, "vsn")?;
                list_of(heap, &[vsn])?
            }
            "compile" => {
                let version = atom_term(atoms, "version")?;
                let t = heap.tuple(&[version, make_small(1)])?;
                list_of(heap, &[t])?
            }
            "md5" => beam_codec::term::make_binary(heap, &[0u8; 16])?,
            "native" => atom_term(atoms, "false")?,
            "native_addresses" => NIL,
            // `self.items` only ever holds the nine names above.
            other => unreachable!("unscripted item {other}"),
        };
        Ok(Some(value))
    }
}

/// An `unsupported_bif` error raise.
fn assert_unsupported(result: Result<Eterm>) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, ExceptionClass::Error);
            assert_eq!(raise.reason, ExitReason::atom("unsupported_bif"));
        }
        other => panic!("expected an unsupported_bif raise, got {other:?}"),
    }
}

/// A `badarg` error.
fn assert_badarg(result: Result<Eterm>) {
    assert!(matches!(result, Err(BeamError::BadArg)), "expected BadArg, got {result:?}");
}

/// The elements of the proper list `term`.
fn list_elements(mut term: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    while term != NIL {
        out.push(cons_head(term).expect("a cons"));
        term = cons_tail(term).expect("a cons");
    }
    out
}

// ---------------------------------------------------------------------------
// `get_module_info/2`
// ---------------------------------------------------------------------------

#[test]
fn item_value_is_returned_directly_not_wrapped() {
    // Oracle: `erlang:get_module_info(lists, module)` ⇒ `lists` (not `{module,lists}`).
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let lists = atom_term(&atoms, "lists").expect("seeded");
    let item = atom_term(&atoms, "module").expect("seeded");
    {
        let mut ctx =
            BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);
        let value = reg
            .call("erlang", "get_module_info", 2, &mut ctx, &[lists, item])
            .expect("the seam answers");
        assert_eq!(name_of(&atoms, value), Some("lists"));
    }
    assert_eq!(hook.calls, vec![("lists".to_string(), "module".to_string())]);
}

#[test]
fn exports_item_is_the_export_list() {
    // Oracle: `get_module_info(lists, exports)` ⇒ a list of `{Name, Arity}` tuples.
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let lists = atom_term(&atoms, "lists").expect("seeded");
    let item = atom_term(&atoms, "exports").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);
    let value = reg
        .call("erlang", "get_module_info", 2, &mut ctx, &[lists, item])
        .expect("the seam answers");

    let elems = list_elements(value);
    assert_eq!(elems.len(), 2);
    for &elem in &elems {
        assert_eq!(tuple_arity(elem).expect("a tuple"), 2);
    }
    let first = elements_of_tuple(elems[0]);
    assert_eq!(name_of(&atoms, first[0]), Some("append"));
}

/// The elements of the 2-tuple `term`.
fn elements_of_tuple(term: Eterm) -> Vec<Eterm> {
    let arity = tuple_arity(term).expect("a tuple");
    (0..arity)
        .map(|i| tuple_element(term, i).expect("in range"))
        .collect()
}

#[test]
fn md5_item_is_a_sixteen_byte_binary() {
    // Oracle: `byte_size(get_module_info(lists, md5))` ⇒ 16.
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let lists = atom_term(&atoms, "lists").expect("seeded");
    let item = atom_term(&atoms, "md5").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);
    let value = reg
        .call("erlang", "get_module_info", 2, &mut ctx, &[lists, item])
        .expect("the seam answers");

    let (bytes, _offset_bits, size_bits) = bitstring_bytes(value).expect("a binary");
    assert_eq!(bytes.len(), 16);
    assert_eq!(size_bits, 128);
}

#[test]
fn every_item_in_the_c_dispatch_chain_is_accepted() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let lists = atom_term(&atoms, "lists").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);

    for name in [
        "module",
        "md5",
        "exports",
        "functions",
        "nifs",
        "attributes",
        "compile",
        "native_addresses",
        "native",
    ] {
        let item = atom_term(&atoms, name).expect("seeded");
        let value = reg
            .call("erlang", "get_module_info", 2, &mut ctx, &[lists, item])
            .unwrap_or_else(|e| panic!("{name} must be accepted, got {e:?}"));
        // Every value is a term (the mock renders one); `native` is the atom `false`.
        if name == "native" {
            assert_eq!(name_of(&atoms, value), Some("false"));
        }
    }
}

#[test]
fn an_unknown_item_atom_is_badarg_without_consulting_the_seam() {
    // Oracle: `erlang:get_module_info(lists, no_such_item)` ⇒ `error:badarg`; and the
    // item is validated before the seam (the C's `what == am_*` chain), so the hook is
    // never asked for an illegal item name.
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let lists = atom_term(&atoms, "lists").expect("seeded");
    let item = atom_term(&atoms, "no_such_item").expect("seeded");
    {
        let mut ctx =
            BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);
        assert_badarg(reg.call("erlang", "get_module_info", 2, &mut ctx, &[lists, item]));
    }
    assert!(hook.calls.is_empty(), "the seam must not be consulted for a bad item");
}

#[test]
fn a_non_atom_item_is_badarg() {
    // Oracle: a list (`[module]`) or an integer (`42`) item is `badarg`.
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let lists = atom_term(&atoms, "lists").expect("seeded");
    let module_atom = atom_term(&atoms, "module").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);

    let list_item = list_of(ctx.heap(), &[module_atom]).expect("fits");
    assert_badarg(reg.call("erlang", "get_module_info", 2, &mut ctx, &[lists, list_item]));
    assert_badarg(reg.call("erlang", "get_module_info", 2, &mut ctx, &[lists, make_small(42)]));
}

#[test]
fn a_non_atom_module_is_badarg() {
    // Oracle: `erlang:get_module_info(42, module)` ⇒ `error:badarg`.
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let item = atom_term(&atoms, "module").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);

    assert_badarg(reg.call("erlang", "get_module_info", 2, &mut ctx, &[make_small(42), item]));
    let list_module = list_of(ctx.heap(), &[make_small(1)]).expect("fits");
    assert_badarg(reg.call("erlang", "get_module_info", 2, &mut ctx, &[list_module, item]));
}

#[test]
fn an_unknown_module_is_badarg() {
    // Oracle: `erlang:get_module_info(no_such_mod_xyz, module)` ⇒ `error:badarg`; the
    // seam signals an unknown module with `Ok(None)`, which this BIF renders as the
    // oracle's `badarg`.
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let unknown = atom_term(&atoms, "no_such_mod").expect("seeded");
    let item = atom_term(&atoms, "module").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);

    assert_badarg(reg.call("erlang", "get_module_info", 2, &mut ctx, &[unknown, item]));
}

#[test]
fn without_a_seam_both_arities_are_unsupported() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let lists = atom_term(&atoms, "lists").expect("seeded");
    let item = atom_term(&atoms, "module").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0));

    assert_unsupported(reg.call("erlang", "get_module_info", 2, &mut ctx, &[lists, item]));
    assert_unsupported(reg.call("erlang", "get_module_info", 1, &mut ctx, &[lists]));
}

// ---------------------------------------------------------------------------
// `get_module_info/1`
// ---------------------------------------------------------------------------

#[test]
fn arity_one_is_the_full_list_in_the_oracles_order() {
    // Oracle: `[element(1,T) || T <- erlang:get_module_info(lists)]` ⇒
    // `[module,exports,attributes,compile,md5]`, each `T` a `{What, Value}` 2-tuple.
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let lists = atom_term(&atoms, "lists").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);
    let value = reg
        .call("erlang", "get_module_info", 1, &mut ctx, &[lists])
        .expect("the seam answers");

    let pairs = list_elements(value);
    let keys: Vec<Option<&str>> = pairs.iter().map(|&p| name_of(&atoms, tuple_element(p, 0).expect("in range"))).collect();
    assert_eq!(keys, vec![Some("module"), Some("exports"), Some("attributes"), Some("compile"), Some("md5")]);
    for &pair in &pairs {
        assert_eq!(tuple_arity(pair).expect("a tuple"), 2);
    }
    // The `module` item's value is the module atom itself.
    let first_value = tuple_element(pairs[0], 1).expect("in range");
    assert_eq!(name_of(&atoms, first_value), Some("lists"));
}

#[test]
fn arity_one_unknown_module_is_badarg() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let unknown = atom_term(&atoms, "no_such_mod").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);

    assert_badarg(reg.call("erlang", "get_module_info", 1, &mut ctx, &[unknown]));
}

#[test]
fn arity_one_non_atom_module_is_badarg() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::full("lists");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);

    assert_badarg(reg.call("erlang", "get_module_info", 1, &mut ctx, &[make_small(42)]));
}

#[test]
fn arity_one_with_a_partially_capable_seam_is_unsupported_not_a_partial_list() {
    // A seam that knows the module (its `module` item resolves) but cannot render a
    // later item must not emit a partial list, nor claim the oracle's unknown-module
    // `badarg` — it is a typed capability gap.
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut hook = MockModuleInfo::new("lists", &["module", "exports", "attributes", "compile"]);
    let lists = atom_term(&atoms, "lists").expect("seeded");
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_module_info(&mut hook);

    assert_unsupported(reg.call("erlang", "get_module_info", 1, &mut ctx, &[lists]));
}

// ---------------------------------------------------------------------------
// Oracle cross-check
// ---------------------------------------------------------------------------

/// Wall-clock budget for the `erl` probe before the oracle is declared unusable.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None`
/// (after printing a clear skip message) when `erl` cannot be spawned / does not
/// answer within [`ERL_TIMEOUT`].
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

#[test]
fn oracle_pins_the_item_value_key_order_and_unknown_item_badarg() {
    // `erlang:get_module_info(lists, module)` ⇒ `lists`;
    // the `/1` key order ⇒ `[module,exports,attributes,compile,md5]`;
    // `erlang:get_module_info(lists, no_such_item)` ⇒ `error:badarg`.
    let expr = concat!(
        "io:format(\"~p~n\",[erlang:get_module_info(lists, module)]),",
        "io:format(\"~p~n\",[[element(1,T)||T<-erlang:get_module_info(lists)]]),",
        "io:format(\"~p~n\",[catch erlang:get_module_info(lists,no_such_item)])",
    );
    let Some(stdout) = erl_stdout(expr) else {
        return;
    };
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    assert_eq!(lines[0], "lists", "oracle item value");
    assert_eq!(
        lines[1], "[module,exports,attributes,compile,md5]",
        "oracle arity-1 key order"
    );
    assert!(lines[2].contains("badarg"), "oracle unknown-item reason: {}", lines[2]);
}
