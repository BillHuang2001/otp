# Real test suites under both runtimes — measured record

Two targets, each run under the **system ERTS** (oracle) and under the **Rust runtime** (`beam_rt`) with
the *identical* command line:

1. **Khepri** (`/home/bill/Source/khepri`, third-party, read-only) — its eunit suite, one row per module,
   driven with `eunit:test('<module>',[verbose])`.
2. **OTP `stdlib`** (`lib/stdlib/test`, read-only) — eight `common_test` suites, one row per **case**,
   driven by the committed runner `data/ct_runner.erl` (each case in its own `spawn_monitor`, per-case
   timeout, suite budget).

Every figure below is re-derivable from the three committed artifacts `results/khepri/report.json`,
`results/stdlib/report.json` and the stdlib budget re-check `results/stdlib/recheck-120/report.json`
(schema `suites/1`); the rendered `summary.md` next to each one is the per-row verdict detail and is
cited, never duplicated, here.
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

# the three committed artifacts
erts_rust/tools/suites/target/debug/suites run --target khepri --jobs 4 --out results/khepri
erts_rust/tools/suites/target/debug/suites run --target stdlib \
    --suite lists_SUITE,maps_SUITE,proplists_SUITE,dict_SUITE,queue_SUITE,binary_module_SUITE,string_SUITE,unicode_SUITE \
    --timeout 10 --suite-budget 300 --jobs 4 --out results/stdlib
erts_rust/tools/suites/target/debug/suites run --target stdlib \
    --suite maps_SUITE,lists_SUITE,binary_module_SUITE \
    --timeout 120 --suite-budget 900 --jobs 1 --out results/stdlib/recheck-120

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
way. The committed re-check report's own `run_command` still records the temporary `--out` it was written
with; the report itself is the committed one.

## 1. Both sides

| artifact | rows | budget (`--timeout` / `--suite-budget` / `--jobs`) | loadavg before → after | drive |
|---|---|---|---|---|
| `results/khepri/report.json` | 51 modules | default 300 s / n/a / 4 | `2.76 6.12 18.63` → `3.98 5.95 17.43` | `eunit:test('<module>',[verbose]), halt(0).` |
| `results/stdlib/report.json` | 214 cases (8 suites) | 10 s / 300 s / 4 | `3.98 5.95 17.43` → `6.28 6.20 16.28` | `ct_runner:run('<SUITE>', 10000, 300000, "<stage>/src"), halt(0).` |
| `results/stdlib/recheck-120/report.json` | 130 cases (3 suites) | 120 s / 900 s / 1 | `6.28 6.20 16.28` → `4.72 5.49 12.15` | same shape, `120000` / `900000` |

| | value |
|---|---|
| host | 16 logical CPUs, **shared** (the loadavg columns above are the honest unit) |
| oracle | the system `erl`/`erlc` on `$PATH` — OTP `29.1.2`, erts `17.1.1` |
| rust runtime | `erts_rust/target/release/beam_rt` (git-ignored, per-worktree), `system_info(version)` → `30/17.1` |
| rust tier | `default` (the native baseline tier; `rust.tier` is `null` in the reports, i.e. no `BEAM_RT_TIER` override) |
| runtime source sha | `71ffbbb37971d0b193e0dc74b60b6c448874349e` |
| khepri source | `2ac09291bac7` (staged `rebar3 as test compile` **reused**, not rebuilt) |

The sha names the workspace commit the runtime binary was built from, not a binary hash — the binary is
git-ignored and per-worktree — and no file under `erts_rust/crates` differs between it and HEAD
(`git diff --stat 71ffbbb379 HEAD -- erts_rust/crates` is empty), so the numbers describe the current
runtime sources.
The reports' own `rust.sha` field records the *harness* checkout's HEAD at sweep time
(`e18f151b…`), which differs from `71ffbbb379` only in this node's committed artifacts — again, nothing
under `erts_rust/crates`.

## 2. Khepri — eunit, one row per module

