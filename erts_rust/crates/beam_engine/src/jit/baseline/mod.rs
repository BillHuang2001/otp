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

//! The native **baseline JIT** tier: typed IR → x86-64 basic blocks + driver loop.
//!
//! This is the backend behind [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit)
//! and the first [`JitBackend`] the engine actually builds. It is the *mechanism*,
//! not an optimiser: every basic block of the module is lowered into a real
//! `extern "C"` native function, but only the cheapest instructions are emitted as
//! machine code — everything else is executed by calling back into the
//! interpreter's per-instruction step. The result is a genuine native execution
//! path with **interpreter-identical semantics** (Part II §F: correctness first,
//! the hot-tier optimisation follows in a later task).
//!
//! # The block ABI
//!
//! Each compiled basic block is
//!
//! ```text
//! extern "C" fn(rt: *mut JitRuntime) -> i64
//! ```
//!
//! It reads and writes the process state through `rt` and returns the **next
//! control-flow target** as one `i64`:
//!
//! * `(func as i64) << 32 | (pc as i64)` — a normal continuation: keep executing at
//!   code offset `pc` of the function at index `func` (see [`encode_target`]). The
//!   encoding reserves the sign bit, so a valid target must satisfy
//!   `func < 2^31` (every real module does).
//! * [`HALT`] (`-1`) — the run finished; the driver yields `machine.finish()`
//!   (`x(0)`).
//! * [`ERROR`] (`-2`) — a raise (or any other typed error) was recorded in
//!   `rt.error`; the driver returns it.
//!
//! The driver ([`run_compiled`]) decodes that word, looks the target up in the
//! block-entry table and calls the next block, falling back to a single
//! interpreter step for any address no block was compiled for.
//!
//! # The runtime block (`JitRuntime`)
//!
//! [`JitRuntime`] is the one argument a block receives: a `#[repr(C)]` bag of raw
//! pointers at the live driver-owned objects plus the helper error slot. Generated
//! code loads exactly two of its fields: `x_regs` (offset [`RUNTIME_XREGS_DISP`]) —
//! the base of the machine's x-register array — and, through the helper scaffolding,
//! nothing else; the remaining fields are read by the Rust-side [`jit_step_one`].
//! `machine` (offset [`RUNTIME_MACHINE_DISP`]) is the [`Machine`] the helper needs;
//! a block never touches it, because `[base + n*8]` addressing must be based on the
//! **array** (`machine.x`), not on the address of the `Vec` field inside a
//! non-`repr(C)` Rust struct.
//!
//! # Register allocation and spill-across-blocks
//!
//! A block caches up to [`CACHE_REGISTERS`]` .len()` (five) x-registers in
//! callee-saved machine registers (`rbp`, `r12`–`r15`); `rbx` is reserved for the
//! machine base pointer (loaded once from `rt.x_regs`) and the `rt` argument itself
//! lives in the 24-byte stack slot the prologue reserves (the byte count that keeps
//! `rsp` 16-byte aligned after the six callee-saved pushes). The prologue pushes
//! every callee-saved register it touches (so the Rust caller's state is preserved),
//! loads the cached x-registers out of `machine.x`, and the epilogue **spills them
//! back at every block exit**. That is the whole liveness model: within a block the
//! cached values live in registers, and at a block boundary they are always back in
//! `machine.x`, so a helper call / GC / the next block sees the interpreter's exact
//! root set (`x[0..live)` + the whole stack) with no per-frame liveness maps
//! (`../../CONTEXT.md` §4).
//!
//! The cache is **not** a coverage limit. An x-register the allocator could not
//! cache is simply addressed **directly in `machine.x`** (`[rbx + x*8]`) — the same
//! slot the exit spill writes — so a `move`/`swap`/`test`/fast-path `gc_bif`/
//! `select_val` touching more registers than the budget runs natively instead of
//! deopting. That is exact because the cache membership is fixed for the whole
//! block: an x-register is either always in its machine register (and written back
//! at every block exit) or never, so the two representations cannot disagree. The
//! allocation is computed by [`allocate_registers`] from the block's **native touch
//! counts** — the registers its inlinable instructions would reference, weighted by
//! how often they appear and ties broken by ascending index — so the most-used
//! (longest-live-range) registers become resident; the direct `machine.x` accesses
//! are the fill/spill for the rest. The highest x-register index any block of a
//! function touches natively is recorded **per function** on the artifact, so the
//! driver can refuse native entry to a register file that does not cover the function
//! it is about to enter.
//!
//! # What is inlined, what is helper-executed
//!
//! **Inlined** (emitted natively, no interpreter round-trip):
//!
//! * the metadata no-ops (`label`/`line`/`func_info`/`int_code_end`/
//!   `executable_line`/`debug_line`) — nothing at all;
//! * `move Src Dst` where `Dst` is an x-register (`x`/`tr`-annotated) **and**
//!   `Src` is an x-register or a constant (`Integer`/`Character`/`Atom`/`Nil`) —
//!   a `mov` between machine registers, a `mov` of the raw `Eterm` word
//!   (`make_small`/`make_atom`/`NIL`), or — for a register outside the cache budget
//!   — a load/store of that register's `machine.x` slot;
//! * `swap A B` where both operands are x-registers — the same three-`mov` shuffle,
//!   each operand through its own representation (machine register or `machine.x`
//!   slot);
//! * `jump Label` whose target resolves in the module-wide label map — the
//!   `(func, pc)` pair is known at compile time, so it is a constant return word;
//! * the **local tail calls** `call_only Arity Label` (a plain jump:
//!   `$DISPATCH_REL(Dest)`) and `call_last Arity Label Deallocate` (the same jump
//!   after the current frame is popped by `jit_tail_call`, the interpreter's own
//!   checked pop, in the interpreter's `resolve`-then-`pop` order) whose target is a
//!   compiled block's entry — see `emit/link`. A target that is not a compiled
//!   block's entry, and every `call_ext_*`, keep the per-instruction deopt below;
//! * the **guarded small-int arithmetic fast path** for `gc_bif` of
//!   `erlang:'+'/2` and `erlang:'-'/2`: the tagged-word arithmetic is emitted
//!   natively behind a small-int guard + overflow + range check, and *any* case
//!   the guard rejects (a non-small operand, an `i64` overflow, a result outside
//!   `MIN_SMALL..=MAX_SMALL`) jumps straight to `jit_step_one`, which runs the
//!   real BIF and produces the bignum/float/error result natively unreachable
//!   here. This is the tier's one optimisation (`../../CONTEXT.md` §7); it is
//!   conservative by construction — it can only ever produce a result the helper
//!   would have produced — see [`try_inline_gc_bif`]. Its operands and its
//!   destination may be registers beyond the cache budget (read/written through
//!   `machine.x`).
//!
//! **Helper-executed** (`jit_step_one`, literally the interpreter's
//! [`Machine::step_instruction`](crate::Machine)): **every other instruction**, plus
//! a `move`/`swap` with a `Y` (or other non-x) operand or a register index outside
//! the engine's [`MAX_REGISTER`](crate::machine::MAX_REGISTER) bound, a `jump` to a
//! label the module does not define (so the interpreter's exact `unknown_label`
//! error surfaces rather than an invented one), and a `gc_bif` whose BIF is outside
//! the fast path's two (or one with an un-addressable operand).
//!
//! A helper-executed instruction does **not** end the block: after the helper has
//! run exactly that instruction (`Step::Next`) the block refills its cached
//! x-registers and carries on natively at the next instruction, so native coverage
//! is whole-block. Only an outcome other than "advanced by one" (`Goto`/`Halt`/
//! `Wait`/`Err`) returns to the driver with that word. This per-instruction deopt is
//! what gives the tier full opcode coverage with interpreter-identical semantics on
//! day one; see `emit` for the emitted sequence.
//!
//! # Two compilation layouts (eager / lazy)
//!
//! [`CompiledModule`] is *what* the tier runs; *how* it was lowered is one of two
//! layouts (see [`code`]):
//!
//! * **Eager** ([`CompiledModule::eager`], produced by [`compile_module`]): every
//!   function is lowered up front into **one** sealed [`CodeBuffer`], and the
//!   seal-time `Linker` links a taken branch to a successor block of **another**
//!   function as a direct jump. This is what [`JitBackend::compile`] and
//!   [`CodeCache::get_or_compile`](crate::jit::cache::CodeCache::get_or_compile)
//!   produce, and the layout every whole-module introspection query answers for.
//! * **Lazy** ([`CompiledModule::lazy`], produced by
//!   [`CodeCache::compiled_handle`](crate::jit::cache::CodeCache::compiled_handle) —
//!   the path behind [`run_baseline_cached*`](crate::jit::cache)): the artifact starts
//!   with **nothing** compiled, and each function is lowered into its **own** sealed
//!   [`CodeBuffer`] on that function's first entry, through
//!   [`NativeCode::enter`] → [`CompiledModule::native_entry`]. A fresh process
//!   therefore pays only for the functions it actually enters. Because a lazy
//!   function's block map covers just that one function, a branch to another
//!   function's block is absent from it: the `Linker` emits the target's `(func, pc)`
//!   word and the driver compiles that function on entry.
//!
//! [`CodeBuffer`]: crate::jit::emitter::CodeBuffer
//!
//! # `unsafe`
//!
//! This is a designated JIT unsafe zone: it reconstructs `&mut Machine` /
//! `&mut dyn ExecutionContext` from [`JitRuntime`]'s raw pointers and calls into
//! sealed executable memory. The crate root keeps `#![deny(unsafe_code)]`; the
//! module-local `#![allow(unsafe_code)]` below is the second JIT exception
//! (alongside [`crate::jit::emitter`]). Every `unsafe` block carries a
//! `// SAFETY:` rationale.
#![allow(unsafe_code)]

