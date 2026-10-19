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

//! End-to-end proof of the **`enif_make_map_from_arrays` duplicate-key refusal** (contract A.3).
//!
//! `tests/fixtures/nif_map_arrays.c` is a real C NIF — compiled at test time with the system `cc`
//! against the system `erl_nif.h` — whose six entry points each run one
//! `enif_make_map_from_arrays` from a sentinel-initialised out slot and answer
//! `{ReturnCode, Detail}`:
//!
//! * on acceptance `Detail` is `maps:size/1` of the out term;
//! * on refusal `Detail` is the atom `overwritten` when the out slot no longer holds the sentinel
//!   (the C *does* store `THE_NON_VALUE` there, `erts/emulator/beam/erl_nif.c:3688-3692`).
//!
//! The C ground truth is `erts_map_from_ks_and_vs` (`erts/emulator/beam/erl_map.c:671-714`),
//! documented `/* Returns THE_NON_VALUE if duplicate keys found. */`: the flat arm refuses as soon
//! as `erts_cmp_flatmap_keys(..) == 0` in `erts_validate_and_sort_flatmap`, the hash arm passes
//! `reject_dupkeys = 1`. The basis is the **exact** (`=:=`) key identity on both sides of the
//! flat/hash threshold — so `1` and `1.0`, which are different map keys, are accepted.
//!
//! Both directions are pinned, because only a refusal for duplicates *and* acceptance for
//! legitimate maps is the C's behaviour:
//!
//! | NIF | input | oracle answer |
//! |---|---|---|
//! | `unique/0` | 3 distinct keys (flat arm) | `{1,3}` |
//! | `unique_hash/0` | 40 distinct keys (hash arm) | `{1,40}` |
//! | `int_float/0` | keys `1` and `1.0` | `{1,2}` |
//! | `dup_flat/0` | `7` twice (flat arm) | `{0,overwritten}` |
//! | `dup_hash/0` | 34 pairs, `5` twice (hash arm) | `{0,overwritten}` |
//! | `dup_tuple/0` | two `=:=`-equal tuples at distinct addresses | `{0,overwritten}` |
//!
//! The `enif_*` symbols the fixture calls are UNDEFINED in the `.so` and bind at
//! `dlopen(RTLD_NOW)` against this test binary (`build.rs` adds `-Wl,--export-dynamic`).
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system `erl_nif.h`; `compile_so` prints a loud
//! `SKIP:` banner and returns `None` when either is missing (the calling test then returns). The
//! same holds for the oracle half: it skips loudly when `erl`/`erlc` are absent. Whenever the
//! toolchain **is** present every assertion is hard.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::loader::{load, NifLibrary};
use beam_abi::NifEnv;
use beam_term::atom::AtomTable;
use beam_term::prelude::{atom_val, is_atom, is_small, signed_value, tuple_element, tuple_size, Eterm};

use common::{compile_so, skip, TestHost};

/// The committed C fixture (see its header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_map_arrays.c");

/// The fixture's `ERL_NIF_INIT` name — the loader *and* the oracle require it to equal the calling
/// module, so it doubles as the name of the Erlang module the oracle test generates.
const MODULE: &str = "test_nif_map_arrays";

// ---------------------------------------------------------------------------------------------
// The cases and their oracle answers
// ---------------------------------------------------------------------------------------------

/// What one call must answer.
#[derive(Clone, Copy)]
enum Want {
    /// Accepted: the return code is `1` and the out term is a map of this size.
    Accepted(usize),
    /// Refused: the return code is `0` and the out slot was overwritten with `THE_NON_VALUE`.
    Refused,
}

#[rustfmt::skip]
const CASES: &[(&str, Want)] = &[
    ("unique",      Want::Accepted(3)),
    ("unique_hash", Want::Accepted(40)),
    ("int_float",   Want::Accepted(2)),
    ("dup_flat",    Want::Refused),
    ("dup_hash",    Want::Refused),
    ("dup_tuple",   Want::Refused),
];

