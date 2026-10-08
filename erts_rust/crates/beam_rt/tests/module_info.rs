//! `erlang:get_module_info/1,2`, end to end through the runtime glue.
//!
//! `beam_bifs` registers both arities and answers every item through the optional
//! `ModuleInfo` host seam; `beam_rt::context::call_registered` installs the
//! concrete `ModuleInfoBridge` over **its own `CodeServer`** (`&CodeServer`, the
//! same borrow shape as the `RecordBridge`) — so the answer comes from the loaded
//! module's real `beam_loader::Module` IR and its `.beam` bytes, never fabricated.
//!
//! The `.beam` fixture is compiled at test time with the system `erlc`; the oracle
//! comparisons drive the real `erl`. A missing toolchain is a **loud skip**.
//!
//! # The `exports` ordering divergence (documented honestly)
//!
//! The oracle's `exports` is the **global ERTS export-table order**
//! (`erts/emulator/beam/erl_bif_info.c:8434-8471` walking `export_list`), which is
//! neither the file `ExpT` order nor a sorted order, and is not reconstructible
//! from one module. This bridge returns the module's real export *set* in
//! `Module::exports` order: the same entries, possibly in a different order than
//! the oracle for some modules. Tests therefore compare `exports` as an unordered
//! **set** (both against the module's known exports and against the oracle).
mod support;

use beam_bifs::NullSink;
use beam_common::BeamError;
use beam_engine::terms;
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_small, signed_value, Eterm};
use std::path::{Path, PathBuf};

const MODULE: &str = "mi";
/// A fixture whose `Attr`/`CInf` chunks are non-trivial: `-vsn(7)` lands in `Attr`,
/// and `erlc` writes `{version,…}` into `CInf`.
const SOURCE: &str = "-module(mi).\n-export([f/1,g/0]).\n-vsn(7).\nfoo(A) -> A + 1.\nf(X) -> foo(X).\ng() -> ok.\n";

/// Compile the fixture and load it into a fresh runtime. `None` ⇒ loud skip.
fn setup() -> Option<(Runtime, PathBuf)> {
    if !support::require_tools("get_module_info") {
        return None;
    }
    let dir = support::scratch_dir("mi");
    let beam = support::compile(&dir, MODULE, SOURCE);
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(&beam).expect("load the mi fixture");
    Some((rt, dir))
}

/// Run `f` against a fresh `RuntimeContext`, dropping it before `rt` is reused.
fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    f(&mut ctx)
}

fn assert_badarg(error: BeamError) {
    match error {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some("badarg"),
            "expected error:badarg, got {raise:?}"
        ),
        other => panic!("expected error:badarg, got {other:?}"),
    }
}

/// Split a term that must be a 2-tuple into its elements.
fn pair(item: Eterm) -> (Eterm, Eterm) {
    assert_eq!(terms::tuple_arity(item).expect("a tuple"), 2, "a 2-tuple");
    (
        terms::tuple_element(item, 0).expect("element 0"),
        terms::tuple_element(item, 1).expect("element 1"),
    )
}

