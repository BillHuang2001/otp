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
past the per-instruction hook and is charged at a deopt, at a heap-admission point, and — as a
**price**, never a refusal — at every BIF dispatch and call resolution (`BudgetProbe::spend`), so a
`call_ext`-only loop's budget really drains and `reductions` counts its work. What such a loop still
cannot do is *end* a slice (see *Known Issues*). That is also why the tier is selected eagerly at the
first entry rather than after a warm-up: the bounded-warm-up alternative was measured and declined
because it would pin exactly those native hot loops to the reference tier for their whole life (the
characterisation and figures are in `PERF.md`).

## API Surface (crate-internal, `pub(crate)`)
* [`entry`] — `ProcessEntry` (the runtime-scheduled entry: an export or a fun), `ProcessEngine`
  (per-pid machine/entry/parked state + the **remembered `tier`**) and `RunnerState`
  (per-pid engine state, recorded `result`/`exit`, `jit_mode` override — its four pid-keyed maps
  `engines`/`results`/`exits`/`errors` are `crate::hasher::FastHashMap`, sound because each is only
  ever probed by pid and never iterated for an observable order).
* [`run`] — `RuntimeRunner`: the `ProcessRunner` implementation (one reduction slice) — the tier
  selection (once, at the first entry) and the tier-aware first-entry/resume execution, the
  parked-machine root hand-off (`park_machine`, whose `ProcessEngine::parked_published` flag gates the
  resume's `resume_from_process_roots`), the
  parked-receive deadline rearming and the recorded outcome.
  An **export** entry is classified once, before the context borrows its budget wrapper, through the
  runtime's host-dispatch predicate (`RuntimeContext::host_serves` — a runtime-native-floor name, a
  NIF-loaded module, or a registered `beam_bifs` key): a **host-served** MFA is dispatched through
  `call_bif` (so `spawn(erlang, send, [Pid, Msg])` — `timer_server:do_apply/2`'s shape — delivers
  the message instead of entering `erlang.beam`'s `nif_error(undef)` stub), and **every** other
  entry keeps the export's own label route (`module.export/2`, with its
  `run_export: export not found` arm). It is deliberately **not** the cross-module seam's
  `resolve_external_call`, whose `Ok(None)` is overloaded with the module-unresolvable,
  missing-function-atom and self-observing-callee refusals: reading it as "host-served" routes a
  resolvable entry to `call_bif`'s nested **non-pumping** `run_export`, where a blocking callee
  folds into the typed `blocked()` gap and the process never parks.
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
* [`budget`] — the slice's reduction budget and the wrapper context that charges it: `BudgetProbe`
  with **two** charging primitives, `charge()` (the **refusing** one: spends a unit or hands back the
  typed `preemption: …` error, the runner's `Yield` signal) and `spend()` (**pricing, never
  refusing**: consumes a unit while the slice still has one, a silent no-op otherwise, and never sets
  `exhausted` — the exhaustion is always reported by the next `charge()`). `BudgetContext` declares
  its own `wants_reduce() == true` (the engine lets a host answer `false` only when its `reduce` is
  provably the no-op default) and implements `reduce` to `charge` once per reduction unit the
  interpreter prices, so a **non-allocating** interpreted loop is pre-empted; the two `ensure_heap`
  doors also refuse (`charge`), as the only refusing charge a native compiled block reaches.
  `spend` prices the **four** seams a compiled block *does* reach: `call_bif`/`call_bif_in_machine`
  one unit **after** the dispatch (an engine arm folds a BIF-seam error into BEAM semantics — a guard
  failure or a caught raise — so a refusal there would be a wrong result, and a charge taken around an
  already-applied BIF could not be re-run exactly once) and
  `resolve_external_call`/`resolve_local_call` one unit **before** forwarding (a refusal there would
  park at the block entry rather than at the instruction — `Known Issues`). Every one of them is also
  **forwarded** — the slice's machine runs on *this* wrapper. As the
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
* **The slice's real context is built once per `run` call (one scheduler slice).** [`run`] constructs
  `RuntimeContext::for_process_with_engines` once, then wraps it in one `BudgetContext` and hands that
  wrapper to the whole machine run — so every BIF/`call_ext`/`call_bif` a slice makes shares **one**
  `RuntimeContext` and therefore one `DispatchMemo` (the 8-slot per-context cache); the memo is rebuilt
  only at a slice boundary (a pre-emption/`receive` park ends the slice) or at the two nested-context
  sites (`FunBridge::nested`, `SendBridge`, both in `src/context/bridges.rs`). The `RuntimeContext`/
  `BudgetContext`/`BudgetProbe` construction is **stack-only** (no `Vec`/`String`/`Rc` on the hot path);
  the only per-slice heap work below the outcome match is the off-heap-payload registration suffix
  (`payload.clone()` on `Process::register_off_heap_binary`, once per *new* payload the machine built).
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
[`run`] therefore hands the machine over at **both** park arms — the pre-emption/`Yield` arm and the
`Ok(EngineRunOutcome::Wait)` arm — through the **one** hand-off `park_machine(table, pid, engine,
fr_count, exempt)`, right before `engines.insert`, and calls `resume_from_process_roots(ctx, pid,
machine, fr_count)` after `ctx.enter_module(module)` when `engine.parked.is_some()`.
`park_machine` always records the live FR-prefix count on `ProcessEngine::fr_published`, but publishes
only a window that is **not** relocation-exempt: an exempt window sets `ProcessEngine::parked_published
= false` and returns `false` without publishing, while a non-exempt one sets `parked_published = true`,
calls `publish_parked_roots` and returns `true`.
The skip is sound because for the exempt window's whole duration nothing can relocate this process's
heap — `Process::collect`/`ensure_capacity`/`collect_if_needed` return the typed refusals,
`ProcessTable::deliver` declines, the host collection gate's `declines_relocation` declines and
`Runtime::serve_gc_request` returns `false` on `is_relocation_exempt` — and `Process::grow_nursery`,
the one growth path that is deliberately not gated, relocates nothing, so the parked machine stays the
authoritative root set and the publish+resync pair would be a pure identity round-trip over a
`min(x.len(), MAX_REG)`-word x file plus the Erlang stack, both ways (the rationale is on `park_machine`
itself).
The `Yield` arm passes `exempt = fr_count > 0` (its post-slice `fr_mark` check already opened the window
exemption for a slice that ended with a live FR prefix) and the `Wait` arm passes `exempt = true`.
`resume_from_process_roots` therefore runs **only** when the matching park published
(`engine.parked_published`), which is then cleared; the window's `set_relocation_exempt(pid, false)` is
cleared unconditionally afterwards.
`publish_parked_roots` and `resume_from_process_roots` delegate to the **one** shared implementation in
[`context::gc`](../context/CONTEXT.md)
(`publish_roots_full`/`scatter_roots_full`, the `fr_count` parameter bounding how many leading FR words
are published), which the in-slice heap-admission points (`ensure_heap_full`/`alloc_words_rooted`) also
use — those hand a `RootSet` already narrowed to each register file's pointer-bearing prefix
(`machine_root_bounds`), with `fr_count` its FR count — so the park/resume and the admission
points can never drift:
* `publish_parked_roots` hands the machine's roots over with `RootSet::ALL_LIVE` — the conservative
  (always-safe) choice, since a parked machine's live count is not exposed and over-approximating a
  copying collector's root set can only keep a term alive, never free a live one.
