# stdlib divergence classes D1…D12 — current verdicts

The per-class record behind `../stdlib-notes.md` §D and `../stdlib/summary.md`.
Every row below was re-measured with its smallest expression against the runtime under test; nothing here is
carried over from an earlier reading, and a claim this file could not reproduce is gone rather than annotated.
Measurement/diagnosis only: no runtime crate was touched from this node.

## Environment and method

| field | value |
|---|---|
| runtime under test | `erts_rust/target/release/beam_rt`, built from this worktree (`cargo.sh build -p beam_rt --release`) |
| runtime commit | `71ffbbb37971d0b193e0dc74b60b6c448874349e` (the runtime sources **are** this sha; the sweep below also ran it) |
| runtime version | `erlang:system_info(version)` → `"17.1"`; `system_info(otp_release)` → `"30"` |
| oracle | `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` (OTP `29.1.2` / erts `17.1.1`) |
| sweep artifacts | `../stdlib/report.json` + `../stdlib/summary.md` (8 suites, `--timeout 10 --suite-budget 300 --jobs 4`) and `../stdlib/recheck-120/` (`maps_SUITE,lists_SUITE,binary_module_SUITE`, `--timeout 120 --jobs 1`) |
| cwd | throwaway dirs under `$TMPDIR`; the staged suite beams are the shared `/tmp/erts-suites-stage/stdlib` (`src/`,`ebin/`,`ctl/`,`cwd/`), read-only |
| host | **shared**; `loadavg` observed `61.39 29.93 17.96` → `58.32 50.07 29.76` for the sweep, `4.16 7.83 20.90` … `12.82 25.26 36.96` during the re-measurement below (16 logical CPUs) |

Every check runs the **same** argv under both runtimes (`-noshell -pa <ebin>… -eval '…, halt(0).'`).
Anything that needs compiled code is compiled with the **oracle's `erlc`** into `$TMPDIR`, so both sides run
byte-identical `.beam` files; the `-eval` route is the tree-walk evaluator, the compiled route is the engine.
Two rules are load-bearing:

- **A raise inside a probe must be caught by the probe.** An uncaught typed gap leaves through the CLI's
  capability-gap channel (`beam_rt: unsupported: …`, exit `3`) or terminates the runtime, so pairs are
  observed as `try … catch`/`catch` results.
- **Ratios only.** No absolute time below is portable; D8 and the timeout table quote same-host ratios and
  the operation counts that produced them.

Reproduce any one-liner with `matrix.sh` (expressions on stdin, one line per expression with both sides):

```
ROOT=$PWD bash erts_rust/tools/suites/results/stdlib-diagnostics/matrix.sh <<'EOF'
apply(fun(X) -> X end, [a])
EOF
```

## Verdicts

