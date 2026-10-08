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

//! End-to-end proof of the NIF C ABI (workspace contract **A.3**): a **real C NIF shared object** —
//! compiled with the system `cc` against the system `erl_nif.h` — is `dlopen`ed, validated, and
//! called through the Rust loader, with its `enif_*` imports resolving out of the *test binary's*
//! dynamic symbol table.
//!
//! # Why this is the load-bearing test
//!
//! `tests/fixtures/test_nif.c` is an ordinary NIF module: it `#include`s `erl_nif.h` and calls
//! `enif_get_int`, `enif_make_atom`, … whose definitions live in *this* crate
//! (`beam_abi::nif::enif`). The linker leaves those six calls **undefined** in the `.so` (verified
//! below by inspecting the `.so`'s dynamic symbol table), so `dlopen(..., RTLD_NOW)` in
//! [`beam_abi::nif::loader::load`] can only succeed if the *loading process* exports them as
//! dynamic symbols. That is what `build.rs` arranges with `-Wl,--export-dynamic` on the test
//! targets; `the_test_binary_exports_the_enif_symbols_dynamically` pins the mechanism directly, and
//! the successful load in `loads_and_calls_a_real_c_nif_library` is the end-to-end consequence.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; inspecting symbol tables
//! needs `nm`/`readelf`. When a *toolchain* facility is missing the affected test prints a loud
//! `SKIP:` banner and returns — but when the toolchain **is** present, every assertion is hard
//! (a mismatch fails, it never silently passes).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::enif::enif_api_table;
use beam_abi::nif::loader::{load, NifCallError, NifLoadError};
use beam_abi::NifEnv;
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::compare;
use beam_term::prelude::{make_small, Eterm, NIL};

use common::{compile_so, skip, TestHost};

/// The committed C fixture — a real NIF module (see the file header for the A.3 rationale).
const FIXTURE_C: &str = include_str!("fixtures/test_nif.c");

/// The `enif_*` symbols the fixture imports; they must be *undefined* in the `.so` and *defined* in
/// the loading process.
const FIXTURE_IMPORTS: [&str; 6] = [
    "enif_get_int",
    "enif_make_int",
    "enif_make_atom",
    "enif_make_reverse_list",
    "enif_make_tuple_from_array",
    "enif_make_badarg",
];

/// The five `{name, arity}` pairs the fixture declares, in its declared order.
const EXPECTED_FUNCS: [(&str, u32); 5] = [
    ("add", 2),
    ("hello", 0),
    ("reverse", 1),
    ("raise", 0),
    ("make_tuple", 2),
];

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `None` (after a loud skip) when the compiled `.so` path is not valid UTF-8.
fn so_path(so: &Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// Build the proper list `[items…]` of small ints on `heap` (right to left).
fn int_list(heap: &mut Heap, items: &[i64]) -> Eterm {
    let mut acc = NIL;
    for &value in items.iter().rev() {
        acc = heap
            .cons(make_small(value), acc)
            .expect("the test heap has room for a three-element list");
    }
    acc
}

/// Run `cmd args…`, returning its stdout on success (`None` when the tool is missing or fails).
fn run_tool(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `Some(true)`/`Some(false)` when a symbol-dump tool inspected `path`, `None` when neither `nm`
/// nor `readelf` is usable. `true` iff every name in `names` is an **undefined** import of `path`.
fn all_symbols_are_undefined_imports(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--undefined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines()
                .any(|line| line.contains(name) && line.contains(" UND "))
        }));
    }
    None
}

/// As [`all_symbols_are_undefined_imports`], but every name must be **defined** in `path`'s
/// dynamic symbol table (i.e. exported by it).
fn all_symbols_are_exported(path: &Path, names: &[&str]) -> Option<bool> {
    let p = path.display().to_string();
    if let Some(out) = run_tool("nm", &["-D", "--defined-only", &p]) {
        return Some(names.iter().all(|name| out.contains(name)));
    }
    if let Some(out) = run_tool("readelf", &["--dyn-syms", "-W", &p]) {
        return Some(names.iter().all(|name| {
            out.lines()
                .any(|line| line.contains(name) && !line.contains(" UND "))
        }));
    }
    None
}

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so("test_nif", FIXTURE_C)
}

// ---------------------------------------------------------------------------------------------
// The frozen `enif_*` table
// ---------------------------------------------------------------------------------------------

