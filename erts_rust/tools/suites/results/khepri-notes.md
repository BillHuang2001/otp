# Khepri eunit parity — sweep notes

Sweep of the **Khepri** eunit suite (`/home/bill/Source/khepri`, third-party, read-only) under the
system ERTS (oracle) and under the Rust runtime (`beam_rt`), driven by this crate (`suites run
--target khepri`). Raw evidence: `khepri/report.json` (schema `suites/1`) and `khepri/summary.md`.
This file adds what the report cannot carry: the root cause of every row that is not `agree`, each
with the smallest failing expression, and the diagnostic that produced it.

**Headline:** 51 candidate modules, `1500/1` sys tests, `285/8` rust tests. **No disagreement of
the "wrong result" kind was found in a test that actually ran to completion** except
`display_tree` (ETF local-fun encoding, RC-C). 29 of the 31 non-agreeing rows are blocked by **two
runtime defects in the module setup path** (`dets`' file write, then `process_flag/2`), one is a
**~250–330× performance gap** in a delete fan-out (RC-D), and one is the pre-existing
oracle-side failure in `stored_procs`.

> **Re-verified since:** the RC-A/RC-B/RC-C/RC-D classes were re-measured against the current
> runtime — see `khepri-diagnostics/FINDINGS.md`, which supersedes the per-class tables below
> where they disagree. Current verdicts: **RC-A fixed** (improper-iodata writes and the `dets`
> file are byte-identical to the oracle), **RC-B still present** (`min_heap_size`,
> `min_bin_vheap_size`, `fullsweep_after` still `badarg`, and it is now the first wall — the
> `ra` fixture dies at `ra_log_wal:init/1` line 313), **RC-C still present** (unchanged class),
> **RC-D still present** (≈170–420× on the same-host ratio; the module's `DeletionTime < 1000`
> assertion still fails).

## Environment

| item | value |
|---|---|
| oracle | `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` — `29/17.1.1` (system `erl`, never the in-tree C build) |
| rust runtime | `erts_rust/target/release/beam_rt` — `30/17.1`, default (baseline-JIT) tier |
| rust workspace commit | `ec44fa3f7fc7d8419ae32f11b23a06448b565ec7` (the commit this worktree is branched from) |
| khepri source | `/home/bill/Source/khepri` at `2ac09291bac7c2fee099380ec1e26d8c9452f776` |
| staged build | `/tmp/erts-suites-stage/khepri/proj`, `rebar3 3.27.1 as test compile` — **reused** a completed build, not rebuilt |
| code path | 11 entries: every `_build/test/lib/*/ebin` + `_build/test/lib/khepri/test` (identical on both sides) |
| drive | `eunit:test('<module>',[verbose]), halt(0).` (identical on both sides) |
| `--jobs` | 4 (each module its own OS process, its own cwd) |
| host loadavg before / after | `169.99 99.02 63.78 166/1773` / `91.59 90.88 63.41 13/1065` on 16 logical CPUs — **shared, heavily loaded** |

### Commands run

```
# build (harness + runtime)
CARGO_HOME=$HOME/.cache/cargo-home cargo build --manifest-path erts_rust/tools/suites/Cargo.toml
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --release

# the sweep behind the committed artifacts
erts_rust/tools/suites/target/debug/suites run --target khepri --jobs 4 \
    --out erts_rust/tools/suites/results/khepri

# attribution: every rust-gap/divergence module re-run serially, both in the plain state and with
# the RC-A workaround (see "Diagnostics"), using the report's own recorded invocation shape
#   <beam_rt> -noshell -pa <11 entries> -eval "eunit:test('<module>',[verbose]), halt(0)."
```

Re-running the modules **serially** (`jobs = 1`) reproduced the `--jobs 4` results row for row
(same first failure, same counts), so no row below is a concurrency/epmd artefact.

Throughout, `sys P/F` and `rust P/F` are `passed/failed` as parsed from eunit's own summary;
`cancelled` is the harness status for "the module's fixture could not be set up, so eunit ran no
tests to completion" (the per-row `detail` is a short excerpt — full reasons below come from
re-running the module).

