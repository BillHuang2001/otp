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
| `generated_at` | `1793256012` | `1793256335` |
| suites | the 8 of §B | `maps`, `lists`, `binary_module` |
| `--timeout` / `--suite-budget` / `--jobs` | `10` / `300000` / `4` | `120` / `900000` / `1` |
| `host.oracle` | `/nix/store/…-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` | same |
| `host.oracle_version` | `29/17.1.1` (tree pins OTP `30.0-rc0`, erts `17.1`) | same |
| `host.nproc` | 16 | 16 |
| `host.loadavg_before` | `27.19 39.03 30.12 2/882 4` | `11.72 26.84 27.05 17/881 4` |
| `host.loadavg_after` | `22.19 32.24 28.68 3/935 1100` | `11.23 16.25 22.54 100/1258 597` |
| `rust.path` | `erts_rust/target/release/beam_rt` (per-worktree, git-ignored) | same |
| `rust.sha` | `125ca3a9f3f7260dfc2400faff18117b67834558` | same |
| `rust.version` | `30/17.1` | same |
| `rust.tier` | `null` (default baseline-JIT tier) | same |

Both reports name the same workspace commit, and no file under `erts_rust/crates` differs between it
and the current tree (`git diff --stat 125ca3a9f3f7 HEAD -- erts_rust/crates` is empty), so both
sweeps describe the current runtime sources. The sha names a commit, not a binary hash: `beam_rt` is
per-worktree and git-ignored. In the recorded report `targets[0].build` is `erlc -o <stage>/ebin -I
<stage>/src <suite>.erl …` with the oracle's `erlc` (`ok: true`, `reused: true`, "compiled 8 suites + 6
helpers + runner"); `targets[0].code_path` is `<stage>/ebin` + `<stage>/ctl` on both sides.

