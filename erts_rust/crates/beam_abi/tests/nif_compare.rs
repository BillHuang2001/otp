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

//! End-to-end proof of the **`enif_compare/2` term ordering** (contract A.3), including the two
//! divergences the rest of the test suite cannot see.
//!
//! `tests/fixtures/nif_compare.c` is a real C NIF — compiled at test time with the system `cc`
//! against the system `erl_nif.h` — whose `enif_compare` call is the C's `CMP(lhs, rhs)`, i.e.
//! `erts_cmp(lhs, rhs, exact=0, eq_only=0)`:
//!
//! * the **arithmetic** mode, so `enif_compare(1, 1.0) == 0` (the exact mode `=:=` says non-zero);
//! * two **atoms ordered by name** (`erts_cmp_atoms`), so `enif_compare('alpha', 'beta') == -1`
//!   regardless of the atom-table indices.
//!
//! Both are observable **only from C** — a Rust-level test cannot see them, because `beam_term`'s
//! `cmp` is the exact mode and a raw index is all `enif_compare` used to have — which is why the
//! evidence is a `cc`-built fixture loaded through this crate's own loader (`src/nif/loader.rs`).
//! The `enif_*` symbols it calls are UNDEFINED in the `.so` and bind at `dlopen(RTLD_NOW)` against
//! this test binary (`build.rs` adds `-Wl,--export-dynamic`).
//!
//! The third test covers the **fresh-atom** case: a NIF that calls `enif_make_atom(env, "alpha")`
//! and `enif_make_atom(env, "beta")` itself and then compares them, against a host whose
//! `intern_atom` only *predicts* an index (mirroring `beam_rt`'s `PendingInterner`) so
//! `NifHost::atom_name` cannot resolve the pair — the assertion can then pass only through the
//! env's per-call atom record (`src/nif/host.rs`).
//!
//! The fourth test covers a **nested atom pair**: `{alpha}` vs `{beta}` / `[alpha]` vs `[beta]`. It
//! is ordered by *name* through the current host's atom table (`NifHost::atom_table`, see the module
//! docs of `src/nif/enif/pred.rs`), so it asserts `{-1, -1}` — matching the oracle — with the two
//! atoms interned `beta`-first so an index fallback would still answer the opposite.
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

use beam_abi::nif::enif::enif_make_double;
use beam_abi::nif::loader::{load, NifLibrary};
use beam_abi::nif::term::eterm_from_nif;
use beam_abi::{NifEnv, NifHost};
use beam_alloc::Heap;
use beam_term::atom::Atom;
use beam_term::prelude::{
    is_small, make_atom, make_small, signed_value, tuple_element, tuple_size, Eterm,
};

use common::{compile_so, skip, TestHost};

/// The committed C fixture (see its header for what it proves).
const FIXTURE_C: &str = include_str!("fixtures/nif_compare.c");

/// The fixture's `ERL_NIF_INIT` name — the loader *and* the oracle require it to equal the calling
/// module, so it doubles as the name of the Erlang module the oracle test generates.
const MODULE: &str = "test_nif_compare";

// ---------------------------------------------------------------------------------------------
// The expected order lattice
// ---------------------------------------------------------------------------------------------

/// `order_pairs/0`'s twelve points, in fixture order, with the value **both** runtimes must answer.
#[rustfmt::skip]
const ORDER_PAIRS: &[i64] = &[
    0,  // 1 vs 1.0            — arithmetic mode: the headline fix
    0,  // 1.0 vs 1
    1,  // 1.5 vs 1
    -1, // 1 vs 2
    -1, // 'alpha' vs 'beta'  — ordered by NAME, not by atom index: the headline fix
    1,  // 'beta' vs 'alpha'
    0,  // 'alpha' vs 'alpha'  — identical words, the raw-word fast path
    -1, // 1 vs 'alpha'        — number < atom
    -1, // 'alpha' vs {1}      — atom < tuple
    -1, // {1} vs #{}          — tuple < map
    -1, // [1] vs <<"ab">>     — list < bitstring
    1,  // <<"ab">> vs 1       — bitstring > number
];

/// One `compare/2` probe: the two arguments written as **Erlang literals** (so the oracle calls the
/// very same `compare/2` with the same terms) plus the value both runtimes must answer. The Rust
/// side builds each literal through [`Args::build`].
struct Probe {
    lhs: &'static str,
    rhs: &'static str,
    want: i64,
}

