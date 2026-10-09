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
- `mod.rs` — `Tier { Interpreter, BaselineJit, Cranelift }` (`ALL`/`name`/`from_name`/`is_available`), `available_tiers()`, the `JitBackend` trait (`compile`/`run_compiled`/`run_export`), `CodeRegion`, feature gating (`jit-cranelift` default OFF; `jit-beamasm` aliases the baseline).
- `cache.rs` — `CodeCache`, `ModuleKey`/`module_fingerprint` (content hash, never a pointer), thread-local `run_baseline_cached{,_outcome,_label_outcome}`, `baseline_code_map`.
- `tiering.rs` — `TierPolicy { baseline_threshold, hot_threshold }` (interpret → baseline → hot; a policy only).
- `baseline/` — `BaselineJit`, `CompiledModule` (`entry`/`block_count`/`cached_registers`/`highest_{cached,native}_register`/`code_len`/`code_address_range`/`code_map`), the block ABI (`runtime.rs`: `encode_target`/`decode_target`/`encode_wait`/`decode_wait`/`HALT=-1`/`ERROR=-2`/`WAIT_BIT`), the drivers (`run_compiled{,_outcome,_label_outcome}`).
- `cranelift/` (feature `jit-cranelift`) — `CraneliftJit`, `CompiledFunctions`, `CodegenReport`.
- `emitter/` — `CodeBuffer`, `Reg`, `Cond`, `Label`: the crate's **only** executable-memory mapping.

## Key contracts (frozen)
- **Block ABI**: `extern "C" fn(*mut JitRuntime) -> i64` returning `(func<<32|pc)` (`func < 2^31`), or a sentinel (`HALT`/`ERROR`/`WAIT_BIT`). `rbx` = base of `machine.x`; up to 5 cached x-registers live in `r12`–`r15`+`rbp`; `rt` in a 24-byte stack slot.
- **Deopt = one interpreter step.** Every un-lowered instruction (or an inline sequence whose run-time guard rejects) spills the cached x-registers, calls `jit_step_one` → `Machine::step_instruction`, then continues **in-block** when the step advanced by exactly one (`Step::Next`); any other outcome returns word to the driver. `FuncInfo` is a raise (`function_clause`), deliberately never a metadata no-op.
- **Root-set / spill**: a block exit may never skip the spill of its cached x-registers to `machine.x`; non-cached registers are addressed directly in `machine.x`. Both `call_r` sites (`emit/mod.rs` and `emit/link.rs`) spill first, so a helper (hence `ensure_heap`/`alloc_words`) sees the interpreter's exact live root set.
- **`Step::Enter`** (cross-module context switch) is interpreter-only; both native tiers return the typed `unsupported("jit: module context switch outside the interpreter driver")`.
- `jit-cranelift` deps are optional/default-off; without the feature `cargo tree -p beam_engine` lists no `cranelift-*`.
- Never RWX: `mmap` RW then `mprotect` RX (`emitter`).

## Routing Table
- `baseline/` → the native x86-64 baseline backend: driver + `enter_machine`/`run_native*`/`drive` (`mod.rs`), block ABI + `jit_step_one`/`jit_tail_call` (`runtime.rs`), unit tests (`tests.rs`); `baseline/emit/` → the per-instruction emitters + usage-weighted regalloc (see its own `CONTEXT.md`).
- `cranelift/` → the optional Cranelift hot tier (feature-gated); see its own `CONTEXT.md`.
- `emitter/` → the x86-64 encoder + executable `CodeBuffer`; see its own `CONTEXT.md`.
- `cache.rs` → cross-run compiled-block cache + host code-map entry; `tiering.rs` → tier policy; `mod.rs` → the tier seam.
- `bench/` (`#[cfg(test)]`) → the in-crate tier measurement harnesses: the fixed-order interpreter-vs-baseline tier table, the `support.rs` scaffolding shared with `cranelift/bench.rs`, and the per-workload result checks; see its own `CONTEXT.md`.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only; no panic, no `unwrap()` on loaded data, no wrong result — an un-lowered op is a typed `unsupported`.
- `unsafe` is confined to `baseline/{mod,runtime}.rs` + `baseline/emit/*` + `emitter/mod.rs` (+ feature-gated `cranelift/mod.rs`); each `unsafe` block carries a `// SAFETY:` rationale.

## See Also
- `../CONTEXT.md` (engine) → the `JitBackend`/tier seam and `Machine::run_*_with_tier`; `INSTRUCTION_SURFACE.md`/`MEASURED.md`/`TEST_NOTES.md`.
- `baseline/emit/CONTEXT.md`, `emitter/CONTEXT.md`, `cranelift/CONTEXT.md`, `bench/CONTEXT.md` → per-file detail.
