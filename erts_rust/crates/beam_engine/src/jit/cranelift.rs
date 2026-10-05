//! The optional **Cranelift hot tier**: typed IR → Cranelift IR → native code.
//!
//! This is the backend behind [`Tier::Cranelift`](crate::jit::Tier::Cranelift),
//! compiled in only when the **`jit-cranelift`** Cargo feature is enabled (default
//! **OFF**, so the default build carries no Cranelift dependency and no Cranelift
//! codegen at all — Part II §F/§H keeps the hot tier off the critical path).
//!
//! # What it compiles
//!
//! For every IR [`Function`](beam_loader::Function) of the module, [`compile`]
//! builds **one** Cranelift function with the System V signature
//!
//! ```text
//! extern "C" fn(rt: i64) -> i64
//! ```
//!
//! (`rt` in `rdi`, the block-ABI word in `rax`) that starts at the function's
//! **entry `pc`** (resolved from the module-wide label map at compile time and
//! baked in as a constant) and then loops:
//!
//! 1. `target = call jit_step_one(rt, FUNC_INDEX, pc)` — the **same
//!    interpreter-backed helper** the baseline tier uses
//!    ([`baseline::jit_step_one`](crate::jit::baseline::jit_step_one) is literally
//!    [`Machine::step_instruction`](crate::Machine));
//! 2. a negative `target` is a sentinel ([`HALT`]/[`ERROR`]): return it to the
//!    driver unchanged;
//! 3. otherwise decode the `(func, pc)` continuation: a **different** function
//!    returns the word to the driver, the **same** function loops natively at the
//!    new `pc`.
//!
//! Control flow *within* a function is therefore native, while the semantics of
//! every single instruction stay the interpreter's — the results are
//! **interpreter-identical by construction**. This is the tier's documented
//! **cheap-deopt boundary**: every instruction the tier does not inline is
//! executed by the shared helper, and the driver falls back to one interpreter step
//! for any address no compiled function starts at.
//!
//! [`compile`]: JitBackend::compile
//!
//! # What it does **not** inline (yet)
//!
//! Nothing beyond the loop scaffolding: the per-instruction work is delegated to
//! the helper. Inlining the trivial instructions (`label`/`line`/`func_info`/
//! `int_code_end`/`executable_line`/`debug_line` no-ops and `move` between
//! x-registers/constants, mirroring the baseline tier's inline set) is a
//! subsequent task; the mechanism — a real Cranelift JIT module, symbol
//! resolution against the host helper, per-function native entries and a driver
//! loop — is what this module delivers.
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
use beam_loader::{Atom, Module};
use beam_term::Eterm;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, AbiParam, InstBuilder, Signature, UserFuncName};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module as ClModule, ModuleError};

use crate::context::ExecutionContext;
use crate::error;
use crate::jit::baseline::{jit_step_one, run_native, JitRuntime, NativeCode};
use crate::jit::JitBackend;
use crate::machine::Machine;

/// The name the per-instruction helper ([`jit_step_one`]) is registered under in
/// the JIT module's symbol table.
const HELPER_SYMBOL: &str = "jit_step_one";

/// The prefix of the symbol every compiled IR function is defined under.
///
/// The function index is appended (e.g. `beam_func_3`), which makes the names
/// unique within a module and legible in a disassembler.
const FUNCTION_PREFIX: &str = "beam_func_";

/// The native signature of a compiled Cranelift function: one `i64` runtime
/// pointer in, the block-ABI control word out.
///
/// This is the Rust-side view of the Cranelift signature `(I64) -> I64`; the two
/// must stay in step.
type NativeFunc = extern "C" fn(i64) -> i64;

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// The optional Cranelift hot tier.
///
/// Compilation is per [`JitBackend::run_export`] call (`run_with_tier` creates a
/// fresh backend per run), exactly like the baseline tier: a cross-run compiled
/// module cache is a *performance* concern deferred to a later task. The backend
/// owns the reusable [`FunctionBuilderContext`], which Cranelift requires as
/// scratch space while lowering a function.
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

    /// Compile `module` and run exported `function/arity` through the driver loop.
    fn run_export(
        &mut self,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let compiled = compile_module(module, &mut self.builder_context)?;
        let code = CompiledEntries { compiled: &compiled };
        run_native(machine, module, function, arity, args, ctx, &code)
    }
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
fn module_error(err: ModuleError) -> beam_common::BeamError {
    error::unsupported(&format!("cranelift jit: {err}"))
}

