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

//! Integration tests for the `ets:new/2` option surface, the `ets:new/2` return
//! shape and the shared table-argument resolver, driven through the public
//! [`BifRegistry::call`] over a real [`TableRegistry`].
//!
//! Every behaviour asserted here was probed against the system `erl`
//! (OTP 29.1.1 / erts-17.1) **first**; the probe output is quoted beside the
//! assertion that pins it. Terms are compared **structurally**
//! ([`beam_term::compare::eq`]) and tuple/list destructuring goes through the
//! registered `erlang`/`lists` BIFs, so the suite uses only the public API.
//!
//! Table-taking BIFs need the context to carry a `&mut TableRegistry`
//! (`.with_tables(&mut tables)`), so every call goes through a small helper over
//! `&mut BifContext` ([`ets_new`]/[`ets_new_ok`]/[`ets_info1`]/[`ets_info2`]).
//!
//! Every option's *effective* `ets:info/2` rendering is pinned too, including the
//! two fields `beam_ets::TableInfo` cannot express (`write_concurrency`,
//! `decentralized_counters` — see
//! [`info_reports_the_effective_write_concurrency_profile`]) and the bare
//! `set`/`protected` no-op rule (see [`the_bare_set_and_protected_atoms_are_no_ops`]).
use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, TableRegistry};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{atom_val, signed_value, AtomTable, Eterm};

/// The atom names the `ets:new/2` option parser, the `ets:info/2` field set and
/// the destructuring helpers need.
///
/// The **first** name is a harmless sentinel. The option parser classifies an
/// option *value* with `atoms.name(atom_val(value))`, and `atom_val` shifts the
/// raw word of a small integer down to atom index `0` (`make_small(1)` is
/// `0b1_1111`, `atom_val` maps it to `0`). Seeding a name that is never a legal
/// option value at index `0` keeps a rejected `{read_concurrency, 1}` from being
/// misread as `{read_concurrency, true}` — the oracle rejects it with `badarg`.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        // Sentinel (atom index 0 — see the doc comment above).
        "sentinel",
        // Booleans and the `none`/`auto` option values.
        "true",
        "false",
        "undefined",
        "none",
        "auto",
        // The table kinds.
        "set",
        "ordered_set",
        "bag",
        "duplicate_bag",
        // The protection modes.
        "public",
        "protected",
        "private",
        // The bare flag options.
        "named_table",
        "compressed",
        // The option keys.
        "keypos",
        "read_concurrency",
        "write_concurrency",
        "decentralized_counters",
        "heir",
        "debug_hash_fixed_number_of_locks",
        // The `ets:info/2` field names.
        "id",
        "name",
        "type",
        "protection",
        "size",
        "memory",
        "owner",
        "node",
        "nonode@nohost",
        // The probe table names, object keys and the unknown-option atom.
        "t",
        "ntab",
        "justname",
        "k",
        "v",
        "not_an_option",
        // The value the `{keypos, atom}` rejection probe uses.
        "atom",
    ]);
    atoms
}

/// `ets:new(Name, Options)` through the registry (`Options` a list of terms).
fn ets_new(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    name: &str,
    options: &[Eterm],
) -> Result<Eterm, BeamError> {
    let name = ctx.atom(name)?;
    let opts = ctx.list(options)?;
    bifs.call("ets", "new", 2, ctx, &[name, opts])
}

/// `ets:new(Name, Options)`, which must succeed.
fn ets_new_ok(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    name: &str,
    options: &[Eterm],
) -> Eterm {
    ets_new(bifs, ctx, name, options).expect("ets:new")
}

/// `ets:info(Tab)`.
fn ets_info1(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
) -> Result<Eterm, BeamError> {
    bifs.call("ets", "info", 1, ctx, &[handle])
}

/// `ets:info(Tab, Item)`.
fn ets_info2(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    item: &str,
) -> Result<Eterm, BeamError> {
    let item = ctx.atom(item)?;
    bifs.call("ets", "info", 2, ctx, &[handle, item])
}

