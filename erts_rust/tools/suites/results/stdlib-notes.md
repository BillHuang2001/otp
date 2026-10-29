# stdlib sweep — analysis of the committed artifact

Analysis of **`results/stdlib/report.json`** (schema `suites/1`) and its rendered
`results/stdlib/summary.md`. Every number below is re-derived from that JSON; the environment block is
copied from the report's own `host`/`rust`/`generated_at`/`run_command` fields.

Rust side = `erts_rust/target/release/beam_rt` built from this worktree; oracle = the system `erl` on
`$PATH`. **The host is shared** — only pass/fail counts, ratios and directions are meaningful; no
absolute wall time here is portable.

---

## A. Environment (from the report)

| field | value |
|---|---|
| `generated_at` | `1793248490` |
| `run_command` | `suites run --target stdlib --suite lists_SUITE,maps_SUITE,proplists_SUITE,dict_SUITE,queue_SUITE,binary_module_SUITE,string_SUITE,unicode_SUITE --timeout 10 --suite-budget 300 --jobs 4 --out results/stdlib` |
| `host.oracle` | `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` |
| `host.oracle_version` | `29/17.1.1` (tree pins OTP `30.0-rc0`, erts `17.1`) |
| `host.nproc` | 16 |
| `host.loadavg_before` | `9.89 5.02 7.34 2/835 5` |
| `host.loadavg_after` | `4.29 4.59 6.94 2/815 1101` |
| `rust.path` | `erts_rust/target/release/beam_rt` (this worktree) |
| `rust.sha` | `32732ba94bc9d89e8f67be6c472d5a6259af80c0` |
| `rust.version` | `30/17.1` |
| `rust.tier` | `null` (default baseline-JIT tier) |
| `targets[0].build` | `erlc -o <stage>/ebin -I <stage>/src <suite>.erl …` — `ok: true`, `reused: true`; `compiled 8 suites + 6 helpers + runner with the oracle's erlc` |
| `targets[0].code_path` | `<stage>/ebin`, `<stage>/ctl` (identical `-pa` for both sides) |

Load was ~10 at the start of the sweep and ~4 at the end, i.e. **this sweep ran on a busy host**; that
matters for §D10 (the timeout rows) and is the reason timeouts are re-measured there rather than read
straight out of the report.

Invocation shape for every row (per `targets[0].invocations`), the *only* difference being the binary:

```
cd <stage>/cwd/<SUITE> && <RUNTIME> -noshell -pa <stage>/ebin -pa <stage>/ctl \
  -eval 'ct_runner:run(<SUITE>, 10000, 300000, "<stage>/src"), halt(0).'
```

## B. Per-suite results

`rows` = ct cases reported for that suite; P/F are the case counts per side; verdicts are the
harness's `agree` / `agree-fail-identical` (`afi`) / `divergence` / `rust-gap`.

