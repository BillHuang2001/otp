# `beam_engine` optional Cranelift hot tier (`src/jit/cranelift/`)

## Intent
The optional **hot tier** behind the default-OFF `jit-cranelift` Cargo feature: one Cranelift
function per IR `Function` over the **same** block ABI and the same shared interpreter-backed helper
as the baseline tier. Measurement-gated and off the critical path (DeepDive §F); correctness-first.
With the feature off the crate carries **no** Cranelift dependency and `Tier::Cranelift` evaluates to
a typed `unsupported("jit-cranelift tier not built")` — never a panic, never a silent fallback.

## API Surface
- `mod.rs` — `CraneliftJit` (the `JitBackend`), `CompiledFunctions` (owns the `JITModule`; accessors
  `entry`/`entry_pc`/`function_count`/`highest_native_register`/`is_enterable` + the `inlined_*`
  introspection), `run_cranelift*`.
- `codegen.rs` — `compile_module` (`emit_pc` per instruction); `inline.rs`/`compare.rs`/`shape.rs` —
  the inline-set lowering and shape tests; `report.rs` — `CodegenReport`
  (`inlined_tests`/`inlined_gc_bifs`/`inlined_comparisons`/`inlined_select_vals`/`inlined_tuple_reads`/
  `inlined_list_reads`/`inlined_swaps`/`inlined_tail_calls`).

## Signature and native entry points
Each compiled function has the signature `extern "C" fn(rt: i64, pc: i64) -> i64` (the Rust-side
view is `codegen::NativeFunc`); `rt` in `rdi`, the entry `pc` in `rsi`, the block-ABI word in `rax`.
The `pc` argument is an **entry `pc`**: the function's prologue
(`codegen::emit_pc_entry_dispatch`, emitted by `emit_function`) dispatches on it over the function's
candidate set, checking the function's own `entry_pc` **first** as a one-comparison fast path and
falling back (defensively, unreachable) to `block_for(entry_pc)`. That set is
`codegen::candidate_pcs` — every IR block start plus every non-tail-call continuation
(`call_site + 1`) — restricted to addresses with a real instruction and exposed through
`CompiledFunctions::is_enterable(func, pc)`. The `NativeCode::enter` impl (`CompiledEntries`) gates
native entry on `is_enterable` and calls `code(rt as i64, pc as i64)`, so the shared driver can enter
a compiled function natively at **any** of those addresses — which is how a cross-function non-tail
call's return (`call_site + 1`) is picked up natively instead of on the driver's per-instruction
interpreter step. An address outside the set (a `wait`/`wait_timeout` suspension `pc`) stays on the
interpreter step — never a wrong result. `CompiledFunctions::entry_pc(func)` (the function's entry
`pc`) is kept for introspection; native entry is gated on the wider `is_enterable` set.
`emit_pc_entry_dispatch` is separate from `emit_dispatch_chain` (the helper-*word* continuation
chain): the two share the candidate set but neither shares the other's mechanism, so the helper-word
chain is unchanged. Since the tier caches **no** x-registers across instructions (it addresses
`machine.x` directly), entering a function at a mid-function `pc` with the machine's current state is
correct by construction.

`run_resume_outcome` remains a typed `error::unsupported("cranelift jit: cannot resume at an arbitrary
pc (the cranelift tier has no resume entry point)")`: unlike first entry it is handed **no** compiled
artifact and the tier has no path that would compile and bind one at a parked address on demand.

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

## Cross-module transfer
The tier runs on the **shared** driver (`../baseline`), which handles a `Step::Enter` (a
`call_ext*` the host resolved through `ExecutionContext::resolve_external_call`) by switching module
context and re-entering. The Cranelift tier inherits `NativeCode::module_blocks`'s default (`None`),
so the callee is not accelerated — the driver runs it on its per-instruction interpreter step — but
it *executes*, so a compiled caller reaches the callee and returns correctly; the return from a
**same-module** cross-function non-tail call is now picked up natively at its continuation `pc`
(see "Signature and native entry points"). Its own `run_resume_outcome` is the typed error above.

## Routing Table
- `codegen.rs`/`inline.rs`/`shape.rs`/`compare.rs` → the inline set + shape tests; `report.rs` → the introspection report; `tests.rs` → in-module unit tests; `bench.rs` → the `#[ignore]`d ABBA Cranelift-vs-baseline measurement harness (its synthetic-`Module` builder, BIF hook and ns/op primitives are the shared `crate::jit::bench::support`, not a second copy); `mod.rs` → the backend + accessors.
- Tier equivalence on real modules: crate `tests/jit_cranelift.rs`, `tests/jit_cranelift_real.rs` (see `../CONTEXT.md`).
- `bench.rs` is run with `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --features jit-cranelift --lib -- --ignored --nocapture cl_bench`.

## Test Strategy
In-module unit tests (`tests.rs`) cover the backend naming/availability, the per-function entry
points, the reported inlined set, a direct native call into a compiled entry, and
`CompiledFunctions::is_enterable` — the latter pinned against an independent test-side oracle (IR
block starts ∪ non-tail-call continuations) on the `coverage.beam` fixture, asserting the entry `pc`
and every `call_site + 1` are enterable while an interior `pc` no boundary names is not. Tier
equivalence on real modules lives in the crate's `tests/jit_cranelift.rs` / `tests/jit_cranelift_real.rs`.
