# stdlib divergence classes D1…D10 — re-checked against the current Rust runtime

Re-verification of the ten root-cause classes recorded in `../stdlib-notes.md` §D, run
against the **current** Rust runtime at the head of this branch. Measurement/diagnosis
only: no runtime crate was touched.

## Environment and method

| field | value |
|---|---|
| runtime under test | `erts_rust/target/release/beam_rt` built from this worktree (`cargo.sh build -p beam_rt --release`) |
| runtime version | `erlang:system_info(version)` → `"17.1"` |
| branch head | `a56bef586b61fcf0a9b2b67c30f13ba112a8ad7d` |
| oracle | system `erl`/`erlc` on `$PATH`, OTP `29.1.2` / erts `17.1.1` |
| cwd | a throwaway directory under `$TMPDIR/stdlib-diag` (`$TMPDIR/erts-suites-stage/stdlib` for the staged suite beams) |
| host | shared; `loadavg` was ~**32** (`32.20 37.09 22.09`) during the D8 measurements |

Every check runs the **same** argv under both runtimes (`-noshell -pa <ebin> -eval '…, halt(0).'`).
Where a check needs compiled code it is compiled with the **oracle's `erlc`** into `$TMPDIR`
only, so both sides run byte-identical `.beam` files.

Two measurement rules are load-bearing here:

- **Stacktraces must be read from compiled modules.** The `-eval` tree-walk evaluator reports an
  **empty** stacktrace for every raise on the Rust side (`catch lists:reverse(a)` →
  `{'EXIT',{function_clause,[]}}`), so an `-eval`-based stacktrace check measures the evaluator,
  not the runtime. D5 below is therefore measured from `stacktrace_head_probe.erl`.
- **Ratios only.** The host is shared, so no absolute time in this document is portable; D8
  quotes the same-host oracle/rust ratio and the operation counts that produced it.

Reproduce any one-liner with `matrix.sh` (reads expressions on stdin, prints one line per
expression with both sides):

```
ROOT=$PWD bash erts_rust/tools/suites/results/stdlib-diagnostics/matrix.sh <<'EOF'
apply(fun(X) -> X end, [a])
EOF
```

## Verdicts

| # | smallest expression | oracle | rust | verdict |
|---|---|---|---|---|
| **D1** | `lists:foreach(fun(_) -> length(lists:duplicate(200000, x)) end, [1])` | returns `ok` | catchable `error:system_limit`; uncaught → `beam_rt: system_limit`, exit 1 | **still present** (wider: the plain process now fails too) |
| **D1b** | `spawn_monitor(fun() -> binary_module_SUITE:error_info([]) end)` (`error_info_child_probe`) | `DOWN=normal`, 1370 lines | `DOWN={function_clause,[]}`, 622 lines; **runtime survives**, exit 0 | **changed class** (was: whole-runtime abort `beam_rt: system_limit` exit 1; now: child-level `function_clause`, runtime exits 0) |
| **D2** | `C = binary:part(<<"z",(binary:copy(<<255>>,1024))/binary>>,1,510), {byte_size(C),binary:referenced_byte_size(C)}` | `{510,2050}` | `{510,510}` | **still present** |
| **D3** | `erlang:trace(self(),true,[running,{tracer,self()}])` | `1` | `1` | **fixed** |
| **D3** | set-wise: `trace(…,[running,{tracer,self()}])`, `trace(…,[call])`, `trace_info(self(),flags)` | `1 1 {flags,[running,call]}` | `1 1 {flags,[running,call]}` | **fixed** |
| **D4** | `M=#{a=>1}, {erts_debug:same(maps:merge(#{},M),M), erts_debug:same(maps:put(a,1,M),M)}` | `true true` | `true true` | **fixed** |
| **D5** | `maps:with([a,b], (fun(X) -> X end)(a))` — from a **compiled** module | head `{maps,with,[[a,b],a]}` | head `{maps,with,[[a,b],a]}` | **fixed** (see the residual frame note) |
| **D6** | `catch erlang:bump_reductions(134217727)` | `true` | `beam_rt: unsupported: erlang:bump_reductions/1`, exit 3 | **changed class** (was a catchable `{'EXIT',{unsupported_bif,_}}`; now the CLI-level `unsupported` signal runs past `catch`) |
| **D7** | `erlang:system_info(snifs)` | list of **553** `{M,F,A}` | list of **556** `{M,F,A}` | **fixed** (published; the tables themselves differ) |
| **D8** | `maps:merge_with(fun(_,V1,V2)->{V1,V2} end, M1, M2)`, 1000-key maps, 50 rounds | reference | **580× / 764×** the oracle | **still present** |
| **D8** | `maps:intersect_with/3`, same shape | reference | **94× / 107×** | **still present** |
| **D8** | `maps:iterator/1` + `maps:next/1` walk, 1000 entries, 50 rounds | reference | **442× / 525×** | **still present** |
| **D8** | `maps:to_list/1` (reference: a BIF that calls no Erlang fun) | reference | **4.8× / 4.3×** | reference row — shows the gap is callback-specific |
| **D9** | `smallest(Args) -> apply(fun(X) -> X end, Args).` called as `smallest([a])` | `a` | `error:badarg` | **still present** (and wider than recorded) |
| **D10** | `erts_debug:set_internal_state(available_internal_state,true)`, then `…(unicode_loop_limit,10)` | `false`, then `40000` + the `Process <0.10.0> enabled access …` warning report | `false`, then `beam_rt: unsupported: bif`, exit 3 | **changed class** (was a catchable `unsupported_bif`; now the CLI-level `unsupported` signal) |

