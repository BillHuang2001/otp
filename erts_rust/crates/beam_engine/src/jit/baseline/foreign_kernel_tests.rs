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
//! Output parity for the four `foreign` benchmark kernels, per execution tier and
//! against the system oracle.
//!
//! # Why outputs, not counters
//!
//! The bench harness (`crate::bench::foreign`, and `tools/bench`'s `foreign` group) reports
//! *counts* per call: instruction totals, heap-door traffic, BIF dispatches, deopts. A count can
//! stay plausible while the value it was counted for is wrong, and that is exactly what happened
//! here: `emqx_base62:decode/2`'s per-character body is
//!
//! ```text
//! BsCreateBin → BsStartMatch4 (fresh) → BsMatch [{ensure_exactly,8},{skip,2},{integer,2,[],6,1,{x,2}}] → BsCreateBin
//! ```
//!
//! and the `bs_match` inline arm used to commit the match context's `start` per command while its
//! `fallback` is a **deopt that re-executes the whole instruction** — so on the baseline tier
//! that shape raised `error:{badmatch,<<N>>}` where the interpreter returned the right character.
//! Every counter still looked sane on both tiers. So this module asserts the **result terms**:
//! for every row, the two tiers must agree **first**, and then that agreement must equal the
//! kernel's real output — which the live system `erl` re-derives independently.
//!
//! # The four kernels and their bench inputs
//!
//! | kernel | input | output |
//! |---|---|---|
//! | `cowboy_bstr:to_lower/1` | [`TO_LOWER_INPUT`] (30 B) | [`TO_LOWER_EXPECTED`] (30 B) |
//! | `cowboy_bstr:capitalize_token/1` | [`CAPITALIZE_INPUT`] (30 B) | [`CAPITALIZE_EXPECTED`] (30 B) |
//! | `emqx_base62:encode/1` | [`ENCODE_INPUT`] (12 B) | [`ENCODE_EXPECTED`] (16 B) |
//! | `emqx_base62:decode/1` | [`DECODE_INPUT`] (15 B) | [`DECODE_EXPECTED`] (11 B) |
//!
//! The input vectors are the ones `tools/bench/src/workloads/foreign.rs` drives, and the expected
//! outputs are that harness' `expected` constants — so a row here and a bench figure there are
//! talking about the same call.
//!
//! # The sources are staged, never copied into the repository
//!
//! Like `crate::bench::foreign`, this module copies the project's own `.erl` file from its
//! **read-only checkout** into a temp directory and compiles it with the system `erlc
//! +deterministic`, so what runs is exactly the upstream code and no copy of it is stored here
//! (which would silently drift). The checkouts are located through an env override first, then
//! the documented default root (`BENCH_COWBOY_REPO` / `BENCH_EMQX_REPO`, the same two the bench
//! harness honours).
//!
//! A missing checkout, a missing source and an unusable `erlc`/`erl` are all **loud skips**: the
//! row prints `SKIP <name>: <reason>` and returns — it never fails, and it never silently passes
//! (a green run without the toolchain proves nothing about corpus parity, which is why the skip
//! is printed).
//!
//! # What each kernel needs from the host
//!
//! `to_lower/1` and `capitalize_token/1` are BIF-free Erlang (a binary comprehension / an append
//! plus a `select_val` character table), so they run on a bare [`Floor`]. `encode/1` calls
//! `erlang:iolist_to_binary/1` once per four encoded characters, and `decode/1` asks
//! `erlang:bit_size/1` about a binary in a guard and about its own bitstring accumulator, so the
//! host carries the same **closed** BIF floor the bench harness does ([`Floor`]): those names are
//! modelled exactly and every other dispatch keeps the inner context's typed refusal, so a
//! kernel that needed an unmodelled BIF reports a loud skip instead of a figure (or a value).
//!
//! # The inline arm is proved to be exercised
//!
//! Tier agreement alone would also hold for an "implementation" that deopted the whole bitstring
//! family to the interpreter. [`the_base62_decode_match_arm_is_emitted_and_not_deopted`] closes
//! that hole with the sibling harness' **compile-time** instrument
//! (`super::bitstr_inline_tests::emitted_shapes_for`): a cache-free compile of the decode
//! function must emit the `bs_match` / `bs_start_match4` arms and record **zero** static deopts
//! for them, so the divergence the output rows now catch cannot be deopted away again.
//!
//! # Where this module sits in the node's harness
//!
//! The tier plumbing mirrors `super::bitstr_tests::with_both_tiers_arg` (a fresh [`Machine`] and
//! a fresh host per tier, the comparison *inside* the call because a returned boxed term points
//! into its run's heap) and the tier comparison is the shared
//! [`super::tests::assert_same_outcome`]. It cannot *be* that helper: `with_both_tiers_arg` hands
//! out a bare [`MinimalContext`], whose BIF hook is allocation-free by construction, and these
//! kernels dispatch `iolist_to_binary/1` and `bit_size/1` — so this module carries the
//! [`Floor`] wrapper with its own [`with_both_tiers`] and still reuses the node's instruments
//! ([`super::bitstr_tests::export`], `emitted_shapes_for`, [`static_deopts`],
//! [`visited_instances`], [`super::tests::error_reason`]).

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_common::{BeamError, Result};
use beam_loader::{load_file, Module};
use beam_term::{is_small, make_small, signed_value, Atom, AtomTable, Eterm, MAX_SMALL, MIN_SMALL};

