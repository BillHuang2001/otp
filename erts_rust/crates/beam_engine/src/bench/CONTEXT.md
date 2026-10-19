# `beam_engine::bench` — in-process foreign-workload decomposition harness

## Intent
A `#[cfg(test)]`-only harness that measures real external benchmark workloads **in process**, so the engine's own per-call cost is decomposed without the runtime's CLI boot (≈0.70–0.77 s, i.e. **35–45 %** of an unpaired `beam_rt` per-op figure), its module loading, its scheduler and its copy-out glue inside the timed loop.
Four workload shapes, one methodology:
1. `list_walk` — the `tools/bench` `list_walk` program (`bench_list_walk:walk/1`, `LEN = 1000`): a BIF-free body-recursive `sum/1`, i.e. the pure **dispatch floor**, with no allocation in the timed loop.
2. `cowboy_bstr:to_lower/1` and `cowboy_bstr:capitalize_token/1` — binary comprehensions whose accumulator **fits** the compiler's `bs_init_writable` hint, so they measure the plain build + dispatch floor on real foreign code.
3. `long_append:expand/1` — a binary accumulator that genuinely **outgrows** its hint, so it measures the writable payload's **growth** path (`BitWriter::ensure_room` → `Machine::grow_writable_payload` → `beam_alloc`'s `RefcBinary::ensure_capacity`).
4. `emqx_base62:encode/1` and `emqx_base62:decode/1` — bitstring matching + creation over a caller-sized input; `decode/1` is the one case that needs `erlang:bit_size/1` (a guard plus the accumulator's size), which the harness' own BIF floor serves.
The foreign sources are **staged** from their read-only checkouts at test time (`compile_staged`, system `erlc +deterministic`); this repository stores no copy that could drift.
It is absent from production builds: `../lib.rs` declares `#[cfg(test)] mod bench;`.

## API Surface
- `mod.rs` → the **shared harness** plus the `cowboy_bstr` fixture.
  - `compile_fixture(module, source, tag)` / `compile_staged(module, source_path, tag)`: fixtures compiled at test time with the system `erlc`; every workload **skips loudly** when `erlc` (or the checkout) is absent.
  - `Prepare` (the closure that builds a batch's argument, run **outside** the tally), `run_calls` (the batch loop), `measure`/`measure_result`, `Measurement` (`min`/`median`) + its `ResultSnapshot`, `instr_per_call`, `ns_per_call`, `env_usize`, `selected_tiers`.
  - `ReportSpec`/`report_generic`: the one report body every workload uses, in per-call or per-unit rows.
  - `baseline_census`/`print_census`: the per-thread `../jit/baseline/counters.rs` snapshot pass; they take an `Option<&Unit>` — `None` prints the per-call driver/block figures (the foreign cases), `Some` adds their **per-unit** twins (`driver_round_trips_per_element`, `block_entry_lookups_per_element`, …) for a workload that is not call-shaped. Both forms print the in-block call/return link counters (`in_block_calls`, `in_block_call_declines`) beside `driver_interp_steps`, because they explain the `driver_round_trips` figure next to them.
- `counting.rs` → `CountingContext`, a delegating `ExecutionContext` over `MinimalContext` that tallies `reduce` (= instructions executed, charged once per interpreted instruction), `alloc_words_rooted` calls/words (the per-built-term door), the `ensure_heap`/`ensure_heap_full` door (the `test_heap`/`allocate_heap` door), `call_bif` plus a `module:function/arity` histogram, `apply`/`apply_fun`, `alloc_words`, and the three resolver seams.
  It also carries the harness' own **closed BIF floor** (`erlang:iolist_to_binary/1`, `erlang:bit_size/1` — served through the bitstring family's own frozen-layout reader `crate::families::bitstrings::bit_size`, i.e. C's `erts_bin_bits_size` — and small-int `+ - * div rem abs`) with a `floor_declines` counter that every report asserts to 0, so an unmeasured BIF cannot silently change a workload.
  The histogram key is `BifHistogram`, whose `record` is **allocation-free and hash-free** on the dispatch path (a memo of the last key, then a pairwise scan of the few distinct keys); the `"module:function/arity"` display strings are built once, by `to_map`, at report time.
- `list_walk.rs` → the dispatch-floor fixture (`LEN = 1000`, `BENCH_K` walks per call), entry point `list_walk_decomposition`, the whole-call report and the paired per-element marginal report.
- `long_append.rs` → the growing-accumulator fixture, entry point `foreign_long_append_decomposition`, its report and the growth-shape probe over `Machine::refc_binaries()`.
- `foreign.rs` → the four real foreign cases staged from the read-only checkouts (`BENCH_COWBOY_REPO`, default `/home/bill/Source/cowboy` → `src/cowboy_bstr.erl`; `BENCH_EMQX_REPO`, default `/home/bill/Source/emqx` → `apps/emqx/src/emqx_base62.erl`), entry points `foreign_cowboy_bstr_decomposition` and `foreign_emqx_base62_decomposition`; one loaded artifact per module serves its cases.
- Env knobs: `BENCH_ITERS`, `BENCH_BATCH`, `BENCH_WARMUP`, `BENCH_TIER` (`interpreter`|`baseline`|`both`), plus `BENCH_K` (walks per call, `list_walk` only).
  Defaults per workload: `bstr_to_lower` and the foreign cases 20 000 / 500 / 2 000; `long_append` 50 / 5 / 5 (a call copies 64 KiB out); `list_walk` 100 / 10 / 10 with `BENCH_K = 20` (one call walks 20 000 elements).

## How to run
```text
# every entry point, dev profile (opt-level = 1); each SKIPs loudly if erlc/the checkout is absent
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_bstr_to_lower_decomposition
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_long_append_decomposition
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture list_walk_decomposition
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_cowboy_bstr_decomposition
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib \
    -- --ignored --nocapture foreign_emqx_base62_decomposition

# release: per-instruction cost is ~2x lower than the opt-level=1 dev profile — use release for ratios
./erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib --release --no-run
taskset -c 3 erts_rust/target/release/deps/beam_engine-<hash> --exact \
    bench::list_walk::list_walk_decomposition --ignored --nocapture

# profile a loop itself (the erlc child is filtered out by --comm)
perf record -g -o "$TMPDIR/p.data" -- taskset -c 3 <binary> --exact <test path> --ignored --nocapture
perf report --stdio -i "$TMPDIR/p.data" --no-children --comm=bench::foreign_ \
    --percent-limit 0.8 --sort symbol
```
Entry points in a 3000-series run: `mod.rs:172` (`foreign_bstr_to_lower_decomposition`), `long_append.rs:188`, `list_walk.rs:133`, `foreign.rs:184`, `foreign.rs:195`.

## Workload shapes (verified with `erlc -S`)
### `cowboy_bstr:to_lower/1` — accumulator fits its hint
`to_lower/1` is a **binary comprehension**, which the compiler lowers to the same accumulator shape a hand-written `<<Acc/binary, (char_to_lower(C))>>` produces: `bs_init_writable` of `{integer,256}` (32-byte capacity) plus the tail loop `'-to_lower/1-lbc$^0/2-0-'/2`, which per character runs `bs_start_match3`, `bs_match` (`ensure_at_least 8`, `integer 8`), a **local** call to `char_to_lower/1` (a 27-clause pure-Erlang `select_val`, no BIF), `bs_create_bin` with `[private_append, {tr,{y,1},{t_bitstring,8,true}}, all, integer 8 {x,0}]`, and `call_last 2`.
**438 instructions and 30 characters per call**, so the 30-byte result fits the 32-byte hint on every call and the payload never grows.
`capitalize_token/1` has the same shape at 30 characters / **660 instructions** (it also calls `erlang:list_to_atom/1`-free `to_lower/1` internally and does a `select_val` per character).

### `long_append:expand/1` — accumulator outgrows its hint
```
input(N)  -> <<16#00112233445566778899aabbccddeeff:N/unit:8,
               16#ffeeddccbbaa99887766554433221100:N/unit:8>>.
expand(B) -> << <<C:8,C:8,C:8,C:8>> || <<C:8>> <= B >>.
```
- **BIF-free by construction** (the inner host rejects every BIF): `input/1` is one `bs_create_bin` of two dynamic-size `integer` segments (an `integer` segment of `N * unit` bits encodes the value zero-extended, so each half is `N` bytes with a distinct 16-byte marker near its end), and `expand/1` appends four `integer 8` segments per input byte.
- `erlc -S` on `expand/1` gives `{move,{integer,256},{x,0}}. bs_init_writable.` (a **256-BIT = 32-byte** hint) and, in the comprehension body `'-expand/1-lbc$^0/2-0-'/2`, `bs_start_match3` + `bs_match` (`ensure_at_least 8`, `integer 8`) then `bs_create_bin` whose **first** segment is `{atom,private_append}, {tr,{x,1},{t_bitstring,32,true}}, {atom,all}` followed by four `integer 8` segments and a tail `call_only 2` — exactly C's writable-accumulator idiom (`erl_bits.c:1965-2020`).
- The hint is **not** an estimate of the result: the argument's size is unknown to `expand/1`, so the compiler falls back to its 256-bit default.
  Building the input *inside* `expand/1` would make the hint `bit_size(Bin) * 4` (exact) and nothing would grow — that is why `input/1` is a separate function, and why a **list** comprehension is the wrong shape (the compiler emits a `length/1`-derived hint).
- Sized to `N_ARG = 8 * 1024` → **16 384 bytes in, 65 536 bytes out (64 KiB), 16 384 appends, 2048× the hint**.
  `input/1` runs once per batch as *setup*: the counters are parked across it and the payload roster is snapshotted after it, so neither its instructions nor its payload land in the tally.

### `list_walk` — the BIF-free dispatch floor
`bench_list_walk:walk/1` builds a 1000-element list with a recursive `build/2` and then runs a body-recursive `sum/1` over it; one measured call is `BENCH_K` walks.
The walk loop body never allocates, never crosses a heap-admission point and never dispatches a BIF (`'+'` on two small ints is an immediate); it is 8 instructions per element, one of them a `call_only` to the loop itself.
`build/2` is setup-shaped cost inside the measured call, which is why the per-element figure is reported as the **paired marginal** `walk(K) − walk(0)` (see below).

### `emqx_base62` — bitstring matching + creation over a caller-sized input
`encode/1` folds four 6-bit groups per output character with a `select_val` character table and calls `erlang:iolist_to_binary/1` once per four encoded characters (3 static call sites, **4 calls** for this 12-byte input), so it is the one foreign case that is **not** BIF-free and reports a real `bif_calls` histogram.
`decode/1` additionally needs `erlang:bit_size/1`, both in a guard (`bit_size(Rest) >= 8`) and as `bit_size(Acc) rem 8` over its bitstring accumulator (which `erlc -S` types as a `t_bs_context`); the harness' BIF floor serves it through the bitstring family's own reader, which is exact for heap bits, an off-heap refc window and a live match context and refuses a non-bitstring (see Constraints).

## Measured
Every figure below is this harness' own output unless a section says otherwise.
The two rounds are two independent passes of the same binary (commit `786b96ba32`), `--test-threads=1`, one process at a time under `setsid -w taskset -c 3`, release profile.
**The host was not quiet:** `/proc/loadavg` 1-min 8.42 → 8.39 (round 1, list_walk) / 8.60 → 8.60 (cowboy) / 9.19 → 9.19 (emqx), and 9.33 → 8.81 (round 2, all three back to back); `long_append` ran at 6.37.
Read the deterministic counters and the `ns per instruction` column; never the absolute wall times.

### Per-workload attribution (whole call)
`list_walk`: `BENCH_K=20 BENCH_ITERS=1000 BENCH_BATCH=50 BENCH_WARMUP=50`; the foreign cases: `BENCH_ITERS=50000 BENCH_BATCH=500 BENCH_WARMUP=5000`; `long_append` at its defaults (`50 / 5 / 5`).

| workload | tier | min / median ns per call (R1 · R2) | instructions/call | ns per instruction (R1 / R2) |
|---|---|---|---|---|
| `list_walk` (20 000 el.) | interpreter | 2 321 030 / 2 372 867 · 2 264 711 / 2 272 057 | 169 418 | 13.70 / 13.37 |
| `list_walk` (20 000 el.) | baseline | **466 821 / 469 133** · 474 145 / 478 177 | 169 418 | 2.76 / 2.80 |
| `to_lower/1` | interpreter | 10 664.8 / 10 723.7 · 10 389.7 / 10 462.0 | 438 | 24.35 / 23.72 |
| `to_lower/1` | baseline | **4 802.5 / 4 844.8** · 4 682.3 / 4 693.0 | 438 | 10.96 / 10.69 |
| `capitalize_token/1` | interpreter | 13 907.7 / 14 155.9 · 13 816.8 / 13 858.6 | 660 | 21.07 / 20.93 |
| `capitalize_token/1` | baseline | **6 596.2 / 6 769.7** · 6 631.0 / 6 778.1 | 660 | 9.99 / 10.05 |
| `base62:encode/1` | interpreter | 4 912.7 / 4 915.3 · 5 325.3 / 5 355.4 | 244 | 20.13 / 21.82 |
| `base62:encode/1` | baseline | **3 064.1 / 3 075.6** · 3 307.2 / 3 331.9 | 244 | 12.56 / 13.55 |
| `base62:decode/1` | interpreter | 9 512.0 / 9 578.1 · 10 350.4 / 10 487.5 | 531 | 17.91 / 19.49 |
| `base62:decode/1` | baseline | **5 600.5 / 5 623.3** · 6 029.6 / 6 072.4 | 531 | 10.55 / 11.36 |
| `long_append:expand/1` | interpreter | 2 527 170 / 2 558 207 | 98 318 | 25.70 |
| `long_append:expand/1` | baseline | **1 961 612 / 2 033 860** | 98 318 | 19.95 |

`ns per instruction` is the discriminating column across a loaded host: the three bitstring-heavy foreign cases sit at **10–14 ns/instruction** on the baseline tier while `list_walk` sits at **2.8**, so a per-call wall time alone would misattribute the difference to dispatch.
`long_append`'s 25.7 / 20.0 ns per instruction against `list_walk`'s 13.7 / 2.8 is the load band, not a mechanism — its deterministic counters (below) are the evidence.
`decode/1` is measured here for the first time (the floor now serves `erlang:bit_size/1`); every other figure in this table moved since the `a72107ce2d` round because both tiers' driver/return paths changed, so do **not** compare across the two round labels.

Deterministic counters per call (identical on both rounds; "—" = the census pass does not cover that case — it runs once per loaded artifact, for the module's **first** case, so `capitalize_token` and `decode` have no census row):

| workload | driver round-trips | in-block calls (declines) | block-entry lookups (misses) | rooted calls / words | heap-door calls | BIF calls | local-call requests | refc payloads |
|---|---|---|---|---|---|---|---|---|
| `list_walk` | 2.00 (>0 miss: **0**) | 21.00 (0) | 2.00 (0) | 1 000 / 2 000 | 1 000 / 2 000 | 21 040 interp · **0** baseline | 21 042 | 0 |
| `to_lower/1` | 3.00 | 30.00 (0) | 3.00 (0) | 32 / 163 | 0 | 0 | 61 | 1 |
| `capitalize_token/1` | — | — | — | 63 / 318 | 0 | 0 | 57 | 1 |
| `base62:encode/1` | 7.00 (`step_ext` 4) | 16.00 (0) | 7.00 (0) | 35 / 130 | 4 / 32 | 20 interp · 4 baseline | 21 | 1 |
| `base62:decode/1` | — | — | — | 62 / 283 | 0 | 33 interp · **17 baseline** | 31 | 1 |
| `long_append:expand/1` | — | — | — | 16 386 / 81 933 | 0 | 0 | 16 385 | 1 accumulator payload |

`list_walk`'s `rooted`/`heap-door` figures are `build/2` (one cons cell and one `test_heap 2 2` per element), **not** the walk loop — the marginal table below is the loop.
The `in_block_calls` column is the native tier's in-block call/return link path (`counters::record_in_block_call`): each one is a driver round-trip the tier did **not** pay, which is why `driver_round_trips_per_call` is 2–7 per call rather than the per-element 1.0 the pre-link tier paid.
BIF histogram: `list_walk` `erlang:+/2` 20 020 000 + `-/2` 1 020 000 over 1 000 calls (interpreter tier only — the baseline tier inlines small-int arithmetic, so it dispatches zero); `encode` `+/2` 800 000 (16/call) + `iolist_to_binary/1` 200 000 (4/call); `decode` `-/2` 800 000 (16/call) + `bit_size/1` 800 000 (16/call) + `rem/2` 50 000 (1/call) on the interpreter, and `bit_size/1` 800 000 + `rem/2` 50 000 on the baseline tier (which inlines the `-`).
`bif_floor_declines_per_call = 0` and `result_is_correct = true` on every case, and the two tiers agree on the result bytes.

### `list_walk` — the per-element marginal (the engine figure)
Paired `walk(20) − walk(0)`: one `build/2` in both phases, so the build (9 018 instructions) cancels.

| quantity | interpreter (R1 / R2) | baseline (R1 / R2) |
|---|---|---|
| `ns_per_element` | **109.92 / 107.04** | **21.57 / 21.94** |
| instructions per element | 8.020 | 8.020 |
| rooted allocations per element | 0.0000 | 0.0000 |
| heap-door calls per element | 0.0000 | 0.0000 |
| BIF calls per element | **1.0020** | **0.0000** |
| local-call seam requests per element | 1.0020 | 1.0020 |
| driver round-trips per element | — | **0.0001** (2 per call) |
| in-block calls per element | — | 0.0010 (21 per call) |
| block-entry lookups / hits per element | — | 0.0001 / 0.0001 (misses 0) |
| `step_one` / `step_ext` / driver interp steps per element | — | 0 / 0 / 0 |
| deopts per element | — | **0** |
| `census_ns_per_element` (the census pass' own loop) | — | 24.55 / 24.16 |

Whole-call `instr_per_element` is **8.471**, so the build is 5.3 % of the call.
The baseline tier pays **2 driver round-trips per 20 000-element call** (not per element): the walk's `call_only` is linked in-block, so the loop runs without returning to the driver, and the interpreter's extra BIF call per element is the small-int `+` the baseline tier inlines.

### `long_append` — the growth path amortises
Deterministic counters per call (release profile; identical on both rounds and on the dev profile): `instr_per_call` **98 318** (6 per appended element), `rooted_calls/words` **16 386 / 81 933** (the 5-word `bs_start_match3` match context per iteration, **not** the accumulator), `local_call_requests` 16 385, heap-door calls 0 (the fixture has no `test_heap`/`allocate_heap`), BIF calls 0 (asserted), `accumulator_payloads_per_call` **1** (one writable payload per call, however many appends).

The accumulator payload after one call (`Machine::refc_binaries()`, printed by the growth-shape probe):

| `len` | `capacity` | `reserved` | `in_place_grows` | `moves` | `bytes_copied` |
|---|---|---|---|---|---|
| 65 536 B | 65 536 B | 65 536 B | 2 | 6 | **64 512 B** |

`bytes_copied ≈ n` with `moves` = 6 (O(log n)) **is** the amortisation proof, and it is structural — no wall clock involved.
The copy-rebuild form this path replaced (`≈ n² / (2 · 4)` = **536 903 680 B** for this build, printed as `copy_rebuild_bytes_counterfactual`) would copy ~**8 300×** more.

### Why the harness' own bookkeeping is allocation-free
The histogram key used to be a `format!("{module}:{function}/{arity}")` **per dispatch**; `perf` self time (interpreter tier) put that family at **14.2 %** of `list_walk`'s samples (`format_inner` 5.66 + `write_str` 4.93 + `core::fmt::write` 3.63) and **7.5 %** of `encode`'s — the harness was inflating the figure it exists to report.
`BifHistogram` (memo of the last key + pairwise key compare; display strings only at report time) removes it: no `format`/`write_str`/`core::fmt::write`/`finish_grow` frame appears above perf's 0.5 % cut-off in either workload, and the only string-comparison residue is `__memcmp_avx2_movbe` at **2.18 %** of `list_walk` (0.92 % before — the key compare plus the engine's own name comparisons).
Paired A/B, two identical-source binaries, `BENCH_TIER=interpreter`, `/proc/loadavg` 39.9–43.6: `list_walk` 3 540 945 → **2 482 150** ns/call (20.90 → **14.65** ns/instruction, **1.43×**), `encode` 8 835 → **7 079** (36.21 → **29.01**, **1.24×**), every deterministic counter identical.

### Where the samples go, tier by tier (`list_walk`, perf self time)
| frame | interpreter | baseline |
|---|---|---|
| `step_instruction_body` | **21.75 %** | — |
| `step_instruction_with_entry` | 21.10 % | — |
| `Machine::drive` | 13.12 % | 8.91 % (`jit::baseline::drive`) |
| `call_bif` + `exec_bif` | 7.10 + 6.49 % | — |
| `exec_census::on_instruction` | 6.27 % | — |
| `families::dispatch` | 3.89 % | 2.28 % |
| `local_call_seam` | 3.79 % | **10.74 %** |
| `jit_call_store` | — | **11.85 %** |
| `jit_publish_y` (+`jit_refresh_y` on the runtime route) | — | 8.34 % |
| `native_entry` | — | 7.55 % |
| `jit_deallocate_return` | — | 7.12 % |
| `jit_allocate` | — | 4.68 % |
| `enter_compiled` | — | 3.69 % |
| emitted code (aggregate) | — | **18.06 %** |
| `reduce` / `return_step` / `eval_test` | 1.95 / 1.33 / 1.22 % | — |
| `__memcmp_avx2_movbe` | 2.18 % | — |

`exec_census::on_instruction`'s **6.27 %** is the recorder's own body, i.e. the price of a
**census-recording** run: the funnel only reaches it when the run's latched `Machine::census_on` gate is
set. A plain (non-recording) run pays one load of that latched `bool` plus a branch per interpreted
instruction and never calls into the instrument, and no thread-local is touched on that path.
On the baseline tier the emitted code is **under a fifth** of the samples: the tier's per-helper crossings (`jit_call_store`, `local_call_seam`, `native_entry`, the frame helpers) plus the driver loop are the rest.

### Foreign cases — the mechanism ranking (runtime route)
Shares are perf self time of the profiled run (`--comm`-filtered), so they are a **ranking** of mechanisms, and the ns/call conversions below are order-of-magnitude.
`to_lower` @8.49 µs/call: `families::dispatch` **12.6 %** (1 068 ns) › `BitWriter::push_bit` **6.5 %** (551 ns = 18.4 ns/char ≈ 2.3 ns/bit) › `bitstrings::matching::exec` **4.9 %** (419 ns) › `jit_family` **4.6 %** (387 ns) › `match_ctx::read_*` 2.8 % (236) › `read_uint_be` 2.2 % › `local_call_seam` 2.1 % › `read_size` 1.8 % (152).
`capitalize_token` @11.0 µs/call: `dispatch` **11.6 %** (1 275 ns) › `jit_family` **6.0 %** (661) › `matching::exec` **5.9 %** (653) › `push_bit` 4.6 % › `match_ctx::read_c` 4.3 % (475).
`encode`: `dispatch` 7.9 % › `matching::exec` 3.6 %.
`decode`: `dispatch` 8.4 % › `matching::exec` 6.4 % › `push_bit` 5.1 % › `jit_family` 4.1 %.

So the foreign residual is **not** the foreign Erlang: it is the per-instruction tax of a non-inlined family (`families::dispatch` with the router inlined, i.e. the helper round-trip + re-decode), the byte-at-a-time `BitWriter::push_bit` in `bs_create_bin`, and the match engine — in that order.

### `gc_walk` is a boot-phase cost, not a per-element one (`list_walk`)
`perf` attributes 15–38 % to `beam_rt::context::gc_walk` / `gc::word_region` / `beam_gc::nursery::contains`, but the **absolute** time is flat while `N` grows 5×:

| profile | wall | `gc_walk::*` share | absolute |
|---|---|---|---|
| boot only (`-eval 'halt(0).'`) | 0.712 s | 35.95 % | ≈256 ms |
| `run <beam> <mfa> 0` (load, no loop) | 0.728 s | 38.12 % | ≈278 ms |
| `list_walk` N=10 000, default tier | 1.053 s | 23.99 % | ≈253 ms |
| `list_walk` N=50 000, default tier | 2.471 s | 9.45 % | ≈233 ms |
| `list_walk` N=50 000, `BEAM_RT_TIER=interpreter` | 10.081 s | 1.56 % | ≈157 ms |

It is the constant ≈233–256 ms start-up share of a 0.7–0.8 s process — the bench tool's paired N-vs-0 marginal cancels it and `perf` cannot.
**No walk is attributable to an element.** The chain (innermost → outermost): `Walk::run` (`beam_rt/src/context/gc_walk.rs:898`) ← `Walk::seed_process_roots` (`:826`) ← `Walk::seed` (`:764`) ← `word_region` (`src/context/gc.rs:370`, called `gc_walk.rs:772`) ← `valve_verdict` (`gc_walk.rs:291`) ← `any_root_is_unscanned` (`:277`) ← **`collect_at_reds_boundary`** (`src/context/gc/reds_door.rs:186`, valve call `:265`) ← `RuntimeRunner::collect_at_reds_boundary` (`src/runner/run.rs:300`, gate `if !engine.parked_published { return }` at `:308`) ← the **`Yield` park arm** (`run.rs:1188` `park_machine`, `:1198` `serve_gc_request`, `:1228` `RunOutcome::Yield`) ← `RuntimeRunner::run` ← `beam_process::Scheduler` over `BudgetContext` (`src/runner/budget.rs:314`).
**The door that fires is the runner's pre-emption arm `collect_at_reds_boundary`**; `alloc_words_rooted`, `ensure_heap_full`, `ensure_heap`, `call_bif` and `reduce` are never the caller of a walk.
Frequency: `LEN = 1000` elements and ≈1 000 reductions per walk (`DEFAULT_REDUCTIONS_PER_SLICE = 4 000 = beam_common::CONTEXT_REDS`, `beam_process/src/scheduler.rs:107`; `RUN_STEP_SLICES = 1_000`, `beam_rt/src/runtime/scheduler.rs:99`) ⇒ **one pre-emption per 1–4 walks**, so one boundary-gate evaluation per ≈1 300–4 000 elements, each O(1).
The gate itself returns early after `build/2` (`reds_door.rs:236-246`): the occupancy test needs `used > 0` at 7/8 capacity (`BOUNDARY_OCCUPANCY_NUM/DEN`, `:143`/`:145`) and `collection_debt` is cleared by the collapsed collection, and the promoted list leaves `used == 0`.
What the loop *does* pay per pre-emption is root-set **publication**: `publish_parked_roots` (`run.rs:534` → `context/gc.rs:787`) copies `x[..live]` + the whole stack (≈1 000 words) + the FR prefix into `Registers` and `resync_parked_roots` copies back (≈12–38 M word copies over 5×10⁷ elements = 0.25–0.75 word-ops/element).
There is **no** "root set unchanged since the last walk" early-out and no root-set-level "nursery holds no object" test; the emptiness test is the capacity/occupancy heuristic one level up.
`Walk`'s scratch (`Visited`, a hand-rolled 1024-slot open-addressing table reset by a generation bump) allocates nothing per walk after warm-up.

### Cross-checks on the runtime route (a decomposition, **not** the `tools/bench` m17 record)
These were measured with the sibling `tools/bench` driver plus `beam_rt` on the same tree, at a **different N and in a different session** than `tools/bench/RESULTS.md` §3.1, so they are not m17 replacements — they decompose and cross-check it.
Five alternating paired rounds, `RUN_DEADLINE = 60 s` never approached (longest ≈1.4 s):

| case | unit | N (rust) | rust min / median | oracle min / median | ratio (median) |
|---|---|---|---|---|---|
| `list_walk:walk/1` | ns/element | 20 000 | 33 966.9 / 37 748.4 | 9 846.6 / 10 224.3 | 3.64× |
| `to_lower/1` | ns/call | 150 000 | 8 376.3 / 8 490.3 | 580.0 / 604.0 | 14.56× |
| `capitalize_token/1` | ns/call | 125 000 | 10 858.2 / 10 996.0 | 518.7 / 536.0 | 20.95× |
| `base62:encode/1` | ns/call | 125 000 | 6 889.0 / 7 619.9 | 326.0 / 327.2 | 23.33× |
| `base62:decode/1` | ns/call | 110 000 | 11 212.8 / 11 827.6 | 417.7 / 420.1 | 27.05× |
| `etf_roundtrip` | ns/op | 500 000 | 2 026.7 / 2 216.9 | 739.4 / 762.3 | 2.95× |
| `send_recv` | ns/op | 75 000 | 14 417.5 / 15 472.9 | 1 771.7 / 2 024.6 | 7.64× |
| `send_recv_plain` | ns/op | 350 000 | 3 567.5 / 3 705.5 | 733.2 / 743.1 | 4.97× |

Per-round ratio spread 1.1×–1.4×; the median ratio is the statistic.
Boot control: `beam_rt -eval 'halt(0).'` min **704 ms** / median ≈**774 ms** vs `erl -noshell -eval 'halt(0).'` min 176.6 / median ≈180 ⇒ **rust boot ≈4.2× the oracle's**, and boot is **35–45 %** of an unpaired rust per-op figure for every case above (0.1 % for `list_walk`, which amortises it).

#### `send_recv − send_recv_plain` — where the 6.7 µs/op actually is
The two cases are **not** "the same work plus a codec call", which three structural facts settle:
- The payload is a compile-time **literal**: every constant in `payload/0` is literal-able, so the compiler hoists it into `LitT` (`lib/compiler/src/beam_ssa_codegen.erl:2959-2969`; in-repo evidence `beam_loader/tests/fixtures/literals_fixture.erl:9-11`, asserted `beam_loader/tests/literals.rs:25-60`).
  The copier shares literal-area words **by identity**, writing zero destination words (`beam_message/src/copy.rs:261-269` `is_shared`, `:291-295` `push_fixup` skips shared children).
  So `send_recv_plain` copies **1 object / 3 words per direction**, while `send_recv`'s reply carries a **heap-resident** map rebuilt by `binary_to_term`: **28 objects / 73 words**.
- `term_to_binary`'s 101-byte image never becomes a refc binary and never enters `Machine::refc_binaries`: `beam_bifs/src/bits.rs:36-43` builds every binary on the heap, deliberately ignoring the 64-byte placement rule (`beam_bifs/src/access.rs:484-510`, `make_heap_bitstring`), and `binary_to_term` installs no `OffHeapBinaries` host (`beam_codec/src/decode/mod.rs:1126-1131`).
  101 > `ERL_ONHEAP_BINARY_LIMIT = 64` (`beam_term/src/refc.rs:101-107`), yet the image is a 15-word on-heap `ErlangHeapBits` — so the "fresh payload per op → park-arm walk" hypothesis is dead.
- The copy gate fires on **capacity**, not shape: `try_copy_fast` (`copy.rs:620-640`) is eligible whenever `dst_off_heap().is_fully_empty()` (`:608-610`) and fails only on capacity.
  The refusal path (`copy.rs:347-355` → `:695`) writes-and-discards (`:187`), rolls back, runs `preflight_words` (bounded walk + `is_acyclic` + unbounded `size_object_with` = **3 walks**), then `send_fragmented` **re-runs `copy_struct`** (`beam_rt/src/context/send.rs:440-445` → `:583-618`) plus a 4th `size_object` (`beam_message/src/mailbox.rs:139`, retry `:117`).

Five measured deltas (fixture `bench_send_split`, paired N-vs-0, min-based / median-based):

| delta | meaning | value |
|---|---|---|
| `p2−p0` | literal payload + codec in the worker (the published gap) | **+11 568.8** (+12 961.4) ns/op |
| `p1−p0` | literal → heap residency **alone**, no codec | **+5 233.9** (+4 868.3) |
| `p3−p2` | the codec's marginal cost on an already-heap-resident payload | **+10 499.5** (+10 622.7) |
| `p3−p1` | codec alone on a heap payload (cross-check) | +16 834.4 |
| `p2−p1` | literal vs heap at fixed codec | +6 334.9 |

Absolute: p0 3 733.6 / 4 121.7, p1 8 967.5 / 8 990.0, p2 15 302.4 / 17 083.1, p3 25 801.9 / 27 705.8 ns/op ⇒ **≈45 % of the `p2−p0` gap is heap residency, not codec.**
`K`-slope (`k/2`, heap payload, K codec round trips in the worker): K=0 9 058.0; K=1 25 113.0 (**step +16 055.0**); K=2 30 625.9 (+5 512.9); K=4 37 169.3 (**linear ≈+3 271.7 per extra round trip**) ⇒ the codec cost is a **K=0→1 step of +16.1 µs (3× the steady-state slope)** followed by a linear ≈+3.3 µs; the step is activation/copy-out/GC path, and the **codec's own marginal cost is the linear term ≈3.3 µs** (oracle ≈0.7 µs ⇒ ≈4.7× per application).
`collections/op` (`erlang:statistics(garbage_collection)` around the loop; rust / oracle): p0 **0.0513** / 0.0303, p1 **0.2095** / 0.0277, p2 **0.5226** / 0.1613, p3 **0.4857** / 0.1360; k0 0.2095, k1 0.5125, k2 0.7000, k4 1.1000 (oracle 0.0234 / 0.1343 / 0.2225 / 0.3455) ⇒ heap residency alone multiplies the collection rate **4.1×**, the first codec round trip a further **2.4×**, and rust collects **7–9× more often per op** than the oracle.

Bucket split of `p2`'s 15 533 ns/op (N=75 000, loop 1 165 ms, boot subtracted):

| bucket | ns/op | share of the loop | owner |
|---|---|---|---|
| (a) `beam_codec::*` encode + decode | **513** | **≈4 %** | `beam_codec` |
| (b) `beam_message::copy` + `beam_term::size`'s sizing walk + `beam_alloc` | **5 140** | **≈33 %** — the largest bucket | `beam_message` driving `beam_term::size` (`sizing_walk` 3.12 % ≈781 + `size_object_bounded_with` 3.18 % ≈796) with an `is_acyclic` `HashSet<usize>` (`RandomState` 10.64 %, `Sip13Rounds`, `reserve_rehash` 2.05 %) and `CarrierAllocator::alloc_impl` 3.42 % ≈856 |
| (c) engine/runtime machinery (`call_registered` ≈496, `binary_search_by` ≈711, `families::dispatch`, `jit_*`) | 3 258 | ≈21 % | `beam_rt` + `beam_engine` |
| allocation, counted separately from (b) | 846 | ≈5 % | `beam_alloc` |
| **unattributed residual** (stated, not guessed) | **5 775** | ≈37 % | — |
| GC (the walk is not on this path; see the `gc_walk` section) | 0 | 0 % | `beam_gc` |

`ProcessTable::deliver` / `resume_woken` / `schedule_receive_timeout` are **≤0.37 %** ⇒ **the scheduler is not the cost.**
`sizing_walk`'s caller chain: `beam_message/src/copy.rs:347-355` `preflight_words` ← `copy.rs:695`, entered only after `try_copy_fast` failed / `is_fast_path_eligible` is false (`:608`).
That `HashSet<usize>` is a **path set for cycle detection only** (insert-on-enter / remove-on-exit, keyed by raw object address, no dedup — `beam_term/src/size.rs:368-373`, `:386-425`, `is_acyclic` `:448-489`), so it is removable outright: a `limit`-bounded walk already terminates on a cycle because every entered object costs ≥2 words ⇒ `total > limit` fires, and the `Cyclic` verdict is re-derived by `is_acyclic` on the `OverLimit` arm (`size.rs:574-580`).
`copy_out_hint` is per-`RuntimeContext`, hence **per-slice** (`beam_rt/src/context/runtime_context.rs:170/189/207/242`), which is why the copy-out share is a long-slice figure.

### Where the engine-side cost is, ranked (what to remove first)
Numbers are in-crate per-element or the runtime-route per-op conversions above.

| # | mechanism | where | worth | removable |
|---|---|---|---|---|
| 1 | `return` is an **unconditional driver round-trip**: `return_word` answers a *word*, never a block address, so `jit_deallocate_return` returns `encode_target(func,pc)` and the driver re-enters via `native_entry` | `jit/baseline/runtime.rs:1197-1216`, `:1303-1325`; driver `jit/baseline/mod.rs:1012-1025`, `:537-568`; `jit/baseline/code.rs:315-368` | **≈9.9 ns/element (27 %)**, 18.3 % on the runtime route | **Yes** — give `JitRuntime` the active function's `entries` base and answer the **block entry address** for a same-function continuation; fall back to the word for `Halt`/cross-context/`sub_entries`-only. A same-function `call` is already linked (`emit/link.rs:183-193`); only the return round-trips. Reduction granularity changes (already-accepted residual, `tests/jit_reductions.rs`) |
| 2 | `Machine::local_call_seam` is asked on **every** local call but can only answer `true` for module `prim_tty` | engine `interpreter/dispatch.rs:1109-1142`, `:1234-1239`; tier `jit/baseline/call.rs:210`, `jit/baseline/runtime.rs:948`; host `beam_rt/src/context/execution.rs:74-78` → `native/prim_tty.rs:123-125` | **10.74 % → 3.89 ns/element** | **Yes, provably** — the answer is a pure function of `(module,function,arity)` over two `&'static` tables; a per-`Module` bool decided at load collapses it. Keep the `probe.spend()` (reduction accounting) |
| 3 | `beam_term::size`'s sizing walk + the `HashSet` path set on a **refused** send (write-and-discard, rollback, 3 walks + `is_acyclic`, a re-run of the copy, a 4th `size_object`) | `beam_term/src/size.rs:386-425`, `:388`/`:412`/`:394`, `is_acyclic` `:448-489`; caller `beam_message/src/copy.rs:347-355` → `:695`; 4th walk `beam_message/src/mailbox.rs:139`; retry `:117`; fragment `beam_rt/src/context/send.rs:440-445` → `:583-618` | **378–881 ns/op (10–24 %)** | **Mostly** — delete the set (a limit-bounded walk terminates anyway), monomorphise the policy, fuse the two child enumerations, reuse scratch, then kill the duplicate walk+copy. The real trigger is the trial failing at all |
| 4 | `jit_call_store` re-decodes the instruction and re-resolves the label the emitter already resolved | helper `jit/baseline/call.rs:138-176`, body `:181-246` (`:194-207` re-decode, `:204` `labels.resolve`); emitter `jit/baseline/emit/block.rs:666-679` | **11.85 % → 4.29 ns/element** (the re-decode+resolve ≈2 ns) | **Yes** — pass `target_func`/`arity` as immediates (`emit_helper_call` already fills `rsi/rdx/rcx/r8/r9`, `emit/mod.rs:310-325`) |
| 5 | the bitstring family is **never inlined**; every `Bs*` pays a helper + a re-decode + a double match | `jit/baseline/emit/block.rs:819-827` → `family::emit_family`; helper `jit/baseline/family.rs:57-120` (`:91-98`); router `families/mod.rs:112-239` | `to_lower`: `dispatch` **1 068 ns/call**, `jit_family` 387, `matching::exec` 419; `capitalize_token`: 1 275 / 661 / 653 | **Partly** — (a) bake a family id + operand-slice index at compile time (mechanical); (b) inline the `bs_match`/`bs_get_integer` loop (needs #1's register-bound discipline). Do **not** inline `bs_create_bin` |
| 6 | `BitWriter::push_bit` = 8 bit-at-a-time stores per `<<C:8>>` | `families/bitstrings/mod.rs:663-682`, `:684-695` (`push_uint`), `:787-803` (`push_whole_bytes` exists); call site `families/bitstrings/create/mod.rs:294-300` | **6.5 % → 551 ns/call = 18.4 ns/char ≈2.3 ns/bit** | **Yes, byte-granular** — `push_uint` at a byte-aligned position with `width % 8 == 0` *is* `push_whole_bytes` (C's `erts_bs_put_integer_*` stores whole bytes); must route through `ensure_room`/`source_overlaps` (`mod.rs:753-780`) |
| 7 | `jit_publish_y` per block prologue (top frame's window + position) | `jit/baseline/runtime.rs:1027-1042`, `refresh_y_base` `:231-272`; `emit/prologue.rs` | **8.34 % → 3.02 ns/element** (runtime route 6.27 % + `jit_refresh_y` 2.99 %) | **Mostly** — only needed when the window can move (`push_frame`/`pop_frame`/`trim`, whose mover is a `Vec` realloc); a slice-reserved stack degenerates it to the position store. The "null window rejects every Y access" guard is load-bearing |
| 8 | `DispatchMemo` per slice *and* per nested send/fun ⇒ 11 roster `binary_search`es + 2 `String` allocs per key | `beam_rt/src/context/dispatch.rs:235-253` (2 allocs `:247-248`); rosters `:118-147` → `beam_bifs/src/hook_sets.rs:547-693`; lifetime `:194-197`, reset `runtime_context.rs:240`; nested `bridges.rs:183-201`, `:380-396`, `:421-434` | `binary_search_by` **2.5–5.4 % → 93–200 ns/op @3.7 µs / 388–837 @15.5 µs**; a permanent collision ≈190–200 ns/dispatch | **Yes** — hoist onto `ProcessEngine`/`RunnerState`, reuse in `FunBridge::nested`/`SendBridge`, key by atom ids to drop both `String`s; every input is immutable for the runtime's life (the one mutable input, the NIF registry, is consulted before `dispatch`, `calls.rs:1409`) |

Read the profiles with these grain-of-salt facts: `families::dispatch` is the per-instruction tax of a *non-inlined* family with the router inlined (not a lookup), `jit::baseline::drive` is ~20 instructions + 2 indirect calls per **block entry** (not a hot loop body), `call_registered`'s per-BIF self is dominated by the copy-out (a `term_to_binary` binary counts as a foreign container by `calls.rs:913-916`), and `RuntimeContext`/`DispatchMemo`/`enter_module` at 0.00 % is inlining, not absence.
Provably redundant per-call work in the baseline tier: `jit_call_store`'s re-decode + `labels.resolve` (#4); the `local_call_seam` ask (#2); `CompiledModule::function`'s `Rc::clone`/drop + `RefCell` borrow per entry for an artifact alive for the whole run (`code.rs:344-368`); and `set_current_code` written 3× per element with the same value (worth ~0.2 ns — not a lever).
Per element, `list_walk` crosses 4 `extern "C"` helpers, **0.0001 driver-loop iterations** (2 per 20 000-element call — the walk's `call_only` is linked in-block), 2 host vtable calls (`resolve_local_call`, `reduce`) and 1 `Rc` refcount RMW pair.

## Constraints / Notes for Agents
- **`MinimalContext` opts out of the reduction hook** (`wants_reduce() == false`), so `CountingContext` MUST override it to `true` or the instruction count silently stays 0.
- **A batch's argument is setup, never measurement.** `run_calls` parks the whole `Counters` set across the `Prepare` call and snapshots `Machine::refc_binaries().len()` after it, so a fixture that builds its own input *through* the engine (as `long_append` must — `make_heap_binary` refuses > 64 bytes) contributes neither instructions nor payloads to the tally. Never move fixture setup inside the timed loop.
- **A `ns_per_element` figure is a difference of two separately measured phases** (`walk(K) − walk(0)`), i.e. a marginal: it cancels the shared `build/2` and the per-batch warm-up, but it inherits both phases' noise, so quote it with the rounds it came from and never as an absolute per-element time.
- **`gc_walk` is start-up cost.** Do not attribute it per element, per call or per instruction — the absolute figure is flat in `N` (see `## Measured`); the bench tool's paired marginal cancels it and `perf` cannot.
- **No collector here.** `MinimalContext` only grows its reserved heap, so this harness measures the engine, never GC cadence — and for these shapes a collection-free figure is faithful: the runtime side records that `bstr_to_lower` reaches **no** heap-admission point at all (`collections: 0`), `long_append` reports `heap_door_calls_per_call = 0`, and `list_walk`'s loop has neither `test_heap` nor `allocate_heap` (see `../../../../crates/beam_rt/src/runner/CONTEXT.md` §Measured and `../../../../crates/beam_rt/M1_GAPS.md`).
- **The floor's `erlang:bit_size/1` is the bitstring family's reader, never a local one.** `decode/1` needs it in a guard and over a `t_bs_context`, and a half-correct `bit_size` would turn a guard failure into a silently **wrong** decode — so the floor calls `crate::families::bitstrings::bit_size` (C's `erts_bin_bits_size`: heap bits, an off-heap refc window, an `ErlSubBits` match context) and `refuse`s a non-bitstring, which surfaces as a `floor_declines` assertion failure rather than a figure. That is why `src/bench/` still needs no `unsafe` of its own.
- **Never `unwrap()` a measurement into an assertion of speed.** Both `#[ignore]`d tests assert only cheap invariants — correct bytes on both tiers, the tiers agreeing, the expected BIF-call count, `floor_declines == 0`, and (long-append, on a growing tree) `bytes_copied < 8 × n` — and **no timing**, so they never fail on a loaded host.
- **Never edit the foreign sources or copy them in.** Both are staged read-only from their checkouts at test time (`cowboy_bstr.erl` is ISC © Loïc Hoguin; `emqx_base62.erl` is EMQ copyright). The `list_walk`/`long_append` fixtures are original to this repository.
- **Batch lifecycle is load-bearing.** `Machine::refc_binaries` and the `MinimalContext` region heap grow monotonically within a batch and are never pruned here (only a host reachability sweep, `Machine::retain_refc_binaries`, or dropping the machine releases them), so every batch builds a fresh `Machine` + context (500 calls by default for the foreign cases, 5 for `long_append`, 10 for `list_walk`) and drops both at batch end.
- The **census pass reloads the module** (a second `load_file`): the per-thread compiled-block cache would otherwise answer the pass from an artifact compiled while recording was off, leaving `instr_instances` empty.
- **The growth-shape probe is tree-aware.** It prints rather than asserts the payload count: on a pre-growth tree `payloads_created_by_the_call` is one per *append* and the returned payload's growth counters are all zero (it was freshly built), which is the structural signature of the copy-rebuild route.
- **Do not re-investigate the older `foreign` regression.** The Θ(N³) cost (the runner re-registering the whole never-cleared `refc_binaries` reservoir onto the process once per reduction slice) is fixed (`07a5720639` + `7d84b12b44`) and re-verified; `tools/bench/RESULTS.md` §8 withdraws the pre-m15 `foreign` figures (different `N` and an estimator fix).

## Routing Table
- `mod.rs` → the shared harness (`compile_fixture`/`compile_staged`, `Prepare`, `run_calls`, `measure`/`measure_result`, `Measurement` + `ResultSnapshot`, `instr_per_call`, `ReportSpec`/`report_generic`), the `cowboy_bstr` fixture, its `#[ignore]`d entry point (`mod.rs:172`) and `baseline_census`/`print_census` (the per-thread `../jit/baseline/counters.rs` snapshot pass, with an optional `&Unit` for per-unit rows).
- `counting.rs` → `CountingContext` (the delegating counting `ExecutionContext`), the closed BIF floor (`host_bif`), `BifHistogram`.
- `list_walk.rs` → the BIF-free per-element dispatch floor (`LEN = 1000`); entry point `list_walk.rs:133`.
- `long_append.rs` → the growing-accumulator fixture, its entry point (`:188`), its report and the growth-shape probe (`Machine::refc_binaries()` → `capacity`/`reserved_bytes`/`growth_counters`).
- `foreign.rs` → the four staged foreign cases and their two entry points (`:184` cowboy, `:195` emqx).

## See Also
- `../../INSTRUCTION_SURFACE.md` → the per-opcode/per-family semantics these figures price; `../../MEASURED.md` (static/dynamic census, `engine_bench`, `bif_cost`, `jit_bench`) and `../../PERF.md` (tier snapshots).
- `../CONTEXT.md` → the crate's frozen seams (`ExecutionContext`, `Mailbox`, the JIT tier seam) — the harness drives exactly those doors and counts nothing else.
- `../../../../tools/bench/RESULTS.md` → the project-level record (session `m17`) whose per-case ratios these in-crate figures decompose; `src/bench/CONTEXT.md`'s runtime-route section is a **decomposition at a different N/session**, never a replacement.
- `../../../../crates/beam_rt/src/context/gc_walk.rs` + `src/context/gc/reds_door.rs` → the one place `gc_walk` is driven from.