* **The FR file is published only up to its live leading prefix `fr_mark()`.** The runner reads the
  count **once** per slice (`Machine::fr_mark()`, the engine's exclusive high-water mark of FR writes
  since the machine's last reset) and reuses it for both the post-slice reaction and the
  `publish_parked_roots(.., fr_count)` call in the same slice; `fr_mark() > 0` also marks the process
  **relocation-exempt for the inter-slice window** (`ProcessTable::set_relocation_exempt(pid, true)`,
  cleared by the next slice entry), and that exemption is exactly what makes a `Yield` park with a live
  FR prefix **skip** its publication (`park_machine`'s `exempt` argument) instead of copying the file:
  the parked machine's own FR slots are protected from the delivery-driven collection — which is not
  gated by `gc_process_enabled` — by the exemption rather than by a copy. The reaction is **per slice,
  never a permanent pin**: inside a slice the engine reaches only the full door, which publishes the
  FR file up to its pointer-bearing prefix and applies no float guard, so a float-using call site
  collects exactly like a float-free one. `fr_mark` is an
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

## Per-slice (not per-instruction) host cost
Beyond the `BudgetContext` charges (a per-instruction `reduce` on the interpreter tier, the two
`ensure_heap` doors, and a per-BIF/per-call `spend` on the native tier), one slice's prologue/epilogue
does this host work (each is amortized over the whole 4000-reduction budget, so it is per-`run`, never
per-iteration):
* **O(1), no allocation:** `RunnerState::engines.remove(&pid)` / `insert` (`run.rs:383`/`817`), the
  FR prefix read `Machine::fr_mark()` (`run.rs:677`), the tier lookup (`run.rs:430-438`),
  `note_reductions` (`run.rs:602`), `serve_gc_request` (early-returns without a pending request),
  and — on the `Yield` arm only — `current_code()` (`run.rs:778`) and
  `reset_reductions`/`bump_reductions` (`run.rs:791-792`).
* **One allocation per slice:** `RuntimeContext::enter_module` (`run.rs:475` →
  `../context/runtime_context.rs:501-508`) builds a fresh `String` of the module name on **every**
  slice (a `RuntimeContext` itself is a stack value: `for_process_with_engines`, no heap alloc).
* **O(size), on a park whose window is NOT relocation-exempt:** `park_machine` (`run.rs:959`,
  `run.rs:1084`) calls `publish_parked_roots` → `gc::publish_roots_full`
  (`../context/gc.rs:370-395`), which copies the whole x file
  (`min(x.len(), MAX_REG)` = up to 1024 words, `beam_process::registers::MAX_REG`) plus the whole
  Erlang stack plus the live FR prefix onto the process's root set. An exempt window — the `Wait`
  arm, and a `Yield` arm whose slice ended with a live FR prefix — skips the copy entirely.
* **Amortized O(1):** the off-heap registration loop (`run.rs:760-769`) walks only the
  not-yet-registered reservoir suffix (`ProcessEngine::registered_payloads`), so a loop that builds
  no off-heap binary pays one comparison.
* **First entry / flag-gated only:** `record_baseline_code` (`run.rs:692-694`, `+JPperf` only), the
  `Arc::clone` of the entry module (`run.rs:413-420`), and `describe_site` (debug-trace only).
  `source_payloads` (`run.rs:840-842`/`970-971`) runs only on the terminating `Returned`/`Err` arms.
* **No production time read:** `Instant` appears only in `runner/tests.rs`; the slice path does no
  `sys` call of its own.

## Measured
The tier is taken on a process's **first** entry and then carried for its whole life: a pre-empted
slice and a `receive` park both resume through `Machine::resume_outcome_with_tier` on the tier the
process was selected for, so a *pre-empting* hot loop keeps its tier across every slice
(`a_preempted_process_resumes_on_its_baseline_tier`/`a_receive_parked_process_resumes_on_its_baseline_tier`
pin this structurally, without a timing). On the **interpreter** tier every interpreted instruction
charges the budget, so any hot loop pre-empts (pinned by
`a_non_allocating_interpreted_loop_is_pre_empted`). On the native tier a compiled block runs natively
past the per-instruction hook, so a loop that reaches no **refusing** seam either — a
binary-accumulating or `call_ext`-only loop, or a natively linked tail loop with no allocation —
prices its BIF/call seams but runs to completion inside one slice on the tier its first entry
selected (pinned by `a_non_allocating_loop_on_the_native_tier_completes_within_one_slice`, and, with
the exact charge, by
[`tests/slice_charging.rs`](tests/slice_charging.rs)::`the_slice_budget_prices_native_work_and_a_refusing_seam_still_ends_the_slice`).
Per-workload speed and memory figures are **measured, never modelled**: the full cross-runtime
record lives in [`../../PERF.md`](../../PERF.md) and the bench driver's
[`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md), and the figures the
scheduled path is judged on are re-measured below rather than carried forward.

`BEAM_RT_TIER=interpreter` is the always-correct reference tier and the **only** kill switch: the
default (`Auto`, and its `baseline` alias) puts every process on the native tier.

**The two-process delivery path** (measured 2026-10-17 with the committed driver,
`bench run --quick --runtime all --workloads copy_bound`, `taskset -c 3`, 15 rounds with the two
runtimes paired **inside** each round and the min taken over the rounds; `rust` = the release
`beam_rt` on its default baseline-JIT tier, `sys` = OTP 29.1.1 / erts-17.1; host `/proc/loadavg`
1.0–3.2):

| case (`case_<name>_ns_per_op`) | rust | sys | rust/sys |
|---|---|---|---|
| `send_recv_plain` (delivery only) | 1 943 ns (median 2 046) | 666.5 ns (median 672.8) | **2.92×** (per-round median 3.01×, range 2.90–3.16×) |
| `send_recv` (+ the ETF codec in the worker) | 7 631 ns (median 7 837) | 1 493 ns (median 1 506) | **5.11×** (median 5.22×) |
| `ets_copy` | 2 367 ns (median 2 391) | 219.3 ns (median 222.9) | **10.8×** (median 10.7×) |
| `etf_roundtrip` | 1 449 ns (median 1 521) | 655.0 ns (median 660.0) | **2.21×** (median 2.30×) |

The loop count behind each marginal is the deterministic, contention-immune `case_*_ops` counter —
rust **15 000** / **10 000** / **200 000** / **40 000**, sys **1 000 000** / **500 000** /
**2 000 000** / **1 500 000** — so the shape of the ratios survives a loaded host even though the
absolute walls do not.

**The `run` route issues no poll sleep on a ping-pong loop, and still sleeps to a parked
`receive … after T`** (`strace -f -e trace=clock_nanosleep -T` on the release binary,
2026-10-17): `beam_rt run tests/fixtures/pingpong.beam pingpong:go/1 200000` — 200 000 synchronous
round trips, the same two-process shape — makes **0** `clock_nanosleep` calls, while
`recv_after:after20/0` (parked on `after 20`, nothing else to run) makes **1** (19.9 ms) and
`recv_after:after2000/0` makes **80** (75 clamped 25 ms poll slices plus 8.6 ms/0.7 ms remainders,
≈2.0 s in total). Both halves are pinned by `tests/run_route_sleep.rs`, which skips loudly when
`strace` is absent.

**The per-call work of a growth-shaped loop is FLAT in N — deterministic counters, never wall clock.**
`the_churn_workloads_per_call_counters_are_flat_in_n` and
`the_heap_growth_workloads_per_call_counters_are_flat_in_n` each drive their fixture at three (resp.
five) increasing iteration counts and print a `counter_table`: N vs slices / yields / waits /
collections / major / fallbacks / peak young-nursery geometry / debt boundaries, plus the derived
per-call ratios — every geometry figure quoted here is re-derived by that line, so read it rather than
this prose for the mature-space numbers.
Measured on the native tier (current figures; the same `churn` shape is pinned at *three* counts by
[`tests/boundary_cost.rs`](tests/boundary_cost.rs)): `churn` collections/call 6.982009 / 6.984002 /
6.984600, `major`/call 1.497751 / 1.497950 / 1.498025, `reductions`/call 1 808.98 / 1 809.71 /
1 809.84 and slices/call 0.452274 / 0.452455 / 0.452464 at N=2 000 / 10 000 / 40 000 — flat, and the
per-iteration rate (13 971 / 2 997 at N=2 000, 27 939 / 5 993 at 4 000, 69 847 / 14 981 at 10 000) is
6.9855 … 6.9847 collections and 1.4985 … 1.4981 majors per iteration, converging on 7 / 1.5 from
below. It is **not** the closed form `collections == 7N − 1` / `major == 3N / 2`: the door now pins a
young-only `CollectionKind::Minor` collection and escalates to an unpinned `collect` only when the
promotion cannot fit the mature space, and the native tier charges its blocks per block, so the slice
counts those rates are expressed against moved (both changes postdate the closed form).
`heap_growth` collections/call 1.001100 / 1.001200 / 1.001220 and slices/call 0.001300 / 0.001280 /
0.001260 at N=10 000 / 25 000 / 50 000: the 401-word request cannot fit a fresh nursery, so the
growth/collection door is reached on **every** iteration, which is the derived relation that assertion
pins (`collections == N + yields`, exact on all five rows — 5 006 / 10 012 / 25 031 / 50 062 / 100 125
against 6 / 12 / 31 / 62 / 125 yields).
In both shapes `peak_young_spaces == 1` at every N, so the retired-nursery chain the
`collection_debt` trigger protects stays collapsed rather than N-dependent; the young capacity
plateaus at **609** words for `churn` (whose mature generation then sits at 987 words capacity / 880
used — the 233-word figure older prose quoted is the pre-growth floor, not the plateau) and **233**
for `heap_growth` (whose 610-word space is its **mature** generation), and `yields == slices − 1` with
`waits == 0` and `debt_bounds == 0` throughout (neither loop ever parks, and neither leaves an
un-settled collection debt at a boundary).
Neither loop's cadence is this node's to set: the door decides *whether* a collection may run (and
the counters above show it declining nothing), the collector's post-collection nursery sizing decides
*how often* — see [`../context/IMPLEMENTATION.md`](../context/IMPLEMENTATION.md) *What the doors do
NOT decide*.

**The reduction/yield-boundary door's own cost on the hot shapes — ON vs OFF out of ONE build.**
`collect_at_reds_boundary` (`../context/gc/reds_door.rs`, reached from the `Yield` arm only) fires on
a **publishing** slice-end park when `REDS_BOUNDARY_MIN_REDS` reductions have elapsed since that
process's last boundary check **and** the young heap is **seven eighths** used (or the collector's
`collection_debt` is set). The watermark was retuned from `1/2` to `7/8` after the churn shape was
found to pay a real amplification; the C's own rule (`ERTS_VBUMP_ALLOC_REDS`) fires when the
allocation area is **exhausted**, not when it is half used. It is judged on per-call counters alone,
never on wall clock, out of **one build**: both sides come from interleaved `A B A B …` legs (15
pairs), the door-disabled side reached by a throwaway `#[cfg(test)]` early return and the watermark by
a throwaway `#[cfg(test)]` override (both removed with the measurement), because copies of the same
tree measured in different processes are not comparable on this host.
The estimator is the **paired per-call difference** (every counter here is deterministic, so the
difference is identical in every pair), the denominators are the shape's own call count `N + 1`,
`door checks/call` counts the publishing `Yield` parks handed to the door and `door colls/call` those
that became a collection (`door.unpublished` is **0** on every shape — no hot shape ends a slice with
a live float-register prefix), `Δ` is ON − OFF, and `fallbacks/call` is 0 on both sides of every row.
The default-tier sweep is the native baseline JIT, where a shape that reaches no **refusing** seam
never pre-empts (so most rows show ≤1 slice for the whole run — the residual under *Known Issues*);
the interpreter sweep (`BEAM_RT_TIER=interpreter`) is where a compute-bound shape really pre-empts
every ~4 000-reduction budget, which is the only regime in which the door is reached often.

The retune (`gc_bound` churn, default tier). At `7/8` the door is **free on the shape that used to
pay**, measured on this tree: the shape's totals stay flat and its retired chain stays collapsed (one
young space, **609**-word capacity peak, at N = 2 000 / 4 000 / 10 000) while the door fires **105 /
214 / 533** times out of **904 / 1 809 / 4 524** boundary checks — one in 8.61 / 8.45 / 8.49, i.e.
0.0525 / 0.0535 / 0.0533 firings per call and **0.75 %** of the run's own 13 971 / 27 939 / 69 847
collections. At `1/2` it had measured **+0.048 collections/call (+0.69 %)** with `major`/call **+8.9 %**
against its door-off build; `1/1` (the C-exact "used ≥ capacity") buys nothing over `7/8` (3 instead of
11 firings at N = 500, the same counters) and is *not* taken: the nursery grows in place, so an exact
`used == capacity` test is satisfied on consecutive boundaries and could chain majors where `7/8`
leaves the growth seam room.
**Explicitly NOT verified here any more — the door-ON vs door-OFF `Δ` and every door-off figure in
this section** (the "identical totals" at 6.984032 / 6.996002 / 6.999200 collections and 1.497006 /
1.499250 / 1.499850 majors per call at N = 500 / 2 000 / 10 000, the `7N − 1` / `3N / 2` closed form,
the door-off `2` spaces / `376` words, and the `1/2`-watermark deltas above). Reproducing them needs a
door-disabled build — a throwaway `#[cfg(test)]` early return inside
[`../context/gc/reds_door.rs`](../context/gc/reds_door.rs) — which is outside the scope of the last
re-derivation, and they were taken on an earlier tree state: N = 2 000 now measures **6.982009**
collections and **1.497751** majors per call at a **609**-word young space, and the closed form no
longer holds at all (the door pins a young-only `CollectionKind::Minor` first and escalates only when
the promotion cannot fit the mature space, so it no longer forces a cadence fullsweep; the native tier
also charges its blocks per block now, which moved the slice and pre-emption counts). The figures that
*are* current are the ON-side ones in the paragraph above, printed and pinned by
`tests/boundary_cost.rs`.
The two shapes that must **not** be lost to the tighter watermark (interpreter tier): `reds_boundary`
reports one collection per pre-emption (`yields == collections`) with `major` 1 at a **610**-word
`heap_size` against the door-off 3 074 841 / 29 485 327 words at N = 2 000 / 20 000, and
`etf_roundtrip` goes 0 → 0.005 collections/call at a bounded nursery against the door-off
1 347 856 / 3 075 217 words over **10 / 11** retired spaces at N = 10 000 / 40 000.
The no-heap-admission shape is **debt-driven** and identical at every watermark ≥ `1/2` (every check
reports `collection_debt`, so the occupancy arm never decides it): one collection per pre-emption, at
the loop's own per-iteration reduction price over the `REDS_BOUNDARY_MIN_REDS` interval — which is why
that interval is retained unchanged, and why the rate moved when the call/BIF seams began to be
**priced** (`tests/reds_door.rs`'s measured 17..=20 reds/iteration band, i.e. ~200–225 iterations per
4 000-reds slice, against ~14 reds/iteration before the pricing).

The remaining shapes of the earlier whole-shape sweep, which the watermark cannot move (their
`door checks/call` is 0 or ≤1 per run, so no gate ever decides them), are **inert — the paired
difference is exactly 0**: `send_recv`/`send_recv_plain` on both tiers (the ping-pong parks through
the **`Wait`** arm, never `Yield`, so no boundary is ever offered) and, on the default tier,
`etf_roundtrip`, `list_walk`, `slice:fib/1` and `heap_growth` at 3 000 (each completes inside one
slice or never pre-empts at all). **Below measurement resolution**: `ets_copy` on the default tier
(0.0002 checks/call, **one** extra collection over 40 000 calls) and `heap_growth` on the default tier
(at most two over 12 000, +0.000167/call) — neither moves `major`, and neither shape's capacity is
larger with the door on at the wider N. `slice:fib/1` on the interpreter tier is the intended fix in
miniature (1 boundary → 1 collection where the door-off side collects nothing), and `list_walk` on the
interpreter tier sees 2.005 checks/call whose collections the shape's own door absorbs (Δ 0).
**Nursery resizing is a real effect of one collection**: `heap_growth` on the interpreter tier peaks at
**843** words with the door on vs **610** off (+38 %), since a boundary collection re-sizes the nursery
and every later admission keeps that size — `ets_copy` on the interpreter tier and `churn` on the
default tier go the other way (the door's own collection collapses the retired chain the door-off side
retains; on this tree `churn`'s peak young capacity is **609** words with the door on, and the `376`
door-off figure is from that earlier A/B, not re-measured).
**Residual (recorded, not taken, and — like the rest of the door-off side above — NOT re-measured
here):** `ets_copy` on the **interpreter** tier still showed `+0.000400` collections/call and
`major`/call `+0.000600` (down from `+0.000800` at `1/2`) — 16 extra collections over 40 000 calls,
~3× the collection-rate change, attributed at the time to a firing boundary also collecting the
**mature** generation. Its *mechanism* sentence is out of date: the door no longer calls the
escalating `beam_process::Process::collect` as its action
([`../context/gc/reds_door.rs:306`](../context/gc/reds_door.rs) pins a young-only
`Process::collect_kind(CollectionKind::Minor)` first and escalates to `collect` only when that minor
refuses because the promotion cannot fit the mature space), so a cadence fullsweep comes only from a
mature generation too small to take the promotion. What is *still* missing is a young-only
collection that **grows** the mature space instead of refusing: `beam_gc::AnyCollector::collect_minor`
reports typed heap exhaustion when the mature space cannot absorb the whole nursery, so every
refusal on these shapes escalates. The handoff is `beam_gc::AnyCollector::collect_young(&mut self,
roots: &mut [Eterm]) -> Result<Collection>` (never escalates; grows/reclaims the mature space instead
of a fullsweep; reports `CollectionKind::Minor`), recorded in `../../M1_GAPS.md` *GC / heap model*.
The pinned, current fact is `churn`'s **0.75 %** firing rate at one collapsed 609-word space (next
paragraph), which is what the ratio above would have to move to matter.
The door's structure is pinned by [`tests/boundary_cost.rs`](tests/boundary_cost.rs)
(`the_boundary_door_is_a_per_preemption_watermarked_site_on_the_churn_shape`, three measured counts
N = 2 000 / 4 000 / 10 000): `door checks == yields` at every row, `door collections` inside the
measured `checks / 9 ..= checks / 7` band (**105/904 = one in 8.61**, **214/1 809 = one in 8.45**,
**533/4 524 = one in 8.49** — no `checks / 8` law holds, which is why the band, not an identity, is
what is pinned), `door.unpublished == 0`, `checks/call` in `0.40..=0.50` (measured 0.4518 / 0.4521 /
0.4524), the shape's own totals as an **exact measured table** (13 971 / 2 997, 27 939 / 5 993,
69 847 / 14 981 — the closed form `7N − 1` / `3N / 2` is *not* what this tree runs), one young space
and a capacity ≤ 610 words, so the amplification cannot come back silently.
The no-admission shape's relations are pinned by [`tests/reds_door.rs`](tests/reds_door.rs)
(`yields == collections`, the 17..=20 reds/iteration band, `minor == collections − major` = `(8, 88)`
at N = 2 000 / 20 000, a converged 610-word `heap_size` and one young space).
**`foreign` per-call cost, with the mechanism that keeps the loops from degenerating.** Measured
2026-10-17 with the committed driver (`bench run --quick --runtime all --workloads foreign`,
`taskset -c 3`, 8 rounds, both runtimes paired per round, min over the rounds; host `/proc/loadavg`
1.0–2.5):