use crate::interpreter::LabelMap;
use std::mem;
use std::rc::Rc;
use std::sync::Arc;

use beam_common::Result;
use beam_loader::{Atom, Label, Module};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter;
use crate::jit::JitBackend;
use crate::machine::{Machine, RunOutcome};
use crate::Step;

mod code;
mod emit;
mod receive;
mod runtime;
mod test_helper;

#[cfg(test)]
mod counters;

#[cfg(test)]
mod tests;

// The block ABI lives in `runtime` and the artifact in `code`; re-export their names
// here so every item path outside this module (`crate::jit::baseline::…`) stays
// exactly what it was.
pub(crate) use emit::compile_module;
pub use code::CompiledModule;
pub use runtime::{decode_target, decode_wait, encode_target, encode_wait, ENTER, ERROR, HALT, WAIT_BIT};
pub(crate) use runtime::{jit_ext_call, jit_set_position, jit_step_one, JitRuntime};

use runtime::NativeBlock;
// ---------------------------------------------------------------------------
// The block ABI
// ---------------------------------------------------------------------------

/// A native tier's compiled-code lookup, consulted once per driver iteration by
/// [`run_native`].
///
/// A tier implements this for whatever it compiled: [`run_native`] hands it the
/// current `(func, pc)` and the live [`JitRuntime`], and the tier either runs that
/// address natively — returning the block-ABI word the code produced — or returns
/// `None`, which makes the driver take exactly one interpreter step. Returning
/// `None` must not touch the runtime: it is the "this tier has no code here"
/// answer, and it is what keeps an uncovered address on the interpreter's exact
/// semantics.
pub(crate) trait NativeCode {
    /// Run `(func, pc)` natively if this tier compiled it for that address.
    ///
    /// `module` is the artifact's own (active) module — the one the driver has bound
    /// for this context — which a lazy artifact needs so it can compile a function on
    /// its first entry ([`CompiledModule::native_entry`]).
    fn enter(&self, rt: *mut JitRuntime<'_>, module: &Module, func: u32, pc: u32) -> Option<i64>;

    /// The compiled blocks this tier holds for `module` — a module the driver has
    /// switched **into** (a non-entry module context) — or `None` when it has none.
    ///
    /// A cross-module transfer ([`Step::Enter`]) executes in a module other than the
    /// one this tier was entered for, so the driver needs that module's block table to
    /// run it natively; an owned handle ([`Rc`]) is returned rather than a borrow
    /// because the tier's store may be touched again while the driver holds it.
    ///
    /// The **default is `None`**, so an implementor that predates this method keeps
    /// the entry-only behaviour: a cross-module transfer still *executes* (the driver
    /// runs it on its per-instruction interpreter step, exactly like any address the
    /// tier compiled no block for), it simply is not accelerated.
    fn module_blocks(&self, module: &Arc<Module>) -> Option<Rc<CompiledModule>> {
        let _ = module;
        None
    }

    /// The module-wide label map this tier already holds for `module`, if any.
    ///
    /// A compiled artifact owns the label map it was built with
    /// ([`CompiledModule::labels`]), so a driver run of it resolves its entry label and
    /// every import against that one instead of rebuilding a per-module value on every
    /// run. The **default is `None`** ("I hold none"), so an implementor that predates
    /// this method keeps the documented per-run build and stays byte-identical.
    fn labels(&self, module: &Module) -> Option<&interpreter::LabelMap> {
        let _ = module;
        None
    }
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// The native baseline tier.
///
/// A zero-sized selector: it holds no per-run code state, so the artifact it
/// compiles ([`CompiledModule`]) is caller-owned and can be run any number of times
/// through [`JitBackend::run_compiled`] without recompiling. The one-shot
/// [`JitBackend::run_export`] compiles once and runs immediately; a caller that runs
/// a module repeatedly either uses the two steps directly with a caller-held artifact
/// or goes through [`Machine::run_with_tier`](crate::Machine::run_with_tier), which
/// reuses the per-thread cross-run cache ([`crate::jit::cache`]) so an identical
/// second run does not recompile. Inlining quality is *performance* work deferred to a
/// later task — this is the mechanism and its correctness.
#[derive(Clone, Copy, Debug, Default)]
pub struct BaselineJit;

impl BaselineJit {
    /// A baseline backend ready to compile and run a module.
    pub fn new() -> BaselineJit {
        BaselineJit
    }
}

impl JitBackend for BaselineJit {
    type Compiled = CompiledModule;

    fn name(&self) -> &'static str {
        "baseline-jit"
    }

    fn available(&self) -> bool {
        true
    }

    /// Compile every basic block of `module` into native code.
    fn compile(&mut self, module: &Module) -> Result<CompiledModule> {
        compile_module(module)
    }

    /// Run exported `function/arity` through the driver loop on an already-compiled
    /// artifact.
    ///
    /// The artifact is caller-owned and the backend holds no per-run code state, so
    /// one [`CompiledModule`] can be run any number of times (on fresh machines)
    /// without recompiling; [`JitBackend::run_export`] is the provided one-shot
    /// convenience over `compile` + this.
    fn run_compiled(
        &self,
        compiled: &CompiledModule,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        run_compiled(compiled, machine, module, function, arity, args, ctx)
    }
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// Run exported `function/arity` through the compiled blocks.
///
/// The native lookup is the compiled block table: any compiled `(func, pc)` is
/// entered, and every other address stays on the interpreter step. See
/// [`run_native`] for the shared driver the two native tiers run on.
pub(crate) fn run_compiled(
    compiled: &CompiledModule,
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<Eterm> {
    let code = block_entries(compiled);
    run_native(machine, module, function, arity, args, ctx, &code)
}

/// Run exported `function/arity` through the compiled blocks, reporting whether the
/// run returned or blocked in a `receive`.
///
/// The outcome-aware counterpart of [`run_compiled`]: identical setup and semantics,
/// except that a `receive` that finds no message arrives as
/// [`RunOutcome::Wait`](crate::RunOutcome::Wait) instead of the folded typed
/// [`crate::error::blocked`]. See [`run_native_outcome`].
pub(crate) fn run_compiled_outcome(
    compiled: &CompiledModule,
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    let code = block_entries(compiled);
    run_native_outcome(machine, module, function, arity, args, ctx, &code)
}

/// Run a raw entry `label` through the compiled blocks, reporting whether the run
/// returned or blocked.
///
/// The label-addressed counterpart of [`run_compiled_outcome`]: the machine is
/// reset and seeded exactly as [`Machine::run_label`](crate::Machine) does (the label
/// is resolved against the module-wide label map, so it may live in any function),
/// then the shared driver loop runs. A label the module does not define is the
/// interpreter's own typed `unknown_label` error.
pub(crate) fn run_compiled_label_outcome(
    compiled: &CompiledModule,
    machine: &mut Machine,
    module: &Module,
    label: Label,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    let code = block_entries(compiled);
    run_native_label_outcome(machine, module, label, arity, args, ctx, &code)
}

/// Resume a suspended machine through the compiled blocks, reporting whether the
/// resumed slice returned or blocked again.
///
/// This is the **no-reset** counterpart of [`run_compiled_outcome`]: the machine is
/// the one a [`RunOutcome::Wait`](crate::RunOutcome::Wait) left suspended (its
/// registers, Erlang stack and every call frame survive), and the driver re-enters
/// at the reported `(func, pc)` — the address the suspension named — instead of
/// resetting and seeding an export. It is what
/// [`Machine::resume_outcome_with_tier`](crate::Machine::resume_outcome_with_tier)
/// runs for [`Tier::BaselineJit`](crate::jit::Tier::BaselineJit), so a slice that
/// suspended on the native tier resumes on it too.
///
/// It mirrors [`Machine::resume_outcome`](crate::Machine::resume_outcome)'s body
/// exactly — the shared per-run prologue
/// ([`Machine::enter_run`](crate::Machine), idempotent for a resume) followed by a
/// driver loop — so the semantics are the interpreter's: the same module-wide label
/// map, the same shared interpreter step ([`jit_step_one`]) for anything a block
/// does not inline, and the same suspension reporting. The one difference is that a
/// compiled block is entered for an address it covers; every other address stays on
/// the documented per-instruction deopt. Because it shares [`drive`], the A.8
/// tracing gate is latched here too.
///
/// Unlike [`run_native_outcome`] it **does not** call [`enter_machine`] (which
/// resets): a resume must preserve the suspended process. It does zero the reduction
/// counter at the slice start, exactly as
/// [`Machine::drive`](crate::Machine) does for the interpreter entry, so the resumed
/// slice's [`Machine::reductions`](crate::Machine::reductions) matches the
/// interpreter's.
///
/// `module` is the run's **entry** module (context `0`). A process suspended
/// **inside a cross-module callee** (a non-zero module context) is served the same
/// way the interpreter's resume serves it ([`Machine::resume_outcome`]): the driver
/// reads the context the process blocked in from
/// [`Machine::current_context`](crate::Machine::current_context), lands in it
/// ([`switch_into`] re-binds the callee's module, label map and literal area and
/// resolves its compiled blocks through [`NativeCode::module_blocks`]), and re-enters
/// the callee's own `(func, pc)`. The entry module's compiled blocks are not entered
/// in that context; every address the tier has no block for stays on the documented
/// per-instruction deopt.
pub(crate) fn run_compiled_resume_outcome(
    compiled: &CompiledModule,
    machine: &mut Machine,
    module: &Module,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<RunOutcome> {
    // One resume is one scheduling slice: start the reduction count at zero, exactly
    // as `Machine::drive` does for the interpreter's resume (`run_loop` -> `drive`).
    machine.reset_reductions();
    machine.enter_run(module, func);
    // The artifact already holds the module-wide label map (built once at creation),
    // so a resume reuses it instead of rebuilding it per resume.
    let labels = compiled.labels();
    let code = block_entries(compiled);
    drive(machine, module, labels, func, pc, ctx, &code)
}

/// The baseline tier's [`NativeCode`] lookup for `compiled`.
///
/// The register-file guard is applied at entry time by [`enter_compiled`] (from the
/// bound [`CompiledModule::native_entry`] returns), and the bound it uses depends on the
/// artifact's layout: an **eager** artifact is guarded by the **whole-module** highest
/// natively-touched register (its seal-time links may tail-call across functions inside
/// the one shared mapping), while a **lazy** artifact is guarded by the bound of the
/// single function being entered (a cross-function transfer round-trips through the
/// driver and is re-checked there).
fn block_entries(compiled: &CompiledModule) -> BlockEntries<'_> {
    BlockEntries { compiled }
}

/// The baseline tier's [`NativeCode`]: any compiled basic block may be entered.
struct BlockEntries<'a> {
    /// The compiled block table the addresses are looked up in.
    compiled: &'a CompiledModule,
}

impl NativeCode for BlockEntries<'_> {
    fn enter(&self, rt: *mut JitRuntime<'_>, module: &Module, func: u32, pc: u32) -> Option<i64> {
        enter_compiled(self.compiled, module, rt, func, pc)
    }

    /// A cross-module callee is not part of the caller-owned artifact, so the tier's
    /// **shared** compiled-block store is consulted: the per-thread
    /// [`CodeCache`](crate::jit::cache) compiles the callee on first sight and hands
    /// back the artifact it owns (so the code stays mapped for the run). A cache that
    /// is unavailable — a genuine `RefCell` re-entrancy, or a compile failure — is
    /// `None`, and the driver then runs that module on its interpreter step rather
    /// than aborting the run.
    fn module_blocks(&self, module: &Arc<Module>) -> Option<Rc<CompiledModule>> {
        crate::jit::cache::cached_module_blocks(module)
    }

    /// The artifact's own label map, built once at compilation: a run of `compiled`
    /// resolves its labels/imports through it instead of rebuilding one per run.
    fn labels(&self, _module: &Module) -> Option<&interpreter::LabelMap> {
        Some(self.compiled.labels())
    }
}

/// Run `(func, pc)` in `compiled` natively, or report "no native entry here".
///
/// [`NativeCode::enter`]'s body, shared with the driver's cross-module path. For a
/// lazy artifact this is the compile-on-entry point: [`CompiledModule::native_entry`]
/// lowers the owning function on first sight and returns the block's entry together
/// with the register-file bound the layout requires. A block is then entered only when
/// the running machine's register file covers every register it may touch natively; a
/// run whose file is too short (or an address with no compiled block) returns `None`
/// and stays on the interpreter step.
///
/// The bound is layout-dependent (see [`CompiledModule::native_entry`]): an **eager**
/// artifact reports the **whole-module** max, because its seal-time links tail-call
/// directly into other functions' blocks within the shared mapping — the entered
/// function's own bound would not cover the code such a link runs. A **lazy** artifact
/// reports the entered **function's** own bound, which is sufficient because each
/// function owns its mapping and a cross-function transfer returns to the driver (and
/// is re-checked there on the target function's own entry).
fn enter_compiled(
    compiled: &CompiledModule,
    module: &Module,
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
) -> Option<i64> {
    let entry = compiled.native_entry(module, func, pc);
    // Structural counter: one driver block-entry lookup, hit or miss.
    #[cfg(test)]
    counters::record_block_entry(entry.is_some());
    let (block_entry, bound) = entry?;
    if let Some(highest) = bound {
        // SAFETY: `rt` is the live driver-owned runtime block and its `machine`
        // pointer points at the driver's live `Machine`; the driver holds no
        // `&mut` alias while a native call is in flight.
        let x_len = unsafe { (*(*rt).machine).x.len() };
        if x_len <= highest as usize {
            return None;
        }
    }
    // Structural counter: the lookup is entered (the register-file guard passed).
    #[cfg(test)]
    counters::record_block_entered();
    // SAFETY: `block_entry` points just past the start of a complete native
    // function emitted by `emit_block` with exactly the `NativeBlock` signature
    // (`extern "C" fn(*mut JitRuntime) -> i64`), inside a buffer sealed
    // read+execute by `compile_module`; `rt` is the live driver-owned runtime
    // block the driver does not otherwise borrow during the call.
    let block: NativeBlock = unsafe { mem::transmute(block_entry) };
    Some(block(rt))
}

/// The module-wide label map a driver run resolves labels/imports through: the tier's
/// own (built once with the artifact) when it holds one, else a fresh build.
///
/// `Deref`s to [`LabelMap`], so a driver call site passes it exactly as it passed the
/// by-value map before ([`drive`] takes `&LabelMap`, [`interpreter::resolve`] too).
pub(crate) enum RunLabels<'a> {
    /// The map the tier's artifact already holds ([`NativeCode::labels`]): reused, so
    /// the run pays no per-run build.
    Stored(&'a interpreter::LabelMap),
    /// The per-run map the driver built because the tier holds none.
    Built(interpreter::LabelMap),
}

impl std::ops::Deref for RunLabels<'_> {
    type Target = interpreter::LabelMap;
    fn deref(&self) -> &interpreter::LabelMap {
        match self {
            RunLabels::Stored(map) => map,
            RunLabels::Built(map) => map,
        }
    }
}

