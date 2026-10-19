# Observed runtime divergences (handoffs)

Genuine, verified differences between the Rust runtime (`beam_rt`) and the system-ERTS
oracle that the expanded corpus found. None is a capability gap the runtime signals honestly
(an `unsupported` raise / exit 3), so a case pinning one would be a `mismatch` — the gate's
one hard failure. Each is therefore **deliberately not pinned** as a corpus case; the owning
crate below must close the gap, after which a case can be added to the corpus.

(The honest capability gaps a *case* pins — the runtime signals them via its documented
`beam_rt: unsupported: …` / exit-3 convention — are recorded in the allowlist table above. A few
exit-3 gaps that no case pins are noted below alongside the divergences, for completeness.)
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
- `enif_thread_type()`'s **exact value** differs (oracle `1`, rust `0`) — only the documented-set
  *membership* shape (one of the four thread kinds) is pinned, never the value itself.
- An `ErlNifMonitor`-based probe that hands the runtime a **zeroed** monitor is a **probe hazard**:
  `enif_demonitor_process(env, NULL, &mon)` over one crashes the oracle VM (an exit signal with
  empty stdout), so it is deliberately not pinned — such a case would be a harness artifact, never
  a runtime difference. The zeroed-monitor reads that *do* survive the oracle are pinned instead
  (`enif_compare_monitors` ordering two equal payloads, `enif_make_monitor_term`'s not-a-ref
  answer).

- `enif_dynamic_resource_call(env, module, name, term, call_data)` compares the resource type's
  **name** only in the Rust runtime where the oracle requires the **(module, name)** pair
  (`crates/beam_abi/src/nif/enif/resources.rs`, the documented "substitution 4" — the env carries no
  module identity): a wrong-**module** call the oracle refuses with `1` the Rust name match accepts
  with `0`. The `nif_extra` widening therefore passes the **matching** module atom; a wrong-module
  probe would be a `mismatch`.
- `enif_thread_name(tid)` for the **scheduler** thread answers `"unknown"` in the oracle
  (`dtid->name == NULL` ⇒ the no-name form) but `NULL` in the Rust runtime; telling that apart from a
  NIF-created unnamed thread needs a longer-running aux thread, so neither shape is pinned.
- `enif_term_size` of an **off-heap/refc** binary (above `ERL_ONHEAP_BINARY_LIMIT`) reports the 8-word
  refc pair in the oracle and the runtime's own counted size in the Rust implementation
  (`crates/beam_abi/src/nif/enif/mem.rs`); only on-heap sizes are pinned.
- `enif_term_size` of a **reference** reports `24` bytes in the oracle (the C's `ErtsORefThing`,
  three words) and `32` in the Rust runtime (a four-word internal-ref block); the 1-tuple control
  term agrees at `16`, so only the ref block's size differs. A plain-integer difference, so a case
  pinning it would be a `mismatch`; only `enif_is_ref/1` is pinned. Owner: `beam_abi`
  (`crates/beam_abi/src/nif/enif/resources.rs`) / the internal-ref block in `beam_term`.
- `enif_free_iovec` is a **probe hazard**: the only safe shape needs the *internal*
  `ERL_NIF_IOVEC_FLAGS_PREALLOC` (absent from the public `erl_nif.h`), and the freeing form of an
  env-attached vector is UB on the oracle — deliberately not pinned.

## A.4 — driver C ABI (owner: `beam_rt` / `beam_io`; `beam_abi` for the timer units)
- `driver_async_port_key(port)` answers the port's opaque `ErlDrvData`/pointer word in the oracle while
  the Rust runtime's `DriverPort` keeps the `DriverHost` trait default (`0`,
  `crates/beam_abi/src/driver/drv/lifecycle.rs`); even with the hook installed the value is
  pointer-derived, so no equality is pinnable. Owner: `crates/beam_rt` (`DriverHost::async_port_key`).
- `erl_drv_init_ack/2` keeps the `DriverHost` trait default in the Rust runtime and has **no
  observable at all**: it only acts on a port whose `start` callback answered
  `ERL_DRV_ERROR_WAIT`, so there is no stable single-line shape to pin. Owner: `crates/beam_rt`.
  The driver's own **port-closing** family (`driver_failure`, `driver_failure_atom`/`_posix`/`_eof`,
  `driver_exit/2`) is **not** in this list: both runtimes close the port and deliver the reason, and
  it is pinned by the `driver_c` selectors `40`-`47` — each reason to a trapping owner
  (`driver_failure(P,0)`'s `normal`, the C's eof flag, not reason `0`), `driver_exit/2`'s silent
  close, the `'DOWN'` a port monitor sees, and the post-failure `port_info`/`port_close` refusals.
  Nor are `erl_drv_set_os_pid/2` (selector `72`) or the `driver_async/5` pool (selectors `70`/`71`),
  which are pinned end to end. Owner: `crates/beam_rt` / `crates/beam_io`.
- **`+A 0`**: the oracle **clamps** its async thread pool to one thread, so its `driver_async/5`
  still queues a job (a positive id, measured `1`); `beam_rt` honours a genuine zero-thread pool
  (job id `0`, with `async_invoke` still running and `ready_async` still firing). Behaviourally
  identical apart from the return code, and the positive-id branch is unreachable on the oracle, so
  the shape stays unpinned (`system_info(thread_pool_size)` answers `1` on **both** sides under the
  switch). Owner: `beam_rt`.
- `driver_dl_open` + `driver_dl_close` round trip is an **oracle-side hazard** — the system `erl`
  core-dumps on the close (the refusal half is pinned by the `driver_abi` selector 49), so the round
  trip is deliberately not pinned (a harness artifact, never a runtime difference).
