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

- `enif_compare/2` on **mixed-type / atom** pairs diverges: `enif_compare(1, 1.0)` → oracle `0`,
  the rust runtime non-zero; `enif_compare('alpha', 'beta')` → oracle `-1`, the rust runtime
  non-zero. The rust `enif_compare` (`crates/beam_abi/src/nif/enif/pred.rs`) compares through
  `no_atoms()`, so it cannot rank atoms or mixed numerics like the oracle. No case pins it.
  Owner: `beam_abi`.
- `enif_priv_data/1` (extra-environment family) diverges: the oracle answers the pointer the load
  callback stored, the rust runtime leaves the `NifHost::priv_data` hook at its default and answers
  `NULL` (`0` vs `11`). No case pins it. Owner: `beam_rt` (`NifHost`) / `beam_abi`.

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
  block. `erl_drv_cond_name`/`erl_drv_rwlock_name` on an object created with a NULL name deviate
  too (oracle `"unknown"`, rust `NULL`) and are likewise unpinned.
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
  (`erlang:system_info(endian)` now agrees — `little` in both — and is pinned.)
- `persistent_term:info/0` returns zero-valued counters in the rust runtime (the oracle reports
  live `count`/`memory`).
- `erlang:system_info(tolerant_timeofday)` is an honest rust capability gap (exit 3 /
  `beam_rt: unsupported: …`) — neither a case nor a mismatch.
- The preloaded ∩ registered intersection
  (`[M || M <- erlang:pre_loaded(), lists:member(M, registered())]`) differs — the rust runtime
  registers far fewer names.
