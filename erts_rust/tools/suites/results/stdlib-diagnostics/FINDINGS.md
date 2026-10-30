# stdlib divergence classes D1…D12 — current verdicts

The per-class record behind `../stdlib-notes.md` §D and `../stdlib/summary.md`.
Every row below was re-measured with its smallest expression against the runtime under test; nothing here is
carried over from an earlier reading, and a claim this file could not reproduce is gone rather than annotated.
Measurement/diagnosis only: no runtime crate was touched from this node.

## Environment and method

| field | value |
|---|---|
| runtime under test | `erts_rust/target/release/beam_rt`, built from this worktree (`cargo.sh build -p beam_rt --release`) |
| runtime commit | `8e8e0ad5d96a11931ff6972779a3b5adbe90bfca` |
| runtime version | `erlang:system_info(version)` → `"17.1"`; `system_info(otp_release)` → `"30"` |
| oracle | system `erl`/`erlc` on `$PATH` — OTP `29.1.2`, erts `17.1.1` |
| sweep artifacts | `../stdlib/report.json` + `../stdlib/summary.md` (8 suites, `--timeout 10 --suite-budget 300 --jobs 4`) |
| cwd | throwaway dirs under `$TMPDIR`; the staged suite beams are the shared `/tmp/erts-suites-stage/stdlib` (`src/`,`ebin/`,`ctl/`,`cwd/`), read-only |
| host | **shared**; only counts and same-host paired ratios are portable |

Every check runs the **same** argv under both runtimes (`-noshell -pa <ebin>… -eval '…, halt(0).'`).
Anything that needs compiled code is compiled with the **oracle's `erlc`** into `$TMPDIR`, so both sides run
byte-identical `.beam` files; the `-eval` route is the tree-walk evaluator, the compiled route is the engine.
Two rules are load-bearing:

- **A raise inside a probe must be caught by the probe.** An uncaught typed gap leaves through the CLI's
  capability-gap channel (`beam_rt: unsupported: …`, exit `3`) or terminates the runtime, so pairs are
  observed as `try … catch`/`catch` results.
- **Ratios only.** No absolute time below is portable; D8 quotes same-host ratios and the operation counts.

## Verdicts

| # | smallest expression | oracle | rust | verdict |
|---|---|---|---|---|
| **D1** | `lists:foreach(fun(_) -> length(lists:duplicate(N, x)) end, [1])` for `N = 65536 / 65537 / 200000` | `ok` every `N` | `ok` every `N` | **fixed** (the 65536-element term-heap `system_limit` is gone) |
| **D1** | `maps:from_keys(lists:seq(1, 1000000), ok)` / `maps:from_list(…)` — **compiled** | the map | catchable `error:system_limit` | **still present, new shape** (the trapping-BIF build path, not a `lists` fun) |
| **D1b** | `error_info_child_probe:main()` | `DONE`, exit `0` | `DONE`, exit `0` (same `{ok,ok}` main result) | **fixed** |
| **D1c** | `error_info_abort_probe:main({prefix, 70})` | `{ok,ok}`, exit `0` | parent receives the atom `'exit <structured reason>'`, exit `0` | **still present** (the raise reason is replaced by a placeholder atom) |
| **D2** | `C = binary:part(<<"z",(binary:copy(<<255>>,1024))/binary>>,1,510), {byte_size(C), binary:referenced_byte_size(C)}` | `{510,2050}` | `{510,510}` | **still present** for every sub-binary of a **refc** parent |
| **D3** | `erlang:trace/3` enable + `trace_info(flags)` readback | `{1,1,{flags,[running,call]}}` | `{1,1,{flags,[running,call]}}` | **fixed** |
| **D4** | `M = #{a=>1,b=>2}, erts_debug:same(maps:put(a,1,M), M)` — **compiled** | `true` | `true` | **fixed** |
| **D5** | `catch maps:with([a,b], (fun(X) -> X end)(a))` — **compiled**, legacy `catch` | head `{maps,with,[[a,b],a],…}` | head `{maps,with,[[a,b],a],…}` | **fixed** (only a benign fun-name suffix on the tail item differs) |
| **D6** | `catch erlang:bump_reductions(134217727)` — **compiled** | `true` | `{'EXIT',{unsupported_bif,[…]}}` | **still present** (a catchable typed gap; `binary_module_SUITE:longest_common_trap` fails on it) |
| **D7** | `erlang:system_info(snifs)` | 553 `{M,F,A}` | 556 `{M,F,A}` | **fixed** (the count is not expected to match) |
| **D8** | `maps_bif_loop_probe:main()` — 50 rounds × 1000-key maps | reference | `merge_with` **662×/614×**, `intersect_with` **92×/95×**, `iterator+next` **438×/480×**, `to_list` control 3.7× | **still present** |
| **D9** | `apply_args_probe:main()` — the `apply/2` argument matrix | see §D9 | agrees on every value; only the arity-mismatch *reason* degrades (`{badarity,Args}` → `badarity`) | **fixed** (residual: one reason shape) |
| **D10** | `erts_debug:set_internal_state(available_internal_state,true)`, then `…(unicode_loop_limit,10)` | `false`, then `40000` | `false`, then `40000` | **fixed** |
| **D11** | `R = erlang:trace_delivered(TraceePid)` after the traced child's `{'DOWN',…}` | a `reference()` | a `reference()` | **fixed** |
| **D12** | `f(E) -> <<E/utf32-little>>.` called as `f(1)` — **compiled** | `<<1,0,0,0>>` | `<<1,0,0,0>>` | **fixed** |
| **D12b** | `unicode_SUITE:latin1` (staged `unicode_SUITE.erl:414`) | `ok` | `ok` | **fixed** (`unicode_SUITE:latin1` now **agrees**) |
| **D13** | `binary_module_SUITE:parts` — `random_parts/1`'s first comprehension | pass | `{badmatch,false}` at `binary_module_SUITE:'-random_parts/1-lc$^1/1-1-'` | **new** (a `binary:part/3` edge shape) |

