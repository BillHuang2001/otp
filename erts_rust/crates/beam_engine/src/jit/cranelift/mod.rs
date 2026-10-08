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

//! The optional **Cranelift hot tier**: typed IR → Cranelift IR → native code.
//!
//! This is the backend behind [`Tier::Cranelift`](crate::jit::Tier::Cranelift),
//! compiled in only when the **`jit-cranelift`** Cargo feature is enabled (default
//! **OFF**, so the default build carries no Cranelift dependency and no Cranelift
//! codegen at all — Part II §F/§H keeps the hot tier off the critical path).
//!
//! # What it compiles
//!
//! For every IR [`beam_loader::Function`] of the module, [`compile`] builds **one** Cranelift
//! function with the System V signature
//!
//! ```text
//! extern "C" fn(rt: i64) -> i64
//! ```
//!
//! (`rt` in `rdi`, the block-ABI word in `rax`). Its body is a **pc-block CFG**:
//! one Cranelift basic block per **reachable** `pc` of the IR function, created
//! lazily by a worklist from the function's entry `pc` (so an unreachable `pc`
//! costs nothing), plus one shared "hand the block-ABI word back to the driver"
//! block. Each `pc`-block either
//!
//! 1. **inlines** the IR instruction and transfers control to a successor block —
//!    the metadata no-ops fall through to `pc + 1`, an inlinable `move` writes the
//!    destination x-register slot and falls through to `pc + 1`, an inlinable
//!    small-int `gc_bif` falls through to `pc + 1` on its fast path, an inlinable
//!    comparison BIF falls through to `pc + 1` after writing its `true`/`false`
//!    word, an inlinable `test` branches to its `fail` label (or falls through to
//!    `pc + 1` on success), and a `jump` whose label resolves **inside this IR
//!    function** jumps straight to the label's `pc` (a `jump` into a *different* IR
//!    function returns the encoded `(func, pc)` word to the driver); or
//! 2. calls the shared per-instruction helper
//!    `jit_step_one(rt, FUNC_INDEX, pc)` — the **same** interpreter-backed helper
//!    the baseline tier uses
//!    ([`baseline::jit_step_one`](crate::jit::baseline::jit_step_one) is literally
//!    [`Machine::step_instruction`](crate::Machine)) — and dispatches on the word
//!    it returned, **exactly as the driver does**: a negative sentinel
//!    (`HALT`/`ERROR`), or a target in a **different** IR function, is returned to
//!    the driver unchanged; a **same-function** target `(this_func, p)` whose `p`
//!    is one of the CFG's candidates jumps to `p`'s block.
//!
//! Control flow *within* a function is therefore native, while the semantics of
//! every single instruction stay the interpreter's — the results are
//! **interpreter-identical by construction**. This is the tier's documented
//! **cheap-deopt boundary**: every instruction the tier does not inline is executed
//! by the shared helper, and the driver falls back to one interpreter step for any
//! address no compiled function starts at.
//!
//! [`compile`]: JitBackend::compile
//!
//! # The continuation candidate set
//!
//! A helper block's `Step::Goto { func, pc }` is a **runtime** value, so the block
//! cannot statically pick a target block. A same-function continuation is always
//! either the label map's target of *some* instruction in the function (every
//! branch/call/fail target in the IR is a [`beam_loader::Label`]) or the return address a
//! `call` stored in the frame's CP slot — which the engine sets to `pc + 1` of the
//! call site. [`codegen::candidate_pcs`] therefore collects **the label positions plus one
//! past every non-tail call site**; the helper block compares the runtime `pc`
//! against that set and jumps to the matching block. The set is a *superset* of the
//! reachable same-function continuations, and a target outside it (there is none
//! today) is handed back to the driver, which takes the interpreter's exact step —
//! never a wrong result.
//!
//! # What it does **not** inline
//!
//! Everything outside the set above: `func_info` (the failed-clause raise, which
//! must **not** be a no-op — see the interpreter's `Instr::FuncInfo` arm), `swap`,
//! every call/return/frame instruction, every `test` shape outside
//! [`inline::inline_test`]'s provably-exact subset (see that function), `select_*`,
//! every BIF outside the small-int `erlang:'+'/2`/`'-'/2`/`'*'/2` fast path and the
//! eight comparison BIFs [`compare::inline_comparison`] recognises, a `move` with a
//! `Y` (or other non-x) operand or an x-register index past the engine's
//! `MAX_REGISTER` bound, and a `jump` to a label the module does not define (so the
//! interpreter's exact `bad_register` / `unknown_label` error surfaces). Each of
//! those runs through `jit_step_one`.
//!
//! # The inlined `test` subset
//!
//! `Instr::Test` is a large share of the helper traffic in real modules, so the
//! tier inlines the shapes whose result is a **word compare** — provably identical
//! to the interpreter's [`cmp_arith`](beam_term::cmp_arith)/[`cmp`](beam_term::cmp)
//! — and deopts for every other shape:
//!
//! * the word-level type tests `is_nil`, `is_atom`, `is_list`, `is_nonempty_list`
//!   and `is_boolean` (the last against the module's `true`/`false` atom words);
//! * `is_eq_exact`/`is_ne_exact` over two x-registers, guarded as untagged small
//!   ints (`(w & 0xF) == 0xF`) so a plain word compare *is* `cmp`'s exact equality,
//!   and over any shape with at least one constant-immediate operand;
//! * `is_eq`/`is_ne`/`is_lt`/`is_ge` when at least one operand is a
//!   constant-immediate (a small int, an atom or `nil`) or, for the non-exact kinds,
//!   both are x-registers — after guarding the register operand(s) as untagged small
//!   ints so the *tagged-word* signed order is the interpreter's
//!   value/arithmetic order.
//!
//! `fail == NO_FAIL` (which *raises*), an unresolved fail label, a `Y`/float/literal
//! operand, a bignum constant, and every other `TestKind` (`is_integer`, `is_tuple`,
//! `is_function2`, …) stay on `jit_step_one`. A failed small-int guard likewise deopts
//! to the helper, so the fast path can only ever *replace* a result the interpreter
//! would compute identically.
//!
//! # The inlined comparison-BIF subset
//!
//! The eight `erlang` comparison operators are the compiler's clause guards and
//! boolean bodies, emitted as a **non-collecting** `bif`, so [`compare`] lowers the
//! provably word-exact shapes natively (returning the module's own `true`/`false`
//! atom word) and deopts everything else — see [`compare::inline_comparison`] for the
//! rule set, which is the baseline tier's rule for rule.
//!
//! # `unsafe`
//!
//! A designated JIT unsafe zone, like [`crate::jit::baseline`]: it hands
//! [`JitRuntime`] out as a raw pointer and calls into the JIT module's executable
//! memory. The crate root keeps `#![deny(unsafe_code)]`; the module-local
//! `#![allow(unsafe_code)]` below is the third JIT exception. Every `unsafe` block
//! carries a `// SAFETY:` rationale.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::mem;