use super::bitstr_inline_tests::{emitted_shapes_for, static_deopts, visited_instances};
use super::bitstr_tests::export;
use super::emit::bitstr_inline::{SHAPE_BS_MATCH, SHAPE_START_MATCH3, SHAPE_START_MATCH4};
use super::tests::{assert_same_outcome, error_reason};
use crate::context::{ExecutionContext, MinimalContext};
use crate::families::bitstrings::bit_size;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::{binary_bytes, list_elements, make_heap_binary};

// ---------------------------------------------------------------------------
// The bench input vectors and their verified outputs
// ---------------------------------------------------------------------------

/// `tools/bench/src/workloads/foreign.rs`'s `bstr_to_lower` input (30 bytes, mixed case, so
/// `char_to_lower/1` really runs its `select_val` per byte instead of the identity clause).
const TO_LOWER_INPUT: &[u8] = b"The Quick Brown Fox 0123456789";
/// The same call's output: every `$A..=$Z` lowered, the digits and spaces unchanged.
const TO_LOWER_EXPECTED: &[u8] = b"the quick brown fox 0123456789";

/// The `bstr_capitalize_token` input (30 bytes, dash separated).
const CAPITALIZE_INPUT: &[u8] = b"the-quick-brown-fox-0123456789";
/// The same call's output: every token's first letter upper, the rest lower.
const CAPITALIZE_EXPECTED: &[u8] = b"The-Quick-Brown-Fox-0123456789";

/// The `emqx_base62_encode` input — 12 bytes, i.e. 16 six-bit groups, so `encode/2` takes its
/// four-fields-per-clause path.
const ENCODE_INPUT: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
/// The same call's output (16 base62 characters).
const ENCODE_EXPECTED: &[u8] = b"AQIDBAUGBwgJCgsM";

/// The `emqx_base62_decode` input — 15 base62 characters, i.e. 90 bits, decoded back to 11 bytes
/// plus a 2-bit tail that the kernel's own `bit_size`/`rem` arithmetic trims.
const DECODE_INPUT: &[u8] = b"ABCDEFGHIJKLMNO";
/// The same call's output (11 bytes, arbitrary — so a row compares bytes, never text).
const DECODE_EXPECTED: &[u8] = &[0, 16, 131, 16, 81, 135, 32, 146, 139, 48, 222];

// ---------------------------------------------------------------------------
// The host: a `MinimalContext` plus the bench harness' closed BIF floor
// ---------------------------------------------------------------------------

/// The deepest iolist nesting [`flatten_iolist`] will walk (a guard rail, not a limit any real
/// iolist reaches).
const MAX_IOLIST_DEPTH: usize = 64;

/// A [`MinimalContext`] whose `call_bif` serves the handful of BIFs these four kernels dispatch.
///
/// The served set is deliberately tiny and **closed** — `erlang:iolist_to_binary/1`,
/// `erlang:bit_size/1` and the small-int `erlang:'+'/2`, `'-'/2`, `'*'/2`, `div/2`, `rem/2`,
/// `abs/1` — exactly the floor `crate::bench::counting::CountingContext` installs, so a row here
/// and a bench figure there are driven by the same host contract. Every other
/// `(module, function, arity)` is forwarded to the inner context, which answers its typed
/// `unsupported_bif`: a kernel that needed more would report a loud skip, never a shaped value.
///
/// The floor **allocates through this context** ([`make_heap_binary`] →
/// [`ExecutionContext::alloc_words`]), which is why it is a wrapper rather than a
/// [`MinimalContext`] BIF hook: a `BifHook` closure is handed no heap.
struct Floor {
    /// The wrapped heap/BIF host (unbounded, bump-allocated, never collecting).
    inner: MinimalContext,
}

impl Floor {
    /// A floor over a fresh, empty region heap.
    fn new() -> Floor {
        Floor {
            inner: MinimalContext::new(),
        }
    }

    /// The floor's own answer, `None` when the call is not one the floor models.
    fn host_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Option<Result<Eterm>> {
        if module != "erlang" || args.len() != arity as usize {
            return None;
        }
        match (function, arity) {
            ("iolist_to_binary", 1) => {
                let mut bytes = Vec::new();
                Some(match flatten_iolist(args[0], &mut bytes, 0) {
                    Ok(()) => make_heap_binary(self, &bytes),
                    Err(error) => Err(error),
                })
            }
            ("+", 2) | ("-", 2) | ("*", 2) | ("div", 2) | ("rem", 2) => {
                Some(small_int_arith(function, args[0], args[1]))
            }
            ("abs", 1) => Some(small_int_abs(args[0])),
            ("bit_size", 1) => Some(bit_size_of(args[0])),
            _ => None,
        }
    }
}

