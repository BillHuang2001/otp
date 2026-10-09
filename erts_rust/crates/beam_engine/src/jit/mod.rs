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

//! JIT tier seam: the execution tiers, the backend trait and the tier roster.
//!
//! The engine executes the frozen typed IR in one of three tiers, identified by
//! [`Tier`]:
//!
//! * [`Tier::Interpreter`] — the direct-threaded loop (`crate::interpreter`).
//!   Always compiled in, always first in [`available_tiers`]; the vertical slice
//!   runs on it. [`Machine::run_export`](crate::Machine::run_export) is and stays
//!   the default interpreter entry point.
//! * [`Tier::BaselineJit`] — the native baseline tier (a Rust port of BeamAsm with
//!   block register allocation). It is **built**: [`baseline::BaselineJit`] lowers
//!   the typed IR's basic blocks to x86-64 (via the encoder and executable code
//!   buffer in the [`emitter`] submodule) and runs a function through a driver loop
//!   that falls back to the interpreter's per-instruction step for anything it does
//!   not lower. See [`baseline`] for the block ABI, the register-allocation scheme
//!   and the inline/helper split.
//! * [`Tier::Cranelift`] — the optional hot tier (Cranelift codegen), gated behind
//!   the **`jit-cranelift`** Cargo feature (default **OFF**). Deliberately off the
//!   critical path: with the feature disabled the crate carries no Cranelift
//!   dependency at all and the build never depends on Cranelift codegen. When the
//!   feature is on, [`cranelift::CraneliftJit`] lowers each IR function into a real
//!   Cranelift function that loops natively over the function's control flow while
//!   delegating per-instruction semantics to the shared interpreter-backed helper;
//!   without the feature selecting the tier is a typed error, never a panic.
//!
//! [`available_tiers`] reports the tier **names** compiled into this build, and
//! [`Tier::from_name`] maps a name back to a [`Tier`]; the two are kept consistent
//! by construction (a unit test asserts the roster is a prefix of [`Tier::ALL`]).
//!
//! # The `jit-beamasm` Cargo feature
//!
//! The historical `jit-beamasm` feature is retained as a **documented alias for
//! the baseline tier**, not as a separate tier: the baseline tier is part of every
//! build's roster (`"baseline-jit"` is always present after `"interpreter"`), so
//! enabling the feature changes nothing.
//!
//! # Backends
//!
//! A native tier implements [`JitBackend`]: it names itself ([`JitBackend::name`]),
//! reports whether it is usable in the current build
//! ([`JitBackend::available`]), compiles a whole [`Module`] into its own
//! [`JitBackend::Compiled`] artifact, and runs an exported function on a
//! [`Machine`]. The trait is the seam the runtime selects a tier through; the
//! interpreter is *not* a [`JitBackend`] (it has nothing to compile — it is
//! reached through [`Tier::Interpreter`] and
//! [`Machine::run_export`](crate::Machine::run_export)). [`baseline::BaselineJit`]
//! and — with the `jit-cranelift` feature — [`cranelift::CraneliftJit`] are the
//! backends built today.
//!
//! # The tiering policy
//!
//! [`tiering::TierPolicy`] models the interpret → baseline → hot **escalation
//! ladder** of Part II §F (§F "Tiering"): a per-function execution counter crosses a
//! baseline threshold to move a function onto [`Tier::BaselineJit`], and a hot
//! threshold to move it onto [`Tier::Cranelift`] *when that tier is compiled in*
//! (otherwise it stays on the baseline). It is a demonstrated **policy** — nothing on
//! an execution path consults it: [`Machine::run_with_tier`](crate::Machine::run_with_tier)
//! still runs the single tier its caller names, and no backend recompiles or promotes
//! mid-run. Wiring a real per-function counter and promotion is later work.

use beam_common::Result;
use beam_loader::{Atom, Label, Module};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::{Machine, RunOutcome};

