# `beam_rt` — per-call dispatch and copy-out cost (`DISPATCH-CUT` · `COPY-OUT-CUT`)

The two per-call cost analyses of the runtime's BIF call path — the registered-BIF dispatch prologue and the scheduler-shape BIF-result copy-out — each priced in-process with `examples/perf_call.rs` at the HEAD it names.
These are **cut records, not the current snapshot**: each prices what one change moved, as a before/after ratio measured on one host, never as today's absolute per-call figure.
Current cross-runtime and execution-tier figures are in [`PERF.md`](PERF.md); the dated `BEFORE`/`AFTER` cross-runtime snapshots (including the CLI-level `nb*` registered-BIF columns) are in [`PERF_HISTORY.md`](PERF_HISTORY.md); the harnesses, the dispatch-cost narrative and the trace seam are in [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md).
`DISPATCH-CUT` cuts the registered-BIF dispatch prologue (the per-context dispatch memo, the byte-hashed slot, the empty-NIF short-circuit, and the path-C regression the byte hash introduced); `COPY-OUT-CUT` cuts the BIF-result copy-out's wasted graph walk and needless retry.
Every number here is **measured, never modelled**, and absolute timings are load-polluted on every host this was run on — read the ratios and the interleaved pairs, with both sides named.

## Measurement provenance — the arm labels (`Path A`/`Path B`, `copy-out ON`/`OFF`)
The two analyses below label their arms **as they were at the HEAD each figure was taken at**, and at every one of those HEADs the BIF-result copy-out had exactly **one** destination: the running process's scanned young heap.
"`copy-out OFF`" and "`Path A`" therefore mean that the copy-out's **process-heap arm was unreachable**: `RuntimeContext::new` leaves `mailbox_table` as `None`, and `move_result_into_process_heap` opened with `mailbox_table.as_deref_mut()?`, so such a context neither copied the result nor rewound the pool.
That is no longer what the same construction does: today `RuntimeContext::call_registered` re-homes a container result on **both** routes — the process's young heap when there is a process, the runtime's `-eval` store (`Runtime::eval_heap` + the escaped-payload table `Runtime::eval_off_heap`, `src/context/pool.rs::move_result_into_runtime_store`) when there is none — and rewinds the pool either way, so the two arms now differ only in their **destination**.
Every figure in this file stands as the record of the comparison it names, never as a prediction of what the same two rows would read on today's tree, where an A/B of these two arms would no longer isolate the copy-out.
The attribution those figures support — that most of the ≈ 149 ns/call is the copy itself — was drawn from the arm that really skipped the copy, which is exactly why it still stands.
[`PERF_HISTORY.md`](PERF_HISTORY.md), [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md) and `examples/perf_copy_out.rs` (whose printed rows keep the historical `copy-out OFF`/`ON` text) all point here rather than restating it.

## `DISPATCH-CUT` snapshot — HEAD `af6d8b07bd` (+ `1b643ead0b`)
### What changed
Three cuts to the **registered-BIF dispatch prologue** (`src/context/calls.rs`,
`src/context/dispatch.rs`, `src/nif.rs`):
1. **The memoized `BifFn` is invoked directly.** `call_registered` now takes the resolved
   `BifFn` (from `DispatchMemo`, via `call_bif_routed`'s `dispatch.bif`) and calls it through the
   new `invoke_registered`, instead of re-running `BifRegistry::call` → `BifRegistry::lookup` —
   i.e. the second `HashMap<String,_>` SipHash lookup per call is gone. `BifRegistry::lookup`
   remains the *only* source of truth for "is this key registered": the memo is never consulted
   as a registry, so it cannot drift.
2. **`dispatch_slot` hashes the key's bytes** (FNV-1a over `module`, `0xFF`, `function`, then the
   arity), not just the name *lengths*. The old length-only mix made same-length keys collide by
   construction (`ets:insert/2` and `ets:lookup/2` shared a slot), so an ETS insert/lookup loop
   was a permanent memo **miss**, paying the whole miss prologue on every iteration.
3. **`NifRegistry::is_empty()` short-circuit** in `try_call_nif`, so an empty NIF registry costs
   one `RefCell` borrow + one length compare instead of a third SipHash lookup.
