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

//! `-on_load` module-hook unit tests.
//!
//! The `-on_load` attribute names a **local** function ERTS runs synchronously when the
//! module loads; a non-`ok` return forgets the module and folds the load into
//! `{error, on_load_failure}` ([`RuntimeContext::run_module_on_load`], `on_load.rs`), and the
//! three `erlang:*` primitives (`call_on_load_function/1`, `has_prepared_code_on_load/1`,
//! `finish_after_on_load/2`) adapt that hook to this runtime's single-generation store.
//!
//! These tests compile fixtures at test time with the system `erlc`, put the fixture
//! directory on the code path, and drive the real `code:ensure_loaded/1` BIF — the route
//! that runs the hook — rather than the direct `Runtime::load_module` path (which does not
//! run it). A missing `erlc`/oracle is a **loud** skip, never a silent pass.

use std::path::PathBuf;

use beam_common::Result;
use beam_engine::terms;
use beam_term::{make_small, signed_value, Eterm};

use crate::native::test_support::{atom, call, runtime, system_runtime};
use crate::runtime::Runtime;

use super::test_support::{compile_fixture, resolve_erlc};

/// `-on_load(load/0)` where `load/0` puts a marker into the process dictionary and answers
/// `ok` — the success case, whose side effect proves the hook body really ran.
const ONLOAD_OK: &str =
    "-module(onload_ok).\n-export([load/0]).\n-on_load(load/0).\n\
     load() -> put(onload_side_effect, done), ok.\n";

/// `-on_load(load/0)` whose `load/0` answers `{error, boom}` — the failure case.
const ONLOAD_FAIL: &str =
    "-module(onload_fail).\n-export([load/0]).\n-on_load(load/0).\nload() -> {error, boom}.\n";

/// A plain module with no `-on_load` attribute — the unchanged-load regression case.
const PLAIN_MODULE: &str =
    "-module(plainmod).\n-export([hello/0]).\nhello() -> ok.\n";

/// `-on_load(load/0)` whose `load/0` **blocks** on a scheduler-produced reply: the hook body
/// parks in a `receive`, so the inline nested-machine path (the regression) folds the load into
/// `blocked()` while the scheduler-boundary drain satisfies it.
const ONLOAD_BLOCK: &str =
    "-module(onload_block).\n-export([load/0, hello/0]).\n-on_load(load/0).\n\
     load() -> Self = self(), spawn(fun() -> Self ! onload_reply end), \
     receive onload_reply -> ok after 10000 -> error(onload_timeout) end.\n\
     hello() -> ok.\n";

/// A driver whose `main/0` loads the blocking hook through the scheduler and reports
/// `{code:ensure_loaded/1, erlang:function_exported/3}`.
const ONLOAD_DRIVER: &str =
    "-module(onload_driver).\n-export([main/0]).\n\
     main() -> {code:ensure_loaded(onload_block), \
                erlang:function_exported(onload_block, hello, 0)}.\n";

/// A driver whose `main/0` loads `onload_ok` through the scheduler and reports
/// `{code:ensure_loaded/1, get(onload_side_effect)}` — the proof the hook body ran **inline**
/// (its process-dictionary side effect is visible in the same scheduled run, not deferred).
const ONLOAD_SYNC_DRIVER: &str =
    "-module(onload_sync_driver).\n-export([main/0]).\n\
     main() -> {code:ensure_loaded(onload_ok), get(onload_side_effect)}.\n";

/// A driver whose `main/0` loads `onload_ok` through the scheduler and reports
/// `{code:ensure_loaded/1, erlang:function_exported/3}` — the freshly-loaded module is
/// immediately usable on the scheduled route.
const ONLOAD_EXPORTED_DRIVER: &str =
    "-module(onload_exported_driver).\n-export([main/0]).\n\
     main() -> {code:ensure_loaded(onload_ok), \
                erlang:function_exported(onload_ok, load, 0)}.\n";