#[rustfmt::skip]
const PROBES: &[Probe] = &[
    Probe { lhs: "1",     rhs: "1.0",   want: 0 },
    Probe { lhs: "1.0",   rhs: "1",     want: 0 },
    Probe { lhs: "1.5",   rhs: "1",     want: 1 },
    Probe { lhs: "1",     rhs: "2",     want: -1 },
    Probe { lhs: "alpha", rhs: "beta",  want: -1 },
    Probe { lhs: "beta",  rhs: "alpha", want: 1 },
    Probe { lhs: "alpha", rhs: "alpha", want: 0 },
    Probe { lhs: "1",     rhs: "alpha", want: -1 },
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

/// The atom terms the probe literals name, interned **before** the env borrows the host.
///
/// `enif_compare` resolves a top-level atom pair through the current NIF host's name lookup, so the
/// host must already know these names.
struct Args {
    alpha: Eterm,
    beta: Eterm,
}

impl Args {
    /// Intern the probe atoms in `host` (the host must not be borrowed by an env yet).
    ///
    /// `beta` is interned **first** on purpose: the atom indices then run *opposite* to the names, so
    /// a comparison that fell back to the index instead of the name would answer the opposite value.
    fn intern(host: &mut TestHost) -> Args {
        let beta = host.atom("beta");
        let alpha = host.atom("alpha");
        Args { alpha, beta }
    }

    /// The term for one probe literal, built exactly as the C fixture builds it (a small integer, a
    /// boxed float, or one of the interned atoms).
    fn build(&self, env: &mut NifEnv, literal: &str) -> Eterm {
        match literal {
            "alpha" => self.alpha,
            "beta" => self.beta,
            "1" => make_small(1),
            "2" => make_small(2),
            "1.0" => float(env, 1.0),
            "1.5" => float(env, 1.5),
            other => panic!("no builder for the probe literal {other:?}"),
        }
    }
}

/// A boxed float term on the env's heap (`enif_make_double`).
fn float(env: &mut NifEnv, value: f64) -> Eterm {
    // SAFETY: `env` is live for the whole test; `enif_make_double` only allocates on its heap.
    eterm_from_nif(unsafe { enif_make_double(env.as_ptr(), value) })
}

/// The `i64` of a term the fixture returned (every fixture result is an `enif_make_int` of `-1`,
/// `0` or `1`).
#[track_caller]
fn small_int(term: Eterm) -> i64 {
    assert!(is_small(term), "expected a small integer, got {term:?}");
    signed_value(term)
}

/// Call `compare/2` through the loaded library and return the `enif_compare` value.
#[track_caller]
fn compare(env: &mut NifEnv, lib: &NifLibrary, lhs: Eterm, rhs: Eterm) -> i64 {
    let got = lib
        .call(env, "compare", 2, &[lhs, rhs])
        .unwrap_or_else(|err| panic!("compare/2 must be callable: {err:?}"));
    small_int(got)
}

/// The integers of a container the fixture returned (a tuple of `enif_make_int`s).
#[track_caller]
fn tuple_ints(term: Eterm) -> Vec<i64> {
    // SAFETY: the fixture returns a tuple of small integers; `tuple_size`/`tuple_element` only read
    // the tuple block (and the element index is bounded by `tuple_size` below).
    unsafe {
        (0..tuple_size(term))
            .map(|i| small_int(tuple_element(term, i)))
            .collect()
    }
}

/// `order_pairs/0` through the loaded library.
#[track_caller]
fn order_pairs(env: &mut NifEnv, lib: &NifLibrary) -> Vec<i64> {
    let term = lib
        .call(env, "order_pairs", 0, &[])
        .unwrap_or_else(|err| panic!("order_pairs/0 must be callable: {err:?}"));
    tuple_ints(term)
}

/// `nested_atoms/0` through the loaded library: `{alpha}` vs `{beta}`, `[alpha]` vs `[beta]`.
#[track_caller]
fn nested_atoms(env: &mut NifEnv, lib: &NifLibrary) -> Vec<i64> {
    let term = lib
        .call(env, "nested_atoms", 0, &[])
        .unwrap_or_else(|err| panic!("nested_atoms/0 must be callable: {err:?}"));
    tuple_ints(term)
}

/// `fresh_atoms/0` through the loaded library: `compare(alpha, beta)`, `compare(beta, alpha)` where
/// **both** atoms are interned by the NIF itself, in this call.
#[track_caller]
fn fresh_atoms(env: &mut NifEnv, lib: &NifLibrary) -> Vec<i64> {
    let term = lib
        .call(env, "fresh_atoms", 0, &[])
        .unwrap_or_else(|err| panic!("fresh_atoms/0 must be callable: {err:?}"));
    tuple_ints(term)
}

// ---------------------------------------------------------------------------------------------
// A host whose `intern_atom` defers the commit (the `beam_rt` `PendingInterner` shape)
// ---------------------------------------------------------------------------------------------

/// A [`NifHost`] that mirrors `beam_rt`'s `PendingInterner`
/// (`beam_rt/src/context/bridges.rs`): [`NifHost::intern_atom`] **predicts** the next atom index
/// and remembers the name, but nothing is committed to a committed table — so
/// [`NifHost::atom_name`] cannot resolve any atom for the whole call.
///
/// That is precisely the situation the real runtime puts `enif_compare` in for a pair the NIF
/// interned moments earlier in the same call, and it makes the env's per-call atom record the only
/// resolver. `beta` is interned before `alpha` by the fixture, so the predicted indices run
/// opposite to the names: an index fallback would answer `+1` where the names say `-1`.
struct DeferredHost {
    heap: Heap,
    /// Names interned during the call, in intern order (the analog of `PendingInterner::names`).
    pending: Vec<String>,
}

impl DeferredHost {
    fn new() -> DeferredHost {
        DeferredHost {
            heap: Heap::new(4096).expect("a test heap is allocatable"),
            pending: Vec::new(),
        }
    }

    /// The names interned so far — **never** committed, so this is the only record of them.
    fn pending(&self) -> &[String] {
        &self.pending
    }
}

impl NifHost for DeferredHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// Predict the next index and remember the name, without committing it anywhere.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        if let Some(offset) = self.pending.iter().position(|pending| pending == name) {
            // A duplicate within this call reuses the predicted index.
            return Some(make_atom(Atom::from_raw(offset as u32)));
        }
        let index = self.pending.len();
        self.pending.push(name.to_string());
        Some(make_atom(Atom::from_raw(index as u32)))
    }

    /// The committed table is empty for the whole call (nothing is ever committed), so every
    /// lookup misses — exactly the `PendingInterner` situation.
    fn atom_name(&mut self, _atom: Eterm) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// The Rust runtime