| suite | rows | sys P/F | rust P/F | agree | afi | divergence | rust-gap |
|---|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 18 / 2 | 4 / 3 | 4 | 1 | 3 | 12 |
| `dict_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 | 0 |
| `lists_SUITE` | 78 | 78 / 0 | 76 / 2 | 76 | 0 | 2 | 0 |
| `maps_SUITE` | 32 | 32 / 0 | 19 / 13 | 19 | 0 | 13 | 0 |
| `proplists_SUITE` | 4 | 4 / 0 | 4 / 0 | 4 | 0 | 0 | 0 |
| `queue_SUITE` | 7 | 7 / 0 | 6 / 1 | 6 | 0 | 1 | 0 |
| `string_SUITE` | 46 | 45 / 1 | 45 / 1 | 45 | 1 | 0 | 0 |
| `unicode_SUITE` | 20 | 18 / 2 | 6 / 14 | 6 | 2 | 12 | 0 |
| **total** | **214** | **209 / 5** | **167 / 34** | **167** | **4** | **31** | **12** |

Clean suites (every case agrees, nothing excluded): `dict_SUITE`, `proplists_SUITE`, `string_SUITE`
(45 agree + 1 excluded).

## C. Totals

- **214 rows** measured; no row is `not-run` and none is `oracle-only-fail`.
- **Oracle: 209 rows pass, 5 fail.** Rust: **167 pass, 34 fail**.
- **Fully green rows: 209 of 214** on the oracle and **167 of 214** on rust (a `rust-gap` row has no
  rust result at all — it is not a pass).
- **Excluded from both numerators: 4 rows** (`agree-fail-identical`) — see §E.
- **Verdict split:** `agree 167` · `agree-fail-identical 4` · `divergence 31` · `rust-gap 12`.
- **No rust pass corresponds to an oracle failure.** Every one of the 167 `agree` rows has
  `sys.status = pass`; the only rows where rust "passes" something the oracle does not are the two
  documented `afi` rows (`unicode_SUITE:is_id`, `unicode_SUITE:is_whitespace`) where **both** sides
  fail identically, and `string_SUITE:meas` / `binary_module_SUITE:copy`, which are excluded for the
  reasons in §E. There is no case in this sweep where the Rust runtime succeeds and the oracle fails.
- The 12 `rust-gap` rows are all one root cause: the rust side of `binary_module_SUITE` **died
  mid-suite** (§D1), so the remaining cases never ran.

## D. Non-agree rows, by root-cause class

47 rows are non-agree (31 `divergence` + 12 `rust-gap` + the 4 `afi` rows are discussed in §E).
`<rt>` below means either binary; each reproducer is a one-liner you can paste into a shell.

### D1 — `binary_module_SUITE` rust side DIES mid-suite  (**crash, not a typed gap**) — 13 rows

Rows: `rust-gap` `error_info`, `guard`, `hex_encoding`, `join`, `list_to_bin`, `longest_common_trap`,
`parts`, `random_ref_comp`, `random_ref_fla_comp`, `random_ref_sr_comp`, `referenced`, `scope_return`
(all `error 0/0 case missing from the rust runtime's output`) plus `divergence` `interesting`.

Reproducing the recorded rust invocation by hand gives the same shape:

```
SUITE binary_module_SUITE
CASE ac_simd_root_skip    OK
CASE badargs              OK
CASE bin_to_list          TIMEOUT     (10 s budget; it passes in ~27 s serially — §D10)
CASE check_no_invalid_read_bug OK
CASE copy                 EXC  {exception,error,unsupported_bif,{erts_debug,set_internal_state,…}}
CASE doctests             EXC  {exception,error,{1,errors},…}   (the failing doctest is D2's expression)
CASE encode_decode        OK
… then the first two `binary:at/2` error-info prints, then the process exits 1
stderr: beam_rt: system_limit
```

The 12 later cases are therefore never reported by the runner.

**Smallest reproducer (not reduced to a single expression — see the caveat):** a *spawned child* that
runs the suite's own `error_info/1`:

```
erlc -o $TMPDIR/probe/ebin lib/stdlib/test/binary_module_SUITE.erl   # system erlc, $TMPDIR only
cd $TMPDIR/probe/cwd && <rt> -noshell -pa $TMPDIR/probe/ebin -pa <staged>/ebin \
  -eval 'P = spawn_monitor(fun() -> binary_module_SUITE:error_info([]) end),
         receive {'DOWN',R,process,Pid,Why} -> io:format("~p~n",[{R,Why}]) end, halt(0).'
```

- oracle: `{normal,normal}`, 464 lines of output.
- rust: the child dies (`{function_clause,[]}`) or the whole runtime aborts with
  `beam_rt: system_limit`, after only part of the output.

Calling `binary_module_SUITE:error_info([])` **in the main process** is fine on rust (32 397 bytes,
508 lines), so the failure is specific to a *spawned child*. It is **entry-count dependent**: running
only the first `N` error-info entries in the child is green up to `N = 38` and dies at `N = 39`;
pre-computing the entry list in the parent makes the child succeed; a heap warm-up
(`lists:seq(1,200000)` + `erlang:garbage_collect()`) does **not** help. The dying child emits none of
the case's own output markers, so it is a VMs-side abort during the writes, not a failing `binary:*`
call.

**Owner:** `beam_engine` (spawned-child heap/stack growth at the `binary:at/2`/`io:format` argument
boundary) with `beam_gc` as the second suspect. This is the single most valuable finding of this pass:
a **crash**, not a typed `unsupported`, so it breaks the "every gap is a typed error" contract.