| # | smallest expression | oracle | rust | verdict |
|---|---|---|---|---|
| **D1** | `lists:foreach(fun(_) -> length(lists:duplicate(N, x)) end, [1])` | `ok` for every `N` | catchable `error:system_limit` for `N ≥ 65537`; uncaught → `beam_rt: system_limit`, exit `1` | **still present**, threshold now `65536` elements |
| **D1b** | `error_info_child_probe:main()` (`binary_module_SUITE:error_info([])` in the main process **and** in a `spawn_monitor` child) | `DONE`, exit `0`, 1370 lines | `DONE`, exit `0`, 1095 lines (same `{ok,ok}` main result) | **fixed** (only the printed line count differs) |
| **D1c** | `error_info_abort_probe:main({prefix, 70})` | `{ok,ok}`, exit `0` | no abort; the parent receives the atom `'exit <structured reason>'`, exit `0` | **changed class** (the suite-level abort is gone; the raise reason is replaced by a placeholder atom) |
| **D2** | `C = binary:part(<<"z",(binary:copy(<<255>>,1024))/binary>>,1,510), {byte_size(C), binary:referenced_byte_size(C)}` | `{510,1025}` | `{510,510}` | **still present** for every sub-binary of a **refc** parent |
| **D3** | `T = erlang:trace(self(),true,[running,{tracer,self()}]), U = erlang:trace(self(),true,[call]), {T,U,erlang:trace_info(self(),flags)}` | `{1,1,{flags,[running,call]}}` | `{1,1,{flags,[running,call]}}` | **fixed** |
| **D4** | `M = #{a=>1,b=>2}, erts_debug:same(maps:put(a,1,M), M)` — **compiled** | `true` | `false` | **still present** (compiled route only; `M#{a=>1}` and `maps:merge(#{},M)` keep identity, `maps:put/3` does not) |
| **D5** | `catch maps:with([a,b], (fun(X) -> X end)(a))` — **compiled**, legacy `catch` | head `{maps,with,[[a,b],a],…}` | head `{d5_probe,legacy_catch,0,…}` (the BIF frame is missing) | **partially present** (`try`/`catch` route agrees; legacy `catch` loses the head frame) |
| **D6** | `catch erlang:bump_reductions(134217727)` — **compiled** | `true` | `{'EXIT',{unsupported_bif,[…]}}` | **changed class** (catchable again, `error:unsupported_bif`; the CLI `unsupported` exit `3` is only the *uncaught* `-eval` shape) |
| **D7** | `erlang:system_info(snifs)` | 553 `{M,F,A}` | 556 `{M,F,A}` | **fixed** (each list is its own static-NIF table; the count is not expected to match) |
| **D8** | `maps_bif_loop_probe:main()` — 50 rounds × 1000-key maps | reference | `merge_with` **662×/614×**, `intersect_with` **92×/95×**, `iterator+next` **438×/480×**, `to_list` control 3.7× | **still present** |
| **D9** | `apply_args_probe:main()` — the `apply/2` argument matrix | see §D9 | **agrees on every value**; only the arity-mismatch *reason* degrades (`{badarity,Args}` → `badarity`) | **fixed** (residual: one reason shape) |
| **D10** | `erts_debug:set_internal_state(available_internal_state,true)`, then `…(unicode_loop_limit,10)` | `false`, then `40000` | `false`, then `40000` (same for `binary_loop_limit` → `0`, `re_loop_limit` → `64000`) | **fixed** |
| **D11** | `R = erlang:trace_delivered(TraceePid)` after the traced child's `{'DOWN',…}` | a `reference()` | `{'EXIT',{badarg,…}}` | **still present**; the **oracle** is right (`erlang:trace_delivered/1`: `badarg` only for a *dead* pid, a terminated-but-delivered pid is legal) |
| **D12** | `f(E) -> <<E/utf32-little>>.` called as `f(1)` — **compiled** | `<<1,0,0,0>>` | `<<0,0,0,1>>` | **new class, still present** (a `utf32` segment ignores `little`/`native` for a value that is not compile-time-known) |
| **D12b** | `unicode_SUITE:latin1`'s `[16#FFF] = lists:flatten(Rest2)` (staged `unicode_SUITE.erl:414`) | `ok` | `{badmatch,<<0,0,…>>}` | **new, separate residual** — the `{error,Bin,Rest}` split of `unicode:characters_to_binary([…,16#FFF],latin1)`; not the utf32 class |

Counts: **5 fixed** (D1b, D3, D7, D9, D10) · **5 still present** (D1, D2, D4, D8, D11) · **1 partially present**
(D5) · **2 changed class** (D1c, D6) · **2 new** (D12, D12b).
`D1c`, `D9`, `D11` and `D12b` are this document's own row names; `../stdlib-notes.md` §D keeps the same
numbering.

## Notes per class

### D1 — the term-heap `system_limit`, threshold `65536` list elements

Measured with the raise caught by the probe, so the class is visible without a runtime exit:

| shape (all with `N = 200000` unless stated) | oracle | rust |
|---|---|---|
| `length(lists:duplicate(N, x))` direct, in the main process | `ok` | `ok` |
| the same 5× in a tail-recursive loop | `ok` | `ok` |
| the same inside a `fun() -> … end` called directly | `ok` | `ok` |
| the same inside a `spawn_monitor` child | `ok` | `ok` |
| the same inside a `lists:foreach/2` fun | `ok` | **`error:system_limit`** |
| the same inside a `lists:map/2` fun | `ok` | **`error:system_limit`** |
| `lists:foreach` fun, `N = 65536` | `ok` | `ok` |
| `lists:foreach` fun, `N = 65537` | `ok` | **`error:system_limit`** |

