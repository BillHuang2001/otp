# Khepri eunit parity — sweep notes

Sweep of the **Khepri** eunit suite (`/home/bill/Source/khepri`, third-party, read-only) under the
system ERTS (oracle) and under the Rust runtime (`beam_rt`), driven by this crate
(`suites run --target khepri`). Raw evidence: `khepri/report.json` (schema `suites/1`) and
`khepri/summary.md`; the re-verified per-class probes with their full observation tables live in
`khepri-diagnostics/FINDINGS.md`, next to this file. What these notes add over the artifact is the
attribution: the first failing test, the smallest failing expression and the owning crate for every
row that is not `agree`.

**Headline.** 51 candidate modules: 43 run real eunit tests on the oracle (`1500` passed / `1` failed),
8 run none. On the rust side **285 passed / 8 failed**, and no test that reaches completion computes a
different result except `display_tree` (RC-C). The 29 `rust-gap` rows are a single wall — every module
whose fixture starts an `ra` system dies in `process_flag(min_bin_vheap_size, 46422)` (RC-B) — so 24 of
the 43 rows execute no test at all and **1 208 of the oracle's 1 501 tests are `unknown`, not failing**.
Of the 8 rust test failures, 6 are that wall (`app` 5, `projections` 1), one is the performance
assertion of RC-D (`keep_while_conditions`) and one is RC-C (`display_tree`); the second `divergence`
verdict, `stored_procs`, is the oracle's own pre-existing failure.

## Environment

| item | value |
|---|---|
| oracle | `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` — `29/17.1.1` (system `erl`, never the in-tree C build) |
| rust runtime | `erts_rust/target/release/beam_rt` — `30/17.1`, default (baseline-JIT) tier |
| rust workspace commit | `125ca3a9f3f7260dfc2400faff18117b67834558` — the commit the run's `beam_rt` was built from |
| khepri source | `/home/bill/Source/khepri` at `2ac09291bac7c2fee099380ec1e26d8c9452f776` |
| staged build | `$TMPDIR/erts-suites-stage/khepri/proj`, `rebar3 3.27.1 as test compile` — **reused** a completed build, not rebuilt |
| code path | 11 entries: every `_build/test/lib/*/ebin` + `_build/test/lib/khepri/test` (identical on both sides) |
| drive | `eunit:test('<module>',[verbose]), halt(0).` (identical on both sides) |
| `--jobs` | 4 (each module its own OS process, its own cwd) |
| host loadavg before / after | `38.75 42.77 30.63` / `31.68 40.27 30.41` on 16 logical CPUs — **shared** |

The sha names the workspace commit, not a binary hash: the `beam_rt` binary is per-worktree and
git-ignored. No file under `erts_rust/crates` differs between that commit and this tree, so the
numbers describe the current runtime sources.

### Commands run

```
# build (harness + runtime)
CARGO_HOME=$HOME/.cache/cargo-home cargo build --manifest-path erts_rust/tools/suites/Cargo.toml
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --release

# the sweep behind the committed artifacts
erts_rust/tools/suites/target/debug/suites run --target khepri --jobs 4 \
    --out erts_rust/tools/suites/results/khepri

# attribution: every rust-gap/divergence module re-run serially with the report's own recorded
# invocation shape
#   <beam_rt> -noshell -pa <11 entries> -eval "eunit:test('<module>',[verbose]), halt(0)."
```

Each row runs in `<stage>/cwd/<module>`, which the harness **clears before the row starts**
(`target::fresh_dir`): a `_test.<module>` tree left behind by an earlier sweep otherwise makes fixtures
fail on *both* sides and turns a green row red. Both sides of a row share the one fresh directory.

Re-running the modules **serially** (`jobs = 1`) reproduces the `--jobs 4` results row for row (same
first failure, same counts), so no row below is a concurrency/epmd artefact.