/// Reset `machine`, seed `x[0..arity)` from `args`, push the bottom frame, bind the
/// module's literal area and bind the module-wide label map — the setup both driver
/// entry points share (it mirrors [`Machine::run_label`](crate::Machine)).
///
/// The label map is **reused** from the tier's artifact when it holds one
/// ([`NativeCode::labels`], i.e. the compiled map) and built here only otherwise: it is
/// a per-module value, so rebuilding it on every run of the same artifact is pure
/// per-run overhead.
fn enter_machine<'a>(
    machine: &mut Machine,
    module: &Module,
    arity: u32,
    args: &[Eterm],
    code: &'a dyn NativeCode,
) -> Result<RunLabels<'a>> {
    machine.reset();
    let n = arity.min(args.len() as u32);
    for (i, arg) in args.iter().take(n as usize).enumerate() {
        machine.set_x(i as u32, *arg)?;
    }
    machine.push_frame(0)?;
    machine.set_literals(module.literals.clone());
    Ok(match code.labels(module) {
        Some(labels) => RunLabels::Stored(labels),
        None => RunLabels::Built(interpreter::build_label_map(module)),
    })
}

/// The driver loop both native tiers run on.
///
/// Mirrors [`Machine::run_label`](crate::Machine)'s setup (reset, seed
/// `x[0..arity)` from `args`, push the bottom frame, bind the module's literal
/// area, resolve the export label and build the module-wide label map), then
/// loops:
///
/// * ask `code` (the tier's [`NativeCode`] lookup) whether `(func, pc)` can run
///   natively; when it can, the returned block-ABI word is folded exactly like a
///   block's return value — [`HALT`] finishes the run, [`ERROR`] propagates
///   `rt.error`, a `(func, pc)` pair continues there;
/// * otherwise take exactly one interpreter step
///   ([`Machine::step_instruction`](crate::Machine)), identically to the
///   interpreter's own loop.
///
/// The second path is not an emergency fallback: it is each tier's documented
/// per-instruction deopt, and it is also what happens for every address the tier
/// compiled no entry point for (and, for the baseline tier, when the running
/// machine's register file is too small for the function it would enter — see
/// [`enter_compiled`]).
///
/// This is the **non-blocking** form: a run that suspends in a `receive` (whether
/// the suspension came from the driver's own interpreter step or from a block's
/// [`jit_step_one`] deopt) is folded into the typed [`crate::error::blocked`] error,
/// exactly as the interpreter's own non-blocking entry is. Use
/// [`run_native_outcome`] to see the suspension instead.
pub(crate) fn run_native(
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
    code: &dyn NativeCode,
) -> Result<Eterm> {
    match run_native_outcome(machine, module, function, arity, args, ctx, code)? {
        RunOutcome::Returned(value) => Ok(value),
        RunOutcome::Wait { .. } => Err(crate::error::blocked()),
    }
}

