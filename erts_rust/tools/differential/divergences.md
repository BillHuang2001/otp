# Observed runtime divergences (handoffs)

Genuine, verified differences between the Rust runtime (`beam_rt`) and the system-ERTS
oracle that the expanded corpus found. None is a capability gap the runtime signals honestly
(an `unsupported` raise / exit 3), so a case pinning one would be a `mismatch` — the gate's
one hard failure. Each is therefore **deliberately not pinned** as a corpus case; the owning
crate below must close the gap, after which a case can be added to the corpus.

(The honest capability gaps — the runtime *does* signal them via its documented
`beam_rt: unsupported: …` / exit-3 convention — are recorded in the allowlist table above,
not here: today only `wire/new_fun_ext_encode`, `proc_signals/statistics_runtime_unsupported`
and the A.9 boot seam remain.)

## A.3 — NIF C ABI (owner: `beam_abi` / `beam_rt` NifHost)
- An atom name first interned *during* a NIF call is not readable back in the same call:
  `enif_make_atom(env, "<new>")` then `enif_get_atom` → `0` in the Rust runtime vs `1` in the
  oracle (same for `enif_get_atom_length`, `enif_make_new_atom(_len)`, `enif_make_atom_len`).
  Cause: `NifHostImpl::atom_name` (`crates/beam_rt/src/nif.rs`) reads only the committed
  `AtomTable`, not the in-flight `PendingInterner` (`intern_atom`). (The fresh name's
  *identity* — one atom, `enif_is_identical` — does agree and is pinned in `nif_abi`.)
- `enif_clear_env` clears a pending exception in the Rust runtime but not in the oracle
  (`crates/beam_abi/src/nif/enif/exception.rs`). (On the *process* env it is illegal C and
  SIGSEGVs the oracle.) The clean-env shape agrees and is pinned.
- Host hooks whose success path has no implementation (a case would be a plain-value
  mismatch):
  `enif_make_ref` (oracle `1`, rust falls back to `enif_make_badarg` → `error:badarg`);
  `enif_term_to_binary` on a valid term (oracle `1`, rust `0`);
  `enif_binary_to_term` on valid ETF (oracle `1`, rust `0`);
  `enif_make_existing_atom` / `enif_make_existing_atom_len` on a **held** name
  (oracle `1`, rust `0` — `NifHost::existing_atom` is a `None` stub,
  `crates/beam_abi/src/nif/host.rs`);
  `enif_whereis_pid` on a **registered** name (oracle `1`, rust `0`).
  Their *failure* shapes (a missing atom/name, malformed ETF) DO agree and are pinned.
- `enif_make_unique_integer` values differ by design (crate counters vs emulator state);
  only the property shapes (`positive` / `monotonic` / default negative) are pinned.
- `enif_system_info` is only partially pinned: `otp_release` (oracle `"29"` vs rust `"30"`),
  `async_threads` / `scheduler_threads` (real counts vs `0`/`1`) and `dirty_scheduler_support`
  (oracle `1` vs rust `0`) are **not** asserted, only the frozen version macros + the
  `thread_support`/`smp_support`/`erts_version` fields.

## A.4 — driver C ABI (owner: `beam_rt` / `beam_io`; `beam_abi` for the timer units)
- `erl_drv_output_term` / `erl_drv_send_term` / `driver_output_term` / `driver_send_term` do
  not deliver their well-formed `ErlDrvTerm` spec to the receiving process: the Rust runtime
  captures the decoded term in `DriverPort::sent` and never sends it (`{data,Bytes}` spec:
  oracle rc `1`, rust rc `0`; the accept/refuse *split* agrees and is pinned).
- The four forms' exact success code on a valid spec differs (oracle `1`, rust `0`).
- `driver_async/5` schedules nothing on the port's pool (oracle a positive handle, rust `-1`).
- `driver_set_timer/2` on an entry **without** a `timeout` callback differs (oracle accepts,
  rust `-1`); the timing shapes (0 ms, large, re-arm, pending/idle read) agree and are pinned.
- `erl_drv_monotonic_time/1` differs by **absolute phase** (oracle non-positive `0`, rust `1`);
  the phase-free properties (monotonicity, unit conversions) agree and are pinned.
- `erl_drv_time_offset/1` differs (oracle a non-zero offset, rust `0`).
- An entry whose only port-command callback is `outputv` never receives `port_command/2`
  (oracle `49`, rust `-1`).
- `driver_enq`/`driver_sizeq` as probed hangs the **oracle**; `driver_async` with a NULL port
  and `erl_drv_output_term(port, NULL, 2)` SIGSEGV the oracle — harness/probe hazards,
  unpinnable, to re-check before treating any as a runtime gap.

## A.5 — distribution wire / ETF (owner: `beam_codec`, `beam_rt`)
- A `[compressed]` image is **not** byte-identical: the framing (`131,80`), the 32-bit
  uncompressed size and the trailing Adler-32 match, but the zlib deflate middle differs.
  Only `binary_to_term(term_to_binary(T, [compressed])) =:= T` round trips are pinned.
- Legacy `PORT_EXT` (102) preserves the port id in the oracle but zeroes it in the Rust runtime.
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
