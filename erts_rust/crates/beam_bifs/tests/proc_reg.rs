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

//! Oracle-pinned tests for the runtime process/name/global-state query BIFs
//! (`src/proc_reg.rs`): `erlang:whereis/1`, `register/2`, `unregister/1`,
//! `registered/0`, `is_process_alive/1`, `processes/0`, `ports/0`,
//! `process_info/1,2`, `process_flag/2` and `group_leader/0`.
//!
//! They are driven through the **public** API only (`default_registry()` +
//! `BifRegistry::call`), in three layers:
//!
//! * **(a)** with **no** `ProcessHook` installed, every key is the typed
//!   `unsupported_bif` capability gap — never `[]`, never `undefined`, never
//!   `false` (workspace `CONTEXT.md` §3);
//! * **(b)** with a mock `ProcessHook`, the argument validation and the term
//!   shapes each key produces;
//! * **(c)** a live cross-check against the system `erl`, which **skips loudly**
//!   when the oracle is unavailable (the `tests/os_bif.rs` / `tests/nif.rs`
//!   pattern).
//!
//! Every (b) golden carries the system-OTP probe that pinned it (OTP 29.1.1 /
//! erts-17.1).

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::Result;
use beam_process::{PortId, ProcessId};
use beam_term::{atom_val, is_list, make_atom, make_small, Atom, AtomTable, Eterm, NIL};

/// A small heap: these BIFs build only short lists.
fn heap() -> Heap {
    Heap::new(1024).expect("test heap")
}

/// The atoms these tests resolve by name.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "undefined", "init", "a", "b", "missing", "status"]);
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

