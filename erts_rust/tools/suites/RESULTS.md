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
lives in `results/khepri-notes.md` and `results/stdlib-notes.md`; the current per-class verdicts are
`results/khepri-diagnostics/FINDINGS.md` and `results/stdlib-diagnostics/FINDINGS.md`.
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

Khepri's `rebar3 as test compile` is the only dependency-fetching step (rebar3 is resolved from `$REBAR3`,
`$PATH`, then the Nix store), with `HOME`/`REBAR_CACHE_DIR`/`HEX_HOME`/`XDG_*` redirected under
`$TMPDIR/erts-suites-stage/khepri`; the upstream checkout is never written.
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
| `results/khepri/report.json` | 51 modules | default 300 s / n/a / 4 | `5.49 6.30 6.10` → `3.74 5.45 5.83` | `eunit:test('<module>',[verbose]), halt(0).` |
| `results/stdlib/report.json` | 214 cases (8 suites) | 10 s / 300 s / 4 | `4.63 5.47 5.82` → `5.30 5.59 5.83` | `ct_runner:run('<SUITE>', 10000, 300000, "<stage>/src"), halt(0).` |

| | value |
|---|---|
| host | 16 logical CPUs, **shared** (the loadavg columns above are the honest unit) |
| oracle | the system `erl`/`erlc` on `$PATH` — OTP `29.1.2`, erts `17.1.1` |
| rust runtime | `erts_rust/target/release/beam_rt` (git-ignored, per-worktree), `system_info(version)` → `30/17.1` |
| rust tier | `default` (the native baseline tier; `rust.tier` is `null` in the reports, i.e. no `BEAM_RT_TIER` override) |
| runtime source sha | `8e8e0ad5d96a11931ff6972779a3b5adbe90bfca` |
| khepri source | `2ac09291bac7` (staged `rebar3 as test compile` **reused**, not rebuilt) |

The sha names the workspace commit the runtime binary was built from, not a binary hash — the binary is
git-ignored and per-worktree — and `8e8e0ad5d9` is the current HEAD, so the numbers describe the current
runtime sources directly.

## 2. Khepri — eunit, one row per module

51 candidate modules: **43** run real eunit tests on the oracle (**1 500** passed / **1** failed), **8**
run none (one `common_test` suite, four helpers, two `proper` modules, one hook).
The oracle's single failure is `stored_procs:crashing_sproc_stacktrace_test`, which fails on this OTP-29
host for reasons of its own — and the rust side fails it identically, so the row is
`agree-fail-identical`.
Rust: **1 437** passed / **3** failed.
Verdicts: measured 43 · `agree` 39 · `agree-fail-identical` 9 · `divergence` 2 · `rust-gap` 1 ·
`oracle-only-fail` 0 · `not-run` 0.
The three non-agree rows are attributed in §4: `bang_functions` (**RC-G**, rust-gap),
`keep_while_conditions` (**RC-D**, divergence) and `projections` (**RC-H**, divergence).

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
| `bang_functions` | 85/0 | cancelled 24/0 | rust-gap | RC-G |
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
| `projections` | 134/0 | 133/1 | divergence | RC-H |
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

**Coverage.** Rust executes **1 440 of the oracle's 1 501 tests** (1 437 passed + 3 failed), so **61
oracle tests are unmeasured**.
All 61 sit in `bang_functions`, whose rust cell is `cancelled` after 24 of 85 tests; every other rust row
reports the oracle's own count (`keep_while_conditions` 27 = 26 passed + 1 failed, `projections` 134 =
133 passed + 1 failed).

## 3. OTP stdlib — common_test suites, one row per case

| suite | cases | oracle P/F | rust P/F | agree | divergence | excluded |
|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 18 / 2 | 9 / 11 | 9 | 9 | 2 |
| `dict_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 |
| `lists_SUITE` | 78 | 78 / 0 | 77 / 1 | 77 | 1 | 0 |
| `maps_SUITE` | 32 | 32 / 0 | 25 / 7 | 25 | 7 | 0 |
| `proplists_SUITE` | 4 | 4 / 0 | 4 / 0 | 4 | 0 | 0 |
| `queue_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 |
| `string_SUITE` | 46 | 45 / 1 | 45 / 1 | 45 | 0 | 1 |
| `unicode_SUITE` | 20 | 18 / 2 | 9 / 11 | 9 | 9 | 2 |
| **total** | **214** | **209 / 5** | **183 / 31** | **183** | **26** | **5** |

`dict_SUITE`, `proplists_SUITE` and `queue_SUITE` agree on every case.
The oracle's five failures are `binary_module_SUITE:copy` and `string_SUITE:meas` (both over the 10 s
budget), `unicode_SUITE:is_id`/`is_whitespace` (missing `UnicodeData.txt`) and
`binary_module_SUITE:interesting` (`{badmatch,false}` on both sides).
No row is `not-run` and none is `oracle-only-fail`; the per-case verdicts (with each row's first
divergence) are `results/stdlib/summary.md`.

