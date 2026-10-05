//! The `erlang` `is_*` type-test (guard) BIFs.
//!
//! Ground truth (read-only): the `ubif erlang:is_*` roster in
//! `../../erts/emulator/beam/bif.tab` (lines 331–344: `is_atom`, `is_list`,
//! `is_tuple`, `is_float`, `is_integer`, `is_number`, `is_pid`, `is_port`,
//! `is_reference`, `is_binary`, `is_function`) plus `is_bitstring`/`is_map`/
//! `is_boolean`, and the implementations in
//! `../../erts/emulator/beam/erl_bif_guard.c`. Every BIF here has arity 1 and
//! returns a boolean.
//!
//! # Two rules the naive wrappers get wrong
//!
//! * **`is_list([])` is `true`.** `beam_term::is_list` is the *cons-cell*
//!   predicate (`[]` is `NIL`, not a cons cell), so this module tests
//!   `is_nil(t) || is_list(t)` — exactly the C guard.
//! * **`is_boolean/1` is true only for the atoms `true` and `false`.** Every other
//!   atom (including a table-missing name) is `false`.
//!
//! `is_binary/1` is a subset of `is_bitstring/1` (a bitstring whose size is a
//! whole number of bytes), so `is_bitstring` delegates to the `beam_term`
//! predicate directly and `is_binary` likewise.

use beam_common::Result;
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::registry::BifRegistry;

/// `erlang:is_atom/1`.
fn is_atom(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_atom(args[0]))
}

/// `erlang:is_list/1` — a cons cell *or* `[]`.
fn is_list(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let t = args[0];
    ctx.boolean(access::is_nil(t) || access::is_list(t))
}

/// `erlang:is_tuple/1`.
fn is_tuple(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_tuple(args[0]))
}

/// `erlang:is_float/1`.
fn is_float(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_float(args[0]))
}

/// `erlang:is_integer/1`.
fn is_integer(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_integer(args[0]))
}

/// `erlang:is_number/1`.
fn is_number(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_number(args[0]))
}

/// `erlang:is_pid/1`.
fn is_pid(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_pid(args[0]))
}

/// `erlang:is_port/1`.
fn is_port(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_port(args[0]))
}

/// `erlang:is_reference/1`.
fn is_reference(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_reference(args[0]))
}

/// `erlang:is_binary/1`.
fn is_binary(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_binary(args[0]))
}

/// `erlang:is_bitstring/1`.
fn is_bitstring(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_bitstring(args[0]))
}

/// `erlang:is_map/1`.
fn is_map(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_map(args[0]))
}

/// `erlang:is_boolean/1` — the atoms `true` and `false` only.
fn is_boolean(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let t = args[0];
    let boolean = access::is_atom(t)
        && matches!(ctx.atoms().name(atom_val(t)), Some("true") | Some("false"));
    ctx.boolean(boolean)
}

/// `erlang:is_function/1`.
fn is_function(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_function(args[0]))
}

/// Registers the `erlang` type-test BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "is_atom", 1, is_atom);
    reg.register("erlang", "is_list", 1, is_list);
    reg.register("erlang", "is_tuple", 1, is_tuple);
    reg.register("erlang", "is_float", 1, is_float);
    reg.register("erlang", "is_integer", 1, is_integer);
    reg.register("erlang", "is_number", 1, is_number);
    reg.register("erlang", "is_pid", 1, is_pid);
    reg.register("erlang", "is_port", 1, is_port);
    reg.register("erlang", "is_reference", 1, is_reference);
    reg.register("erlang", "is_binary", 1, is_binary);
    reg.register("erlang", "is_bitstring", 1, is_bitstring);
    reg.register("erlang", "is_map", 1, is_map);
    reg.register("erlang", "is_boolean", 1, is_boolean);
    reg.register("erlang", "is_function", 1, is_function);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_process::{ProcessId, PortId};
    use beam_term::{make_small, AtomTable, NIL};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a"]);
        atoms
    }

    /// The boolean value of a `true`/`false` atom term (resolved via `atoms`).
    fn truth(atoms: &AtomTable, t: Eterm) -> bool {
        match atoms.name(atom_val(t)) {
            Some("true") => true,
            Some("false") => false,
            other => panic!("expected a boolean atom, got {other:?}"),
        }
    }

    /// Runs `f` over a fresh heap/atom table and returns the resulting boolean.
    fn check(f: fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>, arg: Eterm) -> bool {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let t = f(&mut ctx, &[arg]).expect("a type test never fails");
        truth(&atoms, t)
    }

    #[test]
    fn integers_and_floats() {
        assert!(check(is_integer, make_small(1)));
        assert!(!check(is_integer, NIL));
        assert!(!check(is_float, make_small(1)));

        let mut heap = Heap::new(16).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let float = ctx.make_float(1.0).expect("fits");
        assert!(truth(&atoms, is_float(&mut ctx, &[float]).unwrap()));
        assert!(!truth(&atoms, is_integer(&mut ctx, &[float]).unwrap()));
    }

    #[test]
    fn is_list_accepts_nil_and_cons() {
        // `[]` is a list even though `beam_term::is_list(NIL)` is false.
        assert!(check(is_list, NIL));
        let mut heap = Heap::new(8).expect("test heap");
        let cons = heap.cons(make_small(1), NIL).expect("fits");
        assert!(check(is_list, cons));
        assert!(!check(is_list, make_small(1)));
    }

    #[test]
    fn atoms_tuples_and_booleans() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = atoms();
        let a = beam_term::make_atom(atoms.lookup("a").unwrap());
        let tup = heap.tuple(&[a]).expect("fits");
        let t_true = beam_term::make_atom(atoms.lookup("true").unwrap());

        assert!(check(is_atom, a));
        assert!(!check(is_atom, make_small(1)));
        assert!(check(is_tuple, tup));
        assert!(!check(is_tuple, a));
        assert!(check(is_boolean, t_true));
        assert!(!check(is_boolean, a));
        assert!(!check(is_boolean, make_small(1)));
    }

    #[test]
    fn numbers_pids_ports_refs_funs_and_binaries() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let float = ctx.make_float(1.0).expect("fits");
        let pid = ctx.self_pid().to_eterm();
        let port = PortId::new(3).to_eterm();

        assert!(access::is_float(float));
        assert!(check(is_float, float));
        assert!(check(is_number, float));
        assert!(check(is_number, make_small(1)));
        assert!(!check(is_number, NIL));

        assert!(check(is_pid, pid));
        assert!(!check(is_pid, make_small(1)));
        assert!(check(is_port, port));
        assert!(!check(is_port, NIL));

        // A self-pid is neither a reference nor a function; `[]` likewise.
        assert!(!check(is_reference, pid));
        assert!(!check(is_function, pid));
        // No binary/bitstring/map is constructible in M1; the BIFs are total.
        assert!(!check(is_binary, NIL));
        assert!(!check(is_bitstring, NIL));
        assert!(!check(is_map, NIL));
    }

    #[test]
    fn register_installs_every_type_test() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for name in [
            "is_atom",
            "is_list",
            "is_tuple",
            "is_float",
            "is_integer",
            "is_number",
            "is_pid",
            "is_port",
            "is_reference",
            "is_binary",
            "is_bitstring",
            "is_map",
            "is_boolean",
            "is_function",
        ] {
            assert!(reg.contains("erlang", name, 1), "missing erlang:{name}/1");
        }
        assert_eq!(reg.len(), 14);
    }
}