/// `erlang:length/1` of the list `term`.
fn length(bifs: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> i64 {
    let len = bifs
        .call("erlang", "length", 1, ctx, &[term])
        .expect("erlang:length");
    signed_value(len)
}

/// `erlang:is_reference/1`.
fn is_reference(bifs: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> bool {
    let out = bifs
        .call("erlang", "is_reference", 1, ctx, &[term])
        .expect("erlang:is_reference");
    is_atom_named(ctx.atoms(), out, "true")
}

/// The bare option atom `name`.
fn bare(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("seeded")
}

/// The 2-tuple option `{Key, Value}`.
fn pair(ctx: &mut BifContext<'_>, key: &str, value: Eterm) -> Eterm {
    let key = ctx.atom(key).expect("seeded");
    ctx.tuple(&[key, value]).expect("fits")
}

/// The 3-tuple option `{Key, A, B}`.
fn triple(ctx: &mut BifContext<'_>, key: &str, a: Eterm, b: Eterm) -> Eterm {
    let key = ctx.atom(key).expect("seeded");
    ctx.tuple(&[key, a, b]).expect("fits")
}

fn is_atom_named(atoms: &AtomTable, term: Eterm, name: &str) -> bool {
    atoms.name(atom_val(term)) == Some(name)
}

fn is_true(atoms: &AtomTable, term: Eterm) -> bool {
    is_atom_named(atoms, term, "true")
}

fn is_false(atoms: &AtomTable, term: Eterm) -> bool {
    is_atom_named(atoms, term, "false")
}

fn is_badarg(err: &BeamError) -> bool {
    matches!(err, BeamError::BadArg)
}

/// A heap, an atom table and a table registry — the shared fixture of every test
/// below.
fn parts() -> (Heap, AtomTable, TableRegistry) {
    (
        Heap::new(65536).expect("test heap"),
        seeded_atoms(),
        TableRegistry::new(),
    )
}

/// The atom name `ets:info(Tab, Item)` reports.
fn info_atom(bifs: &BifRegistry, ctx: &mut BifContext<'_>, handle: Eterm, item: &str) -> String {
    let value = ets_info2(bifs, ctx, handle, item).expect("info/2");
    ctx.atoms()
        .name(atom_val(value))
        .expect("an atom")
        .to_string()
}

/// The integer `ets:info(Tab, Item)` reports.
fn info_int(bifs: &BifRegistry, ctx: &mut BifContext<'_>, handle: Eterm, item: &str) -> i64 {
    let value = ets_info2(bifs, ctx, handle, item).expect("info/2");
    signed_value(value)
}

/// The `true`/`false` atom `ets:info(Tab, Item)` reports.
fn info_bool(bifs: &BifRegistry, ctx: &mut BifContext<'_>, handle: Eterm, item: &str) -> bool {
    let value = ets_info2(bifs, ctx, handle, item).expect("info/2");
    if is_true(ctx.atoms(), value) {
        true
    } else {
        assert!(is_false(ctx.atoms(), value), "not a boolean: {item}");
        false
    }
}

// ---------------------------------------------------------------- accept matrix

/// The oracle:
/// ```text
/// 1> [element(1, ets:new(t,O)) || O <- [
///      [set],[ordered_set],[bag],[duplicate_bag],
///      [public],[protected],[private],
///      [set,{keypos,2}],[bag,{keypos,3}],
///      [{read_concurrency,true}],[{read_concurrency,false}],
///      [{write_concurrency,true}],[{write_concurrency,false}],[{write_concurrency,auto}],
///      [{write_concurrency,{debug_hash_fixed_number_of_locks,1}}],
///      [{write_concurrency,{debug_hash_fixed_number_of_locks,32768}}],
///      [{heir,none}],
///      [{decentralized_counters,true}],[{decentralized_counters,false}],
///      [compressed,compressed]]].
/// #Ref<..>                              % every option set is accepted
/// 2> ets:new(ntab,[named_table]).
/// ntab                                  % named_table accepted, returns the name atom
/// 3> P = self(),
///    {ets:new(t,[set,{heir,P}]), ets:new(t,[set,{heir,P,data}])}.
/// {#Ref<..>,#Ref<..>}
/// ```
#[test]
fn accepts_the_documented_option_matrix() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");
    let three = ctx.int(3).expect("fits");
    let tf = ctx.atom("true").expect("seeded");
    let ff = ctx.atom("false").expect("seeded");
    let auto = ctx.atom("auto").expect("seeded");
    let none = ctx.atom("none").expect("seeded");
    let pid = ProcessId::new(1, 0).to_eterm();
    let data = ctx.int(42).expect("fits");

    let set = bare(&mut ctx, "set");
    let ordered_set = bare(&mut ctx, "ordered_set");
    let bag = bare(&mut ctx, "bag");
    let duplicate_bag = bare(&mut ctx, "duplicate_bag");
    let public = bare(&mut ctx, "public");
    let protected = bare(&mut ctx, "protected");
    let private = bare(&mut ctx, "private");
    let compressed = bare(&mut ctx, "compressed");

    let keypos2 = pair(&mut ctx, "keypos", two);
    let keypos3 = pair(&mut ctx, "keypos", three);
    let rc_true = pair(&mut ctx, "read_concurrency", tf);
    let rc_false = pair(&mut ctx, "read_concurrency", ff);
    let wc_true = pair(&mut ctx, "write_concurrency", tf);
    let wc_false = pair(&mut ctx, "write_concurrency", ff);
    let wc_auto = pair(&mut ctx, "write_concurrency", auto);
    let locks1 = {
        let inner = pair(&mut ctx, "debug_hash_fixed_number_of_locks", one);
        pair(&mut ctx, "write_concurrency", inner)
    };
    let locks32k = {
        let n = ctx.int(32_768).expect("fits");
        let inner = pair(&mut ctx, "debug_hash_fixed_number_of_locks", n);
        pair(&mut ctx, "write_concurrency", inner)
    };
    let heir_none = pair(&mut ctx, "heir", none);
    let heir_pid = pair(&mut ctx, "heir", pid);
    let heir_pid_data = triple(&mut ctx, "heir", pid, data);
    let dc_true = pair(&mut ctx, "decentralized_counters", tf);
    let dc_false = pair(&mut ctx, "decentralized_counters", ff);

    let cases: Vec<(&str, Vec<Eterm>)> = vec![
        ("[set]", vec![set]),
        ("[ordered_set]", vec![ordered_set]),
        ("[bag]", vec![bag]),
        ("[duplicate_bag]", vec![duplicate_bag]),
        ("[public]", vec![public]),
        ("[protected]", vec![protected]),
        ("[private]", vec![private]),
        ("[set,public]", vec![set, public]),
        ("[set,private]", vec![set, private]),
        ("[compressed]", vec![compressed]),
        ("[set,{keypos,2}]", vec![set, keypos2]),
        ("[bag,{keypos,3}]", vec![bag, keypos3]),
        ("[{read_concurrency,true}]", vec![rc_true]),
        ("[{read_concurrency,false}]", vec![rc_false]),
        ("[{write_concurrency,true}]", vec![wc_true]),
        ("[{write_concurrency,false}]", vec![wc_false]),
        ("[{write_concurrency,auto}]", vec![wc_auto]),
        ("[{write_concurrency,{locks,1}}]", vec![locks1]),
        ("[{write_concurrency,{locks,32768}}]", vec![locks32k]),
        ("[{heir,none}]", vec![heir_none]),
        ("[{heir,Pid}]", vec![heir_pid]),
        ("[{heir,Pid,Data}]", vec![heir_pid_data]),
        ("[{decentralized_counters,true}]", vec![dc_true]),
        ("[{decentralized_counters,false}]", vec![dc_false]),
    ];
    for (label, options) in &cases {
        let result = ets_new(&bifs, &mut ctx, "t", options);
        assert!(result.is_ok(), "{label} should be accepted: {result:?}");
    }

    // `named_table` is a distinct name (it is registered globally), so it is
    // created on its own — and, per the oracle, returns the *name atom*.
    let named_table = bare(&mut ctx, "named_table");
    let named = ets_new_ok(&bifs, &mut ctx, "ntab", &[named_table]);
    assert!(is_atom_named(&atoms, named, "ntab"));
}

// ---------------------------------------------------------------- reject matrix

/// The oracle — every one of these is `** exception error: bad argument`:
/// ```text
/// 1> [catch ets:new(t,O) || O <- [
///      [{read_concurrency,auto}],
///      [{read_concurrency,{true,4}}],
///      [{read_concurrency,1}],
///      [{write_concurrency,{true,4}}],
///      [{heir,none,1}],
///      [{keypos,0}],
///      [{keypos,-1}],
///      [{keypos,atom}],
///      [ordered_set,{write_concurrency,{debug_hash_fixed_number_of_locks,4}}],
///      [{write_concurrency,{debug_hash_fixed_number_of_locks,0}}],
///      [{write_concurrency,{debug_hash_fixed_number_of_locks,32769}}],
///      [{write_concurrency,{debug_hash_fixed_number_of_locks,-1}}],
///      [not_an_option]]].
/// [{'EXIT',{badarg,..}}, ...]           % all badarg; no table is created
/// ```
///
/// The `{read_concurrency, 1}` case is the sentinel-dependent one documented on
/// [`seeded_atoms`] — the C reads the option value's raw word, and a small
/// integer's raw word aliases atom index `0`.
#[test]
fn rejects_the_documented_bad_option_forms() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let zero = ctx.int(0).expect("fits");
    let one = ctx.int(1).expect("fits");
    let minus_one = ctx.int(-1).expect("fits");
    let four = ctx.int(4).expect("fits");
    let too_many = ctx.int(32_769).expect("fits");
    let tf = ctx.atom("true").expect("seeded");
    let auto = ctx.atom("auto").expect("seeded");
    let none = ctx.atom("none").expect("seeded");
    let atom_value = ctx.atom("atom").expect("seeded");

    let rc_auto = pair(&mut ctx, "read_concurrency", auto);
    let rc_true_four = {
        let inner = ctx.tuple(&[tf, four]).expect("fits");
        pair(&mut ctx, "read_concurrency", inner)
    };
    let rc_one = pair(&mut ctx, "read_concurrency", one);
    let wc_true_four = {
        let inner = ctx.tuple(&[tf, four]).expect("fits");
        pair(&mut ctx, "write_concurrency", inner)
    };
    let heir_none_data = triple(&mut ctx, "heir", none, one);
    let keypos_zero = pair(&mut ctx, "keypos", zero);
    let keypos_neg = pair(&mut ctx, "keypos", minus_one);
    let keypos_atom = pair(&mut ctx, "keypos", atom_value);
    let ordered_set = bare(&mut ctx, "ordered_set");
    let ordered_locks = {
        let inner = pair(&mut ctx, "debug_hash_fixed_number_of_locks", four);
        let wc = pair(&mut ctx, "write_concurrency", inner);
        vec![ordered_set, wc]
    };
    let locks_zero = {
        let inner = pair(&mut ctx, "debug_hash_fixed_number_of_locks", zero);
        pair(&mut ctx, "write_concurrency", inner)
    };
    let locks_too_many = {
        let inner = pair(&mut ctx, "debug_hash_fixed_number_of_locks", too_many);
        pair(&mut ctx, "write_concurrency", inner)
    };
    let locks_neg = {
        let inner = pair(&mut ctx, "debug_hash_fixed_number_of_locks", minus_one);
        pair(&mut ctx, "write_concurrency", inner)
    };
    let unknown = bare(&mut ctx, "not_an_option");

    let cases: Vec<(&str, Vec<Eterm>)> = vec![
        ("[{read_concurrency,auto}]", vec![rc_auto]),
        ("[{read_concurrency,{true,4}}]", vec![rc_true_four]),
        ("[{read_concurrency,1}]", vec![rc_one]),
        ("[{write_concurrency,{true,4}}]", vec![wc_true_four]),
        ("[{heir,none,1}]", vec![heir_none_data]),
        ("[{keypos,0}]", vec![keypos_zero]),
        ("[{keypos,-1}]", vec![keypos_neg]),
        ("[{keypos,atom}]", vec![keypos_atom]),
        ("[ordered_set,{write_concurrency,{locks,4}}]", ordered_locks),
        ("[{write_concurrency,{locks,0}}]", vec![locks_zero]),
        ("[{write_concurrency,{locks,32769}}]", vec![locks_too_many]),
        ("[{write_concurrency,{locks,-1}}]", vec![locks_neg]),
        ("[not_an_option]", vec![unknown]),
    ];
    for (label, options) in &cases {
        let err = ets_new(&bifs, &mut ctx, "t", options).expect_err(label);
        assert!(is_badarg(&err), "{label} should be badarg: {err:?}");
    }
}

