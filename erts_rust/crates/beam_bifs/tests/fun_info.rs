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

//! Integration tests for the fun-introspection BIFs `erlang:fun_info/2`,
//! `erlang:fun_info_mfa/1`, `erlang:fun_to_list/1` and `erlang:make_fun/3`,
//! driven through [`BifRegistry::call`](beam_bifs::BifRegistry::call) exactly as
//! `beam_rt` does.
//!
//! Every expected value is pinned against the **system oracle** (OTP 29.1.1 /
//! erts-17.1). The probes used were:
//!
//! ```text
//! $ erl -noshell -eval 'io:format("~p~n",[erlang:fun_info(fun lists:reverse/1, type)]).'
//! ```
//!
//! and an `escript` over a *compiled* probe module (an `erl_eval` lambda from
//! `-eval` is a template closure, not a `make_fun3` local fun, so it cannot be
//! used to pin the local-fun items):
//!
//! ```erlang
//! %% fiprobe.erl
//! -module(fiprobe).
//! -export([mk/0, mk/1]).
//! mk()    -> fun(X) -> X end.
//! mk(N)   -> fun(X) -> X + N end.
//! ```
//!
//! giving (`erlang:fun_info(F, I)` for each `I`):
//!
//! ```text
//! external `fun lists:reverse/1`:
//!   type={type,external} module={module,lists} arity={arity,1} name={name,reverse}
//!   env={env,[]} new_index={new_index,undefined} new_uniq={new_uniq,undefined}
//!   index={index,undefined} uniq={uniq,undefined} refc={refc,undefined}
//!   function/md5/nargs/foo/id/calls => error:badarg
//!   fun_info_mfa => {lists,reverse,1}   fun_to_list => "fun lists:reverse/1"
//! local `fiprobe:mk(7)`:
//!   type={type,local} module={module,fiprobe} arity={arity,1}
//!   name={name,'-mk/1-fun-0-'} env={env,[7]} refc={refc,1}
//!   new_index={new_index,1} index={index,1} uniq={uniq,85002344}
//!   new_uniq={new_uniq,<<…16 bytes…>>}
//!   fun_info_mfa => {fiprobe,'-mk/1-fun-0-',1}
//!   fun_to_list => "#Fun<fiprobe.1.85002344>"
//! ```
//!
//! The local-fun identity items are an **honest typed gap** in this runtime (the
//! inlined descriptor carries no module/MD5/`old_index`) — see
//! `src/fun_info.rs`'s module docs — so they are asserted as
//! `error:unsupported_bif` here, never as a fabricated value.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::fun::{FunKind, FunPid};
use beam_term::{atom_val, make_small, signed_value, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "undefined",
        "external",
        "local",
        "lists",
        "reverse",
        "fiprobe",
        "'-mk/1-fun-0-'",
        "nonode@nohost",
        "foo",
        "type",
        "module",
        "function",
        "new_index",
        "new_uniq",
        "index",
        "uniq",
        "refc",
        "env",
        "arity",
        "name",
        "md5",
        "nargs",
        "id",
        "calls",
        // The empty atom `''` is a valid (non-item) atom name.
        "",
    ]);
    atoms
}

fn make_ctx<'a>(heap: &'a mut Heap, atoms: &'a AtomTable) -> BifContext<'a> {
    BifContext::new(heap, atoms, ProcessId::new(1, 0))
}

/// The name of an atom term, resolved through `atoms`.
fn atom_name<'a>(atoms: &'a AtomTable, term: Eterm) -> &'a str {
    atoms
        .name(atom_val(term))
        .unwrap_or_else(|| panic!("term {term:?} is not a seeded atom"))
}

/// The `i`-th element of a tuple, via the registered `erlang:element/2` BIF.
fn elem(reg: &BifRegistry, ctx: &mut BifContext<'_>, tuple: Eterm, i: i64) -> Eterm {
    reg.call("erlang", "element", 2, ctx, &[make_small(i), tuple])
        .expect("element/2")
}