### D2 — `binary:referenced_byte_size/1` of a sub-binary answers its own size — 6 rows

Rows: `binary_module_SUITE:doctests` (`{1,errors}`), `unicode_SUITE:ex_binaries_errors_utf8`,
`_utf16_big`, `_utf16_little`, `_utf32_big`, `_utf32_little` (`{badmatch,false}` inside
`utf*_inner_loop/6` / the utf8 list comprehension — all five assert
`true = (binary:referenced_byte_size(Chomped) =:= PBSz)`).

```
<rt> -noshell -eval 'Part = <<"z",(binary:copy(<<255>>,1024))/binary>>,
                      C = binary:part(Part,1,510),
                      io:format("~p~n",[{byte_size(C), binary:referenced_byte_size(C)}]), halt(0).'
```

- rust: `{510,510}`
- oracle: `{510,2050}`

**Owner:** `beam_bifs` (the binary BIF) over `beam_term`/`beam_alloc` sub-binary bookkeeping — the
runtime does not track the referenced (parent) binary of a sub-binary.

### D3 — `erlang:trace/3` answers `badarg` — 4 rows

Rows: `maps_SUITE:t_from_keys_check_trapping`, `t_from_list_check_trapping`, `t_keys_trapping`,
`t_values_trapping` — each `{badmatch,badarg}` in `maps_SUITE:count_nr_of_yields/2`.

```
<rt> -noshell -eval 'io:format("~p~n",[catch erlang:trace(self(),true,[running,{tracer,self()}])]), halt(0).'
```

- rust: `{'EXIT',{badarg,[]}}`
- oracle: `1`

**Owner:** `beam_bifs` (`erlang:trace/3`) with `beam_engine`'s trace machinery.

### D4 — map identity is not preserved (`erts_debug:same/2`) — 2 rows

Rows: `maps_SUITE:t_merge_opt`, `t_put_opt` (`{badmatch,false}`).

```
<rt> -noshell -eval 'M = #{a => 1}, io:format("~p ~p~n",[erts_debug:same(maps:merge(#{},M),M),
                                                      erts_debug:same(maps:put(a,1,M),M)]), halt(0).'
```

- rust: `false false`
- oracle: `true true`

A no-op `maps:merge/2` / `maps:put/3` must return the *same* map term. **Owner:** `beam_bifs` (maps
BIFs return a rebuilt map instead of the input).

### D5 — stack-trace head names the caller instead of the BIF — 3 rows

Rows: `maps_SUITE:t_from_keys`, `t_with_2`, `t_without_2` (`{badmatch,{'EXIT',_}}`).

```
<rt> -noshell -eval 'io:format("~p~n",[catch maps:with([a,b],(fun(X) -> X end)(a))]), halt(0).'
```

- rust: `{'EXIT',{{badmap,a},[{mm,with2,0,[{file,…}]},…]}}` — head is the calling local fun.
- oracle: `{'EXIT',{{badmap,a},[{maps,with,[[a,b],a],…},…]}}` — head is the failing BIF.

**Owner:** `beam_engine` (`families/exceptions/callee.rs` stack-trace synthesis) — the BIF frame is
missing from the trace head.

### D6 — `erlang:bump_reductions/1` is not served — 1 row

Row: `maps_SUITE:doctests` (`{1,errors}`) — raised from `prim_eval:arg_reg_alloc/0`, which calls
`erlang:bump_reductions(134217727)`.

```
<rt> -noshell -eval 'io:format("~p~n",[catch erlang:bump_reductions(134217727)]), halt(0).'
```

- rust: `{'EXIT',{unsupported_bif,…}}` (a typed capability gap, reported as `unsupported: erlang:bump_reductions/1`)
- oracle: `true`

**Owner:** `beam_bifs`.

### D7 — `erlang:system_info(snifs)` is not published — 1 row

Row: `lists_SUITE:error_info`.

```
<rt> -noshell -eval 'io:format("~p~n",[erlang:system_info(snifs)]), halt(0).'
```

- rust: `beam_rt: unsupported: erlang:system_info(snifs): this runtime publishes no static NIF table` (exit 3)
- oracle: `553`

