# Observed runtime divergences (handoffs)

Genuine, verified differences between the Rust runtime (`beam_rt`) and the system-ERTS
oracle that the expanded corpus found. None is a capability gap the runtime signals honestly
(an `unsupported` raise / exit 3), so a case pinning one would be a `mismatch` — the gate's
one hard failure. Each is therefore **deliberately not pinned** as a corpus case; the owning
crate below must close the gap, after which a case can be added to the corpus.

(The honest capability gaps — the runtime *does* signal them via its documented
`beam_rt: unsupported: …` / exit-3 convention — are recorded in the allowlist table above,
not here.)
## A.3 — NIF C ABI (owner: `beam_abi` / `beam_rt` NifHost)
- `enif_make_unique_integer` values differ by design (crate counters vs emulator state);
  only the property shapes (`positive` / `monotonic` / default negative) are pinned.
- `enif_system_info` is only partially pinned: `otp_release` (oracle `"29"` vs rust `"30"`),
  `async_threads` / `scheduler_threads` (the runtime's own counts) and `dirty_scheduler_support`
  (oracle `1` vs rust `0`) are **not** asserted, only the frozen version macros + the
  `thread_support`/`smp_support`/`erts_version` fields.
- `enif_max_atom_cache_index()` differs (oracle `2038`, rust `0`) — only the frozen
  surrounding cache-API shapes are pinned.
- `enif_get_atom_cache_index(env, atom, &i)` differs: for a freshly-made atom the oracle
  answers `1` with `i == 399`, the rust runtime `0`.
- The `enif_hash(ERL_NIF_INTERNAL_HASH, term, salt)` **value** is implementation-defined and
  differs; only the `ERL_NIF_PHASH2` properties are pinned.
- `enif_getenv`'s "too small" path leaves `*value_size` at `capacity-1` in the oracle vs the
  needed capacity in the rust runtime (the success path and the missing-key refusal DO agree
  and are pinned).

## A.4 — driver C ABI (owner: `beam_rt` / `beam_io`; `beam_abi` for the timer units)
- **Landed:** `erl_drv_output_term` / `erl_drv_send_term` / `driver_output_term` /
  `driver_send_term` now *deliver* the decoded `ErlDrvTerm` spec to the receiving process
  (the port's connected process — the delivered message is the term itself, never a
  `{Port, Term}` wrapper, `io.c:6162-6176`). Pinned byte-exactly by the new delivery family
  (`driver_abi` selectors `130..137`, images of `{7,42}`, `{ok,7}` and `{<<1,2,3>>,5}`).
- The four forms' exact **success code** still differs (oracle `1` for a queued delivery, rust `0`
  — the frozen `DriverHost::send_term` "accepted" code), so the return value is not pinned; the
  accept/refuse split (selectors `30..38`) and the delivered term (the delivery family above) are.
- **An `ErlDrvTerm` spec that interns a *new* atom is not delivered observably.** The Rust
  runtime's driver host records the name on its `PendingInterner` and never commits it, so the
  delivered term carries a nameless atom: an `ERL_DRV_ATOM` spec for `"driver_abi_term"` yields
  oracle `{driver_abi_term,7}` / `term_to_binary` `<<131,104,2,119,15,"driver_abi_term",97,7>>`
  vs rust `term_to_binary/1` → `error:badarg` (the atom has no name). A spec over an atom both
  tables already hold (`ok`) agrees and **is** pinned. Owner: `beam_rt` / `beam_io`
  (`crates/beam_rt/src/port/driver.rs`, `commit_atoms/1` is only reached from its unit test).
