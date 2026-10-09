# Observed runtime divergences (handoffs)

Genuine, verified differences between the Rust runtime (`beam_rt`) and the system-ERTS
oracle that the expanded corpus found. None is a capability gap the runtime signals honestly
(an `unsupported` raise / exit 3), so a case pinning one would be a `mismatch` — the gate's
one hard failure. Each is therefore **deliberately not pinned** as a corpus case; the owning
crate below must close the gap, after which a case can be added to the corpus.

(The honest capability gaps — the runtime *does* signal them via its documented
`beam_rt: unsupported: …` / exit-3 convention — are recorded in the allowlist table above,
not here: today only `wire/new_fun_ext_encode` and
`proc_signals/statistics_runtime_unsupported` remain.)
## A.3 — NIF C ABI (owner: `beam_abi` / `beam_rt` NifHost)
- `enif_clear_env` clears a pending exception in the Rust runtime but not in the oracle
  (`crates/beam_abi/src/nif/enif/exception.rs`). (On the *process* env it is illegal C and
  SIGSEGVs the oracle.) The clean-env shape agrees and is pinned.
- Host hooks whose success path has no implementation (a case would be a plain-value
  mismatch):
  `enif_make_ref` (oracle a fresh ref → `1`, rust falls back to `enif_make_badarg` → `error:badarg`);
  `enif_term_to_binary` on a valid term (oracle `1`, rust `0`);
  `enif_binary_to_term` on a valid encoding (oracle the byte count consumed — `3` for the
  3-byte `<<131,97,42>>` — rust `0`);
  `enif_make_existing_atom` / `enif_make_existing_atom_len` on a name held in the **committed**
  atom table but **not** interned during the call, e.g. `enif_make_existing_atom(env, "ok", …)`
  (oracle `1`, rust `0` — `NifHost::existing_atom` is a `None` stub,
  `crates/beam_abi/src/nif/host.rs`; this is distinct from the within-call hit, which agrees and
  is pinned in `nif_abi`);
  `enif_whereis_pid` on a **registered** name (oracle `1`, rust `0`).
  Their *failure* shapes (a missing atom/name, malformed ETF) DO agree and are pinned.
- `enif_make_unique_integer` values differ by design (crate counters vs emulator state);
  only the property shapes (`positive` / `monotonic` / default negative) are pinned.
- `enif_system_info` is only partially pinned: `otp_release` (oracle `"29"` vs rust `"30"`),
  `async_threads` / `scheduler_threads` (real counts vs `0`/`1`) and `dirty_scheduler_support`
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
- `erl_errno_id/1` returns a short POSIX symbol (`enoent`, `einval`, `errno_0`) in the oracle
  but `strerror` prose in the rust runtime.
- `driver_pdl_*` succeed in the oracle but return `NULL` in the rust runtime;
  `driver_pdl_get_refc(NULL)` SIGSEGVs the oracle, so the family stays unpinned.
- `get_port_flags` → oracle `1`, rust `0`; `set_port_control_flags` has no effect in the rust
  runtime.
- `driver_async_port_key/1` → oracle `3`, rust `0`; `erl_drv_consume_timeslice/2` → oracle
  `1`/`1`, rust `0`/`0`.
- The `driver_output*` return code is the delivered byte count in the oracle but `0` in the
  rust runtime (the rust seam ignores `driver_output_binary`'s return value).
- `driver_peekq`/`driver_deq`/`driver_peekqv` differ (port-handle/queue-shape).
- `erl_drv_busy_msgq_limits` writes the handle in the rust runtime instead of leaving the
  bounds; `erl_drv_getenv` sets `size` on an absent key in the rust runtime;
  `driver_dl_error()`'s message length differs (33 vs 25); `driver_dl_open(NULL)` +
  `driver_dl_sym` SIGSEGVs the oracle.

## A.5 — distribution wire / ETF (owner: `beam_codec`, `beam_rt`)
- A `[compressed]` image is **not** byte-identical: the framing (`131,80`), the 32-bit
  uncompressed size and the trailing Adler-32 match, but the zlib deflate middle differs.
  Only `binary_to_term(term_to_binary(T, [compressed])) =:= T` round trips are pinned.
- Legacy `PORT_EXT` (102) preserves the port id in the oracle but zeroes it in the Rust runtime.
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

## A.9 — preloaded / boot (owner: `beam_rt`, `beam_bifs`)
- `code:get_mode/0` → oracle `interactive`, rust `badarg`.
- `init:get_argument(root|home|progname)` → oracle `{ok,[[Path]]}`, rust `error`.
- `application:get_application/0` and `/1` → oracle `undefined` / `{ok,App}`, rust `badarg`.
- `erlang:module_info(exports)` → oracle the export table, rust `undef`
  (`lists:module_info(exports)` works but with a different entry order).
- `whereis(code_server|erl_prim_loader|kernel_sup)` → oracle a pid, rust `undefined`.
- Further surfaces found while probing: `code:lib_dir/2` (a path vs `badarg`),
  `code:get_path/0` (differing path lists), `init:script_id/0` (`{"Erlang/OTP","29"}` vs
  `badarg`), `application:get_key(stdlib, mod)` (`{ok,[]}` vs `undefined`),
  `erl_prim_loader:get_path/0` (`{ok,Paths}` vs `badarg`),
  `erlang:system_info(logical_processors)` (16 vs 14),
  `erlang:system_info(heap_sizes)` (the rust list has an extra entry),
  `erlang:module_loaded(kernel)` (`true` vs `false`).
- `erlang:check_old_code/1`, `erlang:system_info(loaded)` and `erlang:system_info(check_io)`
  are honest capability gaps (rust exit 3 / `beam_rt: unsupported: …`) — neither cases nor
  mismatches; safe to pin as allowlisted cases once wanted.
- `erlang:function_exported/3` is `false` in the rust runtime for `kernel` and `code` (the
  oracle says `true`); it agrees for the preloaded modules and for the file-loaded `lists`.
- `erlang:system_info(schedulers|dirty_cpu_schedulers|dirty_io_schedulers)` → oracle
  `16`/`16`/`10`, rust `14`/`0`/`0`.
- `erlang:system_info(endian)` and `erlang:system_info(build_type)` raise `badarg` in the rust
  runtime (oracle `little` / `opt`).
- `persistent_term:info/0` returns zero-valued counters in the rust runtime (the oracle reports
  live `count`/`memory`).
- `erlang:system_info(tolerant_timeofday)` is an honest rust capability gap (exit 3 /
  `beam_rt: unsupported: …`) — neither a case nor a mismatch.
- The preloaded ∩ registered intersection
  (`[M || M <- erlang:pre_loaded(), lists:member(M, registered())]`) differs — the rust runtime
  registers far fewer names.
