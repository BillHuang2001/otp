# stdlib sweep — analysis of the committed artifacts

Analysis of **`results/stdlib/report.json`** (schema `suites/1`) with its rendered
`results/stdlib/summary.md`, and of the budget re-check **`results/stdlib/recheck-120/report.json`**.
Every number below is re-derived from those two JSONs; the environment block is copied from each report's
own `host`/`rust`/`generated_at`/`run_command` fields, and the per-class verdicts cite
**`results/stdlib-diagnostics/FINDINGS.md`** (the re-verification of the classes against this runtime,
with the probe list and raw measurements). Rust side = `erts_rust/target/release/beam_rt`; oracle = the
system `erl` on `$PATH`. **The host is shared** — only pass/fail counts, verdicts, ratios and directions
are meaningful; no absolute wall time here is portable.

## A. Environment (from the reports)

| field | recorded sweep | budget re-check |
|---|---|---|
| artifact | `results/stdlib/report.json` | `results/stdlib/recheck-120/report.json` |
| `generated_at` | `1793266383` | `1793266833` |
| suites | the 8 of §B | `maps`, `lists`, `binary_module` |
| `--timeout` / `--suite-budget` / `--jobs` | `10` / `300000` / `4` | `120` / `900000` / `1` |
| `host.oracle` | `/nix/store/…-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` | same |
| `host.oracle_version` | `29/17.1.1` (tree pins OTP `30.0-rc0`, erts `17.1`) | same |
| `host.nproc` | 16 | 16 |
| `host.loadavg_before` | `3.98 5.95 17.43 3/843 3605` | `6.28 6.20 16.28 3/881 4704` |
| `host.loadavg_after` | `6.28 6.20 16.28 2/881 4701` | `4.72 5.49 12.15 2/879 5297` |
| `rust.path` | `erts_rust/target/release/beam_rt` (per-worktree, git-ignored) | same |
| `rust.sha` | `e18f151ba89a30f3dffb39d68805b985a1c98133` | `e18f151ba89a` (same worktree) |
| `rust.version` | `30/17.1` | same |
| `rust.tier` | `null` (default baseline-JIT tier) | same |

Both reports were written from the same worktree and name the same workspace commit (`e18f151b…`), and the
runtime sources themselves are `71ffbbb37971d0b193e0dc74b60b6c448874349e`: no file under
`erts_rust/crates` differs between that commit and the current tree
(`git diff --stat 71ffbbb37971d0b193e0dc74b60b6c448874349e HEAD -- erts_rust/crates` is empty), so both
sweeps describe the current runtime sources. The sha names a commit, not a binary hash: `beam_rt` is
per-worktree and git-ignored. In the recorded report `targets[0].build` is `erlc -o <stage>/ebin -I
<stage>/src <suite>.erl …` with the oracle's `erlc` (`ok: true`, `reused: true`, "compiled 8 suites + 6
helpers + runner"); `targets[0].code_path` is `<stage>/ebin` + `<stage>/ctl` on both sides.

Load was ~4–6 during the recorded sweep and ~5–6 during the re-check (the diagnostics probes ran up to
~12) on a **shared** host, which is why the `TIMEOUT` rows are re-measured serially instead of read as
capability statements. Every suite runs in its own cwd (`<stage>/cwd/<SUITE>`) that the harness
**clears before the row starts** (`target::fresh_dir`), so a fixture tree left by an earlier sweep cannot
poison either side;
both sides of a row share it. Invocation shape for every row (per `targets[0].invocations`), only the
binary differing:

```
cd <stage>/cwd/<SUITE> && <RUNTIME> -noshell -pa <stage>/ebin -pa <stage>/ctl \
  -eval 'ct_runner:run(<SUITE>, 10000, 300000, "<stage>/src"), halt(0).'
```

## B. Per-suite results

`rows` = ct cases reported for that suite; P/F are the case counts per side; verdicts are the harness's
`agree` / `agree-fail-identical` (`afi`, the excluded rows of §E) / `divergence` / `rust-gap`.