/// The loud skip banner for a missing toolchain — never a silent pass.
fn skip(banner: &str, why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt on_load {banner} SKIPPED ===============");
    eprintln!("{why}: the -on_load fixture cannot be compiled or loaded.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("==============================================================");
    eprintln!();
}

/// The system `erlc`, or `None` after a loud skip when it is absent.
fn erlc_or_skip() -> Option<PathBuf> {
    match resolve_erlc() {
        Some(erlc) => Some(erlc),
        None => {
            skip("fixture", "no system `erlc`");
            None
        }
    }
}

/// `code:ensure_loaded/1` on `name` (the load route that runs the `-on_load` hook).
fn ensure_loaded(runtime: &mut Runtime, name: &str) -> Result<Eterm> {
    let module = atom(runtime, name);
    call(runtime, "code", "ensure_loaded", &[module])
}

/// The atom name of tuple element `index` of `term`, or `None` when it is not an atom.
fn pair_atom(runtime: &Runtime, term: Eterm, index: usize) -> Option<String> {
    runtime
        .atom_name_of(terms::tuple_element(term, index).ok()?)
        .map(str::to_string)
}

/// Asserts `term` is the `{module, module}` pair a successful load answers.
fn assert_module_pair(runtime: &Runtime, term: Eterm, module: &str) {
    assert_eq!(terms::tuple_arity(term).expect("a 2-tuple"), 2, "{term:?}");
    assert_eq!(pair_atom(runtime, term, 0).as_deref(), Some("module"));
    assert_eq!(pair_atom(runtime, term, 1).as_deref(), Some(module));
}

/// Asserts `term` is the `{error, reason}` pair a folded load failure answers.
fn assert_error_pair(runtime: &Runtime, term: Eterm, reason: &str) {
    assert_eq!(terms::tuple_arity(term).expect("a 2-tuple"), 2, "{term:?}");
    assert_eq!(pair_atom(runtime, term, 0).as_deref(), Some("error"));
    assert_eq!(pair_atom(runtime, term, 1).as_deref(), Some(reason));
}

/// Asserts `name` is loaded: `code:is_loaded/1` is a `{file, _}` pair and
/// `erlang:module_loaded/1` is `true`.
fn assert_loaded(runtime: &mut Runtime, name: &str) {
    let module = atom(runtime, name);
    let loaded = call(runtime, "code", "is_loaded", &[module]).expect("code:is_loaded/1");
    assert_eq!(terms::tuple_arity(loaded).expect("a {file, _} pair"), 2, "{name}");
    assert_eq!(pair_atom(runtime, loaded, 0).as_deref(), Some("file"));
    let module = atom(runtime, name);
    let loaded = call(runtime, "erlang", "module_loaded", &[module]).expect("erlang:module_loaded/1");
    assert_eq!(runtime.atom_name_of(loaded), Some("true"), "{name}");
}

/// Asserts `name` is **not** loaded: both `code:is_loaded/1` and
/// `erlang:module_loaded/1` answer `false`.
fn assert_not_loaded(runtime: &mut Runtime, name: &str) {
    let module = atom(runtime, name);
    let loaded = call(runtime, "code", "is_loaded", &[module]).expect("code:is_loaded/1");
    assert_eq!(runtime.atom_name_of(loaded), Some("false"), "{name} is_loaded");
    let module = atom(runtime, name);
    let loaded = call(runtime, "erlang", "module_loaded", &[module]).expect("erlang:module_loaded/1");
    assert_eq!(runtime.atom_name_of(loaded), Some("false"), "{name} module_loaded");
}

/// An `-on_load` hook that returns `ok` loads the module, reports it loaded, and its body's
/// process-dictionary side effect is observable — the proof the hook really ran.
#[test]
fn on_load_hook_runs_and_its_process_dictionary_side_effect_is_observable() {
    let Some(erlc) = erlc_or_skip() else {
        return;
    };
    let mut runtime = runtime();
    let fixture = compile_fixture(&erlc, "onload_ok", "onload_ok", ONLOAD_OK);
    runtime.code_server.add_path(fixture.path().to_path_buf());

    let answer = ensure_loaded(&mut runtime, "onload_ok").expect("code:ensure_loaded(onload_ok)");
    assert_module_pair(&runtime, answer, "onload_ok");
    assert_loaded(&mut runtime, "onload_ok");

    let key = atom(&mut runtime, "onload_side_effect");
    let effect = call(&mut runtime, "erlang", "get", &[key]).expect("erlang:get(onload_side_effect)");
    assert_eq!(runtime.atom_name_of(effect), Some("done"));
}

/// An `-on_load` hook that answers a non-`ok` term folds the load into
/// `{error, on_load_failure}` and leaves the module forgotten.
#[test]
fn on_load_hook_returning_a_non_ok_term_is_an_error_on_load_failure() {
    let Some(erlc) = erlc_or_skip() else {
        return;
    };
    let mut runtime = runtime();
    let fixture = compile_fixture(&erlc, "onload_fail", "onload_fail", ONLOAD_FAIL);
    runtime.code_server.add_path(fixture.path().to_path_buf());

    let answer = ensure_loaded(&mut runtime, "onload_fail").expect("code:ensure_loaded(onload_fail)");
    assert_error_pair(&runtime, answer, "on_load_failure");
    assert_not_loaded(&mut runtime, "onload_fail");
}

/// A module with no `-on_load` attribute loads and reports as before — the hook-less path is
/// unchanged.
#[test]
fn a_module_without_an_on_load_attribute_loads_unchanged() {
    let Some(erlc) = erlc_or_skip() else {
        return;
    };
    let mut runtime = runtime();
    let fixture = compile_fixture(&erlc, "plainmod", "plainmod", PLAIN_MODULE);
    runtime.code_server.add_path(fixture.path().to_path_buf());

    let answer = ensure_loaded(&mut runtime, "plainmod").expect("code:ensure_loaded(plainmod)");
    assert_module_pair(&runtime, answer, "plainmod");
    assert_loaded(&mut runtime, "plainmod");
}

/// `erlang:call_on_load_function/1` runs a loaded on_load module's hook again and answers its
/// return term; a module with no `-on_load` entry is `badarg`.
#[test]
fn call_on_load_function_runs_the_hook_and_answers_its_term() {
    let Some(erlc) = erlc_or_skip() else {
        return;
    };
    let mut runtime = runtime();
    let onload = compile_fixture(&erlc, "onload_ok", "onload_ok", ONLOAD_OK);
    let plain = compile_fixture(&erlc, "plainmod", "plainmod", PLAIN_MODULE);
    runtime.code_server.add_path(onload.path().to_path_buf());
    runtime.code_server.add_path(plain.path().to_path_buf());
    ensure_loaded(&mut runtime, "onload_ok").expect("onload_ok");
    ensure_loaded(&mut runtime, "plainmod").expect("plainmod");

    let onload = atom(&mut runtime, "onload_ok");
    let term = call(&mut runtime, "erlang", "call_on_load_function", &[onload])
        .expect("erlang:call_on_load_function(onload_ok)");
    assert_eq!(runtime.atom_name_of(term), Some("ok"));

    let plainmod = atom(&mut runtime, "plainmod");
    let error = call(&mut runtime, "erlang", "call_on_load_function", &[plainmod])
        .expect_err("plainmod has no -on_load entry");
    assert_eq!(error.as_atom(), Some("badarg"), "{error}");
}

/// `erlang:has_prepared_code_on_load/1` is `true` for an on_load module, `false` for a plain
/// one, and `badarg` for an unloaded module.
#[test]
fn has_prepared_code_on_load_distinguishes_on_load_from_plain_modules() {
    let Some(erlc) = erlc_or_skip() else {
        return;
    };
    let mut runtime = runtime();
    let onload = compile_fixture(&erlc, "onload_ok", "onload_ok", ONLOAD_OK);
    let plain = compile_fixture(&erlc, "plainmod", "plainmod", PLAIN_MODULE);
    runtime.code_server.add_path(onload.path().to_path_buf());
    runtime.code_server.add_path(plain.path().to_path_buf());
    ensure_loaded(&mut runtime, "onload_ok").expect("onload_ok");
    ensure_loaded(&mut runtime, "plainmod").expect("plainmod");

    let onload = atom(&mut runtime, "onload_ok");
    let yes = call(&mut runtime, "erlang", "has_prepared_code_on_load", &[onload])
        .expect("has_prepared_code_on_load(onload_ok)");
    assert_eq!(runtime.atom_name_of(yes), Some("true"));

    let plainmod = atom(&mut runtime, "plainmod");
    let no = call(&mut runtime, "erlang", "has_prepared_code_on_load", &[plainmod])
        .expect("has_prepared_code_on_load(plainmod)");
    assert_eq!(runtime.atom_name_of(no), Some("false"));

    let missing = atom(&mut runtime, "not_loaded_at_all");
    let error = call(&mut runtime, "erlang", "has_prepared_code_on_load", &[missing])
        .expect_err("an unloaded module is badarg");
    assert_eq!(error.as_atom(), Some("badarg"), "{error}");
}

/// `erlang:finish_after_on_load/2` rejects a non-boolean arg2, and `true`/`false` commit or
/// abort the hook outcome (`true` keeps the module loaded, `false` forgets it).
#[test]
fn finish_after_on_load_rejects_a_non_boolean_and_commits_or_aborts() {
    let Some(erlc) = erlc_or_skip() else {
        return;
    };

    // A non-boolean arg2 is badarg, whether a non-atom or a non-`true`/`false` atom.
    {
        let mut runtime = runtime();
        let fixture = compile_fixture(&erlc, "onload_ok", "onload_ok", ONLOAD_OK);
        runtime.code_server.add_path(fixture.path().to_path_buf());
        ensure_loaded(&mut runtime, "onload_ok").expect("onload_ok");
        let onload = atom(&mut runtime, "onload_ok");
        for bad in [atom(&mut runtime, "maybe"), make_small(42)] {
            let error = call(&mut runtime, "erlang", "finish_after_on_load", &[onload, bad])
                .expect_err("a non-boolean arg2 is badarg");
            assert_eq!(error.as_atom(), Some("badarg"), "{error}");
        }
    }

    // `true` commits: the answer is `true` and the module stays loaded.
    {
        let mut runtime = runtime();
        let fixture = compile_fixture(&erlc, "onload_ok", "onload_ok", ONLOAD_OK);
        runtime.code_server.add_path(fixture.path().to_path_buf());
        ensure_loaded(&mut runtime, "onload_ok").expect("onload_ok");
        let onload = atom(&mut runtime, "onload_ok");
        let keep = atom(&mut runtime, "true");
        let answer = call(&mut runtime, "erlang", "finish_after_on_load", &[onload, keep])
            .expect("finish_after_on_load(true)");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));
        assert_loaded(&mut runtime, "onload_ok");
    }

    // `false` aborts: the answer is `true` but the module is forgotten.
    {
        let mut runtime = runtime();
        let fixture = compile_fixture(&erlc, "onload_ok", "onload_ok", ONLOAD_OK);
        runtime.code_server.add_path(fixture.path().to_path_buf());
        ensure_loaded(&mut runtime, "onload_ok").expect("onload_ok");
        let onload = atom(&mut runtime, "onload_ok");
        let drop = atom(&mut runtime, "false");
        let answer = call(&mut runtime, "erlang", "finish_after_on_load", &[onload, drop])
            .expect("finish_after_on_load(false)");
        assert_eq!(runtime.atom_name_of(answer), Some("true"));
        assert_not_loaded(&mut runtime, "onload_ok");
    }
}

