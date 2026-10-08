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
//! are the fill/spill for the rest. The highest x-register index any block touches
//! natively is recorded on the artifact so the driver can refuse native entry to a
//! register file that does not cover it.
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

use beam_common::Result;
use beam_loader::{Atom, Label, Module};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter;
use crate::jit::emitter::CodeBuffer;
use crate::jit::{CodeRegion, JitBackend};
use crate::machine::{Machine, RunOutcome};
use crate::Step;

mod emit;
mod runtime;

#[cfg(test)]
mod tests;

// The block ABI lives in `runtime`; re-export its names here so every item path
// outside this module (`crate::jit::baseline::…`) stays exactly what it was.
pub(crate) use emit::compile_module;
pub use runtime::{decode_target, decode_wait, encode_target, encode_wait, ERROR, HALT, WAIT_BIT};
pub(crate) use runtime::{jit_step_one, JitRuntime};

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
    fn enter(&self, rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> Option<i64>;
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

/// The artifact [`BaselineJit::compile`] produces: one sealed, executable
/// [`CodeBuffer`] plus the `(func, pc)` → native-entry table.
///
/// The buffer is `mmap`ped read+write, filled, and flipped to read+execute by
/// [`CodeBuffer::seal`] — never writable and executable at the same time (see
/// [`crate::jit::emitter`]). The entries are raw pointers into that mapping, so a
/// `CompiledModule` must outlive every call into it and is neither `Send` nor
/// `Sync`.
pub struct CompiledModule {
    /// The sealed code mapping every entry points into.
    buffer: CodeBuffer,
    /// The native entry point of the block starting at `[func][pc]`, or `None` when
    /// that address compiled no block. A flat index (not a `HashMap`) because the
    /// driver probes it on every cross-block jump.
    entries: Vec<Vec<Option<*const u8>>>,
    /// The number of compiled blocks (`entries`' `Some` count).
    block_count: usize,
    /// Every x-register any block caches in a machine register (sorted, deduped).
    cached_registers: Vec<u32>,
    /// The highest x-register index any block caches in a machine register.
    highest_cached_register: Option<u32>,
    /// The highest x-register index any block touches natively — a cached load,
    /// the exit spill, or a direct `machine.x` access for a register beyond the
    /// cache budget.
    ///
    /// The driver refuses to enter native code when the running machine's register
    /// file is not at least this long (see [`run_compiled`]): every one of those
    /// accesses is unchecked.
    highest_native_register: Option<u32>,
}

impl CompiledModule {
    /// The native entry point of the block starting at `(func, pc)`, if one was
    /// compiled.
    pub fn entry(&self, func: u32, pc: u32) -> Option<*const u8> {
        self.entries
            .get(func as usize)
            .and_then(|row| row.get(pc as usize))
            .copied()
            .flatten()
    }

    /// The number of compiled blocks.
    pub fn block_count(&self) -> usize {
        self.block_count
    }

    /// Every x-register any block holds in a machine register (sorted ascending,
    /// deduped across blocks).
    ///
    /// The allocator is usage-weighted ([`allocate_registers`]), so this is the set
    /// of the most-used registers rather than the first-appearing ones; a register
    /// **not** in it is addressed directly in `machine.x`.
    pub fn cached_registers(&self) -> &[u32] {
        &self.cached_registers
    }

    /// The highest x-register index cached in a machine register by any block.
    pub fn highest_cached_register(&self) -> Option<u32> {
        self.highest_cached_register
    }

    /// The highest x-register index any block touches natively (cached in a machine
    /// register, spilled back, or addressed directly in `machine.x`).
    pub fn highest_native_register(&self) -> Option<u32> {
        self.highest_native_register
    }

    /// The number of bytes of machine code emitted.
    pub fn code_len(&self) -> usize {
        self.buffer.len()
    }

    /// The `[start, end)` address range of the executable code mapping.
    ///
    /// Every entry [`CompiledModule::entry`] returns lies inside it.
    pub fn code_address_range(&self) -> (usize, usize) {
        let start = self.buffer.code_ptr() as usize;
        (start, start + self.buffer.len())
    }

    /// The **real** generated code regions of this artifact, one per compiled basic
    /// block, in ascending address order.
    ///
    /// This is the queryable counterpart of the sealed [`CodeBuffer`] mapping a host
    /// needs to describe the code it is running (e.g. the `+JPperf` perf-map file
    /// `beam_rt` writes): every [`CodeRegion::start`] is the **actual** address
    /// [`CompiledModule::entry`] returns for that block — never a synthesised or
    /// rounded one — and its [`CodeRegion::size`] runs to the next block's entry, or
    /// to the end of [`CompiledModule::code_address_range`] for the last block.
    ///
    /// `module` supplies the names: each [`CodeRegion::name`] is
    /// `<module>:<function>/<arity>/<pc>`, the block's origin in the IR. Because the entries
    /// are distinct points of one buffer ordered by emission, the returned regions
    /// are **disjoint and cover no gap larger than the block padding they measure**;
    /// an artifact that compiled no block yields an empty map.
    pub fn code_map(&self, module: &Module) -> Vec<CodeRegion> {
        // `(address, func, pc)` for every compiled block entry.
        let mut blocks: Vec<(usize, u32, u32)> = Vec::new();
        for (func, row) in self.entries.iter().enumerate() {
            for (pc, slot) in row.iter().enumerate() {
                if let Some(entry) = slot {
                    blocks.push((*entry as usize, func as u32, pc as u32));
                }
            }
        }
        // Emission order is already ascending, but sort so the public contract does
        // not depend on the emitter's block ordering.
        blocks.sort_by_key(|(address, _, _)| *address);

        let (_, end) = self.code_address_range();
        let module_name = module.atom_name(module.name).unwrap_or_default().to_owned();

        let mut regions = Vec::with_capacity(blocks.len());
        let mut index = 0;
        while index < blocks.len() {
            let (start, func, pc) = blocks[index];
            // Skip any further entry that shares this address: two entries at one
            // address describe the same code, and measuring them separately would
            // emit a zero-size region.
            let mut next = index + 1;
            while next < blocks.len() && blocks[next].0 == start {
                next += 1;
            }
            let region_end = blocks.get(next).map_or(end, |(address, _, _)| *address);
            // A BEAM module may define several functions under one name (different
            // arities, e.g. `fib/1` and `fib/3`), so the name carries the arity too:
            // `<module>:<function>/<arity>/<pc>` identifies the block's IR origin
            // unambiguously.
            let (function_name, arity) = module
                .functions
                .get(func as usize)
                .map(|function| {
                    (
                        module.atom_name(function.name).unwrap_or_default(),
                        function.arity,
                    )
                })
                .unwrap_or_default();
            regions.push(CodeRegion {
                start,
                size: region_end - start,
                name: format!("{module_name}:{function_name}/{arity}/{pc}"),
            });
            index = next;
        }
        regions
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
    let code = block_entries(compiled, machine);
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
    let code = block_entries(compiled, machine);
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
    let code = block_entries(compiled, machine);
    run_native_label_outcome(machine, module, label, arity, args, ctx, &code)
}

/// The baseline tier's [`NativeCode`] lookup for `compiled`, carrying whether the
/// running machine's register file is wide enough to enter native code.
///
/// A block loads its cached x-registers out of `machine.x` — and addresses the
/// registers beyond the cache budget there directly — without a bounds check, so a
/// machine whose register file is shorter than the highest natively-touched index
/// cannot enter native code; such a run stays on the interpreter step.
fn block_entries<'a>(compiled: &'a CompiledModule, machine: &Machine) -> BlockEntries<'a> {
    let native_ok = match compiled.highest_native_register {
        Some(highest) => machine.x.len() > highest as usize,
        None => true,
    };
    BlockEntries {
        compiled,
        native_ok,
    }
}

