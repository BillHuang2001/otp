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
`unsafe` only inside the `mod.rs` module subtree: `mod.rs` declares `#![allow(unsafe_code)]` and its
children inherit it, so `codegen.rs`/`inline.rs`/`compare.rs`/`shape.rs`/`report.rs` are safe Rust and
the one `unsafe` block in the `#[cfg(test)] mod tests` is test-only. The tier never touches `Machine`
state a block cannot address — anything requiring a frame pop or context switch deopts to the shared
helper.

## Cross-module transfer
The tier runs on the **shared** driver (`../baseline`), which handles a `Step::Enter` (a
`call_ext*` the host resolved through `ExecutionContext::resolve_external_call`) by switching module
context and re-entering. The Cranelift tier inherits `NativeCode::module_blocks`'s default (`None`),
so the callee is not accelerated — the driver runs it on its per-instruction interpreter step — but
it *executes*, so a compiled caller reaches the callee and returns correctly; the return from a
**same-module** cross-function non-tail call is now picked up natively at its continuation `pc`
(see "Signature and native entry points"). Its own `run_resume_outcome` is the typed error above.

## Measured verdict — the tier stays DEFAULT-OFF
The feature is measurement-gated, and the measurement says **no**: Cranelift is a
correctness-complete mechanism but **not** a performance win over the baseline tier.
Every figure is a **range over independent runs**, one timing process at a time, pinned to a single
core (`taskset -c 3`), on a shared 14-CPU sandbox whose `/proc/loadavg` reads **≈9.5–10 at both ends
of every run** (neighbouring worktrees are the load; it swings by an order of magnitude across days,
so only the *ratios* are signal — never an absolute wall time, and never a single sample).
Column ratios are **cost** ratios (`>1` = slower than the reference).

Per-workload ratio of Cranelift to the baseline, in-crate `crate::jit::bench` (`... -p beam_engine
--features jit-cranelift --test jit_bench -- --ignored --nocapture`), from the min `ns/op` of the
3000-run timed loop. That table runs its tiers in a **fixed** order per workload (interpreter →
baseline → Cranelift), so it is position-biased against Cranelift and is quoted only as the shape
picture; the position-balanced evidence is the ABBA table below.

| workload (shape) | CL ÷ baseline |
|---|---|
| `walk/2` — list walk | **≈0.48×** (the only win) |
| `slice:fib/1` — call chain | 0.95× (tie) |
| `aggr/2` — aggregate reads + `'*'` | 0.99× (tie) |
| `call_ext/0` — declined host seam | 1.07× (tie) |
| `map/2` | 1.08× |
| `build/0` | 1.17× |
| `select/1` | 1.24× |
| `cmp/2` — comparison BIFs | **1.25×** |
| `kernel/2` — inner-product loop | **1.36×** |
| `wide/2` — wide tuple/list reads | **1.39×** |
| `cross/2` — cross-module call | **1.45×** |
| `send_recv/2` — two-process mailbox | **2.18×** |
| `sum/1` — non-tail recursion | **2.50×** |

The order-balanced ABBA harness (`bench.rs`, the `#[ignore]`d `cl_bench` test, `--lib -- --ignored
--nocapture cl_bench`) brackets the same picture with per-round paired ratios (2 samples/tier × 15
rounds, so position drift cancels); paired means over three independent runs:

| kernel | paired cranelift/baseline over 3 runs |
|---|---|
| `aggr` | 0.988–1.009× (tie) |
| `arith` (256 chained `'+'`) | **1.616–1.621×** |
| `cmp` | **1.364–1.422×** |

So the tier is a **tie** on the aggregate-read mix, **≈1.6× slower** on chained small-int arithmetic
and **≈1.4× slower** on comparison-BIF streams.

**Codegen cost — what the hot tier must earn.** Cranelift's one-time compile is **≈6.5–60×** the
baseline's (cheapest on `walk/2` at ≈6.5×, dearest on `build/0` at ≈60×; the range widens under heavy
host load, which inflates the slower compiler far more). A hot tier that compiles that much more
expensively must win on steady-state throughput to earn that cost, and outside `walk/2` it does not.

Why: Cranelift re-loads/re-stores the x-register slots per instruction and **deopts
`call_last`/`call_only`** (a strict subset of the baseline's `Instr` set), whereas the baseline is a
hand-written x86-64 backend that caches the five most-touched x-registers per block, addresses the
rest directly in `machine.x`, links the local tail calls natively and reuses blocks across runs
(§F/`../CONTEXT.md`). Its only real win is the list-walk shape (`walk/2`, ≈0.48× the baseline); the
best everything else reaches is a tie (aggregate reads, the call chain, a declined `call_ext`) or a
mild-to-serious loss (maps, tuple/list builds, `select_val`, comparison streams, wide reads,
cross-module, non-tail recursion, the mailbox) — which cannot justify 6.5–60× the compile latency on a
tier whose whole point is being *hot*.

Consequence: `jit-cranelift` **remains default-OFF** — a correctness-complete mechanism and an
honest negative performance result, not a selected tier. Closing the gap is not a bounded change: it
means porting the baseline's register cache, its direct `machine.x` addressing and its tail-call
linking into the Cranelift lowering. Re-test with `bench.rs` before ever revisiting.

**Measurement caveats (state them with any figure).** The in-crate harness uses **synthetic
modules**, so it covers the tier's inline families, `CallOnly` tail calls and a declined `call_ext`,
but it is **not** the `foreign` / `list_walk` / `gc_bound churn` workloads of `tools/bench`:
`beam_rt` has **no `Tier::Cranelift` selection** (`beam_rt/src/runner/tier.rs` maps `BEAM_RT_TIER` →
`JitMode::{Auto,Interpreter,Baseline}` with `Auto | Baseline ⇒ Tier::BaselineJit`), so the Cranelift
tier is not reachable end-to-end and cannot be measured through the runtime. Also note the `foreign`
workload's *interpreter* side carries a known super-linear `beam_rt` pathology owned elsewhere, so it
is not a clean tier comparison even where reachable.

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