impl ExecutionContext for Floor {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // The floor first; anything it does not model keeps the inner host's refusal, so a
        // half-modelled BIF can never turn a guard failure into a wrong result.
        match self.host_bif(module, function, arity, args) {
            Some(result) => result,
            None => self.inner.call_bif(module, function, arity, args),
        }
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        // Unbounded heap: the inner context's admission point is a no-op, and there is no
        // collector behind it (this host is the test/embedding heap, not a process heap).
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn heap_available(&self) -> usize {
        self.inner.heap_available()
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        // The engine's `alloc_words_rooted` default forwards here, so every in-crate term
        // builder (rooted or not) allocates on the inner region heap.
        self.inner.alloc_words(n)
    }

    fn atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable {
        // The module's own load-time snapshot is the namespace these kernels' guards and
        // `select_val`s resolve names through, and it is what the harness compares outcomes
        // with.
        self.inner.atoms(module)
    }
}

/// The floor's refusal: a typed error naming the floor, so a printed skip says "the test host
/// declined this" rather than "the engine failed".
fn refuse(what: &str) -> BeamError {
    crate::error::unsupported(&format!("foreign-kernel BIF floor: {what}"))
}

/// Flatten `term` — a byte, a binary or a proper (possibly nested) list of those — into `out`,
/// reproducing `erlang:iolist_to_binary/1`.
///
/// Anything it cannot flatten **exactly** is [`refuse`]d: a non-byte small integer, a
/// non-byte-aligned bitstring, an improper tail, an atom/float/tuple element, or nesting deeper
/// than [`MAX_IOLIST_DEPTH`]. Nothing is ever coerced.
fn flatten_iolist(term: Eterm, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > MAX_IOLIST_DEPTH {
        return Err(refuse("iolist nesting is deeper than MAX_IOLIST_DEPTH"));
    }
    if is_small(term) {
        // `is_byte` is the frozen `0..=255` small-int test, so a negative or wide integer is
        // declined instead of being truncated to a byte.
        if !beam_term::is_byte(term) {
            return Err(refuse("iolist element is a small integer outside 0..=255"));
        }
        out.push(signed_value(term) as u8);
        return Ok(());
    }
    if beam_term::is_nil(term) {
        return Ok(());
    }
    // A byte-aligned binary — on-heap or off-heap refc. A non-byte-aligned bitstring is not a
    // legal iolist element and `binary_bytes` rejects it, so it falls through to the refusal.
    if let Ok(bytes) = binary_bytes(term) {
        out.extend_from_slice(bytes);
        return Ok(());
    }
    if beam_term::is_list(term) {
        for element in list_elements(term)? {
            flatten_iolist(element, out, depth + 1)?;
        }
        return Ok(());
    }
    Err(refuse(
        "iolist element is neither a byte, a binary nor a proper list",
    ))
}

/// One operand of the floor's arithmetic: a **small** integer, never a bignum, float or atom.
fn small_operand(function: &str, term: Eterm) -> Result<i64> {
    if !is_small(term) {
        return Err(refuse(&format!(
            "erlang:{function} was given a non-small-integer operand"
        )));
    }
    Ok(signed_value(term))
}

/// A checked small-int result: outside [`MIN_SMALL`]`..=`[`MAX_SMALL`] is [`refuse`]d (the real
/// BIF would promote to a bignum, which this floor does not model — declining is the honest
/// answer).
fn small_result(function: &str, value: i64) -> Result<Eterm> {
    if value < MIN_SMALL || value > MAX_SMALL {
        return Err(refuse(&format!(
            "erlang:{function} left the small-integer range ({value})"
        )));
    }
    Ok(make_small(value))
}

/// `erlang:bit_size/1` over a bitstring **or** a live match context.
///
/// The size comes from the bitstring family's own frozen-layout reader
/// ([`crate::families::bitstrings::bit_size`]) — heap bits and an off-heap refc window take their
/// size, an `ErlSubBits` match context its remaining window, which is what C's
/// `erts_bin_bits_size` does for every `SUB_BITS` thing. A subject that is neither is
/// [`refuse`]d (the real BIF raises `badarg`), and a size outside the small-integer range is
/// [`refuse`]d rather than truncated, so nothing is ever coerced into a value.
///
/// This is what makes `emqx_base62:decode/1` runnable at all: it asks `bit_size/1` about a
/// binary in a **guard** and about its own bitstring accumulator, and a wrong size would
/// silently change the decode rather than fail it.
fn bit_size_of(subject: Eterm) -> Result<Eterm> {
    let Some(bits) = bit_size(subject) else {
        return Err(refuse(
            "erlang:bit_size/1 was given something that is neither a bitstring nor a match \
             context",
        ));
    };
    let Ok(bits) = i64::try_from(bits) else {
        return Err(refuse("erlang:bit_size/1 does not fit a small integer"));
    };
    small_result("bit_size", bits)
}