Throughout, `sys P/F` and `rust P/F` are `passed/failed` as parsed from eunit's own summary. The rust
`status` column is the harness's: `cancelled` means the module's fixture could not be set up, so the
`P/F` pair beside it is only the prefix that ran before it died — `0/0` means not one test executed.

## Per-module table (all 51 candidate modules)

| module | sys P/F | rust status | rust P/F | verdict | root-cause class |
|---|---|---|---|---|---|
| `advanced_delete` | 33/0 | cancelled | 0/0 | rust-gap | RC-B |
| `advanced_get` | 13/0 | cancelled | 0/0 | rust-gap | RC-B |
| `advanced_put` | 40/0 | cancelled | 0/0 | rust-gap | RC-B |
| `advanced_tx_delete` | 39/0 | cancelled | 0/0 | rust-gap | RC-B |
| `advanced_tx_get` | 15/0 | cancelled | 0/0 | rust-gap | RC-B |
| `advanced_tx_put` | 43/0 | cancelled | 0/0 | rust-gap | RC-B |
| `app` | 36/0 | cancelled | 22/5 | rust-gap | RC-B (the 5 failures are the `ra` fixture) |
| `async_option` | 20/0 | cancelled | 0/0 | rust-gap | RC-B |
| `bang_functions` | 85/0 | cancelled | 0/0 | rust-gap | RC-B |
| `cluster_SUITE` | 0/0 | no_tests | 0/0 | agree-fail-identical | `common_test` suite: no eunit tests |
| `conditions` | 23/0 | pass | 23/0 | agree | — |
| `cth_log_redirect_any_domains` | 0/0 | no_tests | 0/0 | agree-fail-identical | no eunit tests |
| `db_info` | 24/0 | cancelled | 3/0 | rust-gap | RC-B |
| `delete_command` | 12/0 | pass | 12/0 | agree | — |
| `display_tree` | 11/0 | fail | 10/1 | divergence | RC-C |
| `export` | 56/0 | cancelled | 0/0 | rust-gap | RC-B |
| `export_erlang` | 63/0 | cancelled | 0/0 | rust-gap | RC-B |
| `export_helper` | 0/0 | no_tests | 0/0 | agree-fail-identical | no eunit tests |
| `favor_option` | 5/0 | cancelled | 0/0 | rust-gap | RC-B |
| `fence` | 6/0 | cancelled | 0/0 | rust-gap | RC-B |
| `helpers` | 0/0 | no_tests | 0/0 | agree-fail-identical | no eunit tests |
| `keep_while_conditions` | 27/0 | cancelled | 21/1 | rust-gap | RC-B (setup) + **RC-D** (the 1 failure) |
| `machine_code_called_from_ra` | 15/0 | cancelled | 0/0 | rust-gap | RC-B |
| `mod_used_for_transactions` | 0/0 | no_tests | 0/0 | agree-fail-identical | no eunit tests |
| `non_versioned_commands` | 15/0 | pass | 15/0 | agree | — |
| `path_operations` | 51/0 | pass | 51/0 | agree | — |
| `pattern_tree` | 5/0 | pass | 5/0 | agree | — |
| `prefix_tree` | 4/0 | pass | 4/0 | agree | — |
| `projections` | 134/0 | cancelled | 2/1 | rust-gap | RC-B |
| `prop_path_operations` | 0/0 | no_tests | 0/0 | agree-fail-identical | `proper` module: no eunit tests |
| `prop_state_machine` | 0/0 | no_tests | 0/0 | agree-fail-identical | `proper` module: no eunit tests |
| `protect_against_dups_option` | 13/0 | cancelled | 5/0 | rust-gap | RC-B |
| `put_command` | 19/0 | pass | 19/0 | agree | — |
| `queries` | 16/0 | pass | 16/0 | agree | — |
| `return_indirect_deletes_option` | 15/0 | cancelled | 0/0 | rust-gap | RC-B |
| `root_node` | 22/0 | pass | 22/0 | agree | — |
| `sigils` | 4/0 | pass | 4/0 | agree | — |
| `simple_delete` | 42/0 | cancelled | 0/0 | rust-gap | RC-B |
| `simple_get` | 94/0 | cancelled | 0/0 | rust-gap | RC-B |
| `simple_put` | 38/0 | cancelled | 0/0 | rust-gap | RC-B |
| `simple_tx_delete` | 46/0 | cancelled | 0/0 | rust-gap | RC-B |
| `simple_tx_get` | 106/0 | cancelled | 0/0 | rust-gap | RC-B |
| `simple_tx_misc` | 45/0 | cancelled | 2/0 | rust-gap | RC-B |
| `simple_tx_put` | 47/0 | cancelled | 0/0 | rust-gap | RC-B |
| `stored_procs` | 12/1 | cancelled | 0/0 | divergence | oracle-side pre-existing failure; rust cancelled on RC-B |
| `test_ra_server_helpers` | 0/0 | no_tests | 0/0 | agree-fail-identical | no eunit tests |
| `timeout` | 6/0 | cancelled | 0/0 | rust-gap | RC-B |
| `triggers` | 118/0 | cancelled | 0/0 | rust-gap | RC-B |
| `tx_funs` | 42/0 | pass | 42/0 | agree | — |
| `tx_sprocs` | 33/0 | cancelled | 0/0 | rust-gap | RC-B |
| `utils` | 7/0 | pass | 7/0 | agree | — |

