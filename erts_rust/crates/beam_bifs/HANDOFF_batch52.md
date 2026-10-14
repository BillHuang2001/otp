# Handoff — batch-52 BIF-surface gap report (cross-crate findings)

Owner: `beam_bifs`. Everything below was found by the batch-52 differential triage and is **not** fixable inside
this crate; each entry names the owning node, the exact location and a one-line reproduction. Verified against the
system oracle (OTP 29.1.1 / erts-17.1) with a `target/debug/beam_rt` built at the same commit.

## Closed here (for the record)
`erlang:convert_time_unit/3` and `zlib:compress/1`/`uncompress/1` were registered as stdlib fast paths in
`src/time.rs` / `src/zlib.rs`. `timer:tc/1,2,3,4` (every clause ends in `convert_time_unit(T2-T1,native,Unit)`) and
the two `zlib` convenience functions therefore work:
`beam_rt -noshell -eval 'io:format("~p~n",[element(2,timer:tc(fun() -> 1+1 end))])'` => `2` (oracle `2`);
`... [byte_size(zlib:compress(<<"hello">>))]` => `13` (oracle `13`).

## Not a gap (stale report)
- **`string:concat/1`** — `undef` on **both** sides; the oracle exports only `{concat,2}`.
  `string:concat/2`, `timer:now_diff/2` and `timer:apply_after/4` already agree; the `apply_after` `compare`
  mismatch is only the reference term's id/creation (the known ref-identity residual in `beam_process`/`beam_rt`),
  not the timer call.
- **`enif_make_ref` / `enif_term_to_binary` / `enif_binary_to_term`** — already closed at HEAD. Wiring:
  `beam_abi/src/nif/enif/{make,get,mem,proc}.rs` -> the `NifHost` trait (`src/nif/host.rs:73` `intern_atom`,
  `:104` `term_to_binary`, `:113` `binary_to_term`, `:122` `make_ref`) -> `beam_rt/src/nif.rs:478/486/575/530/556/586`
  (`RuntimeNifServices`). Differential cases `nif_abi/*`, `nif_abi_extra/*` agree. Caveat: `enif_make_ref` is live
  only on the **per-call** host (`beam_rt/src/context/calls.rs:810-823`); the load/unload host is deliberately
  `services = None` (`context/bridges.rs:485`) and raises `badarg` by design — no corpus case exercises it.
- **There is no `enif_atom_len` symbol.** The real names are `enif_make_atom_len` / `enif_get_atom_length` /
  `enif_make_existing_atom_len`, all implemented. The two `enif_*` slots that *do* differ are the atom-**cache**
  ones, owned by `beam_abi` (`src/nif/enif/proc.rs:146` `enif_get_atom_cache_index` rust `0` vs oracle `1`;
  `:160` `enif_max_atom_cache_index` rust `0` vs oracle `2038`) and deliberately unpinned in `beam_abi/GAPS.md` §A.3.

## Required from `beam_rt`

### 1. `-pz` is merged into the `-pa` head, so a `-pz` directory shadows system modules
`Builder::code_paths()` (`beam_rt/src/cli.rs:487-491`) merges `-pa` and `-pz` into one list; it is seeded at
`runtime.rs:579-581` and both land **before** the system ebins (`codeserver.rs:173-178/215-229`). The oracle puts
`-pa` in front of the system path but `-pz` at the **tail** (after `"."`).

Repro (rust => the shadowing file, oracle => stdlib):
`target/debug/beam_rt -noshell -pz <d> -eval 'io:format("~p~n",[code:which(proplists)])'`
Also visible as `code:get_path()`: rust `[P, Q, stdlib...]` vs oracle `[..., ".", Q]` for `-pa P -pz Q`.
Fix: keep `-pz` in a separate tail list. Not expressible through the differential harness today (its `--pa` has no
`-pz` sibling).

### 2. `+S 4:2` parses the online count and discards it
`parse_schedulers` (`beam_rt/src/cli.rs:684-694`) validates the whole `Total:Online` shape but returns only the first
number, and `EmulatorConfig.schedulers: Option<u32>` (`beam_common/src/config.rs:269-270`) has no online field, so
`erlang:system_info(schedulers_online)` is answered from `schedulers_count` (`info/schedulers.rs:84` ->
`info.rs:629`) = the total.

Repro: `target/debug/beam_rt +S 4:2 -noshell -eval 'io:format("~p~n",[erlang:system_info(schedulers_online)])'`
=> rust `4`, oracle `2`.
Fix: store the online count (a `beam_common` `EmulatorConfig` field) and report it at
`info/schedulers.rs:80,84,110-114` (plus `schedulers_state` :130 and `all_schedulers_state` :147).
`+S 4` (no `:`) already agrees. Secondary divergences in the same area: default `schedulers` and
`logical_processors` are 14 vs the oracle's 16 on this host, `dirty_cpu`/`dirty_io` `0/0` vs `4/10`.

### 3. `process_info/2`: the position items are unsupported even for a live module-scheduled process
`position_value` (`beam_rt/src/native/memory/mod.rs:669` — `current_function`, `initial_call`, `current_location`,
`current_stacktrace`) and `catchlevel` (:706) are gated on "no engine for the process". The root cause is
`runner/run.rs:252` removing the engine on a scheduler pass, so `runner/entry.rs:261`'s `engine_at` misses — which
contradicts that module doc's "reports all 17". `backtrace`, `magic_ref`, `parent`, `label`, `async_dist`,
`priority_messages` and `garbage_collection_info` are in `UNSUPPORTED_ITEMS` (:282, reasons :297).

Repro: `target/debug/differential compare 'process_info(self(), current_function)'` => `[UNSUPPORTED]`.

Items that **agree** (16): `status`, `message_queue_len`, `messages`, `monitors`, `monitored_by`, `dictionary`,
`error_handler`, `priority`, `min_heap_size`, `min_bin_vheap_size`, `fullsweep_after`, `message_queue_data`,
`trace`, `sequential_trace_token`, `suspending`, `last_calls`. The remaining differences are own-state, not bugs:
`registered_name` (`[]` vs `{registered_name,init}`), `links` (`[<0.0.0>]` vs `[]`), `trap_exit` (`true` vs `false`),
`group_leader` (`<0.70.0>` vs `<0.0.0>`), volatile numerics, `max_heap_size` key order, `binary` (3 entries vs
`[]`). `process_info/1`'s list form is 15 keys vs the oracle's 16 (has `registered_name`, lacks
`current_function`/`initial_call`). **No corpus case covers any of these items.**

## Documentation corrections owed by their owners
- `tools/differential/src/corpus/nif_abi_extra.rs:2092-2095` still claims `enif_make_ref`, `enif_term_to_binary` and
  `enif_binary_to_term` are divergent.
- `crates/beam_rt/CONTEXT.md` still claims those three "remain defaulted `None` in `NifHostImpl`".

## Genuinely blocked in `beam_bifs`
- `zlib:zip/1` / `zlib:unzip/1` (gzip / RFC 1952) and the whole zlib **stream** API stay `undef`: they need a
  per-stream resource with a controlling process, and `beam_codec` exposes no gzip framing.
- `zlib:uncompress/1` diverges for a decompressed payload above `beam_codec::compress::MAX_UNCOMPRESSED` (32 MiB):
  `data_error` here, success in the oracle. Typed, never a wrong value.
