# `beam_rt::eval::scratch` — the `-eval` statement window on the runtime term store (`../scratch.rs` · `tests.rs`)
## Intent
The window that bounds `Runtime::eval_heap`'s growth to **one top-level statement**: at the boundary *before* each
statement, the current region's dead `[mark, used)` prefix is rewound once a reachability descent proves no live
term stands inside it.
The directory holds that mechanism's behaviour pins (`tests.rs`); the mechanism itself is `../scratch.rs`, and the
sibling that owns everything the window does not — the retired regions and the engine arena — is
`../runtime/reclaim.rs` (`../../runtime/reclaim/CONTEXT.md`). This file is the window's design record; the parent
`../CONTEXT.md` states what a `-eval` run must obey and points here for the mechanics.
## API Surface
- **`EvalRegionStats`** (`pub`): what `Runtime::eval_region_stats` publishes, and what `tests.rs` reads — `used` /
  `capacity` / `retired_regions` / `retired_capacity_words` (the store's figures), `opens` / `resets` / `declines` /
  `last_decline` / `words_reclaimed` (this window's counters) and `reclaim: RegionReclaimStats` (the sibling pass's
  counters for the same runs, so one value answers both mechanisms).
- **`MIN_WINDOW_WORDS = 1024`** (1 024 words = 8 KiB), `pub(crate)`: the smallest window worth a descent. Below it a
  boundary does nothing and the window keeps growing (deferred, never lost). The store move applies the same figure
  to its candidates' **total** words, so a store that never grew a window worth resetting never pays for a copy.
- **`ScratchState`** (`pub(crate)`, a `Runtime` field): `window: Option<Window>`, the diagnostic kill switch
  `enabled` (`Runtime::set_eval_scratch_enabled`; while off no window opens and no state is recorded, so a
  measurement can reproduce the growth on the same binary) and the five counters above.
- **`Window`** (`pub(crate)`): `base: usize` (the region's first word — the identity that makes `mark` mean
  something), `mark: usize` (the region's `used` frontier at the window's start), `pinned: Option<&'static str>`
  (the first reason of either kind) and `lifetime_pin: Option<&'static str>` (the first *runtime-lifetime* reason).
  `heap_region_base`-style derivation (`hp - used`) is used because an empty region's `used_words_mut` slice is
  dangling.
- **`PinKind`** (`Lifetime` | `Transient`) and **`WindowState`** (`open`, `pinned`, `lifetime_pinned`) — what the
  window reports to a region-reclaim pass.
- **The entry points**: `open(runtime)` (called by the driver), `boundary(runtime, env: &mut Env)`,
  `close(runtime, …)`, `pin(runtime, reason)` (`PinKind::Lifetime`), `pin_transient(runtime, reason)`
  (`PinKind::Transient`) and `ScratchState::stop` (drops the window after a store move re-anchored the region).
- **`WindowProbe`**: the `Reach` sink the shared descent asks "does any reachable term stand inside this range?".
## The rewind rule (`safe_to_rewind`, and why it is never optimistic)
The window is rewound **only** when the complete descent from `Runtime::gather_off_heap_roots` plus the evaluator's
`Env` found **no** reachable word inside `[mark, used)`; every other outcome **declines** and keeps every word.
The one soundness argument, in three parts:
1. **A reset must never free a named word.** The descent is the escaped-payload prune's own (`../../eval/reclaim.rs`
   `walk` with a `WindowProbe` sink), reused rather than restated, so it shares that module's refusal rule.
2. **A holder the descent cannot see latches the window.** Every writer whose term may be retained outside the
   enumerated root families calls `pin`/`pin_transient`, which makes the window **unresettable** and merely advances
   its mark. A pinned window is one statement's bounded leak, never a dangle.
3. **Doubt ⇒ keep.** No fast path skips a check. The cheap states — "no window", "window pinned", "window smaller
   than `MIN_WINDOW_WORDS`" — all leave every word where it was.
The **decline list** (`last_decline` / `declines`): the descent could not complete (its visit budget was spent or it
met an unknown boxed header), the region was replaced (`grow_eval_heap` — the `base` no longer matches, and a mark
only means something in the region it was taken in), the mark is above the frontier, a reached word was found in the
window, the rewind itself was refused, or the window is pinned.
## The writer / holder classification (the enumeration this mechanism depends on)
Writers of `eval_heap`, and their treatment:
- `RuntimeContext::move_result_into_runtime_store` (a registered BIF's container result, handed straight back to its
  caller) and the store's growth retry — **transient**: the descent reaches it through the environment, or it died
  with the statement.