/// Asserts the term is a tuple `{atom, value}` whose tag is `tag`, and returns
/// its value element. `fun_info/2` must return exactly this shape.
fn pair_value(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    term: Eterm,
    tag: &str,
) -> Eterm {
    assert_eq!(
        reg.call("erlang", "tuple_size", 1, ctx, &[term])
            .expect("tuple_size/1"),
        make_small(2),
        "fun_info/2 must return a 2-tuple"
    );
    let item = elem(reg, ctx, term, 1);
    assert_eq!(atom_name(atoms, item), tag, "the item element");
    elem(reg, ctx, term, 2)
}

fn call(reg: &BifRegistry, ctx: &mut BifContext<'_>, name: &str, args: &[Eterm]) -> Result<Eterm> {
    let arity = args.len() as u32;
    reg.call("erlang", name, arity, ctx, args)
}

/// Structural term equality: `assert_eq!` on a boxed `Eterm` compares by
/// *address*, so two separately built but identical terms (e.g. two
/// `{lists,reverse,1}` tuples) are never equal that way. This is the oracle's
/// `=:=` (`beam_term::compare::eq`).
fn assert_eq_term(atoms: &AtomTable, got: Eterm, expected: Eterm, what: &str) {
    assert!(
        beam_term::compare::eq(got, expected, atoms),
        "{what}: the returned term is not the expected one"
    );
}

/// Asserts the call failed with the crate's typed `unsupported_bif` raise (the
/// documented gap for the local-fun identity items), never a wrong value and
/// never some other failure. `BeamError` has no `Unsupported` variant: the
/// typed gap is a `Raise` whose reason atom is `unsupported_bif`.
fn assert_unsupported_bif(result: Result<Eterm>, what: &str) {
    match result {
        Err(ref err) if err.as_atom() == Some("unsupported_bif") => {}
        other => panic!("{what}: expected unsupported_bif, got {other:?}"),
    }
}

/// `fun_info(F, Item)` with the item interned from `item`.
fn fun_info(reg: &BifRegistry, ctx: &mut BifContext<'_>, fun: Eterm, item: &str) -> Result<Eterm> {
    let item = ctx.atom(item).expect("seeded item atom");
    call(reg, ctx, "fun_info", &[fun, item])
}

// ---------------------------------------------------------------------------
// Term fixtures
// ---------------------------------------------------------------------------

/// An `EXPORT_EXT` (external) fun term on `heap`.
fn external_fun(ctx: &mut BifContext<'_>, arity: usize, module: &str, function: &str) -> Eterm {
    let module = ctx.atom(module).expect("seeded module");
    let function = ctx.atom(function).expect("seeded function");
    beam_codec::term::make_fun_external(ctx.heap(), arity, atom_val(module), atom_val(function))
        .expect("the external fun fits")
}

/// A `make_fun3` (`FunKind::Local`) fun term plus the storage owning its words.
///
/// The frozen layout is `[header, uniq, index, env…]` with
/// `header = FUN_SUBTAG | arity << 8 | num_free << 16` (`beam_term::fun`,
/// `beam_engine::terms::fun`).
fn local_fun(arity: u32, uniq: u64, index: u32, env: &[Eterm]) -> (Box<[Eterm]>, Eterm) {
    let header = beam_term::FUN_SUBTAG
        | ((arity as usize) << 8)
        | ((env.len() as usize) << beam_term::fun::FUN_HEADER_ENV_SIZE_OFFS);
    let mut words: Vec<Eterm> = vec![
        Eterm::from_raw(header),
        Eterm::from_raw(uniq as usize),
        Eterm::from_raw(index as usize),
    ];
    words.extend_from_slice(env);
    let words = words.into_boxed_slice();
    let term = beam_term::make_boxed(words.as_ptr());
    (words, term)
}

/// A `NEW_FUN_EXT` (`FunKind::Imported`) fun term on the context heap.
fn imported_fun(
    ctx: &mut BifContext<'_>,
    arity: usize,
    module: &str,
    uniq: &[u8; 16],
    index: u32,
    old_index: u32,
    old_uniq: u32,
    env: &[Eterm],
) -> Eterm {
    let module = ctx.atom(module).expect("seeded module");
    let creator = FunPid {
        node: atom_val(ctx.atom("nonode@nohost").expect("seeded node")),
        number: 1,
        serial: 2,
        creation: 3,
    };
    beam_codec::term::make_fun_imported(
        ctx.heap(),
        arity,
        atom_val(module),
        uniq,
        index,
        old_index,
        old_uniq,
        creator,
        env,
    )
    .expect("the imported fun fits")
}

