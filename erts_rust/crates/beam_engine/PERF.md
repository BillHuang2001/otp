# `beam_engine` — Execution-tier performance snapshots

Current-state record of execution-tier performance investigations (the default **baseline-JIT** tier vs `BEAM_RT_TIER=interpreter`) and the fixes they produced.
Detailed current measured figures and execution-coverage evidence live in `MEASURED.md`; this file owns the *tier-regression* narrative (what was slower than what, why, and what changed).
**The sandbox is heavily oversubscribed**, so every figure here is wall-clock with no CPU pinning: a single sample is meaningless — read the explicitly two-sided **ratio** and the `/proc/loadavg` recorded with it, never an absolute number.

## Batch 55 — baseline-JIT code quality: variable-length frame + live-in prologue fill
### What changed
- **Lever A — variable-length native frame** (`src/jit/baseline/emit/mod.rs::native_frame_regs` + `frame_slot_len`, torn down by `src/jit/baseline/emit/link.rs::emit_frame_teardown`): a block's prologue now pushes only `rbx` plus the callee-saved machine registers it actually holds as cache slots — not the canonical six-register `SAVED_REGISTERS` set (`native_frame_regs` pins every per-block subset against it with a `debug_assert!`) — and reserves an `8`- or `16`-byte `rt` slot (`frame_slot_len(k)`, `8` when `1 + slots.len()` is even else `16`) so `rsp` stays 16-byte aligned at every helper call for the pushes the block actually makes.
  A block with no cache slots does no callee-saved push/pop at all.
  `emit_frame_teardown` derives the matching `lea rsp,[rsp+frame_slot_len(k)]` from the same `native_frame_regs`, so the two directions cannot drift.
- **Lever B — live-in prologue entry fill** (`src/jit/baseline/emit/liveness.rs::fill_sets` + `regalloc.rs::cacheable_reads` + `Allocation::entry_fill`): the prologue now loads only the block's **live-in** cache slots (`Allocation::entry_fill`) instead of the whole cache.
  `fill_sets` gains a backward pass (`live = (live \ cacheable_writes) ∪ reads`) whose final live set is the block's live-in set, and `cacheable_reads` (the read-set twin of `cacheable_touches`, fed by new per-shape `reads()` accessors in `emit/{move,aggregate,aggregate/list,arith,compare,bif}.rs`) supplies the reads.
  A cached register the block writes before it first reads is set by that writing instruction and never loaded — the prologue's rule is **self-containment** (each block/sub-block loads its own cache), not the post-helper `live ∪ written` rule.
  The returned `fill_sets` are bit-identical: the only registers whose touches-vs-reads liveness differs are write-set members the fill set's existing `written` union already contains.
- **New file `src/jit/baseline/emit/prologue.rs` (158 lines)** — the block prologue (`emit_block_prologue`: frame setup, the `rt` slot, the position store / Y publish, the live-in cache load) plus its three helpers (`emit_position_store`/`emit_publish_y`/`emit_prologue_helper`), moved **verbatim** out of `emit/block.rs` (`1093 → 993` lines).
  Proof it is a pure move: the deterministic census grand total is byte-identical.
### Structural counters (deterministic, `src/jit/baseline/counters.rs`) — one recorded corpus pass
`src/jit/baseline/census.rs::the_register_cache_traffic_census_over_the_corpus_is_printed` over the generically-discovered installed-OTP corpus (1353 modules / 103 008 functions / 597 333 blocks / 3 585 490 IR instructions):

| counter | before | after | delta |
|---|---|---|---|
| emitted code bytes | 320 065 790 | 306 024 443 | −4.39 % |
| prologue/push+pop register ops | 7 167 996 | 2 707 930 | −62.2 % |
| prologue cache loads | 756 632 | 352 537 | −53.4 % |
| liveness-restricted `fill` loads | 3 695 119 | 3 291 024 | −11.0 % |
| spill stores (dirty set) | 1 594 678 | 1 594 678 | untouched |

