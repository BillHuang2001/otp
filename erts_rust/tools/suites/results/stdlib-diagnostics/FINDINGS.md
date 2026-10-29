# stdlib divergence classes D1…D11 — re-checked against the current Rust runtime

Re-verification of the ten root-cause classes recorded in `../stdlib-notes.md` §D, run
against the **current** Rust runtime at the head of this branch, plus the two rows this
document adds: `D1c` (the suite-path abort) and `D11` (`erlang:apply/2`, the
`queue_SUITE:oops` value disagreement). Measurement/diagnosis only: no runtime crate was
touched.

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
| **D11** | `f(As) -> apply(fun(X) -> X end, As).` called as `f([a])` — from a **compiled** module (`queue_oops_probe`) | `a` | `error:badarg` | **new row, still present** — `erlang:apply/2` is served by an `apply/3`-shaped floor; this is the `queue_SUITE:oops` value disagreement. See §*queue_SUITE:oops — the value disagreement* |
| **D11** | `apply(erlang, self)` (two atoms, compiled) | `error:badarg` | the **calling process's pid** (`apply(M, F)` runs `M:F()`) | **still present** — the *wrong-value* half of the same class |

Counts: **5 fixed** (D1b, D3, D4, D5, D7) · **6 still present** (D1, D1c, D2, D8, D9, D11) · **2 changed
class** (D6, D10). `D1c` is the row this document adds: the suite-level abort the sweep loses 13
cases to, which is *not* the child-level `function_clause` D1b used to record. `D11` is the row that
turns the sweep's `queue_SUITE:oops` `{diff,…}` throw into a named runtime defect (§*queue_SUITE:oops*
below); D9 is the same defect, and its mechanism note is corrected there.

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

**Mechanism corrected (D11):** the "incoming argument register" reading above is wrong. `erlang:apply/2`
is not *mis-read*, it is **not implemented at all** — every genuine `call_ext erlang:apply/2`
reaches `beam_rt`'s native floor, which decodes it as the `apply/3` `(Module, Function, Args)` shape
and calls `Module:Function()`. The rows that pass are rows the **compiler rewrote** into
`call_fun`/`call` (the compiler does that whenever the *length* of the argument list is known at
compile time — the fun itself need not be), so they never reach the floor; the rows that fail are
the ones left as a real
`erlang:apply/2`. It is therefore not "literal vs register" but "statically inlined vs actually
called", and the arity-mismatch degradation follows from the same place (a `badarg` from the floor,
never a `badarity` from the applied fun). Evidence and the smallest reproduction:
§*queue_SUITE:oops — the value disagreement* below.

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

### D11 — `erlang:apply/2`

Not implemented: every genuine `call_ext erlang:apply/2` reaches the `beam_rt` native floor, which
decodes it as `apply/3` and calls `Module:Function()`. The full evidence (the `queue_SUITE:oops`
op-by-op attribution, the smallest reproduction, the wrong-value shape `apply(erlang, self)`, the
owning crate and the fix seam) is §*queue_SUITE:oops — the value disagreement* below; the probe is
`queue_oops_probe.erl`.

## Extra observation (not one of D1…D11): `erts_debug:flat_size/1`

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
| `queue_oops_probe.erl` | D11 | `erlang:apply/2` from **compiled** code: the smallest reproduction, the `queue_SUITE:call/2` shape, the two-atom `apply(M,F)` → `M:F()` shape, the arity-mismatch reason, the external-fun shape, and the compiler-rewritten control |
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


## queue_SUITE:oops — the value disagreement

`queue_SUITE:oops` (`../stdlib/report.json`, verdict `divergence`) is the **only** row of the
committed stdlib sweep whose Rust detail is the suite's own model disagreement —
`{exception,throw,{diff,[{value,…},{…}|…]},{queue_SUITE,emul,4,…}}` — where the other 28
`divergence` rows are timeouts, doctest counts, `badmatch`es or typed `unsupported_bif`s. A
`{diff,…}` throw is `queue_SUITE:emul/4`'s "the two models disagree" signal, i.e. the suite
itself believes it has positive evidence of a wrong value, so it had to be separated into *a
wrong `queue` result* (serious) versus *the suite catching a runtime error* (still a runtime
defect, a different one).

Measured at branch head `173e121ec6` with `erts_rust/target/release/beam_rt` built from it
(`cargo.sh build -p beam_rt --release`); host `loadavg` `13.67 16.92 16.03` (shared — no
duration below is a claim); oracle OTP `29.1.2` / erts `17.1.1` against this runtime's
`system_info(version)` `"17.1"`. The staged suite beams are the shared
`/tmp/erts-suites-stage/stdlib` ones, compiled by the oracle's `erlc`, unchanged; the Rust
runtime needs no recompilation of the suite.