So the class is *"a large allocation inside a fun invoked by a higher-order `lists` BIF"* and the threshold is
exactly **65536** list elements (131072 words = 1 MiB of cons cells) — half of what the previous record stated
for the compiled route. The `-eval` route is looser (ok at 65536, `beam_rt: unsupported: bif` exit `3` from
`70000`) and is **not** catchable, i.e. the evaluator turns the same refusal into the CLI capability-gap
channel. Uncaught on the compiled route it terminates the runtime with `beam_rt: system_limit`, exit `1`.
It is a plain `error`, never a wrong value.
**Owner:** `beam_rt` heap admission (`enforce_term_heap_limit` / the per-depth BIF-region reservation).

### D1b — the spawned child is fine

`error_info_child_probe` runs `binary_module_SUITE:error_info([])` in the main process and, separately, in a
`spawn_monitor` child whose `{'DOWN',…}` reason is inspected. Both sides: `DONE`, exit `0`, main result
`{ok,ok}`, child `DOWN=normal`. The only difference is the number of printed lines (rust 1095, oracle 1370) —
these cases print more than one assertion per line, so the count is not an assertion count and no assertion
was observed to fail. The child-level `{function_clause,[]}` this row used to name does not reproduce.

### D1c — the suite-level abort is gone; the raise reason is a placeholder

`error_info_abort_probe:main({prefix, N})` drives `error_info/1`'s 73-entry list through the
`ct_runner:run_case/4` shape (`spawn_monitor` + `try`/`catch` + `exit({ct_case_result, R})`):

| N | oracle | rust |
|---|---|---|
| 68 | `{'EXIT',{error_info_abort_probe,…}}`, exit `0` | identical, exit `0` |
| 69 | identical | identical |
| **70** | `{ok,ok}`, exit `0` | `MAIN_GOT 'exit <structured reason>'`, exit `0` |
| **73** (`all`) | `{ok,ok}`, exit `0` | `MAIN_GOT 'exit <structured reason>'`, exit `0` |
| `{one,error_info}` | `RESULT error_info {ct_case_result,{ok,ok}}` | `RESULT error_info {ct_case_result,{ok,ok}}` |

The runtime no longer dies here (the whole `binary_module_SUITE` is measured now — 20/20 rows), and the case's
own logic passes standalone. What remains is the **death-boundary reason**: the parent receives the atom
`'exit <structured reason>'` instead of the child's `{ct_case_result,{ok,ok}}`.
That atom is the `Display` of a structured raise reason — `crates/beam_common/src/error.rs:321`
(`ExitReason::Structured`) — interned at `crates/beam_rt/src/runner/reason.rs` when
`live_reason_term()` answers `None`; the payload was dropped earlier by
`Runtime::stabilize_error_live_reason` → `degrade_live_reason` (`crates/beam_rt/src/runtime/gc.rs`), whose
price/copy gate refuses this reason. Note the ladder of `exit/1` shapes (`d1c_probe:rows()`): all ten —
bare atom, tuple, `{ct_case_result,…}`, `{exception,error,{badmatch,false},{m,f,1,[]}}`, a frame with
`file`/`line`, a binary, a map, a 1M-element list, a 5000-byte binary, a nested frame list — cross the
boundary **identically** on both sides, so the trigger is a specific term shape inside `error_info`'s reason,
not the ct hand-off.
**Verdict:** the abort is fixed; the residual is that the opaque placeholder atom is *observable* (an Erlang
program can match it), i.e. a wrong value rather than a typed gap.
**Owner:** `beam_rt` — the escape-boundary stabilization (`runtime/gc.rs`) and the
`intern(reason.to_string())` fallback in `runner/reason.rs`, which should degrade to the bare class/reason
instead of an invented atom. Effect on the sweep: `binary_module_SUITE:error_info` is a divergence whose detail
is the placeholder, so the real reason stays invisible to the harness.

### D2 — `binary:referenced_byte_size/1` of a sub-binary

