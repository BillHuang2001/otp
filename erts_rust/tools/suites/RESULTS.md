# Real test suites under both runtimes — measured record

Two targets, each run under the **system ERTS** (oracle) and under the **Rust runtime** (`beam_rt`) with
the *identical* command line:

1. **Khepri** (`/home/bill/Source/khepri`, third-party, read-only) — its eunit suite, one row per module,
   driven with `eunit:test('<module>',[verbose])`.
2. **OTP `stdlib`** (`lib/stdlib/test`, read-only) — eight `common_test` suites, one row per **case**,
   driven by the committed runner `data/ct_runner.erl` (each case in its own `spawn_monitor`, per-case
   timeout, suite budget).

Every figure below is re-derivable from the two committed artifacts `results/khepri/report.json` and
`results/stdlib/report.json` (schema `suites/1`); the rendered `summary.md` next to each one is the
per-row verdict detail and is cited, never duplicated, here.
Per-row attribution (the smallest failing expression for every non-agree row and the crate that owns it)
lives in `results/khepri-diagnostics/FINDINGS.md` and `results/stdlib-diagnostics/FINDINGS.md`.
The host is **shared**: only counts, verdicts and same-host ratios are portable; no absolute wall time
here is a benchmark.

## 0. Reproduce

```
# prerequisites: the oracle `erl`/`erlc` on $PATH (or $ERTS_ORACLE / $ERLC), and a release runtime
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --release

# the harness (a standalone cargo workspace; the read-only $HOME needs a writable CARGO_HOME)
cd erts_rust/tools/suites && CARGO_HOME=${CARGO_HOME:-$HOME/.cache/cargo-home} cargo build
#   ... or `cargo run -- <args>`; $BEAM_RT overrides erts_rust/target/release/beam_rt

# the two committed artifacts
erts_rust/tools/suites/target/debug/suites run --target khepri --jobs 4 --out results/khepri
erts_rust/tools/suites/target/debug/suites run --target stdlib \
    --suite lists_SUITE,maps_SUITE,proplists_SUITE,dict_SUITE,queue_SUITE,binary_module_SUITE,string_SUITE,unicode_SUITE \
    --timeout 10 --suite-budget 300 --jobs 4 --out results/stdlib

# narrow one row: `--module` is an alias of `--suite`, and a larger budget is what turns a
# `timeout` into a statement about the runtime rather than about the host
suites run --target stdlib --suite maps_SUITE --timeout 120 --jobs 1 --out $TMPDIR/one-suite
suites run --target khepri --module keep_while_conditions --out $TMPDIR/one-module
suites list            # the two targets, and whether each side resolves on this host
```

Khepri's `rebar3 as test compile` is the only dependency-fetching step; the staged copy is **reused**
(never rebuilt), so `rebar3` is needed only to satisfy the harness's resolution gate when no rebuild is
requested. It is resolved from `$REBAR3`, `$PATH`, then the Nix store; on this host the Nix-store `rebar3`
was garbage-collected, so `$REBAR3=/home/bill/.mix/elixir/1-20-otp-29/rebar3` (a mix-installed escript)
was set for the run — it is not executed, only recorded in `build.command`.
`HOME`/`REBAR_CACHE_DIR`/`HEX_HOME`/`XDG_*` are redirected under `$TMPDIR/erts-suites-stage/khepri`; the
upstream checkout is never written.
The `-pa` set is identical on both sides — every `_build/test/lib/*/ebin` plus
`_build/test/lib/khepri/test` (11 entries) — and each module runs in its own cwd
(`$TMPDIR/erts-suites-stage/khepri/cwd/<module>`) that the harness **clears before the row starts**
(`target::fresh_dir`), so a `_test.<module>` tree left by an earlier sweep cannot poison either side.
`rebar3 eunit` is never used for the comparison numbers.

The stdlib side is copied into `$TMPDIR/erts-suites-stage/stdlib/src` and compiled there with the
oracle's `erlc`: `targets[0].build` is `erlc -o <stage>/ebin -I <stage>/src <suite>.erl …`
(`ok: true`, `reused: true`, "compiled 8 suites + 6 helpers + runner"), the `-pa` set is `<stage>/ebin` +
`<stage>/ctl` on both sides, and each suite runs in its own cwd (`<stage>/cwd/<SUITE>`) cleared the same
way.