// ---------------------------------------------------------- repeated options

/// The oracle:
/// ```text
/// 1> T = ets:new(t,[set,ordered_set]), ets:info(T,type).
/// ordered_set
/// 2> T = ets:new(t,[bag,duplicate_bag]), ets:info(T,type).
/// duplicate_bag
/// 3> T = ets:new(t,[duplicate_bag,bag]), ets:info(T,type).
/// bag
/// 4> T = ets:new(t,[{keypos,2},{keypos,3}]), ets:info(T,keypos).
/// 3
/// 5> T = ets:new(t,[{read_concurrency,true},{read_concurrency,false}]), ets:info(T,read_concurrency).
/// false
/// 6> T = ets:new(t,[{read_concurrency,false},{read_concurrency,true}]), ets:info(T,read_concurrency).
/// true
/// ```
///
/// A repeated option is accepted (`ets:new/2` never rejects a duplicate) and the
/// **last** occurrence of a scalar option wins. The table-kind combinations pinned
/// here are the ones whose win is observable — `{set,ordered_set}` and the two
/// `bag`/`duplicate_bag` orders; `{keypos,_}` and `{read_concurrency,_}` show the
/// last-wins rule for a scalar.
///
/// (The bare `set` / `protected` atoms are a *separate* rule — the C treats both
/// as no-ops, `../../erts/emulator/beam/erl_db.c:2710`, so `[ordered_set,set]`
/// stays `ordered_set` — pinned by [`the_bare_set_and_protected_atoms_are_no_ops`];
/// the `{write_concurrency,_}` last-wins is pinned by
/// [`info_reports_the_effective_write_concurrency_profile`].)
#[test]
fn repeated_options_are_accepted_and_the_last_one_wins() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let two = ctx.int(2).expect("fits");
    let three = ctx.int(3).expect("fits");
    let tf = ctx.atom("true").expect("seeded");
    let ff = ctx.atom("false").expect("seeded");
    let set = bare(&mut ctx, "set");
    let ordered_set = bare(&mut ctx, "ordered_set");
    let bag = bare(&mut ctx, "bag");
    let duplicate_bag = bare(&mut ctx, "duplicate_bag");

    let keypos2 = pair(&mut ctx, "keypos", two);
    let keypos3 = pair(&mut ctx, "keypos", three);
    let rc_true = pair(&mut ctx, "read_concurrency", tf);
    let rc_false = pair(&mut ctx, "read_concurrency", ff);

    // (options, kind, keypos, read_concurrency)
    let cases: Vec<(Vec<Eterm>, &str, i64, bool)> = vec![
        (vec![set, ordered_set], "ordered_set", 1, false),
        (vec![ordered_set, ordered_set], "ordered_set", 1, false),
        (vec![bag, duplicate_bag], "duplicate_bag", 1, false),
        (vec![duplicate_bag, bag], "bag", 1, false),
        (vec![ordered_set, bag], "bag", 1, false),
        (vec![set, keypos2, keypos3], "set", 3, false),
        (vec![keypos2, keypos3], "set", 3, false),
        (vec![rc_true, rc_false], "set", 1, false),
        (vec![rc_false, rc_true], "set", 1, true),
    ];
    for (options, kind, keypos, read_concurrency) in &cases {
        let handle = ets_new_ok(&bifs, &mut ctx, "t", options);
        assert_eq!(
            info_atom(&bifs, &mut ctx, handle, "type"),
            *kind,
            "{options:?} type"
        );
        assert_eq!(
            info_int(&bifs, &mut ctx, handle, "keypos"),
            *keypos,
            "{options:?} keypos"
        );
        assert_eq!(
            info_bool(&bifs, &mut ctx, handle, "read_concurrency"),
            *read_concurrency,
            "{options:?} read_concurrency"
        );
    }
}

