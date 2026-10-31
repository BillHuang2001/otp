# stdlib divergence classes D1…D13 — current verdicts

The per-class record behind `../stdlib-notes.md` §D and `../stdlib/summary.md`.
Every row below was re-measured with its smallest expression against the runtime under test; nothing here is
carried over from an earlier reading, and a claim this file could not reproduce is gone rather than annotated.
Measurement/diagnosis only: no runtime crate was touched from this node.

## Environment and method

| field | value |
|---|---|
| runtime under test | `erts_rust/target/release/beam_rt`, built from this worktree (`cargo.sh build -p beam_rt --release`) |
| runtime commit | `e98a5e1e231706a13b103f8c18c218ecb8d0d212` |
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
| **D1** | `lists:foreach(fun(_) -> length(lists:duplicate(N, x)) end, [1])` for `N = 65536 / 65537 / 200000` | `ok` every `N` | `ok` every `N` | **fixed** |
| **D1** | `maps:from_keys(lists:seq(1, 1000000), ok)` / `maps:from_list(…)` — **compiled** | the map | the map | **fixed** (the `system_limit` shape is gone) |
| **D1** | the suite's trapping-yield count (`count_nr_of_yields > 2` for `from_keys`/`from_list`/`keys`/`values`) | `NoYields > 2` | `{badmatch,false}` (≤ 2 yields) | **still present, new shape** (the BIF does not trap through the yield/trace funnel) |
| **D1b** | `error_info_child_probe:main()` | `DONE`, exit `0` | `DONE`, exit `0` | **fixed** |
| **D1c** | `error_info_abort_probe:main({prefix, 70})` | `{ok,ok}`, exit `0` | parent receives the atom `'exit <structured reason>'`, exit `0` | **still present** (see the precise site below) |
| **D2** | `B = <<"z",(binary:copy(<<255>>,1024))/binary>>`, `C = binary:part(B, 1, 510)`, `{byte_size(C), referenced_byte_size(C)}` | `{510,1025}` | `{510,510}` | **mostly fixed** — whole-binary and several sub-binary shapes agree; a `binary:part/3` sub-binary of a refc parent still reports its own size |
| **D3** | `erlang:trace/3` enable + `trace_info(flags)` readback | `{1,1,{flags,[running,call]}}` | `{1,1,{flags,[running,call]}}` | **fixed** |
| **D4** | `M = #{a=>1,b=>2}, erts_debug:same(maps:put(a,1,M), M)` — **compiled** | `true` | `true` | **fixed** |
| **D5** | `catch maps:with([a,b], (fun(X) -> X end)(a))` — **compiled**, legacy `catch` | head `{maps,with,[[a,b],a],…}` | head `{maps,with,[[a,b],a],…}` | **fixed** |
| **D6** | `catch erlang:bump_reductions(134217727)` — **compiled** | `true` | `true` | **fixed** |
| **D7** | `erlang:system_info(snifs)` | 553 `{M,F,A}` | 556 `{M,F,A}` | **fixed** (the count is not expected to match) |
| **D8** | `maps_bif_loop_probe:main()` — 50 rounds × 1000-key maps | reference | `merge_with` **662×/614×**, `intersect_with` **92×/95×**, `iterator+next` **438×/480×**, `to_list` control 3.7× | **still present** (performance) |
| **D9** | `apply_args_probe:main()` — the `apply/2` argument matrix | agrees on every value | agrees on every value | **fixed** |
| **D10** | `erts_debug:set_internal_state(available_internal_state,true)`, then `…(unicode_loop_limit,10)` | `false`, then `40000` | `false`, then `40000` | **fixed** |
| **D11** | `R = erlang:trace_delivered(TraceePid)` after the traced child's `{'DOWN',…}` | a `reference()` | a `reference()` | **fixed** |
| **D12** | `f(E) -> <<E/utf32-little>>.` called as `f(1)` — **compiled** | `<<1,0,0,0>>` | `<<1,0,0,0>>` | **fixed** |
| **D12b** | `unicode_SUITE:latin1` (staged `unicode_SUITE.erl:414`) | `ok` | `ok` | **fixed** |
| **D13** | `binary_module_SUITE:parts` — `random_parts/1`'s `binary:part/3` boundary masking | pass | `timeout` at 10 s (was `{badmatch,false}`) | **fixed** (correctness); the row is now performance-bound |