/// The name of an atom term, as the shared atom table spells it.
fn atom_name(atoms: &AtomTable, term: Eterm) -> String {
    atoms.name(atom_val(term)).expect("an interned atom name").to_string()
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
/// raises for the invalid cases it decides from the arguments alone).
fn assert_badarg(result: Result<Eterm>) {
    match result {
        Err(err) => assert_eq!(err.as_atom(), Some("badarg"), "expected badarg, got {err:?}"),
        Ok(_) => panic!("expected badarg, got a value"),
    }
}

/// Whether the list term `list` equals the proper list built from `expected`.
fn list_eq(ctx: &mut BifContext<'_>, list: Eterm, expected: &[Eterm]) -> bool {
    let want = ctx.list(expected).expect("the expected list fits");
    beam_term::compare::eq(list, want, ctx.atoms())
}

/// The value the stub reports for one `process_info/2` item: a small integer, a
/// float *built on the caller's heap* (so the heap threading is exercised) or an
/// atom (so the atom table threading is).
#[derive(Clone, Copy)]
enum StubItemValue {
    Small(i64),
    Float(f64),
    Atom(Atom),
}

impl StubItemValue {
    /// Materializes the value on `heap`.
    fn build(self, heap: &mut Heap) -> Result<Eterm> {
        match self {
            StubItemValue::Small(v) => Ok(make_small(v)),
            StubItemValue::Float(v) => beam_codec::term::make_float(heap, v),
            StubItemValue::Atom(a) => Ok(make_atom(a)),
        }
    }
}

/// A `ProcessHook` test double: a name registry, a set of live pids, the
/// process/port inventories, the `process_info` item vocabulary and the
/// `process_flag` table — the state a real `beam_rt` provider holds.
#[derive(Default)]
struct StubProcesses {
    registry: Vec<(Atom, Eterm)>,
    alive: Vec<ProcessId>,
    pids: Vec<ProcessId>,
    ports: Vec<PortId>,
    /// The item names `process_info/1` reports, in order — the runtime's own set.
    info_keys: Vec<Atom>,
    /// The `process_info/2` item vocabulary: an item outside it is `Unknown`.
    info_items: Vec<(Atom, StubItemValue)>,
    /// The process-flag table (`process_flag/2`).
    flags: Vec<(Atom, Eterm)>,
    /// The flag whose value must be the atom `true`/`false` (the oracle's
    /// `trap_exit`), if any: any other value is the oracle's `badarg`.
    boolean_flag: Option<Atom>,
    /// The calling process's group leader (`group_leader/0`).
    leader: Option<ProcessId>,
}

impl StubProcesses {
    /// Whether `id` is already registered under some name.
    fn is_named(&self, id: Eterm) -> bool {
        self.registry.iter().any(|&(_, stored)| stored == id)
    }

    /// The stored value of the item `item`, or `None` when the stub does not know it.
    fn item_value(&self, item: Atom) -> Option<StubItemValue> {
        self.info_items.iter().find(|&&(i, _)| i == item).map(|&(_, v)| v)
    }

    /// Whether `term` is the atom `true` or `false` of the stub's atom table.
    fn is_boolean(atoms: &AtomTable, term: Eterm) -> bool {
        if !beam_term::is_atom(term) {
            return false;
        }
        Some(atom_val(term)) == atoms.lookup("true") || Some(atom_val(term)) == atoms.lookup("false")
    }
}

impl ProcessHook for StubProcesses {
    fn whereis(&mut self, name: Atom) -> Result<Option<Eterm>> {
        Ok(self.registry.iter().find(|&&(n, _)| n == name).map(|&(_, id)| id))
    }

    fn register(&mut self, name: Atom, id: Eterm) -> Result<bool> {
        // A taken name, or an id that already has a name, is the oracle's badarg.
        if self.registry.iter().any(|&(n, _)| n == name) || self.is_named(id) {
            return Ok(false);
        }
        // A pid that is not alive cannot be registered.
        if let Ok(pid) = ProcessId::from_eterm(id) {
            if !self.alive.contains(&pid) {
                return Ok(false);
            }
        }
        self.registry.push((name, id));
        Ok(true)
    }

    fn unregister(&mut self, name: Atom) -> Result<bool> {
        match self.registry.iter().position(|&(n, _)| n == name) {
            Some(index) => {
                self.registry.remove(index);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn registered(&mut self) -> Result<Vec<Atom>> {
        Ok(self.registry.iter().map(|&(n, _)| n).collect())
    }

    fn is_process_alive(&mut self, pid: ProcessId) -> Result<bool> {
        Ok(self.alive.contains(&pid))
    }

    fn processes(&mut self) -> Result<Vec<ProcessId>> {
        Ok(self.pids.clone())
    }

    fn ports(&mut self) -> Result<Vec<PortId>> {
        Ok(self.ports.clone())
    }

    fn process_info_keys(&mut self, pid: ProcessId) -> Result<Option<Vec<Atom>>> {
        if !self.alive.contains(&pid) {
            return Ok(None);
        }
        Ok(Some(self.info_keys.clone()))
    }

    fn process_info_item(
        &mut self,
        heap: &mut Heap,
        atoms: &AtomTable,
        pid: ProcessId,
        item: Atom,
    ) -> Result<ProcessInfoItem> {
        // The item is validated *before* the process is inspected, exactly as the
        // oracle's `pi_arg2ix` does: an unknown item is `badarg` even for a dead pid.
        let Some(value) = self.item_value(item) else {
            return Ok(ProcessInfoItem::Unknown);
        };
        if !self.alive.contains(&pid) {
            return Ok(ProcessInfoItem::Dead);
        }
        let _ = atoms;
        Ok(ProcessInfoItem::Value(value.build(heap)?))
    }

    fn process_flag(
        &mut self,
        _heap: &mut Heap,
        atoms: &AtomTable,
        flag: Atom,
        value: Eterm,
    ) -> Result<Option<Eterm>> {
        let Some(index) = self.flags.iter().position(|&(f, _)| f == flag) else {
            // An unknown flag is the oracle's badarg.
            return Ok(None);
        };
        // A flag whose value must be a boolean rejects anything else (badarg).
        if self.boolean_flag == Some(flag) && !Self::is_boolean(atoms, value) {
            return Ok(None);
        }
        let previous = self.flags[index].1;
        self.flags[index].1 = value;
        Ok(Some(previous))
    }

    fn group_leader(&mut self) -> Result<ProcessId> {
        // The mock always has a leader (a real provider's invoking process does too).
        Ok(self.leader.unwrap_or_else(|| ProcessId::new(1, 0)))
    }
}

// ---------------------------------------------------------------------------
// (a) No hook: every key is the typed capability gap
// ---------------------------------------------------------------------------

#[test]
fn every_query_is_unsupported_without_a_hook() {
    // A fabricated `[]`, `undefined` or `false` would be exactly the anti-pattern
    // the crate forbids; every one of the eleven keys must instead report the gap.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let init = atom_term(&atoms, "init");
    let status = atom_term(&atoms, "status");
    let pid = ProcessId::new(7, 0).to_eterm();

    assert_unsupported(call(&reg, &mut ctx, "erlang", "whereis", 1, &[init]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "register", 2, &[init, pid]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "unregister", 1, &[init]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "registered", 0, &[]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "is_process_alive", 1, &[pid]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "processes", 0, &[]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "ports", 0, &[]));
    // The four process-introspection keys: never a fabricated item list, never
    // `undefined`, never a made-up previous flag value, never a fabricated pid.
    assert_unsupported(call(&reg, &mut ctx, "erlang", "process_info", 1, &[pid]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "process_info", 2, &[pid, status]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "process_flag", 2, &[init, pid]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "group_leader", 0, &[]));
}

#[test]
fn argument_errors_are_badarg_even_without_a_hook() {
    // The oracle's `badarg` cases the arguments alone decide are validated *before*
    // the seam is consulted, so they are `badarg` — not `unsupported_bif` — even
    // with no runtime hook installed.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let init = atom_term(&atoms, "init");
    let undefined = atom_term(&atoms, "undefined");
    let status = atom_term(&atoms, "status");
    let pid = ProcessId::new(7, 0).to_eterm();
    let non_atom = make_small(42);

    // `whereis/1`/`unregister/1`: a non-atom name (`bif.c:2346`, `:2327`).
    assert_badarg(call(&reg, &mut ctx, "erlang", "whereis", 1, &[non_atom]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "unregister", 1, &[non_atom]));
    // `register/2`: a non-atom name, the atom `undefined`, or a non-pid/port id
    // (`register.c:184,190`).
    assert_badarg(call(&reg, &mut ctx, "erlang", "register", 2, &[non_atom, pid]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "register", 2, &[undefined, pid]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "register", 2, &[init, non_atom]));
    // `is_process_alive/1`: a term that is not an internal pid
    // (`erl_bif_info.c:4072`).
    assert_badarg(call(&reg, &mut ctx, "erlang", "is_process_alive", 1, &[non_atom]));
    assert_badarg(call(
        &reg,
        &mut ctx,
        "erlang",
        "is_process_alive",
        1,
        &[PortId::new(1).to_eterm()],
    ));
    // `process_info/1,2`: a `Pid` that is not an internal pid (`erl_bif_info.c:1556`;
    // a port is `badarg` too) and a non-atom/non-list item specification.
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 1, &[non_atom]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 1, &[PortId::new(1).to_eterm()]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[non_atom, status]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[pid, non_atom]));
    // `process_flag/2`: a `Flag` that is not an atom (`bif.c:2024-2025`).
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_flag", 2, &[non_atom, pid]));
}

// ---------------------------------------------------------------------------
// (b) With the mock hook: argument validation and term shapes
// ---------------------------------------------------------------------------

#[test]
fn whereis_returns_the_registered_id_or_undefined() {
    // ORACLE: `erlang:whereis(init)` => `<0.0.0>`;
    //         `erlang:whereis(no_such_name)` => `undefined`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let a = atoms.lookup("a").expect("seeded");
    let pid = ProcessId::new(7, 0);

    let mut hook = StubProcesses::default();
    hook.registry.push((a, pid.to_eterm()));
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    let name_a = make_atom(a);
    let found = call(&reg, &mut ctx, "erlang", "whereis", 1, &[name_a]).expect("the hook answers");
    assert_eq!(found, pid.to_eterm());
    assert!(ProcessId::is_process(found));

    let missing = atom_term(&atoms, "missing");
    let none = call(&reg, &mut ctx, "erlang", "whereis", 1, &[missing]).expect("the hook answers");
    assert_eq!(atom_name(&atoms, none), "undefined");
}

#[test]
fn whereis_rejects_a_non_atom_name() {
    // ORACLE: `catch erlang:whereis(42)` => `{'EXIT',{badarg,…}}`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut hook = StubProcesses::default();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    assert_badarg(call(&reg, &mut ctx, "erlang", "whereis", 1, &[make_small(42)]));
}

#[test]
fn register_returns_true_and_rejects_the_badarg_cases() {
    // ORACLE (erts-17.1):
    //   `erlang:register(my_name, self())` => `true`
    //   `erlang:register(init, self())` (taken) => badarg
    //   `erlang:register(undefined, self())` => badarg
    //   `erlang:register(42, self())` (non-atom name) => badarg
    //   `erlang:register(my_name, 42)` (non-pid/port id) => badarg
    //   `erlang:register(my_name, <dead pid>)` => badarg
    //   register a name twice => badarg; give one pid a second name => badarg
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let a = atoms.lookup("a").expect("seeded");
    let b = atoms.lookup("b").expect("seeded");
    let undefined = atoms.lookup("undefined").expect("seeded");
    let live = ProcessId::new(7, 0);
    let other = ProcessId::new(8, 0);
    let dead = ProcessId::new(9, 0);
    let port = PortId::new(1);

    let mut hook = StubProcesses::default();
    hook.alive.push(live);
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    let name_a = make_atom(a);
    let name_b = make_atom(b);

    // A fresh name for a live pid.
    let ok = call(&reg, &mut ctx, "erlang", "register", 2, &[name_a, live.to_eterm()])
        .expect("the hook answers");
    assert_eq!(atom_name(&atoms, ok), "true");

    // The name is now taken; the id already has a name; a dead pid is refused.
    assert_badarg(call(&reg, &mut ctx, "erlang", "register", 2, &[name_a, other.to_eterm()]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "register", 2, &[name_b, live.to_eterm()]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "register", 2, &[name_b, dead.to_eterm()]));

    // The argument-only `badarg` cases.
    assert_badarg(call(&reg, &mut ctx, "erlang", "register", 2, &[make_small(42), live.to_eterm()]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "register", 2, &[make_atom(undefined), live.to_eterm()]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "register", 2, &[name_b, make_small(42)]));

    // A port id is a valid registry entry too (`register.c:190`).
    let ok = call(&reg, &mut ctx, "erlang", "register", 2, &[name_b, port.to_eterm()])
        .expect("the hook answers");
    assert_eq!(atom_name(&atoms, ok), "true");
}

#[test]
fn unregister_returns_true_and_rejects_unregistered_names() {
    // ORACLE: `erlang:unregister(init)` => `true`;
    //         `erlang:unregister(no_such_name)` => badarg;
    //         `erlang:unregister(42)` => badarg.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let a = atoms.lookup("a").expect("seeded");

    let mut hook = StubProcesses::default();
    hook.registry.push((a, ProcessId::new(7, 0).to_eterm()));
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    let name_a = make_atom(a);
    let ok = call(&reg, &mut ctx, "erlang", "unregister", 1, &[name_a]).expect("the hook answers");
    assert_eq!(atom_name(&atoms, ok), "true");
    // Gone now, and a name that never existed, are both badarg.
    assert_badarg(call(&reg, &mut ctx, "erlang", "unregister", 1, &[name_a]));
    assert_badarg(call(
        &reg,
        &mut ctx,
        "erlang",
        "unregister",
        1,
        &[atom_term(&atoms, "missing")],
    ));
    // A non-atom name is badarg too.
    assert_badarg(call(&reg, &mut ctx, "erlang", "unregister", 1, &[make_small(42)]));
}

#[test]
fn registered_lists_the_names_in_the_hooks_order() {
    // ORACLE: `erlang:registered()` => a list of name atoms (order unspecified).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let a = atoms.lookup("a").expect("seeded");
    let b = atoms.lookup("b").expect("seeded");
    let init = atoms.lookup("init").expect("seeded");

    let mut hook = StubProcesses::default();
    hook.registry.push((a, ProcessId::new(7, 0).to_eterm()));
    hook.registry.push((b, ProcessId::new(8, 0).to_eterm()));
    hook.registry.push((init, PortId::new(1).to_eterm()));
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    let listed = call(&reg, &mut ctx, "erlang", "registered", 0, &[]).expect("the hook answers");
    assert!(is_list(listed));
    let expected = [make_atom(a), make_atom(b), make_atom(init)];
    assert!(list_eq(&mut ctx, listed, &expected), "the names are rendered in the hook's order");
}

#[test]
fn is_process_alive_is_true_false_or_badarg() {
    // ORACLE: `erlang:is_process_alive(self())` => `true`;
    //         `erlang:is_process_alive(<dead pid>)` => `false`;
    //         `erlang:is_process_alive(42)` => badarg.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let live = ProcessId::new(7, 0);
    let dead = ProcessId::new(9, 0);

    let mut hook = StubProcesses::default();
    hook.alive.push(live);
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    let alive = call(
        &reg,
        &mut ctx,
        "erlang",
        "is_process_alive",
        1,
        &[live.to_eterm()],
    )
    .expect("the hook answers");
    assert_eq!(atom_name(&atoms, alive), "true");

    let gone = call(
        &reg,
        &mut ctx,
        "erlang",
        "is_process_alive",
        1,
        &[dead.to_eterm()],
    )
    .expect("the hook answers");
    assert_eq!(atom_name(&atoms, gone), "false");
}

#[test]
fn processes_and_ports_list_their_ids() {
    // ORACLE: `erlang:processes()` => a list of pids;
    //         `erlang:ports()` => a list of ports (order unspecified).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let p1 = ProcessId::new(7, 0);
    let p2 = ProcessId::new(8, 0);
    let port1 = PortId::new(1);
    let port2 = PortId::new(2);

    let mut hook = StubProcesses::default();
    hook.pids = vec![p1, p2];
    hook.ports = vec![port1, port2];
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    let pids = call(&reg, &mut ctx, "erlang", "processes", 0, &[]).expect("the hook answers");
    assert!(is_list(pids));
    assert!(
        list_eq(&mut ctx, pids, &[p1.to_eterm(), p2.to_eterm()]),
        "processes/0 renders the hook's pids"
    );

    let ports = call(&reg, &mut ctx, "erlang", "ports", 0, &[]).expect("the hook answers");
    assert!(is_list(ports));
    assert!(
        list_eq(&mut ctx, ports, &[port1.to_eterm(), port2.to_eterm()]),
        "ports/0 renders the hook's ports"
    );

    // An empty inventory is an empty list (never a fabricated element).
    let mut hook = StubProcesses::default();
    let mut empty =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);
    let none = call(&reg, &mut empty, "erlang", "ports", 0, &[]).expect("the hook answers");
    assert_eq!(none, NIL);
}

// ---------------------------------------------------------------------------
// (b) The process-introspection keys over the mock hook
// ---------------------------------------------------------------------------

/// The elements of the proper list `list` (the lists in these tests are short).
fn list_items(list: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    let mut tail = list;
    while is_list(tail) {
        out.push(beam_codec::term::cons_head(tail).expect("a cons head"));
        tail = beam_codec::term::cons_tail(tail).expect("a cons tail");
    }
    assert!(beam_term::is_nil(tail), "the list must be proper");
    out
}

/// Whether `term` is the `{Key, Value}` 2-tuple whose key is the atom `key` and
/// whose value equals `value` (boxed `Eterm`s compare by address, so equality goes
/// through `beam_term::compare::eq`).
fn is_pair(ctx: &BifContext<'_>, term: Eterm, key: &str, value: Eterm) -> bool {
    if !matches!(beam_codec::term::tuple_arity(term), Ok(2)) {
        return false;
    }
    let k = beam_codec::term::tuple_element(term, 0).expect("a tuple element");
    let v = beam_codec::term::tuple_element(term, 1).expect("a tuple element");
    beam_term::compare::eq(k, atom_term(ctx.atoms(), key), ctx.atoms())
        && beam_term::compare::eq(v, value, ctx.atoms())
}

#[test]
fn process_info_1_renders_the_hooks_item_list() {
    // ORACLE (erts-17.1): `erlang:process_info(self())` =>
    //   current_function, initial_call, status, message_queue_len, links,
    //   dictionary, trap_exit, error_handler, priority, group_leader,
    //   total_heap_size, heap_size, stack_size, reductions, garbage_collection,
    //   suspending
    // with `registered_name` *omitted* for a process that has no registered name
    // (`erl_bif_info.c:1606-1633`). The item set and its order are the hook's, so
    // the BIF layer can never fabricate either.
    let reg = default_registry();
    let mut heap = heap();
    let mut atoms = seeded_atoms();
    atoms.seed(["status", "priority", "message_queue_len"]);
    let status = atoms.lookup("status").expect("seeded");
    let priority = atoms.lookup("priority").expect("seeded");
    let mql = atoms.lookup("message_queue_len").expect("seeded");
    let ok = atoms.lookup("ok").expect("seeded");
    let live = ProcessId::new(7, 0);
    let dead = ProcessId::new(9, 0);

    let mut hook = StubProcesses::default();
    hook.alive.push(live);
    hook.info_keys = vec![status, mql, priority];
    hook.info_items = vec![
        (status, StubItemValue::Small(3)),
        (mql, StubItemValue::Small(4)),
        (priority, StubItemValue::Atom(ok)),
    ];
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    let info = call(&reg, &mut ctx, "erlang", "process_info", 1, &[live.to_eterm()])
        .expect("the hook answers");
    let items = list_items(info);
    assert_eq!(items.len(), 3, "one pair per hook-supplied item, in the hook's order");
    assert!(is_pair(&ctx, items[0], "status", make_small(3)));
    assert!(is_pair(&ctx, items[1], "message_queue_len", make_small(4)));
    // The float/atom values prove the hook built them on the context's heap/table.
    assert!(is_pair(&ctx, items[2], "priority", make_atom(ok)));

    // A dead pid is the atom `undefined` — never an empty or partial list.
    let gone = call(&reg, &mut ctx, "erlang", "process_info", 1, &[dead.to_eterm()])
        .expect("the hook answers");
    assert_eq!(atom_name(&atoms, gone), "undefined");
}

#[test]
fn process_info_2_single_item_and_list_shapes() {
    // ORACLE (erts-17.1): `erlang:process_info(self(), status)` => `{status,running}`;
    //   `erlang:process_info(self(), [status, message_queue_len])` =>
    //     `[{status,running},{message_queue_len,0}]`;
    //   `erlang:process_info(self(), [])` => `[]`.
    let reg = default_registry();
    let mut heap = heap();
    let mut atoms = seeded_atoms();
    atoms.seed(["status", "message_queue_len"]);
    let status = atoms.lookup("status").expect("seeded");
    let mql = atoms.lookup("message_queue_len").expect("seeded");
    let live = ProcessId::new(7, 0);

    let mut hook = StubProcesses::default();
    hook.alive.push(live);
    hook.info_items = vec![
        (status, StubItemValue::Float(1.5)),
        (mql, StubItemValue::Small(0)),
    ];
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    // A single item atom yields the bare `{Item, Value}` pair.
    let one = call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), make_atom(status)])
        .expect("the hook answers");
    let expected = beam_codec::term::make_float(ctx.heap(), 1.5).expect("a float fits");
    assert!(is_pair(&ctx, one, "status", expected));

    // A list yields the corresponding list of pairs, in the requested order.
    let spec = ctx.list(&[make_atom(mql), make_atom(status)]).expect("the spec fits");
    let listed = call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), spec])
        .expect("the hook answers");
    let items = list_items(listed);
    assert_eq!(items.len(), 2);
    assert!(is_pair(&ctx, items[0], "message_queue_len", make_small(0)));
    let expected = beam_codec::term::make_float(ctx.heap(), 1.5).expect("a float fits");
    assert!(is_pair(&ctx, items[1], "status", expected));

    // The empty item list is the empty list for a live process.
    let spec = ctx.list(&[]).expect("the spec fits");
    let none = call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), spec])
        .expect("the hook answers");
    assert_eq!(none, NIL);
}

