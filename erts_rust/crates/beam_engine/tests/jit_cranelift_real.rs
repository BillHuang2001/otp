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

//! The Cranelift hot tier against the interpreter, driven by **real** OTP modules.
//!
//! `tests/jit_cranelift.rs` pins the tier on purpose-built synthetic modules (so
//! every inlined shape can be probed in isolation). This file is the complementary
//! evidence: the same [`Tier::Cranelift`] backend is run over **real, compiled
//! system stdlib modules**, loaded through the multi-module test host
//! (`tests/support`), which also cross-checks the values against the system `erl`.
//!
//! # What is proven
//!
//! 1. **Tier equivalence on real code.** For every target driven here the
//!    Cranelift tier produces an *identical* outcome to [`Tier::Interpreter`]:
//!    the same term (compared with `beam_term::cmp`, the module's exact `=:=`
//!    ordering) or the *same* typed [`BeamError`] (same raise class and reason,
//!    with structured reason terms compared structurally). The results are
//!    interpreter-identical **by construction** -- everything the tier does not
//!    inline is executed by the shared `jit_step_one` helper
//!    (`beam_engine::jit::baseline::jit_step_one` *is* `Machine::step_instruction`)
//!    -- and this file is the observable check.
//! 2. **Both halves of the cheap-deopt boundary are exercised.**
//!    * The **inline** path is proven to have been *compiled* through the
//!      artifact's read-only codegen report, and to have *run natively* through a
//!      counting [`ExecutionContext`]: a real `lists:sum/1` call over small
//!      integers dispatches **zero** `erlang:'+'/2` calls on the Cranelift tier
//!      (the fast path computed the sum) while the interpreter dispatches one per
//!      element.
//!    * The **deopt** path is proven twice, behaviourally: once by the same
//!      counting host (a float operand makes the small-int guard reject, so the
//!      tier falls back to the helper and the `erlang:'+'/2` calls reappear, with
//!      the identical result), and once by a failed head clause raising the bare
//!      atom `function_clause` -- a reason only the never-inlined `func_info`
//!      instruction produces.
//! 3. **The tier is behind the default-OFF `jit-cranelift` feature.** This whole
//!    file is `cfg`-ed out without it, and with it on the roster
//!    ([`available_tiers`]) lists `"jit-cranelift"` after the interpreter and the
//!    baseline tier, and `Tier::Cranelift` is available and runs. (The feature-OFF
//!    half -- `Tier::Cranelift` is the typed `unsupported("jit-cranelift tier not
//!    built")` error, never a panic and never a silent fallback -- cannot be
//!    asserted from a file that does not exist without the feature; it is pinned by
//!    `src/jit/mod.rs`'s `cranelift_tier_is_a_typed_error_not_a_silent_fallback`
//!    unit test, and by `cargo tree -p beam_engine` listing no `cranelift-*` crate.)
//! 4. **The whole committed corpus, not a sample of it.** The two sweeps drive
//!    **every** row of the same two corpora `tests/jit_real_module.rs` drives on the
//!    interpreter + baseline pair -- all **504** breadth calls over **171** distinct
//!    installed modules and all **232** error-path calls over **215** modules -- so
//!    the three tiers are measured on exactly the same evidence, with no bounded
//!    prefix and no per-module cap. See
//!    [`breadth_corpus_agrees_between_the_interpreter_and_cranelift`] and
//!    [`error_path_corpus_agrees_and_exercises_the_deopt_path`].
//! 5. **Oracle fidelity per tier, not only tier agreement.** Every corpus row's baked
//!    expectation is the system `erl`'s own answer, and *each* tier is checked
//!    against it -- so a value both tiers compute identically but **wrongly** is now
//!    a failure, and so is a raise whose reason the system `erl` never produces. The
//!    corpus parsing, the argument/expectation builders, the `~w` renderer and the
//!    error comparator are **not** re-implemented here: they are the one shared
//!    implementation in `tests/support/oracle.rs`
//!    (`parse_breadth_target`/`breadth_args`/`parse_expected_value`/
//!    `Expected::matches`/`render_term`, `parse_error_targets`/`verify_error`).
//!
//! # Termination
//!
//! Nothing here runs an unbounded loop. The curated targets are fixed calls with
//! representative small arguments whose values were taken verbatim from the system
//! `erl`, and the two sweeps iterate the committed, oracle-checked corpora
//! (`tests/fixtures/stdlib_breadth_targets.txt` and
//! `tests/fixtures/error_path_targets.txt`) line for line. Every corpus line was
//! admitted only after a real `erl` run **terminated** on it (a value or an
//! exception), which is the termination argument for the sweep: the sweep is exactly
//! as bounded as the corpus is.
//!
//! The module cache compiles each real module **once** and reuses the artifact (the
//! tier's compile-once / run-many [`JitBackend::run_compiled`] entry), so the cost
//! of Cranelift codegen is paid at most once per swept module -- 171 compiles for the
//! whole 504-call breadth corpus.
//!
//! # What this suite measures today
//!
//! Measured on this tree with the feature on
//! (`cargo test -p beam_engine --features jit-cranelift --test jit_cranelift_real --
//! --nocapture`), the whole file in ~84 s (the tests are serialized -- see
//! [`JIT_SERIAL`] and the note there on `cranelift-jit`'s per-allocation `mmap`):
//!
//! ```text
//! BREADTH: selected 504 case(s); compared 504 over 171 module(s); oracle-checked
//!          interpreter=504 cranelift=504; blocked=0 skipped=0 (504 agreed on a value)
//! ERROR-PATHS: selected 232 case(s); compared 232 over 215 module(s), 207 bare-atom
//!          `function_clause` agreement(s) (the deopted `func_info` path);
//!          oracle-checked interpreter=223 cranelift=223; blocked=9 skipped=0
//! ```
//!
//! So every one of the 736 corpus rows agrees between the two tiers, and **727**
//! (504 + 223) are oracle-checked on *each* tier. The other **9** error-path rows are
//! the fixture's own `unsupported` expectation -- a documented **host** gap
//! (`receive_marker_reserve`/`receive_next`, or a blocking `receive`), not a tier
//! divergence: both tiers answer the identical typed error, and the row is printed as
//! a `BLOCKED:` line and counted. **No row disagrees with the oracle.** The per-corpus
//! floors below are the guard.
#![cfg(feature = "jit-cranelift")]