/// Compile every IR function of `module` into one Cranelift function.
///
/// Functions without code (or without a resolvable entry label) are skipped: the
/// driver simply keeps interpreting them, which is always correct.
fn compile_module(
    module: &Module,
    builder_context: &mut FunctionBuilderContext,
) -> Result<CompiledFunctions> {
    let mut builder = JITBuilder::new(default_libcall_names()).map_err(module_error)?;
    // Register the shared per-instruction helper under its symbol name so the JIT
    // resolves the import against this very process (no dlsym guesswork).
    builder.symbol(HELPER_SYMBOL, jit_step_one as *const () as *const u8);
    let mut jit = JITModule::new(builder);

    // The helper's signature: `(rt: i64, func: i32, pc: i32) -> i64`.
    let mut helper_sig = jit.make_signature();
    helper_sig.params.push(AbiParam::new(types::I64));
    helper_sig.params.push(AbiParam::new(types::I32));
    helper_sig.params.push(AbiParam::new(types::I32));
    helper_sig.returns.push(AbiParam::new(types::I64));
    let helper_id = jit
        .declare_function(HELPER_SYMBOL, Linkage::Import, &helper_sig)
        .map_err(module_error)?;

    // One shared signature per IR function: `(rt: i64) -> i64`.
    let mut function_sig = jit.make_signature();
    function_sig.params.push(AbiParam::new(types::I64));
    function_sig.returns.push(AbiParam::new(types::I64));

    let mut entry_pc: HashMap<u32, u32> = HashMap::new();
    let mut defined: Vec<(u32, FuncId)> = Vec::new();
    for (index, function) in module.functions.iter().enumerate() {
        if function.code.is_empty() {
            continue;
        }
        let Some(&pc) = function.labels.get(&function.entry_label) else {
            continue;
        };
        let func_index = index as u32;
        let name = format!("{FUNCTION_PREFIX}{func_index}");
        let id = jit
            .declare_function(&name, Linkage::Export, &function_sig)
            .map_err(module_error)?;
        emit_function(
            &mut jit,
            builder_context,
            helper_id,
            &function_sig,
            id,
            func_index,
            pc as u32,
        )?;
        entry_pc.insert(func_index, pc as u32);
        defined.push((func_index, id));
    }

    // Link the module: resolve the helper import, patch relocations and flip the
    // code memory to read+execute.
    jit.finalize_definitions().map_err(module_error)?;

    let mut entries: HashMap<u32, *const u8> = HashMap::new();
    for (func_index, id) in defined {
        entries.insert(func_index, jit.get_finalized_function(id));
    }

    Ok(CompiledFunctions {
        _module: jit,
        entries,
        entry_pc,
    })
}