#[test]
fn process_info_2_validates_the_item_before_the_process() {
    // ORACLE (erts-17.1): `erlang:process_info(<dead>, [status])` => `undefined`,
    //   `erlang:process_info(<dead>, registered_name)` => `undefined`,
    //   `erlang:process_info(<dead>, [])` => `undefined`, but
    //   `erlang:process_info(<dead>, [status, frobnicate])` => badarg and
    //   `erlang:process_info(<dead>, frobnicate)` => badarg: the C validates every
    //   requested item (`pi_arg2ix`) *before* it inspects the process.
    let reg = default_registry();
    let mut heap = heap();
    let mut atoms = seeded_atoms();
    atoms.seed(["status", "frobnicate"]);
    let status = atoms.lookup("status").expect("seeded");
    let frobnicate = atoms.lookup("frobnicate").expect("seeded");
    let live = ProcessId::new(7, 0);
    let dead = ProcessId::new(9, 0);

    let mut hook = StubProcesses::default();
    hook.alive.push(live);
    hook.info_items = vec![(status, StubItemValue::Small(3))];
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    // A valid item on a dead pid is `undefined`…
    for spec_atom in [status] {
        let gone = call(&reg, &mut ctx, "erlang", "process_info", 2, &[dead.to_eterm(), make_atom(spec_atom)])
            .expect("the hook answers");
        assert_eq!(atom_name(&atoms, gone), "undefined");
    }
    // …and so is the empty list (the oracle inspects the process even then)…
    let empty = ctx.list(&[]).expect("the spec fits");
    let gone = call(&reg, &mut ctx, "erlang", "process_info", 2, &[dead.to_eterm(), empty])
        .expect("the hook answers");
    assert_eq!(atom_name(&atoms, gone), "undefined");

    // …but an unknown item beats the dead process (badarg, not undefined).
    assert_badarg(call(
        &reg,
        &mut ctx,
        "erlang",
        "process_info",
        2,
        &[dead.to_eterm(), make_atom(frobnicate)],
    ));
    let spec = ctx.list(&[make_atom(status), make_atom(frobnicate)]).expect("the spec fits");
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[dead.to_eterm(), spec]));

    // For a *live* process the same list is badarg for the same reason.
    let spec = ctx.list(&[make_atom(frobnicate)]).expect("the spec fits");
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), spec]));
}

