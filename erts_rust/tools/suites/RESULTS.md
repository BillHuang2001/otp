# Real test suites under both runtimes — measured record

Two targets, each run under the **system ERTS** (oracle) and under the **Rust runtime** (`beam_rt`) with
the *identical* command line:

1. **Khepri** (`/home/bill/Source/khepri`, third-party, read-only) — its eunit suite, one row per module,
   driven with `eunit:test('<module>',[verbose])`.
2. **OTP `stdlib`** (`lib/stdlib/test`, read-only) — eight `common_test` suites, one row per **case**,
   driven by the committed runner `data/ct_runner.erl` (each case in its own `spawn_monitor`, per-case
   timeout, suite budget).

Every figure below is re-derivable from the committed artifacts `results/khepri/report.json`,
`results/stdlib/report.json` and the stdlib budget re-check `results/stdlib/recheck-120/report.json`
(schema `suites/1`). The per-row analysis — the smallest failing expression for every non-agree row and
the crate that owns the fix — lives next to the artifacts in `results/khepri-notes.md` (§Root causes
RC-B…RC-D) and `results/stdlib-notes.md` (§D1…§D11).
The host is **shared**: only counts, verdicts and same-host ratios are portable; no absolute wall time
here is a benchmark.

## 0. Reproduce

```
# prerequisites: the oracle `erl`/`erlc` on $PATH (or $ERTS_ORACLE / $ERLC), and a release runtime
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --release

# the harness (a standalone cargo workspace; read-only $HOME needs CARGO_HOME)
cd erts_rust/tools/suites && CARGO_HOME=${CARGO_HOME:-$HOME/.cache/cargo-home} cargo build
#   ... or `cargo run -- <args>`; $BEAM_RT overrides target/release/beam_rt

erts_rust/tools/suites/target/debug/suites run --target khepri --jobs 4 --out results/khepri
erts_rust/tools/suites/target/debug/suites run --target stdlib \
    --suite lists_SUITE,maps_SUITE,proplists_SUITE,dict_SUITE,queue_SUITE,binary_module_SUITE,string_SUITE,unicode_SUITE \
    --timeout 10 --suite-budget 300 --jobs 4 --out results/stdlib
erts_rust/tools/suites/target/debug/suites run --target stdlib \
    --suite maps_SUITE,lists_SUITE,binary_module_SUITE \
    --timeout 120 --suite-budget 900 --jobs 1 --out results/stdlib/recheck-120

suites list            # what the two targets are, and whether each side resolves on this host
suites run --target stdlib --suite maps_SUITE --timeout 60 --jobs 1 --out $TMPDIR/one-suite
```

Khepri's `rebar3 as test compile` is the only dependency-fetching step (rebar3 is resolved from the Nix
store when it is not on `PATH`), with `HOME`/`REBAR_CACHE_DIR`/`HEX_HOME`/`TMPDIR` redirected under
`$TMPDIR/erts-suites-stage/khepri`; the upstream checkout is never written. The `-pa` set is identical
on both sides — every `_build/test/lib/*/ebin` plus `_build/test/lib/khepri/test` (11 entries).
Each row runs in its own cwd (`$TMPDIR/erts-suites-stage/khepri/cwd/<module>`) that the harness
**clears before the row starts** (`target::fresh_dir`), so a `_test.<module>` tree left by an earlier
sweep cannot poison either side's fixtures; both sides of one row share that one fresh directory.
`rebar3 eunit` is never used for the comparison numbers.

The stdlib side is copied into `$TMPDIR/erts-suites-stage/stdlib/src` and compiled there with the
oracle's `erlc`: `targets[0].build` is `erlc -o <stage>/ebin -I <stage>/src <suite>.erl …` (`ok: true`,
`reused: true`), the `-pa` set is `<stage>/ebin` + `<stage>/ctl` on both sides, and each suite runs in its
own cwd (`$TMPDIR/erts-suites-stage/stdlib/cwd/<SUITE>`) that the harness clears the same way before the
row starts. The `--timeout 120 --jobs 1` re-check above is a second artifact under
`results/stdlib/recheck-120/`; §3 reads the two together. (That report's own `run_command` still records
the temporary `--out /tmp/recheck-timeouts` it was written with; the committed copy is the same report.)