use beam_common::Result;
use beam_loader::{Atom, Label, Module, TestKind};
use beam_term::Eterm;
use cranelift_frontend::FunctionBuilderContext;
use cranelift_jit::JITModule;
use cranelift_module::ModuleError;

use crate::context::ExecutionContext;
use crate::error;
use crate::jit::baseline::{
    run_native, run_native_label_outcome, run_native_outcome, JitRuntime, NativeCode,
};
use crate::jit::JitBackend;
use crate::machine::{Machine, RunOutcome};

mod codegen;
mod compare;
mod inline;
mod report;
mod shape;

#[cfg(test)]
mod tests;

pub use inline::SmallArith;

use codegen::{compile_module, NativeFunc};

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------
/// The optional Cranelift hot tier.
///
/// The artifact it compiles ([`CompiledFunctions`]) is caller-owned, so a module can
/// be compiled once and run many times through [`JitBackend::run_compiled`]; the
/// one-shot [`JitBackend::run_export`] (`run_with_tier` creates a fresh backend per
/// run) compiles and runs in one call. The backend owns the reusable
/// [`FunctionBuilderContext`], which Cranelift requires as scratch space while
/// lowering a function.
pub struct CraneliftJit {
    /// Scratch space reused across the module's functions (Cranelift clears it
    /// after every [`FunctionBuilder`] completes).
    builder_context: FunctionBuilderContext,
}

impl CraneliftJit {
    /// A Cranelift backend ready to compile and run a module.
    pub fn new() -> CraneliftJit {
        CraneliftJit {
            builder_context: FunctionBuilderContext::new(),
        }
    }
}

impl Default for CraneliftJit {
    fn default() -> CraneliftJit {
        CraneliftJit::new()
    }
}
impl JitBackend for CraneliftJit {
    type Compiled = CompiledFunctions;

