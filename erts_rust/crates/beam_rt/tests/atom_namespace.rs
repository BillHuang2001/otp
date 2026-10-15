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

//! The **atom-namespace** host seam: guard/`select_val`/map-key comparisons must order atoms by
//! **name**, exactly as the `<`/`==` BIFs do.
//!
//! `beam_term`'s `cmp_atoms` compares atom *names* but falls back to the raw atom **id** when a
//! name is unknown to the supplied table. A module loaded early cannot name an atom interned into
//! the runtime's shared table *afterwards* (its global id lies beyond the module's load-time
//! snapshot), so a comparison resolving names through `&module.atom_table` — the
//! [`ExecutionContext::atoms`](beam_engine::ExecutionContext::atoms) trait default — silently
//! ordered such a pair by id and took the wrong clause.
//!
//! `beam_rt` owns the runtime-wide shared table and overrides that seam with `Runtime::atoms()`,
//! on **both** of its `ExecutionContext` impls:
//!
//! * [`RuntimeContext`](beam_rt::RuntimeContext) — the single-pass `run_function`/`-eval` route,
//!   pinned by [`the_sofs_repro_orders_atoms_by_name`] (the reported real-stdlib shape) and by the
//!   `run_function` half of [`both_execution_impls_order_atoms_by_name`];
//! * `BudgetContext` — the **scheduled** slice's context (`src/runner/budget.rs`), which forwards
//!   the seam by hand, pinned by the `run_scheduled` half of the same test.
//!
//! Dropping either override leaves the module's id-ordered snapshot in place, so the compiled
//! guard below answers `less` where the oracle answers `greater`.
//!
//! A missing toolchain is a **loud skip**, never a silent pass (the shared [`support`] module
//! resolves `erl`/`erlc` from the filesystem and bounds every probe).

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_rt::Runtime;

/// The fixture's module name (also its source-file stem and its compiled `.beam` name).
const MODULE: &str = "atomns";

/// The banner for the loud skip.
const BANNER: &str = "atom namespace";

/// The fixture: one exported function whose comparison sits in a **guard**, so the compiler emits
/// the engine's `is_lt` test instruction — the site that resolves atom names through
/// [`ExecutionContext::atoms`](beam_engine::ExecutionContext::atoms) — rather than the
/// `erlang:'<'/2` *BIF* (which already compares through the runtime's shared table, and which the
/// compiler picks for a `<` in value position).
///
/// `module_info` is an atom this module's own chunk carries (the compiler emits `module_info/0,1`
/// for every module). `g` is created at run time from a `binary_to_term/1` `ATOM_EXT` image: it is
/// neither an atom literal (so it stays out of the module's atom chunk) nor a foldable constant
/// (so the comparison is not constant-folded away — `list_to_atom("g")` **is**, and would turn the
/// whole body into `move false`), so its id lies beyond this module's load-time snapshot and only
/// the runtime's live shared table can name it. By name `g < module_info`, so the guard does not
/// hold and the oracle answers `greater`; the id-order fallback answers `less`.
const FIXTURE_ERL: &str = "-module(atomns).\n\
     -export([guard/0]).\n\
     guard() ->\n\
         A = {module_info, 0},\n\
         B = {binary_to_term(<<131,100,0,1,103>>), 1},\n\
         if\n\
             A < B -> less;\n\
             true -> greater\n\
         end.\n";
/// The oracle probe for the fixture: `~w` — the printer `beam_rt run` uses — so the two
/// renderings are directly comparable.
const ORACLE_FIXTURE_EVAL: &str = "io:format(\"~w~n\",[atomns:guard()]), halt(0).";

/// The reported real-stdlib repro, driven through the CLI `-eval` action exactly as the
/// differential corpus's `selfhost/*` cases drive the same code path.
///
/// `sofs` (and `erl_lint`) are primLoaded by the boot-script drive **before** the expression's
/// atoms are interned, so `sofs`'s load-time snapshot cannot name `g`; `sofs:restrict_n/5`'s
/// `{module_info,0} < {g,1}` guard then decides whether `drestriction/2` returns the difference
/// (oracle: `[]`) or the restriction (the id-order fallback: `[{{module_info,0},0}]`). The same
/// expression is handed to the oracle through `erl -noshell -eval … -s init stop`, so the two
/// runtimes are compared directly.
const SOFS_REPRO: &str = "R = sofs:relation([{{module_info,0},0}],[{func,location}]), \
     S = sofs:from_external([{g,1},{module_info,0}],[func]), \
     io:format(\"~p~n\",[sofs:to_external(sofs:drestriction(R,S))]), \
     io:format(\"~p~n\",[sofs:to_external(sofs:restriction(R,S))]), halt(0).";
/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

// ---------------------------------------------------------------------------------------
// Toolchain discovery + the loud skip
// ---------------------------------------------------------------------------------------

/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