## 1. Both sides

| | Khepri sweep | stdlib sweep |
|---|---|---|
| artifact | `results/khepri/` | `results/stdlib/` (+ `results/stdlib/recheck-120/`) |
| oracle | `/nix/store/…-erlang-29.1.2/…/erts-17.1.1/bin/erl` → `29/17.1.1` | same |
| rust runtime | `beam_rt` `30/17.1`, default baseline-JIT tier | same |
| runtime source sha | `125ca3a9f3f7260dfc2400faff18117b67834558` | same |
| host loadavg before → after | `38.75 42.77 30.63` → `31.68 40.27 30.41` | `27.19 39.03 30.12` → `22.19 32.24 28.68` |
| `--jobs` | 4 | 4 (re-check: 1) |
| drive | `eunit:test('<module>',[verbose]), halt(0).` | `ct_runner:run('<suite>',10000,300000,"<data>"), halt(0).` |

The sha names the workspace commit the run's `beam_rt` was built from — not a binary hash: the runtime
binary is per-worktree and git-ignored. Both columns name the same commit, and no file under
`erts_rust/crates` differs between it and this tree (`git diff --stat 125ca3a9f3f7 HEAD --
erts_rust/crates` is empty), so both sweeps describe the current runtime sources. The stdlib column's
per-case budget is 10 s with a 300 s suite budget; its serial re-check raises that to 120 s / 900 s.

## 2. Khepri — eunit, one row per module

51 candidate modules = **43 with real eunit tests** (on the oracle) + 8 that carry none.

| | rows that ran a test | fully green | tests passed | tests failed |
|---|---|---|---|---|
| oracle | 43 | **42** | **1500** | **1** (`stored_procs:crashing_sproc_stacktrace_test`, a pre-existing OTP-29/checkout issue) |
| rust | 19 | **12** | **285** | **8** (`app` 5, `keep_while_conditions` 1, `projections` 1, `display_tree` 1) |

Verdicts: `agree` 12 · `rust-gap` 29 · `divergence` 2 · `agree-fail-identical` 8 (the `no_tests` rows).
On rust 19 of the 43 rows executed at least one test: 12 ran their module to completion green,
`display_tree` to completion red, and 6 (`app`, `db_info`, `keep_while_conditions`, `projections`,
`protect_against_dups_option`, `simple_tx_misc`) stopped part-way. The remaining **24** rows executed no
test at all — `cancelled` in setup — so **1 208 of the oracle's 1 501 tests never ran on the rust side
and are `unknown`, not failing.** The 12 rows green on rust are a strict subset of the oracle's 42:
nothing passes on rust that fails on the oracle. Three of the partial rows (`db_info` 3/0,
`protect_against_dups_option` 5/0, `simple_tx_misc` 2/0) passed every test they reached, but they are
not green — the module's remaining tests never ran.

Every `rust-gap` row is `cancelled`: the `rust P/F` pair is the prefix that ran before the fixture died,
and `0/0` means not one test of that module executed.

