//! The seven `persistent_term:*` BIFs, end to end through the runtime glue.
//!
//! `beam_bifs`'s `persistent_term.rs` registers and renders all seven BIFs but cannot hold
//! the store itself: persistent terms are a **node-global** key/value table
//! (`the_hash_table`, `../../erts/emulator/beam/erl_bif_persistent.c:187`) outside every
//! process heap, updated under a table-wide lock with thread-progress hand-off — Stage-5
//! runtime state. Every operation therefore goes through the optional `PersistentTerm` host
//! seam, and `beam_rt::context::call_registered` installs the concrete `PersistentTermBridge`
//! over `Runtime`'s shared `Rc<RefCell<PersistentTermStore>>` (the port/NIF/ETS pattern).
//! This suite pins that whole path at the library boundary — the BIF's argument handling, the
//! `ok`/`true`/`false`/`badarg` outcomes, and the `get/0` list and `info/0` map shapes.
//!
//! # Oracle ground truth (OTP 29.1.1 / erts-17.1)
//!
//! ```text
//! $ erl -noshell -eval '
//!     P=persistent_term:put(mi_k,1), G=persistent_term:get(mi_k),
//!     D=persistent_term:get(mi_missing,default_v),
//!     E1=persistent_term:erase(mi_k), E2=persistent_term:erase(mi_k),
//!     PN1=persistent_term:put_new(mi_n,1), PN2=persistent_term:put_new(mi_n,1),
//!     io:format("~p ~p ~p ~p ~p ~p ~p ~p~n",[G,D,E1,E2,PN1,PN2,
//!         is_badarg(persistent_term:put_new(mi_n,2)),
//!         is_badarg(persistent_term:get(mi_missing))]),
//!     io:format("info=~p~n",[persistent_term:info()]), halt(0).' -s init stop
//! 1 default_v true false ok ok true true
//! info=#{count => 27,memory => 33352}
//! ```
//!
//! so `put/2` is `ok`, `erase/1` is **`true`/`false`** (not `ok`), `get/1` of an absent key
//! is `badarg`, `put_new/2` of a key holding a **different** value is `badarg` while the
//! same value is a no-op `ok`, and `info/0` is the flat map `#{count => N, memory => Bytes}`.
//! The oracle's `count`/`memory` include its own ~27 predefined persistent terms and its
//! literal-area overhead; this runtime's are its own bookkeeping (the entry count and the
//! heap-word total of the stored terms), so only the **shape** is compared in
//! [`the_goldens_match_the_live_oracle`].
//!
//! The suite needs no toolchain for its assertions; only the oracle comparison needs `erl`,
//! and it skips **loudly** without it.
mod support;

use beam_bifs::NullSink;
use beam_engine::{terms, ExecutionContext};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_small, make_small, signed_value, Eterm};

use support::assert_badarg;

/// A `Runtime` whose node-global persistent-term store starts empty.
fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// Run `f` against a fresh `RuntimeContext` over `rt`; the context (and its sink) are
/// dropped before `rt` is borrowed again.
fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    f(&mut ctx)
}

/// Call a `persistent_term:*` BIF through the runtime's `ExecutionContext` seam.
fn call(rt: &mut Runtime, name: &str, arity: u32, args: &[Eterm]) -> beam_common::Result<Eterm> {
    with_context(rt, |ctx| ctx.call_bif("persistent_term", name, arity, args))
}

/// An atom as the runtime names it, for the scalar comparison helpers below.
fn scalar(rt: &Runtime, term: Eterm) -> String {
    if is_small(term) {
        signed_value(term).to_string()
    } else {
        rt.atom_name_of(term)
            .unwrap_or_else(|| panic!("{term:?} is neither a small integer nor a named atom"))
            .to_string()
    }
}

// ---------------------------------------------------------------------------------------
// The seven BIFs
// ---------------------------------------------------------------------------------------

