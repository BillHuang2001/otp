# Khepri divergence classes RC-A…RC-D — re-verified at HEAD

Measurement/diagnosis only: nothing in this directory changes a runtime crate, and
`/home/bill/Source/khepri` was never written (the staged copy
`$TMPDIR/erts-suites-stage/khepri/proj` was used read-only).

## Verdicts

| # | Check (smallest expression) | Oracle (`erl`) | Rust (`beam_rt`) | Verdict |
|---|---|---|---|---|
| RC-A | `file:write(Fd, [<<"ab">> \| <<"cd">>])` on a `[write,raw,binary,read]` fd | `ok` | `ok` | **fixed** |
| RC-A | `file:pwrite(Fd, 0, [<<"ab">> \| <<"cd">>])` / `file:pwrite(Fd, [{0, [<<"ab">> \| <<"cd">>]}])` | `ok` | `ok` | **fixed** |
| RC-A | `dets:open_file(t, [{file, "dets_probe.dets"}])` + `insert/sync/lookup/close/reopen` | `{ok, t}`, 3 records, size 3, reopen `{ok, t}` | identical | **fixed** |
| RC-A | md5 of the files those writes produce (`raw_probe.dat`, `dets_probe.dets`) | `827f51cc…`, `d9259f29…` | same two md5s (byte-identical) | **fixed** |
| RC-B | `process_flag(min_heap_size, 233)` | `233` | `error:badarg` | **still present** |
| RC-B | `process_flag(min_bin_vheap_size, 46422)` | `46422` | `error:badarg` | **still present** |
| RC-B | `process_flag(fullsweep_after, 0)` | `65535` | `error:badarg` | **still present** |
| RC-B | `process_flag(max_heap_size, #{size => 0})`, `save_calls`, `message_queue_data`, `priority` | previous value | same previous value | agree |
| RC-B | `process_info(self(), trap_exit)` in the `-eval` process | `true` | `false` | **still present** (detail narrowed: the **initial state of the `-eval` process**, not the BIF) |
| RC-B | `process_flag(trap_exit, true)` in the `-eval` process | `true` | `false` (follows from the row above) | **still present** |
| RC-B | `process_flag(trap_exit, true)` in a plain `spawn/1` process | `false` | `false` | agree |
| RC-B | `spawn_opt(Fun, [{min_heap_size,233}])` / `[{fullsweep_after,0}]` / `[{min_bin_vheap_size,46422}]` | `233` / `0` / `46422` | same | agree |
| RC-B (Khepri wall) | `ra_log_wal:init/1`'s flag preface (ra/src/ra_log_wal.erl:308-314) on one process | `[{trap_exit,false},{message_queue_data,on_heap},{min_bin_vheap_size,46422},{min_heap_size,233}]` | `error:badarg` at the third call (line 313) | **still present**, now the first wall |
| RC-C | `erlang:external_size(fun() -> ok end)` (bare fun) | `87` | `87` | agree |
| RC-C | `erlang:external_size({x, fun() -> ok end, y})` | `95` | `error:unsupported_local_fun` | **still present** |
| RC-C | `term_to_binary({fun() -> ok end})` / `({x, fun() -> ok end})` / `([fun() -> ok end])` / `(#{k => fun() -> ok end})` | `89` / `92` / `93` / `95` bytes | `error:unsupported_local_fun` on all four | **still present** |
| RC-C | `keep_while_conditions`-style row: `display_tree` eunit module | 11 passed / 0 failed | 10 passed / 1 failed (`machine_overview_test`, `error:unsupported_local_fun`) | **still present** |
| RC-D | 2 500 keep-while dependents, one `#delete{path = [target]}` | 28 / 37 / 43 / 47 ms (4 runs) | 7 234 / 7 343 / 8 591 / 10 250 / 11 784 ms (5 runs) | **still present** (≈ 170–420×, same order of magnitude as the old record) |
| RC-D | the module's own assertion `?assert(DeletionTime < 1000)` (test/keep_while_conditions.erl:800) | passes | **fails** (`keep_while_conditions` eunit: 21 passed / 1 failed, then context cancelled) | **still present** |
| RC-D | deleted node set of that delete | 2 502 nodes | 2 502 nodes | agree (latency only) |
| RC-D | 100-node dependency chain (`delete_node_with_dependency_chain/0`) | 2 ms | 50 ms (≈ 25×) | slower, assertion still passes |