| module | sys P/F | rust P/F | verdict | class |
|---|---|---|---|---|
| `advanced_delete` | 33/0 | 0/0 | rust-gap | RC-B |
| `advanced_get` | 13/0 | 0/0 | rust-gap | RC-B |
| `advanced_put` | 40/0 | 0/0 | rust-gap | RC-B |
| `advanced_tx_delete` | 39/0 | 0/0 | rust-gap | RC-B |
| `advanced_tx_get` | 15/0 | 0/0 | rust-gap | RC-B |
| `advanced_tx_put` | 43/0 | 0/0 | rust-gap | RC-B |
| `app` | 36/0 | 22/5 | rust-gap | RC-B (all 5 failures are the `ra` fixture) |
| `async_option` | 20/0 | 0/0 | rust-gap | RC-B |
| `bang_functions` | 85/0 | 0/0 | rust-gap | RC-B |
| `cluster_SUITE` | 0/0 | 0/0 | excluded | `common_test` suite: no eunit tests |
| `conditions` | 23/0 | 23/0 | agree | — |
| `cth_log_redirect_any_domains` | 0/0 | 0/0 | excluded | no eunit tests |
| `db_info` | 24/0 | 3/0 | rust-gap | RC-B |
| `delete_command` | 12/0 | 12/0 | agree | — |
| `display_tree` | 11/0 | 10/1 | divergence | RC-C |
| `export` | 56/0 | 0/0 | rust-gap | RC-B |
| `export_erlang` | 63/0 | 0/0 | rust-gap | RC-B |
| `export_helper` | 0/0 | 0/0 | excluded | no eunit tests |
| `favor_option` | 5/0 | 0/0 | rust-gap | RC-B |
| `fence` | 6/0 | 0/0 | rust-gap | RC-B |
| `helpers` | 0/0 | 0/0 | excluded | no eunit tests |
| `keep_while_conditions` | 27/0 | 21/1 | rust-gap | RC-B (setup) + **RC-D** (the 1 failure) |
| `machine_code_called_from_ra` | 15/0 | 0/0 | rust-gap | RC-B |
| `mod_used_for_transactions` | 0/0 | 0/0 | excluded | no eunit tests |
| `non_versioned_commands` | 15/0 | 15/0 | agree | — |
| `path_operations` | 51/0 | 51/0 | agree | — |
| `pattern_tree` | 5/0 | 5/0 | agree | — |
| `prefix_tree` | 4/0 | 4/0 | agree | — |
| `projections` | 134/0 | 2/1 | rust-gap | RC-B |
| `prop_path_operations` | 0/0 | 0/0 | excluded | `proper` module: no eunit tests |
| `prop_state_machine` | 0/0 | 0/0 | excluded | `proper` module: no eunit tests |
| `protect_against_dups_option` | 13/0 | 5/0 | rust-gap | RC-B |
| `put_command` | 19/0 | 19/0 | agree | — |
| `queries` | 16/0 | 16/0 | agree | — |
| `return_indirect_deletes_option` | 15/0 | 0/0 | rust-gap | RC-B |
| `root_node` | 22/0 | 22/0 | agree | — |
| `sigils` | 4/0 | 4/0 | agree | — |
| `simple_delete` | 42/0 | 0/0 | rust-gap | RC-B |
| `simple_get` | 94/0 | 0/0 | rust-gap | RC-B |
| `simple_put` | 38/0 | 0/0 | rust-gap | RC-B |
| `simple_tx_delete` | 46/0 | 0/0 | rust-gap | RC-B |
| `simple_tx_get` | 106/0 | 0/0 | rust-gap | RC-B |
| `simple_tx_misc` | 45/0 | 2/0 | rust-gap | RC-B |
| `simple_tx_put` | 47/0 | 0/0 | rust-gap | RC-B |
| `stored_procs` | 12/1 | 0/0 | divergence | oracle-side pre-existing failure + RC-B (rust) |
| `test_ra_server_helpers` | 0/0 | 0/0 | excluded | no eunit tests |
| `timeout` | 6/0 | 0/0 | rust-gap | RC-B |
| `triggers` | 118/0 | 0/0 | rust-gap | RC-B |
| `tx_funs` | 42/0 | 42/0 | agree | — |
| `tx_sprocs` | 33/0 | 0/0 | rust-gap | RC-B |
| `utils` | 7/0 | 7/0 | agree | — |

## 3. OTP `stdlib` — `common_test` suites, one row per case

214 cases over 8 suites; 4 rows are excluded (§5).

