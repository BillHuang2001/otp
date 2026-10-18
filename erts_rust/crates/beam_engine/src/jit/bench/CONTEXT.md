# `beam_engine` JIT measurement harnesses (`src/jit/bench/`)

## Intent
The in-crate measurement harness for the execution tiers, over **thirteen** workloads split by
lowering story. It is a `pub mod` (not `#[cfg(test)]`) because the `#[test]` entry points live in the
integration test `tests/jit_bench.rs`, which cannot see a `#[cfg(test)]` lib module; the library only
exposes two plain functions and the harness itself is never on the engine's execution path.

Thirteen workloads, split by how the baseline tier lowers them:

* **Deopted** (every instruction is a `jit_step_one` helper round-trip on a native tier) — in
  `workloads.rs`: **`build/0`** (`put_tuple2`/`put_list` term building), **`map/2`**
  (`has_map_fields`/`get_map_elements`/`put_map_assoc`), **`select/1`** (a body dominated by
  `select_val`, deliberately mixing the inlined immediate shape with the deopted register shape) and
  **`walk/2`** (a hot tail-recursive cons-list walk: an `is_nonempty_list` guard + `get_list` + a
  small-int `'+'/2` + `call_only`).
* **Partially inlined** — in `workloads.rs`: **`sum/1`** (the **non**-tail-recursive list sum: the
  caller's frame — and therefore the Y slot holding the head — stays live across the recursive call,
  so its `gc_bif '+'/2` names a `Y` operand and a `Y` destination; the tier inlines both behind its
  per-block published frame window, as it does the `Move` guarding the recursion, so the body is
  mostly native). The only per-level deopts are the `Allocate{2}` frame push and the **non-tail**
  local `Call`, whose return address `pc + 1` — which `beam_loader` does not cut a block at — has its
  own native **continuation sub-block**, so the driver re-enters natively at the post-call
  continuation instead of stepping the block tail one interpreter instruction at a time.
* **Inlined** (the tier handles the body natively) — in `inlined.rs`: **`slice:fib/1`** (a real
  `.beam` fixture), **`kernel/2`**, **`wide/2`**, **`aggr/2`** and **`cmp/2`**.
* **Cross-module** — in `workloads.rs`: **`cross/2`**, a two-module
  `jit_bench_cross_a:ping/2` ⇄ `jit_bench_cross_b:pong/2` ping-pong whose every hop is a
  `call_ext_last`. Neither deopted body work nor inlined body work: the call is *not* in a native
  tier's inline set, so it deopts to one interpreter step, but that step resolves the import through
  the host's cross-module seam (`ExecutionContext::resolve_external_call`) and hands the **driver** a
  `Step::Enter`, so the transfer is a driver-mediated module-context switch. On the baseline tier the
  callee's blocks are resolved through `NativeCode::module_blocks` →
  `crate::jit::cache::cached_module_blocks`, which compiles the callee into the per-thread `CodeCache`
  on first sight — so the callee runs **natively** too, and the workload's figure isolates the
  *transfer* itself.
* **Declined `call_ext`** — in `workloads.rs`: **`call_ext/0`**, a counted loop of
  `call_ext 2 erlang:'+'/2` calls the host answers "run it synchronously in this process"
  (`Ok(None)` — the common BIF shape and the `MinimalContext` default). This is the workload for the
  `call_ext*` **decline** path: on the baseline tier the call is *not* a plain deopt, it is lowered
  through the native `jit_ext_call` helper, which consults the host's seam **once** and (on the
  decline) hands the instruction to `jit_step_ext` with that already-computed answer, so
  `Machine::call_ext_dispatch` never re-asks the seam — before that helper existed the tier resolved
  the same import **twice** (once in `jit_ext_call`, once inside the `jit_step_one` fallback). The
  deterministic per-instruction pin is
  `src/jit/baseline/tests.rs::the_baseline_tier_agrees_with_the_interpreter_on_the_declined_call_ext_seam_count`;
  `call_ext/0` is its end-to-end hot-loop counterpart, so the harness measures the decline from now
  on. The import is `erlang:'+'/2` (`Fixture::with_plus`'s index-0 import — answered by the harness's
  `arith_hook` on the synchronous `call_bif` route), the same shape the per-instruction pin uses; it is
  deliberately *synthetic* (`erlc` emits `+`/2 as a `gc_bif`), because the instruction under
  measurement is the `call_ext` **seam**, not the arithmetic. The loop `allocate`s its own frame first
  and `deallocate`s it before the final `return` — a `run_export` body's bottom frame is the `Halt`
  frame, so the frame discipline is load-bearing.