| shape | oracle | rust |
|---|---|---|
| `part(<<"z", (binary:copy(<<255>>,1024))/binary>>, 1, 510)` | `{510,1025}` | `{510,510}` |
| `part(erts_debug:unaligned_bitstring(<0..255>,3), 1, 100)` | `{100,257}` | `{100,100}` |
| `part(iolist_to_binary([copy(<<1>>,2000), copy(<<2>>,50)]),10,510)` | `{510,2050}` | `{510,510}` |
| a sub-binary **of a sub-binary** | `{510,2050}` | `{510,510}` |
| a literal binary, `list_to_binary/1`, `iolist_to_binary/1`, whole `binary:copy/2` | own size | own size (**agree**) |

So the fix that landed covers the *whole refc binary* shape only: every sub-binary whose parent is a
**refc** binary reports its own size instead of the referenced parent size. Rows: `binary_module_SUITE:referenced`
(`{badmatch,1023}`), `binary_module_SUITE:doctests`, `binary_module_SUITE:copy`'s referenced assertions, and
`unicode_SUITE:ex_binaries_errors_*`'s `binary:referenced_byte_size(Chomped) =:= PBSz` checks (see D12 for the
two unicode rows that are *not* this).
**Owner:** `beam_bifs` `binary:referenced_byte_size/1` together with the sub-binary bookkeeping in
`beam_term`/`beam_alloc` (the parent/live size is not carried on the sub-binary).

### D3 — `erlang:trace/3` (fixed)

`{1,1,{flags,[running,call]}}` on both sides for the enable + `trace_info(flags)` readback, i.e. the flags
round-trip through the runtime's own trace state.

### D4 — map no-op identity, compiled route only

| expression (compiled) | oracle | rust |
|---|---|---|
| `M = #{a=>1,b=>2}, erts_debug:same(maps:put(a,1,M), M)` | `true` | **`false`** |
| `M = #{a=>1}, erts_debug:same(maps:put(a,1,M), M)` | `true` | `true` |
| `M = #{a=>1,b=>2}, erts_debug:same(M#{a=>1}, M)` (update syntax) | `true` | `true` |
| 100-key map: `erts_debug:same(maps:put(1,1,M), M)` | `true` | **`false`** |
| 100-key map: `erts_debug:same(maps:merge(#{},M), M)` | `true` | `true` |
| 100-key map: `erts_debug:same(M` with a *different* value `)` — control | `false` | `false` |

The `-eval` route answers `true` on every row; only the compiled route diverges, and there only for
`maps:put/3` on a map with **≥2** keys (a 1-key flatmap short-circuits). `put_map_assoc` (the `M#{K=>V}`
instruction) keeps identity, so the residual is the `maps:put/3` path writing every map through
`terms::make_map_rooted` with no found-key short-circuit.
**Owner:** `beam_engine` (`families/maps`, `terms::make_map_rooted`).

### D5 — stacktrace head

From `stacktrace_head_probe.erl` (**compiled**), the frame list below the raise agrees for `maps:keys/1`,
`maps:get/2` and `binary:part/3`.

| shape | oracle | rust |
|---|---|---|
| `try maps:with([a,b], (fun(X) -> X end)(a)) catch C:R:S` | head `{maps,with,[[a,b],a]}` (with `{file,"maps.erl"},{line,…}`) | head identical, no `file`/`line` on that item |
| `catch maps:with([a,b], (fun(X) -> X end)(a))` (legacy) | head `{maps,with,[[a,b],a],…}` | head `{d5_probe,legacy_catch,0,…}` — **the BIF frame is missing** |
| `catch lists:reverse(a)` (legacy) | head `{lists,reverse,1,…}` | head `{d5_probe,legacy_catch,0,…}` |
| tail call out of a fun (`row/2` → `fun` → `maps:with/2`) | fun frame replaced by the tail call | fun frame `'-main/0-fun-0-'` kept |