// ---------------------------------------------------------------------------
// fun_info/2 — external
// ---------------------------------------------------------------------------

#[test]
fn fun_info_2_external_items_match_the_oracle() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let fun = external_fun(&mut ctx, 1, "lists", "reverse");

    // Oracle: `fun_info(fun lists:reverse/1, type)` = {type,external}.
    let info = fun_info(&reg, &mut ctx, fun, "type").expect("type");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "type");
    assert_eq!(atom_name(&atoms, value), "external");

    let info = fun_info(&reg, &mut ctx, fun, "module").expect("module");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "module");
    assert_eq!(atom_name(&atoms, value), "lists");

    let info = fun_info(&reg, &mut ctx, fun, "name").expect("name");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "name");
    assert_eq!(atom_name(&atoms, value), "reverse");

    let info = fun_info(&reg, &mut ctx, fun, "arity").expect("arity");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "arity");
    assert_eq!(signed_value(value), 1);

    // `{env,[]}`.
    let info = fun_info(&reg, &mut ctx, fun, "env").expect("env");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "env");
    assert_eq!(value, NIL, "an external fun has no environment");

    // `{refc,undefined}` and the four never-set identity items, all `undefined`.
    for item in ["refc", "new_index", "new_uniq", "index", "uniq"] {
        let info = fun_info(&reg, &mut ctx, fun, item).expect(item);
        let value = pair_value(&reg, &mut ctx, &atoms, info, item);
        assert_eq!(
            atom_name(&atoms, value),
            "undefined",
            "{item} of an external fun"
        );
    }
}

// ---------------------------------------------------------------------------
// fun_info/2 — local / imported
// ---------------------------------------------------------------------------

#[test]
fn fun_info_2_local_kind_independent_items_match_the_oracle() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let seven = ctx.int(7).expect("7 fits");
    let (_storage, fun) = local_fun(1, 85002344, 1, &[seven]);

    // Oracle (fiprobe:mk(7)): {type,local}, {arity,1}, {env,[7]}, {refc,1}.
    let info = fun_info(&reg, &mut ctx, fun, "type").expect("type");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "type");
    assert_eq!(atom_name(&atoms, value), "local");

    let info = fun_info(&reg, &mut ctx, fun, "arity").expect("arity");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "arity");
    assert_eq!(signed_value(value), 1);

    let info = fun_info(&reg, &mut ctx, fun, "refc").expect("refc");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "refc");
    assert_eq!(signed_value(value), 1);

    let info = fun_info(&reg, &mut ctx, fun, "env").expect("env");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "env");
    assert_eq!(
        reg.call("erlang", "length", 1, &mut ctx, &[value])
            .expect("length/1"),
        make_small(1)
    );
    // The environment is the *list* `[7]`, so its single free variable is the
    // list's head.
    let head = reg
        .call("erlang", "hd", 1, &mut ctx, &[value])
        .expect("hd/1 of the env list");
    assert_eq!(signed_value(head), 7);

    // A closure with no free variables reports `{env,[]}` (fiprobe:mk/0).
    let (_storage0, fun0) = local_fun(1, 85002344, 0, &[]);
    let info = fun_info(&reg, &mut ctx, fun0, "env").expect("env");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "env");
    assert_eq!(value, NIL);
}

#[test]
fn fun_info_2_local_identity_items_are_the_documented_typed_gap() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let (_storage, fun) = local_fun(1, 85002344, 1, &[]);

    // The oracle answers `{module,fiprobe}`, `{name,'-mk/1-fun-0-'}`,
    // `{new_index,1}`, `{new_uniq,<<…>>}`, `{index,1}` and `{uniq,85002344}`; the
    // M1 inlined descriptor carries none of those `ErlFunEntry` fields, so the
    // honest answer is a typed gap — never a fabricated module/MD5/index.
    for item in ["module", "name", "new_index", "new_uniq", "index", "uniq"] {
        assert_unsupported_bif(
            fun_info(&reg, &mut ctx, fun, item),
            &format!("fun_info(LocalFun, {item})"),
        );
    }
}