- `erts_ioq_pushqv`'s stale `b_start` on the installed oracle (OTP 29.1.1 / erts-17.1) — also an
  **oracle-side hazard**, not a runtime difference. A `binv[0] == NULL` "create a binary from the
  iovec" vector pushed with `driver_pushqv` onto a **fresh** queue makes the installed oracle write
  `q->v_head->iov_base` *before* decrementing `q->v_head`
  (`erts/emulator/beam/erl_io_queue.c:317`), so the head insert leaves `q->b_head` one entry below
  `q->b_start`; at port close `erts_ioq_clear` frees that stale interior pointer and the VM dies of
  SIGSEGV *after* producing the correct observable (measured ≈1 run in 10; backtrace
  `erts_alcu_free_thr_pref ← erts_ioq_clear ← terminate_port`). Upstream OTP 30 fixed it (commit
  `f4c2a134bc`, "erts: Fix erts_ioq_pushqv overwrite" — decrement `v_head` first, then allocate into
  the decremented slot), which is the form this repo's reference C already carries. The corpus case
  `driver_extra/driver_pushqv_with_a_partial_skip_enqueues_the_tail` therefore presents its vector
  **binary-backed** (`driver_alloc_binary` + `binv[0] = bin`, the shape `driver_pushq_bin` builds for
  itself, `io.c:6874-6890`), which exercises the same skip walk and head insert without reaching the
  defect. A 20-rep sweep of every pinned selector of `driver_extra` and `driver_abi` (3 320 direct
  oracle VMs) plus 5× every `driver/*` and `driver_c/*` case found no other oracle crash or hang.
- (`erl_drv_binary_to_term` is **not** a symbol in `erts/emulator/beam/erl_driver.h` nor in the Rust
  exports, and `driver_async_cancel` does not exist anywhere in OTP — neither can be a corpus case.)
- The `erl_drv_output_term` / `erl_drv_send_term` / `driver_output_term` / `driver_send_term`
  forms' exact **success code** differs (oracle `1` for a queued delivery, rust `0` — the frozen
  `DriverHost::send_term` "accepted" code), so the return value is not pinned; the accept/refuse
  split (selectors `30..38`) and the delivered term are pinned, the latter byte-exactly by the
  delivery family (`driver_abi` selectors `130..137`, images of `{7,42}`, `{ok,7}` and
  `{<<1,2,3>>,5}`; the delivered message is the term itself, never a `{Port, Term}` wrapper,
  `io.c:6162-6176`).
- `erl_drv_monotonic_time/1` differs by **absolute phase** (a non-positive `0` in the oracle, a
  positive value in the rust runtime); the phase-free properties (monotonicity, unit conversions)
  agree and are pinned.
- `erl_drv_time_offset/1` differs (oracle a non-zero offset, rust `0`).
- An `ErlDrvTerm` `ERL_DRV_LIST N` spec differs: `INT 1, INT 2, INT 3, LIST 3` is the
  **improper** list `[1,2|3]` in the oracle (the last popped word is the tail) but the proper
  list `[1,2,3]` in the Rust runtime. No case pins it.
- `driver_enq`/`driver_sizeq` as probed hangs the **oracle**; `driver_async` with a NULL port
  and `erl_drv_output_term(port, NULL, 2)` SIGSEGV the oracle — harness/probe hazards,
  unpinnable, to re-check before treating any as a runtime gap.
- `driver_mk_port(NULL)` returns the nil term in the rust runtime but crashes the **oracle** VM
  (an exit signal with empty stdout) — a probe hazard, unpinnable; excluded from the `driver_extra`
  block.
- `driver_pdl_get_refc(NULL)` SIGSEGVs the oracle, so the `driver_pdl_*` family stays unpinned
  (the PDL calls themselves succeed in both).
- `driver_async_port_key/1` → oracle `3`, rust `0`; `erl_drv_consume_timeslice/2` → oracle `0` then
  `1` (percent `1` then `100`), rust `0`/`0`.
- `erl_drv_busy_msgq_limits` leaves the caller's bounds untouched in the oracle (measured `111`/`222`
  before and after) and writes `SIZE_MAX` into both in the rust runtime; `erl_drv_getenv` on an
  **absent** key answers `-1` and leaves `*size` at `capacity-1` (`63` for a 64-byte buffer) in the
  oracle while the rust runtime answers `-1` and leaves `*size` at the capacity (`64`) — the
  **present**-key round trip agrees (`put=0 present=0 size=3 val=abc` on both) and is pinned by
  `driver_c/erl_drv_putenv_and_getenv_round_trip_inside_the_driver`; `driver_dl_error()` with no
  prior failed `driver_dl_open` is a 33-byte message in the oracle and `NULL` in the rust runtime;
  `driver_dl_open(NULL)` + `driver_dl_sym` SIGSEGVs the oracle.

## A.5 — distribution wire / ETF (owner: `beam_codec`, `beam_rt`)
- A **multi-key atom-keyed** small map has **no pinnable interior order**. The Rust runtime
  keeps the map's construction order — both `term_to_binary(#{a => 1, b => 2})` and
  `term_to_binary(#{b => 2, a => 1})` emit the two keys in source order — while the oracle's
  order for the same term is a *per-VM* order that is canonical **within** one VM (independent
  of the source order) but **not stable across `-eval` VM starts**: `maps:keys(#{a => 1, b => 2})`
  answers `[a,b]` on some starts and `[b,a]` on others, and the map's `MAP_EXT` image flips with
  it. So a byte or `~w` assertion on a multi-key atom-keyed map would be flaky on the *oracle*
  side, not merely on the runtime's; no corpus case pins one — only single-key images (where the
  order is vacuous) and `=:=`/sorted/`maps:size` projections are pinned.
  **Integer**-, **float**-, **binary**- and **list**-keyed maps are order-stable in both runtimes
  and are pinned (`wire.rs`/`wire_extra.rs`, incl. the non-canonical-wire-order canonicalization
  case and the >32-key HAMT round trip). Owner: `beam_codec` / `beam_term`.

## A.7 — process signals (owner: `beam_bifs` / `beam_process`)
- `unlink/1` on a **port** raises `error:badarg` in the Rust runtime (the port's link is not
  removable) where the oracle answers `true`. The port's close signalling itself agrees and is
  pinned (`driver/port_close_delivers_exit_to_a_trapping_owner`,
  `driver/port_close_delivers_down_to_a_port_monitor`).
## A.7 — program result / CLI (owner: `beam_rt` / `beam_engine`, `beam_bifs`)
- A bare (unqualified) name in the `-eval` path resolves differently: `phash2(T)` is `error:undef`
  under the oracle's `-eval` (a local call) but `ok …` under the rust runtime (it resolves the
  `erlang:phash2/1` BIF); `make_tuple(1,2)` behaves the same way. The `erlang:phash2/1`-qualified
  shapes agree and are pinned in `phash.rs`. Owner: `beam_rt` (`-eval` evaluator).

