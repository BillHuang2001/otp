//! The **headline Milestone-1 evidence**: the widened typed-IR interpreter runs
//! real, non-trivial **system stdlib** Erlang modules.
//!
//! Every other engine integration test drives a purpose-built fixture produced
//! by `erlc` from the engine's own sources. This one instead loads the
//! **stdlib `lists` module shipped with the running Erlang** (OTP 29 /
//! `erts-17.1`, the differential oracle) straight from
//! `<stdlib>/ebin/lists.beam`, plus a thin harness (`stdlists_harness`) whose
//! every export forwards to a real `lists` function — including two that pass an
//! **anonymous fun** into `lists:map/2` / `lists:filter/2`.
//!
//! This forces the engine, end to end, to:
//!
//! * load a large real module (~247 functions) and run its exports on a fresh
//!   [`Machine`] (`lists:reverse/1`, `lists:seq/2`, `lists:sort/1`, …);
//! * dispatch the `lists`↔`harness` **external calls** the wrappers make;
//! * resolve and apply a **cross-module closure** (`make_fun3` in the harness,
//!   `call_fun2` in `lists`) — the `ExecutionContext::apply_fun` seam;
//! * keep **one shared term heap and one shared atom table** across modules, so a
//!   `true`/`false` a predicate returns is recognised by `lists:filter`
//!   (see `support`).
//!
//! Every expected value is the **real** system ERTS result, cross-checked with
//! `erl`:
//!
//! ```text
//! lists:reverse([1,2,3])                       = [3,2,1]
//! lists:seq(1,5)                               = [1,2,3,4,5]
//! stdlists_harness:reverse([10,20,30])         = [30,20,10]
//! stdlists_harness:seq(2,5)                    = [2,3,4,5]
//! stdlists_harness:map_double([1,2,3])         = [2,4,6]
//! stdlists_harness:sort([3,1,2])               = [1,2,3]
//! stdlists_harness:filter_pos([-1,0,2,-3,5])   = [2,5]
//! ordsets:from_list([3,1,2])                   = [1,2,3]
//! ordsets:union([1,2],[2,3])                   = [1,2,3]
//! ordsets:intersection([1,2],[2,3])            = [2]
//! ordsets:subtract([1,2,3],[1,2])              = [3]
//! ordsets:is_element(2,[2,3])                  = true
//! ordsets:is_element(7,[2,3])                  = false
//! proplists:get_value(a,[{a,1},{b,2}])         = 1
//! proplists:get_value(c,[{a,1},{b,2}],9)       = 9
//! proplists:get_value(c,[{a,1},{b,2}])         = undefined
//! proplists:lookup_all(a,[{a,1},{b,2}])        = [{a,1}]
//! proplists:get_bool(a,[{a,1},{b,2}])          = false
//! ```
//!
//! Three real stdlib modules run: **`lists`** directly and through the harness,
//! **`ordsets`** (pure Erlang over `lists:usort`/`umerge`/`foldl`/`filter`), and
//! **`proplists`** (tuple access through `tuple_size/1` + `element/2`).
//!
//! The stdlib modules are located at **test time** (never a baked-in path); when
//! `erl`/stdlib is unavailable each test **skips loudly** rather than failing.

mod support;

use beam_engine::terms;
use support::{
    assert_atom, assert_int_list, assert_keyed_list, int_list, keyed_tuple, skip_reason,
    stdlib_ebin, ModuleHost,
};

/// The committed harness fixture (a real `.beam` from the system `erlc`).
const HARNESS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/stdlists_harness.beam"
);

/// A host over the system stdlib `ebin` with the harness registered.
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `require_stdlib`)");
    let mut host = ModuleHost::new(ebin);
    host.register("stdlists_harness", HARNESS_BEAM);
    host
}

/// Skips the calling test (loudly) when the oracle stdlib is unavailable;
/// returns `true` when the caller should return early.
fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Running the system `lists` module directly
// ---------------------------------------------------------------------------

#[test]
fn stdlib_lists_reverse_runs_the_real_module() {
    // `lists:reverse/1` for a three-element list delegates to the `lists`
    // `reverse/2` NIF, which this host serves — so the whole real function runs.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[1, 2, 3]);
    let result = host
        .call("lists", "reverse", &[input])
        .expect("lists:reverse/1 must run");
    assert_int_list(&host, result, &[3, 2, 1]);
}

#[test]
fn stdlib_lists_seq_runs_the_real_module() {
    // `lists:seq/2` guards on integers, computes the length with `-`/`+`, then
    // loops in the real `lists:seq_loop/3`.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let result = host
        .call("lists", "seq", &[beam_term::make_small(1), beam_term::make_small(5)])
        .expect("lists:seq/2 must run");
    assert_int_list(&host, result, &[1, 2, 3, 4, 5]);
}