/// The baseline tier's [`NativeCode`]: any compiled basic block may be entered.
struct BlockEntries<'a> {
    /// The compiled block table the addresses are looked up in.
    compiled: &'a CompiledModule,
    /// Whether the running machine's register file covers every register the
    /// compiled blocks touch natively; when it does not, no block may be entered
    /// (see [`run_compiled`]).
    native_ok: bool,
}

impl NativeCode for BlockEntries<'_> {
    fn enter(&self, rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> Option<i64> {
        if !self.native_ok {
            return None;
        }
        let block_entry = self.compiled.entry(func, pc)?;
        // SAFETY: `block_entry` points just past the start of a complete native
        // function emitted by `emit_block` with exactly the `NativeBlock` signature
        // (`extern "C" fn(*mut JitRuntime) -> i64`), inside a buffer sealed
        // read+execute by `compile_module`; `rt` is the live driver-owned runtime
        // block the driver does not otherwise borrow during the call.
        let block: NativeBlock = unsafe { mem::transmute(block_entry) };
        Some(block(rt))
    }
}

/// Reset `machine`, seed `x[0..arity)` from `args`, push the bottom frame, bind the
/// module's literal area and build the module-wide label map — the setup both driver
/// entry points share (it mirrors [`Machine::run_label`](crate::Machine)).
fn enter_machine(
    machine: &mut Machine,
    module: &Module,
    arity: u32,
    args: &[Eterm],
) -> Result<interpreter::LabelMap> {
    machine.reset();
    let n = arity.min(args.len() as u32);
    for (i, arg) in args.iter().take(n as usize).enumerate() {
        machine.set_x(i as u32, *arg)?;
    }
    machine.push_frame(0)?;
    machine.set_literals(module.literals.clone());
    Ok(interpreter::build_label_map(module))
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
/// machine's register file is too small — see [`run_compiled`]).
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

    let labels = enter_machine(machine, module, arity, args)?;
    let (func, pc) = interpreter::resolve_label(module, entry)?;
    // Record the run's entry call, exactly as every other run entry does
    // (`Machine::run_label_outcome`, `run_native_label_outcome`): without it the
    // export path would leave `Machine::entry_call` unset — the introspection
    // backing `process_info/2`'s `initial_call`/`current_function` — and report a
    // different run-entry position than the interpreter for the same module/export.
    machine.enter_run(module, func);
    drive(machine, module, labels, func, pc, ctx, code)
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
    let labels = enter_machine(machine, module, arity, args)?;
    let (func, pc) = interpreter::resolve(&labels, label)?;
    machine.enter_run(module, func);
    drive(machine, module, labels, func, pc, ctx, code)
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
fn drive(
    machine: &mut Machine,
    module: &Module,
    labels: interpreter::LabelMap,
    mut func: u32,
    mut pc: u32,
    ctx: &mut dyn ExecutionContext,
    code: &dyn NativeCode,
) -> Result<RunOutcome> {
    // Latch the per-run tracing gate (see the `Machine::trace_on` field docs): the
    // deopt path's `step_instruction` reads the plain `bool`, so this single call
    // replaces a vtable call per call/return instruction.
    machine.trace_on = ctx.trace_enabled();
    // The objects the runtime's raw pointers refer to. `machine_ptr`/`ctx_ptr` are
    // the same addresses the [`JitRuntime`] holds; all of them outlive the loop, and
    // no `&mut` of the driver's aliases them while a native call is in flight.
    let x_regs: *mut Eterm = machine.x.as_mut_ptr();
    let machine_ptr: *mut Machine = &mut *machine;
    let ctx_ptr = &mut *ctx as *mut dyn ExecutionContext;
    let mut rt = JitRuntime {
        machine: machine_ptr,
        x_regs,
        module: module as *const Module,
        labels: &labels as *const LabelMap,
        ctx: ctx_ptr,
        error: None,
    };

    loop {
        if let Some(word) = code.enter(&mut rt, func, pc) {
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
        }

        // SAFETY: the native call returned, so no `&mut` derived from `rt` is alive;
        // `machine_ptr`/`ctx_ptr` point at the driver's live, distinct objects
        // (`Machine` and the `ExecutionContext`), never at each other.
        let step = unsafe { (*machine_ptr).step_instruction(module, &labels, func, pc, &mut *ctx_ptr)? };
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
            // A module-context switch is the interpreter driver's job (see
            // `jit_step_one`): the blocks, the label map and the literal area of this
            // driver are all bound for the entry module.
            Step::Enter { .. } => {
                return Err(error::unsupported(
                    "jit: module context switch outside the interpreter driver",
                ))
            }
            Step::Done => {
                // SAFETY: `machine_ptr` points at the live driver-owned `Machine`.
                return unsafe { (*machine_ptr).finish() }.map(RunOutcome::Returned);
            }
        }
    }
}