| suite | rows | sys P/F | rust P/F | agree | afi | divergence | rust-gap |
|---|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 18 / 2 | 9 / 11 | 9 | 2 | 9 | 0 |
| `dict_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 | 0 |
| `lists_SUITE` | 78 | 78 / 0 | 77 / 1 | 77 | 0 | 1 | 0 |
| `maps_SUITE` | 32 | 32 / 0 | 20 / 12 | 20 | 0 | 12 | 0 |
| `proplists_SUITE` | 4 | 4 / 0 | 4 / 0 | 4 | 0 | 0 | 0 |
| `queue_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 | 0 |
| `string_SUITE` | 46 | 45 / 1 | 45 / 1 | 45 | 1 | 0 | 0 |
| `unicode_SUITE` | 20 | 18 / 2 | 8 / 12 | 8 | 2 | 10 | 0 |
| **total** | **214** | **209 / 5** | **177 / 37** | **177** | **5** | **32** | **0** |

`dict_SUITE`, `proplists_SUITE` and `queue_SUITE` agree on every case; `lists_SUITE` and `string_SUITE`
differ on one row each (a 10 s-budget timeout and an identically failing case). The oracle's five failures are
`binary_module_SUITE:copy` and `string_SUITE:meas` (both over the 10 s budget),
`unicode_SUITE:is_id`/`is_whitespace` (missing `UnicodeData.txt`) and `binary_module_SUITE:interesting`
(`{badmatch,false}` in `do_interesting/1` on both sides). No row is `rust-gap` or `not-run`.

The re-check covers three of the eight suites, which own 12 of the 15 rust-side `TIMEOUT` rows:

| suite | rows | sys P/F | rust P/F | agree | afi | divergence |
|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 19 / 1 | 13 / 7 | 13 | 1 | 6 |
| `lists_SUITE` | 78 | 78 / 0 | 78 / 0 | 78 | 0 | 0 |
| `maps_SUITE` | 32 | 32 / 0 | 23 / 9 | 23 | 0 | 9 |
| **total** | **130** | **129 / 1** | **114 / 16** | **114** | **1** | **15** |

## C. Totals

- **214 rows** measured; no row is `not-run`, none is `oracle-only-fail` and none is `rust-gap`.
- **Oracle: 209 rows pass, 5 fail.** Rust: **177 pass, 37 fail**.
- **Fully green rows: 209 of 214** on the oracle and **177 of 214** on rust; **5 rows are excluded from
  both numerators** (`agree-fail-identical`, §E).
- **Verdict split:** `agree 177` · `agree-fail-identical 5` · `divergence 32`.
- **No rust pass corresponds to an oracle failure.** All 177 `agree` rows have `sys.status = pass`; the
  only rows whose rust side lacks a pass are the `afi` ones, where **both** sides fail.
- **Every suite runs to its end in one process**: a suite is 20–78 cases and the per-case budget is what
  stops a case, so no row loses its turn to an earlier crash.
- **Re-check (120 s, serial):** 130 rows over 3 suites — sys 129/1, rust 114/16; verdicts
  `agree 114` · `divergence 15` · `agree-fail-identical 1` (`binary_module:interesting`). It moves 7 of
  the re-checked suites' 22 divergence rows to `agree` (`binary_module:bin_to_list`, `parts`,
  `random_ref_fla_comp`, `lists:suffix`, `maps:t_intersect`, `t_iterator_2`, `t_merge_with`), leaves two
  over the budget (`binary_module:random_ref_comp`, `random_ref_sr_comp`) and shows two as D1's
  `system_limit` rather than slowness (`maps:t_keys_trapping`, `t_values_trapping`).
  `binary_module:copy` moves from excluded to **`agree`**: both sides time out at 10 s and both pass at
  120 s.

## D. Non-agree rows, by class

