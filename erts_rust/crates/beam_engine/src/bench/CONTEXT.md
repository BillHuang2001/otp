# `beam_engine::bench` — in-process foreign-workload decomposition harness

## Intent
A `#[cfg(test)]`-only harness that measures real external benchmark workloads **in process**, so the engine's own per-call cost is decomposed without the runtime's CLI boot, its module loading, its scheduler and its copy-out glue inside the timed loop.
The CLI route cannot do that: a `beam_rt` CLI boot is ≈0.7–0.8 s, i.e. **35–45 %** of an unpaired per-op figure (0.1 % for `list_walk`, which amortises it).
Five instruments share one methodology: `list_walk` (the dispatch floor), `cowboy_bstr` (plain build + dispatch on real foreign code), `long_append` (the writable payload's growth path), `emqx_base62` (bitstring matching + creation, the one case with BIFs) and `send_split` (the two-process copy-bound marginal).
A batch is a **fresh inner host** per `BENCH_BATCH` calls, an untimed warmup phase, then the measured rounds; nothing measured touches the runtime's CLI, boot script or scheduler.
The foreign sources are **staged read-only** from their checkouts at test time (`compile_staged`, system `erlc +deterministic`); this repository stores no copy that could drift.
It is absent from production builds: `../lib.rs` declares `#[cfg(test)] mod bench;`.

## API Surface
- `mod.rs` → the **shared harness** plus the `cowboy_bstr` fixture and the `#[ignore]`d entry point `foreign_bstr_to_lower_decomposition`.
  `compile_fixture(module, source, tag)` / `compile_staged(module, source_path, tag)` compile a fixture with the system `erlc`; every workload **skips loudly** when `erlc` (or the checkout) is absent.
  `Prepare` (the closure that builds a batch's argument, run **outside** the tally), `run_calls` (the batch loop), `measure`/`measure_result`, `Measurement` (`min`/`median`) + its `ResultSnapshot`, `instr_per_call`, `ns_per_call`, `env_usize`, `selected_tiers`, `ReportSpec`/`report_generic` (the one report body every workload uses, in per-call or per-unit rows).
  `baseline_census`/`print_census`: the per-thread `../jit/baseline/counters.rs` snapshot pass; it takes an `Option<&Unit>` — `None` prints the per-call driver/block figures (the foreign cases), `Some` adds their **per-unit** twins (`driver_round_trips_per_element`, `block_entry_lookups_per_element`, …) for a workload that is not call-shaped; both forms print `in_block_calls`/`in_block_call_declines` beside `driver_interp_steps`, because those explain the `driver_round_trips` figure next to them.
- `counting.rs` → `CountingContext`, a delegating `ExecutionContext` over `MinimalContext` that tallies `reduce` (= instructions executed, charged once per interpreted instruction), `alloc_words_rooted` calls/words (the per-built-term door), the `ensure_heap`/`ensure_heap_full` door (the `test_heap`/`allocate_heap` door), `call_bif` plus a `module:function/arity` histogram, `apply`/`apply_fun`, `alloc_words`, and the three resolver seams. (`MinimalContext` opts out of `reduce`, so this is where `wants_reduce` becomes `true`.)
  It also carries the harness' own **closed BIF floor** (`host_bif`: `erlang:iolist_to_binary/1`, `erlang:bit_size/1` — the bitstring family's own reader `crate::families::bitstrings::bit_size`, i.e. C's `erts_bin_bits_size` — and small-int `+ - * div rem abs`) with a `floor_declines` counter that every report asserts to 0, so an unmeasured BIF cannot silently change a workload.
  The histogram key is `BifHistogram`, whose `record` is **allocation-free and hash-free** on the dispatch path (a memo of the last key, then a pairwise scan of the few distinct keys); the `"module:function/arity"` display strings are built once, by `to_map`, at report time.
- `list_walk.rs` → the dispatch-floor fixture (`LEN = 1000`, `BENCH_K` walks per call), entry point `list_walk_decomposition`, the whole-call report and the paired per-element marginal report.
- `long_append.rs` → the growing-accumulator fixture (`N_ARG = 8 * 1024`), entry point `foreign_long_append_decomposition`, its report and the growth-shape probe (`Machine::refc_binaries()` → `capacity`/`reserved_bytes`/`growth_counters`).
- `foreign.rs` → the four real foreign cases staged from the read-only checkouts (`BENCH_COWBOY_REPO`, default `/home/bill/Source/cowboy` → `src/cowboy_bstr.erl`; `BENCH_EMQX_REPO`, default `/home/bill/Source/emqx` → `apps/emqx/src/emqx_base62.erl`), entry points `foreign_cowboy_bstr_decomposition` and `foreign_emqx_base62_decomposition`; one loaded artifact per module serves its cases.
- `send_split.rs` → the `send_recv − send_recv_plain` split instrument, entry point `send_recv_split_decomposition` plus its structural gate `the_two_legs_of_the_copy_bound_split_agree`.
- Env knobs: `BENCH_ITERS`, `BENCH_BATCH`, `BENCH_WARMUP`, `BENCH_TIER` (`interpreter`|`baseline`|`both`), plus `BENCH_K` (walks per call, `list_walk` only); `env_usize` rejects a zero or unparsable value and falls back to the workload default.
  Defaults per workload: `bstr_to_lower` and the foreign cases 20 000 / 500 / 2 000; `long_append` 50 / 5 / 5 (a call copies 64 KiB out); `list_walk` 100 / 10 / 10 with `BENCH_K = 20` (one call walks 20 000 elements).
  **`BENCH_TIER`'s default (unset or empty) is the baseline tier alone**, because that is the tier every `beam_rt` process entry runs on and therefore what a cross-runtime profile must describe; the interpreter leg is roughly twice as slow per call and would own most of the samples, silently rewriting a profile's attribution (a `both` profile of the foreign byte-loop kernels puts the interpreter's `select_val` ladder in the top ten even though the native tier inlines it).
  An **unrecognised** value is a loud error naming `interpreter|baseline|both`, never a silent coercion — a misspelt tier must not quietly measure a different one.

## How to run
```text
# every #[ignore]d entry point, dev profile (opt-level = 1); each SKIPs loudly if erlc/the checkout is absent
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_bstr_to_lower_decomposition
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_long_append_decomposition
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture list_walk_decomposition
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_cowboy_bstr_decomposition
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_emqx_base62_decomposition

# release: per-instruction cost is ~2x lower than the opt-level=1 dev profile — use release for ratios
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib --release --no-run
taskset -c 3 setsid -w erts_rust/target/release/deps/beam_engine-<hash> --exact \
    bench::list_walk::list_walk_decomposition --ignored --nocapture

# the copy-bound split, one benchmark process at a time (BENCH_* are the usual knobs)
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --release --lib --no-run
taskset -c 3 setsid -w erts_rust/target/release/deps/beam_engine-<hash> --ignored --nocapture \
    --test-threads=1 send_recv_split_decomposition

# profile a loop itself (the erlc child is filtered out by --comm)
perf record -g -o "$TMPDIR/p.data" -- taskset -c 3 <binary> --exact <test path> --ignored --nocapture
perf report --stdio -i "$TMPDIR/p.data" --no-children --comm=bench::foreign_ \
    --percent-limit 0.8 --sort symbol
```

## Workload set
Every shape is a real `erlc`-compiled fixture staged at test time; all are BIF-free in the timed loop except `base62:encode/1`/`decode/1`.
- `list_walk` — `bench_list_walk:walk/1` (the `tools/bench` `list_walk` program): `build/2` makes a `LEN = 1000`-element list, then a body-recursive `sum/1` walks it, and one measured call is `BENCH_K` walks (20 000 elements).
  The loop body never allocates, never crosses a heap-admission point and never dispatches a BIF (`'+'` on two small ints is an immediate); it is 8 instructions per element, one of them a `call_only` to the loop itself ⇒ the pure **dispatch floor**.
  `build/2` is setup-shaped cost *inside* the measured call, which is why the per-element figure is the paired marginal `walk(K) − walk(0)`.
- `cowboy_bstr:to_lower/1` and `capitalize_token/1` — binary comprehensions whose accumulator **fits** the compiler's `bs_init_writable` hint (`{integer,256}` = 32-byte capacity), so they measure the plain build + dispatch floor on real foreign code.
  The per-character loop is `bs_start_match3`, `bs_match` (`ensure_at_least 8`, `integer 8`), a **local** call to a pure-Erlang `select_val` helper (27 clauses, no BIF), `bs_create_bin` with `[private_append, {tr,{y,1},{t_bitstring,8,true}}, all, integer 8 {x,0}]`, and `call_last 2` — 438 instructions per call for `to_lower/1`, 660 for `capitalize_token/1`, at 30 characters each, so the 30-byte result never grows the payload.
- `long_append:expand/1` — a binary accumulator that genuinely **outgrows** its hint, so it measures the writable payload's **growth** path (`BitWriter::ensure_room` → `Machine::grow_writable_payload` → `beam_alloc`'s `RefcBinary::ensure_capacity`).
  The fixture (verified with `erlc -S`):
  ```erlang
  input(N)  -> <<16#00112233445566778899aabbccddeeff:N/unit:8,
                 16#ffeeddccbbaa99887766554433221100:N/unit:8>>.
  expand(B) -> << <<C:8,C:8,C:8,C:8>> || <<C:8>> <= B >>.
  ```
  It is **BIF-free by construction** (the fixture's host rejects every BIF): `input(N)` is one `bs_create_bin` of two dynamic-size `integer` segments (zero-extended, so each half is `N` bytes with a distinct 16-byte marker near its end) and `expand(B)` appends four `integer 8` segments per input byte.
  `erlc -S` gives `{move,{integer,256},{x,0}}. bs_init_writable.` — a **256-BIT = 32-byte** hint — and in the comprehension body `'-expand/1-lbc$^0/2-0-'/2` the first `bs_create_bin` segment is `{atom,private_append}, {tr,{x,1},{t_bitstring,32,true}}, {atom,all}` followed by four `integer 8` segments and a tail `call_only 2`, i.e. exactly C's writable-accumulator idiom.
  The hint is **not** an estimate of the result: the argument's size is unknown to `expand/1`, so the compiler falls back to its 256-bit default.
  Sized to `N_ARG = 8 * 1024` → **16 384 bytes in, 65 536 bytes out (64 KiB), 16 384 appends, 2048× the hint**.
  `input/1` is a separate function **on purpose**: building the input inside `expand/1` would make the hint `bit_size(Bin) * 4` (exact) and nothing would grow, and a **list** comprehension is the wrong shape (the compiler derives its hint from `length/1`).
  `input/1` runs once per batch as setup — the counters are parked across it and the payload roster is snapshotted after it.
- `emqx_base62:encode/1` and `decode/1` — bitstring matching + creation over a caller-sized input.
  `encode/1` folds four 6-bit groups per output character with a `select_val` character table and calls `erlang:iolist_to_binary/1` once per four encoded characters (3 static sites, **4 calls** for the 12-byte input), so it is the one foreign case that is **not** BIF-free and reports a real `bif_calls` histogram.
  `decode/1` additionally needs `erlang:bit_size/1`, both in a guard (`bit_size(Rest) >= 8`) and as `bit_size(Acc) rem 8` over a `t_bs_context` accumulator.
- `send_split.rs` — the `send_recv − send_recv_plain` split instrument: `#[cfg(test)]`-only and **not** on any production path, it drives `copy_bound`'s two-process ping-pong over the engine's own seams (`ExecutionContext` + `Mailbox`), so every engine-side stage is a **deterministic host-side tally** and the wall clock comes from the same process.
  Three legs differ only in how the payload reaches the worker's reply: **`plain`** (`T = payload()`, every field literal-able ⇒ the whole map hoisted into the literal area, one `move` per op: the `send_recv_plain` shape), **`heap`** (`T = payload(N)`, one counter-derived field ⇒ `put_map_assoc` every op: what `send_recv` reaches once `binary_to_term` has rebuilt the payload as an unshared heap term) and **`codec`** (`heap` plus the real worker's two host crossings, `bench_codec:encode/1`/`decode/1`, served as **identity** stubs — the crossing without the image).
- Sources: `cowboy_bstr.erl` is ISC © Loïc Hoguin and `emqx_base62.erl` is EMQ copyright — staged read-only, never edited or vendored; the `list_walk`/`long_append` fixtures are original to this repository.

## Measurement method
This node owns the discipline; every figure lives in `../../MEASURED.md`, `../../PERF.md` and `../../../../tools/bench/RESULTS.md`.
- **Paired, interleaved marginal.** Each round samples the N-op pass and the 0-op pass back to back and takes the median paired difference `(c[N] − c[0]) / N`. A non-interleaved schedule is the WRONG estimator: it attributes host drift and load to the workload and can overstate a gap by up to ~56×.
- **A per-element figure is a marginal** `walk(K) − walk(0)`: one `build/2` in both phases, so the shared build cancels; it inherits both phases' noise, so always quote it with the round it came from, never as an absolute per-element time.
- **A phase is `SUB_RUNS = 5` timed sub-runs** of `ceil(iters / SUB_RUNS)` calls each; `Measurement` keeps one ns sample per sub-run and reports their `min`/`median`, and the counters are tallied once over the whole phase, so a counter is exact while a nanosecond is a sample.
- **`instr_per_call` is the INTERPRETER's `reduce`-hook count** (one charge per interpreted instruction) and is reported for **both** tiers, because the instruction count is a property of the code, not of the tier; the native tier offers the same hook once per driver-loop iteration (a block entry), which is why its own counter is driver iterations, not instructions.
  A `BENCH_TIER=baseline` run therefore performs one extra interpreter pass purely to derive the instruction figure.
- **Price the default tier** (native baseline JIT) on `list_walk`: BIF-free there (the small-int `+` is inlined), `call_only` linked in-block, no heap-admission point ⇒ the pure dispatch floor. The interpreter tier's per-element cost is BIF-dominated (~42 % of its cycles are `call_bif`), so it is not part of that record.
- The default tier's **A/A noise floor is ±1.3 %** on cycles.
- **The host is SHARED.** Quote ratios plus the deterministic counters, never an absolute wall time; record `/proc/loadavg` 1-min before and after every run and state it.
  `ns per instruction` is the discriminating column across a loaded host — the bitstring-heavy foreign cases sit an order above `list_walk` — so a per-call wall time alone misattributes the difference to dispatch.
- **Deterministic counters are the primary evidence** (identical round to round; only nanoseconds move): instructions/call, driver round-trips, in-block calls/declines, block-entry lookups/misses, rooted calls/words, heap-door calls, BIF calls + histogram, local-call requests, refc payloads.
  `floor_declines == 0` and `result_is_correct == true` on every case, and both tiers must agree on the result bytes.
- **`gc_walk` is START-UP cost, flat in N** (`beam_rt::context::gc_walk` / `gc::word_region` / `beam_gc::nursery::contains`): never attribute it per element, per call or per instruction — the bench tool's paired marginal cancels it and `perf` cannot.
  The door that fires is the runner's pre-emption arm `collect_at_reds_boundary`, reachable only from the `Yield` park arm; the engine's admission doors (`alloc_words_rooted`/`ensure_heap_full`/`ensure_heap`/`call_bif`/`reduce`) are never its caller.
  Frequency ≈1 pre-emption per 1–4 walks (`CONTEXT_REDS` = 4 000 reductions per slice against ≈1 000 reductions per walk); each gate is O(1), but the loop DOES pay root-set publication (`publish_parked_roots` + `resync_parked_roots`, ~1 000 words each way) per pre-emption.
  There is no "root set unchanged since the last walk" early-out, and no root-set-level "nursery holds no object" test: the emptiness test is the capacity/occupancy heuristic one level up.
- **`long_append`'s amortisation proof is structural, no wall clock**: one accumulator payload per call however many appends, `moves` = O(log n) (6 for a 64 KiB build) and `bytes_copied ≈ n` (64 512 B); the copy-rebuild form it replaces would copy ≈8 300× more (`≈ n²/(2·4)`).
- **The harness' own bookkeeping must stay allocation-free**: a per-dispatch `format!("{module}:{function}/{arity}")` histogram key once put 14.2 % of `list_walk`'s `perf` samples inside the harness, i.e. it inflated the figure it exists to report.
  `BifHistogram` (a memo of the last key + a pairwise key compare; display strings only at report time) removes it.

### What each shape's deterministic counters mean
- `list_walk`: the rooted/word and heap-door figures belong to `build/2` (one cons cell and one `test_heap 2 2` per element), **not** to the walk loop, which is why the per-element figure is the marginal and not the whole call.
  The baseline tier pays **2 driver round-trips per 20 000-element call**, not per element: the walk's `call_only` is linked in-block, so the loop never returns to the driver.
  The interpreter's extra BIF call per element is the small-int `+` the baseline tier inlines — the same loop, two different mechanisms, so the two tiers' figures are not comparable.
- `long_append`: **98 318 instructions per call** (6 per appended element), rooted **16 386 calls / 81 933 words** (the 5-word `bs_start_match3` match context per iteration, **not** the accumulator), 16 385 local-call requests, **0** heap-door calls (the fixture has no `test_heap`/`allocate_heap`), **0** BIF calls (asserted) and **1** accumulator payload.
  Those counters are identical on the dev and release profiles.
- `send_split`: every `*_per_op` counter is identical in every round and on all three legs — 1 send, 4 `receive_next`, 2 `receive_remove`, 0 skips/resets, 2 parks, 2 resume slices, 2 heap-door calls / 6 words, 3 local-call requests, 0 external-call requests, `floor_declines == 0` — so only the nanoseconds move, which is exactly what makes the split visible.
  The baseline tier adds its structural counters (`driver_round_trips`, `jit_step_one` for the `Bif0`, `driver_interp_steps = 0`, `in_block_calls`, `block_entry_hits`, `ext_call_* = 0`).
- The in-block call/return link counters explain the driver figure beside them: each in-block call is a driver round-trip the native tier did **not** pay.

## Headline figures (owned elsewhere)
The numbers this node quotes; every table, round and per-case ratio lives in `../../MEASURED.md`, `../../PERF.md` and `../../../../tools/bench/RESULTS.md`.
- `list_walk` on the default tier: ~**57.3 cycles/element** against the system oracle's ~**9.25**, i.e. a ~**6.2×** gap on that shape — the pure dispatch floor.
- On the baseline tier the **emitted code is under a fifth** of the `perf` samples; the tier's per-helper crossings (`jit_call_store`, `local_call_seam`, `native_entry`, the frame helpers) plus the driver loop are the rest.
- A **census-recording** run pays the recorder body per interpreted instruction; a plain run pays one load of the latched `Machine::census_on` bool plus a branch, and touches no thread-local.
- The foreign residual ranks as (1) the per-instruction tax of a **non-inlined** family (`families::dispatch`, i.e. the helper round-trip + re-decode), (2) the byte-at-a-time `BitWriter::push_bit` in `bs_create_bin`, (3) the match engine — in that order, and it is **not** the foreign Erlang itself.
- `send_split`: instruction dispatch and the runtime hops are **identical on all three legs**, so they price the absolute and never the marginal; the marginal is dominated by the payload's residency path outside this crate (`beam_message`'s copy/sizing of the now-unshared object graph, `beam_codec`'s image, the GC cadence they induce).
- `perf` shares are a **ranking** of mechanisms, never ns: convert them only as order-of-magnitude, and read them with four caveats — `families::dispatch` is the per-instruction tax of a non-inlined family with the router inlined (not a lookup), `jit::baseline::drive` is ~20 instructions + 2 indirect calls per **block entry** (not a hot loop body), `call_registered`'s per-BIF self is dominated by the copy-out (a `term_to_binary` binary counts as a foreign container), and `RuntimeContext`/`DispatchMemo`/`enter_module` at 0.00 % is inlining, not absence.
- Per element, `list_walk` crosses a handful of `extern "C"` helpers, essentially zero driver-loop iterations, roughly one `reduce` charge per block entry and in-block call, **no** local-call-seam host consultation (its verdict table is filled once per callee per run) and one `Rc` refcount RMW pair.

## Open levers (what to remove first)
One line per mechanism, with its owner; no figures here — every ns/element, ns/op and sample share lives in `../../MEASURED.md`.
1. **`return` is an unconditional driver round-trip** — `return_word` answers a *word*, never a block address, so `jit_deallocate_return` hands back `encode_target(func, pc)` and the driver re-enters via `native_entry`.
   Fix: give `JitRuntime` the active function's `entries` base and answer the **block entry address** for a same-function continuation, falling back to the word for `Halt`/cross-context/`sub_entries`-only; a same-function `call` is already linked in-block, so only the return round-trips.
   Owner `beam_engine` (`jit/baseline/runtime.rs`, the driver, `jit/baseline/code.rs`); reduction granularity shifts, which is an accepted residual (`tests/jit_reductions.rs`).
2. **`Machine::local_call_seam`** — the host consultation is already memoised once per distinct callee per run and the per-call body is one flag read plus one bounds-checked slot load; what is left inside the seam is the inlined invalidation walk, so the next step is `#[cold]`/`#[inline(never)]` on `Machine::invalidate_local_call_verdicts`.
   Owner `beam_engine`; detail in `../../machine/CONTEXT.md`.
3. **`beam_term::size`'s sizing walk + the `HashSet` path set on a REFUSED send** (write-and-discard, rollback, 3 walks + `is_acyclic`, a re-run of the copy, a 4th `size_object`).
   The set is removable because a `limit`-bounded walk already terminates on a cycle: every entered object costs ≥2 words, so `total > limit` fires, and the `Cyclic` verdict is re-derived by `is_acyclic` on the `OverLimit` arm.
   Owner `beam_term`/`beam_message`; the real trigger is the trial failing at all, so removing the set only shrinks the penalty path.
4. **`jit_call_store` re-decodes the instruction and re-resolves a label the emitter already resolved** — pass `target_func`/`arity` as immediates (`emit_helper_call` already fills `rsi/rdx/rcx/r8/r9`).
   Owner `beam_engine` (`jit/baseline/call.rs`, emitter `jit/baseline/emit/block.rs`).
5. **The bitstring family is never inlined** — every `Bs*` pays a helper, a re-decode and a double match.
   Two steps: bake a family id + operand-slice index at compile time (mechanical), then inline the `bs_match`/`bs_get_integer` loop (needs lever 1's register-bound discipline). Do **NOT** inline `bs_create_bin`.
   Owner `beam_engine` (`jit/baseline/emit/`, `jit/baseline/family.rs`, `families/mod.rs`).
6. **`BitWriter::push_bit` is bit-at-a-time** (8 stores per `<<C:8>>`): a `push_uint` at a byte-aligned position with `width % 8 == 0` **IS** `push_whole_bytes` (C's `erts_bs_put_integer_*` stores whole bytes), routed through `ensure_room`/`source_overlaps`.
   Owner `beam_engine` (`families/bitstrings/`).
7. **`jit_publish_y` per block prologue** — only needed when the top frame's Y window can move (`push_frame`/`pop_frame`/`trim`, whose mover is a `Vec` realloc); a slice-reserved stack degenerates it to the position store.
   Owner `beam_engine`; the "null window rejects every Y access" guard is load-bearing.
8. **`beam_rt`'s `DispatchMemo`** — a roster `binary_search` plus two `String` allocations per key, per slice *and* per nested send/fun.
   Hoist it onto `ProcessEngine`/`RunnerState`, reuse it in `FunBridge::nested`/`SendBridge`, and key by atom ids to drop both `String`s; every input is immutable for the runtime's life (the one mutable input, the NIF registry, is consulted before `dispatch`).

Additionally provable per-call redundancy, worth naming but not worth a lever: `CompiledModule::function`'s `Rc::clone`/drop plus a `RefCell` borrow per entry for an artifact alive for the whole run, and `set_current_code` written three times per element with the same value.

## Known Issues
- **The harness delegates the copy engine**: `MinimalContext` runs no `copy_struct` (the `Mailbox` seam hands the message over verbatim), so `send_split` prices only the engine side of the marginal; the copy/sizing buckets are measured on the runtime route and are `perf`-derived there.
- **No collector runs here**, so no figure from this harness says anything about GC cadence or promotion; `send_split` reports the induced-GC bucket as unattributed rather than guessed.
- **A census row exists only for a module's FIRST case** (the pass runs once per loaded artifact), so `capitalize_token` and `decode` have no per-call driver/block census row.
- **The growth-shape probe prints rather than asserts** and is tree-aware: on a tree that took the copy-rebuild route it reports one payload per append with all-zero growth counters instead of failing.
- **`perf` cannot price the boot phase**: `gc_walk`'s share of samples moves inversely with N while its absolute time stays flat, so a `perf` profile of a short run over-attributes it. Never read a short run's `perf` shares as per-element costs.
- **Wall-clock rows are load-sensitive**: on a shared host the per-round min still spreads tens of percent and individual per-round marginals can invert sign or spike, so only ratios and deterministic counters are quotable.
- **Real fixtures need the system toolchain** (`erlc`, and for the foreign cases the checkouts): absent them every workload SKIPs loudly rather than failing — a green run without `erlc` proves nothing about any figure here.

## Constraints / Notes for Agents
- **`MinimalContext` opts out of the reduction hook** (`wants_reduce() == false`), so `CountingContext` MUST override it to `true` or the instruction count silently stays 0.
- **A batch's argument is setup, never measurement.** `run_calls` parks the whole `Counters` set across the `Prepare` call and snapshots `Machine::refc_binaries().len()` after it, so a fixture that builds its own input *through* the engine contributes neither instructions nor payloads to the tally (as `long_append` must — `make_heap_binary` refuses > 64 bytes). Never move fixture setup inside the timed loop.
- **No collector here.** `MinimalContext` only grows its reserved heap, so this harness measures the engine, never GC cadence — and for these shapes a collection-free figure is faithful: `bstr_to_lower` reaches **no** heap-admission point at all, `long_append` reports `heap_door_calls_per_call = 0`, and `list_walk`'s loop has neither `test_heap` nor `allocate_heap` (see `../../../../crates/beam_rt/src/runner/CONTEXT.md` §Measured and `../../../../crates/beam_rt/M1_GAPS.md`).
- **The floor's `erlang:bit_size/1` is the bitstring family's reader, never a local one.** `decode/1` needs it in a guard and over a `t_bs_context`, and a half-correct `bit_size` would turn a guard failure into a silently **wrong** decode — so the floor calls `crate::families::bitstrings::bit_size` (C's `erts_bin_bits_size`: heap bits, an off-heap refc window, an `ErlSubBits` match context) and `refuse`s a non-bitstring, which surfaces as a `floor_declines` assertion failure rather than a figure. That is also why `src/bench/` needs no `unsafe` of its own.
- **Never `unwrap()` a measurement into an assertion of speed.** The `#[ignore]`d tests assert only cheap invariants — correct bytes on both tiers, the tiers agreeing, the expected BIF-call count, `floor_declines == 0`, and (long-append, on a growing tree) `bytes_copied < 8 × n` — and **no timing**, so they never fail on a loaded host.
  `send_split`'s gate (`the_two_legs_of_the_copy_bound_split_agree`) follows the same rule: 16 sends per process, every message consumed, `floor_declines == 0`, exactly 2 crossings/op on the `codec` leg, the heap legs building through the rooted door — and no timing.
- **The other non-`#[ignore]`d test is a structural gate on the fixture, not a measurement**: `every_bstr_select_val_pair_is_decided_without_a_general_compare` asserts that on both tiers every `select_val` pair of `cowboy_bstr:to_lower/1`'s 27-clause `char_to_lower/1` (one `select_val` over 26 small-int alternatives) is decided by **word identity**, with `cmp_pairs == 0` and a non-zero `word_pairs` on the interpreter — so the measured ladder really is a jump table over unboxed `Operand::Integer` constants and not a `beam_term::compare` ordering.
  It also asserts the fixture's result bytes on both tiers, skips loudly without `erlc`, and never times anything.
- **Batch lifecycle is load-bearing.** `Machine::refc_binaries` and the `MinimalContext` region heap grow monotonically within a batch and are never pruned here (only a host reachability sweep, `Machine::retain_refc_binaries`, or dropping the machine releases them), so every batch builds a fresh `Machine` + context (500 calls by default for the foreign cases, 5 for `long_append`, 10 for `list_walk`) and drops both at batch end.
- **The census pass reloads the module** (a second `load_file`): the per-thread compiled-block cache would otherwise answer the pass from an artifact compiled while recording was off, leaving `instr_instances` empty.
- **Never edit the foreign sources or copy them in.** Both are staged read-only from their checkouts at test time (`cowboy_bstr.erl` is ISC © Loïc Hoguin; `emqx_base62.erl` is EMQ copyright).
- **Do not re-investigate the `foreign` regression.** The Θ(N³) reservoir re-registration is gone, and `../../../../tools/bench/RESULTS.md` §8 withdraws the pre-m15 `foreign` figures (different `N`, estimator fix).
- Run everything through `erts_rust/scripts/cargo.sh` (`$HOME` is read-only); never `cd`, and never touch the read-only siblings (`../../../../tools/**`, the foreign checkouts).

## Routing Table
- `mod.rs` → the shared harness (`compile_fixture`/`compile_staged`, `Prepare`, `run_calls`, `measure`/`measure_result`, `Measurement` + `ResultSnapshot`, `instr_per_call`, `ns_per_call`, `env_usize`, `selected_tiers`, `ReportSpec`/`report_generic`), the `cowboy_bstr` fixture, its `#[ignore]`d entry point `foreign_bstr_to_lower_decomposition`, and `baseline_census`/`print_census` (the per-thread `../jit/baseline/counters.rs` snapshot pass, with an optional `&Unit` for per-unit rows).
- `counting.rs` → `CountingContext` (the delegating counting `ExecutionContext`), the closed BIF floor (`host_bif`), `BifHistogram`.
- `list_walk.rs` → the BIF-free per-element dispatch floor (`LEN = 1000`, `BENCH_K`); entry point `list_walk_decomposition`, plus the whole-call and paired per-element marginal reports.
- `long_append.rs` → the growing-accumulator fixture (`N_ARG = 8 * 1024`); entry point `foreign_long_append_decomposition`, its report and the growth-shape probe (`Machine::refc_binaries()` → `capacity`/`reserved_bytes`/`growth_counters`).
- `foreign.rs` → the four staged foreign cases (`BENCH_COWBOY_REPO`/`BENCH_EMQX_REPO`) and their two entry points, `foreign_cowboy_bstr_decomposition` and `foreign_emqx_base62_decomposition`.
- `send_split.rs` → the `send_recv − send_recv_plain` split instrument: the three legs (`plain`/`heap`/`codec`), entry point `send_recv_split_decomposition` and its structural gate `the_two_legs_of_the_copy_bound_split_agree`.

## See Also
- `../../MEASURED.md` (static/dynamic census, `engine_bench`, `bif_cost`, `jit_bench`, the lever figures) and `../../PERF.md` (tier snapshots) → every measured figure this harness produces.
- `../../../../tools/bench/RESULTS.md` → the project-level record whose per-case ratios these in-crate figures decompose; the harness' runtime-route cross-checks were a decomposition at a different N/session, never a replacement for that record.
- `../../INSTRUCTION_SURFACE.md` → the per-opcode/per-family semantics these figures price.
- `../CONTEXT.md` → the crate's frozen seams (`ExecutionContext`, `Mailbox`, the JIT tier seam) — the harness drives exactly those doors and counts nothing else.
- `../../../../crates/beam_rt/src/context/gc_walk.rs` + `src/context/gc/reds_door.rs` → the one place `gc_walk` is driven from.
