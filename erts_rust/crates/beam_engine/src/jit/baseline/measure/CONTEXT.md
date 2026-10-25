# `measure` — the baseline tier's measured record

## Intent
Every measured figure, its instrument, its caveats and its rejected alternatives for the native baseline tier, kept in one node so the tier's contract files stay readable.
The mechanisms live beside their code (`../runtime/`, `../emit/{prologue,spill,regalloc,link}/`); this file is the evidence that chose them and the negative results that must not be re-investigated.
The canonical cross-runtime table stays `../../../../tools/bench/RESULTS.md`; the crate-level record is `../../../MEASURED.md`.

## Instrument and discipline (how any figure here was taken)
- Count **retired user instructions** with `perf stat -x, -e instructions:u`, matching the `cpu_core/instructions/u` line (this box is hybrid, so the `cpu_atom` line reads `<not counted>`); perf writes its report to stderr, so a pipeline needs `2>&1 >/dev/null |`.
- The counter is a virtualized PMU reading ≈**17×** the real retired-instruction count, so **only the ratio between two legs measured this way means anything**.
- A per-call figure is a per-rep **slope** `i(N1) − i(N0)` with N large enough that the work dominates the ≈7.05e9-unit boot intercept; never difference two independently minimized samples (run-to-run counts are bimodal on a loaded host — a clean mode plus a phase-dependent mode ~2–3 % higher).
- Sides are **interleaved with the order alternated** round by round, one process at a time, `taskset -c 3`, `/proc/loadavg` recorded at every round, and both the **min and the median** of the per-round ratio reported.
- Wall clock is not resolvable on this host at the 1–3 % effect size; the structural counters are deterministic and identical across repeats, so they are the evidence for a lever and a wall-clock A/B is quoted only as an honest negative.

## Call/return linking and the per-call seam/helper costs
Instrument: the per-rep slope `i(101 000) − i(1 000)` of `BENCH_TIER=baseline BENCH_ITERS=N bench::foreign::foreign_cowboy_bstr_decomposition` (one iteration = `cowboy_bstr:to_lower/1` **plus** `cowboy_bstr:capitalize_token/1`, 438 + 660 IR instructions, 61 + 57 local-call requests per call), pre-change binary vs HEAD, alternating side order, 5 paired rounds, `/proc/loadavg` 29–48.

| leg | slope (instr per harness iteration) | rounds |
|---|---|---|
| pre (33 driver round-trips per `to_lower/1` call) | 433 545.0 / 433 455.6 / 433 457.7 / 433 455.1 / 433 456.9 | 5 |
| post (3 driver round-trips per `to_lower/1` call) | 424 727.2 / 424 723.7 / 424 863.4 / 424 770.9 / 424 772.1 | 5 |

Paired `post/pre` per round: 0.97966 / 0.97985 / 0.98016 / 0.97997 / 0.98001 → **min 0.980×, median 0.980×** (**−2.00 %** retired instructions, zero overlap between the two legs; spread within a leg ≤0.03 %).
That is ≈**8 685 instructions per iteration**, i.e. ≈**289** per removed round-trip (34 removed across the two cases).
Structural counters, deterministic and identical across repeats: `driver_round_trips_per_call` `to_lower/1` **33.00 → 3.00**, `list_walk` (`walk(20)/1`) **23.00 → 2.00**; `block_entry_lookups` 660 000 → 60 000 (foreign, 20 000 iters) and 2 300 → 200 (list_walk, 100 iters) with `block_entry_hits` equal to the lookups and `misses = 0` on both sides; `instr_per_call`, `local_call_requests_per_call`, `step_one_per_call`, `step_ext_per_call`, `driver_interp_steps_per_call` and every static/deopt counter unchanged; `deopt_count` empty ("lowered every executed instance").
Paired `baseline ÷ interpreter` ns/call ratios over 7 alternating rounds read 0.48–0.59 on **both** legs (the untouched interpreter column alone varies ±22 % at load 30–48), so the evidence is the counter, not a throughput number.
`list_walk`'s own ns/element is flat for the same reason and because its remaining round-trips are **per call** (21 → 2 over 20 000 elements), not per element.

## The family-owned instruction path
Most of what a binary-lowercasing workload spends is per-instruction, not per-call: `to_lower/1` is 438 IR instructions for a 30-byte subject, `capitalize_token/1` 660, `emqx_base62:encode/1` 244.
Same instrument and discipline (side order alternated, one process at a time, `/proc/loadavg` recorded per round; `list_walk` slope `i(9 000) − i(1 000)`, `foreign_cowboy_bstr` `i(101 000) − i(1 000)`, `emqx_base62` `i(252 000) − i(2 000)`):

| lever | `foreign_cowboy_bstr` | `emqx_base62` | `list_walk` | rounds / load |
|---|---|---|---|---|
| the family helper stops boxing its `Ok` | −1.85 % cycles slope (`to_lower` 59 170.69 → 58 076.39 /call; `objdump` of the symbol: 2 `malloc` + 3 `free` + 2 drop-glue calls → 0 `malloc` + 1 `free`) | — | — | 5 |
| the whole bitstring family reaches its body without the router's re-match (`../bitstr.rs`) | **−5.50 %** (min 0.94484 / median 0.94496) | **−3.25 %** (0.96722 / 0.96749) | 1.000000 | 3 / 11.4–31.9 |
| post-helper refill set is `cacheable_writes(pc)`, not the block's | −0.104 % (0.998942 / 0.998964) | −0.20 % (0.997557 / 0.998113) | −0.0275 % (0.999725) | 5 / 13.0–54.7 |
| **all three, vs the round's start `56c23f0725`** | **−7.03 % / −7.02 %** (0.9297 / 0.9298) | **−4.39 % / −4.34 %** (0.9561 / 0.9566) | **−0.154 % / −0.137 %** (0.99846 / 0.99863) | 5 / 1.8–3.6 |

Every per-round ratio is sign-consistent and the leg spread is ≤0.04 % of the slope (10–10 000× below the effect).
The steady state on the default tier, one quiet window (load 1.2 → 2.6), min-of-5 with the median of the per-rep medians in brackets; `driver_round_trips_per_call` is 3.00 / 3.00 / 7.00 / 2.00 and `local_call_requests_per_call` 61 / 57 / 21 throughout — none of the three levers moved them:

| workload | min ns/call | median ns/call | instr/call | ns/instruction |
|---|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 3 818.9 | 3 932.2 | 438 | 8.72 |
| `cowboy_bstr:capitalize_token/1` | 5 028.6 | 5 067.2 | 660 | 7.62 |
| `emqx_base62:encode/1` | 2 836.0 | 2 903.2 | 244 | 11.62 |
| `list_walk` (marginal `walk(20) − walk(0)`) | 18.03 ns/element | 18.34 ns/element | 8.020/element | — |

The refill lever's mechanical evidence is the register-cache traffic census (1 358 modules / 103 066 functions / 597 507 blocks): `fill_registers` **3 291 217 → 1 529 718 (−53.5 %)**, helper-site refills **2 938 620 → 1 177 121 (−59.9 %)**, prologue refills unchanged (352 597), `spill_stores` / `blocks` / `cache_slots` / `written_slots` / `instr` unchanged, `emitted_bytes` **315 369 212 → 303 038 719 (−3.91 % = exactly 7 B × 1 761 499** — one `mov r64,[rbx+disp32]` per removed refill).
The narrowing is purely syntactic and decided at allocation time: a helper call **is** the interpreter, so the only register a helper can have changed relative to the block's cache expectation is one written by the instruction **at that `pc`** — a guarded inline lowering that rejected to `jit_step_one`.

What remains on this path is **not in this node**: `families::bitstrings`' `run_commands` (9.6 %) and `create::assemble` (8.8 %), `matching::exec` (7.1 %), `Machine::local_call_seam`'s residual verdict-slot load, and `enter_in_block`'s residual (`set_current_code` + the reduction bump + `ctx.reduce(1)`, all frozen).
The run entry itself is bounded by design: `Machine::reset` refills only each register file's used extent (`machine/registers.rs::used_extent`) and `Machine::label_map` builds a module's map once per machine, 0 times on a re-entry (`machine/label_cache.rs`).
Within this node the largest remaining row is `jit_refresh_y` (5.0 %), whose removal is unsound without closing the `fold_step` `pc + 1` hole.

