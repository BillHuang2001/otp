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
- `enif_monitor_process(env, res, pid, &mon)` with a **live** pid creates a real monitor in the
  oracle (`0`, the monitor stored) while the Rust runtime answers `1` unless a host installs the
  `monitor_process` hook, which `beam_rt` does not (`crates/beam_rt/src/nif.rs`). Only the
  `enif_set_pid_undefined` target — which both answer `MONITOR_TARGET_DEAD` — is pinned.
- `enif_thread_name(tid)` for the **scheduler** thread answers `"unknown"` in the oracle
  (`dtid->name == NULL` ⇒ the no-name form) but `NULL` in the Rust runtime; telling that apart from a
  NIF-created unnamed thread needs a longer-running aux thread, so neither shape is pinned.
- `enif_term_size` of an **off-heap/refc** binary (above `ERL_ONHEAP_BINARY_LIMIT`) reports the 8-word
  refc pair in the oracle and the runtime's own counted size in the Rust implementation
  (`crates/beam_abi/src/nif/enif/mem.rs`); only on-heap sizes are pinned.
- `enif_free_iovec` is a **probe hazard**: the only safe shape needs the *internal*
  `ERL_NIF_IOVEC_FLAGS_PREALLOC` (absent from the public `erl_nif.h`), and the freeing form of an
  env-attached vector is UB on the oracle — deliberately not pinned.

## A.4 — driver C ABI (owner: `beam_rt` / `beam_io`; `beam_abi` for the timer units)
- `driver_async_port_key(port)` answers the port's opaque `ErlDrvData`/pointer word in the oracle while
  the Rust runtime's `DriverPort` keeps the `DriverHost` trait default (`0`,
  `crates/beam_abi/src/driver/drv/lifecycle.rs`); even with the hook installed the value is
  pointer-derived, so no equality is pinnable. Owner: `crates/beam_rt` (`DriverHost::async_port_key`).