// ---------------------------------------------------------------------------
// The harness wrappers (external calls into `lists`)
// ---------------------------------------------------------------------------

#[test]
fn harness_reverse_forwards_to_stdlib_lists() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[10, 20, 30]);
    let result = host
        .call("stdlists_harness", "reverse", &[input])
        .expect("stdlists_harness:reverse/1 must run");
    assert_int_list(&host, result, &[30, 20, 10]);
}

#[test]
fn harness_seq_forwards_to_stdlib_lists() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let result = host
        .call(
            "stdlists_harness",
            "seq",
            &[beam_term::make_small(2), beam_term::make_small(5)],
        )
        .expect("stdlists_harness:seq/2 must run");
    assert_int_list(&host, result, &[2, 3, 4, 5]);
}

#[test]
fn harness_map_double_applies_a_closure_over_stdlib_lists_map() {
    // The headline cross-module case: the harness builds `fun(X) -> X * 2 end`
    // and hands it to the real `lists:map/2`, whose `call_fun2` reaches back into
    // the harness through `ExecutionContext::apply_fun`.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[1, 2, 3]);
    let result = host
        .call("stdlists_harness", "map_double", &[input])
        .expect("stdlists_harness:map_double/1 must run");
    assert_int_list(&host, result, &[2, 4, 6]);
}

#[test]
fn harness_sort_runs_the_real_lists_sort() {
    // `lists:sort/1` is a hand-written merge sort over `test` opcodes only — no
    // BIFs, no funs — so this exercises the interpreter's control flow on a real
    // module's recursive helpers.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[3, 1, 2]);
    let result = host
        .call("stdlists_harness", "sort", &[input])
        .expect("stdlists_harness:sort/1 must run");
    assert_int_list(&host, result, &[1, 2, 3]);
}

#[test]
fn harness_filter_pos_matches_the_predicate_result_across_modules() {
    // `lists:filter/2` calls the harness predicate (a closure returning a
    // `true`/`false` atom) and then `select_val`s on those atoms **in `lists`** —
    // which is exactly what the shared atom table makes consistent.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[-1, 0, 2, -3, 5]);
    let result = host
        .call("stdlists_harness", "filter_pos", &[input])
        .expect("stdlists_harness:filter_pos/1 must run");
    assert_int_list(&host, result, &[2, 5]);
}

// ---------------------------------------------------------------------------
// A second and a third real stdlib module: `ordsets` and `proplists`
// ---------------------------------------------------------------------------

#[test]
fn stdlib_ordsets_from_list_sorts_and_dedups() {
    // `ordsets:from_list/1` is pure Erlang that delegates to `lists:usort/1`
    // (and `lists:umerge/1`), so this drives a second real stdlib module *and*
    // the `ordsets` -> `lists` external-call edge.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let input = int_list(&mut host, &[3, 1, 2]);
    let result = host
        .call("ordsets", "from_list", &[input])
        .expect("ordsets:from_list/1 must run");
    assert_int_list(&host, result, &[1, 2, 3]);
}

#[test]
fn stdlib_ordsets_union_intersection_and_subtract() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[2, 3]);
    let union = host
        .call("ordsets", "union", &[a, b])
        .expect("ordsets:union/2 must run");
    assert_int_list(&host, union, &[1, 2, 3]);

    let a = int_list(&mut host, &[1, 2]);
    let b = int_list(&mut host, &[2, 3]);
    let intersection = host
        .call("ordsets", "intersection", &[a, b])
        .expect("ordsets:intersection/2 must run");
    assert_int_list(&host, intersection, &[2]);

    let a = int_list(&mut host, &[1, 2, 3]);
    let b = int_list(&mut host, &[1, 2]);
    let difference = host
        .call("ordsets", "subtract", &[a, b])
        .expect("ordsets:subtract/2 must run");
    assert_int_list(&host, difference, &[3]);
}

#[test]
fn stdlib_ordsets_is_element_returns_the_shared_true_and_false() {
    // The `true`/`false` the real `ordsets` returns must be *the same atoms* the
    // shared table names — the invariant the shared atom table exists to keep.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let list = int_list(&mut host, &[2, 3]);

    let hit = host
        .call("ordsets", "is_element", &[beam_term::make_small(2), list])
        .expect("ordsets:is_element/2 must run");
    assert_atom(&host, hit, "true");

    let miss = host
        .call("ordsets", "is_element", &[beam_term::make_small(7), list])
        .expect("ordsets:is_element/2 must run");
    assert_atom(&host, miss, "false");
}