The `-eval` route now attaches **one derived raise-site item** per raise (an empty trace is no longer
observed), but it names the raise site where OTP names the caller.
**Consequence for the suite:** `maps_SUITE:t_from_keys/t_with_2/t_without_2/t_merge_opt` fail with
`{badmatch,{'EXIT',…}}` because the legacy `catch`-and-match on the BIF head frame cannot succeed.
**Owner:** `beam_engine` `families/exceptions/callee.rs` (legacy-`catch` head-frame synthesis + the
`file`/`line` metadata on a synthesized BIF item).

### D6 — `erlang:bump_reductions/1` is a catchable typed gap

| shape | oracle | rust |
|---|---|---|
| `catch erlang:bump_reductions(134217727)` (compiled) | `true` | `{'EXIT',{unsupported_bif,[{d6_probe,caught,0,…}]}}` |
| `try … catch C:R` (compiled) | `true` | `{error,unsupported_bif}` |
| `-eval` uncaught | `true` | `beam_rt: unsupported: bif`, exit `3` |

So the reason **is** an observable Erlang exception (`error:unsupported_bif`); the CLI `unsupported` exit `3`
appears only when the raise escapes the program on the `-eval` route. It stays a typed gap — never a wrong
answer — and it is what `binary_module_SUITE:longest_common_trap` reports
(`{exception,error,unsupported_bif,{erlang,bump_reductions,[…]}}`).
**Owner:** `beam_bifs` (the `erlang:bump_reductions/1` registry entry).

### D7 — `erlang:system_info(snifs)` (fixed)

556 3-tuples, head `{erlang,ref_to_binary,1}`; the oracle answers 553 with head
`{records,get_definition,2}`. Each is that runtime's own static-NIF table, so only the *availability* of the
list is the contract, not the count.

### D8 — BIF-driven Erlang callbacks are orders of magnitude late

`maps_bif_loop_probe.erl`, 1000-key maps, 50 rounds, **one** invocation per runtime, two runs with the order
swapped (`loadavg` `19.33` → `18.60`; only the ratio is meaningful):

| workload | rounds × size | ratio, run 1 | ratio, run 2 |
|---|---|---|---|
| `maps:merge_with/3` | 50 × 1000 keys | **662×** | **614×** |
| `maps:intersect_with/3` | 50 × 1000 keys | **92×** | **95×** |
| `maps:iterator/1` + `maps:next/1` | 50 × 1000 entries | **438×** | **480×** |
| `maps:to_list/1` (no Erlang callback — control) | 50 × 1000 entries | 3.7× | 3.7× |

Both sides computed the same work (`checksum = 0` on both sides for every row). The control row shows a pure
BIF over the same maps costs ~4×, so essentially the whole residual is the per-callback-call cost of a
higher-order BIF. Same kind as recorded, roughly in the same magnitude.
**Owner:** `beam_engine`/`beam_bifs` boundary — the callback-crossing path (`apply_fun` from inside a BIF).

### D9 / D11 — `erlang:apply/2` (fixed, one reason shape left)

`apply/2` is fixed: a fun applied to a proper argument list, `badarg` for a non-fun/non-list, and
`apply(erlang, self)` is `badarg`.