// ----------------------------------- the bare set/protected no-op rule

/// The oracle:
/// ```text
/// 1> T = ets:new(t,[ordered_set,set]), ets:info(T,type).
/// ordered_set
/// 2> T = ets:new(t,[bag,set]), ets:info(T,type).
/// bag
/// 3> T = ets:new(t,[duplicate_bag,set]), ets:info(T,type).
/// duplicate_bag
/// 4> T = ets:new(t,[private,protected]), ets:info(T,protection).
/// private
/// 5> T = ets:new(t,[public,protected]), ets:info(T,protection).
/// public
/// 6> T = ets:new(t,[set]), ets:info(T,type).          ets:new(t,[protected]), ets:info(T,protection).
/// set                                                 protected
/// ```
///
/// The bare `set` and `protected` atoms are **no-ops**: the C `ets_new_2`'s
/// `else if (val == am_set || val == am_protected) ;`
/// (`../../erts/emulator/beam/erl_db.c:2710`) swallows both, so neither ever
/// overrides an earlier kind/access — unlike every other bare atom, which follows
/// the last-wins rule pinned by
/// [`repeated_options_are_accepted_and_the_last_one_wins`]. `set`/`protected` are
/// also the defaults, so a lone occurrence still yields a `set`/`protected` table
/// (the last two probes).
#[test]
fn the_bare_set_and_protected_atoms_are_no_ops() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let set = bare(&mut ctx, "set");
    let ordered_set = bare(&mut ctx, "ordered_set");
    let bag = bare(&mut ctx, "bag");
    let duplicate_bag = bare(&mut ctx, "duplicate_bag");
    let public = bare(&mut ctx, "public");
    let protected = bare(&mut ctx, "protected");
    let private = bare(&mut ctx, "private");

    // (options, type, protection)
    let cases: Vec<(Vec<Eterm>, &str, &str)> = vec![
        (vec![ordered_set, set], "ordered_set", "protected"),
        (vec![bag, set], "bag", "protected"),
        (vec![duplicate_bag, set], "duplicate_bag", "protected"),
        (vec![private, protected], "set", "private"),
        (vec![public, protected], "set", "public"),
        // A lone `set`/`protected` is still the default.
        (vec![set], "set", "protected"),
        (vec![protected], "set", "protected"),
    ];
    for (options, kind, protection) in &cases {
        let handle = ets_new_ok(&bifs, &mut ctx, "t", options);
        assert_eq!(
            info_atom(&bifs, &mut ctx, handle, "type"),
            *kind,
            "{options:?} type"
        );
        assert_eq!(
            info_atom(&bifs, &mut ctx, handle, "protection"),
            *protection,
            "{options:?} protection"
        );
    }
}