**The frame-changing helpers re-publish the Y window themselves** (`jit_allocate`/`jit_allocate_heap` publish it from the `(y_base, y_count)` `Machine::push_frame_window` just computed — `JitRuntime::publish_y_window`, no derivation — while `jit_trim`/`jit_deallocate` re-derive it through `JitRuntime::refresh_y_base`; the emitted continue arm never calls `jit_refresh_y` for them).
Same instrument, same discipline, before/after binary of the same worktree, 5 paired rounds:

| workload | per-rep slope before | after | paired min / median |
|---|---|---|---|
| `bench::list_walk::list_walk_decomposition` (N 1 000 → 9 000) | 22 829 883.98 / 22 830 021.50 / 22 829 995.05 / 22 830 024.06 / 22 829 942.61 | 22 489 520.48 / 22 489 594.87 / 22 489 610.86 / 22 489 587.09 / 22 489 490.30 | **0.985087 / 0.985089 (−1.49 %)** |
| `bench::foreign::foreign_cowboy_bstr_decomposition` (N 1 000 → 101 000) | 424 726 / 424 728 / 424 849 / 424 838 / 424 865 | 424 159 / 424 160 / 424 125 / 424 063 / 424 110 | **0.998177 / 0.998296 (−0.17 %)** |

Both legs' own spread is ≤0.033 %, so both deltas are real (zero overlap); `list_walk` clears the 0.3 % bar by 5×, `foreign` does not — kept because neither workload regresses and `list_walk`'s tiny Y-touching blocks are exactly the shape the removed emitted call targeted.
`../tests.rs::the_y_window_traffic_of_the_committed_fixtures_is_measured` reports the committed fixtures' `y_invalidations` at **19**.

## The per-call seam work and the single-spill exit
Same instrument, same discipline: `taskset -c 3`, `setsid -w`, one process at a time, side order alternated round by round, `/proc/loadavg` 4.2–24.9 recorded at every round; `list_walk` slope `i(9 000) − i(1 000)`, `foreign` slope `i(101 000) − i(1 000)`.
Each lever is paired against a binary carrying only its predecessor, so a keeper and a revert are both legible; the combined row is a direct pre-change-vs-HEAD pairing over 7 rounds.

| lever | `list_walk` min / median | `foreign` min / median | rounds / leg spread |
|---|---|---|---|
| A — bake the tail call's resolution into the probe's immediates | **0.995936 / 0.995940 (−0.406 %)** | **0.993313 / 0.993417 (−0.669 / −0.658 %)** | 5 / ≤0.016 % |
| B — one spill on a probed local call's exit (`ExitSpill::Done`) | **0.998059 / 0.998074 (−0.194 / −0.193 %)** | 0.999364 / 0.999606 (−0.064 / −0.039 %) | 5 / ≤0.034 % |
| A + B, pre-change vs HEAD | **0.994016 / 0.994022 (−0.598 %)** | **0.992857 / 0.993020 (−0.714 / −0.698 %)** | 7 / ≤0.033 % |

Every per-round ratio is sign-consistent across all rounds, and the split is additive to within 0.001 pp (A + B = the combined row), which is the internal consistency check on the slopes.
Lever B's `foreign` effect sits at its own leg spread (it is entry-side work that `foreign`'s 660-instruction call body dilutes) and is kept on the strength of `list_walk` — where it resolves at 100× the spread — plus its deterministic counter reduction.
`list_walk` per element: 8 995 865 → 8 942 031 counter units.