- `Evaluator::materialize_bits` (a binary/bitstring literal of the statement) — **transient**.
- `src/native/**`'s `beam_codec::term::make_binary(self.eval_heap(), …)` (a BIF's own binary reply) — **transient**: a
  message reply is copied into the receiver's mailbox, which *is* an enumerated root family.
- `Runtime::stabilize_term`/`stabilize_term_with` — **runtime-lifetime ⇒ pin**: the runner's recorded outcomes,
  `try_stabilize_escaped*` (pending spawns, the native message/function stores, the `RuntimeHooks` store, a
  `persistent_term` value, a pending timer's message, the exit reason).
- `src/port/completion.rs::decode_caller_tag` (a decoded port-message tag kept in runtime-level port state) —
  **runtime-lifetime ⇒ pin**.
- `Runtime::grow_eval_heap` (nothing — it *replaces* the region) — a stale `mark` declines instead.
- `Runtime::adopt_escaped_binaries` — off-heap payload handles, not region words: irrelevant here.
Holders (a family can hold a region word without writing one): the evaluator's `Env` (a root of the descent);
`Runtime::eval_closures[..].env`, every live process's dictionary/mailbox/registers/saved arguments/signals, pending
timer deliveries, `seq_trace` tokens, pending spawns, an engine's arena-stable arguments and a parked machine's root
set (all enumerated root families); a nested run's machine (`run_export_blocking`/`run_label_blocking`), whose
registers this module cannot enumerate — **latched at the call site when the run actually wrote on the store**
(`Runtime::pin_window_if_store_written`; a run that copied nothing leaves the window resettable, which is why the
window outlives an ordinary nested call and still bounds the statements around it); a nested `Evaluator` applied by a
loaded module (`apply_eval_closure`), whose copies the *module's* machine holds — **pinned unconditionally at the call
site**; and `../native/file_server/get_until.rs` (`EngineContinuation::call`), which builds its own `Machine` and
drives `run_export_outcome` with no latch — **the one unlatching nested-run site**, sound only because the term it
hands out stays inside the `{get_until,…}` request/continuation. `../boot/drive.rs` builds a `Machine` the same way
but boot completes before `open` takes its mark, so its words lie below every window.
## What a reset does to the other readers of the region
`Runtime::for_each_off_heap_region` — and through it the escaped-payload prune's word scan and the NIF-resource
prune's — is handed `eval_heap`'s written prefix. A reset only ever *lowers* `used` in the current region, and the
words it drops are the ones the descent proved dead, so those readers see strictly fewer, still-live words; retired
regions are untouched (a reset never frees a region).
## The window's two sites are the pass's sites
`open` opens both windows; `boundary` and `close` each run one region-reclaim pass over the same frontier and the same
pin latches, so the two mechanisms cannot disagree about which words belong to the run.
The pass **may move a word** (`../../runtime/reclaim/compact.rs`), which is exactly why `boundary`/`close`/
`Evaluator::reset_scratch` hand the environment over as **writable slots** rather than as a snapshot, and why a fired
move ends by `ScratchState::stop`ping this window — the mark it held named a region that is no longer current.
A pin's classification changes nothing here: every pin makes the window unresettable and advances its mark, identical
counters and reasons for both kinds; the classification exists only so the pass can honour a lifetime pin at the
**exit** as well as at the boundary, and a transient one at the boundary alone.
## Window lifetime, and the shapes that never reset
The window is `Some` for the **whole** driver run (`open` … `close`), i.e. it is open while a nested module body
runs, but a **reset** only happens at a boundary *before* a top-level statement.
So a **one-statement** `-eval` (the `compile:file/2` shape) never resets at all: the single-expression arm takes one
boundary before the statement, and `close` drops the window **without** resetting it — it runs the exit-site pass,
which is handed the driver's own live terms and is what reclaims that shape's retired chain.
A single giant statement is inherently retained (its value *is* the result), and so is a statement whose value is
bound into the environment; both by the language, not by this mechanism.
## Cost
A boundary costs one `Option` test and one `usize` subtraction while the window is closed, pinned, or below
`MIN_WINDOW_WORDS`. Only a window worth reclaiming pays the descent, and the descent stops at its first hit.
Figures, the OFF control and the oracle cross-check (`../../../tests/eval_region.rs`) are `../../../PERF.md`
*The `-eval` statement window*; nothing here quotes them.
## Phase 2 — no mid-run site can carry a move (the blocked residual)
Everything above runs at the two **non-running** sites. A compile's own words are written while it runs, and none of
the three in-run points can carry a move:
- **POINT A — the nested-run exit** (`../../context/pump.rs::run_export_blocking`/`run_label_blocking`, inside
  `with_relocation_exempt`): the current machine's `&mut` *is* in hand there (`machine_pool::acquire` … `release`) and
  `adopt_nested_payloads` already publishes `RootSet::ALL_LIVE`, but `RuntimeContext` holds no machine field, the
  **parent**/in-flight machines are unreachable (`Runtime::in_machine` is one position slot; a parent lives only on
  the enclosing epilogue's Rust frame; `FunBridge::nested` builds a fresh context per applied fun), and this node's
  own live terms (`Env` as a `HashMap`, `Evaluator::terms`, `Flow::Raise{reason}`, the match-arm locals) are Rust
  locals of a *different* frame with no `&mut` slot. The smallest additive seam is a runtime-owned stack of in-flight
  machine holders pushed/popped at those two epilogues and surfaced as a `RootFamily`, **plus** `Env`
  index-stabilisation (`Vec<Option<Eterm>>` keyed by a name index — a rewrite of the env plumbing, not a seam),
  **plus** a mutable `Raise::live_reason` accessor in `beam_common`.
- **POINT B — the store-growth point** (`Runtime::grow_eval_heap`, `&mut self`, no context and no machine) and the
  arena frontier rollover (`ContextHeap::alloc`, grow-in-place else a doubled fresh region, so the arena has no
  dead-and-droppable suffix): same parent/`Env` findings, and `grow_eval_heap` is too late for a *move* — the words
  it retires are already intermixed with live ones in the same `used` prefix.
- **POINT C — the engine heap-admission safepoint** (`ExecutionContext::ensure_heap`/`ensure_heap_full`): the
  machine's roots arrive **as arguments**, the context holds no `*mut Machine`, parents are unreachable, and
  `ensure_heap` returns early when `armed == false` (every nested machine) while `ensure_heap_full` bails at
  `current_module.is_none()`.
A move there is not merely inconvenient: `MinimalContext::reclaim_heap`'s `forward` is fallible and an `Err` leaves
the arena empty with the epoch bumped, its `FromSet` cannot span the store (so a store↔arena term graph cannot be
relocated by one relocator), and several holders have no `&mut Eterm` slot at all — which is why the move declines by
**address** when a recorded reason word stands in a candidate range.
**Conclusion:** the single-compile peak is not bounded by this mechanism; the binding holder is the in-flight machine
stack together with the evaluator's Rust-frame root set. The exile window (`with_relocation_exempt`) therefore stays
where it is, and what *is* bounded today is what the two committed mechanisms deliver at the two non-running sites.
## Constraints
- The window can only *disable* an action, never a guard: `Runtime::set_eval_scratch_enabled` and
  `BEAM_RT_REGION_RECLAIM` (`all`/`store`/`store-whole`/`arena`/`nomove`/`off`) are diagnostic kill switches.
- No `unsafe` (the crate is `#![deny(unsafe_code)]`; the descent's raw reads live in `../reclaim.rs`, which carries the
  workspace-sanctioned override).
- A stale `base` or a stale range must never answer a false **miss**: a moved region re-anchors the pass's cursor and
  stops this window.
## Test Strategy
`tests.rs` holds the window's 7 asserting pins: flat-in-N against the OFF control, and a `persistent_term` value, an
environment binding, a mailbox message and a pending timer's message each surviving every reset, plus the decline
reason. `the_growth_figures` is `#[ignore]`d and its table lives in `../../../PERF.md`.
The end-to-end oracle cross-check at the CLI boundary is `../../../tests/eval_region.rs` (20 interposed builds of
`lists:seq(1,2000)`, golden + system `erl`, loud skip without a toolchain).
Crate gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt`.
## Routing Table
- `tests.rs` → the window's 7 asserting pins plus the `#[ignore]`d growth measurement.
- `../scratch.rs` (the parent module this directory documents) → `EvalRegionStats`, `MIN_WINDOW_WORDS`, `ScratchState`/
  `Window`/`WindowState`/`PinKind`, `open`/`close`/`pin`/`pin_transient`/`boundary`, `WindowProbe`.
## See Also
- `../CONTEXT.md` → the evaluator as a whole, and the `-eval` route's term-storage rules.
- `../../runtime/reclaim/CONTEXT.md` → the sibling pass (retired regions, the arena, the store move).
- `../../../PERF.md` → the measured window figures; `../../../tests/eval_region.rs` → the retention contract.
