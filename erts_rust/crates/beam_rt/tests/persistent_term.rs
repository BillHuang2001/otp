//! The seven `persistent_term:*` BIFs, end to end through the runtime glue.
//!
//! `beam_bifs` registers `get/0`, `get/1`, `get/2`, `put/2`, `put_new/2`,
//! `erase/1` and `info/0` and forwards every one of them to the optional
//! `PersistentTerm` host seam; `beam_rt::context::call_registered` installs the
//! concrete handle over `Runtime`'s node-global `PersistentTermStore`
//! (`Rc<RefCell<…>>`, the same pattern as the port/NIF/ETS registries).
//!
//! The goldens reproduced here are the oracle probes for OTP 29.1.1 / erts-17.1
//! (`beam_bifs/src/persistent_term.rs`, `erl_bif_persistent.c`):
//!
//! ```text
//! ok,1,default,true,false
//! ```
//!
//! for `put(k,1)`, `get(k)`, `get(nope,default)`, `erase(k)`, `erase(k)` — so
//! `erase/1` is `true`/`false` (never `ok`), `get/1` of an absent key is
//! `badarg`, and `put_new/2` of a key holding a *different* value is `badarg`
//! (`get/0` and `info/0` are asserted structurally: `get/0` ⇒ `[{k,1}]`, `info/0`
//! ⇒ a flat map whose `count` is the entry count).
use beam_bifs::NullSink;
use beam_common::BeamError;
use beam_engine::terms;
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_small, signed_value, Eterm};

/// Run `f` against a fresh `RuntimeContext` over `rt`, returning its result.
/// The context (and its sink) are dropped before `rt` is borrowed again.
fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    f(&mut ctx)
}

/// Whether `error` is the oracle's plain `badarg`.
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