#[test]
fn fun_info_2_imported_fun_is_a_local_fun_with_the_identity_items_gapped() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let uniq = [1u8; 16];
    let fun = imported_fun(&mut ctx, 1, "fiprobe", &uniq, 1, 1, 85002344, &[]);

    // A `NEW_FUN_EXT`-decoded fun is a *local* fun in the C
    // (`MAKE_FUN_HEADER(arity, num_free, 0)`, `external.c:5127`), so the oracle
    // reports `{type,local}` and `{refc,1}` for it.
    let info = fun_info(&reg, &mut ctx, fun, "type").expect("type");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "type");
    assert_eq!(atom_name(&atoms, value), "local");
    let info = fun_info(&reg, &mut ctx, fun, "refc").expect("refc");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "refc");
    assert_eq!(signed_value(value), 1);
    let info = fun_info(&reg, &mut ctx, fun, "arity").expect("arity");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "arity");
    assert_eq!(signed_value(value), 1);

    for item in ["module", "name", "new_index", "new_uniq", "index", "uniq"] {
        assert_unsupported_bif(
            fun_info(&reg, &mut ctx, fun, item),
            &format!("fun_info(ImportedFun, {item})"),
        );
    }
}

// ---------------------------------------------------------------------------
// fun_info/2 — badarg forms
// ---------------------------------------------------------------------------

#[test]
fn fun_info_2_unknown_items_and_non_atoms_are_badarg() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let fun = external_fun(&mut ctx, 1, "lists", "reverse");

    // Oracle: `function` is *not* a `fun_info` item (`badarg`), and neither are
    // the other OTP-30/`~p` spellings, nor the empty atom `''` (probed with
    // `erl -noshell -eval 'erlang:fun_info(fun lists:reverse/1, '')'` ⇒
    // `error:badarg`). Every name below is seeded, so each resolves to a real
    // atom term.
    for item in ["function", "md5", "nargs", "foo", "id", "calls", ""] {
        let item_term = ctx.atom(item).expect("seeded item atom");
        let result = call(&reg, &mut ctx, "fun_info", &[fun, item_term]);
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "fun_info(F, {item}) must be badarg"
        );
    }

    // A non-atom item: a list, a tuple, an integer, a binary-shaped NIL.
    let list = ctx.list(&[make_small(1)]).expect("fits");
    let tuple = ctx.tuple(&[make_small(1)]).expect("fits");
    let float = ctx.make_float(1.0).expect("fits");
    for item in [list, tuple, NIL, make_small(1), float] {
        assert!(
            matches!(
                call(&reg, &mut ctx, "fun_info", &[fun, item]),
                Err(BeamError::BadArg)
            ),
            "fun_info(F, {item:?}) must be badarg"
        );
    }
}

#[test]
fn fun_info_2_a_non_fun_is_badarg() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let item = ctx.atom("type").expect("seeded");
    let tuple = ctx.tuple(&[item]).expect("fits");

    for non_fun in [make_small(42), NIL, item, tuple] {
        assert!(
            matches!(
                call(&reg, &mut ctx, "fun_info", &[non_fun, item]),
                Err(BeamError::BadArg)
            ),
            "fun_info({non_fun:?}, type) must be badarg"
        );
    }
}

#[test]
fn fun_info_2_reuses_the_item_argument_as_element_one() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let fun = external_fun(&mut ctx, 1, "lists", "reverse");
    let item = ctx.atom("type").expect("seeded");

    let result = call(&reg, &mut ctx, "fun_info", &[fun, item]).expect("fun_info/2");
    // The C builds `TUPLE2(hp, what, val)` — element 1 **is** the argument term.
    assert_eq!(elem(&reg, &mut ctx, result, 1), item);
}

// ---------------------------------------------------------------------------
// fun_info_mfa/1
// ---------------------------------------------------------------------------