The main 10 s sweep is the record; the `results/stdlib/recheck-120/` artifact is an earlier-round record
and was **not** re-measured this round, so its numbers are not quoted here.

Three honesty notes this table needs:

**(a) The 10 s budget, not capability, explains 8 of the 26 `divergence` rows.** These read a rust-side
`timeout` at 10 s under `--jobs 4` on a loaded host — `binary_module_SUITE:bin_to_list`,
`binary_module_SUITE:random_ref_fla_comp`, `lists_SUITE:suffix`, `maps_SUITE:t_iterator_2`,
`maps_SUITE:t_put_opt`, `unicode_SUITE:random_lists`, `unicode_SUITE:roundtrips` and
`unicode_SUITE:utf8_illegal_sequences_bif`.
Two further rows are real, pure-performance divergences that stay slow at any budget:
`binary_module_SUITE:random_ref_comp` and `random_ref_sr_comp` (>100×, never a wrong value).
`results/stdlib-diagnostics/FINDINGS.md` §"Sweep rows" is the authority for that split.

**(b) The typed capability gaps among the divergence rows** are `D1` (the trapping map-build
`system_limit`), `D1c` (the placeholder raise reason), `D2` (`binary:referenced_byte_size/1` of a
sub-binary), `D6` (`erlang:bump_reductions/1`) and `D13` (`binary_module_SUITE:parts`).
`results/stdlib-diagnostics/FINDINGS.md` §Verdicts is the authority for each of them and is not restated
here.

**(c) `binary_module_SUITE:copy` and `string_SUITE:meas` are budget artifacts, not divergences.** At the
10 s budget **both** sides time out, so both rows are excluded (agree-fail-identical).

## 4. Divergences, by root-cause class

Every cell below is taken from `results/khepri-diagnostics/FINDINGS.md` (RC-*) and
`results/stdlib-diagnostics/FINDINGS.md` (D*), which re-measured each class against this runtime.

| class | rows | smallest failing expression (`rust` vs `oracle`) | owning crate | verdict |
|---|---|---|---|---|
| **RC-D** (khepri) | 1 | `keep_while_fanout_probe:main/0`, 2 500 dependents deleted: `5 688`–`5 749` ms vs `27/27/28/27` ms (≈210×), `deleted_count` `2 502` both sides | `beam_ets` (the keep-while condition walk) | still present |
| **RC-G** (khepri) | 1 | `seshat:labels_to_binary/1`'s `binary:part(B, 0, byte_size(B) - 1)` during `ra_log_wal` startup → `badarg` (exit 3, 24 passed first) vs 85 passed / 0 failed | `beam_bifs` (`binary:part/3`) | new |
| **RC-H** (khepri) | 1 | `crypto:strong_rand_bytes(16)` in `projection_with_custom_horus_options_test_` → `{nif_not_loaded,module,crypto,…}` vs 16 random bytes | `beam_bifs`/`beam_abi` (the crypto NIF load seam) | new |
| **D1** (stdlib) | 4 | `maps:from_keys(lists:seq(1, 1000000), ok)` — **compiled** → catchable `error:system_limit` vs the map | `beam_rt` heap admission (`enforce_term_heap_limit`) on the trapping map-BIF path | changed class (the lists-fun shape is fixed; the trapping map build remains) |
| **D1c** | 1 | `error_info_abort_probe:main({prefix, 70})` → the parent receives the atom `'exit <structured reason>'` vs `{ok,ok}` | `beam_rt` (`runtime/gc.rs` stabilization + `runner/reason.rs` interning) | still present (the abort is gone; the reason is a placeholder) |
| **D2** | 8 | `binary:part(<<"z",(binary:copy(<<255>>,1024))/binary>>,1,510)` then `referenced_byte_size/1` → `{510,510}` vs `{510,2050}` (parent `{1025,1025}` vs `{1025,2050}`) | `beam_bifs` (`binary:referenced_byte_size/1`) over `beam_term`/`beam_alloc` sub-binary bookkeeping | still present |
| **D6** | 1 | `catch erlang:bump_reductions(134217727)` → `{'EXIT',{unsupported_bif,[…]}}` vs `true` | `beam_bifs` (the `erlang:bump_reductions/1` entry) | still present (a catchable typed gap) |
| **D8** | 0 | `maps:merge_with/3`, 50 rounds × 1 000 keys → **662×/614×**; `maps:to_list/1` control 3.7× | `beam_engine`/`beam_bifs` (the callback-crossing path) | still present |
| **D13** | 1 | `binary_module_SUITE:parts` → `{badmatch,false}` at `'-random_parts/1-lc$^1/1-1-'` | `beam_bifs` (`binary:part/3` error masking) | new |