/// The `crypto` application's `-on_load` hook loads its NIF library end to end:
/// `code:ensure_loaded(crypto)` answers `{module, crypto}` and
/// `crypto:strong_rand_bytes(16)` yields a 16-byte binary.
///
/// A host whose `crypto.so` cannot load makes crypto's own `-on_load` return non-`ok`, so the
/// load folds into `{error, on_load_failure}` — an **environment** limitation, not a runtime
/// defect — and this test skips loudly rather than failing.
#[test]
fn crypto_on_load_end_to_end_strong_rand_bytes() {
    let Some(mut runtime) = system_runtime() else {
        return; // `system_runtime()` already printed the loud "no system code path" skip.
    };
    let crypto = atom(&mut runtime, "crypto");
    let answer = call(&mut runtime, "code", "ensure_loaded", &[crypto])
        .expect("code:ensure_loaded(crypto)");
    if pair_atom(&runtime, answer, 0).as_deref() == Some("error") {
        eprintln!();
        eprintln!("=============== beam_rt on_load crypto SKIPPED ===============");
        eprintln!("code:ensure_loaded(crypto) answered {{error, _}}: crypto's NIF library");
        eprintln!("cannot load on this host (an environment limitation, not a runtime");
        eprintln!("defect). Re-run on a host with crypto.so available. NOT a pass.");
        eprintln!("==============================================================");
        eprintln!();
        return;
    }
    assert_module_pair(&runtime, answer, "crypto");
    assert_loaded(&mut runtime, "crypto");

    let sixteen = make_small(16);
    let bytes = call(&mut runtime, "crypto", "strong_rand_bytes", &[sixteen])
        .expect("crypto:strong_rand_bytes(16)");
    let size = call(&mut runtime, "erlang", "byte_size", &[bytes]).expect("erlang:byte_size/1");
    assert_eq!(signed_value(size), 16, "strong_rand_bytes(16) is 16 bytes");
}

