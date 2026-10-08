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

//! Integration coverage for the dynamic NIF loader (`beam_abi::nif::loader`).
//!
//! The fixtures below are compiled with the shared `tests/common` toolchain (`cc -shared -fPIC`
//! against the system `erl_nif.h`). They deliberately reference **no** `enif_*` symbol — every NIF
//! body returns `argv[0]` or a constant word — so `dlopen(..., RTLD_NOW)` must succeed without the
//! runtime's `enif_*` exports (those are a separate step). When there is no C compiler or no
//! `erl_nif.h`, `common::compile_so` prints a loud `SKIP:` banner and the test returns.
//!
//! The `load`/`unload` callbacks are observed through a Rust `extern "C" fn` smuggled in through
//! `load_nif/2`'s `LoadInfo` argument (a bare machine word): the fixture stores it as its `priv`
//! pointer and calls it from `unload`, which lets the test prove that a drop really invoked the
//! unload callback without depending on libc file I/O inside the fixture.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};

use beam_abi::nif::loader::{load, NifCallError, NifLoadError};
use beam_abi::NifEnv;
use beam_term::prelude::{make_small, Eterm};

use common::{compile_so, skip, TestHost};

/// Set by the fixture's `unload` callback (called from `NifLibrary::drop`).
static UNLOAD_RAN: AtomicBool = AtomicBool::new(false);

/// The `void(void)` callback the happy fixture stores as its `priv` pointer.
extern "C" fn mark_unload() {
    UNLOAD_RAN.store(true, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------------------------
// C fixtures
// ---------------------------------------------------------------------------------------------

/// A well-formed NIF library named `t`: `echo/1` returns `argv[0]`, `const/0` a fixed small int,
/// `load` stores the `LoadInfo` word as `priv`, `unload` calls it.
const HAPPY: &str = r#"
#include <erl_nif.h>
#include <stdint.h>

static ERL_NIF_TERM t_echo(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)env; (void)argc;
    return argv[0];
}

static ERL_NIF_TERM t_const(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)env; (void)argc; (void)argv;
    return (ERL_NIF_TERM)((12345UL << 4) | 0xFUL);
}

static int t_load(ErlNifEnv* env, void** priv, ERL_NIF_TERM info) {
    (void)env;
    *priv = (void*)(uintptr_t)info;
    return 0;
}

static void t_unload(ErlNifEnv* env, void* priv) {
    (void)env;
    if (priv) ((void (*)(void))priv)();
}

static ErlNifFunc t_nif_funcs[] = {
    {"echo", 1, t_echo, 0},
    {"const", 0, t_const, 0}
};

ERL_NIF_INIT(t, t_nif_funcs, t_load, NULL, NULL, t_unload)
"#;

/// A minimal well-formed NIF library (`echo/1` only, no load/unload callbacks).
const ECHO_ONLY: &str = r#"
#include <erl_nif.h>

static ERL_NIF_TERM t_echo(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)env; (void)argc;
    return argv[0];
}

static ErlNifFunc t_nif_funcs[] = { {"echo", 1, t_echo, 0} };

ERL_NIF_INIT(t, t_nif_funcs, NULL, NULL, NULL, NULL)
"#;

/// A shared object with an unrelated symbol and no `nif_init`.
const NO_INIT: &str = r#"
#include <erl_nif.h>

int beam_abi_not_a_nif_init(void) { return 0; }
"#;