`D1`'s three `system_limit` rows are `maps_SUITE:t_keys_trapping`/`t_values_trapping`/
`t_from_keys_check_trapping`; its fourth row, `maps_SUITE:t_from_list_check_trapping`, is the same
large-allocation shape surfacing as `{badmatch,false}` in `count_nr_of_yields/2`.
Of the 26 stdlib divergence rows the table accounts for 15 (`D1` 4, `D1c` 1, `D2` 8, `D6` 1, `D13` 1);
the remaining 11 carry no D class — the 8 budget-only timeouts and the 2 pure-performance `random_ref_*`
rows of note (a), plus `maps_SUITE:doctests` (ct_doctest errors).
The classes the FINDINGS mark **closed** own no row here: `RC-A` (the improper-iodata write walk and the
`dets` bytes, `beam_rt`), the three `RC-B` heap-sizing flags (`beam_rt`), `RC-C` for single-closure terms
(`beam_bifs`/`beam_codec`), `RC-E` (the nested-run pump, `beam_rt`), `RC-F` (the stale match-spec
SIGSEGV, `beam_bifs`/`beam_ets`), the `D1` lists-fun `system_limit` shape, `D1b` (the spawned
`error_info` child), `D3` (`erlang:trace/3`), `D4` (map no-op identity), `D5` (the legacy-`catch` BIF
head), `D7` (`system_info(snifs)`), `D9` (`erlang:apply/2`, one degraded `badarity` reason left), `D10`
(`erts_debug:set_internal_state/2`), `D11` (`erlang:trace_delivered/1`), `D12` (the `utf32-little`
segment) and `D12b` (`unicode_SUITE:latin1`) — the gate is what closed them, and re-opening one is a
regression.
**Nothing in this node fixes anything**: these files measure and attribute only, and every owning crate
named above is outside `erts_rust/tools/`.

## 5. Exclusions (in neither numerator)

| row | why |
|---|---|
| khepri `cluster_SUITE`, `cth_log_redirect_any_domains`, `export_helper`, `helpers`, `mod_used_for_transactions`, `test_ra_server_helpers`, `prop_path_operations`, `prop_state_machine` | no eunit tests on **either** side (a `common_test` suite, a hook, four helpers, two `proper` modules). `0/0` is reported verbatim; counting it as a pass would be a lie. |
| khepri `stored_procs` | the oracle itself fails `crashing_sproc_stacktrace_test` (12/1) on this OTP-29 host — a checkout/host property, not a parity target — and the rust side fails it identically (12/1), so the row reads `agree-fail-identical` without a comparable pass. |
| stdlib `binary_module_SUITE:copy` | both sides `TIMEOUT` at the 10 s budget, so this is a budget artifact. |
| stdlib `string_SUITE:meas` | both sides `TIMEOUT` at the 10 s budget (`detail: timeout` on each side): budget-only, and no rust evidence either way. |
| stdlib `unicode_SUITE:is_id`, `is_whitespace` | both fail identically with `{badmatch,{error,enoent}}` in `parse_properties/1`: the suite wants `UnicodeData.txt`, which lives in `lib/stdlib/uc_spec` rather than in the `unicode_SUITE_data` dir the runner resolves, and the suite exports no `init_per_suite` to fetch it. |
| stdlib `binary_module_SUITE:interesting` | both fail identically with `{badmatch,false}` in `do_interesting/1`, so the row says nothing either way. |

These are the five stdlib `agree-fail-identical` rows of §3, the eight khepri `no_tests` rows and khepri
`stored_procs` (also `agree-fail-identical`); no other row is excluded.

## 6. What these numbers do not say

- **`cancelled` and `no_tests` are neither passes nor failures.** One khepri row carries `cancelled`
  (`bang_functions`, 24 of 85 tests passed before a `binary:part/3` `badarg` cut the run short), and
  eight carry `no_tests`. Only a parsed eunit summary produces a count.
- **A partially-run module reports only its prefix.** `bang_functions` printed 24 passing tests before
  eunit cancelled the rest, and the artifact records `24/0`, because the harness counts what eunit
  summarised.
- **No `common_test` group or per-testcase hooks.** The stdlib driver calls `init_per_suite`/`end_per_suite`
  only, so a case that depends on `init_per_group`/`init_per_testcase` fails on *both* sides and lands in
  `agree-fail-identical`, never in a numerator.
- **The host is shared.** `--jobs 4` at a loadavg of ≈4.6–5.8 is what produced the rust-side `TIMEOUT`
  rows at the 10 s budget; only counts and same-host ratios travel, and a `TIMEOUT` is a budget statement.
- **Coverage, not conformance.** These rows sample the runtime through real third-party and OTP code. The
  compatibility gate is `tools/differential`, which pins every case against a golden; a green row here is
  evidence, not parity.