Items 4 (`RuntimePorts::new` construction) and 7 (`BifContext` size) from the ranked list were
**not** taken: item 4's unconditional construction is the only thing that keeps the shared
`ports` cell reachable from every arm's `BifContext` (gating it needs nine more field types for
~two `Rc` bumps), and item 7's 20 fields are built through one chained builder shared by all
arms. `reentrancy_capable` (the arity-proportional `is_fun` scan) was **kept** — it is what makes
the shared-tables borrow safe. The rewind preconditions (`safe_to_rewind`: empty source table,
`!reentrant`, `Ok`, scheduled process, successful copy) and the `pending.is_empty()` guard before
`atoms_mut()` are unchanged.
### Method
`examples/perf_call.rs` (in-process, best of 5, 100 000 calls/sample, one warm-up run), **release**
profile. Marginal = `ns/call − control`; the control is the same loop with inlined arithmetic only.
**BEFORE** = the pre-cut tree, **AFTER** = the cut tree (both rebuilt at their own HEAD). Three
runs each; the tables below are the per-run figures, not an average.
```sh
# BEFORE (source stashed) and AFTER (source in place), identical commands:
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p beam_rt --example perf_call
erts_rust/target/release/examples/perf_call          # 3 x, /proc/loadavg printed by the harness
```
#### Host-load caveat (read before quoting any absolute number)
This is a shared, oversubscribed 14-CPU host and the two windows were **not** equally loaded:
BEFORE ran at 1-min loadavg **22.13 / 22.13 / 20.60**, AFTER at **25.72 / 25.72 / 23.82**, the
AFTER-FIXED pass at **2.51 / 2.31 / 2.31**. Absolute ns therefore do **not** compare across the
three blocks; the **marginal-vs-control** figure and the **before/after ratio at the same row**
(with both sides named) are the signal, and even those carry the load caveat.
### Registered-BIF per-call cost — `perf_call`
Path A = `RuntimeContext::new` (the `-eval` shape: at this HEAD the copy-out skipped by construction — see *Measurement provenance*).
Path B = `RuntimeContext::for_process` (the scheduler shape: copy-out + pooled rewind ON; today path A copies too, onto the `-eval` store instead of the young heap).
| row (path A) | BEFORE ns/call | AFTER ns/call | ratio (AFTER/BEFORE) |
|---|---|---|---|
| `erlang:abs/1` | 67.37 | 46.51 | 0.69 |
| `erlang:trunc/1` | 71.28 | 53.81 | 0.75 |
| `lists:reverse/1` | 89.13 | 65.42 | 0.73 |
| `erlang:'++'/2` | 74.78 | 53.94 | 0.72 |
| `lists:seq/2` | 99.99 | 72.29 | 0.72 |
| **control** (no BIF call) | 0.11 | 0.11 | 1.00 |
| `ets:insert/2` (A2) | 214.34 | 196.26 | 0.92 |
| `ets:lookup/2` (A2) | 165.96 | 149.70 | 0.90 |
| `erlang:abs/1` (B) | 70.79 | 49.06 | 0.69 |
| `lists:reverse/1` (B) | 144.04 | 126.76 | 0.88 |
| `lists:seq/2` (B) | 215.66 | 192.87 | 0.89 |
| `ets:insert/2` (B2) | 222.20 | 199.19 | 0.90 |
| `ets:lookup/2` (B2) | 234.90 | 203.19 | 0.86 |
The marginals (ns/call − control) move by the same ratios, since the control is ~0.1 ns/call on
both sides. Reading: the **immediate-returning** rows (items 1 + 3) fall **25–31 %**, the ETS rows
(the two names that shared a slot) fall **8–16 %**, and the container rows of path B fall **11–12 %**
— the copy-out + rewind they pay dominates them, so the prologue cut is a smaller share there.
### The path-C regression the byte hash introduced — found and fixed
The byte hash put `lists:reverse/1` **and** `erlang:length/1` in the same memo slot, so every path-C
(`Runtime::run_scheduled`, a compiled `lists:reverse/1` + `erlang:length/1` loop) call was a permanent
**miss** and paid the whole prologue (registry lookup + ten `needs_*` `binary_search`es + two `String`
allocations) per iteration — path C at **659.53 / 663.73 / 645.48** ns/iteration, **2.3× dearer** than
BEFORE's 285.23 / 284.96 / 286.29 and reproducible at *low* load too (662.31 at loadavg 3.97).
**Fix:** a fixed avalanche multiply after the FNV fold, taking the high bits
(`dispatch.rs::dispatch_slot`), chosen so the measured hot key set (`lists:reverse/1`, `erlang:length/1`,
`ets:insert/2`, `ets:lookup/2`, `erlang:abs/1`, `erlang:trunc/1`, `erlang:'++'/2`, `lists:seq/2`) maps to
**eight distinct slots**; a collision costs a recomputed prologue and never a different answer (the memo
tags every entry with its key). The memo is `DISPATCH_MEMO_SLOTS` = 8 slots, pinned by
`src/context/tests.rs` (the 16-key probe set exceeds the memo so slot churn is exercised).
#### AFTER-FIXED pass (3 runs, loadavg 2.51 / 2.31 / 2.31)
| row | run 1 | run 2 | run 3 |
|---|---|---|---|
| `erlang:abs/1` (A) | 64.34 | 68.46 | 65.02 |
| `erlang:trunc/1` (A) | 58.23 | 50.60 | 58.88 |
| `lists:reverse/1` (A) | 64.96 | 64.02 | 64.13 |
| `erlang:'++'/2` (A) | 51.40 | 52.09 | 52.20 |
| `lists:seq/2` (A) | 74.11 | 72.87 | 72.40 |
| control (A) | 0.15 | 0.17 | 0.16 |
| `ets:insert/2` (A2) | 177.88 | 185.40 | 174.90 |
| `ets:lookup/2` (A2) | 139.08 | 144.36 | 140.67 |
| `erlang:abs/1` (B) | 45.42 | 46.07 | 47.13 |
| `lists:reverse/1` (B) | 118.31 | 120.52 | 116.75 |
| `lists:seq/2` (B) | 189.14 | 192.90 | 184.80 |
| `ets:insert/2` (B2) | 176.57 | 195.08 | 175.77 |
| `ets:lookup/2` (B2) | 179.86 | 198.55 | 195.20 |
| **block C slope** | **223.86** | **223.41** | **213.90** |
Path C is back **below** the BEFORE band (≈ 0.78× of 285.23 at a *lower* load — do not read the
0.78 as a clean speed-up; the load differs). The in-process rows sit within noise of the buggy-AFTER
rows (their keys were never colliding in the offending slot), so the fix is specific to the
collision, not a general re-measurement.
### The `emqx_base62:encode/1` A/B — controlled, copy-out ON vs OFF
Provenance: this A/B's `OFF` arm is the copy-out (and with it the rewind) removed by a temporary, never-committed edit, i.e. a container result left on the pooled heap — the *Measurement provenance* section above records why neither label describes today's arms, of which both copy and only the destination differs.
Between m9 and m10 `tools/bench/RESULTS.md` recorded `emqx_base62_encode` at 10 283 → 16 601 ns/call
(+61.4 %) while its three sibling `foreign` cases improved 8–9 %; two hypotheses were on the table —
**(i)** the m10 copy-out seam (`move_result_into_process_heap` + its rewind guard) made
container-returning calls dearer, **(ii)** m9's figure was anomalously low.
**A/B (one binary, copy-out toggled by a temporary never-committed edit, reverted completely before the
final tree):** `ab_drv:enc/1` (the `encode/2` shape) reads **1 375 853 ns/iteration** with copy-out **ON
and OFF** — identical to three significant figures — and `ab_drv:dec/1` 37 341.60 ns/iteration.
**Verdict: neither hypothesis is supported** — the copy-out contributes **≈ 0 %**.
The blow-up is in `emqx_base62.erl` itself: `encode/2` appends
`<<Acc/binary, (iolist_to_binary(CharList))/binary>>` once per group of three input bytes, a
**superlinear** linear-append idiom under an interpreter, each `iolist_to_binary/1` a real registered BIF
call (copy-out ON, loadavg 2.32: `sum/1` 25 000 → 20 ms, 100 000 → 26 ms; `enc/1` 1930 ms → 101 522 ms).
This tree's own `foreign` `emqx_base62_encode` row reports **76 698 ns/call** (vs 16 601 in m10) —
consistent with a superlinear workload measured at different operating points.
**The A/B resolves** the copy-out's direction and order of magnitude (≈ 0 %) and rules out the m10 seam as
the mover, but **cannot** produce a per-call figure (the workload has no stable per-call cost at any N).
It also produced a negative result: with copy-out forced off, `perf_call`'s path-B loop **dies with exit 1**
(`perf_call: lists:reverse/1 raised system_limit`) — the pool exhaustion the copy-out + rewind prevents.
### Gate
`erts_rust/tools/differential/gate.sh` on this tree: `differential gate: total=2497 agree=2495
mismatch=0 unsupported=2 skipped=0 allowed=2 unauthorized=0 stale=0` → **PASS (exit 0)** at that HEAD;
the recorded capability gap is `wire/new_fun_ext_encode` (A.5). The A.9
`boot_extra/init_script_id_names_the_running_otp_release` case **agrees** on the current tree: with no
`-boot` the default route drives this runtime's own release's script
(`CodeServer::own_bootstrap_boot_script`), so `init:script_id()` answers `{"Erlang/OTP","30"}`,
matching `erlang:system_info(otp_release)` (`"30"`). A dispatch change is
behaviour-adjacent, so this is the authoritative check that no result moved.
### Caveats that invalidate a comparison (`DISPATCH-CUT`)
- The three windows have different host load (22.13 / 25.72 / 2.51 1-min); only per-row ratios and
  marginals are signal, and never across windows without naming both sides.