#[test]
fn the_enif_table_is_populated_for_every_symbol_the_fixture_imports() {
    // Calling `enif_api_table()` both pins the frozen table shape and forces the
    // `enif_*`-carrying codegen unit into the link, so `--export-dynamic` can publish it.
    let table = enif_api_table();
    assert!(table.enif_alloc.is_some());
    assert!(table.enif_get_int.is_some());
    assert!(table.enif_make_int.is_some());
    assert!(table.enif_make_atom.is_some());
    assert!(table.enif_make_reverse_list.is_some());
    assert!(table.enif_make_tuple_from_array.is_some());
    assert!(table.enif_make_badarg.is_some());
}

// ---------------------------------------------------------------------------------------------
// The `--export-dynamic` mechanism, checked directly
// ---------------------------------------------------------------------------------------------

#[test]
fn the_test_binary_exports_the_enif_symbols_dynamically() {
    // Force the `enif_*` definitions into the link before inspecting it.
    assert!(enif_api_table().enif_alloc.is_some());

    let Ok(exe) = std::fs::canonicalize("/proc/self/exe") else {
        skip("the_test_binary_exports_the_enif_symbols: /proc/self/exe is unavailable");
        return;
    };
    match all_symbols_are_exported(&exe, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "`build.rs` emits `-Wl,--export-dynamic` for the test targets, so {} must appear in \
             {:?}'s dynamic symbol table",
            FIXTURE_IMPORTS.join("/"),
            exe
        ),
        None => skip("the_test_binary_exports_the_enif_symbols: no nm/readelf available"),
    }
}

// ---------------------------------------------------------------------------------------------
// End-to-end: load a real NIF `.so` and call its NIFs
// ---------------------------------------------------------------------------------------------

#[test]
fn loads_and_calls_a_real_c_nif_library() {
    // Load-bearing: this pins the frozen `enif_*` table AND keeps the `enif_*`-carrying codegen unit
    // in the link (without it the `.so`'s imports would not resolve and `dlopen` would fail).
    assert!(enif_api_table().enif_alloc.is_some());

    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    // --- the `.so` really imports `enif_*` (they are bound at load time, not at link time) -------
    match all_symbols_are_undefined_imports(&so, &FIXTURE_IMPORTS) {
        Some(true) => {}
        Some(false) => panic!(
            "the fixture must reference {} as undefined dynamic symbols; inspect {} with `nm -D`",
            FIXTURE_IMPORTS.join("/"),
            so.display()
        ),
        None => skip("loads_and_calls: no nm/readelf to inspect the .so's dynamic symbols"),
    }

    // --- dlopen(RTLD_NOW) + the `ErlNifEntry` validation + the `load` callback ------------------
    let mut host = TestHost::new();
    let lib = load(&path, "test_nif", &mut host, Eterm::nil())
        .expect("a real NIF library must load: its enif_* imports resolve from the test binary");

    assert_eq!(lib.module_name(), "test_nif");
    assert_eq!(lib.funcs().len(), EXPECTED_FUNCS.len());
    for (name, arity) in EXPECTED_FUNCS {
        assert!(
            lib.funcs().iter().any(|(n, a)| n == name && *a == arity),
            "the fixture declares {name}/{arity}; found {:?}",
            lib.funcs()
        );
    }
    assert!(lib.priv_data().is_null(), "the fixture's `load` stores no priv");

    let mut env = NifEnv::new(&mut host);

    // add/2 -> 5.
    assert_eq!(
        lib.call(&mut env, "add", 2, &[make_small(2), make_small(3)])
            .expect("add/2"),
        make_small(5)
    );

    // hello/0 -> the atom `hello` (atoms are immediates and interned once, so `==` is exact).
    let hello = env.host().intern_atom("hello").expect("interns");
    assert_eq!(lib.call(&mut env, "hello", 0, &[]).expect("hello/0"), hello);

    // reverse/1 of [1,2,3] -> [3,2,1] (structurally — the cells are freshly allocated).
    let input = int_list(env.heap(), &[1, 2, 3]);
    let reversed = lib.call(&mut env, "reverse", 1, &[input]).expect("reverse/1");
    let expected = int_list(env.heap(), &[3, 2, 1]);
    assert!(
        compare::eq(reversed, expected, &AtomTable::new()),
        "reverse/1 must return [3,2,1]"
    );

    // make_tuple/2 of two small ints -> {1,2} (structurally — freshly allocated on both sides).
    let expected_tuple = env
        .heap()
        .tuple(&[make_small(1), make_small(2)])
        .expect("the test heap has room for a 2-tuple");
    let got_tuple = lib
        .call(&mut env, "make_tuple", 2, &[make_small(1), make_small(2)])
        .expect("make_tuple/2");
    assert!(
        compare::eq(got_tuple, expected_tuple, &AtomTable::new()),
        "make_tuple/2 must return {{1,2}}"
    );

    // raise/0 -> the NIF's `enif_make_badarg` surfaces as a raised exception.
    let badarg = env.host().intern_atom("badarg").expect("interns");
    match lib.call(&mut env, "raise", 0, &[]) {
        Err(NifCallError::Raised(reason)) => assert_eq!(reason, badarg),
        other => panic!("expected NifCallError::Raised(badarg), got {other:?}"),
    }
}

