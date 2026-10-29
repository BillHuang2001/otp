# Real test suites under both runtimes — measured record

Two targets, each run under the **system ERTS** (oracle) and under the **Rust runtime** (`beam_rt`) with
the *identical* command line:

1. **Khepri** (`/home/bill/Source/khepri`, third-party, read-only) — its eunit suite, one row per module,
   driven with `eunit:test('<module>',[verbose])`.
2. **OTP `stdlib`** (`lib/stdlib/test`, read-only) — eight `common_test` suites, one row per **case**,
   driven by the committed runner `data/ct_runner.erl` (each case in its own `spawn_monitor`, per-case
   timeout, suite budget).

Every figure below is re-derivable from the committed artifacts `results/khepri/report.json` and
`results/stdlib/report.json` (schema `suites/1`). The per-row analysis — the smallest failing
expression for every non-agree row and the crate that owns the fix — lives next to the artifacts in
`results/khepri-notes.md` (§Root causes RC-A…RC-D) and `results/stdlib-notes.md` (§D1…§D10).
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

suites list            # what the two targets are, and whether each side resolves on this host
suites run --target stdlib --suite maps_SUITE --timeout 60 --jobs 1 --out $TMPDIR/one-suite
```

Khepri's build is the only staged step: `rebar3 as test compile` (rebar3 is resolved from the Nix store
when it is not on `PATH`), with `HOME`/`REBAR_CACHE_DIR`/`HEX_HOME`/`TMPDIR` redirected under
`$TMPDIR/erts-suites-stage/khepri`; the upstream checkout is never written. The `-pa` set is identical
on both sides — every `_build/test/lib/*/ebin` plus `_build/test/lib/khepri/test` (11 entries).
`rebar3 eunit` is never used for the comparison numbers.

## 1. Both sides

| | Khepri sweep | stdlib sweep |
|---|---|---|
| artifact | `results/khepri/` | `results/stdlib/` |
| oracle | `/nix/store/…-erlang-29.1.2/…/erts-17.1.1/bin/erl` → `29/17.1.1` | same |
| rust runtime | `beam_rt` `30/17.1`, default baseline-JIT tier | same |
| runtime source sha | `ec44fa3f7fc7d8419ae32f11b23a06448b565ec7` | `32732ba94bc9d89e8f67be6c472d5a6259af80c0` |
| host loadavg before → after | `169.99 99.02 63.78` → `91.59 90.88 63.41` | `9.89 5.02 7.34` → `4.29 4.59 6.94` |
| `--jobs` | 4 | 4 |
| drive | `eunit:test('<module>',[verbose]), halt(0).` | `ct_runner:run('<suite>',10000,300000,"<data>"), halt(0).` |

The two shas differ only in `erts_rust/tools/suites` — `git diff --stat ec44fa3f7f 32732ba94b --
erts_rust/crates` is empty, so **both sweeps describe the same runtime sources**. The runtime binary is
per-worktree and git-ignored, which is why the sha is the workspace commit and not a binary hash.

## 2. Khepri — eunit, one row per module

51 candidate modules = **43 with real eunit tests** (on the oracle) + 8 that carry none.

| | rows that ran a test | fully green | tests passed | tests failed |
|---|---|---|---|---|
| oracle | 43 | **42** | **1500** | **1** (`stored_procs:crashing_sproc_stacktrace_test`, a pre-existing OTP-29/checkout issue) |
| rust | 19 | **15** | **285** | **8** (`app` 5, `keep_while_conditions` 1, `projections` 1, `display_tree` 1) |

Verdicts: `agree` 12 · `rust-gap` 29 · `divergence` 2 · `agree-fail-identical` 8 (the `no_tests` rows).
On rust **30** rows are `cancelled` — the fixture never came up, so **0** of their tests executed: of the
oracle's 1 501 tests, **1 208 were never run on the rust side and are `unknown`, not failing.**
The 15 green-on-rust modules are a strict subset of the 42 green-on-oracle modules: nothing passes on
rust that fails on the oracle.

| module | sys P/F | rust P/F | verdict | class |
|---|---|---|---|---|
| `advanced_delete` | 33/0 | 0/0 | rust-gap | RC-A |
| `advanced_get` | 13/0 | 0/0 | rust-gap | RC-A |
| `advanced_put` | 40/0 | 0/0 | rust-gap | RC-A |
| `advanced_tx_delete` | 39/0 | 0/0 | rust-gap | RC-A |
| `advanced_tx_get` | 15/0 | 0/0 | rust-gap | RC-A |
| `advanced_tx_put` | 43/0 | 0/0 | rust-gap | RC-A |
| `app` | 36/0 | 22/5 | rust-gap | RC-A |
| `async_option` | 20/0 | 0/0 | rust-gap | RC-A |
| `bang_functions` | 85/0 | 0/0 | rust-gap | RC-A |
| `cluster_SUITE` | 0/0 | 0/0 | excluded | `common_test` suite: no eunit tests |
| `conditions` | 23/0 | 23/0 | agree | — |
| `cth_log_redirect_any_domains` | 0/0 | 0/0 | excluded | no eunit tests |
| `db_info` | 24/0 | 3/0 | rust-gap | RC-A |
| `delete_command` | 12/0 | 12/0 | agree | — |
| `display_tree` | 11/0 | 10/1 | divergence | RC-C |
| `export` | 56/0 | 0/0 | rust-gap | RC-A |
| `export_erlang` | 63/0 | 0/0 | rust-gap | RC-A |
| `export_helper` | 0/0 | 0/0 | excluded | no eunit tests |
| `favor_option` | 5/0 | 0/0 | rust-gap | RC-A |
| `fence` | 6/0 | 0/0 | rust-gap | RC-A |
| `helpers` | 0/0 | 0/0 | excluded | no eunit tests |
| `keep_while_conditions` | 27/0 | 21/1 | rust-gap | RC-A + RC-D |
| `machine_code_called_from_ra` | 15/0 | 0/0 | rust-gap | RC-A |
| `mod_used_for_transactions` | 0/0 | 0/0 | excluded | no eunit tests |
| `non_versioned_commands` | 15/0 | 15/0 | agree | — |
| `path_operations` | 51/0 | 51/0 | agree | — |
| `pattern_tree` | 5/0 | 5/0 | agree | — |
| `prefix_tree` | 4/0 | 4/0 | agree | — |
| `projections` | 134/0 | 2/1 | rust-gap | RC-A |
| `prop_path_operations` | 0/0 | 0/0 | excluded | `proper` module: no eunit tests |
| `prop_state_machine` | 0/0 | 0/0 | excluded | `proper` module: no eunit tests |
| `protect_against_dups_option` | 13/0 | 5/0 | rust-gap | RC-A |
| `put_command` | 19/0 | 19/0 | agree | — |
| `queries` | 16/0 | 16/0 | agree | — |
| `return_indirect_deletes_option` | 15/0 | 0/0 | rust-gap | RC-A |
| `root_node` | 22/0 | 22/0 | agree | — |
| `sigils` | 4/0 | 4/0 | agree | — |
| `simple_delete` | 42/0 | 0/0 | rust-gap | RC-A |
| `simple_get` | 94/0 | 0/0 | rust-gap | RC-A |
| `simple_put` | 38/0 | 0/0 | rust-gap | RC-A |
| `simple_tx_delete` | 46/0 | 0/0 | rust-gap | RC-A |
| `simple_tx_get` | 106/0 | 0/0 | rust-gap | RC-A |
| `simple_tx_misc` | 45/0 | 2/0 | rust-gap | RC-A |
| `simple_tx_put` | 47/0 | 0/0 | rust-gap | RC-A |
| `stored_procs` | 12/1 | 0/0 | divergence | oracle-side pre-existing failure + RC-A |
| `test_ra_server_helpers` | 0/0 | 0/0 | excluded | no eunit tests |
| `timeout` | 6/0 | 0/0 | rust-gap | RC-A |
| `triggers` | 118/0 | 0/0 | rust-gap | RC-A |
| `tx_funs` | 42/0 | 42/0 | agree | — |
| `tx_sprocs` | 33/0 | 0/0 | rust-gap | RC-A |
| `utils` | 7/0 | 7/0 | agree | — |

## 3. OTP `stdlib` — `common_test` suites, one row per case

214 cases over 8 suites; 4 rows are excluded (§5).

| suite | cases | oracle P/F | rust P/F | agree | divergence | rust-gap | excluded |
|---|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 18/2 | 4/3 | 4 | 3 | 12 | 1 |
| `dict_SUITE` | 7 | 7/0 | 7/0 | 7 | 0 | 0 | 0 |
| `lists_SUITE` | 78 | 78/0 | 76/2 | 76 | 2 | 0 | 0 |
| `maps_SUITE` | 32 | 32/0 | 19/13 | 19 | 13 | 0 | 0 |
| `proplists_SUITE` | 4 | 4/0 | 4/0 | 4 | 0 | 0 | 0 |
| `queue_SUITE` | 7 | 7/0 | 6/1 | 6 | 1 | 0 | 0 |
| `string_SUITE` | 46 | 45/1 | 45/1 | 45 | 0 | 0 | 1 |
| `unicode_SUITE` | 20 | 18/2 | 6/14 | 6 | 12 | 0 | 2 |
| **total** | **214** | **209/5** | **167/34** | **167** | **31** | **12** | **4** |

Two honesty notes on the shape of this table. (a) The 12 `rust-gap` rows are all one event: the rust
side of `binary_module_SUITE` **dies mid-suite** (D1), so the cases after the crash are reported `error
0/0 case missing from the rust runtime's output` — not measured, not failed. (b) A `TIMEOUT` row is a
*budget* statement, not a capability: every one was re-measured serially, and three of the five are
load/cumulative artefacts (`results/stdlib-notes.md` §D10).

## 4. Divergences, by root-cause class

Each class names the affected rows, the smallest failing expression (full text and both sides'
observations in the notes), and the crate that owns the fix. **Nothing here was fixed from this node** —
`./erts_rust/tools` may only report.

| class | rows | smallest failing expression (rust vs oracle) | owner |
|---|---|---|---|
| **RC-A** a valid improper iolist with a binary tail is rejected | 29 khepri (all `ra`-backed modules) | `file:write(Fd,[<<"ab">> \| <<"cd">>])` → `{error,badarg}` vs `ok`; then `dets:open_file/2` → `{case_clause,{error,badarg}}` vs `{ok,t}` | `beam_rt` (`native/prim_file.rs::flatten_iodata`) |
| **RC-B** heap-sizing `process_flag/2` flags answer `badarg` | the same 29, behind RC-A | `process_flag(min_heap_size,233)` → `error:badarg` vs `233` (same for `min_bin_vheap_size`, `fullsweep_after`) | `beam_rt` (`context/process_bridge.rs`) |
| **RC-C** ETF encode of a term that *contains* a local fun | 1 khepri (`display_tree`), same encoder behind stdlib doctests | `term_to_binary({fun() -> ok end})` → `error:unsupported_local_fun` vs `80` bytes (the bare fun agrees) | `beam_bifs` (`binconv.rs::term_encoder`) + `beam_codec` (single `FunIdentity`) |
| **RC-D** keep-while delete fan-out is 240–330× slower | 1 khepri (`keep_while_conditions`) | the suite's own `?assert(DeletionTime < 1000)`; 2 500 dependents | not narrowed to one opcode (needs a profile) |
| **D1** rust side **dies** mid-suite (stderr `beam_rt: system_limit`, exit 1) | 13 stdlib (`binary_module_SUITE`) | a *spawned* child running `binary_module_SUITE:error_info([])`: dies at entry 39 where the main process does not | `beam_engine` (spawned-child write path), `beam_gc` second |
| **D2** `binary:referenced_byte_size/1` of a sub-binary answers its own size | 6 stdlib | `Part=<<"z",(binary:copy(<<255>>,1024))/binary>>, C=binary:part(Part,1,510), {byte_size(C),binary:referenced_byte_size(C)}` → `{510,510}` vs `{510,2050}` | `beam_bifs` (+`beam_term`) |
| **D3** `erlang:trace/3` answers `badarg` | 4 stdlib (`maps` trapping rows) | `erlang:trace(self(),true,[running,{tracer,self()}])` → `badarg` vs `1` | `beam_bifs` |
| **D4** map identity is not preserved | 2 stdlib (`maps:merge/3`,`put/3` opt rows) | `M=#{a=>1}, erts_debug:same(maps:merge(#{},M),M)` → `false` vs `true` | `beam_bifs` |
| **D5** stacktrace head names the caller, not the BIF | 3 stdlib (`maps:with/2`,`without/2`) | `catch maps:with([a,b],(fun(X)->X end)(a))` → head `{mm,with2,0,…}` vs `{maps,with,[[a,b],a],…}` | `beam_engine` (`families/exceptions/callee.rs`) |
| **D6** `erlang:bump_reductions/1` is not served | 1 stdlib (`maps:doctests`) | `erlang:bump_reductions(134217727)` → `unsupported_bif` vs `true` | `beam_bifs` |
| **D7** `erlang:system_info(snifs)` is not published | 1 stdlib (`lists_SUITE:error_info`) | `erlang:system_info(snifs)` → `unsupported: … no static NIF table` vs a 553-element list | `beam_rt` |
| **D8** no preemption inside a BIF-driven Erlang loop | 3 stdlib (`maps:t_intersect_with`,`t_iterator_2`,`t_merge_with`) | the cases pass alone, 10.1 s / 41.0 s / 31.4 s vs 11 / 47 / 23 ms — same results, ~900–1000× late | `beam_bifs` + `beam_engine` (reductions) |
| **D9** `apply/2` over an argument list built by a `case` | 1 stdlib (`queue_SUITE:oops`) | `Args = case Ts of [] -> [] end, apply(fun(peek)->{[],[]} end, Args)` → `badarg` vs `{[]}` | `beam_engine` (BIF/`apply` argument resolution) |
| **D10** `erts_debug:set_internal_state/2` unserved | 7 stdlib (`unicode` + `binary_module:copy`) | `erts_debug:set_internal_state(unicode_loop_limit,10)` → typed `unsupported_bif` vs `"Process enabled access"` | `beam_bifs`/`beam_rt` |

D1 is the one that is **not** a typed gap: it breaks the "every gap is a typed error, never a wrong
result" contract and costs 12 rows. It is the single most valuable finding of this pass.

## 5. Exclusions (in neither numerator)

| row | why |
|---|---|
| `cluster_SUITE` + 7 khepri `no_tests` modules (`cth_log_redirect_any_domains`, `export_helper`, `helpers`, `mod_used_for_transactions`, `test_ra_server_helpers`, `prop_path_operations`, `prop_state_machine`) | no eunit tests on **either** side (a `common_test` suite / hook / helper / `proper` module). `0/0` is reported verbatim; counting it as a pass would be a lie. |
| `stored_procs` | the oracle itself fails `crashing_sproc_stacktrace_test` (12/1) on this OTP-29 host — a checkout/host property, not a parity target. Its rust side is `cancelled` on RC-A, so the row currently reads `divergence`; re-measure once RC-A/RC-B close. |
| stdlib `binary_module_SUITE:copy` | oracle needs 11.3 s > the 10 s budget (TIMEOUT); rust stops at 8.97 s on D10's typed gap. Neither side produces a comparable observation — the row is a budget mismatch, not agreement. |
| stdlib `string_SUITE:meas` | both sides need ~118 s; both TIMEOUT at 10 s. |
| stdlib `unicode_SUITE:is_id`, `is_whitespace` | both fail identically with `{badmatch,{error,enoent}}` in `parse_properties/1`: the suite wants `UnicodeData.txt`, which the staged `data_dir` does not carry (the harness runs no `init_per_suite`). |

## 6. What these numbers do not say

- **`cancelled` is not `pass` and not `fail`.** 30 of 43 Khepri modules never executed a single test; their
  1 208 oracle-passing tests are `unknown`. Fixing RC-A alone re-partitions that unknown — expect the
  rust pass count to move for reasons that are not new capability.
- **The host is shared.** The Khepri sweep started at 1-minute loadavg ~170 on 16 logical CPUs; the
  stdlib sweep ran near ~10. `--jobs 4` plus a loaded host is what produced three of the five stdlib
  `TIMEOUT` rows and one `agree-fail-identical` row. Only counts, verdicts and same-host ratios travel.
- **No `common_test` hooks.** The stdlib driver calls `init_per_suite`/`end_per_suite` only; group and
  per-testcase hooks are not executed, so a case that depends on them fails on *both* sides and shows up
  as `agree-fail-identical` rather than as a divergence.
- **Coverage, not conformance.** These rows sample the runtime through real third-party and OTP code;
  they are not the compatibility gate (`tools/differential`, which pins every case against a golden).
  A green row here is evidence, not parity.