Counts: **10 fixed** (D1b, D3, D4, D5, D7, D9, D10, D11, D12, D12b) · **4 still present** (D1, D1c, D2, D6, D8)
· **1 new** (D13). D1 is **split**: the `lists`-fun `system_limit` is fixed, but the trapping-BIF map-build
shape remains.

## Notes per class

### D1 — the term-heap `system_limit` is gone for `lists` funs; the trapping map-build still raises it

The former class — a large allocation inside a fun invoked by a higher-order `lists` BIF, threshold 65536 list
elements — no longer reproduces: `lists:foreach(fun(_) -> length(lists:duplicate(N, x)) end, [1])` answers `ok`
for `N = 65536`, `65537` and `200000`, as does `length(lists:seq(1, 1000000))` (`1000000`).

The surviving shape is the **trapping BIF build** of a map from a 1M-element list:

| shape (compiled) | oracle | rust |
|---|---|---|
| `maps:from_keys(lists:seq(1, 1000000), ok)` | a 1M-key map | catchable `error:system_limit` |
| `maps:from_list([{I,I} \|\| I <- lists:seq(1, 1000000)])` | a 1M-entry map | catchable `error:system_limit` |
| `lists:foreach` / `lists:duplicate` / `lists:seq` (the old class) | `ok` | `ok` |

The trap yield inside the map build still refuses a term-heap admission. **Owner:** `beam_rt` heap admission
(`enforce_term_heap_limit` / the per-depth BIF-region reservation) on the trapping map-BIF path.
**Suite rows:** `maps_SUITE:t_keys_trapping` / `t_values_trapping` / `t_from_keys_check_trapping` — all
`{badmatch,system_limit}` from `maps_SUITE:count_nr_of_yields/2`.

### D1b — the spawned child is fine (fixed, unchanged)

`error_info_child_probe` runs `binary_module_SUITE:error_info([])` in the main process and in a `spawn_monitor`
child. Both sides: `DONE`, exit `0`, main result `{ok,ok}`, child `DOWN=normal`.

### D1c — the raise reason is a placeholder atom (still present)

`error_info_abort_probe:main({prefix, N})` drives `error_info/1`'s 73-entry list through the
`ct_runner:run_case/4` shape. At `N = 70` and `N = 73` the oracle answers `{ok,ok}` while the rust side hands the
parent the atom `'exit <structured reason>'` instead of the child's `{ct_case_result,{ok,ok}}`. The abort
itself is gone; what remains is the **death-boundary reason** — an interned placeholder (`Display` of a
structured raise reason) rather than the real term, because `live_reason_term()` answered `None`.
**Owner:** `beam_rt` escape-boundary stabilization (`runtime/gc.rs`) + the `intern(reason.to_string())` fallback
in `runner/reason.rs`. **Suite row:** `binary_module_SUITE:error_info`.

### D2 — `binary:referenced_byte_size/1` (still present)

| shape | oracle | rust |
|---|---|---|
| parent `B = <<"z",(binary:copy(<<255>>,1024))/binary>>` — `{byte_size(B), referenced_byte_size(B)}` | `{1025,2050}` | `{1025,1025}` |
| `C = binary:part(B, 1, 510)` — `{byte_size(C), referenced_byte_size(C)}` | `{510,2050}` | `{510,510}` |
| `part(erts_debug:unaligned_bitstring(<0..255>,3), 1, 100)` | `{100,257}` | `{100,100}` |
| a literal binary, `list_to_binary/1`, whole `binary:copy/2` | own size | own size (**agree**) |