#[test]
fn process_info_2_rejects_a_bad_item_specification() {
    // ORACLE (erts-17.1): `process_info(self(), 42)`, `…, {a,b})`, `…, [[status]])`
    // and `…, [status|frobnicate])` are all badarg, and so is a list with a
    // non-atom element (`…, [42])`) — the item specification is an atom or a proper
    // list of atoms, nothing else.
    let reg = default_registry();
    let mut heap = heap();
    let mut atoms = seeded_atoms();
    atoms.seed(["status"]);
    let status = atoms.lookup("status").expect("seeded");
    let live = ProcessId::new(7, 0);

    let mut hook = StubProcesses::default();
    hook.alive.push(live);
    hook.info_items = vec![(status, StubItemValue::Small(3))];
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 1, &[make_small(42)]));
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), make_small(42)]));
    let tuple = ctx.tuple(&[make_small(1), make_small(2)]).expect("the tuple fits");
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), tuple]));
    // A list with a non-atom element.
    let spec = ctx.list(&[make_small(42)]).expect("the spec fits");
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), spec]));
    // A nested list.
    let inner = ctx.list(&[make_atom(status)]).expect("the inner list fits");
    let spec = ctx.list(&[inner]).expect("the spec fits");
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), spec]));
    // An improper list.
    let spec = ctx.cons(make_atom(status), make_atom(status)).expect("the cons fits");
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), spec]));
    // A non-pid `Pid` on either arity (a port included).
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 1, &[PortId::new(1).to_eterm()]));
    assert_badarg(call(
        &reg,
        &mut ctx,
        "erlang",
        "process_info",
        2,
        &[make_small(42), make_atom(status)],
    ));
}

