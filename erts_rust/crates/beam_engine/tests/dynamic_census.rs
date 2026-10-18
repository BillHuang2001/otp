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

//! **The corpus-scale DYNAMIC per-variant execution ranking** of the engine, measured
//! with the shipped [`beam_engine::exec_census`] instrument.
//!
//! `tests/census.rs` answers the *static* question — "which `Instr` variants does the
//! whole installed OTP contain, and does the engine claim each of them?". This suite
//! answers the *dynamic* one: **how often does each variant actually execute** when real
//! compiler output runs, and which corpus-reachable variants never execute at all?
//!
//! # The three layers
//!
//! Every layer runs on its own [`ModuleHost`] (one shared term heap + one shared atom
//! table per layer), because layer (c) registers fixtures that **shadow** stdlib module
//! names (`fixtures/lists.beam` …) and must never share a host with layers (a)/(b).
//!
//! **(a) — the corpus sweep.** `support::all_otp_beams()` discovers the whole installed
//! OTP generically (≈1366 `.beam` over ≈34 applications). Every module is loaded into the
//! layer-(a) host, its instruction stream is tallied **statically** (via
//! [`exec_census::variant_name`], giving the corpus-reachable variant set *computed*,
//! never hardcoded), and then `module_info/0` plus `module_info/1` for the keys
//! `module`, `exports`, `attributes`, `compile`, `md5` are executed on it through
//! [`Machine::run_with_tier`]`(…, `[`Tier::Interpreter`]`)`. Served and unserved runs are
//! both counted and the unserved ones are printed with their exact typed error — a module
//! the host cannot serve is **reported, never silently skipped**.
//!
//! **(b) — program workloads.** 41 named workloads over the real installed stdlib, each
//! `module:function(args)` shape copied **verbatim** from an existing oracle-proven suite:
//! `lists`/`ordsets`/`proplists` (`tests/stdlib_lists.rs`), `string`/`base64`/`io_lib`
//! (`tests/stdlib_text.rs`), `lists:foldr`/`search` + `ordsets:filter` closures
//! (`tests/stdlib_text_more.rs`), `calendar`/`filename`/`array`/`orddict`
//! (`tests/stdlib_collections.rs`), `gb_trees`/`gb_sets`
//! (`tests/stdlib_collections_trees.rs`), `queue` (`tests/stdlib_collections_queue_maps.rs`),
//! the closure-taking `orddict:map`/`lists:zipwith` (`tests/stdlib_more.rs`) and
//! `ordsets:map` (`tests/stdlib_exports_more.rs`) — the last five drive the
//! `fixtures/*_harness.beam` wrappers, so they exercise `make_fun3` + `call_fun*` +
//! cross-module `apply_fun`. The exact catalog (and each shape's provenance comment) is
//! [`catalog::WORKLOADS`].
//!
//! **(c) — the committed fixtures.** 14 `tests/fixtures/*.beam` (plus the sibling
//! `beam_loader` crate's `slice.beam`), driven at the entry points read off each
//! fixture's own `.erl` source: `slice`, `tuples`, `records`, `floats`, `bitstrings`,
//! `bscreate`, `bsmatch`, `bsstring`, `bignum`, `exceptions`, `catchclasses`, `mapfx`,
//! `tailcall`, `funs`. Catalog: [`catalog::FIXTURES`].
//!
//! # Honest caveats (read these before quoting any number)
//!
//! * This is a **named workload set over real compiler output**, not whole-corpus
//!   throughput. Layer (a) executes only the metadata forwarders, so the bulk of the
//!   dynamic weight comes from layers (b)+(c) — a different workload set would produce a
//!   different ranking, and `not reached by this workload set` is exactly that: *not
//!   reached here*, not "unsupported by the engine".
//! * **Two blind spots of the instrument.** (i) A natively-inlined baseline-JIT
//!   instruction never passes the interpreter funnel and is therefore unobserved, which
//!   is why every run here pins [`Tier::Interpreter`]. (ii) A raise caught inside a live
//!   in-machine `try`/`catch` counts as **executed**, not as an error — only an `Err`
//!   escaping the funnel is an "error".
//! * **The `ModuleHost` BIF floor is the ceiling.** `tests/support/bifs.rs` serves a
//!   small `erlang:*`/`lists:*` set; the `maps:*`, `ets:*` and `re:*` families are not
//!   served, so they contribute **zero** dynamic weight here. That is measured as a
//!   `BLOCKED` table with the census **off** (so it cannot pollute the ranking), never
//!   silently omitted.
//! * `erlang:get_module_info/1,2` is an **echo placeholder** in the local `CensusHost`
//!   wrapper below (the same stand-in `tests/coverage.rs` uses): the census needs the
//!   forwarder's instruction stream to run, and nothing here asserts a `module_info`
//!   *value*.
//!
//! # What is asserted (liveness and honesty only — never a brittle pinned count)
//!
//! Load breadth, a large floor on the combined executed count, the computed static
//! top-10 all executing, a hand-written core-variant slice executing, the
//! `not reached` list being exactly the corpus-reachable variants with zero dynamic
//! count, and that every dynamically observed variant name also occurs statically in the
//! loaded corpus (an instrument-coherence check). No exact count is pinned anywhere.

// The workload catalog lives in its own file to keep this driver under the ~1000-line
// target. `#[path]` is required: this file IS the test-crate root, and rustc resolves a
// crate root's submodules in the root's OWN directory (`tests/`), not in a
// `tests/<stem>/` directory — so a bare `mod catalog;` would look for `tests/catalog.rs`
// (which Cargo would then also build as its own test target).
#[path = "dynamic_census/catalog.rs"]
mod catalog;
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use beam_common::{BeamError, Result};
use beam_engine::exec_census::{self, Snapshot};
use beam_engine::{ExecutionContext, Machine, Tier};
use beam_loader::Label;
use beam_term::Eterm;