    fn name(&self) -> &'static str {
        "jit-cranelift"
    }

    fn available(&self) -> bool {
        true
    }

    /// Compile every IR function of `module` into a Cranelift function.
    fn compile(&mut self, module: &Module) -> Result<CompiledFunctions> {
        compile_module(module, &mut self.builder_context)
    }

    /// Run exported `function/arity` through the driver loop on an already-compiled
    /// artifact.
    ///
    /// The artifact ([`CompiledFunctions`]) is caller-owned and kept alive by the
    /// caller for the call; the backend holds no per-run code state, so one artifact
    /// can be run any number of times without recompiling. [`JitBackend::run_export`]
    /// is the provided one-shot convenience over `compile` + this.
    fn run_compiled(
        &self,
        compiled: &CompiledFunctions,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let code = CompiledEntries { compiled };
        run_native(machine, module, function, arity, args, ctx, &code)
    }
}

/// Compile `module` and run exported `function/arity`, reporting whether the run
/// returned or blocked in a `receive`.
///
/// The outcome-aware sibling of [`JitBackend::run_export`]: the artifact is compiled
/// and immediately run through the shared outcome-aware driver
/// ([`run_native_outcome`](crate::jit::baseline::run_native_outcome)), so a `receive`
/// that finds no message arrives as [`RunOutcome::Wait`] with the interpreter's own
/// resume address instead of the folded typed `blocked` error. Everything else is
/// identical, including the per-instruction deopt to the shared interpreter helper.
pub(crate) fn run_outcome(
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(module)?;
    let code = CompiledEntries {
        compiled: &compiled,
    };
    run_native_outcome(machine, module, function, arity, args, ctx, &code)
}

/// Compile `module` and run the raw entry `label`, reporting whether the run
/// returned or blocked in a `receive`.
///
/// The label-addressed counterpart of [`run_outcome`]: the shared outcome-aware
/// label driver ([`run_native_label_outcome`](crate::jit::baseline::run_native_label_outcome))
/// resolves `label` against the module-wide label map and enters the compiled code
/// wherever a function's entry `pc` matches. A label that is not a compiled
/// function's entry simply stays on the driver's interpreter step — the documented
/// per-instruction deopt, never a wrong result.
pub(crate) fn run_label_outcome(
    machine: &mut Machine,
    module: &Module,
    label: Label,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(module)?;
    let code = CompiledEntries {
        compiled: &compiled,
    };
    run_native_label_outcome(machine, module, label, arity, args, ctx, &code)
}