/// The outcome-aware driver loop both native tiers run on.
///
/// The exact body of [`run_native`] with a [`RunOutcome`] result instead of the
/// folded `x(0)`: the machine is reset, `x[0..arity)` is seeded from `args`, the
/// bottom frame is pushed, the export label is resolved and the driver loop runs.
///
/// Every way a suspension can reach the driver is reported as
/// [`RunOutcome::Wait`](crate::RunOutcome::Wait) with the exact resume address the
/// interpreter would report:
///
/// * a compiled block that deopts to [`jit_step_one`] for a `wait`/`wait_timeout`
///   returns the [`encode_wait`] word, which is decoded here (the block is left
///   with its registers spilled, so `machine` is ready to be resumed);
/// * the driver's own interpreter step returns [`Step::Wait`](crate::Step::Wait)
///   directly.
///
/// A [`HALT`] word finishes the run with `x(0)` and an [`ERROR`] word propagates
/// `rt.error`; every other word is a `(func, pc)` continuation. Nothing is
/// silently continued and nothing is invented.
pub(crate) fn run_native_outcome(
    machine: &mut Machine,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
    code: &dyn NativeCode,
) -> Result<RunOutcome> {
    let entry = module
        .export(function, arity)
        .ok_or_else(|| error::unsupported("jit: export not found"))?;

    let labels = enter_machine(machine, module, arity, args, code)?;
    let (func, pc) = interpreter::resolve_label(module, entry)?;
    // Record the run's entry call, exactly as every other run entry does
    // (`Machine::run_label_outcome`, `run_native_label_outcome`): without it the
    // export path would leave `Machine::entry_call` unset — the introspection
    // backing `process_info/2`'s `initial_call`/`current_function` — and report a
    // different run-entry position than the interpreter for the same module/export.
    machine.enter_run(module, func);
    drive(machine, module, &labels, func, pc, ctx, code)
}