// ------------------------------------------------------------- info: the fields
/// The oracle — the probe prints `{type, keypos, compressed, read_concurrency}`
/// (with `ets:info(T, type | keypos | compressed | read_concurrency)`):
/// ```text
/// [set]                             -> {set,1,false,false}
/// [ordered_set]                     -> {ordered_set,1,false,false}
/// [bag]                             -> {bag,1,false,false}
/// [duplicate_bag]                   -> {duplicate_bag,1,false,false}
/// [set,{keypos,2}]                  -> {set,2,false,false}
/// [bag,{keypos,3}]                  -> {bag,3,false,false}
/// [compressed]                      -> {set,1,true,false}
/// [compressed,compressed]           -> {set,1,true,false}
/// [{read_concurrency,true}]         -> {set,1,false,true}
/// [{read_concurrency,false}]        -> {set,1,false,false}
/// [{read_concurrency,true},{read_concurrency,false}] -> {set,1,false,false}
/// [public]                          -> protection public
/// [private]                         -> protection private
/// ```
#[test]
fn info_reports_the_effective_type_keypos_compression_and_read_concurrency() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let two = ctx.int(2).expect("fits");
    let three = ctx.int(3).expect("fits");
    let tf = ctx.atom("true").expect("seeded");
    let ff = ctx.atom("false").expect("seeded");
    let set = bare(&mut ctx, "set");
    let ordered_set = bare(&mut ctx, "ordered_set");
    let bag = bare(&mut ctx, "bag");
    let duplicate_bag = bare(&mut ctx, "duplicate_bag");
    let compressed = bare(&mut ctx, "compressed");
    let keypos2 = pair(&mut ctx, "keypos", two);
    let keypos3 = pair(&mut ctx, "keypos", three);
    let rc_true = pair(&mut ctx, "read_concurrency", tf);
    let rc_false = pair(&mut ctx, "read_concurrency", ff);

    // (options, kind, keypos, compressed, read_concurrency)
    let cases: Vec<(Vec<Eterm>, &str, i64, bool, bool)> = vec![
        (vec![set], "set", 1, false, false),
        (vec![ordered_set], "ordered_set", 1, false, false),
        (vec![bag], "bag", 1, false, false),
        (vec![duplicate_bag], "duplicate_bag", 1, false, false),
        (vec![set, keypos2], "set", 2, false, false),
        (vec![bag, keypos3], "bag", 3, false, false),
        (vec![compressed], "set", 1, true, false),
        (vec![compressed, compressed], "set", 1, true, false),
        (vec![rc_true], "set", 1, false, true),
        (vec![rc_false], "set", 1, false, false),
        (vec![rc_true, rc_false], "set", 1, false, false),
    ];
    for (options, kind, keypos, compressed_expected, rc) in &cases {
        let handle = ets_new_ok(&bifs, &mut ctx, "t", options);
        assert_eq!(
            info_atom(&bifs, &mut ctx, handle, "type"),
            *kind,
            "{options:?} type"
        );
        assert_eq!(
            info_int(&bifs, &mut ctx, handle, "keypos"),
            *keypos,
            "{options:?} keypos"
        );
        assert_eq!(
            info_bool(&bifs, &mut ctx, handle, "compressed"),
            *compressed_expected,
            "{options:?} compressed"
        );
        assert_eq!(
            info_bool(&bifs, &mut ctx, handle, "read_concurrency"),
            *rc,
            "{options:?} read_concurrency"
        );
    }

    // `protection` reads the access the option list selected.
    let public = bare(&mut ctx, "public");
    let private = bare(&mut ctx, "private");
    let protected = bare(&mut ctx, "protected");
    for (options, expected) in [
        (vec![public], "public"),
        (vec![private], "private"),
        (vec![protected], "protected"),
        (vec![set], "protected"), // the default
    ] {
        let handle = ets_new_ok(&bifs, &mut ctx, "t", &options);
        assert_eq!(
            info_atom(&bifs, &mut ctx, handle, "protection"),
            expected,
            "{options:?} protection"
        );
    }

    // `named_table` and the recorded `name` field (the oracle records the `Name`
    // argument whether or not `named_table` was given).
    let named_table = bare(&mut ctx, "named_table");
    let named = ets_new_ok(&bifs, &mut ctx, "ntab", &[named_table]);
    assert!(info_bool(&bifs, &mut ctx, named, "named_table"));
    assert_eq!(info_atom(&bifs, &mut ctx, named, "name"), "ntab");
    let plain = ets_new_ok(&bifs, &mut ctx, "t", &[set]);
    assert!(!info_bool(&bifs, &mut ctx, plain, "named_table"));
    assert_eq!(info_atom(&bifs, &mut ctx, plain, "name"), "t");
}

// ------------------------------------------------- info: the profile fields

/// The rendered `{debug_hash_fixed_number_of_locks, L}` term for a normalized lock
/// count `L` — the `ets:info(T, write_concurrency)` shape of the hidden
/// `ets:new/2` option.
fn locks_term(ctx: &mut BifContext<'_>, n: i64) -> Eterm {
    let key = ctx
        .atom("debug_hash_fixed_number_of_locks")
        .expect("seeded");
    let value = ctx.int(n).expect("fits");
    ctx.tuple(&[key, value]).expect("fits")
}

