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

//! `erlang:get_module_info/1,2`, end to end through the runtime glue.
//!
//! `beam_bifs`'s `modinfo.rs` decodes the two arguments and validates the item against the
//! C's nine-atom dispatch chain, but the **answers** live with the *loaded*
//! `beam_loader::Module` that the runtime's `CodeServer` owns — Stage-5 state no BIF can
//! reach. Every item therefore goes through the optional `ModuleInfo` host seam, and
//! `beam_rt::context::call_registered` installs the concrete `ModuleInfoBridge` over its own
//! `CodeServer` (the same `&CodeServer` borrow shape as the `RecordBridge`). This suite
//! pins that whole path — the BIF surface, the bridge, the loader IR and the `Attr`/`CInf`
//! chunk decode — at the library boundary.
//!
//! # The module under test
//!
//! The **committed** fixture `beam_loader/tests/fixtures/slice.beam` (built once by the
//! system `erlc` — the same file the loader/engine slices use) is loaded into a fresh
//! `Runtime` with `Runtime::load_module`. Loading first is not optional: the oracle resolves
//! the module with `erts_get_module` and answers `badarg` for a module that is on the code
//! path but not loaded (probed: `erl -noshell -pa <dir> -eval
//! 'erlang:get_module_info(slice, module).'` ⇒ `badarg`, while the same call after
//! `code:ensure_loaded(slice)` ⇒ `slice`).
//!
//! Because the fixture is committed, **every assertion below runs with no toolchain at all**;
//! only [`the_answers_match_the_live_oracle`] needs the system `erl` and skips **loudly**
//! when it is absent.
//!
//! # The `exports` ordering divergence (documented honestly)
//!
//! The oracle's `exports` is the **global ERTS export-table** order
//! (`../../erts/emulator/beam/erl_bif_info.c:8434-8471` walks `export_list`) — not the file
//! `ExpT` order, not sorted, and not reconstructible from one module. The bridge returns the
//! module's real export *set* in `Module::exports` order: the same entries, possibly in a
//! different order than the oracle for some modules. This suite therefore compares `exports`
//! as an unordered **set** and pins its membership exactly.
//!
//! # Oracle ground truth (OTP 29.1.1 / erts-17.1)
//!
//! ```text
//! $ erl -noshell -pa <beam_loader/tests/fixtures> -eval '
//!     {module,slice} = code:ensure_loaded(slice),
//!     io:format("~p~n", [erlang:get_module_info(slice)]),
//!     io:format("~p~n", [erlang:get_module_info(slice, functions)]),
//!     io:format("~p~n", [erlang:get_module_info(slice, exports)]),
//!     io:format("~p~n", [erlang:get_module_info(slice, nifs)]),
//!     io:format("~p~n", [erlang:get_module_info(slice, native)]),
//!     io:format("~p~n", [erlang:get_module_info(slice, native_addresses)]),
//!     io:format("~p~n", [erlang:get_module_info(slice, attributes)]),
//!     io:format("~p~n", [erlang:get_module_info(slice, compile)]),
//!     halt(0).' -s init stop
//! [{module,slice},
//!  {exports,[{add,2},{fib,1},{pick,1},{module_info,0},{module_info,1}]},
//!  {attributes,[{vsn,[3352480567141373659144118619863872548]}]},
//!  {compile,[{version,"10.0.6"}]},
//!  {md5,<<2,133,170,13,173,204,31,130,161,1,140,211,209,13,116,36>>}]
//! [{add,2},{fib,1},{fib,3},{pick,1},{module_info,0},{module_info,1}]
//! [{add,2},{fib,1},{pick,1},{module_info,0},{module_info,1}]
//! []
//! false
//! []
//! [{vsn,[3352480567141373659144118619863872548]}]
//! [{version,"10.0.6"}]
//! ```
//!
//! (the `-pa` must come **before** `-eval`: erl evaluates the expression in command-line
//! order, so a code path added afterwards is too late). The md5 is
//! `0285aa0dadcc1f82a1018cd3d10d7424`.
mod support;

use beam_bifs::NullSink;
use beam_engine::{terms, ExecutionContext};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_small, make_small, signed_value, Eterm};
use std::path::Path;

use support::assert_badarg;