mod support;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use beam_common::{BeamError, ExceptionClass, Raise};
use beam_engine::jit::cranelift::{CompiledFunctions, CraneliftJit, SmallArith};
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, terms, ExecutionContext, Machine, Tier};
use beam_loader::Module;
use beam_term::{cmp, make_atom, make_small, AtomTable, Eterm};
use support::{
    all_otp_ebins, assert_atom, assert_int_list, assert_keyed_list, assert_small, breadth_args,
    describe_error, int_list, keyed_tuple, parse_breadth_target, parse_error_targets,
    parse_expected_value, render_term, skip_reason, stdlib_ebin, term_list, verify_error,
    ErrorVerdict, ModuleHost,
};

/// The oracle-checked call corpus (module/function/args/expected-value; the same data
/// `tests/stdlib_lists.rs` drives on the interpreter and `tests/jit_real_module.rs` on
/// the interpreter + baseline pair).
const BREADTH_TXT: &str = include_str!("fixtures/stdlib_breadth_targets.txt");

/// The oracle-checked error-path corpus (poisoned calls that must raise).
const ERROR_TXT: &str = include_str!("fixtures/error_path_targets.txt");

/// How many breadth corpus rows the sweep must actually compare (the corpus holds
/// 504), so a degenerate corpus read fails the suite instead of silently weakening it.
const MIN_BREADTH_CASES: usize = 480;

/// How many breadth rows **each tier** must reproduce the baked oracle value for
/// (504 today) -- a floor on the oracle-checked count per tier, so a regression that
/// stops checking can never pass.
const MIN_BREADTH_ORACLE_CHECKS: usize = 480;

/// How many distinct breadth corpus modules the sweep must reach (the corpus spans
/// 171, and so must the sweep).
const MIN_BREADTH_MODULES: usize = 171;

/// How many error-path corpus rows the sweep must actually compare (the corpus holds
/// 232).
const MIN_ERROR_CASES: usize = 220;

/// How many error-path rows **each tier** must reproduce the oracle's exception class
/// and reason for (223 today) -- the per-tier floor on oracle-checked raises.
const MIN_ERROR_ORACLE_CHECKS: usize = 200;

/// How many distinct error-path corpus modules the sweep must reach (the corpus spans
/// 215).
const MIN_ERROR_MODULES: usize = 200;

/// The smallest number of error-path cases that must raise `function_clause` on
/// **both** tiers -- the behavioural proof that the never-inlined `func_info` step
/// was executed by the shared helper on the Cranelift tier.
const MIN_FUNCTION_CLAUSE_AGREEMENTS: usize = 200;

// ---------------------------------------------------------------------------
// The real-module host
// ---------------------------------------------------------------------------

/// The process-wide serialization of this file's tests.
///
/// **Why a lock, and why it changes no assertion.** `cranelift-jit` 0.116 allocates
/// **one `mmap` per allocation** in its code and writable memories
/// (`cranelift_jit::memory::Memory::allocate`, whose own comment reads
/// `// TODO: Allocate more at a time.`), and it emits a 16-byte PLT stub for every
/// declared function whose `rel32` displacement must reach that function's 8-byte GOT
/// slot (`JITModule::declare_function` -> `new_func_plt_entry` ->
/// `write_plt_entry_bytes`). One corpus is a few hundred modules and some 10^5
/// declared functions, i.e. some 10^5 single-page mappings; two corpora's modules
/// **alive at the same time** walk the address space far enough that the PLT entry and
/// its GOT slot drift more than 2 GiB apart, and the dependency then panics with
/// `TryFromIntError(NegOverflow)` from `i32::try_from(what - at).unwrap()` -- an
/// address-space limitation of the dependency, never a tier divergence (the
/// interpreter half of the same row had already completed).
///
/// Taking this lock makes at most **one** corpus's JIT modules live at a time, which
/// is deterministic and is exactly what `--test-threads=1` did (measured: 2/2 green
/// runs serial, ~50 % panics with only the two sweeps in parallel). The floors, the
/// comparisons and the skip policy are untouched.
static JIT_SERIAL: Mutex<()> = Mutex::new(());

/// Serializes the calling test with the rest of this file. A poisoned lock (an
/// earlier test in the same binary already failed) is taken over, so the remaining
/// tests still run and report their own verdicts instead of dying on the poison.
fn serialize_jit() -> MutexGuard<'static, ()> {
    JIT_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Skips the calling test (loudly) when the oracle OTP install is unavailable.
fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

/// A host over the system stdlib `ebin` whose search path is **every** installed
/// OTP application, so a call into any swept module resolves by name.
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut host = ModuleHost::new(ebin);
    if let Some(apps) = all_otp_ebins() {
        for app in apps {
            host.add_search_dir(app.dir);
        }
    }
    host
}

// ---------------------------------------------------------------------------
// The tier comparison helpers
// ---------------------------------------------------------------------------

/// A loaded module together with its compiled Cranelift artifact (compiled once,
/// run many times -- the tier's [`JitBackend::run_compiled`] contract).
struct CompiledModule {
    /// The loaded IR the artifact was compiled from; the *same* instance must be
    /// used for the run (the compiled code bakes the module's instruction widths).
    module: Arc<Module>,
    /// The caller-owned native artifact.
    compiled: CompiledFunctions,
}

/// The per-test compile cache, keyed by module name.
type Cache = HashMap<String, CompiledModule>;

/// Make sure `module_name` is in `cache`, compiling it with the Cranelift backend
/// on first use. A Cranelift codegen failure is the engine's documented typed
/// error, never a panic.
fn ensure_compiled(
    host: &mut ModuleHost,
    cache: &mut Cache,
    module_name: &str,
) -> Result<(), BeamError> {
    if cache.contains_key(module_name) {
        return Ok(());
    }
    let module = host.load(module_name)?;
    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(&module)?;
    cache.insert(
        module_name.to_string(),
        CompiledModule { module, compiled },
    );
    Ok(())
}

/// Run `module:function/args.len()` on the Cranelift tier through the cached
/// artifact, keeping the run's [`Machine`] alive in `keep`.
///
/// The machine owns the off-heap ref-counted binary payloads its run may have
/// built (`Machine::alloc_refc_binary`); a returned term can point at one, so
/// dropping the machine before the interpreter's own result has been compared
/// would free a payload still in use. `keep` is the test-host analogue of a
/// process heap outliving a function return.
fn run_cranelift(
    host: &mut ModuleHost,
    cache: &mut Cache,
    keep: &mut Vec<Machine>,
    module_name: &str,
    function: &str,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    ensure_compiled(host, cache, module_name)?;
    let entry = cache.get(module_name).expect("compiled just above");
    let function_atom = entry
        .module
        .atom_table
        .lookup(function)
        .ok_or(BeamError::BadArg)?;
    let mut machine = Machine::new();
    // The artifact is `&self`-run (it is caller-owned and reusable), so the backend
    // is a cheap zero-state wrapper recreated per run.
    let backend = CraneliftJit::new();
    let result = backend.run_compiled(
        &entry.compiled,
        &mut machine,
        &entry.module,
        function_atom,
        args.len() as u32,
        args,
        host,
    );
    keep.push(machine);
    result
}