/// The label-addressed outcome-aware driver loop.
///
/// [`run_native_outcome`] with a raw entry `label` instead of an export: the label
/// is resolved against the module-wide label map (so it may live in any function)
/// and the run's entry call is recorded, exactly as
/// [`Machine::run_label_outcome`](crate::Machine) does. A label the module does not
/// define is the interpreter's own typed `unknown_label` error.
pub(crate) fn run_native_label_outcome(
    machine: &mut Machine,
    module: &Module,
    label: Label,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
    code: &dyn NativeCode,
) -> Result<RunOutcome> {
    let labels = enter_machine(machine, module, arity, args, code)?;
    let (func, pc) = interpreter::resolve(&labels, label)?;
    machine.enter_run(module, func);
    drive(machine, module, &labels, func, pc, ctx, code)
}

/// One non-entry module context the native driver has entered, with its compiled
/// blocks resolved **once** (a context id is stable for the run, so a repeated or
/// recursive cross-module call re-enters the same artifact instead of re-resolving it).
struct CalleeCode {
    /// The module context id this code belongs to.
    context: u32,
    /// The callee's compiled blocks, from the tier's own store
    /// ([`NativeCode::module_blocks`]); `None` when the tier has none, in which case the
    /// driver runs the module on its per-instruction interpreter step. The register-file
    /// guard is applied at entry time ([`enter_compiled`]), not here.
    compiled: Option<Rc<CompiledModule>>,
}

