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
//! **(b) — program workloads.** 44 named workloads over the real installed stdlib, each
//! `module:function(args)` shape copied **verbatim** from an existing oracle-proven suite:
//! `lists`/`ordsets`/`proplists` (`tests/stdlib_lists.rs`), `string`/`base64`/`io_lib`
//! (`tests/stdlib_text.rs`), `lists:foldr`/`search` + `ordsets:filter` closures
//! (`tests/stdlib_text_more.rs`), `calendar`/`filename`/`array`/`orddict`
//! (`tests/stdlib_collections.rs`), `gb_trees`/`gb_sets`
//! (`tests/stdlib_collections_trees.rs`), `queue` (`tests/stdlib_collections_queue_maps.rs`),
//! the closure-taking `orddict:map`/`lists:zipwith` (`tests/stdlib_more.rs`) and
//! `ordsets:map` (`tests/stdlib_exports_more.rs`) — the last five drive the
//! `fixtures/*_harness.beam` wrappers, so they exercise `make_fun3` + `call_fun*` +
//! cross-module `apply_fun`. Three of the 44 are **deliberate clause failures**: a call
//! whose every head clause fails jumps to the function's `func_label` — the label
//! *before* `func_info` — so the `FuncInfo` arm executes and raises `function_clause`
//! (C's `i_func_info_IaaI`). The exact catalog (and each shape's provenance comment) is
//! [`catalog::WORKLOADS`].
//!
//! **(c) — the committed fixtures.** 22 `tests/fixtures/*.beam` entries (plus the sibling
//! `beam_loader` crate's `slice.beam`) covering `slice`, `tuples`, `records`, `floats`,
//! `bitstrings`, `bscreate`, `bsmatch`, `bsstring`, `bignum`, `exceptions`, `excpaths`,
//! `catchtrace`, `stacktrace_frames`, `catchclasses`, `mapfx`, `tailcall`, `funs` and
//! `atomapply`/`atomtarget` (a runtime-atom `apply/3`), driven at the entry points read
//! off each fixture's own `.erl` source; catalog [`catalog::FIXTURES`]. The same layer
//! also drives the one **mailbox-seam** fixture, `recv.beam`, on a [`CensusHost`] (the
//! only host here that owns a [`Mailbox`]) — nine runs covering `send`,
//! `loop_rec`/`loop_rec_end`, `remove_message`, `wait`, `wait_timeout`, `timeout` and the
//! four `recv_marker_*` opcodes; catalog [`catalog::MAILBOX_FIXTURES`].
//!
//! # Honest caveats (read these before quoting any number)
//!
//! * This is a **named workload set over real compiler output**, not whole-corpus
//!   throughput. Layer (a) executes only the metadata forwarders, so the bulk of the
//!   dynamic weight comes from layers (b)+(c) — a different workload set would produce a
//!   different ranking, and `not reached by this workload set` is exactly that: *not
//!   reached here*, not "unsupported by the engine".
//! * **Three blind spots of the instrument.** (i) A natively-inlined baseline-JIT
//!   instruction never passes the interpreter funnel and is therefore unobserved, which
//!   is why every run here pins [`Tier::Interpreter`]. (ii) A raise caught inside a live
//!   in-machine `try`/`catch` counts as **executed**, not as an error — only an `Err`
//!   escaping the funnel is an "error" (so e.g. `Raise`/`Badmatch`/`TryCase` carry
//!   dynamic weight with a zero error count). (iii) The `ModuleHost` BIF floor decides
//!   which *families* can run at all: `maps:*`, `ets:*`, `re:*` and the
//!   `unicode`/`binary` NIFs are outside it, so they contribute zero dynamic weight here
//!   (measured as a `BLOCKED` table with the census **off**, never silently omitted).
//! * **There are no unexpected errors.** Every raise this run produces is one of the
//!   declared shapes in [`DECLARED_RAISES`] (three clause failures, the `dict` host gap,
//!   and the two `recv` shapes that would suspend), and the test fails if anything else
//!   raises.
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
//! Load breadth, a large floor on the combined executed count, the **full computed**
//! static top-10 executing, a hand-written core-variant slice executing, every raise
//! being a declared shape (and every declared shape really raising), the `not reached`
//! list being exactly the corpus-reachable variants with zero dynamic count, and that
//! every dynamically observed variant name also occurs statically in the loaded corpus
//! (an instrument-coherence check). No exact count is pinned anywhere.

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
use beam_engine::{ExecutionContext, Machine, Mailbox, Tier};
use beam_loader::Label;
use beam_term::{is_small, make_small, Eterm};

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
/// suite measured on the reference host (**37 159** instructions over the three layers —
/// see the `TOTAL EXECUTED` line the test prints), so a workload that silently stops
/// running is caught while normal drift is not.
const TOTAL_EXECUTED_FLOOR: u64 = 18_000;