/// The committed fixture module the loader/engine slices share — a real module produced by
/// the system `erlc`, so no toolchain is needed to run the assertions.
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// The directory holding `slice.beam`, safe to hand to the oracle as an `-pa`: it contains
/// only `bsalloc`, `coverage`, `records_fixture` and `slice` — no `lists.beam` and nothing
/// else that shadows stdlib (unlike this crate's own `tests/fixtures/`).
const SLICE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../beam_loader/tests/fixtures");

/// The module name under test (the fixture's `-module(slice)`).
const MODULE: &str = "slice";

/// The oracle's md5 for `slice.beam`, lowercase hex (recorded above).
const SLICE_MD5_HEX: &str = "0285aa0dadcc1f82a1018cd3d10d7424";

/// The oracle's `functions` for `slice` — every function the module defines, in its order.
const SLICE_FUNCTIONS: [&str; 6] = [
    "add/2",
    "fib/1",
    "fib/3",
    "pick/1",
    "module_info/0",
    "module_info/1",
];

/// The oracle's `exports` for `slice` — `module_info/0,1` included, the private `fib/3` not.
const SLICE_EXPORTS: [&str; 5] = ["add/2", "fib/1", "pick/1", "module_info/0", "module_info/1"];

/// A `Runtime` with `slice.beam` really loaded (the BIF requires it, exactly like the
/// oracle).
fn runtime_with_slice() -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(SLICE_BEAM).expect("slice.beam loads");
    rt
}

/// Run `f` against a fresh `RuntimeContext` over `rt`; the context (and its sink) are
/// dropped before `rt` is borrowed again.
fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    f(&mut ctx)
}

/// `erlang:get_module_info(MODULE, item)`, panicking with the runtime's own reason.
fn item(rt: &mut Runtime, name: &str) -> Eterm {
    let m = rt.intern(MODULE);
    let key = rt.intern(name);
    with_context(rt, |ctx| {
        ctx.call_bif("erlang", "get_module_info", 2, &[m, key])
            .unwrap_or_else(|error| panic!("get_module_info({MODULE}, {name}): {error:?}"))
    })
}

/// `erlang:get_module_info(MODULE)` — the full five-item list.
fn full(rt: &mut Runtime) -> Eterm {
    let m = rt.intern(MODULE);
    with_context(rt, |ctx| {
        ctx.call_bif("erlang", "get_module_info", 1, &[m])
            .expect("get_module_info(slice)")
    })
}

/// The atom name of `term` as the runtime's shared table spells it.
fn atom_name(rt: &Runtime, term: Eterm) -> String {
    rt.atom_name_of(term)
        .unwrap_or_else(|| panic!("{term:?} is not a named atom"))
        .to_string()
}

/// A `{Function, Arity}` 2-tuple split into `("name", arity)`.
fn callable(rt: &Runtime, pair: Eterm) -> (String, i64) {
    assert_eq!(terms::tuple_arity(pair).expect("a tuple"), 2, "a 2-tuple");
    let name = atom_name(rt, terms::tuple_element(pair, 0).expect("element 0"));
    let arity = terms::tuple_element(pair, 1).expect("element 1");
    assert!(is_small(arity), "an arity is a small integer: {arity:?}");
    (name, signed_value(arity))
}

/// A `[{Function, Arity}]` list rendered as `name/arity` strings, in the list's order.
fn callables(rt: &Runtime, list: Eterm) -> Vec<String> {
    terms::list_elements(list)
        .expect("a proper callable list")
        .into_iter()
        .map(|pair| {
            let (name, arity) = callable(rt, pair);
            format!("{name}/{arity}")
        })
        .collect()
}

/// A `[{Key, Value}]` list's keys, in order.
fn pair_keys(rt: &Runtime, list: Eterm) -> Vec<String> {
    terms::list_elements(list)
        .expect("a proper list of pairs")
        .into_iter()
        .map(|pair| {
            assert_eq!(terms::tuple_arity(pair).expect("a tuple"), 2, "a 2-tuple");
            atom_name(rt, terms::tuple_element(pair, 0).expect("the key"))
        })
        .collect()
}

/// The value of `key` in a `[{Key, Value}]` list.
fn pair_value(rt: &Runtime, list: Eterm, key: &str) -> Option<Eterm> {
    terms::list_elements(list)
        .expect("a proper list of pairs")
        .into_iter()
        .find(|pair| {
            let k = terms::tuple_element(*pair, 0).expect("the key");
            rt.atom_name_of(k) == Some(key)
        })
        .map(|pair| terms::tuple_element(pair, 1).expect("the value"))
}