/// The complete golden sequence: `put/2` ⇒ `ok`, `get/1` ⇒ the value, `get/2` ⇒ the default,
/// `get/1` of an absent key ⇒ `badarg`, `put_new/2` onto a *different* value ⇒ `badarg` and
/// onto the *same* value ⇒ `ok`, `erase/1` ⇒ `true` then `false`, `get/0` ⇒ the stored
/// `{Key, Value}` pairs, `info/0` ⇒ `#{count => N, memory => Bytes}`.
#[test]
fn the_seven_bifs_follow_the_oracle() {
    let mut rt = runtime();
    let k = rt.intern("k");
    let missing = rt.intern("missing_key_xyz");
    let default = rt.intern("default_value_xyz");
    let ok_atom = rt.intern("ok");
    let true_term = rt.intern("true");
    let false_term = rt.intern("false");
    let one = make_small(1);
    let two = make_small(2);

    assert_eq!(
        call(&mut rt, "put", 2, &[k, one]).expect("put(k, 1)"),
        ok_atom,
        "put/2 returns ok"
    );
    assert_eq!(
        call(&mut rt, "get", 1, &[k]).expect("get(k)"),
        one,
        "get/1 returns the stored value"
    );
    assert_eq!(
        call(&mut rt, "get", 2, &[missing, default]).expect("get(missing, Default)"),
        default,
        "get/2 returns the default for an absent key"
    );
    assert_badarg(
        call(&mut rt, "get", 1, &[missing]).expect_err("get(missing) is badarg"),
    );
    assert_badarg(
        call(&mut rt, "put_new", 2, &[k, two]).expect_err("put_new onto a different value"),
    );
    assert_eq!(
        call(&mut rt, "put_new", 2, &[k, one]).expect("put_new onto the same value"),
        ok_atom,
        "put_new/2 of a key already holding that exact value is a no-op ok"
    );

    // `get/0` — the stored `{Key, Value}` pairs, in the store's (insertion) order. The
    // oracle's own order is an unspecified hash-table walk, so only this runtime's order
    // is pinned (insertion order, stable across an overwrite).
    let all = call(&mut rt, "get", 0, &[]).expect("get()");
    let pairs = terms::list_elements(all).expect("get/0 is a proper list");
    assert_eq!(pairs.len(), 1, "exactly the one stored entry: {pairs:?}");
    assert_eq!(terms::tuple_arity(pairs[0]).expect("a 2-tuple"), 2);
    assert_eq!(terms::tuple_element(pairs[0], 0).expect("the key"), k);
    assert_eq!(terms::tuple_element(pairs[0], 1).expect("the value"), one);

    // `info/0` — a **flat map** `#{count => N, memory => Bytes}` with this runtime's real
    // bookkeeping: one entry, and the heap-word total of the stored terms in bytes.
    let info = call(&mut rt, "info", 0, &[]).expect("info()");
    let entries = terms::map_entries(info).expect("info/0 is a map");
    assert_eq!(entries.len(), 2, "the `count` and `memory` keys: {entries:?}");
    let count = map_value(&rt, info, "count").expect("an info/0 `count` key");
    let memory = map_value(&rt, info, "memory").expect("an info/0 `memory` key");
    assert!(is_small(count), "count is an integer: {count:?}");
    assert_eq!(signed_value(count), 1, "one stored entry");
    assert!(is_small(memory), "memory is an integer: {memory:?}");
    assert!(signed_value(memory) >= 0, "memory is non-negative");

    // `erase/1` ⇒ `true` for a present key, then `false` (`erl_bif_persistent.c:539,548`).
    assert_eq!(
        call(&mut rt, "erase", 1, &[k]).expect("erase(k)"),
        true_term,
        "erase/1 of a present key is true"
    );
    assert_eq!(
        call(&mut rt, "erase", 1, &[k]).expect("erase(k) again"),
        false_term,
        "erase/1 of an absent key is false"
    );

    // The store is empty again: `get/0` ⇒ `[]` and `info/0`'s `count` ⇒ 0.
    assert_eq!(
        terms::list_elements(call(&mut rt, "get", 0, &[]).expect("get()")).expect("a list"),
        vec![],
        "the store is empty"
    );
    let empty_info = call(&mut rt, "info", 0, &[]).expect("info()");
    let count = map_value(&rt, empty_info, "count").expect("a `count` key");
    assert_eq!(signed_value(count), 0, "no entries left");
}