// ---------------------------------------------------------------------------------------------

#[test]
fn the_fixed_order_points_round_trip_through_a_real_c_nif() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    // Intern the probe atoms *before* the env borrows the host, so `enif_compare` can resolve their
    // names through the host (this is what makes the atom-vs-atom fix observable).
    let args = Args::intern(&mut host);
    let lib = load(&path, MODULE, &mut host, Eterm::nil())
        .expect("the fixture must load: every enif_* it imports is defined by this crate");
    let mut env = NifEnv::new(&mut host);

    let one = args.build(&mut env, "1");
    let two = args.build(&mut env, "2");
    let one_float = args.build(&mut env, "1.0");
    let one_five = args.build(&mut env, "1.5");

    // The arithmetic mode: the C `enif_compare` is `erts_cmp(lhs, rhs, exact = 0, eq_only = 0)`.
    assert_eq!(compare(&mut env, &lib, one, one_float), 0, "1 == 1.0");
    assert_eq!(compare(&mut env, &lib, one_float, one), 0, "1.0 == 1");
    assert_eq!(compare(&mut env, &lib, one_five, one), 1, "1.5 > 1");
    // ... while an exact-mode comparison would have ordered them by class rank.

    // The atom pair is ordered by NAME (`erts_cmp_atoms`), not by the atom table's indices: the
    // host interned `beta` before `alpha` (see `Args::intern`), so an index fallback would answer
    // `1`/`-1` here — the exact opposite.
    assert_eq!(
        compare(&mut env, &lib, args.alpha, args.beta),
        -1,
        "'alpha' sorts before 'beta' by name, not by atom index"
    );
    assert_eq!(compare(&mut env, &lib, args.beta, args.alpha), 1);

    // Identical terms keep the raw-word fast path, and a name-free pair is unaffected.
    assert_eq!(compare(&mut env, &lib, args.alpha, args.alpha), 0);
    assert_eq!(compare(&mut env, &lib, one, two), -1);

    // The whole fixed lattice, built inside the NIF.
    assert_eq!(
        order_pairs(&mut env, &lib),
        ORDER_PAIRS,
        "every order-lattice point must be the C's answer"
    );
}

#[test]
fn the_same_fixture_agrees_under_the_erl_oracle() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    // This runtime's answers, rendered exactly as the oracle's `~w` renders them.
    let mut host = TestHost::new();
    let args = Args::intern(&mut host);
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let mut env = NifEnv::new(&mut host);

    let mut ours: Vec<String> = Vec::new();
    for probe in PROBES {
        let lhs = args.build(&mut env, probe.lhs);
        let rhs = args.build(&mut env, probe.rhs);
        let got = compare(&mut env, &lib, lhs, rhs);
        assert_eq!(
            got, probe.want,
            "enif_compare({}, {}) must be {}",
            probe.lhs, probe.rhs, probe.want
        );
        ours.push(got.to_string());
    }
    ours.push(erl_tuple(&order_pairs(&mut env, &lib)));

    let exprs: Vec<String> = PROBES
        .iter()
        .map(|probe| format!("compare({}, {})", probe.lhs, probe.rhs))
        .chain(std::iter::once("order_pairs()".to_string()))
        .collect();
    let Some(lines) = oracle_print(&so, &exprs) else {
        return;
    };
    assert_eq!(
        lines, ours,
        "the system Erlang and this crate must agree on every enif_compare point"
    );
}