| case (`case_<name>_ns_per_call`) | rust | sys | rust/sys |
|---|---|---|---|
| `bstr_to_lower` | 12 413 ns | 324.3 ns | 38.3× |
| `bstr_capitalize_token` | 15 255 ns | 317.2 ns | 48.1× |
| `emqx_base62_encode` | 10 512 ns | 206.3 ns | 51.0× |
| `emqx_base62_decode` | 10 111 ns | 282.5 ns | 35.8× |

The mechanism is the **literal-area tagging**: `Nursery::grow` retires the old space into `retired`
and only `Nursery::collapse` — reached at a collection — clears it, so a runtime that never collects
keeps every retired space's capacity resident (RSS grows per call) and pays the valve's per-word
scanned test (`root_is_unscanned` → `outside_scanned`) over a longer chain on every admission — that
test is `O(1)` for an address outside the aggregate `scanned_bounds` and `O(log #spaces)` otherwise
(`Nursery::contains` reads its ordered `retired_regions` index, `beam_gc/src/nursery.rs:194-213`; it
never walks `spaces()`). `beam_loader`'s `tag_literal_area`
(`crates/beam_loader/src/literal.rs`, the safe form of the C load-time `erts_set_literal_tag`) tags
the decoded literal area, so the walk recognizes these modules' constant literal-area binary as an
off-heap literal — not a container in a region the collector does not scan at all
(`context/gc.rs::word_region` answers `None` for a literal, which no relocation touches) — so the
valve never declines, the descent latch stays closed, and the retired chain stays collapsed
instead of growing with the iteration count. A constant payload a module *builds* would instead be
a foreign container that forwards nothing scanned: also inert, so also collected.

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
* **The slice budget's pricing** is its own suite, [`tests/slice_charging.rs`](tests/slice_charging.rs)
  (`the_slice_budget_prices_native_work_and_a_refusing_seam_still_ends_the_slice`), over the committed
  `tests/fixtures/native_reds.{erl,beam}` fixture: the exact `reductions == 5N + 9` charge on the native
  tier to the derived budget boundary (N = 798 charges 3 999 in one slice, N = 799 charges 4 004 and needs two) and its flat **4 006** ceiling above it (`budget` 4 000 + a measured 6-unit epilogue), the bounded-iterations-per-slice
  pre-emption of the allocating shape (`yields + 1 == slices`, `slices == ceil(reductions / budget)`),
  and the residual tuple `(slices, yields, waits, collections, major) == (2, 1, 0, 1, 1)` with its
  interpreter-tier control. It asserts the default tier (and fails loudly if `BEAM_RT_TIER` pins the
  interpreter), never a wall clock.