Counts: **12 fixed** (D1 lists-shape, D1 map-build `system_limit`, D1b, D3, D4, D5, D6, D7, D9, D10, D11,
D12, D12b, D13) · **still present** D1c (correctness) and D1 trapping-yield shape, D2 residual, D8 (performance).

## Notes per class

### D1 — the `system_limit` shapes are fixed; the trapping BIF yield count remains

Both former `system_limit` shapes are gone: `lists:foreach(fun(_) -> length(lists:duplicate(N, x)) end, [1])`
answers `ok` for `N = 65536`, `65537`, `200000`, and the compiled `maps:from_keys(lists:seq(1, 1000000), ok)`
and `maps:from_list([{I,I} || I <- lists:seq(1, 1000000)])` return the map (verified this session, both sides).

The surviving shape is a **yield-count** divergence, not a wrong value: the suite's
`count_nr_of_yields(Fun, {maps, from_keys, 2})` (and the `from_list`/`keys`/`values` twins) asserts
`NoYields > 2`, and on this runtime the trace-driven count is ≤ 2, so `true = NoYields > 2` fails with
`{badmatch,false}`.
The BIF builds the 1M-element map correctly but does not trap through the `out`/`in` trace funnel the way the
oracle does, so the trapping test observes too few yields.
**Owner:** `beam_bifs`/`beam_engine` — the trapping map-BIF yield/trace path.
**Suite rows:** `maps_SUITE:t_from_keys_check_trapping` / `t_from_list_check_trapping` / `t_keys_trapping`
(all `{badmatch,false}`) and `t_values_trapping` (now `timeout` at 10 s).

### D1b — the spawned child is fine (fixed, unchanged)

`error_info_child_probe` runs `binary_module_SUITE:error_info([])` in the main process and in a `spawn_monitor`
child. Both sides: `DONE`, exit `0`, main result `{ok,ok}`, child `DOWN=normal`.

### D1c — the raise reason is a placeholder atom (still present, precise site)

`error_info_abort_probe:main({prefix, N})` drives `error_info/1`'s 73-entry list through the
`ct_runner:run_case/4` shape. At `N = 70` and `N = 73` the oracle answers `{ok,ok}` while the rust side hands the
parent the atom `'exit <structured reason>'` instead of the child's `{ct_case_result,{ok,ok}}`.
**Root cause (bisected):** a GC-timing bug — the synthesized BIF head item's `ArgList` shares raw register
words that a later raw-word landing leaves unrooted, so the structured exit reason degrades to the placeholder
atom.
**Precise site:** `beam_engine/src/families/exceptions/bif_frame.rs::carve` (line ~494) — `sink.list(&values)`
shares the BIF register operands verbatim; the finished head item is later landed raw as `x(2)` and re-embedded
without a fresh rooted reservation.
**Fix direction:** deep-copy the boxed arguments into the reserved sink, or root the finished head item across
the subsequent allocating door.
**Owner:** `beam_engine`. **Suite row:** `binary_module_SUITE:error_info`.

### D2 — `binary:referenced_byte_size/1` (mostly fixed; one sub-binary shape remains)

| shape | oracle | rust |
|---|---|---|
| whole refc parent `B = <<"z",(binary:copy(<<255>>,1024))/binary>>` — `{byte_size(B), referenced_byte_size(B)}` | `{1025,1025}` | `{1025,1025}` (**agree**) |
| `C = binary:part(B, 1, 510)` — `{byte_size(C), referenced_byte_size(C)}` | `{510,1025}` | `{510,510}` |
| a literal binary, `list_to_binary/1`, whole `binary:copy/2` | own size | own size (**agree**) |