// ---------------------------------------------------------------------------------------------
// The fresh-atom case: both operands interned *by the NIF*, in this very call
// ---------------------------------------------------------------------------------------------

#[test]
fn atoms_interned_inside_the_nif_are_ordered_by_name() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    // The deferred host cannot resolve *any* atom name (`atom_name` always misses, nothing is ever
    // committed), so the pair can be ordered by name only through the env's per-call atom record.
    let mut host = DeferredHost::new();
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let mut env = NifEnv::new(&mut host);

    let got = fresh_atoms(&mut env, &lib);
    drop(env);
    assert_eq!(
        got,
        vec![-1, 1],
        "the env's per-call atom record orders the fresh pair by NAME"
    );

    // What the fixture interned, and the indices the host predicted: `beta` → 0, `alpha` → 1, i.e.
    // the index order is the reverse of the name order — so the raw-index fallback would have
    // answered `vec![1, -1]` here.
    assert_eq!(host.pending().len(), 2, "exactly the pair was interned");
    assert_eq!(host.pending()[0], "beta");
    assert_eq!(host.pending()[1], "alpha");
    assert_eq!(
        host.atom_name(make_atom(Atom::from_raw(1))),
        None,
        "the host committed no name, so it cannot have resolved the pair"
    );

    // The oracle agrees on the very same `.so`.
    let Some(lines) = oracle_print(&so, &["fresh_atoms()".to_string()]) else {
        return;
    };
    assert_eq!(
        lines,
        vec![erl_tuple(&[-1, 1])],
        "the oracle orders both fresh atoms by NAME"
    );
}

// ---------------------------------------------------------------------------------------------
// An atom pair nested inside a compound (ordered by name via `NifHost::atom_table`)
// ---------------------------------------------------------------------------------------------

#[test]
fn a_nested_atom_pair_orders_by_name() {
    let Some(so) = compile_fixture() else { return };
    let Some(path) = so_path(&so) else { return };

    let mut host = TestHost::new();
    // Intern `beta` FIRST: the two atoms then have an index order opposite to their name order, so a
    // comparison that ignored the exposed table would still answer `{+1, +1}` here. `atom_table` now
    // hands this same table to the structural comparison, so the nested pair follows the names.
    let _beta = host.atom("beta");
    let _alpha = host.atom("alpha");
    let lib = load(&path, MODULE, &mut host, Eterm::nil()).expect("the fixture must load");
    let mut env = NifEnv::new(&mut host);

    let got = nested_atoms(&mut env, &lib);
    assert_eq!(
        got,
        vec![-1, -1],
        "a nested atom pair is ordered by NAME through the host's exposed atom table"
    );

    let Some(lines) = oracle_print(&so, &["nested_atoms()".to_string()]) else {
        return;
    };
    assert_eq!(
        lines,
        vec![erl_tuple(&[-1, -1])],
        "the oracle orders the nested atoms by NAME: enif_compare({{alpha}}, {{beta}}) == -1"
    );
    assert_eq!(
        lines[0],
        erl_tuple(&got),
        "this crate and the oracle must agree on the nested-atom pair"
    );
}

// ---------------------------------------------------------------------------------------------
// The system-Erlang oracle (skipped loudly when the toolchain is absent)
// ---------------------------------------------------------------------------------------------

/// Render a slice of integers as the Erlang text `~w` prints for a tuple of them.
fn erl_tuple(values: &[i64]) -> String {
    let inner: Vec<String> = values.iter().map(i64::to_string).collect();
    format!("{{{}}}", inner.join(","))
}

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
    let mut erl = format!("-module({MODULE}).\n");
    erl.push_str("-export([compare/2, order_pairs/0, nested_atoms/0, fresh_atoms/0]).\n");
    erl.push_str("-on_load(init/0).\n\n");
    erl.push_str(&format!(
        "init() ->\n    erlang:load_nif(\"{nif_path}\", 0).\n\n"
    ));
    erl.push_str("compare(_, _) -> erlang:nif_error(nif_not_loaded).\n");
    erl.push_str("order_pairs() -> erlang:nif_error(nif_not_loaded).\n");
    erl.push_str("nested_atoms() -> erlang:nif_error(nif_not_loaded).\n");
    erl.push_str("fresh_atoms() -> erlang:nif_error(nif_not_loaded).\n");

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