/// `erlang:'+'/2`, `'-'/2`, `'*'/2`, `div/2`, `rem/2` over two small integers.
fn small_int_arith(function: &str, lhs: Eterm, rhs: Eterm) -> Result<Eterm> {
    let lhs = small_operand(function, lhs)?;
    let rhs = small_operand(function, rhs)?;
    let value = match function {
        "+" => lhs.checked_add(rhs),
        "-" => lhs.checked_sub(rhs),
        "*" => lhs.checked_mul(rhs),
        "div" => lhs.checked_div(rhs),
        // `checked_rem` panics on `i64::MIN % -1` in debug builds, so exclude it explicitly.
        "rem" => {
            if rhs == 0 || (lhs == i64::MIN && rhs == -1) {
                None
            } else {
                Some(lhs % rhs)
            }
        }
        other => return Err(refuse(&format!("erlang:{other}/2 is not modelled"))),
    };
    let Some(value) = value else {
        return Err(refuse(&format!(
            "erlang:{function}/2 on {lhs} and {rhs} (zero divisor or overflow)"
        )));
    };
    small_result(function, value)
}

/// `erlang:abs/1` over a small integer.
fn small_int_abs(operand: Eterm) -> Result<Eterm> {
    let value = small_operand("abs", operand)?;
    let Some(value) = value.checked_abs() else {
        return Err(refuse("erlang:abs/1 overflowed"));
    };
    small_result("abs", value)
}

// ---------------------------------------------------------------------------
// One tier's observable result
// ---------------------------------------------------------------------------

/// What one tier's run produced, **copied out while its host is alive** (a returned boxed term
/// points into that host's region heap, which dies with the host).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Observation {
    /// The run returned a byte-aligned binary: its bytes.
    Bytes(Vec<u8>),
    /// The run returned a small integer: its value.
    Small(i64),
    /// The run returned some other term (an atom, a tuple, a non-byte-aligned bitstring).
    Other,
    /// The run did not return a value: the typed error, or an outcome these kernels cannot have.
    Failed(String),
}

impl Observation {
    /// Snapshot `outcome`; call this while the run's host is still alive.
    fn of(outcome: &Result<RunOutcome>) -> Observation {
        match outcome {
            Ok(RunOutcome::Returned(term)) => {
                if is_small(*term) {
                    return Observation::Small(signed_value(*term));
                }
                match binary_bytes(*term) {
                    Ok(bytes) => Observation::Bytes(bytes.to_vec()),
                    Err(_) => Observation::Other,
                }
            }
            Ok(other) => Observation::Failed(format!("{other:?}")),
            Err(error) => Observation::Failed(error_reason(error)),
        }
    }

    /// The bytes of a [`Observation::Bytes`], for an assertion message or an oracle compare.
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Observation::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Running one kernel on both tiers
// ---------------------------------------------------------------------------

/// Run exported `function/1` with `input` on the **interpreter** and the **baseline** tier, each
/// on its own fresh [`Machine`] and [`Floor`], and hand the two outcomes to `check`.
///
/// The comparison has to happen inside this call: a returned binary may live on the run's own
/// host heap, and a [`MinimalContext`] never frees what it hands out — but it must still outlive
/// every term built through it, so both hosts stay alive for the whole `check`.
fn with_both_tiers<R>(
    module: &Module,
    entry: Atom,
    input: &[u8],
    check: impl FnOnce(&Result<RunOutcome>, &Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_host = Floor::new();
    let value = make_heap_binary(&mut interpreter_host, input).expect("the input binary builds");
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        entry,
        1,
        &[value],
        &mut interpreter_host,
        Tier::Interpreter,
    );

    let mut baseline = Machine::new();
    let mut baseline_host = Floor::new();
    let value = make_heap_binary(&mut baseline_host, input).expect("the input binary builds");
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        entry,
        1,
        &[value],
        &mut baseline_host,
        Tier::BaselineJit,
    );

    check(&interpreter_outcome, &baseline_outcome)
}

/// Assert one kernel row in the required order: **tier equality first**, then the concrete
/// expected output. Returns the agreed observation so the caller can hand it to the oracle leg.
fn assert_kernel_row(
    label: &str,
    module: &Module,
    entry: Atom,
    input: &[u8],
    expected: &[u8],
) -> Observation {
    with_both_tiers(module, entry, input, |interpreter, baseline| {
        // 1. The two tiers must agree — the harness' own structural comparison, which also
        //    covers a *raise* (both sides reporting the same typed error is agreement too).
        assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        let interpreter_observation = Observation::of(interpreter);
        let baseline_observation = Observation::of(baseline);
        assert_eq!(
            baseline_observation, interpreter_observation,
            "{label}: the tiers observed different results"
        );
        // 2. That agreement must be the kernel's real output. A shared mistake — a fix that is
        //    applied on both tiers — fails here, which is why the expected value is asserted and
        //    not merely compared across tiers.
        assert_eq!(
            interpreter_observation,
            Observation::Bytes(expected.to_vec()),
            "{label}: the result is not the kernel's expected output"
        );
        interpreter_observation
    })
}

