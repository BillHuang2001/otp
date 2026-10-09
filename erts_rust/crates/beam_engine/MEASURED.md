# `beam_engine` — Measured Performance & Execution Coverage

Current-state measurement and execution-coverage evidence for `beam_engine`.
Relocated from the sibling `CONTEXT.md` (which now only points here) so the crate context stays under the Context Tree per-file read limit.
The numbers are current-state evidence, not a change log: re-run the cited command for the live figure.

## Execution coverage
Measured by `tests/census.rs` (an executable census **and** regression gate over synthetic modules and the corpus):
- **107 `beam_loader::Instr` variants = 49 EXECUTED + 58 CLAIMED-BLOCKED + 0 UNCLAIMED** (31 core arms + 76 family-claimed); `unclaimed == 0` is asserted, so a regression fails the build. "Claimed-blocked" = a core arm or a family *owns* the variant but the probe operand returned a typed capability error (mostly degenerate-probe artifacts from atom-ish operands driven with `Operand::Nil`/empty tables). **`TestKind`: 25/25 executed, 0 unclaimed.**
- **Corpus reachability:** 1366/1366 installed `.beam` over **34 applications** load, 2,888,257 instructions, **zero unmodelled opcodes**; the 12 unused variants are the seven native-record variants (an OTP-29 feature this toolchain does not enable), `Deallocate` (every corpus `deallocate` is fused into `DeallocateReturn` by `beam_loader`), `BsTestTail2`, `ExecutableLine`, `DebugLine` and the `Generic` sentinel. Oracle-checked execution-breadth floors (asserted in-suite): happy-path **171/261 modules / 504 calls**, error-path **213/261 modules / 232 targets**.
- **Every remaining gap is a typed error, pinned** (`tests/gaps.rs`; `tests/stdlib_sweep.rs::blocked_mechanisms_report_their_typed_error`; `tests/bitstrings_create_fail.rs`), and the exception reason atoms are seeded by the test host (`tests/support/mod.rs::SEED_ATOMS`, append-only so indices never shift; production `beam_rt` must do the same -- Handoffs #2).

## Measured (interpreter ns/instruction -- `tests/engine_bench.rs`)
Measured at this node's HEAD with `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --test engine_bench -- --nocapture` (median of 5 samples per run). The sandbox is heavily oversubscribed, so a **single sample is meaningless**: the same row moves by ~1.6x between a quiet and a busy window, so read a range over 3-4 runs (or interleave two binaries) and never one number.
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

## Measured (JIT micro-benchmark)
`... -p beam_engine --test jit_bench -- --nocapture` (add `--features jit-cranelift` for the feature-ON roster). Wall-clock ns/op -- noisy; the tiers are **mechanisms, not optimisers** (compile-once/run-many; one-time `compile` excluded).
- **Feature OFF** -- interpreter -> baseline-jit speed-up: `fib/1(20)` 2.4x; `kernel/2` 13.6x; `wide/2` 13.9x; `aggr/2` 11.8x; `cmp/2` 14.9x.
- **Feature ON** -- interpreter -> baseline-jit -> Cranelift: `fib` 2.0x; `kernel` 11.1x; `wide` 13.0x; `aggr` 6.2x; `cmp` 11.0x (not yet a win over the baseline).
- **Local tail-call linking (`call_last`/`call_only`)** removes one driver round-trip per local tail call: `slice:fib/1(20)` baseline-jit **665.6 -> 580.4 ns/op** (paired means over 6 runs).
- **Not done (honest):** `call_ext_last` still round-trips through the driver; `PutTuple2`/`BsTestTail` deopt; residency is block-fixed.
- **The engine's own hot path is O(1) and per-BIF allocation-free** (dense `LabelMap` fetch, Vec-index registers; a wide BIF/`call_ext` reuses a per-`Machine` `args_scratch`, pinned by `tests/bif_alloc.rs`); the multi-microsecond per-iteration cost of `beam_rt run ...` is HOST-side (see above; also `../beam_rt/CONTEXT.md` and `../beam_bifs/CONTEXT.md` "Performance Notes").