#[test]
fn process_info_2_reports_the_dictionary_item_form_as_a_typed_gap() {
    // ORACLE (erts-17.1): the C's `pi_arg2ix` (`erl_bif_info.c:992-1005`) also
    // accepts the 2-tuple `{dictionary, Key}` as an item, both as the whole second
    // argument and as a list element:
    //   `erlang:process_info(self(), {dictionary, foo})` => `{{dictionary,foo},undefined}`
    //   `erlang:process_info(self(), [{dictionary, foo}])` => `[{{dictionary,foo},undefined}]`
    // while `{a,b}` (a tuple whose head is not the atom `dictionary`) and
    // `{a,b,c}` are badarg.
    //
    // The lookup reads the *inspected* process's dictionary, which this
    // milestone's ProcessHook item vocabulary (a single atom) cannot express, so the
    // form is recognized and reported as the typed capability gap: never a
    // fabricated pair, and never `badarg` for a form the oracle accepts.
    let reg = default_registry();
    let mut heap = heap();
    let mut atoms = seeded_atoms();
    atoms.seed(["dictionary", "status", "foo"]);
    let dictionary = atoms.lookup("dictionary").expect("seeded");
    let status = atoms.lookup("status").expect("seeded");
    let foo = atoms.lookup("foo").expect("seeded");
    let live = ProcessId::new(7, 0);

    let mut hook = StubProcesses::default();
    hook.alive.push(live);
    hook.info_items = vec![(status, StubItemValue::Small(3))];
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    // `{dictionary, Key}` as the whole second argument.
    let dict = make_atom(dictionary);
    let key = make_atom(foo);
    let tuple = ctx.tuple(&[dict, key]).expect("the tuple fits");
    assert_unsupported(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), tuple]));

    // …and as an element of an item list.
    let spec = ctx.list(&[tuple]).expect("the spec fits");
    assert_unsupported(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), spec]));

    // A 2-tuple whose head is another atom is still the oracle's badarg.
    let other = make_atom(status);
    let key = make_small(1);
    let tuple = ctx.tuple(&[other, key]).expect("the tuple fits");
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), tuple]));
    // As is a 3-tuple (the C only special-cases arity 2).
    let dict = make_atom(dictionary);
    let key = make_atom(foo);
    let extra = make_small(0);
    let triple = ctx.tuple(&[dict, key, extra]).expect("the tuple fits");
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_info", 2, &[live.to_eterm(), triple]));
}