/// **The declared raise roster**: `(label, class, reason)` for every workload or fixture
/// run that is *expected* to raise, spelled as `Display for Raise` renders it —
/// `"{class} {reason}"` (`beam_common/src/error.rs`) — because that is verbatim the
/// `first_error` text the census records.
///
/// Three shapes, and nothing else, may raise:
///
/// * **A deliberate clause failure** (`dict_update_counter_guard_fails`,
///   `gen_name_pid_guard_fails`, `excpaths:fc/1`): every head clause of the callee fails,
///   so `func_info` raises class `error` with the bare atom `function_clause`. The engine
///   carries that atom in the out-of-band live-reason slot (`Raise::live_reason`) while
///   `ExitReason::Structured` stays fieldless, so `Display` prints the class plus the
///   reason *placeholder* `<structured reason>` — the exact text
///   `src/exec_census.rs`'s own unit test pins. The declared `reason` below is therefore
///   that recorded text, not a re-spelling of the engine's internal reason.
/// * **A host gap, declared rather than hidden** (`dict_update_counter_hit`):
///   `dict:update_counter/3` with a *number* increment raises `Badrecord` out of
///   `dict:get_slot/2` because this host cannot serve the dictionary's map-backed slot
///   record, so the "hit" arm cannot produce a value here. It is a real finding about the
///   host, not a shape the workload set can drop.
/// * **A receive that would suspend** (`recv:wait/0 (empty)`, `recv:aftervar/5`): with an
///   empty mailbox those two reach `wait`/`wait_timeout`, and the non-blocking
///   `run_export` answers the engine's typed `error::blocked()` raise
///   (`unsupported_opcode: run: process blocked (no message available)`).
///
/// `BeamError::BadArg` renders as the bare `badarg` with **no** class, so no declared
/// shape may bottom out in `erlang:error(badarg)`.
const DECLARED_RAISES: &[(&str, &str, &str)] = &[
    ("dict_update_counter_hit", "error", "<structured reason>"),
    ("dict_update_counter_guard_fails", "error", "<structured reason>"),
    ("gen_name_pid_guard_fails", "error", "<structured reason>"),
    ("excpaths:fc/1", "error", "<structured reason>"),
    (
        "recv:wait/0 (empty)",
        "error",
        "unsupported_opcode: run: process blocked (no message available)",
    ),
    (
        "recv:aftervar/5",
        "error",
        "unsupported_opcode: run: process blocked (no message available)",
    ),
];

/// The declared `(class, reason)` for `label`, when that run is *expected* to raise.
fn declared_raise(label: &str) -> Option<(&'static str, &'static str)> {
    DECLARED_RAISES
        .iter()
        .find(|(candidate, _, _)| *candidate == label)
        .map(|(_, class, reason)| (*class, *reason))
}

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
            // The retained text is the first error this census ever saw for the variant
            // (`first_error` is cumulative, so only the declared-raise variants carry one).
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