/// The native driver's "which module is executing" state.
///
/// The interpreter driver carries the same information as `module`/`labels` plus the
/// machine's `Contexts` handle ([`Machine::drive`]); the native driver needs it as
/// explicit raw pointers because generated code holds raw pointers into the active
/// module and its label map, and both change at a module-context transfer.
struct DriverContext {
    /// The active module context (see [`Machine::current_context`]).
    context: u32,
    /// The active module (`context == 0` is the caller's `module`, else the callee's).
    module: *const Module,
    /// The active module's label map.
    labels: *const LabelMap,
    /// The `Arc<Module>` of the active callee, if any: an explicit liveness handle for
    /// `module` (the machine holds the module too, but a raw pointer's provenance
    /// should not rest on that).
    callee_module: Option<Arc<Module>>,
    /// The compiled blocks of every non-entry context entered so far.
    callees: Vec<CalleeCode>,
    /// The index into `callees` of the active non-entry context, `None` in context `0`
    /// (or for a non-entry context the tier has no blocks for — the driver then takes
    /// its per-instruction interpreter step there).
    active: Option<usize>,
}

/// Land the native driver in module context `context`, binding the active module, its
/// label map and (for a non-entry context) its compiled blocks.
///
/// This replicates the interpreter driver's [`Step::Enter`] handling
/// ([`Machine::drive`]) exactly: the machine's context is switched through
/// [`Machine::switch_context`] — which re-binds the literal area, is idempotent for
/// the context already current, and reports an unknown context as a typed error — and
/// the driver's own module/label handles follow it.
fn switch_into(
    machine: &mut Machine,
    code: &dyn NativeCode,
    entry_module: &Module,
    entry_labels: &LabelMap,
    state: &mut DriverContext,
    context: u32,
) -> Result<()> {
    if context != machine.current_context() {
        machine.switch_context(context)?;
    }
    if context == state.context {
        return Ok(());
    }
    if context == 0 {
        state.module = entry_module as *const Module;
        state.labels = entry_labels as *const LabelMap;
        state.callee_module = None;
        state.active = None;
    } else {
        let handle = machine
            .module_context(context)
            .ok_or_else(|| error::unsupported("jit: unknown module context"))?;
        // Bind the callee's literal area, exactly as [`Machine::drive`] does when it
        // lands in a non-entry context. The `switch_context` above already did it when
        // the machine was elsewhere, but it is a no-op for the context **already**
        // current — which is precisely the resume case ([`run_compiled_resume_outcome`]
        // re-enters the context the process blocked in), where the entry module's
        // literals would otherwise stay bound and resolve an `Operand::Literal` against
        // the wrong module.
        machine.set_literals(handle.module.literals.clone());
        let module = &*handle.module as *const Module;
        let labels = &handle.labels as *const LabelMap;
        // Resolve the callee's compiled blocks once per context. A context the tier
        // has no blocks for keeps `active = None`: the driver runs that module on its
        // per-instruction interpreter step, exactly as it does an address its entry
        // artifact did not compile.
        let index = match state.callees.iter().position(|code| code.context == context) {
            Some(index) => index,
            None => {
                let index = state.callees.len();
                // Resolve the callee's compiled blocks once per context. A module the tier
                // has no blocks for keeps `compiled = None` / `active = None`: the driver
                // runs it on its per-instruction interpreter step, exactly as it does an
                // address its entry artifact did not compile.
                let compiled = code.module_blocks(&handle.module);
                state.callees.push(CalleeCode { context, compiled });
                index
            }
        };
        state.active = if state.callees[index].compiled.is_some() {
            Some(index)
        } else {
            None
        };
        state.module = module;
        state.labels = labels;
        state.callee_module = Some(Arc::clone(&handle.module));
    }
    state.context = context;
    Ok(())
}

