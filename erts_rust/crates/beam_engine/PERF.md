# `beam_engine` — Execution-tier performance snapshots

Current-state record of execution-tier performance investigations (the default **baseline-JIT** tier vs `BEAM_RT_TIER=interpreter`) and the fixes they produced.
Detailed current measured figures and execution-coverage evidence live in `MEASURED.md`; this file owns the *tier-regression* narrative (what was slower than what, why, and what changed).
**The sandbox is heavily oversubscribed**, so every figure here is wall-clock with no CPU pinning: a single sample is meaningless — read the explicitly two-sided **ratio** and the `/proc/loadavg` recorded with it, never an absolute number.

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