37 rows are non-agree in the recorded sweep (32 `divergence` + 5 `afi`); the `afi` rows are
§E's exclusions and are in neither numerator.
Of the 32 divergence rows, 20 carry a class id below or in §D12/D12b, 1 is D12b's latin1 row and the
remaining 11 are budget or pure performance (§D8, §F): 9 agree at a larger budget and 2 are the
`random_ref_*` rows.
`<rt>` below means either binary. Class ids are the ones in
`results/stdlib-diagnostics/FINDINGS.md`, which re-verifies each one against the current runtime: **5 fixed**
(D1b, D3, D7, D9, D10), **5 still present** (D1, D2, D4 — `maps:put/3` identity on the compiled route — D8,
D11), **1 partially present** (D5, the legacy-`catch` head frame only), **2 changed class** (D1c — the suite
abort is gone, the raise reason is now the placeholder atom `'exit <structured reason>'` — and D6) and **2 new**
(D12, a `utf32` segment ignoring `little`/`native`, and D12b, `unicode:characters_to_binary/3`'s latin1
error split). `FINDINGS.md` is authoritative for every verdict below.

### D1 — the term-heap `system_limit` — 3 rows, one mechanism

**D1a, allocation inside a `lists` BIF callback — 3 rows.** `maps_SUITE:t_from_keys_check_trapping`,
`t_keys_trapping`, `t_values_trapping` all end `{badmatch,system_limit}` in
`maps_SUITE:count_nr_of_yields/2` (a 1 000 000-element list built inside a fun); in the recorded sweep the
two `t_*_trapping` rows reach that answer only in the 120 s re-check, after a 10 s-budget timeout.
The reduced class is *"a large allocation inside a fun invoked by a higher-order `lists` BIF"*:

```
<rt> -noshell -eval 'io:format("~p~n",[lists:foreach(fun(_) ->
      length(lists:duplicate(65537, x)) end, [1])]), halt(0).'
```

- oracle: `ok`; rust: catchable `error:system_limit` — uncaught it terminates the runtime with
  `beam_rt: system_limit`, exit 1.

The threshold is exactly **65 536** list elements (`FINDINGS.md` §D1: `N = 65536` is `ok`, `N = 65537`
refuses — 131 072 words, 1 MiB of cons cells); the same work in a tail-recursive loop, in a
directly-called fun, in a `spawn_monitor` child or at `N = 65536` is `ok`, so only the *higher-order BIF
callback* shape refuses. It is a plain `error`, not a wrong value.
**Owner:** `beam_rt` (heap admission / `enforce_term_heap_limit` from a BIF-invoked fun).

**D1c, the suite-level abort is gone; the raise reason is a placeholder — 1 row.**
`binary_module_SUITE:error_info` is the only D1c row, and it no longer takes the suite down: all 20
`binary_module_SUITE` cases are measured in one process now.
What remains is the *death-boundary reason*: `error_info_abort_probe:main({prefix, 70})` (the suite's
`error_info/1` list driven through `ct_runner:run_case/4`'s `spawn_monitor` + `try`/`catch` +
`exit({ct_case_result, R})` shape) answers `{ok,ok}` on the oracle and hands the parent the atom
`'exit <structured reason>'` here — the `Display` of a structured raise reason, interned when the live
reason was dropped (`crates/beam_rt/src/runner/reason.rs` over `runtime/gc.rs`'s
`stabilize_error_live_reason` → `degrade_live_reason`, whose price/copy gate refuses this payload).
An Erlang program can match that atom, so it is a wrong value rather than a typed gap.
**Owner:** `beam_rt` (the escape-boundary stabilization; the fallback should degrade to the bare
class/reason instead of an invented atom).
The standalone measurements behind the two slow `binary_module_SUITE` rows still hold and are quoted
where they matter (§D8, §F): `random_ref_comp` and `random_ref_sr_comp` are over 400 s against the
oracle's 4 s / 3 s, while `random_ref_fla_comp`, `parts`, `bin_to_list` and `list_to_bin` finish and agree
once the budget is 120 s.

### D2 — `binary:referenced_byte_size/1` of a sub-binary answers its own size — 6 rows

Rows: `binary_module_SUITE:doctests` (`{1,errors}` — the failing doctest is this expression, `A test
failed in function referenced_byte_size/1: 6> {byte_size(C), binary:referenced_byte_size(C)}.`),
`binary_module_SUITE:referenced` (`{badmatch,1023}`), `unicode_SUITE:binaries_errors_limit` and
`unicode_SUITE:ex_binaries_errors_utf8`, `_utf16_big`, `_utf16_little` (`{badmatch,false}` inside
`utf*_inner_loop/6` / the utf8 list comprehension — they assert
`true = (binary:referenced_byte_size(Chomped) =:= PBSz)`).
The two `utf32` rows are **D12**, not this class.