use support::ModuleHost;

/// The applications layered on top of `stdlib` on every host this suite builds (the
/// sibling stdlib suites' recipe), so a cross-application call resolves.
const EXTRA_APPS: [&str; 11] = [
    "kernel",
    "compiler",
    "syntax_tools",
    "parsetools",
    "xmerl",
    "inets",
    "eunit",
    "dialyzer",
    "asn1",
    "snmp",
    "edoc",
];

/// The `module_info/1` keys every layer-(a) module is asked for (the oracle's set).
const MODULE_INFO_KEYS: [&str; 5] = ["module", "exports", "attributes", "compile", "md5"];

/// The harness fixtures layer (b)'s closure workloads call into.
const HARNESS_FIXTURES: [&str; 4] = [
    "stdlists_harness",
    "stdlib_text_harness",
    "stdlib_more_harness",
    "stdlib_exports_harness",
];

/// Rows printed per layer table (the combined table is printed in full).
const TOP_ROWS: usize = 20;

/// The load-breadth floor: the whole installed OTP is ≈1366 modules, so a floor of 1000
/// fails only on a broken walk (a missing application) — never on a point release.
const LOADED_FLOOR: usize = 1000;

/// The floor on the combined executed-instruction count: **roughly half** the figure this
/// suite measured on the reference host (36 836 instructions over the three layers; see the
/// `TOTAL EXECUTED` line the test prints), so a workload that silently stops running is
/// caught while normal drift is not.
const TOTAL_EXECUTED_FLOOR: u64 = 18_000;

/// The static top-10 members allowed to carry **zero** dynamic weight, with the
/// measurement that justifies each one. Empty in spirit: `FuncInfo` is the single
/// exception, and it is not a gap in the workload set —
///
/// `func_info` is the *function-entry clause check*. The loader keeps both of a
/// function's labels (`Function.func_label`, the label immediately **before**
/// `func_info`, and `Function.entry_label`, the label immediately **after** it), but
/// `Export.label == Function.entry_label` (`beam_loader/src/ir.rs`) — and every engine
/// entry point resolves through the export table: `run_export` (this suite's layer(a)/(b)
/// /(c) driver), `resolve_external_call` (the `call_ext*` route) and `apply`. Local
/// `call`/`call_only`/`call_last` targets are likewise the compiler's entry labels. So no
/// real program of any shape can enter a `func_info`: it is unreachable **by
/// construction**, not unexercised. (This is the `FuncInfo` whose 103 749 static
/// occurrences the crate docs already call out as "never executed".)
///
/// The claim is kept honest by an assertion rather than by this comment: layer (a) counts
/// the corpus's `ExpT` entries and how many of them point at a `func_label`, and the test
/// requires that count to be **zero** — the day the loader resolves an export onto a
/// `func_info`, that assertion fires and this exclusion must go.
const SILENT_TOP_ALLOWED: [&str; 1] = ["FuncInfo"];

/// The `beam_loader::Instr` roster pinned by `tests/census.rs` (108 variants). Quoted for
/// readability of the "N distinct names out of the 108-variant roster" line only — it is
/// NOT re-derived here, and a `Test(...)` variant contributes one name per `TestKind`.
const INSTR_ROSTER: usize = 108;

/// The name [`exec_census::variant_name`] gives each of the roster's **107 non-`Test`**
/// variants, transcribed from `beam_loader/src/instr.rs`.
///
/// This exists so the "**zero-static-occurrence** variant set" can be *computed* (roster
/// minus the statically observed names) instead of quoted: it is printed, never asserted,
/// and the number it must reproduce — 13 names — is the forward-compat pin
/// `tests/census.rs` derives from the same corpus. A `Test` variant is absent because
/// [`exec_census::variant_name`] keys it per `TestKind` (see [`TEST_KIND_NAMES`]).
const INSTR_NAMES: [&str; 107] = [
    "Label", "Line", "FuncInfo", "IntCodeEnd", "Move", "Swap", "Call", "CallLast",
    "CallOnly", "CallExt", "CallExtOnly", "CallExtLast", "Bif0", "Bif", "GcBif", "CallFun",
    "CallFun2", "Apply", "ApplyLast", "MakeFun3", "SelectVal", "SelectTupleArity", "Jump",
    "Allocate", "AllocateHeap", "TestHeap", "InitYregs", "Deallocate", "Trim", "Return",
    "DeallocateReturn", "PutTuple2", "GetTupleElement", "SetTupleElement", "PutList",
    "GetList", "GetHd", "GetTl", "GetMapElements", "HasMapFields", "PutMapAssoc",
    "PutMapExact", "UpdateRecord", "IsAnyNativeRecord", "IsNativeRecord",
    "GetRecordElements", "PutRecord", "IsRecordAccessible", "GetRecordField",
    "GetRecordElementsId", "UpdateRecordId", "Badmatch", "CaseEnd", "IfEnd", "Try", "Catch",
    "TryCase", "TryEnd", "TryCaseEnd", "CatchEnd", "Raise", "Badrecord", "BuildStacktrace",
    "RawRaise", "Fmove", "Fconv", "Fadd", "Fsub", "Fmul", "Fdiv", "Fnegate", "Send",
    "LoopRec", "LoopRecEnd", "Wait", "WaitTimeout", "Timeout", "RemoveMessage",
    "RecvMarkerReserve", "RecvMarkerBind", "RecvMarkerClear", "RecvMarkerUse",
    "BsStartMatch3", "BsStartMatch4", "BsMatch", "BsMatchString", "BsGetTail",
    "BsGetPosition", "BsSetPosition", "BsGetInteger2", "BsGetFloat2", "BsGetBinary2",
    "BsSkipBits2", "BsTestTail2", "BsGetUtf8", "BsGetUtf16", "BsGetUtf32", "BsSkipUtf8",
    "BsSkipUtf16", "BsSkipUtf32", "BsInitWritable", "BsCreateBin", "ExecutableLine",
    "DebugLine", "NifStart", "OnLoad", "Generic",
];

