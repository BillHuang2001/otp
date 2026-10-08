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

//! A **broad real-module LOAD sweep** over the running system's OTP install.
//!
//! Where `tests/container.rs`, `tests/ir.rs` and `tests/widened_ir.rs` load a
//! couple of committed oracle fixtures, this file is the *wide* evidence: it walks
//! the **whole installed OTP `lib` root** — every versioned application directory
//! and every `*.beam` found under it, recursively (ebin plus doc/examples and
//! `proper_ext`) — and loads each one through the crate's public entry point
//! [`beam_loader::load_file`], i.e. the whole pipeline
//! (`container::parse` → `chunk::parse` → `decode::decode_stream` →
//! `lower::build_module`).
//!
//! # What the sweep proves
//!
//! * **Load breadth.** The public loader pipeline parses an entire real OTP
//!   install; a broken walk (a missing application, a directory-resolution bug) is
//!   caught by floors on the number of applications and modules discovered.
//! * **Every discovered module must load.** A single module failing to parse fails
//!   the test — a load failure is a regression, never tolerated. Each failure is
//!   still printed by name with its exact typed error before the test fails.
//! * **Semantic decoding.** [`nested_fun_literals_decode_to_their_mfa`] loads two
//!   real modules whose `LitT` pools nest `EXPORT_EXT` funs inside compound
//!   literals, and asserts the decoded funs still carry their `{module, function,
//!   arity}` identity — so the sweep proves *what* decoded, not merely *how many*.
//!
//! # Locating the OTP install (generic, no hardcoded application names)
//!
//! The `lib` root is resolved at test time by [`otp_lib_dir`]: `$ERTS_LIB_DIR`
//! when set (it must be the `lib` root), otherwise
//! `erl -noshell -eval 'io:format("~s",[code:lib_dir()])'` — OTP's own
//! `code:lib_dir/0`. The versioned application directories are the root's direct
//! subdirectories; the `.beam` files are collected recursively beneath each.
//!
//! # Skip-vs-assert policy (explicit)
//!
//! * **A missing OTP install skips LOUDLY.** When `erl` is absent (or the
//!   `stdlib`/`kernel` `ebin` directories cannot be located, or the `lib` root
//!   cannot be read) the test prints `SKIP: <reason>` and returns — it never fails
//!   in that case. It never silently passes without OTP.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader
//! ```
//!
//! With `-- --nocapture` the per-module failures and the `SWEEP: …` summary are
//! printed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_codec::term::{cons_head, cons_tail, flatmap_parts, tuple_arity, tuple_element};
use beam_codec::{fun_arity_term, fun_function, fun_kind_term, fun_module, FunKind};
use beam_loader::Module;
use beam_term::prelude::{is_list, Eterm};

// ---------------------------------------------------------------------------
// Locating the installed OTP `lib` root and its `ebin` directories
// ---------------------------------------------------------------------------

/// The running Erlang's `lib` root — the directory holding the versioned
/// application directories (`stdlib-2.0`, `kernel-11.0`, …) — or `None` when it
/// cannot be located.
///
/// Resolution order:
/// 1. `$ERTS_LIB_DIR` when set (it must be the `lib` root);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir()]),halt().'` evaluated at
///    test time (OTP's `code:lib_dir/0`).
///
/// A caller that gets `None` must **skip loudly** (never fail) — see
/// [`skip_reason`].
fn otp_lib_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ERTS_LIB_DIR") {
        return Some(PathBuf::from(dir));
    }

    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir()]),halt().",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// The `ebin` directory of the running Erlang's `app` application (`stdlib`,
/// `kernel`, `compiler`, …), or `None` when it cannot be located.
///
/// Resolution order:
/// 1. `$ERTS_<APP>_DIR` (uppercased app name; accepts either the application dir
///    — `…/lib/stdlib-8.1` — or its `ebin` subdir);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir(<app>)]),halt().'`
///    evaluated at test time (OTP's `code:lib_dir/1` returns the *application*
///    dir, whose `ebin` subdirectory holds the `.beam` files).
///
/// A caller that gets `None` must **skip loudly** (never fail) — see
/// [`skip_reason`].
fn app_ebin(app: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(format!("ERTS_{}_DIR", app.to_uppercase())) {
        let dir = PathBuf::from(dir);
        if has_beams(&dir) {
            return Some(dir);
        }
        let ebin = dir.join("ebin");
        return has_beams(&ebin).then_some(ebin);
    }

    let eval = format!("io:format(\"~s\",[code:lib_dir({app})]),halt().");
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &eval])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let app_dir = PathBuf::from(stdout.trim());
    if has_beams(&app_dir) {
        return Some(app_dir);
    }
    let ebin = app_dir.join("ebin");
    has_beams(&ebin).then_some(ebin)
}

/// True when `dir` is a directory that contains at least one `*.beam` file.
fn has_beams(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        entry
            .path()
            .extension()
            .map(|extension| extension == "beam")
            .unwrap_or(false)
    })
}