* **Message passing** — in `send_recv.rs`: **`send_recv/2`**, a tail-recursive loop that `send`s
  `{Self, N}` to itself and `receive`s it straight back through the `Mailbox` seam, folding the
  payload into an accumulator. It is the in-process counterpart of the runtime-level two-process
  `send_recv` copy loop whose default-tier 2.15×–3.31× regression is recorded in
  `../../../../../../tools/bench/RESULTS.md` §8 item 2. The module is the compiled lowering of
  `send_recv(0, Acc) -> Acc; send_recv(N, Acc) -> Self ! {Self, N}, receive {Self, T} ->
  send_recv(N - 1, Acc + T) end` — the same instruction sequence the installed `erlc` emits, with
  `self/0` replaced by a small-int constant in `Y(0)` (one `MinimalContext` process has no `self/0`
  BIF). Per round, **eight** instructions deopt to `jit_step_one` (the `allocate_heap` frame push,
  the `put_tuple2` message build, `send`, `loop_rec`, `remove_message`, the two `Y`-operand
  `gc_bif`s and the `Y`-operand bound-variable `is_eq_exact`), while the rest of the body is
  inlined (the entry clause `is_eq_exact`, the four `move`s, the `is_tuple`/`test_arity` header
  tests, the two `get_tuple_element`s, `swap`, `call_last`). The mailbox is `LoopbackMailbox`, a
  one-process FIFO whose `send_message` enqueues the message the very next `receive` scans, so the
  `wait` arm is present in the shape but never reached; its scan position is a **saturating**
  cursor with `beam_message::Queue`'s `QueueCursor` semantics, and the `recv_marker_*` opcodes are
  served over the same cursor (`workloads.rs`'s `Workload::with_mailbox`/`install_mailbox` are the
  seam's host half). Unlike every other workload it is **not** deopt-only: the inlined
  body work outweighs the eight deopts, so the tier is net *faster* here (see the measurement
  below) — the runtime-level regression is therefore a runtime-glue effect (the second process,
  the blocking `receive`/resume cycle, the scheduler, the ETF cop), **not** an engine-tier effect
  on this compiled shape.

## API Surface (crate-visible; `crate::jit::bench::…`)
- `support` (`pub(crate) mod`) — the scaffolding shared with the sibling Cranelift harness
  (`crate::jit::cranelift::bench`): `arith_hook(truthy, falsy) -> BifHook` (small-int `erlang`
  `+`/`-`/`*` and the eight comparison operators), the synthetic-`Module` builder `Fixture`
  (`new`/`with_plus`/`import`/`add`/`finish`/`intern`), `Samples` (feature-gated on `jit-cranelift`;
  `min`/`median`) and the report primitives `ns_per_op`/`ops_per_sec`.
- `workloads` — the `Workload` type (module + exported `function/arity` + argument source + a value
  check pinning the intended result + an optional `CrossCall` + an optional mailbox factory), the
  five `workloads.rs` builders (four deopted plus `sum/1`),
  the cross-module pair, their round constants
  (`BUILD_ROUNDS`/`MAP_ROUNDS`/`SELECT_ROUNDS`/`WALK_LENGTH`/`SUM_LENGTH`/`CROSS_STEPS`,
  `CALL_EXT_ROUNDS`/`CALL_EXT_INCREMENT`, `TIMED_RUNS`/`AGREEMENT_RUNS`) and the in-context argument
  builders (`map_args`/`walk_args`/`sum_args`); `workloads()` returns all thirteen (the seven here
  plus `inlined::inlined_workloads()` plus `send_recv::send_recv_workload()`), and
  `sum_workload()`'s `Y`-operand/`Y`-destination `gc_bif` is
  pinned by a `#[cfg(test)]` shape guard
  (`tests::the_sum_workload_keeps_its_y_slot_operand_and_destination`), so the workload cannot quietly
  stop exercising the `Y` operand/destination shape the inlined frame-window path handles — as is
  `call_ext_workload()`'s declined `call_ext`
  (`tests::the_call_ext_workload_keeps_a_declined_call_ext_in_its_loop`). `Workload::bif_hook()`
  installs the arithmetic hook (with working booleans only when the module interns both `true` and
  `false`).
  `CrossCall` carries the callee's shared `Arc<Module>`, the import `(module, function, arity)` the
  caller resolves and the callee's export **entry** label; `Workload::with_cross` attaches it and
  `Workload::install_cross(ctx)` installs the `ExternalResolver` answering **both** ping-pong
  directions (the callee handle, and a handle to the workload's own module for the return hop),
  leaving every other import on the synchronous `call_bif` path. `Workload::with_mailbox` attaches a
  `fn() -> Box<dyn Mailbox>` factory and `Workload::install_mailbox(ctx)` calls it once per
  measurement and hands the result to `MinimalContext::set_mailbox`, so every run starts from an
  empty mailbox; a workload without one keeps the receive seam's typed `unsupported` default.
- `inlined` — the five tier-inlined workload builders and their pinned result constants
  (`FIB_RESULT`/`KERNEL_RESULT`/`WIDE_RESULT`/`AGGR_RESULT`, `pub(super)` for `checks`).
- `send_recv` — the `send_recv/2` message-passing loopback builder, its `SEND_RECV_ROUNDS`
  parameter, and the host half of the receive seam: `new_mailbox() -> Box<dyn Mailbox>` and
  `LoopbackMailbox`, a one-process FIFO over a `VecDeque<Eterm>` with a saturating `cursor`
  (`send_message` enqueues, `receive_next` reads at the cursor, `receive_skip` advances with
  saturation, `receive_remove` unlinks at the cursor and repositions at the first message,
  `receive_reset` repositions at the first message) plus the `recv_marker_*` group served over the
  same cursor (`reserved` marker→position, `bound` reference→marker; an unknown marker/reference is
  a typed `unsupported`, never a silent no-op). Its two `#[cfg(test)]` guards are
  `tests::the_send_recv_workload_keeps_its_send_and_receive_shape` (the `send`/`loop_rec`/
  `loop_rec_end`/`wait`/`remove_message`/`put_tuple2`/bound-match counts) and
  `tests::the_loopback_mailbox_completes_the_send_receive_loop` (the mailbox round-trips the whole
  64-step loop without ever suspending and returns the triangular sum).
- `checks` — the thirteen per-workload result checks (`verify_build`/`verify_map`/`verify_select`/
  `verify_walk`/`verify_sum`/`verify_fib`/`verify_kernel`/`verify_wide`/`verify_aggr`/`verify_cmp`/
  `verify_cross`/`verify_call_ext`/`verify_send_recv`), each
  `fn(&Workload, Eterm) -> Result<(), String>`.
  `verify_cross` pins the exported result to `CROSS_STEPS`, which a loop that ran a different number
  of transfers cannot produce; `verify_sum` pins the triangular sum
  `SUM_LENGTH * (SUM_LENGTH + 1) / 2`; `verify_call_ext` pins
  `CALL_EXT_ROUNDS * CALL_EXT_INCREMENT`, which a loop that skipped, repeated or reordered a declined
  `call_ext` cannot produce; `verify_send_recv` pins the triangular sum
  `SEND_RECV_ROUNDS * (SEND_RECV_ROUNDS + 1) / 2`, which a loop that skipped, repeated or corrupted a
  `send` + `receive` round-trip cannot produce.
- `mod.rs` — the tier seam (`Runner`), the timing loop (`Measurement`) and the reporting
  (`report`/`bench_workload`), plus the two public entry points `assert_tier_agreement()` (fast, the
  agreement half) and `run_measurement_table()` (the `#[ignore]`d timing table), and the
  crate-visible `run_workloads_on(tier)` — the **one-tier** driver (every workload on exactly ONE
  tier, results discarded, no cross-tier comparison, no printing) that a *dynamic* census needs,
  since `assert_tier_agreement` deliberately runs every tier (twice) and would mix the tiers'
  instruction mixes into one sample. Its consumer is
  `../baseline/census.rs::dynamic_execution_census` (the `exec_census` execution ranking, run on the
  interpreter tier). `Measurement::run`
  installs the workload's cross-module resolver (when it has one) and its mailbox (when it has one)
  right after the BIF hook.

## Constraints
- Wired by `pub mod bench;` in `../mod.rs`; the crate-public surface is the two entry functions only
  (the internal `mod`s stay `pub(crate)`/`pub(super)`).
- Uses only the crate's own seams (`JitBackend`, `Machine::run_export`, `MinimalContext`,
  `context::Mailbox`); a native tier is compiled **once** per measurement (outside the timed loop)
  and only `run_compiled` is timed.
  `cross/2` needs no pre-compiled callee: the tier's own per-thread code cache compiles it on first
  sight, through the driver's cross-module transfer.
- The mailbox is the harness's **only** receive-seam host. `send_recv/2` drives a non-blocking
  loopback (the `send` delivers the message the very next `loop_rec` reads), so its `wait` arm is
  never taken and the harness's non-blocking `run_export` never sees a suspend. That is deliberate:
  `MinimalContext` runs **one** process with no scheduler, so a genuinely blocking receive is not
  expressible here — the runtime-level blocking shape is measured by the external driver
  (`../../../../../../tools/bench/`), not by this table.
- `support` is the **single** home for the scaffolding both in-crate harnesses share; do not re-add a
  private copy in either harness.
- Measurement strategy is **not** shared: this module's fixed-order tier table and the ABBA paired
  ratio of `../../cranelift/bench.rs` stay separate.

## Test Strategy
- `tests/jit_bench.rs` is the thin driver: the agreement `#[test]` runs in the plain suite; the
  `#[ignore]`d timing table prints via
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --test jit_bench -- --ignored --nocapture`
  (run one test so the workloads never run concurrently; add `--features jit-cranelift` for the
  Cranelift column).
- Numbers are wall-clock on a shared sandbox (order-of-magnitude evidence only); correctness — each
  tier agreeing on the workload's *intended* value — is the only thing asserted, never "the JIT is
  faster". The sandbox noise is large enough that a whole-run set drifts (a controlled run pair had
  the untouched **interpreter** column move ~17 %), so quote a **min over ≥3 runs** and compare only
  figures from the same run set.

## Measured: `sum/1` — the `Y`-slot `'+'` path is native (min of 3 timed runs)
`sum/1` is the harness's **partially inlined** workload: the small-int `'+'/2` `gc_bif` names a `Y`
operand (the head survives in `Y(0)` across the non-tail recursive call) and writes a `Y` destination
(`{y,1}`), and the tier inlines both behind its per-block published frame window, as it does the
`Move` guarding the recursion. `sum/1` is therefore **not** a deopt-dominated twin of `walk/2`: min of
3 timed runs is interpreter **4718.7 ns/op** vs baseline-jit **2675.8 ns/op**, i.e. baseline ≈
**1.76×** the interpreter. The per-level deopts that remain are the `Allocate{2}` frame push and the
**non-tail** local `Call`; the call's continuation `pc + 1` — which `beam_loader` does not cut a block
at — has its own native **continuation sub-block**, so the driver re-enters natively at the post-call
continuation instead of stepping the block tail one interpreter instruction at a time. Both tiers pin
the exact triangular sum (`SUM_LENGTH * (SUM_LENGTH + 1) / 2` = 2080).

Full min-of-3 table (min per column), `ns/op` (interpreter → baseline-jit), from
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --test jit_bench -- --ignored --nocapture`
(`/proc/loadavg` `23.12 19.00 11.23` before runs 1–2, `21.59 18.75 11.19` before run 3):
`build/0` 7222.4 → 7154.9 (1.01×) · `map/2` 10205.5 → 8979.8 (1.14×) ·
`select/1` 2210.1 → 1363.6 (1.62×) · `walk/2` 2388.7 → 432.5 (5.52×) ·
**`sum/1` 4718.7 → 2675.8 (1.76×)** · `slice:fib/1` 1493.9 → 447.2 (3.34×) ·
`kernel/2` 4423.5 → 552.6 (8.00×) · `wide/2` 5549.9 → 573.7 (9.67×) ·
`aggr/2` 8447.2 → 2228.0 (3.79×) · `cmp/2` 9062.4 → 1483.9 (6.11×) ·
`cross/2` 7465.3 → 6695.5 (1.11×) · `call_ext/0` 9443.9 → 6025.3 (1.57×) ·
**`send_recv/2` 13337.7 → 9268.9 (1.44×)**. Every workload is faster on
the baseline tier in this run set, `build/0` and `cross/2` only marginally; quote only ratios from **one**
run set — never a bare absolute.

## Measured: `cross/2` (min of 3 timed runs, shared sandbox)
`cross/2`'s baseline figure sits just above the interpreter's: min-of-3 is interpreter
**7465.3 ns/op** vs baseline-jit **6695.5 ns/op**, i.e. ≈ **1.11×**. A `call_ext_last` is not inlined,
so every hop pays the caller block's per-instruction deopt (spill every cached x-register + an
interpreter step, which itself calls `resolve_external_call`) **plus** a full driver-visible exit —
`switch_into` (`Machine::switch_context` + the callee's literal rebind + callee-block resolution) —
and a fresh entry into the callee's compiled block, bypassing the block linker entirely (on the
interpreter a `Step::Enter` is just a `Goto`); the columns nearly meet because the callee block body
itself still runs natively. Because the pair is a ping-pong, the tier's pre-compiled caller artifact is used
for the very first hop only, so this workload measures the **un-amortised** cross-module transfer by
construction.