## 1. Both sides

| artifact | rows | budget (`--timeout` / `--suite-budget` / `--jobs`) | loadavg before → after | drive |
|---|---|---|---|---|
| `results/khepri/report.json` | 51 modules | default 300 s / n/a / 4 | `9.62 5.53 2.58` → `8.28 7.25 3.74` | `eunit:test('<module>',[verbose]), halt(0).` |
| `results/stdlib/report.json` | 214 cases (8 suites) | 10 s / 300 s / 4 | `3.35 5.04 3.45` → `2.89 4.34 3.73` | `ct_runner:run('<SUITE>', 10000, 300000, "<stage>/src"), halt(0).` |

| | value |
|---|---|
| host | 16 logical CPUs, **shared** (the loadavg columns above are the honest unit) |
| oracle | the system `erl`/`erlc` on `$PATH` — OTP `29.1.2`, erts `17.1.1` |
| rust runtime | `erts_rust/target/release/beam_rt` (git-ignored, per-worktree), `system_info(version)` → `30/17.1` |
| rust tier | `default` (the native baseline tier; `rust.tier` is `null` in the reports, i.e. no `BEAM_RT_TIER` override) |
| runtime source sha | `e98a5e1e231706a13b103f8c18c218ecb8d0d212` |
| khepri source | `2ac09291bac7` (staged `rebar3 as test compile` **reused**, not rebuilt) |

The sha names the workspace commit the runtime binary was built from, not a binary hash — the binary is
git-ignored and per-worktree — and `e98a5e1e2` is the current HEAD, so the numbers describe the current
runtime sources directly.
This is the closing re-measurement after the correctness round in `beam_rt`/`beam_bifs`/`beam_engine`/
`beam_loader`/`beam_alloc` (the `binary:part` boundary shapes, `referenced_byte_size/1`,
`erlang:bump_reductions/1`, the `-on_load` module hook, map-constructor admission, and a partial GC-rooting
fix).

## 2. Khepri — eunit, one row per module

51 candidate modules: **43** run real eunit tests on the oracle (**1 500** passed / **1** failed), **8**
run none (one `common_test` suite, four helpers, two `proper` modules, one hook).
The oracle's single failure is `stored_procs:crashing_sproc_stacktrace_test`, which fails on this OTP-29
host for reasons of its own — and the rust side fails it identically, so the row is
`agree-fail-identical`.
Rust: **1 497** passed / **4** failed.
Verdicts: measured 43 · `agree` 40 · `agree-fail-identical` 9 · `divergence` 2 · `rust-gap` 0 ·
`oracle-only-fail` 0 · `not-run` 0.
The two divergence rows are attributed in §4: `keep_while_conditions` (**RC-D**, performance) and
`projections` (**RC-H** new shape, crypto export table).
`bang_functions` moved from `rust-gap` (**RC-G**) to **agree** (`85/0` both sides).