/// `put/2` of an existing key **overwrites in place** (never reorders), and a second,
/// distinct key appends — so `get/0` keeps insertion order.
#[test]
fn put_overwrites_in_place_and_get_all_keeps_insertion_order() {
    let mut rt = runtime();
    let k = rt.intern("k");
    let n = rt.intern("n");
    let ok_atom = rt.intern("ok");
    let one = make_small(1);
    let nine = make_small(9);

    assert_eq!(
        call(&mut rt, "put", 2, &[k, one]).expect("put(k, 1)"),
        ok_atom
    );
    assert_eq!(
        call(&mut rt, "put", 2, &[k, nine]).expect("put(k, 9)"),
        ok_atom,
        "an overwrite is ok"
    );
    assert_eq!(
        call(&mut rt, "get", 1, &[k]).expect("get(k)"),
        nine,
        "the overwrite took effect"
    );

    call(&mut rt, "put", 2, &[n, one]).expect("put(n, 1)");
    let all = call(&mut rt, "get", 0, &[]).expect("get()");
    let pairs = terms::list_elements(all).expect("a proper list");
    let keys: Vec<Eterm> = pairs
        .iter()
        .map(|pair| terms::tuple_element(*pair, 0).expect("the key"))
        .collect();
    assert_eq!(keys, vec![k, n], "insertion order survives an overwrite");
}

/// Any term may be a key: an atom, an integer, a binary, a tuple and a list all store and
/// read back (the oracle accepts every term shape — `beam_bifs/src/persistent_term.rs`).
///
/// The boxed keys come from `Runtime::eval`, whose results are built on the runtime's own
/// (never-reset) process heap — the same pointer stability the store's raw-`Eterm` design
/// relies on.
#[test]
fn any_term_can_be_a_key() {
    let mut rt = runtime();
    let atom = rt.intern("an_atom_key");
    let binary = rt.eval("<<1,2,3>>").expect("a binary literal");
    let tuple = rt.eval("{a,2}").expect("a tuple literal");
    let list = rt.eval("[1,2,3]").expect("a list literal");
    let value = rt.intern("stored");
    let ok_atom = rt.intern("ok");

    for key in [atom, make_small(7), binary, tuple, list] {
        assert_eq!(
            call(&mut rt, "put", 2, &[key, value]).expect("put(Key, stored)"),
            ok_atom
        );
        assert_eq!(
            call(&mut rt, "get", 1, &[key]).expect("get(Key)"),
            value,
            "the value read back for key {key:?}"
        );
    }
    let all = call(&mut rt, "get", 0, &[]).expect("get()");
    assert_eq!(
        terms::list_elements(all).expect("a list").len(),
        5,
        "five distinct keys"
    );
}
// ---------------------------------------------------------------------------------------
// The live-oracle cross-check
// ---------------------------------------------------------------------------------------