#[test]
fn fun_info_mfa_1_matches_the_oracle() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);

    // Oracle: `fun_info_mfa(fun lists:reverse/1)` = {lists,reverse,1}.
    let fun = external_fun(&mut ctx, 1, "lists", "reverse");
    let result = call(&reg, &mut ctx, "fun_info_mfa", &[fun]).expect("mfa");
    assert_eq!(
        reg.call("erlang", "tuple_size", 1, &mut ctx, &[result])
            .expect("tuple_size/1"),
        make_small(3)
    );
    assert_eq!(atom_name(&atoms, elem(&reg, &mut ctx, result, 1)), "lists");
    assert_eq!(
        atom_name(&atoms, elem(&reg, &mut ctx, result, 2)),
        "reverse"
    );
    assert_eq!(signed_value(elem(&reg, &mut ctx, result, 3)), 1);

    // A `make_fun/3` external fun is the same term (oracle: `=:=` is true).
    let (m, f) = (
        ctx.atom("lists").expect("seeded"),
        ctx.atom("reverse").expect("seeded"),
    );
    let made = call(&reg, &mut ctx, "make_fun", &[m, f, make_small(1)]).expect("make_fun/3");
    // ... whose `fun_info_mfa/1` is the same `{lists,reverse,1}` (oracle: `=:=`).
    let made_mfa = call(&reg, &mut ctx, "fun_info_mfa", &[made]).expect("mfa");
    assert_eq_term(
        &atoms,
        made_mfa,
        result,
        "fun_info_mfa/1 of a make_fun/3 fun",
    );

    // A non-fun is `badarg`; a local fun is the documented typed gap.
    for non_fun in [make_small(42), NIL] {
        assert!(matches!(
            call(&reg, &mut ctx, "fun_info_mfa", &[non_fun]),
            Err(BeamError::BadArg)
        ));
    }
    let (_storage, local) = local_fun(1, 85002344, 1, &[]);
    assert_unsupported_bif(
        call(&reg, &mut ctx, "fun_info_mfa", &[local]),
        "fun_info_mfa/1 of a local fun",
    );
}

// ---------------------------------------------------------------------------
// fun_to_list/1
// ---------------------------------------------------------------------------

/// Renders `term` as a Rust `String` through the `erlang:list_to_binary/1`
/// registry path? No — through `lists:flatten`-free byte walking: this helper
/// walks the proper list of small integers directly.
fn list_to_string(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> String {
    let mut out = String::new();
    let mut cur = term;
    loop {
        if cur == NIL {
            return out;
        }
        let head = reg
            .call("erlang", "hd", 1, ctx, &[cur])
            .expect("hd/1 of a proper list");
        let value = signed_value(head);
        assert!((0..=255).contains(&value), "a byte-sized element");
        out.push(value as u8 as char);
        cur = reg.call("erlang", "tl", 1, ctx, &[cur]).expect("tl/1");
    }
}

#[test]
fn fun_to_list_1_external_matches_the_oracle() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let fun = external_fun(&mut ctx, 1, "lists", "reverse");

    // Oracle: erlang:fun_to_list(fun lists:reverse/1) = "fun lists:reverse/1".
    let text = call(&reg, &mut ctx, "fun_to_list", &[fun]).expect("fun_to_list");
    assert_eq!(list_to_string(&reg, &mut ctx, text), "fun lists:reverse/1");

    // ... and an arity-0 fun (oracle: make_fun(lists,reverse,0) ⇒
    // "fun lists:reverse/0").
    let (m, f) = (
        ctx.atom("lists").expect("seeded"),
        ctx.atom("reverse").expect("seeded"),
    );
    let zero = call(&reg, &mut ctx, "make_fun", &[m, f, make_small(0)]).expect("make_fun/3");
    let text = call(&reg, &mut ctx, "fun_to_list", &[zero]).expect("fun_to_list");
    assert_eq!(list_to_string(&reg, &mut ctx, text), "fun lists:reverse/0");
}

