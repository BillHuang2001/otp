//! Oracle-pinned tests for the runtime process/name/global-state query BIFs
//! (`src/proc_reg.rs`): `erlang:whereis/1`, `register/2`, `unregister/1`,
//! `registered/0`, `is_process_alive/1`, `processes/0` and `ports/0`.
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
    atoms.seed(["true", "false", "ok", "undefined", "init", "a", "b", "missing"]);
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

/// A `ProcessHook` test double: a name registry, a set of live pids, and the
/// process/port inventories — the state a real `beam_rt` provider holds.
#[derive(Default)]
struct StubProcesses {
    registry: Vec<(Atom, Eterm)>,
    alive: Vec<ProcessId>,
    pids: Vec<ProcessId>,
    ports: Vec<PortId>,
}

impl StubProcesses {
    /// Whether `id` is already registered under some name.
    fn is_named(&self, id: Eterm) -> bool {
        self.registry.iter().any(|&(_, stored)| stored == id)
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
}

// ---------------------------------------------------------------------------
// (a) No hook: every key is the typed capability gap
// ---------------------------------------------------------------------------

#[test]
fn every_query_is_unsupported_without_a_hook() {
    // A fabricated `[]`, `undefined` or `false` would be exactly the anti-pattern
    // the crate forbids; every one of the seven keys must instead report the gap.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let init = atom_term(&atoms, "init");
    let pid = ProcessId::new(7, 0).to_eterm();

    assert_unsupported(call(&reg, &mut ctx, "erlang", "whereis", 1, &[init]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "register", 2, &[init, pid]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "unregister", 1, &[init]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "registered", 0, &[]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "is_process_alive", 1, &[pid]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "processes", 0, &[]));
    assert_unsupported(call(&reg, &mut ctx, "erlang", "ports", 0, &[]));
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

#[test]
fn register_installs_the_seven_keys() {
    let reg = default_registry();
    assert!(reg.contains("erlang", "whereis", 1));
    assert!(reg.contains("erlang", "register", 2));
    assert!(reg.contains("erlang", "unregister", 1));
    assert!(reg.contains("erlang", "registered", 0));
    assert!(reg.contains("erlang", "is_process_alive", 1));
    assert!(reg.contains("erlang", "processes", 0));
    assert!(reg.contains("erlang", "ports", 0));
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