```
<rt> -noshell -eval 'Part = <<"z",(binary:copy(<<255>>,1024))/binary>>,
                      C = binary:part(Part,1,510),
                      io:format("~p~n",[{byte_size(C), binary:referenced_byte_size(C)}]), halt(0).'
```

- rust: `{510,510}` · oracle: `{510,1025}`

**Owner:** `beam_bifs` (the binary BIF) over `beam_term`/`beam_alloc` sub-binary bookkeeping — the
runtime does not track the referenced (parent) binary of a sub-binary.

### D5 — the BIF frame is missing from the trace head on the legacy `catch` route — 4 rows

Rows: `maps_SUITE:t_from_keys`, `t_with_2`, `t_without_2`, `t_merge_opt`. Each case matches a
`?badmap`/`?badarg` macro of the form `{'EXIT', {{badmap,V}, [{maps,F,Args,_}|_]}}`, i.e. the **first**
trace frame must be the raising `maps` BIF. Rust answers a trace whose only frame is the caller:

```
-module(sp). -export([m/0]).
m() -> catch maps:with([a,b], id(a)).
id(X) -> X.
```

- rust: `{'EXIT',{{badmap,a},[{sp,m,0,[{file,…},{line,3}]}]}}` — no `{maps,with,…}` frame
- oracle: `{'EXIT',{{badmap,a},[{maps,with,[[a,b],a],[{file,"maps.erl"},{line,1303},…]},{sp,m,0,…}]}`

The frame is present when the same raise is caught by `try … catch` (the shape of
`results/stdlib-diagnostics/stacktrace_head_probe.erl`, whose `maps`/`binary`/`lists` rows agree on both
sides), and absent when it is caught by the legacy `catch Expr` expression — the shape these four suites
use. **Owner:** `beam_engine` (`families/exceptions/callee.rs`, trace-head synthesis on the `catch` path).

### D6 — `erlang:bump_reductions/1` is not served — 2 rows

Rows: `maps_SUITE:doctests` (`{1,errors}`) and `binary_module_SUITE:longest_common_trap`
(`{exception,error,unsupported_bif,{erlang,bump_reductions,[…]}}`). The captured doctest transcript
(`results/stdlib/raw/maps-rust-per-case-5s.txt`) names the failing doctest expression:

```
** exception error: 'unsupported: erlang:bump_reductions/1'
     in function  erlang:bump_reductions/1
        called as erlang:bump_reductions(134217727)
     in call from prim_eval:arg_reg_alloc/0
```

- compiled code: a catchable `error:unsupported_bif` (legacy `catch` sees `{'EXIT',{unsupported_bif,[…]}}`)
- through `-eval`, uncaught: the CLI-level `beam_rt: unsupported: bif`, exit 3 — the suite never observes this,
  because `setlimit`-style callers are compiled code and the reason is a real Erlang exception there
- oracle: `true`

A typed gap, never a wrong answer; the two manifestations differ by caller route (the doctest evaluator
catches the Erlang error, the `-eval` driver reports the CLI signal). **Owner:** `beam_bifs`.

### D8 — no preemption inside a BIF-driven Erlang loop — 2 rows

Rows: `maps_SUITE:t_merge_with`, `t_iterator_2`. Both blow the 10 s per-case budget in the parallel sweep
(rust `fail`, detail `timeout`) and both pass in the serial 120 s re-check, so their class is slowness,
not a wrong result. Isolated ratios (`maps_bif_loop_probe.erl`, 1 000-key maps, 50 rounds, one invocation
per runtime, run twice with the order swapped, host loadavg ~19 — only the ratio is meaningful):

| workload | rounds × size | ratio, run 1 | ratio, run 2 |
|---|---|---|---|
| `maps:merge_with/3` | 50 × 1000 keys | **662×** | **614×** |
| `maps:intersect_with/3` | 50 × 1000 keys | **92×** | **95×** |
| `maps:iterator/1` + `maps:next/1` | 50 × 1000 entries | **438×** | **480×** |
| `maps:to_list/1` (no Erlang callback) | 50 × 1000 entries | 3.7× | 3.7× |