#[test]
fn fun_to_list_1_quotes_atoms_like_the_c_printer() {
    let reg = default_registry();
    let mut atoms = seeded_atoms();
    atoms.seed(["a-esc", "with@at", "'a\\'b'"]);
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);

    // The C `FUN_DEF` printer uses `print_atom_name`, whose `IS_ALNUM` excludes
    // `@` — so `nonode@nohost` is quoted here even though `io_lib:format("~p",…)`
    // prints it bare. Oracle-verified:
    //   erlang:fun_to_list(erlang:make_fun(nonode@nohost, foo, 1))
    //     => "fun 'nonode@nohost':foo/1"
    //   erlang:fun_to_list(erlang:make_fun('a-esc','b-esc',1))
    //     => "fun 'a-esc':'b-esc'/1"
    let cases = [
        ("nonode@nohost", "foo", "fun 'nonode@nohost':foo/1"),
        ("lists", "a-esc", "fun lists:'a-esc'/1"),
        ("with@at", "reverse", "fun 'with@at':reverse/1"),
    ];
    for (module, function, expected) in cases {
        let fun = external_fun(&mut ctx, 1, module, function);
        let text = call(&reg, &mut ctx, "fun_to_list", &[fun]).expect("fun_to_list");
        assert_eq!(list_to_string(&reg, &mut ctx, text), expected);
    }
}

#[test]
fn fun_to_list_1_gaps_a_local_fun_and_badargs_a_non_fun() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);

    // The oracle renders a local fun as `#Fun<Module.OldIndex.OldUniq>` (e.g.
    // "#Fun<fiprobe.1.85002344>") — a triple the inlined descriptor cannot
    // supply, so this is the documented typed gap rather than a wrong string.
    let (_storage, local) = local_fun(1, 85002344, 1, &[]);
    assert_unsupported_bif(
        call(&reg, &mut ctx, "fun_to_list", &[local]),
        "fun_to_list/1 of a local fun",
    );

    for non_fun in [make_small(42), NIL] {
        assert!(matches!(
            call(&reg, &mut ctx, "fun_to_list", &[non_fun]),
            Err(BeamError::BadArg)
        ));
    }
}

// ---------------------------------------------------------------------------
// make_fun/3
// ---------------------------------------------------------------------------

#[test]
fn make_fun_3_matches_the_oracle() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let lists = ctx.atom("lists").expect("seeded");
    let reverse = ctx.atom("reverse").expect("seeded");

    let made =
        call(&reg, &mut ctx, "make_fun", &[lists, reverse, make_small(1)]).expect("make_fun");

    // Oracle: `erlang:make_fun(lists,reverse,1)` =:= `fun lists:reverse/1` — in
    // this layout both are the same 3-word `EXPORT_EXT` block. (`assert_eq!` on a
    // boxed `Eterm` compares *addresses*, so the comparison is structural, i.e.
    // the oracle's `=:=`.)
    let literal = external_fun(&mut ctx, 1, "lists", "reverse");
    assert_eq_term(&atoms, made, literal, "make_fun/3 vs the export lambda");

    // Oracle: `{type,external}`, `{name,reverse}`, `{module,lists}`, `{arity,1}`.
    let info = fun_info(&reg, &mut ctx, made, "type").expect("type");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "type");
    assert_eq!(atom_name(&atoms, value), "external");
    let info = fun_info(&reg, &mut ctx, made, "name").expect("name");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "name");
    assert_eq!(atom_name(&atoms, value), "reverse");

    // Oracle: the maximum arity 255 is accepted, 256 is `badarg` (`bif.c:4486`).
    let max = call(
        &reg,
        &mut ctx,
        "make_fun",
        &[lists, reverse, make_small(255)],
    )
    .expect("255 is a valid arity");
    let info = fun_info(&reg, &mut ctx, max, "arity").expect("arity");
    let value = pair_value(&reg, &mut ctx, &atoms, info, "arity");
    assert_eq!(signed_value(value), 255);
    assert!(matches!(
        call(
            &reg,
            &mut ctx,
            "make_fun",
            &[lists, reverse, make_small(256)]
        ),
        Err(BeamError::BadArg)
    ));

    // Oracle: no existence check — `make_fun(no_such_mod, no_such_fun, 1)`
    // succeeds and reports `{module,no_such_mod}` / `{no_such_mod,…}`.
    let mut atoms2 = atoms;
    atoms2.seed(["no_such_mod", "no_such_fun"]);
    let mut heap2 = Heap::new(4096).expect("test heap");
    let mut ctx2 = make_ctx(&mut heap2, &atoms2);
    let (m2, f2) = (
        ctx2.atom("no_such_mod").expect("seeded"),
        ctx2.atom("no_such_fun").expect("seeded"),
    );
    let made =
        call(&reg, &mut ctx2, "make_fun", &[m2, f2, make_small(1)]).expect("no existence check");
    let info = fun_info(&reg, &mut ctx2, made, "module").expect("module");
    let value = pair_value(&reg, &mut ctx2, &atoms2, info, "module");
    assert_eq!(atom_name(&atoms2, value), "no_such_mod");
}