/// The golden sequence compared **value for value** against the live oracle (the recorded
/// probe in the module docs). Skips loudly without a toolchain.
#[test]
fn the_goldens_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("persistent_term") else {
        return;
    };

    // `badarg` is reported as a bare atom rather than the full `{'EXIT', {badarg, …}}`
    // stacktrace, so the two sides compare on the observable outcome.
    let expr = "\
Bad = fun(X) -> case X of {'EXIT',{badarg,_}} -> badarg; Other -> Other end end,
G = persistent_term:put(mi_k,1),
V = persistent_term:get(mi_k),
D = persistent_term:get(mi_missing,default_v),
E1 = persistent_term:erase(mi_k),
E2 = persistent_term:erase(mi_k),
PN1 = persistent_term:put_new(mi_n,1),
PN2 = persistent_term:put_new(mi_n,1),
PN3 = Bad(catch persistent_term:put_new(mi_n,2)),
G1 = Bad(catch persistent_term:get(mi_missing)),
I = persistent_term:info(),
io:format(\"~p ~p ~p ~p ~p ~p ~p ~p~n\",[G,V,D,E1,E2,PN1,PN2,PN3]),
io:format(\"missing=~p~n\",[G1]),
io:format(\"info_keys=~p~n\",[lists:sort(maps:keys(I))]),
halt(0).";
    let Some(rendered) = support::oracle_eval(&oracle, None, expr) else {
        eprintln!("SKIP the live-oracle comparison: the erl probe could not answer");
        return;
    };
    let oracle_lines: Vec<String> = rendered.lines().map(|l| l.trim().to_string()).collect();
    let mut rt = runtime();
    let k = rt.intern("mi_k");
    let n = rt.intern("mi_n");
    let missing = rt.intern("mi_missing");
    let default = rt.intern("default_v");
    let ok_atom = rt.intern("ok");
    let true_term = rt.intern("true");
    let false_term = rt.intern("false");
    let badarg = rt.intern("badarg");
    let one = make_small(1);
    let two = make_small(2);

    let put = call(&mut rt, "put", 2, &[k, one]).expect("put");
    let get = call(&mut rt, "get", 1, &[k]).expect("get");
    let defaulted = call(&mut rt, "get", 2, &[missing, default]).expect("get/2");
    let erase1 = call(&mut rt, "erase", 1, &[k]).expect("erase");
    let erase2 = call(&mut rt, "erase", 1, &[k]).expect("erase again");
    let put_new_new = call(&mut rt, "put_new", 2, &[n, one]).expect("put_new (fresh)");
    let put_new_same = call(&mut rt, "put_new", 2, &[n, one]).expect("put_new (same)");
    let put_new_diff = match call(&mut rt, "put_new", 2, &[n, two]) {
        Ok(value) => value,
        Err(error) => {
            assert_badarg(error);
            badarg
        }
    };
    let get_missing = match call(&mut rt, "get", 1, &[missing]) {
        Ok(value) => value,
        Err(error) => {
            assert_badarg(error);
            badarg
        }
    };
    let info = call(&mut rt, "info", 0, &[]).expect("info");

    let ours = vec![
        format!(
            "{} {} {} {} {} {} {} {}",
            scalar(&rt, put),
            scalar(&rt, get),
            scalar(&rt, defaulted),
            scalar(&rt, erase1),
            scalar(&rt, erase2),
            scalar(&rt, put_new_new),
            scalar(&rt, put_new_same),
            scalar(&rt, put_new_diff),
        ),
        format!("missing={}", scalar(&rt, get_missing)),
        format!("info_keys={}", info_keys(&rt, info)),
    ];
    assert_eq!(
        ours, oracle_lines,
        "the persistent_term goldens must match the live oracle"
    );
    // The `ok`/`true`/`false` outcomes are the oracle's atoms, not merely printable strings.
    assert_eq!(put, ok_atom);
    assert_eq!(erase1, true_term);
    assert_eq!(erase2, false_term);
    assert_eq!(put_new_same, ok_atom);
}
// ---------------------------------------------------------------------------------------
// Term builders + readers
// ---------------------------------------------------------------------------------------

/// The value stored under `key` in the flat map `map`, or `None` when the key is absent.
fn map_value(rt: &Runtime, map: Eterm, key: &str) -> Option<Eterm> {
    terms::map_entries(map)
        .expect("a map")
        .into_iter()
        .find(|(k, _)| rt.atom_name_of(*k) == Some(key))
        .map(|(_, value)| value)
}

/// The `info/0` map's keys, sorted — `[count, memory]` exactly as `maps:keys/1` sorts them.
fn info_keys(rt: &Runtime, map: Eterm) -> String {
    let mut keys: Vec<String> = terms::map_entries(map)
        .expect("a map")
        .into_iter()
        .map(|(k, _)| {
            rt.atom_name_of(k)
                .unwrap_or_else(|| panic!("{k:?} is not a named atom"))
                .to_string()
        })
        .collect();
    keys.sort();
    format!("[{}]", keys.join(","))
}