The whole refc binary reports its own `byte_size` instead of the 2× allocated size, and every sub-binary of a
**refc** parent reports its own size instead of the parent's referenced size. **Owner:** `beam_bifs`
`binary:referenced_byte_size/1` together with the sub-binary bookkeeping in `beam_term`/`beam_alloc`.
**Suite rows:** `binary_module_SUITE:referenced` (`{badmatch,1023}`), `binary_module_SUITE:doctests`,
`unicode_SUITE:binaries_errors_limit` + the four `unicode_SUITE:ex_binaries_errors_utf*` rows (all
`{badmatch,false}` on `binary:referenced_byte_size(Chomped) =:= PBSz`).

### D3 — `erlang:trace/3` (fixed, unchanged)

`{1,1,{flags,[running,call]}}` on both sides.

### D4 — map no-op identity (fixed)

`M = #{a=>1,b=>2}, erts_debug:same(maps:put(a,1,M), M)` is now `true` on the compiled route (and the `-eval`
route), so the ≥2-key `maps:put/3` no-op no longer allocates a fresh map.

### D5 — stacktrace head (fixed)

`stacktrace_head_probe:main()` (**compiled**, legacy `catch`) now yields the BIF head frame
`{maps,with,[[a,b],a],…}` on both sides for `maps:with/2`, and the `try … catch` route agrees. Only a benign
suffix on the tail item's fun name differs. Consequently `maps_SUITE:t_from_keys`/`t_with_2`/`t_without_2`/
`t_merge_opt` now **agree** (were `{badmatch,{'EXIT',…}}`).

### D6 — `erlang:bump_reductions/1` is a catchable typed gap (still present)

| shape | oracle | rust |
|---|---|---|
| `catch erlang:bump_reductions(134217727)` (compiled) | `true` | `{'EXIT',{unsupported_bif,[…]}}` |
| `try … catch C:R` (compiled) | `true` | `{error,unsupported_bif}` |
| `-eval` uncaught | `true` | `beam_rt: unsupported: bif`, exit `3` |

A typed gap, never a wrong answer. **Owner:** `beam_bifs` (the `erlang:bump_reductions/1` registry entry).
**Suite row:** `binary_module_SUITE:longest_common_trap` (`{exception,error,unsupported_bif,…}`).

### D7 — `erlang:system_info(snifs)` (fixed, unchanged)

556 vs 553 3-tuples; each is that runtime's own static-NIF table.

### D8 — BIF-driven Erlang callbacks are orders of magnitude late (still present)

`maps_bif_loop_probe.erl`, 1000-key maps, 50 rounds, one invocation per runtime, order swapped:

| workload | rounds × size | ratio, run 1 | ratio, run 2 |
|---|---|---|---|
| `maps:merge_with/3` | 50 × 1000 keys | **662×** | **614×** |
| `maps:intersect_with/3` | 50 × 1000 keys | **92×** | **95×** |
| `maps:iterator/1` + `maps:next/1` | 50 × 1000 entries | **438×** | **480×** |
| `maps:to_list/1` (no Erlang callback — control) | 50 × 1000 entries | 3.7× | 3.7× |

Both sides computed the same work (`checksum = 0`). The control row shows a pure BIF costs ~4×, so the residual
is the per-callback-call cost of a higher-order BIF. **Owner:** `beam_engine`/`beam_bifs` boundary.

### D9 / D11 — `erlang:apply/2` and `erlang:trace_delivered/1` (fixed)

`apply/2` agrees on every value; only the arity-mismatch **reason** degrades (`{badarity,Fun|Args}` → `badarg`),
and `queue_SUITE:oops` **agrees**. `erlang:trace_delivered/1` after the traced child's `{'DOWN',…}` now returns a
`reference()` on both sides (was `{'EXIT',{badarg,…}}`).

### D10 — `erts_debug:set_internal_state/2` (fixed, unchanged)

The gate opens, then `unicode_loop_limit`/`binary_loop_limit`/`re_loop_limit` answer the previous value on both
sides.

### D11 — see D9 (fixed)

`maps_SUITE:t_from_list_check_trapping` now fails with `{badmatch,false}` rather than the old
`{exception,error,badarg,{erlang,trace_delivered,…}}` — the trace-delivered defect is gone and the remaining
failure is the D1-shaped large allocation inside the traced child's `count_nr_of_yields`, not a trace reason.

### D12 — a `utf32` segment (fixed)

`unicode_utf32_probe:u32_little_from_var(1)` now answers `<<1,0,0,0>>` on both sides (was byte-swapped
`<<0,0,0,1>>`). `unicode_SUITE:latin1` (D12b) also **agrees**. The four `unicode_SUITE:ex_binaries_errors_utf*`
rows that still diverge are **D2** (`binary:referenced_byte_size`), not this class.

### D13 — `binary_module_SUITE:parts` (new)

