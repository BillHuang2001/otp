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
native baseline tier ([`beam_engine::Tier::BaselineJit`]) unless the mode pins the reference
interpreter ([`beam_engine::Tier::Interpreter`]) or the optional Cranelift hot tier
([`beam_engine::Tier::Cranelift`]), and the decision is **remembered on the engine**
(`ProcessEngine::tier`) and re-used by every later re-entry,
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
  **Term-word holders a rewriting pass must reach:** `ProcessEngine::args` (`entry.rs:126`),
  `ProcessEntry::Fun::fun` (`entry.rs:81`) + `target`'s `LocalFunTarget::args`, `RunnerState::results`/
  `exits` (`entry.rs:345`/`:347` — the words are on `Runtime::eval_heap`, not the arena) and
  `ProcessEngine::machine`'s root slices (`Machine::with_roots`'s `RootSet`); none has a `&mut` getter.
  `gather_off_heap_roots` (`entry.rs:490`) is the one `&mut` **value** walk — an
  `arena_args && !args_consumed` engine's args+fun, and an un-published parked machine's roots (`live`
  filters that family only) — and it never visits `results`/`exits`.
  The four **mutable** doors are its family-for-family siblings, handing `&mut [Eterm]` slices to a
  rewriting visitor (`FnMut(&mut [Eterm]) -> beam_common::Result<()>`, lengths fixed, order
  unspecified, an `f` error stopping that family): `for_each_engine_args_root_mut` (`entry.rs:545` —
  the same stability/consumption gates, `live` accepted-and-unused like the value gather, the
  `args`/`entry` borrow split by destructuring `ProcessEngine`), `for_each_parked_machine_root_mut`
  (`entry.rs:583` — the same `parked_published`/`live` gates and `fr_mark()` clamp, through
  `Machine::with_roots` + `live_x_mut()`/`stack`/`fr[..fr]`), and the **split** recorded-outcome pair
  `for_each_result_root_mut` / `for_each_exit_root_mut` (`entry.rs:629`/`:655` — the `results` and the
  `exits` words the value gather deliberately skips, one method each so the runtime-level walk can tag
  them `RootFamily::RunnerResult`/`RunnerExit` separately). `errors`' live reason word is **not**
  reachable: `BeamError` exposes no mutable term accessor.
* [`run`] — `RuntimeRunner`: the `ProcessRunner` implementation (one reduction slice) — the tier
  selection (once, at the first entry) and the tier-aware first-entry/resume execution, the
  parked-machine root hand-off (`park_window`, whose `ProcessEngine::parked_published` flag gates the
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
  `Runtime::stabilize_term_with`, the raise through `Runtime::stabilize_error_live_reason`. An
  off-heap binary (`bs_init_writable`/`bs_create_bin` above the on-heap limit) is therefore copied
  rather than refused with `badarg` (the return path) or kept un-copied against a dangling payload
  (the raise path) — whether the process **built** it or **received** it.
  The machine's `refc_binaries()` reservoir is **not** copied onto the running process: it is the one
  holder of every payload the process's own compiled bitstring code built, so
  `erlang:process_info(Pid, binary)` reads it straight through `RunnerState::reservoir_at(pid)` for
  **another** pid (and through the in-machine door's published roster for the **executing** one, the
  machine the pass holds out of `RunnerState` for the whole slice) and the
  process's own off-heap table stays empty. That is what keeps `beam_gc::OffHeap`'s never-compacted slot
  space out of the per-call path — mirroring the reservoir onto the PCB hands that space a slot per
  payload per slice, and the O(slots) `overhead_bytes`/`sweep` walks it implies make a build-a-binary
  loop quadratic in its own iteration count. The reservoir is **pruned, not accumulated**: the `Yield`
  arm reclaims every payload the parked process's roots no longer reach once its binary virtual heap
  triggers (see *The off-heap refc-binary reclamation at the slice boundary*).
  `Runtime::eval_off_heap` is likewise not attributed to any process: it is the pid-less
  `-eval` store.
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
  NIF library's stub, then `beam_bifs::serves_local_call` for `zlib`'s 19 stream keys). Every other
  method of the seam is forwarded unchanged, so the redirect into `inner` is purely mechanical.
* **The slice's real context is built once per `run` call (one scheduler slice).** [`run`] constructs
  `RuntimeContext::for_process_with_engines` once, then wraps it in one `BudgetContext` and hands that
  wrapper to the whole machine run — so every BIF/`call_ext`/`call_bif` a slice makes shares **one**
  `RuntimeContext` and therefore one `DispatchMemo` (the 8-slot per-context cache); the memo is rebuilt
  only at a slice boundary (a pre-emption/`receive` park ends the slice) or at the two nested-context
  sites (`FunBridge::nested`, `SendBridge`, both in `src/context/bridges.rs`). The `RuntimeContext`/
  `BudgetContext`/`BudgetProbe` construction is **stack-only** (no `Vec`/`String`/`Rc` on the hot path);
  the only per-slice heap work below the outcome match is the machine's own off-heap reservoir, which
  is left where it is (no registration, no slot handed out).
* [`tier`] — `JitMode` (`Auto` (**the `#[default]`**) / `Interpreter` / `Baseline` / `Cranelift`),
  `mode_from_value(Option<&str>)` (**the pure** `BEAM_RT_TIER` value semantics),
  `env_default()` (the memoized unset-variable default), `select_entry_tier(...)` — the single
  tier-decision seam — and the **startup capability check** `tier_unavailable(JitMode)` /
  `unbuilt_tier_request()` (`pub(crate)`, re-exported by [`super`]) that [`crate::cli`]'s `drive`
  calls once before it resolves or drives the boot script.
* [`reason`] — the exit-reason translation only (`normal` + `exit_reason`); the finite-`receive`
  deadline recovery lives in the engine (`Machine::wait_deadline`/`Machine::timeout_delay`), so no
  duplicated delay decoding sits here.
* [`perf`] — the `+JPperf` code-map feed (contract A.8).
* [`run`]'s `#[cfg(test)]` slice/park accounting — `RUN_SLICE_COUNTERS` (a per-thread `Cell<RunSliceCounters>`), read through `run_slice_counters()` and zeroed by `reset_run_slice_counters()`: `note_slice_entered()` counts one `RuntimeRunner::run` call (one reduction slice) and `note_slice_parked(preempted)` counts one slice that handed the machine back (the `Yield` pre-emption arm, the `Suspend` BIF-park arm, or the `receive` `Wait` arm). The increments are `#[cfg(test)]`-only, so a build without tests carries none of them.

## The `BEAM_RT_TIER` modes (`mode_from_value` is the single definition)
| value | mode | meaning |
|---|---|---|
| unset | `Auto` | **the default**: the native baseline tier for **every** process entry |
| `auto` | `Auto` | the default, named explicitly |
| `baseline` | `Baseline` | the same selection, forced explicitly (a measurement/debugging alias of the default) |
| `cranelift`, `jit-cranelift` | `Cranelift` | the optional Cranelift hot tier (`Tier::Cranelift`); selectable only in a build with the runtime's `cranelift` Cargo feature — see `## The Cranelift tier and its feature gate` |
| `interpreter`, anything else | `Interpreter` | the always-correct reference tier, and the **kill switch**; an unknown value resolves to it, never an error |

