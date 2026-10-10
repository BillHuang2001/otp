# `beam_engine` JIT / execution-tier seam (`src/jit/`)

## Intent
The execution-tier seam of the engine: the `Tier`/`JitBackend` abstraction, the cross-run
compiled-block cache, the tiering policy, and two native backends (a baseline x86-64 tier and an
optional Cranelift hot tier) that execute `beam_loader`'s typed IR with **interpreter-identical
semantics**. These are correctness-first *mechanisms*, not optimisers (DeepDive §F); the baseline
tier is a genuine native path whose every un-lowered instruction deopts per-instruction to the
interpreter's own step. Block ABI, register cache, spill/root-set model and the per-instruction
deopt are the load-bearing contracts.

## API Surface (crate-visible; `beam_engine::jit::…`)
- `mod.rs` — `Tier { Interpreter, BaselineJit, Cranelift }` (`ALL`/`name`/`from_name`/`is_available`), `available_tiers()`, the `JitBackend` trait (`compile`/`run_compiled`/`run_export`), `CodeRegion`, feature gating (`jit-cranelift` default OFF; `jit-beamasm` aliases the baseline), and the feature-dispatched Cranelift entries (`run_cranelift{,_outcome,_label_outcome,_resume_outcome}`).
- `cache.rs` — `CodeCache`, `ModuleKey`/`module_fingerprint` (content hash, never a pointer), thread-local `run_baseline_cached{,_outcome,_label_outcome,_resume_outcome}`, `baseline_code_map`. `run_baseline_cached_resume_outcome` is the **resume** entry (backs `Machine::resume_outcome_with_tier` for the baseline tier): it reuses the cached artifact (compiling uncached only on a `RefCell` re-entry) and delegates to the no-reset `baseline::run_compiled_resume_outcome`.
- `tiering.rs` — `TierPolicy { baseline_threshold, hot_threshold }` (interpret → baseline → hot; a policy only).
- `baseline/` — `BaselineJit`, `CompiledModule` (`entry`/`block_count`/`cached_registers`/`highest_{cached,native}_register`/`code_len`/`code_address_range`/`code_map`), the block ABI (`runtime.rs`: `encode_target`/`decode_target`/`encode_wait`/`decode_wait`/`HALT=-1`/`ERROR=-2`/`WAIT_BIT`), the drivers (`run_compiled{,_outcome,_label_outcome}`) plus the no-reset `run_compiled_resume_outcome`.
- `cranelift/` (feature `jit-cranelift`) — `CraneliftJit`, `CompiledFunctions`, `CodegenReport`, `run_resume_outcome` (a typed error — see below).
- `emitter/` — `CodeBuffer`, `Reg`, `Cond`, `Label`: the crate's **only** executable-memory mapping.