**Answer: no `queue` value is ever wrong, and the suite's emulation is fair. The models do
not diverge at all — the disagreement is the *first* op, and it is `erlang:apply/2`
raising `badarg` for a legitimate call (class D11).**

### 1. The first diverging op is op 1, with both models empty

`queue_SUITE:oops/1` seeds `rand:seed(exsplus, {1,2,4})`, draws 3142 op indices and folds
`emul/4` over them; each round prints `% Op % AsA % AsB :` before comparing. Same argv on
both sides (`-noshell -pa <stage>/ebin -pa <stage>/ctl -eval 'R = (catch queue_SUITE:oops([])), …'`):

| side | op lines (`^% `) | op 1 | RESULT |
|---|---|---|---|
| oracle | **3142** | `% peek % [{[],[]}] % [{[]}] :<empty> \|[]\| ` | `ok` |
| rust | **1** | `% peek % [{[],[]}] % [{[]}] :` | `{diff,[{value,empty},{error,badarg},peek,[{[],[]}],[{[]}]]}` |

Op 1 is `peek` on two **empty** queues: `XX = queue:peek({[],[]})` → `{value,empty}` and
`YY` (the suite's emulation fun) → `{error,badarg}`, so `emul/4`'s `{Exception,Exception}`
arm cannot match and it throws `{diff,…}`. There is no earlier op to bisect to — **the
first op is the first divergence** — and no state has been built yet, so the "the two
models' states (`QsA0`/`QsB0`) diverged on an earlier op" reading does not hold: `AsA=[{[],[]}]`
and `AsB=[{[]}]` are simply the two empty-queue representations. The `queue` module itself
agrees (`queue:peek/1` `empty`, `queue:len/1` `0`, `queue:is_queue/1` `true` on both sides).

### 2. Attribution: patch `apply/2` out of the suite and the whole case passes

An instrumented copy of `queue_SUITE` in `$TMPDIR` (module renamed so the stage beams stay
pristine, compiled with the oracle's `erlc`, everything else untouched) changes exactly one
line of `call/2` — `_ -> apply(Func, As)` becomes `_ -> dyn(Func, As)` with
`dyn/2` spelling out `F()`, `F(A)`, `F(A,B)`, `F(A,B,C)`:

* patched driver, **Rust**: **3142** op lines, `RESULT=ok` — every op agrees, `queue`,
  `rand:uniform_s/2`, `chk/5` and `queue:to_list/1` included.
* unpatched driver, **Rust**: 1 op line, `RESULT={diff,…}` (op 1, above).

So the whole of `oops/1` is correct on this runtime except `erlang:apply/2`. (A first
attempt patched the line to `apply(erlang, apply, [Func, As])`, which does **not** help: a
nested `apply of apply/3` is unwrapped by the engine's `resolve_call_ext_apply`
(`crates/beam_engine/src/interpreter/dispatch.rs`) straight back onto the host's `apply/2`.)

### 3. Root cause: `erlang:apply/2` is served by an `apply/3`-shaped floor

`apply(Fun, Args)` compiles (OTP's `erlc`) to `call_ext 2 erlang:apply,2`; the engine
deliberately declines that arity and leaves it to the host —
`crates/beam_engine/tests/call_ext_apply.rs:43-57` pins "must reach
`call_bif("erlang","apply",2,[Fun,Args])`, the host's own `apply/2`" — and that same test records
that the **engine** used to fabricate `Module:Function()` here and was fixed by handing the call to
the host. The host then fabricates exactly that, i.e. the fabrication moved one layer down:

* `crates/beam_rt/src/native.rs:414` — `("erlang", "apply", 2) => Some(self.apply_dynamic(args, false))`
* `crates/beam_rt/src/native.rs:781` — `apply_dynamic` reads `args[0]` as the **module** and
  `args[1]` as the **function**, and calls it with **zero** arguments.

Consequences, all measured from a **compiled** module (`queue_oops_probe`; the `-eval`
tree-walk evaluator has its own correct `apply_2` in `beam_rt/src/eval/mod.rs`, so an
`-eval`-only probe hides everything here):

| compiled expression | oracle | rust |
|---|---|---|
| `f(As) -> apply(fun(X) -> X end, As).` called `f([a])` | `a` | **`error:badarg`** |
| `call(Func, As)` = `try apply(Func, As) of …` (the suite's shape), `call(fun(X)->X end, [a])` | `{value,a}` | **`{error,badarg}`** |
| `apply(erlang, self)` | `error:badarg` | **`<0.35.4>` — it ran `erlang:self/0` and returned a pid** |
| `apply(lists, reverse)` | `error:badarg` | **`error:undef`** — it ran `lists:reverse/0` |
| `mismatch(F, As) -> apply(F, As)` with `fun() -> x end`, `[a]` | `error:{badarity,{Fun,[a]}}` | **`error:badarg`** |
| `apply(fun lists:reverse/1, As)` with `As=[1,2]` | `error:{badarity,{fun lists:reverse/1,[1,2]}}` | **`error:badarg`** |
| `known_length(F) -> apply(F, [a]).` — unknown fun, list of known **length** | `a` | `a` (the compiler rewrote it: `erlc -S` shows `{call_fun,1}`) |
| `apply(fun(X) -> X end, [a])` — literal fun **and** list | `a` | `a` (same rewrite, `{call_fun,1}`) |

The last two rows are the controls that explain why this survived: an `apply/2` whose argument
list has a statically known **length** is rewritten by the compiler into `call_fun` (the fun may
be unknown — `known_length/1` is the proof: `call_fun,1`, no `apply/2`), so only the genuinely
dynamic ones reach the runtime — which is why the same defect looks like "works with literals,
fails with parameters" and why it was recorded (wrongly) as a register/mis-read issue under D9. The floor's own unit test,
`crates/beam_rt/src/native.rs:1790` `apply_of_two_arguments_calls_a_zero_arity_bif` ("`erlang:apply(erlang,
self)` — a registered `erlang` BIF reached dynamically", asserting the result is a pid) asserts
exactly the behaviour the oracle rejects with `badarg` — so it pins the defect rather than
catching it.

### 4. Owning crate and fix seam

* **Owning crate: `beam_rt`** — the native floor (`crates/beam_rt/src/native.rs:414` and
  `apply_dynamic` at `:781`). It is reached *by design*: the engine
  (`crates/beam_engine/src/interpreter/dispatch.rs:803-825`) documents that `erlang:apply/2`
  is not a `fixed_apply` shape and must stay "the host's own `erlang:apply/2`", and
  `beam_bifs` is not involved either (`apply/2` is C's `i_apply_fun` instruction, not a SNIF;
  the registry only answers the `erlang:is_builtin` special case).
* **Correct shape:** a fun (or external fun) in `args[0]` applied to the proper list in
  `args[1]` via `ExecutionContext::apply_fun` (the C's `apply_fun(c_p, x(0), x(1), reg)`);
  anything else in `args[0]`, a non-list/improper `args[1]`, and any arity mismatch must raise
  the oracle's reason — `badarg` for the first two, `badarity` for the mismatch.
* **Do not just delete the arm:** the floor's own unit tests drive the two-atom reading —
  `crates/beam_rt/src/native.rs:1795`, `:1811`, `:1821` call `("erlang","apply")` with
  `[module, function]` (the first one is the pid test above); they must move to the explicit
  `apply/3` form (`[module, function, []]`) and the pid test must be replaced by a `badarg`
  assertion when the arm changes meaning.

### 5. What class is this, stated plainly

* **Not a wrong `queue` result**, and **not** the suite's emulation being unfair — §2 rules
  both out; `queue_SUITE:oops`'s `divergence` verdict in `../stdlib/report.json` is correct
  and must stay.
* **A runtime defect whose error surface is `badarg`**, i.e. the *most* misleading kind: a
  legal Erlang program gets a plain, catchable `error:badarg` that is indistinguishable from
  a genuine argument error, instead of either the right answer or the runtime's documented
  typed `unsupported` channel. If the differential corpus pinned a genuinely dynamic
  `apply/2`, both tools would classify it as a `mismatch` (a plain Erlang `badarg` is an
  observation, never one of the two `unsupported` signals) — see the coverage note below.
* **And it does produce a wrong value** in the sibling shape: `apply(M, F)` with two atoms
  *executes* `M:F/0` (`apply(erlang, self)` → a pid) where the oracle raises `badarg`. So the
  class is wrong-result, not merely unimplemented — it is just that `queue_SUITE` sits on the
  spurious-`badarg` side of it.
* Blast radius outside this row: `lib/stdlib/test` contains 63 textual `apply(` sites (mixing
  `apply/2` and `apply/3`), so several suites can reach the same defect; `queue_SUITE:oops`
  is simply the one whose very first op is a dynamic `apply/2`.

### 6. Why the differential gate did not catch it (out of this node's scope, reported)

`tools/differential` has exactly one case naming `apply/2` —
`bifs/apply_and_binary_list_conversions` (`corpus/bifs.rs:1514`), whose `erlang:apply(fun(X) ->
X+1 end, [1])` agrees. It cannot see this defect for **two independent** reasons: it is an
`Eval` case, so it runs in the `-eval` tree-walk evaluator (which has its own correct
`apply_2`), and even as compiled code its fun *and* list are literals, so the compiler rewrites
the call into `call_fun`. A gate case that pins `apply/2` needs **compiled** code (`CaseKind::
Module`/`ModuleEnv`) with both operands arriving as parameters — i.e. exactly
`queue_oops_probe:smallest/1`, plus the two-atom `apply(erlang, self)` shape for the
wrong-value half. Adding it belongs to `tools/differential`, not to this node.