#[test]
fn process_flag_returns_the_previous_value_and_rejects_badarg_cases() {
    // ORACLE (erts-17.1):
    //   `process_flag(trap_exit, true)` => the previous `false`;
    //   `process_flag(trap_exit, true)` again => `true`;
    //   `process_flag(priority, high)` => `normal`;
    //   `process_flag(frobnicate, 1)` (unknown flag) => badarg;
    //   `process_flag(trap_exit, not_a_bool)` (illegal value) => badarg;
    //   `process_flag(42, true)` (non-atom flag) => badarg.
    let reg = default_registry();
    let mut heap = heap();
    let mut atoms = seeded_atoms();
    atoms.seed(["trap_exit", "priority", "high", "normal", "frobnicate", "not_a_bool"]);
    let trap_exit = atoms.lookup("trap_exit").expect("seeded");
    let priority = atoms.lookup("priority").expect("seeded");
    let high = atoms.lookup("high").expect("seeded");
    let normal = atoms.lookup("normal").expect("seeded");
    let false_ = atom_term(&atoms, "false");
    let true_ = atom_term(&atoms, "true");

    let mut hook = StubProcesses::default();
    hook.boolean_flag = Some(trap_exit);
    hook.flags = vec![(trap_exit, false_), (priority, make_atom(normal))];
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    // The previous value is returned, and the flag really changes.
    let previous = call(&reg, &mut ctx, "erlang", "process_flag", 2, &[make_atom(trap_exit), true_])
        .expect("the hook answers");
    assert_eq!(atom_name(&atoms, previous), "false");
    let previous = call(&reg, &mut ctx, "erlang", "process_flag", 2, &[make_atom(trap_exit), false_])
        .expect("the hook answers");
    assert_eq!(atom_name(&atoms, previous), "true");

    let previous = call(&reg, &mut ctx, "erlang", "process_flag", 2, &[make_atom(priority), make_atom(high)])
        .expect("the hook answers");
    assert_eq!(atom_name(&atoms, previous), "normal");

    // An unknown flag and an illegal value for a known one are the oracle's badarg.
    let frobnicate = make_atom(atoms.lookup("frobnicate").expect("seeded"));
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_flag", 2, &[frobnicate, make_small(1)]));
    let not_a_bool = make_atom(atoms.lookup("not_a_bool").expect("seeded"));
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_flag", 2, &[make_atom(trap_exit), not_a_bool]));
    // A non-atom flag never reaches the hook.
    assert_badarg(call(&reg, &mut ctx, "erlang", "process_flag", 2, &[make_small(42), true_]));
}