- Path C is a *mixed* row: each iteration is one `lists:reverse/1` `call_ext` **plus** one
  `erlang:length/1` `call_ext` plus the engine's surrounding instructions, so it prices the memo
  (hits vs misses) rather than any single BIF.
- The `MIX` constant in `dispatch_slot` is **chosen against the measured hot key set**, not derived;
  a workload whose hot key set differs can still collide (cost only, never a wrong answer). The
  8-slot table is the binding limit; enlarging it requires the `src/context/tests.rs` churn
  assertion to change with it.
- **Cold start:** `perf_call`'s path C spawns/loads per run and the example compiles the fixture
  with `erlc` once per invocation, so its *first* samples carry first-entry compile + load cost;
  the slope cancels that, the absolute ms do not.
## The BIF-result copy-out cut (`COPY-OUT-CUT`)
Provenance: the `BEFORE — path A` block below is the table-less `RuntimeContext::new` arm, which at HEAD `9d238e22bc` had no re-homing destination at all — `move_result_into_process_heap` opened with `mailbox_table.as_deref_mut()?`, so it neither copied nor rewound — and today's tree instead copies it onto the `-eval` store; the figures stand as that comparison's record (see *Measurement provenance* above).
HEAD `9d238e22bc8f143cec056e95ca9d19af0fa960b7` (BEFORE) → `4de5b69109e93a116c38793054b472fab35190ec` (AFTER).
Host: 8-core x86-64 dev container, loadavg 1-min **6.55 → 20.5** during the BEFORE window and
**≈ 1.15** during the AFTER A/B (per-block `/proc/loadavg` is quoted with each table below, exactly as
`perf_call` prints it). Release profile. Commands:

