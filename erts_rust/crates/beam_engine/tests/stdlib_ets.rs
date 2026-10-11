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

//! **`ets` host-gap slice** — the honest record that the runtime's table store is
//! not wired into this engine's test host.
//!
//! `ets` is stdlib's thin wrapper over the ETS NIF/BIF surface: almost every
//! operation body is an `erlang:nif_error(undef)` stub (`lib/stdlib/src/ets.erl`,
//! e.g. `new/2`, `insert/2`, `lookup/2`, `delete/1,2`, `info/1,2`, `match/2`,
//! `select/2`, `update_counter/3`, `first/1`, `next/2`, `last/1`, `prev/2`), and the
//! few wrappers that fold over the stubs reach the same wall (`delete_all_objects/1`
//! → `internal_delete_all/2`, `tab2list/1` → `match_object/2`, `all/0` →
//! `internal_request_all/0`). Every one bottoms out in an ETS operation the test
//! host's BIF floor (`tests/support/bifs.rs`) does **not** serve.
//!
//! Closing this gap needs an **ETS host seam** — `beam_bifs`'s `ets` BIF surface
//! (the `erlang:ets_*` operations plus its `ets:info/1` bridge) over `beam_ets`'s
//! `Table`/set/ordered_set interiors — wired into `tests/support/bifs.rs`. That seam
//! does not exist anywhere under `tests/support/`, and this crate does not own it, so
//! this suite is the loud, honest record until it lands: it loads `ets`, then probes
//! a representative set of real exports, printing exactly
//! `HOST GAP: <module>:<function>/<arity> — <typed error>` for each and asserting the
//! **exact** typed error — never a fabricated table, tid or value.
//!
//! The module **loads** (its literal area / atom chunk decode fine); what cannot run
//! is any *operation*, because the host has no table registry to operate on. Every
//! `// oracle:` comment quotes the real system-ERTS result (`erl -noshell -eval …`,
//! OTP 29 / `erts-17.1`, the differential oracle) that the coverage is *intended* to
//! reproduce once the seam exists — so the target is recorded even though the host
//! cannot serve it yet.
//!
//! Policy: a probe that returns a value where the oracle would operate on a table is a
//! hard failure (a fabricated result must never pass); a probe the host cannot serve
//! prints its `HOST GAP:` line **and asserts the exact typed error**, so a regression
//! that turns a typed error into a wrong result — or into a hang — is caught rather
//! than silently passing. See [`ets_exports_report_their_host_gap`].

mod support;

use beam_common::{BeamError, ExceptionClass};
use beam_term::{make_small, Eterm};
use support::{
    assert_atom, skip_reason, stdlib_ebin, term_list, term_tuple, ModuleHost,
};

// ---------------------------------------------------------------------------
// Host + helpers
// ---------------------------------------------------------------------------

/// A host over the system stdlib `ebin` (there is no fixture harness here).
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    ModuleHost::new(ebin)
}

/// Skips the calling test (loudly) when the oracle stdlib is unavailable.
fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

/// Runs a target the host **cannot** serve, prints its `HOST GAP:` line, and returns
/// the typed error. A returned value is a hard failure: these targets are pinned as
/// gaps, so a resolution turning silently into a fabricated table/value must be
/// caught, not printed.
fn host_gap<T>(name: &str, result: Result<T, BeamError>) -> BeamError {
    match result {
        Err(error) => {
            println!("HOST GAP: {name} — {error:?}");
            error
        }
        Ok(_) => panic!("`{name}` is pinned as an unservable host gap, but returned a value"),
    }
}

/// Asserts `err` is the typed BEAM `badarg` — the shape the host's
/// `erlang:nif_error/1` refusal produces for a NIF-backed `ets` export.
fn assert_blocked_badarg(name: &str, err: &BeamError) {
    assert!(matches!(err, BeamError::BadArg), "unexpected `{name}` error: {err:?}");
    assert_eq!(err.as_atom(), Some("badarg"), "`{name}` must be the typed BEAM `badarg`");
}

/// Asserts `err` is an uncaught class-`error` raise whose lively reason term is the
/// bare atom `atom` — the shape a `func_info` failed-clause raise carries out of the
/// machine. (The reason term rides in the raise's opaque live-reason slot; `as_atom`
/// stays `None` for the `ExitReason::Structured` the uncaught funnel produces.)
fn assert_raise_atom(host: &ModuleHost, name: &str, err: &BeamError, atom_name: &str) {
    let raise = match err {
        BeamError::Raise(raise) => raise,
        other => panic!("`{name}` must be a typed raise, got {other:?}"),
    };
    assert_eq!(
        raise.class,
        ExceptionClass::Error,
        "`{name}` must raise class `error`"
    );
    let reason = raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
        .unwrap_or_else(|| panic!("`{name}` must carry a live reason term, got {raise:?}"));
    assert_atom(host, reason, atom_name);
}