pub mod baseline;
/// The engine's interpreter-vs-native measurement harness and its workload set.
///
/// Public (not `#[cfg(test)]`) because the tier table is measured from **both** the
/// in-crate unit tests (the Cranelift ABBA harness consumes its `support`) and the
/// integration test `tests/jit_bench.rs`, which owns the `#[test]` entry points and
/// drives [`bench::run_measurement_table`]/[`bench::assert_tier_agreement`]. Keeping
/// one harness here is what stops the two from growing a copy each.
pub mod bench;
pub mod cache;
#[cfg(feature = "jit-cranelift")]
pub mod cranelift;
pub mod emitter;
pub mod tiering;

pub use cache::CodeCache;
pub use tiering::{TierPolicy, DEFAULT_BASELINE_THRESHOLD, DEFAULT_HOT_THRESHOLD};

/// One contiguous region of **generated** machine code produced by a tier.
///
/// This is the queryable code-range surface a host needs to describe the native code
/// the engine is running — most concretely the `+JPperf` perf-map file `beam_rt`
/// writes, which is empty without it. Every range is the **real** address of a
/// compiled basic block (a [`baseline::CompiledModule::entry`]), never a synthesised
/// or rounded one; an artifact that compiled no block reports no region.
///
/// Produced by [`baseline::CompiledModule::code_map`] (a caller holding its own
/// artifact) and by [`cache::baseline_code_map`] (a host running the shared cached
/// path), both of which return the regions in ascending address order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeRegion {
    /// The address of the region's first byte: a compiled block's entry point.
    pub start: usize,
    /// The region's size in bytes (always `> 0`).
    ///
    /// The region runs from [`CodeRegion::start`] to the next region's `start`, or to
    /// the end of the artifact's executable mapping for the last block.
    pub size: usize,
    /// A human-readable origin of the region, e.g. `"slice:fib/0"` for the block at
    /// code offset `0` of `slice:fib/0`. Never empty.
    pub name: String,
}

/// An execution tier of the typed IR.
///
/// The variants are ordered as the roster is ([`Tier::ALL`]): the interpreter
/// first, the optional Cranelift hot tier last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// The direct-threaded interpreter (`crate::interpreter`).
    ///
    /// Always available; the reference semantics every other tier must match.
    Interpreter,
    /// The native baseline JIT (a Rust port of BeamAsm with block register
    /// allocation).
    ///
    /// Named `"baseline-jit"`. The backend is [`baseline::BaselineJit`]: it compiles
    /// the typed IR's basic blocks to x86-64 and runs the export through a driver
    /// loop with a per-instruction interpreter deopt.
    BaselineJit,
    /// The optional Cranelift hot tier.
    ///
    /// Named `"jit-cranelift"` and compiled in only when the **`jit-cranelift`**
    /// Cargo feature is enabled (default OFF). With the feature on the backend is
    /// [`cranelift::CraneliftJit`]: it lowers each IR function to a native Cranelift
    /// function that loops over the function's control flow and delegates
    /// per-instruction semantics to the shared interpreter-backed helper. With the
    /// feature off the tier has no backend, so selecting it returns a typed
    /// [`unsupported`](crate::error::unsupported) error rather than falling back
    /// silently.
    Cranelift,
}

impl Tier {
    /// Every tier the engine knows about, in roster order (interpreter first).
    pub const ALL: [Tier; 3] = [Tier::Interpreter, Tier::BaselineJit, Tier::Cranelift];

    /// The tier's stable name, as reported by [`available_tiers`] and parsed by
    /// [`Tier::from_name`].
    pub fn name(&self) -> &'static str {
        match self {
            Tier::Interpreter => "interpreter",
            Tier::BaselineJit => "baseline-jit",
            Tier::Cranelift => "jit-cranelift",
        }
    }

    /// Parse a tier name back into a [`Tier`], or `None` if the name is unknown.
    ///
    /// The inverse of [`Tier::name`]; an unknown name is `None`, never a default
    /// tier — a typo in a configuration string must not silently pick a tier.
    pub fn from_name(name: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|tier| tier.name() == name)
    }

    /// Whether this tier is compiled into the current build.
    ///
    /// `true` exactly for the tiers [`available_tiers`] lists.
    pub fn is_available(&self) -> bool {
        available_tiers().contains(&self.name())
    }
}