#[test]
fn group_leader_returns_the_hooks_pid() {
    // ORACLE (erts-17.1): `erlang:group_leader()` => a pid (its own `self()` is
    // `<0.70.0>` in a plain `-eval` run, i.e. whatever `init` set).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let leader = ProcessId::new(70, 0);

    let mut hook = StubProcesses::default();
    hook.leader = Some(leader);
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_process_hook(&mut hook);

    let got = call(&reg, &mut ctx, "erlang", "group_leader", 0, &[]).expect("the hook answers");
    assert_eq!(got, leader.to_eterm());
    assert!(ProcessId::is_process(got));
}

#[test]
fn register_installs_the_eleven_keys() {
    let reg = default_registry();
    assert!(reg.contains("erlang", "whereis", 1));
    assert!(reg.contains("erlang", "register", 2));
    assert!(reg.contains("erlang", "unregister", 1));
    assert!(reg.contains("erlang", "registered", 0));
    assert!(reg.contains("erlang", "is_process_alive", 1));
    assert!(reg.contains("erlang", "processes", 0));
    assert!(reg.contains("erlang", "ports", 0));
    assert!(reg.contains("erlang", "process_info", 1));
    assert!(reg.contains("erlang", "process_info", 2));
    assert!(reg.contains("erlang", "process_flag", 2));
    assert!(reg.contains("erlang", "group_leader", 0));
}

