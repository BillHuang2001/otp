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
| **D1b** | `spawn_monitor(fun() -> binary_module_SUITE:error_info([]) end)` (`error_info_child_probe`) | `DOWN=normal`, 1370 lines, `main result={ok,ok}` | `DOWN=normal`, 1095 lines, `main result={ok,ok}`; runtime exits 0 | **fixed** (re-measured: the child-level `{function_clause,[]}` this row used to record is gone; only the line count differs) |
| **D1c** | `error_info_abort_probe:main({prefix, 70})` — the first **70** of `error_info/1`'s 73 entries through `ct_runner:run_case/4`'s shape | `{ok,ok}`, exit 0 | `beam_rt: system_limit` on stderr, **exit 1** (69 entries: `{ok,ok}`, exit 0) | **still present** — this is the suite-path abort; see §D1 below |
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

Counts: **5 fixed** (D1b, D3, D4, D5, D7) · **5 still present** (D1, D1c, D2, D8, D9) · **2 changed
class** (D6, D10). `D1c` is the row this document adds: the suite-level abort the sweep loses 13
cases to, which is *not* the child-level `function_clause` D1b used to record.

## Notes per class

### D1 / D1b / D1c — the `system_limit` abort

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

The suite-level path moved in the opposite (better) direction: `error_info_child_probe.erl` returns
`{ok,ok}` in the main process and lets the runtime exit **0**, and at the head measured below the
spawned child *also* finishes (`DOWN=normal`, 1095 printed lines against the oracle's 1370). The
`{function_clause,[]}` after 622 lines this section used to record no longer reproduces, so the
child-level defect it described is gone; **the suite-level abort is not** — see §*D1 — the
suite-path abort* below, which is the abort the sweep still loses 13 cases to.

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
| `error_info_abort_probe.erl` | D1c | `error_info/1`'s entry list through `ct_runner:run_case/4`'s shape: `{prefix, N}` (the 69→70 abort threshold) and `{one, Case}` (one lost case, alone in a fresh process) |
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

## D1 — the suite-path abort: smallest reproduction

Measured at branch head `f869e63416d9e8c77c918ee1b3e5728bba916b71` with
`erts_rust/target/release/beam_rt` built from it (`cargo.sh build -p beam_rt --release`);
host `loadavg` `16.21 12.66 14.93` (shared: no absolute time below is a claim).
The staged suite beams are the shared `/tmp/erts-suites-stage/stdlib` ones, unchanged.

**Probe: `error_info_abort_probe.erl`** (new, in this directory). It drives the suite's own
code through the **same shape as `ct_runner:run_case/4`** — a `spawn_monitor`'d child, a
`try`/`catch` around the case, and `exit({ct_case_result, Result})` — with two modes:
`{prefix, N}` (the first `N` of `error_info/1`'s **73**-entry literal list through
`error_info_lib:test_error_info(binary, Prefix)`) and `{one, Case}` (one named case, alone in a
fresh OS process).

### 1. It is still the same abort, and it is stable

The harness's own invocation, run by hand (throwaway cwd):

```
beam_rt -noshell -pa <stage>/ebin -pa <stage>/ctl \
        -eval 'ct_runner:run('\''binary_module_SUITE'\'', 10000, 300000, "<stage>/src"), halt(0).'
```

→ **exit 1**, stderr exactly `beam_rt: system_limit`, stdout stops after `CASE encode_decode OK`
plus the first two `error_info` blocks (44 lines; the ~1100 lines the case really prints are lost
with the port buffer when the runtime exits). Reproduced 2/2 runs at this head, and 6/6 with the
same shape narrowed to `error_info` alone, so the abort is deterministic **for a given build**.

### 2. Smallest reproduction: an entry-count threshold inside `error_info`

`error_info_abort_probe:main({prefix, N})`, identical argv on both sides:

| N (entries of `error_info`'s list) | oracle | rust |
|---|---|---|
| 68 | `{exception,exit,{test_case_failed,…}}`, exit 0 | same, exit 0 |
| 69 | same, exit 0 | same, exit 0 |
| **70** | `{ok,ok}`, exit 0 | **`beam_rt: system_limit`, exit 1** |
| 73 (= `all`) | `{ok,ok}`, exit 0 | **`beam_rt: system_limit`, exit 1** |

(`error_info_lib` fills every BIF the given list does not cover with `nyi` entries, which is why a
*short* prefix still raises `{test_case_failed,…}` on both sides — that is the probe's control
row.) The suite path is the N = 73 row. The threshold is **not** the reason term's size: a child
that exits with `{x, lists:seq(1, 1000000)}` — three orders of magnitude more words — copies fine
and the parent receives it. Nor is it a single offending entry: dropping any one entry (including
entry #70, `{matches,[<<1:1>>,{bm,make_ref()}],…}`) leaves the abort in place.

### 3. Which ingredient is load-bearing: the ct shape, not the spawn

Same `binary_module_SUITE:error_info` call, one shape per OS process:

| shape | rust |
|---|---|
| main process, plain | `ok`, exit 0 |
| main process, inside `try/catch` | `ok`, exit 0 |
| `spawn_monitor` child, plain (`error_info_child_probe`, D1b) | `DOWN=normal`, exit 0 |
| child, value returned from the fun (no `exit/1`) | `DOWN=normal`, exit 0 |
| child, `exit({ct_case_result, error_info(… )})` — no `catch` | `{ct_case_result,ok}`, exit 0 |
| child, `try/catch` + `exit({ct_case_result, R})` where `R` is the **caught** result, `try` in a **called function** (ct_runner's exact shape) | **abort** — `beam_rt: system_limit` (exit 1) or SIGSEGV (exit 139) |
| child, the same with the `try/catch` **inline** in the fun instead | passed 3/3 at this build |

So the load-bearing ingredients are `spawn` **together with** the `catch`-and-`exit` hand-off: the
child must *raise out* of itself with the structured result, i.e. the raise's reason has to cross
the runtime's escape boundary. `error_info` alone in that same shape and in a fresh process
**passes** (`{ok,ok}` on both sides — see the per-case table in §5), so the case's own logic is not
the defect. The difference between the last two rows is one function boundary, which is itself the
signal: the outcome flips with heap layout, and the two failure *manifests* (a refusal and a
SIGSEGV) come from the same instruction.

### 4. Mechanism: (b) — a failed escape-store copy of the raise's reason, not an escaping

`system_limit`. No `catch` in the program can see it, because the runtime produces it *while
handling* the raise:

- `BEAM_RT_BOOT_TRACE=1` shows the child's raise last — class `Exit`, `reason: Structured`,
  `live_reason: Some("<live reason>")` at `d1_variants_probe:normalize/1` (the `exit/1` that
  `ct_runner`'s `normalize/1` performs) — immediately followed by
  `round: … result=Err(SystemLimit)` and `call_bif … -> SystemLimit`, then
  `beam_rt: system_limit`.
- The stack of that round, from `gdb` on the SIGSEGV manifest of the same program:

  ```
  #0 beam_term::bitstring::bit_at            (crates/beam_term/src/bitstring.rs:339)
  #1 beam_message::copy::copy_match_context  (crates/beam_message/src/copy.rs:365)
  #2 beam_message::copy::try_copy_fast       (copy.rs:537)   ← copy_struct / copy_struct_simple
  #5 beam_rt::runtime::Runtime::stabilize_term            (src/runtime.rs:2650)
  #7 beam_rt::runtime::gc::stabilize_error_live_reason    (src/runtime/gc.rs:1034)
  #8 beam_rt::runner::run                                 (src/runner/run.rs:1718)
  ```

  i.e. the arm whose comment reads "a reason that genuinely cannot be copied is the honest
  `error:system_limit`". `crate::error::enforce_term_heap_limit` is the mapping pointed at by
  `Runtime::stabilize_term`, and it turns **only** `AllocKind::Heap` (or message-heap exhaustion)
  into `SystemLimit`; `beam_rt::cli`'s `EXIT_RUNTIME` (= 1) is what the shell then sees.
- `copy_match_context` takes its bit count from `ErlSubBits`'s `start`/`end` words and dereferences
  the **base pointer stored in the term** (`bitstring_parts`/`bit_at`) with no owning handle to
  validate it — because the copy ran on the **empty**-source-table path (`stabilize_term`, not
  `stabilize_term_with`: `src.is_empty()` in `Runtime::stabilize_term_with` short-circuits to it).
  Garbage there yields an impossible window size (→ the destination reservation is refused → the
  `system_limit` above) or an unmapped page (→ the SIGSEGV at the same instruction). Which payload
  inside the reason is dead is **unknown** from the outside; what is established is that the reader
  is unsound on that path, not that the term is genuinely too large.

### 5. The 13 cases the sweep reports as `error` — each measured alone

`error_info_abort_probe:main({one, Case})`, one process per row, both sides:

| case | rust | oracle | would record as |
|---|---|---|---|
| `error_info` | `{ok,ok}` | `{ok,ok}` | **agree** |
| `guard` | `{ok,{comment,"…"}}` | same | agree |
| `hex_encoding` | `{ok,ok}` | `{ok,ok}` | agree |
| `interesting` | `{case_exception,error,{badmatch,false},…}` | identical | agree-fail-identical |
| `join` | `{ok,<<>>}` | same | agree |
| `list_to_bin` | `{ok,ok}` | `{ok,ok}` | agree |
| `longest_common_trap` | `{case_exception,error,unsupported_bif,{erts_debug,set_internal_state,[binary_loop_limit,10],[]}}` | `{ok,ok}` | rust-gap (typed) |
| `parts` | `{ok,ok}` | `{ok,ok}` | agree |
| `random_ref_comp` | **timeout** (60 s budget; log reached `Number of successes: 5287`) | `{ok,ok}` | rust-gap (too slow) |
| `random_ref_fla_comp` | `{ok,ok}` | `{ok,ok}` | agree |
| `random_ref_sr_comp` | **timeout** (60 s; `Number of successes: 7500`) | `{ok,ok}` | rust-gap (too slow) |
| `referenced` | `{case_exception,error,{badmatch,1023},…}` | `{ok,ok}` | **divergence** (D2, `binary:referenced_byte_size/1`) |
| `scope_return` | `{ok,ok}` | `{ok,ok}` | agree |

Nine of the thirteen are measurable today and eight of those agree (the ninth, `interesting`, fails
identically on both sides); the losses that remain are one typed gap, one D2 divergence and two
cases too slow to finish (the sweep's own per-case budget is 10 s, so those two were never
measurable *in the sweep* either — `random_ref_fla_comp` finishing in the same run shows the
timeouts are workload cost, not a hang). So the report's "case missing from the rust runtime's
output" is a property of **running the suite in one process**, not of the cases: the abort takes
the process down at `error_info` and every later case loses its turn.