/// The reason the test should skip (and return early), or `None` when the oracle
/// OTP install is available. `stdlib` and `kernel` are required; a caller that
/// gets `Some` must print it and return — never fail.
fn skip_reason() -> Option<String> {
    if app_ebin("stdlib").is_none() {
        return Some(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the real-module sweep requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    if app_ebin("kernel").is_none() {
        return Some(
            "SKIP: system Erlang/kernel not found (need `erl` on PATH or $ERTS_KERNEL_DIR) — \
             the real-module sweep requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    None
}

// ---------------------------------------------------------------------------
// Discovering every `.beam` under the OTP `lib` root
// ---------------------------------------------------------------------------

/// The discovered OTP install: the versioned application directories that hold at
/// least one `.beam`, and every `.beam` file found beneath the `lib` root.
struct LibSweep {
    root: PathBuf,
    /// Number of versioned application directories that contributed at least one
    /// `.beam` file.
    app_count: usize,
    /// Every `.beam` file under the root, in sorted order.
    beams: Vec<PathBuf>,
}

/// Recursively collects every `*.beam` file beneath `dir` into `out`.
fn collect_beams(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_beams(&path, out);
        } else if path.extension().map(|ext| ext == "beam").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Discovers the installed OTP applications and every `.beam` under `root`. Each
/// direct subdirectory of `root` is a versioned application directory; one that
/// yields no `.beam` (e.g. `erl_interface`, which ships no `ebin`) is not counted.
fn lib_sweep(root: &Path) -> Option<LibSweep> {
    let mut apps: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    apps.sort();

    let mut app_count = 0usize;
    let mut beams = Vec::new();
    for app in &apps {
        let mut app_beams = Vec::new();
        collect_beams(app, &mut app_beams);
        if !app_beams.is_empty() {
            app_count += 1;
            beams.append(&mut app_beams);
        }
    }
    beams.sort();
    Some(LibSweep {
        root: root.to_path_buf(),
        app_count,
        beams,
    })
}

/// A short, stable label for reporting: `path` relative to `root` when possible.
fn rel_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// Loads every `*.beam` under the installed OTP `lib` root through
/// [`beam_loader::load_file`] (the public pipeline) and proves each one parses to a
/// usable module. Every load failure is a hard test failure: a module that stops
/// loading is a regression.
#[test]
fn loads_every_installed_otp_beam() {
    if let Some(reason) = skip_reason() {
        eprintln!("{reason}");
        return;
    }

    let Some(root) = otp_lib_dir() else {
        eprintln!(
            "SKIP: could not resolve the OTP `lib` root (need `erl` on PATH or $ERTS_LIB_DIR) — \
             the real-module sweep requires the installed OTP `.beam` files"
        );
        return;
    };
    let Some(sweep) = lib_sweep(&root) else {
        eprintln!(
            "SKIP: OTP `lib` root {} is not a readable directory — \
             the real-module sweep requires the installed OTP `.beam` files",
            root.display()
        );
        return;
    };
    assert!(
        !sweep.beams.is_empty(),
        "the sweep found no `.beam` files under {}",
        sweep.root.display()
    );

    println!(
        "SWEEP: discovered {} application(s) and {} `.beam` module(s) under {}",
        sweep.app_count,
        sweep.beams.len(),
        sweep.root.display()
    );

    let total = sweep.beams.len();
    let mut loaded = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut empty: Vec<String> = Vec::new();

    for path in &sweep.beams {
        let label = rel_label(&sweep.root, path);
        match beam_loader::load_file(path) {
            Ok(loaded_module) => {
                loaded += 1;
                // A successful load must have produced at least one function: an
                // empty function list means the pipeline parsed the chunks but
                // silently dropped the code — still a defect.
                if loaded_module.functions.is_empty() {
                    empty.push(label);
                }
            }
            Err(error) => {
                // Surface every failure loudly, with the exact typed error; the
                // assertion below then fails the test on any of them.
                let message = format!("{error:?}");
                println!("LOAD-FAIL {label}: {message}");
                failures.push((label, message));
            }
        }
    }

    let mut report = String::new();
    let _ = write!(
        report,
        "SWEEP: loaded {loaded}/{total} module(s) across {} application(s)",
        sweep.app_count
    );
    println!("{report}");
    if !failures.is_empty() {
        println!("SWEEP: {} load failure(s):", failures.len());
        for (label, error) in &failures {
            println!("  - {label}: {error}");
        }
    }

    // A broad real sweep must cover the whole install. The floors are well below
    // the real count (≈35 apps, ≈1366 modules) so an OTP point release
    // adding/removing a few does not break them, but a broken walk (or a missing
    // application) does.
    assert!(
        sweep.app_count >= 30,
        "the sweep only found {} application(s) — expected the full OTP install (≈35)",
        sweep.app_count
    );
    assert!(
        total >= 1000,
        "the sweep only found {total} `.beam` files — expected the full OTP `lib` root (≈1366)"
    );
    assert!(
        loaded == total,
        "{} of {total} installed `.beam` module(s) failed to load (a regression): {failures:?}",
        total - loaded,
    );
    assert!(
        empty.is_empty(),
        "{} module(s) loaded but yielded no function: {empty:?}",
        empty.len()
    );
}

// ---------------------------------------------------------------------------
// Semantic evidence: the nested fun literals decode to their real MFA
// ---------------------------------------------------------------------------

/// Recursively collects every fun term reachable inside `term`, descending through
/// cons spines, tuples and flat maps — the shape a `.beam` literal pool (`LitT`)
/// has. Mirrors the walker in `beam_codec/tests/ground_truth.rs`.
fn collect_funs(term: Eterm, out: &mut Vec<Eterm>) {
    if fun_kind_term(term).is_ok() {
        out.push(term);
        return;
    }
    // Walk a cons spine; the final (non-cons) tail is handled below.
    let mut cursor = term;
    while is_list(cursor) {
        collect_funs(cons_head(cursor).expect("a cons cell has a head"), out);
        cursor = cons_tail(cursor).expect("a cons cell has a tail");
    }
    if let Ok(arity) = tuple_arity(cursor) {
        for index in 0..arity {
            collect_funs(
                tuple_element(cursor, index).expect("an in-range tuple element"),
                out,
            );
        }
    } else if let Ok((keys, values)) = flatmap_parts(cursor) {
        for key in keys {
            collect_funs(*key, out);
        }
        for value in values {
            collect_funs(*value, out);
        }
    }
}

/// Every fun reachable inside a loaded module's decoded `LitT` literals.
fn literal_funs(module: &Module) -> Vec<Eterm> {
    let mut funs = Vec::new();
    for &term in module.literals().terms() {
        collect_funs(term, &mut funs);
    }
    funs
}

/// The `(module, function, arity)` names of a fun term, resolved through `module`'s
/// own atom table (its literals were decoded with that same table, so the atoms
/// resolve). `None` where the term does not carry that field.
fn fun_mfa(module: &Module, fun: Eterm) -> (Option<&str>, Option<&str>, Option<usize>) {
    (
        fun_module(fun).ok().and_then(|atom| module.atom_name(atom)),
        fun_function(fun).ok().and_then(|atom| module.atom_name(atom)),
        fun_arity_term(fun).ok(),
    )
}

/// Proves the previously-blocking fun-literal shape decodes **semantically**, not
/// just by count: two real stdlib modules whose `LitT` pools nest `EXPORT_EXT` funs
/// inside compound literals are loaded through the public pipeline, the nested funs
/// are walked out of the decoded literals, and their `{module, function, arity}`
/// identity is asserted (the oracle ground truth pinned in
/// `beam_codec/tests/ground_truth.rs`).
#[test]
fn nested_fun_literals_decode_to_their_mfa() {
    let Some(ebin) = app_ebin("stdlib") else {
        eprintln!(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the fun-literal semantic check requires the installed OTP `.beam` files"
        );
        return;
    };

    // `maps`' literal pool nests `{2, fun maps:remove/2}` — an `EXPORT_EXT` fun
    // inside a tuple inside a list.
    let maps = beam_loader::load_file(ebin.join("maps.beam"))
        .expect("maps.beam must load: its nested `fun maps:remove/2` literal must decode");
    let maps_funs = literal_funs(&maps);
    let remove = maps_funs
        .iter()
        .copied()
        .find(|&fun| fun_mfa(&maps, fun) == (Some("maps"), Some("remove"), Some(2)))
        .unwrap_or_else(|| {
            let found: Vec<_> = maps_funs.iter().map(|&fun| fun_mfa(&maps, fun)).collect();
            panic!("maps' literal pool must nest the external `fun maps:remove/2`; found {found:?}")
        });
    assert!(
        matches!(fun_kind_term(remove), Ok(FunKind::External)),
        "the nested `maps:remove` literal must decode as an external (`EXPORT_EXT`) fun"
    );

    // `gen_server`'s (larger) pool nests four `fun gen_server:format_log/N` funs
    // with arities 1 and 2, inside flat maps inside tuples/lists.
    let gen_server = beam_loader::load_file(ebin.join("gen_server.beam")).expect(
        "gen_server.beam must load: its nested `fun gen_server:format_log/N` literals must decode",
    );
    let gen_server_funs = literal_funs(&gen_server);
    assert!(
        gen_server_funs
            .iter()
            .all(|&fun| matches!(fun_kind_term(fun), Ok(FunKind::External))),
        "gen_server's pool nests only external (`EXPORT_EXT`) funs"
    );
    let mut arities: Vec<usize> = gen_server_funs
        .iter()
        .map(|&fun| fun_mfa(&gen_server, fun))
        .map(|(module, function, arity)| {
            assert_eq!(
                (module, function),
                (Some("gen_server"), Some("format_log")),
                "every nested gen_server fun is `gen_server:format_log/N`"
            );
            arity.expect("an external fun carries its arity")
        })
        .collect();
    arities.sort_unstable();
    arities.dedup();
    assert_eq!(
        arities,
        vec![1, 2],
        "gen_server's pool nests `format_log/1` and `format_log/2`"
    );
}