#[test]
fn call_reports_unknown_functions_and_wrong_types_without_panicking() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let lib = load(&path, "test_nif", &mut host, Eterm::nil()).expect("the fixture must load");
    let mut env = NifEnv::new(&mut host);

    // A bogus name.
    match lib.call(&mut env, "does_not_exist", 3, &[]) {
        Err(NifCallError::UnknownFunction { name, arity }) => {
            assert_eq!(name, "does_not_exist");
            assert_eq!(arity, 3);
        }
        other => panic!("expected UnknownFunction, got {other:?}"),
    }
    // A real name with the wrong arity.
    assert!(matches!(
        lib.call(&mut env, "add", 1, &[make_small(1)]),
        Err(NifCallError::UnknownFunction { .. })
    ));

    // A real NIF with a wrong-typed argument: the fixture's `enif_make_badarg` is surfaced.
    let not_an_int = env.host().intern_atom("not_an_int").expect("interns");
    match lib.call(&mut env, "add", 2, &[not_an_int, make_small(3)]) {
        Err(NifCallError::Raised(reason)) => {
            assert_eq!(reason, env.host().intern_atom("badarg").expect("interns"))
        }
        other => panic!("expected Raised(badarg) for add/2(atom, 3), got {other:?}"),
    }

    // reverse/1 of a non-list raises the same way (the fixture vetoes a non-list).
    assert!(matches!(
        lib.call(&mut env, "reverse", 1, &[make_small(7)]),
        Err(NifCallError::Raised(_))
    ));
}

// ---------------------------------------------------------------------------------------------
// The oracle's `bad_lib` validation, through the real loader
// ---------------------------------------------------------------------------------------------

#[test]
fn rejects_a_library_that_needs_a_newer_runtime() {
    const FUTURE_C: &str = r#"
#include <erl_nif.h>

static ErlNifEntry futuristic_entry = {
    .major = 99,
    .minor = 0,
    .name = "futuristic",
    .num_of_funcs = 0,
    .funcs = NULL,
    .load = NULL,
    .reload = NULL,
    .upgrade = NULL,
    .unload = NULL,
    .vm_variant = "beam.vanilla",
    .options = 1,
    .sizeof_ErlNifResourceTypeInit = 0,
    .min_erts = "erts-99.0",
};

ErlNifEntry* nif_init(void) { return &futuristic_entry; }
"#;

    let Some(so) = compile_so("nif_end_to_end_future", FUTURE_C) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let err = load(&path, "futuristic", &mut host, Eterm::nil()).expect_err("must be rejected");
    assert!(matches!(err, NifLoadError::BadLib(_)), "got {err:?}");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert!(
        err.text().contains("needs") && err.text().contains("or newer"),
        "unexpected text: {}",
        err.text()
    );
}

#[test]
fn rejects_a_shared_object_without_nif_init() {
    const NO_INIT_C: &str = r#"
#include <erl_nif.h>

int beam_abi_not_a_nif_init(void) { return 0; }
"#;

    let Some(so) = compile_so("nif_end_to_end_noinit", NO_INIT_C) else {
        return;
    };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let err = load(&path, "test_nif", &mut host, Eterm::nil()).expect_err("must be rejected");
    assert!(matches!(err, NifLoadError::BadLib(_)), "got {err:?}");
    assert_eq!(err.reason_atom(), "bad_lib");
    assert!(
        err.text().contains("Failed to find library init function"),
        "unexpected text: {}",
        err.text()
    );
}