/// A module whose `-on_load` body blocks on a scheduler-produced reply loads through the
/// **scheduled** `run_scheduled` route: the hook is deferred to the scheduler boundary's drain
/// (a pumpable context, exactly the `-eval` route) instead of folding into `blocked()` inside the
/// slice, so `code:ensure_loaded/1` answers `{module, _}` and `erlang:function_exported/3`
/// answers `true`.
#[test]
fn a_blocking_on_load_hook_loads_through_the_scheduled_route() {
    let Some(erlc) = erlc_or_skip() else {
        return;
    };
    let mut runtime = runtime();
    let block = compile_fixture(&erlc, "onload_block", "onload_block", ONLOAD_BLOCK);
    let driver = compile_fixture(&erlc, "onload_driver", "onload_driver", ONLOAD_DRIVER);
    runtime.code_server.add_path(block.path().to_path_buf());
    runtime.code_server.add_path(driver.path().to_path_buf());

    let driver_beam = driver.path().join("onload_driver.beam");
    let answer = runtime
        .run_scheduled(driver_beam, "main", 0, &[])
        .expect("main/0 through the scheduler");

    assert_eq!(terms::tuple_arity(answer).expect("a 2-tuple"), 2, "{answer:?}");
    let ensure = terms::tuple_element(answer, 0).expect("element 0");
    assert_module_pair(&runtime, ensure, "onload_block");
    let exported = terms::tuple_element(answer, 1).expect("element 1");
    assert_eq!(runtime.atom_name_of(exported), Some("true"), "{exported:?}");

    // The deferred hook also leaves `onload_block` loaded after the scheduled run, so both the
    // export predicate and the introspection BIFs agree through the pumpable route too.
    let module = atom(&mut runtime, "onload_block");
    let hello = atom(&mut runtime, "hello");
    let exported = call(&mut runtime, "erlang", "function_exported", &[module, hello, make_small(0)])
        .expect("erlang:function_exported(onload_block, hello, 0)");
    assert_eq!(runtime.atom_name_of(exported), Some("true"));
    assert_loaded(&mut runtime, "onload_block");
}