`JitMode`'s derived `Default` is `Auto`, so omitting the mode anywhere selects the native tier;
`env_default()` memoizes the environment resolution in a `OnceLock` (one `getenv` per process).
`jit-cranelift` is accepted because it is the spelling of [`Tier::Cranelift`](beam_engine::Tier::Cranelift)'s own `name()`, so a value copied from a tier report selects that tier back.
`hot` is deliberately not a value: the hot tier is named by what it is, not by how fast it should be.

## The Cranelift tier and its feature gate
The Cranelift backend is compiled in only behind the runtime's **`cranelift`** Cargo feature, which forwards `beam_engine/jit-cranelift`; it is **default OFF**, so a plain `cargo build` has no hot tier at all.
The **selection** is not feature-gated — `select_entry_tier` answers `Tier::Cranelift` for `JitMode::Cranelift` unconditionally — so the choice of tier never depends on the build; what depends on the build is whether that tier can be served, and that is decided at **two** places, in a fixed order.
1. **The startup rejection, owned by `beam_rt`'s CLI.** `tier_unavailable(mode)` answers the typed capability gap for a `Cranelift` request in a build whose `cranelift` feature is off (`None` for every other mode in every build, and `None` for `Cranelift` with the feature on), and `runner::unbuilt_tier_request()` is that check over the effective `BEAM_RT_TIER`. [`crate::cli`]'s `drive` calls it once, **before** the boot script is resolved or driven, so the invocation reports `beam_rt: unsupported: BEAM_RT_TIER=cranelift: this build has no cranelift execution tier (rebuild with the `cranelift` Cargo feature)` and exits `EXIT_UNSUPPORTED` (3) having done no boot work. It is a compile-time fact, so it never depends on the arguments, and `-version` is exempt (it runs no code, so it selects no tier).
2. **The engine's feature-gated entry points, the second line of defence.** They answer `Err(unsupported("jit-cranelift tier not built"))` at the moment a Cranelift entry is actually attempted. The two can never disagree because they read the **same** feature — and they are ordered this way round because without the startup check the boot processes are the first to try the tier, their per-process failures are re-wrapped by the boot drive, and the CLI prints the exit-1 `beam_rt: exit <structured reason>`, which names no feature and reads like a crash.
`beam_rt` therefore adds no second *feature* gate and no availability arm inside the selection.

A Cranelift process **resumes on its remembered tier**: `Machine::resume_outcome_with_tier` sends `Tier::Cranelift` to `jit::run_cranelift_resume_outcome`, which takes the artifact from the tier's retained store (`beam_engine/src/jit/cranelift/cache.rs`, keyed by `jit::cache::CacheKey`) and hands it to the shared native resume seam — no recompile, no reset, and the same labels the park was compiled with.
An address that is **not an enterable block start** advances by the driver's per-instruction interpreter step — the documented deopt, i.e. the interpreter's exact semantics: a `wait`/`wait_timeout` suspension pc, a mid-block reduction refusal, a suspended BIF's own instruction, and every instruction of a cross-module callee (a tier this narrow holds no blocks for the callee). That is never a wrong result and never a silent change of tier: the process keeps `ProcessEngine::tier` for its whole life.
**So a feature-ON CLI session completes under `BEAM_RT_TIER=cranelift`** — both a `run`/`-eval` action and the whole `-boot` boot script, whose own processes (`heart`, `logger_server`, `application_controller`, …) all park and resume — while a feature-OFF build refuses the request at startup as above.
The three-tier agreement (interpreter / baseline / Cranelift) is evidenced in process by [`tests`] — `fib_runs_to_the_same_result_on_all_three_tiers` for a first entry, `a_preempted_process_resumes_on_its_cranelift_tier` and `a_receive_parked_process_resumes_on_its_cranelift_tier` for the two suspension kinds — and at the CLI by [`tests/tier_selection.rs`](../../tests/tier_selection.rs).

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
`JitMode::Cranelift` selects its own tier (`Tier::Cranelift`) and changes nothing else about the decision; whether that tier can be **served** in the current build is the `cranelift` Cargo feature, read by the CLI-side startup check `tier_unavailable`/`unbuilt_tier_request` and, as the second line, by the engine's entry points — never by an availability arm inside the selection (see `## The Cranelift tier and its feature gate`).