- The **failure** family — `driver_failure` / `driver_failure_atom` / `_posix` / `_eof`, `driver_exit/2`,
  `erl_drv_set_os_pid/2`, `erl_drv_init_ack/2` — terminates/closes the port in the oracle (the module
  then sees a linked-port exit signal / the `-1` sentinel) while the Rust runtime answers via
  `DriverHost` defaults; the close reason is a linked-port signal, not a VM-stable observable, so none
  is pinned. Owner: `crates/beam_rt` / `crates/beam_io`.
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
- **`driver_async/5` schedules nothing.** Measured: the C's `driver_async(port, &key, invoke,
  data, free)` answers a positive handle (oracle `1`, the queued job's id) and later runs
  `async_invoke`; the Rust runtime keeps the [`beam_abi::DriverHost`] default and answers `-1`.
  A case observing the handle is a plain-value `mismatch` (oracle `49` vs rust `11569`), so the
  scheduler family stays unpinned. Owner: `beam_rt` / `beam_io`.
- `erl_drv_monotonic_time/1` differs by **absolute phase** (oracle non-positive `0`, rust `1`);
  the phase-free properties (monotonicity, unit conversions) agree and are pinned.
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
- `erl_errno_id/1` returns a short POSIX symbol (`enoent`, `einval`, `errno_0`) in the oracle
  but `strerror` prose in the rust runtime.
- `driver_pdl_get_refc(NULL)` SIGSEGVs the oracle, so the `driver_pdl_*` family stays unpinned
  (the PDL calls themselves succeed in both).
- `driver_async_port_key/1` → oracle `3`, rust `0`; `erl_drv_consume_timeslice/2` → oracle
  `1`/`1`, rust `0`/`0`.
- `erl_drv_busy_msgq_limits` writes the handle in the rust runtime instead of leaving the
  bounds; `erl_drv_getenv` sets `size` on an absent key in the rust runtime;
  `driver_dl_error()`'s message length differs (33 vs 25); `driver_dl_open(NULL)` +
  `driver_dl_sym` SIGSEGVs the oracle.

## A.5 — distribution wire / ETF (owner: `beam_codec`, `beam_rt`)
- A `[compressed]` image is byte-identical only for a term whose image compresses to a single
  stored block (e.g. `term_to_binary({ok,42}, [compressed])` matches exactly); beyond that the
  framing (`131,80`), the 32-bit uncompressed size and the trailing Adler-32 still match but the
  zlib deflate middle differs (measured for a larger term, `lists:seq(1,1000)`: oracle 1827 vs
  rust 1802 bytes). Only `binary_to_term(term_to_binary(T, [compressed])) =:= T` round trips are
  pinned.
- The `[used]` byte count of `binary_to_term/2` on a `[compressed]` image differs —
  a consequence of the differing zlib bodies above (the compressed image's size is not
  byte-stable, so it is unpinnable; only the round trip is pinned).
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
- **`port_command/2` on a socket-less inet port — measured and unpinnable, not a divergence.**
  The oracle answers `true` and the port then dies via `driver_failure_posix(EINVAL)` (a following
  `port_info(P,name)` is `undefined`, `port_close/1` raises `badarg`); with a caller-ref prefix
  (`<<0,0,"hi">>`) the oracle still answers `true`, the port survives and no `{error,enotconn}`
  ever arrives (a 500 ms receive times out). `beam_rt` raises `badarg` (`{'EXIT',{badarg,[]}}`) and
  leaves the port alive. Neither the return value, the port's survival nor the reason is a stable
  observable on both sides, so the shape is deliberately unpinned (a case would be a committed
  `mismatch`). The related refusal that *does* agree is pinned (`udp/port_command_on_a_closed_port_is_badarg`).
  Owner: `beam_rt` / `beam_io`.
- **`port_info(P, memory)` after `port_set_data/2`**: the item agrees on a plain port
  (`{memory, N}`), but the word count differs once a term is stored on it (oracle `136` vs rust
  `120`) — an accounting residual, so only the plain-port item and the
  `port_set_data/2`/`port_get_data/1` round trip (which returns `true`, not `ok`) are pinned.
  Owner: `beam_rt`.
- A `driver_output2` **DATA reply** carries a different term shape: the oracle delivers the
  header/body split as an improper list `{data,[2,0,0,0,N|<<Body>>]}`, the Rust producer flattens
  it to the proper list `{data,[2,0,0,0,N,…]}`. The bytes are identical, so the delivery family is
  pinned through `iolist_to_binary/1` (`ram_file/read_write_seek_truncate_replies`), never raw.
  Owner: `beam_abi`.
- **`port_connect/2` to another process only half-lands**: `erlang:port_connect(P, Other)` itself
  agrees (`port_info(P,connected)` is `{connected,Other}` on both), but once **`Other` has exited**
  the oracle kills the port — `port_info(P)` and every item are `undefined`, `port_close/1` raises
  `badarg` — while `beam_rt` still reports the stale `{connected,Pid}` and closes normally. Only the
  `port_connect(P, self())` forms are pinned (`driver/port_connect_answers_true`,
  `driver/erts_internal_port_connect_answers_true`). Owner: `beam_rt`.
- **A datagram send never receives its driver reply** (no case pins it; owner `beam_rt`, the
  port-task reply bridge): `gen_udp:send(S,{127,0,0,1},P,<<"hi">>)` answers `ok` under the oracle, but
  `prim_inet:do_sendto/4`'s `erlang:port_command/2` answers `true` and then blocks in
  `receive {inet_reply,S,Reply,Ref}`, which no port round ever fills — the `-eval` route exits `3`
  with `beam_rt: unsupported: opcode: run: process blocked (no message available)`, and the
  compiled-module route (`beam_rt run`) hangs to its 60 s deadline. The driver's own write path
  (`beam_io`) is in place; the reply bridge is what is missing.
- **The datagram read path is still a gap** (owner `beam_io` / `beam_rt`): an incoming datagram is
  never delivered — a `{active,true}` socket sees `{udp,S,{127,0,0,1},_,<<"abc">>}` under the oracle
  and nothing under the runtime with a datagram injected from outside the VM, and `gen_udp:recv/3`
  raises `{case_clause,{ok,…}}` inside `prim_inet:recvfrom0/3` where the oracle answers
  `{error,einval}`.
- **`gen_tcp:send/2` on a listening socket** answers `{error,einval}` where the oracle answers
  `{error,enotconn}` (owner `beam_io`), and **`gen_tcp:accept/2`** with a short timeout fails with
  `{case_clause,{ok,[]}}` inside `prim_inet:async_accept/2` where the oracle answers
  `{error,timeout}` (owner `beam_rt`, the `inet:getopts/2` reply shape). The *option/control* half of
  both drivers agrees and is pinned (`udp/port_control_*`, `inet_control/*`), including
  `inet:getstat/1`'s counter reply, `gen_tcp:connect/3`, `gen_tcp:listen/2`, the
  `{mode,list|binary}` store and the `INET_REQ_FDOPEN` pre-adoption ladder
  (`udp/port_control_fdopen_refusal_ladder`); only the data path waits.
- The `{fd,…}` port grammar is disjoint: `open_port({fd,1,1},[out])` opens under the oracle and is
  `error:badarg` under `beam_rt`, `open_port({fd,1},[out])` the reverse. `eof` is never delivered by
  `beam_rt`. A non-iodata `port_command/2` raises `badarg` on **both** sides now, but the oracle
  additionally writes a racy timestamped `=ERROR REPORT==== … Bad value on output port` line to
  stdout (measured agreeing on roughly one run in six), so the shape stays unpinned. A missing
  `{spawn,Cmd}` shell diagnostic differs only in `argv[0]` (`sh:` vs `/bin/sh:`); the exit status
  `127` agrees.- The **`{line,N}` half now agrees and is pinned**: the runtime reproduces the oracle's line framing
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
    `[{data,{noeol,<<"abc">>}},{exit_status,0}]` (the flush leads). The same command under
    `{spawn, …}` agrees, so it is confined to `spawn_executable`.

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
- `ets:match_spec_compile/1`: the empty **binary** `<<>>` is `badarg` on both (pinned by
  `specs/match_spec_compile_empty_spec_is_badarg`), but the empty **list** `[]` is `badarg` in the
  oracle and a compiled handle (`#Ref<…>`) in the Rust runtime. Owner: `beam_bifs`.
- ETS access control: the mode set at `ets:new/2` **is** enforced — a non-owner `ets:lookup/2` of a
  `private` table and a non-owner `ets:insert/2` of a `private`/`protected` table raise
  `error:badarg` in both runtimes — but the raised **reason shape** differs: the rust runtime raises
  the bare atom `badarg`, the oracle `{badarg, Stacktrace}`. The *denied* cells are therefore
  unpinned (the allowed half and the ungated `ets:info/1,2` surface are pinned).
  Owner: `beam_bifs` / `beam_ets`.
- `erlang:module_loaded/1`: `module_loaded(lists)` is `true` vs `false`.
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

## A.7 — zlib (owner: `beam_bifs`)
- `zlib:zip/1`'s **compressed image is the encoder's own choice**: for the 100-byte body
  `list_to_binary(lists:seq(1,100))` the oracle's stream is 102 bytes and the Rust runtime's 105
  (both valid DEFLATE — a different block choice), so `byte_size(zlib:zip(…))` is unpinnable and
  only the bodies the oracle reproduces byte for byte carry a byte-exact golden; the 100-byte body
  is pinned by the round-trip identity only (`src/corpus/zlib.rs`).
- The zlib-**wrapped** sibling `zlib:compress/1` diverges the same way on a repetitive body: the
  1 KiB zero body compresses to 17 bytes in the oracle
  (`<<120,156,99,96,24,5,163,96,20,140,84,0,0,4,0,0,1>>`) and 23 in the Rust runtime
  (`<<120,156,237,192,1,1,0,0,0,130,32,255,175,110,72,80,192,171,1,4,0,0,1>>`) — both valid zlib
  streams, so `byte_size(zlib:compress(<<0:8192>>))` (`ok 17` vs `ok 23`) is a plain-value
  `mismatch` and stays unpinned. The tiny body is byte-identical
  (`zlib:compress(<<"abc">>)` → `<<120,156,75,76,74,6,0,2,77,1,39>>` on both), so that form and the
  `uncompress(compress(B)) =:= B` round trip are now pinned in `src/corpus/zlib.rs`.

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
- **Residual divergence: the `Docs` chunk's compressed image.** Every other chunk of the produced
  artifact is byte-identical between the two runtimes — `AtU8`, `Code`, `StrT`, `ImpT`, `ExpT`,
  `FunT`, `LitT`, `Meta`, `LocT`, `Attr`, `CInf`, `Dbgi`, `Line`, `Type` — and `beam_lib:md5/1`
  (which covers `AtU8`/`Code`/`StrT`/`ImpT`/`ExpT`/`FunT`/`LitT`/`Meta`, i.e. the object code
  itself) is the very same `<<212,65,234,56,60,147,88,139,85,190,88,223,28,10,50,95>>` on both
  sides. Only `Docs` differs, and only in its **compressed image**: 4463 bytes on the oracle
  against 4465 on the Rust runtime, while `binary_to_term/1` of either chunk yields the *identical*
  `docs_v1` term (the module doc, the `#{otp_doc_vsn => {1,0,0}, behaviours => [], source_anno =>
  {22,2}}` metadata and all 38 documented functions compare `=:=`). That is the known zlib encoder
  choice (the A.7 zlib entries), not a compiler divergence, so **whole-file byte identity of the
  artifact stays unpinnable** and the case pins the source-derived **export table** of the produced
  image instead of a compiler-release hash.
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