## Per-module table (all 51 candidate modules)

| module | sys P/F | rust P/F | verdict | root-cause class |
|---|---|---|---|---|
| `advanced_delete` | 33/0 | cancelled (0/0) | rust-gap | RC-A |
| `advanced_get` | 13/0 | cancelled (0/0) | rust-gap | RC-A |
| `advanced_put` | 40/0 | cancelled (0/0) | rust-gap | RC-A |
| `advanced_tx_delete` | 39/0 | cancelled (0/0) | rust-gap | RC-A |
| `advanced_tx_get` | 15/0 | cancelled (0/0) | rust-gap | RC-A |
| `advanced_tx_put` | 43/0 | cancelled (0/0) | rust-gap | RC-A |
| `app` | 36/0 | cancelled (22/5) | rust-gap | RC-A (all 5 failures are the ra fixture) |
| `async_option` | 20/0 | cancelled (0/0) | rust-gap | RC-A |
| `bang_functions` | 85/0 | cancelled (0/0) | rust-gap | RC-A |
| `cluster_SUITE` | no_tests | no_tests | agree-fail-identical | excluded: a `common_test` suite, 0 eunit tests |
| `conditions` | 23/0 | 23/0 | agree | — |
| `cth_log_redirect_any_domains` | no_tests | no_tests | agree-fail-identical | excluded: no eunit tests |
| `db_info` | 24/0 | cancelled (3/0) | rust-gap | RC-A |
| `delete_command` | 12/0 | 12/0 | agree | — |
| `display_tree` | 11/0 | 10/1 | divergence | RC-C |
| `export` | 56/0 | cancelled (0/0) | rust-gap | RC-A |
| `export_erlang` | 63/0 | cancelled (0/0) | rust-gap | RC-A |
| `export_helper` | no_tests | no_tests | agree-fail-identical | excluded: no eunit tests |
| `favor_option` | 5/0 | cancelled (0/0) | rust-gap | RC-A |
| `fence` | 6/0 | cancelled (0/0) | rust-gap | RC-A |
| `helpers` | no_tests | no_tests | agree-fail-identical | excluded: no eunit tests |
| `keep_while_conditions` | 27/0 | cancelled (21/1) | rust-gap | RC-A (setup) + **RC-D** (the 1 failure) |
| `machine_code_called_from_ra` | 15/0 | cancelled (0/0) | rust-gap | RC-A |
| `mod_used_for_transactions` | no_tests | no_tests | agree-fail-identical | excluded: no eunit tests |
| `non_versioned_commands` | 15/0 | 15/0 | agree | — |
| `path_operations` | 51/0 | 51/0 | agree | — |
| `pattern_tree` | 5/0 | 5/0 | agree | — |
| `prefix_tree` | 4/0 | 4/0 | agree | — |
| `projections` | 134/0 | cancelled (2/1) | rust-gap | RC-A |
| `prop_path_operations` | no_tests | no_tests | agree-fail-identical | excluded: no eunit tests (property module) |
| `prop_state_machine` | no_tests | no_tests | agree-fail-identical | excluded: no eunit tests (property module) |
| `protect_against_dups_option` | 13/0 | cancelled (5/0) | rust-gap | RC-A |
| `put_command` | 19/0 | 19/0 | agree | — |
| `queries` | 16/0 | 16/0 | agree | — |
| `return_indirect_deletes_option` | 15/0 | cancelled (0/0) | rust-gap | RC-A |
| `root_node` | 22/0 | 22/0 | agree | — |
| `sigils` | 4/0 | 4/0 | agree | — |
| `simple_delete` | 42/0 | cancelled (0/0) | rust-gap | RC-A |
| `simple_get` | 94/0 | cancelled (0/0) | rust-gap | RC-A |
| `simple_put` | 38/0 | cancelled (0/0) | rust-gap | RC-A |
| `simple_tx_delete` | 46/0 | cancelled (0/0) | rust-gap | RC-A |
| `simple_tx_get` | 106/0 | cancelled (0/0) | rust-gap | RC-A |
| `simple_tx_misc` | 45/0 | cancelled (2/0) | rust-gap | RC-A |
| `simple_tx_put` | 47/0 | cancelled (0/0) | rust-gap | RC-A |
| `stored_procs` | 12/1 | cancelled (0/0) | divergence | RC-A (rust) / **oracle-side pre-existing failure** |
| `test_ra_server_helpers` | no_tests | no_tests | agree-fail-identical | excluded: no eunit tests (helper module) |
| `timeout` | 6/0 | cancelled (0/0) | rust-gap | RC-A |
| `triggers` | 118/0 | cancelled (0/0) | rust-gap | RC-A |
| `tx_funs` | 42/0 | 42/0 | agree | — |
| `tx_sprocs` | 33/0 | cancelled (0/0) | rust-gap | RC-A |
| `utils` | 7/0 | 7/0 | agree | — |