/// The 25 `beam_loader::TestKind` variants: each contributes the roster name
/// `Test(<Kind>)`.
const TEST_KIND_NAMES: [&str; 25] = [
    "IsLt", "IsGe", "IsEq", "IsNe", "IsEqExact", "IsNeExact", "IsInteger", "IsFloat",
    "IsNumber", "IsAtom", "IsPid", "IsReference", "IsPort", "IsNil", "IsBinary", "IsBitstr",
    "IsList", "IsNonemptyList", "IsTuple", "IsMap", "IsBoolean", "IsFunction", "IsFunction2",
    "IsTaggedTuple", "TestArity",
];

/// A hand-written slice of core variants that **must** carry dynamic weight: the block /
/// call / guard / term-building backbone every real program executes. (Each is asserted
/// non-zero; the computed static top-10 is asserted separately, so a new corpus cannot
/// silently drop these out of the ranking.)
const MUST_EXECUTE: [&str; 15] = [
    "Move",
    "Label",
    "Return",
    "CallOnly",
    "CallExtOnly",
    "CallExtLast",
    "GcBif",
    "Test(IsEqExact)",
    "SelectVal",
    "Allocate",
    "TestHeap",
    "PutList",
    "PutTuple2",
    "CallFun2",
    "MakeFun3",
];

// ---------------------------------------------------------------------------
// The instrument plumbing
// ---------------------------------------------------------------------------

/// The per-layer attribution of one boundary pair: the **differences** between two
/// consecutive [`Snapshot`]s.
///
/// The live census is never reset between layers, so the snapshot taken at the end is the
/// COMBINED one (which is what [`exec_census::write_dump`] must write) while each layer's
/// own weight is exactly the delta.
fn diff(before: &Snapshot, after: &Snapshot) -> Snapshot {
    let mut out = Snapshot::default();
    for (name, &value) in &after.executed {
        let prior = before.executed.get(name).copied().unwrap_or(0);
        if value > prior {
            out.executed.insert(name.clone(), value - prior);
        }
    }
    for (name, &value) in &after.errors {
        let prior = before.errors.get(name).copied().unwrap_or(0);
        if value > prior {
            out.errors.insert(name.clone(), value - prior);
            // The retained text is the first error this census ever saw for the variant;
            // with the expected zero-error run there is nothing to attribute.
            if let Some(text) = after.first_error.get(name) {
                out.first_error.insert(name.clone(), text.clone());
            }
        }
    }
    out.total_executed = after.total_executed.saturating_sub(before.total_executed);
    out.total_errors = after.total_errors.saturating_sub(before.total_errors);
    out
}

/// The rows of a census, sorted by `executed` descending with a stable tie-break by name.
fn ranking(snapshot: &Snapshot) -> Vec<(String, u64, u64)> {
    let mut rows: Vec<(String, u64, u64)> = snapshot
        .executed
        .iter()
        .map(|(name, &executed)| {
            let errors = snapshot.errors.get(name).copied().unwrap_or(0);
            (name.clone(), executed, errors)
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

/// Print one ranked table (`rank  variant  executed  errors`), plus the total line.
fn print_ranking(title: &str, note: &str, snapshot: &Snapshot, limit: Option<usize>) {
    println!("\n===== {title} =====");
    if !note.is_empty() {
        println!("{note}");
    }
    println!(
        "total_executed={}  distinct_variant_names={}  total_errors={}",
        snapshot.total_executed,
        snapshot.executed.len(),
        snapshot.total_errors
    );
    println!("{0:>4}  {1:<28} {2:>12} {3:>8}", "rank", "variant", "executed", "errors");
    let rows = ranking(snapshot);
    let shown = limit.unwrap_or(rows.len()).min(rows.len());
    for (index, (name, executed, errors)) in rows.iter().take(shown).enumerate() {
        println!("{0:>4}  {1:<28} {2:>12} {3:>8}", index + 1, name, executed, errors);
    }
    if shown < rows.len() {
        println!("      … {} more variant name(s) (see the TSV dump)", rows.len() - shown);
    }
}

/// Print the typed-error map, loudly and under a banner, when it is not empty.
///
/// The expected state is empty (every layer is a *successful* run and a raise caught
/// in-machine is not an error), so a non-empty map is a finding, not a pass: it is
/// printed with each variant's exact first-error text and never hidden.
fn print_errors(snapshot: &Snapshot) {
    if snapshot.total_errors == 0 {
        println!("typed-error map: EMPTY (no instruction returned an error out of the funnel)");
        return;
    }
    println!("\n*** CENSUS ERRORS ***");
    println!(
        "{} instruction(s) returned an error out of the funnel across {} variant(s):",
        snapshot.total_errors,
        snapshot.errors.len()
    );
    println!("{0:<28} {1:>10}  {2}", "variant", "errors", "first error text");
    for (name, errors) in &snapshot.errors {
        let text = snapshot
            .first_error
            .get(name)
            .map(String::as_str)
            .unwrap_or("-");
        println!("{name:<28} {errors:>10}  {text}");
    }
}

// ---------------------------------------------------------------------------
// The layer-(a) host: `ModuleHost` + the `get_module_info` placeholder
// ---------------------------------------------------------------------------

/// The layer-(a) execution context: [`ModuleHost`], with the two `module_info` forwarder
/// targets answered.
///
/// Every real module's `module_info/0,1` body is a `call_ext_only` to
/// `erlang:get_module_info/1,2`, and the test host's BIF floor does not serve it — so
/// without this wrapper every one of the ≈6800 layer-(a) runs would raise `badarg` and
/// the census error map would be dominated by a host gap rather than by the engine.
///
/// The two arms **echo the forwarded argument** (the module name for `/1`, the key for
/// `/2`). That is the established placeholder of `tests/coverage.rs:168-175` and
/// `tests/jit_coverage.rs:96-102`: a real `get_module_info/2` returns the exports list,
/// the attribute list, the compile-info list, …, and nothing in this suite asserts a
/// `module_info` *value* — the census only needs the forwarder's instruction stream to
/// run. Everything else is forwarded to the host unchanged, so the BIF floor, the
/// cross-module calls and the `apply_fun` seam behave exactly as they do for every other
/// stdlib suite.
struct CensusHost {
    inner: ModuleHost,
}

impl CensusHost {
    fn new(inner: ModuleHost) -> CensusHost {
        CensusHost { inner }
    }

    /// The wrapped host (module loading and interning are host operations).
    fn host(&mut self) -> &mut ModuleHost {
        &mut self.inner
    }
}

impl ExecutionContext for CensusHost {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        match (module, function, arity) {
            ("erlang", "get_module_info", 1) => args.first().copied().ok_or(BeamError::BadArg),
            ("erlang", "get_module_info", 2) => args.get(1).copied().ok_or(BeamError::BadArg),
            _ => self.inner.call_bif(module, function, arity, args),
        }
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        self.inner.apply_fun(fun, args)
    }
}

/// A host over the system stdlib with the extra applications on the search path.
fn workload_host() -> ModuleHost {
    let ebin = support::stdlib_ebin()
        .expect("the oracle stdlib `ebin` (this test skip-loudly guards its absence)");
    let mut host = ModuleHost::new(ebin);
    for app in EXTRA_APPS {
        if let Some(dir) = support::app_ebin(app) {
            host.add_search_dir(dir);
        }
    }
    host
}

/// Registers the four stdlib harness fixtures layer (b) calls into.
fn register_harnesses(host: &mut ModuleHost) {
    for name in HARNESS_FIXTURES {
        host.register(name, fixture_path(name));
    }
}

/// `tests/fixtures/<file>.beam`.
fn fixture_path(file: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}/tests/fixtures/{file}.beam",
        env!("CARGO_MANIFEST_DIR")
    ))
}