/// The artifact [`CraneliftJit::compile`] produces: the Cranelift [`JITModule`]
/// that owns the executable mapping plus the per-function native entries.
///
/// The entries are raw pointers into the JIT module's code memory, so a
/// `CompiledFunctions` must outlive every call into it and is neither `Send` nor
/// `Sync` (the JIT module frees the mapping when dropped). The module field exists
/// to keep that mapping alive; nothing reads it after compilation.
pub struct CompiledFunctions {
    /// The JIT module whose executable mapping every entry points into.
    ///
    /// Never read after compilation: it is held only to **own** the mapping the
    /// entries point into (Cranelift frees it when the module drops), hence the
    /// leading underscore that tells the dead-code lint so.
    _module: JITModule,
    /// The function index → finalized native entry point.
    entries: HashMap<u32, *const u8>,
    /// The function index → the `pc` its compiled function starts at.
    ///
    /// A compiled function bakes its entry `pc` in, so it may only be entered at
    /// that address (the driver checks it; see [`CompiledEntries::enter`]).
    entry_pc: HashMap<u32, u32>,
    /// The highest x-register index any inlined instruction addresses directly in
    /// `machine.x`.
    ///
    /// The inlined `move`/`test` fast paths read and write `[x_regs + x*8]`
    /// **without a bounds check**, so the driver refuses native entry when the
    /// running machine's register file is not at least this long (see
    /// [`CompiledEntries::enter`]) — the same belt-and-braces guard the baseline
    /// tier uses.
    highest_native_register: Option<u32>,
    /// The `test` kinds the tier inlined natively (deduped, in first-inlined
    /// order).
    ///
    /// An artifact-level observation, not a behaviour: it is what lets a test
    /// assert that a `test` really took the **inline** path rather than silently
    /// falling through to `jit_step_one` (see [`inlined_test_kinds`]).
    ///
    /// [`inlined_test_kinds`]: CompiledFunctions::inlined_test_kinds
    inlined_tests: Vec<TestKind>,
    /// The small-int `gc_bif` ops the tier inlined natively (deduped, in
    /// first-inlined order).
    ///
    /// The same kind of artifact-level observation as [`inlined_tests`] — the
    /// evidence that the arithmetic fast path was compiled (see
    /// [`inlined_gc_bif_ops`]).
    ///
    /// [`inlined_tests`]: CompiledFunctions::inlined_tests
    /// [`inlined_gc_bif_ops`]: CompiledFunctions::inlined_gc_bif_ops
    inlined_gc_bifs: Vec<SmallArith>,
    /// The comparison-BIF operators the tier inlined natively (deduped, in
    /// first-inlined order), named by their `erlang` BIF spelling (`"=:="`, `"<"`, …).
    ///
    /// The same kind of artifact-level observation as [`inlined_tests`] and
    /// [`inlined_gc_bifs`] — the evidence that the comparison fast paths (see
    /// `src/jit/cranelift/compare.rs`) were compiled rather than silently deopted (see
    /// [`inlined_comparison_ops`]).
    ///
    /// [`inlined_tests`]: CompiledFunctions::inlined_tests
    /// [`inlined_gc_bifs`]: CompiledFunctions::inlined_gc_bifs
    /// [`inlined_comparison_ops`]: CompiledFunctions::inlined_comparison_ops
    inlined_comparisons: Vec<&'static str>,
    /// How many `select_val` instructions the tier inlined natively.
    ///
    /// The same kind of artifact-level observation as [`inlined_tests`] — the
    /// evidence that the exact-value-switch fast path was compiled (see
    /// [`inlined_select_val_count`]).
    ///
    /// [`inlined_tests`]: CompiledFunctions::inlined_tests
    /// [`inlined_select_val_count`]: CompiledFunctions::inlined_select_val_count
    inlined_select_vals: usize,
    /// The tuple-read kinds the tier inlined natively (deduped, in first-inlined
    /// order).
    ///
    /// Same artifact-level observation: the names are the `ops.tab` spellings of the
    /// tuple reads that took the **inline** path (`get_tuple_element` /
    /// `select_tuple_arity`); a shape that deopted names nothing (see
    /// [`inlined_tuple_read_kinds`]).
    ///
    /// [`inlined_tuple_read_kinds`]: CompiledFunctions::inlined_tuple_read_kinds
    inlined_tuple_reads: Vec<&'static str>,
    /// The list-read kinds the tier inlined natively (deduped, in first-inlined
    /// order): `get_list` / `get_hd` / `get_tl`.
    ///
    /// The list counterpart of [`inlined_tuple_reads`].
    ///
    /// [`inlined_tuple_reads`]: CompiledFunctions::inlined_tuple_reads
    inlined_list_reads: Vec<&'static str>,
}

impl CompiledFunctions {
    /// The native entry point compiled for function index `func`, if any.
    pub fn entry(&self, func: u32) -> Option<*const u8> {
        self.entries.get(&func).copied()
    }

    /// The `pc` the compiled function for `func` starts at, if it was compiled.
    pub fn entry_pc(&self, func: u32) -> Option<u32> {
        self.entry_pc.get(&func).copied()
    }

    /// The number of compiled functions.
    pub fn function_count(&self) -> usize {
        self.entries.len()
    }

    /// The highest x-register index any inlined instruction addresses directly in
    /// `machine.x`, if any instruction was inlined.
    pub fn highest_native_register(&self) -> Option<u32> {
        self.highest_native_register
    }

    /// The `test` kinds this artifact inlined natively, deduped and in
    /// first-inlined order (empty when the tier inlined no `test`).
    ///
    /// Read-only introspection: it names **which** `TestKind`s the compile actually
    /// took the inline path for, so a test can prove the fast path ran instead of
    /// silently deopting to `jit_step_one` (agreeing results alone would not).
    pub fn inlined_test_kinds(&self) -> &[TestKind] {
        &self.inlined_tests
    }

    /// The small-int `gc_bif` ops this artifact inlined natively, deduped and in
    /// first-inlined order (empty when the tier inlined no `gc_bif`).
    ///
    /// Read-only introspection, the arithmetic counterpart of
    /// [`inlined_test_kinds`](CompiledFunctions::inlined_test_kinds): it names
    /// **which** of the two BIFs the compile took the inline fast path for, so a
    /// test can prove the fast path ran instead of silently deopting to
    /// `jit_step_one` (agreeing results alone would not). Each entry carries its
    /// own [`name`](SmallArith::name).
    pub fn inlined_gc_bif_ops(&self) -> &[SmallArith] {
        &self.inlined_gc_bifs
    }