#[test]
fn the_seven_bifs_follow_the_oracle() {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let k = rt.intern("k");
    let missing = rt.intern("missing_key_xyz");
    let default = rt.intern("default_value_xyz");
    let ok_atom = rt.intern("ok");
    let one = beam_term::prelude::make_small(1);
    let two = beam_term::prelude::make_small(2);
    let true_term = rt.intern("true");
    let false_term = rt.intern("false");

    // `put(k,1)` ⇒ `ok`; `get(k)` ⇒ `1`; `get(missing,Default)` ⇒ `Default`;
    // `get(missing)` ⇒ `badarg`; `put_new(k,2)` ⇒ `badarg` (a different value);
    // `put_new(k,1)` ⇒ `ok` (the same value is a no-op success).
    let (put_ret, get_ret, default_ret, missing_err, put_new_diff_err, put_new_same_ret) =
        with_context(&mut rt, |ctx| {
            let put_ret = ctx
                .call_bif("persistent_term", "put", 2, &[k, one])
                .expect("put(k, 1)");
            let get_ret = ctx
                .call_bif("persistent_term", "get", 1, &[k])
                .expect("get(k)");
            let default_ret = ctx
                .call_bif("persistent_term", "get", 2, &[missing, default])
                .expect("get(missing, default)");
            let missing_err = ctx
                .call_bif("persistent_term", "get", 1, &[missing])
                .expect_err("get(missing) is badarg");
            let put_new_diff_err = ctx
                .call_bif("persistent_term", "put_new", 2, &[k, two])
                .expect_err("put_new(k, 2) is badarg");
            let put_new_same_ret = ctx
                .call_bif("persistent_term", "put_new", 2, &[k, one])
                .expect("put_new(k, 1) is ok");
            (
                put_ret,
                get_ret,
                default_ret,
                missing_err,
                put_new_diff_err,
                put_new_same_ret,
            )
        });
    assert_eq!(put_ret, ok_atom, "put/2 returns ok");
    assert_eq!(get_ret, one, "get/1 returns the stored value");
    assert_eq!(default_ret, default, "get/2 returns the default");
    assert_badarg(missing_err);
    assert_badarg(put_new_diff_err);
    assert_eq!(put_new_same_ret, ok_atom, "put_new/2 of the same value is ok");

    // `get/0` ⇒ `[{k,1}]` in insertion order, and `info/0` ⇒ a 2-key flat map.
    let (all, info) = with_context(&mut rt, |ctx| {
        let all = ctx
            .call_bif("persistent_term", "get", 0, &[])
            .expect("get()");
        let info = ctx
            .call_bif("persistent_term", "info", 0, &[])
            .expect("info()");
        (all, info)
    });
    let entries = terms::list_elements(all).expect("get/0 is a proper list");
    assert_eq!(entries.len(), 1, "exactly the one stored entry, got {entries:?}");
    let pair = entries[0];
    assert_eq!(terms::tuple_arity(pair).expect("a 2-tuple"), 2);
    assert_eq!(terms::tuple_element(pair, 0).expect("key"), k);
    assert_eq!(terms::tuple_element(pair, 1).expect("value"), one);

    // `info/0` is a **flat map** `#{count => N, memory => Bytes}`; `count` is the
    // real entry count, `memory` a non-negative byte figure of this runtime's own
    // accounting (the oracle's is larger: it also counts literal-area overhead).
    let pairs = terms::map_entries(info).expect("info/0 is a map");
    assert_eq!(pairs.len(), 2, "count and memory, got {pairs:?}");
    let mut count = None;
    let mut memory = None;
    for (key, value) in pairs {
        match rt.atom_name_of(key) {
            Some("count") => count = Some(value),
            Some("memory") => memory = Some(value),
            other => panic!("unexpected info/0 key: {other:?}"),
        }
    }
    let count = count.expect("an info/0 `count` key");
    assert!(is_small(count), "count is an integer");
    assert_eq!(signed_value(count), 1, "one stored entry");
    let memory = memory.expect("an info/0 `memory` key");
    assert!(is_small(memory), "memory is an integer");
    assert!(signed_value(memory) >= 0, "memory is non-negative");

    // `erase(k)` ⇒ `true`, then `false` (the oracle's rule, not `ok`).
    let (first_erase, second_erase) = with_context(&mut rt, |ctx| {
        let first = ctx
            .call_bif("persistent_term", "erase", 1, &[k])
            .expect("erase(k)");
        let second = ctx
            .call_bif("persistent_term", "erase", 1, &[k])
            .expect("erase(k) again");
        (first, second)
    });
    assert_eq!(first_erase, true_term, "erase/1 of a present key is true");
    assert_eq!(second_erase, false_term, "erase/1 of an absent key is false");

    // With the store emptied, `get/0` is `[]` and `info/0`'s count is 0.
    let (empty, empty_info) = with_context(&mut rt, |ctx| {
        let all = ctx
            .call_bif("persistent_term", "get", 0, &[])
            .expect("get()");
        let info = ctx
            .call_bif("persistent_term", "info", 0, &[])
            .expect("info()");
        (all, info)
    });
    assert_eq!(terms::list_elements(empty).expect("a list"), vec![]);
    let pairs = terms::map_entries(empty_info).expect("a map");
    let count = pairs
        .iter()
        .find(|(key, _)| rt.atom_name_of(*key) == Some("count"))
        .map(|(_, value)| *value)
        .expect("an info/0 `count` key");
    assert_eq!(signed_value(count), 0, "the store is empty");
}

/// Overwriting an existing key with a *different* value succeeds (`put/2`), and
/// the new value is what `get/1` reports — the store does not reorder.
#[test]
fn put_overwrites_in_place() {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let k = rt.intern("k");
    let n = rt.intern("n");
    let one = beam_term::prelude::make_small(1);
    let nine = beam_term::prelude::make_small(9);
    let ok_atom = rt.intern("ok");

    let (first, second, read) = with_context(&mut rt, |ctx| {
        let first = ctx
            .call_bif("persistent_term", "put", 2, &[k, one])
            .expect("put(k, 1)");
        let second = ctx
            .call_bif("persistent_term", "put", 2, &[k, nine])
            .expect("put(k, 9)");
        let read = ctx
            .call_bif("persistent_term", "get", 1, &[k])
            .expect("get(k)");
        (first, second, read)
    });
    assert_eq!(first, ok_atom);
    assert_eq!(second, ok_atom, "an overwrite is ok");
    assert_eq!(read, nine, "the overwrite took effect");

    // A second, distinct key appends; `get/0` keeps insertion order.
    let all = with_context(&mut rt, |ctx| {
        ctx.call_bif("persistent_term", "put", 2, &[n, one])
            .expect("put(n, 1)");
        ctx.call_bif("persistent_term", "get", 0, &[])
            .expect("get()")
    });
    let entries = terms::list_elements(all).expect("a list");
    assert_eq!(entries.len(), 2);
    let keys: Vec<Eterm> = entries
        .iter()
        .map(|p| terms::tuple_element(*p, 0).expect("key"))
        .collect();
    assert_eq!(keys, vec![k, n], "insertion order is preserved");
}
