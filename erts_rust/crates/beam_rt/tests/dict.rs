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

//! The process dictionary and the `undef` reason producers, end to end.
//!
//! Two coordinated `beam_rt` behaviours are pinned here, both through the public API:
//!
//! 1. **The process dictionary is wired into the runtime.** `beam_bifs` registers the five
//!    `erlang` dictionary BIFs (`put/2`, `get/1`, `erase/1`, `get_keys/0`, `get_keys/1`) and
//!    forwards them to the `DictHook` seam; `beam_rt::context::call_registered` installs the
//!    concrete `DictBridge` over the **running process's** `beam_process::Dict` — the
//!    runtime's own process table for a `-eval` context, the caller-supplied one for a
//!    scheduler pass (`RuntimeContext::for_process`).
//! 2. **A genuinely missing module or function raises `exception error undef`** (class
//!    `error`, bare reason atom `undef`), exactly as the oracle does — *not* a message and
//!    never a capability gap.
//!
//! The three dictionary goldens are the differential corpus probes, reproduced here at
//! the library boundary: `begin put(k,1), get(k) end` → `1`,
//! `get(a_nonexistent_key_xyz)` → `undefined`, and
//! `begin put(k,1), erase(k), get(k) end` → `undefined`.
use beam_bifs::NullSink;
use beam_common::{BeamError, ExceptionClass};
use beam_engine::{terms, ExecutionContext};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_list, is_small, make_small, signed_value, Eterm};

/// The committed oracle fixture (`beam_loader/tests/fixtures/slice.beam`), reused by
/// relative path so there is a single source of truth for it — never copied.
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// Whether `term` is the atom `undefined` as `rt`'s shared table names it.
fn is_undefined(rt: &Runtime, term: Eterm) -> bool {
    rt.atom_name_of(term) == Some("undefined")
}

/// Assert `error` is the oracle's `exception error undef`: class `error`, reason `undef`.
fn assert_undef(error: BeamError) {
    match error {
        BeamError::Raise(raise) => {
            assert_eq!(
                raise.class,
                ExceptionClass::Error,
                "undef is an `error`-class raise: {raise:?}"
            );
            assert_eq!(raise.reason.as_atom(), Some("undef"), "{raise:?}");
        }
        other => panic!("expected an error:undef raise, got {other:?}"),
    }
}

// --- the three dictionary goldens, through `Runtime::eval` ------------------------

/// `begin put(k, 1), get(k) end` → `1` (oracle-confirmed; the differential corpus probe).
#[test]
fn eval_put_then_get_returns_the_stored_value() {
    let mut rt = runtime();
    let value = rt
        .eval("begin put(k, 1), get(k) end")
        .expect("begin put(k,1), get(k) end");
    assert!(is_small(value), "1 is a small integer, got {value:?}");
    assert_eq!(signed_value(value), 1);
}

/// `get(a_nonexistent_key_xyz)` → `undefined` (oracle-confirmed).
#[test]
fn eval_get_of_a_missing_key_is_undefined() {
    let mut rt = runtime();
    let value = rt
        .eval("get(a_nonexistent_key_xyz)")
        .expect("get(a_nonexistent_key_xyz)");
    assert!(
        is_undefined(&rt, value),
        "expected `undefined`, got {value:?}"
    );
}

/// `begin put(k, 1), erase(k), get(k) end` → `undefined` (oracle-confirmed).
#[test]
fn eval_put_erase_then_get_is_undefined() {
    let mut rt = runtime();
    let value = rt
        .eval("begin put(k, 1), erase(k), get(k) end")
        .expect("begin put(k,1), erase(k), get(k) end");
    assert!(
        is_undefined(&rt, value),
        "expected `undefined`, got {value:?}"
    );
}

// --- per-operation semantics through `ExecutionContext::call_bif` -----------------

/// `put/2` returns the **previous** value (`undefined` on a fresh key) and the value is
/// really stored; `get/1` (missing or present) and `erase/1` follow the same rule.
#[test]
fn put_get_erase_semantics_through_call_bif() {
    let mut rt = runtime();
    // Immediate atom/bits terms are captured before the context borrows `rt`.
    let undefined = rt.intern("undefined");
    let key = rt.intern("k");
    let missing = rt.intern("missing");
    let mut sink = NullSink;

    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
    // A fresh key stores and reports `undefined` — the value it replaced.
    let first = ctx
        .call_bif("erlang", "put", 2, &[key, make_small(1)])
        .expect("put(k, 1)");
    assert_eq!(first, undefined, "a fresh put returns undefined");

    // Overwriting returns the old value.
    let replaced = ctx
        .call_bif("erlang", "put", 2, &[key, make_small(2)])
        .expect("put(k, 2)");
    assert_eq!(replaced, make_small(1), "put returns the replaced value");

    // `get` of a present key is the stored value; of a missing key, `undefined`.
    let found = ctx.call_bif("erlang", "get", 1, &[key]).expect("get(k)");
    assert_eq!(found, make_small(2));
    let absent = ctx
        .call_bif("erlang", "get", 1, &[missing])
        .expect("get(missing)");
    assert_eq!(absent, undefined);

    // `erase` returns the old value; a second erase is `undefined`.
    let erased = ctx
        .call_bif("erlang", "erase", 1, &[key])
        .expect("erase(k)");
    assert_eq!(erased, make_small(2), "erase returns the removed value");
    let again = ctx
        .call_bif("erlang", "erase", 1, &[key])
        .expect("erase(k) again");
    assert_eq!(again, undefined);
}