    /// The comparison-BIF operators this artifact inlined natively, deduped and in
    /// first-inlined order (empty when the tier inlined none). Each entry is the
    /// operator's `erlang` BIF spelling: `"=:="`, `"=/="`, `"=="`, `"/="`, `"<"`,
    /// `"=<"`, `">"` or `">="`.
    ///
    /// Read-only introspection, the comparison counterpart of
    /// [`inlined_gc_bif_ops`](CompiledFunctions::inlined_gc_bif_ops): it names **which**
    /// operators the compile actually took the inline path for, so a test can prove
    /// the fast path ran instead of silently deopting to `jit_step_one` (agreeing
    /// results alone would not).
    pub fn inlined_comparison_ops(&self) -> &[&'static str] {
        &self.inlined_comparisons
    }

    /// How many `select_val` instructions this artifact inlined natively (zero when
    /// the tier inlined none).
    ///
    /// Read-only introspection, the exact-value-switch counterpart of
    /// [`inlined_test_kinds`](CompiledFunctions::inlined_test_kinds): a module that
    /// silently deopted would report zero here.
    pub fn inlined_select_val_count(&self) -> usize {
        self.inlined_select_vals
    }

    /// The tuple-read kinds this artifact inlined natively, deduped and in
    /// first-inlined order (empty when the tier inlined none). The names are the
    /// `ops.tab` spellings `"get_tuple_element"` / `"select_tuple_arity"`.
    pub fn inlined_tuple_read_kinds(&self) -> &[&'static str] {
        &self.inlined_tuple_reads
    }

    /// The list-read kinds this artifact inlined natively, deduped and in
    /// first-inlined order (empty when the tier inlined none). The names are the
    /// `ops.tab` spellings `"get_list"` / `"get_hd"` / `"get_tl"`.
    pub fn inlined_list_read_kinds(&self) -> &[&'static str] {
        &self.inlined_list_reads
    }
}
/// The Cranelift tier's [`NativeCode`] lookup: a compiled function is entered
/// exactly at its entry `pc`.
struct CompiledEntries<'a> {
    /// The compiled functions of the module being run.
    compiled: &'a CompiledFunctions,
}

impl NativeCode for CompiledEntries<'_> {
    fn enter(&self, rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> Option<i64> {
        // The compiled function starts at the IR function's entry `pc`; entering it
        // anywhere else would run the wrong code, so every other address stays on the
        // driver's interpreter step (the documented per-instruction deopt).
        if self.compiled.entry_pc(func)? != pc {
            return None;
        }
        // An inlined `move` addresses `machine.x` unchecked, so a machine whose
        // register file is shorter than the highest natively addressed index cannot
        // enter native code; such a run stays on the interpreter step.
        if let Some(highest) = self.compiled.highest_native_register {
            // SAFETY: `rt` is the live driver-owned runtime block and its `machine`
            // pointer points at the driver's live `Machine`; the driver holds no
            // `&mut` alias while the native call is in flight.
            let x_len = unsafe { (*(*rt).machine).x.len() };
            if x_len <= highest as usize {
                return None;
            }
        }
        let entry = self.compiled.entry(func)?;
        // SAFETY: `entry` is the finalized entry of a function declared with the
        // `NativeFunc` signature (`extern "C" fn(i64) -> i64`) in the JIT module that
        // owns it, so the pointer is callable with exactly that ABI; `rt` is the live
        // driver-owned runtime block, passed as the `i64` first parameter (a pointer
        // and an `i64` use the same argument register under the System V AMD64 ABI).
        let code: NativeFunc = unsafe { mem::transmute(entry) };
        Some(code(rt as i64))
    }
}
// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------
/// Turn a Cranelift module error into the engine's documented typed error.
///
/// Cranelift failures (an unsupported host ISA, a declaration/signature clash, a
/// codegen or allocation failure) are not a BEAM exception: they are reported with
/// the engine's `unsupported` sentinel, carrying Cranelift's own message, rather
/// than invented.
pub(super) fn module_error(err: ModuleError) -> beam_common::BeamError {
    error::unsupported(&format!("cranelift jit: {err}"))
}