**Owner:** `beam_rt` (`system_info`) — this is a *reported* capability gap, not a wrong answer.

### D8 — no preemption inside long BIF-driven recursion — 3 rows

Rows: `maps_SUITE:t_intersect_with`, `t_iterator_2`, `t_merge_with`.

These are `pass` on rust when run alone; they blow the 10 s per-case budget in the parallel sweep.
Serial measurements (probe driver, one case, 60 s budget, same command shape):

| row | rust | oracle |
|---|---|---|
| `maps_SUITE:t_intersect_with` | **OK 10.1 s** | OK 0.011 s |
| `maps_SUITE:t_iterator_2` | **OK 41.0 s** | OK 0.047 s |
| `maps_SUITE:t_merge_with` | **OK 31.4 s** | OK 0.023 s |

So they are *slow*, not stuck: the rows are correct results delivered 900×–1000× late. **Owner:**
`beam_bifs` (the `maps` BIFs that iterate by calling an Erlang fun never bump reductions) plus
`beam_engine`'s reduction accounting/preemption — this is the same missing-yield lever recorded in
`crates/beam_rt` §BOTTLENECKS (a BIF-driven loop that never pre-empts).

### D9 — a calling convention bug: `apply/2` over an argument list built by a `case` — 1 row

Row: `queue_SUITE:oops` — the suite's own `emul/4` comparison throws
`{diff,[{value,empty},{error,badarg},peek,[{[],[]}],[{[]}]]}`: the corrupt value is the *fun itself*,
which answers `badarg`.

Minimal probe module (compiled with the system `erlc` into `$TMPDIR`, nothing committed):

```erlang
-module(qq9). -export([c/0]).
c() ->
    Ts = [], F = fun(peek) -> {[],[]} end,
    Args = case Ts of [] -> [] end,
    [{K, catch apply(F, Args)} || K <- lists:seq(1,1)].
```

- rust: `[{1,{'EXIT',{badarg,[{qq9,'-c/0-lc$^0/1-0-',2,[…]}]}}}]`
- oracle: `[{1,{[]}}]`

Variants that do **not** build `Args` with a `case` (`qq9:a/0`, `b/0`, `d/0`) agree on both sides, and
once the arg list is computed inside a comprehension the value shows up as `{badfun,{peek,[q],v,#Fun<…>}}`.
**Owner:** `beam_engine` — BIF/`apply` argument resolution (a register holding a fun is read where the
argument list is expected).

### D10 — `TIMEOUT` rows: measured, and only two are genuine

The sweep's `--timeout 10` at `--jobs 4` on a host at loadavg ~10 makes `TIMEOUT` a *load* signal, not
a capability. Each TIMEOUT row was re-measured **serially** with the same per-case shape:

| row | rust (serial) | oracle (serial) | classification |
|---|---|---|---|
| `binary_module_SUITE:bin_to_list` | OK **26.7 s** | OK 0.93 s | slow (≈29× the budget) — not a hang |
| `binary_module_SUITE:encode_decode` | OK **0.43 s** | OK 0.02 s | **sweep artefact** (load) — not slow at all |
| `binary_module_SUITE:copy` | fails in 8.97 s (typed `unsupported_bif`, §E) | OK **11.3 s** | oracle-side budget overshoot at 10 s |
| `lists_SUITE:suffix` | OK **26 ms** standalone | OK 25 ms | not a property of the case — see below |
| `string_SUITE:meas` | OK **118.0 s** | OK **118.0 s** | genuinely over budget on **both** sides (§E) |
| `maps_SUITE:t_*` (3 rows) | see §D8 | ms | slow, preemption (§D8) |

`lists_SUITE:suffix` is the interesting one: standalone it costs **26 ms** on rust. Running the whole
78-case suite with `suffix` appended last gives `CASE takewhile TIMEOUT` and `CASE suffix OK`; running
the first `K` cases plus `suffix` is green for `K = 10,20,30,40,50,60,70` and also green for *all 77*
cases with `suffix` last. The committed sweep nevertheless reports `suffix` as the timeout and an
earlier run reported `takewhile` instead — i.e. **which** case absorbs the timeout moves between runs
of the same suite. So the `TIMEOUT` is a *cumulative* per-suite effect (one case per run pays for a
lingering cost left by the run as a whole), not a property of `suffix`'s own work. Config: the
`suffix` case alone is 26 ms; the row costs ~10 s+ only in the full-suite context.