// ---------------------------------------------------------------------------
// Staging a foreign program from its read-only checkout
// ---------------------------------------------------------------------------

/// One foreign program: the checkout that holds it, the source file and the module `erlc`
/// produces (the module name is the file's stem, exactly as the bench harness relies on).
struct Program {
    /// The module `erlc` produces (and the artifact is loaded as).
    module: &'static str,
    /// The env var naming the checkout root.
    repo_env: &'static str,
    /// The checkout root when the env var is unset.
    repo_default: &'static str,
    /// The source file, relative to the checkout root.
    source: &'static str,
    /// The temp-directory tag the staged compile uses.
    tag: &'static str,
}

/// Cowboy's `cowboy_bstr` (<https://github.com/ninenines/cowboy>, ISC licence, (c) Loic Hoguin):
/// `to_lower/1`, `capitalize_token/1` and the `char_to_lower/1`/`char_to_upper/1` tables.
static COWBOY: Program = Program {
    module: "cowboy_bstr",
    repo_env: "BENCH_COWBOY_REPO",
    repo_default: "/home/bill/Source/cowboy",
    source: "src/cowboy_bstr.erl",
    tag: "foreign_kernel_cowboy",
};

/// EMQX's `emqx_base62` (<https://github.com/emqx/emqx>, Apache-2.0, (c) EMQ Technologies):
/// `encode/1`, `decode/1` and their interval functions.
static EMQX: Program = Program {
    module: "emqx_base62",
    repo_env: "BENCH_EMQX_REPO",
    repo_default: "/home/bill/Source/emqx",
    source: "apps/emqx/src/emqx_base62.erl",
    tag: "foreign_kernel_emqx",
};

/// A staged foreign module: the loaded artifact plus the temp directory it was compiled into
/// (the oracle needs that directory on its code path).
struct Staged {
    /// The loaded module.
    module: Module,
    /// The directory holding the compiled `.beam` (and the staged `.erl`).
    dir: PathBuf,
}

/// Stage `program`'s source into a temp directory, compile it with the system `erlc
/// +deterministic` and load the artifact.
///
/// Every failure is a `String` reason the caller skips loudly on: a missing checkout/source, an
/// unspawnable or failing `erlc` (an external tool, never a gate), or an artifact the loader
/// refuses.
fn stage(program: &Program) -> std::result::Result<Staged, String> {
    let repo = match std::env::var(program.repo_env) {
        Ok(root) => PathBuf::from(root),
        Err(_) => PathBuf::from(program.repo_default),
    };
    let source = repo.join(program.source);
    if !source.is_file() {
        return Err(format!(
            "{} is not a file (set {} to the checkout root)",
            source.display(),
            program.repo_env
        ));
    }
    let dir = std::env::temp_dir().join(format!(
        "beam_engine_{}_{}",
        program.tag,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("create {}: {error}", dir.display()))?;
    let staged = dir.join(format!("{}.erl", program.module));
    std::fs::copy(&source, &staged).map_err(|error| {
        format!(
            "copy {} -> {}: {error}",
            source.display(),
            staged.display()
        )
    })?;
    let erlc = std::env::var("ERLC").unwrap_or_else(|_| "erlc".to_string());
    let compiled = Command::new(&erlc)
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(&staged)
        .output()
        .map_err(|error| format!("no usable `{erlc}` on PATH ({error})"))?;
    if !compiled.status.success() {
        return Err(format!(
            "`{erlc}` exited {} on {}: {}",
            compiled.status,
            staged.display(),
            String::from_utf8_lossy(&compiled.stderr).trim()
        ));
    }
    let beam = dir.join(format!("{}.beam", program.module));
    let module = load_file(&beam)
        .map_err(|error| format!("load {}: {error:?}", beam.display()))?;
    Ok(Staged { module, dir })
}

/// The atom `name` in `module`'s table — a staged artifact always interns its own function and
/// module names.
fn export_atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("`{name}` must be interned in the staged artifact"))
}