// ---------------------------------------------------------------------------
// Layer (a): the whole-OTP corpus sweep + a STATIC per-variant tally
// ---------------------------------------------------------------------------

/// What layer (a) measured (the host itself is dropped before the next layer starts).
#[derive(Default)]
struct LayerA {
    /// Static occurrence count per variant name over every loaded module's code.
    statics: BTreeMap<String, u64>,
    /// Modules that loaded.
    loaded: usize,
    /// Distinct application directories covered.
    apps: usize,
    /// `(module, exact typed error)` per module that failed to LOAD (must be empty).
    load_failures: Vec<(String, Vec<String>)>,
    /// Modules whose `module_info` export could not be resolved at all.
    without_module_info: Vec<String>,
    /// `module_info` runs that returned a value.
    served: u64,
    /// `module_info` runs that returned an error (with the first few exact texts).
    unserved: u64,
    unserved_samples: Vec<String>,
    /// Total `ExpT` entries over the loaded corpus.
    exports: u64,
    /// Export entries whose label is a function's `func_label` (the label *before*
    /// `func_info`) — the only way the export route could reach the `FuncInfo` arm.
    exports_on_func_label: u64,
}

impl LayerA {
    /// The static per-variant ranking (descending occurrences).
    fn top(&self, n: usize) -> Vec<(String, u64)> {
        let mut rows: Vec<(String, u64)> = self
            .statics
            .iter()
            .map(|(name, &count)| (name.clone(), count))
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        rows.truncate(n);
        rows
    }
}

/// Run layer (a): load the whole installed corpus, tally it statically, and execute every
/// module's `module_info/0,1` on the interpreter tier.
fn layer_a(beams: &[support::BeamFile]) -> LayerA {
    let mut census = CensusHost::new(workload_host());
    // The `module_info/1` key terms, interned in the SHARED table (every loaded module's
    // atom-index snapshot is a clone of it, so an id interned here is valid everywhere).
    let keys: Vec<Eterm> = MODULE_INFO_KEYS
        .iter()
        .map(|key| census.host().intern(key))
        .collect();

    // One machine, reused: every `run_label_outcome` resets it, and a `module_info` run
    // builds no off-heap payload to outlive it — the result is the host's own arena term
    // (or the placeholder echo's argument), so there is nothing for a per-run machine to
    // keep alive. (`ModuleHost::call` retains a machine per run precisely because an
    // arbitrary stdlib result can hold an off-heap refc binary; layer (a) cannot.)
    let mut machine = Machine::new();
    let mut out = LayerA::default();
    let mut apps: BTreeSet<String> = BTreeSet::new();

    for beam in beams {
        apps.insert(beam.app.clone());
        // Key by the full path so two same-named modules can never mask one another.
        let key = beam.path.to_string_lossy().into_owned();
        let module = match census.host().load_from_path(&key, &beam.path) {
            Ok(module) => module,
            Err(error) => {
                let text = format!("{error:?}");
                println!("LOAD-FAIL {}/{}: {text}", beam.app, beam.module);
                out.load_failures.push((format!("{}/{}", beam.app, beam.module), vec![text]));
                continue;
            }
        };
        out.loaded += 1;

        // The STATIC tally: every instruction instance this module's code contains.
        for function in &module.functions {
            for instr in &function.code {
                *out.statics.entry(exec_census::variant_name(instr)).or_insert(0) += 1;
            }
        }

        // The `FuncInfo` reachability measurement (see `SILENT_TOP_ALLOWED`).
        //
        // The loader keeps BOTH labels of a function (`Function.func_label`, the label
        // *before* `func_info`, and `Function.entry_label`, the label *after* it), but
        // `Export.label == Function.entry_label` (`beam_loader/src/ir.rs`) — so every
        // engine entry point that resolves through the export table (`run_export`,
        // `resolve_external_call` → `call_ext*`, `apply`) starts AFTER the `func_info`
        // and can never execute it. This counts the export entries that would have to
        // land on a `func_label` for the `FuncInfo` arm to be reachable that way.
        let func_labels: BTreeSet<Label> =
            module.functions.iter().map(|function| function.func_label).collect();
        for export in &module.exports {
            out.exports += 1;
            if func_labels.contains(&export.label) {
                out.exports_on_func_label += 1;
            }
        }
        let Some(function) = module.atom_table.lookup("module_info") else {
            out.without_module_info
                .push(format!("{}/{}", beam.app, beam.module));
            continue;
        };

        // `module_info/0`, then `module_info/1` for each key, on the INTERPRETER tier so
        // every instruction passes the census funnel.
        for (arity, args) in std::iter::once((0u32, &[][..]))
            .chain(keys.iter().map(|key| (1u32, std::slice::from_ref(key))))
        {
            match machine.run_with_tier(&module, function, arity, args, &mut census, Tier::Interpreter)
            {
                Ok(_) => out.served += 1,
                Err(error) => {
                    out.unserved += 1;
                    if out.unserved_samples.len() < 8 {
                        out.unserved_samples
                            .push(format!("{}:module_info/{arity}: {error}", beam.module));
                    }
                }
            }
        }
    }

    out.apps = apps.len();
    out
}