/// A native execution tier for the typed IR.
///
/// A backend owns whatever state compiling the IR needs (a code buffer, per-module
/// caches, …), hence the `&mut self` receiver on [`compile`] — it may be called
/// again for another module and mutates that owned state.
///
/// # Compile once, run many
///
/// Compiling a [`Module`] and running it are **two separate operations**: [`compile`]
/// produces this backend's [`Compiled`](JitBackend::Compiled) artifact, and
/// [`run_compiled`](JitBackend::run_compiled) executes an export against an artifact
/// the caller owns. A caller that runs the same module more than once should
/// [`compile`] once and then call [`run_compiled`](JitBackend::run_compiled) as often
/// as it likes — that is the whole point of the split, since compilation, not
/// execution, dominates the cost of the naive path.
///
/// [`run_export`] is the one-shot convenience built on both: it compiles and
/// immediately runs, and is a **provided** method with a single definition here so
/// no backend can drift from it. A backend implements [`compile`] and
/// [`run_compiled`](JitBackend::run_compiled) and gets the one-shot entry for free.
///
/// The engine never picks a backend implicitly: [`available_tiers`] reports what is
/// compiled in, [`Tier::is_available`] answers for one tier, and
/// [`Machine::run_with_tier`](crate::Machine::run_with_tier) is the entry that
/// selects a tier. A backend that cannot serve a request returns a typed
/// [`beam_common::BeamError`] — the engine never panics, and never falls back to a
/// *different* tier behind the caller's back.
///
/// [`compile`]: JitBackend::compile
/// [`run_compiled`]: JitBackend::run_compiled
/// [`run_export`]: JitBackend::run_export
pub trait JitBackend {
    /// The artifact [`compile`](JitBackend::compile) produces for one [`Module`].
    ///
    /// Left to the backend: a code buffer plus entry table, a lowered module, or a
    /// cache handle. The engine treats it as opaque.
    type Compiled;

    /// The tier's stable name (e.g. `"baseline-jit"`), as returned by
    /// [`available_tiers`].
    fn name(&self) -> &'static str;

    /// Whether this tier is usable in the current build.
    ///
    /// Mirrors [`Tier::is_available`]; a backend may also report `false` at runtime
    /// (e.g. a missing executable-memory facility), which must be a
    /// [`beam_common::BeamError`] at the call site rather than a panic.
    fn available(&self) -> bool;

    /// Compile `module` into this backend's [`Compiled`](JitBackend::Compiled)
    /// artifact.
    ///
    /// Compile latency is a first-class concern (Part II §C/§F): a backend must
    /// either stay bounded or decline with a typed error, never stall the caller.
    fn compile(&mut self, module: &Module) -> Result<Self::Compiled>;