/// Print one layer's **declared-outcome** split: the raises it produced (declared class /
/// reason beside the observed text), the per-variant instruction-error map, and — loudly,
/// under a banner — anything that is not in [`DECLARED_RAISES`].
///
/// The expected state is "no unexpected raises": a declared raise is a *measured* shape
/// (the workload reached the failure it claims to exercise), while a raise no declaration
/// covers is a finding, never a pass, and the caller asserts its emptiness.
fn print_raise_split(snapshot: &Snapshot, raised: &[(&'static str, String)]) {
    println!("\n===== DECLARED RAISES ({}) =====", raised.len());
    if raised.is_empty() {
        println!("no run in this layer raised");
    } else {
        println!(
            "{0:<32} {1:<6} {2:<34} {3}",
            "run", "class", "declared reason", "observed (Display)"
        );
        for (label, observed) in raised {
            let (class, reason) = declared_raise(label).unwrap_or(("-", "-"));
            println!("{label:<32} {class:<6} {reason:<34} {observed}");
        }
    }
    println!(
        "\ncensus instruction errors: {} across {} variant(s)",
        snapshot.total_errors,
        snapshot.errors.len()
    );
    for (name, errors) in &snapshot.errors {
        let text = snapshot
            .first_error
            .get(name)
            .map(String::as_str)
            .unwrap_or("-");
        println!("  {name:<28} {errors:>8}  {text}");
    }
    let unexpected: Vec<&(&'static str, String)> = raised
        .iter()
        .filter(|(label, _)| declared_raise(label).is_none())
        .collect();
    if unexpected.is_empty() {
        println!("UNEXPECTED: none (every raise above is in `DECLARED_RAISES`)");
    } else {
        println!("\n*** UNEXPECTED CENSUS ERRORS ***");
        for (label, text) in unexpected {
            println!("  {label}: {text}");
        }
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
    /// The queued messages, in arrival order — the [`Mailbox`] seam's storage.
    messages: Vec<Eterm>,
    /// The scan position: `loop_rec` peeks here, `LoopRecEnd` advances it.
    scan: usize,
    /// The reserved receive markers: `(marker term, bound reference, position)`.
    markers: Vec<(Eterm, Option<Eterm>, usize)>,
    /// Distinguishes consecutive reserved markers.
    next_marker: i64,
    /// What `receive_marker_reserve` answers for an **empty** queue (the C `undefined`).
    undefined: Eterm,
    /// The fresh value `erlang:make_ref/0` hands out (`recv:marked/0` only compares it).
    next_ref: i64,
    /// The `self()` atom — a send destination this mailbox ignores.
    self_atom: Eterm,
    /// Every [`Machine`] `call` ran, kept alive so an off-heap result outlives the run
    /// (the same rationale as `ModuleHost`'s own `machines` field, which is private).
    machines: Vec<Machine>,
}

impl CensusHost {
    fn new(inner: ModuleHost) -> CensusHost {
        let mut inner = inner;
        let undefined = inner.intern("undefined");
        let self_atom = inner.intern("self");
        CensusHost {
            inner,
            messages: Vec::new(),
            scan: 0,
            markers: Vec::new(),
            next_marker: 0,
            undefined,
            next_ref: 100,
            self_atom,
            machines: Vec::new(),
        }
    }

    /// The wrapped host (module loading and interning are host operations).
    fn host(&mut self) -> &mut ModuleHost {
        &mut self.inner
    }

    /// Interns `name` in the layer's one shared atom table.
    fn intern(&mut self, name: &str) -> Eterm {
        self.inner.intern(name)
    }

    /// Queues `message` as if it had been sent to this mailbox.
    fn push_message(&mut self, message: Eterm) {
        self.messages.push(message);
    }

    /// Empties the mailbox and repositions the scan, so each fixture run starts from the
    /// mailbox state its own comment describes (a queued message from an earlier run — or
    /// `recv:bomb/1`'s own `self() ! 42` — must not leak into the next one).
    fn clear_mailbox(&mut self) {
        self.messages.clear();
        self.scan = 0;
        self.markers.clear();
    }

    /// The `self()` term (`recv:bomb/1`'s destination).
    fn self_term(&self) -> Eterm {
        self.self_atom
    }

    /// Runs `module:function(args)` on a fresh machine whose context is **this host**,
    /// so the run sees the mailbox (`ModuleHost::call` would run it on the plain host).
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let module = self.inner.load(module)?;
        let function_atom = module
            .atom_table
            .lookup(function)
            .ok_or(BeamError::BadArg)?;
        let mut machine = Machine::new();
        let result = machine.run_export(&module, function_atom, args.len() as u32, args, self);
        self.machines.push(machine);
        result
    }
}

/// The engine's mailbox seam over a `Vec<Eterm>` plus a saturating scan position —
/// the shape `support::ProbeMailbox` and `tests/receive.rs`'s `TestMailbox` use (the
/// C contract at `../../erts/emulator/beam/erl_proc_sig_queue.h:2225-2394`).
impl Mailbox for CensusHost {
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        let _ = to;
        self.push_message(message);
        Ok(())
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(self.messages.get(self.scan).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.scan += 1;
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        if self.scan < self.messages.len() {
            self.messages.remove(self.scan);
        }
        self.scan = 0;
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.scan = 0;
        Ok(())
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        if self.messages.is_empty() {
            return Ok(self.undefined);
        }
        let term = make_small(9000 + self.next_marker);
        self.next_marker += 1;
        self.markers.push((term, None, self.scan));
        Ok(term)
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        if !is_small(marker) {
            return Ok(());
        }
        if let Some(entry) = self.markers.iter_mut().find(|(term, _, _)| *term == marker) {
            entry.1 = Some(reference);
        }
        Ok(())
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        if let Some((_, _, position)) = self
            .markers
            .iter()
            .find(|(_, bound, _)| *bound == Some(reference))
        {
            self.scan = *position;
        }
        Ok(())
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        if let Some(index) = self
            .markers
            .iter()
            .position(|(_, bound, _)| *bound == Some(reference))
        {
            self.markers.remove(index);
        }
        Ok(())
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
            // `recv:marked/0`'s two prologue BIFs: a stand-in reference (the fixture only
            // compares it for equality) and `self()`. Exactly the two hooks
            // `tests/receive.rs::context` installs.
            ("erlang", "make_ref", 0) => {
                let value = make_small(self.next_ref);
                self.next_ref += 1;
                Ok(value)
            }
            ("erlang", "self", 0) => Ok(self.self_atom),
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

    // The nine receive/message seam methods, forwarded to this host's `Mailbox`
    // (`ModuleHost` has none, so the wrapper is the only route).
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        Mailbox::send_message(self, to, message)
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Mailbox::receive_next(self)
    }

    fn receive_skip(&mut self) -> Result<()> {
        Mailbox::receive_skip(self)
    }

    fn receive_remove(&mut self) -> Result<()> {
        Mailbox::receive_remove(self)
    }

    fn receive_reset(&mut self) -> Result<()> {
        Mailbox::receive_reset(self)
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        Mailbox::receive_marker_reserve(self)
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        Mailbox::receive_marker_bind(self, marker, reference)
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        Mailbox::receive_marker_use(self, reference)
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        Mailbox::receive_marker_clear(self, reference)
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
    /// Runs that returned a value.
    ran: usize,
    /// `(label, exact rendered error)` for every run that raised — declared or not. The
    /// partitioning into declared vs unexpected is [`print_raise_split`]'s job (over the
    /// whole run, not per layer), so a raise is never hidden by the layer that saw it.
    raised: Vec<(&'static str, String)>,
}

impl LayerRuns {
    /// Records one run's outcome (`Display`, i.e. exactly the text the census keeps as a
    /// variant's `first_error`).
    fn record(&mut self, label: &'static str, outcome: Result<Eterm>) {
        match outcome {
            Ok(_) => self.ran += 1,
            Err(error) => self.raised.push((label, format!("{error}"))),
        }
    }
}

/// Run layer (b): the program workloads over the real stdlib.
fn layer_b() -> LayerRuns {
    let mut host = workload_host();
    register_harnesses(&mut host);
    let mut out = LayerRuns::default();
    for (label, run) in catalog::WORKLOADS {
        out.record(label, run(&mut host));
    }
    out
}

/// Run layer (c): the committed fixtures, on a fresh host (they shadow stdlib names).
fn layer_c() -> LayerRuns {
    let mut host = workload_host();
    // `atomapply:run/3` `apply/3`s a runtime atom naming `atomtarget`, so the *target*
    // module must be loadable as well — it lives beside `atomapply.beam` in
    // `tests/fixtures/`, not on the stdlib search path.
    host.register("atomtarget", fixture_path("atomtarget"));
    let mut out = LayerRuns::default();
    for fixture in catalog::FIXTURES {
        let path = match fixture.path {
            Some(path) => PathBuf::from(path),
            None => fixture_path(fixture.module),
        };
        host.register(fixture.module, path);
        for (label, run) in fixture.runs {
            out.record(label, run(&mut host));
        }
    }
    out
}

/// Run the mailbox-seam fragment of layer (c): [`catalog::MAILBOX_FIXTURES`] on a
/// [`CensusHost`], the only host in this suite that owns a [`Mailbox`].
///
/// The mailbox is cleared before each run so every run starts from the state its own
/// comment describes; the two shapes that would suspend record the engine's typed
/// `blocked` raise (declared in [`DECLARED_RAISES`]).
fn layer_c_mailbox() -> LayerRuns {
    let mut census = CensusHost::new(workload_host());
    let mut out = LayerRuns::default();
    for fixture in catalog::MAILBOX_FIXTURES {
        census
            .host()
            .register(fixture.module, fixture_path(fixture.module));
        for (label, run) in fixture.runs {
            census.clear_mailbox();
            out.record(label, run(&mut census));
        }
    }
    out
}

/// Print a workload layer's ran/raised split, with the exact rendered error per raise.
fn print_runs(title: &str, layer: &LayerRuns) {
    println!("{title}: {} ran, {} raised", layer.ran, layer.raised.len());
    for (label, text) in &layer.raised {
        println!("  RAISED {label}: {text}");
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

    // Layer (c) is TWO fragments inside ONE boundary pair: the `ModuleHost` fixtures and
    // the mailbox-seam `recv` fixture driven on a `CensusHost`, so `s2..s3` is the layer's
    // whole weight and the live snapshot after `s3` is still the COMBINED one.
    let fixture_runs: usize = catalog::FIXTURES.iter().map(|f| f.runs.len()).sum();
    let mailbox_runs: usize = catalog::MAILBOX_FIXTURES.iter().map(|f| f.runs.len()).sum();
    let mut layer_c = layer_c();
    let mailbox = layer_c_mailbox();
    layer_c.ran += mailbox.ran;
    layer_c.raised.extend(mailbox.raised);
    let s3 = exec_census::snapshot();

    // Every raise the workload layers produced, in one list: partitioned into declared vs
    // unexpected below, and printed beside the combined ranking.
    let mut all_raised: Vec<(&'static str, String)> = Vec::new();
    all_raised.extend(layer_b.raised.iter().cloned());
    all_raised.extend(layer_c.raised.iter().cloned());

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
    print_raise_split(&layer_a_stats, &[]);

    // ---- layer (b) report -------------------------------------------------
    println!("\n===== LAYER (b): {} program workloads over the real stdlib =====", catalog::WORKLOADS.len());
    print_runs("layer (b) workloads", &layer_b);
    print_ranking(
        "LAYER (b) DYNAMIC RANKING",
        "stdlib workloads; every `module:function(args)` shape is copied from an oracle-proven suite",
        &layer_b_stats,
        Some(TOP_ROWS),
    );
    print_raise_split(&layer_b_stats, &layer_b.raised);

    // ---- layer (c) report -------------------------------------------------
    println!(
        "\n===== LAYER (c): {} committed fixture entries ({} runs) + {} mailbox-seam runs =====",
        catalog::FIXTURES.len(),
        fixture_runs,
        mailbox_runs
    );
    print_runs("layer (c) fixtures", &layer_c);
    print_ranking(
        "LAYER (c) DYNAMIC RANKING",
        "the committed `.beam` fixtures (entry points read off each fixture's own `.erl` source)",
        &layer_c_stats,
        Some(TOP_ROWS),
    );
    print_raise_split(&layer_c_stats, &layer_c.raised);

    // ---- the combined report ---------------------------------------------
    print_ranking(
        "COMBINED DYNAMIC RANKING (a)+(b)+(c) — the table `write_dump(\"dynamic_corpus\")` holds",
        "one `reset()` at the start, so this is the live census the dump writes",
        &combined,
        None,
    );
    print_raise_split(&combined, &all_raised);

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
    // The declared-outcome scheme: NOTHING may raise except the shapes in
    // `DECLARED_RAISES`, and every declared shape must really have raised with exactly
    // its declared `"{class} {reason}"` text — a declared raise that returned a value
    // never reached the shape it claims to exercise, so it is a harness bug.
    let mut unexpected: Vec<(&'static str, String)> = all_raised
        .iter()
        .filter(|(label, _)| declared_raise(label).is_none())
        .cloned()
        .collect();
    unexpected.sort();
    assert!(
        unexpected.is_empty(),
        "{} UNEXPECTED raise(s) over {} runs — every raise must be declared in \
         `DECLARED_RAISES` with its exact class+reason text: {unexpected:?}",
        unexpected.len(),
        catalog::WORKLOADS.len() + fixture_runs + mailbox_runs
    );
    for (label, class, reason) in DECLARED_RAISES {
        let observed = all_raised
            .iter()
            .find(|(candidate, _)| candidate == label)
            .map(|(_, text)| text.as_str())
            .unwrap_or_else(|| {
                panic!(
                    "the declared raise `{label}` did not raise — it returned a value (or \
                     never ran), so the workload no longer reaches the shape it declares"
                )
            });
        assert_eq!(
            observed,
            format!("{class} {reason}"),
            "the declared raise `{label}` raised with a text other than declared class+reason"
        );
    }
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

    // The computed static top-10 must ALL carry dynamic weight. `FuncInfo` is the
    // interesting one, and the `ExpT` measurement below is its *explanation*:
    //
    // `func_info` is the *function-entry clause check*. The loader keeps both of a
    // function's labels (`Function.func_label`, the label immediately **before**
    // `func_info`, and `Function.entry_label`, the label immediately **after** it), but
    // `Export.label == Function.entry_label` (`beam_loader/src/ir.rs`) — and every engine
    // entry point resolves through the export table: `run_export` (this suite's layer(a)/(b)
    // /(c) driver), `resolve_external_call` (the `call_ext*` route) and `apply`. Local
    // `call`/`call_only`/`call_last` targets are likewise the compiler's entry labels. So
    // no real program enters a `func_info` *through the entry route* — which is why the
    // `ExpT`-on-`func_label` count is asserted to be **zero**. `FuncInfo` carries dynamic
    // weight here only because the clause-failure workloads reach a `func_label` through a
    // FAILED head clause (`i_func_info_IaaI`), the one in-machine route to it.
    println!(
        "\nFUNC-INFO REACHABILITY: {} corpus `ExpT` entries; {} of them point at a \
         function's `func_label` (the label BEFORE `func_info`) — the export route can \
         therefore never land on a `FuncInfo`; the clause-failure workloads reach it \
         through a failed head clause instead",
        layer_a.exports, layer_a.exports_on_func_label
    );
    let top10 = layer_a.top(10);
    assert_eq!(
        top10.len(),
        10,
        "the static corpus tally produced only {} distinct variant name(s)",
        top10.len()
    );
    for (name, static_count) in &top10 {
        let executed = combined.executed.get(name).copied().unwrap_or(0);
        println!("TOP-10 STATIC {name:<28} static={static_count:>10} executed={executed:>12}");
        assert!(
            executed > 0,
            "the computed static top-10 variant `{name}` ({static_count} corpus \
             occurrences) executed ZERO times — a shape that dense runs on almost every \
             module, so the workload set has lost it"
        );
    }
    // The `FuncInfo` explanation's own guard: if the loader ever resolved an export onto a
    // `func_label`, the entry route WOULD reach a `func_info` and the explanation above
    // would be wrong.
    assert_eq!(
        layer_a.exports_on_func_label, 0,
        "{} of {} corpus export entries now resolve onto a `func_label`, i.e. onto a \
         `func_info` — the export route DOES reach `FuncInfo` after all, so the \
         FUNC-INFO REACHABILITY explanation must be rewritten",
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
        "IntCodeEnd" => "the `int_code_end` trailer of a module's code (never entered)",
        "BsSkipBits2" | "BsGetBinary2" | "BsGetInteger2" =>
            "a sized binary/integer segment match (`<<B:N/binary>>`, `<<I:N>>`); the \
             committed `bs*` fixtures only read fixed-width and utf segments",
        "BsSkipUtf8" => "no committed fixture emits it, and no exported corpus function \
                         contains one (the corpus `bs_skip_utf8` sites are all in \
                         non-exported helpers)",
        "Test(IsNe)" => "`is_ne` is emitted only inside non-exported helpers — the one \
                         exported corpus function with one (`inet_dns_tsig:sign/3`) is \
                         `kernel`-side, and no fixture guards on `=/=` against a literal",
        "Test(IsBitstr)" => "`is_bitstr` occurs only in non-exported `io_lib`/`io_lib_pretty` \
                             helpers, so no exported entry runs one and no fixture uses it",
        "Test(IsPort)" => "`is_port` occurs only in `win32reg` (a Windows-only stdlib \
                           application) and `gen_sctp` (which needs real sockets)",
        "Test(IsBoolean)" => "the exported `is_boolean` guards live in \
                              `gen:{wait,check,receive}_response/3` (they also need a \
                              `map_size` guard the BIF floor does not serve) and \
                              `application:permit/2` (needs the application controller)",
        "Test(IsReference)" => "the exported `is_reference` guards are in `gen_server`/\
                                `gen_statem` reply-dispatch paths that need a live server \
                                process; `recv:marked/0` compares a reference but never \
                                guards on one",
        "Test(IsFunction)" => "a guard on a fun value; the exported occurrences are in \
                               `gen_server`/`gen_statem` reply dispatch (needs a live \
                               server) and no fixture guards a fun",
        "ApplyLast" => "a tail-position `apply/3`; the corpus emits it mostly in the \
                        `*:format_error/1` error formatters (`xref_base`, `qlc`, \
                        `beam_lib`), which no workload drives",
        "IfEnd" => "the `if_end` trailer of an `if` whose branch actually matches — the \
                    fixtures' `if` shapes either select a branch or fail every guard, and \
                    the failure path re-raises out of `func_info`",
        "RawRaise" => "the `raw_raise` instruction is only emitted for a re-raise that \
                       carries an explicit stacktrace/class triple; the fixtures' \
                       `catchclasses` re-raises go through `erlang:raise/3`'s BIF route",
        "IsAnyNativeRecord" => "the native-record type test (an OTP-29 shape this corpus \
                                contains only statically)",
        "OnLoad" => "the `on_load` stub body of a NIF module (`crypto`, `zstd`, `gl`, \
                     `dyntrace`, …); no NIF library is loaded here, so no `on_load` runs",
        "SetTupleElement" => "an in-place tuple write (the corpus lowers none)",
        "Deallocate" => "the loader fuses every `deallocate` into `DeallocateReturn`",
        "Generic" => "the loader's residual sentinel (the corpus lowers none)",
        _ => "no shape in this named workload set reaches it",
    }
}