- **`driver_async/5` schedules nothing.** Measured: the C's `driver_async(port, &key, invoke,
  data, free)` answers a positive handle (oracle `1`, the queued job's id) and later runs
  `async_invoke`; the Rust runtime keeps the [`beam_abi::DriverHost`] default and answers `-1`.
  A case observing the handle is a plain-value `mismatch` (oracle `49` vs rust `11569`), so the
  scheduler family stays unpinned. Owner: `beam_rt` / `beam_io`.
- `driver_set_timer/2` on an entry **without** a `timeout` callback differs (oracle accepts,
  rust `-1`); the timing shapes (0 ms, large, re-arm, pending/idle read) agree and are pinned.
- `erl_drv_monotonic_time/1` differs by **absolute phase** (oracle non-positive `0`, rust `1`);
  the phase-free properties (monotonicity, unit conversions) agree and are pinned.
- `erl_drv_time_offset/1` differs (oracle a non-zero offset, rust `0`).
- An entry whose only port-command callback is `outputv` never receives `port_command/2`
  (oracle `49`, rust `-1`).
- An `ErlDrvTerm` `ERL_DRV_LIST N` spec differs: `INT 1, INT 2, INT 3, LIST 3` is the
  **improper** list `[1,2|3]` in the oracle (the last popped word is the tail) but the proper
  list `[1,2,3]` in the Rust runtime. No case pins it.
- `driver_enq`/`driver_sizeq` as probed hangs the **oracle**; `driver_async` with a NULL port
  and `erl_drv_output_term(port, NULL, 2)` SIGSEGV the oracle — harness/probe hazards,
  unpinnable, to re-check before treating any as a runtime gap.
- `driver_mk_port(NULL)` returns the nil term in the rust runtime but crashes the **oracle** VM
  (an exit signal with empty stdout) — a probe hazard, unpinnable; excluded from the `driver_extra`
  block.
- `erl_errno_id/1` returns a short POSIX symbol (`enoent`, `einval`, `errno_0`) in the oracle
  but `strerror` prose in the rust runtime.
- `driver_pdl_get_refc(NULL)` SIGSEGVs the oracle, so the `driver_pdl_*` family stays unpinned
  (the PDL calls themselves now succeed in both).
- `driver_async_port_key/1` → oracle `3`, rust `0`; `erl_drv_consume_timeslice/2` → oracle
  `1`/`1`, rust `0`/`0`.
- `erl_drv_busy_msgq_limits` writes the handle in the rust runtime instead of leaving the
  bounds; `erl_drv_getenv` sets `size` on an absent key in the rust runtime;
  `driver_dl_error()`'s message length differs (33 vs 25); `driver_dl_open(NULL)` +
  `driver_dl_sym` SIGSEGVs the oracle.
- The monitor family `driver_monitor_process` / `driver_demonitor_process` /
  `driver_get_monitored_process` / `driver_compare_monitors` succeeds in the oracle but is refused
  by the rust runtime (the `DriverHost` hooks are left at `None`), so no case pins it.
  Owner: `beam_rt` / `beam_io`.

## A.5 — distribution wire / ETF (owner: `beam_codec`, `beam_rt`)
- A `[compressed]` image is **not** byte-identical: the framing (`131,80`), the 32-bit
  uncompressed size and the trailing Adler-32 match, but the zlib deflate middle differs.
  Only `binary_to_term(term_to_binary(T, [compressed])) =:= T` round trips are pinned.
- The `[used]` byte count of `binary_to_term/2` on a `[compressed]` image differs by 1 byte —
  a consequence of the differing zlib bodies above (the compressed image's size is not
  byte-stable, so it is unpinnable; only the round trip is pinned).
- (The `-eval` **literal/parser** limits — a big-integer literal, an improper-list tail, an
  integer binary segment wider than 64 bits — are no longer divergences: `beam_rt` now evaluates
  each shape, so they are pinned as **agreeing** regression cases; see the table above, owner
  `beam_rt`.)

## A.7 — process signals (owner: `beam_bifs` / `beam_process`)
- A `{'DOWN', …}` for a monitored **port** is not delivered after `port_close/1`: the oracle
  sends `{'EXIT',Port,normal}` (the link) then `{'DOWN',Ref,port,Port,normal}`, the Rust
  runtime sends nothing. The `unlink/1` workaround needed to isolate it also raises
  `error:badarg` in the Rust runtime. (The monitor **creation** now agrees and is pinned.)

## A.7 — program result / CLI (owner: `beam_rt` / `beam_engine`, `beam_bifs`)
- `filelib:is_dir/1` / `is_file/1` / `is_regular/1` (and a literal-path `filelib:wildcard/1`)
  still hit the typed `#file_info{}` `access(2)` gap on an **existing** path
  (`beam_rt: unsupported: file_server: …`, exit 3); their absent-path answers and the glob
  `filelib:wildcard/1` path agree and are pinned. `re:version/0` and `re:split/2` remain rust
  capability gaps (`beam_rt: unsupported: re:version/0` / `re:compile/2`, exit 3) — neither
  cases nor mismatches.
