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

## Measured verdict — the tier stays DEFAULT-OFF
The feature is measurement-gated, and the measurement says **no**: Cranelift is *not* a win. Two
independent harnesses agree (the fixed-order tier table in crate `tests/jit_bench.rs`, and the
order-balanced ABBA harness below). Every figure is the min of ≥30 samples per tier — the sandbox is
shared and heavily loaded (load average 20–72 observed while measuring), so only the min is stable.
The table gives the range over 6 independent sessions; the per-round paired ratio
(2 samples/tier × 15 rounds, so position drift cancels) brackets it the same way.

| workload (shape) | cranelift/baseline ns/op | paired-ratio range | one-time compile |
|---|---|---|---|
| `aggr/2` — aggregate reads + `'*'` | **0.92–0.95× (5–8% faster)** | 0.61–1.07 (mean 0.88–0.95) | ~140–295 ms vs ~2 ms |
| `arith/2` — 256 chained small-int `'+'` | **1.34–1.44× (34–44% slower)** | 1.05–1.65 (mean 1.33–1.44) | ~22–47 ms vs ~0.3–0.5 ms |
| `cmp/2` — 512 comparison BIFs | **1.00–1.08× (tie to 8% slower)** | 0.75–1.19 (mean 1.01–1.08) | ~55–122 ms vs ~1.2–2.5 ms |
| `slice:fib/1` (call chain) | 1.01× (tie) | — | ~1.4 ms vs ~0.05 ms |

The `aggr` win and the `arith` loss are far outside the noise band (tight paired ratios); `cmp`
ranges from a tie to a small loss depending on how loaded the box is (the baseline slows more than
Cranelift when contended, which narrows its lead). Note also that **fixed-order measurement
understates the Cranelift loss**: `tests/jit_bench.rs` always runs the baseline first, so drift is
charged to the later tier — the ABBA harness is what separates position from cost.

Why: the baseline's emitter is a hand-written x86-64 backend with a per-block register cache (the
five most-touched x-registers resident, the rest addressed directly in `machine.x`), native linking
of the local tail calls (`call_only`/`call_last`) and a cross-run compiled-block cache. Cranelift
re-loads/re-stores the x-register slots per instruction and **deopts `call_last`/`call_only`** (a
strict subset by `Instr` variant), so it pays for every straight-line inlined op and never links a
tail chain. It wins only on the aggregate-read mix — 5–8%, which cannot justify 45–80× the compile
latency on the tier whose whole point is a *hot* tier, and it loses on everything else.

Consequence: `jit-cranelift` **remains default-OFF**; the tier is a correctness-complete mechanism
(interpreter-identical by construction) and an honest negative performance result, not a selected
tier. Closing the gap is not a bounded change — it means porting the baseline's register cache, its
direct `machine.x` addressing and its tail-call linking into the Cranelift lowering. Re-test with
`bench.rs` before ever revisiting.

## Routing Table
- `codegen.rs`/`inline.rs`/`shape.rs`/`compare.rs` → the inline set + shape tests; `report.rs` → the introspection report; `tests.rs` → in-module unit tests; `bench.rs` → the `#[ignore]`d ABBA Cranelift-vs-baseline measurement harness; `mod.rs` → the backend + accessors.
- Tier equivalence on real modules: crate `tests/jit_cranelift.rs`, `tests/jit_cranelift_real.rs` (see `../CONTEXT.md`).
- `bench.rs` is run with `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --features jit-cranelift --lib -- --ignored --nocapture cl_bench`.