/// The live reason term a raise carries, if any (the opaque `Raise::live_reason`
/// slot's `Eterm` payload).
fn live_reason_term(raise: &Raise) -> Option<Eterm> {
    raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
}

/// Whether two typed errors are *the same* error: the same raise class and reason
/// (a structured reason's live term compared structurally), or the same non-raise
/// variant.
fn errors_agree(atoms: &AtomTable, a: &BeamError, b: &BeamError) -> bool {
    match (a, b) {
        (BeamError::Raise(raise_a), BeamError::Raise(raise_b)) => {
            raise_a.class == raise_b.class
                && raise_a.reason == raise_b.reason
                && match (live_reason_term(raise_a), live_reason_term(raise_b)) {
                    (Some(term_a), Some(term_b)) => cmp(term_a, term_b, atoms).is_eq(),
                    (None, None) => true,
                    // One side carries a live reason term and the other does not: the
                    // same raise was not produced.
                    _ => false,
                }
        }
        // Non-raise variants (`BadArg`/`BadArith`/`BadArity`/`BadMatch`/
        // `SystemLimit`/`Alloc`/`TermNotFound`/`Io`): the `Debug` rendering is a
        // faithful variant comparison.
        _ => format!("{a:?}") == format!("{b:?}"),
    }
}

/// Whether the two tiers produced the *same* outcome (the same term, or the same
/// typed error).
fn outcomes_agree(
    atoms: &AtomTable,
    a: &Result<Eterm, BeamError>,
    b: &Result<Eterm, BeamError>,
) -> bool {
    match (a, b) {
        (Ok(term_a), Ok(term_b)) => cmp(*term_a, *term_b, atoms).is_eq(),
        (Err(err_a), Err(err_b)) => errors_agree(atoms, err_a, err_b),
        _ => false,
    }
}

/// Renders an outcome for a failure message.
fn describe(outcome: &Result<Eterm, BeamError>) -> String {
    match outcome {
        Ok(value) => format!("Ok({value:?})"),
        Err(error) => format!("Err({error:?})"),
    }
}

/// Run `module:function/args.len()` on **both** tiers and assert they agree;
/// returns the interpreter's outcome.
fn check_agree(
    host: &mut ModuleHost,
    cache: &mut Cache,
    keep: &mut Vec<Machine>,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    let interpreted = host.call(module, function, args);
    let cranelift = run_cranelift(host, cache, keep, module, function, args);
    assert!(
        outcomes_agree(host.atom_table(), &interpreted, &cranelift),
        "tier divergence on {module}:{function}/{}: interpreter {} vs cranelift {}",
        args.len(),
        describe(&interpreted),
        describe(&cranelift)
    );
    interpreted
}

/// The interpreter's outcome as a value (panics on a typed error -- used only for
/// curated targets that must succeed).
fn ok(outcome: Result<Eterm, BeamError>, label: &str) -> Eterm {
    outcome.unwrap_or_else(|error| panic!("{label} returned a typed error: {error:?}"))
}

// ---------------------------------------------------------------------------
// 1. The tier roster
// ---------------------------------------------------------------------------

#[test]
fn the_feature_builds_the_cranelift_tier_into_the_roster() {
    // With the feature on the roster is exactly interpreter, baseline, cranelift.
    // The roster itself is the engine's own static data, so this half needs no OTP
    // install at all.
    assert_eq!(
        available_tiers(),
        &["interpreter", "baseline-jit", "jit-cranelift"]
    );
    assert!(available_tiers().contains(&"jit-cranelift"));
    assert_eq!(Tier::from_name("jit-cranelift"), Some(Tier::Cranelift));
    assert!(Tier::Cranelift.is_available());

    // Selecting the tier runs Cranelift (it does not return the feature-OFF typed
    // error and does not fall back to another tier); this is the feature-ON half of
    // the "no silent fallback" claim. It needs the real `lists` module, so it is
    // guarded like every other test in this file.
    if skip_if_unavailable() {
        return;
    }
    let _guard = serialize_jit();
    let mut host = host();
    let mut cache = Cache::new();
    let mut keep = Vec::new();
    let a = int_list(&mut host, &[1, 2, 3, 4]);
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "sum", &[a]),
        "lists:sum/1",
    );
    assert_small(&host, value, 10);
}

// ---------------------------------------------------------------------------
// 2. Curated real stdlib targets (values are the system `erl`'s)
// ---------------------------------------------------------------------------