## A.9 — preloaded / boot (owner: `beam_rt`, `beam_bifs`)
- `application:get_all_key/1` / `application:get_key(App, env)`'s **`env` member** differs: the
  oracle injects its runtime-added entries (`logger`, `connect_all`, …) with a per-VM-start
  order, the rust runtime returns the raw, shorter `.app` env. The `{ok,…}` shape and the other
  keys agree and are pinned. Owner: `beam_rt`.
- `application:get_key(erts, vsn)` differs: the oracle answers `undefined` (the emulator is not a
  *loaded* application — the same conclusion as the pinned `loaded_applications_excludes_erts` and
  `application:get_application(erts)` cases), the rust runtime answers `{ok,"17.1"}`. The
  `kernel`/`stdlib` forms agree and are pinned. Owner: `beam_rt`.
- `application:get_application/1` on a **pid** differs: the oracle answers `{ok,App}` for a
  process an application owns and `undefined` for every other process (`application:get_application(self())`
  and `application:get_application(spawn(fun() -> ok end))` are both `undefined` under `-noshell`),
  while the rust runtime raises `error:function_clause` (it runs no application-owned process).
  The **module** form agrees and is pinned (`boot_extra/application_get_application_1_*`).
  Owner: `beam_rt`.
- `erlang:module_info(exports)` → oracle the export table, rust `undef`
  (`lists:module_info(exports)` works but with a different entry order).
- The kernel registered names (`code_server`, `kernel_sup`, `application_controller`, `logger`,
  `user`, `standard_error`, `erl_prim_loader`) ARE live registered pids in both and are pinned
  (`boot_extra/whereis_*_is_registered`).