```
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_call -- 100000   # BEFORE (HEAD 9d238e22bc)
erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release -p beam_rt --example perf_call -- 100000   # AFTER  (HEAD 4de5b69109)
```
The harness measures best-of-5 per row at 100 000 calls/sample and prints `/proc/loadavg` before and
after every block; its own `control` row (the same loop with inlined arithmetic only) is the floor
that every marginal is taken against.

### What changed
`src/context/calls.rs::move_result_into_process_heap` used to pay, on every container-returning BIF
call in the scheduler shape (path B): a **failed** copier preflight walk, then
`beam_term::size_object(term)` (a *second* full walk, plus a `HashSet` and a `Vec`), then a fresh
`OffHeapTable` + `CopyContext`, then a second copy — up to three graph traversals where one copy needs
one. `copy_struct`'s refusal is a preflight: it refuses **before** writing any destination word, so the
first attempt is an entire wasted walk. The new body sources the growth hint from the refusal the
copier itself already produced (`AllocFailure::requested / size_of::<Eterm>()`, the `beam_message::error`
contract's exact flat word count — no second traversal), gives each attempt its own **fresh**
`OffHeapTable` destination (which is what keeps the copier's fast path eligible), and gates the retry on
`beam_message::is_message_heap_exhaustion` so a refusal
growing the destination cannot fix (above all the malformed-graph `badarg`) no longer grows a whole
space and fails identically. Nothing in the frozen precondition set was weakened: the rewind still
requires an empty source `OffHeapTable`, `!reentrant`, an `Ok` result, a scheduled process
(`mailbox_table.is_some()`) and a **successful** copy before `Heap::reset_to(mark)`; a term that cannot
be re-homed leaves both the result and the pool exactly as they were.

### BEFORE — path A (`RuntimeContext::new`, copy-out skipped by construction at this HEAD — see *Measurement provenance*), 100 000 calls
loadavg 1-min: 6.55 / 2.00 / 4.00 across the three runs.

| row | run 1 | run 2 | run 3 |
|---|---|---|---|
| `erlang:abs/1` | 64.86 | 69.11 | 64.05 |
| `erlang:trunc/1` | 67.08 | 80.13 | 72.47 |
| `lists:reverse/1` | 65.15 | 95.82 | 65.00 |
| `erlang:'++'/2` | 53.50 | 78.56 | 52.35 |
| `lists:seq/2` | 68.80 | 72.67 | 70.28 |
| `ets:insert/2` (A2) | 175.53 | 177.09 | 176.79 |
| `ets:lookup/2` (A2) | 144.47 | 139.29 | 142.76 |
| **control** | 0.16 | 0.16 | 0.15 |

### BEFORE — path B (`RuntimeContext::for_process`, copy-out + pooled rewind ON)
Same three blocks/loads as above.

| row | run 1 | run 2 | run 3 |
|---|---|---|---|
| `erlang:abs/1` | 46.98 | 47.00 | 46.02 |
| `lists:reverse/1` | 115.57 | 115.76 | 114.98 |
| `lists:seq/2` | 187.28 | 188.11 | 184.06 |
| `ets:insert/2` (B2) | 182.29 | 181.71 | 177.38 |
| `ets:lookup/2` (B2) | 183.03 | 176.90 | 177.47 |
| **block C slope** | 224.28 | 221.69 | — |

Copy-out delta B−A (marginal, the harness's own `copy-out delta` table): `lists:reverse/1` **+50.42 /
+19.94 ns**, `lists:seq/2` **+118.48 / +115.45 ns**, `ets:lookup/2` **+38.56 / +37.61 ns** — taken
against a 0.16 ns control.

### AFTER — interleaved binary A/B (`BASE` = HEAD `9d238e22bc`, `OPT1` = the new body)
Same script, same host, same 30-minute window, loadavg **≈ 1.15** throughout (1-min);
`BASE`/`OPT1`/`OPT1`/`BASE` per round, two samples each, path B only (the block the change touches).
This interleaved binary A/B — not a sequential BEFOR/AFTER pair — is the AFTER evidence: the shared
host's load varied by more than the effect across the session, and interleaving is what makes the
ratio citable.

| row (path B) | BASE min / mean | OPT1 min / mean | ratio (OPT1 / BASE, mean) | ratio (OPT1 / BASE, min) |
|---|---|---|---|---|
| `lists:reverse/1` | 115.20 / 117.95 | 110.14 / 112.02 | **0.950** | 0.956 |
| `lists:seq/2` | 185.84 / 190.09 | 164.74 / 166.65 | **0.877** | 0.886 |
| `ets:lookup/2` (B2) | 177.94 / 185.61 | 175.65 / 180.94 | **0.975** (noise-dominated) | 0.987 |
| control | 0.15 | 0.15 | 1.0 | 1.0 |

Every `OPT1` sample was ≤ its paired `BASE` sample within every round. **Marginals vs the control:**
`lists:reverse/1` **≈ −5.9 ns/call** (117.95 → 112.02), `lists:seq/2` **≈ −23.4 ns/call**
(190.09 → 166.65), `ets:lookup/2` **≈ −4.7 ns/call** (inside the row's own spread — not claimed as a
win). The control is unchanged at 0.15 ns, so the deltas are the copy-out's, not the harness loop's.

### Verdict
**The cut wins where the copy-out dominates, and it does not regress anything measured.**
`lists:seq/2` — the row with the largest graph per call — improves **≈ 12 %** (`lists:seq/2` OPT1/BASE
mm 0.877), `lists:reverse/1` **≈ 5 %** (0.950), `ets:lookup/2` is unchanged within noise (0.975/0.987).
`erlang:abs/1` is an immediate result and never enters the copy-out at all, so it is expected flat
before *and* after. **Unchanged rows are reported as unchanged:** the ETS path B2 row is not claimed.
Absolute latency is not signal on this host — the BEFORE window ran at load 6.55–20.5 and the AFTER
window at ≈ 1.15, so no BEFORE ns/call is compared to an AFTER ns/call anywhere above; only the
interleaved ratios and the marginals-vs-control are.

### Negative results (measured, then discarded)
- **"Size first, grow if needed, one copy"** (temporary, never-committed edit): `size_object(term)`,
  `if available < needed { grow_nursery(needed.max(256)) }`, then one `copy_struct`. Measured **≈ 290
  ns/call for `lists:reverse/1`** against a BASE of ≈ 121 ns — **≈ 2.5× worse** — *and* it grew the
  nursery on **every** call (the target `max(capacity, next_words(need, 0))` walked 3→6→12→…→256
  words), i.e. it strictly worsens the item-4 peak-RSS effect below. Discarded on both counts.
- **Why a size-first policy loses in general:** `beam_term::size_object` and the copier's own
  `preflight_words` cost the same order, but a size-first policy pays that walk on **every** call
  whereas the retry policy pays the copier's own bounded walk only on the (in the scheduler shape,
  every-call) refusal path — and the refusal already carries the count back.
- **Forcing the copy-out off entirely** (temporary edit): `perf_call` path B dies with
  `beam_rt: lists:reverse/1 raised system_limit`. The copy-out + `reset_to(mark)` rewind is what
  bounds the 16 MiB pooled BIF heap; it is load-bearing and was never removed.
- **Instrumentation count:** a temporary `eprintln!` in the failure arm showed **32 001 failure
  events for a 20 000-iteration run** — the retry really does fire on essentially every container
  copy-out in the scheduler shape, which is what makes the removed `size_object` walk a per-call cost.

### Reachability of the second attempt
The retry is now reached **only** for `beam_message::is_message_heap_exhaustion` — i.e. the copier's
typed `AllocKind::Message` refusal from its own preflight. Every other refusal a legal BIF result can
produce (`BadArg` for a malformed graph, an `AllocKind::Heap`/`LiveObject` failure from a nested push,
a `Raised` reason) is permanent for this destination and now returns `None` immediately. Pinned by
`src/context/calls.rs::tests::a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing` (a
`make_header(0, 0)` term: the nursery's `young_space_count()` and the destination's `used()` are both
unchanged and the call answers `None`) and its positive control
`an_exhaustion_refusal_grows_the_nursery_and_the_retry_copies` (an oversized `lists:seq(1, 2000)`
result still grows the nursery and is copied on the retry).

### The copy-out destination is always fast-path-eligible
`beam_message`'s copier takes its allocation-free single-traversal path (`try_copy_fast`) exactly when
`copy.rs::is_fast_path_eligible` holds, i.e. when the destination's `OffHeapTable` is fully empty in all
three of its lists — the destination **heap**'s `used` is deliberately not part of that gate.
`src/context/calls.rs::copy_result_fresh` builds a **fresh** `beam_message::OffHeapTable::new()` for each
copy attempt, so the gate is true every time and every copy-out is served by the one traversal, with no
`preflight_words` walk before the first word is written.
The `grow_nursery` retry gets its **own** fresh table, so a partial attempt's registrations can never
reach it.
On success the local table's off-heap references are adopted into `Runtime::eval_off_heap`; on a refusal
nothing is adopted and the dropped table releases the node references the abandoned attempt took.

### Gate
The same corpus run covers this cut: `erts_rust/tools/differential/gate.sh` on the AFTER tree →
`differential gate: total=2497 agree=2495 mismatch=0 unsupported=2 skipped=0 allowed=2
unauthorized=0 stale=0` → **PASS (exit 0)**, against a real oracle (`erl` present on `$PATH`), with the
identical recorded capability gap `wire/new_fun_ext_encode` (A.5), and the A.9
`boot_extra/init_script_id_names_the_running_otp_release` case **agrees** here exactly as the
`DISPATCH-CUT` gate above explains.
A copy-out change moves terms between heaps, so this is the authoritative check that no result moved.
### Caveats that invalidate a comparison (`COPY-OUT-CUT`)
- The BEFORE and AFTER windows have very different host load (6.55–20.5 vs ≈ 1.15 1-min). **No BEFORE
  ns/call is compared to an AFTER ns/call anywhere in this section**; the AFTER evidence is the
  interleaved `BASE`-vs-`OPT1` binary A/B, which shares one window and one load.
- `erlang:abs/1` is under 50 ns and the `control` under 0.2 ns, so both are at the resolution floor;
  neither is used to claim a win.
- **A fresh 3-run AFTER pass on the final committed HEAD was not run** — the turn budget went to the
  code, the units and the gate. The A/B above is on the same body that the commit `4de5b69109` pins;
  re-run the two commands at the top of this section to reproduce a sequential pair.
- The change does **not** address the item-4 growth in retained nursery space (see below): it removes a
  wasted graph walk and a needless retry, not the `grow_nursery` itself.

### `ets_copy` peak RSS (measured 94.68 MiB at N = 100 000, still linear in N) — a structural limit, not the ETS adopt path
**`ets_copy` peak RSS (measured 94.68 MiB at N = 100 000, still linear in N) — a structural limit, not the ETS adopt path.** The recorded `RESULTS.md` §8 item-5 lead (the ETS adopt path / the rewind guard) is **refuted**: `safe_to_rewind` is already true for `ets:*` and the pooled heap is already rewound. The growth is per-iteration, ≈790 B ≈ 99 words, and it was bisected: a float-free ETS loop of the same shape grows identically, while a float-carrying churn loop with no ETS is flat — so it tracks **BIF calls**, not floats and not ETS table state. Mechanism: almost all of the loop's heap traffic is the BIF-result copy-out, which by design cannot collect (the machine's live x registers / Erlang stack are not published at a BIF call, so a collection there would free live terms); a full nursery is met by `Process::grow_nursery(hint)` (`src/context/calls.rs::move_result_into_process_heap`), and a **retired young space is retained**, so the retained capacity accumulates linearly. On the **default** (native baseline JIT) tier the loop never reaches a real collection either: `ensure_heap_full` returns at `!process.should_collect(heap_need)` (`src/context/gc.rs` → `beam_gc`'s `nursery.current().available_words() < need_words`) because the loop's own `test_heap` prefixes are tiny (a 233-word default nursery, `beam_gc/src/policy.rs`), `alloc_words_rooted` is never entered (the BIF, not the engine, builds the result), and the reduction/yield-boundary door (`src/context/gc/reds_door.rs`) needs a slice to end — which a natively linked block never lets happen. Removing the caller-module float guard at the two FR-publishing GC doors was tried and **reverted**: it changes nothing here (94.68 → 95.02 MiB) and contradicts the pinned float-collection contract. Closing this needs either a **`beam_process`** seam that can shrink/reuse a retired young space (`Process::grow_nursery` retains it) or a **`beam_engine`/runner** seam that publishes the driving machine's roots at the BIF call so the copy-out can collect — both sibling-crate changes, not reachable from `src/context/calls.rs`.

## See Also

- [`PERF.md`](PERF.md) → the current cross-runtime figures and the execution-tier effect.
- [`PERF_HISTORY.md`](PERF_HISTORY.md) → the dated `BEFORE`/`AFTER` cross-runtime snapshots and their `nb*` registered-BIF columns.
- [`TRACE_AND_PERF.md`](TRACE_AND_PERF.md) → the `perf_call`/`perf_bif`/`perf_copy_out` harnesses and the dispatch-cost narrative.
- [`CONTEXT.md`](CONTEXT.md) → the runtime crate's contracts; `src/context/IMPLEMENTATION.md` documents the dispatch memo this file prices.
