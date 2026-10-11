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

//! End-to-end proof of the **NIF thread / mutex / cond / rwlock / TSD** slots (contract A.3).
//!
//! `tests/fixtures/nif_threads.c` is a real C NIF — compiled at test time with the system `cc`
//! against the system `erl_nif.h` — whose `enif_mutex_*`, `enif_cond_*`, `enif_rwlock_*`,
//! `enif_tsd_*` and `enif_thread_*` calls are **undefined** in the `.so` and are bound at
//! `dlopen(RTLD_NOW)` against this test binary's dynamic symbol table (`build.rs` adds
//! `-Wl,--export-dynamic`). A successful load therefore already proves those `#[no_mangle]`
//! definitions exist and are callable; the assertions below pin the *observable behaviour* of each.
//!
//! Every fixture NIF returns a tuple of small integers, so the **same** `.so` can be loaded by the
//! system Erlang and its printed result compared with the Rust runtime's — which is what the second
//! test does (it skips loudly when `erl`/`erlc` are absent).
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; `compile_so` prints a loud
//! `SKIP:` banner and returns `None` when either is missing. When the toolchain **is** present every
//! assertion is hard.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::loader::{load, NifLibrary};
use beam_abi::NifEnv;
use beam_term::atom::AtomTable;
use beam_term::compare;
use beam_term::prelude::{make_small, Eterm};

use common::{compile_so, skip, TestHost};

/// The committed C fixture (see the file header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_threads.c");

/// The fixture's `ERL_NIF_INIT` name — the loader (and the oracle) require it to match the calling
/// module, so it doubles as the name of the Erlang module the oracle test generates.
const MODULE: &str = "test_nif_threads";

// ---------------------------------------------------------------------------------------------
// The expected results
// ---------------------------------------------------------------------------------------------

/// Every fixture function whose result is **identical** in this runtime and in the system Erlang,
/// with the tuple it must return.
///
/// The single excluded slot is `thread_type/0`: this runtime never runs a NIF on a scheduler thread,
/// so it reports `ERL_NIF_THR_UNDEFINED` (0) where the oracle reports a scheduler type — a documented
/// deviation (see `src/nif/enif/threads.rs`).
#[rustfmt::skip]
const COMPARABLE: &[(&str, &[i64])] = &[
    //                                  created, trylock(held) = EBUSY, trylock(free) = 0
    ("mutex_basic",         &[1, libc::EBUSY as i64, 0]),
    //                                  created, join ret, the waiter returned 42
    ("cond_wait_signal",    &[1, 0, 1]),
    //                                  created, join ret 1, join ret 2, 7 + 7
    ("cond_wait_broadcast", &[1, 0, 0, 14]),
    //                                  created, tryrwlock(while rlocked) = EBUSY, rwlock round-trip
    ("rwlock_basic",        &[1, libc::EBUSY as i64, 0]),
    //                                  key ret, create ret, child saw NULL, parent unchanged, join
    ("tsd_roundtrip",       &[0, 0, 1, 1, 0]),
    //                                  opts, create ret, join ret, resp 42, self non-NULL, equal
    ("thread_create_join",  &[1, 0, 0, 42, 1, 1]),
    //                                  create ret, join ret, the exit argument
    ("thread_exit_value",   &[0, 0, 99]),
    //                                  the three NAMED handles report their names
    ("names",               &[1, 1, 1]),
    //                                  unnamed handles report "unknown"; NULL handles report NULL
    ("names_unnamed",       &[1, 1, 1, 1, 1, 1]),
    //                                  both creates ok; unnamed thread "unknown"; named thread named;
    //                                  a NULL tid reports NULL
    ("thread_names",        &[1, 1, 1, 1]),
];

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The fixture compiled into a shared object, or `None` (after a loud skip) when the toolchain is
/// absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(MODULE, FIXTURE_C)
}