* The park hand-off and its exempt-window short-circuit:
  `a_live_fr_park_skips_the_hand_off_and_opens_the_window_exemption` and
  `a_float_free_park_does_not_publish_the_fr_file` assert the structural `park_publish_counters()` —
  `skipped` moves and `published` stays `0` for an exempt park (a live-FR `Yield` window and the `Wait`
  window), while the parked process's published stack stays empty, i.e. the machine remains the
  authoritative root set; `a_parked_process_stays_relocation_exempt_for_its_parked_window` signals an
  oversized reason into a parked receiver and asserts the window's exemption (not the host gate) declined
  the relocation, with the nursery unmoved and the process collecting normally after the resume clears
  the mark; `a_parked_machines_pending_exception_reason_survives_a_moving_collection` asserts a
  **published** park's extra root is relocated and read back into `Machine::pending_exception_mut`.
* End-to-end: `slice:fib/1 10` through the scheduled path (`RuntimeRunner`) records `55` under both an
  explicit `JitMode::Auto` (→ baseline) and `JitMode::Interpreter`, and the scheduler path records it
  under whatever mode the environment selects.
* The two transport sites' off-heap resolution is covered end to end by
  `tests/engine_off_heap_transport.rs` — the `Returned`/exit-reason arms for a binary the process
  **built** *and* one it **received from another process** (`received_off_heap_binaries_survive_the_transport_copy`),
  each byte-compared with the live oracle.