| suite | cases | oracle P/F | rust P/F | agree | divergence | rust-gap | excluded |
|---|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 18/2 | 4/3 | 4 | 3 | 12 | 1 |
| `dict_SUITE` | 7 | 7/0 | 7/0 | 7 | 0 | 0 | 0 |
| `lists_SUITE` | 78 | 78/0 | 77/1 | 77 | 1 | 0 | 0 |
| `maps_SUITE` | 32 | 32/0 | 20/12 | 20 | 12 | 0 | 0 |
| `proplists_SUITE` | 4 | 4/0 | 4/0 | 4 | 0 | 0 | 0 |
| `queue_SUITE` | 7 | 7/0 | 6/1 | 6 | 1 | 0 | 0 |
| `string_SUITE` | 46 | 45/1 | 45/1 | 45 | 0 | 0 | 1 |
| `unicode_SUITE` | 20 | 18/2 | 6/14 | 6 | 12 | 0 | 2 |
| **total** | **214** | **209/5** | **169/32** | **169** | **29** | **12** | **4** |

`dict_SUITE` and `proplists_SUITE` agree on every case; `lists_SUITE` and `string_SUITE` differ on one
row each. Rust's green set is a subset of the oracle's — no case passes on rust that fails on the
oracle.

Three honesty notes on the shape of this table. (a) The 12 `rust-gap` rows are one event, not twelve
findings: the rust side of `binary_module_SUITE` **dies mid-suite** (D1), so every case after the abort
is reported `error 0/0 case missing from the rust runtime's output` — not measured, not failed. Measured
one process per row those cases split 8 `agree`, 1 `agree-fail-identical`, 1 typed gap, 1 D2 divergence
and 2 too slow to finish (`results/stdlib-notes.md` §D1). (b) A `TIMEOUT` row is a *budget* statement,
not a capability: seven rows time out on the rust side only, and five of them agree in the serial
`--timeout 120 --jobs 1` re-check (`results/stdlib/recheck-120/`), while two are D1's allocation
`system_limit`. (c) `binary_module_SUITE:copy` is the one recorded `agree-fail-identical` that does not
survive a larger budget: at 120 s the oracle passes and rust still fails on D10, so it is a divergence
there (§5).

## 4. Divergences, by root-cause class

Each class names the affected rows, the smallest failing expression (full text and both sides'
observations in the notes), and the crate that owns the fix. **Nothing here was fixed from this node** —
`./erts_rust/tools` may only report.