51 candidate modules: **43** run real eunit tests on the oracle (**1 500** passed / **1** failed), **8**
run none (one `common_test` suite, four helpers, two `proper` modules, one hook).
The oracle's single failure is `stored_procs:crashing_sproc_stacktrace_test`, which fails on this OTP-29
host for reasons of its own.
Rust: **403** passed / **1** failed.
Verdicts: measured 43 · `agree` 21 · `agree-fail-identical` 8 · `divergence` 2 · `rust-gap` 20 ·
`oracle-only-fail` 0 · `not-run` 0.
Every one of the 20 `rust-gap` rows is one of two runtime defects — 19 block on the nested-run pump
(**RC-E**) and 1 dies by SIGSEGV on a stale compiled match spec (**RC-F**) — and both `divergence` rows
are attributed in §4 (**RC-D** for the rust failure, RC-E for `stored_procs`' rust cell).

| module | sys P/F | rust P/F | verdict | class |
|---|---|---|---|---|
| `advanced_delete` | 33/0 | unsupported | rust-gap | RC-E |
| `advanced_get` | 13/0 | 13/0 | agree | — |
| `advanced_put` | 40/0 | unsupported | rust-gap | RC-E |
| `advanced_tx_delete` | 39/0 | error | rust-gap | RC-F |
| `advanced_tx_get` | 15/0 | 15/0 | agree | — |
| `advanced_tx_put` | 43/0 | unsupported | rust-gap | RC-E |
| `app` | 36/0 | 36/0 | agree | — |
| `async_option` | 20/0 | unsupported | rust-gap | RC-E |
| `bang_functions` | 85/0 | unsupported | rust-gap | RC-E |
| `cluster_SUITE` | no tests | no tests | agree-fail-identical | — |
| `conditions` | 23/0 | 23/0 | agree | — |
| `cth_log_redirect_any_domains` | no tests | no tests | agree-fail-identical | — |
| `db_info` | 24/0 | unsupported | rust-gap | RC-E |
| `delete_command` | 12/0 | 12/0 | agree | — |
| `display_tree` | 11/0 | 11/0 | agree | — |
| `export` | 56/0 | unsupported | rust-gap | RC-E |
| `export_erlang` | 63/0 | unsupported | rust-gap | RC-E |
| `export_helper` | no tests | no tests | agree-fail-identical | — |
| `favor_option` | 5/0 | unsupported | rust-gap | RC-E |
| `fence` | 6/0 | 6/0 | agree | — |
| `helpers` | no tests | no tests | agree-fail-identical | — |
| `keep_while_conditions` | 27/0 | 26/1 | divergence | RC-D |
| `machine_code_called_from_ra` | 15/0 | 15/0 | agree | — |
| `mod_used_for_transactions` | no tests | no tests | agree-fail-identical | — |
| `non_versioned_commands` | 15/0 | 15/0 | agree | — |
| `path_operations` | 51/0 | 51/0 | agree | — |
| `pattern_tree` | 5/0 | 5/0 | agree | — |
| `prefix_tree` | 4/0 | 4/0 | agree | — |
| `projections` | 134/0 | unsupported | rust-gap | RC-E |
| `prop_path_operations` | no tests | no tests | agree-fail-identical | — |
| `prop_state_machine` | no tests | no tests | agree-fail-identical | — |
| `protect_against_dups_option` | 13/0 | 13/0 | agree | — |
| `put_command` | 19/0 | 19/0 | agree | — |
| `queries` | 16/0 | 16/0 | agree | — |
| `return_indirect_deletes_option` | 15/0 | unsupported | rust-gap | RC-E |
| `root_node` | 22/0 | 22/0 | agree | — |
| `sigils` | 4/0 | 4/0 | agree | — |
| `simple_delete` | 42/0 | 42/0 | agree | — |
| `simple_get` | 94/0 | unsupported | rust-gap | RC-E |
| `simple_put` | 38/0 | unsupported | rust-gap | RC-E |
| `simple_tx_delete` | 46/0 | unsupported | rust-gap | RC-E |
| `simple_tx_get` | 106/0 | unsupported | rust-gap | RC-E |
| `simple_tx_misc` | 45/0 | unsupported | rust-gap | RC-E |
| `simple_tx_put` | 47/0 | unsupported | rust-gap | RC-E |
| `stored_procs` | 12/1 | unsupported | divergence | RC-E (rust cell) |
| `test_ra_server_helpers` | no tests | no tests | agree-fail-identical | — |
| `timeout` | 6/0 | 6/0 | agree | — |
| `triggers` | 118/0 | unsupported | rust-gap | RC-E |
| `tx_funs` | 42/0 | 42/0 | agree | — |
| `tx_sprocs` | 33/0 | unsupported | rust-gap | RC-E |
| `utils` | 7/0 | 7/0 | agree | — |

**Coverage.** Rust executes **404 of the oracle's 1 501 tests** (403 passed + 1 failed), so **1 097 oracle
tests are unmeasured**.
All 1 097 sit in the 21 modules whose rust cell carries no counts: the 19 RC-E rows, RC-F's
`advanced_tx_delete`, and `stored_procs`.
No module loses tests after it starts — every rust row that reports counts reports the oracle's own count
(`keep_while_conditions` 27 = 26 passed + 1 failed).

**Run-to-run variance.** Two sweeps of this same runtime source are committed. The artifact above reads
rust `403`/`1` with **21** `agree`; the earlier one, `git show
9e860feef6:erts_rust/tools/suites/results/khepri/report.json`, reads rust `412`/`1` with **20** `agree` —
9 tests and one verdict apart, produced by RC-E's intermittency, not by a capability difference.
Passes in the earlier run only: `return_indirect_deletes_option` 15/0, `simple_tx_delete` 46/0,
`tx_sprocs` 33/0.
Passes in the committed run only: `advanced_get` 13/0, `advanced_tx_get` 15/0,
`machine_code_called_from_ra` 15/0, `simple_delete` 42/0.
Both artifacts are committed (the earlier one by sha, since the path holds one report), so a single khepri
sweep is a *sample* of the same runtime, not a fixed number.

## 3. OTP stdlib — common_test suites, one row per case

| suite | cases | oracle P/F | rust P/F | agree | divergence | excluded |
|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 18 / 2 | 9 / 11 | 9 | 9 | 2 |
| `dict_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 |
| `lists_SUITE` | 78 | 78 / 0 | 77 / 1 | 77 | 1 | 0 |
| `maps_SUITE` | 32 | 32 / 0 | 20 / 12 | 20 | 12 | 0 |
| `proplists_SUITE` | 4 | 4 / 0 | 4 / 0 | 4 | 0 | 0 |
| `queue_SUITE` | 7 | 7 / 0 | 7 / 0 | 7 | 0 | 0 |
| `string_SUITE` | 46 | 45 / 1 | 45 / 1 | 45 | 0 | 1 |
| `unicode_SUITE` | 20 | 18 / 2 | 8 / 12 | 8 | 10 | 2 |
| **total** | **214** | **209 / 5** | **177 / 37** | **177** | **32** | **5** |

`dict_SUITE`, `proplists_SUITE` and `queue_SUITE` agree on every case.
The oracle's five failures are `binary_module_SUITE:copy` and `string_SUITE:meas` (both over the 10 s
budget), `unicode_SUITE:is_id`/`is_whitespace` (missing `UnicodeData.txt`) and
`binary_module_SUITE:interesting` (`{badmatch,false}` on both sides).
No row is `not-run` and none is `oracle-only-fail`; the per-case verdicts (with each row's first
divergence) are `results/stdlib/summary.md`.

The budget re-check, same runtime, one suite process at a time:

| suite | cases | oracle P/F | rust P/F | agree | divergence | excluded |
|---|---|---|---|---|---|---|
| `binary_module_SUITE` | 20 | 19 / 1 | 13 / 7 | 13 | 6 | 1 |
| `lists_SUITE` | 78 | 78 / 0 | 78 / 0 | 78 | 0 | 0 |
| `maps_SUITE` | 32 | 32 / 0 | 23 / 9 | 23 | 9 | 0 |
| **total** | **130** | **129 / 1** | **114 / 16** | **114** | **15** | **1** |

Three honesty notes this table needs:

**(a) The 10 s budget, not capability, explains most of the 32 `divergence` rows.** 13 of them are a
rust-side `timeout` at 10 s under `--jobs 4` on a loaded host.
The re-check covers 3 of the 8 suites (130 of the 214 rows) and there 7 of its 22 divergence rows become
`agree` — `binary_module_SUITE:bin_to_list`, `parts`, `random_ref_fla_comp`, `lists_SUITE:suffix`,
`maps_SUITE:t_intersect`, `t_iterator_2`, `t_merge_with` — and `binary_module_SUITE:copy` moves out of the
excluded set (§5).
Two rows are still slow at 120 s: `binary_module_SUITE:random_ref_comp` and `random_ref_sr_comp` (rust
still running at >400 s standalone against the oracle's 4 s / 3 s — a >100× performance gap, never a wrong
value).
Two further rows that timed out at 10 s turn out to be typed `system_limit` raises at 120 s
(`maps_SUITE:t_keys_trapping`, `t_values_trapping`, class D1), and the two `unicode_SUITE` timeouts were
never re-checked.

**(b) The typed capability gaps among the divergence rows** are `D1` (`system_limit`),
`D2` (`binary:referenced_byte_size/1` of a sub-binary), `D5` (the legacy-`catch` head frame), `D6`
(`erlang:bump_reductions/1`), `D11` (`erlang:trace_delivered/1` after the tracee is gone), `D12`
(`<<E/utf32-little>>` from a runtime value) and `D1c`'s placeholder reason.
`results/stdlib-diagnostics/FINDINGS.md` §Verdicts is the authority for each of them and is not restated
here.

**(c) `binary_module_SUITE:copy` is a budget artifact, not a divergence.** At the 10 s budget **both**
sides time out, so the row is excluded; at 120 s **both** sides pass it.

## 4. Divergences, by root-cause class

Every cell below is taken from `results/khepri-diagnostics/FINDINGS.md` (RC-*) and
`results/stdlib-diagnostics/FINDINGS.md` (D*), which re-measured each class against this runtime.

| class | rows | smallest failing expression (`rust` vs `oracle`) | owning crate | verdict |
|---|---|---|---|---|
| **RC-E** (khepri) | 20 (19 `rust-gap` + `stored_procs`' rust cell) | `eunit:test('simple_get',[verbose])` in the `-eval` process → `beam_rt: unsupported: opcode: run: process blocked (no message available)`, exit 3, vs `All 94 tests passed` | `beam_rt` (the pump: `context/pump.rs::pump_once`, `runtime/scheduler.rs::pump_scheduler_boot`); the text is `beam_engine::error::blocked` | still present |
| **RC-F** (khepri) | 1 | `eunit:test('advanced_tx_delete',[verbose])` → SIGSEGV, exit 139, vs 39 passed / 0 failed | `beam_bifs` (`ets::insert_spec`/`spec_raw` stores a caller-heap alias) + `beam_ets` (`select::list_elements` dereferences a non-list word) | still present |
| **RC-D** (khepri) | 1 | `keep_while_fanout_probe:main/0`, 2 500 dependents deleted: `5 688`–`5 749` ms vs `27/27/28/27` ms (≈210×), `deleted_count` `2 502` both sides | `beam_ets` (the keep-while condition walk) | still present |
| **D1** (stdlib) | 3 | `lists:foreach(fun(_) -> length(lists:duplicate(N,x)) end,[1])` at `N ≥ 65537` → `error:system_limit` vs `ok` | `beam_rt` heap admission (`enforce_term_heap_limit`) | still present |
| **D1c** | 1 | `error_info_abort_probe:main({prefix,70})` → the parent receives the atom `'exit <structured reason>'` vs `{ok,ok}` | `beam_rt` (`runtime/gc.rs` stabilization + `runner/reason.rs` interning) | changed class (the abort is gone; the reason is a placeholder) |
| **D2** | 6 | `binary:part(<<"z",(binary:copy(<<255>>,1024))/binary>>,1,510)` then `referenced_byte_size/1` → `{510,510}` vs `{510,1025}` | `beam_bifs` (`binary:referenced_byte_size/1`) over `beam_term`/`beam_alloc` sub-binary bookkeeping | still present |
| **D4** | 0 | compiled `erts_debug:same(maps:put(a,1,M),M)` → `false` vs `true` | `beam_engine` (`families/maps`, `terms::make_map_rooted`) | still present (no suite row) |
| **D5** | 4 | legacy `catch maps:with([a,b], (fun(X) -> X end)(a))` head → `{d5_probe,legacy_catch,0,…}` vs `{maps,with,[[a,b],a],…}` | `beam_engine` (`families/exceptions/callee.rs`) | partially present (`try`/`catch` agrees) |
| **D6** | 2 | `catch erlang:bump_reductions(134217727)` → `{'EXIT',{unsupported_bif,[…]}}` vs `true` | `beam_bifs` (the `erlang:bump_reductions/1` entry) | changed class (a catchable typed gap) |
| **D8** | 0 at these budgets (2 at 10 s) | `maps:merge_with/3`, 50 rounds × 1 000 keys → **662×/614×**; `maps:to_list/1` control 3.7× | `beam_engine`/`beam_bifs` (the callback-crossing path) | still present |
| **D11** | 1 | `erlang:trace_delivered(P)` after `{'DOWN',…}` → `{'EXIT',{badarg,…}}` vs `#Ref<…>` | `beam_rt` (`src/trace.rs::trace_delivered_1`) | still present |
| **D12** | 3 | `f(E) -> <<E/utf32-little>>.` called `f(1)` → `<<0,0,0,1>>` vs `<<1,0,0,0>>` | `beam_engine` (`families/bitstrings/create`) | still present (new class) |

`D12b` (`unicode:characters_to_binary/3`'s latin1 error split, `unicode_SUITE:latin1`, 1 row, owner not yet
attributed) is the one further open class in the stdlib FINDINGS.
The remaining 11 of the 32 stdlib divergence rows carry no class at all: 9 are budget-only (§3a) and 2 are
the pure-performance `random_ref_*` rows above.
The classes the FINDINGS mark **closed** own no row here: `RC-A` (the improper-iodata write walk and the
`dets` bytes, `beam_rt`), the three `RC-B` heap-sizing flags (`beam_rt`), `RC-C` for single-closure terms
(`beam_bifs`/`beam_codec`), `D1b` (the spawned `error_info` child), `D3` (`erlang:trace/3`), `D7`
(`system_info(snifs)`), `D9` (`erlang:apply/2`, one degraded `badarity` reason left) and `D10`
(`erts_debug:set_internal_state/2`) — the gate is what closed them, and re-opening one is a regression.
**Nothing in this node fixes anything**: these files measure and attribute only, and every owning crate
named above is outside `erts_rust/tools/`.

## 5. Exclusions (in neither numerator)

| row | why |
|---|---|
| khepri `cluster_SUITE`, `cth_log_redirect_any_domains`, `export_helper`, `helpers`, `mod_used_for_transactions`, `test_ra_server_helpers`, `prop_path_operations`, `prop_state_machine` | no eunit tests on **either** side (a `common_test` suite, a hook, four helpers, two `proper` modules). `0/0` is reported verbatim; counting it as a pass would be a lie. |
| khepri `stored_procs` | the oracle itself fails `crashing_sproc_stacktrace_test` (12/1) on this OTP-29 host — a checkout/host property, not a parity target — and its rust cell is the RC-E block, so the row reads `divergence` without a comparable measurement. |
| stdlib `binary_module_SUITE:copy` | both sides `TIMEOUT` at the 10 s budget; **both** sides pass at 120 s, so this is a budget artifact. |
| stdlib `string_SUITE:meas` | both sides `TIMEOUT` at the 10 s budget (`detail: timeout` on each side) and both still do when measured standalone: budget-only, and no rust evidence either way. |
| stdlib `unicode_SUITE:is_id`, `is_whitespace` | both fail identically with `{badmatch,{error,enoent}}` in `parse_properties/1`: the suite wants `UnicodeData.txt`, which lives in `lib/stdlib/uc_spec` rather than in the `unicode_SUITE_data` dir the runner resolves, and the suite exports no `init_per_suite` to fetch it. |
| stdlib `binary_module_SUITE:interesting` | both fail identically with `{badmatch,false}` in `do_interesting/1`, so the row says nothing either way. |

These are the five stdlib `agree-fail-identical` rows of §3 and the eight khepri `no_tests` rows, plus
`stored_procs`; no other row is excluded.

## 6. What these numbers do not say

- **`unsupported`, `error` and `no_tests` are neither passes nor failures.** 20 khepri rows carry
  `unsupported` (a typed capability gap, exit 3), one carries `error` (a SIGSEGV, no parseable summary)
  and eight carry `no_tests`. Only a parsed eunit summary produces a count.
- **A partially-run module reports only its prefix.** `advanced_tx_delete` printed two passing tests before
  it died, and the artifact still records `0/0`, because the harness counts what eunit summarised.
- **No `common_test` group or per-testcase hooks.** The stdlib driver calls `init_per_suite`/`end_per_suite`
  only, so a case that depends on `init_per_group`/`init_per_testcase` fails on *both* sides and lands in
  `agree-fail-identical`, never in a numerator.
- **The host is shared.** `--jobs 4` at a loadavg of 4–6 (and up to ~12 during the diagnostics) is what
  produced the rust-side `TIMEOUT` rows at the 10 s budget; only counts and same-host ratios travel, and a
  `TIMEOUT` is a budget statement.
- **Coverage, not conformance.** These rows sample the runtime through real third-party and OTP code. The
  compatibility gate is `tools/differential`, which pins every case against a golden; a green row here is
  evidence, not parity.
