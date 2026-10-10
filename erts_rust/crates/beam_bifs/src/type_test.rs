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

//! The `erlang` `is_*` type-test (guard) BIFs.
//!
//! Ground truth (read-only): the `ubif erlang:is_*` roster in
//! `../../erts/emulator/beam/bif.tab` (lines 331–344: `is_atom`, `is_list`,
//! `is_tuple`, `is_float`, `is_integer`, `is_number`, `is_pid`, `is_port`,
//! `is_reference`, `is_binary`, `is_function/1,2`, `is_record/2,3`, plus
//! `is_record/1` at line 852) and the implementations in
//! `../../erts/emulator/beam/erl_bif_op.c` (`is_function_2` line 232 /
//! `erl_is_function` line 237, `is_record_1` line 279, `is_record_2` line 293,
//! `is_record_3` line 325). The `is_*`/1 tests return a boolean and never fail;
//! `is_function/2` and `is_record/2,3` have `badarg` cases (below).
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
//!
//! # The record tests (oracle-verified, OTP 29 / erts-17.1)
//!
//! * `is_record/1` is the **native-record** test (`RECORD_SUBTAG`), *not* a tuple
//!   test: `erlang:is_record({a,b})` is `false`.
//! * `is_record(Term, Tag)` is true iff `Tag` is an atom **and** either `Term` is a
//!   tuple of arity ≥ 1 whose first element is `Tag`, **or** `Term` is a native
//!   record whose definition `name` is `Tag` (`is_record_2`,
//!   `erl_bif_op.c:293-315`). The `arity >= 1` guard is what makes
//!   `erlang:is_record({}, a)` `false`; a non-atom `Tag` is `badarg`
//!   (`erlang:is_record({a,b}, 1)` ⇒ `error:badarg`).
//! * `is_record(Term, Tag, Size)`: for a small-integer `Size` the tuple form with
//!   the *exact* tuple arity `Size` (so `erlang:is_record({}, a, 0)` is `false`,
//!   while `erlang:is_record({a}, a, 1)` is `true`); for an atom `Size` the
//!   native-record form — `Term` is a native record whose definition module is
//!   `Tag` **and** name is `Size` (`erl_is_native_record`).
//!
//! The native-record halves read the frozen `ErtsRecordDefinition` through
//! `crate::access`'s safe wrappers over [`beam_term::record`]. Note that a direct,
//! compiled `is_record/2` call on a native record is translated *by the compiler*
//! to the tuple test alone (so it answers `false`); the BIF implemented here is the
//! `apply(erlang, is_record, [R, Name])` behaviour the oracle's BIF actually has.

use beam_common::Result;
use beam_term::{atom_val, signed_value, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error;
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

/// `erlang:is_function/2` — a fun whose arity is exactly the second argument.
///
/// `erl_is_function` (`erl_bif_op.c:237-267`): the arity argument must be a
/// non-negative small integer, or a **positive** bignum; everything else —
/// including a negative bignum, a float, an atom or a list — is `badarg`. A
/// positive bignum is a valid arity no fun can have, so it answers `false` (the
/// oracle: `erlang:is_function(fun() -> ok end, 1 bsl 64)` is `false`).
fn is_function_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let arity_term = args[1];
    if access::is_small(arity_term) {
        let arity = signed_value(arity_term);
        if arity < 0 {
            return Err(error::badarg());
        }
        // A fun's arity fits 8 bits (`erl_term.h:403`), so an arity that does not
        // fit a `u32` cannot match — and must not be truncated into one that does.
        let matches = match u32::try_from(arity) {
            Ok(wanted) => access::fun_arity(args[0]) == Some(wanted),
            Err(_) => false,
        };
        return ctx.boolean(matches);
    }
    if access::is_big(arity_term) {
        // The sign lives in the bignum header (`_BIG_SIGN_BIT`, `erl_term.h`).
        let (negative, _) = beam_codec::term::big_parts(arity_term)?;
        if negative {
            return Err(error::badarg());
        }
        return ctx.boolean(false);
    }
    Err(error::badarg())
}

/// `erlang:is_record/1` — a native record (`RECORD_SUBTAG`), not a tuple.
///
/// `is_record_1` (`erl_bif_op.c:279-285`). A native record is built by the
/// `records:create/4` family (`crate::records`); `erlang:is_record({a,b})` is
/// `false`.
fn is_record_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(access::is_record(args[0]))
}