## Measured: `call_ext/0` (min of 3 timed runs, shared sandbox)
`call_ext/0`'s baseline figure is **above** the interpreter's: min-of-3 is interpreter **9443.9 ns/op**
vs baseline-jit **6025.3 ns/op**, i.e. baseline ≈ **1.57×** the interpreter. That is the expected shape
for the decline path: the `call_ext` instruction itself is
lowered **natively** through `jit_ext_call` (so the tier pays one helper call, not a full
decode + dispatch interpreter step) and the rest of the loop — the two `move`s and the `test is_ge` —
is inlined, so the interpreter is the slower tier. The figure still isolates the decline cost the
helper exposes: 256 declined calls per run at ≈ **23.5 ns/call** on the baseline (≈ **36.9 ns/call** on
the interpreter), against a loop whose non-call work is two register moves and one compare per
iteration. Quote it as the **ratio within one run set**, never as a bare absolute — the sandbox is
shared and heavily loaded.

## Measured: `send_recv/2` (min of 3 timed runs, shared sandbox)
`send_recv/2` is the message-passing workload, and its figure is the **honest counterpoint** to the
runtime-level regression: in this in-process, non-blocking loopback the baseline tier is **faster**,
min-of-3 interpreter **13337.7 ns/op** vs baseline-jit **9268.9 ns/op** (≈ **1.44×**, i.e. ≈ **208.4
ns** vs ≈ **144.8 ns** per `send` + `receive` round-trip over 64 rounds). The per-round instruction mix
is **eight** deopts (`allocate_heap`, `put_tuple2`, `send`, `loop_rec`, `remove_message`, the two
`Y`-operand `gc_bif`s and the `Y`-operand bound-variable `is_eq_exact`) against an inlined remainder
(the entry clause `is_eq_exact`, four `move`s, `is_tuple`/`test_arity`, two `get_tuple_element`s,
`swap`, `call_last`), and here the inlined work outweighs the deopt overhead — so the shape does
**not** reproduce the runtime-level 2.15×–3.31× default-tier deficit on the two-process `send_recv`
loop (`../../../../../../tools/bench/RESULTS.md` §8 item 2). The two figures are not in conflict: this
harness runs **one** process on a `MinimalContext` whose mailbox delivers synchronously, so the
receive never blocks, there is no second process, no scheduler resume and no ETF cop in the loop —
exactly the runtime glue the external driver measures and this table cannot. The reading is therefore
that the runtime regression is a **runtime-glue** effect, not an engine-tier lowering effect on this
compiled shape.

## Routing Table
- `support.rs` → the shared scaffolding (see API Surface); `workloads.rs` → the four deopted workloads
  (`build/0`, `map/2`, `select/1`, `walk/2`), the partially-inlined `sum/1`, the cross-module `cross/2`
  pair, the declined-`call_ext` `call_ext/0` **and** the `Workload` type + `with_cross`/`install_cross`
  + `with_mailbox`/`install_mailbox`; `inlined.rs` → the five tier-inlined workloads;
  `send_recv.rs` → the `send_recv/2` message-passing loopback + its `LoopbackMailbox`;
  `checks.rs` → the result checks; `mod.rs` → the tier-table harness.
- `../../cranelift/bench.rs` → the sibling order-balanced baseline-vs-Cranelift harness, which
  consumes `support` through `crate::jit::bench::support`.

## See Also
- `../../CONTEXT.md` → the JIT / execution-tier seam; `../../cranelift/CONTEXT.md` → the Cranelift harness.
- `../../../MEASURED.md` → the measured tier table this harness produces.