/// The shared driver loop over an already-prepared machine and entry address.
///
/// The A.8 tracing gate is latched here — **once per run** — so the driver's own
/// per-instruction interpreter step
/// ([`Machine::step_instruction`](crate::Machine)) sees the same cached
/// `Machine::trace_on` flag the interpreter's dispatch loop would have
/// latched, without the per-instruction `trace_enabled()` vtable call. Both native
/// tiers enter here (`run_native_outcome` and `run_native_label_outcome`, the latter
/// shared with the Cranelift tier), so the flag is set for either.
///
/// A **traced** run refuses native entry entirely: it hands the whole slice to the
/// interpreter's own driver ([`Machine::drive`]). A natively executed block emits no
/// trace event (the engine reports trace events from `step_instruction` only) and the
/// gate is latched once, so letting native code run would make a traced tier-run
/// observably different from a traced interpreter run; refusing costs one
/// `trace_enabled()` call per run and nothing at all when tracing is off.
///
/// The driver owns the machine's **module context** the way the interpreter driver
/// does: a [`Step::Enter`] (from its own interpreter step, or handed back by a block
/// as the [`ENTER`] word) switches context, re-binds the active module, its label map
/// and its compiled blocks ([`switch_into`]), and continues — in the callee's compiled
/// code when the tier has it, otherwise on the interpreter step. A cross-module
/// transfer is therefore a **driver round-trip by construction**: a seal-time link
/// across modules is unsound (each [`CompiledModule`] owns a separate executable
/// mapping and its `rel32` fixups are buffer-local), and the runtime bakes the entry
/// module into [`JitRuntime::module`]/[`JitRuntime::labels`].
fn drive(
    machine: &mut Machine,
    module: &Module,
    labels: &interpreter::LabelMap,
    mut func: u32,
    mut pc: u32,
    ctx: &mut dyn ExecutionContext,
    code: &dyn NativeCode,
) -> Result<RunOutcome> {
    if ctx.trace_enabled() {
        machine.trace_on = true;
        return machine.drive(module, Some(labels), func, pc, ctx);
    }
    // Latch the per-run tracing gate (see the `Machine::trace_on` field docs): the
    // deopt path's `step_instruction` reads the plain `bool`, so this single call
    // replaces a vtable call per call/return instruction.
    machine.trace_on = false;
    // The objects the runtime's raw pointers refer to. `machine_ptr`/`ctx_ptr` are
    // the same addresses the [`JitRuntime`] holds; all of them outlive the loop, and
    // no `&mut` of the driver's aliases them while a native call is in flight.
    let x_regs: *mut Eterm = machine.x.as_mut_ptr();
    let machine_ptr: *mut Machine = &mut *machine;
    let ctx_ptr = &mut *ctx as *mut dyn ExecutionContext;
    // The starting context: `0` for a fresh run; the context a suspended process
    // blocked in for a resume (which does not reset, so a cross-module callee's
    // context is still registered).
    let mut state = DriverContext {
        context: 0,
        module: module as *const Module,
        labels: labels as *const LabelMap,
        callee_module: None,
        callees: Vec::new(),
        active: None,
    };
    let start_context = unsafe { (*machine_ptr).current_context() };
    if start_context != 0 {
        // SAFETY: `machine_ptr` points at the live driver-owned `Machine`; no native
        // code is running here.
        switch_into(
            unsafe { &mut *machine_ptr },
            code,
            module,
            labels,
            &mut state,
            start_context,
        )?;
    }
    let mut rt = JitRuntime {
        machine: machine_ptr,
        x_regs,
        module: state.module,
        labels: state.labels,
        ctx: ctx_ptr,
        error: None,
        enter: None,
        y_base: std::ptr::null_mut(),
        y_end: std::ptr::null_mut(),
    };

    loop {
        // Native entry for the ACTIVE module: the caller-owned artifact in context
        // `0`, the callee's blocks (if the tier has any) in a non-entry context.
        let native = match state.active {
            // Context 0 is the caller-owned entry artifact; the active module there is
            // `state.module`, i.e. the run's entry module. `state.module` is a raw
            // pointer at a `Module` the driver holds alive for the whole loop (the
            // caller's entry module, or a callee's `Arc<Module>` in
            // `state.callee_module`), and no `&mut` aliases it while native code runs.
            None if state.context == 0 => code.enter(&mut rt, unsafe { &*state.module }, func, pc),
            None => None,
            Some(index) => {
                let resolved = &state.callees[index];
                match &resolved.compiled {
                    // SAFETY: `state.module` points at the live active module (see the
                    // context-0 arm above); no `&mut` aliases it while native code runs.
                    Some(compiled) => {
                        enter_compiled(compiled, unsafe { &*state.module }, &mut rt, func, pc)
                    }
                    None => None,
                }
            }
        };
        if let Some(word) = native {
            // Structural counter: the driver had to decode a block-ABI word (a
            // block-return round-trip), classified by that word.
            #[cfg(test)]
            counters::record_driver_round_trip(word);
            // The compiled block just ran, storing straight into `machine.x` (and,
            // potentially, the FR file) through the raw pointer it holds — writes the
            // `set_x`/`set_fr` high-water marks cannot observe. Declare the register
            // files escaped so the next `Machine::reset` refills them in full instead of
            // trusting the marks (a stale register would be a wrong result).
            //
            // SAFETY: `machine_ptr` points at the live driver-owned `Machine`; the
            // block has returned, so no native code is running and no `&mut` of it is
            // outstanding.
            unsafe { (*machine_ptr).mark_regs_escaped() };
            if word == HALT {
                // SAFETY: `machine_ptr` points at the live driver-owned `Machine`;
                // no native code is running here.
                return unsafe { (*machine_ptr).finish() }.map(RunOutcome::Returned);
            }
            if word == ERROR {
                return Err(rt
                    .error
                    .take()
                    .unwrap_or_else(|| error::unsupported("jit: helper error")));
            }
            if word == ENTER {
                let (context, target_func, target_pc) = rt
                    .enter
                    .take()
                    .ok_or_else(|| error::unsupported("jit: enter word without a target"))?;
                // SAFETY: `machine_ptr` points at the live driver-owned `Machine`;
                // no native code is running here.
                switch_into(
                    unsafe { &mut *machine_ptr },
                    code,
                    module,
                    labels,
                    &mut state,
                    context,
                )?;
                rt.module = state.module;
                rt.labels = state.labels;
                func = target_func;
                pc = target_pc;
                continue;
            }
            if let Some((wait_func, wait_pc)) = decode_wait(word) {
                // A block deopted at a `wait`/`wait_timeout`: its exit already spilled
                // the cached registers, so the machine is exactly as the interpreter's
                // own suspension leaves it and the host can resume at this address.
                return Ok(RunOutcome::Wait {
                    func: wait_func,
                    pc: wait_pc,
                });
            }
            match decode_target(word) {
                Some((next_func, next_pc)) => {
                    func = next_func;
                    pc = next_pc;
                    continue;
                }
                None => return Err(error::unsupported("jit: bad block target")),
            }
        } else {
            // Structural counter: no native entry for this address, so the driver
            // takes exactly one interpreter step (the documented per-instruction
            // deopt for an address the artifact compiled no block for).
            #[cfg(test)]
            counters::record_driver_interp_step();
        }

        // SAFETY: the native call returned, so no `&mut` derived from `rt` is alive;
        // `machine_ptr`/`ctx_ptr` point at the driver's live, distinct objects
        // (`Machine` and the `ExecutionContext`), never at each other; `state.module`/
        // `state.labels` point at the active module and its label map, both alive for
        // the loop (the entry module is the caller's, a callee's is held by the machine
        // and by `state`).
        let step = unsafe {
            (*machine_ptr).step_instruction(&*state.module, &*state.labels, func, pc, &mut *ctx_ptr)
        };
        // The uniform raise funnel's second half, exactly as `Machine::drive` runs it:
        // `step_instruction` attached the frames itself when the raise happened in the
        // run's **entry** context (`0`); a raise inside a cross-module callee was left
        // for here, where the driver knows the true entry module.
        let step = step.map_err(|err| {
            // SAFETY: `machine_ptr` points at the live driver-owned `Machine`; no
            // native code is running here.
            unsafe { (*machine_ptr).with_uncaught_frames(*err, Some(module), func, pc) }
        })?;
        match step {
            Step::Next => pc += 1,
            Step::Goto {
                func: next_func,
                pc: next_pc,
            } => {
                func = next_func;
                pc = next_pc;
            }
            Step::Wait {
                func: wait_func,
                pc: wait_pc,
            } => {
                return Ok(RunOutcome::Wait {
                    func: wait_func,
                    pc: wait_pc,
                })
            }
            Step::Enter {
                context,
                func: target_func,
                pc: target_pc,
            } => {
                // SAFETY: `machine_ptr` points at the live driver-owned `Machine`;
                // no native code is running here.
                switch_into(
                    unsafe { &mut *machine_ptr },
                    code,
                    module,
                    labels,
                    &mut state,
                    context,
                )?;
                rt.module = state.module;
                rt.labels = state.labels;
                func = target_func;
                pc = target_pc;
            }
            Step::Done => {
                // SAFETY: `machine_ptr` points at the live driver-owned `Machine`.
                return unsafe { (*machine_ptr).finish() }.map(RunOutcome::Returned);
            }
        }
    }
}