/// Prints the loud skip banner — a missing compiler is never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt {BANNER} SKIPPED ===============");
    eprintln!("{why}: the atom-namespace comparison cannot run.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("========================================================");
    eprintln!();
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory: a safe single `-pa` for the oracle (no `lists.beam` in it).
    dir: PathBuf,
    /// The compiled `atomns.beam`.
    beam: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The directory is ours and disposable; a cleanup failure is reported, not hidden.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Compiles [`FIXTURE_ERL`] with the system `erlc`, or `None` with a loud skip when the
/// toolchain cannot do it.
fn build_fixture(erlc: &Path) -> Option<Fixture> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_atomns_{}_{n}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        skip_loudly(&format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let source = dir.join(format!("{MODULE}.erl"));
    if let Err(error) = std::fs::write(&source, FIXTURE_ERL) {
        skip_loudly(&format!("cannot write {}: {error}", source.display()));
        return None;
    }
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            skip_loudly(&format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        skip_loudly(&format!(
            "`erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    let beam = dir.join(format!("{MODULE}.beam"));
    if !beam.is_file() {
        skip_loudly(&format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixture { dir, beam })
}

// ---------------------------------------------------------------------------------------
// The probes
// ---------------------------------------------------------------------------------------

/// `beam_rt -noshell -eval <expr>` through the built binary, as the differential harness drives it.
fn beam_rt_eval(expr: &str) -> String {
    let output = support::run_bounded(Path::new(BINARY), &["-noshell", "-eval", expr])
        .unwrap_or_else(|| panic!("`{BINARY} -eval` could not be run to completion"));
    assert!(
        output.status.success(),
        "`{BINARY} -noshell -eval` must exit 0, got {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// The reported real-stdlib repro: `sofs:drestriction/2` returns the **difference** and
/// `sofs:restriction/2` the **restriction**, byte-identical to the live oracle.
///
/// Running through the CLI's `-eval` action means the boot-script drive primLoads `sofs` first,
/// which is exactly the shape in which the defect surfaced (and the shape the corpus's
/// `selfhost/*` cases drive). The expected value is re-probed from the live oracle, so a stale
/// recording cannot pass.
#[test]
fn the_sofs_repro_orders_atoms_by_name() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(expected) = support::oracle_eval(&oracle, None, SOFS_REPRO) else {
        skip_loudly("the oracle probe could not answer the sofs repro");
        return;
    };
    assert_eq!(
        expected.lines().collect::<Vec<_>>(),
        vec!["[]", "[{{module_info,0},0}]"],
        "the oracle's own answer for the repro must be the difference then the restriction"
    );

    let ours = beam_rt_eval(SOFS_REPRO);
    eprintln!("atom_namespace: beam_rt {ours:?}; oracle {expected:?}");
    assert_eq!(
        ours, expected,
        "the runtime must answer the sofs repro exactly as the oracle does"
    );
}

/// One compiled guard, executed through **both** `ExecutionContext` impls `beam_rt` owns: the
/// single-pass [`Runtime::run_function`] ([`RuntimeContext`](beam_rt::RuntimeContext)) and the
/// scheduled [`Runtime::run_scheduled`] (`BudgetContext`, the slice wrapper that must forward the
/// seam by hand). Each answer is compared to the live oracle's.
#[test]
fn both_execution_impls_order_atoms_by_name() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        skip_loudly("no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixture) = build_fixture(&erlc) else {
        return;
    };
    let Some(expected) = support::oracle_eval(&oracle, Some(&fixture.dir), ORACLE_FIXTURE_EVAL)
    else {
        skip_loudly("the oracle probe could not answer for the fixture");
        return;
    };
    assert_eq!(
        expected, "greater",
        "by name `g < module_info`, so the guard `{{module_info,0}} < {{g,1}}` does not hold"
    );
    let beam = fixture
        .beam
        .to_str()
        .expect("the fixture path is UTF-8")
        .to_string();

    // The single-pass route: `run_function` runs the export on a `RuntimeContext`.
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let value = rt
        .run_function(&beam, "guard", 0, &[])
        .expect("atomns:guard/0 runs on the runtime");
    let single_pass = rt
        .format_term_w(value, None)
        .expect("the result is printable");
    eprintln!("atom_namespace: run_function rendered {single_pass:?}; oracle {expected:?}");
    assert_eq!(
        single_pass.trim(),
        expected,
        "the single-pass route must order atoms by name"
    );

    // The scheduled route: `run_scheduled` spawns a process whose slice machine runs on the
    // `BudgetContext` wrapper, so this half fails if the wrapper does not forward `atoms`.
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let value = rt
        .run_scheduled(&beam, "guard", 0, &[])
        .expect("atomns:guard/0 runs on the scheduled route");
    let scheduled = rt
        .format_term_w(value, None)
        .expect("the result is printable");
    eprintln!("atom_namespace: run_scheduled rendered {scheduled:?}; oracle {expected:?}");
    assert_eq!(
        scheduled.trim(),
        expected,
        "the scheduled route must order atoms by name"
    );
}