/// The oracle — `{write_concurrency, decentralized_counters}` as
/// `ets:info(T, write_concurrency)` / `ets:info(T, decentralized_counters)` report
/// them, probed live against the system `erl` (OTP 29.1.1 / erts-17.1):
/// ```text
/// [set]                                                          -> {false,false}
/// [set,{write_concurrency,true}]                                 -> {true,false}
/// [set,{write_concurrency,false}]                                -> {false,false}
/// [set,{write_concurrency,auto}]                                 -> {auto,true}
/// [{write_concurrency,{debug_hash_fixed_number_of_locks,1}}]     -> {{…,64},true}
/// [{write_concurrency,{debug_hash_fixed_number_of_locks,4}}]     -> {{…,64},true}
/// [{write_concurrency,{debug_hash_fixed_number_of_locks,63}}]    -> {{…,64},true}
/// [{write_concurrency,{debug_hash_fixed_number_of_locks,65}}]    -> {{…,64},true}
/// [{write_concurrency,{debug_hash_fixed_number_of_locks,100}}]   -> {{…,64},true}
/// [{write_concurrency,{debug_hash_fixed_number_of_locks,128}}]   -> {{…,128},true}
/// [{write_concurrency,{debug_hash_fixed_number_of_locks,32767}}] -> {{…,16384},true}
/// [{write_concurrency,{debug_hash_fixed_number_of_locks,32768}}] -> {{…,32768},true}
/// [ordered_set,{write_concurrency,true}]                         -> {true,true}
/// [ordered_set]                                                  -> {false,false}
/// [bag,{write_concurrency,true}]                                 -> {true,false}
/// [set,{read_concurrency,true}]                                  -> {false,false}   % read_concurrency = true
/// [set,{decentralized_counters,true}]                            -> {false,false}
/// [ordered_set,{decentralized_counters,true}]                    -> {false,false}
/// [{write_concurrency,true},{decentralized_counters,true}]       -> {true,true}
/// [{decentralized_counters,true},{write_concurrency,true}]       -> {true,true}
/// [{write_concurrency,auto},{decentralized_counters,false}]      -> {auto,false}
/// [set,{write_concurrency,true},private]                         -> {false,false}
/// ```
///
/// `write_concurrency` is what `db_hash_create` derives from the raw option: the
/// hidden lock count rounds **down** to a power of two and clamps up to `64`
/// (`NLOCKS_WITH_ITEM_COUNTERS`). `decentralized_counters` is the CATREE rule —
/// true only for a **fine-locked** table — so a bare `{decentralized_counters,true}`
/// is not enough (there are no locks to decentralize) and a `private` table is
/// never fine-locked at all.
///
/// These two fields are the ones `beam_ets::TableInfo` cannot express: their
/// effective values are computed by the `ets:new/2` option parser
/// (`TableRegistry::profiles`) and must **survive the take/restore round trip**
/// every table-taking BIF performs, so each case below warms the table up with an
/// `insert/2` + `lookup/2` before asking for `info` (a later `ets:delete/1` is the
/// only permanent removal, and it drops the profile).
#[test]
fn info_reports_the_effective_write_concurrency_profile() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let tf = ctx.atom("true").expect("seeded");
    let ff = ctx.atom("false").expect("seeded");
    let auto = ctx.atom("auto").expect("seeded");
    let set = bare(&mut ctx, "set");
    let ordered_set = bare(&mut ctx, "ordered_set");
    let bag = bare(&mut ctx, "bag");
    let private = bare(&mut ctx, "private");

    let wc_true = pair(&mut ctx, "write_concurrency", tf);
    let wc_false = pair(&mut ctx, "write_concurrency", ff);
    let wc_auto = pair(&mut ctx, "write_concurrency", auto);
    let rc_true = pair(&mut ctx, "read_concurrency", tf);
    let dc_true = pair(&mut ctx, "decentralized_counters", tf);
    let dc_false = pair(&mut ctx, "decentralized_counters", ff);

    let mut locks = Vec::new();
    for n in [1i64, 4, 63, 65, 100, 128, 32_767, 32_768] {
        let value = ctx.int(n).expect("fits");
        let inner = pair(&mut ctx, "debug_hash_fixed_number_of_locks", value);
        locks.push(pair(&mut ctx, "write_concurrency", inner));
    }
    let l64 = locks_term(&mut ctx, 64);
    let l128 = locks_term(&mut ctx, 128);
    let l16384 = locks_term(&mut ctx, 16_384);
    let l32768 = locks_term(&mut ctx, 32_768);

    // (label, options, expected write_concurrency, expected decentralized_counters)
    let cases: Vec<(&str, Vec<Eterm>, Eterm, bool)> = vec![
        ("[set]", vec![set], ff, false),
        ("[set,{write_concurrency,true}]", vec![set, wc_true], tf, false),
        ("[set,{write_concurrency,false}]", vec![set, wc_false], ff, false),
        (
            "[set,{write_concurrency,auto}]",
            vec![set, wc_auto],
            auto,
            true,
        ),
        ("[{locks,1}]", vec![locks[0]], l64, true),
        ("[{locks,4}]", vec![locks[1]], l64, true),
        ("[{locks,63}]", vec![locks[2]], l64, true),
        ("[{locks,65}]", vec![locks[3]], l64, true),
        ("[{locks,100}]", vec![locks[4]], l64, true),
        ("[{locks,128}]", vec![locks[5]], l128, true),
        ("[{locks,32767}]", vec![locks[6]], l16384, true),
        ("[{locks,32768}]", vec![locks[7]], l32768, true),
        (
            "[ordered_set,{write_concurrency,true}]",
            vec![ordered_set, wc_true],
            tf,
            true,
        ),
        ("[ordered_set]", vec![ordered_set], ff, false),
        ("[bag,{write_concurrency,true}]", vec![bag, wc_true], tf, false),
        ("[set,{read_concurrency,true}]", vec![set, rc_true], ff, false),
        (
            "[set,{decentralized_counters,true}]",
            vec![set, dc_true],
            ff,
            false,
        ),
        (
            "[ordered_set,{decentralized_counters,true}]",
            vec![ordered_set, dc_true],
            ff,
            false,
        ),
        (
            "[{write_concurrency,true},{decentralized_counters,true}]",
            vec![wc_true, dc_true],
            tf,
            true,
        ),
        (
            "[{decentralized_counters,true},{write_concurrency,true}]",
            vec![dc_true, wc_true],
            tf,
            true,
        ),
        (
            "[{write_concurrency,auto},{decentralized_counters,false}]",
            vec![wc_auto, dc_false],
            auto,
            false,
        ),
        (
            "[set,{write_concurrency,true},private]",
            vec![set, wc_true, private],
            ff,
            false,
        ),
    ];
    for (label, options, expected_wc, expected_dc) in &cases {
        let handle = ets_new_ok(&bifs, &mut ctx, "t", options);
        // Warm the table up through two table-taking BIFs before the `info`: the
        // take/restore round trip must leave the creation-time profile in place.
        let one = ctx.int(1).expect("fits");
        let obj = ctx.tuple(&[one, one]).expect("fits");
        bifs.call("ets", "insert", 2, &mut ctx, &[handle, obj])
            .expect("insert");
        bifs.call("ets", "lookup", 2, &mut ctx, &[handle, one])
            .expect("lookup");

        let wc = ets_info2(&bifs, &mut ctx, handle, "write_concurrency").expect("info/2 wc");
        assert!(
            beam_term::compare::eq(wc, *expected_wc, &atoms),
            "{label} write_concurrency: oracle {expected_wc:?}, runtime {wc:?}"
        );
        assert_eq!(
            info_bool(&bifs, &mut ctx, handle, "decentralized_counters"),
            *expected_dc,
            "{label} decentralized_counters"
        );
    }

    // `{read_concurrency, true}` is an independent flag: it turns
    // `read_concurrency` on without making the table fine-locked.
    let rc = ets_new_ok(&bifs, &mut ctx, "t", &[set, rc_true]);
    assert!(info_bool(&bifs, &mut ctx, rc, "read_concurrency"));
    assert!(!info_bool(&bifs, &mut ctx, rc, "write_concurrency"));
}