| module | sys P/F | rust P/F | verdict | class |
|---|---|---|---|---|
| `advanced_delete` | 33/0 | 33/0 | agree | — |
| `advanced_get` | 13/0 | 13/0 | agree | — |
| `advanced_put` | 40/0 | 40/0 | agree | — |
| `advanced_tx_delete` | 39/0 | 39/0 | agree | — |
| `advanced_tx_get` | 15/0 | 15/0 | agree | — |
| `advanced_tx_put` | 43/0 | 43/0 | agree | — |
| `app` | 36/0 | 36/0 | agree | — |
| `async_option` | 20/0 | 20/0 | agree | — |
| `bang_functions` | 85/0 | 85/0 | agree | — |
| `cluster_SUITE` | no tests | no tests | agree-fail-identical | — |
| `conditions` | 23/0 | 23/0 | agree | — |
| `cth_log_redirect_any_domains` | no tests | no tests | agree-fail-identical | — |
| `db_info` | 24/0 | 24/0 | agree | — |
| `delete_command` | 12/0 | 12/0 | agree | — |
| `display_tree` | 11/0 | 11/0 | agree | — |
| `export` | 56/0 | 56/0 | agree | — |
| `export_erlang` | 63/0 | 63/0 | agree | — |
| `export_helper` | no tests | no tests | agree-fail-identical | — |
| `favor_option` | 5/0 | 5/0 | agree | — |
| `fence` | 6/0 | 6/0 | agree | — |
| `helpers` | no tests | no tests | agree-fail-identical | — |
| `keep_while_conditions` | 27/0 | 26/1 | divergence | RC-D |
| `machine_code_called_from_ra` | 15/0 | 15/0 | agree | — |
| `mod_used_for_transactions` | no tests | no tests | agree-fail-identical | — |
| `non_versioned_commands` | 15/0 | 15/0 | agree | — |
| `path_operations` | 51/0 | 51/0 | agree | — |
| `pattern_tree` | 5/0 | 5/0 | agree | — |
| `prefix_tree` | 4/0 | 4/0 | agree | — |
| `projections` | 134/0 | 132/2 | divergence | RC-H |
| `prop_path_operations` | no tests | no tests | agree-fail-identical | — |
| `prop_state_machine` | no tests | no tests | agree-fail-identical | — |
| `protect_against_dups_option` | 13/0 | 13/0 | agree | — |
| `put_command` | 19/0 | 19/0 | agree | — |
| `queries` | 16/0 | 16/0 | agree | — |
| `return_indirect_deletes_option` | 15/0 | 15/0 | agree | — |
| `root_node` | 22/0 | 22/0 | agree | — |
| `sigils` | 4/0 | 4/0 | agree | — |
| `simple_delete` | 42/0 | 42/0 | agree | — |
| `simple_get` | 94/0 | 94/0 | agree | — |
| `simple_put` | 38/0 | 38/0 | agree | — |
| `simple_tx_delete` | 46/0 | 46/0 | agree | — |
| `simple_tx_get` | 106/0 | 106/0 | agree | — |
| `simple_tx_misc` | 45/0 | 45/0 | agree | — |
| `simple_tx_put` | 47/0 | 47/0 | agree | — |
| `stored_procs` | 12/1 | 12/1 | agree-fail-identical | — |
| `test_ra_server_helpers` | no tests | no tests | agree-fail-identical | — |
| `timeout` | 6/0 | 6/0 | agree | — |
| `triggers` | 118/0 | 118/0 | agree | — |
| `tx_funs` | 42/0 | 42/0 | agree | — |
| `tx_sprocs` | 33/0 | 33/0 | agree | — |
| `utils` | 7/0 | 7/0 | agree | — |

**Coverage.** Rust executes **1 501 of the oracle's 1 501 tests** (1 497 passed + 4 failed); no oracle
test is unmeasured any more — `bang_functions` runs to completion (85/85) instead of cancelling at 24.
The four rust failures are `stored_procs` (1, `agree-fail-identical`, the oracle's own failure),
`keep_while_conditions` (1, RC-D) and `projections` (2, RC-H new shape).

## 3. OTP stdlib — common_test suites, one row per case

| suite | cases | oracle P/F | rust P/F | agree | divergence | excluded |
|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 18 / 2 | 10 / 10 | 10 | 8 | 2 |
| `dict_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 |
| `lists_SUITE` | 78 | 78 / 0 | 77 / 1 | 77 | 1 | 0 |
| `maps_SUITE` | 32 | 32 / 0 | 26 / 6 | 26 | 6 | 0 |
| `proplists_SUITE` | 4 | 4 / 0 | 4 / 0 | 4 | 0 | 0 |
| `queue_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 |
| `string_SUITE` | 46 | 45 / 1 | 45 / 1 | 45 | 0 | 1 |
| `unicode_SUITE` | 20 | 18 / 2 | 6 / 3 | 6 | 5 | 9 |
| **total** | **214** | **209 / 5** | **182 / 21** | **182** | **20** | **12** |

The stdlib total is `agree 182 · divergence 20 · rust-gap 9 · agree-fail-identical 3` (the `excluded`
column above folds `agree-fail-identical` and `rust-gap`).
`dict_SUITE`, `proplists_SUITE` and `queue_SUITE` agree on every case; `binary_module_SUITE` and
`unicode_SUITE` carry the only remaining `divergence` rows.
The oracle's five failures are `binary_module_SUITE:copy` and `string_SUITE:meas` (both over the 10 s
budget), `unicode_SUITE:is_id`/`is_whitespace` (missing `UnicodeData.txt`) and
`binary_module_SUITE:interesting` (`{badmatch,false}` on both sides).
No row is `not-run` and none is `oracle-only-fail`; the per-case verdicts (with each row's first
divergence) are `results/stdlib/summary.md`.