/// `erlang:is_record/2` — `Term` is a tuple of arity ≥ 1 whose first element is the
/// atom `Tag`, or a native record whose definition name is `Tag`.
///
/// `is_record_2` (`erl_bif_op.c:293-315`). A non-atom `Tag` is `badarg`; a term
/// that is neither such a tuple nor such a native record is `false` — including the
/// empty tuple `{}`, which is why the `arity >= 1` guard is present
/// (`tuple_element(term, 0)` would otherwise read past a 0-arity tuple).
fn is_record_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (term, tag) = (args[0], args[1]);
    if !access::is_atom(tag) {
        return Err(error::badarg());
    }
    if access::is_tuple(term)
        && access::tuple_size(term) >= 1
        && access::tuple_element(term, 0) == tag
    {
        return ctx.boolean(true);
    }
    if access::is_record(term) {
        // A native record: compare the definition's `name` with `Tag`
        // (`is_record_2`, `erl_bif_op.c:306-312`). `Tag` is already an atom.
        return ctx.boolean(access::native_record_name_is(term, atom_val(tag)));
    }
    ctx.boolean(false)
}

/// `erlang:is_record/3` — `is_record/2` with an explicit size.
///
/// `is_record_3` (`erl_bif_op.c:325-350`): a non-atom `Tag` is `badarg`; an atom
/// `Size` selects the native-record form (an atom tag/name pair); a small-integer
/// `Size` selects the tuple form, which additionally requires the tuple's arity to
/// be *exactly* `Size`. Any other `Size` (a float, a bignum, a list, …) is
/// `badarg`.
fn is_record_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (term, tag, size) = (args[0], args[1], args[2]);
    if !access::is_atom(tag) {
        return Err(error::badarg());
    }
    if access::is_atom(size) {
        // The native-record form (`erl_is_native_record`): match the record's
        // definition module (`Tag`) and name (`Size`), both atoms.
        if access::is_record(term) {
            return ctx.boolean(access::native_record_is_native(
                term,
                atom_val(tag),
                atom_val(size),
            ));
        }
        return ctx.boolean(false);
    }
    if access::is_small(size) {
        let wanted = signed_value(size);
        // `wanted >= 1` first: `erlang:is_record({}, a, 0)` is `false` in the oracle,
        // and it also bounds `tuple_element(term, 0)`.
        let matches = wanted >= 1
            && access::is_tuple(term)
            && access::tuple_size(term) as i64 == wanted
            && access::tuple_element(term, 0) == tag;
        return ctx.boolean(matches);
    }
    Err(error::badarg())
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
    reg.register("erlang", "is_function", 2, is_function_2);
    reg.register("erlang", "is_record", 1, is_record_1);
    reg.register("erlang", "is_record", 2, is_record_2);
    reg.register("erlang", "is_record", 3, is_record_3);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_process::{ProcessId, PortId};
    use beam_term::{make_small, AtomTable, NIL};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a", "b"]);
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
        for (name, arity) in [
            ("is_atom", 1),
            ("is_list", 1),
            ("is_tuple", 1),
            ("is_float", 1),
            ("is_integer", 1),
            ("is_number", 1),
            ("is_pid", 1),
            ("is_port", 1),
            ("is_reference", 1),
            ("is_binary", 1),
            ("is_bitstring", 1),
            ("is_map", 1),
            ("is_boolean", 1),
            ("is_function", 1),
            ("is_function", 2),
            ("is_record", 1),
            ("is_record", 2),
            ("is_record", 3),
        ] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 18);
    }

    /// The words of a **local** fun term: `[MAKE_FUN_HEADER(arity, 0, 0), uniq, index]`.
    ///
    /// The fun layout is `[thing_word, entry, env…]` (`erl_fun.h`, see
    /// `beam_engine::terms::make_fun`) and its header (`erl_term.h:413-424`) packs
    /// the arity at bit 8 and the environment size at bit 16; `is_function/2` reads
    /// only the header. This is a `Local` block (`fun_words(FunKind::Local, 0) == 3`
    /// — no kind bit set), so the widened *external* descriptor (4 words) does not
    /// affect it.
    fn fun_words(arity: usize) -> [Eterm; 3] {
        [
            Eterm::from_raw(beam_term::FUN_SUBTAG | (arity << 8)),
            Eterm::from_raw(0), // uniq
            Eterm::from_raw(0), // index
        ]
    }

    #[test]
    fn is_function_2_checks_the_exact_arity() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let words = fun_words(2);
        let fun = beam_term::make_boxed(words.as_ptr());

        assert!(truth(
            &atoms,
            is_function_2(&mut ctx, &[fun, make_small(2)]).unwrap()
        ));
        assert!(!truth(
            &atoms,
            is_function_2(&mut ctx, &[fun, make_small(0)]).unwrap()
        ));
        assert!(!truth(
            &atoms,
            is_function_2(&mut ctx, &[fun, make_small(3)]).unwrap()
        ));
        // A non-fun is `false` for every valid arity.
        assert!(!truth(
            &atoms,
            is_function_2(&mut ctx, &[make_small(1), make_small(2)]).unwrap()
        ));
        assert!(!truth(
            &atoms,
            is_function_2(&mut ctx, &[NIL, make_small(0)]).unwrap()
        ));

        // A negative small, a float, an atom and a list arity are `badarg`.
        let float = ctx.make_float(0.0).expect("fits");
        let a = beam_term::make_atom(atoms.lookup("a").unwrap());
        for bad in [make_small(-1), float, a, NIL] {
            assert!(
                matches!(
                    is_function_2(&mut ctx, &[fun, bad]),
                    Err(beam_common::BeamError::BadArg)
                ),
                "is_function(Fun, {bad:?}) must be badarg"
            );
        }
    }

    #[test]
    fn is_record_is_a_tuple_tag_test_not_a_native_record_test() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let a = beam_term::make_atom(atoms.lookup("a").unwrap());
        let b = beam_term::make_atom(atoms.lookup("b").unwrap());
        let record = ctx.tuple(&[a, make_small(1)]).expect("fits"); // {a, 1}
        let one = ctx.tuple(&[a]).expect("fits"); // {a}
        let empty = ctx.tuple(&[]).expect("fits"); // {}

        // `is_record/1` is the native-record test: a tuple is *not* a record.
        assert!(!truth(&atoms, is_record_1(&mut ctx, &[record]).unwrap()));
        assert!(!truth(&atoms, is_record_1(&mut ctx, &[a]).unwrap()));

        // `is_record/2`: arity >= 1 and `element(1) =:= Tag`.
        assert!(truth(&atoms, is_record_2(&mut ctx, &[record, a]).unwrap()));
        assert!(truth(&atoms, is_record_2(&mut ctx, &[one, a]).unwrap()));
        assert!(!truth(&atoms, is_record_2(&mut ctx, &[record, b]).unwrap()));
        // The empty tuple has no first element (`erlang:is_record({}, a) = false`).
        assert!(!truth(&atoms, is_record_2(&mut ctx, &[empty, a]).unwrap()));
        assert!(!truth(
            &atoms,
            is_record_2(&mut ctx, &[make_small(1), a]).unwrap()
        ));
        // A non-atom tag is `badarg`.
        assert!(matches!(
            is_record_2(&mut ctx, &[record, make_small(1)]),
            Err(beam_common::BeamError::BadArg)
        ));

        // `is_record/3`: exact size for a small-integer size.
        assert!(truth(
            &atoms,
            is_record_3(&mut ctx, &[record, a, make_small(2)]).unwrap()
        ));
        assert!(!truth(
            &atoms,
            is_record_3(&mut ctx, &[record, a, make_small(1)]).unwrap()
        ));
        assert!(!truth(
            &atoms,
            is_record_3(&mut ctx, &[empty, a, make_small(0)]).unwrap()
        ));
        // An atom size is the native-record form: a tuple is `false`.
        assert!(!truth(
            &atoms,
            is_record_3(&mut ctx, &[record, a, a]).unwrap()
        ));
        // A non-atom tag, and a size that is neither a small nor an atom, are `badarg`.
        assert!(matches!(
            is_record_3(&mut ctx, &[record, make_small(1), make_small(2)]),
            Err(beam_common::BeamError::BadArg)
        ));
        let float = ctx.make_float(1.0).expect("fits");
        assert!(matches!(
            is_record_3(&mut ctx, &[record, a, float]),
            Err(beam_common::BeamError::BadArg)
        ));
    }
}