## Totals

| | rows that ran a test | modules fully green | tests passed | tests failed |
|---|---|---|---|---|
| oracle (`erl`, erts-17.1.1) | 43 | **42** | **1500** | **1** (`stored_procs:crashing_sproc_stacktrace_test`) |
| rust (`beam_rt`, erts-17.1) | 19 | **12** | **285** | **8** (`app` 5, `keep_while_conditions` 1, `projections` 1, `display_tree` 1) |

51 candidate modules = 43 with real eunit tests + 8 with none (`no_tests` on **both** sides). Rust's 19
rows that executed a test are the 12 `pass` rows, `display_tree`, and six rows that stopped part-way
(`app` 22/5, `db_info` 3/0, `keep_while_conditions` 21/1, `projections` 2/1,
`protect_against_dups_option` 5/0, `simple_tx_misc` 2/0). The other **24** rows executed nothing, so
**1 208 of the oracle's 1 501 tests never ran on the rust side** (`1 501 − 285 − 8`); the 29
`rust-gap` rows alone own 1 257 oracle-passing tests, of which 1 195 never ran.

The 12 modules fully green on rust are a strict subset of the 42 green on the oracle: nothing passes on
rust that fails on the oracle. The three partial rows with 0 failures (`db_info`,
`protect_against_dups_option`, `simple_tx_misc`) passed every test they reached but are not green —
their remaining tests never ran.

## Root causes (first failing test, smallest failing expression, owning crate)

The classes are the ones named in `khepri-diagnostics/FINDINGS.md`, which carries the per-probe
observation tables and the exact commands; this section states the attribution and points there for the
raw rows. **RC-A** (the improper-iodata `file:write`/`pwrite` walk) does not appear: that class does not
reproduce on this tree — the write shapes, the `dets` lifecycle and the on-disk `.dets` bytes are
oracle-identical (`dets_probe.erl`, md5 `d9259f29…` on both sides), so the `ra` fixture gets past
`ra_log_ets` and dies one step later at RC-B.

### RC-B — heap-sizing `process_flag/2` flags answer `badarg`; this is the wall
**29 rows** (every module whose fixture calls `helpers:start_ra_system/1`). First failing test is the
eunit `{setup, …}` context — `helpers:start_ra_system/1` (test/helpers.erl:59, `throw/1` at :75) — and
eunit reports `cancelled`. The crash chain is one BIF:

```
exception error: bad argument
  in function  process_flag/2
  in call from ra_log_wal:init/1 (…/lib/ra/src/ra_log_wal.erl:313)
  in call from gen_batch_server:init_it/2 (…/gen_batch_server.erl:225)
```

`ra_log_wal:init/1` calls, in order, `process_flag(trap_exit,true)` (:308, agrees),
`process_flag(message_queue_data,off_heap)` (:312, agrees),
`process_flag(min_bin_vheap_size, MinBinVheapSize)` (:313, **badarg**), then
`process_flag(min_heap_size, MinHeapSize)` (:314, **badarg**).

Smallest failing expressions (`process_flag_probe.erl`; full table in `FINDINGS.md`):

| expression | rust | oracle |
|---|---|---|
| `process_flag(min_heap_size, 233)` | `error:badarg` | `233` |
| `process_flag(min_bin_vheap_size, 46422)` | `error:badarg` | `46422` |
| `process_flag(fullsweep_after, 0)` | `error:badarg` | `65535` |
| `process_flag(max_heap_size, #{size => 0})`, `save_calls`, `message_queue_data`, `priority` | previous value | same previous value |
| `spawn_opt(Fun, [{min_heap_size,233}])` / `[{fullsweep_after,0}]` / `[{min_bin_vheap_size,46422}]` | `233` / `0` / `46422` | same |

Owning crate: **`beam_rt`** — `crates/beam_rt/src/context/process_bridge.rs::process_flag` matches
`trap_exit`, `sensitive`, `async_dist`, `priority`, `save_calls`, `error_handler`,
`message_queue_data`, `max_heap_size` and falls through to `_ => Ok(None)` for the three heap-sizing
flags, which the BIF layer turns into `badarg`. The state those flags set lives in `beam_process`'s
`ProcessFlags`/collector policy and `beam_gc`'s heap sizing, so the fix spans `beam_rt` +
`beam_process`/`beam_gc`, not `beam_bifs`.

Narrowed, and distinct from the fallthrough: **the `-eval` process is born with `trap_exit = false`**
where the oracle's is born `true`, so the first `process_flag(trap_exit, true)` answers `false`
(`trap_exit_probe.erl`). A plain `spawn/1` process agrees (`false` / `false`) and `spawn_opt` birth
options agree, so this is the CLI bootstrap process's birth flag, not the BIF — owning crate `beam_rt`
(no `set_trap_exit` on the `-eval` path under `crates/beam_rt/src/eval*`).

**Depth beyond RC-B is unmeasured.** A BIF that raises `badarg` cannot be shimmed from Erlang, so
nothing behind `ra_log_wal:init/1` can be reached and the ~1 200 tests those 29 modules would run are
`unknown`. RC-B is the single change that re-partitions the whole rust column.

### RC-C — ETF encode of a term that *contains* a local fun: typed `unsupported_local_fun`
**1 row** (`display_tree`: rust 10 passed / 1 failed, oracle 11/0) — the only row whose completed test
computed something different. First failing test: `display_tree:machine_overview_test/0`
(test/display_tree.erl:380) → `khepri_machine:compute_command_size/1` (src/khepri_machine.erl:2994) →
`erlang:external_size({put_v,{put_v1,[baz,fizz],{p_sproc,#Fun<display_tree.0.…>,false},…},…})`.

| expression | rust | oracle |
|---|---|---|
| `term_to_binary(fun() -> ok end)` (bare fun) | `87` bytes | `87` bytes |
| `term_to_binary(fun Fact(0)->1; Fact(N)->N*Fact(N-1) end)` (named) | `87` bytes | `87` bytes |
| `term_to_binary({fun() -> ok end})` / `({x, fun() -> ok end})` | `error:unsupported_local_fun` | `89` / `92` bytes |
| `term_to_binary([fun() -> ok end])` / `(#{k => fun() -> ok end})` | `error:unsupported_local_fun` | `93` / `95` bytes |
| `erlang:external_size({x, fun() -> ok end, y})` | `error:unsupported_local_fun` | `95` |