## Totals

| | rows with real eunit tests | modules fully green | tests passed | tests failed |
|---|---|---|---|---|
| oracle (`erl`, erts-17.1.1) | 43 | **42** | **1500** | **1** (`stored_procs:crashing_sproc_stacktrace_test`) |
| rust (`beam_rt`, erts-17.1) | 19 | **15** | **285** | **8** (`app` 5, `keep_while_conditions` 1, `projections` 1, `display_tree` 1) |

51 candidate modules = 43 with real eunit tests + 8 with none (`no_tests` on **both** sides).
On rust only **19** of the 43 owned a fixture that got far enough to run a test *at all* (12 `agree` +
`db_info`, `protect_against_dups_option`, `simple_tx_misc` fully green, and `app`/`display_tree`/
`keep_while_conditions`/`projections` partly), and the remaining **24** cancelled in setup — so of the
oracle's 1 501 tests, **1 208 never ran on the rust side and are `unknown`, not failing**.
The 15 modules fully green on rust are a strict subset of the 42 fully green on the oracle —
nothing passes on the rust side that fails on the oracle.

## Root causes (each with its first failing test and smallest failing expression)

### 1. RC-A — `{error,badarg}` from the `prim_file` iodata walk: a valid improper iolist whose tail is a binary is rejected
**29 rows.** Every Khepri module whose fixture starts an `ra` system dies in setup: `ra_log_ets`
opens its `dets` directory file, and `dets` cannot open it.

Affected rows (all 29 `rust-gap` rows whose fixture is ra-backed): `advanced_delete`,
`advanced_get`, `advanced_put`, `advanced_tx_delete`, `advanced_tx_get`, `advanced_tx_put`, `app`,
`async_option`, `bang_functions`, `db_info`, `export`, `export_erlang`, `favor_option`, `fence`,
`keep_while_conditions`, `machine_code_called_from_ra`, `projections`,
`protect_against_dups_option`, `return_indirect_deletes_option`, `simple_delete`, `simple_get`,
`simple_put`, `simple_tx_delete`, `simple_tx_get`, `simple_tx_misc`, `simple_tx_put`, `timeout`,
`triggers`, `tx_sprocs` (plus `stored_procs`, whose verdict is `divergence` for the same reason).

First failing test (of any of the 29 rows): `helpers:start_ra_system/1` (test/helpers.erl:75) —
i.e. the eunit `{setup, …}` context, so eunit reports `cancelled` (or, in `app`/`projections`/
`keep_while_conditions`/`protect_against_dups_option`/`simple_tx_misc`/`db_info`, the tests that ran
before the ra-backed ones pass and the rest cancel).