#[test]
fn stdlib_proplists_get_value_walks_the_property_list() {
    // `proplists:get_value/3` guards with `tuple_size/1` + `element/2` BIFs, so it
    // exercises tuple access across a third real stdlib module.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let a = keyed_tuple(&mut host, "a", 1);
    let b = keyed_tuple(&mut host, "b", 2);
    let props = terms::make_list(&mut host, &[a, b]).expect("build a proplist");
    let key_a = host.intern("a");

    let found = host
        .call("proplists", "get_value", &[key_a, props])
        .expect("proplists:get_value/2 must run");
    assert!(
        beam_term::cmp(found, beam_term::make_small(1), host.atom_table()).is_eq(),
        "get_value(a, [{{a,1}},{{b,2}}]) must be 1, got {found:?}"
    );

    // A missing key falls through to the default.
    let key_c = host.intern("c");
    let default = host
        .call("proplists", "get_value", &[key_c, props, beam_term::make_small(9)])
        .expect("proplists:get_value/3 must run");
    assert!(
        beam_term::cmp(default, beam_term::make_small(9), host.atom_table()).is_eq(),
        "get_value(c, props, 9) must be 9, got {default:?}"
    );

    // ... and `/2` uses the `undefined` default.
    let undefined = host
        .call("proplists", "get_value", &[key_c, props])
        .expect("proplists:get_value/2 must run");
    assert_atom(&host, undefined, "undefined");
}

#[test]
fn stdlib_proplists_lookup_all_and_get_bool() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let a = keyed_tuple(&mut host, "a", 1);
    let b = keyed_tuple(&mut host, "b", 2);
    let props = terms::make_list(&mut host, &[a, b]).expect("build a proplist");
    let key_a = host.intern("a");

    let matches = host
        .call("proplists", "lookup_all", &[key_a, props])
        .expect("proplists:lookup_all/2 must run");
    assert_keyed_list(&host, matches, &[("a", 1)]);

    let key_c = host.intern("c");
    let none = host
        .call("proplists", "lookup_all", &[key_c, props])
        .expect("proplists:lookup_all/2 must run");
    assert!(terms::list_elements(none).expect("a list").is_empty());

    // `get_bool/2` is `true` only when the value is literally `true`.
    let not_bool = host
        .call("proplists", "get_bool", &[key_a, props])
        .expect("proplists:get_bool/2 must run");
    assert_atom(&host, not_bool, "false");
}

// ---------------------------------------------------------------------------
// Robustness guards (do not depend on the oracle)
// ---------------------------------------------------------------------------

#[test]
fn skip_reason_and_stdlib_ebin_agree() {
    // A cheap consistency check: whenever a skip reason exists, `stdlib_ebin`
    // must be `None`, and vice versa — so the skip guard never masks a usable
    // stdlib.
    assert_eq!(skip_reason().is_some(), stdlib_ebin().is_none());
}

#[test]
fn a_missing_module_is_a_typed_error_not_a_panic() {
    // The host must never panic on an unknown module; the loader's `io`/`bad_file`
    // error surfaces unchanged.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let err = host.call("no_such_module_xyz", "f", &[]);
    assert!(err.is_err(), "an unknown module must be a typed error");
}

#[test]
fn one_shared_atom_table_resolves_names_across_modules() {
    // The invariant the whole approach rests on: a name interned while loading
    // one module resolves to the same index in every other loaded module's table.
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let lists = host.load("lists").expect("load lists");
    let harness = host.load("stdlists_harness").expect("load harness");
    let true_in_lists = lists.atom_table.lookup("true").expect("lists names true");
    let true_shared = host
        .atom_table()
        .lookup("true")
        .expect("the shared table names true");
    assert_eq!(true_in_lists, true_shared);
    // The harness never mentions `true`/`false`; the shared table still resolves
    // them for it at the same index `lists` uses.
    assert_eq!(
        harness.atom_table.name(true_shared),
        Some("true"),
        "the shared `true` index resolves by name in every module's table"
    );
    // The engine's exact comparison of the two atom terms agrees.
    let a = beam_term::make_atom(true_in_lists);
    let b = beam_term::make_atom(true_shared);
    assert!(beam_term::cmp(a, b, host.atom_table()).is_eq());
}

// Sanity: the helper forms used above compose (guards a silent regression in the
// `support` list helpers themselves).
#[test]
fn list_helpers_round_trip() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let list = int_list(&mut host, &[7, 8, 9]);
    assert_eq!(terms::list_elements(list).unwrap().len(), 3);
    assert_int_list(&host, list, &[7, 8, 9]);
}