// ---------------------------------------------------------------------------
// `ets` — the whole operation surface is a host gap
// ---------------------------------------------------------------------------

/// The real installed `ets` module **loads**, but every *operation* it exports is
/// blocked by the test host's BIF floor: the ETS operations are
/// `erlang:nif_error(undef)` stubs (`lib/stdlib/src/ets.erl`) the floor serves as the
/// typed `BeamError::BadArg`, and the wrappers that fold over them reach the same wall.
/// The one exception probed here, `fun2ms/1`, fails its `is_function/1` guard first and
/// raises the oracle's own bare-atom `function_clause`.
///
/// Each probe is a **real installed `ets` export** built from plausible test-side
/// arguments (a table-name atom `t`, an `{k,1}` object, the `'_'` pattern, a match
/// spec): it prints the required `HOST GAP:` line and asserts the **exact** typed
/// error, so a regression turning a typed error into a fabricated tid/list/value — or
/// into a hang — is caught rather than silently passing. Owning crate for closing
/// these gaps: `beam_bifs`'s ETS BIF surface + its `ets:info/1` bridge over
/// `beam_ets`, wired into `tests/support/bifs.rs`; not this engine.
///
/// Opcode families (up to the blocking `call_ext` into `ets.beam`): the `call_ext`
/// cross-module seam, `select_val` clause dispatch, `test,is_function` /
/// `test,is_list` guards, and the tuple/cons builders behind the arguments.
#[test]
fn ets_exports_report_their_host_gap() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    host.load("ets").expect("`ets` must load (its atom chunk / literal area decode)");

    // Plausible, oracle-shaped arguments. The table-name atom `t` stands in for a
    // `tid()` a full runtime would return from `ets:new/2`; the exact values never
    // reach a table store here — the host refuses the operation first.
    let table = host.intern("t");
    let key = host.intern("k");
    let object = term_tuple(&mut host, &[key, make_small(1)]);
    let pattern = host.intern("'_'");
    let size = host.intern("size");
    let empty = term_list(&mut host, &[]);
    // A plausible `ets:select/2` match spec: `[{'$1',[],['$1']}]`.
    let dollar1 = host.intern("'$1'");
    let dollar1_list = term_list(&mut host, &[dollar1]);
    let spec_tuple = term_tuple(&mut host, &[dollar1, empty, dollar1_list]);
    let match_spec = term_list(&mut host, &[spec_tuple]);

    // oracle: erl -noshell -eval 'io:format("~w~n",[is_reference(ets:new(t,[]))]),halt().'
    //         = true  (a real runtime yields a fresh `tid()` reference)
    // `ets:new/2` is an `erlang:nif_error(undef)` stub: with no table registry, table
    // creation is a typed `badarg`, never a fake tid.
    let err = host_gap("ets:new/2", host.call("ets", "new", &[table, empty]));
    assert_blocked_badarg("ets:new/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),io:format("~w~n",[ets:insert(T,{k,1})]),halt().'
    //         = true
    let err = host_gap("ets:insert/2", host.call("ets", "insert", &[table, object]));
    assert_blocked_badarg("ets:insert/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),io:format("~w~n",[ets:insert_new(T,{k,1})]),halt().'
    //         = true
    let err = host_gap("ets:insert_new/2", host.call("ets", "insert_new", &[table, object]));
    assert_blocked_badarg("ets:insert_new/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:lookup(T,k)]),halt().'
    //         = [{k,1}]
    let err = host_gap("ets:lookup/2", host.call("ets", "lookup", &[table, key]));
    assert_blocked_badarg("ets:lookup/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:lookup_element(T,k,2)]),halt().'
    //         = 1
    let err = host_gap(
        "ets:lookup_element/3",
        host.call("ets", "lookup_element", &[table, key, make_small(2)]),
    );
    assert_blocked_badarg("ets:lookup_element/3", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),io:format("~w~n",[ets:delete(T)]),halt().'
    //         = true
    let err = host_gap("ets:delete/1", host.call("ets", "delete", &[table]));
    assert_blocked_badarg("ets:delete/1", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),io:format("~w~n",[ets:delete(T,k)]),halt().'
    //         = true
    let err = host_gap("ets:delete/2", host.call("ets", "delete", &[table, key]));
    assert_blocked_badarg("ets:delete/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),io:format("~w~n",[ets:delete_all_objects(T)]),halt().'
    //         = true
    // `delete_all_objects/1` folds over `internal_delete_all/2`, itself a
    // `erlang:nif_error(undef)` stub — the wrapper reaches the same wall.
    let err = host_gap(
        "ets:delete_all_objects/1",
        host.call("ets", "delete_all_objects", &[table]),
    );
    assert_blocked_badarg("ets:delete_all_objects/1", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:delete_object(T,{k,1})]),halt().'
    //         = true
    let err = host_gap(
        "ets:delete_object/2",
        host.call("ets", "delete_object", &[table, object]),
    );
    assert_blocked_badarg("ets:delete_object/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:member(T,k)]),halt().'
    //         = true
    let err = host_gap("ets:member/2", host.call("ets", "member", &[table, key]));
    assert_blocked_badarg("ets:member/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),io:format("~w~n",[ets:info(T)]),halt().'
    //         = [{name,t},{id,#Ref<...>},{type,set},...]  (the 15-item info list)
    let err = host_gap("ets:info/1", host.call("ets", "info", &[table]));
    assert_blocked_badarg("ets:info/1", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),io:format("~w~n",[ets:info(T,size)]),halt().'
    //         = 0
    let err = host_gap("ets:info/2", host.call("ets", "info", &[table, size]));
    assert_blocked_badarg("ets:info/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,0}),io:format("~w~n",[ets:update_counter(T,k,1)]),halt().'
    //         = 1
    let err = host_gap(
        "ets:update_counter/3",
        host.call("ets", "update_counter", &[table, key, make_small(1)]),
    );
    assert_blocked_badarg("ets:update_counter/3", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:match(T,'_')]),halt().'
    //         = [{k,1}]
    let err = host_gap("ets:match/2", host.call("ets", "match", &[table, pattern]));
    assert_blocked_badarg("ets:match/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:select(T,[{'"'"'$1'"'"',[],[ '"'"'$1'"'"']}])]),halt().'
    //         = [{k,1}]
    let err = host_gap("ets:select/2", host.call("ets", "select", &[table, match_spec]));
    assert_blocked_badarg("ets:select/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:tab2list(T)]),halt().'
    //         = [{k,1}]
    // `tab2list/1` is `ets:match_object(T, '_')`; that wrapper reaches the same
    // unserved stub, so it is the typed `badarg` too.
    let err = host_gap("ets:tab2list/1", host.call("ets", "tab2list", &[table]));
    assert_blocked_badarg("ets:tab2list/1", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),io:format("~w~n",[ets:all()]),halt().'
    //         = [t]  (named tables + tids at the node)
    // `all/0` calls `ets:internal_request_all/0`, an `erlang:nif_error(undef)` stub,
    // before it ever reaches `erlang:system_info/1` or its `receive`.
    let err = host_gap("ets:all/0", host.call("ets", "all", &[]));
    assert_blocked_badarg("ets:all/0", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:first(T)]),halt().'
    //         = k
    let err = host_gap("ets:first/1", host.call("ets", "first", &[table]));
    assert_blocked_badarg("ets:first/1", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:next(T,k)]),halt().'
    //         = '$end_of_table'
    let err = host_gap("ets:next/2", host.call("ets", "next", &[table, key]));
    assert_blocked_badarg("ets:next/2", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:last(T)]),halt().'
    //         = k
    let err = host_gap("ets:last/1", host.call("ets", "last", &[table]));
    assert_blocked_badarg("ets:last/1", &err);

    // oracle: erl -noshell -eval 'T=ets:new(t,[]),ets:insert(T,{k,1}),io:format("~w~n",[ets:prev(T,k)]),halt().'
    //         = '$end_of_table'
    let err = host_gap("ets:prev/2", host.call("ets", "prev", &[table, key]));
    assert_blocked_badarg("ets:prev/2", &err);

    // oracle: erl -noshell -eval 'try ets:fun2ms(genesis_poison) catch C:R -> io:format("~p:~p~n",[C,R]) end,halt().'
    //         = error:function_clause
    // The ONE probed export whose exact typed error is a *raise* rather than `badarg`:
    // `fun2ms/1`'s single clause is guarded by `is_function(ShellFun)`, so an atom
    // argument fails the `test,is_function` guard and jumps to `func_info`, raising the
    // bare-atom `function_clause` — exactly the oracle's own result, so this probe is
    // **oracle-agreeing**, not a floor gap (a real *fun* argument would need the
    // `erl_eval`/`ms_transform` machinery the host also lacks). Asserted by its live
    // reason term, since an uncaught raise carries `ExitReason::Structured` (whose
    // `as_atom` is `None`) plus the reason term in the opaque slot.
    let poison = host.intern("genesis_poison");
    let err = host_gap("ets:fun2ms/1", host.call("ets", "fun2ms", &[poison]));
    assert_raise_atom(&host, "ets:fun2ms/1", &err, "function_clause");
}