Nothing in RC-A…RC-D became *worse*; the only class that changed at all is RC-A (fixed) and
RC-B's `trap_exit` entry (root cause narrowed to the eval process's birth flag, not the BIF).

## What was run

- Rust runtime: `erts_rust/target/release/beam_rt` built from this worktree at
  `a56bef586b61fcf0a9b2b67c30f13ba112a8ad7d`
  (`erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt --release`).
- Oracle: the system `erl`/`erlc` on `$PATH` — OTP 29.1.2, erts `17.1.1` (the tree pins
  OTP 30.0-rc0 / erts `17.1`, so the oracle's `erts` patch differs by design).
- Probes: `erlc -o <dir> results/khepri-diagnostics/*.erl`, then each side run in its **own**
  throwaway cwd:
  `<runtime> -noshell -pa <dir> -eval '<probe>:main(), halt(0).'`
  (own cwd matters for `dets_probe.erl`, which writes relative paths so the two files can be
  compared with `cmp`/`md5sum`).
- Khepri: staged build at `$TMPDIR/erts-suites-stage/khepri/proj`; code path = the 10
  `_build/test/lib/*/ebin` plus `_build/test/lib/khepri/test` (11 `-pa` entries), identical on
  both sides; eunit rows via `eunit:test(<module>, [verbose]), halt(0).`.
- The host is shared and was loaded throughout (1-min loadavg 7–67 during this session);
  **only counts and same-host ratios are portable** — the millisecond columns are host
  observations, never benchmark claims. RC-D's ratio is far outside the noise band.

## RC-A — fixed (iodata walk + on-disk bytes)

`iodata_probe.erl` (unchanged) now prints `ok` / `ok` / `ok` / `ok` / `{ok, iodata_probe_tab}`
on both runtimes — the improper-iolist write, both `pwrite` shapes and `dets:open_file/2`.

`dets_probe.erl` (new) extends that to the bytes and to the `dets` lifecycle:

| expression | oracle | rust |
|---|---|---|
| `file:open("raw_probe.dat", [write,raw,binary,read])` | `{ok, Fd}` | `{ok, Fd}` (fd descriptor map differs — see "side observations") |
| `file:write(Fd, [<<"ab">> \| <<"cd">>])` | `ok` | `ok` |
| `file:pwrite(Fd, 8, [<<"ef">> \| <<"gh">>])` | `ok` | `ok` |
| `file:pwrite(Fd, [{0, [<<"AB">> \| <<"CD">>]}])` | `ok` | `ok` |
| `file:position(Fd, eof)` / `file:read_file/1` | `12` / `<<"ABCD",0,0,0,0,"efgh">>` | identical |
| `dets:open_file/insert/sync/lookup/info size/close` | `{ok,t}` / `ok` / `ok` / `[{k2,"two"}]` / `3` / `ok` | identical |
| reopen the written file, `lookup(k1)`, close | `{ok,t}` / `[{k1,1}]` / `ok` | identical |
| file size of the `dets` file | 5 933 bytes | 5 933 bytes |
| `md5sum` of `raw_probe.dat` | `827f51cc021984df97cb6c8973af2198` | `827f51cc021984df97cb6c8973af2198` |
| `md5sum` of `dets_probe.dets` | `d9259f291bac3a555d6afb4b8bc77af8` | `d9259f291bac3a555d6afb4b8bc77af8` |

So the `ra_log_ets` step of the Khepri fixture now gets through: the crash chain that used to
be `dets:init/2 → dets_v9:init_file/17 → dets_utils:pwrite/3 → {case_clause,{error,badarg}}`
is gone, and the next wall is RC-B (below).

## RC-B — still present, and it is the Khepri wall

`process_flag_probe.erl` (unchanged):

| expression | oracle | rust |
|---|---|---|
| `process_flag(min_heap_size, 233)` | `233` | `{'EXIT',{badarg,[{process_flag_probe,'-main/0-fun-0-',1,…}]}}` |
| `process_flag(min_bin_vheap_size, 46422)` | `46422` | same `badarg` |
| `process_flag(fullsweep_after, 0)` | `65535` | same `badarg` |
| `process_flag(max_heap_size, #{size => 0})`, `save_calls 5`, `message_queue_data off_heap`, `priority high` | previous value | same previous value (the map's *printing order* differs — see "side observations") |

Owning crate: **`beam_rt`** — `crates/beam_rt/src/context/process_bridge.rs::process_flag`
matches `trap_exit`, `sensitive`, `async_dist`, `priority`, `save_calls`, `error_handler`,
`message_queue_data`, `max_heap_size` and falls through to `_ => Ok(None)`, which the BIF layer
turns into `badarg`, for the three heap-sizing flags (lines 306–461; the fallthrough is the
`_ => Ok(None)` arm at 460). The state the flags would set lives in `beam_process`'s
`ProcessFlags`/collector policy and `beam_gc`'s heap sizing, so a fix spans `beam_rt` +
`beam_process`/`beam_gc`, not `beam_bifs`.

`trap_exit`, narrowed: the divergence is **not** in the BIF. On the oracle the process that runs
`-eval` is born with `trap_exit = true`, so the first `process_flag(trap_exit, true)` answers
`true`; on this runtime the eval process is born with the spawn default `false`, so it answers
`false`. A plain `spawn/1`ed process agrees (`false` / `false`), and `spawn_opt` birth options
agree. Owning crate: **`beam_rt`** (the CLI/`-eval` bootstrap process is never given
`trap_exit`; there is no `set_trap_exit` on that path anywhere under `crates/beam_rt/src/eval*`).

### The Khepri wall: smallest failing expression

`ra_wall_probe.erl` reproduces the fixture call exactly
(`ra_system:start(ra_system:default_config()#{name => …, data_dir => …, wal_data_dir => …,
wal_max_size_bytes => 16*1024, names => ra_system:derive_names(…)}`):

- oracle: `application:ensure_all_started(ra)` → `{ok,[sasl,crypto,aten,gen_batch_server,seshat,ra]}`,
  `ra_system:start/1` → `{ok, <0.106.0>}`.
- rust: `ensure_all_started(ra)` also succeeds, then the `ra_log_wal` child dies with the
  runtime's own crash report:

```
exception error: bad argument
  in function  process_flag/2
  in call from ra_log_wal:init/1 (…/lib/ra/src/ra_log_wal.erl:313)
  in call from gen_batch_server:init_it/2 (…/gen_batch_server.erl:225)
```

`ra_log_wal:init/1` calls, in order, `process_flag(trap_exit,true)` (:308, agrees),
`process_flag(message_queue_data,off_heap)` (:312, agrees),
`process_flag(min_bin_vheap_size, MinBinVheapSize)` (:313, **badarg**),
`process_flag(min_heap_size, MinHeapSize)` (:314, **badarg**).

**Smallest failing expression: `process_flag(min_bin_vheap_size, 46422)`** (`min_heap_size`
fails identically; there is no Erlang-level workaround for a BIF that raises `badarg`, so
nothing behind this point can be measured). Owning crate: **`beam_rt`** as above.

## RC-C — still present, unchanged class

`external_size_probe.erl` (extended with the nested shapes the old record listed):

| expression | oracle | rust |
|---|---|---|
| `erlang:external_size(fun() -> ok end)` | `87` | `87` |
| `term_to_binary(fun Fact(0)->1; Fact(N)->N*Fact(N-1) end)` | `87` | `87` |
| `erlang:external_size({x, fun() -> ok end, y})` | `95` | `error:unsupported_local_fun` |
| `term_to_binary({x, fun() -> ok end, y})` | `95` | `error:unsupported_local_fun` |
| `term_to_binary({fun() -> ok end})` | `89` | `error:unsupported_local_fun` |
| `term_to_binary({x, fun() -> ok end})` | `92` | `error:unsupported_local_fun` |
| `term_to_binary([fun() -> ok end])` / `erlang:external_size([fun() -> ok end])` | `93` / `93` | `error:unsupported_local_fun` |
| `term_to_binary(#{k => fun() -> ok end})` | `95` | `error:unsupported_local_fun` |
| `term_to_binary(an_atom)` (control) | `10` | `10` |

(The old record's `78` bytes for the bare fun is stale — both sides answer `87` for the probe as
committed now, so the bare-fun row is an *agreement* either way; the discriminating rows are the
nested ones.)

In situ: `display_tree` → oracle `All 11 tests passed`, rust `Failed: 1. Passed: 10.` with

```
exception error: unsupported_local_fun
 in function erlang:external_size/1
   called as erlang:external_size({put_v,{put_v1,[baz,fizz],{p_sproc,#Fun<display_tree.0.…>,false},…},…})
 in call from khepri_machine:compute_command_size/1 (src/khepri_machine.erl:2994)
 in call from display_tree:machine_overview_test/0 (test/display_tree.erl:380)
```

Owning crates: **`beam_bifs`** — `crates/beam_bifs/src/binconv.rs::term_encoder` asks the
identity hook for the *whole* term (`ctx.local_fun_identity(term)` at :1010) and installs it as
`Encoder::with_fun_identity` only when that answer is `Some`; a tuple/list/map containing the fun
answers `None`, so the nested encode reaches `beam_codec`'s `encode_fun` with
`fun_identity == None` (`crates/beam_codec/src/encode/fun.rs:66`) and gets the typed
`unsupported_local_fun`. `beam_codec`'s `Encoder` holds a single `FunIdentity`
(`with_fun_identity`), so a fix needs a per-fun resolver in the encoder API plus the caller
passing every fun — the `beam_rt` side already has a per-term resolver
(`crates/beam_rt/src/context/fun_identity.rs`), it simply is not consulted for nested funs.

## RC-D — still present (performance), assertion still fails

`keep_while_fanout_probe.erl` (new) reproduces both latency tests of
`test/keep_while_conditions.erl` standalone (they are pure `khepri_machine:init/1` +
`khepri_machine:apply/3` work — no `ra` — which is why they can be measured even though the
module's other rows are blocked). One target node (`[target]`), `2 500` keep-while dependents
(`?MAX_SIBLINGS_PER_PATTERN * 5 div 2`), then a single `#delete{path = [target]}`:

| run | oracle `delete` | rust `delete` | ratio |
|---|---|---|---|
| 1 | 47 ms | 8 591 ms | ≈ 183× |
| 2 | 43 ms | 7 234 ms | ≈ 168× |
| 3 | 37 ms | 7 343 ms | ≈ 198× |
| 4 | 28 ms | 10 250 ms | ≈ 366× |
| 5 | 37 ms | 11 784 ms | ≈ 318× |

Deleted-node count is **2 502 on both sides** in every run — the *result* agrees, only the
latency does not, so `?assert(DeletionTime < 1000)` at test/keep_while_conditions.erl:800 fails
on rust (7–12 s > 1 s) and passes on the oracle (28–47 ms < 1 s). Old record was 240–330×; this
re-measurement lands in the same band (≈170–420×, host loaded throughout), i.e. **not fixed and
not materially changed**.

Through the real module: `eunit:test(keep_while_conditions)` → oracle `All 27 tests passed`;
rust `Failed: 1. Passed: 21.` with the failure at that line, then the remaining rows cancel on
the RC-B fixture.

Scaling (same probe, delete only), which is the extra evidence the old record lacked:

| dependents | oracle delete | oracle µs/dependent | rust delete | rust µs/dependent |
|---|---|---|---|---|
| 250 | 2 ms | 8 | 113 ms | 452 |
| 500 | 5 ms | 10 | 422 ms | 844 |
| 1 000 | 14 ms | 14 | 1 601 ms | 1 601 |
| 2 500 | 43 ms | 17 | 7 234 ms | 2 893 |

The oracle's per-dependent cost is near-flat (8 → 17 µs); rust's rises ≈ 6.4× over a 10×
increase in dependents, i.e. **super-linear**, on top of a ≈ 50–170× constant. The insertion
loop that builds the 2 500 dependents shows the same shape (oracle 11–24 ms, rust 2 912–5 180 ms
→ ≈ 120–470×), and the 100-node dependency chain is ≈ 25× (2 ms → 50 ms).
Owning crate: **not attributable from this measurement** — no profile was taken, and the probe
only shows the shape. Two components are visible in the numbers: a per-`khepri_machine:apply/3`
constant (the chain case, consistent with the baseline-JIT per-instruction machinery in
`beam_engine` that `BOTTLENECKS.md` already names for the `foreign/*` rows) and a super-linear
term in the number of keep-while dependents (the Khepri tree sweep: `khepri_tree`'s
keep-while-condition matching over per-pattern chunks — Erlang code running on `beam_engine`,
not a single BIF). Narrowing it needs a profile, which is outside this provenance task.

## Currently-blocked Khepri module groups

| group | rows | first failing test | smallest failing expression | owning crate |
|---|---|---|---|---|
| `ra`-fixture group (every module whose fixture calls `helpers:start_ra_system/1`) | the 29 rows `results/khepri-notes.md` lists as RC-A/RC-B blocked | the eunit `{setup, …}` context — `helpers:start_ra_system/1` (test/helpers.erl:59, `throw/1` at :75) | `process_flag(min_bin_vheap_size, 46422)` (reached via `ra_log_wal:init/1`, ra/src/ra_log_wal.erl:313) | `beam_rt` (`context/process_bridge.rs::process_flag` fallthrough) |
| `display_tree` (RC-C) | 1 | `display_tree:machine_overview_test/0` (test/display_tree.erl:380) | `erlang:external_size({x, fun() -> ok end, y})` | `beam_bifs` (`binconv.rs::term_encoder`) + `beam_codec` (single-identity `Encoder`) |
| `keep_while_conditions` (RC-D) | 1 | `keep_while_conditions:delete_node_with_many_dependents/0` (line 800, the `DeletionTime < 1000` assert) | the 2 500-dependent `#delete{path = [target]}` of `keep_while_fanout_probe.erl` | unknown without a profile — shape points at `beam_engine`'s baseline tier + Khepri's keep-while sweep |

Verified for the first group by running representatives on both sides (rust fails the fixture,
oracle is green): `simple_put` (rust cancelled in setup / oracle green), `simple_get`
(rust `Failed: 0. Passed: 0.` + `*** context setup failed ***` / oracle `All 94 tests passed`),
`advanced_put` (0/0 / 40), `triggers` (0/0 / 118). Every one carries the identical
`ra_log_wal.erl:313` → `process_flag/2` → `badarg` signature, so the group is still one wall.

**Unknown, not "passing":** with `process_flag(min_bin_vheap_size, …)` raising `badarg` and no
Erlang-level workaround for a BIF, nothing behind `ra_log_wal:init/1` can be measured — the
~1 200 Khepri tests those modules would run are `unknown`.

## Side observations (not RC classes, reported rather than acted on)

1. **Small-map key order can differ** (`maps:to_list/1`, `maps:keys/1`, and therefore `~p` of a
   map with atom keys). Observed: `maps:keys(maps:from_list([{b,1},{a,2},{c,3}]))` →
   oracle `[c,b,a]`, rust `[b,a,c]`; likewise the previous-value map of
   `process_flag(max_heap_size, #{size => 0})` prints as
   `#{error_logger,… , include_shared_binaries,… , kill,… , size,…}` on the oracle and as
   `#{kill,… , error_logger,… , include_shared_binaries,… , size,…}` on rust. Map iteration
   order is **undefined by the language**, and both runtimes order small maps the same way —
   by atom *index*: freshly created atoms agree exactly
   (`[list_to_atom("zz"||I)]` → `[zz1,zz2,zz3]` on both, whatever order they are inserted in),
   so the difference comes from the **atom-table insertion order the two runtimes assign while
   loading a module's atoms**, not from map code. Owning crate: `beam_term` (atom table /
   small-map interior). Not a contract violation; it is a false-mismatch risk for any
   differential case that compares `~p` of an atom-keyed map, so it is worth a decision.
2. The `file:open/2` port descriptor map differs in shape (`owner`/`fd_registry_id` keys and
   `#Ref` values), which is an internal representation; every *documented* observable agrees.
3. Nothing else in the probes disagreed.

## Committed probes

| file | class | run lines |
|---|---|---|
| `iodata_probe.erl` (kept) | RC-A | `beam_rt -noshell -pa . -eval 'iodata_probe:main(), halt(0).'` |
| `dets_probe.erl` (new) | RC-A bytes | same form; run each side in its own cwd, then `cmp`/`md5sum` `raw_probe.dat` and `dets_probe.dets` |
| `process_flag_probe.erl` (kept) | RC-B | `… -eval 'process_flag_probe:main(), halt(0).'` |
| `trap_exit_probe.erl` (new) | RC-B `trap_exit`/`spawn_opt` | `… -eval 'trap_exit_probe:main(), halt(0).'` |
| `ra_wall_probe.erl` (new) | the Khepri wall | `… <khepri -pa…> -pa . -eval 'ra_wall_probe:main(), halt(0).'` |
| `external_size_probe.erl` (kept, extended) | RC-C | `… -eval 'external_size_probe:main(), halt(0).'` |
| `keep_while_fanout_probe.erl` (new) | RC-D | `erlc -DTEST -I <staged khepri proj> -o <dir> …`; `… <khepri -pa…> -eval 'keep_while_fanout_probe:main(), halt(0).'` (also `scaling/0`) |