/// A hand-written `nif_init` returning a `static ErlNifEntry` filled field by field.
///
/// `funcs` is spliced into the `.funcs` initialiser (`"NULL"` or `"t_funcs"`), `load` into `.load`
/// (`"NULL"` or `"veto_load"`), and `name` into `.name` (`"NULL"` produces a null name pointer).
fn custom_entry_source(
    major: i32,
    minor: i32,
    name: &str,
    variant: &str,
    min_erts: &str,
    num_of_funcs: i32,
    funcs: &str,
    load: &str,
) -> String {
    format!(
        r#"#include <erl_nif.h>

static int veto_load(ErlNifEnv* env, void** priv, ERL_NIF_TERM info) {{
    (void)env; (void)priv; (void)info;
    return 7;
}}

static ERL_NIF_TERM t_one(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {{
    (void)env; (void)argc;
    return argv[0];
}}

static ErlNifFunc t_funcs[] = {{ {{"one", 1, t_one, 0}} }};

static ErlNifEntry t_entry = {{
    .major = {major},
    .minor = {minor},
    .name = {name},
    .num_of_funcs = {num_of_funcs},
    .funcs = {funcs},
    .load = {load},
    .reload = NULL,
    .upgrade = NULL,
    .unload = NULL,
    .vm_variant = {variant},
    .options = 1,
    .sizeof_ErlNifResourceTypeInit = 0,
    .min_erts = {min_erts},
}};

ErlNifEntry* nif_init(void) {{ return &t_entry; }}
"#
    )
}

/// `None` (after a loud skip) when the compiled `.so` path is not valid UTF-8.
fn so_path(so: &std::path::Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------------------------

#[test]
fn loads_calls_and_unloads_a_working_library() {
    UNLOAD_RAN.store(false, Ordering::SeqCst);
    let Some(so) = compile_so("loader_happy", HAPPY) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };
    // Strip the suffix so `load` has to append it (the documented `FILE_EXT` behaviour).
    let stem = path
        .strip_suffix(".so")
        .expect("`compile_so` always writes a `.so`")
        .to_string();

    let callback = mark_unload as extern "C" fn();
    let mut host = TestHost::new();
    let lib = load(&stem, "t", &mut host, Eterm::from_raw(callback as usize))
        .expect("the fixture NIF library must load");

    assert_eq!(lib.module_name(), "t");
    assert_eq!(lib.funcs().len(), 2);
    assert!(lib.funcs().iter().any(|(n, a)| n == "echo" && *a == 1));
    assert!(lib.funcs().iter().any(|(n, a)| n == "const" && *a == 0));
    assert_eq!(
        lib.priv_data() as usize,
        callback as usize,
        "the `load` callback ran and stored its priv pointer"
    );

    let mut env = NifEnv::new(&mut host);
    let x = make_small(4321);
    assert_eq!(lib.call(&mut env, "echo", 1, &[x]).expect("echo/1"), x);
    assert_eq!(
        lib.call(&mut env, "const", 0, &[]).expect("const/0"),
        make_small(12345)
    );

    drop(lib);
    assert!(
        UNLOAD_RAN.load(Ordering::SeqCst),
        "dropping the library invoked the `unload` callback"
    );
}

#[test]
fn call_reports_unknown_functions_and_raised_exceptions() {
    let Some(so) = compile_so("loader_echo", ECHO_ONLY) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let lib = load(&path, "t", &mut host, Eterm::nil()).expect("the fixture NIF library must load");
    let mut env = NifEnv::new(&mut host);

    // Unknown name.
    match lib.call(&mut env, "nope", 2, &[]) {
        Err(NifCallError::UnknownFunction { name, arity }) => {
            assert_eq!(name, "nope");
            assert_eq!(arity, 2);
        }
        other => panic!("expected UnknownFunction, got {other:?}"),
    }
    // Known name, wrong arity.
    assert!(matches!(
        lib.call(&mut env, "echo", 2, &[]),
        Err(NifCallError::UnknownFunction { .. })
    ));

    // A recorded exception is surfaced after the call.
    let reason = env.host().intern_atom("badarg").expect("interns");
    env.set_exception(reason);
    match lib.call(&mut env, "echo", 1, &[make_small(7)]) {
        Err(NifCallError::Raised(t)) => assert_eq!(t, reason),
        other => panic!("expected Raised, got {other:?}"),
    }
    env.clear();
    assert_eq!(
        lib.call(&mut env, "echo", 1, &[make_small(7)]).unwrap(),
        make_small(7)
    );
}

// ---------------------------------------------------------------------------------------------
// Validation failures (oracle order + verbatim texts)
// ---------------------------------------------------------------------------------------------

#[test]
fn rejects_a_library_that_needs_a_newer_runtime() {
    let Some(so) = compile_so(
        "loader_new",
        &custom_entry_source(
            99,
            0,
            "\"t\"",
            "\"beam.vanilla\"",
            "\"erts-99.0\"",
            0,
            "NULL",
            "NULL",
        ),
    ) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let err = load(&path, "t", &mut host, Eterm::nil()).expect_err("must be rejected");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert_eq!(
        err.text(),
        "That 't' NIF library needs erts-99.0 or newer. Either try to recompile the NIF lib or use a newer erts runtime."
    );
}

#[test]
fn rejects_an_old_library_version() {
    let Some(so) = compile_so(
        "loader_old",
        &custom_entry_source(
            1,
            0,
            "\"t\"",
            "\"beam.vanilla\"",
            "\"erts-1.0\"",
            0,
            "NULL",
            "NULL",
        ),
    ) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let err = load(&path, "t", &mut host, Eterm::nil()).expect_err("must be rejected");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert_eq!(
        err.text(),
        "That old NIF library (1.0) is not compatible with this erts runtime (2.18). Try recompile the NIF lib."
    );
}

#[test]
fn rejects_a_mismatched_vm_variant() {
    let Some(so) = compile_so(
        "loader_variant",
        &custom_entry_source(
            2,
            18,
            "\"t\"",
            "\"other.vm\"",
            "\"erts-14.0\"",
            0,
            "NULL",
            "NULL",
        ),
    ) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let err = load(&path, "t", &mut host, Eterm::nil()).expect_err("must be rejected");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert_eq!(
        err.text(),
        "Library (other.vm) not compiled for this vm variant (beam.vanilla)."
    );
}

#[test]
fn rejects_a_mismatched_module_name() {
    let Some(so) = compile_so(
        "loader_name",
        &custom_entry_source(
            2,
            18,
            "\"other\"",
            "\"beam.vanilla\"",
            "\"erts-14.0\"",
            0,
            "NULL",
            "NULL",
        ),
    ) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let err = load(&path, "t", &mut host, Eterm::nil()).expect_err("must be rejected");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert_eq!(
        err.text(),
        "Library module name 'other' does not match calling module 't'"
    );
}

#[test]
fn rejects_a_library_without_an_init_function() {
    let Some(so) = compile_so("loader_noinit", NO_INIT) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let err = load(&path, "t", &mut host, Eterm::nil()).expect_err("must be rejected");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert!(
        err.text()
            .starts_with("Failed to find library init function: '"),
        "unexpected text: {}",
        err.text()
    );
    assert!(
        err.text().contains("undefined symbol: _nif_init"),
        "unexpected text: {}",
        err.text()
    );
}

#[test]
fn reports_a_vetoing_load_callback() {
    let Some(so) = compile_so(
        "loader_veto",
        &custom_entry_source(
            2,
            18,
            "\"t\"",
            "\"beam.vanilla\"",
            "\"erts-14.0\"",
            0,
            "NULL",
            "veto_load",
        ),
    ) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let err = load(&path, "t", &mut host, Eterm::nil()).expect_err("must be rejected");
    assert_eq!(err.reason_atom(), "load");
    assert_eq!(err.text(), "Library load-call unsuccessful (7).");
}

// ---------------------------------------------------------------------------------------------
// `dlopen` failure and malformed input (never a panic, never a NULL deref)
// ---------------------------------------------------------------------------------------------

#[test]
fn reports_a_failed_dlopen() {
    let missing = std::env::temp_dir().join(format!(
        "beam_abi-loader-does-not-exist-{}.so",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&missing);

    let mut host = TestHost::new();
    let err = load(
        missing.to_str().expect("temp path is UTF-8"),
        "t",
        &mut host,
        Eterm::nil(),
    )
    .expect_err("a nonexistent path must not load");
    assert!(matches!(err, NifLoadError::LoadFailed(_)));
    assert_eq!(err.reason_atom(), "load_failed");
    assert!(
        err.text().starts_with("Failed to load NIF library"),
        "unexpected text: {}",
        err.text()
    );
    assert!(
        err.text().contains("does-not-exist"),
        "the loader's message names the path: {}",
        err.text()
    );
}

#[test]
fn malformed_entries_are_rejected_without_panicking() {
    // (a) a function count with a NULL function table: the C would dereference NULL.
    let Some(null_funcs) = compile_so(
        "loader_null_funcs",
        &custom_entry_source(
            2,
            18,
            "\"t\"",
            "\"beam.vanilla\"",
            "\"erts-14.0\"",
            3,
            "NULL",
            "NULL",
        ),
    ) else {
        return;
    };
    let Some(path) = so_path(&null_funcs) else {
        return;
    };
    let mut host = TestHost::new();
    let err = load(&path, "t", &mut host, Eterm::nil()).expect_err("a NULL table must be rejected");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert!(
        err.text().contains("function table is NULL"),
        "unexpected text: {}",
        err.text()
    );

    // (b) a NULL module name: treated as a name mismatch, never a segfault.
    let Some(null_name) = compile_so(
        "loader_null_name",
        &custom_entry_source(
            2,
            18,
            "NULL",
            "\"beam.vanilla\"",
            "\"erts-14.0\"",
            0,
            "NULL",
            "NULL",
        ),
    ) else {
        return;
    };
    let Some(path) = so_path(&null_name) else {
        return;
    };
    let err = load(&path, "t", &mut host, Eterm::nil()).expect_err("a NULL name must be rejected");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert_eq!(
        err.text(),
        "Library module name '' does not match calling module 't'"
    );

    // (c) a function table that is present and well-formed must still load (guards (a)/(b) against
    // an over-eager check).
    let Some(ok) = compile_so(
        "loader_ok_table",
        &custom_entry_source(
            2,
            18,
            "\"t\"",
            "\"beam.vanilla\"",
            "\"erts-14.0\"",
            1,
            "t_funcs",
            "NULL",
        ),
    ) else {
        return;
    };
    let Some(path) = so_path(&ok) else { return };
    let lib = load(&path, "t", &mut host, Eterm::nil()).expect("a well-formed table must load");
    assert_eq!(lib.funcs(), &[("one".to_string(), 1)]);
}