/// A binary as lowercase hex.
fn hex(term: Eterm) -> String {
    terms::binary_bytes(term)
        .expect("a byte-aligned binary")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The most general "list of callables" comparison: `[]`, `[a]`, `[a,b]`, `[b,a]` all render
/// to the same comma-joined string, so an order divergence is not silently invisible — the
/// caller always asserts the *set* through a sort and, where the oracle's order is
/// reproducible, the order too.
fn sorted<S: AsRef<str>>(values: &[S]) -> Vec<String> {
    let mut v: Vec<String> = values.iter().map(|s| s.as_ref().to_string()).collect();
    v.sort();
    v
}
// ---------------------------------------------------------------------------------------
// `/1` — the full five-item list
// ---------------------------------------------------------------------------------------

/// `get_module_info/1` is the oracle's five-item list in the oracle's key order —
/// `module, exports, attributes, compile, md5` — with the `module` item being the module
/// name atom, and each value agreeing with the corresponding `/2` item.
#[test]
fn the_full_form_is_the_oracles_five_item_list() {
    let mut rt = runtime_with_slice();
    let m = rt.intern(MODULE);
    let value = full(&mut rt);

    let items: Vec<Eterm> = terms::list_elements(value).expect("a proper list");
    assert_eq!(items.len(), 5, "the oracle's FULL_ITEMS are exactly five");

    let keys: Vec<String> = items
        .iter()
        .map(|entry| atom_name(&rt, terms::tuple_element(*entry, 0).expect("the key")))
        .collect();
    assert_eq!(
        keys,
        vec!["module", "exports", "attributes", "compile", "md5"],
        "the oracle's key order (`module_info_0:5962-5992`)"
    );

    let values: Vec<Eterm> = items
        .iter()
        .map(|entry| terms::tuple_element(*entry, 1).expect("the value"))
        .collect();

    assert_eq!(values[0], m, "the `module` item is the module-name atom");
    assert_eq!(hex(values[4]), SLICE_MD5_HEX, "the md5 of `slice.beam`");

    // Every item agrees with its `/2` counterpart, so the two arities cannot drift apart.
    // The comparisons are **structural** (`==` on a boxed `Eterm` is pointer identity, and
    // the two arities build independent terms): the callable lists are canonicalized and the
    // chunk items rendered, while the md5 is compared byte for byte.
    let slash_two_exports = item(&mut rt, "exports");
    let slash_two_attributes = item(&mut rt, "attributes");
    let slash_two_compile = item(&mut rt, "compile");
    let slash_two_md5 = item(&mut rt, "md5");
    assert_eq!(
        sorted(&callables(&rt, values[1])),
        sorted(&callables(&rt, slash_two_exports)),
        "the `exports` item agrees with `exports`"
    );
    let from_full = render(&mut rt, values[2]);
    let from_slash_two = render(&mut rt, slash_two_attributes);
    assert_eq!(from_full, from_slash_two, "the `attributes` item");
    let from_full = render(&mut rt, values[3]);
    let from_slash_two = render(&mut rt, slash_two_compile);
    assert_eq!(from_full, from_slash_two, "the `compile` item");
    assert_eq!(hex(values[4]), hex(slash_two_md5), "the `md5` item");
}
// ---------------------------------------------------------------------------------------
// `/2` — every item of the C's nine-atom dispatch chain
// ---------------------------------------------------------------------------------------

/// Each of the nine `/2` items answers the oracle's value: `module` is the name atom,
/// `exports` the real export set (as a **set** — see the ordering note above), `functions`
/// every function including the private `fib/3`, `nifs`/`native_addresses` the empty list and
/// `native` the atom `false` (no NIF library, no native code), `md5` the 16-byte digest, and
/// `attributes`/`compile` the decoded `Attr`/`CInf` chunks.
#[test]
fn each_slash_two_item_answers_the_oracles_value() {
    let mut rt = runtime_with_slice();
    let m = rt.intern(MODULE);

    // `module` — the module-name atom (`get_module_info:5932-5934`).
    let module_item = item(&mut rt, "module");
    assert_eq!(module_item, m, "the `module` item");

    // `exports` — the real export set, `module_info/0,1` included, private `fib/3` excluded.
    let export_list = item(&mut rt, "exports");
    assert_eq!(
        sorted(&callables(&rt, export_list)),
        sorted(&SLICE_EXPORTS),
        "the module's real export set"
    );

    // `functions` — every function the module defines, in the oracle's (IR/code) order; the
    // private `fib/3` helper is listed here and not in `exports`.
    let function_list = item(&mut rt, "functions");
    assert_eq!(
        callables(&rt, function_list),
        SLICE_FUNCTIONS.to_vec(),
        "every function in the oracle's order, `module_info/0,1` last"
    );

    // No NIF library is loaded and no native code is emitted.
    let nifs = item(&mut rt, "nifs");
    let native_addresses = item(&mut rt, "native_addresses");
    let native = item(&mut rt, "native");
    assert_eq!(nifs, beam_term::NIL, "`nifs` is []");
    assert_eq!(native_addresses, beam_term::NIL, "`native_addresses` is []");
    assert_eq!(
        atom_name(&rt, native),
        "false",
        "`native` is false with no native code"
    );

    // `md5` — the 16-byte digest of the `.beam` file, matching the oracle exactly.
    let md5 = item(&mut rt, "md5");
    assert_eq!(
        terms::binary_bytes(md5).expect("a byte-aligned binary").len(),
        16,
        "an MD5 digest is 16 bytes"
    );
    assert_eq!(hex(md5), SLICE_MD5_HEX, "the oracle's digest");

    // `attributes` — the decoded `Attr` chunk: the fixture's `-vsn(…)` appears as
    // `[{vsn,[<big>]}]`, the atom resolved by name into the caller's shared table.
    let attributes = item(&mut rt, "attributes");
    assert_eq!(pair_keys(&rt, attributes), vec!["vsn"], "the `Attr` chunk");
    let vsn = pair_value(&rt, attributes, "vsn").expect("the `vsn` attribute");
    let vsn_list = terms::list_elements(vsn).expect("`vsn` is a list");
    assert_eq!(vsn_list.len(), 1, "one value, got {vsn_list:?}");
    assert!(
        !is_small(vsn_list[0]) || signed_value(vsn_list[0]) >= 0,
        "a `vsn` value, got {:?}",
        vsn_list[0]
    );

    // `compile` — the decoded `CInf` chunk: `[{version,"10.0.6"}]`.
    let compile = item(&mut rt, "compile");
    assert_eq!(
        pair_keys(&rt, compile),
        vec!["version"],
        "the `CInf` chunk"
    );
    let version = pair_value(&rt, compile, "version").expect("the `version` pair");
    let rendered: String = terms::list_elements(version)
        .expect("a version string is a char list")
        .into_iter()
        .map(|c| {
            assert!(is_small(c), "a char is a small integer: {c:?}");
            char::from(u8::try_from(signed_value(c)).expect("an ASCII char"))
        })
        .collect();
    assert_eq!(rendered, "10.0.6", "the compiler version the oracle reports");
}

// ---------------------------------------------------------------------------------------
// The failure paths — the oracle's plain `badarg`
// ---------------------------------------------------------------------------------------

/// A module the runtime has not loaded is the oracle's plain `badarg`, for both arities.
#[test]
fn an_unknown_module_is_badarg() {
    let mut rt = runtime_with_slice();
    let absent = rt.intern("no_such_module_here_xyz");
    let module_item = rt.intern("module");
    let (slash_two, slash_one) = with_context(&mut rt, |ctx| {
        let slash_two = ctx
            .call_bif("erlang", "get_module_info", 2, &[absent, module_item])
            .expect_err("an unloaded module is badarg");
        let slash_one = ctx
            .call_bif("erlang", "get_module_info", 1, &[absent])
            .expect_err("an unloaded module is badarg for /1 too");
        (slash_two, slash_one)
    });
    assert_badarg(slash_two);
    assert_badarg(slash_one);
}

/// An item atom outside the C's nine-atom chain is the oracle's plain `badarg`.
#[test]
fn an_unknown_item_is_badarg() {
    let mut rt = runtime_with_slice();
    let m = rt.intern(MODULE);
    let unknown = rt.intern("no_such_item_xyz");
    let error = with_context(&mut rt, |ctx| {
        ctx.call_bif("erlang", "get_module_info", 2, &[m, unknown])
            .expect_err("an unknown item is badarg")
    });
    assert_badarg(error);
}

/// A **non-atom** module or item is `badarg` too (the oracle's `is_not_atom` checks run before
/// anything else, `module_info_1:6002-6009`).
#[test]
fn a_non_atom_module_or_item_is_badarg() {
    let mut rt = runtime_with_slice();
    let m = rt.intern(MODULE);
    let module_item = rt.intern("module");
    let not_an_atom = make_small(42);
    let (bad_module, bad_item) = with_context(&mut rt, |ctx| {
        let bad_module = ctx
            .call_bif("erlang", "get_module_info", 2, &[not_an_atom, module_item])
            .expect_err("a non-atom module is badarg");
        let bad_item = ctx
            .call_bif("erlang", "get_module_info", 2, &[m, not_an_atom])
            .expect_err("a non-atom item is badarg");
        (bad_module, bad_item)
    });
    assert_badarg(bad_module);
    assert_badarg(bad_item);
}

// ---------------------------------------------------------------------------------------
// The live-oracle cross-check
// ---------------------------------------------------------------------------------------

/// The same fixture, answered by the live system `erl`: the two callable lists (`functions`
/// in order, `exports` as a sorted set, per the ordering note), the `md5` digest, and the
/// trivial `module`/`nifs`/`native`/`native_addresses` items must all agree. Skips loudly
/// without a toolchain.
#[test]
fn the_answers_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("module_info") else {
        return;
    };
    let dir = Path::new(SLICE_DIR);

    let expr = "\
{module,slice} = code:ensure_loaded(slice),
F = erlang:get_module_info(slice, functions),
E = erlang:get_module_info(slice, exports),
M = erlang:get_module_info(slice, md5),
N = erlang:get_module_info(slice, nifs),
A = erlang:get_module_info(slice, native_addresses),
T = erlang:get_module_info(slice, native),
K = erlang:get_module_info(slice, module),
X = erlang:get_module_info(slice, attributes),
C = erlang:get_module_info(slice, compile),
io:format(\"functions=~s~n\",[lists:join(\",\",[atom_to_list(Fn)++\"/\"++integer_to_list(Ar)||{Fn,Ar}<-F])]),
io:format(\"exports=~s~n\",[lists:join(\",\",lists:sort([atom_to_list(Fn)++\"/\"++integer_to_list(Ar)||{Fn,Ar}<-E]))]),
io:format(\"md5=~s~n\",[lists:flatten([io_lib:format(\"~2.16.0b\",[B])||B<-binary_to_list(M)])]),
io:format(\"nifs=~p~n\",[N]),
io:format(\"native_addresses=~p~n\",[A]),
io:format(\"native=~p~n\",[T]),
io:format(\"module=~p~n\",[K]),
io:format(\"attributes=~p~n\",[X]),
io:format(\"compile=~p~n\",[C]),
halt(0).";
    let Some(rendered) = support::oracle_eval(&oracle, Some(dir), expr) else {
        eprintln!("SKIP the live-oracle comparison: the erl probe could not answer");
        return;
    };
    let oracle_lines: Vec<String> = rendered.lines().map(|l| l.trim().to_string()).collect();

    let mut rt = runtime_with_slice();
    let functions = item(&mut rt, "functions");
    let exports = item(&mut rt, "exports");
    let md5 = item(&mut rt, "md5");
    let nifs = item(&mut rt, "nifs");
    let native_addresses = item(&mut rt, "native_addresses");
    let native = item(&mut rt, "native");
    let module_item = item(&mut rt, "module");
    let attributes = item(&mut rt, "attributes");
    let compile_item = item(&mut rt, "compile");

    let ours = vec![
        format!("functions={}", callables(&rt, functions).join(",")),
        format!("exports={}", sorted(&callables(&rt, exports)).join(",")),
        format!("md5={}", hex(md5)),
        format!("nifs={}", render(&mut rt, nifs)),
        format!("native_addresses={}", render(&mut rt, native_addresses)),
        format!("native={}", render(&mut rt, native)),
        format!("module={}", atom_name(&rt, module_item)),
        format!("attributes={}", render(&mut rt, attributes)),
        format!("compile={}", render(&mut rt, compile_item)),
    ];
    assert_eq!(
        ours, oracle_lines,
        "every cross-checkable `get_module_info` answer must match the live oracle"
    );
}

/// Render a term through the runtime's own `~p` printer (the registered `io:format/2` BIF),
/// so the trivial items (`[]`, atoms) are compared without re-deriving their shape. The
/// printer terminates its output with a newline, which `~p` in the oracle probe does not, so
/// the rendering is trimmed.
fn render(rt: &mut Runtime, term: Eterm) -> String {
    rt.format_term(term, None)
        .expect("the runtime's printer renders the item")
        .trim()
        .to_string()
}