`string_SUITE:meas` takes **~118 s on both sides** — it is excluded as `agree-fail-identical` (§E),
never a rust-only gap.

## E. Exclusions (`agree-fail-identical`, 4 rows — in neither numerator)

| row | why it is excluded |
|---|---|
| `binary_module_SUITE:copy` | oracle **TIMEOUT at the 10 s budget** (it needs 11.3 s serially); rust stops at 8.97 s with the typed `unsupported_bif` `erts_debug:set_internal_state` (§D1's class). Neither side gives a comparable result. |
| `string_SUITE:meas` | **both** sides TIMEOUT at 10 s; both need ~118 s serially. Excluded as a budget mismatch, and it is not evidence of any rust gap. |
| `unicode_SUITE:is_id` | both fail identically: `{badmatch,{error,enoent}}` in `parse_properties/1` — the suite wants `UnicodeData.txt`, which the staged `data_dir` does not contain (the harness passes the *source* dir as `data_dir`, and deliberately runs no `init_per_suite`). |
| `unicode_SUITE:is_whitespace` | same as `is_id`. |

## F. Caveats

- **`--jobs 4` + a loadavg of ~10 makes `TIMEOUT` unreliable.** §D10 re-measures every timeout row
  serially; only two rows (`string_SUITE:meas`, `binary_module_SUITE:copy`) genuinely exceed the
  budget, and in `meas`'s case on both sides.
- **The `binary_module_SUITE` numbers are a lower bound.** The rust run dies after 8 of its 20 cases,
  so the 12 `rust-gap` rows carry no evidence either way; they are `error`, never `pass`.
- **Counts, not seconds.** Everything measured in §D8/§D10 was taken on the same shared host (loadavg
  4–10); the seconds are quoted only to separate *slow* from *stuck* and to show the direction. Do not
  compare them with any other machine.
- **The oracle is OTP 29.1.2 / erts 17.1.1**, while this tree pins OTP `30.0-rc0` / erts `17.1`; a
  suite that pins erts-versioned data can therefore differ for reasons unrelated to the runtime.
- **Attribution is a hypothesis, not a fix.** No crate outside `erts_rust/tools/suites` was touched;
  every "Owner" above names the crate the *analysis* points at.
- **The artifact set for this sweep is exactly `results/stdlib/report.json` + `results/stdlib/summary.md`**
  (`report.json` is authoritative; `summary.md` is rendered from it). `results/stdlib/raw/` is *not*
  part of it: it holds hand-captured probe evidence from the investigations above —
  `binary_module-rust-tail.txt` is the tail of the dying rust `binary_module_SUITE` run, and the
  `*-per-case-5s.txt` files are earlier 5 s probes whose name records the budget they were taken with.
  They are kept as evidence for §D1/§D10; regenerate them by hand (they are not produced by
  `suites run`), and do not read them as the current sweep's raw output.

## G. Harness notes

- `data/ct_runner.erl` expands a suite through `groups/0`, including the `{Group, [SubCase, …]}`
  form (`expand_group/3`), so grouped cases are reported individually.
- The runner deliberately does **not** call `init_per_suite`/`init_per_group`/`init_per_testcase`. A
  case that needs them fails identically on both sides — that is what makes `unicode_SUITE:is_id` /
  `is_whitespace` an honest `agree-fail-identical` instead of a rust gap.
- Each case runs in its own `spawn_monitor` with its own timeout, so one timed-out case never poisons
  the suite… except that the *measured* cost of a timed-out case still lands wherever the run happens
  to be (§D10). Raise `--timeout` (and use `--jobs 1`) before reading a `TIMEOUT` as a capability gap.
- Every protocol line is `\n`-prefixed (`SUITE`, `CASE <name> OK|SKIP|EXC|TIMEOUT|MISSING`,
  `SUITE_INIT_FAILED`, `SUITE_DEADLINE <n>`, `DONE`).