/// `get_keys/0` returns the stored keys as a list, and `get_keys/1` the keys holding a
/// given value (the `beam_process::Dict` raw-equality contract).
#[test]
fn get_keys_lists_the_stored_keys() {
    let mut rt = runtime();
    let key = rt.intern("k");
    let other = rt.intern("other");
    let mut sink = NullSink;

    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
    ctx.call_bif("erlang", "put", 2, &[key, make_small(1)])
        .expect("put(k, 1)");
    ctx.call_bif("erlang", "put", 2, &[other, make_small(1)])
        .expect("put(other, 1)");

    let keys = ctx
        .call_bif("erlang", "get_keys", 0, &[])
        .expect("get_keys()");
    assert!(is_list(keys), "get_keys/0 returns a list, got {keys:?}");
    assert_eq!(
        terms::list_elements(keys).expect("a proper list"),
        vec![key, other],
        "both stored keys, in insertion order"
    );

    // `get_keys(Value)` selects by value.
    let by_value = ctx
        .call_bif("erlang", "get_keys", 1, &[make_small(1)])
        .expect("get_keys(1)");
    assert_eq!(
        terms::list_elements(by_value).expect("a proper list"),
        vec![key, other]
    );
    let none = ctx
        .call_bif("erlang", "get_keys", 1, &[make_small(42)])
        .expect("get_keys(42)");
    assert_eq!(terms::list_elements(none).expect("a proper list"), vec![]);
}

/// The **scheduler-pass** dict test: a context built with `RuntimeContext::for_process`
/// over a caller-owned [`beam_process::ProcessTable`] stores into *that* process's
/// dictionary (resolved through the same route the mailbox/`self` seam uses), and the
/// stored value is visible on the process afterwards.
#[test]
fn for_process_stores_into_the_caller_supplied_process() {
    let mut rt = runtime();
    let mut atoms = beam_term::prelude::AtomTable::new();
    let mut table =
        beam_process::ProcessTable::new(beam_process::SignalAtoms::from_table(&mut atoms));
    let pid = table.spawn_default().expect("spawn a process");
    let undefined = rt.intern("undefined");
    let key = rt.intern("k");

    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    let first = ctx
        .call_bif("erlang", "put", 2, &[key, make_small(7)])
        .expect("put(k, 7)");
    assert_eq!(first, undefined, "a fresh put returns undefined");
    let got = ctx.call_bif("erlang", "get", 1, &[key]).expect("get(k)");
    assert_eq!(got, make_small(7));

    // Drop the context to release its borrow of `table`, then inspect the process.
    drop(ctx);
    assert_eq!(
        table.get(pid).expect("the process").dict().get(key),
        Some(make_small(7)),
        "the value landed in the caller-supplied process's dictionary"
    );
}

// --- the scheduler pass -----------------------------------------------------------

/// A **scheduler-pass** dict call: a process spawned and run through the runtime's own
/// `ProcessRunner` stores and reads its dictionary through the caller-supplied table
/// (the moved `Runtime::processes`, which the `for_process` context resolves against —
/// *not* the runtime's own, now-empty table). A wrong resolution would raise
/// `no_mailbox` rather than return `1`, so this pins the branch.
#[test]
fn a_spawned_closure_uses_its_own_process_dictionary() {
    let mut rt = runtime();
    let fun = rt
        .eval("fun() -> put(k, 1), get(k) end")
        .expect("a closure that puts and gets");
    let pid = rt
        .spawn_fun_process(fun, vec![])
        .expect("spawn the closure process");
    rt.run_scheduler().expect("run the closure to completion");
    assert_eq!(
        rt.process_result(pid),
        Some(make_small(1)),
        "put stored under the running process and get read it back"
    );
}

// --- the `undef` reason producers -------------------------------------------------

/// A call to a module the release does not have raises `exception error undef`
/// (`CodeServer::not_found` → the reason the oracle gives).
#[test]
fn a_call_to_a_missing_module_raises_undef() {
    let mut rt = runtime();
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
    let error = ctx
        .call_bif("nosuchmodule_xyz", "nosuchfun", 0, &[])
        .expect_err("the module does not exist");
    assert_undef(error);
}

/// A call to a **missing function** on a loaded module raises `exception error undef`
/// (`RuntimeContext::run_export`).
#[test]
fn a_call_to_a_missing_function_raises_undef() {
    let mut rt = runtime();
    // Cache the fixture in the code server so the call resolves the module (not the code
    // path) and reaches `run_export`.
    rt.load_module(SLICE_BEAM).expect("cache slice.beam");
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
    let error = ctx
        .call_bif("slice", "nosuchfun", 0, &[])
        .expect_err("the function does not exist");
    assert_undef(error);
}

/// An unknown `erlang` BIF name (not a curated capability gap) raises `undef` too — the
/// oracle's reason for a name the release does not have.
#[test]
fn an_unknown_erlang_bif_name_raises_undef() {
    let mut rt = runtime();
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(&mut rt, &mut sink);
    let error = ctx
        .call_bif("erlang", "no_such_bif_xyz", 1, &[make_small(1)])
        .expect_err("the BIF does not exist");
    assert_undef(error);
}