/// Emit the native loop for one IR function: `(rt: i64) -> i64`.
///
/// The function starts at `entry_pc` and loops, calling the shared helper once per
/// instruction; see the module docs for the exact control flow.
fn emit_function(
    jit: &mut JITModule,
    builder_context: &mut FunctionBuilderContext,
    helper_id: FuncId,
    signature: &Signature,
    id: FuncId,
    func_index: u32,
    entry_pc: u32,
) -> Result<()> {
    let mut ctx = jit.make_context();
    ctx.func.signature = signature.clone();
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, builder_context);
        let helper = jit.declare_func_in_func(helper_id, &mut builder.func);

        let entry = builder.create_block();
        let step_loop = builder.create_block();
        let decode = builder.create_block();
        let stay = builder.create_block();
        let leave = builder.create_block();
        builder.append_block_param(step_loop, types::I32);
        builder.append_block_param(leave, types::I64);

        // --- entry: jump into the loop at the function's entry `pc`. -----------
        builder.switch_to_block(entry);
        builder.append_block_params_for_function_params(entry);
        let rt = builder.block_params(entry)[0];
        let start_pc = builder.ins().iconst(types::I32, i64::from(entry_pc));
        builder.ins().jump(step_loop, &[start_pc]);

        // --- loop: one helper-executed instruction. ----------------------------
        builder.switch_to_block(step_loop);
        let pc = builder.block_params(step_loop)[0];
        let this_func = builder.ins().iconst(types::I32, i64::from(func_index));
        let call = builder.ins().call(helper, &[rt, this_func, pc]);
        let target = builder.inst_results(call)[0];
        // HALT (`-1`) / ERROR (`-2`) are the only negative words a block returns.
        let sentinel = builder.ins().icmp_imm(IntCC::SignedLessThan, target, 0);
        builder.ins().brif(sentinel, leave, &[target], decode, &[]);

        // --- decode: `(func, pc)` = `(target >> 32, target & 0xFFFF_FFFF)`. ----
        builder.switch_to_block(decode);
        let high = builder.ins().ushr_imm(target, 32);
        let target_func = builder.ins().ireduce(types::I32, high);
        let elsewhere = builder
            .ins()
            .icmp(IntCC::NotEqual, target_func, this_func);
        builder.ins().brif(elsewhere, leave, &[target], stay, &[]);

        // --- stay: same function — take the native loop round. -----------------
        builder.switch_to_block(stay);
        let target_pc = builder.ins().ireduce(types::I32, target);
        builder.ins().jump(step_loop, &[target_pc]);

        // --- leave: hand the word back to the driver. --------------------------
        builder.switch_to_block(leave);
        let word = builder.block_params(leave)[0];
        builder.ins().return_(&[word]);

        builder.seal_all_blocks();
        builder.finalize();
    }
    jit.define_function(id, &mut ctx).map_err(module_error)?;
    jit.clear_context(&mut ctx);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::baseline::HALT;
    use crate::{Machine, MinimalContext};
    use beam_common::BeamError;
    use beam_loader::load_file;
    use beam_term::{is_small, make_small, signed_value};

    /// The `beam_loader` oracle fixture (reused, never copied).
    const SLICE_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    );

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
        if !is_small(args[0]) || !is_small(args[1]) {
            return Err(BeamError::BadArith);
        }
        let (a, b) = (signed_value(args[0]), signed_value(args[1]));
        match function {
            "+" => Ok(make_small(a + b)),
            "-" => Ok(make_small(a - b)),
            _ => Err(BeamError::BadArg),
        }
    }

    #[test]
    fn the_tier_names_itself_and_reports_available() {
        let backend = CraneliftJit::new();
        assert_eq!(JitBackend::name(&backend), "jit-cranelift");
        assert!(JitBackend::available(&backend));
    }

    #[test]
    fn compile_emits_one_native_entry_per_non_empty_function() {
        let module = load_file(SLICE_BEAM).expect("load slice.beam");
        let mut backend = CraneliftJit::new();
        let compiled = backend.compile(&module).expect("compile slice.beam");

        let expected: usize = module
            .functions
            .iter()
            .filter(|function| {
                !function.code.is_empty() && function.labels.contains_key(&function.entry_label)
            })
            .count();
        assert!(expected > 0, "the fixture must have functions");
        assert_eq!(compiled.function_count(), expected);

        // Every compiled function's entry is non-null and its entry `pc` is the
        // function's own entry label.
        for (index, function) in module.functions.iter().enumerate() {
            let Some(&pc) = function.labels.get(&function.entry_label) else {
                continue;
            };
            let func = index as u32;
            assert_ne!(compiled.entry(func), Some(std::ptr::null()));
            assert_eq!(compiled.entry_pc(func), Some(pc as u32));
        }
    }

    #[test]
    fn a_function_without_an_entry_label_is_not_compiled() {
        // An empty module compiles to an empty artifact (no functions, no entries) —
        // the driver keeps interpreting it.
        let module = Module::default();
        let mut backend = CraneliftJit::new();
        let compiled = backend.compile(&module).expect("compile an empty module");
        assert_eq!(compiled.function_count(), 0);
    }

    #[test]
    fn the_compiled_function_runs_the_export_natively() {
        // Evidence that native code really is entered and executed (rather than the
        // driver silently interpreting): build the runtime block by hand, call the
        // compiled `slice:add/2` entry **directly** and check both the control word
        // its native loop returns and the x-register it left behind.
        let module = load_file(SLICE_BEAM).expect("load slice.beam");
        let add = module
            .atom_table
            .lookup("add")
            .expect("the fixture interns `add`");
        let entry_label = module.export(add, 2).expect("add/2 is exported");
        let labels = crate::interpreter::build_label_map(&module);
        let (func, pc) = crate::interpreter::resolve_label(&module, entry_label)
            .expect("the export label resolves");

        let mut backend = CraneliftJit::new();
        let compiled = backend.compile(&module).expect("compile slice.beam");
        // The function is entered at its own entry address; any other address is the
        // driver's interpreter step.
        assert_eq!(compiled.entry_pc(func), Some(pc));
        let code = CompiledEntries { compiled: &compiled };

        let mut machine = Machine::new();
        machine.reset();
        machine.set_x(0, make_small(2)).expect("seed x0");
        machine.set_x(1, make_small(3)).expect("seed x1");
        machine.push_frame(0).expect("push the bottom frame");
        machine.set_literals(module.literals.clone());

        let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
        let x_regs: *mut Eterm = machine.x.as_mut_ptr();
        let machine_ptr: *mut Machine = &mut machine;
        let ctx_ptr = &mut ctx as *mut dyn ExecutionContext;
        let mut rt = JitRuntime {
            machine: machine_ptr,
            x_regs,
            module: &module as *const Module,
            labels: &labels as *const _,
            ctx: ctx_ptr,
            error: None,
        };

        let word = code
            .enter(&mut rt, func, pc)
            .expect("the entry address is compiled");
        // The native loop ran to the function's `return`: the bottom frame's
        // continuation, i.e. HALT.
        assert_eq!(word, HALT);
        // SAFETY: `machine_ptr` points at the live `machine` above; no native code is
        // running here.
        assert_eq!(unsafe { (*machine_ptr).x(0) }, Some(make_small(5)));
    }
}