Owning crates: **`beam_bifs`** — `crates/beam_bifs/src/binconv.rs::term_encoder` asks the identity hook
for the **whole** term (`ctx.local_fun_identity(term)`) and installs it as
`Encoder::with_fun_identity` only when that answer is `Some`; a tuple/list/map containing the fun
answers `None` — together with **`beam_codec`**, whose `Encoder` holds a single `FunIdentity`
(`crates/beam_codec/src/encode/fun.rs`), so the nested encode answers the typed
`unsupported_local_fun`. The `beam_rt` side already has a per-term resolver
(`crates/beam_rt/src/context/fun_identity.rs`); it is simply not consulted for nested funs, so the fix
is a per-fun resolver in the `beam_codec` encoder API plus the caller passing every fun. (The
differential corpus' recorded A.5 gap `wire/new_fun_ext_encode` is the `erl_eval`-lambda flavour of the
same limitation.)

### RC-D — a keep-while delete fan-out is ~170–420× slower (performance, not semantics)
**1 row** (`keep_while_conditions`: rust 21 passed / 1 failed; the module's other tests cancel on
RC-B). First failing test: `keep_while_conditions:delete_node_with_many_dependents/0`
(test/keep_while_conditions.erl:800) — `?assert(DeletionTime < 1000)` around one
`khepri_machine:apply(?META, #delete{path = [target]}, S1)` that deletes
`?MAX_SIBLINGS_PER_PATTERN * 5 div 2` = 2 500 keep-while dependents.

Measured with `keep_while_fanout_probe.erl` (pure `khepri_machine:init/1` + `apply/3`, no `ra`, so it
is measurable despite RC-B), same host, interleaved runs:

| side | delete of 2 500 dependents | deleted nodes |
|---|---|---|
| oracle | 28–47 ms | 2 502 |
| rust | 7 234–11 784 ms | 2 502 |