The main 10 s sweep is the record. Three honesty notes this table needs:

**(a) The 10 s budget, not capability, explains most of the `divergence` rows.** The rust-side `timeout`
rows at 10 s under `--jobs 4` are `binary_module_SUITE:bin_to_list`, `random_ref_fla_comp`, `lists_SUITE:suffix`,
`maps_SUITE:t_iterator_2` (budget-only, both pass standalone) and `binary_module_SUITE:random_ref_comp`,
`random_ref_sr_comp` (>100×, never a wrong value).
`results/stdlib-diagnostics/FINDINGS.md` §"Sweep rows" is the authority for that split.

**(b) The typed capability gaps among the divergence rows** are `D1c` (the placeholder raise reason in
`binary_module_SUITE:error_info` — the **only** row that still fails for a result) and the `D1`
trapping-yield shape (`maps_SUITE:t_keys_trapping`/`t_from_keys_check_trapping`/`t_from_list_check_trapping`,
`{badmatch,false}` on the yield count, not a wrong map).
`binary_module_SUITE:parts` (D13) now runs past its old `{badmatch,false}` and times out, so D13 is closed
for correctness and performance-bound here.
`results/stdlib-diagnostics/FINDINGS.md` §Verdicts is the authority for each and is not restated here.

**(c) `binary_module_SUITE:copy` and `string_SUITE:meas` are budget artifacts, not divergences.** At the
10 s budget **both** sides time out, so both rows are excluded (`agree-fail-identical`).

**(d) The `unicode_SUITE` budget expired this round.** Because the `referenced_byte_size` (D2) fix turned
fast-fail rows into slow-but-running rows, 11 `unicode_SUITE` cases were **not measured** at the
10 s / 300 s setting (they read `rust-gap` "the suite budget expired"), including `is_id`/`is_whitespace`
which previously read `agree-fail-identical`. This is a budget artifact, not a result; re-checking
`unicode_SUITE` at a larger budget separates the perf rows from the still-divergent `utf16_little`/`utf32_little`
D2-residual rows.

## 4. Divergences, by root-cause class

Every cell below is taken from `results/khepri-diagnostics/FINDINGS.md` (RC-*) and
`results/stdlib-diagnostics/FINDINGS.md` (D*), which re-measured each class against this runtime.

| class | rows | smallest failing expression (`rust` vs `oracle`) | owning crate | verdict |
|---|---|---|---|---|
| **RC-D** (khepri) | 1 | 2 500 keep-while dependents deleted: `5 688`–`5 749` ms vs `27/27/28/27` ms (≈210×), `deleted_count` `2 502` both sides | `beam_ets` (the keep-while condition walk) | still present (performance) |
| **RC-H** (khepri) | 2 | `crypto:strong_rand_bytes/1` through horus at "Register the projection" → `throw:{horus,call_to_unexported_function,#{mfa => {crypto,strong_rand_bytes,1}}}` vs 134/0 | `beam_rt`/`beam_bifs` (the crypto `-on_load`/NIF export registration order) | changed class (the `nif_not_loaded` shape is fixed; the export table is the new shape) |
| **D1** (stdlib) | 3 | `count_nr_of_yields > 2` for a 1M-element `maps:from_keys`/`from_list`/`keys` → `{badmatch,false}` (≤ 2 yields) vs `> 2` | `beam_bifs`/`beam_engine` (the trapping map-BIF yield/trace funnel) | changed class (both `system_limit` shapes are fixed; the yield count remains) |
| **D1c** | 1 | `error_info_abort_probe:main({prefix, 70})` → the parent receives `'exit <structured reason>'` vs `{ok,ok}` | `beam_engine` (`bif_frame.rs::carve`, unrooted shared register words) | still present (correctness) |
| **D2** | 2 | `binary:part(B, 1, 510)` sub-binary `referenced_byte_size` → `{510,510}` vs `{510,1025}` | `beam_bifs`/`beam_term`/`beam_alloc` (sub-binary bookkeeping) | mostly fixed (whole-binary + several sub-binary shapes agree; one `binary:part/3`-of-refc-parent shape remains) |
| **D8** | 1 | `maps:merge_with/3`, 50 rounds × 1 000 keys → **662×/614×**; `to_list` control 3.7× | `beam_engine`/`beam_bifs` (the callback-crossing path) | still present (performance) |

