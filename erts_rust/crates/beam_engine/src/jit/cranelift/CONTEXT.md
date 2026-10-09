# `beam_engine` optional Cranelift hot tier (`src/jit/cranelift/`)

## Intent
The optional **hot tier** behind the default-OFF `jit-cranelift` Cargo feature: one Cranelift
function per IR `Function` over the **same** block ABI and the same shared interpreter-backed helper
as the baseline tier. Measurement-gated and off the critical path (DeepDive §F); correctness-first.
With the feature off the crate carries **no** Cranelift dependency and `Tier::Cranelift` evaluates to
a typed `unsupported("jit-cranelift tier not built")` — never a panic, never a silent fallback.

## API Surface
- `mod.rs` — `CraneliftJit` (the `JitBackend`), `CompiledFunctions` (owns the `JITModule`; accessors `entry`/`entry_pc`/`function_count`/`highest_native_register` + the `inlined_*` introspection), `run_cranelift*`.
- `codegen.rs` — `compile_module` (`emit_pc` per instruction); `inline.rs`/`compare.rs`/`shape.rs` — the inline-set lowering and shape tests; `report.rs` — `CodegenReport` (`inlined_tests`/`inlined_gc_bifs`/`inlined_comparisons`/`inlined_select_vals`/`inlined_tuple_reads`/`inlined_list_reads`/`inlined_swaps`/`inlined_tail_calls`).

## Inline set relative to the baseline
**Strict subset by `Instr` variant set**: identical to the baseline's *except* `CallLast`, which the
baseline links natively and Cranelift always deopts (`call_last` needs the Rust-side frame pop).
Shape-level the two are near-identical, with Cranelift marginally more permissive in two
behaviourally-neutral spots (an un-interned `true`/`false` in `is_boolean`; a non-integer
`select_tuple_arity` alternative). Both remain observationally identical to the interpreter.
Inlined: metadata no-ops, `Move`, `Swap`, `Jump`, `CallOnly`, `Test`, small-int `GcBif`, comparison
`GcBif`/`Bif`, `SelectVal`, `SelectTupleArity`, `GetTupleElement`, `GetList`/`GetHd`/`GetTl`.

## Constraints
`unsafe` only in `mod.rs`; the tier never touches `Machine` state a block cannot address — anything
requiring a frame pop or context switch deopts to the shared helper.

## Routing Table
- `codegen.rs`/`inline.rs`/`shape.rs`/`compare.rs` → the inline set + shape tests; `report.rs` → the introspection report; `tests.rs` → in-module unit tests; `mod.rs` → the backend + accessors.
- Tier equivalence on real modules: crate `tests/jit_cranelift.rs`, `tests/jit_cranelift_real.rs` (see `../CONTEXT.md`).