Counts: **4 fixed** (D3, D4, D5, D7) · **4 still present** (D1, D2, D8, D9) · **3 changed class**
(D1b, D6, D10).

## Notes per class

### D1 / D1b — the `system_limit` abort

The recorded fix ("need-aware per-depth BIF-region reservation") is **not observable from the
suites' perspective**: `results/stdlib-diagnostics/bigterm_spawn_probe.erl` still aborts on its very
first line with `beam_rt: system_limit`, exit 1, in the **plain** process.

Reduced (`k3`/`k4` measurements, not committed — reproduce via `matrix.sh`):

| shape | oracle | rust |
|---|---|---|
| `length(lists:duplicate(1 bsl 21, x))` direct | ok | ok |
| the same 5× in a tail-recursive loop | ok | ok |
| the same inside `fun() -> … end` called directly | ok | ok |
| the same inside a `spawn_monitor` child | ok | ok |
| the same inside a `lists:foreach/2` fun | ok | `error:system_limit` |
| the same inside a `lists:map/2` fun | ok | `error:system_limit` |
| the same via `lists:foreach` at 131072 elements | ok | ok |
| the same via `lists:foreach` at 200000 elements | ok | `error:system_limit` |

So the class is now *"a large allocation inside a fun invoked by a higher-order `lists` BIF"*, not
*"a spawned child"*: the threshold sits between **131072** and **200000** list elements (2–3 MiB of
cons cells), the raise is a **catchable `error:system_limit`**, and an uncaught one terminates the
runtime with `beam_rt: system_limit` + exit 1 — exactly the recorded signature. It is a plain
`error`, not a typed capability gap.

The suite-level path moved in the opposite (better) direction: `error_info_child_probe.erl` now
returns `{ok,ok}` in the main process and lets the runtime exit **0**; only the child dies, with a
`{function_clause,[]}` after 622 printed lines (oracle: `normal`, 1370 lines). The notes' count
(`464` lines) does not match either side today, so use the probe's own numbers.

### D2 — `binary:referenced_byte_size/1`

Unchanged: a sub-binary still reports its own size, so `unicode_SUITE`'s five
`binary:referenced_byte_size(Chomped) =:= PBSz` assertions and `binary_module_SUITE:doctests` still
fail for the same reason.

### D3 — `erlang:trace/3`

Both the single-flag form and the set-wise form agree with the oracle, including the flags read back
by `trace_info(self(),flags)`. The four `maps_SUITE:t_*_trapping` rows that failed on
`{badmatch,badarg}` in `count_nr_of_yields/2` should now get past this point (their remaining
behaviour was not re-measured here).

### D4 — map identity