#[test]
fn make_fun_3_badargs() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);
    let lists = ctx.atom("lists").expect("seeded");
    let reverse = ctx.atom("reverse").expect("seeded");

    // A non-atom module or function, a non-small arity, and an out-of-range
    // arity are all `badarg` (`bif.c:4479-4488`).
    let one = make_small(1);
    let float = ctx.make_float(1.0).expect("fits");
    let list = ctx.list(&[make_small(49)]).expect("fits");
    let bads: [(Eterm, Eterm, Eterm); 8] = [
        (one, reverse, one),     // make_fun(1, reverse, 1)
        (lists, one, one),       // make_fun(lists, 1, 1)
        (lists, reverse, float), // make_fun(lists, reverse, 1.0)
        (lists, reverse, list),  // make_fun(lists, reverse, "1")
        (lists, reverse, NIL),   // make_fun(lists, reverse, [])
        (lists, reverse, make_small(-1)),
        (lists, reverse, make_small(256)),
        (lists, reverse, make_small(1_000_000)),
    ];
    for (module, function, arity) in bads {
        assert!(
            matches!(
                call(&reg, &mut ctx, "make_fun", &[module, function, arity]),
                Err(BeamError::BadArg)
            ),
            "make_fun({module:?}, {function:?}, {arity:?}) must be badarg"
        );
    }

    // A *negative bignum* arity is likewise `badarg` (not a small, so the C's
    // `is_not_small` guard fires first).
    let big = beam_codec::term::make_big(ctx.heap(), true, &[1, 1]).expect("the bignum fits");
    assert!(matches!(
        call(&reg, &mut ctx, "make_fun", &[lists, reverse, big]),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------------

#[test]
fn the_fun_info_bifs_are_registered() {
    let reg = default_registry();
    for (name, arity) in [
        ("fun_info", 2),
        ("fun_info_mfa", 1),
        ("fun_to_list", 1),
        ("make_fun", 3),
    ] {
        assert!(
            reg.contains("erlang", name, arity),
            "missing erlang:{name}/{arity}"
        );
    }
    // `bif.tab` declares no `fun_info/1`; the C's `fun_info_2` with a non-item
    // second argument is `badarg`, so `fun_info/1` must stay unregistered.
    assert!(!reg.contains("erlang", "fun_info", 1));

    let core = BifRegistry::with_core_bifs();
    assert!(core.contains("erlang", "fun_info", 2));
    assert!(core.contains("erlang", "make_fun", 3));
}

/// The kind classifier the module relies on: a raw `make_fun3` block is `Local`,
/// an `EXPORT_EXT` block is `External` and a `NEW_FUN_EXT` block is `Imported`.
#[test]
fn the_three_fun_kinds_are_distinguished() {
    let atoms = seeded_atoms();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut ctx = make_ctx(&mut heap, &atoms);

    let external = external_fun(&mut ctx, 1, "lists", "reverse");
    assert_eq!(
        beam_codec::term::fun_kind_term(external).expect("a fun"),
        FunKind::External
    );

    let (_storage, local) = local_fun(1, 1, 0, &[]);
    assert_eq!(
        beam_codec::term::fun_kind_term(local).expect("a fun"),
        FunKind::Local
    );

    let imported = imported_fun(&mut ctx, 1, "fiprobe", &[0u8; 16], 0, 0, 0, &[]);
    assert_eq!(
        beam_codec::term::fun_kind_term(imported).expect("a fun"),
        FunKind::Imported
    );
}