/// A curated set of **real stdlib** calls spanning the tier's inlined cheap set
/// (`+`/`-`/comparisons/`test`/`select_val`) and the deopt path (external and local
/// calls, list and tuple reads, record/case machinery). Every expected value is the
/// verbatim result the system `erl` (OTP 29 / erts-17.1) prints, e.g.
///
/// ```text
/// $ erl -noshell -eval 'io:format("~w~n",[lists:seq(1,5)]),halt().'
/// [1,2,3,4,5]
/// ```
///
/// and each is asserted **twice** over: the interpreter must produce it, and the
/// Cranelift tier must agree with the interpreter.
#[test]
fn curated_real_stdlib_targets_agree_and_match_the_oracle() {
    if skip_if_unavailable() {
        return;
    }
    let _guard = serialize_jit();
    let mut host = host();
    let mut cache = Cache::new();
    let mut keep = Vec::new();
    let mut compared = 0usize;

    // -- `lists`: recursion + `+`/`-` + `<`/`>` comparisons + list reads --------
    let args = [int_list(&mut host, &[1, 2, 3, 4])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "sum", &args),
        "lists:sum/1",
    );
    assert_small(&host, value, 10); // oracle: lists:sum([1,2,3,4]) = 10
    compared += 1;

    let args = [make_small(1), make_small(5)];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "seq", &args),
        "lists:seq/2",
    );
    assert_int_list(&host, value, &[1, 2, 3, 4, 5]); // oracle: [1,2,3,4,5]
    compared += 1;

    let args = [make_small(1), make_small(10), make_small(2)];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "seq", &args),
        "lists:seq/3",
    );
    assert_int_list(&host, value, &[1, 3, 5, 7, 9]); // oracle: [1,3,5,7,9]
    compared += 1;

    let args = [int_list(&mut host, &[3, 1, 2])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "sort", &args),
        "lists:sort/1",
    );
    assert_int_list(&host, value, &[1, 2, 3]); // oracle: [1,2,3]
    compared += 1;

    let args = [int_list(&mut host, &[3, 1, 2, 1])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "usort", &args),
        "lists:usort/1",
    );
    assert_int_list(&host, value, &[1, 2, 3]); // oracle: [1,2,3]
    compared += 1;

    let args = [int_list(&mut host, &[3, 1, 2])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "max", &args),
        "lists:max/1",
    );
    assert_small(&host, value, 3); // oracle: 3
    compared += 1;

    let args = [int_list(&mut host, &[3, 1, 2])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "min", &args),
        "lists:min/1",
    );
    assert_small(&host, value, 1); // oracle: 1
    compared += 1;

    let args = [make_small(3), make_small(7)];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "duplicate", &args),
        "lists:duplicate/2",
    );
    assert_int_list(&host, value, &[7, 7, 7]); // oracle: [7,7,7]
    compared += 1;

    let args = [make_small(2), int_list(&mut host, &[10, 20, 30])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "nth", &args),
        "lists:nth/2",
    );
    assert_small(&host, value, 20); // oracle: 20
    compared += 1;

    let args = [make_small(2), int_list(&mut host, &[1, 2, 3, 4])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "nthtail", &args),
        "lists:nthtail/2",
    );
    assert_int_list(&host, value, &[3, 4]); // oracle: [3,4]
    compared += 1;

    let args = [int_list(&mut host, &[1, 2, 3])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "reverse", &args),
        "lists:reverse/1",
    );
    assert_int_list(&host, value, &[3, 2, 1]); // oracle: [3,2,1]
    compared += 1;

    // -- `lists`: a non-NIF pure-Erlang target --------------------------------
    let args = [int_list(&mut host, &[1, 2, 3, 4]), make_small(2)];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "sublist", &args),
        "lists:sublist/2",
    );
    assert_int_list(&host, value, &[1, 2]); // oracle: lists:sublist([1,2,3,4],2) = [1,2]
    compared += 1;

    let args = [int_list(&mut host, &[1, 2, 3])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "lists", "last", &args),
        "lists:last/1",
    );
    assert_small(&host, value, 3); // oracle: lists:last([1,2,3]) = 3
    compared += 1;

    // (The NIF-stub targets `lists:member/2` and `lists:keyfind/3` are driven
    // further down, once the `proplists` tuple list they need exists.)

    // -- `ordsets` (pure Erlang over `lists:usort`) ----------------------------
    let args = [int_list(&mut host, &[3, 1, 2, 1])];    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "ordsets", "from_list", &args),
        "ordsets:from_list/1",
    );
    assert_int_list(&host, value, &[1, 2, 3]); // oracle: [1,2,3]
    compared += 1;

    let args = [int_list(&mut host, &[1, 3]), int_list(&mut host, &[2, 3])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "ordsets", "union", &args),
        "ordsets:union/2",
    );
    assert_int_list(&host, value, &[1, 2, 3]); // oracle: [1,2,3]
    compared += 1;

    let args = [int_list(&mut host, &[1, 2, 3]), int_list(&mut host, &[2, 3, 4])];
    let value = ok(
        check_agree(
            &mut host,
            &mut cache,
            &mut keep,
            "ordsets",
            "intersection",
            &args,
        ),
        "ordsets:intersection/2",
    );
    assert_int_list(&host, value, &[2, 3]); // oracle: [2,3]
    compared += 1;

    let args = [int_list(&mut host, &[1, 2, 3]), int_list(&mut host, &[2])];
    let value = ok(
        check_agree(&mut host, &mut cache, &mut keep, "ordsets", "subtract", &args),
        "ordsets:subtract/2",
    );
    assert_int_list(&host, value, &[1, 3]); // oracle: [1,3]
    compared += 1;

    let args = [make_small(2), int_list(&mut host, &[1, 2, 3])];
    let value = ok(
        check_agree(
            &mut host,
            &mut cache,
            &mut keep,
            "ordsets",
            "is_element",
            &args,
        ),
        "ordsets:is_element/2",
    );
    assert_atom(&host, value, "true"); // oracle: true
    compared += 1;

    let args = [make_small(7), int_list(&mut host, &[1, 2, 3])];
    let value = ok(
        check_agree(
            &mut host,
            &mut cache,
            &mut keep,
            "ordsets",
            "is_element",
            &args,
        ),
        "ordsets:is_element/2",
    );
    assert_atom(&host, value, "false"); // oracle: false
    compared += 1;

    // -- `proplists`: tuple reads + exact key comparison ----------------------
    let key_a = host.intern("a");
    let key_c = host.intern("c");
    let k1 = keyed_tuple(&mut host, "a", 1);
    let k2 = keyed_tuple(&mut host, "b", 2);
    let props = term_list(&mut host, &[k1, k2]);

    let value = ok(
        check_agree(
            &mut host,
            &mut cache,
            &mut keep,
            "proplists",
            "get_value",
            &[key_a, props],
        ),
        "proplists:get_value/2",
    );
    assert_small(&host, value, 1); // oracle: proplists:get_value(a,[{a,1},{b,2}]) = 1
    compared += 1;

    let value = ok(
        check_agree(
            &mut host,
            &mut cache,
            &mut keep,
            "proplists",
            "get_value",
            &[key_c, props, make_small(9)],
        ),
        "proplists:get_value/3",
    );
    assert_small(&host, value, 9); // oracle: proplists:get_value(c,[{a,1},{b,2}],9) = 9
    compared += 1;

    // -- `lists:member/2` / `lists:keyfind/3` are NIF stubs --------------------
    // In OTP 29 both are `erlang:nif_error(undef)` stubs (verified with
    // `beam_lib:chunks(..., [abstract_code])`), so no tier can execute their
    // bodies: the point of driving them here is that both tiers report the *same*
    // typed error (`BadArg`) rather than one of them inventing a value.
    let key_b = host.intern("b");
    let args = [make_small(20), int_list(&mut host, &[10, 20, 30])];
    let outcome = check_agree(&mut host, &mut cache, &mut keep, "lists", "member", &args);
    assert!(
        outcome.is_err(),
        "the NIF-stub `lists:member/2` unexpectedly produced a value: {}",
        describe(&outcome)
    );
    compared += 1;

    let args = [key_b, make_small(1), props];
    let outcome = check_agree(&mut host, &mut cache, &mut keep, "lists", "keyfind", &args);
    assert!(
        outcome.is_err(),
        "the NIF-stub `lists:keyfind/3` unexpectedly produced a value: {}",
        describe(&outcome)
    );
    compared += 1;

    // -- `calendar`: record/tuple shapes + `div`/`rem`/`+` arithmetic ----------
    let date = terms::make_tuple(&mut host, &[make_small(1970), make_small(1), make_small(1)])
        .expect("build the {1970,1,1} date tuple");    let value = ok(
        check_agree(
            &mut host,
            &mut cache,
            &mut keep,
            "calendar",
            "date_to_gregorian_days",
            &[date],
        ),
        "calendar:date_to_gregorian_days/1",
    );
    assert_small(&host, value, 719_528); // oracle: 719528
    compared += 1;

    let date = terms::make_tuple(&mut host, &[make_small(2024), make_small(1), make_small(1)])
        .expect("build the {2024,1,1} date tuple");
    let value = ok(
        check_agree(
            &mut host,
            &mut cache,
            &mut keep,
            "calendar",
            "day_of_the_week",
            &[date],
        ),
        "calendar:day_of_the_week/1",
    );
    assert_small(&host, value, 1); // oracle: 1
    compared += 1;

    // -- `proplists:lookup_all/2`: a keyed-tuple list result -------------------
    let k3 = keyed_tuple(&mut host, "a", 3);
    let props_with_duplicate = term_list(&mut host, &[k1, k2, k3]);
    let value = ok(
        check_agree(
            &mut host,
            &mut cache,
            &mut keep,
            "proplists",
            "lookup_all",
            &[key_a, props_with_duplicate],
        ),
        "proplists:lookup_all/2",
    );
    // oracle: proplists:lookup_all(a,[{a,1},{b,2},{a,3}]) = [{a,1},{a,3}]
    assert_keyed_list(&host, value, &[("a", 1), ("a", 3)]);
    compared += 1;    println!(
        "CURATED: {compared} real stdlib target(s), all agreeing on both tiers and \
         matching the system `erl` oracle"
    );
    assert!(compared >= 20, "the curated set shrank: {compared} < 20");
}