## Key contracts (frozen)
- **Block ABI**: `extern "C" fn(*mut JitRuntime) -> i64` returning `(func<<32|pc)` (`func < 2^31`), or a sentinel (`HALT`/`ERROR`/`WAIT_BIT`/`ENTER`). `rbx` = base of `machine.x`; up to 5 cached x-registers live in `r12`–`r15`+`rbp`; `rt` in a 24-byte stack slot. Every compiled block's **prologue** calls `jit_set_position(rt, func, block_start_pc)` so `Machine::current_code` advances at block granularity across a *linked* branch; a deopt (`jit_step_one`) and a `call_last` frame pop (`jit_tail_call`) record/keep the exact instruction position.
- **Deopt = one interpreter step.** Every un-lowered instruction (or an inline sequence whose run-time guard rejects) spills the cached x-registers, calls `jit_step_one` → `Machine::step_instruction`, then continues **in-block** when the step advanced by exactly one (`Step::Next`); any other outcome returns word to the driver. `FuncInfo` is a raise (`function_clause`), deliberately never a metadata no-op.
- **Root-set / spill**: a block exit may never skip the spill of its cached x-registers to `machine.x`; non-cached registers are addressed directly in `machine.x`. Both `call_r` sites (`emit/mod.rs` and `emit/link.rs`) spill first, so a helper (hence `ensure_heap`/`alloc_words`) sees the interpreter's exact live root set.
- **`Step::Enter`** (cross-module context switch). A compiled block hands the resolved `(context, func, pc)` back to the shared driver with the `ENTER` block-ABI word (its per-instruction deopt, `jit_step_one`); the driver performs the switch exactly as `Machine::drive` does — `Machine::switch_context`, re-bind the active module, label map and literal area, and (through `NativeCode::module_blocks`, which for the tier's shared store compiles the callee on first sight) enter the callee's compiled blocks, or its documented per-instruction step when the tier has none. `call_ext*` is therefore not inlined: a cross-module transfer is a **driver round-trip by construction** (each `CompiledModule` owns a separate executable mapping with buffer-local `rel32` fixups, and `JitRuntime::module`/`labels` are baked for one module). A **traced** run (`ctx.trace_enabled()`) hands the whole slice to `Machine::drive` instead, since native code emits no trace events.
- **Resume.** The baseline tier resumes a suspended process **without reset** (`run_compiled_resume_outcome`: `enter_run` + the shared `drive`, entering the compiled block at the reported `(func, pc)`), so a slice that suspended on the baseline tier resumes on it and stays interpreter-identical (same label map, same deopt, same reduction accounting at slice start). A resume in a **non-entry** module context (a suspension inside a cross-module callee) is served the same way the interpreter's resume serves it: the shared driver reads the context from `Machine::current_context`, lands in it (`switch_into` re-binds the callee's module, label map and literal area and resolves its blocks through `NativeCode::module_blocks`), and re-enters the callee's own `(func, pc)`. The Cranelift tier bakes each compiled function's entry `pc` in, so a mid-function suspension address is unserviceable: `cranelift::run_resume_outcome` returns the typed `unsupported("cranelift jit: cannot resume at an arbitrary pc …")` — never a silent fallback to the interpreter. The Cranelift tier inherits `NativeCode::module_blocks`'s default (`None`), so a cross-module callee it enters runs on the driver's per-instruction step; it does not accelerate the switch.
- `jit-cranelift` deps are optional/default-off; without the feature `cargo tree -p beam_engine` lists no `cranelift-*`.
- Never RWX: `mmap` RW then `mprotect` RX (`emitter`).

## Routing Table
- `baseline/` → the native x86-64 baseline backend: driver + `enter_machine`/`run_native*`/`drive` (`mod.rs`), block ABI + `jit_step_one`/`jit_tail_call` (`runtime.rs`), unit tests (`tests.rs`); `baseline/emit/` → the per-instruction emitters + usage-weighted regalloc (see its own `CONTEXT.md`).
- `cranelift/` → the optional Cranelift hot tier (feature-gated); see its own `CONTEXT.md`.
- `emitter/` → the x86-64 encoder + executable `CodeBuffer`; see its own `CONTEXT.md`.
- `cache.rs` → cross-run compiled-block cache + host code-map entry; `tiering.rs` → tier policy; `mod.rs` → the tier seam.
- `bench/` → the in-crate tier measurement harness — a **`pub mod bench`** (not `#[cfg(test)]`): a ten-workload interpreter-vs-baseline tier table (`fib`/`kernel`/`wide`/`aggr`/`cmp` + `build`/`map`/`select`/`walk` + the cross-module `cross/2`), the `support.rs` scaffolding shared with `cranelift/bench.rs`, and the per-workload result checks; its `#[test]` entry points live in the thin `tests/jit_bench.rs`. See its own `CONTEXT.md`.
- The **baseline inline set** now includes the boxed-term header `TestKind`s: the word-level `Test`s (`is_nil`/`is_atom`/`is_list`/`is_nonempty_list`/`is_boolean`) plus **14** header kinds (`is_integer`/`is_number`/`is_float`/`is_tuple`/`is_map`/`is_bitstring`/`is_binary`/`is_function`/`is_function2`/`is_tagged_tuple`/`test_arity`/`is_pid`/`is_port`/`is_reference`); everything else still deopts (see `baseline/emit/CONTEXT.md`).

## Constraints
- Errors are `beam_common::{Result, BeamError}` only; no panic, no `unwrap()` on loaded data, no wrong result — an un-lowered op is a typed `unsupported`.
- `unsafe` is confined to `baseline/{mod,runtime}.rs` + `baseline/emit/*` + `emitter/mod.rs` (+ feature-gated `cranelift/mod.rs`); each `unsafe` block carries a `// SAFETY:` rationale.

## See Also
- `../CONTEXT.md` (engine) → the `JitBackend`/tier seam and `Machine::run_*_with_tier`; `INSTRUCTION_SURFACE.md`/`MEASURED.md`/`TEST_NOTES.md`.
- `baseline/emit/CONTEXT.md`, `emitter/CONTEXT.md`, `cranelift/CONTEXT.md`, `bench/CONTEXT.md` → per-file detail.