- Further surfaces found while probing: `code:get_path/0` (differing path lists and **ordering** —
  `hd(code:get_path())` is the kernel `ebin` in the oracle and the stdlib `ebin` in the rust
  runtime, and every rust entry is absolute (`filename:absname(S) =:= S` holds for all) while the
  oracle's are not),
  `init:script_id/0` (a **release-tag** difference like `otp_release`: both answer
  `{"Erlang/OTP", <release>}`, the oracle `"29"` and the rust runtime `"30"`; the self-consistency
  invariant `init:script_id() =:= {"Erlang/OTP", erlang:system_info(otp_release)}` AGREES and is
  pinned by `boot_extra/init_script_id_names_the_running_otp_release`),
  `erl_prim_loader:get_path/0` (`badarg` vs `{ok,Paths}` — the oracle's `erl_prim_loader` is a
  test-only module with no `get_path/0`, while the runtime serves it),
  `erlang:system_info(logical_processors)` (16 vs 14),
  `erlang:system_info(heap_sizes)` (the rust list has extra trailing entries),
  `erlang:module_loaded(lists|kernel)` (`true` vs `false`).
- `erlang:system_info(emu_type)` and `erlang:system_info(debug_compiled)` report the runtime's
  **own build profile**, so their values track the artifact the harness resolved rather than the
  release tag: a **debug** build answers `debug`/`true` where the oracle (and the release build the
  harness prefers) answers `opt`/`false`. Only the profile-**invariant** shape/relation is pinnable,
  and that is exactly what `system_info/emu_type` asserts (`lists:member(…, [opt, debug])` plus the
  `emu_type`/`debug_compiled` consistency invariant; golden `ok true`) — a case asserting a value
  would be a `mismatch` on any tree with no `target/release/beam_rt`, which is what `gate.sh` and a
  clean CI checkout build. `erlang:system_info(compat_rel)`
  is release-dependent like `otp_release` (oracle `29`, rust `30`) — only `is_integer` is pinned.
  Owner: `beam_rt`.
- `erlang:system_info(loaded)` and `erlang:system_info(check_io)`
  are honest capability gaps (rust exit 3 / `beam_rt: unsupported: …`) — neither cases nor
  mismatches; safe to pin as allowlisted cases once wanted.
- `erlang:function_exported/3` answers out of **which modules the side has loaded**, so its answer
  for a non-preloaded module differs: the oracle boots the whole stdlib while this runtime's
  `-eval`/`run` path loads only the 22 preloaded modules, so `function_exported(kernel, module_info, 0)`
  and `function_exported(code, is_loaded, 1)` are `true` in the oracle and `false` in the rust
  runtime (the same applies to any other not-yet-loaded module, e.g. a bare
  `function_exported(lists, sort, 1)`). It agrees for the preloaded modules (`init`, `erlang`), for
  a module that has been `code:ensure_loaded/1`ed (`function_exported(lists, sort, 1)` after
  `code:ensure_loaded(lists)`), and for a `bif.tab`-declared builtin such as `lists:member/2` — the
  corpus case `boot_extra/function_exported_lists_sort_1_is_true` is ensure-load-explicit for that
  reason. Owner: `beam_rt` / `beam_bifs`.
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

## A.8 — introspection (owner: `beam_rt` / `beam_bifs`)
- `erlang:process_info(self(), registered_name)` answers the bare empty list `[]` in the oracle for
  an unregistered process where the rust runtime answers `{registered_name,init}` (its `-eval` root
  process is registered as `init`). Only the **spawned**-process forms (bare `[]`,
  `{trap_exit,false}`, `links` length 0) are pinned. Owner: `beam_rt`.
- `erlang:process_info(P, Item)` for a **spawned** `P` and the position items `catchlevel`,
  `initial_call`, `current_function` and `current_stacktrace` are answered by the oracle but raise
  `beam_rt: unsupported: process_info: <item>` (exit 3) in the rust runtime, whose native floor
  serves them only for `self()` — an honest capability gap, not a mismatch.
- `erlang:system_monitor/0` (oracle `undefined`) and `erlang:system_monitor/1` (oracle
  `error:badarg`) raise `erlang:system_monitor/N is a known BIF that is not implemented` (exit 3) in
  the rust runtime — honest capability gaps, not mismatches.
- `erlang:system_info(check_io)` (oracle a poll-set list) and `erlang:system_info(loaded)` (oracle
  `true`) are exit-3 capability gaps in the rust runtime, as are `build_type` (oracle `opt`) and
  `tolerant_timeofday` (oracle `false`).
- `erlang:fun_info/1` of a **local** fun (and its `module`/`name`/`index`/`uniq`/`new_index`/
  `new_uniq`/`pid` items) is exit-3 (`beam_rt: unsupported: bif`) — the closable gap with its recipe
  in `CASE_DRAFTS.md` §8; the `arity`/`type`/`env` items agree.

## A.1 — terms, records, exceptions (owner: `beam_engine`, `beam_bifs` / `beam_rt`)
- Calling a non-fun (`5(1)`) raises `{badfun,5}` in the oracle vs `badarg` in the rust runtime.
- `put_record`'s create form for a **foreign** module's record id is a typed `unsupported` in the
  rust runtime (the running module's own records are supported and pinned).
- Generic opcodes **192/193** cannot be emitted by the installed OTP-29 `erlc`, so no case can pin them.

## A.3 — NIF load/call shapes (owner: `beam_abi` / `beam_rt`)
- A **`.so`-suffixed** `load_nif/2` path fails in the oracle — it appends the platform suffix again,
  so `load_nif("<dir>/lib.so", 0)` resolves `lib.so.so`, the load fails and the call exits `1` —
  where the rust runtime de-duplicates the suffix and loads the library (the NIF's functions then
  run). Owner: `beam_abi`.
- A **local** NIF call runs the NIF in the oracle vs the `erlang:nif_error` stub in the rust runtime
  (the rust runtime intercepts only **external** dispatch).
- A module that does **not** stub every entry in the library's `ErlNifFunc` loads in the rust runtime
  where the oracle refuses it.

## A.4 — port/driver observable grammar (owner: `beam_rt` / `beam_io`)
- **`port_info(P, memory)` after `port_set_data/2`**: the item agrees on a plain port
  (`{memory,48}` on both), but the word count differs once a term is stored on it (`{memory,160}` in
  the oracle against `{memory,120}` in the runtime for a 3-element list) — an accounting residual, so
  only the plain-port item and the `port_set_data/2`/`port_get_data/1` round trip (which returns
  `true`, not `ok`) are pinned. Owner: `beam_rt`.
- **`port_connect/2` to another process only half-lands**: `erlang:port_connect(P, Other)` itself
  agrees (`port_info(P,connected)` is `{connected,Other}` on both), but once **`Other` has exited**
  the oracle kills the port — `port_info(P)` and every item are `undefined`, `port_close/1` raises
  `badarg` (measured: `{true,undefined,{'EXIT',{badarg,…}}}`) — while `beam_rt` still reports the
  stale `{connected,Pid}` and closes normally (`{true,[{name,"cat"},…,{connected,Pid},…],true}`).
  Only the `port_connect(P, self())` forms are pinned (`driver/port_connect_answers_true`,
  `driver/erts_internal_port_connect_answers_true`). Owner: `beam_rt`.
- The `{fd,…}` port grammar is disjoint: `open_port({fd,1,1},[out])` opens under the oracle and is
  `error:badarg` under `beam_rt`, `open_port({fd,1},[out])` the reverse (measured:
  `{port,{exit,badarg}}` on the oracle against `{{exit,badarg},port}` on the runtime). `eof` is never
  delivered by `beam_rt`. A non-iodata `port_command/2` raises `badarg` on **both** sides now, but
  the oracle additionally writes a racy timestamped `=ERROR REPORT==== … Bad value on output port`
  line to stdout (measured agreeing on roughly one run in six), so the shape stays unpinned. A
  missing `{spawn,Cmd}` shell diagnostic differs only in `argv[0]` (`sh:` vs `/bin/sh:`); the exit
  status `127` agrees.
- The **`{line,N}` half now agrees and is pinned**: the runtime reproduces the oracle's line framing
  exactly — the `{eol, Line}` / `{noeol, Line}` frames, the split at exactly N bytes with no EOL, the
  512-byte default `line` boundary and the EOF no-EOL flush (the CR payload included)
  (`driver/line_option_matrix`, `driver/line_frames_exact_n_noeol_split`,
  `driver/line_full_line_noeol_flush`, `driver/line_default_512_boundary`,
  `driver/line_frame_term_bytes`, `driver/line_eof_noeol_flush_precedes_exit_status`,
  `driver/line_eof_noeol_flush_then_exit_status_payloads` — the two `…flush…` ids name the consuming
  sequence of a selective receive, **not** a mailbox order: with a non-selective receive both
  runtimes deliver `[{exit_status,0},{data,…}]`).
- Two further line/EOF-ordering findings are unpinned because they are plain observables (a case
  pinning one would be a `mismatch`):
  - the **`eof`** option: `printf abc` under `[binary,line,eof,exit_status]` ⇒ oracle
    `[{exit_status,0},{data,{noeol,<<"abc">>}},eof]` vs the Rust runtime the same **without** the
    `{Port, eof}` (the receive times out — no `eof` is delivered);
  - the **`spawn_executable`** path: a child that closes both fds before sleeping
    (`{spawn_executable,"/bin/sh"}` + `{args,["-c","printf abc; exec 1>&- 2>&-; sleep 1"]}`,
    `[binary,line,exit_status]`) ⇒ oracle `[{exit_status,0},{data,{noeol,<<"abc">>}}]` vs Rust
    `[{data,{noeol,<<"abc">>}},{exit_status,0}]` (the flush leads). **It is a race, not a fixed
    order**: the oracle came back exit-first 5/5 and the runtime flipped once in 5, so a case may pin
    only the order-free pair, never the sequence. The same command under `{spawn, …}` agrees, so it
    is confined to `spawn_executable`.

## A.6 — CLI flags & environment (owner: `beam_rt` / the `-eval` evaluator)
- `-extra foo`: the oracle prints a SIGTERM info report and **hangs** (no exit within a 20 s timeout);
  the Rust runtime exits `0`. A **plain argument after `--`** (`-- foo`) is a `mismatch`:
  `init:get_plain_arguments/0` answers `[[102,111,111]]` in the oracle and `[]` in the Rust runtime.
  The `--` neighbourhood itself agrees and is pinned (`flags/double_dash_is_not_recorded`,
  `flags/double_dash_keeps_a_following_switch_a_switch`).
- An **extra `-eval <instr>` instruction** ahead of the probe's own `-eval` is evaluated by the oracle
  first (its side effects are observable — a printing one emits its output ahead of the probe's, and a
  `put(marker,seen)` instruction is read back) while the Rust runtime evaluates only the last `-eval`;
  an unbound `-eval` variable is a loud error vs silent. The *recording* half agrees and is pinned
  (`flags/eval_instruction_extra_is_not_an_init_argument`).
- A **non-switch (positional) argv token** before the probe (`erl -noshell foo -eval <probe>`) stops the
  oracle's switch parsing and enters the interactive shell — the version banner + `Eshell V17.1 …`,
  never the probe, then `*** Terminating erlang (nonode@nohost)` at stdin EOF, exit `0` — where the Rust
  runtime ignores the stray token and RUNS the probe (exit `0`, probe output).
- A `+`-flag with a **missing operand**: bare `+K` takes the banner/`Terminating` route in the oracle
  (exit `0`, no probe output) while the Rust runtime produces empty stdout, exit `0`; bare
  `+S`/`+P`/`+Q` swallow the following `-eval <instr>` as their operand, so both runtimes exit `1`
  with empty stdout and only the **stderr** differs (oracle `bad amount of schedulers -eval` +
  the full `erts_usage()` block vs `beam_rt: bad value "-eval" for switch +S|+P|+Q`) — unpinnable,
  since stderr is compared for expression cases only and the oracle block is huge.
- `-extra foo`: the oracle prints a SIGTERM info report and **hangs** (no exit within a 20 s timeout);
  the Rust runtime exits `0`.
- `-env Var` with a single value (and `-env` with no value pair): the oracle prints its banner and
  terminates (single value) or hangs (no pair); the Rust runtime ignores the switch and runs the probe
  (the two-token `-env Var Value` shape is the separate entry below).
- Unknown/unsupported-flag **stderr** shapes are not byte-comparable (stdout + exit agree): `+k` →
  oracle `unknown flag -k` + the full ~7.4 KB usage, rust `beam_rt: unknown flag -k` (~25 B);
  `+smp enable` → oracle `bad scheduling option -smp` + usage; `-smp disable` → oracle
  `Argument '-smp disable' not supported. Use "+S 1" instead.`. Unpinnable: the abort leaves stdout
  empty (a corpus case needs a non-empty stdout golden) and stderr is compared only for expression cases.
- `+hmax` overflow: the oracle **kills the process** where the rust evaluator raises
  `error:system_limit`; only the tiny bounded-allocation case is pinned.
- A large `-eval` iolist raises `error:out_of_memory` in the rust evaluator, so `iolist_size/1`'s
  bignum promotion is not observable and has no case.
- The `ERL_*FLAGS` merge **order** IS pinned (`flags/env_*`), as is an `-env`/`-eval` switch carried by
  `ERL_FLAGS`/`ERL_AFLAGS` being **recorded** (`flags/env_erl_flags_carries_env_switch`,
  `flags/env_erl_aflags_carries_env_switch`, `flags/env_erl_aflags_carries_eval_instruction`);
  `ERL_OTP<sysver>_FLAGS` (29 vs 30) is not. The carried switch's **effect** does differ: the oracle
  applies it (the carried `-env` variable reaches the child, and a carried
  `-eval persistent_term:put(k,v)` runs) where the Rust runtime only records it — the variable is
  `false` under `os:getenv/1` and the `-eval` does not run (`ERL_AFLAGS='-eval persistent_term:put(k,v)'`
  ⇒ oracle `v`, rust `error:badarg`; `ERL_FLAGS='-eval 1+1'` ⇒ oracle runs it, rust prints nothing).
  Owner: `beam_rt`.
- `-config <missing path>`: both runtimes exit `1` with empty stdout, but the **stderr text
  differs** — the oracle writes `Could not start kernel pid (application_controller) ("error in
  config file \"<path>\" (none): configuration file not found")` plus a crash-dump line, the Rust
  runtime `beam_rt: error cannot read config file <path>`. An `Eval` case compares stderr (for
  expression cases) so it would be a `mismatch`, and a `Command` case cannot pin exit `1` at all
  (its golden requires non-empty stdout and exit `0`): the shape is unpinnable by the harness.
  Owner: `beam_rt`.
- A malformed atom-table size is refused at a **different exit code with different stderr**: `+t 2`
  exits `1` in the oracle with `bad atom table size 2` + the `beam.smp` usage block on stderr
  (7331 bytes), and exits `2` in the Rust runtime with `beam_rt: error bad value "2" for switch +t`
  + the `beam_rt` usage — the crate's documented uniform malformed-value convention (`+t 1000` has
  the same shape). The `-t 2` / `-t 1000` spellings are **not** `+t` aliases on either runtime
  (both accept them silently, exit `0`), so only the `+t` form diverges. Owner: `beam_rt`
  (`crates/beam_rt/src/cli/flags.rs`, `parse_atom_table_size`).
- `-env Var Value`: both runtimes accept the switch (exit `0`), but only the oracle **applies** it
  to the spawned process environment: `os:getenv("Var")` is the value (the charlist `"hello"` for
  `-env Var hello`) in the oracle and `false` in the Rust runtime, which consumes the two tokens
  and drops them. `init:get_argument(env)` and `init:get_argument(Var)` are `error` on both, so only
  that recording half is pinnable (`flags/env_switch_is_not_an_init_argument`,
  `flags/env_switch_value_is_not_an_init_argument`); the recording shapes that DO agree are pinned
  (`flags/env_switch_repeated_last_value_wins` for last-value-wins, and the value list stopping at the
  next switch in `flags/env_switch_value_does_not_swallow_the_next_switch`, plus the
  `-env Name=VALUE Value` split in `flags/env_switch_name_containing_equals`). Owner: `beam_rt`.

## A.7 — BIFs, ETS, printing, math (owner: `beam_bifs`, `beam_ets`)
- ETS query surface: `ets:select/1` rejects the `'$end_of_table'` sentinel; the continuation is a
  bare single-use ref where the oracle answers its own opaque `{Ref, _, _, Ref, _, _}` handle (so
  only the `{Matches, continuation}` split and the ref-ness are pinned).
- ETS access control: the mode set at `ets:new/2` **is** enforced — a non-owner `ets:lookup/2` of a
  `private` table and a non-owner `ets:insert/2` of a `private`/`protected` table raise
  `error:badarg` in both runtimes — but the raised **reason shape** differs: the rust runtime raises
  the bare atom `badarg`, the oracle `{badarg, Stacktrace}`. The *denied* cells are therefore
  unpinned (the allowed half and the ungated `ets:info/1,2` surface are pinned).
  Owner: `beam_bifs` / `beam_ets`.
- `erlang:module_loaded/1`: `module_loaded(lists)` is `true` vs `false`.
- `erlang:delete_module/1` **unloads**, where the oracle only *retires* code: this runtime has no
  old-code slot, so the two runtimes agree on the first delete and part ways *after* it. Measured
  (OTP 29.1.1 / `erts-17.1`): a non-atom is `error:badarg` and an unheld name is the atom
  `undefined` on both, and the first `erlang:delete_module(lists)` is `true` with the code server
  surviving on both — pinned by `bifs/delete_module_of_a_non_atom_is_badarg`,
  `bifs/delete_module_of_an_unheld_name_is_undefined`,
  `bifs/delete_module_of_a_held_module_is_true` and
  `bifs/delete_module_keeps_the_path_based_code_which_answer`. After that delete the oracle answers
  `badarg` to a **second** delete of the same name while this runtime answers `undefined`, and
  `erlang:module_loaded(lists)` / `erlang:check_old_code(lists)` stay `true` there (curr demoted to
  old) against `false` here. No case reads a load-state observable after a delete, and no case
  deletes twice. Owner: `beam_rt`.
- `code:is_loaded/1` answers out of the code server's **load state** in both runtimes — the
  `{file, Path}` pair for a loaded module, the atom `false` for one that exists on the code path but
  was never loaded (`code:is_loaded(dets)` and `code:is_loaded(lists)` are both `false` in the rust
  runtime, and `dets` is `false` in the oracle too) — so the answers differ only where the **boot
  set** does: the oracle boots the whole stdlib, so `code:is_loaded(lists)` / `code:is_loaded(kernel)`
  are `{file, …}` there and `false` in a runtime whose `-eval`/`run` path loads only the 22 preloaded
  modules. The affected corpus cases (`bifs/module_loaded_lists_shape`,
  `boot_extra/code_is_loaded_lists_is_a_file_pair`, `boot_extra/code_is_loaded_kernel_is_file`) are
  therefore ensure-load-explicit and pin the loaded answer. Owner: `beam_rt`.
- `erlang:trace_info/2`: the `all`/`traced`/`match_spec` selectors differ (`error:badarg` in the
  oracle vs a value in the rust runtime); only `flags`/`tracer` agree.
- `erlang:trace(self(), true, [call, {tracer, self()}])` is `ok 1` in the oracle but `error:badarg`
  in the rust runtime (the `{tracer, Pid}` option is not accepted). Owner: `beam_rt`
  (`erlang:trace/3`).
- `erlang:statistics(runtime)`'s two counters are per-VM, so their **values** differ by construction
  (e.g. `{338,338}` vs `{491,491}`); only the 2-tuple shape is pinned, never the values.
- `erlang:exit(self(), normal)`: the oracle ignores it (`true`), the rust runtime raises `exit:normal`.
- `erlang:memory/0,1` is pinned **structurally** (A.8): the 9-key order/count,
  `is_integer(memory(total))`, the selection shape and the unknown-key `badarg`; the volatile byte
  totals are never asserted.
- `+Q N` / `ERL_MAX_PORTS` -> `system_info(port_limit)` **is** pinned (the `next_power_of_two` sizing
  matrix); `+Q legacy` / no `+Q` -> 65536.

## A.7 — erl_ddll / decode_packet (owner: `beam_bifs` / `beam_io`)
- `erl_ddll`'s refusal shapes all agree and are pinned: the missing-object / missing-directory /
  non-string-name `{error,{open_error,-10}}`, the not-loaded `{error,not_loaded}`, the
  `linked_in_driver` answers of `try_load(".", "tcp_inet", [])` and `try_unload("tcp_inet", [])`,
  and every `info/2` `error:badarg` refusal (`ddll.rs`).- `erlang:decode_packet/3` shows **no observed divergence**: the four integer forms (`0`/`raw`,
  `1`, `2`, `4`) and all twelve named types (`raw`, `asn1`, `sunrm`, `cdr`, `fcgi`, `line`, `tpkt`,
  `ssl_tls`, `http`, `httph`, `http_bin`, `httph_bin`), together with their `{more,N}` /
  `{more,undefined}` / `{error,invalid}` / `error:badarg` shapes, agree and are pinned in
  `decode_packet.rs` (`ftp` is not a packet type — `error:badarg` on both).
## A.7 — re (owner: `beam_bifs`; the underlying literal parser is `beam_rt`)
- The `<<"…"/utf8>>` **binary literal** (a string segment carrying the `utf8` type) cannot be
  constructed by the runtime's `-eval` path: `<<"åäö"/utf8>>` — and any use of it, e.g.
  `byte_size(<<"åäö"/utf8>>)` — raises a plain `error:badarg` where the oracle yields the UTF-8
  binary. Two `re` cases promoted from `crates/beam_bifs/src/re/oracle.rs` use that literal as a
  *subject*, so they are **not** pinned (a plain-value `mismatch`, not an honest gap) and are
  deliberately absent from `src/corpus/re.rs`:
  - `unicode/utf8-binary-subject` — `re:run(<<"åäö"/utf8>>,<<"ä"/utf8>>,[unicode])`
    (oracle `{match,[{2,2}]}`, rust `error:badarg`);
  - `non-unicode/dot-is-a-byte` — `re:run(<<"åäö"/utf8>>,".")`
    (oracle `{match,[{0,1}]}`, rust `error:badarg`).
  The gap is in literal **construction**, not in the `re` engine: both cases agree once the
  literal exists, and the byte-identical integer-segment subject (`<<195,165,195,164,195,182>>`)
  *is* evaluable and agrees (so the equivalent of each can be pinned today, and the two source
  expressions verbatim once the literal builds). The source's own crate-level sweep for both
  (`crates/beam_bifs/src/re/oracle.rs`) still passes because it builds the subject as raw bytes
  rather than through the `-eval` literal parser.

## A.9 — boot/init (owner: beam_rt)

- The runtime drives a real boot chain, so `init:script_id/0`, `whereis/1`, `application:*`,
  `code:*` and the registered-name manifest are observed live rather than stubbed; the remaining
  boot/init divergences are recorded with the other introspection entries above (A.8 / A.9
  "preloaded / boot").

## A.9 — code server (owner: `beam_rt`)
- **Sticky rule.** `code:load_binary/3` is served, and its refusal shapes agree (a corrupt image is
  the frozen `{error,badfile}`; a valid image of a module already *loaded from* a sticky directory is
  `{error,sticky_directory}` — both pinned by `boot_extra/code_load_binary_shapes`). The divergence is
  the **not-yet-loaded** rule: for a module that merely **resolves under** the installation the oracle
  loads the image while the Rust runtime refuses it —
  `begin {Mod,Bin,_}=code:get_object_code(base64), {Mod, code:load_binary(Mod,"base64.beam",Bin)} end`
  ⇒ oracle `{base64,{module,base64}}` (exit `0`), Rust `{base64,{error,sticky_directory}}` (exit `0`):
  the Rust rule refuses a module *resolving under* a sticky dir where the oracle refuses only a module
  *already loaded from* one. Needs `-kernel logger_level none` to be deterministic (the oracle's
  `beam_load.c` "resides in sticky dir" report races onto stdout).

## A.9 — in-process compile of a large stdlib module (owner: `beam_bifs` / `beam_rt`)
- `selfhost/compile_file_of_a_module_folding_over_an_ordered_iterator` **is in the corpus and
  agrees**: it compiles `lib/stdlib/src/timer.erl` (1017 source lines, addressed repo-relatively —
  the harness spawns both runtimes with the repository root as their cwd) through `-eval` with
  `[binary, no_spawn_compiler_process, deterministic]`. Measured at the commit that landed it: rust
  ≈ **2.3 s**, oracle ≈ **0.3 s**, both `[AGREE]` over repeated runs. The former entry's ≈ 615 s
  cost and its `gc_walk::Walk::seed` ≈ 72 % hotspot no longer reproduce (the whole compile now costs
  2.3 s); that hotspot was **not** re-measured.
- The case is the group's only **expression** case because the module-run argv shape cannot carry
  this source at an acceptable cost: the same in-process compile driven through the runtime's `run`
  entry point costs ≈ **43 s**, and through the *spawned* compiler process (the `sched_case` default
  route) ≈ **36.6 s**, against ≈ 2.3 s in the calling process — the same penalty the `ordsets`
  sibling shows (7.6 s versus 0.8 s). What is slow is work executed in a freshly spawned process,
  and the module-run entry process is one. Owner: `beam_rt` / `beam_engine` (execution-tier
  selection for spawned processes).
- **The default (spawned-compiler) `[binary]` route agrees but no case can carry it**:
  `compile:file("lib/stdlib/src/timer.erl", [binary, return_errors])` answers `ok timer 13200` on
  **both** sides, in 119–211 s on the runtime against 0.7 s on the oracle (six serial runs, 158/127/
  211/169/130/119 s) — the spawned-process cost above, not a divergence. Owner: `beam_rt` /
  `beam_engine`. **Load flake, not part of the agreement**: under a heavy machine load (several
  concurrent workspace builds) this same route has twice died with exit `139` (SIGSEGV, empty
  stdout) after 202 s / 328 s, and it runs clean whenever the machine is quiet — so a case pinning it
  would flake, and the crash itself is a runtime robustness question for the owner, not a harness
  artifact.
- **`no_spawn_compiler_process` + `[binary]` on the same source is the cheap, honest route**: ≈2.3 s,
  already a case (`selfhost/compile_file_of_a_module_folding_over_an_ordered_iterator`).
- **No case pins a whole-file byte count or a hash value.** The artifact's `beam_lib:md5/1` image and
  every md5-significant chunk are identical between the two runtimes, but a hash golden is
  **release-dependent** (CI's oracle is OTP 28 while every golden here is measured on OTP 29), so the
  artifact cases pin relations and structures instead — the produced `Code` chunk against the
  module's own image, the chunk set, the `debug_info`/`attributes`/`Docs` shape and the export table.
  Owner: `beam_bifs`.
## A.2 — term/ETF image (owner: `beam_bifs`)
- `term_to_binary(Term, [local])` is served: the image is `131`, `LOCAL_EXT` (121), a 4-byte local
  node **hash**, then the ordinary ETF body. The hash is randomised per VM start on **both** sides,
  so `etf/term_to_binary_local_masks_the_node_hash` masks exactly those 4 bytes and compares the rest
  of the image byte-for-byte (and `etf/term_to_binary_local_replaces_the_local_node_atom` pins the
  local-node marker, `NEW_PID_EXT` followed by `NIL_EXT`); no hash, pid id, serial or creation value
  is ever asserted.
- `binary_to_term/1` of a `LOCAL_EXT` image whose 4-byte hash is **not** this VM's hash: the oracle
  refuses with `error:badarg` (`<<131,121,0,0,0,0,97,1>>` ⇒ `badarg`), the Rust runtime decodes it
  (`1`). A plain-value difference, so no case pins it. Owner: `beam_bifs` / `beam_codec` (the BIF's
  decoder is called without a local hash, so a foreign hash is accepted).
- A **multi-key map** image (and its `~w`) is not pinnable as a byte golden: `MAP_EXT` mandates no key
  order and the oracle's own flatmap/HAMT order varies between VM starts for the same map, so the
  corpus classifies a map-valued result into sorted key/pair lists and probes with `Probe::Value`.
  An encoding-order choice, not a semantic divergence. Owner: `beam_codec` / `beam_bifs`.

## A.4 — driver C ABI: the zero-length delivery (owner: `beam_rt`)
- **A wholly zero-length delivery never arrives in the Rust runtime, so it stays unpinned.**
  Measured with a `cc`-built driver: `driver_output2(port, NULL, 0, NULL, 0)` delivers
  `{Port,{data,[]}}` to the owner in the oracle and **nothing** in `beam_rt`, and a zero-length
  `driver_output(port, "", 0)` delivers `{Port,{data,<<>>}}` there and nothing here — the runtime's
  `PortRegistry::fold_notifications` folds an empty payload away. The `driver_c` empty-body selector
  (`33`) does **not** cover this: its header is non-empty, so the term it pins is `[104,100|<<>>]`,
  never an empty payload. Owner: `beam_rt`.

## A.5 — distribution / ETF wire (owner: `beam_rt`, `beam_codec`)

### Live cross-runtime traffic (measured)
One Rust node and one system node were driven by hand (a `beam_rt -sname <name>` runtime and a
system `erl -sname <peer>`), `net_kernel:verbose(3)` on the system side, `epmd -names` and `ss -tn`
for the transport state.
- **Both directions complete.** The ERTS → Rust direction was audited fresh: the system side's
  `net_adm:ping/1` answers `pong` in 28–200 ms, `verbose(3)` shows the full
  `{auto_connect,…}` → `{init_setup_check,…}` → `{setup_check,…}` → `{setup,…}` →
  `{dist_ctrlr,…}` → `{nodeup,…}` ladder with no `setup_timer_timeout`, and `ss -tn` shows
  Recv-Q/Send-Q zero on both sides. The earlier `pang` / `setup_timer_timeout` symptom was a
  **missing `net_kernel` service**, not a wire defect: `net_adm:ping/1` is a
  `gen:call({net_kernel,Node}, '$gen_call', {is_auth,node()}, infinity)`, so with no process holding
  the name the request was dropped and the caller parked in an untimed receive until the oracle's
  setup timeout fired.
- **The Rust node is a published node, not hidden**: it appears in the peer's `nodes/0`, a
  `nodes(hidden)` call stays `[]`, and `DistNode::hidden` defaults to `false`.
- **Both directions carry user traffic**: `{sink,Peer} ! Msg` reaches a registered process on the
  other side and the peer's reply reaches the Rust side's plain `receive`; `erlang:monitor_node/2`
  answers `true` and both `{nodeup,…}` and `{nodedown,…}` are genuinely delivered; and
  `erlang:send/2` to a remote registered name auto-dials a reachable node.
- **No corpus case can express cross-runtime traffic.** `harness::execute` runs the oracle leg to
  completion **before** the Rust leg and compares each child's own stdout + exit status, so no case
  can carry a message between the two runtimes; a `Command` oracle script could only **hard-code** a
  cross-runtime observation (exactly the tautology the `Command`-kind rule forbids), and having the
  oracle child spawn `beam_rt` would bake an artifact path into an argv — against
  `runtime::resolve_rust`'s `$BEAM_RT`-else-workspace-relative resolution.
- The genuine cross-runtime property is pinned instead by the crate-local
  `crates/beam_rt/tests/dist_two_vm.rs` (two `#[ignore]`d tests, a private `epmd`, both sides
  asserted).
- **A per-side two-node pair IS expressible and is pinned**: `CommandRun.peer: Option<PeerRun>`
  starts a second process of *that side's own* runtime (the Rust leg a harness-spawned `beam_rt`, the
  oracle leg the system `erl`'s own `peer:start_link/1`), both legs isolated in their own process
  group under a per-invocation nonce, and a peer that never becomes ready is a loud `Skipped`.
  `dist/live_self_pair_handshake_and_message` therefore compares reachable-node behaviour
  symmetrically: the handshake (`pong`), the published-node count (`nodes(hidden)` empty),
  one message each way twice, and `{nodedown,_}`. What remains inexpressible is a leg that talks to
  the **other** runtime — a peer leg is always the same runtime, precisely because the two legs never
  coexist.

### Distribution capability gaps and plain-value divergences (measured)
- **A message to `net_kernel` that is not a `{'$gen_call', From, Request}` tuple is FATAL**: a peer
  sending a plain term to `{net_kernel, Node}` kills the runtime with exit `3` (only the `up` line
  has been printed), and a stray term is not catchable from `-eval`. Owner: `beam_rt`.
- **`net_kernel`: `<request> is not served`** (e.g. `monitor_nodes/2`) and **`erlang:monitor_node/3`**
  both exit `3` — honest capability gaps, never mismatches. Owner: `beam_rt`.
- `erlang:send({RegName, UnreachableNode}, Msg)` **raises**
  (`beam_rt: error distribution: no established connection to node …`) where the oracle silently
  drops the message and returns it. Owner: `beam_rt`.
- `erl_distribution:start_link()` on an already-distributed node answers `ignore` where the oracle
  answers `{error,{already_started,<pid>}}`. Owner: `beam_rt`.
- `net_adm:ping/1` with a **non-atom** is `badarg` where the oracle raises `function_clause`. Owner:
  `beam_rt`.
- `catch net_kernel:node()`'s **stacktrace** is one frame against the oracle's eight; the `undef`
  class and reason themselves agree (`net_kernel:node()` is `undef` on **both** sides — not a gap), so
  the probe wrapper pins them. Owner: `beam_rt`.
- The adjacent shapes agree and are pinned: off the `-sname`/`-name` path `node()` is
  `nonode@nohost`, `is_alive()` is `false` and `nodes()` is `[]`, and with `-sname` the
  node-name/aliveness shapes agree.

### ETF decode bound (measured)
- **A 3 000-deep nested list decodes on the oracle and raises here.**
  `binary_to_term(term_to_binary(T))` of a 3 000-deep right-nested list is `ok true` under the
  oracle and `exception error system_limit` in the Rust runtime. A case pinning it would be a
  `mismatch`, so it stays out of the corpus. Owner: `beam_codec` (the decoder's nesting bound).

## A.9 — in-process compile: measured routes that cannot be cases (owner: `beam_bifs`, `beam_engine`/`beam_rt`)
Each route below was measured while the `selfhost` artifact block was added. None is a divergence:
both sides matched, and the route was not turned into a case for the reason given.
- **`compile:string/2,3` and `compile:forms(…, [report])` are `undef` on both sides** — no such export
  exists in the installed OTP 29 stdlib — so there is no observable at all. Nothing to close; the
  owner-if-closed is `beam_bifs`.
- **A `{outdir,…}` + `-include` of a kernel/stdlib header is `{badmatch,error}` on both sides
  *without* `{i,…}`.** With `[{i,"lib/kernel/include"},{d,'BEFORE_REWRITE',true}]` both sides compile
  (≈16 s each) and produce **identical** export tables, so there is no discriminating observable; it
  was dropped for cost. Owner of the cost: `beam_engine`/`beam_rt` (the freshly spawned compiler
  process path).
- **A "path-free artifact" assertion is false on both sides.** `binary:match(Bin, <<"lib/stdlib/src">>)`
  finds the `epp`-injected `file` attribute at offset 4963, and `deterministic` does **not** strip an
  input `file` attribute; likewise two independently compiled artifacts differ in `byte_size` even
  under `deterministic`, so only structural identities are usable as a relation. Owner-if-ever:
  `beam_bifs` (the compile surface).
- **`compile:forms/2` IS supported by `beam_rt`** — the two landed cases compile *and execute*
  through it — so the "measure first, drop if unsupported" rule did not bite there (recorded so a
  future pass does not re-measure it as a gap).