Both sides compute the same work (the probe's checksum is `0` on both sides for every row). The last row
is the control: a pure BIF over the same map costs ~4–5×, so essentially the whole residual is the
per-callback-call cost of a BIF that keeps invoking an Erlang fun. **Owner:** `beam_bifs` (the `maps` BIFs
that iterate by calling an Erlang fun never bump reductions) plus `beam_engine`'s reduction
accounting/preemption. A third timeout row, `maps_SUITE:t_intersect`, is not of this shape — it is the
suite's heavy `check_map_combiners_same_small/large` workload (a fold over `gb_sets` intersections),
which also agrees in the 120 s re-check.

### D9 — `erlang:apply/2` — served; the row agrees — 0 rows

The value matrix now agrees (`apply_args_probe.erl`, `queue_oops_probe.erl`): a fun applied to a proper
argument list, `badarg` for a non-fun / non-list and for `apply(erlang, self)`. The only residual is the
arity-mismatch **reason**, which degrades `{badarity,{Fun,Args}}` to plain `badarg` (**Owner:** `beam_rt`,
the native floor's arity check). `queue_SUITE:oops` is `pass`/`pass` = `agree` in `../stdlib/report.json`.
The paragraphs below record the mechanism the fix addressed; `FINDINGS.md` §D9 carries the current matrix.

Row: `queue_SUITE:oops`. The case drives 3 142 random `queue` operations and compares `queue:*` with the
suite's own emulation model; it throws `{diff,[{value,empty},{error,badarg},peek,[{[],[]}],[{[]}]]}` —
the `queue` side answers `empty`, the **model** side answers `{error,badarg}`. The divergence is at op 1
(the two queue states never disagree); the model side is the suite's own
`call(Func, As) -> … apply(Func, As)` helper, so the failing call is `erlang:apply/2`.
Patching that one `apply/2` in the suite's helper to a direct fun dispatch makes the Rust side run all
3 142 ops to `ok`, which attributes the row to `apply/2` and to nothing in `queue`.

`erlang:apply/2` used to be unimplemented: a genuine `call_ext erlang:apply/2` reached `beam_rt`'s native
floor, which decoded it as the `apply/3` `(Module, Function, Args)` shape
(`crates/beam_rt/src/native.rs`, `apply_dynamic`), so the two operands are read as a module and a
function name.

Minimal probe module (compiled with the system `erlc` into `$TMPDIR`; the `-eval` evaluator has its own
correct `apply/2` and cannot see this, so the probe must be compiled):

```erlang
-module(m). -export([smallest/1]).
smallest(Args) -> apply(fun(X) -> X end, Args).
```

- rust: `error:badarg` · oracle: `a`

The matrix (`apply_args_probe.erl`, `queue_oops_probe.erl`): a literal argument list, `apply/3`, `F(Args)`
and the `-eval` evaluator all agree — the compiler rewrites a literal-length argument list into
`call_fun` (`erlc -S` shows `{call_fun,1}`), so only a *dynamically* built list reaches the floor.
It was wrong on the value side too, not just the error side: `apply(erlang, self)` answered a pid where
the oracle raises `badarg` (it ran `erlang:self/0`), and `apply(lists, reverse)` answered `undef`; both are
`badarg` on both sides now.
**Owner:** `beam_rt` (the native `apply/2` floor) for the one residual reason shape. The engine and `beam_bifs` are correct by contract —
`crates/beam_engine/src/families/funs/call_ext_apply.rs` pins that arity 2 must reach the host's own
`apply/2`.

### D10 — `erts_debug:set_internal_state/2` — served (the xchg rule agrees) — 0 rows

D10 owns **no** row of the current sweep: the `setlimit/1`-driven `unicode_SUITE` cases that used to hit
it are measured now, and the divergence rows among them belong to D2 (`binaries_errors_limit`,
`ex_binaries_errors_utf16_*`, `_utf8`), D12 (`_utf32_*`, `roundtrips`) and D12b (`latin1`).

The gate call has to be reproduced carefully, because the oracle needs access enabled first:

```
erts_debug:set_internal_state(available_internal_state,true)  → false   (previous value; both sides agree)
erts_debug:set_internal_state(unicode_loop_limit,10)          → 40000   (previous limit, oracle)
                                                              → error:unsupported_bif (rust, compiled)
```

Both sides answer the **previous** value for every limit (`unicode_loop_limit` `40000`, `binary_loop_limit` `0`,
`re_loop_limit` `64000`), the gate-off call answers `undef` on both sides, and an uncaught `-eval` call no longer
leaves through the CLI channel. **Owner:** `beam_bifs` (`src/runtime/erts_debug.rs`).
`binary_module_SUITE:copy` is therefore no longer blocked by this class: both sides time out at the 10 s
budget and both **pass** it at 120 s.

### D11 — `erlang:trace_delivered/1` rejects an exited pid — 1 row

Row: `maps_SUITE:t_from_list_check_trapping` (`{exception,error,badarg,{erlang,trace_delivered,…}}`).
`count_nr_of_yields/2` calls `erlang:trace_delivered(Pid)` **after** the traced child's `{'DOWN',…}` has
arrived:

```
<rt> -noshell -eval 'P = spawn(fun() -> ok end), R = monitor(process,P),
    receive {'"'"'DOWN'"'"',R,process,P,_} -> ok end,
    io:format("~p~n",[catch erlang:trace_delivered(P)]), halt(0).'
```

- rust: `{'EXIT',{badarg,[]}}` · oracle: `#Ref<…>`

For a **live** pid both sides answer a delivery ref, so the function is served; what is missing is the
delivery bookkeeping for a pid that has already exited. **Owner:** `beam_rt`
(`src/trace.rs::trace_delivered_1` rejects a pid the process table no longer holds, while OTP's
`trace_delivered_1` accepts any internal pid plus `all`).

### D12 / D12b — the `utf32` segment flag, and the latin1 error split — 4 rows
D12 (3 rows): `unicode_SUITE:ex_binaries_errors_utf32_big`, `_utf32_little` and `roundtrips`. Construction of
a `utf32` segment with an explicit `little`/`native` flag drops the flag for a runtime value —
`f(E) -> <<E/utf32-little>>.` called `f(1)` answers `<<0,0,0,1>>` here and `<<1,0,0,0>>` on the oracle — while a
compile-time constant, `<<E/utf32>>`, `<<E/utf32-big>>` and the `utf16`/integer families agree.
**Owner:** `beam_engine` (`families/bitstrings/create`: `put_utf32` takes no `little` parameter).
D12b (1 row): `unicode_SUITE:latin1` fails at staged `unicode_SUITE.erl:414`, where
`{error,DoubleUtf8,Rest2} = unicode:characters_to_binary([All,AllBin,16#FFF], latin1)` followed by
`[16#FFF] = lists:flatten(Rest2)` answers `{badmatch,<<0,0,…>>}` — the wrong remainder for a latin1 error in a
mixed character-list/binary argument. **Owner:** not yet attributed (candidate: `beam_bifs`'s
`unicode:characters_to_binary/3` error/rest construction on the latin1 character-list path).
`results/stdlib-diagnostics/FINDINGS.md` §D12 and §D12b carry the full matrices.
### Classes that own no row in this sweep

`erlang:trace/3` is served: `erlang:trace(self(),true,[running,{tracer,self()}])` answers `1` on both
sides, and `trace_info(self(),flags)` reads back `{flags,[running,call]}` (D3). `erlang:system_info(snifs)`
publishes 556 `{M,F,A}` triples against the oracle's 553 — each runtime's own static-NIF table, so the
counts are not expected to match (D7). D4 (map no-op identity) survives only on the **compiled** route and
only for `maps:put/3` on a map with ≥ 2 keys (`erts_debug:same(maps:put(a,1,M),M)` → `false` vs `true`; the
`-eval` route, the `M#{a=>1}` update syntax, `maps:merge(#{},M)` and a 1-key flatmap all agree), and no
suite row reaches it.

## E. Exclusions (`agree-fail-identical`, 5 rows — in neither numerator)

| row | why it is excluded |
|---|---|
| `binary_module_SUITE:copy` | **A budget artefact.** **Both** sides TIMEOUT at the 10 s budget (`detail: timeout` on each side), so the row is excluded; at 120 s **both** sides **pass** it, which is why the re-check reports it `agree`. |
| `string_SUITE:meas` | **both** sides TIMEOUT at the 10 s budget (`detail: timeout` on each side). A budget mismatch, and no evidence of a rust gap. |
| `unicode_SUITE:is_id` | both fail identically: `{badmatch,{error,enoent}}` in `parse_properties/1` — the suite wants `UnicodeData.txt`, which lives in `lib/stdlib/uc_spec`, not in the `unicode_SUITE_data` dir the runner resolves, and the suite exports no `init_per_suite` to fetch it. |
| `unicode_SUITE:is_whitespace` | same as `is_id`. |
| `binary_module_SUITE:interesting` | both fail identically with `{badmatch,false}` in `do_interesting/1`, so the row carries no evidence either way. |

## F. Caveats

- **`--jobs 4` on a loaded host makes `TIMEOUT` unreliable.** 13 divergence rows plus 2 excluded ones carry
  a rust-side `timeout` at 10 s; the serial 120 s re-check covers 12 of them and 8 pass there
  (`binary_module:bin_to_list`, `parts`, `random_ref_fla_comp`, `copy`, `lists:suffix`,
  `maps:t_intersect`, `t_iterator_2`, `t_merge_with`), 2 are still over the budget
  (`binary_module:random_ref_comp`, `random_ref_sr_comp`) and 2 are really D1's `system_limit`
  (`maps:t_keys_trapping`, `t_values_trapping`). A `TIMEOUT` is a budget statement, never a pass.
- **The `binary_module_SUITE` numbers are complete at the 10 s budget.** All 20 cases are measured in one
  process (no `rust-gap` row), and the two rows still over the budget at 120 s are performance, not value,
  divergences.
- **Counts, not seconds.** Every ratio in §D8 and every figure in §D1 came from the same shared host, so
  the seconds only separate *slow* from *stuck*; do not compare them with another machine.
- **The oracle is OTP 29.1.2 / erts 17.1.1**, while this tree pins OTP `30.0-rc0` / erts `17.1`; a suite
  that pins erts-versioned data can differ for reasons unrelated to the runtime.
- **Attribution is a hypothesis, not a fix.** No crate outside `erts_rust/tools/suites` was touched; each
  "Owner" above names the crate the analysis points at.
- **The artifact set is `results/stdlib/report.json` + `results/stdlib/summary.md` and the re-check
  `results/stdlib/recheck-120/`** (`report.json` is authoritative; `summary.md` is rendered from it).
  `results/stdlib/raw/` is *not* part of it: hand-captured probe evidence (per-suite stdout, per-case 5 s
  probes, the tail of an earlier `binary_module_SUITE` run), not `suites run` output.

## G. Harness notes

- `data/ct_runner.erl` expands a suite through `groups/0`, including the `{Group, [SubCase, …]}` form
  (`expand_group/3`), so grouped cases are reported individually.
- The runner calls `init_per_suite`/`end_per_suite` when the suite exports them and deliberately does
  **not** call `init_per_group`/`end_per_group`/`init_per_testcase`/`end_per_testcase`; a case that needs
  the latter fails identically on both sides, so it lands in `agree-fail-identical`, never in a numerator.
- Each case runs in its own `spawn_monitor` with its own timeout, so one timed-out case never poisons the
  suite… except that the *measured* cost of a timed-out case still lands wherever the run happens to be;
  raise `--timeout` (and use `--jobs 1`) before reading a `TIMEOUT` as a capability gap.
- Every protocol line is `\n`-prefixed (`SUITE`, `CASE <name> OK|SKIP|EXC|TIMEOUT|MISSING`,
  `SUITE_INIT_FAILED`, `SUITE_DEADLINE <n>`, `DONE`), so a case that prints without a trailing newline
  cannot glue onto the next line.