`blocks`/`cache_slots`/`written_slots` are unchanged (neither lever changes register allocation).
Repro: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib the_register_cache_traffic_census_over_the_corpus_is_printed -- --nocapture`.
### Wall-clock — HONEST NEGATIVE
9 **paired** rounds (`bench run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound`, **at most one benchmark process at a time**, the order reversed between rounds, `/proc/loadavg` **8 → 123** recorded at every run): every case's paired after/before ratio flips sign across rounds and each side's own run-to-run spread is **1.5–5.2×**, so no case clears the spread (the min-over-N ratios 0.86–1.98 are confounded — the two minima come from different rounds).
- **Verdict: a ≲2 % wall-clock effect, below this host's resolution — no attributable movement.** The evidence is the deterministic count/byte reduction, not a measured speedup; do not re-run this A/B expecting signal on a loaded host.
- The tier-ratio instrument itself *is* stable (default JIT ÷ `BEAM_RT_TIER=interpreter`): `list_walk` reads **6.87×** here, reproducing `tools/bench/RESULTS.md` §5's **6.86×**; the default tier is the cheaper side on 9 of 10 cases.
### Gate
- `-p beam_engine`: **74 suites, 1593 passed / 0 failed / 2 ignored**; with `--features jit-cranelift`: **74 suites, 1646 passed / 0 failed / 3 ignored**.
- Independent end-to-end confirmation: a **52**-case per-case differential slice, **0** mismatch, against the OTP 29.1.1 / erts-17.1 oracle.
- The **Cranelift verdict is unchanged**: `default = []`, no `cranelift*` crate in `cargo tree` without the feature, and the feature still compiles and tests green — nothing reversed.
### Pins
`src/jit/baseline/emit/prologue.rs` (the moved prologue) + `src/jit/baseline/census.rs`'s byte-identical grand total; no previously pinned counter value changed except the frame/fill counts in the table above.

## Batch 54 — block register cache: dirty-set (already-clean-aware) spill stores
### What changed
- **Dirty-set spill stores** (`src/jit/baseline/emit/regalloc.rs` + `src/jit/baseline/emit/regs.rs`): the allocation pass already walked the block once to derive `written`/`y_last`; that same forward walk now maintains the **dirty set** — the cached x-registers written since their last store — for every instruction index, recorded as `Allocation::spill_sets` (indexed by pc, including the exit pc) and read back through `Allocation::spill_slots_at(pc)`.
  `spill(buffer, allocation, site, pc)` therefore stores exactly the registers dirty **at that instruction's own pc** (`mov [rbx + x_offset(x)], reg`) instead of the static write set `written`.
  Membership is untouched — a block still caches the same fixed, live-interval-ranked set of ≤5 x-registers — and `fill` is unchanged.
  The rule is a compile-time table lookup, so the emitted block's *shape* is identical to the written-set model; only the number of stores differs.
- **Soundness rules** (each one-directional — a modelling mistake can only store *more*, never drop a live value): dirty ⊆ written and ∅ at block start (a register the prologue does not load is written before it is first read, so it is dirty from that write on, and nothing resident is unsynced); an instruction lowered **in part** in-block (an inline whose run-time guard can reject) **unions** its `cacheable_writes`; an instruction sent **entirely** to a helper (a full `emit_deopt` — the family arm, the receive/`call_ext`/frame-trio/heap families, the `_` arm) **resets** dirty to ∅, because its pre-call spill has just synced the whole cache through the helper; an **inlined** instruction that writes nothing must **not** reset.
  `is_inlined` mirrors the emitter's dispatch and is biased toward "inlined" (a wrong `true` costs one redundant store; only a wrong `false` could drop a value, and the "certain reset" list is explicit).
  An `Allocation` with no dirty table (a hand-built synthetic) falls back to storing the whole `written` set, exactly as `fill_slots` falls back to the whole cache.
  `fill` deliberately stays `(live ∪ written) ∩ slots` — that is what keeps `written ⊆ fill_slots`, i.e. what makes the reset rule sound.
- **Fused-`deallocate N; return` store-pc fix** (`src/jit/baseline/emit/{block,frame}.rs`): `emit_block` fuses an adjacent `deallocate N; return` into a single `emit_deallocate_return`, but passed the *second* instruction's pc as the spill's `store_pc` — whose dirty entry the forward walk had already reset — so the pre-call spill stored nothing and left `machine.x` stale for the helper.
  `emit_deallocate_return` now takes `store_pc` (the pair's first instruction) separately from `position_pc`.
  Signature of the defect: `--lib` 683 passed / 11 failed, every failure a Y-operand tier-agreement assertion; an isolation experiment (forcing `spill_slots_at` back to the whole `written` set) went green, proving it was confined to that one call site.
  Pinned by `src/jit/baseline/emit/spill_tests.rs`.
### Design source
"Classic handwritten-JIT register-cache store elimination", argued from `../ERTS_RUST_REWRITE_DEEPDIVE.md` §F (the baseline/hot split and its correctness-first register cache) and the read-only legacy reference `../../erts/emulator/beam/jit/x86/beam_asm*.cpp` with `../../erts/emulator/beam/jit/beam_jit_register_cache.hpp`.
That reference is a **position-keyed whole-cache** cache (`consolidate(offset)`/`validAt(offset)` evict the whole cache when the emit position moves) with **no** per-register dirty bit, so this rule is the tier's own design, not a port.
Full design note: `src/jit/baseline/emit/regalloc.rs` module docs + `src/jit/baseline/emit/regs/CONTEXT.md` §"The soundness contract".
### Structural counters (test-only, `src/jit/baseline/counters.rs`) — one recorded pass, all three models
`src/jit/baseline/census.rs::the_register_cache_traffic_census_over_the_corpus_is_printed` (32 committed `erlc` fixtures + a 1321-module installed-OTP sample = 103 008 functions / 597 333 blocks / 3 585 490 IR instructions).
The counters compute the **replaced** models alongside the shipped one **in the same run**, so the store-count comparison is a single measurement — the counterfactual columns are exactly the before/after:

| model | spill stores | note |
|---|---|---|
| whole cache (naive) | 5 578 219 | up to 5 stores per spill site |
| written set (Batch 53) | 4 678 655 | the replaced model |
| **dirty set (shipped)** | **1 594 678** | **−65.9 %** vs written-set |

3 083 977 redundant already-clean stores removed; emitted bytes **341 653 629 → 320 065 790 (−6.32 %)**, exactly 7 B × the removed store count.
Fixtures only (32 modules / 351 functions / 960 blocks / 3 650 IR instrs): spill stores 342 dirty / 1 035 written / 1 473 full-cache; `code_len` **253 272 → 248 288 B**.
`fill` is **unchanged** by this batch: 3 695 119 liveness-restricted loads vs 3 950 215 whole-cache.
The static deopt census is **unchanged** — 3 113 483 instances / 119 840 deopts over 8 deopting variants (FuncInfo 103 008, Bif 10 014, GcBif 3 282, Bif0 3 204, NifStart 300, Test(IsBoolean) 17, Test(IsFunction2) 9, OnLoad 6) — because the batch changes only *how many stores a spill emits*, never which instruction deopts.
Two-process copy-bound probe (`src/jit/baseline/tests.rs::the_two_process_copy_bound_shape_is_measured_on_both_tiers`; 14 blocks / 70 IR instrs / 12 cached slots):

| counter | before (written-set) | after (dirty-set) |
|---|---|---|
| spill stores | 82 (`Helper` 58, `Exit` 24) | **18** (`Helper` 14, `Exit` 4) |
| — whole-cache model | 102 | 102 |
| emitted bytes | 8 266 | 7 818 |
| fill loads | 69 | 69 |
### Wall-clock — both sides named, load recorded
`src/jit/bench` (13 workloads) + the copy-bound probe, `/proc/loadavg` **42 → 57 → 47** over the windows, on **14 cores** (heavily oversubscribed: a single sample is meaningless).
`jit_bench --ignored`, speed-up of baseline-jit over interpreter (ratio = 1/speed-up): `cmp/2` 8.40× (0.119), `wide/2` 7.93× (0.126), `kernel/2` 7.62× (0.131), `aggr/2` 6.67× (0.150), `walk/2` 2.23× (0.448), `slice:fib/1` 2.23× (0.448), `send_recv/2` 1.99× (0.503), `sum/1` 1.97× (0.508), `select/1` 1.51× (0.662), `call_ext/0` 1.31× (0.763), `map/2` 1.20× (0.833), `cross/2` 1.16× (0.862), `build/0` 0.82× (**1.22 = slower than the interpreter**).
**The spread across run-sets exceeds any effect the batch could have**: minutes apart, the same binary read `wide/2` **7.45×** (0.134) and then **7.93×**, and `cross/2` **1.05×** (0.952) and then **1.16×**.
The copy-bound probe is likewise load-sensitive and multi-modal (baseline 12 295 vs interpreter 1 395 ns/op in one window, 129 recorded `WAIT`s) and is structurally neutral to the cache policy (Batch 53).
- **Verdict: no attributable wall-clock movement.** The dirty-set rule removes *stores*, not instructions, and a store is a small fraction of a slice; nothing to revert — a large, sound reduction in emitted store traffic shipped, without a load-bearing speedup. The structural counters are the honest evidence.
### Rejected levers (with the numbers that rejected them)
1. **Per-helper clobber sets to shrink `fill` at a helper site.** The liveness-restricted `fill` is already `(live ∪ written) ∩ slots`, and **no sound *clobber* set narrower than "the whole cache" exists for a helper call**: the heap-admission family (`test_heap`/`allocate_heap`/`allocate` ⇒ `ensure_heap_full`/`alloc_words_rooted`) may be a **collecting** host that rewrites *every* published root in `machine.x`, and the value helpers can allocate through `read_operand`, so a narrower set would need a per-host promise the engine cannot make — the liveness set is the correct conservative choice there. (The **prologue** site is the other half of `fill` and is different: its sound target set is the block's own live-in cached registers (`Allocation::entry_fill`), a property of the block's code that needs no host promise — that lever landed in Batch 55 and removed 404 095 corpus prologue loads; see the Batch 55 section.)
2. **`CACHE_REGISTERS` budget / a better ranking criterion.** The 5-slot budget and the live-interval-descending ranking are the Batches-52/53 design; the counters expose `block_cache_slots`/`block_written_slots`/`live_candidates`/`live_total`/`live_max` so saturation is visible per block, and no per-register benefit counter exists to rank on beyond the interval — nothing to change without a new measurement basis.
### Pins
New: `src/jit/baseline/emit/spill_tests.rs` (the `spill_slots_at` model, a dirty store at a second helper call, no reset on an inlined no-write instruction, the fused-`deallocate+return` store site, the whole-`written` fallback) and `src/jit/baseline/census.rs`'s corpus register-cache census (all three models into one TSV; asserts liveness/consistency only, never a threshold).
No previously pinned counter value changed except the dirty-set store counts themselves.
The tier's **default did not change** (baseline-jit for every `beam_rt` process entry; `BEAM_RT_TIER=interpreter` the only kill switch); the **Cranelift verdict did not change** (still default-OFF, still slower on most shapes).
Independent end-to-end confirmation: `BEAM_RT_TIER=baseline` per-case differential, **25/25 tier-reachable cases agree, 0 mismatch**, against the OTP 29.1.1 / erts-17.1 oracle.

## Batch 53 — block register allocator (liveness-based partial spill/fill) + the sub-block O(n²) fix

### What changed
- **Live-interval register selection** (`src/jit/baseline/emit/regalloc.rs`): the block's cached set is now chosen by **live interval** — each candidate's interval is `[first touch, last touch]` over the block's instruction indices, ranked by **descending interval length**, ties by **ascending register index** — replacing the old usage-weighted touch-count ranking. (The `tests/jit.rs` fixture `swaps_memory_module` was retuned — one `add(2,0,4)` before `Return`, plus the test renamed to `the_multi_register_blocks_cache_the_longest_lived_registers` — so it still exercises x5/x6 in `machine.x` under the interval basis; its second assertion requires both to stay out of the cache.)
- **Partial spill / partial fill** (`src/jit/baseline/emit/regs.rs`, new; `emit/liveness.rs`, new). The block cache is still a *fixed* per-block set of 5 x-registers (`Allocation::slots`, live-interval-ranked), but the register traffic is now liveness-restricted instead of whole-cache:
  - `spill(.., SpillSite)` stores only `Allocation::written` — the block's static write set (`regalloc.rs::cacheable_writes`, a superset of what the inline lowerings actually write). A resident register outside it still holds its `machine.x` value, so storing it is a no-op. Sites: `SpillSite::Helper` (before an in-block helper — the helper *is* the interpreter and reads `machine.x`) and `SpillSite::Exit`.
  - `fill(.., FillSite, pc)` reloads `Allocation::fill_slots(pc)` = `(live ∪ written) ∩ slots`, where `live` is a block-local continuation liveness from a backwards walk (`liveness.rs::fill_sets`). The prologue load stays whole-cache; an empty/absent table falls back to the conservative whole-cache fill.
  - Soundness: every store writes exactly the write set on every path (fast path ⇒ register authoritative; guarded fallback ⇒ `machine.x` authoritative + reload), so the two representations never disagree — proven by a runtime clobber test. **Y slots stay uncached** (honest scoping: full Y-slot caching was not attempted; no gain proven, no wrong result shipped).
- **Sub-block range dedup** (`src/jit/baseline/emit/compile.rs`): each continuation sub-block's range is capped at the **next continuation address** in the same function (`sub_block_end`), so the emitted ranges are disjoint and the total lowered length is O(range) instead of O(n²).

### Structural counters (test-only, `src/jit/baseline/counters.rs`) — full-cache → partial, one recorded pass
Two-process copy-bound probe, baseline-jit tier, recording ON:

| counter | before (full cache) | after (partial) |
|---|---|---|
| spill stores | 91 | 59 (−35 %) |
| — `SpillSite::Helper` | 66 | 41 |
| — `SpillSite::Exit` | 25 | 18 |
| fill loads | 75 | 61 (−19 %) |
| — `FillSite::Helper` | 64 | 50 |
| — `FillSite::Prologue` | 11 | 11 |
| `jit_step_one` deopts | 964 | 964 (unchanged) |

New counters: `record_block` (per-block IR size / emitted bytes / cache- and write-set sizes + histograms), `record_spill`/`record_fill` by site, and both cache models side-by-side in one run (`spill_stores` vs `spill_stores_full`, `fill_registers` vs `fill_registers_full`).

### Interleaved same-window A/B — both sides named, min-over-N=7, primed
`src/jit/baseline/tests.rs::the_copy_bound_tier_ab_is_interleaved_and_min_over_n`:
- **before** (temp full-cache patch, reverted): `Interpreter` min **887.89** ns/op vs `BaselineJit` min **706.71** ns/op → **0.796×**.
- **after** (partial model): `Interpreter` min **905.70** ns/op vs `BaselineJit` min **689.16** ns/op → **0.761×**.
- `/proc/loadavg` over the window: 5.48 → 4.56 → 4.14.
- **Verdict: the wall-clock effect is within noise** (`BaselineJit` min −2.5 %, interpreter min drifted +2 %). The structural counters are the honest evidence: a real reduction in register-cache traffic, not a measured wall-clock win. Nothing to revert — a strict, sound traffic reduction was shipped, just not a load-bearing speedup.

### Live-interval selection A/B (compute-bound, register-pressure shape) — both sides named
`wide/2`-style straight-line body (8 x-registers, 256 `move`+`erlang:'+'/2` steps, ONE basic block; the 5-slot budget IS saturated, `cached_registers=[0,4,5,6,7]`, x1/x2/x3 in `machine.x`) — NOT the copy-bound probe, which is deopt-dominated and structurally neutral to the cache policy:
- `Interpreter` min **4264.3** ns/op vs `BaselineJit` min **608.6** ns/op → **0.143×** (~7.0× faster); stable 6.9–9.6× across four run-sets; the committed `jit_bench` `wide/2` row corroborates (9.26×). Both tiers returned the identical value.
- `/proc/loadavg` 0.94 / 2.65 / 4.54.
- This supersedes the copy-bound probe above as the *allocator's* evidence; the copy-bound figure is the tier's general position, not a cache-policy effect. On the copy-bound probe (blocks never saturate the budget — max cache size 3) the new policy is structurally neutral, as expected.

### Y-slot caching — ATTEMPTED then REVERTED (no code shipped)
A census of the whole committed `erlc` fixture corpus (27 fixtures → 325 functions / 900 blocks / 3,442 IR instrs) found only **12 Y-window accesses total** (2 loads, 10 stores; 21 invalidations; 1 same-block slot repeat). A 1-slot-per-block value cache could therefore save at most 1 access, and 10 of the 12 are STORES a value cache cannot eliminate. It is also not cheaply sound: a cached value cannot cross a linked exit (`emit/link.rs`'s teardown pops every `SAVED_REGISTER`; the successor reloads only `machine.x`) nor survive an in-block helper (which may write the slot or move/null the window). Reverted — **Y slots stay uncached** (guarded `lea`). Kept as evidence: a test-only Y-traffic instrument (`counters.rs`: `y_loads`/`y_stores`/`y_bytes`/`y_blocks`/`y_invalidations`/`y_slot_repeats`, zero production cost), pinned by `src/jit/baseline/tests.rs::the_y_window_traffic_of_the_committed_fixtures_is_measured`.

### Sub-block O(n²) → O(n) (`emit/compile.rs`) — emitted bytes for one call-dense IR block

| calls (`CallExt`, one block) | OLD eager | OLD lazy | NEW eager | NEW lazy |
|---|---|---|---|---|
| 64 | 256 439 | — | 24 032 | — |
| 256 | 3 924 791 | 3 003 703 | 95 456 | 81 148 |
| 1024 | 62 097 719 | — | 381 152 | — |

4× calls → ~4× bytes (linear) vs ~16× before. At 256 calls: ~41× smaller eager, ~37× smaller lazy. Real-module figures are unchanged where a block holds one continuation per tail (the cap is inert). Pinned by `emit/compile.rs::the_continuation_sub_blocks_scale_linearly_not_quadratically` (both layouts) and `::the_disjoint_sub_blocks_still_run_every_call_ext`.

### Pins
No existing pinned counter value changed — the new counters are purely additive. New pins: the interleaved A/B test; the two `emit/compile.rs` sub-block tests; the partial-fill clobber test. `src/jit/baseline/tests.rs::the_receive_and_send_family_is_lowered_without_a_jit_step_one_deopt`, `tests/jit_real_module.rs` (736/736 tier equivalence), `tests/jit_cross_module.rs` and `tests/jit_linking.rs` all stay green; `-p beam_engine` = **1408 passed / 0 failed / 2 ignored**; `--features jit-cranelift` builds warning-free.

## Batch 52 — item 2: baseline-JIT `send_recv` regression (CLOSED)

### Symptom
On the two-process `send_recv` copy-bound workload (`../../tools/bench`, the `copy_bound` case: `T = payload(), Worker ! {self(), T}, receive {Worker, T2} -> case T2 of T -> … end after TIMEOUT -> exit(message_timeout) end`, the worker answering `binary_to_term(term_to_binary(T))`) the DEFAULT baseline tier was **2.15×–3.31× dearer** than the interpreter tier (Rust-vs-Rust; no oracle involved).

| run | default (baseline-JIT) | interpreter | ratio (default ÷ interp) |
|---|---|---|---|
| pre-fix, `--full` | 79 518.12 ns/op | 36 969.39 ns/op | 2.15× dearer |
| pre-fix, `--quick` | 97 517.77 ns/op | 29 492.93 ns/op | 3.31× dearer |
| post-fix, `--quick` | 23 219.88 ns/op | 27 165.83 ns/op | **0.855×** (default faster) |
| post-fix, `--quick` ×2 | 23 617.33 / 23 777.75 | 28 759.25 / 22 829.74 | 0.821× / 1.041× |
| post-fix, `--full` (10 samples) | 22 320.07 ns/op | 22 899.16 ns/op | **0.975×** |

Post-fix sibling `copy_bound` cases: `etf_roundtrip` 7 520.93 vs 8 187.27 = 0.919×; `ets_copy` 3 227.04 vs 4 367.28 = 0.739×. Every post-fix run is parity-or-better, so the regression no longer reproduces (`/proc/loadavg` 2.4–14.5 across the runs).

### Diagnosis (structural counters, not wall-clock)
Test-only counters (`src/jit/baseline/counters.rs`, `#[cfg(test)]`, absent from every production build) count per-`Instr`-variant `jit_step_one`/`jit_step_ext` deopts, block entries/lookups/entered, driver round-trips by block-ABI word, and cache IR-fingerprint calls. The three hypothesized causes were **measured and are ≈0**; the real cause is elsewhere:
- *(a) per-instruction `jit_step_one` deopt cost* — real but small: a helper-executed instruction does not end the block, but it pays a spill of the cached x-registers + call + interpreter step + refill. The `Test` class it dominated was 128 deopts in a 64-op counting pass; total `jit_step_one` 1092 → 964 (~3 % of an op).
- *(b) block-boundary driver round trips* — **not a factor**: block entries lookups/hits/entered are 387/387/387 and round-trips `{"GOTO":257,"HALT":1,"WAIT":129}`, **identical** before and after.
- *(c) `emit/select.rs::select_val_plan` refusing the receive match* — **wrong premise**: `case T2 of T` lowers to two `is_eq_exact` `Test`s, not a `SelectVal`. Those `Test`s *were* deopting (fixed below).
- *lazy `CompiledModule::native_entry`/`CompiledFunction` lookup* — not a factor (O(1) `RefCell` probe + `Rc::clone`).
- **ACTUAL ROOT CAUSE: a per-slice full-IR re-hash in `CodeCache`.** Every `run_baseline_cached*` slice entry/resume resolved the entry module through `CodeCache::compiled_handle(&Module)`; its O(1) address memo is filled only by the cross-module `Arc<Module>` route, so the bare `&Module` entry fell through to `ModuleKey::of` → `module_fingerprint` (a `Debug`-render hash of the whole decoded IR) once per slice — i.e. once per `receive` park and per resume. The interpreter tier never pays it. `fingerprint_calls()` = 1/slice pre-fix, **0** post-fix.