The classes the FINDINGS mark **closed** own no divergence row here: `RC-A` (iodata/dets), the three
`RC-B` heap-sizing flags, `RC-C` single-closure encodings, `RC-E` (the nested-run pump), `RC-F` (the stale
match-spec SIGSEGV), `RC-G` (`binary:part/3` in `bang_functions` — now `85/0`), the `D1` `lists`-fun and
map-build `system_limit` shapes, `D1b`, `D3`, `D4`, `D5`, `D6` (`bump_reductions/1`), `D7`, `D9`, `D10`,
`D11`, `D12`, `D12b` and `D13` (correctness) — the gate is what closed them, and re-opening one is a
regression.
Two side observations from `khepri-diagnostics` remain open but own **no** sweep row: the `-eval` process's
birth `trap_exit` (`RC-B`) and multi-closure `term_to_binary`/`external_size` (`RC-C`).
**Nothing in this node fixes anything**: these files measure and attribute only, and every owning crate
named above is outside `erts_rust/tools/`.

## 5. Exclusions (in neither numerator)

| row | why |
|---|---|
| khepri `cluster_SUITE`, `cth_log_redirect_any_domains`, `export_helper`, `helpers`, `mod_used_for_transactions`, `test_ra_server_helpers`, `prop_path_operations`, `prop_state_machine` | no eunit tests on **either** side (a `common_test` suite, a hook, four helpers, two `proper` modules). `0/0` is reported verbatim; counting it as a pass would be a lie. |
| khepri `stored_procs` | the oracle itself fails `crashing_sproc_stacktrace_test` (12/1) on this OTP-29 host — a checkout/host property, not a parity target — and the rust side fails it identically (12/1), so the row reads `agree-fail-identical` without a comparable pass. |
| stdlib `binary_module_SUITE:copy` | both sides `TIMEOUT` at the 10 s budget, so this is a budget artifact. |
| stdlib `string_SUITE:meas` | both sides `TIMEOUT` at the 10 s budget (`detail: timeout` on each side): budget-only, and no rust evidence either way. |
| stdlib `binary_module_SUITE:interesting` | both fail identically with `{badmatch,false}` in `do_interesting/1`, so the row says nothing either way. |
| stdlib 11 `unicode_SUITE` rows | **not measured** this round: the 300 s suite budget expired after the D2 fix turned fast-fail rows into slow-but-running rows (`rust-gap` "the suite budget expired"), including `is_id`/`is_whitespace` which previously read `agree-fail-identical`. |

These are the three stdlib `agree-fail-identical` rows of §3, the nine stdlib `rust-gap` rows (all the
unicode budget expiry), the eight khepri `no_tests` rows and khepri `stored_procs` (also
`agree-fail-identical`).

## 6. What these numbers do not say

- **`cancelled` and `no_tests` are neither passes nor failures.** Eight khepri rows carry `no_tests`; no
  row is `cancelled` this round (the prior `bang_functions` cancellation is gone). Only a parsed eunit
  summary produces a count.
- **No `common_test` group or per-testcase hooks.** The stdlib driver calls `init_per_suite`/`end_per_suite`
  only, so a case that depends on `init_per_group`/`init_per_testcase` fails on *both* sides and lands in
  `agree-fail-identical`, never in a numerator.
- **The host is shared.** `--jobs 4` on a loaded host is what produces the rust-side `TIMEOUT` rows at the
  10 s budget; only counts and same-host ratios travel, and a `TIMEOUT` is a budget statement.
- **Coverage, not conformance.** These rows sample the runtime through real third-party and OTP code. The
  compatibility gate is `tools/differential`, which pins every case against a golden; a green row here is
  evidence, not parity.
