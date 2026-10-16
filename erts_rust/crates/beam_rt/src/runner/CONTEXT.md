# `beam_rt::runner` — the process runner behind the scheduler (tier selection · slice execution · result recording)

## Intent
The concrete [`ProcessRunner`](beam_process::ProcessRunner) that `beam_process`'s scheduler drives: one
reduction slice of one scheduled process. It builds a [`RuntimeContext`](crate::context::RuntimeContext)
that runs **as that pid** in the caller-supplied process table, takes the pid's engine state out of
[`RunnerState`], executes it (`run_export_outcome_with_tier` for a first run,
`resume_outcome_with_tier` — on the process's **remembered** tier — for a process parked at a blocking
`receive` or pre-empted on the reduction budget) and records the observable result
(the return value on the pointer-stable arena, or the raise's reason term).

This node additionally owns **the execution-tier decision**: a process's *first* entry runs on the
native baseline tier ([`beam_engine::Tier::BaselineJit`]) unless the mode pins the interpreter, and the
decision is **remembered on the engine** (`ProcessEngine::tier`) and re-used by every later re-entry,
so a process keeps its tier for its whole life. The **default is `Auto`**, which selects the baseline
tier for **every** process entry: the engine's driver handles the module-context switch itself, so no
per-module safety predicate exists (see `## The tier is sound for any module`).
The remembered tier is only *observable* at a **slice boundary** (a slice `Yield` on the reduction
budget, or a `receive` park): a process that instead runs to completion inside its first slice never
re-enters. On the **interpreter** tier the budget is charged once per interpreted instruction, so any
hot loop is pre-empted at the budget; on the **native baseline** tier a compiled block runs natively
past the per-instruction hook and charges only at a deopt or a heap-admission point, so a
**binary-accumulating or `call_ext`-only hot loop charges nothing, never pre-empts, and could never be
switched**. That is exactly why the tier is selected eagerly at the first entry rather than after a
warm-up: the bounded-warm-up alternative was measured and declined because it would pin exactly those
zero-charge native hot loops to the reference tier for their whole life (the characterisation and
figures are in `PERF.md`).

## API Surface (crate-internal, `pub(crate)`)
* [`entry`] — `ProcessEntry` (the runtime-scheduled entry: an export or a fun), `ProcessEngine`
  (per-pid machine/entry/parked state + the **remembered `tier`**) and `RunnerState`
  (per-pid engine state, recorded `result`/`exit`, `jit_mode` override).
* [`run`] — `RuntimeRunner`: the `ProcessRunner` implementation (one reduction slice) — the tier
  selection (once, at the first entry) and the tier-aware first-entry/resume execution, the
  parked-machine root publication (`publish_parked_roots`/`resume_from_process_roots`), the
  parked-receive deadline rearming and the recorded outcome.
  The parked `receive … after T` deadline is recovered with the engine's **cross-module-aware**
  `Machine::wait_deadline` (passed the process's entry module; it reads the callee module the machine
  is really parked in, so a `call_ext*` callee's `after T` is not lost), `delay_ms == Some(d)` gates
  the arming — a bare `wait`/unreadable delay arms nothing — and `WaitDeadline::resume` is the
  `timeout` continuation the fired timer re-enters.
  Both recorded terms are transported with the `SourcePayloads` built by
  [`Runtime::source_payloads`](crate::runtime::Runtime::source_payloads) — the executing **machine's**
  handles (`engine.machine.refc_binaries()`) **unioned** with the whole off-heap family the pid's scan
  retained from the messages it received (`MailboxState::retained_binaries`/`retained_off_heap`/
  `retained_nodes`), since a payload the process *received from another process* carries its handles on
  the message envelope, not on this machine. They are stabilized *before*
  the engine goes back into — or is dropped out of — `RunnerState`: the return value through
  `Runtime::stabilize_term_with`, the exit reason through `Runtime::try_stabilize_escaped_with`. An
  off-heap binary (`bs_init_writable`/`bs_create_bin` above the on-heap limit) is therefore copied
  rather than refused with `badarg` (the return path) or kept un-copied against a dangling payload
  (the exit path) — whether the process **built** it or **received** it.
  The same `engine.machine.refc_binaries()` reservoir is also copied onto the **running process's**
  own off-heap table once per slice, on every slice outcome (`Process::register_off_heap_binary`, no
  matter whether the slice pre-empted, parked, returned or raised), so a process that has built a
  payload above the on-heap limit comes to *own* it for `erlang:process_info(Pid, binary)`; the
  population is dedup-by-`data_ptr`/idempotent, so its over-report (a payload an earlier reachability
  sweep pruned) mirrors the oracle, while `Runtime::eval_off_heap` is deliberately left out as the
  pid-less `-eval` store.
  Because that reservoir is **append-only** and never cleared, the seam advances a per-engine cursor —
  `ProcessEngine::registered_payloads`, the count of reservoir slots already registered — and each slice
  copies only its **not-yet-registered suffix** (`min`-clamped against the reservoir length), so a
  payload becomes owned at the end of the first slice that built it and the total registration work over
  a process's life is linear in the payloads it built, rather than re-walking the whole reservoir (and
  paying an O(slots) slot scan) on every slice.
  The cursor rides the `ProcessEngine`, which is removed from and re-inserted into `RunnerState::engines`
  at every slice boundary, so it survives a pre-emption and a `receive` park.
* [`budget`] — the slice's reduction budget and the wrapper context that charges it: `BudgetContext`
  declares its own `wants_reduce() == true` (the engine lets a host answer `false` only when its
  `reduce` is provably the no-op default) and implements `reduce` to charge the probe once per
  reduction unit the interpreter prices, so a **non-allocating** interpreted loop is pre-empted; the
  two `ensure_heap` doors stay charged as the only charge a native compiled block reaches. As the
  scheduled path's outermost `ExecutionContext` it forwards every engine seam it does not itself
  implement — `send_message_with_off_heap` in particular, whose trait default would otherwise drop
  the sender machine's off-heap handles and make an engine-built off-heap send a `badarg`. Three of
  those forwards are **load-bearing** rather than merely faithful: `wants_bif_in_machine`/
  `call_bif_in_machine` carry the engine's in-machine execution-position door
  ([`../context/CONTEXT.md`](../context/CONTEXT.md), `src/context/position.rs`) onto the scheduled
  path. The slice's machine runs on *this* wrapper, so the engine's once-per-run latch reads the
  probe **here**: without the `wants_bif_in_machine` forward it would see the trait default (`false`),
  never take the door, and a **scheduled** process could not answer its own
  `erlang:process_info(self(), current_stacktrace)` (`gen_server:handle_common_reply/5`'s
  `?STACKTRACE()`); `call_bif_in_machine` is forwarded for the same reason, since it is the method
  the door dispatches through. `resolve_local_call` is load-bearing for the same reason: the engine's
  `Call`/`CallOnly`/`CallLast` arms — and the native baseline tier's `jit_local_call` probe — consult
  *this* wrapper, so without the forward they would see the trait default (`Ok(false)`), always
  decline, and jump a same-module local `call` straight into a non-exported callee's own body instead
  of letting the host that serves it dispatch it through `call_bif`
  (`RuntimeContext::resolve_local_call` → `crate::native::prim_tty::serves_local_call` for a built-in
  NIF library's stub). Every other method of the seam is forwarded unchanged, so the redirect into
  `inner` is purely mechanical.
* [`tier`] — `JitMode` (`Auto` (**the `#[default]`**) / `Interpreter` / `Baseline`),
  `mode_from_value(Option<&str>)` (**the pure** `BEAM_RT_TIER` value semantics),
  `env_default()` (the memoized unset-variable default) and `select_entry_tier(...)` — the single
  tier-decision seam.
* [`reason`] — the exit-reason translation only (`normal` + `exit_reason`); the finite-`receive`
  deadline recovery now lives in the engine (`Machine::wait_deadline`/`Machine::timeout_delay`), so no
  duplicated delay decoding remains here.
* [`perf`] — the `+JPperf` code-map feed (contract A.8).
* [`run`]'s `#[cfg(test)]` slice/park accounting — `RUN_SLICE_COUNTERS` (a per-thread `Cell<RunSliceCounters>`, mirroring the file's existing `REGISTERED_PAYLOAD_TOTAL`), read through `run_slice_counters()` and zeroed by `reset_run_slice_counters()`: `note_slice_entered()` counts one `RuntimeRunner::run` call (one reduction slice) and `note_slice_parked(preempted)` counts one slice that handed the machine back (the `Yield` pre-emption arm, or the `receive` `Wait` arm). The increments are `#[cfg(test)]`-only, so an untested build is byte-identical to before.

## The `BEAM_RT_TIER` modes (`mode_from_value` is the single definition)
| value | mode | meaning |
|---|---|---|
| unset | `Auto` | **the default**: the native baseline tier for **every** process entry |
| `auto` | `Auto` | the default, named explicitly |
| `baseline` | `Baseline` | the same selection, forced explicitly (a measurement/debugging alias of the default) |
| `interpreter`, anything else | `Interpreter` | the always-correct reference tier, and the **kill switch**; an unknown value resolves to it, never an error |

`JitMode`'s derived `Default` is `Auto`, so omitting the mode anywhere selects the native tier;
`env_default()` memoizes the environment resolution in a `OnceLock` (one `getenv` per process).

## How the tier is selected
In [`run`]'s slice, **before** the `RuntimeContext` borrow (the context takes `Runtime` mutably):

```text
tier = match engine.parked { Some(_) => engine.tier,            // a resume re-uses the remembered tier
                             None    => { let t = select_entry_tier(runtime, state, &engine.entry, state.jit_mode());
                                          engine.tier = t; t } }
```

The tier is chosen **once**, at a process's first entry, and written to the engine
(`ProcessEngine::tier`); every later slice — a pre-emption on the reduction budget, or a `receive`
park — takes the `Some(_)` arm and resumes on that same tier. It is passed to **both** first-entry
sites — the export entry (after `enter_run`, the export's own label) and the resolved local-fun entry —
and to the resume through `Machine::resume_outcome_with_tier`, whose `Tier::Interpreter` arm delegates
to the plain `Machine::resume_outcome` (so an interpreter-tier process is byte-for-byte unchanged).
[`select_entry_tier`] is the runner's single tier-decision seam, and the **mode alone** decides it: the
baseline tier is sound for any module (see `## The tier is sound for any module`), so neither the
entry's module nor its code is inspected. The mode resolves from `state.jit_mode` if set (tests), else
the process-wide `BEAM_RT_TIER` default ([`env_default`]).

## The tier is sound for any module
[`select_entry_tier`] picks `BaselineJit` from the mode alone: there is **no module the baseline tier
cannot run**, so there is no per-module predicate to consult. A `Step::Enter` — a step that continues
in *another* module's code — was historically the one shape a native tier could not take (a compiled
block's code, label map and literal area are all bound to a single module), and the engine's driver now
handles it: a `Step::Enter` (emitted by a compiled `call_ext`/`return`, or by the interpreter's own
step) switches the machine context (`Machine::switch_context`), rebinds the module / label map /
literal area and continues in the **callee's** compiled blocks — including the first sight of a
not-yet-compiled module. The transition is a driver round-trip, i.e. a performance property rather than
a correctness one; the engine-side detail is `crates/beam_engine/src/jit/CONTEXT.md`.

The one remaining host-side obligation is **enforced outright**, not through a predicate:

* **The host's cross-module seam must be idempotent.** The native `call_ext*` arm resolves the import
  through `ExecutionContext::resolve_external_call` natively, and when the host declines (`Ok(None)`)
  the *whole instruction* is re-run by the interpreter step — which consults the same seam a **second**
  time. A seam whose answer changed between the two consultations (a module cached by the first one)
  would route the callee two different ways and could enter it in-machine under the caller's module
  identity. [`RuntimeContext::resolve_external_call`](crate::context) is written to be idempotent for
  exactly this reason, pinned by the regression tests in `src/context/tests.rs` and `tests/nif.rs`.

## Parked-machine root publication (the runner↔GC hand-off)
A suspended process's machine lives in [`RunnerState::engines`], but its registers are only a *copy* of
the process's own root set. A collection driven while the process is parked — `Process::deliver`'s exit
cascade (`beam_process`) is **not** gated by `Runtime::gc_process_enabled` — relocates the process's heap
against its own `regs`, so a parked machine that had not published its roots would be collected against a
partial root set and left with a stale word in its `x`.
[`run`] therefore calls `publish_parked_roots(table, pid, machine, fr_count)` at **both** park arms —
the pre-emption/`Yield` arm and the `Ok(EngineRunOutcome::Wait)` arm — right before `engines.insert`, and
`resume_from_process_roots(ctx, pid, machine, fr_count)` after `ctx.enter_module(module)` when
`engine.parked.is_some()`.
Both delegate to the **one** shared implementation in [`context::gc`](../context/CONTEXT.md)
(`publish_roots_full`/`scatter_roots_full`, the `fr_count` parameter bounding how many leading FR words
are published), which the in-slice heap-admission points (`ensure_heap_full`/`alloc_words_rooted`) also
use — those always pass `roots.fr.len()`, the **whole** file — so the park/resume and the admission
points can never drift:
* `publish_parked_roots` hands the machine's roots over with `RootSet::ALL_LIVE` — the conservative
  (always-safe) choice, since a parked machine's live count is not exposed and over-approximating a
  copying collector's root set can only keep a term alive, never free a live one.
* **The FR file is published only up to its live leading prefix `fr_mark()`.** The runner reads the
  count **once** per slice (`Machine::fr_mark()`, the engine's exclusive high-water mark of FR writes
  since the machine's last reset) and reuses it for both the post-slice reaction and the
  `publish_parked_roots(.., fr_count)` call in the same slice; `fr_mark() > 0` also marks the process
  **relocation-exempt for the inter-slice window** (`ProcessTable::set_relocation_exempt(pid, true)`,
  cleared by the next slice entry), so a live FR is always published and the window's collection sites
  decline the move (a `Yield`-parked process's window is exempt too, and a delivery-driven collection
  is not gated by `gc_process_enabled`, so a live FR must still relocate). The reaction is **per slice,
  never a permanent pin**: inside a slice the engine reaches only the full door, which publishes the
  whole FR file, so a float-using call site collects exactly like a float-free one. `fr_mark` is an
  exact upper bound after a slice — every FR write goes through `Machine::set_fr`, and the baseline JIT
  tier refuses an FR destination (deopting to the interpreter) — and a bound can only err by
  over-protecting: a slot at or above it is `NIL`, and a `NIL` word roots nothing, so a float-free park
  publishes **no** FR word instead of the 1024 slots `process.regs` used to carry. The count is stored
  on `ProcessEngine::fr_published` at the park and replayed at the resume, so `scatter_roots_full` never
  sees a length mismatch (which would halt the process's collection).
* `publish_parked_roots` **also** publishes the parked machine's live pending-exception reason
  (`Machine::pending_exception`) — the one term a parked machine holds that the closed `RootSet`
  shape cannot carry, because it is a machine *field*, not an `x`/stack/FR slot — as the process's
  `Registers::extra_roots` word (the host escape hatch
  `beam_process`'s `gather_roots`/`scatter_roots` relocate in place). With **no** live pending
  exception the extra-root set is left **empty** (cleared, never a NIL/placeholder), so a stale root
  can never keep an otherwise-dead term alive.
* `resume_from_process_roots(ctx, pid, machine, fr_count)` is the context's
  `RuntimeContext::resync_parked_roots`, called with the **same** `fr_count` the matching park used
  (`engine.fr_published`): it copies the process's authoritative root set back into the machine (the
  leading FR words only up to `fr_count`), reads the (already-relocated) extra-root
  reason out and writes it into `Machine::pending_exception_mut()`, **clears** the process's extra
  roots (the machine owns the relocated term now), and, if the slices no longer match, **halts the
  process's collection** (`Runtime::halt_process_gc`) rather than run against inconsistent roots.
* The **`Wait` park arm** additionally marks the process **relocation-exempt** for the whole parked window (`ProcessTable::set_relocation_exempt(pid, true)`), and the resume clears it, so a delivery- or signal-driven collection of a waiting (or just-woken, not-yet-resumed) process is declined and the delivery takes the ordinary heap-fragment fallback. The **`Yield`** (pre-emption) arm is *not* exempt by the park itself, but the post-slice `fr_mark` check opens the same window exemption for **any** slice that ends with a live FR prefix (see the bullet above), and the admission points inside a resumed slice are unaffected because the next slice entry clears the mark. This is **additional** to the installed host collection gate (`../context/gc_walk.rs`'s `RuntimeCollectionGate`, installed on the runtime's table by `Runtime::new`): the gate declines only when an unscanned container is reachable from the receiver's **gathered** roots, while the exemption refuses *any* relocation for the window — covering a live term the host holds outside that gathered set, which the walk cannot see. Both park arms call `Runtime::serve_gc_request` (the `erlang:garbage_collect/0,1` request, see the crate node): the `Yield` arm serves it when the slice ended with a dead FR prefix (`fr_mark() == 0`, so no exemption was opened) and it is declined by the exemption when the prefix was live, while the `Wait` arm's request is declined by the parked-window exemption.
The runner's raise arm likewise copies the recorded exit reason through
`Runtime::try_stabilize_escaped_with` — with the copy's `SourcePayloads` built by
`Runtime::source_payloads` (the **same machine's** `refc_binaries()` unioned with the whole off-heap
family the pid retained from received messages — refc binaries, MSO handles and node identities),
so a reason that *is* an off-heap binary (`exit(<<big/binary>>)`) is resolved and shared
instead of tripping the valve, whether the process built it or received it; a reason that genuinely
cannot be copied is still the honest `error:system_limit`, never a stored dangling word.

## Measured
The tier is taken on a process's **first** entry and then carried for its whole life: a pre-empted
slice and a `receive` park both resume through `Machine::resume_outcome_with_tier` on the tier the
process was selected for, so a *pre-empting* hot loop keeps its tier across every slice
(`a_preempted_process_resumes_on_its_baseline_tier`/`a_receive_parked_process_resumes_on_its_baseline_tier`
pin this structurally, without a timing). On the **interpreter** tier every interpreted instruction
charges the budget, so any hot loop pre-empts (pinned by
`a_non_allocating_interpreted_loop_is_pre_empted`). On the native tier a compiled block runs natively
past the per-instruction hook, so a loop that reaches no heap-admission point either — a
binary-accumulating or `call_ext`-only loop, or a natively linked tail loop with no allocation — charges
nothing and runs to completion inside one slice on the tier its first entry selected (pinned by
`a_non_allocating_loop_on_the_native_tier_completes_within_one_slice`).
Per-workload speed and memory figures are **measured, never
modelled**, and live in [`../../PERF.md`](../../PERF.md) and the bench driver's
[`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md); they are re-measured there
rather than transcribed here.

`BEAM_RT_TIER=interpreter` is the always-correct reference tier and the **only** kill switch: the
default (`Auto`, and its `baseline` alias) puts every process on the native tier.

**The per-call work of a growth-shaped loop is FLAT in N — deterministic counters, never wall clock.**
`the_churn_workloads_per_call_counters_are_flat_in_n` and
`the_heap_growth_workloads_per_call_counters_are_flat_in_n` each drive their fixture at three
increasing iteration counts and print a `counter_table`: N vs slices / yields / waits / collections /
major / fallbacks / peak young-nursery geometry / debt boundaries, plus the derived per-call ratios.
Measured (release): `churn` collections/call 6.996002 / 6.999200 / 6.999800 and slices/call
0.191904 / 0.191981 / 0.191995 at N=2 000 / 10 000 / 40 000 — flat to within 0.06 %, with
`collections == 7N − 1` exactly at every N; `heap_growth` collections/call 0.999900 / 0.999960 /
0.999980 and slices/call 0.000300 / 0.000280 / 0.000260 at N=10 000 / 25 000 / 50 000 — one
collection per iteration throughout, since the 401-word request cannot fit a fresh nursery. In both
shapes `peak_young_spaces == 2` and the young capacity plateaus (376 / 610 words) at every N, so the
retired-nursery chain the `collection_debt` trigger protects is bounded rather than N-dependent, and
`yields == slices − 1` with `waits == 0` throughout (neither loop ever parks).

**The recorded `foreign` / `emqx_base62:encode` regression is measurable as CLOSED, and the module-literal
tagging is why.** Interleaved matched-N A/B through the bench harness (`--quick --runtime rust
--workloads foreign`, `$BEAM_RT` per variant, N = 25 000, 3 rounds per side, host loadavg 23–61):
`case_emqx_base62_encode_ns_per_call` HEAD `75ee31bef3` 17 730 / 17 277 / 22 217 ns vs the m10 baseline
`b4593818b3` 16 381 / 22 797 / 18 697 ns — **ratio of medians 0.95×**, mean of the three round ratios
1.01×; two independent quiet-host replications (loadavg 2.5–4 and 0.96–1.40, 3 rounds each) give
**1.06×** and **1.07×**, i.e. parity within the run-to-run spread. The m13 tree `a141d6878c`, which
reproduces the recorded regression on this host at **3.91×–3.97×** m10, shows the comparison is
discriminating.
Startup-cancelled per-call marginals (`N = 0 / 5 000 / 25 000 / 60 000`): HEAD 9.4 / 16.6 / 24.4 µs, m10
13.2 / 16.8 / 27.5 µs → **head/m10 0.71 / 0.99 / 0.89, flat in N** with no superlinear residual, while
m13 climbs 36.3 / 68.1 / 126.9 µs (5.2× HEAD at N = 60 000).
Deterministic counters (`erlang:statistics(garbage_collection)` + `process_info(self(), heap_size)` over
the same driver): HEAD collects `{667,155964,0}` / `{16667,3899977,0}` / `{40000,9359864,0}` at
N = 1 000 / 25 000 / 60 000 with `heap_size` flat at 466 words, whereas m10 **and** m13 run **zero**
collections with a linearly growing nursery (m10 147 249 / 3 756 849 / 9 020 849 ≈ 150.4 words/call, m13
the same shape).
The mechanism is the one the previous pass recorded, and it is off the path now: `Nursery::grow` retires
the old space into `retired` and only `Nursery::collapse` — reached at a collection — clears it, so a
runtime that never collects walks an ever-longer `spaces()` = `retired.chain(current)` through
`root_is_unscanned` → `Nursery::contains` on every admission; HEAD tags the driver's constant
literal-area binary (ancestor `98d65f2fd3`, `crates/beam_loader/src/literal.rs`), the root is scanned, the
valve no longer fires, a collection runs about every 1.5 calls and the chain stays collapsed.

## Constraints
* **`#![deny(unsafe_code)]`**; no `unwrap()` on runtime data; typed `beam_common::Result` errors.
* The tier is resolved **before** the `RuntimeContext` borrow (the selection happens in the slice's
  prologue, before the context takes `Runtime` mutably).
* A test forces a tier by setting `state.jit_mode = Some(..)` — never `std::env::set_var` (edition 2024
  makes it `unsafe`). `BEAM_RT_TIER` is the CLI-facing equivalent, and its semantics are pinned through
  the pure `mode_from_value` (not through the environment).
* `JitMode::Baseline` is an alias of the default selection (`Tier::BaselineJit`); only
  `JitMode::Interpreter` turns the native tier off.

## Test Strategy
* `#[cfg(test)]` units in [`tests`]: `select_entry_tier` over the real tier fixtures — the default
  (`Auto`) and the explicit `Baseline` alias both select `BaselineJit` for **every** entry, including a
  cross-module-calling module (`stdlib_more_harness`) and a dynamic-`apply` one (`funs`), and an
  unresolved **fun** entry takes the same tier as an export entry; only `Interpreter` selects
  `Tier::Interpreter`. `the_unset_default_resolves_to_auto` pins the `BEAM_RT_TIER` value semantics and
  `JitMode::default()` through the pure `mode_from_value`.
* Tier persistence across a resume, proven through the per-thread **baseline compiled-block cache**
  (`beam_engine::jit::cache::{baseline_cache_len, clear_baseline_cache}`) — empty the cache between the
  park and the resume, so a resume that came back on the baseline tier is the only thing that can
  repopulate it: `a_preempted_process_resumes_on_its_baseline_tier` (`preempt:alloc_loop/1` under a tiny
  budget → `Yield`, then the resume completes the loop on the remembered tier),
  `a_receive_parked_process_resumes_on_its_baseline_tier` (`recv_after:wait_forever/0` → `Wait`, then a
  `ProcessTable::deliver_simple` message wakes it and the resume takes it on the remembered tier), and
  `an_interpreter_tier_process_is_unchanged_across_a_resume` (same loop on `JitMode::Interpreter` records
  the same result and compiles nothing on either slice). Each asserts `ProcessEngine::tier` as well.
* Pre-emption of a **non-allocating** loop on both tiers, over the committed `preempt` fixture's
  `spin/1` (whose `spin/1 -> spin/2` disassembly contains no heap-admission instruction at all):
  `a_non_allocating_interpreted_loop_is_pre_empted` (interpreter tier — a whole-budget slice yields and
  records nothing, a second short process registered behind it runs to its `return` while the loop is
  unfinished, and the loop's final value is its iteration count, which changes if a resume skips or
  double-applies an instruction) and
  `a_non_allocating_loop_on_the_native_tier_completes_within_one_slice` (the same call with the same
  iteration count completes inside one budget-bounded slice on the native tier — the documented residual
  limit, pinned rather than papered over).
* End-to-end: `slice:fib/1 10` through the scheduled path (`RuntimeRunner`) records `55` under both an
  explicit `JitMode::Auto` (→ baseline) and `JitMode::Interpreter`, and the scheduler path records it
  under whatever mode the environment selects.
* The two transport sites' off-heap resolution is covered end to end by
  `tests/engine_off_heap_transport.rs` — the `Returned`/exit-reason arms for a binary the process
  **built** *and* one it **received from another process** (`received_off_heap_binaries_survive_the_transport_copy`),
  each byte-compared with the live oracle.
* Crate gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt`.

## Known Issues / Handoffs
* **A matched-N `ns_per_call` comparison against the m10/m13 trees is not like-for-like on an
  allocation-shaped loop.** They run **zero** collections on the `foreign` loops — the admission declined
  and the heap grew instead (m10's `enc` nursery is 147 249 words at N = 1 000, 3 756 849 at N = 25 000
  and 9 020 849 at N = 60 000) — whereas HEAD collects about once per call, so `bstr_to_lower`,
  `bstr_capitalize_token` and `emqx_base62_decode` read 1.25×–1.60× dearer on HEAD at matched N on both
  the default and the interpreter tier. The difference is the memory management HEAD performs and the
  unbounded heap the old trees retained, not dispatch or tier; `emqx_base62_encode` — the case the
  regression was recorded on — is at parity.
* **A natively executed compiled block is charged only at a deopt / heap-admission boundary.** The
  per-instruction charge belongs to the interpreter's loop; the engine's baseline driver runs compiled
  blocks and only deopts (or reaches `ensure_heap` through the host), so a natively linked tail loop that
  neither allocates nor leaves the block still runs to its next `return`/`wait` inside the one slice that
  entered it — and therefore never leaves its tier either. The interpreter tier carries no such limit:
  `BudgetContext::reduce` charges every instruction there.

## Routing Table
* `mod.rs` → module docs, sub-module wiring, root re-exports (`ProcessEntry`, `RunnerState`,
  `RuntimeRunner`).
* `entry.rs` → `ProcessEntry`, `ProcessEngine` (incl. the remembered `tier`, the park's published
  FR-prefix count `fr_published`, and the registration cursor `registered_payloads`) + `RunnerState`
  (the `jit_mode` field and the `jit_mode()` accessor).
* `run.rs` → `RuntimeRunner`: the (first-entry) tier selection and the tier-aware resume, the
  parked-machine root publication (`publish_parked_roots`/`resume_from_process_roots`, whose `fr_count`
  argument is the live prefix `Machine::fr_mark()` — the value that also opens the inter-slice window's
  relocation exemption), the
  return-value / exit-reason stabilization (whose `SourcePayloads` come from
  `Runtime::source_payloads` — the machine's handles unioned with the pid's retained mailbox
  off-heap families).
* `budget.rs` → the reduction budget + the charging wrapper context (`wants_reduce`/`reduce`: one
  charge per interpreted instruction, plus the two `ensure_heap` doors a native block alone reaches),
  forwarding `resolve_external_call`/`resolve_local_fun`/`resolve_local_call` (the engine consults the
  wrapper, never the inner context, from its `call_ext*`, `call_fun`/`call_fun2` and
  `Call`/`CallOnly`/`CallLast` arms and the native baseline tier's `jit_local_call` probe) and the
  in-machine door pair `wants_bif_in_machine`/`call_bif_in_machine` to the inner `RuntimeContext`
  (`src/context/position.rs`).
* `tier.rs` → the tier mode + its `BEAM_RT_TIER` semantics (`mode_from_value`), the environment default
  and the selection (`select_entry_tier`).
* `reason.rs` / `perf.rs` → the exit-reason translation / the `+JPperf` code map.
* `tests.rs` → the unit + scheduled-path tests above.
