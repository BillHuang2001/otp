# `beam_engine` — Measured Performance & Execution Coverage

Current-state measurement and execution-coverage evidence for `beam_engine`.
Relocated from the sibling `CONTEXT.md` (which now only points here) so the crate context stays under the Context Tree per-file read limit.
The numbers are current-state evidence, not a change log: re-run the cited command for the live figure.
**The sandbox is heavily oversubscribed**, so every figure below is wall-clock with no CPU pinning: a single sample is meaningless — read a **range over 3-4 runs** (or interleave two binaries) and never one number.

## Execution coverage
Measured by `tests/census.rs` (an executable census **and** regression gate over synthetic modules and the corpus):
- **107 `beam_loader::Instr` variants = 49 EXECUTED + 58 CLAIMED-BLOCKED + 0 UNCLAIMED** (31 core arms + 76 family-claimed); `unclaimed == 0` is asserted, so a regression fails the build. "Claimed-blocked" = a core arm or a family *owns* the variant but the probe operand returned a typed capability error (mostly degenerate-probe artifacts from atom-ish operands driven with `Operand::Nil`/empty tables). **`TestKind`: 25/25 executed, 0 unclaimed.**
- **Corpus reachability:** 1366/1366 installed `.beam` over **34 applications** load, 2,888,257 instructions, **zero unmodelled opcodes**; the 12 unused variants are the seven native-record variants (an OTP-29 feature this toolchain does not enable), `Deallocate` (every corpus `deallocate` is fused into `DeallocateReturn` by `beam_loader`), `BsTestTail2`, `ExecutableLine`, `DebugLine` and the `Generic` sentinel. Oracle-checked execution-breadth floors (asserted in-suite): happy-path **171/261 modules / 504 calls**, error-path **213/261 modules / 232 targets**.
- **Every remaining gap is a typed error, pinned** (`tests/gaps.rs`; `tests/stdlib_sweep.rs::blocked_mechanisms_report_their_typed_error`; `tests/bitstrings_create_fail.rs`), and the exception reason atoms are seeded by the test host (`tests/support/mod.rs::SEED_ATOMS`, append-only so indices never shift; production `beam_rt` must do the same -- Handoffs #2).

## Measured (interpreter ns/instruction -- `tests/engine_bench.rs`)
Measured at this node's HEAD with `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --test engine_bench -- --nocapture` (median of 5 samples per run).
- per-run **setup floor** (`Machine::reset` + run entry for a 2-instruction run): **~115-125 ns/run** in a quiet window (`setup/empty` is the noisiest row of all; a busy window reaches ~315).
- `dispatch/noop` (shared-label no-op loop): **2.2-2.5 ns/instr**; `dispatch/many-labels` **7.7-8.0**.
- `move/2` **4.2-4.7** ns/instr - `test/2` (no branch) **5.8-6.0** - `ctrl/2` (call/return chain) **3.8-3.9** - `branch-loop/2` (taken test + jump) **7.9-8.0** - `tail-loop/2` (test + `call_only`) **6.9-7.2** - real `erlc` `tailcall:even/1` **6.3-7.0**.
- **`list-walk/1 (recursive)`** -- the BIF-free 1,000,000-element recursive list walk (`sum([H|T]) -> H + sum(T); sum([]) -> 0.`, exactly `7 * N + 6` instructions/run, one small-int `'+'/2` per element, no term built and no heap requested): **5.4-5.7 ns/instr** in the quiet windows (a busy-window sample reads ~11). It is the most sensitive row for the per-call/per-return engine path (one `test` + `allocate` + `get_list` + `call` going down an element, `gc_bif` + `deallocate_return` coming back up); it is still far above the system ERTS JIT (`~0.2 ns/instr` on the same body), most of the residual being the arity-2 `gc_bif` `'+'/2` marshal (see the attribution below).
- `gc_bif` `'+'/2` **13.7-13.8 ns/instr** -- the most expensive instruction; the context's own BIF dispatch dominates it (see the attribution section below).
- the two **`gate/call+return`** and **`gate/call_only`** ns/instr rows (same-body twins of `ctrl/2`/`tail-loop/2`) prove the A.8 gate latched per run costs ~0: a pinned, interleaved before/after 10-pair measurement took `ctrl/2` from **6.14 -> 5.04** ns/instr (10/10 pairs faster) and `tail-loop/2` from **8.09 -> 7.88** (9/10), i.e. latching removed the per-instruction `&mut dyn` call the first cut paid.

## Measured (engine-side per-BIF-call boundary -- `tests/bif_cost.rs`)
`... -p beam_engine --test bif_cost -- --nocapture` isolates the ENGINE's side of a BIF call (clean runs; the sandbox is noisy):
- arity-2 `gc_bif` (the `STACK_ARGS` stack-array path, trivial hook): **11-12 ns**; arity-0 `bif0` **7.5-11**; arity-9 (the reused `args_scratch` wide path) **44-50** -> slope ~**4.2 ns per argument**; `test_heap` -> one `ensure_heap` vtable call + root-set slice **1.7-2.4 ns**.
- `&mut dyn ExecutionContext::call_bif` round trip (trivial hook, 2 args) **~6.0 ns** (about half the arity-2 figure); `Module::atom_table.lookup` **~24 ns** / `Module::atom_name` **~1.4 ns** bound the uncached import fallback.
- **Allocation-free per call** (counting-global-allocator proof): 2048 and 4096 `gc_bif`s both give 4 allocations / 71 bytes per run.
- **Attribution:** the engine's own side is **~11-12 ns (<= 0.15%)** of the runtime's **~10.5 us** per registered BIF call, so **~99.85% is HOST-side** (`beam_rt`'s `ExecutionContext::call_bif`).

## Measured (the reserved-heap `MinimalContext` arena -- `tests/gc_arena.rs`)
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --test gc_arena -- --nocapture --test-threads=1`, same-body comparison against the pre-region shape (one `Box<[Eterm]>` per allocation) reproduced in the test, `N = 100 000` allocations of 4 words (3 200 000 bytes of payload). The **allocation counts are exact**; RSS is `/proc/self/statm` delta over the measured region (4 KiB pages), so it is indicative rather than exact (glibc reuses its arena):
- **reserved-heap region** (`beam_alloc::ReservedHeap`, committed lazily and grown in place): **2** heap allocations (**1** reservation region -- the 400 000-word payload fits well inside the ~4 MiB address-space reservation, so no second region is created), RSS **+611 pages ≈ 2.4 MiB**.
- **box-per-allocation**: **100 020** heap allocations, RSS **+2017 pages ≈ 7.9 MiB**.
- So the allocation **calls** fall ~50 000x (2 vs 100 020), and an allocation now costs essentially its payload: the region is reserved up front as *address space* (lazily faulted, so unused reservation costs no RSS) and grown by raising the committed limit in place. The structural suite also pins the shape: 10 000 x 4-word allocations come from ONE region grown in place (every consecutive pair exactly `words` apart -- zero boundaries), a pointer stays byte-identical and keeps its words across 5 000 later allocations, an oversized request triggers the fresh-region (`MoveRequired`) path while every earlier pointer stays valid, and a zero-word request stays non-null/aligned.
- **Caveat.** The figure is the `MinimalContext` *test* arena; the measured per-BIF interpreter path still allocates nothing per call (`tests/bif_alloc.rs`, above), and the production heap is `beam_rt`'s, not this arena.

## Measured (JIT tier micro-benchmark -- `tests/jit_bench.rs` over `src/jit/bench/**`)
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --test jit_bench -- --ignored --nocapture` (add `--features jit-cranelift` for the Cranelift column). Ten workloads, `3000` timed runs each, warm-up excluded; a native tier is compiled **once** outside the timed loop (that one-time `compile` is **excluded**, and reported separately — 12 us-1.4 ms for the baseline, 0.3-120 ms for Cranelift). `ns/op` is the **mean** and includes the per-run setup (`Machine::reset` + the label-map build).

Every workload **asserts the tiers agree** on its intended value (structural `beam_term::cmp`; the tiers have separate heaps) — never "the JIT is faster" (that is environment-dependent). These tiers are **correctness-first mechanisms, not optimisers** (Part II §F).

**`beam_rt` still hardcodes `Tier::Interpreter` on its process-run path**, so these are **ENGINE-Mechanism** figures — the baseline/Cranelift columns are *not* end-to-end runtime figures (the runtime never selects a native tier yet).

**Feature OFF** — interpreter -> baseline-jit (two runs, showing the sandbox spread):

| workload (category) | interpreter ns/op | baseline-jit ns/op | speed-up |
|---|---|---|---|
| `build/0` (tuple/list BUILDING: `put_tuple2`/`put_list`) | 7.9k-14.3k | 6.7k-8.7k | 1.2-1.7x |
| `map/2` (map build + get + put) | 10.1k-10.5k | 9.1k-9.2k | 1.1-1.2x |
| `select/1` (`select_val`-heavy branches) | 2.5k-2.6k | 1.5k | 1.7x |
| `walk/2` (recursive non-allocating list walk) | 3.3k | 0.40k-0.41k | 8.1-8.2x |
| `slice:fib/1 (20)` (real `.beam` fixture) | 1.96k | 0.59k | 3.3x |
| `kernel/2` (256 chained `gc_bif '+'/2`) | 3.8k | 0.56k | 6.7-6.8x |
| `wide/2` (256 steps over 8 x-registers) | 5.5k | 0.59k | 9.2-9.3x |
| `aggr/2` (128 steps of aggregate reads + `'*'/2`) | 9.9k | 2.2k | 4.4x |
| `cmp/2` (64 rounds of the eight comparison BIFs) | 7.8k | 1.9k | 4.1x |
| `cross/2` (cross-module `call_ext_last` ping-pong -- see the note below) | 11.8k | 16.1k | 0.73x |

`cross/2` is the one workload whose baseline figure is **below** the interpreter's (≈0.73×): a `call_ext_last` is not inlined and a cross-module transfer cannot be linked (separate executable mappings, buffer-local `rel32` fixups), so every hop pays the caller's per-instruction deopt **plus** a driver-visible `switch_into` (context switch + the callee's literal/block resolution) and a fresh callee block entry. The transfer is deliberately **un-amortised** by construction -- the callee's blocks still run natively; only the transfer is not. Detail in `src/jit/bench/CONTEXT.md`.
**Feature ON** (`--features jit-cranelift`) — interpreter -> baseline-jit -> Cranelift (`cl` speed-up over the interpreter; one run — the sandbox was busier, e.g. `build` interpreter read 17.7k vs 7.9k feature-off):

| workload | interpreter ns/op | baseline-jit ns/op | Cranelift ns/op | baseline speed-up | Cranelift speed-up |
|---|---|---|---|---|---|
| `build/0` | 17.7k | 17.3k | 13.4k | 1.0x | 1.3x |
| `map/2` | 9.1k | 7.9k | 8.5k | 1.2x | 1.1x |
| `select/1` | 2.6k | 1.5k | 1.5k | 1.7x | 1.8x |
| `walk/2` | 3.3k | 0.42k | 0.44k | 7.9x | 7.5x |
| `slice:fib/1 (20)` | 1.98k | 0.62k | 0.66k | 3.2x | 3.0x |
| `kernel/2` | 3.8k | 0.59k | 0.82k | 6.3x | 4.6x |
| `wide/2` | 5.5k | 0.61k | 0.88k | 9.0x | 6.3x |
| `aggr/2` | 10.0k | 2.2k | 2.1k | 4.5x | 4.7x |
| `cmp/2` | 7.9k | 1.9k | 2.0k | 4.2x | 3.9x |

- **Cranelift is NOT a win.** It sits behind the default-OFF `jit-cranelift` feature and is **not enabled**. In the fixed-order table it is roughly level with the baseline on `aggr`/`select`/`walk` and clearly slower on `kernel`/`wide`/`fib`/`cmp`. The order-balanced ABBA harness (`src/jit/cranelift/bench.rs::cl_bench_cranelift_vs_baseline_interleaved`, 15 rounds × 2 samples/tier; ratio < 1.0 = Cranelift faster) confirms it: `aggr` **0.946x** (marginally faster), `arith/2` **1.425x** (slower), `cmp` **1.072x** (slower). It is a correctness-first mechanism over the **same** shared interpreter helper, not an optimiser — the tiering seam keeps it available for future inlining work, but the measured outcome is a negative result and it stays off.
- **Local tail-call linking (`call_last`/`call_only`)** removes one driver round-trip per local tail call: `slice:fib/1(20)` baseline-jit **665.6 -> 580.4 ns/op** (paired means over 6 runs, an older sample; re-run for the live figure). The Cranelift tier lowers the frame-less `call_only` the same way (`DISPATCH_REL` -> the `jump` lowering) and inlines `swap`; both are interpreter-identical and counted by `CompiledFunctions::{inlined_swap_count,inlined_tail_call_count}` (`tests/jit_cranelift.rs`).
- **Not done (honest):** `call_ext_last` executes on the baseline tier but as a driver-mediated transfer (a seal-time cross-module link is unsound -- separate executable mappings / buffer-local `rel32` fixups), so the transfer is not amortised; the frame-popping `call_last` stays on the helper in Cranelift (the pop is Rust-side `Machine` state); `PutTuple2`/`BsTestTail` deopt; residency is block-fixed. Both tiers allocate in the never-freed `MinimalContext` arena, so a term-building workload grows RSS with the iteration count (the run counts keep that bounded).
- **Native-tier observability (#9).** Every compiled block's PROLOGUE records its own `(func, pc)` via `Machine::set_current_code`, so `Machine::current_code` is block-exact even across linked chains (which never return to the driver). The store did **not materially regress** `jit_bench`: the per-workload deltas are within the sandbox drift (a controlled run pair moved the untouched interpreter column ~17 %, so quote a min-of-N and compare only same-run figures). A **traced** run (`ctx.trace_enabled()`) refuses native entry and runs the interpreter driver, so a traced tier-run is byte-identical to a traced interpreter run -- O(1) per run, zero cost when untraced.

## Measured (rooted-allocation memory bound -- `src/terms/alloc_tests.rs`)
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_rooted_seam_bounds_peak_live_words_as_the_loop_grows -- --nocapture`. An allocation-heavy loop of `N` iterations runs **twice** over the same synthetic builders — once through an **UNROOTED** context (a fallback that hands out fresh words and never relocates, modelling a host that ignores `alloc_words_rooted`) and once through a **rooted** context that installs a mock **moving semi-space** collector behind `ExecutionContext::alloc_words_rooted` (it relocates every live root on each collection). Each loop keeps a small live set (a running accumulator), so the *retained* footprint should stay constant while the unrooted churn grows with `N`:

| N | BEFORE: unrooted words | AFTER: rooted live words | collections |
|---|---|---|---|
| 1 000 | 18 000 | 8 | 3 000 |
| 5 000 | 90 000 | 8 | 15 000 |
| 20 000 | 360 000 | 8 | 60 000 |
| 50 000 | 900 000 | 8 | 150 000 |

- The unrooted peak grows **exactly linearly** in `N` (18 x `N` words); the rooted live set stays **constant at 8 words** regardless of `N`, and a real collection fires **~3 per iteration** (`N * 3`). The test asserts `collections >= 3 * N`, `live < 64`, a strictly increasing unrooted series and a constant rooted series.
- **Real collection is demonstrated by a test** (not just counted): the live terms survive relocation and stay correct after every collection —
  - `tests/rooted_alloc.rs::a_moving_collector_relocates_the_live_operands_and_the_terms_stay_correct` runs the term-construction arms (`ralloc_tuple`/`ralloc_list`/`ralloc_build`) through the rooted seam under a `MovingContext` that moves the arena on each collection, and reads the built terms back (addresses changed, values intact);
  - `src/terms/alloc_tests.rs::the_mock_collector_relocates_a_root_across_every_collection` and `a_moving_collection_during_the_reserve_keeps_a_built_tuple_correct` pin the same for a `terms::make_*` builder.
- **Caveat.** The collector is a **test mock** (a moving semi-space); the production heap is `beam_rt`'s, and **no crate overrides `alloc_words_rooted` yet**, so the ordinary run still allocates through the unrooted seam and triggers no collection (an `ensure_heap`/root-set hand-off is in place, but the `terms::make_*` call sites have not moved to it — see `CONTEXT.md` Handoff #11). This table is the mechanism-level evidence that the rooted seam *can* bound retained memory, not a runtime figure.