`parts/1`'s `random_parts/1` loop fails at its first comprehension
(`binary_module_SUITE:'-random_parts/1-lc$^1/1-1-'`) with `{badmatch,false}` — one of the three-way
`?MASK_ERROR(binary:part(Str,Z)) =:= ?MASK_ERROR(binref:part(Str,Z))`, `=:= ?MASK_ERROR(erlang:binary_part(Str,Z))`,
`=:= ?MASK_ERROR(binary:part(make_unaligned(Str),Z))` equalities is false for some `{Pos,Len}` (the suite's
`Parts1` includes negative lengths `{X+Y,-Y}`, so the differing arm is the error masking of `binary:part/3` on a
boundary argument). The basic aligned/unaligned positive-part case agrees; the minimized boundary shape is not
yet reduced to a one-liner. **Owner:** `beam_bifs` (`binary:part/3` error masking). This is the stdlib sibling of
the khepri RC-G `binary:part/3` divergence.

## Extra observation — `erts_debug:flat_size/1`

`erts_debug:flat_size/1` raises a **catchable** `error:unsupported_bif` once its argument reaches
**524288 (`1 bsl 19`)** list elements (500000 answer `1000000`; 524288 answers `unsupported_bif`). The oracle
computes both. Independent of `lists:duplicate/2`, which is fine well past `1 bsl 21`.

## Sweep rows: which timeouts are budget-only and which are real

The 10 s sweep rows that read `rust = timeout` (sys `pass`) fall into two buckets:

| row | classification |
|---|---|
| `binary_module:bin_to_list`, `binary_module:random_ref_fla_comp` | **budget-only** (perf; both sides pass standalone, rust slower) |
| `binary_module:random_ref_comp`, `binary_module:random_ref_sr_comp` | **real, perf** (>100×; progress `Number of successes: …` reaches thousands) |
| `lists_SUITE:suffix` | **budget-only** (both pass standalone) |
| `maps_SUITE:t_iterator_2`, `maps_SUITE:t_put_opt` | **budget-only** (both pass standalone) |
| `unicode_SUITE:random_lists`, `unicode_SUITE:roundtrips`, `unicode_SUITE:utf8_illegal_sequences_bif` | **budget-only** (perf) |

Real divergences that are *not* timeouts at any budget: `binary_module:doctests` + `referenced` (D2),
`binary_module:error_info` (D1c), `binary_module:longest_common_trap` (D6), `binary_module:parts` (D13),
`maps:doctests`, `maps:t_from_keys_check_trapping`/`t_keys_trapping`/`t_values_trapping` (D1 trapping-map shape),
`maps:t_from_list_check_trapping` (D1 shape), and the five `unicode:binaries_errors_limit`/`ex_binaries_errors_*`
rows (D2).

## Probe files

| file | class | what it measures |
|---|---|---|
| `apply_args_probe.erl` | D9 | the `apply/2` argument matrix (each row guarded) |
| `bigterm_spawn_probe.erl` | D1 | `lists:duplicate(1 bsl 21, x)` 5× plain, 5× in a `spawn_monitor` child, a deep-recursion/big-term loop, and the `flat_size` threshold |
| `error_info_child_probe.erl` | D1b | `binary_module_SUITE:error_info([])` in the main process and in a spawned child (needs the staged suite `ebin`) |
| `error_info_abort_probe.erl` | D1c | `error_info/1`'s entry list through `ct_runner:run_case/4`'s shape |
| `maps_bif_loop_probe.erl` | D8 | `merge_with`/`intersect_with`/`iterator+next` reduced-size workloads, checksum + no-callback control |
| `queue_oops_probe.erl` | D9 | `erlang:apply/2` from **compiled** code: smallest repro, arity-mismatch reason, external-fun shape |
| `stacktrace_head_probe.erl` | D5 | stacktrace head from **compiled** code for `maps:with/2`, `maps:keys/1`, `maps:get/2`, `binary:part/3` |
| `unicode_utf32_probe.erl` | D12 | `<<E/utf32-little>>` from a parameter vs a literal, the `/utf32-native`, shift-segment, `utf16-little`, match-context shapes |
| `matrix.sh` | shared | one-expression-per-line runner that prints both sides |

Build and run any of them with:

```
erlc -o $TMPDIR/stdlib-diag/ebin erts_rust/tools/suites/results/stdlib-diagnostics/<probe>.erl
cd $TMPDIR/stdlib-diag/cwd   # a throwaway cwd
<rt>  -noshell -pa $TMPDIR/stdlib-diag/ebin -eval '<probe>:main(), halt(0).'
erl   -noshell -pa $TMPDIR/stdlib-diag/ebin -eval '<probe>:main(), halt(0).'
```

Staged-suite probes additionally need `-pa /tmp/erts-suites-stage/stdlib/ebin -pa /tmp/erts-suites-stage/stdlib/ctl`.