/// The `.so` path as a `&str`, or `None` (after a loud skip) when it is not valid UTF-8.
fn so_path(so: &Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// Call `name/0` through the loaded library and assert the returned tuple equals `want` (a tuple of
/// small integers — no atoms are involved, so `compare::eq` is exact with an empty table).
#[track_caller]
fn assert_tuple(env: &mut NifEnv, lib: &NifLibrary, name: &str, want: &[i64]) {
    let got = lib
        .call(env, name, 0, &[])
        .unwrap_or_else(|err| panic!("{name}/0 must be callable: {err:?}"));
    let terms: Vec<Eterm> = want.iter().map(|value| make_small(*value)).collect();
    let expected = env
        .heap()
        .tuple(&terms)
        .expect("the expected tuple fits the test heap");
    assert!(
        compare::eq(got, expected, &AtomTable::new()),
        "{name}/0: got {got:?}, want {expected:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The Rust runtime
// ---------------------------------------------------------------------------------------------

#[test]
fn the_thread_family_round_trips_through_a_real_c_nif() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    let mut env = NifEnv::new(&mut host);

    for (name, want) in COMPARABLE {
        assert_tuple(&mut env, &lib, name, want);
    }

    // The one slot that legitimately differs from the oracle: this runtime has no scheduler threads,
    // so a NIF always reports `ERL_NIF_THR_UNDEFINED` (the C's answer for a non-scheduler thread).
    let ty = lib
        .call(&mut env, "thread_type", 0, &[])
        .expect("thread_type/0 must be callable");
    assert!(
        compare::eq(ty, make_small(0), &AtomTable::new()),
        "enif_thread_type must report ERL_NIF_THR_UNDEFINED, got {ty:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The system-Erlang oracle (skipped loudly when the toolchain is absent)
// ---------------------------------------------------------------------------------------------

/// Render one expected tuple as the Erlang text `~w` prints for it.
fn erl_tuple(values: &[i64]) -> String {
    let inner: Vec<String> = values.iter().map(i64::to_string).collect();
    format!("{{{}}}", inner.join(","))
}

/// Whether `cmd` can be spawned with `args` at all (the probe for the oracle toolchain; a non-zero
/// exit is fine — e.g. `erlc -h` — only a missing binary counts as absent).
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok()
}

/// Load the fixture into the **system** Erlang and print every comparable result, one per line.
///
/// Returns `None` (after a loud skip) when `erl`/`erlc` are unavailable or the generated module could
/// not be built; a *runtime* failure of the Erlang side is a hard error, because that would mean the
/// fixture is not a valid NIF rather than that the toolchain is missing.
fn oracle_print(so: &Path) -> Option<Vec<String>> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-h"]) {
        skip("the erl/erlc oracle toolchain is not available");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.file_stem()?.to_str()?;
    let nif_path = dir.join(stem);
    let nif_path = nif_path.to_str()?;

    // The oracle module: the stub bodies are replaced by the NIFs on `load_nif`. The compiler marks
    // every function `nif_start` because `init/0` calls `load_nif/2`, which is what makes the
    // replacement possible.
    let mut erl = format!("-module({MODULE}).\n");
    let exports: Vec<String> = COMPARABLE
        .iter()
        .map(|(name, _)| format!("{name}/0"))
        .chain(std::iter::once("thread_type/0".to_string()))
        .collect();
    erl.push_str(&format!("-export([{}]).\n", exports.join(", ")));
    erl.push_str("-on_load(init/0).\n\n");
    erl.push_str(&format!(
        "init() ->\n    erlang:load_nif(\"{nif_path}\", 0).\n\n"
    ));
    for (name, _) in COMPARABLE {
        erl.push_str(&format!("{name}() -> erlang:nif_error(nif_not_loaded).\n"));
    }
    erl.push_str("thread_type() -> erlang:nif_error(nif_not_loaded).\n");

    let erl_file = dir.join(format!("{MODULE}.erl"));
    std::fs::write(&erl_file, erl).expect("the oracle module is writable");

    let compiled = Command::new("erlc")
        .arg("-o")
        .arg(&dir)
        .arg(&erl_file)
        .output()
        .expect("erlc is runnable (probed above)");
    assert!(
        compiled.status.success(),
        "erlc failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut expr = String::new();
    for (name, _) in COMPARABLE {
        expr.push_str(&format!("io:format(\"~w~n\", [{MODULE}:{name}()]),\n"));
    }
    expr.push_str("halt(0).\n");

    let out = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-eval")
        .arg(&expr)
        .output()
        .expect("erl is runnable (probed above)");
    assert!(
        out.status.success(),
        "the oracle run failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

#[test]
fn the_same_fixture_agrees_under_the_erl_oracle() {
    let Some(so) = compile_fixture() else { return };
    let Some(lines) = oracle_print(&so) else { return };

    let expected: Vec<String> = COMPARABLE
        .iter()
        .map(|(_, values)| erl_tuple(values))
        .collect();
    assert_eq!(
        lines, expected,
        "the system Erlang and this crate must agree on every comparable fixture result"
    );
}