* The `run` route's **sleep accounting** is pinned end to end by `tests/run_route_sleep.rs` over the
  committed `pingpong` fixture: `pingpong:go/1` (a two-process synchronous round trip — every iteration
  parks the parent on an empty mailbox, so the pass boundary is reached repeatedly) is run by the CLI
  and compared with the oracle, and under `strace -f -e trace=clock_nanosleep -T` it must show **no**
  `clock_nanosleep` while `recv_after:after20/0` (parked on a finite deadline, nothing else to run) must
  show at least one — the run route's "never sleep while it has work" half and the anti-spin half. The
  syscall test skips loudly without `strace`; the value test skips loudly without an oracle.
* Crate gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt`.

## Known Issues / Handoffs
* **A matched-N `ns_per_call` comparison against a runtime that never collects is not like-for-like
  on an allocation-shaped loop.** Such a runtime runs **zero** collections — the admission declines and
  the nursery grows instead, ≈150 words/call — whereas this one collects about once per call, so the
  allocation-shaped `foreign` decode cases read 1.25×–1.60× dearer here at matched N on both the
  default and the interpreter tier. The difference is the memory management this runtime performs and
  the unbounded heap the other retains, not dispatch or tier.
* **A natively executed compiled block is charged at a deopt, a heap-admission point and each **priced**
  BIF/call seam — but only a *refusing* seam can end its slice.** The per-instruction charge belongs to
  the interpreter's loop, so what a native block's budget drains is [`budget`]'s `spend`
  (`call_bif`/`call_bif_in_machine` after the dispatch, `resolve_external_call`/`resolve_local_call`
  before the forward) plus the two `ensure_heap` doors and any deopt.
  **Measured**: the non-allocating BIF-and-call fixture prices exactly `reductions == 5N + 9` (5 units
  per iteration + a constant 9 for the entry/epilogue seams, at N = 0/10/100/400/700/798 — the N = 0
  row is that constant alone) and above the budget it saturates at exactly **4 006** (`budget` 4 000 +
  the epilogue's measured 6) — it was **0** before the pricing; the allocating
  `heap_growth` shape charges `reductions == 5N + 5` and pre-empts into `slices` 3/13/26/101, `yields`
  2/12/25/100 and `collections` 2 002/10 012/20 025/80 100 at N = 2 000/10 000/20 000/80 000 — a bounded
  **800** iterations per 4 000-unit slice (`collections == N + yields`: one admission collection per
  iteration plus one boundary collection per pre-emption).
  **Residual**: a **non-allocating BIF-and-call-only loop still ends no slice of its own** on the native
  tier — `spend` never refuses and is a silent no-op once the budget is gone, so the *single* `Yield`
  those rows read is the run's **epilogue** seam (a same-shape fixture with no epilogue ends **zero**
  slices: `slices=1 yields=0 reductions=4 000` at N = 8 000) and the boundary collection door stays
  unreachable for the loop. Only the memory half of the old residual has closed: that one `Yield` does
  collect, so `heap_size` sits on the collector's floor (233) instead of tracking N. The
  interpreter-tier control on the same fixture at N = 2 000 reads 10 slices / 9 yields / 9 collections
  / 1 major / `reductions` 36 018 / `heap_size` 233.
  A **refusing** charge cannot be taken at a call seam today: the native tier publishes
  `machine.current_code` at **block** granularity (`crates/beam_engine/src/jit/baseline/runtime.rs:993`)
  and its call paths consult the seam before storing any position of their own
  (`jit/baseline/call.rs:210` vs `:232`; `runtime.rs:948`; `runtime.rs:818`), so a refusal parks at
  block entry and the resume re-executes the block prefix — measured with a draft `charge()?`:
  `-eval 'halt(0).'` and `run slice.beam` both exited 3 with `unsupported: opcode: run: process
  blocked (no message available)`.
  **Handoff (out of this crate)**: `beam_engine` must store the instruction address
  (`machine.set_current_code(func, pc)`) before consulting the seam at those three sites, after which a
  call seam may refuse and the third pin of [`tests/slice_charging.rs`](tests/slice_charging.rs) is the
  assertion that must change. That suite holds the three pins (the exact charge, the bounded
  iterations-per-slice pre-emption, and the residual tuple plus its interpreter control) over the
  committed `tests/fixtures/native_reds.{erl,beam}` fixture.

## The collection-site inventory this node owns (and what each site publishes)
**The scheduled path is the ONLY path that collects at all.** `RuntimeContext::gc_armed` is
[`take`](std::mem::take)n as the **first statement** of each rooted door
(`../context/execution.rs:472`/`:486`/`:510`, forwarded as the `armed` argument to
`gc::{ensure_heap, ensure_heap_full, alloc_words_rooted}`), and the **only** arming sites in the
workspace are [`budget`]'s three doors — `budget.rs:534` (`ensure_heap`), `:549`
(`ensure_heap_full`), `:605` (`alloc_words_rooted`). So on the `-eval` route, a nested
`run_export`, a fun a BIF applied and the evaluator, `armed` is `false` and every door refuses to
collect: **a heap can only move during a scheduler slice.** `BudgetContext::call_bif_in_machine`
(`budget.rs:365`) and `reduce` (`:566`) do **not** arm, so no collection can run inside a BIF
dispatch — the invariant `../context/position.rs:67-69` relies on.
| site | reached from | publishes | gate |
|---|---|---|---|
| `../context/gc.rs:1252` `ensure_heap_full` | `Instr::TestHeap`/`AllocateHeap` → `Machine::ensure_heap` (`beam_engine/src/machine/frames.rs:283-289`) + the baseline-JIT twins | `x[..pointer_prefix_len(x)]`, whole stack, `fr[..pointer_prefix_len(fr)]` (`gc.rs:1307`, rebuild `:1382-1383`) | `armed` + the unscanned-root valve |
| `../context/gc.rs:1568` `alloc_words_rooted` | `terms/slack.rs:170` (`terms::reserve`, i.e. every `*_rooted` builder, per built term) | same narrowing (`gc.rs:1674`) | `armed` + valve |
| `../context/gc.rs:1034` `ensure_heap` (narrow) | **no engine call site** (`gc.rs:1807`) | `x[..live]` + stack, **no FR file**; float-guarded | `armed` |
| `../context/gc/reds_door.rs:176` `collect_at_reds_boundary` | `run.rs:1249` (the `Yield` arm only) | `RootSet::ALL_LIVE`: **all 1024 x slots** + whole stack + `fr[..fr_count]` (`reds_door.rs:279` → `gc.rs:786`) | `run.rs:337 parked_published`, `gc_process_enabled`, 4 000-red interval, `is_relocation_exempt`, 7/8 occupancy or `collection_debt`, valve |
| `../context/gc.rs:1465` `alloc_words` | anywhere | **never collects** (grow + retry, else arena) | — |
Out of slice, `ProcessTable::deliver`/`enqueue_exit_signal`/`grow_receiver_for_signal` relocate a
**non-running** receiver only (`beam_process/src/ptab.rs:197`, `:243`, `:2661`, `:2715` all refuse on
`has_unpublished_roots()`, which is `relocation_exempt || Running`), so they cannot touch a running
process's heap.
**Publication is airtight for `x`/`stack`/`fr`; the open root-set question is a holder that is in NO
root set.** The park publishes all 1024 x slots (`gc.rs:786` sets `live = min(x.len(), MAX_REG)`), so
`XRegs::gather_roots` (`beam_process/src/registers.rs:250-256`, which reads `x[0..live)` only)
gathers every published slot, and no production site shrinks `live`. `is_pointer_bearing` is
`(word & 3) != 3`, i.e. exactly "not `TAG_PRIMARY_IMMED1`" (`beam_term/src/tag.rs:37-45`,
`boxed.rs:73-74`, `list.rs:68`), so an atom/small/NIL can never be bound away by
`pointer_prefix_len`. What is NOT covered by any root set: `Machine::pending_exception`'s reason
(`beam_engine/src/machine/mod.rs:151`; unrooted in-slice, published only across a park as
`Registers::extra_roots`, `run.rs:527-540`), the marshalled BIF `args` copy
(`beam_engine/src/interpreter/bif.rs:232-246`, `:157-168`) at the `&self` door
`Machine::dispatch_bif_call` (`bif.rs:291`) which has no write-back channel, and the `Runtime`-owned
term stores (`Runtime::in_machine`, `timers`' reference key, `references`, `mailbox` markers,
`trace`/`seq_trace` specs, `eval_closures`).

## Routing Table
* `mod.rs` → module docs, sub-module wiring, root re-exports (`ProcessEntry`, `RunnerState`,
  `RuntimeRunner`).
* `entry.rs` → `ProcessEntry`, `ProcessEngine` (incl. the remembered `tier`, the park's published
  FR-prefix count `fr_published`, the park's publish/skip flag `parked_published`, and the registration
  cursor `registered_payloads`) + `RunnerState`
  (the `jit_mode` field and the `jit_mode()` accessor).
* `run.rs` → `RuntimeRunner`: the (first-entry) tier selection and the tier-aware resume, the
  parked-machine root hand-off (`park_machine` → `publish_parked_roots`/`resume_from_process_roots`,
  whose `fr_count` argument is the live prefix `Machine::fr_mark()` — the value that also opens the
  inter-slice window's relocation exemption, whose presence makes `park_machine` skip the publication),
  the
  return-value / exit-reason stabilization (whose `SourcePayloads` come from
  `Runtime::source_payloads` — the machine's handles unioned with the pid's retained mailbox
  off-heap families).
* `budget.rs` → the reduction budget + the charging wrapper context: `BudgetProbe::charge` (the
  **refusing** primitive — one charge per interpreted instruction and per `ensure_heap` door, the only
  charge a native block's *slice* can end on) and `BudgetProbe::spend` (the **pricing**, never-refusing
  primitive of the four BIF/call seams), plus the wrapper's own `wants_reduce`/`reduce`,
  its forwards of `resolve_external_call`/`resolve_local_fun`/`resolve_local_call` (the engine consults
  the wrapper, never the inner context, from its `call_ext*`, `call_fun`/`call_fun2` and
  `Call`/`CallOnly`/`CallLast` arms and the native baseline tier's `jit_local_call` probe) and the
  in-machine door pair `wants_bif_in_machine`/`call_bif_in_machine` to the inner `RuntimeContext`
  (`src/context/position.rs`).
* `tier.rs` → the tier mode + its `BEAM_RT_TIER` semantics (`mode_from_value`), the environment default
  and the selection (`select_entry_tier`).
* `reason.rs` / `perf.rs` → the exit-reason translation / the `+JPperf` code map.
* `tests.rs` → the unit + scheduled-path tests above.