    /// Execute exported `function/arity` against an already-compiled artifact;
    /// yields `x(0)`.
    ///
    /// This is the **compile-once / run-many** entry: `compiled` is the artifact
    /// [`compile`](JitBackend::compile) produced for `module`, and the caller owns it
    /// and keeps it alive for the call (the backend holds no per-run code state —
    /// hence the shared `&self` receiver, which also lets one artifact be run against
    /// several [`Machine`]s without recompiling).
    ///
    /// Setup and semantics are exactly [`run_export`](JitBackend::run_export)'s: the
    /// machine is reset, `x[0..arity)` is seeded from `args`, the bottom frame is
    /// pushed, the module's literals are bound, the export is resolved and the run
    /// proceeds on this tier's native code with the per-instruction interpreter step
    /// for everything the tier does not lower. Only the compilation is skipped.
    ///
    /// `compiled` must have been produced from the same `module` (label/function
    /// indices are module-local); running a mismatched pair is a caller error, not a
    /// diagnosable one, but never a panic — a missing export is still a typed error.
    ///
    /// Failure is return-based: a missing export, an unsupported instruction or a
    /// raising BIF is an `Err`, never a panic.
    fn run_compiled(
        &self,
        compiled: &Self::Compiled,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm>;

    /// Compile `module` and immediately run exported `function/arity`; yields `x(0)`.
    ///
    /// The one-shot convenience over [`compile`](JitBackend::compile) +
    /// [`run_compiled`](JitBackend::run_compiled), provided here so every backend
    /// behaves identically: a caller that will run the module again should call
    /// those two directly and keep the artifact instead of recompiling on every
    /// call.
    ///
    /// The backend owns the entry into its compiled code but borrows the engine's
    /// process state: `machine` supplies the register file / stack / frames and
    /// `ctx` the [`ExecutionContext`] seam (BIF dispatch, heap, fun application).
    /// Failure is return-based: a missing export, an unsupported instruction or a
    /// raising BIF is an `Err`, never a panic.
    fn run_export(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let compiled = self.compile(module)?;
        self.run_compiled(&compiled, machine, module, function, arity, args, ctx)
    }
}

/// The execution tiers compiled into this build.
///
/// Exactly `["interpreter", "baseline-jit"]`, plus `"jit-cranelift"` when the
/// **`jit-cranelift`** feature is enabled. The interpreter is always present and
/// always first; the roster is a prefix of [`Tier::ALL`] mapped to
/// [`Tier::name`].
pub fn available_tiers() -> &'static [&'static str] {
    TIERS
}

/// The roster of tiers compiled into this build (interpreter first).
#[cfg(feature = "jit-cranelift")]
const TIERS: &[&str] = &["interpreter", "baseline-jit", "jit-cranelift"];

/// The roster of tiers compiled into this build (interpreter first).
#[cfg(not(feature = "jit-cranelift"))]
const TIERS: &[&str] = &["interpreter", "baseline-jit"];