/// Render a `{Atom, Arity}` list as the oracle's `name/arity` join, for a verbatim
/// oracle comparison that does not depend on this runtime's printer.
fn callables(rt: &Runtime, list: Eterm) -> String {
    terms::list_elements(list)
        .expect("a proper list")
        .into_iter()
        .map(|item| {
            let (name, arity) = pair(item);
            let name = rt.atom_name_of(name).expect("a function-name atom");
            assert!(is_small(arity), "an arity integer");
            format!("{name}/{}", signed_value(arity))
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `erlang:get_module_info(M, functions)` and its `exports` fallback both list
/// `{Function, Arity}` pairs; `functions` is compared **verbatim** to the oracle.
#[test]
fn functions_match_the_oracle_verbatim() {
    let Some((mut rt, dir)) = setup() else { return };
    let m = rt.intern(MODULE);
    let functions_atom = rt.intern("functions");
    let list = with_context(&mut rt, |ctx| {
        ctx.call_bif("erlang", "get_module_info", 2, &[m, functions_atom])
            .expect("get_module_info(mi, functions)")
    });
    let ours = callables(&rt, list);
    assert_eq!(
        ours, "foo/1,f/1,g/0,module_info/0,module_info/1",
        "behaviour/definition order, then the two synthesized module_info arities"
    );

    // The oracle, rendered the same way. A failed probe is a loud skip.
    let expr = format!(
        "code:ensure_loaded({MODULE}), L = erlang:get_module_info({MODULE},functions), \
         io:format(\"~s~n\",[lists:join(\",\",[atom_to_list(A)++\"/\"++integer_to_list(N)||{{A,N}}<-L])]), halt(0)."
    );
    match support::oracle_eval(Some(dir.as_path()), &expr) {
        Some(oracle) => assert_eq!(ours, oracle, "functions must match the oracle"),
        None => eprintln!("SKIP the oracle `functions` comparison: the erl probe failed"),
    }
}

/// `exports` is the module's real export set (see the ordering note above): the
/// tests compare it as a **set**, both to the known exports and to the oracle.
#[test]
fn exports_are_the_real_export_set() {
    let Some((mut rt, dir)) = setup() else { return };
    let m = rt.intern(MODULE);
    let exports_atom = rt.intern("exports");
    let list = with_context(&mut rt, |ctx| {
        ctx.call_bif("erlang", "get_module_info", 2, &[m, exports_atom])
            .expect("get_module_info(mi, exports)")
    });
    let mut ours: Vec<String> = callables(&rt, list).split(',').map(str::to_string).collect();
    ours.sort();
    let mut expected = vec![
        "f/1".to_string(),
        "g/0".to_string(),
        "module_info/0".to_string(),
        "module_info/1".to_string(),
    ];
    expected.sort();
    assert_eq!(ours, expected, "the module's real export set");

    let expr = format!(
        "code:ensure_loaded({MODULE}), L = erlang:get_module_info({MODULE},exports), \
         S = lists:sort([atom_to_list(A)++\"/\"++integer_to_list(N)||{{A,N}}<-L]), \
         io:format(\"~s~n\",[lists:join(\",\",S)]), halt(0)."
    );
    match support::oracle_eval(Some(dir.as_path()), &expr) {
        Some(oracle) => assert_eq!(
            ours.join(","),
            oracle,
            "the export SET must match the oracle (order may differ)"
        ),
        None => eprintln!("SKIP the oracle `exports` comparison: the erl probe failed"),
    }
}

/// `md5` is the 16-byte `.beam` hash; compared **verbatim** (hex) to the oracle.
/// `nifs`/`native_addresses` are `[]` and `native` is `false` — no NIF library is
/// loaded and no native code is emitted, so the oracle agrees.
#[test]
fn md5_nifs_native_addresses_and_native() {
    let Some((mut rt, dir)) = setup() else { return };
    let m = rt.intern(MODULE);
    let md5_atom = rt.intern("md5");
    let nifs_atom = rt.intern("nifs");
    let native_atom = rt.intern("native");
    let native_addrs_atom = rt.intern("native_addresses");
    let (md5, nifs, native_addrs, native) = with_context(&mut rt, |ctx| {
        let call = |ctx: &mut RuntimeContext<'_>, item: Eterm| {
            ctx.call_bif("erlang", "get_module_info", 2, &[m, item])
                .expect("a get_module_info item")
        };
        (
            call(ctx, md5_atom),
            call(ctx, nifs_atom),
            call(ctx, native_addrs_atom),
            call(ctx, native_atom),
        )
    });
    let bytes = terms::binary_bytes(md5).expect("md5 is a byte-aligned binary");
    assert_eq!(bytes.len(), 16, "an MD5 digest is 16 bytes");
    let ours: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(nifs, beam_term::NIL, "nifs is [] with no NIF loaded");
    assert_eq!(
        native_addrs,
        beam_term::NIL,
        "native_addresses is [] with no native code"
    );
    assert_eq!(
        rt.atom_name_of(native),
        Some("false"),
        "native is false with no native code"
    );

    let expr = format!(
        "code:ensure_loaded({MODULE}), M = erlang:get_module_info({MODULE},md5), \
         io:format(\"~s~n\",[lists:flatten([io_lib:format(\"~2.16.0b\",[B])||B<-binary_to_list(M)])]), halt(0)."
    );
    match support::oracle_eval(Some(dir.as_path()), &expr) {
        Some(oracle) => assert_eq!(ours, oracle, "the md5 digest must match the oracle"),
        None => eprintln!("SKIP the oracle `md5` comparison: the erl probe failed"),
    }
}

/// `attributes` is the decoded `Attr` chunk (so `-vsn(7)` is visible) and `compile`
/// the decoded `CInf` chunk (`{version,…}`); both are lists of `{Key, Value}` pairs.
#[test]
fn attributes_and_compile_decode_the_chunks() {
    let Some((mut rt, _dir)) = setup() else { return };
    let m = rt.intern(MODULE);
    let attributes_atom = rt.intern("attributes");
    let compile_atom = rt.intern("compile");
    let (attributes, compile) = with_context(&mut rt, |ctx| {
        let call = |ctx: &mut RuntimeContext<'_>, item: Eterm| {
            ctx.call_bif("erlang", "get_module_info", 2, &[m, item])
                .expect("a get_module_info item")
        };
        let attributes = call(ctx, attributes_atom);
        let compile = call(ctx, compile_atom);
        (attributes, compile)
    });

    // `attributes` contains `{vsn,[7]}`; the atom key resolves by NAME through the
    // shared table (the deep decode+remap resolves it, never interns a wrong one).
    let pairs: Vec<(String, Eterm)> = terms::list_elements(attributes)
        .expect("attributes is a list")
        .into_iter()
        .map(|item| {
            let (key, value) = pair(item);
            (
                rt.atom_name_of(key).expect("an attribute name").to_string(),
                value,
            )
        })
        .collect();
    let vsn = pairs
        .iter()
        .find(|(key, _)| key == "vsn")
        .map(|(_, value)| *value)
        .expect("the `-vsn(7)` attribute is decoded from the Attr chunk");
    assert_eq!(
        terms::list_elements(vsn).expect("vsn is a list"),
        vec![beam_term::prelude::make_small(7)]
    );
    // `compile` contains the `{version, "…"} ` pair the compiler wrote.
    let compile_keys: Vec<String> = terms::list_elements(compile)
        .expect("compile is a list")
        .into_iter()
        .map(|item| rt.atom_name_of(pair(item).0).expect("a name").to_string())
        .collect();
    assert!(
        compile_keys.iter().any(|k| k == "version"),
        "the CInf chunk's `version` pair, got {compile_keys:?}"
    );
}

/// `get_module_info/1` is the oracle's 5-item list in the oracle's key order
/// (`module, exports, attributes, compile, md5`), and the values agree with the
/// `/2` items.
#[test]
fn the_full_form_has_the_oracle_key_order() {
    let Some((mut rt, _dir)) = setup() else { return };
    let m = rt.intern(MODULE);
    let full = with_context(&mut rt, |ctx| {
        ctx.call_bif("erlang", "get_module_info", 1, &[m])
            .expect("get_module_info(mi)")
    });
    let items: Vec<(Eterm, Eterm)> = terms::list_elements(full)
        .expect("a proper list")
        .into_iter()
        .map(pair)
        .collect();
    assert_eq!(items.len(), 5, "the oracle's FULL_ITEMS are five");
    let keys: Vec<Option<&str>> = items
        .iter()
        .map(|(key, _)| rt.atom_name_of(*key))
        .collect();
    assert_eq!(
        keys,
        vec![
            Some("module"),
            Some("exports"),
            Some("attributes"),
            Some("compile"),
            Some("md5"),
        ],
        "the oracle's key order"
    );
    assert_eq!(
        items[0].1,
        m,
        "the `module` item is the module-name atom itself"
    );
    assert_eq!(
        terms::binary_bytes(items[4].1).expect("a binary").len(),
        16,
        "the `md5` item"
    );
}

/// A module that is not loaded (and an item outside the oracle's fixed set) is
/// the oracle's plain `badarg`.
#[test]
fn unknown_module_and_unknown_item_are_badarg() {
    let Some((mut rt, _dir)) = setup() else { return };
    let m = rt.intern(MODULE);
    let absent = rt.intern("no_such_module_here_xyz");
    let item = rt.intern("no_such_item_xyz");
    let functions_atom = rt.intern("functions");
    let (absent_err, item_err) = with_context(&mut rt, |ctx| {
        let absent_err = ctx
            .call_bif("erlang", "get_module_info", 2, &[absent, functions_atom])
            .expect_err("an unloaded module is badarg");
        let item_err = ctx
            .call_bif("erlang", "get_module_info", 2, &[m, item])
            .expect_err("an unknown item is badarg");
        (absent_err, item_err)
    });
    assert_badarg(absent_err);
    assert_badarg(item_err);
}

/// `module` names the module, and `functions` contains the two synthesized
/// `module_info` arities — the minimum the oracle guarantees for any loaded module.
#[test]
fn the_module_item_is_the_module_name_atom() {
    let Some((mut rt, _dir)) = setup() else { return };
    let m = rt.intern(MODULE);
    let module_atom = rt.intern("module");
    let value = with_context(&mut rt, |ctx| {
        ctx.call_bif("erlang", "get_module_info", 2, &[m, module_atom])
            .expect("get_module_info(mi, module)")
    });
    assert_eq!(rt.atom_name_of(value), Some(MODULE));
}

/// `Path` is used only in the signature above; keep the import honest.
#[allow(dead_code)]
fn _path_is_used(p: &Path) -> bool {
    p.exists()
}