- A bare (unqualified) name in the `-eval` path resolves differently: `phash2(T)` is `error:undef`
  under the oracle's `-eval` (a local call) but `ok …` under the rust runtime (it resolves the
  `erlang:phash2/1` BIF). The `erlang:phash2/1`-qualified shapes agree and are pinned in
  `phash.rs`. Owner: `beam_rt` (`-eval` evaluator).

## A.9 — preloaded / boot (owner: `beam_rt`, `beam_bifs`)
- `init:get_arguments/0` → the oracle reports every flag **plus** its automatic
  `root`/`bindir`/`progname`/`home` entries, the rust runtime only its modelled flags.
  (`init:get_argument(root|home|progname|bindir)` returns the same automatic value in both and
  is pinned.)
- `application:get_all_key/1` / `application:get_key(App, env)`'s **`env` member** differs: the
  oracle injects its runtime-added entries (`logger`, `connect_all`, …) with a per-VM-start
  order, the rust runtime returns the raw, shorter `.app` env. The `{ok,…}` shape and the other
  keys agree and are pinned. Owner: `beam_rt`.
- `application:get_application/1` on a **pid** differs: the oracle answers `{ok,App}` for a
  process an application owns and `undefined` for every other process (`application:get_application(self())`
  and `application:get_application(spawn(fun() -> ok end))` are both `undefined` under `-noshell`),
  while the rust runtime raises `error:function_clause` (it runs no application-owned process).
  The **module** form agrees and is pinned (`boot_extra/application_get_application_1_*`).
  Owner: `beam_rt`.
- `erlang:module_info(exports)` → oracle the export table, rust `undef`
  (`lists:module_info(exports)` works but with a different entry order).
- `whereis(code_server|erl_prim_loader|kernel_sup)` → oracle a pid, rust `undefined`.
- Further surfaces found while probing: `code:get_path/0` (differing path lists),
  `init:script_id/0` (rust `beam_rt: unsupported: init:script_id/0 (no boot script has been
  loaded)`, exit 3; the oracle `{"Erlang/OTP","29"}`),
  `erl_prim_loader:get_path/0` (`{ok,Paths}` vs `badarg`),
  `erlang:system_info(logical_processors)` (16 vs 14),
  `erlang:system_info(heap_sizes)` (the rust list has an extra entry),
  `erlang:module_loaded(lists|kernel)` (`true` vs `false`).
- `erlang:system_info(loaded)` and `erlang:system_info(check_io)`
  are honest capability gaps (rust exit 3 / `beam_rt: unsupported: …`) — neither cases nor
  mismatches; safe to pin as allowlisted cases once wanted.
- `erlang:function_exported/3` is `false` in the rust runtime for `kernel` and `code` (the
  oracle says `true`); it agrees for the preloaded modules and for the file-loaded `lists`.
- `erlang:system_info(schedulers|dirty_cpu_schedulers|dirty_io_schedulers)` → oracle
  `16`/`16`/`10`, rust `14`/`0`/`0`.
- `erlang:system_info(build_type)` is an honest rust capability gap (exit 3 /
  `beam_rt: unsupported: …`; the oracle answers `opt`) — neither a case nor a mismatch.
  (`erlang:system_info(endian)` agrees — `little` in both — and is pinned by the oracle-backed
  `beam_rt` `system_info_oracle` test.)
- `persistent_term:info/0` returns zero-valued counters in the rust runtime (the oracle reports
  live `count`/`memory`).
- `erlang:system_info(tolerant_timeofday)` is an honest rust capability gap (exit 3 /
  `beam_rt: unsupported: …`) — neither a case nor a mismatch.
- The preloaded ∩ registered intersection
  (`[M || M <- erlang:pre_loaded(), lists:member(M, registered())]`) differs — the rust runtime
  registers far fewer names.