| shape (compiled, `apply_args_probe` / `queue_oops_probe`) | oracle | rust |
|---|---|---|
| `smallest([a])` with `smallest(Args) -> apply(fun(X) -> X end, Args)` | `a` | `a` |
| `call(F, As)` = `try apply(F,As) of …` (the suite's shape), `call(fun(X)->X end,[a])` | `{value,a}` | `{value,a}` |
| `apply(erlang, self)` (two atoms) | `error:badarg` | `error:badarg` (head frame `{queue_oops_probe,two_atoms,0,…}`; the oracle names `{erlang,apply,…}`) |
| `apply(lists, reverse)` | `error:badarg` | `error:badarg` |
| `mismatch(F,As) -> apply(F,As)` with `fun() -> x end`, `[a]` | `error:{badarity,{Fun,[a]}}` | **`error:badarg`** |
| `apply(fun lists:reverse/1, [b,c])` (arity mismatch) | `error:{badarity,{fun lists:reverse/1,[b,c]}}` | **`error:badarg`** |
| `known_length(F) -> apply(F,[a]).` / `apply(fun(X)->X end,[a])` | `a` | `a` (compiler-rewritten `call_fun`, both sides) |

So every *value* agrees and the whole matrix is closed except the arity-mismatch **reason**, which degrades
from `{badarity,Fun|Args}` to plain `badarg` (and the degraded `badarg` names the caller instead of
`erlang:apply`).
**Owner:** `beam_rt` native floor (`src/native.rs`, the `("erlang","apply",2)` arm) — the arity check must
raise `badarity` with the fun/args payload instead of reusing the `badarg` path.
**`queue_SUITE:oops`:** the row now **agrees** (both sides `pass`) in `../stdlib/report.json`; the case's
op-1 `queue:peek({[],[]})` disagreement was entirely this defect, so the suite's model never diverged.

### D10 — `erts_debug:set_internal_state/2` (fixed)

The gate has to be opened first; a bare call answers `undef` on both sides. With the gate open:

| call | oracle | rust |
|---|---|---|
| `set_internal_state(available_internal_state,true)` | `false` | `false` |
| `set_internal_state(unicode_loop_limit,10)` | `40000` (previous limit) | `40000` |
| `set_internal_state(binary_loop_limit,10)` | `0` | `0` |
| `set_internal_state(re_loop_limit,10)` | `64000` | `64000` |

The reported value is the **previous** value (the xchg rule), served from
`crates/beam_bifs/src/runtime/erts_debug.rs`, and the gate-off answer is `undef` with a trace that names
`erts_debug:set_internal_state/2` on both sides (the rust trace is one item shorter).
Consequently `binary_module:copy` is no longer blocked by this class.

### D11 — `erlang:trace_delivered/1` after the traced child is gone

| expression (compiled, one process per row) | oracle | rust |
|---|---|---|
| `after_down`: `trace(Pid,true,[running,{tracer,self()}])`, receive `{'DOWN',…}`, then `trace_delivered(Pid)` | `#Ref<…>` | `{'EXIT',{badarg,…}}` |
| `live`: the traced process is still alive | `#Ref<…>` | `#Ref<…>` |
| `dead_no_monitor`: a pid that terminated without being monitored | `#Ref<…>` | `{'EXIT',{badarg,…}}` |

The **oracle** is right: `erlang:trace_delivered/1`'s documented failure is `badarg` only when the argument
"does not refer to a process (dead or alive)", and the C `trace_delivered_1` accepts any internal pid plus
the `all` atom. The rust seam rejects a pid the process table no longer holds:
`crates/beam_rt/src/trace.rs` `trace_delivered_1` → `if !self.process_table().contains(pid) { Err(BadArg) }`.
**Owner:** `beam_rt` trace (the delivery bookkeeping must outlive the process, as `trace_delivered` is
`all`-scoped to the tracer's history, not to a live tracee). This is what
`maps_SUITE:t_from_list_check_trapping` reports (`{exception,error,badarg,{erlang,trace_delivered,…}}`).

### D12 — a `utf32` bit-syntax segment ignores `little`/`native` for a runtime value

| expression (compiled) | oracle | rust |
|---|---|---|
| `f(E) -> <<E/utf32-little>>.` called `f(1)` | `<<1,0,0,0>>` | **`<<0,0,0,1>>`** |
| `f(E) -> <<E/utf32-native>>.` called `f(1)` (little-endian host) | `<<1,0,0,0>>` | **`<<0,0,0,1>>`** |
| `<<1/utf32-little>>` (compile-time constant), `<<E/utf32>>`, `<<E/utf32-big>>` | `<<1,0,0,0>>` | `<<1,0,0,0>>` |
| `<<E:32/little>>` (integer segment) | `<<1,0,0,0>>` | `<<1,0,0,0>>` |
| `<<E/utf16-little>>`, `<<E/utf16-native>>` | `<<1,0>>` | `<<1,0>>` |
| match: `<<C/utf32-little,Rest/binary>> = <<1:32/little>>` | `{1,<<>>}` | `{1,<<>>}` |
| the suite helper shape: `list_to_binary([<<E/utf32-little>> || E <- Bytes])` | utf32-little bytes | **byte-swapped** |

So only **construction** of a `utf32` segment with an explicit `little`/`native` flag is affected, and only
when the value is not compile-time constant (a literal folds to the correct bytes; a `Big` segment and the
`utf16`/integer families are correct). `BEAM_RT_TIER=interpreter` reproduces the same wrong bytes, so it is
not a JIT-only path. Smallest reproduction: `unicode_utf32_probe:u32_little_from_var/1`.
**Seam:** `crates/beam_engine/src/families/bitstrings/create/prepared.rs` (`(UtfKind::Utf32, Some(v)) =>
put_utf32(v)`) and `…/create/segments.rs` (`put_utf32` → `v.to_be_bytes()`) — `put_utf32` takes **no
`little` parameter**, unlike `put_utf16(value, little)`, so the flag is dropped on that arm.
**Owner:** `beam_engine` (`families/bitstrings/create`).
**Suite rows:** `unicode_SUITE:ex_binaries_errors_utf32_little/…_utf32_big`'s `freeze/…` assertion at the
`utf32_inner_loop` step, plus `unicode_SUITE:roundtrips` — `do_roundtrips/1`'s
`Utf32_Little = list_to_utf32_little_bsyntax(List,{utf32,little})` builds `<<E/utf32-little>>` in a
comprehension, so the child raises `{badmatch,…}` and the parent's `normal = Reason` fails with
`{badmatch,{{badmatch,…},[…]}}` at `ex_roundtrips/1`. (The utf16/utf8 `inner_loop` rows are D2, not this.)

### D12b — `unicode:characters_to_binary/3`'s latin1 error split (open, unattributed)

`unicode_SUITE:latin1` fails at staged `unicode_SUITE.erl:414`, i.e.
`{error,DoubleUtf8,Rest2} = unicode:characters_to_binary([All,AllBin,16#FFF], latin1)` followed by
`[16#FFF] = lists:flatten(Rest2)` → `{badmatch,<<0,0,…>>}`. The list mixes a *character list* and a
*binary* and the trailing `16#FFF` is a latin1 error, so the residual reports the wrong remainder (a binary
instead of `[16#FFF]`). Recorded separately because it is not the utf32 construct: the utf32 lines above it
in `ex_latin1` pass. **Owner:** not yet attributed — the candidate is `beam_bifs`'s
`unicode:characters_to_binary/3` error/rest construction on the latin1 character-list path.

## Extra observation — `erts_debug:flat_size/1`

`erts_debug:flat_size/1` raises a **catchable** `error:unsupported_bif` once its argument reaches
**524288 (`1 bsl 19`)** list elements (500000 elements answer `1000000`; 524288 answers `unsupported_bif`).
The oracle computes both. Independent of `lists:duplicate/2`, which is fine well past `1 bsl 21`.

## Sweep rows: which timeouts are budget-only and which are real

The 10 s sweep was load-perturbed (`loadavg` 61.39 before, 58.32 after); the 120 s re-check is authoritative
for `maps_SUITE`, `lists_SUITE` and `binary_module_SUITE`. Each remaining row was then measured **standalone**
(same argv, no harness budget, one process) at `loadavg` `4.16`–`12.82`:

| row | 10 s | 120 s | standalone (both sides) | classification |
|---|---|---|---|---|
| `binary_module:bin_to_list` | timeout | — | `ok` both (rust 31 s vs sys 1 s) | **budget-only** (perf) |
| `binary_module:list_to_bin` | timeout | — | `ok` both (1 s / 1 s) | **budget-only** |
| `binary_module:parts` | timeout | — | `ok` both (rust 27 s vs sys 2 s) | **budget-only** (perf) |
| `binary_module:random_ref_fla_comp` | timeout | — | `ok` both (rust 24 s vs sys <1 s) | **budget-only** (perf) |
| `binary_module:random_ref_comp` | timeout | timeout | rust still >400 s (progress: `Number of successes: …`), sys 4 s | **real, perf** (>100×; not a value divergence) |
| `binary_module:random_ref_sr_comp` | timeout | timeout | rust still >400 s (`Number of successes: 22532`), sys 3 s | **real, perf** (>100×) |
| `binary_module:copy` | timeout (sys too) | **pass both** | — | **budget-only** (D10 was the earlier blocker) |
| `dict_SUITE:store` | timeout | — | `ok` both (rust 5 s) | **budget-only** |
| `lists_SUITE:suffix` | timeout | timeout | `ok` both (<1 s) | **budget-only** (harness/load artifact) |
| `maps:t_intersect` / `t_intersect_with` / `t_iterator_2` / `t_merge_with` | timeout | **pass both** | — | **budget-only** |
| `maps:t_keys_trapping` | timeout | `{badmatch,system_limit}` | D1 (`count_nr_of_yields/2` builds a 65537+ element list) | **real** (D1) |
| `maps:t_values_trapping` / `t_from_keys_check_trapping` | timeout | `{badmatch,system_limit}` | same | **real** (D1) |
| `maps:t_from_list_check_trapping` | `badarg trace_delivered` | same | D11 | **real** (D11) |
| `string_SUITE:meas` | timeout (sys too) | — | both sides time out | **budget-only** (system-side; `agree-fail-identical`) |
| `unicode:category`, `unicode:random_lists`, `unicode:utf8_illegal_sequences_bif` | timeout | — | `ok` / `ok` both standalone | **budget-only** (perf) |
| `unicode:binaries_errors_limit` | `badmatch,false` | — | D2 | **real** (D2) |

Real divergences that are *not* timeouts at any budget, from both artifacts: `binary_module:doctests`,
`binary_module:error_info` (D1c), `binary_module:longest_common_trap` (D6), `binary_module:referenced` (D2),
`maps:doctests`, `maps:t_from_keys`/`t_merge_opt`/`t_with_2`/`t_without_2` (D5), and the five
`unicode:ex_binaries_errors_*` rows plus `unicode:latin1`/`roundtrips` (D2/D12).

## Probe files

| file | class | what it measures |
|---|---|---|
| `apply_args_probe.erl` | D9 | the `apply/2` argument matrix (each row guarded) |
| `bigterm_spawn_probe.erl` | D1 | `lists:duplicate(1 bsl 21, x)` 5× plain, 5× in a `spawn_monitor` child, a deep-recursion/big-term loop, and the `flat_size` threshold |
| `error_info_child_probe.erl` | D1b | `binary_module_SUITE:error_info([])` in the main process and in a spawned child (needs the staged suite `ebin`) |
| `error_info_abort_probe.erl` | D1c | `error_info/1`'s entry list through `ct_runner:run_case/4`'s shape: `{prefix, N}` (the 69→70 threshold) and `{one, Case}` |
| `maps_bif_loop_probe.erl` | D8 | `merge_with`/`intersect_with`/`iterator+next` reduced-size workloads, with a checksum and a no-callback control row |
| `queue_oops_probe.erl` | D9 | `erlang:apply/2` from **compiled** code: the smallest reproduction, the `queue_SUITE:call/2` shape, the two-atom `apply(M,F)`, the arity-mismatch reason, the external-fun shape, and the compiler-rewritten control |
| `stacktrace_head_probe.erl` | D5 | stacktrace head from **compiled** code for `maps:with/2`, `maps:keys/1`, `maps:get/2`, `lists:reverse/1`, `binary:part/3`, incl. the tail-call-from-a-fun and legacy-`catch` rows |
| `unicode_utf32_probe.erl` | D12 | `<<E/utf32-little>>` from a parameter vs a literal, the `/utf32-native`, shift-segment, `utf16-little`, match-context and comprehension (suite-helper) shapes |
| `matrix.sh` | shared | one-expression-per-line runner that prints both sides |

Build and run any of them with:

```
erlc -o $TMPDIR/stdlib-diag/ebin erts_rust/tools/suites/results/stdlib-diagnostics/<probe>.erl
cd $TMPDIR/stdlib-diag/cwd   # a throwaway cwd
<rt>  -noshell -pa $TMPDIR/stdlib-diag/ebin -eval '<probe>:main(), halt(0).'
erl   -noshell -pa $TMPDIR/stdlib-diag/ebin -eval '<probe>:main(), halt(0).'
```

Staged-suite probes additionally need `-pa /tmp/erts-suites-stage/stdlib/ebin -pa /tmp/erts-suites-stage/stdlib/ctl`.