/// A **non-blocking** `-on_load` hook runs synchronously on the **scheduled** route: the hook
/// body's process-dictionary side effect is observable in the very `run_scheduled` run that
/// loaded it, proving the hook ran inline during `code:ensure_loaded/1` rather than being
/// deferred to the scheduler boundary's drain.
#[test]
fn a_non_blocking_on_load_hook_runs_synchronously_on_the_scheduled_route() {
    let Some(erlc) = erlc_or_skip() else {
        return;
    };
    let mut runtime = runtime();
    let fixture = compile_fixture(&erlc, "onload_ok_sched", "onload_ok", ONLOAD_OK);
    let driver = compile_fixture(
        &erlc,
        "onload_sync_driver",
        "onload_sync_driver",
        ONLOAD_SYNC_DRIVER,
    );
    runtime.code_server.add_path(fixture.path().to_path_buf());
    runtime.code_server.add_path(driver.path().to_path_buf());

    let driver_beam = driver.path().join("onload_sync_driver.beam");
    let answer = runtime
        .run_scheduled(driver_beam, "main", 0, &[])
        .expect("main/0 through the scheduler");

    assert_eq!(terms::tuple_arity(answer).expect("a 2-tuple"), 2, "{answer:?}");
    let ensure = terms::tuple_element(answer, 0).expect("element 0");
    assert_module_pair(&runtime, ensure, "onload_ok");
    let effect = terms::tuple_element(answer, 1).expect("element 1");
    assert_eq!(runtime.atom_name_of(effect), Some("done"), "{effect:?}");
}

/// `erlang:function_exported/3` answers `true` immediately after `code:ensure_loaded/1` in the
/// same **scheduled** run — the freshly-loaded module is immediately usable, which is the
/// load-order a NIF-hosting module's consumers rely on (`ensure_loaded` then `function_exported`).
#[test]
fn function_exported_answers_true_immediately_after_load_on_the_scheduled_route() {
    let Some(erlc) = erlc_or_skip() else {
        return;
    };
    let mut runtime = runtime();
    let fixture = compile_fixture(&erlc, "onload_ok_exported", "onload_ok", ONLOAD_OK);
    let driver = compile_fixture(
        &erlc,
        "onload_exported_driver",
        "onload_exported_driver",
        ONLOAD_EXPORTED_DRIVER,
    );
    runtime.code_server.add_path(fixture.path().to_path_buf());
    runtime.code_server.add_path(driver.path().to_path_buf());

    let driver_beam = driver.path().join("onload_exported_driver.beam");
    let answer = runtime
        .run_scheduled(driver_beam, "main", 0, &[])
        .expect("main/0 through the scheduler");

    assert_eq!(terms::tuple_arity(answer).expect("a 2-tuple"), 2, "{answer:?}");
    let ensure = terms::tuple_element(answer, 0).expect("element 0");
    assert_module_pair(&runtime, ensure, "onload_ok");
    let exported = terms::tuple_element(answer, 1).expect("element 1");
    assert_eq!(runtime.atom_name_of(exported), Some("true"), "{exported:?}");
}