The whole-binary case and the suite rows that used to fail on it now agree — `binary_module_SUITE:referenced`
and `unicode_SUITE:binaries_errors_limit` are green, and three of the five `unicode_SUITE:ex_binaries_errors_utf*`
rows (`utf16_big`, `utf32_big`, `utf8`) agree.
The remaining shape is a `binary:part/3` sub-binary of a refc parent, which still reports its own `byte_size`
instead of the parent's referenced size; `unicode_SUITE:ex_binaries_errors_utf16_little` and `_utf32_little`
now read `timeout` at the 10 s budget rather than agreeing.
**Owner:** `beam_bifs` `binary:referenced_byte_size/1` together with the sub-binary bookkeeping in
`beam_term`/`beam_alloc`.

### D3 / D4 / D5 / D6 / D7 / D9 / D10 / D11 / D12 / D12b — fixed

`erlang:trace/3` flags readback, the ≥2-key `maps:put/3` no-op identity, the legacy-`catch` stacktrace head,
`erlang:bump_reductions/134217727` (now `true`, was `unsupported_bif`), `system_info(snifs)`,
`erlang:apply/2`, `erts_debug:set_internal_state/2`, `erlang:trace_delivered/1`, the `/utf32-little` segment and
`unicode_SUITE:latin1` all agree on both sides this session.

### D8 — BIF-driven Erlang callbacks are orders of magnitude late (still present, performance)

`maps_bif_loop_probe.erl`, 1000-key maps, 50 rounds: `merge_with` **662×/614×**, `intersect_with` **92×/95×**,
`iterator+next` **438×/480×**, `to_list` control 3.7×. Both sides compute the same work.
**Owner:** `beam_engine`/`beam_bifs` boundary (the per-callback-call cost of a higher-order BIF).
**Suite row:** `maps_SUITE:t_merge_with` (now `timeout` at 10 s).

### D13 — `binary_module_SUITE:parts` (fixed; the row is now performance-bound)

`parts/1` no longer fails with `{badmatch,false}` at `'-random_parts/1-lc$^1/1-1-'`; it now runs past the
`binary:part/3` boundary-masking equalities and reads `timeout` at the 10 s budget (the `random_parts` loop is
slow on this runtime, not wrong).
**Owner:** `beam_bifs` (`binary:part/3` error masking) — closed; the remaining row cost is performance.

## Sweep rows: which timeouts are budget-only and which are real

The 10 s sweep reads `rust = timeout` (sys `pass`) on the rows below; the split is budget/performance, and
`binary_module_SUITE:error_info` (D1c) is the only row that still fails **for a result**.

| row | classification |
|---|---|
| `binary_module:bin_to_list`, `random_ref_fla_comp`, `lists_SUITE:suffix`, `maps_SUITE:t_iterator_2` | **budget-only** (both sides pass standalone, rust slower) |
| `binary_module:random_ref_comp`, `random_ref_sr_comp`, `maps_SUITE:t_merge_with` (D8), `unicode_SUITE:category` | **real, perf** (>100× / the higher-order-BIF callback cost) |
| `binary_module:doctests`, `parts`, `encode_decode` | **perf** (in-process compiler throughput: `doctests` is correct but ~81 s vs the oracle's 0.17 s; `parts`/`encode_decode` run past their old failure and time out) |
| `unicode_SUITE:ex_binaries_errors_utf16_little`, `_utf32_little` | **D2 residual** (referenced_byte_size of a `binary:part/3` sub-binary) + perf |
| `maps_SUITE:t_values_trapping` | the D1 trapping-yield shape surfacing as `timeout` |

The `unicode_SUITE` budget also expired at the 10 s/300 s setting this round: 11 cases
(`exceptions`, `huge_illegal_code_points`, `incomplete_utf32_to_latin1`, `is_id`, `is_whitespace`, `latin1`,
`normalize`, `random_lists`, `roundtrips`, `utf16_illegal_sequences_bif`, `utf8_illegal_sequences_bif`) were
**not measured** (`rust-gap`, "the suite budget expired"), because the D2 fix turned fast-fail rows into
slow-but-running rows and consumed the per-suite budget.
Those 11 are a measurement artifact, not a result.

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