// ---------------------------------------------------------------------------
// 3. The behavioural inline / deopt probe (a counting BIF seam)
// ---------------------------------------------------------------------------

/// A [`ModuleHost`] wrapper that counts the `erlang:'+'/2` BIF dispatches the
/// engine makes through the BIF seam, delegating everything else unchanged.
///
/// This is what makes "the fast path computed this" and "the guard rejected this
/// and the helper computed it" *observable* rather than inferred: an inlined
/// `gc_bif` writes its result without ever calling
/// [`ExecutionContext::call_bif`](beam_engine::ExecutionContext::call_bif).
struct CountingHost<'a> {
    inner: &'a mut ModuleHost,
    /// The number of `erlang:'+'/2` dispatches seen.
    plus_calls: usize,
}

impl ExecutionContext for CountingHost<'_> {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm, BeamError> {
        if module == "erlang" && function == "+" && arity == 2 {
            self.plus_calls += 1;
        }
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<(), BeamError> {
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm, BeamError> {
        self.inner.alloc_words(n)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm, BeamError> {
        self.inner.apply_fun(fun, args)
    }
}

#[test]
fn the_small_int_fast_path_runs_natively_and_its_guard_deopts() {
    if skip_if_unavailable() {
        return;
    }
    let _guard = serialize_jit();
    let mut host = host();
    let module = host
        .load("lists")
        .expect("load the system `lists` module (guarded by `skip_if_unavailable`)");
    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(&module).expect("compile `lists`");
    let sum = module
        .atom_table
        .lookup("sum")
        .expect("`lists:sum/1` is interned");

    // The fast path must be *in* the artifact; otherwise "zero BIF calls" would be
    // meaningless (everything would have deopted).
    assert!(
        compiled.inlined_gc_bif_ops().contains(&SmallArith::Add),
        "the small-int `erlang:'+'/2` fast path was not compiled into `lists`"
    );

    // (a) All-small operands: the fast path computes the sum natively.
    let smalls = int_list(&mut host, &[1, 2, 3, 4]);
    let mut interpreter_keep = Machine::new();
    let mut counting = CountingHost {
        inner: &mut host,
        plus_calls: 0,
    };
    let interpreted = interpreter_keep
        .run_export(&module, sum, 1, &[smalls], &mut counting)
        .expect("lists:sum([1,2,3,4]) runs on the interpreter");
    let interpreter_calls = counting.plus_calls;
    drop(counting);
    assert_small(&host, interpreted, 10);
    assert!(
        interpreter_calls >= 1,
        "the interpreter must dispatch `erlang:'+'/2` through the BIF seam (got {interpreter_calls})"
    );

    let mut native_keep = Machine::new();
    let mut counting = CountingHost {
        inner: &mut host,
        plus_calls: 0,
    };
    let native = backend
        .run_compiled(
            &compiled,
            &mut native_keep,
            &module,
            sum,
            1,
            &[smalls],
            &mut counting,
        )
        .expect("lists:sum([1,2,3,4]) runs on the Cranelift tier");
    let native_calls = counting.plus_calls;
    drop(counting);
    assert_small(&host, native, 10);
    assert_eq!(
        native_calls, 0,
        "the Cranelift tier must compute small-int `+` natively (0 BIF calls), \
         but dispatched `erlang:'+'/2` {native_calls} time(s)"
    );

    // (b) A float operand: the run-time guard rejects, so the tier deopts to the
    // shared helper -- which dispatches the BIF and produces the identical result.
    let three_point_zero = terms::make_float(&mut host, 3.0).expect("build the 3.0 float");
    let mixed = terms::make_list(&mut host, &[make_small(1), make_small(2), three_point_zero])
        .expect("build the [1,2,3.0] list");

    let mut counting = CountingHost {
        inner: &mut host,
        plus_calls: 0,
    };
    let mut deopt_keep = Machine::new();
    let deopted = backend
        .run_compiled(
            &compiled,
            &mut deopt_keep,
            &module,
            sum,
            1,
            &[mixed],
            &mut counting,
        )
        .expect("lists:sum([1,2,3.0]) runs on the Cranelift tier");
    let deopt_calls = counting.plus_calls;
    drop(counting);
    assert!(
        deopt_calls >= 1,
        "a float operand must reject the small-int guard and deopt to the helper, \
         but no `erlang:'+'/2` dispatch was seen"
    );
    assert_eq!(
        terms::float_value(deopted).expect("a boxed float result"),
        6.0,
        "the deopted helper must compute the oracle's 6.0"
    );

    // The interpreter produces the same 6.0 through its own (always-deopted) path.
    let mut interpreter_keep = Machine::new();
    let mut counting = CountingHost {
        inner: &mut host,
        plus_calls: 0,
    };
    let interpreted = interpreter_keep
        .run_export(&module, sum, 1, &[mixed], &mut counting)
        .expect("lists:sum([1,2,3.0]) runs on the interpreter");
    drop(counting);
    assert_eq!(
        terms::float_value(interpreted).expect("a boxed float result"),
        6.0
    );

    println!(
        "PROBE lists:sum/1: small-int `+` interpret={interpreter_calls} BIF call(s) vs \
         cranelift={native_calls}; float operand cranelift={deopt_calls} (deopt)"
    );
}

// ---------------------------------------------------------------------------
// 4. The full breadth sweep over the oracle-checked call corpus
// ---------------------------------------------------------------------------

/// The quoted `erl -noshell -eval …` command a breadth-corpus line carries as its
/// trailing `# oracle:` comment, so a mismatch is reproducible from the failure
/// message alone.
///
/// [`parse_breadth_target`] drops the comment, so this reads it back out of the raw
/// line; the target grammar itself is **not** re-implemented here (there is one
/// parser, in `tests/support/oracle.rs`).
fn oracle_command(raw: &str) -> &str {
    raw.split("# oracle:").nth(1).map(str::trim).unwrap_or("")
}

/// The union of the inlined-op categories the real compiled modules report, so a
/// sweep can prove the Cranelift tier genuinely took the inline path (agreeing
/// results alone would also hold if every instruction had deopted).
#[derive(Default)]
struct InlinedReport {
    test_kinds: BTreeSet<String>,
    gc_bifs: BTreeSet<String>,
    comparisons: BTreeSet<String>,
    tuple_reads: BTreeSet<String>,
    list_reads: BTreeSet<String>,
    select_vals: usize,
    modules: usize,
}

/// Folds one compiled real module's read-only codegen report into `into`.
///
/// Called once per **newly compiled** module (never once per corpus row), so
/// [`InlinedReport::modules`] is a distinct-module count rather than a row count.
fn record_inlined(compiled: &CompiledFunctions, into: &mut InlinedReport) {
    into.modules += 1;
    for kind in compiled.inlined_test_kinds() {
        into.test_kinds.insert(format!("{kind:?}"));
    }
    for op in compiled.inlined_gc_bif_ops() {
        into.gc_bifs.insert(op.name().to_string());
    }
    for op in compiled.inlined_comparison_ops() {
        into.comparisons.insert((*op).to_string());
    }
    for kind in compiled.inlined_tuple_read_kinds() {
        into.tuple_reads.insert((*kind).to_string());
    }
    for kind in compiled.inlined_list_read_kinds() {
        into.list_reads.insert((*kind).to_string());
    }
    into.select_vals += compiled.inlined_select_val_count();
}

/// **The headline measurement: every happy-path corpus call, on both tiers, checked
/// against the real system `erl`.**
///
/// Iterates the **whole** committed corpus -- all 504 calls over 171 distinct
/// installed modules, the same rows `tests/jit_real_module.rs` drives on the
/// interpreter + baseline pair -- with no bounded prefix and no per-module cap. Each
/// row makes **two** assertions:
///
/// 1. **The two tiers agree with each other** ([`outcomes_agree`]): the same term
///    under `beam_term::cmp`, or the same typed [`BeamError`]. A typed error on one
///    tier and a value on the other fails.
/// 2. **Each tier reproduces the oracle's value.** The corpus's `expected` column is
///    the verbatim `~w` rendering the system `erl` printed for that very call, parsed
///    back into a term and compared with `beam_term::cmp` -- on
///    [`Tier::Interpreter`] **and** on [`Tier::Cranelift`]. Tier agreement alone can
///    therefore no longer hide a mistake both tiers share.
///
/// A row whose call the engine cannot execute at all on **either** tier (a typed
/// gap, identical on both since they agree) is printed as a loud `BLOCKED:` line and
/// counted -- never silently folded into an agreement. A module Cranelift cannot
/// compile is a loud `SKIP:` line, counted the same way.
#[test]
fn breadth_corpus_agrees_between_the_interpreter_and_cranelift() {
    if skip_if_unavailable() {
        return;
    }
    let _guard = serialize_jit();
    let mut host = host();
    let mut cache = Cache::new();
    let mut keep = Vec::new();
    let mut selected = 0usize;
    let mut compared = 0usize;
    let mut values = 0usize;
    let mut checked = [0usize; 2];
    let mut modules: BTreeSet<String> = BTreeSet::new();
    let mut blocked: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut skips: Vec<String> = Vec::new();
    let mut inlined = InlinedReport::default();

    for raw in BREADTH_TXT.lines() {
        let Some(target) = parse_breadth_target(raw) else {
            continue;
        };
        selected += 1;
        let args = breadth_args(&mut host, target.args);
        let expected = parse_expected_value(&mut host, target.expected);
        let label = target.label();

        // A module Cranelift cannot compile is skipped LOUDLY (a codegen failure is
        // the tier's documented typed error, not a wrong result); every other
        // divergence fails.
        let newly_compiled = !cache.contains_key(target.module);
        if let Err(error) = ensure_compiled(&mut host, &mut cache, target.module) {
            skips.push(format!("SKIP: {label} -- cranelift codegen: {error:?}"));
            continue;
        }
        if newly_compiled {
            if let Some(compiled) = cache.get(target.module) {
                record_inlined(&compiled.compiled, &mut inlined);
            }
        }

        let interpreted = host.call(target.module, target.function, &args);
        let cranelift = run_cranelift(
            &mut host,
            &mut cache,
            &mut keep,
            target.module,
            target.function,
            &args,
        );
        compared += 1;

        // (a) the two tiers must agree with each other ...
        if !outcomes_agree(host.atom_table(), &interpreted, &cranelift) {
            failures.push(format!(
                "DIVERGENCE {label} on args ({}) -- interpreter {} vs cranelift {}",
                target.args,
                describe(&interpreted),
                describe(&cranelift)
            ));
            continue;
        }
        modules.insert(target.module.to_string());

        // ... and (b) EACH tier must reproduce the oracle's value.
        match (&interpreted, &cranelift) {
            (Ok(interpreted), Ok(cranelift)) => {
                values += 1;
                for (slot, tier_name, actual) in [
                    (0usize, "Tier::Interpreter", *interpreted),
                    (1usize, "Tier::Cranelift", *cranelift),
                ] {
                    if expected.matches(&host, actual) {
                        checked[slot] += 1;
                    } else {
                        failures.push(format!(
                            "ORACLE MISMATCH for `{label}` on {tier_name}: the engine returns {}, \
                             the system erl returns {}  [{}]",
                            render_term(&host, actual).unwrap_or_else(|| format!("{actual:?}")),
                            expected
                                .render(&host)
                                .unwrap_or_else(|| "<unrenderable>".to_string()),
                            oracle_command(raw),
                        ));
                    }
                }
            }
            (Err(error), _) | (_, Err(error)) => blocked.push(format!(
                "BLOCKED: {label} -- the oracle returns a value, both tiers answered {}",
                describe_error(error)
            )),
        }
    }

    println!(
        "BREADTH: selected {selected} case(s); compared {compared} over {} module(s); \
         oracle-checked interpreter={} cranelift={}; blocked={} skipped={} ({values} agreed on a \
         value)",
        modules.len(),
        checked[0],
        checked[1],
        blocked.len(),
        skips.len()
    );
    for line in &skips {
        println!("{line}");
    }
    for line in &blocked {
        println!("{line}");
    }
    println!(
        "INLINED ({} compiled real module(s)): test_kinds={:?} gc_bifs={:?} comparisons={:?} \
         select_vals={} tuple_reads={:?} list_reads={:?}",
        inlined.modules,
        inlined.test_kinds,
        inlined.gc_bifs,
        inlined.comparisons,
        inlined.select_vals,
        inlined.tuple_reads,
        inlined.list_reads,
    );

    // The Cranelift tier genuinely *ran* on the swept real modules: this is what
    // rules out "the two tiers agreed because everything deopted to the helper".
    assert!(
        inlined.modules >= MIN_BREADTH_MODULES,
        "only {} module(s) reported a codegen artifact (floor {MIN_BREADTH_MODULES})",
        inlined.modules
    );
    assert!(
        !inlined.gc_bifs.is_empty(),
        "no swept real module inlined a small-int `+`/`-`/`*` fast path"
    );
    assert!(
        !inlined.test_kinds.is_empty(),
        "no swept real module inlined a `test` kind"
    );

    assert!(
        failures.is_empty(),
        "{} breadth case(s) diverge between the interpreter and the Cranelift tier, or disagree \
         with the system `erl`:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        blocked.is_empty(),
        "{} breadth corpus row(s) became a typed gap on both tiers (see the `BLOCKED:` lines \
         above): this corpus executes on this host today, so a gap here is a regression",
        blocked.len()
    );
    assert!(
        compared >= MIN_BREADTH_CASES,
        "only {compared} breadth case(s) were compared (floor {MIN_BREADTH_CASES})"
    );
    assert!(
        checked[0] >= MIN_BREADTH_ORACLE_CHECKS,
        "only {} breadth value(s) were oracle-checked on Tier::Interpreter (floor \
         {MIN_BREADTH_ORACLE_CHECKS})",
        checked[0]
    );
    assert!(
        checked[1] >= MIN_BREADTH_ORACLE_CHECKS,
        "only {} breadth value(s) were oracle-checked on Tier::Cranelift (floor \
         {MIN_BREADTH_ORACLE_CHECKS})",
        checked[1]
    );
    assert!(
        modules.len() >= MIN_BREADTH_MODULES,
        "only {} breadth corpus module(s) were compared (floor {MIN_BREADTH_MODULES})",
        modules.len()
    );
}

// ---------------------------------------------------------------------------
// 5. The full error-path sweep (the deopt path, behaviourally, on both tiers)
// ---------------------------------------------------------------------------

/// Whether `outcome` is a class-`error` raise whose reason is the bare atom
/// `function_clause` -- the failed-head-clause shape only the **never-inlined**
/// `func_info` instruction produces.
///
/// The reason may arrive either as [`ExitReason::Atom`](beam_common::ExitReason)
/// (`as_atom`) or -- the shape the engine actually produces -- as the structured
/// reason whose live term *is* that atom (`Raise::live_reason`), so both spellings
/// are accepted and compared against the shared atom table.
fn is_function_clause(atoms: &AtomTable, outcome: &Result<Eterm, BeamError>) -> bool {
    let Err(BeamError::Raise(raise)) = outcome else {
        return false;
    };
    if raise.class != ExceptionClass::Error {
        return false;
    }
    if raise.reason.as_atom() == Some("function_clause") {
        return true;
    }
    let Some(atom) = atoms.lookup("function_clause") else {
        return false;
    };
    let expected = make_atom(atom);
    live_reason_term(raise).is_some_and(|term| cmp(term, expected, atoms).is_eq())
}

/// A short label for an outcome's reason, for the sweeps' histogram printout.
fn reason_label(outcome: &Result<Eterm, BeamError>) -> String {
    match outcome {
        Ok(_) => "Ok".to_string(),
        Err(BeamError::Raise(raise)) => format!("Raise({:?},{:?})", raise.class, raise.reason),
        Err(other) => format!("{other:?}"),
    }
}

/// **The error-path measurement: every poisoned corpus call, on both tiers, checked
/// against the real system `erl`.**
///
/// Iterates the **whole** committed error-path corpus -- all 232 poisoned calls over
/// 215 distinct installed modules, the same rows `tests/jit_real_module.rs` drives on
/// the interpreter + baseline pair -- with no bounded prefix and no per-module cap.
/// Every target makes the *compiled code itself* raise (`func_info` ->
/// `function_clause`, `case_end`, `badmatch`, `badrecord`), so this is the
/// raise/unwind path's tier equivalence **and** its oracle fidelity.
///
/// Each row asserts two things:
///
/// 1. **The two tiers agree**: the native tier must reproduce the interpreter's
///    typed error exactly (class, reason and live reason term), never a value and
///    never a different error.
/// 2. **Each tier agrees with the oracle**: [`verify_error`] requires the raise to
///    carry the class and reason term the system `erl` produces
///    ([`ErrorVerdict::Matches`]). A typed gap ([`ErrorVerdict::TypedGap`]) is a
///    counted `BLOCKED:` line; a disagreement ([`ErrorVerdict::Wrong`]) fails.
#[test]
fn error_path_corpus_agrees_and_exercises_the_deopt_path() {
    if skip_if_unavailable() {
        return;
    }
    let _guard = serialize_jit();
    let targets = parse_error_targets(ERROR_TXT);

    let mut host = host();
    let mut cache = Cache::new();
    let mut keep = Vec::new();
    let mut compared = 0usize;
    let mut function_clauses = 0usize;
    let mut histogram: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut checked = [0usize; 2];
    let mut modules: BTreeSet<String> = BTreeSet::new();
    let mut blocked: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut skips: Vec<String> = Vec::new();

    for target in &targets {
        let args = target.args(&mut host);
        assert_eq!(
            args.len(),
            target.arity as usize,
            "poison/arity mismatch for {}",
            target.label()
        );
        let label = target.label();

        if let Err(error) = ensure_compiled(&mut host, &mut cache, &target.module) {
            skips.push(format!("SKIP: {label} -- cranelift codegen: {error:?}"));
            continue;
        }

        let interpreted = host.call(&target.module, &target.function, &args);
        let cranelift = run_cranelift(
            &mut host,
            &mut cache,
            &mut keep,
            &target.module,
            &target.function,
            &args,
        );
        compared += 1;
        *histogram.entry(reason_label(&interpreted)).or_insert(0) += 1;

        // (a) the tiers must agree with each other ...
        if !outcomes_agree(host.atom_table(), &interpreted, &cranelift) {
            failures.push(format!(
                "DIVERGENCE {label} -- interpreter {} vs cranelift {}",
                describe(&interpreted),
                describe(&cranelift)
            ));
            continue;
        }
        modules.insert(target.module.clone());
        if is_function_clause(host.atom_table(), &interpreted)
            && is_function_clause(host.atom_table(), &cranelift)
        {
            function_clauses += 1;
        }

        // ... and (b) EACH tier must reproduce the oracle's exception.
        let mut gaps: Vec<String> = Vec::new();
        for (slot, tier_name, outcome) in [
            (0usize, "Tier::Interpreter", &interpreted),
            (1usize, "Tier::Cranelift", &cranelift),
        ] {
            match verify_error(&mut host, outcome, target) {
                ErrorVerdict::Matches => checked[slot] += 1,
                ErrorVerdict::TypedGap(gap) => gaps.push(format!("{tier_name}: {gap}")),
                ErrorVerdict::Wrong(detail) => failures.push(format!(
                    "ORACLE MISMATCH for `{label}` on {tier_name}: {detail}"
                )),
            }
        }
        if gaps.len() == 2 {
            blocked.push(format!("BLOCKED: {label} -- {}", gaps.join("; ")));
        }
    }

    println!(
        "ERROR-PATHS: selected {} case(s); compared {compared} over {} module(s), \
         {function_clauses} bare-atom `function_clause` agreement(s) (the deopted `func_info` \
         path); oracle-checked interpreter={} cranelift={}; blocked={} skipped={}",
        targets.len(),
        modules.len(),
        checked[0],
        checked[1],
        blocked.len(),
        skips.len()
    );
    for (label, count) in &histogram {
        println!("  reason {label} x{count}");
    }
    for line in &skips {
        println!("{line}");
    }
    for line in &blocked {
        println!("{line}");
    }

    assert!(
        failures.is_empty(),
        "{} error-path case(s) diverge between the interpreter and the Cranelift tier, or \
         disagree with the system `erl`:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        compared >= MIN_ERROR_CASES,
        "only {compared} error-path case(s) were compared (floor {MIN_ERROR_CASES})"
    );
    assert!(
        checked[0] >= MIN_ERROR_ORACLE_CHECKS,
        "only {} error-path raise(s) were oracle-checked on Tier::Interpreter (floor \
         {MIN_ERROR_ORACLE_CHECKS})",
        checked[0]
    );
    assert!(
        checked[1] >= MIN_ERROR_ORACLE_CHECKS,
        "only {} error-path raise(s) were oracle-checked on Tier::Cranelift (floor \
         {MIN_ERROR_ORACLE_CHECKS})",
        checked[1]
    );
    assert!(
        modules.len() >= MIN_ERROR_MODULES,
        "only {} error-path corpus module(s) were compared (floor {MIN_ERROR_MODULES})",
        modules.len()
    );
    assert!(
        function_clauses >= MIN_FUNCTION_CLAUSE_AGREEMENTS,
        "only {function_clauses} `function_clause` agreement(s): the never-inlined `func_info` \
         deopt path was not exercised (floor {MIN_FUNCTION_CLAUSE_AGREEMENTS})"
    );
}

// ---------------------------------------------------------------------------
// 6. The codegen report of a real module (the inline path was compiled)
// ---------------------------------------------------------------------------

#[test]
fn the_real_module_report_shows_inlined_instructions() {
    if skip_if_unavailable() {
        return;
    }
    let _guard = serialize_jit();
    let mut host = host();
    let module = host.load("lists").expect("load the system `lists` module");
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the real `lists` module with the Cranelift tier");

    let expected_functions = module
        .functions
        .iter()
        .filter(|function| {
            !function.code.is_empty() && function.labels.contains_key(&function.entry_label)
        })
        .count();
    assert!(expected_functions > 0, "`lists` must have functions");
    assert_eq!(
        compiled.function_count(),
        expected_functions,
        "one native entry per loadable IR function"
    );

    println!(
        "REPORT lists: functions={} tests={} gc_bifs={:?} comparisons={:?} select_vals={} \
         tuple_reads={:?} list_reads={:?} highest_native_register={:?}",
        compiled.function_count(),
        compiled.inlined_test_kinds().len(),
        compiled
            .inlined_gc_bif_ops()
            .iter()
            .map(|op| op.name())
            .collect::<Vec<_>>(),
        compiled.inlined_comparison_ops(),
        compiled.inlined_select_val_count(),
        compiled.inlined_tuple_read_kinds(),
        compiled.inlined_list_read_kinds(),
        compiled.highest_native_register(),
    );

    // The artifact inlined *something* of a real module: this is what rules out
    // "the two tiers agreed because the Cranelift tier deopted everything".
    let inlined = compiled.inlined_test_kinds().len()
        + compiled.inlined_gc_bif_ops().len()
        + compiled.inlined_comparison_ops().len()
        + compiled.inlined_select_val_count()
        + compiled.inlined_tuple_read_kinds().len()
        + compiled.inlined_list_read_kinds().len();
    assert!(
        inlined > 0,
        "the Cranelift tier inlined no instruction of the real `lists` module"
    );
}