Deterministic corpus counters (the register-cache traffic census, 1358 modules / 103 066 functions / 597 507 blocks — these, not the slopes, are the lever's mechanical evidence):

| counter | before | after |
|---|---|---|
| spill calls, exit site | 797 396 | **705 569** (−91 827, −11.5 %) |
| spill calls, total | 2 603 645 | **2 511 818** (−3.53 %) |
| dirty-set spill stores (`spill_slots_at(pc)`), total | 1 496 158 | **1 359 648** (−136 510, −9.12 %) |
| dirty-set spill stores, exit site | 415 661 | **279 151** (−32.8 %) |
| fill registers (liveness) | 3 291 217 | 3 291 217 (**unchanged** — the lever removes stores only) |
| emitted bytes | 316 324 782 | **315 369 212** (−955 570, −0.30 %) |

The emitted-bytes row is measured against the tree that already carries Lever A's immediates.
Those immediates cost **+2 432 640 bytes (+0.78 %)** across the corpus on their own (three extra `mov`-immediate setups per probed tail-call site; per kind, `CallLast (unlinked exit)` **200 → 230** bytes), so the net over both levers is **+1 477 070 / +0.47 %** — code size grew in exchange for −0.6 % retired instructions on both workloads.
`local_call_requests_per_call`, `driver_round_trips_per_call`, `block_entry_*`, `step_one`/`step_ext`/`driver_interp_steps_per_call`, `instr_per_call` and every static/deopt counter are unchanged by both levers, and `deopt_count` stays empty.

## The dead prologue Y-window publish
A block whose **every** `Y` access lies after an in-block `allocate`/`allocate_heap`/`trim` does not emit the prologue's `jit_publish_y`: that frame helper re-publishes the window itself on its in-block continue arm, so the prologue's publish would be overwritten before anything reads it (`Allocation::prologue_publishes_y` — the rule and its soundness argument are in `../emit/prologue/CONTEXT.md`; the static split is `../counters.rs`'s `y_prologue_publishes` against `y_blocks`, the runtime count is `Counters::publish_y`).
Instrument: retired **user** instructions (`perf stat -x, -e instructions:u`, matching the `cpu_core/instructions/u` line — this box is hybrid), as a per-rep **slope** (one rep = one call), the two legs built from the same tree with only this lever differing, sides interleaved with the order alternated round by round, `taskset -c 3`, one process at a time, `/proc/loadavg` recorded at every round.

| workload | pre (instr/rep) | post (instr/rep) | paired min / median |
|---|---|---|---|
| `bench::list_walk::list_walk_decomposition` (`BENCH_TIER=baseline BENCH_K=20`, slope `i(4000) − i(500)`, 5 paired rounds, load band 15.75–17.81) | **22 355 310.8** (rounds 22 355 051.6 – 22 355 532.5) | **21 834 000**-level (rounds 21 833 930.8 – 21 834 120.1) | **0.976676 / 0.976684 (−2.33 %)** |
| `bench::foreign::foreign_cowboy_bstr_decomposition` (`to_lower/1` 438 IR instructions + `capitalize_token/1` 660, slope `i(101 000) − i(1 000)`, 5 paired rounds, load band 16.38–22.95) | **421 134.0–421 362.6** | **419 679.2–419 980.6** | **0.996005 / 0.996398 (−0.40 / −0.36 %)** |

`list_walk`'s per-round `post/pre` ratios are **0.976676 / 0.976681 / 0.976684 / 0.976693 / 0.976695** — zero overlap between the legs (≈520 000 units apart, each leg's own spread ≤0.007 %), so the effect is ~330× the noise.
`cowboy_bstr`'s are **0.997261 / 0.996398 / 0.996005 / 0.996766 / 0.996381**: sign-consistent with zero overlap but small, the entry-side saving diluted by the 1 098-instruction call body.
Corpus-wide static split (the deterministic counter, whole installed-OTP corpus via `the_static_deopt_census_over_the_corpus_is_printed`: 1 359 modules / 103 079 functions): `blocks = 185 141` (`y_blocks`, every emitted range whose body touches a `Y` slot), `publish = 132 908` (`y_prologue_publishes`), **`elided = 52 233` — 28.2 % of `Y`-touching emitted ranges do not publish**; `y_invalidations = 544 282` (the in-block refresh gate is untouched).
Over the committed `erlc` fixtures the same split is **14** `Y`-touching blocks with **6** publishing prologues (**8** elided), `loads=4` / `stores=13` / `invalidations=19` (`../tests.rs::the_y_window_traffic_of_the_committed_fixtures_is_measured`).
Both are deterministic structural counts, not measurements.

Why the workloads respond: `bench_list_walk` is `walk/1` → `walk/3` → `sum/1` (`LEN = 1000`), and **every** `Y`-touching IR block there opens with the frame push — `walk/1` (`allocate 1 1`, `move {x,0} → {y,0}`, `call build/2`, `call_last walk/3`), `walk/3` (`gc_bif '-'`, `allocate 3 3`, then every `Y` read/write) and `sum/1` (`allocate 1 1`, `get_list {x,0} → {y,0},{x,0}`, `call sum/1`, `gc_bif '+'` over `{y,0}`, `deallocate 1`, `return`) — so the gate removes every IR-block prologue publish.
`to_lower/2` has the same shape (`bs_start_match4`, `bs_match`, `allocate 2 3`, then every `Y` access).

### The remaining `jit_publish_y` self time (the continuation sub-blocks) is NOT a live lever — the range-crossing witness is rejected
A call's continuation is emitted as its own range `[pc+1, next continuation address)`, whose `Allocation` sees no `allocate` (it sits *before* the `call`), so `prologue_publishes_y` is true there and the sub-block publishes; `list_walk` has two such ranges per element (after `walk/3`'s `call sum/1` and after `sum/1`'s recursive `call`).
A **range-crossing witness** — a `JitRuntime` flag meaning "the published `[y_base, y_end)` already belongs to the frame the next range will run in", set wherever the window is derived and cleared wherever the frame or the position can change without republishing — does eliminate most of those derivations: over the `jit::bench` workload set on this tier, `publish_y_skips = 325/640` (**51 %**) and `refresh_y_skips = 960/960` (**100 %**), i.e. **80 %** of the 1600 derivations.
It is still rejected, on two independent grounds.
1. **It is a net regression.** Paired retired-user-instruction A/B (7 rounds, side order alternated, `taskset -c 3`, one process at a time, load band 5.6–28.2): `list_walk` **+1.15 %** (post/pre min 1.011495, median 1.011502, zero leg overlap) against `cowboy_bstr` **−0.05 %** (min 0.999229, median 0.999515, legs overlapping — 4–6× below the 0.3 % keep bar). `list_walk`'s hot path is linked `call_last`/`call_only` chains, so `jit_tail_call` (which *must* clear — it pops a frame) charges a store per element while each successor prologue still derives: ≈+1 retired instruction per element. Only long-block code like `to_lower/1` reaches the skips, and there the effect is inside the noise.
2. **Its invalidation set cannot be made exhaustive in-node.** `../runtime.rs::fold_step` encodes `Step::Next` and `Step::Goto { pc: pc + 1 }` as the **same** block-ABI word, so a **family raise whose catch handler is its own `pc + 1`** is folded into an in-block continue; the block then runs off its end and `../emit/link.rs::emit_exit` links **straight into the handler block**, so no driver-loop iteration runs between `Machine::unwind_to_frame` (which truncates `machine.frames` *and* `machine.stack`) and the handler. A handler `Y` read then resolves through the destroyed frame's window: observed interpreter `11` vs this tier `59` (`0x3B` — a word read past the truncated stack). Closing it needs either a family-`Goto` block-ABI word distinct from `encode_target(func, pc + 1)` or an invalidation on that route, i.e. a change in the family arm (`../family.rs`/`../emit/family.rs`) plus the shared `fold_step`.

`../raise_tier_tests.rs` pins that shape as a **frame-discipline** requirement.
It passes because the two emitted window derivations (the block prologue's publish and the gated in-block refresh after a family helper) are **both unconditional** — a variant that made either one conditional alone still read correctly, and only making both conditional diverged — so any future lever that makes a window derivation, the `Machine::set_current_code` store or the reduction charge depend on prior state must close the `pc + 1` hole first.
`y_prologue_publishes` vs `y_blocks` is therefore **not** a counter to move.

## The position store vs the window derivation (the split)
`jit_publish_y` = `JitRuntime::refresh_y_base` (re-derive `[y_base, y_end)` from `Machine::frames.last()` + `machine.stack.as_mut_ptr()`) **then** `Machine::set_current_code(func, pc)`.
Removing **only** the position store (a temporary variant, derive kept) gives a paired retired-instruction ratio of **0.964147 / 0.964160** on `list_walk` — so the store is **3.58 %** of the per-rep count.
The inverse variant (derive removed, store kept) is **not a valid split**: with no window published, the emitted Y guard rejects, the tier deopts and the workload's result is wrong (`result_is_correct = false`), so its ratio is a deopt artifact and must not be quoted.
The derivation is therefore the **majority** of the ≈20 % row — confirmed by `perf annotate` of the 190-byte function (79 K samples): the `#[cfg(test)]` counter probe plus its increment ≈**7.6 %**, `frames.len()`/`×32` ≈**8 %**, the frame deref + `stack` pointer + mul-add ≈**42 %**, the `movl` position store ≈**18.3 %**.
The store is a **frozen** publication (`Machine::current_code` is what a resume and `process_info/2` read), so it is not a removal candidate — which is why the lever attacks the derivation (by not needing it) rather than the store.
Honest attribution: about half of the measured refresh cost in a **test** build is the `#[cfg(test)]` structural counter probe (`Counters::record_publish_y` is the first statement of `jit_publish_y`), so the counter-borne saving is overstated relative to a production build; the sign and the mechanism are unaffected.
The split is printed by `../census.rs::the_static_deopt_census_over_the_corpus_is_printed` as `Y prologue: blocks=… publish=… elided=… invalidations=…`.

## Flat profile
`list_walk`, `perf record -e cpu_core/cycles/u -g --call-graph fp`, 73 945 samples, self time (`perf report --no-children`): `jit_allocate` **16.63 %**, `Machine::local_call_seam` **11.25 %** (sibling-owned), `jit_publish_y` **9.80 %** (20.45 % before the prologue-publish gate), `enter_in_block` **8.74 %**, `call::call_store_impl` **7.61 %**, `jit_deallocate_return` **4.99 %**, `family::jit_family` **4.95 %**, `call::jit_call_store` **4.63 %**, `jit_set_position` **4.37 %** (the ranges the gate demoted off `jit_publish_y`), `CountingContext::resolve_local_call` **2.18 %**, `CountingContext::reduce` **2.12 %**.
Shares inflate as the total shrinks, so compare only across one window.
That profile predates the verdict table, so its `Machine::local_call_seam` row prices a once-per-call host consultation and its `CountingContext::resolve_local_call` row a host call that now happens once per distinct callee per run; the seam's per-call residual today is one bounds-checked verdict-slot load plus the invalidation `Cell` read (see `../../../../machine/CONTEXT.md`'s verdict-table section).

## The variable-length frame and the live-in prologue fill (counters)
Deterministic corpus counters (`../counters.rs`, grand total over **that recorded pass's** corpus — 1 353 modules / 103 008 functions, the then-32 committed fixtures + a 1321-module installed-OTP sample; the corpus as it stands today is `../census/CONTEXT.md`'s, 1359 modules / 103 079 functions / 597 565 blocks / 3 586 410 range-instances):

| counter | before | after |
|---|---|---|
| `blocks` | 597 333 | 597 333 |
| `cache_slots` | 756 632 | 756 632 |
| push/pop ops | 7 167 996 | **2 707 930** (−62.2 %: `blocks + cache_slots` per direction instead of `6 · blocks`) |
| `emitted_bytes` (frame lever alone) | 320 065 790 | **308 853 108** (−3.50 %) |
| prologue loads (live-in fill) | 756 632 | **352 537** (−53.4 %) |
| `emitted_bytes` (both levers, cumulative) | 320 065 790 | **306 024 443** (−4.39 %) |

`ir_instrs = 3 585 490` (that pass's range-instance base), `written_slots = 621 058`, and the per-block cache-size histogram, every fill/spill **count** and both per-block histograms are unchanged — the levers change no register allocation.

**Both levers reduce counts but have no measurable throughput effect on this host (honest negative).**
Interleaved A/B (before = the pre-lever tree, after = the variable-length frame + the live-in prologue fill; 9 paired rounds with the order reversed between rounds, `bench run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound`, one benchmark process at a time, `/proc/loadavg` 8–123 recorded at every run): every case's paired after/before ratio flips sign across rounds and each side's own run-to-run spread is 1.5–5.2×, so no case clears the spread (the min-over-N ratios 0.86–1.98 are confounded, because the two minima come from different rounds).
The same harness *can* resolve a per-instruction difference: the tier ratio (default JIT ÷ `BEAM_RT_TIER=interpreter`) is stable and reproduces `tools/bench/RESULTS.md` §5 (`list_walk` **6.87×** here vs 6.86× there; the default tier is cheaper on 9 of 10 cases), so a ≲2 % wall-clock effect is below this host's resolution.
Do not re-run this A/B expecting signal on a loaded host; the levers' evidence is the deterministic emitted-instruction/byte reduction.

## The entry-module resolution and the priming probe
Every `run_baseline_cached*` slice entry/resume resolves the entry module through `CodeCache::compiled_handle(&Module)` (`../cache/mod.rs`), whose `ModuleKey::of` is a **two-variant** identity: `Digest([u8; 16])` = the module's **load-time MD5** (`Module::checksum`, read as a plain `Copy` — no hash, no allocation) for a real `.beam`, else `Content { name, fingerprint }` over the whole-IR hash (`module_fingerprint`, a full `Debug`-render hash of the decoded IR) for a synthetic all-zero-checksum `Module::default()`.
A real `.beam` therefore resolves with **zero** IR hashes on every slice, primed or not, and only a synthetic module can fall to `module_fingerprint` per slice; the address memo (filled by `cached_module_blocks(&Arc<Module>)` on the cross-module *callee* path and by the host-facing `cache::remember_module(&Arc<Module>)` on the *entry* path) is a redundant micro-optimisation on that digest path.
The interpreter tier never touches `CodeCache`.
Measured by the two-process copy-bound probe (`../tests.rs::the_entry_module_priming_is_measured_on_both_tiers`, both tiers named, with `/proc/loadavg` and the per-pass `module_fingerprint` count): the probe's **synthetic** modules take the `Content` fallback, so unprimed they re-hash per slice (the probe's printed ≈**6.8×**-of-interpreter reading) and primed they read ≈**0.8×** with **zero** hashes — both are **single-run estimates from that probe, not pinned results** (that test asserts the `module_fingerprint` counts only; the authoritative tier ratios live in `../../../MEASURED.md`).
`remember_module` is the entry-path priming API and has **no production caller** — its only callers are tests (`../tests.rs`, `../cache/tests.rs`); a host would call it once per module it enters, and the tier cannot pre-empt it (`compiled_handle` is called before any `baseline::run_compiled*` body).
Deopt inlining — including the natively-lowered receive family and the family arm — does **not** move the wall clock on this shape: the deopt chain is cheap relative to that hash.
A same-function block entry does not re-resolve its lazy function at all: the driver hands the entry the function artifact its previous entry resolved (`DriverContext::entry_cached` for the context-`0` artifact, `CalleeCode::cached` for each callee), so it costs the two-entry block-table probe with no `RefCell` probe and no `Rc::clone`; only an entry into a *different* function (a cross-function transfer, a call into another function, a fresh slice) re-resolves through `CompiledModule::native_entry_cached`.
The driver's one charge per **block entry** against the interpreter's one per **instruction** is a real unit divergence, but ≈1 ns per iteration.

## Rejected: the per-function published `EntryTables` slot (the artifact lookup is not a live lever)
`CompiledModule::function` was reported as 1.5–3.3 % of a `gc_bound/churn` run and re-derived each time: a `RefCell` borrow of the per-function slot, an `Rc::clone`, then `EntryTables::of(func, &function.entries, &function.sub_entries)`. The shape was implemented and **measured as a wash, then reverted** — do not re-open it.
**Attribution first, on the base binary** (`release`, `bench_gc_bound:churn/1` at N=200 000 so boot is ≈3 % of the process, `taskset -c 1 perf record -F 3000 -e cpu_core/cycles/u -g --call-graph dwarf`, 29 542 samples; children / self from `perf report`):
| frame | inclusive | self |
|---|---|---|
| `CompiledModule::function` | 0.28 % | **0.07 %** |
| `runtime::enter_in_block` | 0.20 % | 0.06 % |
| `runtime::native_entry_tables` (inlined) | 0.14 % | 0.00 % |
| `resolve_resume_entry` / `cross_function_entry` (inlined) | 0.15 % / 0.15 % | 0.00 % |
| `runtime::jit_local_call` | 7.03 % | 1.83 % |
| `Machine::local_call_seam` (sibling-owned) | 5.23 % | 5.23 % |
So the reported 1.5–3.3 % does **not** reproduce: the symbol's *inclusive* share — the absolute ceiling for any shape of this lever — is **0.28 %** of the whole process, i.e. ≈0.38 % of the loop once boot is excluded. Inside that 0.07 % self time the split is the out-of-line `call` plus the `RefCell` borrow guard (the `+1`/`-1` on the borrow flag and its `Drop`) first, the `Rc::clone` second, and `EntryTables::of` (five pointer/length stores) a rounding error. `churn` asks the artifact only ≈6 times per churn (three calls into another function of the module — `build_list/2` twice, `build_bytes/2` once — and their three in-block continuation returns), against ≈768 same-function local-call entries that never touch it.
**The shape tried.** Each compiled function built its `(EntryTables, register-file bound)` **once**, in `CompiledFunction::new` (the new single construction site, still the one `EntryTables::of` builder), and `LazyLayout` kept one `Cell<PublishedFunction>` per module function, filled in `CompiledModule::function` at the one point that publishes a function — so the lazy arms of `native_entry_tables`/`entry_tables` answered a repeat entry with one bounds-checked load of a `Copy` value: no `RefCell`, no `Rc`, no re-derivation, compile-on-first-sight preserved (`EntryTables::NONE`'s `u32::MAX` `func` marked "not compiled"), and a missing function index still `None`.
**It worked mechanically.** `function`'s self time fell **0.07 % → 0.01 %** (the residual is `native_entry`/`code_map`), `native_entry_tables` inclusive 0.14 % → 0.09 %, `cross_function_entry` 0.15 % → 0.10 %; deterministic counters read one artifact miss per function first entered and a hit for every repeat.
**It is still a wash.** Paired `gc_bound/churn` retired-instruction slope `i(20 000) − i(0)` (`cpu_core/instructions/u`), 6 interleaved rounds with the side order alternated, one process at a time, `taskset -c 1`, `/proc/loadavg` 3.2–3.8, control rebuilt from source at the base commit: post/pre **0.999148 / 0.999505 / 0.999303 / 0.999044 / 0.999826 / 0.999925** → **min −0.096 %, median −0.043 %** (≈**−495** counter units per churn against a ≈1.14 M-unit slope), i.e. **7× below the 0.3 % keep bar** with the two legs' 0.06–0.07 % spreads overlapping; cycles showed no resolvable effect either.
**Why no variant can clear the bar.** The ceiling is the 0.28 % inclusive above, and three of the six per-churn lookups are the compile-free hit path already costing ≈13 counter units each. What is left on the transfer path is the **frozen skipped-iteration reproduction** (`set_current_code` → `reductions` bump → `ctx.reduce(1)?`) plus the `EntryTables` publication — the same conclusion the per-run in-block memo reached (`../runtime/CONTEXT.md`) — not the artifact lookup. The real per-call weight on this workload is `Machine::local_call_seam` (5.23 % self) over ≈768 same-function local calls per churn, and the tier must not short-circuit it.

## Whole-process, cross-runtime and noise anchors (entry/call-path changes)
The reference point for any entry- or call-path change is `list_walk`'s **instructions per element**, and later steps are reported as **paired ratios** of retired user instructions (min / median of the per-round ratio, ≥5 rounds, sides interleaved with the order alternated, one process at a time), because the absolute charged-unit instrument was retired.
| tree | instr/element |
|---|---|
| m15-era `a34ca61332` | 556.1 |
| pre-optimisation HEAD | 610.5 |
| `ba245ddc9c` (the `call` helper takes the emitter's compile-time resolution) | 570.9 |
| `ce0936464a` (a block entry resolved from the driver's function cache) | 530.6 |
| `d1596601b3` (same-function continuation return tail-jumps in-block) | 497.3 |
| `26a196a1a1` (same-module cross-function continuation return tail-jumps in-block) | **502.2** — the last absolute |
- **Noise floor**: a fresh release build of the same tree measures within **0.01–0.04 %** of the binary under test on both counters, so every effect in the tables above is 13–70× noise.
- **`26a196a1a1` is a `list_walk`/`to_lower` trade, measured**: `list_walk` **+0.90 %** (497.1 → 502.2, min-of-slopes 1.0087 / median 1.0097, zero overlap, reproduced on two independent N pairs to 0.03 pp) against `to_lower/1` **−2.78 %**. It is kept for the foreign-code workloads, which is where the remaining ratio gap is; the `list_walk` cost is the cross-function entry probe that runs on a same-function return too, and reclaiming it needs the same-function probe answered without touching the artifact.
- **`to_lower/1`, whole-process instructions per call**: **130 439–130 466** → **126 812–126 842** (**−2.78 %**, spread 0.05 %, no overlap), a per-rep slope at N = 50 000 → 450 000 with `-pa <bstr dir>`. The pure boot intercept is identical on both sides (7 036 182 170 vs 7 036 275 469, −0.0013 %), so the gain is per-call and the `EntryTables` publication in `enter_compiled_cached` costs no measurable startup.
- **Both transfer directions are linked; the per-call residual is the host-owned seam.** The remaining round-trips are **`3` per `to_lower/1` call** (the slice entry plus the outermost call's own `enter_in_block`/`call_store_impl` round-trip) and **`2` per `list_walk` call**, with `block_entry_misses` still `0`; `local_call_requests_per_call` is the host-consultation counter, and the seam's verdict table holds it at one per **distinct callee per run** instead of one per local call. What is left on the call path is not transfer bookkeeping: `Machine::local_call_seam` — the per-callee verdict table, with the callee identity and the host consultation behind it on the once-per-callee path, so its per-call residual is one bounds-checked slot load plus the invalidation `Cell` read — lives **outside this node** (`src/machine/**`, `src/interpreter/dispatch.rs`); the tier must not short-circuit it. The remaining in-node candidates are `jit_allocate` (a real heap/frame-admission door) and `enter_in_block` (the frozen skipped-iteration reproduction).
- **The in-crate `src/bench` decomposition harness** (`foreign_bstr_to_lower_decomposition`, release, pinned, same window): **min 5965.9 / median 6054.0 ns/call** at `instr_per_call = 438.00`, `local_call_requests_per_call = 61.00`, `driver_round_trips_per_call = 3.00`. The pre-work baseline read min 5521.5 / median 10394.7 in a far more loaded window, so those two are **not** comparable absolutes; the deterministic per-call counters are, and the whole-process instruction count is the figure that moved (130 439–130 466 → 126 812–126 842). What remains on this workload is **not** call setup: 438 IR instructions for a 30-byte lowercase (`~14.6` per byte) is the per-instruction execution cost, a per-*instruction* lever rather than a per-call one -- the two `bs_*` inline levers below take it.
- **`list_walk` against the system oracle, same window, paired** (`tools/bench --quick --workloads list_walk --runtime all`, 3 alternated rounds each, one process at a time): rust **20.37 ns/element** (min-of-runs) / 21.9 (median) against the oracle's **2.15** / 2.49–2.70 → **7.69× (min) / 8.80× (median)** under a load band of 8.6–16.9 (1-min), the round-by-round ratios 7.69 / 8.80 / 10.28; an earlier window at load 5.2–5.7 read **21.03 / 21.45** vs **2.02 / 2.70** → **7.71× / 7.99×**, so the min ratio is unchanged and the median moves with load. Both sides scale with load, so the ratio is the comparable figure and an absolute ns is a same-window reading only, with `case_list_walk_elements` (100 M sys / 50 M rust) the contention-immune anchor. Startup belongs to the boot path, not this node: it reads 3.64× in the quiet window.
- The `tools/bench` `slice`-style system-module per-process rows drift with host load far more than the tier moves them: the baseline-vs-interpreter per-process effect is **1.0–1.3×** (`../../../MEASURED.md`, N=30 interleaved sampling; the `slice` workload itself read 31.97 ms default / 31.92 ms interpreter, min-of-3), and the untouched interpreter column drifted up to ~29 % across a fresh 7-run in-crate eager set. Quote a min-over-N and compare only figures from the same run set; a bare cross-date absolute on a shared host is not evidence.
- **A foreign module needs `-pa <its dir>`** on the `beam_rt` command; without it the run is `error undef` and the counter measures boot only (a ~7.1e9 "per-call" figure is that failure, not a measurement).

## The `foreign` byte-loop kernels: where the baseline tier's cycles actually go
`tools/bench`'s four `foreign` cases (`cowboy_bstr:to_lower/1`, `cowboy_bstr:capitalize_token/1`,
`emqx_base62:encode/1`, `emqx_base62:decode/1`) are **BIF-free compiled BEAM code**, not host
dispatch: the in-crate decomposition harness reads `bif_calls_per_call = 0.00` for both Cowboy
cases (`rooted_calls_per_call = 2.00`, `local_call_requests_per_call` 2–3,
`driver_round_trips_per_call = 3.00`, `step_one`/`step_ext` `0`, `deopt_count` empty,
`block_entry_misses = 0`). So the ratio is the **bitstring family's per-instruction cost**, and any
"BIF-call gap" framing of these four cases is wrong.
**Profile the baseline tier alone, on the real `beam_rt` path, and exclude boot.** `src/bench/mod.rs`'s
`selected_tiers` takes `interpreter|baseline|both`, **defaults to `baseline`** and **panics** on an
unrecognised value. A `both` profile is never valid for an attribution profile: the interpreter leg is
~2.2× slower per call (8 322.9 vs 3 734.6 ns/call on `to_lower/1` at `instr_per_call = 438.00`), so it
owns ~69 % of the samples, and the rows a `both` profile puts in its top ten — `select::pair_eq`
(5.60 %) + `select::first_match` (3.67 %) — are the **interpreter's** `select_val` ladder, which the
native tier inlines (its `select_pairs_by_word_per_call` is `0.00`).
The per-run boot cost must be excluded the same way: an in-crate batch that builds a fresh
`Machine`/context per run folds `AtomTable::fork` / `register_context` / `switch_context` into the
marginal, and that — not the table's hand-over — is what a `drop_glue::<(u64, Program)>` (1.75 %) +
`drop_glue::<Vec<(u64, Program)>>` (1.42 %) or a `*::prepared::prepare` row in such a profile is.
Measured on the runtime path instead — `to_lower/1` at N = 400 000, `cpu_core/cycles/u -F 2000`,
`taskset -c 3`, self time — the table already **hits**: `bitstrings::exec` 21.4 %,
`matching::exec` 16.4 %, `exec_other` 7.6 %, `create::exec::{closure}` 4.6 %, `jit_bitstr` 4.1 %, and
**no `drop_glue` rows at all**. The per-instruction cost is inside the bitstring family, not in the
hand-over: `exec`/`exec_other`/`create::exec` are ≈41 % of self time on `to_lower`, spread over ~20
symbols with no single >10 % hotspot.
**The hand-over itself measures neutral.** The table's placement — the registry's parallel `Vec` and
the context-`0` field rather than a `RefCell` inside the shared `ModuleContext` — plus the run-entry
retention rule read **+0.17 % to +0.34 %** retired-instruction slope against a control rebuilt from
source at the base commit, alternated inside each round at load 1.35–1.36: code-layout noise. They are
there for **mechanism** correctness (no atomic on the per-instruction path, no table destruction per
run entry), never as a speedup, consistent with the neutral record at `../../../MEASURED.md`
(`Measured (binary_append …)`, "Measured neutral, kept only as a simplification").
**Two levers, each measured as its own paired A/B.** The compiled-block inline set for the `bs_*`
shapes (`../inlineset/CONTEXT.md`, `../emit/bitstr_inline/CONTEXT.md`) runs a shape inside a block
instead of through one helper call per instruction, per byte of the subject; the `bs_create_bin`
construction arm on top of it writes an append accumulator's field bytes in-block.
**Lever 1, the `bs_*` match/position shapes — an earlier session's paired A/B, kept as the only measurement of that shape set as a whole.** Measured as a **paired A/B inside one session**: two `beam_rt` binaries built from the same tree by
reverting only `src/jit` to `2115f8d696` (`$BEAM_RT` selects the binary; the bench records which one
it ran in `runtimes[].program`), the two legs **interleaved** round by round, 3 rounds,
`taskset -c 3` + `setsid -w`, `BENCH_TIER=baseline BEAM_RT_TIER=baseline`, release, one process at a
time, `/proc/loadavg` 1.92–3.74 recorded at every leg:

| case (rust, matched N) | sys min ns | no-inline ns | inline ns | **A/B** | per-round A/B | ratio no-inline | ratio inline |
|---|---|---|---|---|---|---|---|
| `case_bstr_capitalize_token_ns_per_call` (N 40 000) | 336.7 | 3 739.2 | **2 834.9** | **1.319×** | 1.322 / 1.319 / 1.317 | 11.11 | **8.42** |
| `case_bstr_to_lower_ns_per_call` (N 50 000) | 326.0 | 2 825.2 | **2 718.0** | **1.039×** | 1.028 / 1.042 / 1.040 | 8.67 | **8.34** |
| `case_emqx_base62_decode_ns_per_call` (N 60 000) | 284.0 | 4 746.4 | **4 318.0** | **1.099×** | 1.099 / 1.080 / 1.083 | 16.72 | **15.21** |
| `case_emqx_base62_encode_ns_per_call` (N 60 000) | 209.2 | 2 931.5 | **2 736.3** | **1.071×** | 1.075 / 1.078 / 1.067 | 14.01 | **13.08** |
- Each case's A/B ratio reproduces to ≤0.6 % across the three paired rounds (spread 0.005 for
  `capitalize_token`), so the effect is far outside this workload's round noise; the `sys` column is
  the same round's oracle leg, and the ratio columns are the load-robust figures.
- **`capitalize_token` gains 1.32× and `to_lower` only 1.04×** because `to_lower`'s inner loop was
  already on the prepared fast form (one `integer` bind per byte) while `capitalize_token` spends its
  cycles on `bs_get_position`/`bs_set_position` + three `bs_create_bin` shapes per element, exactly
  the position/`bs_match` bookkeeping the inline set removes.
- A **cross-session** before/after pair (the "before" round set at `/proc/loadavg` 48–65, the "after"
  at 6–12) reads 13.38→8.86 / 8.96→8.34 / 16.30→15.28 / 12.47→12.98 — consistent with the paired A/B
  on three cases and *wrong* on `encode`, because the oracle leg itself moved 32 % with the load
  (309.2→208.9 ns). Use the paired A/B; a cross-date pair of ratios is not evidence on this host.
**Lever 2, the `bs_create_bin` construction arm — this round's authoritative paired A/B.** A private-append build's fields are written in-block on **three routes**: straight-line stores at `cursor + field_offset` for the byte-aligned route, in-block field stores for a bit-granular plan whose every field is at most `MAX_INLINE_FIELD_BITS` (57) bits, and one `binary all` copy-and-commit for a whole-binary tail (`../emit/bitstr_inline/CONTEXT.md`). Measured on the merged tree as **two `beam_rt` release binaries from the same source tree**: the "inline" leg is this HEAD (`85e5293043`), the "no-inline" leg its base `63daa107fc` built in a detached worktree, so the only difference between the two binaries is the round's `src/jit` + `src/families` diff. `tools/bench`'s `foreign` workload only, `$BEAM_RT` selecting the binary (the bench records it in `runtimes[].program`), `taskset -c 3`, `BENCH_TIER=baseline BEAM_RT_TIER=baseline`, **6 rounds with the two legs interleaved and the order rotated every round**, each invocation `bench run --quick --workloads foreign --runtime all` (3 internal rounds), per-case `ns_per_call` **medians over the 6 rounds**:

| case (matched N) | sys ns | no-inline ns | inline ns | **A/B (median)** | ratio no-inline | ratio inline |
|---|---|---|---|---|---|---|
| `case_bstr_to_lower_ns_per_call` (50 000) | 335.8 | 2 180.9 | **2 155.2** | **1.0119** (1.2 % faster) | 6.50 | **6.58** |
| `case_bstr_capitalize_token_ns_per_call` (40 000) | 316.8 | 2 231.0 | **2 223.9** | **1.0032** (0.3 % faster) | 7.04 | **6.96** |
| `case_emqx_base62_encode_ns_per_call` (60 000) | 210.9 | 2 783.6 | **2 558.2** | **1.0881** (8.8 % faster) | 13.20 | **12.08** |
| `case_emqx_base62_decode_ns_per_call` (60 000) | 286.9 | 3 986.6 | **3 775.7** | **1.0558** (5.6 % faster) | 13.89 | **13.26** |

Per-round A/B (no-inline ÷ inline, measured inside the same round) — the stability evidence:
- `to_lower`: 1.0105 1.0052 1.0129 1.0206 1.0108 1.0161 (median 1.0118).
- `capitalize_token`: 1.0076 1.0182 1.0035 1.0025 1.0084 0.9976 (median 1.0056).
- `encode`: 1.0826 1.1027 1.0894 1.0770 1.1000 1.0817 (median 1.0860).
- `decode`: 1.0637 1.0457 1.0270 1.0825 1.0474 1.0570 (median 1.0522).
- `encode` and `decode` clear the session noise floor in every one of the six rounds; `to_lower`'s 1.2 % and `capitalize_token`'s 0.3 % sit **at** it (their per-round A/B straddles 1.0), so only the two big movers are claimed as effects.
- **The sys-side cross-leg drift is that noise floor, measured:** the *same* oracle binary runs in both legs, and its no-inline ÷ inline ratio reads `to_lower` 1.0081, `capitalize_token` 0.9802, `encode` 0.9955, `decode` 1.0097 — 0.2 % to 2.0 % per case with no code difference at all.
- Discipline: `/proc/loadavg` (1-min) recorded immediately before and after every leg, **1.88–3.22** throughout on a 14-core host, every run pinned to CPU 3, wall 16.8–17.0 s per invocation, one process at a time. The deterministic counters were **identical on all 12 runs** (rust calls: `to_lower` 50 000, `capitalize_token` 40 000, `encode` 60 000, `decode` 60 000), so they are the contention-immune anchor and only the paired A/B is quoted as a timing.
- **The seven newly inlined match-context shapes are coverage-only on these four kernels and contribute none of the deltas above.** `beam_disasm` of the staged upstream sources (`cowboy_bstr.erl`, `emqx_base62.erl`) shows **zero** `bs_match_string`, `bs_skip_utf8`/`bs_get_utf8`, `bs_skip_utf16`/`bs_get_utf16` or `bs_skip_utf32`/`bs_get_utf32` instructions — OTP folds a binary literal ≤ 64 bits into `bs_match` — so every measured delta comes from the **construction arm's helper cut** (the retired `jit_open_append_bin_y` door, and `jit_write_bit_field` now emitted only for a field wider than 57 bits), never from the new shapes. A kernel that reaches one of those seven is what would exercise them; these four do not.
- The per-shape emission counts agree (`../foreign_kernel_tests.rs`'s cache-free compile, ten rows, all passing; deterministically identical on every run): zero deopts on all four kernels and `encode/2`'s `bs_match`/`bs_start_match4` also at zero, with the construction instances splitting as the "construction sites each kernel actually lowers" list below records.
- A `bs_start_match3/4` **fresh** context is also still helper-backed (a rooted 5-word
  `ErlSubBits` allocation); only the reuse/rebind path is inlined.
- Method caveat: the bench's Rust-side `case_*_ns_per_call` is **N-dependent** (its own note records
  this), so only same-N comparisons are valid — every row above used one fixed N per case on both
  legs. A per-instruction C-like fast path inside the family remains the wrong lever: the family cost
  is spread thin across the prepared walk.

**The construction sites each kernel actually lowers** — deterministic, from `../foreign_kernel_tests.rs`'s cache-free compile of the staged upstream source (`SHAPE_CREATE_BIN`'s `inlined` delta plus the static-deopt census):
- `cowboy_bstr:capitalize_token/3` — **3 of 3** builds lower (`x accumulators = 1`, `y accumulators = 2`, `const-bytes fields = 1`), `declined = 0`, `deopts = 0` of the 3 visited instances.
- `cowboy_bstr:to_lower/1`'s `-to_lower/1-` comprehension loop — **1 of 1** build lowers, with the **`Y`-slot** accumulator, `deopts = 0`.
- `emqx_base62:decode/2` — **2 of 6** visited instances lower, both **bit-granular** (`bit-granular = 2`, `aligned = 0`), `declined = 4`, `deopts = 0`.
- `emqx_base62:encode/2` — all three are `binary all` accumulator builds and all three lower (`lowered = 3`, `declined = 0`). Its source is `iolist_to_binary/1`'s **4-byte** result, an on-heap `ErlHeapBits` binary, so the door's **source gate** is what decides whether the route is reachable at all: with an off-heap-refc-only gate every execution refused, and the loaded artifact read `deopt_count[BsCreateBin] = 4.00/call` (the single executed build, every time) with `step_one_per_call = 5.00` (those four plus `BsStartMatch4`'s one). The gate now serves **any** byte-aligned bitstring window through the family's own `raw::heap_bits` accessor — the same one the `fallback` family body reads its bytes through, `HEADER_SUB_BITS` and `ErlHeapBits` both — so the door and the interpreter share one definition of "the source's bytes".

### The create arm's helper symbols, per kernel (self-time shares)
Protocol, shared by the after column and the before column: the native baseline tier alone (`BENCH_TIER=baseline BEAM_RT_TIER=baseline`), release, `BENCH_ITERS=3000000`, `taskset -c 3`, `perf record -g -F 2000 -e cpu_core/cycles/u` around one `bench::foreign::foreign_{cowboy_bstr,emqx_base62}_decomposition` test binary (`--ignored --nocapture`), **two legs per kernel** (a / b) so a row that moves together with every other row is load rather than code, the kernel's own timed phase isolated with `perf report -i <data> --time <t0>,<t1> --no-children -g none --stdio` from the `workload = …` timestamps the harness prints, self time, 8–16 K samples per kernel.
`perf record --clockid CLOCK_MONOTONIC` is required for that slicing — with the default clock the samples sit ~50 s off wall time and `--time` selects nothing.
`/proc/loadavg` (1-min) recorded immediately before and after every leg: cowboy-a **24.55 → 19.55**, cowboy-b **19.55 → 27.42**, emqx-a **27.42 → 17.06**, emqx-b **17.06 → 10.74**, with the 15-min average drifting 16.5 → 16.0 across the session, so the host was shared throughout.
The harness' `min_ns_per_call` on these legs reads **1342.1 / 1388.8 ns** (`to_lower`), **1420.4 / 1543.1** (`capitalize_token`), **1698.1 / 1615.7** (`encode`) and **2655.4 / 2611.4** (`decode`), higher than earlier sessions', so no cross-session absolute is comparable and only the **shares** below are.
The **before** column is the pre-change tree's shares under exactly this protocol, quoted as measured then and **not** re-measured here; several other levers (the `bs_get_utf8`/`bs_get_utf16`/`bs_skip_utf8` shapes) landed between the two trees, so a moved share is the two trees' difference, not one commit's.
`call_registered` and `step_instruction_inner` **do not exist in this harness' binary** (its context is `CountingContext`, and `step_instruction_inner` is inlined away at this HEAD), so those two published rows are answered by the reachable `Machine::exec_bif` + `ExecutionContext::call_bif` and `step_instruction_with_entry` + `step_instruction_body` rows.

**`cowboy_bstr:to_lower/1`** (one `Y`-slot accumulator, absent → `declined = 0`):

| symbol | before | after (a / b) |
|---|---|---|
| `[JIT]` DSO, all blocks | — | 27.29 / 27.42 |
| `jit_open_append_bin_y` | **6.16** | **absent** (0.00) |
| `jit_append_cursor` | — | **1.32 / 1.02** |
| `jit_append_binary_all` / `jit_write_bit_field` / `jit_open_append_bin_any*` | — | 0.00 / 0.00 / 0.00 |
| `enter_in_block` | — | 16.80 / 17.59 |
| `families::dispatch` | — | 0.22 / 0.17 |
| `bitstrings::exec` / `create::exec` | — | 0.29 / 0.28 |
| `BitWriter::push_bits_from` / `raw::copy_bytes` | — | 0.00 / 0.00 |
| `memmove` | — | 0.04 / 0.01 |
| `alloc_words_rooted` | — | 0.07 / 0.08 |
| `step_instruction_with_entry` / `_body` | — | 0.28 / 0.76 |
| `jit_step_one` | — | 0.11 / 0.08 |

Top-5: `enter_in_block` 16.80, `jit_publish_y` 6.82, `CompiledModule::function` 5.47, `call_store_impl` 5.28, `jit_allocate` 4.31.
**It moved, and the profile explains it as a substitution rather than a disappearance:** the deleted `jit_open_append_bin_y` (6.16 %) is gone from the binary and the aligned route's whole door is now the in-block shape proof plus **`jit_append_cursor` 1.32 / 1.02 %**, while the residual is the per-instruction baseline machinery (`enter_in_block` ~17 %, `jit_publish_y` ~6 %, `call_store_impl` ~5 %, `CompiledModule::function` ~5.5 %, `jit_allocate` ~4.5 %, atom-hash `get` 2.85 %) and the family rows are ~0.3 % because this kernel lowers.

**`cowboy_bstr:capitalize_token/1`** (three builds per call, `declined = 0`):

| symbol | before | after (a / b) |
|---|---|---|
| `[JIT]` DSO, all blocks | — | 34.37 / 34.08 |
| `jit_open_append_bin_y` | **5.55** | **absent** (0.00) |
| `jit_open_append_bin` | **0.54** | **absent** (0.00) |
| `jit_append_cursor` | — | **1.18 / 1.39** |
| `jit_append_binary_all` / `jit_write_bit_field` / `jit_open_append_bin_any*` | — | 0.00 / 0.00 / 0.00 |
| `enter_in_block` | — | 13.30 / 13.79 |
| `families::dispatch` | — | 0.13 / 0.11 |
| `bitstrings::exec` / `create::exec` | — | 0.25 / 0.17 |
| `BitWriter::push_bits_from` / `raw::copy_bytes` | — | 0.00 / 0.00 |
| `memmove` | — | 0.07 / 0.01 |
| `alloc_words_rooted` | — | 0.07 / 0.06 |
| `step_instruction_with_entry` / `_body` | — | 0.30 / 1.14 |
| `jit_step_one` | — | 0.10 / 0.10 |

Top-5: `enter_in_block` 13.30, `CompiledModule::function` 6.17, `jit_publish_y` 5.22, `jit_allocate` 4.86, `call_store_impl` 4.04.
**It moved the same way and for the same reason:** both retired door symbols (`jit_open_append_bin_y` 5.55 % + `jit_open_append_bin` 0.54 %) are gone and all three builds now pay the inline proof plus **`jit_append_cursor` 1.18 / 1.39 %**, so the remaining ~13–14 % `enter_in_block`, ~5–6 % `jit_publish_y`/`CompiledModule::function` and ~4–5 % `jit_allocate`/`call_store_impl` are the tier's per-instruction machinery, not the create arm.

**`emqx_base62:decode/2`** (bit-granular; `2 of 6` visited instances lower, `declined = 4`):

| symbol | before | after (a / b) |
|---|---|---|
| `[JIT]` DSO, all blocks | — | 12.78 / 12.67 |
| `jit_write_bit_field` | **6.09** | **absent** (0.00) |
| `jit_open_append_bin_any_y` | 1.06 | **1.47 / 1.28** |
| `jit_append_cursor` / `jit_append_binary_all` | — | 0.00 / 0.00 |
| `bitstrings::exec` | 5.64 | **2.87 / 2.67** |
| `bitstrings::create::exec` (+ `{closure#0}`) | — | 6.19 / 2.55 (a), 6.21 / 2.43 (b) |
| `create::prepared::assemble_prepared` | — | 3.84 / 3.55 |
| `matching::exec_other` / `jit_bitstr` | — | 3.76 / 1.70 (a), 4.14 / 1.70 (b) |
| `step_instruction_inner` → `_with_entry` / `_body` | 8.93 | 2.72 / 6.09 (a), 3.00 / 6.31 (b) |
| `call_registered` → `Machine::exec_bif` + `ExecutionContext::call_bif` | 6.72 | 2.82 + 4.80 (a), 2.78 + 4.64 (b) |
| `alloc_words_rooted` | 5.38 | **0.60 / 0.65** |
| `families::dispatch` | — | 1.56 / 1.43 |
| `enter_in_block` | — | 3.65 / 3.87 |
| `jit_step_one` | — | 1.10 / 1.14 |
| `memmove` / `raw::copy_bytes` | — | 0.18 / 0.00 |

Top-5: `create::exec` 6.19, `step_instruction_body` 6.09, `ExecutionContext::call_bif` 4.80, `jit_refresh_y` 4.66, `assemble_prepared` 3.84.
**It moved and the profile explains it as the per-field helper's removal plus a routing shift:** `jit_write_bit_field` (6.09 %) is **absent** — the ≤57-bit plans write their field bytes as in-block straight-line stores behind the single door — the door itself is the unchanged `jit_open_append_bin_any_y` (1.47 / 1.28 %, against 1.06 % before), and the four declined instances keep the family on the path (`create::exec` 6.19 + `{closure#0}` 2.55 + `assemble_prepared` 3.84 + `exec_other` 3.76 + `jit_bitstr` 1.70), with this kernel's `erlang:bit_size/1` (48 M calls) showing as `exec_bif` 2.82 + `call_bif` 4.80.

**`emqx_base62:encode/2`** (a `binary all` accumulator build):

| symbol | before | after (a / b) |
|---|---|---|
| `[JIT]` DSO, all blocks | — | 10.59 / 10.02 |
| `jit_append_binary_all` | — | **1.14 / 1.23** |
| `jit_append_cursor` / `jit_write_bit_field` / `jit_open_append_bin_any*` | — | 0.00 / 0.00 / 0.00 |
| `families::dispatch` | 4.50 | **6.78 / 7.51** |
| `bitstrings::create::exec` | 2.06 (`{closure#0}`) | 2.11 / 2.24 (+ `{closure#0}` 0.96 / 1.16) |
| `bitstrings::exec` | 3.00 | **1.19 / 1.14** |
| `BitWriter::push_bits_from` | 0.42 | 0.46 / 0.63 |
| `memmove` (`__memmove_avx_unaligned_erms`) | 1.49 | 1.08 / 1.07 |
| `raw::copy_bytes` | absent | absent (0.00) |
| `create::prepared::assemble_prepared` | — | 3.76 / 3.23 |
| `step_instruction_with_entry` / `_body` | — | 0.89 / 2.24 (a), 0.91 / 2.11 (b) |
| `alloc_words_rooted` | — | 0.30 / 0.40 |
| `enter_in_block` | — | 5.81 / 6.66 |
| `jit_step_one` | — | 0.43 / 0.30 |
| `jit_family` / `jit_ext_call` | — | 3.39 / 1.69 (a), 3.38 / 1.33 (b) |

Top-5: `families::dispatch` 6.78, atom-hash `HashMap<Arc<str>,Atom>::get` 6.31, `enter_in_block` 5.81, `CompiledModule::function` 4.09, `assemble_prepared` 3.76.
**The `binary all` tail is a native door here and the arm's whole tier→family transition is gone**, so this kernel's residual family cost is the bitstring **matching** opcodes, not the construction. The encode half of a post-gate decomposition (`--time 1784332.4,1784334.3`, the same protocol: baseline tier only, release, `BENCH_ITERS=1000000`, `taskset -c 3`, `perf record --clockid CLOCK_MONOTONIC -g -F 2000 -e cpu_core/cycles/u`, `/proc/loadavg` **10.94 → 8.59**, ~3 600 samples in the window) reads `enter_in_block` **6.51**, atom-hash `get` **6.24**, `families::dispatch` **6.00**, `flatten_iolist` **3.76** (the harness' own iolist flattening, outside the tier), `CompiledModule::function` **3.49**, `jit_publish_y` **3.03**, `jit_family` **2.70**, `jit_append_binary_all` **1.29** — and **no** `create::exec`, **no** `assemble_prepared`, **no** `bitstrings::exec` and **no** `jit_step_one` above the 1.09 % cut.
The decode half of the same decomposition (`--time 1784334.3,1784339.2`) is where the family create path still lives — `create::exec` **3.42**, matching `exec_other` **2.23**, `memmove` **2.38**, `realloc` **2.48** — because its four **declined** (non-append) builds are untouched by this route.
`jit_append_binary_all`'s own 1.29 % **is** the copy: a `binary all` source's length is a run-time fact, so the copy stays a variable-length `copy_nonoverlapping` behind one positioned call, while the tier emits the shape proof, the admission and the destination publish around it.

**The binary-all route's reachability, in run-time counters** (deterministic, `bench::foreign::foreign_emqx_base62_decomposition` over one loaded artifact, `BENCH_ITERS=200`, release):

| counter | off-heap-refc-only source gate | `heap_bits` source gate |
|---|---|---|
| `deopt_count[BsCreateBin]` | 800 (4.00/call) | **absent (0)** |
| `step_one_per_call` | 5.00 | **1.00** |
| `deopt_count[BsStartMatch4]` | 200 (1.00/call) | 200 (1.00/call) |
| `deopt_count[CallExt]` | 800 (4.00/call), the kernel's `iolist_to_binary/1` | 800 (4.00/call) |
| `census_min_ns_per_call` | 8 424.1 | 4 414.9 |
| `min_ns_per_call` (`encode/1`) | 2 735.0 | 1 807.5 |

`step_one_per_call` is the load-independent verdict: `4 + 1 = 5.00` is **four whole-instruction interpreter re-executions per call** (one per `bs_create_bin` execution, since a `fallback` is `jit_step_one`) plus `BsStartMatch4`, and `1.00` is `BsStartMatch4` alone.
The two wall rows are **not** a paired measurement — the 1-min load was 25.62 and 2.78 on entry — so only the counters and the shares above carry the claim; the exclusive paired A/B is run outside this node.
The static `deopts = 0` that `../foreign_kernel_tests.rs` prints is the **per-compile** counter and reads 0 for a family-served arm as well, so it never proved this route reachable — the run-time `deopt_count[BsCreateBin]` is the counter that does, and it is now 0 because the arm is served, not because it declines.

**Helper calls a `bs_create_bin` build still pays per instruction** (from the emitter, `../emit/bitstr_inline/`):
- The **aligned** route pays **exactly one** positioned `jit_append_cursor` call (`create::create_door::emit_append_cursor`): the accumulator's five `ErlSubBits`/`BinRef` words are proved in-block as refusal-only branches, and the one call is the machine-private `&Machine` byte-cursor query.
- The **bit-granular** route pays **one** positioned `jit_open_append_bin_any` (x-register accumulator) or `jit_open_append_bin_any_y` (`Y`-slot accumulator) call, and then **no per-field call** whenever every field of the plan is at most `MAX_INLINE_FIELD_BITS` (57) bits (`create_field::emit_bit_field_store`).
- A bit-granular plan with **any field wider than 57 bits** keeps one `jit_write_bit_field` call per field (`create::emit_bit_fields`'s else arm); that helper remains defined and reachable only from there.
- The **`binary all` tail** pays **one** positioned `jit_append_binary_all` call (`create::create_binary::emit_binary_all`), which performs both the copy and the build's single window-end commit; the source may be an off-heap refc binary **or** an on-heap `ErlHeapBits` binary (the gate is a byte-aligned bitstring window at both ends, read through the family's `raw::heap_bits`), and a non-bitstring source, a window that is not byte-aligned at both ends, an overlap with the accumulator's own payload or a short room answers `0` and hands the whole instruction to `fallback`.
- `jit_open_append_bin` and `jit_open_append_bin_y` **no longer exist**: they have no definition and no call site in any `.rs` file — the aligned route's inline proof replaced them, so the name survives only in the historical profile tables above.
- No route emits a call for a **field** on the aligned route (straight-line stores) or an **in-block commit** (the window end is one store, or folded into `jit_append_binary_all`).

The caveat that governs every row above: the host is shared, so the **load-normalised wall rows in `../../../../tools/bench/RESULTS.md` are the canonical cross-runtime figures** and only these self-time **shares** are comparable across legs — never the absolute ns, and never a share across the two trees without the other rows moving with it if the load did.

## See Also
- `../CONTEXT.md` → the tier contract (the two artifact layouts, the block ABI, the driver loop, the routing table).
- `../runtime/CONTEXT.md` → the block ABI, `JitRuntime` and the helpers the figures above price.
- `../emit/prologue/CONTEXT.md` → the Y-window publish/refresh mechanism whose measurements are above; `../emit/regalloc/CONTEXT.md` → the register picker; `../emit/spill/CONTEXT.md` → the spill/fill call-site discipline.
- `../census/CONTEXT.md` → the censuses that produce the deterministic counters; `../testing/CONTEXT.md` → the test enumeration.
- `../../../MEASURED.md` → the crate-level record; `../../../../tools/bench/RESULTS.md` → the canonical cross-runtime table.