Load was 27 → 22 during the recorded sweep and ~12–27 during the re-check, i.e. **both ran on a busy
host**, which is why the `TIMEOUT` rows are re-measured serially instead of read as capability
statements. Every suite runs in its own cwd (`<stage>/cwd/<SUITE>`) that the harness **clears before the
row starts** (`target::fresh_dir`), so a fixture tree left by an earlier sweep cannot poison either side;
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
| `binary_module_SUITE` | 20 | 18 / 2 | 4 / 3 | 4 | 1 | 3 | 12 |
| `dict_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 | 0 |
| `lists_SUITE` | 78 | 78 / 0 | 77 / 1 | 77 | 0 | 1 | 0 |
| `maps_SUITE` | 32 | 32 / 0 | 20 / 12 | 20 | 0 | 12 | 0 |
| `proplists_SUITE` | 4 | 4 / 0 | 4 / 0 | 4 | 0 | 0 | 0 |
| `queue_SUITE` | 7 | 7 / 0 | 6 / 1 | 6 | 0 | 1 | 0 |
| `string_SUITE` | 46 | 45 / 1 | 45 / 1 | 45 | 1 | 0 | 0 |
| `unicode_SUITE` | 20 | 18 / 2 | 6 / 14 | 6 | 2 | 12 | 0 |
| **total** | **214** | **209 / 5** | **169 / 32** | **169** | **4** | **29** | **12** |

`dict_SUITE` and `proplists_SUITE` agree on every case; `lists_SUITE` and `string_SUITE` differ on one
row each (a 10 s-budget timeout and an identically failing case). The oracle's five failures are
`binary_module_SUITE:copy` and `string_SUITE:meas` (both over the 10 s budget),
`unicode_SUITE:is_id`/`is_whitespace` (missing `UnicodeData.txt`) and `binary_module_SUITE:interesting`
(`{badmatch,false}` in `do_interesting/1` on both sides).

The re-check covers the three suites that own every rust-side `TIMEOUT`:

| suite | rows | sys P/F | rust P/F | agree | divergence | rust-gap |
|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 19 / 1 | 5 / 2 | 5 | 3 | 12 |
| `lists_SUITE` | 78 | 78 / 0 | 78 / 0 | 78 | 0 | 0 |
| `maps_SUITE` | 32 | 32 / 0 | 23 / 9 | 23 | 9 | 0 |

## C. Totals

- **214 rows** measured; no row is `not-run` and none is `oracle-only-fail`.
- **Oracle: 209 rows pass, 5 fail.** Rust: **169 pass, 32 fail**, plus **13 `error`** rows that carry no
  per-row measurement from the sweep (the 12 `rust-gap` rows and `binary_module_SUITE:interesting`).
- **Fully green rows: 209 of 214** on the oracle and **169 of 214** on rust; **4 rows are excluded from
  both numerators** (`agree-fail-identical`, §E). A `rust-gap` row has no rust result at all — not a pass.
- **Verdict split:** `agree 169` · `agree-fail-identical 4` · `divergence 29` · `rust-gap 12`.
- **No rust pass corresponds to an oracle failure.** All 169 `agree` rows have `sys.status = pass`; the
  only rows whose rust side lacks a pass are the `afi` ones, where **both** sides fail.
- The 12 `rust-gap` rows are one event: the rust side of `binary_module_SUITE` **dies mid-suite** (§D1), so
  every later case loses its turn. Measured one process per row they split 8 `agree`, 1
  `agree-fail-identical`, 1 typed gap, 1 D2 divergence and 2 too slow to finish (§D1's table).
- **Re-check (120 s, serial):** 130 rows over 3 suites — sys 129/1, rust 106/11 + 13 `error`; verdicts
  `agree 106` · `divergence 12` · `rust-gap 12`. It moves five of the seven one-sided `TIMEOUT` rows to
  `agree` (`bin_to_list`, `lists:suffix`, `maps:t_intersect`, `t_iterator_2`, `t_merge_with`), leaves two
  as `system_limit` (`maps:t_keys_trapping`, `t_values_trapping`) and turns `binary_module:copy` into a
  **divergence** (oracle passes at 120 s; rust still fails on D10).

## D. Non-agree rows, by class

41 rows are non-agree in the recorded sweep (29 `divergence` + 12 `rust-gap`); the 4 `afi` rows are
§E's exclusions. The per-class counts below add up to 43 rather than 41 because two rows are named
twice on purpose: `binary_module:copy` is §E's excluded row that D10 explains, and
`longest_common_trap` is both one of D1c's 13 missing cases and the D10 gap that case needs.
`<rt>` below means either binary. Class ids are the ones in
`results/stdlib-diagnostics/FINDINGS.md`, which re-verifies each one against the current runtime: **5 fixed**
(D1b, D3, D7, D9, D10), **5 still present** (D1, D2, D4 — `maps:put/3` identity on the compiled route — D8,
D11), **1 partially present** (D5, the legacy-`catch` head frame only), **2 changed class** (D1c — the suite
abort is gone, the raise reason is now the placeholder atom `'exit <structured reason>'` — and D6) and **2 new**
(D12, a `utf32` segment ignoring `little`/`native`, and D12b, `unicode:characters_to_binary/3`'s latin1
error split). `FINDINGS.md` is authoritative for every verdict below.

### D1 — the `system_limit` family — 16 rows, two mechanisms

**D1a, allocation inside a `lists` BIF callback — 3 rows.** `maps_SUITE:t_from_keys_check_trapping`,
`t_keys_trapping`, `t_values_trapping` all end `{badmatch,system_limit}` in
`maps_SUITE:count_nr_of_yields/2` (a 1 000 000-element list built inside a fun); in the recorded sweep the
two `t_*_trapping` rows reach that answer only in the 120 s re-check, after a 10 s-budget timeout.
The reduced class is *"a large allocation inside a fun invoked by a higher-order `lists` BIF"*:

```
<rt> -noshell -eval 'io:format("~p~n",[lists:foreach(fun(_) ->
      length(lists:duplicate(200000, x)) end, [1])]), halt(0).'
```

- oracle: `ok`; rust: catchable `error:system_limit` — uncaught it terminates the runtime with
  `beam_rt: system_limit`, exit 1.

The threshold sits between 131 072 and 200 000 list elements (2–3 MiB of cons cells); the same work in a
tail-recursive loop, in a directly-called fun, or in a `spawn_monitor` child is `ok`, so only the
*higher-order BIF callback* shape refuses. It is a plain `error`, not a typed capability gap.
**Owner:** `beam_rt` (heap admission / `enforce_term_heap_limit` from a BIF-invoked fun).

**D1c, the suite-path abort — 13 rows.** The recorded sweep reports 12 of them `error 0/0 case missing
from the rust runtime's output` and `interesting` as a `divergence`:

```
SUITE binary_module_SUITE
CASE … encode_decode       OK
… the first two error_info blocks, then the process exits 1
stderr: beam_rt: system_limit
```

The abort is deterministic for a given build and reproduces by hand with the harness's own argv. It is
**entry-count dependent**: `error_info_abort_probe:main({prefix, N})` (the suite's `error_info/1` list
through `ct_runner:run_case/4`'s shape) completes with exit 0 up to `N = 69` and aborts at `N = 70` of
the 73 entries (`N = 68`/`69` answer a ct exception result, not a passing case — the point is that the
runtime survives them).
The load-bearing ingredient is the ct hand-off — `spawn_monitor` child, `try`/`catch` around the case,
`exit({ct_case_result, Result})` out of the child: the case's own logic passes in the main process, in a
plain child, and when the value is merely returned, and dropping any single entry does not move the
threshold.

The mechanism is inside the runtime, not an escaping user error: the raise's reason crosses the runtime's
escape boundary and the **error-live-reason stabilization** copies a term it should not, and the arm maps
the failure to `error:system_limit`, so no `catch` in the program can see it —
`runtime/gc.rs::stabilize_error_live_reason` → `Runtime::stabilize_term` (src/runtime.rs:2650) →
`beam_message::copy::try_copy_fast`/`copy_struct_simple` → `copy_match_context`
(crates/beam_message/src/copy.rs:365) → `beam_term::bitstring::bit_at`
(crates/beam_term/src/bitstring.rs:339). `copy_match_context` takes its bit window from the term's
`ErlSubBits` words and dereferences the base pointer stored in the term with no owning handle to validate
it, because the copy runs on the empty-source-table path (`Runtime::stabilize_term`, not
`stabilize_term_with`). Garbage there yields an impossible window (→ the reservation is refused →
`system_limit`) or an unmapped page (→ SIGSEGV at the same instruction); the same program has produced
both manifests. **Which payload inside the reason is dead is unknown from the outside**; what is
established is that the reader is unsound on that path. **Owners:** `beam_rt` (the stabilization path),
`beam_message` (`copy_match_context`), `beam_term` (`bit_at`). This is the only class that is **not** a
typed gap: it breaks the "every gap is a typed error, never a wrong result" contract.

The 13 cases, each measured alone in a fresh process (`error_info_abort_probe:main({one, Case})`,
`FINDINGS.md` §D1):

| case | rust (alone) | oracle | would record as |
|---|---|---|---|
| `error_info` | `{ok,ok}` | `{ok,ok}` | agree |
| `guard` | `{ok,{comment,"…"}}` | same | agree |
| `hex_encoding` | `{ok,ok}` | `{ok,ok}` | agree |
| `interesting` | `{case_exception,error,{badmatch,false},…}` | identical | agree-fail-identical |
| `join` | `{ok,<<>>}` | same | agree |
| `list_to_bin` | `{ok,ok}` | `{ok,ok}` | agree |
| `longest_common_trap` | `{case_exception,error,unsupported_bif,{erts_debug,set_internal_state,[binary_loop_limit,10],[]}}` | `{ok,ok}` | rust-gap (typed, D10) |
| `parts` | `{ok,ok}` | `{ok,ok}` | agree |
| `random_ref_comp` | timeout (60 s; log reached 5 287 successes) | `{ok,ok}` | rust-gap (too slow) |
| `random_ref_fla_comp` | `{ok,ok}` | `{ok,ok}` | agree |
| `random_ref_sr_comp` | timeout (60 s; 7 500 successes) | `{ok,ok}` | rust-gap (too slow) |
| `referenced` | `{case_exception,error,{badmatch,1023},…}` | `{ok,ok}` | divergence (D2) |
| `scope_return` | `{ok,ok}` | `{ok,ok}` | agree |

So "case missing from the rust runtime's output" is a property of **running the suite in one process**,
not of the cases: the abort takes the process down at `error_info` and every later case loses its turn.
`random_ref_comp`/`random_ref_sr_comp` are over the sweep's own 10 s per-case budget as well, so they are
unmeasured there too; `random_ref_fla_comp` finishing in the same run shows the two timeouts are
workload cost, not a hang.

### D2 — `binary:referenced_byte_size/1` of a sub-binary answers its own size — 6 rows

Rows: `binary_module_SUITE:doctests` (`{1,errors}` — the failing doctest is this expression, `A test
failed in function referenced_byte_size/1: 6> {byte_size(C), binary:referenced_byte_size(C)}.`) and
`unicode_SUITE:ex_binaries_errors_utf8`, `_utf16_big`, `_utf16_little`, `_utf32_big`, `_utf32_little`
(`{badmatch,false}` inside `utf*_inner_loop/6` / the utf8 list comprehension — all five assert
`true = (binary:referenced_byte_size(Chomped) =:= PBSz)`).

```
<rt> -noshell -eval 'Part = <<"z",(binary:copy(<<255>>,1024))/binary>>,
                      C = binary:part(Part,1,510),
                      io:format("~p~n",[{byte_size(C), binary:referenced_byte_size(C)}]), halt(0).'
```

- rust: `{510,510}` · oracle: `{510,2050}`

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

### D6 — `erlang:bump_reductions/1` is not served — 1 row

Row: `maps_SUITE:doctests` (`{1,errors}`). The captured doctest transcript
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
per runtime, run twice with the order swapped, host loadavg ~32 — only the ratio is meaningful):