The **result agrees** (identical 2 502-node deleted set); only the latency does not, so the module's
one-second assertion fails on rust and passes on the oracle. The same probe's scaling rows show the
gap is **super-linear** in the number of dependents (rust's per-dependent cost rises ≈ 6.4× over a 10×
increase: 452 → 2 893 µs/dependent, where the oracle stays at 8 → 17), and the 100-node dependency
chain is ≈ 25× (2 ms → 50 ms) while still passing its assertion. Owning crate: **none named from this
measurement** — no profile was taken, and the shape shows two components: a per-`apply/3` constant
consistent with `beam_engine`'s baseline-tier per-instruction machinery (the same lever
`../../BOTTLENECKS.md` names for the `foreign/*` rows) plus a super-linear term in Khepri's own
keep-while sweep (`khepri_tree`'s per-pattern chunks, Erlang code on `beam_engine`). Narrowing it needs
a profile.

### Not root-caused
None: every one of the 31 non-`agree` rows that ran a test is attributed above (29 RC-B, 1 RC-C, 1
RC-D, 1 oracle-side pre-existing failure in `stored_procs`, whose rust side is blocked by RC-B).

## Side observation — small-map key order can differ (not a class)

`maps:keys/1`, `maps:to_list/1` and therefore `~p` of a map with atom keys can print in a different
order on the two runtimes: `maps:keys(maps:from_list([{b,1},{a,2},{c,3}]))` → oracle `[c,b,a]`, rust
`[b,a,c]`; the previous-value map of `process_flag(max_heap_size, #{size => 0})` prints its keys in a
different order too. Map iteration order is **undefined by the language**, and both runtimes order
small maps the same way — by atom *index*: freshly created atoms agree exactly
(`[list_to_atom("zz"||I)]` → `[zz1,zz2,zz3]` on both, whatever the insertion order). The difference
comes from the **atom-table insertion order each runtime assigns while loading a module's atoms**, not
from map code. Owning crate: **`beam_term`** (atom table / small-map interior). It is not a contract
violation, but it is a false-mismatch risk for any differential case that compares `~p` of an
atom-keyed map.

A second, purely internal difference: the `file:open/2` port descriptor map differs in shape
(`owner`/`fd_registry_id` keys, `#Ref` values). Every *documented* observable agrees; nothing else in
the probes disagreed.

## Exclusions

- **`cluster_SUITE`** — a `common_test` **suite**, not an eunit module: it has no eunit tests, so both
  sides report `no_tests` (0/0). Counting it as a pass (or a fail) would be a lie; the harness reports
  `no_tests` with the verdict `agree-fail-identical` only because neither side ran anything. It is not
  a parity target in this sweep (its cases are a `ct` target, not an eunit one).
- **Seven more `no_tests` rows**, none of which are eunit suites: `cth_log_redirect_any_domains` (a
  `common_test` hook module), `export_helper`, `helpers`, `mod_used_for_transactions`,
  `test_ra_server_helpers` (helper modules), `prop_path_operations` and `prop_state_machine` (`proper`
  property modules). Same rationale: 0 eunit tests on both sides.
- **`stored_procs` — the pre-existing oracle-side failure.** The oracle itself fails
  `stored_procs:crashing_sproc_stacktrace_test` (12 passed / 1 failed): its pinned `?_assertMatch` on
  the stacktrace of the `throw` caught from `khepri:run_sproc/3` does not match the trace this OTP-29
  host produces (`{file,_}`/`{line,_}` location shapes and one `stored_procs` frame differ). That
  failure is a property of the checkout plus the host OTP release, reproducible without this runtime,
  and is **not** a Rust parity target. Its rust side is `cancelled` on RC-B, so the harness verdict is
  `divergence` rather than the `agree-fail-identical` exclusion it should become once the fixture comes
  up; re-measure and re-classify after RC-B.

## Diagnostics used (reproducible)

`khepri-diagnostics/` holds the committed probes behind every table above
(`iodata_probe.erl`, `dets_probe.erl` for the `dets` bytes, `process_flag_probe.erl`,
`trap_exit_probe.erl`, `ra_wall_probe.erl` for the Khepri wall, `external_size_probe.erl`,
`keep_while_fanout_probe.erl`). Each prints the same expressions on both runtimes, each side in its
**own** throwaway cwd (the `dets` probe writes relative paths so the two files can be `cmp`/`md5sum`-ed):

```
erlc -o $TMPDIR/probe results/khepri-diagnostics/*.erl
<runtime> -noshell -pa $TMPDIR/probe -eval '<probe>:main(), halt(0).'
# keep_while_fanout_probe needs the staged khepri on the code path and the test macros:
erlc -DTEST -I <stage> -I <stage>/test -I <stage>/include -I <stage>/src -o $TMPDIR/probe \
    results/khepri-diagnostics/keep_while_fanout_probe.erl
<runtime> -noshell <khepri -pa…> -pa $TMPDIR/probe -eval 'keep_while_fanout_probe:main(), halt(0).'
```

`khepri-diagnostics/FINDINGS.md` records the observation tables, the probe-to-class mapping and the
`ra_wall_probe.erl` reproduction of the fixture call `ra_system:start/1` does.

## Caveat

The host is shared and was loaded throughout (1-minute loadavg 38.8 → 31.7 on 16 logical CPUs at sweep
time, `--jobs 4`; 7–67 during the diagnostics session). **Only counts, verdicts and same-host ratios in
this file are portable**: the pass/fail counts, the module tallies and the per-row verdicts are
re-derivable by re-running `suites run --target khepri`, while every millisecond quoted for RC-D is an
indicative host observation, never a benchmark. Re-measure RC-D with the probe on an idle host before
publishing a number: the *ratio* (~2–3 orders of magnitude, and its super-linear shape) is what
survives, the absolute values are not.