// ---------------------------------------------------------------------------
// Layers (b) and (c): the named workload sets
// ---------------------------------------------------------------------------

/// What a workload layer ran.
#[derive(Default)]
struct LayerRuns {
    /// Workloads that returned a value.
    ran: usize,
    /// `(label, exact typed error)` for each workload that did not.
    failures: Vec<(&'static str, String)>,
}

/// Run layer (b): the program workloads over the real stdlib.
fn layer_b() -> LayerRuns {
    let mut host = workload_host();
    register_harnesses(&mut host);
    let mut out = LayerRuns::default();
    for (label, run) in catalog::WORKLOADS {
        match run(&mut host) {
            Ok(_) => out.ran += 1,
            Err(error) => out.failures.push((label, format!("{error:?}"))),
        }
    }
    out
}

/// Run layer (c): the committed fixtures, on a fresh host (they shadow stdlib names).
fn layer_c() -> LayerRuns {
    let mut host = workload_host();
    let mut out = LayerRuns::default();
    for fixture in catalog::FIXTURES {
        let path = match fixture.path {
            Some(path) => PathBuf::from(path),
            None => fixture_path(fixture.module),
        };
        host.register(fixture.module, path);
        for (label, run) in fixture.runs {
            match run(&mut host) {
                Ok(_) => out.ran += 1,
                Err(error) => out.failures.push((label, format!("{error:?}"))),
            }
        }
    }
    out
}

/// Print a workload layer's served/failed split, with the exact typed error per failure.
fn print_runs(title: &str, layer: &LayerRuns) {
    println!(
        "{title}: {} ran, {} failed",
        layer.ran,
        layer.failures.len()
    );
    for (label, text) in &layer.failures {
        println!("  WORKLOAD-FAIL {label}: {text}");
    }
}

// ---------------------------------------------------------------------------
// The blocked-mechanism probe (census OFF)
// ---------------------------------------------------------------------------

/// One blocked-mechanism probe result.
struct Blocked {
    name: &'static str,
    /// Why the family is out of reach here (the mechanism, not the error).
    note: &'static str,
    outcome: Result<Eterm>,
}

/// Probe the families the test host cannot serve, with the census **off**.
///
/// These shapes are the ones the sibling suites pin as blocked mechanisms
/// (`tests/stdlib_collections_queue_maps.rs` for `maps:*`,
/// `tests/stdlib_ets.rs` for `ets:*`, `tests/stdlib_text.rs` for the
/// `unicode:characters_to_list/1` / `binary:compile_pattern/1` NIFs). They contribute
/// **no** dynamic weight to the ranking above; recording them here is what makes that an
/// explicit measurement instead of a silent omission.
fn blocked_probe() -> Vec<Blocked> {
    let mut host = workload_host();
    let mut out: Vec<Blocked> = Vec::new();

    let a = host.intern("a");
    let b = host.intern("b");
    let flat = beam_engine::terms::make_flatmap(&mut host, &[
        (a, beam_term::make_small(1)),
        (b, beam_term::make_small(2)),
    ])
    .expect("build a flat map on the host arena");
    let keys = beam_engine::terms::make_list(&mut host, &[a])
        .expect("build the `[a]` key list on the host arena");

    let outcome = host.call("maps", "get", &[a, flat]);
    out.push(Blocked { name: "maps:get/2 (erlang:map_get/2 NIF)", note: "the floor serves no map BIF; `maps` loads but its body calls the C `erlang:map_get/2`", outcome });

    let outcome = host.call("maps", "to_list", &[flat]);
    out.push(Blocked { name: "maps:to_list/1 (erts_internal:map_next/3)", note: "`erts_internal` is a C module with no `.beam`, so the external-call route fails to LOAD it (Io NotFound), not BadArg", outcome });

    let outcome = host.call("maps", "with", &[keys, flat]);
    out.push(Blocked { name: "maps:with/2 (folds over maps:get/2)", note: "a fold that bottoms out in `erlang:map_get/2`", outcome });

    let empty = beam_engine::terms::make_list(&mut host, &[])
        .expect("build the `[]` option list on the host arena");
    let outcome = host.call("ets", "new", &[a, empty]);
    out.push(Blocked { name: "ets:new/2 (needs a process + the ets BIFs)", note: "ETS needs a table registry + a process identity the floor does not provide", outcome });

    let outcome = host.call("ets", "lookup", &[a, b]);
    out.push(Blocked { name: "ets:lookup/2 (the ets BIFs)", note: "same — no table can be created, so no lookup can succeed", outcome });

    let text = support::chars(&mut host, "abc");
    let outcome = host.call("unicode", "characters_to_list", &[text]);
    out.push(Blocked { name: "unicode:characters_to_list/1 (NIF)", note: "a pure NIF (`unicode` calls `erts_internal`/NIF code) with no Rust-side host hook", outcome });

    let text = support::chars(&mut host, "abc");
    let outcome = host.call("binary", "compile_pattern", &[text]);
    out.push(Blocked { name: "binary:compile_pattern/1 (NIF)", note: "a pure NIF (`binary` compiles a pattern via `erts_internal`)", outcome });

    let text = support::chars(&mut host, "a b");
    let seps = support::chars(&mut host, " ");
    let outcome = host.call("string", "lexemes", &[text, seps]);
    out.push(Blocked { name: "string:lexemes/2 (via binary:compile_pattern/1)", note: "`string:lexemes/2` lowers to `binary:compile_pattern/1`", outcome });

    out
}

/// Print the blocked-mechanism table, with each probe's exact typed error.
fn print_blocked(blocked: &[Blocked]) {
    println!("\n===== BLOCKED MECHANISMS (probed with the census OFF — zero dynamic weight) =====");
    println!(
        "The test host's BIF floor (`tests/support/bifs.rs`) serves a small `erlang:*`/\
         `lists:*` set; these families are outside it, so a workload over them would only\n\
         record a typed refusal. They are therefore NOT part of the ranking above, and no\n\
         variant they would have exercised carries dynamic weight from them."
    );
    for probe in blocked {
        let verdict = match &probe.outcome {
            Ok(_) => "NOT refused (the floor serves it)".to_string(),
            Err(error) => format!("refused, exact typed error: {error:?}"),
        };
        println!("  {}", probe.name);
        println!("      why : {}", probe.note);
        println!("      what: {verdict}");
    }
}

// ---------------------------------------------------------------------------
// The test: one serialized run (the census is thread-local)
// ---------------------------------------------------------------------------

/// **The dynamic per-variant execution ranking** over real compiled Erlang.
///
/// One test only, deliberately: the census is a thread-local and the boundary snapshots
/// below must be taken on one thread with no other counted run interleaved. See the
/// module docs for the three layers, the caveats and exactly what is asserted.
#[test]
fn dynamic_variant_ranking_over_real_compiled_erlang() {
    if let Some(reason) = support::skip_reason() {
        println!("{reason}");
        return;
    }
    let Some(beams) = support::all_otp_beams() else {
        println!(
            "SKIP: the whole-OTP walk found no lib root (`erl`/the OTP install is absent) — \
             a dynamic census needs the installed `.beam` corpus"
        );
        return;
    };
    assert!(
        !beams.is_empty(),
        "the whole-OTP walk found an empty `.beam` set"
    );

    // ---- the instrument: ONE reset, then a boundary snapshot per layer -----
    let _ = exec_census::is_enabled();
    exec_census::reset();
    exec_census::enable();
    let s0 = exec_census::snapshot();

    println!(
        "DYNAMIC CENSUS: {} `.beam` files discovered; tier=Interpreter for every run",
        beams.len()
    );

    let layer_a = layer_a(&beams);
    let s1 = exec_census::snapshot();

    let layer_b = layer_b();
    let s2 = exec_census::snapshot();

    let layer_c = layer_c();
    let s3 = exec_census::snapshot();

    // ---- the blocked-mechanism probe, with the census OFF ------------------
    exec_census::disable();
    let blocked = blocked_probe();

    // The live census is still exactly the COMBINED one (nothing above counted while
    // disabled), so this dump is the combined ranking the deliverable asks for.
    let dump = exec_census::write_dump("dynamic_corpus");
    let combined = exec_census::snapshot();

    let layer_a_stats = diff(&s0, &s1);
    let layer_b_stats = diff(&s1, &s2);
    let layer_c_stats = diff(&s2, &s3);

    // ---- layer (a) report -------------------------------------------------
    println!("\n===== LAYER (a): whole-OTP corpus sweep (`module_info/0,1` per module) =====");
    println!(
        "modules loaded={} over {} applications; load failures={}",
        layer_a.loaded,
        layer_a.apps,
        layer_a.load_failures.len()
    );
    println!(
        "module_info runs: served={} unserved={} (modules without a `module_info` export: {})",
        layer_a.served,
        layer_a.unserved,
        layer_a.without_module_info.len()
    );
    for sample in &layer_a.unserved_samples {
        println!("  UNSERVED {sample}");
    }
    for (module, text) in &layer_a.load_failures {
        println!("  LOAD-FAIL {module}: {text:?}");
    }
    println!("\nSTATIC corpus weight — corpus-reachable variant names: {}", layer_a.statics.len());
    println!("{0:>4}  {1:<28} {2:>12}", "rank", "variant", "static occurrences");
    for (index, (name, count)) in layer_a.top(TOP_ROWS).into_iter().enumerate() {
        println!("{0:>4}  {1:<28} {2:>12}", index + 1, name, count);
    }
    print_ranking(
        "LAYER (a) DYNAMIC RANKING",
        "(only the metadata forwarders run in this layer; the depth comes from (b)+(c))",
        &layer_a_stats,
        Some(TOP_ROWS),
    );
    print_errors(&layer_a_stats);

    // ---- layer (b) report -------------------------------------------------
    println!("\n===== LAYER (b): {} program workloads over the real stdlib =====", catalog::WORKLOADS.len());
    print_runs("layer (b) workloads", &layer_b);
    print_ranking(
        "LAYER (b) DYNAMIC RANKING",
        "stdlib workloads; every `module:function(args)` shape is copied from an oracle-proven suite",
        &layer_b_stats,
        Some(TOP_ROWS),
    );
    print_errors(&layer_b_stats);

    // ---- layer (c) report -------------------------------------------------
    println!(
        "\n===== LAYER (c): {} committed fixtures =====",
        catalog::FIXTURES.len()
    );
    print_runs("layer (c) fixtures", &layer_c);
    print_ranking(
        "LAYER (c) DYNAMIC RANKING",
        "the committed `.beam` fixtures (entry points read off each fixture's own `.erl` source)",
        &layer_c_stats,
        Some(TOP_ROWS),
    );
    print_errors(&layer_c_stats);

    // ---- the combined report ---------------------------------------------
    print_ranking(
        "COMBINED DYNAMIC RANKING (a)+(b)+(c) — the table `write_dump(\"dynamic_corpus\")` holds",
        "one `reset()` at the start, so this is the live census the dump writes",
        &combined,
        None,
    );
    print_errors(&combined);

    // ---- the not-reached analysis ----------------------------------------
    let static_reachable: BTreeSet<&String> = layer_a.statics.keys().collect();
    let dynamic: BTreeSet<&String> = combined.executed.keys().collect();
    let mut not_reached: Vec<(&String, u64)> = static_reachable
        .iter()
        .filter(|name| !dynamic.contains(**name))
        .map(|name| (*name, layer_a.statics[*name]))
        .collect();
    not_reached.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    println!(
        "\n===== NOT REACHED BY THIS WORKLOAD SET ({} of {} corpus-reachable variant names) =====",
        not_reached.len(),
        static_reachable.len()
    );
    println!("{0:<28} {1:>12}  {2}", "variant", "static", "one-line reason");
    for (name, count) in &not_reached {
        println!("{0:<28} {1:>12}  {2}", name, count, not_reached_reason(name));
    }

    // Every dynamically observed name also occurs statically in the loaded corpus: a
    // coherence check on the instrument (nothing is counted that no loaded module holds).
    let mut dynamic_only: Vec<&String> = dynamic
        .iter()
        .filter(|name| !static_reachable.contains(**name))
        .map(|name| *name)
        .collect();
    dynamic_only.sort();
    println!(
        "\ndynamic-but-never-static variant names (must be empty): {dynamic_only:?}"
    );

    // The COMPUTED zero-static-occurrence set: roster names minus the names the loaded
    // corpus holds. Informational only (no assertion) — and the figure it reproduces is the
    // 13-name forward-compat pin `tests/census.rs` derives from the same corpus.
    let roster: BTreeSet<String> = INSTR_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .chain(TEST_KIND_NAMES.iter().map(|kind| format!("Test({kind})")))
        .collect();
    let zero_static: Vec<&String> = roster
        .iter()
        .filter(|name| !static_reachable.contains(name))
        .collect();
    println!(
        "\n===== ZERO STATIC OCCURRENCE ({} of {} roster names; computed, not asserted) =====",
        zero_static.len(),
        roster.len()
    );
    println!(
        "no loaded module of the whole installed corpus contains any of these, so no workload \
         over real compiler output can execute them:\n  {zero_static:?}"
    );

    println!(
        "\nDISTINCT VARIANTS: layer (a)={} layer (b)={} layer (c)={} combined={} \
         (names; the {} -variant `Instr` roster splits into more names by `Test(<Kind>)`)",
        layer_a_stats.executed.len(),
        layer_b_stats.executed.len(),
        layer_c_stats.executed.len(),
        combined.executed.len(),
        INSTR_ROSTER,
    );
    println!(
        "TOTAL EXECUTED: combined={} (floor pinned at {TOTAL_EXECUTED_FLOOR})",
        combined.total_executed
    );

    print_blocked(&blocked);

    match &dump {
        Some(path) => println!("\nTSV dump: {}", path.display()),
        None => println!("\nWARNING: the TSV dump could not be written (`write_dump` returned None)"),
    }

    // ---- assertions: liveness and honesty only ---------------------------
    assert!(
        layer_a.loaded >= LOADED_FLOOR,
        "layer (a) only loaded {} modules (expected the whole installed OTP, ≥{LOADED_FLOOR})",
        layer_a.loaded
    );
    assert!(
        layer_a.load_failures.is_empty(),
        "{} of {} discovered modules failed to LOAD: {:?}",
        layer_a.load_failures.len(),
        layer_a.loaded + layer_a.load_failures.len(),
        layer_a.load_failures
    );
    assert!(
        layer_b.failures.is_empty(),
        "{} of {} layer-(b) workloads failed (each with its exact typed error above): {:?}",
        layer_b.failures.len(),
        catalog::WORKLOADS.len(),
        layer_b.failures
    );
    assert!(
        layer_c.failures.is_empty(),
        "{} layer-(c) fixture run(s) failed (each with its exact typed error above): {:?}",
        layer_c.failures.len(),
        layer_c.failures
    );
    assert!(
        combined.total_executed > TOTAL_EXECUTED_FLOOR,
        "the combined census executed {} instructions — the floor is {TOTAL_EXECUTED_FLOOR}; \
         a workload layer has stopped running",
        combined.total_executed
    );
    assert!(
        layer_a.unserved == 0,
        "{} `module_info` run(s) were unserved (first samples: {:?}) — the layer-(a) sweep must \
         be able to run every module's metadata forwarders",
        layer_a.unserved,
        layer_a.unserved_samples
    );

    // The computed static top-10 must all carry dynamic weight — bar the documented
    // `SILENT_TOP_ALLOWED` exceptions, whose unreachability is measured (not asserted away)
    // right below.
    println!(
        "\nFUNC-INFO REACHABILITY: {} corpus `ExpT` entries; {} of them point at a \
         function's `func_label` (the label BEFORE `func_info`) — the only way any export \
         route could execute a `FuncInfo`",
        layer_a.exports, layer_a.exports_on_func_label
    );
    let top10 = layer_a.top(10);
    let mut silent_top: Vec<&str> = Vec::new();
    for (name, static_count) in &top10 {
        let executed = combined.executed.get(name).copied().unwrap_or(0);
        println!("TOP-10 STATIC {name:<28} static={static_count:>10} executed={executed:>12}");
        if executed == 0 {
            silent_top.push(name);
        }
    }
    for name in &silent_top {
        assert!(
            SILENT_TOP_ALLOWED.contains(name),
            "the computed static top-10 variant `{name}` executed ZERO times and is not a \
             documented exception — extend the workload set so a real program of this shape \
             reaches it (documented exceptions: {SILENT_TOP_ALLOWED:?})"
        );
    }
    // The exception's justification, asserted: if the loader ever resolved an export onto a
    // `func_info` label, `FuncInfo` WOULD be reachable and the exclusion above would be a
    // lie — so this is the guard that keeps it honest.
    assert_eq!(
        layer_a.exports_on_func_label, 0,
        "{} of {} corpus export entries now resolve onto a `func_label`, i.e. onto a \
         `func_info` — `FuncInfo` is reachable through the export route after all, so the \
         `SILENT_TOP_ALLOWED` exclusion for it must be removed",
        layer_a.exports_on_func_label, layer_a.exports
    );

    // The hand-written core-variant slice.
    for variant in MUST_EXECUTE {
        let executed = combined.executed.get(variant).copied().unwrap_or(0);
        assert!(
            executed > 0,
            "the core variant `{variant}` executed ZERO times — the workload set no longer \
             covers the common code shape it stands for"
        );
    }

    // The printed "not reached" list is EXACTLY the corpus-reachable set with zero
    // dynamic count, asserted over the whole computed static set.
    for name in &static_reachable {
        let executed = combined.executed.get(*name).copied().unwrap_or(0);
        let listed = not_reached.iter().any(|(candidate, _)| candidate == name);
        assert_eq!(
            executed > 0,
            !listed,
            "the `not reached` list disagrees with the census for `{name}` \
             (executed={executed}, listed={listed})"
        );
    }

    assert!(
        dynamic_only.is_empty(),
        "these variant names executed without occurring statically in the loaded corpus: \
         {dynamic_only:?} — the census instrument and the loaded corpus disagree"
    );

    println!(
        "\nOK: {} modules, {} distinct variant names executed, {} corpus-reachable names not \
         reached by this workload set (listed above)",
        layer_a.loaded,
        combined.executed.len(),
        not_reached.len()
    );
}

/// One-line provenance/reason hint for a corpus-reachable variant this workload set does
/// not reach.
///
/// Each arm names the *mechanism* the variant needs; the fallback says the variant simply
/// has no shape in the named workload set. These are hints for the reader (and the pointer
/// for whoever extends the catalog), not program logic.
fn not_reached_reason(variant: &str) -> &'static str {
    match variant {
        "Send" | "LoopRec" | "LoopRecEnd" | "Wait" | "WaitTimeout" | "Timeout"
        | "RemoveMessage" | "RecvMarkerReserve" | "RecvMarkerBind" | "RecvMarkerClear"
        | "RecvMarkerUse" => "needs a mailbox seam (the test host installs none)",
        "DebugLine" | "ExecutableLine" => "debug/line-coverage codegen (not in this corpus build)",
        "FuncInfo" => "unreachable by construction: every export entry is the label AFTER \
                       `func_info` (asserted: 0 exports on a `func_label`)",
        "Badrecord" => "a record-access failure inside a `try`, i.e. a raise (no workload raises)",
        "Bif0" => "a 0-arity BIF call whose BIF the test host's BIF floor serves",
        "IntCodeEnd" => "the `int_code_end` trailer of a module's code (never entered)",
        "BsSkipBits2" | "BsSkipUtf8" | "BsSkipUtf16" | "BsSkipUtf32" | "BsGetUtf16"
        | "BsGetUtf32" | "BsGetInteger2" | "BsGetBinary2" | "BsMatchString" =>
            "a bitstring match/skip shape the committed fixtures do not contain",
        "Test(IsPid)" | "Test(IsPort)" | "Test(IsReference)" | "Test(IsFunction)"
        | "Test(IsNumber)" | "Test(IsBoolean)" | "Test(IsNe)" | "Test(IsBitstr)" =>
            "a guard on a value type the named workloads never test for",
        "ApplyLast" => "a tail-position `apply/3`",
        "IfEnd" => "an `if` expression with no matching branch (a raise path)",
        "TryCaseEnd" => "a `try ... of` clause-end (the fixtures use `try ... catch` only)",
        "PutMapExact" => "a `Map#{K := V}` EXACT update (the fixtures only build with `=>`)",
        "IsAnyNativeRecord" => "the native-record type test (an OTP-29 shape, static-only here)",
        "Fdiv" | "Fnegate" => "float division / float negation",
        "NifStart" | "OnLoad" => "compiled NIF stub body (no NIF library is loaded here)",
        "RawRaise" | "BuildStacktrace" => "a `raise`/`try ... catch` stacktrace-building shape",
        "BsInitWritable" => "a writable-binary accumulator (compiler-proven linear append)",
        "SetTupleElement" => "an in-place tuple write (the corpus lowers none)",
        "Deallocate" => "the loader fuses every `deallocate` into `DeallocateReturn`",
        "Generic" => "the loader's residual sentinel (the corpus lowers none)",
        _ => "no shape in this named workload set reaches it",
    }
}