| workload | rounds × size | ratio, run 1 | ratio, run 2 |
|---|---|---|---|
| `maps:merge_with/3` | 50 × 1000 keys | **580×** | **764×** |
| `maps:intersect_with/3` | 50 × 1000 keys | **94×** | **107×** |
| `maps:iterator/1` + `maps:next/1` | 50 × 1000 entries | **442×** | **525×** |
| `maps:to_list/1` (no Erlang callback) | 50 × 1000 entries | 4.8× | 4.3× |

Both sides compute the same work (the probe's checksum is `0` on both sides for every row). The last row
is the control: a pure BIF over the same map costs ~4–5×, so essentially the whole residual is the
per-callback-call cost of a BIF that keeps invoking an Erlang fun. **Owner:** `beam_bifs` (the `maps` BIFs
that iterate by calling an Erlang fun never bump reductions) plus `beam_engine`'s reduction
accounting/preemption. A third timeout row, `maps_SUITE:t_intersect`, is not of this shape — it is the
suite's heavy `check_map_combiners_same_small/large` workload (a fold over `gb_sets` intersections),
which also agrees in the 120 s re-check.

### D9 — `erlang:apply/2` is not implemented — 1 row

Row: `queue_SUITE:oops`. The case drives 3 142 random `queue` operations and compares `queue:*` with the
suite's own emulation model; it throws `{diff,[{value,empty},{error,badarg},peek,[{[],[]}],[{[]}]]}` —
the `queue` side answers `empty`, the **model** side answers `{error,badarg}`. The divergence is at op 1
(the two queue states never disagree); the model side is the suite's own
`call(Func, As) -> … apply(Func, As)` helper, so the failing call is `erlang:apply/2`.
Patching that one `apply/2` in the suite's helper to a direct fun dispatch makes the Rust side run all
3 142 ops to `ok`, which attributes the row to `apply/2` and to nothing in `queue`.

`erlang:apply/2` is unimplemented: a genuine `call_ext erlang:apply/2` reaches `beam_rt`'s native floor,
which decodes it as the `apply/3` `(Module, Function, Args)` shape
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
Wrong on the value side too, not just the error side: `apply(erlang, self)` answers a pid where the
oracle raises `badarg` (it ran `erlang:self/0`), `apply(lists, reverse)` answers `undef`, and an arity
mismatch degrades `{badarity,{Fun,Args}}` to plain `badarg`.
**Owner:** `beam_rt` (the native `apply/2` floor; three of that floor's own unit tests pin the wrong
semantics and must move with the fix). The engine and `beam_bifs` are correct by contract —
`crates/beam_engine/src/families/funs/call_ext_apply.rs` pins that arity 2 must reach the host's own
`apply/2`.

### D10 — `erts_debug:set_internal_state/2` is not served — 9 rows

Rows: `unicode_SUITE:binaries_errors_limit`, `exceptions`, `latin1`, `random_lists`, `roundtrips`,
`utf16_illegal_sequences_bif`, `utf8_illegal_sequences_bif` (all through `setlimit/1`) plus
`binary_module_SUITE:copy` and `longest_common_trap`.

The gate call has to be reproduced carefully, because the oracle needs access enabled first:

```
erts_debug:set_internal_state(available_internal_state,true)  → false   (previous value; both sides agree)
erts_debug:set_internal_state(unicode_loop_limit,10)          → 40000   (previous limit, oracle)
                                                              → error:unsupported_bif (rust, compiled)
```

Through `-eval` the same second call leaves as the CLI-level `beam_rt: unsupported: bif` and exit 3,
which is what `FINDINGS.md`'s one-liner observes; the suite records the catchable form, because
`setlimit/1` is compiled code. **Owner:** `beam_bifs`/`beam_rt`. `binary_module_SUITE:copy` is this class
and not a capability statement about copying: at the 10 s budget the oracle times out and the row records
`agree-fail-identical`, while at 120 s the oracle passes and rust still stops on this gap (§E).

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
delivery bookkeeping for a pid that has already exited. **Owner:** `beam_bifs`.

### Classes that own no row in this sweep

`erlang:trace/3` is served: `erlang:trace(self(),true,[running,{tracer,self()}])` answers `1` on both
sides, and `trace_info(self(),flags)` reads back `{flags,[running,call]}` (D3). `erts_debug:same/2` is
`true` for both the no-op `maps:merge(#{},M)` and `maps:put(a,1,M)` (D4). `erlang:system_info(snifs)`
publishes 556 `{M,F,A}` triples against the oracle's 553 — each runtime's own static-NIF table, so the
counts are not expected to match (D7).

## E. Exclusions (`agree-fail-identical`, 4 rows — in neither numerator)

| row | why it is excluded |
|---|---|
| `binary_module_SUITE:copy` | **A budget artefact that flips into a divergence.** At the 10 s budget the oracle TIMEOUTs while rust fails `{exception,error,unsupported_bif,{erts_debug,set_internal_state,…}}` (D10), so neither side gives a comparable result and the row is excluded. At 120 s the oracle **passes** and rust still fails on D10 — a divergence at a larger budget, never agreement. |
| `string_SUITE:meas` | **both** sides TIMEOUT at the 10 s budget (`detail: timeout` on each side). A budget mismatch, and no evidence of a rust gap. |
| `unicode_SUITE:is_id` | both fail identically: `{badmatch,{error,enoent}}` in `parse_properties/1` — the suite wants `UnicodeData.txt`, which lives in `lib/stdlib/uc_spec`, not in the `unicode_SUITE_data` dir the runner resolves, and the suite exports no `init_per_suite` to fetch it. |
| `unicode_SUITE:is_whitespace` | same as `is_id`. |

## F. Caveats

- **`--jobs 4` on a loadavg-27 host makes `TIMEOUT` unreliable.** Seven rows time out on the rust side
  only at 10 s; the serial 120 s re-check turns five into `agree` (`binary_module:bin_to_list`,
  `lists:suffix`, `maps:t_intersect`, `t_iterator_2`, `t_merge_with`), leaves two as `system_limit`
  (`maps:t_keys_trapping`, `t_values_trapping`), and turns `copy` into a divergence. A `TIMEOUT` is a
  budget statement, never a pass.
- **The `binary_module_SUITE` numbers are a lower bound.** The rust run dies after 8 of its 20 cases, so
  the 12 `rust-gap` rows carry no sweep verdict either way; their measured values are §D1's per-case
  table. They are never `pass`.
- **Counts, not seconds.** Every ratio in §D8 and every figure in §D1 came from the same shared host, so
  the seconds only separate *slow* from *stuck*; do not compare them with another machine.
- **The oracle is OTP 29.1.2 / erts 17.1.1**, while this tree pins OTP `30.0-rc0` / erts `17.1`; a suite
  that pins erts-versioned data can differ for reasons unrelated to the runtime.
- **Attribution is a hypothesis, not a fix.** No crate outside `erts_rust/tools/suites` was touched; each
  "Owner" above names the crate the analysis points at.
- **The artifact set is `results/stdlib/report.json` + `results/stdlib/summary.md` and the re-check
  `results/stdlib/recheck-120/`** (`report.json` is authoritative; `summary.md` is rendered from it).
  `results/stdlib/raw/` is *not* part of it: hand-captured probe evidence (the tail of a dying rust
  `binary_module_SUITE` run, per-suite stdout, earlier 5 s probes), not `suites run` output.

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