## A.1 — terms, records, exceptions (owner: `beam_engine`, `beam_bifs` / `beam_rt`)
- Calling a non-fun (`5(1)`) raises `{badfun,5}` in the oracle vs `badarg` in the rust runtime.
- `records:update/4`'s unknown-field reason is nested in the oracle vs flat in the rust runtime.
- `put_record`'s create form for a **foreign** module's record id is a typed `unsupported` in the
  rust runtime (the running module's own records are supported and pinned).
- Generic opcodes **192/193** cannot be emitted by the installed OTP-29 `erlc`, so no case can pin them.

## A.3 — NIF load/call shapes (owner: `beam_abi` / `beam_rt`)
- A **`.so`-suffixed** `load_nif/2` path loads in the oracle vs `…load_failed` in the rust runtime.
- A **local** NIF call runs the NIF in the oracle vs the `erlang:nif_error` stub in the rust runtime
  (the rust runtime intercepts only **external** dispatch).
- A module that does **not** stub every entry in the library's `ErlNifFunc` loads in the rust runtime
  where the oracle refuses it.

## A.4 — port/driver observable grammar (owner: `beam_rt` / `beam_io`)
- The `{fd,…}` port grammar is disjoint; `{line,N}` returns raw bytes; `eof` is never delivered;
  a non-iodata `port_command/2` is accepted; `port_info(P,registered_name)` is `badarg`; only the
  oracle writes a missing-`{spawn,Cmd}` shell stderr line.

## A.6 — CLI flags & environment (owner: `beam_rt` / the `-eval` evaluator)
- Plain `-extra`/`--` arguments are unusable through the harness; a repeated `-eval` is recorded
  verbatim where `init:get_argument(eval)` -> `error`; an unbound `-eval` variable is a loud error
  vs silent.
- `erlang:halt/1`: `halt([1,2])` prints nothing (exit 1) vs exiting with a `beam_rt_dump:` message;
  only `halt({bad})` -> `badarg` is pinned.
- `+hmax` overflow: the oracle **kills the process** where the rust evaluator raises
  `error:system_limit`; only the tiny bounded-allocation case is pinned.
- A large `-eval` iolist raises `error:out_of_memory` in the rust evaluator, so `iolist_size/1`'s
  bignum promotion is not observable and has no case.
- The `ERL_*FLAGS` merge **order** IS pinned (`flags/env_*`); `ERL_OTP<sysver>_FLAGS` (29 vs 30) and
  an `-eval` carried by `ERL_FLAGS`/`ERL_AFLAGS` are deliberately not.

## A.7 — BIFs, ETS, printing, math (owner: `beam_bifs`, `beam_ets`)
- `binary:match/2,3` with no match: `nomatch` vs `beam_rt: term not found` (exit 1); the agreeing
  no-match shapes are pinned in `binmatch.rs`.
- `lists:usort/2` with an **ascending** comparator returns a wrong result: `[1,2,3]` vs `[1,1,3]`;
  the other comparator shapes agree.
- Plain `~w` float notation uses exponent notation below ~`1.0e-4` / at-or-above `1.0e5` vs plain
  decimal (`short` formatting agrees, pinned in `printing.rs`); `~W` of a binary depth-elides only at
  depth 1 (`~P` and `~W` at depths 0/1/4 agree and are pinned).
- `math:erf/1`/`erfc/1` are not bit-exact (1–5 ULP); every other `math` BIF is pinned bit-exactly.
- ETS query surface: `ets:select/1` rejects the `'$end_of_table'` sentinel; the continuation is a
  bare single-use ref; a `named_table` lookup by name and `ets:info/1,2` on a deleted table are
  `badarg`; `ets:info(T, owner)` is `undefined`.
- ETS access control: the mode is recorded at `ets:new/2` but **not enforced** for a non-owner
  (a private read returns `[]`, a protected/private write returns `true`); `ets:info(T, owner)` is
  `undefined`.
- `erlang:module_loaded/1`: `module_loaded(lists)` is `true` vs `false`.
- `erlang:trace_info/2`: the `all`/`traced` selectors differ (`badarg` vs a value list); only
  `flags`/`tracer` agree.
- `erlang:exit(self(), normal)`: the oracle ignores it (`true`), the rust runtime raises `exit:normal`.
- `erlang:memory/0,1` is pinned **structurally** (A.8): the 9-key order/count,
  `is_integer(memory(total))`, the selection shape and the unknown-key `badarg`; the volatile byte
  totals are never asserted.
- `+Q N` / `ERL_MAX_PORTS` -> `system_info(port_limit)` **is** pinned (the `next_power_of_two` sizing
  matrix); `+Q legacy` / no `+Q` -> 65536.