| class | rows | smallest failing expression (rust vs oracle) | owner |
|---|---|---|---|
| **RC-B** heap-sizing `process_flag/2` flags answer `badarg` | 29 khepri (all `ra`-backed modules) | `process_flag(min_bin_vheap_size,46422)` → `error:badarg` vs `46422` (same for `min_heap_size`, `fullsweep_after`) | `beam_rt` (`context/process_bridge.rs::process_flag`) |
| **RC-C** ETF encode of a term that *contains* a local fun | 1 khepri (`display_tree`), same encoder behind stdlib doctests | `term_to_binary({fun() -> ok end})` → `error:unsupported_local_fun` vs `89` bytes (the bare fun agrees) | `beam_bifs` (`binconv.rs::term_encoder`) + `beam_codec` (single `FunIdentity`) |
| **RC-D** keep-while delete fan-out is 170–420× slower | 1 khepri (`keep_while_conditions`) | the suite's own `?assert(DeletionTime < 1000)`; 2 500 dependents, 7.2–11.8 s vs 28–47 ms | not narrowed to one opcode (needs a profile; shape points at `beam_engine`'s baseline tier) |
| **D1** rust side **dies** mid-suite (`stderr beam_rt: system_limit`, exit 1), plus `system_limit` from a big allocation inside a `lists` callback | 16 stdlib (13 in `binary_module_SUITE` — 12 `rust-gap` + `interesting` — and 3 `maps` `t_*_trapping` rows) | the ct hand-off (`try` + `exit({ct_case_result,R})` out of a `spawn_monitor` child) around `binary_module_SUITE:error_info([])`: aborts from the 70th of the function's 73 entries while the main process, a plain child and a returned value all pass; separately `lists:foreach(fun(_) -> length(lists:duplicate(200000,x)) end, [1])` → `error:system_limit` vs `ok` | `beam_rt` (escape copy of the raise's reason: `runtime/gc.rs::stabilize_error_live_reason` → `Runtime::stabilize_term`), `beam_message` (`copy::copy_match_context`), `beam_term` (`bitstring::bit_at`) |
| **D2** `binary:referenced_byte_size/1` of a sub-binary answers its own size | 6 stdlib (`binary_module:doctests` + 5 `unicode` `ex_binaries_errors_*`) | `Part=<<"z",(binary:copy(<<255>>,1024))/binary>>, C=binary:part(Part,1,510), {byte_size(C),binary:referenced_byte_size(C)}` → `{510,510}` vs `{510,2050}` | `beam_bifs` (+`beam_term`) |
| **D5** the BIF frame is missing from the trace head on the legacy `catch` route | 4 stdlib (`maps:t_from_keys`, `t_with_2`, `t_without_2`, `t_merge_opt`) | compiled `m() -> catch maps:with([a,b], id(a)).` → head `{sp,m,0,…}` vs `{maps,with,[[a,b],a],…}`; under `try/catch` both sides carry the BIF frame | `beam_engine` (`families/exceptions/callee.rs`) |
| **D6** `erlang:bump_reductions/1` is not served | 1 stdlib (`maps:doctests`) | `erlang:bump_reductions(134217727)` → catchable `error:'unsupported: erlang:bump_reductions/1'` (through `-eval`, the CLI line `beam_rt: unsupported: …`, exit 3) vs `true` | `beam_bifs` |
| **D8** no preemption inside a BIF-driven Erlang loop | 2 stdlib (`maps:t_merge_with`, `t_iterator_2` — both agree at 120 s) | isolated: `maps:merge_with/3` 580×/764×, `maps:intersect_with/3` 94×/107×, `maps:iterator/1`+`next/1` 442×/525×, against a `maps:to_list/1` control at 4.3×/4.8× | `beam_bifs` + `beam_engine` (reductions) |
| **D9** `erlang:apply/2` is not implemented (the native floor reads the two operands as the `apply/3` `(M,F,Args)` shape) | 1 stdlib (`queue_SUITE:oops`; the suite's own `call/2` helper calls `apply(Func,As)`, so the emulation model answers `{error,badarg}` and the row surfaces as a `{diff,…}` value disagreement) | compiled `smallest(Args) -> apply(fun(X) -> X end, Args).` → `badarg` vs `a`; a literal list, `apply/3` and the `-eval` evaluator agree (the compiler rewrites a literal-length list into `call_fun`); `apply(erlang,self)` answers a pid vs `badarg` | `beam_rt` (native `apply/2` floor) |
| **D10** `erts_debug:set_internal_state/2` is not served | 9 stdlib (7 `unicode` + `binary_module:copy` + `longest_common_trap`) | `…(available_internal_state,true)` → `false` (agrees), then `…(unicode_loop_limit,10)` → `error:unsupported_bif` vs `40000`; through `-eval` it is the CLI line `beam_rt: unsupported: bif`, exit 3 | `beam_bifs`/`beam_rt` |
| **D11** `erlang:trace_delivered/1` rejects an exited pid | 1 stdlib (`maps:t_from_list_check_trapping`) | `erlang:trace_delivered(P)` after the traced child's `{'DOWN',…}` → `badarg` vs a delivery ref (a live pid answers a ref on both sides) | `beam_bifs` |
| *budget, no class* | 3 stdlib (`binary_module:bin_to_list`, `lists:suffix`, `maps:t_intersect`) | all three agree in the 120 s re-check; their 10 s `TIMEOUT` is load/cumulative, not a capability | — |

A row can be named by two classes, so the column lists 43 rows over the 41 non-agree ones (29
`divergence` + 12 `rust-gap`): `binary_module:copy` is §5's excluded row that D10 explains, and
`longest_common_trap` is both one of D1's 13 case-missing rows and the D10 gap that case needs.

Khepri class ids follow `results/khepri-diagnostics/FINDINGS.md`; the stdlib ones follow
`results/stdlib-diagnostics/FINDINGS.md`, which re-verifies each against this runtime and carries the
probe list. **RC-A does not appear because that class does not reproduce on this tree** — the
improper-iodata `file:write`/`pwrite` shapes and the on-disk `dets` bytes are oracle-identical, so the
`ra` fixture gets past `dets` and dies one step later at RC-B. RC-B also carries one narrowed
observation that is not the `process_flag` fallthrough: the oracle's `-eval` process is born
`trap_exit = true` and this runtime's is born `false` (a plain `spawn/1` process and `spawn_opt` birth
options agree). The flag state itself belongs to `beam_process`'s `ProcessFlags`/collector policy and
`beam_gc`'s heap sizing, so the RC-B fix spans `beam_rt` + those two crates.