`erts_debug:same/2` is `true` for both the no-op `maps:merge(#{},M)` and the no-op
`maps:put(a,1,M)`, matching the oracle.

### D5 — stacktrace head

Fixed for the recorded claim: the trace head names the **BIF** (`{maps,with,[[a,b],a]}`,
`{maps,keys,[a]}`, `{erlang,map_get,[a,b]}`, `{binary,part,[<<1,2>>,0,5]}`) on both sides, and the
following frames agree too.

Residual (a *different*, smaller class): where the raising call is a **tail call out of an anonymous
fun**, the Rust trace keeps the fun's frame while the oracle has replaced it —
`[ {maps,with,[[a,b],a]}, stacktrace_head_probe:'-main/0-fun-0-',0, stacktrace_head_probe:row/2 ]`
vs the oracle's `[ {maps,with,[[a,b],a]}, stacktrace_head_probe:row/2, stacktrace_head_probe:main/0 ]`.
That also shifts the `-main/0-fun-N-` numbering by one on those rows. `maps:keys/1`,
`maps:get/2` and `binary:part/3` (BIF calls, no tail-call-from-fun) match exactly.

### D6 — `erlang:bump_reductions/1`

Still not served, but the **failure surface changed**: `catch erlang:bump_reductions(134217727)`
does not catch — the runtime terminates with one `beam_rt: unsupported: erlang:bump_reductions/1`
line and exit **3** (the CLI capability-gap channel), where the notes recorded a catchable
`{'EXIT',{unsupported_bif,…}}`. Still a typed gap, still never a wrong answer, but a probe that
wraps the call in `catch`/`try` cannot observe it as an Erlang exception any more. Contrast
`erts_debug:flat_size/1` below, whose gap **is** catchable as `error:unsupported_bif`.

### D7 — `erlang:system_info(snifs)`

Now published: `556` elements, all 3-tuples, head `{erlang,ref_to_binary,1}`. The oracle answers
`553` with head `{records,get_definition,2}` — the two lists are each runtime's own static-NIF
table, so the *count* is not expected to match; the capability gap is closed.

### D8 — BIF-driven Erlang loops are still ~2–3 orders of magnitude late

`maps_bif_loop_probe.erl`, 1000-key maps, 50 rounds, **one** invocation per runtime, run twice with
the order swapped (host `loadavg` ~32 both times; only the ratio is meaningful):

| workload | rounds × size | ratio, run 1 | ratio, run 2 |
|---|---|---|---|
| `maps:merge_with/3` | 50 × 1000 keys | **580×** | **764×** |
| `maps:intersect_with/3` | 50 × 1000 keys | **94×** | **107×** |
| `maps:iterator/1` + `maps:next/1` | 50 × 1000 entries | **442×** | **525×** |
| `maps:to_list/1` (no Erlang callback) | 50 × 1000 entries | 4.8× | 4.3× |