/// The Erlang text `~w` prints for the answer of `want` — the expected constant on both sides.
fn want_text(want: Want) -> String {
    match want {
        Want::Accepted(size) => format!("{{1,{size}}}"),
        Want::Refused => "{0,overwritten}".to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The fixture compiled into a shared object, or `None` (after a loud skip) when the toolchain is
/// absent.
fn compile_fixture() -> Option<PathBuf> {
    compile_so(MODULE, FIXTURE_C)
}

/// The path `load` takes — the compiled `.so` **without** its suffix, which the loader appends
/// itself — or `None` (after a loud skip) when it is not valid UTF-8.
fn load_path(so: &Path) -> Option<String> {
    match so.with_extension("").to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// The elements of a tuple term the fixture returned.
#[track_caller]
fn elements(term: Eterm) -> Vec<Eterm> {
    // SAFETY: the fixture returns a 2-tuple; `tuple_size`/`tuple_element` only read that block and
    // the index is bounded by `tuple_size`.
    unsafe { (0..tuple_size(term)).map(|i| tuple_element(term, i)).collect() }
}

/// Render the terms of one `{ReturnCode, Detail}` answer the way `~w` does: small integers as
/// numbers, atoms by name.
#[track_caller]
fn render(term: Eterm, atoms: &AtomTable) -> String {
    let parts: Vec<String> = elements(term)
        .into_iter()
        .map(|element| {
            if is_small(element) {
                return signed_value(element).to_string();
            }
            if is_atom(element) {
                let name = atoms
                    .name(atom_val(element))
                    .expect("the fixture's atoms are interned in the test host")
                    .to_string();
                return name;
            }
            panic!("unexpected element in the fixture's answer: {element:?}");
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// Run every case through the loaded library and render the answers.
#[track_caller]
fn run_all(lib: &NifLibrary, host: &mut TestHost) -> Vec<String> {
    let mut raws = Vec::with_capacity(CASES.len());
    {
        let mut env = NifEnv::new(host);
        for (name, _) in CASES {
            let term = lib
                .call(&mut env, name, 0, &[])
                .unwrap_or_else(|err| panic!("{name}/0 must be callable: {err:?}"));
            raws.push(term);
        }
    }
    // The env's borrow ends here, so the host's atom table is readable again.
    raws.into_iter()
        .map(|term| render(term, host.atoms()))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The Rust runtime
// ---------------------------------------------------------------------------------------------

#[test]
fn a_duplicate_key_is_refused_and_a_unique_key_set_is_accepted() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = TestHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    let ours = run_all(&lib, &mut host);

    let want: Vec<String> = CASES.iter().map(|(_, want)| want_text(*want)).collect();
    for ((name, _), (got, want)) in CASES.iter().zip(ours.iter().zip(want.iter())) {
        assert_eq!(
            got, want,
            "enif_make_map_from_arrays in {name}/0 must answer {want}"
        );
    }
}

#[test]
fn the_same_fixture_agrees_under_the_erl_oracle() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = load_path(&so) else { return };

    let mut host = TestHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let ours = run_all(&lib, &mut host);

    let exprs: Vec<String> = CASES
        .iter()
        .map(|(name, _)| format!("{name}()"))
        .collect();
    let Some(lines) = oracle_print(&so, &exprs) else {
        return;
    };
    assert_eq!(
        lines, ours,
        "the system Erlang and this crate must agree on every enif_make_map_from_arrays answer"
    );
    // ... and both equal the measured oracle constants, so an accidentally identical divergence
    // still fails.
    let want: Vec<String> = CASES.iter().map(|(_, want)| want_text(*want)).collect();
    assert_eq!(lines, want, "the measured oracle answers");
}

// ---------------------------------------------------------------------------------------------
// The system-Erlang oracle (skipped loudly when the toolchain is absent)
// ---------------------------------------------------------------------------------------------

/// Whether `cmd` can be spawned with `args` at all (the probe for the oracle toolchain; a non-zero
/// exit is fine — e.g. `erlc -h` — only a missing binary counts as absent).
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok()
}

/// Load the fixture into the **system** Erlang and print `exprs` (each an Erlang expression calling
/// the fixture), one `~w` line per expression.
///
/// Returns `None` (after a loud skip) when `erl`/`erlc` are unavailable or the generated module
/// could not be built; a *runtime* failure of the Erlang side is a hard error, because that would
/// mean the fixture is not a valid NIF rather than that the toolchain is missing.
fn oracle_print(so: &Path, exprs: &[String]) -> Option<Vec<String>> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-h"]) {
        skip("the erl/erlc oracle toolchain is not available");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.file_stem()?.to_str()?;
    let nif_path = dir.join(stem);
    let nif_path = nif_path.to_str()?;

    // The oracle module: the stub bodies are replaced by the NIFs on `load_nif`, and the compiler
    // marks every function `nif_start` because `init/0` calls `load_nif/2` — which is what makes the
    // replacement possible. Every `ErlNifFunc` the fixture declares must be exported here (the load
    // refuses a declared NIF the module does not have).
    let exports: Vec<String> = CASES
        .iter()
        .map(|(name, _)| format!("{name}/0"))
        .collect();
    let mut erl = format!("-module({MODULE}).\n");
    erl.push_str(&format!("-export([{}]).\n", exports.join(", ")));
    erl.push_str("-on_load(init/0).\n\n");
    erl.push_str(&format!(
        "init() ->\n    erlang:load_nif(\"{nif_path}\", 0).\n\n"
    ));
    for (name, _) in CASES {
        erl.push_str(&format!("{name}() -> erlang:nif_error(nif_not_loaded).\n"));
    }

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
    for e in exprs {
        expr.push_str(&format!("io:format(\"~w~n\", [{MODULE}:{e}]),\n"));
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