**D3, D4 and D7 own no row in this sweep.** `erlang:trace/3` answers `1` on both sides and its flag
read-back agrees; `erts_debug:same/2` is `true` for both no-op `maps` calls; and
`erlang:system_info(snifs)` publishes 556 `{M,F,A}` triples against the oracle's 553 (each runtime's own
static-NIF table). Within D5 only the legacy `catch` route is left; the `try/catch` shape carries the BIF
frame on both sides.

D1 is the one class that is **not** a typed gap: it breaks the "every gap is a typed error, never a wrong
result" contract and holds 16 rows. It is the single most valuable finding of this pass.

## 5. Exclusions (in neither numerator)

| row | why |
|---|---|
| `cluster_SUITE` + 7 khepri `no_tests` modules (`cth_log_redirect_any_domains`, `export_helper`, `helpers`, `mod_used_for_transactions`, `test_ra_server_helpers`, `prop_path_operations`, `prop_state_machine`) | no eunit tests on **either** side (a `common_test` suite / hook / helper / `proper` module). `0/0` is reported verbatim; counting it as a pass would be a lie. |
| `stored_procs` | the oracle itself fails `crashing_sproc_stacktrace_test` (12/1) on this OTP-29 host — a checkout/host property, not a parity target. Its rust side is `cancelled` on RC-B, so the row currently reads `divergence`; re-measure once RC-B closes. |
| stdlib `binary_module_SUITE:copy` | at the 10 s budget the oracle TIMEOUTs while rust fails on D10's typed gap (`{exception,error,unsupported_bif,{erts_debug,set_internal_state,…}}`), so neither side gives a comparable result. At 120 s the oracle **passes** and rust still fails on D10 — a budget mismatch at 10 s, a divergence at a larger budget, never agreement. |
| stdlib `string_SUITE:meas` | both sides `TIMEOUT` at the 10 s budget (`detail: timeout` on each side). |
| stdlib `unicode_SUITE:is_id`, `is_whitespace` | both fail identically with `{badmatch,{error,enoent}}` in `parse_properties/1`: the suite wants `UnicodeData.txt`, which lives in `lib/stdlib/uc_spec` rather than in the `unicode_SUITE_data` dir the runner resolves, and the suite exports no `init_per_suite` to fetch it. |

## 6. What these numbers do not say

- **`cancelled` is not `pass` and not `fail`.** 24 of the 43 Khepri rows executed no test at all and six
  more stopped part-way, so 1 208 of the oracle's 1 501 tests are `unknown`. Closing RC-B re-partitions
  that unknown — expect the rust pass count to move for reasons that are not new capability.
- **The host is shared.** The Khepri sweep ran at 1-minute loadavg 38.8 → 31.7 and the stdlib sweep at
  27.2 → 22.2 on 16 logical CPUs. `--jobs 4` plus that load is what produced the seven one-sided stdlib
  `TIMEOUT` rows at the 10 s budget; five of them agree in the serial 120 s re-check and two are D1's
  `system_limit`. Only counts, verdicts and same-host ratios travel.
- **No `common_test` hooks.** The stdlib driver calls `init_per_suite`/`end_per_suite` only; group and
  per-testcase hooks are not executed, so a case that depends on them fails on *both* sides and shows up
  as `agree-fail-identical` rather than as a divergence.
- **Coverage, not conformance.** These rows sample the runtime through real third-party and OTP code;
  they are not the compatibility gate (`tools/differential`, which pins every case against a golden).
  A green row here is evidence, not parity.