/// [`stage`], printing the loud skip (and its reason) instead of failing when the toolchain or
/// the checkout is absent.
fn stage_or_skip(test: &str, program: &Program) -> Option<Staged> {
    match stage(program) {
        Ok(staged) => Some(staged),
        Err(why) => {
            println!("SKIP {test}: {why} -- the output-parity row did not run");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The oracle: the system `erl`
// ---------------------------------------------------------------------------

/// One oracle call: `module:function(<<input>>)`.
#[derive(Clone, Copy)]
struct Call<'a> {
    /// The module to call.
    module: &'a str,
    /// The function to call (arity 1).
    function: &'a str,
    /// The one argument, as the input binary's bytes.
    input: &'a [u8],
}

/// Ask the system `erl` (the oracle) for `calls`' results — each as the returned binary's bytes
/// — from inside `dirs` (the temp directories holding the staged artifacts).
///
/// One `erl` process answers every call, printing one `[...]` byte list per line, so the parsing
/// is line-based and a multi-call row costs one boot. Every failure is a `String` reason the
/// caller skips loudly on (no `ERTS_ORACLE`/`erl` on `PATH`, a non-zero exit, or output it
/// cannot read).
fn oracle_answers(
    dirs: &[&Path],
    calls: &[Call<'_>],
) -> std::result::Result<Vec<Vec<u8>>, String> {
    let erl = std::env::var("ERTS_ORACLE").unwrap_or_else(|_| "erl".to_string());
    let mut eval = String::new();
    for call in calls {
        let literal = call
            .input
            .iter()
            .map(|byte| byte.to_string())
            .collect::<Vec<_>>()
            .join(",");
        eval.push_str(&format!(
            "io:format(\"~w~n\", [binary_to_list({module}:{function}(<<{literal}>>))]), ",
            module = call.module,
            function = call.function,
        ));
    }
    eval.push_str("halt().");
    let mut command = Command::new(&erl);
    command.arg("-noshell");
    for dir in dirs {
        command.arg("-pa").arg(dir);
    }
    let output = command
        .arg("-eval")
        .arg(&eval)
        .output()
        .map_err(|error| format!("no usable `{erl}` on PATH ({error})"))?;
    if !output.status.success() {
        return Err(format!(
            "`{erl}` exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() != calls.len() {
        return Err(format!(
            "`{erl}` printed {} line(s) for {} call(s): {stdout}",
            lines.len(),
            calls.len()
        ));
    }
    lines
        .iter()
        .map(|line| {
            let digits = line
                .trim_end_matches('.')
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']');
            digits
                .split(',')
                .map(|field| {
                    field
                        .trim()
                        .parse::<u8>()
                        .map_err(|error| format!("unparsable oracle byte {field:?}: {error}"))
                })
                .collect::<std::result::Result<Vec<u8>, String>>()
        })
        .collect()
}

/// Compare `observation` with the oracle's answer for `call`, skipping loudly when the oracle
/// toolchain is absent or unusable.
///
/// The oracle leg re-derives the expected bytes from the live system `erlang`, so a hand-edited
/// constant above cannot go unnoticed: the row would then disagree with itself.
fn assert_oracle_row(test: &str, dirs: &[&Path], call: Call<'_>, observation: &Observation) {
    match oracle_answers(dirs, &[call]) {
        Err(why) => {
            println!("SKIP {test}: {why} -- the system-oracle leg of the row did not run");
        }
        Ok(answers) => {
            assert_eq!(
                answers.len(),
                1,
                "{test}: the oracle must answer the row's one call"
            );
            assert_eq!(
                observation.bytes(),
                Some(answers[0].as_slice()),
                "{test}: {}:{}/1 disagrees with the system oracle (rust {observation:?}, \
                 oracle {:?})",
                call.module,
                call.function,
                answers[0],
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The rows: one per kernel
// ---------------------------------------------------------------------------

/// `cowboy_bstr:to_lower/1` on the bench input vector: both tiers agree, the bytes are the
/// input's lower case, and the system `erl` says the same.
///
/// The kernel is a binary comprehension, so this row runs `bs_start_match3` + `bs_match`
/// (`ensure_at_least`/`integer` 8) + a per-byte `call` into the 27-clause `select_val` table +
/// `bs_create_bin` with `[private_append, …]` — the append accumulator, not a matched window.
#[test]
fn cowboy_to_lower_agrees_on_both_tiers_and_with_the_oracle() {
    let Some(staged) = stage_or_skip("cowboy_to_lower", &COWBOY) else {
        return;
    };
    let entry = export(&staged.module, COWBOY.module, "to_lower");
    let call = Call {
        module: COWBOY.module,
        function: "to_lower",
        input: TO_LOWER_INPUT,
    };
    let observation = assert_kernel_row(
        "cowboy_bstr:to_lower/1",
        &staged.module,
        entry,
        call.input,
        TO_LOWER_EXPECTED,
    );
    assert_oracle_row("cowboy_to_lower", &[&staged.dir], call, &observation);
}

/// `cowboy_bstr:capitalize_token/1` on the bench input vector: both tiers agree, the bytes are
/// the capitalized form, and the system `erl` says the same.
///
/// This is the four-clause binary-match recursion: `<< $-, Rest/bits >>` / `<< C, Rest/bits >>`
/// twice, each rebuilding the accumulator with `bs_create_bin`'s binary append, and a
/// `char_to_upper`/`char_to_lower` `select_val` per byte.
#[test]
fn cowboy_capitalize_token_agrees_on_both_tiers_and_with_the_oracle() {
    let Some(staged) = stage_or_skip("cowboy_capitalize_token", &COWBOY) else {
        return;
    };
    let entry = export(&staged.module, COWBOY.module, "capitalize_token");
    let call = Call {
        module: COWBOY.module,
        function: "capitalize_token",
        input: CAPITALIZE_INPUT,
    };
    let observation = assert_kernel_row(
        "cowboy_bstr:capitalize_token/1",
        &staged.module,
        entry,
        call.input,
        CAPITALIZE_EXPECTED,
    );
    assert_oracle_row(
        "cowboy_capitalize_token",
        &[&staged.dir],
        call,
        &observation,
    );
}

/// `emqx_base62:encode/1` on the bench input vector: both tiers agree, the bytes are the base62
/// text, and the system `erl` says the same.
///
/// Twelve input bytes are sixteen six-bit groups, so `encode/2` takes its
/// four-fields-per-clause path: one `bs_start_match*` + `bs_match` with four static 6-bit
/// `integer` commands and a `get_tail`, one `iolist_to_binary/1` (the floor's only allocating
/// BIF here, one per clause) and the `encode_char/1` guard chain.
#[test]
fn emqx_base62_encode_agrees_on_both_tiers_and_with_the_oracle() {
    let Some(staged) = stage_or_skip("emqx_base62_encode", &EMQX) else {
        return;
    };
    let entry = export(&staged.module, EMQX.module, "encode");
    let call = Call {
        module: EMQX.module,
        function: "encode",
        input: ENCODE_INPUT,
    };
    let observation = assert_kernel_row(
        "emqx_base62:encode/1",
        &staged.module,
        entry,
        call.input,
        ENCODE_EXPECTED,
    );
    assert_oracle_row("emqx_base62_encode", &[&staged.dir], call, &observation);
}

/// `emqx_base62:decode/1` on the bench input vector: both tiers agree, the bytes are the decoded
/// payload, and the system `erl` says the same.
///
/// **This is the row the silent divergence survived.** The kernel's first clause matches one
/// byte, asks `bit_size/1` about the tail in a guard, and — in its `$9` branch — rematches the
/// next byte; the second clause is where the per-character body runs:
///
/// ```text
/// BsCreateBin → BsStartMatch4 (fresh) → BsMatch [{ensure_exactly,8},{skip,2},{integer,2,[],6,1,{x,2}}] → BsCreateBin
/// ```
///
/// so a `bs_match` arm that committed the context's `start` per command and then fell back to a
/// deopt re-executing the whole instruction produced `error:{badmatch,<<N>>}` on the baseline
/// tier while the interpreter returned the right 6-bit field. The expected bytes here are the
/// oracle's own answer for the same call.
#[test]
fn emqx_base62_decode_agrees_on_both_tiers_and_with_the_oracle() {
    let Some(staged) = stage_or_skip("emqx_base62_decode", &EMQX) else {
        return;
    };
    let entry = export(&staged.module, EMQX.module, "decode");
    let call = Call {
        module: EMQX.module,
        function: "decode",
        input: DECODE_INPUT,
    };
    let observation = assert_kernel_row(
        "emqx_base62:decode/1",
        &staged.module,
        entry,
        call.input,
        DECODE_EXPECTED,
    );
    assert_oracle_row("emqx_base62_decode", &[&staged.dir], call, &observation);
}

/// **The whole bench input-vector set, in one row**: all four kernels, each asserted on the
/// interpreter **and** the baseline tier (tier equality, then its pinned output), and all four
/// re-derived by **one** system-`erl` invocation.
///
/// The per-kernel rows above isolate a failure; this row is the set as the bench defines it, so
/// a changed input vector, a changed expectation or a kernel that only agrees on one of the two
/// artifacts cannot pass unnoticed.
#[test]
fn the_whole_bench_input_vector_set_agrees_on_both_tiers_and_with_the_oracle() {
    let Some(cowboy) = stage_or_skip("foreign_kernel_vector_set(cowboy)", &COWBOY) else {
        return;
    };
    let Some(emqx) = stage_or_skip("foreign_kernel_vector_set(emqx)", &EMQX) else {
        return;
    };
    // The set exactly as `tools/bench`'s `foreign` group defines it: (program, function, input,
    // expected output).
    let cases: [(&Staged, &str, &str, &[u8], &[u8]); 4] = [
        (
            &cowboy,
            COWBOY.module,
            "to_lower",
            TO_LOWER_INPUT,
            TO_LOWER_EXPECTED,
        ),
        (
            &cowboy,
            COWBOY.module,
            "capitalize_token",
            CAPITALIZE_INPUT,
            CAPITALIZE_EXPECTED,
        ),
        (
            &emqx,
            EMQX.module,
            "encode",
            ENCODE_INPUT,
            ENCODE_EXPECTED,
        ),
        (
            &emqx,
            EMQX.module,
            "decode",
            DECODE_INPUT,
            DECODE_EXPECTED,
        ),
    ];
    let calls: Vec<Call<'_>> = cases
        .iter()
        .map(|(_, module, function, input, _)| Call {
            module,
            function,
            input,
        })
        .collect();
    // Per kernel: tier equality first, then its pinned output.
    let mut observations: Vec<Observation> = Vec::with_capacity(cases.len());
    for (staged, module, function, input, expected) in cases {
        let entry = export(&staged.module, module, function);
        let observation = assert_kernel_row(
            &format!("{module}:{function}/1"),
            &staged.module,
            entry,
            input,
            expected,
        );
        observations.push(observation);
    }
    // Then one system-`erl` invocation re-derives all four answers, each compared with the
    // tier-agreed result above.
    match oracle_answers(&[&cowboy.dir, &emqx.dir], &calls) {
        Err(why) => println!(
            "SKIP foreign_kernel_vector_set: {why} -- the system-oracle leg of the row did not run"
        ),
        Ok(answers) => {
            for ((_, module, function, _, expected), (observation, answer)) in
                cases.iter().zip(observations.iter().zip(answers.iter()))
            {
                assert_eq!(
                    observation.bytes(),
                    Some(answer.as_slice()),
                    "{module}:{function}/1: the tier-agreed result disagrees with the system \
                     oracle (rust {observation:?}, oracle {answer:?})"
                );
                assert_eq!(
                    answer.as_slice(),
                    *expected,
                    "{module}:{function}/1: the system oracle's own answer is not the pinned \
                     expectation"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The instrument: the inline arms the decode body needs really are emitted
// ---------------------------------------------------------------------------

/// **The `bs_match` arm the base62 decode body runs is emitted, not deopted** — taken through a
/// cache-free compile (`super::bitstr_inline_tests::emitted_shapes_for`) so the process-wide
/// content-keyed [`crate::jit::cache::CodeCache`] cannot answer from a warmed artifact that
/// re-emits nothing.
///
/// Without this, every output row above would also pass for an "implementation" that deopted the
/// whole bitstring family to the interpreter — the divergence being caught is *inside* the arm,
/// so the row has to know the arm ran. The function is `decode/2` and `encode/2` (arity 2: the
/// accumulator recursion; `decode/1`/`encode/1` are one-line wrappers with no `Bs*` match).
#[test]
fn the_base62_decode_match_arm_is_emitted_and_not_deopted() {
    let Some(staged) = stage_or_skip("base62_match_arm", &EMQX) else {
        return;
    };
    for (function, arity) in [("decode", 2u32), ("encode", 2u32)] {
        let name = export_atom(&staged.module, function);
        let Some(index) = staged
            .module
            .functions
            .iter()
            .position(|candidate| candidate.arity == arity && candidate.name == name)
        else {
            panic!(
                "{} must define {function}/{arity}: the staged upstream source changed shape",
                EMQX.module
            );
        };
        let index = u32::try_from(index)
            .unwrap_or_else(|_| panic!("{function}/{arity}'s function index must fit a u32"));
        let (emitted, snapshot) = emitted_shapes_for(&staged.module, index);
        println!(
            "inline_arms[{function}/{arity}] = {emitted:?}, BsMatch deopts = {}, \
             BsMatch visited = {}, BsStartMatch4 deopts = {}, BsStartMatch4 visited = {}",
            static_deopts(&snapshot, "BsMatch"),
            visited_instances(&snapshot, "BsMatch"),
            static_deopts(&snapshot, "BsStartMatch4"),
            visited_instances(&snapshot, "BsStartMatch4"),
        );
        assert!(
            visited_instances(&snapshot, "BsMatch") >= 1,
            "{function}/{arity} must contain a `BsMatch` instruction (the census name is what \
             makes the deopt assertion below non-vacuous)"
        );
        assert!(
            visited_instances(&snapshot, "BsStartMatch4") + visited_instances(&snapshot, "BsStartMatch3")
                >= 1,
            "{function}/{arity} must open a match context (`BsStartMatch3`/`BsStartMatch4`)"
        );
        // `decode/2`'s command list is fully static (`ensure_exactly` + `skip` + a 6-bit
        // `integer`), so its `bs_match` must be lowered in-block, never handed to the family
        // helper: a static deopt there is exactly the path that would let the historical
        // divergence hide again.
        if function == "decode" {
            assert!(
                emitted.contains(&SHAPE_BS_MATCH),
                "decode/{arity}: the `bs_match` inline arm must emit (emitted: {emitted:?})"
            );
            assert_eq!(
                static_deopts(&snapshot, "BsMatch"),
                0,
                "decode/{arity}: `BsMatch` must be lowered, not statically deopted (deopts: {:?})",
                snapshot.instr_deopts,
            );
            assert!(
                emitted.contains(&SHAPE_START_MATCH4) || emitted.contains(&SHAPE_START_MATCH3),
                "decode/{arity}: the match-context opening arm must emit (emitted: {emitted:?})"
            );
        }
    }
}