### Fix
- **O(1) cache identity** (`src/jit/cache/mod.rs`): `ModuleKey` is a private two-variant enum — `Digest([u8;16])` = the module's **load-time MD5** (`Module::checksum()`, `Copy`, no hash and no allocation) for a real `.beam`, else `Content { name, fingerprint }` over the whole-IR hash for a synthetic `Module::default()` (all-zero checksum). Variants hash their discriminant so they can never cross-key. This removes the per-slice re-hash with no `beam_rt` change.
- **Native `send`/`receive` lowering** (`src/jit/baseline/receive.rs` + `emit::emit_receive`): the whole receive-control family (`Send`/`LoopRec`/`LoopRecEnd`/`Wait`/`WaitTimeout`/`Timeout`/`RemoveMessage`) runs **in-block** through `jit_receive` (the same family body the interpreter's dispatch runs) instead of a per-instruction deopt.
- **In-block comparison-test lowering** (`src/jit/baseline/test_helper.rs` + `emit/test.rs::emit_test_helper`): the six comparison `Test`s (`is_eq_exact`/`is_ne_exact`/`is_eq`/`is_ne`/`is_lt`/`is_ge`) at two operands with a resolvable non-`NO_FAIL` fail label are lowered in-block via the boolean helper `jit_test`, interpreter-identical by construction (routed through `beam_term`'s `cmp`/`cmp_arith`; a boxed pointer is never word-compared). `TEST_TRUE` refills and continues in-block, `TEST_FALSE` branches to the resolved fail label, anything else hands the driver the block-ABI word. `NO_FAIL`, unresolved labels, non-comparison kinds and ≠2 operands keep the old `jit_step_one` path byte-identically.

In-process confirmation on a real loaded `.beam` (`src/jit/cache/tests.rs::the_entry_and_resume_slice_cost_is_measured_on_both_tiers`, loadavg ≈33.95): interpreter **351.19** vs baseline-jit **101.70** ns/slice-entry = **0.29×**, with **0** IR hashes.

### Pins
- `src/jit/cache/tests.rs::a_real_loaded_beam_never_rehashes_its_ir_on_any_entry_path` — 0 fingerprints across all four `run_baseline_cached*` paths plus park/resume.
- `src/jit/cache/tests.rs::{remember_module_priming_makes_all_four_entry_paths_rehash_free, two_loads_of_the_same_beam_share_one_digest_keyed_artifact, the_key_of_a_real_beam_is_its_load_time_digest, a_memoized_lookup_does_not_rehash_the_ir, different_content_with_the_same_name_does_not_collide}`.
- `src/jit/baseline/tests.rs::the_two_process_copy_bound_shape_is_measured_on_both_tiers` plus its in-block comparison-test pin and the declined control, alongside the existing `the_baseline_tier_agrees_with_the_interpreter_on_the_declined_call_ext_seam_count` (which still passes, proving the single-resolution `call_ext` design is intact).

## Batch 52 — item 6: the `slice` per-process rise is a host-load artifact (NO CODE CHANGE)

Claim: the `slice` system-module per-process rows rose +7–44 % vs m9 (`gb_trees_size` 18.13→25.12, `base64_encode_size` 19.44→26.68, `ordsets_from_size` 18.09→26.15, `queue_from_size` 22.40→26.00, `string_str_length` 41.29→48.12 ms), hypothesised to be the lazy per-function compile.

**Verdict: not real — a measurement/load confound.**
- The lazy per-function compile landed **2026-10-12 02:16** (`78b39e77dd`), *before* the m9 record it is compared against (2026-10-13 22:17) — m9 already contained it; m9→m10 differ only by the m10 batch, whose `jit/cache` changes *remove* per-process cost.
- Committed interleaved A/B (N=30): default 31.86–45.45 ms (min 31.86) vs interpreter 25.51–40.48 ms (min 25.51) → **1.22–1.25×**, and the `slice` workload's own row read **1.00×**. The eager whole-module path this replaced read **405.29 ms vs 33.68 ms ≈ 12.3×**, so lazy compilation is a large *win*, not the regression.
- In-crate eager tier table (`tests/jit_bench.rs -- --ignored`, 7 repeats, `/proc/loadavg` 8.01 throughout): the default tier is **faster on 11/12** workloads (0.127×–0.659×), parity on the deopt-dominated `build/0` (0.856×–1.157×) and the cross-module `cross/2` (0.941×–0.969×).
- The untouched interpreter column itself drifted up to ~29 % across those 7 runs, and within the m10 archive one run set shows the pure fixture `add` *falling* −5.67 % while every system-module case rose — a code change cannot do both.

**The one real, attributable engine cost behind this hypothesis is now removed:** the `CodeCache` entry-path IR re-hash (item 2's root cause above, now O(1) via the digest key). It scales with module size (~2 µs tiny, ~141 µs at 2048 instructions), i.e. up to a few hundred µs per process on a `string.beam`-sized module.

**Closed in Batch 53** (it was a latent hazard, *not* the slice rise): `emit_sub_blocks` / `emit_shared_sub_blocks` (`src/jit/baseline/emit/compile.rs`) now cap each sub-block range at the next continuation address, so the ranges are disjoint and the emitted code is linear. The old shape — one block `[pc+1, block.end)` per frame-preserving call with **no range dedup**, O(n²) for a call-dense straight-line block (a 256-`call_ext` unrolled block emitted ~33k instructions) — is gone (256 calls: ~3.9 MB → 95 KB eager, ~3.0 MB → 81 KB lazy). See the Batch 53 section above.