// --------------------------------------------- a deleted table leaves no profile

/// The oracle:
/// ```text
/// 1> T = ets:new(t,[set,{write_concurrency,true}]), ets:info(T, write_concurrency).
/// true
/// 2> ets:delete(T).
/// true
/// 3> ets:info(T, write_concurrency).       % the tid's table is gone
/// undefined
/// ```
///
/// `ets:delete/1` is the only table-taking BIF that never restores its table, so
/// it is the only one that drops the [`TableRegistry`] profile
/// (`TableRegistry::forget_profile`): a deleted handle must leave nothing behind.
#[test]
fn a_deleted_table_leaves_no_profile_behind() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let tf = ctx.atom("true").expect("seeded");
    let set = bare(&mut ctx, "set");
    let wc_true = pair(&mut ctx, "write_concurrency", tf);

    let handle = ets_new_ok(&bifs, &mut ctx, "t", &[set, wc_true]);
    assert!(
        info_bool(&bifs, &mut ctx, handle, "write_concurrency"),
        "the profile is live before the delete"
    );

    let deleted = bifs
        .call("ets", "delete", 1, &mut ctx, &[handle])
        .expect("ets:delete/1");
    assert!(is_true(&atoms, deleted), "delete/1 returns true");

    // The handle no longer names a live table, so `ets:info/2` answers the bare
    // atom `undefined` (the oracle) — never the stale profile.
    let after = ets_info2(&bifs, &mut ctx, handle, "write_concurrency").expect("info/2");
    assert!(
        is_atom_named(&atoms, after, "undefined"),
        "info of a deleted table: {after:?}"
    );
    let live = ctx.tables().expect("a registry is installed").len();
    assert_eq!(live, 0, "the table is gone from the registry");
}
// ------------------------------------------------------------- return shape

/// The oracle:
/// ```text
/// 1> ets:new(ntab,[named_table]).
/// ntab                                   % the NAME ATOM
/// 2> is_reference(ets:new(unnamed,[set])).
/// true                                   % a tid reference
/// 3> T = ets:new(unnamed,[set]), ets:info(T,id) =:= T.
/// true
/// 4> N = ets:new(ntab,[named_table]), ets:info(N,id) =:= N.
/// false                                  % the tid is a reference, N is the atom
/// ```
#[test]
fn new_returns_the_name_atom_for_a_named_table_and_a_reference_otherwise() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let named_table = bare(&mut ctx, "named_table");
    let named = ets_new_ok(&bifs, &mut ctx, "ntab", &[named_table]);
    assert!(
        is_atom_named(&atoms, named, "ntab"),
        "named handle is the name atom"
    );

    let set = bare(&mut ctx, "set");
    let unnamed = ets_new_ok(&bifs, &mut ctx, "t", &[set]);
    assert!(
        is_reference(&bifs, &mut ctx, unnamed),
        "unnamed handle is a reference"
    );

    // `ets:info(T, id)` is the tid reference, `=:=` to the handle of an unnamed
    // table.
    let id = ets_info2(&bifs, &mut ctx, unnamed, "id").expect("info/2 id");
    assert!(is_reference(&bifs, &mut ctx, id), "info/2 id is a reference");
    assert!(
        beam_term::compare::eq(id, unnamed, &atoms),
        "info/2 id =:= T"
    );

    // ...and it is *not* the name atom of a `named_table`.
    let named_id = ets_info2(&bifs, &mut ctx, named, "id").expect("info/2 id");
    assert!(
        is_reference(&bifs, &mut ctx, named_id),
        "named id is a reference"
    );
    assert!(
        !beam_term::compare::eq(named_id, named, &atoms),
        "info(ntab,id) =:= ntab must be false"
    );
}

// ------------------------------------------------------------- table resolver