// ---------------------------------------------------------------------------
// (c) Live oracle cross-check
// ---------------------------------------------------------------------------

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the test then skips loudly).
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
fn live_oracle_agrees_on_the_process_info_and_flag_semantics() {
    // One live cross-check against the system ERTS (OTP 29.1.1 / erts-17.1): the
    // `process_info/1` item order (and the `registered_name` omission), the
    // `process_info/2` single-item/list/empty-list shapes, the dead-pid `undefined`,
    // the item *before* the process validation (`badarg`), the `process_flag/2`
    // previous-value round trip and its `badarg` cases, and `group_leader/0`.
    // Skips loudly when the oracle is unavailable.
    let Some(output) = oracle_eval(
        "P = spawn(fun() -> ok end), \
         M = erlang:monitor(process, P), \
         receive {'DOWN', M, _, _, _} -> ok end, \
         Keys = [K || {K, _} <- erlang:process_info(self())], \
         R1 = erlang:process_flag(trap_exit, true), \
         R2 = erlang:process_flag(trap_exit, true), \
         R3 = erlang:process_flag(trap_exit, false), \
         R4 = erlang:process_flag(trap_exit, R1), \
         io:format(\"~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p~n\",[\
             Keys =:= [current_function, initial_call, status, message_queue_len, links,\
                       dictionary, trap_exit, error_handler, priority, group_leader,\
                       total_heap_size, heap_size, stack_size, reductions,\
                       garbage_collection, suspending],\
             lists:member(registered_name, Keys) =:= false,\
             erlang:process_info(self(), registered_name) =:= [],\
             [erlang:element(1, T) || T <- erlang:process_info(self(), [status])] =:= [status],\
             erlang:process_info(self(), []) =:= [],\
             erlang:process_info(P, status) =:= undefined,\
             erlang:process_info(P, [status]) =:= undefined,\
             erlang:process_info(P, []) =:= undefined,\
             erlang:element(1, catch erlang:process_info(P, [status, frobnicate])) =:= 'EXIT',\
             erlang:element(1, catch erlang:process_info(self(), frobnicate)) =:= 'EXIT',\
             erlang:element(1, catch erlang:process_info(self(), 42)) =:= 'EXIT',\
             erlang:element(1, catch erlang:process_info(42, status)) =:= 'EXIT',\
             erlang:is_boolean(R1) andalso R2 =:= true andalso R3 =:= true andalso R4 =:= false,\
             erlang:element(1, catch erlang:process_flag(frobnicate, 1)) =:= 'EXIT',\
             erlang:element(1, catch erlang:process_flag(trap_exit, not_a_bool)) =:= 'EXIT',\
             erlang:element(1, catch erlang:process_flag(42, true)) =:= 'EXIT',\
             erlang:is_pid(erlang:group_leader())])",
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true|true",
        "the live oracle disagrees with the pinned process_info/process_flag semantics"
    );
}

#[test]
fn live_oracle_agrees_on_the_registry_and_liveness_semantics() {
    // One live cross-check against the system ERTS: every shape the (a)/(b) layers
    // pin must also come out of the real `erl` today. Skips loudly when the oracle
    // is unavailable.
    let Some(output) = oracle_eval(
        "erlang:register(zz_proc_reg_oracle, self()), \
         R1 = (catch erlang:register(zz_proc_reg_oracle, self())), \
         R2 = (catch erlang:register(zz_proc_reg_other, self())), \
         R3 = erlang:unregister(zz_proc_reg_oracle), \
         R4 = (catch erlang:unregister(zz_proc_reg_oracle)), \
         io:format(\"~p|~p|~p|~p|~p|~p|~p|~p|~p|~p|~p~n\",[\
             erlang:is_pid(erlang:whereis(init)),\
             erlang:whereis(zz_no_such_name_zz),\
             element(1,catch erlang:whereis(42)) =:= 'EXIT',\
             element(1,catch erlang:register(init,self())) =:= 'EXIT',\
             element(1,catch erlang:register(undefined,self())) =:= 'EXIT',\
             element(1,catch erlang:register(zz_proc_reg_atom,42)) =:= 'EXIT',\
             element(1,R1) =:= 'EXIT',\
             element(1,R2) =:= 'EXIT',\
             R3,\
             element(1,R4) =:= 'EXIT',\
             element(1,catch erlang:is_process_alive(42)) =:= 'EXIT'])",
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "true|undefined|true|true|true|true|true|true|true|true|true",
        "the live oracle disagrees with the pinned registry/liveness semantics"
    );
}

#[test]
fn live_oracle_agrees_on_the_inventory_shapes() {
    // `registered/0`, `processes/0` and `ports/0` return lists of atoms/pids/ports.
    // (`ports/0` may legitimately be empty, so its head is only inspected when the
    // list is non-empty.)
    let Some(output) = oracle_eval(
        "io:format(\"~p|~p|~p|~p~n\",[\
             erlang:is_list(erlang:registered()) andalso erlang:is_atom(hd(erlang:registered())),\
             erlang:is_list(erlang:processes()) andalso erlang:is_pid(hd(erlang:processes())),\
             erlang:is_list(erlang:ports()),\
             erlang:ports() =:= [] orelse erlang:is_port(hd(erlang:ports()))])",
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "true|true|true|true",
        "the live oracle disagrees with the pinned inventory shapes"
    );
}