/// Execute `function/arity` on the Cranelift hot tier.
///
/// With the **`jit-cranelift`** feature enabled this builds
/// [`cranelift::CraneliftJit`] and runs the export through it (the module is
/// compiled to native code and the export runs on the shared driver loop, with the
/// per-instruction interpreter helper for everything the tier does not inline).
/// Without the feature the backend is not compiled in at all, so this is a typed
/// [`unsupported`](crate::error::unsupported) error — never a panic and never a
/// silent fallback to another tier.
///
/// This is the body of [`Machine::run_with_tier`](crate::Machine::run_with_tier)
/// for [`Tier::Cranelift`]; the two definitions are the feature-on and feature-off
/// halves of the same entry point.
#[cfg(feature = "jit-cranelift")]
pub(crate) fn run_cranelift(
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<Eterm> {
    let mut backend = cranelift::CraneliftJit::new();
    backend.run_export(machine, module, function, arity, args, ctx)
}

/// Execute `function/arity` on the Cranelift hot tier (backend not compiled in).
///
/// The feature-off half of the [`Tier::Cranelift`] entry point; see the
/// feature-on definition for the contract. Without the **`jit-cranelift`**
/// feature the tier has no backend, so this is always a typed
/// [`unsupported`](crate::error::unsupported) error.
#[cfg(not(feature = "jit-cranelift"))]
pub(crate) fn run_cranelift(
    _machine: &mut Machine,
    _module: &Module,
    _function: Atom,
    _arity: u32,
    _args: &[Eterm],
    _ctx: &mut dyn ExecutionContext,
) -> Result<Eterm> {
    Err(crate::error::unsupported("jit-cranelift tier not built"))
}

/// Execute `function/arity` on the Cranelift hot tier, reporting whether the run
/// returned or blocked in a `receive`.
///
/// The outcome-aware counterpart of [`run_cranelift`]: identical compilation and
/// execution, except that a `receive` that finds no message arrives as
/// [`RunOutcome::Wait`] instead of the folded typed [`crate::error::blocked`]. With
/// the **`jit-cranelift`** feature off the tier has no backend, so this is the same
/// typed `unsupported` error (never a panic, never a silent fallback).
#[cfg(feature = "jit-cranelift")]
pub(crate) fn run_cranelift_outcome(
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    cranelift::run_outcome(machine, module, function, arity, args, ctx)
}

/// Execute `function/arity` on the Cranelift hot tier, reporting its outcome
/// (backend not compiled in).
#[cfg(not(feature = "jit-cranelift"))]
pub(crate) fn run_cranelift_outcome(
    _machine: &mut Machine,
    _module: &Module,
    _function: Atom,
    _arity: u32,
    _args: &[Eterm],
    _ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    Err(crate::error::unsupported("jit-cranelift tier not built"))
}

/// Execute the raw entry `label` on the Cranelift hot tier, reporting whether the
/// run returned or blocked in a `receive`.
///
/// The label-addressed counterpart of [`run_cranelift_outcome`]; see it for the
/// contract. With the feature off this is the same typed `unsupported` error.
#[cfg(feature = "jit-cranelift")]
pub(crate) fn run_cranelift_label_outcome(
    machine: &mut Machine,
    module: &Module,
    label: Label,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    cranelift::run_label_outcome(machine, module, label, arity, args, ctx)
}

/// Execute the raw entry `label` on the Cranelift hot tier, reporting its outcome
/// (backend not compiled in).
#[cfg(not(feature = "jit-cranelift"))]
pub(crate) fn run_cranelift_label_outcome(
    _machine: &mut Machine,
    _module: &Module,
    _label: Label,
    _arity: u32,
    _args: &[Eterm],
    _ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    Err(crate::error::unsupported("jit-cranelift tier not built"))
}

/// Resume a suspended machine on the Cranelift hot tier, reporting whether the
/// resumed slice returned or blocked again.
///
/// Backs
/// [`Machine::resume_outcome_with_tier`](crate::Machine::resume_outcome_with_tier)
/// for [`Tier::Cranelift`]: with the **`jit-cranelift`** feature enabled the tier's
/// compiled functions bake in their entry `pc`, so a mid-function suspension address
/// is unserviceable and the tier reports the honest typed
/// [`unsupported`](crate::error::unsupported) error (see
/// [`cranelift::run_resume_outcome`]). Without the feature the backend is not
/// compiled in at all, so this is the same typed `unsupported` error the first-entry
/// Cranelift path reports — never a panic and never a silent fallback to another
/// tier.
#[cfg(feature = "jit-cranelift")]
pub(crate) fn run_cranelift_resume_outcome(
    machine: &mut Machine,
    module: &Module,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    cranelift::run_resume_outcome(machine, module, func, pc, ctx)
}

/// Resume a suspended machine on the Cranelift hot tier (backend not compiled in).
///
/// The feature-off half of the [`Tier::Cranelift`] resume entry point; see the
/// feature-on definition for the contract. Without the **`jit-cranelift`** feature
/// the tier has no backend, so this is always a typed
/// [`unsupported`](crate::error::unsupported) error.
#[cfg(not(feature = "jit-cranelift"))]
pub(crate) fn run_cranelift_resume_outcome(
    _machine: &mut Machine,
    _module: &Module,
    _func: u32,
    _pc: u32,
    _ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    Err(crate::error::unsupported("jit-cranelift tier not built"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;
    use beam_loader::load_file;
    use beam_term::{make_small, signed_value, Atom};

    /// The `beam_loader` oracle fixture (reused, never copied).
    const SLICE_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    );

    /// Resolve a name interned in the module's atom table.
    fn atom(module: &Module, name: &str) -> Atom {
        module
            .atom_table
            .lookup(name)
            .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
    }

    /// The slice BIF hook: `erlang:'+'/2` and `erlang:'-'/2` over small integers.
    fn arith_hook(
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> core::result::Result<Eterm, BeamError> {
        if module != "erlang" || arity != 2 || args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        if !beam_term::is_small(args[0]) || !beam_term::is_small(args[1]) {
            return Err(BeamError::BadArith);
        }
        let (a, b) = (signed_value(args[0]), signed_value(args[1]));
        match function {
            "+" => Ok(make_small(a + b)),
            "-" => Ok(make_small(a - b)),
            _ => Err(BeamError::BadArg),
        }
    }

    fn slice_module() -> Module {
        load_file(SLICE_BEAM).expect("load slice.beam")
    }

    /// Run `slice:add/2` on `tier`.
    fn run_add(tier: Tier) -> Result<Eterm> {
        let module = slice_module();
        let mut machine = crate::Machine::new();
        let mut ctx = crate::MinimalContext::with_bif_hook(Box::new(arith_hook));
        machine.run_with_tier(
            &module,
            atom(&module, "add"),
            2,
            &[make_small(2), make_small(3)],
            &mut ctx,
            tier,
        )
    }

    #[test]
    fn interpreter_is_always_first_and_the_roster_is_a_tier_prefix() {
        let tiers = available_tiers();
        assert_eq!(tiers.first().copied(), Some("interpreter"));
        assert!(!tiers.is_empty());

        // Every roster name parses back to a tier, in `Tier::ALL` order.
        let names: Vec<&str> = Tier::ALL.iter().map(|tier| tier.name()).collect();
        assert_eq!(tiers, &names[..tiers.len()]);
    }

    #[test]
    fn baseline_jit_tier_is_part_of_every_build() {
        assert!(available_tiers().contains(&"baseline-jit"));
        assert_eq!(Tier::from_name("baseline-jit"), Some(Tier::BaselineJit));
        assert!(Tier::BaselineJit.is_available());
    }

    #[test]
    fn cranelift_tier_tracks_the_feature() {
        let has = available_tiers().contains(&"jit-cranelift");
        assert_eq!(has, cfg!(feature = "jit-cranelift"));
        assert_eq!(Tier::Cranelift.is_available(), has);
    }

    #[test]
    fn tier_names_round_trip() {
        for tier in Tier::ALL {
            assert_eq!(Tier::from_name(tier.name()), Some(tier));
        }
        assert_eq!(Tier::Interpreter.name(), "interpreter");
        assert_eq!(Tier::BaselineJit.name(), "baseline-jit");
        assert_eq!(Tier::Cranelift.name(), "jit-cranelift");
        // An unknown name is `None`, never a default tier.
        assert_eq!(Tier::from_name("jit-beamasm"), None);
        assert_eq!(Tier::from_name(""), None);
        assert_eq!(Tier::from_name("Interpreter"), None);
    }

    #[test]
    fn run_with_tier_interpreter_matches_run_export() {
        let module = slice_module();
        let add = atom(&module, "add");
        let args = [make_small(2), make_small(3)];

        let mut direct = crate::Machine::new();
        let mut ctx = crate::MinimalContext::with_bif_hook(Box::new(arith_hook));
        let expected = direct
            .run_export(&module, add, 2, &args, &mut ctx)
            .expect("add(2, 3) via run_export");

        let mut tiered = crate::Machine::new();
        let mut ctx = crate::MinimalContext::with_bif_hook(Box::new(arith_hook));
        let got = tiered
            .run_with_tier(&module, add, 2, &args, &mut ctx, Tier::Interpreter)
            .expect("add(2, 3) via run_with_tier");

        assert_eq!(got, expected);
        assert_eq!(got, make_small(5));
    }

    #[test]
    fn baseline_tier_runs_the_module_on_the_native_backend() {
        // The baseline tier now compiles and runs native code; its observable result
        // must be the interpreter's.
        assert_eq!(run_add(Tier::BaselineJit).expect("add(2, 3)"), make_small(5));
    }

    #[test]
    #[cfg(not(feature = "jit-cranelift"))]
    fn cranelift_tier_is_a_typed_error_not_a_silent_fallback() {
        let err = run_add(Tier::Cranelift).expect_err("no Cranelift backend is built");
        assert!(matches!(err, BeamError::Raise(_)));
    }

    #[test]
    #[cfg(feature = "jit-cranelift")]
    fn cranelift_tier_runs_the_module_on_its_native_backend() {
        // With the feature on the tier compiles the module with Cranelift and runs
        // it; its observable result must be the interpreter's.
        assert_eq!(run_add(Tier::Cranelift).expect("add(2, 3)"), make_small(5));
    }
}