Crash chain: `dets:open_file/2` → `dets:init/2` → `dets_v9:init_file/17` →
`dets_utils:pwrite/3` → `case_clause {error,badarg}`; the ra crash report names
`{failed_to_start_child,ra_log_ets,{{badmatch,{'EXIT',…}}}}`.

Smallest failing expressions (measured on both runtimes, `iodata_probe.erl` in
`results/khepri-diagnostics/`):

| expression | rust | oracle |
|---|---|---|
| `file:write(Fd, [<<"ab">> \| <<"cd">>])` | `{error,badarg}` | `ok` |
| `file:pwrite(Fd, 0, [<<"ab">> \| <<"cd">>])` | `{error,badarg}` | `ok` |
| `file:pwrite(Fd, [{0, [<<"ab">> \| <<"cd">>]}])` | `{error,badarg}` | `ok` |
| `file:write(Fd, [<<"ab">>, <<"cd">>])` (proper list) | `ok` | `ok` |
| `dets:open_file(t, [{file, "/tmp/x.dets"}])` | `{'EXIT',{{case_clause,{error,badarg}},…}}` | `{ok,t}` |

`iodata()` is `maybe_improper_list(byte() \| binary() \| iolist(), binary() \| [])`, so a binary tail
is legal — and `dets_v9:init_file/17` writes exactly that shape
(`{0, [FileHeader | <<0:(4*?SEGARRSZ)/unit:8>>]}`). Failing crate: **`beam_rt`**
(`crates/beam_rt/src/native/prim_file.rs`, `flatten_iodata`, the `if !is_list(cell) { return None }`
arm of the list walk — an improper tail that is a binary terminates the walk as `badarg` instead of
being appended). Not a documented divergence in that module's header table. Fix belongs to
`beam_rt` (out of this node's write scope) — one extra arm for a binary tail would unblock the
whole `ra` family.

### 2. RC-B — `process_flag/2` answers `badarg` for `min_heap_size` / `min_bin_vheap_size` / `fullsweep_after`
**Revealed behind RC-A** (with RC-A worked around, the same 29 rows then die in `ra_log_wal:init/1`).
Affected rows: the same 29 as RC-A.

First failing test: the same fixture (`helpers:start_ra_system/1`), now via
`ra_log_wal:init/1` (ra/src/ra_log_wal.erl:313–315) →
`{badarg,[{erlang,process_flag,2,…}]}` and the `ra_log_wal_sup` start error.

Smallest failing expressions (`process_flag_probe.erl`):

| expression | rust | oracle |
|---|---|---|
| `process_flag(min_heap_size, 233)` | `error:badarg` | `233` (previous value) |
| `process_flag(min_bin_vheap_size, 46422)` | `error:badarg` | `46422` |
| `process_flag(fullsweep_after, 0)` | `error:badarg` | `65535` |
| `process_flag(max_heap_size, #{size => 0})`, `save_calls`, `message_queue_data`, `priority`, `trap_exit` | agree | agree |

Failing crate: **`beam_rt`** (`crates/beam_rt/src/context/process_bridge.rs`, `ProcessBridge::process_flag` — the match covers `trap_exit`, `sensitive`, `async_dist`, `priority`,
`save_calls`, `error_handler`, `message_queue_data`, `max_heap_size` and falls through to
`Ok(None)` ⇒ `badarg` for the three heap-sizing flags). Out of this node's write scope.

**Depth of the chain is unmeasured past RC-B:** with both defects outstanding the whole `ra` family
is blocked, so whether the ~1 200 not-yet-run Khepri tests would then pass is *unknown*, not
"failing". Both classes must be fixed (or worked around) before the next sweep can say more.

### 3. RC-C — `term_to_binary`/`external_size` of a term that *contains* a local fun: typed `unsupported_local_fun`
**1 row** (`display_tree`: rust 10 passed / 1 failed, the oracle 11/0) — the single genuine
"computed something different" divergence in this sweep.

First failing test: `display_tree:machine_overview_test/0` (test/display_tree.erl:380) →
`khepri_machine:compute_command_size/1` (src/khepri_machine.erl:2994) →
`erlang:external_size({put_v,{put_v1,[baz,fizz],{p_sproc,#Fun<display_tree.0.…>,false},…}})`
→ `error:unsupported_local_fun`.

Smallest failing expressions (`external_size_probe.erl` + a nested-shape probe):

| expression | rust | oracle |
|---|---|---|
| `term_to_binary(fun() -> ok end)` (bare fun) | `78` bytes | `78` bytes |
| `term_to_binary({fun() -> ok end})` | `error:unsupported_local_fun` | `80` bytes |
| `term_to_binary({x, fun() -> ok end})` | `error:unsupported_local_fun` | `83` bytes |
| `term_to_binary([fun() -> ok end])` | `error:unsupported_local_fun` | `84` bytes |
| `term_to_binary(#{k => fun() -> ok end})` | `error:unsupported_local_fun` | `86` bytes |
| `erlang:external_size({x, fun() -> ok end, y})` | `error:unsupported_local_fun` | `95` |
| `term_to_binary(fun Fact(0)->1; Fact(N)->N*Fact(N-1) end)` (named) | `78` bytes | `78` bytes |

Failing crates: **`beam_bifs`** (`crates/beam_bifs/src/binconv.rs::term_encoder` resolves
`ctx.local_fun_identity(term)` on the **whole term**, so an identity is installed only when the
top-level term *is* the local fun) together with **`beam_codec`**, whose encoder API takes a single
`FunIdentity` (`Encoder::with_fun_identity`) rather than a per-fun resolver — the nested encode
path therefore answers the typed `unsupported_local_fun`. Not a wrong image, but a real observable
difference for any term carrying a `#Fun<…>` (Khepri's stored procedures). Out of this node's write
scope. (The differential corpus' recorded A.5 gap `wire/new_fun_ext_encode` is the `erl_eval`
lambda flavour of the same encoder limitation.)

### 4. RC-D — performance: a keep-while delete fan-out is ~250–330× slower than the oracle
**1 row** (`keep_while_conditions`: rust 21 pass, 1 fail — the only failure is this one; the rest of
the module's rows cancel on RC-A).

First failing test: `keep_while_conditions:delete_node_with_many_dependents/0`
(test/keep_while_conditions.erl:800) — `?assert(DeletionTime < 1000)` on a one-second wall-clock
budget around a single `khepri_machine:apply(?META, #delete{path = [target]}, S1)` that must delete
`?MAX_SIBLINGS_PER_PATTERN * 5 div 2` keep-while dependents (2 500, with `?MAX_SIBLINGS_PER_PATTERN`
being 1 000 in khepri's `src/khepri_tree.hrl`).

Smallest reproducer: the test itself; measured by copying the test module to `$TMPDIR`, replacing
the assertion with `io:format(user, "DELETION_TIME_MS=~p~n", [DeletionTime])`, compiling it with
`erlc -DTEST -I <stage> -I <stage>/test -I <stage>/include -I <stage>/src`, putting that dir first
on the code path and calling
`{timeout,_,F} = keep_while_conditions:delete_node_with_many_dependents_test_(), F()`:

| runtime | `DeletionTime` (3 runs) | ratio |
|---|---|---|
| `beam_rt` | 10278 / 9454 / 10062 ms | — |
| `erl` (oracle) | 39 / 33 / 31 ms | rust ≈ **240–330×** the oracle |

This is a *performance* divergence, not a semantic one (the deleted node set is asserted later in
the same test and the remaining assertions were not reached because the first one aborts the test).
Unlike RC-A/-B/-C it is safe to re-run: the ~3-orders-of-magnitude gap is far outside the host's
noise band, and both sides ran on the same loaded host. The affected work is Khepri's tree
keep-while dependency sweep on `beam_rt`; it is *not* attributable to a single opcode/BIF from this
measurement alone — narrowing it needs a profile, which is outside this provenance task.

### Not root-caused
None: every one of the 31 non-`agree` rows is attributed above.

## Exclusions

- **`cluster_SUITE`** — a `common_test` **suite**, not an eunit module: it has no eunit tests, so
  both sides report `no_tests` (0/0). Counting it as a pass (or a fail) would be a lie; the harness
  reports `no_tests` and the verdict `agree-fail-identical` only because neither side ran anything.
  It is not a parity target in this sweep (its cases are a `ct` target, not an eunit one).
- **Seven more `no_tests` rows**, none of which are eunit suites: `cth_log_redirect_any_domains`
  (a `common_test` hook module), `export_helper`, `helpers`, `mod_used_for_transactions`,
  `test_ra_server_helpers` (helper modules), `prop_path_operations` and `prop_state_machine`
  (`proper` property modules). Same rationale: 0 eunit tests on both sides.
- **`stored_procs` — the pre-existing oracle-side failure.** The oracle itself fails
  `stored_procs:crashing_sproc_stacktrace_test` (12 passed / 1 failed): its pinned
  `?_assertMatch` on the stacktrace of the `throw` caught from `khepri:run_sproc/3` does not match
  the trace this OTP-29 host produces (`{file,_}`/`{line,_}` location shapes and one
  `stored_procs` frame differ). That failure is a property of the checkout plus the host OTP
  release, reproducible without this runtime, and is **not** a Rust parity target. Caveat: the rust side of this row is `cancelled` on RC-A, so the
  harness verdict is `divergence` rather than the `agree-fail-identical` exclusion it should
  become — this module's tests run against an `ra` fixture, so none of them runs on the rust side. Once RC-A+RC-B are fixed the row should be
  re-measured and, if the stacktrace shape then matches the oracle's bytes, re-classified.

## Diagnostics used (reproducible)

`results/khepri-diagnostics/` holds the three probe modules used above (`iodata_probe.erl`,
`process_flag_probe.erl`, `external_size_probe.erl`); each prints the same expressions on both
runtimes:

```
erlc -o /tmp/probe results/khepri-diagnostics/*.erl
<runtime> -noshell -pa /tmp/probe -eval 'iodata_probe:main(), halt(0).'
```

The first *workaround* below was used to look behind RC-A; **it is not in this repository** (it was
built in `$TMPDIR`, and neither side of the committed sweep used it — the committed numbers are the
unmodified runtime):

1. **dets fix (diagnostic only).** Copy `lib/stdlib/src/{dets_utils.erl,dets.hrl}` to `$TMPDIR`,
   replace the `pwrite/3` body with
   `Flat = [{Pos, iolist_to_binary(Bin)} || {Pos, Bin} <- Bins], case file:pwrite(Fd, Flat) of …`,
   `erlc -I <tmp>`, and put that directory **first** on the code path. Effect: `dets:open_file/2`
   becomes `{ok, Tab}` and the 29 rows get past `ra_log_ets` — straight into RC-B.
2. **no workaround exists for RC-B** (a BIF raising `badarg` cannot be shimmed from Erlang), so the
   chain stops there and nothing beyond `ra_log_wal:init/1` was measured.

## Caveat

The host is shared and was under heavy external load at sweep time (1-minute loadavg 170 → 92 on 16
logical CPUs, `--jobs 4`). Therefore **only counts and ratios in this file are portable**: the
pass/fail counts, the module tallies and the per-row verdicts are re-derivable by re-running
`suites run --target khepri`, while the absolute times quoted for RC-D (and any wall time implied by
the sweep's own duration) are indicative host observations, not benchmarks; re-measure RC-D with the
documented instrumented test on an idle host before publishing a number, and expect the *ratio*
(~2–3 orders of magnitude) to survive and the absolute values not to.