/// The oracle:
/// ```text
/// 1> N = ets:new(ntab,[named_table]),
///    ok = ets:insert(ntab,{k,v}),
///    {ets:lookup(ntab,k), length(ets:info(ntab)), ets:info(ntab,name)}.
/// {[{k,v}],15,ntab}
/// 2> ets:info(ntab,id) =:= ets:whereis(ntab).
/// true
/// ```
///
/// The `named_table` name atom is accepted as the table argument by the same
/// table-taking BIFs a handle is: `insert/2`, `lookup/2` and `info/1`.
#[test]
fn a_named_table_name_atom_resolves_as_a_table_argument() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let named_table = bare(&mut ctx, "named_table");
    let _handle = ets_new_ok(&bifs, &mut ctx, "ntab", &[named_table]);
    let name = ctx.atom("ntab").expect("seeded");

    let obj = {
        let k = ctx.atom("k").expect("seeded");
        let v = ctx.atom("v").expect("seeded");
        ctx.tuple(&[k, v]).expect("fits")
    };

    // `insert/2` by name.
    let inserted = bifs
        .call("ets", "insert", 2, &mut ctx, &[name, obj])
        .expect("ets:insert/2 by name");
    assert!(is_true(&atoms, inserted));

    // `lookup/2` by name finds the object.
    let key = ctx.atom("k").expect("seeded");
    let found = bifs
        .call("ets", "lookup", 2, &mut ctx, &[name, key])
        .expect("ets:lookup/2 by name");
    let expected = ctx.list(&[obj]).expect("fits");
    assert!(
        beam_term::compare::eq(found, expected, &atoms),
        "lookup by name"
    );

    // `info/1` by name returns the 15-field proplist.
    let proplist = ets_info1(&bifs, &mut ctx, name).expect("ets:info/1 by name");
    assert_eq!(length(&bifs, &mut ctx, proplist), 15);

    // ...and `info/2 name` still reads the registered name back.
    let reported = ets_info2(&bifs, &mut ctx, name, "name").expect("info/2 name");
    assert!(is_atom_named(&atoms, reported, "ntab"));
}

/// The oracle:
/// ```text
/// 1> ets:new(justname,[set]).
/// #Ref<..>                                % created by name but NOT a named_table
/// 2> ets:insert(justname,{k,v}).
/// ** exception error: bad argument
/// 3> ets:lookup(justname,k).
/// ** exception error: bad argument
/// 4> ets:insert(unknown_tab,{k,v}).
/// ** exception error: bad argument
/// ```
///
/// An atom that does not name a `named_table` — whether or not a table was ever
/// created under that name — is not a table argument.
#[test]
fn an_atom_that_is_not_a_named_table_is_badarg() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    // A table created *without* `named_table` records the name field only, and the
    // name is not registered.
    let set = bare(&mut ctx, "set");
    let _plain = ets_new_ok(&bifs, &mut ctx, "justname", &[set]);
    let obj = {
        let k = ctx.atom("k").expect("seeded");
        let v = ctx.atom("v").expect("seeded");
        ctx.tuple(&[k, v]).expect("fits")
    };
    let key = ctx.atom("k").expect("seeded");

    let justname = ctx.atom("justname").expect("seeded");
    let err = bifs
        .call("ets", "insert", 2, &mut ctx, &[justname, obj])
        .expect_err("insert by a non-named name");
    assert!(is_badarg(&err), "insert(justname,..): {err:?}");
    let err = bifs
        .call("ets", "lookup", 2, &mut ctx, &[justname, key])
        .expect_err("lookup by a non-named name");
    assert!(is_badarg(&err), "lookup(justname,..): {err:?}");

    // A name that was never created at all is badarg too.
    let unknown = bare(&mut ctx, "not_an_option");
    let err = bifs
        .call("ets", "lookup", 2, &mut ctx, &[unknown, key])
        .expect_err("lookup by an unknown name");
    assert!(is_badarg(&err), "lookup(unknown,..): {err:?}");
}

/// The oracle:
/// ```text
/// 1> ets:info({t, node()}).              ets:info({t,'nonode@nohost'}).
/// ** exception error: bad argument       ** exception error: bad argument
/// 2> ets:info({t, 42}).                  ets:insert({t,node()},{k,v}).
/// ** exception error: bad argument       ** exception error: bad argument
/// ```
///
/// The C `db_get_table_aux` accepts only an atom (a `named_table` name) or a tid —
/// the distributed `{Name, Node}` form is **not** a table argument in OTP 29, for
/// every node shape (`node()`, the literal `'nonode@nohost'`, or a non-atom).
#[test]
fn the_distributed_name_node_form_is_not_a_table_argument() {
    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let name = ctx.atom("t").expect("seeded");
    let node = ctx.atom("nonode@nohost").expect("seeded");
    let forty_two = ctx.int(42).expect("fits");
    let form_node = ctx.tuple(&[name, node]).expect("fits");
    let form_int = ctx.tuple(&[name, forty_two]).expect("fits");

    let obj = {
        let k = ctx.atom("k").expect("seeded");
        let v = ctx.atom("v").expect("seeded");
        ctx.tuple(&[k, v]).expect("fits")
    };
    let key = ctx.atom("k").expect("seeded");

    for form in [form_node, form_int] {
        let err = ets_info1(&bifs, &mut ctx, form).expect_err("info/1 of {Name,Node}");
        assert!(is_badarg(&err), "info({{t,_}}): {err:?}");
        let err = bifs
            .call("ets", "insert", 2, &mut ctx, &[form, obj])
            .expect_err("insert into {Name,Node}");
        assert!(is_badarg(&err), "insert({{t,_}},..): {err:?}");
        let err = bifs
            .call("ets", "lookup", 2, &mut ctx, &[form, key])
            .expect_err("lookup in {Name,Node}");
        assert!(is_badarg(&err), "lookup({{t,_}},..): {err:?}");
    }
}