## The tier is sound for any module
[`select_entry_tier`] picks `BaselineJit` from the mode alone: there is **no module the baseline tier
cannot run**, so there is no per-module predicate to consult. A `Step::Enter` — a step that continues
in *another* module's code — is the one shape a compiled block cannot take on its own (a compiled
block's code, label map and literal area are all bound to a single module), so the engine's driver
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
A suspended process's machine lives in [`RunnerState::engines`], but its registers are only a *copy* of the process's own root set.
A collection driven while the process is parked — `Process::deliver`'s exit cascade (`beam_process`) is **not** gated by `Runtime::gc_process_enabled` — relocates the process's heap against its own `regs`, so a parked machine that had not published its roots would be collected against a partial root set and left with a stale word in its `x`.
[`run`] therefore hands the machine over at **all three** park arms — the pre-emption/`Yield` arm, the `Ok(EngineRunOutcome::Wait)` arm and the `Ok(EngineRunOutcome::Suspend)` (BIF-park) arm — through the **one** entry point `park_window(table, pid, engine, fr_count, ParkExemption)`, right before `engines.insert`, and calls `resume_from_process_roots(ctx, pid, machine, fr_count)` after `ctx.enter_module(module)` when `engine.parked.is_some()`.
**The invariant is `skipped publication ⇒ the window holds the relocation exemption`**, and it is structurally impossible to split: `park_window` derives one `exempt` value from its `ParkExemption` argument, writes the flag from that value **in both directions** (so a publishing window can never inherit a stale `true`) and then delegates the publication to `park_machine(table, pid, engine, fr_count, exempt)`. A park arm therefore chooses a `ParkExemption` and cannot forget the flag — `Always` (the `receive` park, whose window a delivery or a signal really enters) or `LiveFloats` (`fr_count > 0`, the pre-emption and BIF-suspend parks, whose machine keeps those FR slots as roots of its own). The exemption has exactly **one writer** (`park_window`) and one clearer (`run`'s parked branch, `set_relocation_exempt(pid, false)`, which is where a parked window ends).
`park_machine` is the publication half only (`fn`, `park_window` its sole caller): it always records the live FR-prefix count on `ProcessEngine::fr_published`, but publishes only a window that is **not** relocation-exempt — an exempt window sets `ProcessEngine::parked_published = false` and returns `false` without publishing, while a non-exempt one sets `parked_published = true`, calls `publish_parked_roots` and returns `true`.
`parked_published` is therefore an **unconditional** claim, not `publish_roots_full`'s answer: `publish_parked_roots` (`run.rs:653-655`) discards the helper's `bool` and `park_machine` (`run.rs:745-746`) never reads one — sound only because both of `publish_roots_full`'s `false` arms are structurally unreachable today (`live = roots.x.len().min(MAX_REG)` cannot fail `set_live`, and the stack-length check cannot mismatch on the one append order), so any future change that can make it fail must gate `parked_published` on the answer instead, or the `Yield` arm's `collect_at_reds_boundary`/`reclaim_off_heap_binaries` would run against a **partially** published root set.
The `Yield` and `Suspend` arms pass `ParkExemption::LiveFloats` and the `Wait` arm passes `ParkExemption::Always`.
`resume_from_process_roots` therefore runs **only** when the matching park published (`engine.parked_published`), which is then cleared; the window's `set_relocation_exempt(pid, false)` is cleared unconditionally afterwards.
**The skip is sound because for the exempt window's whole duration nothing can relocate this process's heap**: `Process::collect`/`ensure_capacity`/`collect_if_needed` return the typed refusals, `ProcessTable::deliver` declines, the host collection gate's `declines_relocation` declines and `Runtime::serve_gc_request` returns `false` on `is_relocation_exempt`, and `Process::grow_nursery` — the one growth path that is deliberately not gated — relocates nothing.
So the parked machine stays the authoritative root set and the publish+resync pair would be a pure identity round-trip over a `min(x.len(), MAX_REG)`-word x file plus the Erlang stack, both ways (the rationale is on `park_machine` itself).

What the two calls publish and read back is the **shared** root-hand-off mechanism, not this node's: `publish_parked_roots` and `resume_from_process_roots` delegate to `context::gc`'s one implementation (`publish_roots_full`/`scatter_roots_full`, the `fr_count` parameter bounding how many leading FR words are published), which the in-slice heap-admission points (`ensure_heap_full`/`alloc_words_rooted`, bound by `machine_root_bounds`) also use — so the park/resume and the admission points can never drift (the mechanism's own copy: [`../context/IMPLEMENTATION.md`](../context/IMPLEMENTATION.md), [`../context/gc/CONTEXT.md`](../context/gc/CONTEXT.md) *A parked machine's live `Machine::pending_exception` reason IS a GC root across the park window*).
This node owns **which** arm publishes and **when** it may skip, and these are its facts about the two calls:
* `publish_parked_roots` hands the machine's roots over with `RootSet::ALL_LIVE` — the conservative (always-safe) choice, since a parked machine's live count is not exposed and over-approximating a copying collector's root set can only keep a term alive, never free a live one.
* **The FR file is published only up to its live leading prefix `fr_mark()`.** The runner reads the count **once** per slice (`Machine::fr_mark()`, the engine's exclusive high-water mark of FR writes since the machine's last reset) and reuses it for both the post-slice reaction and the `publish_parked_roots(.., fr_count)` call in the same slice; `fr_mark() > 0` also marks the process **relocation-exempt for the inter-slice window** (`park_window`'s `ParkExemption::LiveFloats`, cleared by `run`'s parked branch on resume), and that exemption is exactly what makes a `Yield` park with a live FR prefix **skip** its publication (`park_machine`'s `exempt` argument) instead of copying the file: the parked machine's own FR slots are protected from the delivery-driven collection — which is not gated by `gc_process_enabled` — by the exemption rather than by a copy.
* The reaction is **per slice, never a permanent pin**: inside a slice the engine reaches only the full door, which publishes the FR file up to its pointer-bearing prefix and applies no float guard, so a float-using call site collects exactly like a float-free one.
* `fr_mark` is an **exact upper bound** after a slice — every FR write goes through `Machine::set_fr`, and the baseline JIT tier refuses an FR destination (deopting to the interpreter) — and a bound can only err by over-protecting: a slot at or above it is `NIL`, and a `NIL` word roots nothing, so a float-free park publishes **no** FR word instead of the 1024 slots `process.regs` would carry.
* The count is stored on `ProcessEngine::fr_published` at the park and replayed at the resume, so `scatter_roots_full` never sees a length mismatch (which would halt the process's collection).
* `publish_parked_roots` **also** publishes the parked machine's live pending-exception reason (`Machine::pending_exception`) — the one term a parked machine holds that the closed `RootSet` shape cannot carry, because it is a machine *field*, not an `x`/stack/FR slot — as the process's `Registers::extra_roots` word (the host escape hatch `beam_process`'s `gather_roots`/`scatter_roots` relocate in place); with **no** live pending exception the extra-root set is left **empty** (cleared, never a NIL/placeholder), so a stale root can never keep an otherwise-dead term alive.
* **The word it publishes is live by construction, so it is published unfiltered.** `Machine::with_roots` parks the machine's loose heap terms — the two failed-clause argument latches, the pending exception's reason **and** the two supplied-trace term latches (`beam_engine/src/machine/mod.rs`, `const LOOSE_HEAP_TERMS = 5`) — at the tail of the Erlang stack for the closure's duration and re-seats every field from the slot the host wrote back, and that funnel sits behind `Machine::alloc_rooted`/`Machine::ensure_heap`, hence behind **every** in-slice collection door on every tier: a machine that has left the slice and parked here was never moved out from under the field.
  An extra root is a **relocation channel, not a liveness claim**, so the published word is always an immediate or a literal-area pointer (nothing a collection moves) or a container inside a region `pid`'s own collector relocates — never a word a collection would have to follow without being able to move it.
  Pinned by `beam_engine`'s `tests/raise_rooting.rs::pending_exception_reason_survives_a_collection` and by this node's `a_parked_machines_pending_exception_reason_survives_a_moving_collection`, `publishing_a_machine_without_a_pending_exception_leaves_no_extra_root` and `publishing_again_after_the_pending_exception_is_consumed_drops_the_stale_extra_root`.
* `resume_from_process_roots(ctx, pid, machine, fr_count)` is `RuntimeContext::resync_parked_roots`, called with the **same** `fr_count` the matching park used (`engine.fr_published`): it copies the process's authoritative root set back into the machine (the leading FR words only up to `fr_count`), reads the (already-relocated) extra-root reason out and writes it into `Machine::pending_exception_mut()`, **clears** the process's extra roots (the machine owns the relocated term now), and, if the slices no longer match, **halts the process's collection** (`Runtime::halt_process_gc`) rather than run against inconsistent roots.

The **`Wait` park arm** marks the process **relocation-exempt** for the whole parked window (`ProcessTable::set_relocation_exempt(pid, true)`), and the resume clears it, so a delivery- or signal-driven collection of a waiting (or just-woken, not-yet-resumed) process is declined and the delivery takes the ordinary heap-fragment fallback.
The **`Yield`** (pre-emption) arm is *not* exempt by the park itself, but the post-slice `fr_mark` check opens the same window exemption for **any** slice that ends with a live FR prefix, and the admission points inside a resumed slice are unaffected because the next slice entry clears the mark.
This is **additional** to the installed host collection gate (`../context/gc_walk.rs`'s `RuntimeCollectionGate`, installed on the runtime's table by `Runtime::new`): the gate declines only when an unscanned container is reachable from the receiver's **gathered** roots, while the exemption refuses *any* relocation for the window — covering a live term the host holds outside that gathered set, which the walk cannot see.
Both park arms call `Runtime::serve_gc_request` (the `erlang:garbage_collect/0,1` request, see the crate node): the `Yield` arm serves it when the slice ended with a dead FR prefix (`fr_mark() == 0`, so no exemption was opened) and it is declined by the exemption when the prefix was live, while the `Wait` arm's request is declined by the parked-window exemption.

The runner's raise arm **is the escape boundary for a raised error**: it stabilizes the `BeamError` itself — its structured reason term is a raw `Eterm` in the raise's live-reason slot, built on the dying process's heap or on the recyclable pooled BIF region — through `Runtime::stabilize_error_live_reason`, with the copy's `SourcePayloads` built by `Runtime::source_payloads` (the **same machine's** `refc_binaries()` unioned with the whole off-heap family the pid retained from received messages — refc binaries, MSO handles and node identities), so a reason that *is* an off-heap binary (`exit(<<big/binary>>)`) is resolved and shared instead of tripping the valve, whether the process built it or received it. The recorded **exit reason** is then derived from the stabilized error (`reason::exit_reason`: atom → the stabilized live term → interned `Display` text), so one copy serves both the exit propagation and every later rendering; a reason that genuinely cannot be copied is still the honest `error:system_limit`, never a stored dangling word. `Runtime::as_action_process` is the same boundary for the `-eval`/`-s`/`-run`/boot-directive routes.

## The off-heap refc-binary reclamation at the slice boundary
`run.rs`'s `Yield` arm is the **one** site that prunes the machine's payload reservoir: `RuntimeRunner::reclaim_off_heap_binaries` (`run.rs:456`) → `context::gc::reclaim`, called immediately after `collect_at_reds_boundary` and before `take_max_heap_kill` on the same park (`run.rs:1412`).
It is here because a prune is sound only against the process's **complete** root set, and the park is the one site holding `&mut Machine` (the runner just drove it), `&mut Process` (through the table) and `&mut Runtime` at once; a heap-admission door runs *inside* a slice, where the engine is held out of `RunnerState::engines` and the `ExecutionContext` seam exposes no `&mut Machine` at all.
`reclaim_off_heap_binaries` refuses unless `engine.parked_published` — the park's own contract that the machine's whole root set was handed to the process.
`Process::has_unpublished_roots()` is deliberately **not** the gate: it is `true` for a `Running` process, which a pre-empted process still is.
The state it needs is per process engine (`ProcessEngine::off_heap`, built lazily from `process.collector().policy()` so a `+hms`/`+hbm*` policy is honoured and dead with the machine), so one call reaches `&mut machine` and `&mut off_heap` without a second table.
The gates themselves — relocation exemption, `gc_process_enabled`, the bin-vheap trigger, the collection valve, the prune-only valve, the reachability walk — and the release predicate are [`../context/CONTEXT.md`](../context/CONTEXT.md) *The off-heap refc-binary reclamation*; every one of them is a decline, so this call never fails a program.
Per-slice cost on a non-triggered bin vheap is **one cached observation**: `OffHeapReclaimer::observe` returns the recorded overhead while the reservoir's handle count has not moved, so `Machine::off_heap_overhead_bytes` (`O(reservoir)`) is read only when it moved or once per triggered prune.
**Residuals — each a leak, never a dangle.** The `Wait` arm does not prune: a `receive` park is relocation-exempt and publishes nothing, so the machine stays the authoritative root set and the process's roots would be stale by construction — a process that blocks forever keeps its reservoir until its machine drops, and one that wakes and yields again prunes then.
A declined prune retains the whole reservoir.
The `-eval`/single-pass route never prunes **this reservoir** at all (its machine is not in `RunnerState` and allocates on the pointer-stable arena, so its park publishes nothing); the store it *does* prune is the separate escaped-payload store `Runtime::eval_off_heap`, swept at the evaluator's depth-0 statement boundaries and at the nested-run epilogue (`../context/CONTEXT.md`).
The cadence is the **pre-emption** cadence: every slice this arm ends prunes, so a build-a-binary loop whose payloads land in the machine's reservoir is bounded by the bin-vheap budget even when it never reaches a `receive`.
The per-fixture envelope (one surviving payload, no growth in N) is measured on [`../../tests/off_heap_reclaim.rs`](../../tests/off_heap_reclaim.rs)'s direct-recursion shape — figures in [`MEASURED.md`](MEASURED.md) *The off-heap prune's per-fixture envelope*.
**A fun-loop body is covered by the OTHER prune, not this one.** `lists:foldl/3`/`lists:foreach/2` is entered as a *nested* external call, and each application of the lambda is served by `RuntimeContext::apply_fun` → `run_label_blocking`'s own short-lived `Machine` (`context/pump.rs`); when that machine drops its reservoir is handed to the runtime's **escaped-payload store** (`Runtime::eval_off_heap`), never to the process's `Machine::refc_binaries` — so this arm cannot bound it, and the nested-run epilogue does: `RuntimeContext::adopt_nested_payloads` publishes the finished machine's own root set and prunes that store against it (`Runtime::prune_off_heap`, `O(1)`-gated), releasing only the handles the run itself adopted.
Figures at both payload sizes, and the linear pre-prune counterpart: [`MEASURED.md`](MEASURED.md) *The off-heap prune's per-fixture envelope* — flat in N, with the process's own reservoir at `0` handles throughout and its `erlang:statistics(reductions)` delta `0` (it neither pre-empts nor parks).
The `-eval` route's escaped store is pruned at the evaluator's depth-0 statement boundaries (`src/eval/reclaim.rs`), so that route is bounded.
What is measured, and where the figures live: `erlang:memory(binary)` (`binary_alloc`'s live bytes) and the process's RSS across a build-a-binary loop, against the oracle's — the measured envelope and its caveats are [`../../CONTEXT.md`](../../CONTEXT.md) *Known Issues / Intentional M1 gaps*'s off-heap bullet, whose suite is [`../../tests/off_heap_reclaim.rs`](../../tests/off_heap_reclaim.rs) (the bench driver's [`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md) carries the loop workloads' RSS).
The end-to-end effect on a real workload — `emqx_base62:encode/1` on the unmodified EMQX module, against the oracle and against the same module before the reclamation — is [`MEASURED.md`](MEASURED.md) *The off-heap reclamation end to end*.

## Per-slice (not per-instruction) host cost
Beyond the `BudgetContext` charges (a per-instruction `reduce` on the interpreter tier, the two
`ensure_heap` doors, and a per-BIF/per-call `spend` on the native tier), one slice's prologue/epilogue
does this host work (each is amortized over the whole 4000-reduction budget, so it is per-`run`, never
per-iteration):
* **O(1), no allocation:** `RunnerState::engines.remove(&pid)` / `insert` (`run.rs:896`/`:1340`), the
  FR prefix read `Machine::fr_mark()` (`run.rs:1296`), the tier lookup (`run.rs:430-438`),
  `note_reductions` (`run.rs:1219`), `serve_gc_request` (early-returns without a pending request),
  and — on the `Yield` arm only — `current_code()` (`run.rs:1329`) and
  `reset_reductions`/`bump_reductions` (`run.rs:1348-1349`).
* **One allocation per slice:** `RuntimeContext::enter_module` (`run.rs:475` →
  `../context/runtime_context.rs:501-508`) builds a fresh `String` of the module name on **every**
  slice (a `RuntimeContext` itself is a stack value: `for_process_with_engines`, no heap alloc).
* **O(size), on a park whose window is NOT relocation-exempt:** `park_window` → `park_machine` (`run.rs:959`,
  `run.rs:1084`) calls `publish_parked_roots` → `gc::publish_roots_full`
  (`../context/gc.rs:799-824`), which copies the whole x file
  (`min(x.len(), MAX_REG)` = up to 1024 words, `beam_process::registers::MAX_REG`) plus the whole
  Erlang stack plus the live FR prefix onto the process's root set. An exempt window — the `Wait`
  arm, and a `Yield` arm whose slice ended with a live FR prefix — skips the copy entirely.
* **Amortized O(1):** the off-heap trigger — `Machine::off_heap_overhead_bytes` is `O(reservoir)` and is
  read only when the reservoir's handle count moved, or once per triggered prune, so a slice that built
  no off-heap payload pays one cached observation; the payloads themselves need no per-slice work at all,
  since no slot is ever handed out on the process's own (never-compacted) off-heap table, and the
  reservoir is pruned only on a triggered `Yield` park (see *The off-heap refc-binary reclamation at the
  slice boundary*).
* **First entry / flag-gated only:** `record_baseline_code` (`run.rs:1300-1302`, `+JPperf` only), the
  `Arc::clone` of the entry module (`run.rs:935`), and `describe_site` (debug-trace only).
  `source_payloads` (`run.rs:1455`/`:1674`) runs only on the terminating `Returned`/`Err` arms.
* **No production time read:** `Instant` appears only in `runner/tests.rs`; the slice path does no
  `sys` call of its own.

## Measured
The tier is taken on a process's **first** entry and then carried for its whole life, and the observable consequences are pinned structurally rather than timed: a pre-empted slice and a `receive` park both resume through `Machine::resume_outcome_with_tier` on the tier the process was selected for (`a_preempted_process_resumes_on_its_baseline_tier` / `a_receive_parked_process_resumes_on_its_baseline_tier`), an interpreter-tier process never changes through a resume (`an_interpreter_tier_process_is_unchanged_across_a_resume`), and a native block's slice charge is exact ([`tests/slice_charging.rs`](tests/slice_charging.rs)'s `the_slice_budget_prices_native_work_and_a_refusing_seam_still_ends_the_slice`).
`BEAM_RT_TIER=interpreter` is the always-correct reference tier and the **only** kill switch: the default (`Auto`, and its `baseline` alias) puts every process on the native tier, and `cranelift`/`jit-cranelift` names the optional hot tier without changing the default.
Per-workload speed and memory figures are **measured, never modelled**; the full cross-runtime record lives in [`../../PERF.md`](../../PERF.md) and the bench driver's [`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md).
Headline rust/sys ratios on this runtime's default baseline-JIT tier against OTP 29.1.1 / erts-17.1 — each with its own estimator, loop count and caveats in [`MEASURED.md`](MEASURED.md):

| workload | rust/sys |
|---|---|
| `send_recv_plain` (delivery only) | **2.92×** |
| `send_recv` (+ the ETF codec in the worker) | **5.11×** |
| `ets_copy` | **10.8×** |
| `etf_roundtrip` | **2.21×** |
| `foreign` | **38.3×** (`bstr_to_lower`), **48.1×** (`bstr_capitalize_token`), **51.0×** (`emqx_base62_encode`), **35.8×** (`emqx_base62_decode`) |
| `emqx_base62:encode/1`, end to end | **27.4×**, with `erlang:memory(binary)` and `VmHWM` flat in N |

- **The growth shapes' per-call work is flat in N, on deterministic counters.** `churn` collects 2.362819 / 2.375462 / 2.376366 times per call (4 728 / 23 757 / 95 057 collections, 325 / 1 617 / 6 465 majors) at `reductions`/call 1 808.98–1 809.84 for N = 2 000 / 10 000 / 40 000; `heap_growth` satisfies `collections == 1 + N / K` exactly, with `K == 2` iterations carried by its settled 843-word nursery; both hold one young space at every N, on a capacity that does not move with N (2 585 words for `churn`, 843 for `heap_growth`).
- **The boundary door's rationing is pinned; its marginal cost is not.** It takes one `churn` boundary in 8.5–8.6 (105 / 210 / 532 of the 904 / 1 809 / 4 524 checks handed to it at N = 2 000 / 4 000 / 10 000), a **constant** 2.22 / 2.22 / 2.24 % of the run's own 4 728 / 9 479 / 23 757 collections, and a shape it is handed ≤1 boundary on per run cannot take a door collection at all; the ON-vs-OFF `Δ` needs a door-disabled build, so none is claimed.
- **The `run` route never sleeps while it has work**: 0 `clock_nanosleep` on a 200 000-round two-process ping-pong, and still ≥1 for a parked `receive … after T`.
- **The native tier's slice charge is exact**: `reductions == 5N + 9` with a **4 006** ceiling, and a **refusing** seam — not the pricing one — is what ends a slice.
Everything behind those lines — the per-workload tables and their loop counts, the per-iteration collection/major rates and nursery geometry, the boundary door's rationing and the `Δ` it does not claim, the `foreign` literal-area-tagging mechanism, the slice budget's full charge table, the off-heap prune's per-fixture envelope and the `emqx_base62:encode/1` end-to-end record — is [`MEASURED.md`](MEASURED.md).
A matched-N `ns_per_call` comparison against a runtime that never collects is not like-for-like on an allocation-shaped loop — see *Known Issues / Handoffs* and [`MEASURED.md`](MEASURED.md).

## Constraints
* **`#![deny(unsafe_code)]`**; no `unwrap()` on runtime data; typed `beam_common::Result` errors.
* The tier is resolved **before** the `RuntimeContext` borrow (the selection happens in the slice's
  prologue, before the context takes `Runtime` mutably).
* A test forces a tier by setting `state.jit_mode = Some(..)` — never `std::env::set_var` (edition 2024
  makes it `unsafe`). `BEAM_RT_TIER` is the CLI-facing equivalent, and its semantics are pinned through
  the pure `mode_from_value` (not through the environment).
* `JitMode::Baseline` is an alias of the default selection (`Tier::BaselineJit`); only
  `JitMode::Interpreter` turns the native tier off, and `JitMode::Cranelift` is the third selection
  (whether that tier can be served is the `cranelift` Cargo feature, read by the CLI-side startup
  check `tier_unavailable` — never a mode, and never an arm inside the selection).

## Test Strategy
* `#[cfg(test)]` units in [`tests`]: `select_entry_tier` over the real tier fixtures — the default
  (`Auto`) and the explicit `Baseline` alias both select `BaselineJit` for **every** entry, including a
  cross-module-calling module (`stdlib_more_harness`) and a dynamic-`apply` one (`funs`), and an
  unresolved **fun** entry takes the same tier as an export entry; only `Interpreter` selects
  `Tier::Interpreter`. `the_unset_default_resolves_to_auto` pins the `BEAM_RT_TIER` value semantics and
  `JitMode::default()` through the pure `mode_from_value`.
* The third tier's rows of the same suite pin its contract: the pure resolution — `cranelift` and
  `jit-cranelift` both `Cranelift`, the four named values and the **unknown-value rule** (an
  unrecognised string is `Interpreter`, never an error and never the hot tier) — `select_entry_tier`
  answering `Tier::Cranelift` over the same real tier fixtures the baseline rows use, and the
  **feature-off typed gap** (the entry fails with the engine's
  `unsupported: opcode: jit-cranelift tier not built`, never a substituted tier); on a `cranelift`
  build a **three-tier first-entry agreement** runs one entry on `Interpreter`/`BaselineJit`/`Cranelift`
  and asserts the same answer on all three. The `cranelift` Cargo feature is not required by the crate
  gate, so the feature-off row is the one that always runs.
  `tier_unavailable_refuses_only_the_unbuilt_cranelift_tier` pins the **startup check** in both
  builds: the feature-off build gets the typed gap whose reason names `cranelift`, `BEAM_RT_TIER` and
  the Cargo feature to rebuild with, the feature-on build gets `None`, and in **both** builds every
  served mode (`Auto`/`Baseline`/`Interpreter`) is `None` — the property that keeps the CLI check
  from ever refusing a working invocation.
* [`tests/tier_selection.rs`](../../tests/tier_selection.rs) is the **CLI-level** suite for the same
  surface: real `beam_rt` children through [`support::run_bounded_with_env`](../../tests/support/mod.rs),
  with `BEAM_RT_TIER` set on the child (or **removed** for the unset row) so the test process's own
  environment is never mutated. Its rows: a feature-off `cranelift` request on
  `run slice.beam slice:fib/1 10` exits **3** with empty stdout and one `beam_rt: unsupported: …`
  line naming the feature; `-version` under the same setting still exits **0** with the banner; and
  unset/`auto`/`baseline`/`interpreter` still print `55` and exit 0 (the negative control — the check
  must fire on nothing else). A feature-on row asserts the startup check stays silent there.
* **The suites that enumerate tiers as strings enumerate exactly two, deliberately.** `tests/nif.rs`'s
  `TIERS`, `tests/zlib_{guards,one_shot,stream_levels,stream_dict}.rs`'s `Tier` enums,
  `tests/decoded_fun_badfun.rs`'s `TIERS` and `tests/compile_file.rs`'s tier loop all pin the
  **default** and the **kill switch**, and the third tier needs the `cranelift` Cargo feature, which
  those suites must not require; each one sets or **removes** `BEAM_RT_TIER` in the child explicitly, so
  an ambient value cannot reach them.* Tier persistence across a resume, proven through the per-thread **baseline compiled-block cache**
  (`beam_engine::jit::cache::{baseline_cache_len, clear_baseline_cache}`) — empty the cache between the
  park and the resume, so a resume that came back on the baseline tier is the only thing that can
  repopulate it: `a_preempted_process_resumes_on_its_baseline_tier` (`preempt:alloc_loop/1` under a tiny
  budget → `Yield`, then the resume completes the loop on the remembered tier),
  `a_receive_parked_process_resumes_on_its_baseline_tier` (`recv_after:wait_forever/0` → `Wait`, then a
  `ProcessTable::deliver_simple` message wakes it and the resume takes it on the remembered tier), and
  `an_interpreter_tier_process_is_unchanged_across_a_resume` (same loop on `JitMode::Interpreter` records
  the same result and compiles nothing on either slice). Each asserts `ProcessEngine::tier` as well.
  **The hot tier's two resume rows** (`#[cfg(feature = "cranelift")]`) cover the same two suspension
  kinds on `JitMode::Cranelift`: `a_preempted_process_resumes_on_its_cranelift_tier` (the loop parks on a
  tiny budget, re-parks on a second tiny slice — the "never silently downgraded" observation — then
  completes, and its recorded list is compared **element by element by value** with an
  interpreter-tier reference run of the same fixture on the same schedule, since two runs live on two
  `Runtime` stores) and `a_receive_parked_process_resumes_on_its_cranelift_tier` (`recv_after:wait_forever/0`
  → `Wait`, `ProcessTable::deliver_simple`, resume takes `42`). The baseline rows discriminate through the
  compiled-block cache; this tier's artifact-store observables are `#[cfg(test)] pub(super)` inside
  `beam_engine`, so the hot tier's evidence is the tier remembered on `ProcessEngine::tier` across the
  park, the **absence of the typed resume gap** (`take_error` is `None` and a value is recorded), and the
  value agreeing with the interpreter.
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
  and the residual tuple `(slices, yields, waits, collections) == (2, 1, 0, 1)` on one young space, with
  `major` deliberately **unpinned** (`0` on the native rows, `1` on the interpreter control: the door asks
  for a young-only minor and escalates only when the promotion cannot fit the mature region, so the kind of
  that one firing is a consequence of the mature generation's sizing, not a property of the door). It
  asserts the native baseline tier (and fails loudly if `BEAM_RT_TIER` pins any other
  mode — the interpreter, or the cranelift hot tier), never a wall clock.
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
  The off-heap prune's own suite lives with its module ([`../context/gc/bin_vheap.rs`](../context/gc/bin_vheap.rs)); what this node must keep true around it is the park contract — a non-publishing window is refused before any work, and the call sits after the boundary collection and before the heap-kill drain.

## A process's own signal work before it sleeps
* **A signal a process queues on ITSELF is delivered before its next schedule-in — two mechanisms, because the C has both.**
  **The in-scan fetch** ([`../context/execution.rs`](../context/execution.rs) `receive_next`, the C `i_loop_rec` → `erts_proc_sig_receive_helper` → `erts_proc_sig_fetch`, `../../erts/emulator/beam/emu/msg_instrs.tab:80-112`, `../../erts/emulator/beam/erl_proc_sig_queue.c:7729`, contract comment `:7739-7759`, splice + re-peek `:7771-7799`): an exhausted scan with a non-empty signal queue applies its **receive-applicable prefix** through the public `ProcessTable::service_receive_signals` and re-reads the scan, so a trapped `{'EXIT', …}` and an awaited `'DOWN'` both cost no extra scheduling round.
  That drain is strictly FIFO and stops *before* the head it cannot apply: a trapped exit (`{'EXIT', From, Reason}`) and a monitor DOWN (`{'DOWN', …}`) are delivered, an `Ignore` head (a `normal` exit to a non-trapping receiver) is discarded, and the walk halts at the first head whose `exit_policy` is `Terminate`, leaving it and everything behind it queued.
  The halt is what keeps the drain non-fatal from **inside** the machine's instruction stream: a `Terminate(_)` verdict has no channel there to report a death or its propagation, so the terminating classes are left to the net below rather than duplicating `exit_policy` here.
  **The schedule-out net** ([`run.rs`](run.rs) `run`'s `EngineRunOutcome::Wait` arm, the C `schedule_out_process` / `active_sys_enqueue`, `../../erts/emulator/beam/erl_process.c:6777-6801` and `:7027-7094`, flag `erts_proc_notify_new_sig`, `../../erts/emulator/beam/erl_proc_sig_queue.h:2062-2075`): after every park bookkeeping step, a process whose `pending_signals` are non-zero — i.e. one whose queue still holds a `Terminate` head the scan could not apply — returns `RunOutcome::Yield` instead of `Wait`, so the scheduler's `slice_end` `Yield` arm (`../../beam_process/src/scheduler.rs:745-759`) re-enqueues it and `slice_begin`'s `service_signals` applies the queue at its next entry — the C's re-enqueue-instead-of-sleep, which is also the only site that may terminate it.
  Three invariants hold: the pid is put back on its run queue by exactly the arm that owns the `Runnable` transition (no double scheduling, no lost wakeup); the queue drains monotonically (every applied signal is popped, every fatal one terminates the pid at the next schedule-in), so a process cannot spin — one re-entry either matches the materialised message or parks again with an empty queue; and the `Waiting` + relocation-exempt window set by the park is overwritten to `Runnable` with no relocation site able to move the heap in between, so a delivered message takes the documented heap-fragment fallback at worst.
  Pinned end to end by `tests/signal_self_delivery.rs` over the committed `tests/fixtures/sigself.erl`: all eight shapes (`killed`, `natural`, `killed_after`, `ran_before_kill`, `spawn_monitor_kill`, `trapping_link`, `bare_receive` with no `after` clause, and `not_matching`) print the live oracle's own line with exit 0, individually and as `all/0`'s list.
  The residual is scheduling **granularity**, not the answer: the drain applies the whole receive-applicable prefix in one call where the C's walk is reduction-bounded (`erts_proc_sig_handle_incoming`'s `limit`/`abs_lim`), so pre-emption inside a long signal burst is the engine's own reduction budget at the caller; no observable term or order changes, only where a yield lands (`ProcessTable::service_receive_signals`'s own documented approximation).

## Known Issues / Handoffs
* **An early return out of `run` between the engine take-out and a re-insert drops that process's machine.** The take-out is `run.rs:896`; the `Yield` arm's `current_code() == None` return (`run.rs:1329`) and the two `?`s at `run.rs:1313`/`:1321` all return while `engine` is still a local. The pid stays on the run queue, so the next slice answers `refuse_engine_less` → `Wait` with the state restored to `Runnable` (`run.rs:313-334`) and nothing ever runs it again — and the machine, the only holder of that process's live x/stack while it is parked, is gone. Minimal fix: re-insert the engine before returning the error.
* **`RunnerState`'s per-pid maps are never pruned on process death.** The only `engines.remove` is the slice take-out (`run.rs:896`), so a terminated parked process keeps its machine (and that machine's refc-binary reservoir) for the runtime's life; `gather_off_heap_roots` gates the dead machine's dangling root set on `live(pid)` (`entry.rs:506`), so the residue is a leak, never a read.
* **A matched-N `ns_per_call` comparison against a runtime that never collects is not like-for-like on an allocation-shaped loop.** Such a runtime runs **zero** collections (the admission declines and the nursery grows instead) where this one runs 4 728 collections and 325 majors on the same 2 001-call `churn` run (**2.3628** collections and **0.1624** majors per call, at the same per-call rate at N = 40 000: 95 057 / 6 465 over 40 001 calls = **2.3764** / **0.1616**), so the difference is the memory management each performs and the heap the other retains, not dispatch or tier.
  The figures behind that, and the dated 1.25×–1.60× band the allocation-shaped `foreign` rows once read, are [`MEASURED.md`](MEASURED.md) *Not like-for-like*.
* **A natively executed compiled block is charged at a deopt, a heap-admission point and each **priced**
  BIF/call seam — but only a *refusing* seam can end its slice.** The per-instruction charge belongs to
  the interpreter's loop, so what a native block's budget drains is [`budget`]'s `spend`
  (`call_bif`/`call_bif_in_machine` after the dispatch, `resolve_external_call`/`resolve_local_call`
  before the forward) plus the two `ensure_heap` doors and any deopt.
  **Residual**: a **non-allocating BIF-and-call-only loop still ends no slice of its own** on the native
  tier — `spend` never refuses and is a silent no-op once the budget is gone, so the *single* `Yield`
  such a row reads is the run's **epilogue** seam and the boundary collection door stays unreachable for
  the loop. The memory half of that residual is served: that one `Yield` does collect, so
  `heap_size` sits on the collector's floor (233) instead of tracking N.
  The exact charge (`reductions == 5N + 9` with its **4 006** ceiling), the allocating shape's
  slices/yields/collections table, the no-epilogue zero-slice row and the interpreter-tier control are
  [`MEASURED.md`](MEASURED.md) *The slice budget's charge on the native tier*.
  A **refusing** charge cannot be taken at a call seam today: the native tier publishes
  `machine.current_code` at **block** granularity (`crates/beam_engine/src/jit/baseline/runtime.rs:993`)
  and its call paths consult the seam before storing any position of their own
  (`jit/baseline/call.rs:210` vs `:232`; `runtime.rs:948`; `runtime.rs:818`), so a refusal parks at
  block entry and the resume re-executes the block prefix.
  **Handoff (out of this crate)**: `beam_engine` must store the instruction address
  (`machine.set_current_code(func, pc)`) before consulting the seam at those three sites, after which a
  call seam may refuse and the third pin of [`tests/slice_charging.rs`](tests/slice_charging.rs) is the
  assertion that must change. That suite holds the three pins (the exact charge, the bounded
  iterations-per-slice pre-emption, and the residual tuple plus its interpreter control) over the
  committed `tests/fixtures/native_reds.{erl,beam}` fixture.

* **An uncaught raise's stacktrace is never rendered on the compiled route.** The runner flattens a raise
  to a reason **term** ([`mod.rs::reason::exit_reason`](reason.rs): atom → live term → `Display` text) and
  the pump hands the original `BeamError` back to the CLI, which prints **one** line —
  `beam_rt: <class> <reason>` (`beam_common`'s `Display for Raise`, with `cli.rs::reported_reason` swapping
  in the `~w` rendering of a live structured reason on every route: `run`, `-boot`, `-eval`, `-s`
  and `-run`).
  The term it renders is safe because the raise was **stabilized at the escape boundary** — the
  runner's raise arm (`Runtime::stabilize_error_live_reason`) for a scheduled process,
  `Runtime::as_action_process` for the action routes — before the owning heap was terminated or
  rewound.
  There is no `** exception error:` / `=ERROR REPORT` crash-report renderer in this crate (the one ERTS
  report text is the `max_heap_size` block, `../context/gc.rs`), so `Raise::stacktrace` — the symbolic
  frames the engine attaches when a raise escapes a machine — has **no consumer** here: its only readers
  are the `-eval` catch fold (`../eval/mod.rs::cook_supplied_trace`) and
  `process_info(_, current_stacktrace)` (`../native/memory/`, from the live machine, not from a raise).

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
| `../context/gc.rs:1273` `ensure_heap_full` | `Instr::TestHeap`/`AllocateHeap` → `Machine::ensure_heap` (`beam_engine/src/machine/frames.rs:283-289`) + the baseline-JIT twins | `x[..pointer_prefix_len(x)]`, whole stack, `fr[..pointer_prefix_len(fr)]` (`gc.rs:1307`, rebuild `:1382-1383`) | `armed` + the unscanned-root valve |
| `../context/gc.rs:1648` `alloc_words_rooted` | `terms/slack.rs:170` (`terms::reserve`, i.e. every `*_rooted` builder, per built term) | same narrowing (`gc.rs:1761`) | `armed` + valve |
| `../context/gc.rs:1055` `ensure_heap` (narrow) | **no engine call site** (`gc.rs:1895`) | `x[..live]` + stack, **no FR file**; float-guarded | `armed` |
| `../context/gc/reds_door.rs:176` `collect_at_reds_boundary` | `run.rs:1393` (the `Yield` arm only) | `RootSet::ALL_LIVE`: **all 1024 x slots** + whole stack + `fr[..fr_count]` (`reds_door.rs:279` → `gc.rs:786`) | `run.rs:337 parked_published`, `gc_process_enabled`, 4 000-red interval, `is_relocation_exempt`, 7/8 occupancy or `collection_debt`, valve |
| `../context/gc.rs:1536` `alloc_words` | anywhere | **never collects** (grow + retry, else arena) | — |
The slice-boundary off-heap prune is **not** a collection site and gets no row: it relocates nothing and publishes nothing of its own, it runs immediately after `collect_at_reds_boundary` on the same `Yield` park, and it prunes the machine's payload reservoir rather than a heap — see *The off-heap refc-binary reclamation at the slice boundary*.
Out of slice, `ProcessTable::deliver`/`enqueue_exit_signal`/`grow_receiver_for_signal` relocate a
**non-running** receiver only (`beam_process/src/ptab.rs:197`, `:243`, `:2661`, `:2715` all refuse on
`has_unpublished_roots()`, which is `relocation_exempt || Running`), so they cannot touch a running
process's heap.
**Publication is airtight for `x`/`stack`/`fr`; the open root-set question is a holder that is in NO
root set.** The park publishes all 1024 x slots (`gc.rs:802` sets `live = min(x.len(), MAX_REG)`), so
`XRegs::gather_roots` (`beam_process/src/registers.rs:250-256`, which reads `x[0..live)` only)
gathers every published slot, and no production site shrinks `live`. `is_pointer_bearing` is
`(word & 3) != 3`, i.e. exactly "not `TAG_PRIMARY_IMMED1`" (`beam_term/src/tag.rs:37-45`,
`boxed.rs:73-74`, `list.rs:68`), so an atom/small/NIL can never be bound away by
`pointer_prefix_len`. `Machine::pending_exception`'s reason is covered on **both** sides of a park: in
slice by `Machine::with_roots`' loose-heap-term parking (`beam_engine/src/machine/mod.rs:867-1006`) and
across one as `Registers::extra_roots` (`run.rs:656-662`). What is NOT covered by any root set: the
marshalled BIF `args` copy (`beam_engine/src/interpreter/bif.rs:103-133`, `:165-187`) at the `&self`
door `Machine::dispatch_bif_call` (`bif.rs:324`) which has no write-back channel, and the
`Runtime`-owned term stores (`Runtime::in_machine`, `timers`' reference key, `references`, `mailbox`
markers, `trace`/`seq_trace` specs, `eval_closures`).
**Two hand-off asymmetries a dangling-pointer hunt must know.** (a) `publish_parked_roots` discards
`publish_roots_full`'s answer (`run.rs:653-655`) while `park_machine` claims `parked_published = true`
unconditionally (`run.rs:745`), so a failed publication would leave a **stale** published set behind a
window a collection may still move; unreachable today only because `XRegs::set_live` errs on `n > MAX_REG`
alone (`beam_process/src/registers.rs:77-83`) and `publish_roots_full` clamps to it (`gc.rs:802`).
(b) A failed **scatter** halts collection *after* the collection already moved the heap (`gc.rs:1425-1427`,
`../context/runtime_context.rs:515-517`), which is not the "unmoved heap"
[`Runtime::halt_process_gc`](../runtime/gc.rs:801-809) documents: the parked machine keeps its from-space
words and the process keeps running on them, so the failure surfaces later as a dangling term read rather
than a typed error.
The resume's re-derivation is self-consistent only because nothing on the host side touches the machine
between park and resume: `scatter_roots_full` recomputes the expected width as `machine.stack.len() +
armed loose terms` (`gc.rs:856`), and that count can only differ from the park's if the machine's stack or
its loose-term latches changed while parked.

## Routing Table
* `mod.rs` → module docs, sub-module wiring, root re-exports (`ProcessEntry`, `RunnerState`,
  `RuntimeRunner`).
* `entry.rs` → `ProcessEntry`, `ProcessEngine` (incl. the remembered `tier`, the park's published
  FR-prefix count `fr_published`, the park's publish/skip flag `parked_published`, the per-process
  binary virtual heap `off_heap: Option<OffHeapReclaimer>` and the `arena_args` stability flag) +
  `RunnerState::gather_off_heap_roots`, whose **two** sub-families are an `arena_args` engine's
  **un-consumed** arguments (plus a `ProcessEntry::Fun` entry's fun term, which stays live for a fun
  entry with no local target because that entry re-applies them on every run) and an **un-published**
  parked machine's own root set for a pid still in the process table, and which is **empty during a
  pass** (`RunnerState` is `mem::take`n out of the runtime for the pass's duration) + its four
  **mutable** siblings (`for_each_engine_args_root_mut`, `for_each_parked_machine_root_mut`,
  `for_each_result_root_mut`, `for_each_exit_root_mut` — family-for-family, handing `&mut [Eterm]`
  slices to a rewriting visitor; the last two cover `results`/`exits`, which the value gather skips,
  and are split so the runtime-level walk tags them as two families) + `RunnerState`
  (the `jit_mode` field, the `jit_mode()` accessor and the `reservoir_at(pid)` read-only off-heap
  roster seam).
* `run.rs` → `RuntimeRunner`: the (first-entry) tier selection and the tier-aware resume, the
  parked-machine root hand-off (`park_window` → `park_machine` → `publish_parked_roots` /
  `resume_from_process_roots`; `ParkExemption` makes the exemption and the publication one decision),
  whose `fr_count` argument is the live prefix `Machine::fr_mark()` — the value that also opens the
  inter-slice window's relocation exemption, whose presence makes `park_machine` skip the publication),
  the
  return-value / exit-reason stabilization (whose `SourcePayloads` come from
  `Runtime::source_payloads` — the machine's handles unioned with the pid's retained mailbox
  off-heap families), and the `Yield` arm's slice-boundary off-heap prune
  (`reclaim_off_heap_binaries`).
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
* [`MEASURED.md`](MEASURED.md) → **sibling doc, not code**: the measured workload record behind `## Measured` — the `copy_bound` delivery table, the `run` route's sleep accounting, the growth shapes' per-call counters and nursery geometry, the boundary door's rationing and the `Δ` it does not claim, the native tier's exact slice charge, the `foreign` per-call table with the literal-area-tagging mechanism, the off-heap prune's per-fixture envelope and the `emqx_base62:encode/1` end-to-end record.
  Crate-level figures live in [`../../PERF.md`](../../PERF.md), dated snapshots in [`../../PERF_HISTORY.md`](../../PERF_HISTORY.md), the per-call overhead harnesses in [`../../TRACE_AND_PERF.md`](../../TRACE_AND_PERF.md), and the bench driver's record in [`../../../../tools/bench/RESULTS.md`](../../../../tools/bench/RESULTS.md).