Both sides computed the same work (the probe's `checksum` is `0` on both sides for every row). The
last row is the control: a pure BIF over the same map costs ~4–5×, so essentially the whole
residual is the per-callback-call cost of a BIF that keeps invoking an Erlang fun — i.e. the class
is unchanged in kind and roughly in size versus the recorded ~900–1000× (the recorded figures came
from full-size suite cases, so the two are not directly comparable; both are "orders of
magnitude", not "slightly slower").

### D9 — `apply/2` mis-reads its argument list (now with a much smaller repro)

Still present, and **wider** than recorded. The trigger is not `case`-built lists: it is the
**second argument of `apply/2` arriving in an incoming argument register**.

```erlang
-module(m).
-export([smallest/1]).
smallest(Args) -> apply(fun(X) -> X end, Args).
```

`m:smallest([a])` → oracle `a`, rust `error:badarg`.

The full matrix (`apply_args_probe.erl`, every row guarded):

| shape | oracle | rust |
|---|---|---|
| `apply(F, [])`-style: fun param, literal arg list | `a` | `a` |
| literal fun, **arg list from a parameter** | `a` | **`badarg`** |
| both fun and arg list from parameters | `a` | **`badarg`** |
| both from a function-clause pattern | `a` | **`badarg`** |
| both bound by a **body** match (`{F,As} = {…}`) | `a` | `a` |
| external fun (`fun lists:reverse/1`), arg list from a parameter | ok / `{badarity,[b,c]}` | **`badarg`** |
| `apply/3` with all three from parameters | `[3,2,1]` | `[3,2,1]` |
| direct call `F(Args)` | `[a]` | `[a]` |
| `lists:foreach(fun({_,F,As}) -> apply(F,As) end, Lit)` | `a` | **`badarg`** |
| arity mismatch (`apply(fun(X)->X end, [])` compiled) | `{badarity,[]}` | **`badarg`** |

So: `apply/2` is correct when the argument list is a literal or a body-bound value, and wrong
whenever it is an **incoming argument register** — and when the arity actually mismatches the
reason degrades from `{badarity,{Fun,Args}}` to plain `badarg`. That is exactly the
`queue_SUITE:oops` shape (`call(Func, As) -> … apply(Func, As)`), which therefore still fails. Note
the `-eval` evaluator happens to answer these correctly, which is why an `-eval`-only probe hides
the bug.

### D10 — `erts_debug:set_internal_state/2`

The oracle's own behaviour has to be reproduced carefully: a bare
`erts_debug:set_internal_state(unicode_loop_limit,10)` answers **`undef`** (as does
`…(available_internal_state)`); access has to be enabled first. On this oracle:

```
erts_debug:set_internal_state(available_internal_state,true)  → false   (previous value)
=WARNING REPORT==== … Process <0.10.0> enabled access to the emulator internal state.
erts_debug:set_internal_state(unicode_loop_limit,10)          → 40000   (previous limit)
```

The notes' `"Process enabled access"` is that warning-report line, i.e. the *earlier* observation
looked at the wrong half of the exchange. The Rust side answers `false` to the gate call (agreeing)
and then `beam_rt: unsupported: bif`, exit **3**. So the capability gap stands, but the class
changed: the raise is now the CLI-level `unsupported` signal (not a catchable `unsupported_bif`),
and the oracle's comparable value is `40000`, not a success message.

## Extra observation (not one of D1…D10): `erts_debug:flat_size/1`

`erts_debug:flat_size/1` raises a **catchable** `error:unsupported_bif` once its argument reaches
**524288 (`1 bsl 19`)** list elements — 500000 elements answer `1000000`, 524288 answer
`unsupported_bif`. The oracle computes both. This has nothing to do with `lists:duplicate/2` (which
is fine up to `1 bsl 21` and beyond) but it *is* what made the first version of the D1 probe abort,
so it is recorded here rather than silently worked around.

## Probe files

| file | class | what it measures |
|---|---|---|
| `apply_args_probe.erl` | D9 | the `apply/2` argument-position matrix (each row guarded) |
| `bigterm_spawn_probe.erl` | D1 | the notes' `lists:duplicate(1 bsl 21, x)` repro 5× plain, 5× in a `spawn_monitor` child, plus a deep-recursion/big-term loop and the `flat_size` threshold |
| `error_info_child_probe.erl` | D1b | `binary_module_SUITE:error_info([])` in the main process and in a spawned child (needs the staged suite `ebin`) |
| `maps_bif_loop_probe.erl` | D8 | `merge_with` / `intersect_with` / `iterator+next` reduced-size workloads, with a checksum and a no-callback control row |
| `stacktrace_head_probe.erl` | D5 | stacktrace head from **compiled** code for `maps:with/2`, `maps:keys/1`, `maps:get/2`, `lists:reverse/1`, `binary:part/3` |
| `matrix.sh` | shared | one-expression-per-line runner that prints both sides; used for the D2/D3/D4/D6/D7/D10 one-liners |

Build and run any of them with:

```
erlc -o $TMPDIR/stdlib-diag/ebin erts_rust/tools/suites/results/stdlib-diagnostics/<probe>.erl
cd $TMPDIR/stdlib-diag/cwd   # a throwaway cwd
<rt>  -noshell -pa $TMPDIR/stdlib-diag/ebin -eval '<probe>:main(), halt(0).'
erl   -noshell -pa $TMPDIR/stdlib-diag/ebin -eval '<probe>:main(), halt(0).'
```
