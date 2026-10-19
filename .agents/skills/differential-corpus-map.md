---
name: differential-corpus-map
description: The per-group inventory of the differential conformance corpus (`erts_rust/tools/differential/src/corpus/*.rs`) — every group's id prefix, case count and the behaviours it pins, plus the pinning traps that are easy to get wrong. Load this instead of reading the crate CONTEXT.md routing table in full; it is that table's detail.
---
# The differential corpus — per-group inventory
Each group is a module `src/corpus/<group>.rs` exposing `group() -> Vec<Case>`, concatenated by
`corpus::mod::build_corpus()`. A case id is `<prefix>/<name>`, where the prefix is a *subdivision*
and NOT the file name (one file may emit several prefixes). Counts below are for the current corpus
(3322 cases total, one allowlisted gap, `wire/new_fun_ext_encode`); re-derive them with
`erts_rust/target/release/differential list 2>/dev/null | grep -c '^<prefix>/'` rather than trusting
this file, which drifts on every corpus round.
## A.1 — language semantics
- `terms.rs` — ordering/equality/arithmetic, incl. atom order **by code point** (`erts_cmp_atoms`),
  not by length: `terms/atom_order_is_by_name`, `terms/atom_order_is_not_by_length`.
- `semantics.rs` — precedence, bignum, `==`/`=:=`/`=/=`, ordering, `case`/`if`/`try`, funs incl.
  `closure_*`, comprehensions, maps/tuples/binaries.
- `errors.rs` — `throw`/`exit`/`error` classes + reasons, `badarg`/`badarith`, structured `try`
  reasons, `function_clause`/`undef`, and the synthesized callee-head `undef` stacktrace head item
  `{Module,Function,ArgList,Location}` with a real arg LIST on the `-eval` route
  (`errors/undef_head_item_{arg_list,shape}_from_apply`, `errors/undef_head_item_arg_list_from_call_ext`);
  the compiled-code route is pinned from `modules.rs`.
- `ordered_walk.rs` — the `ets` `ordered_set` walk across the `ORDER`=32 node split
  (`ets:first/1`+`next/2`, `ets:last/1`+`prev/2` over >2×ORDER keys interleaved with insert/delete),
  the absent-key range-bound anchor and the terminal `'$end_of_table'`.
## A.2 — `.beam`/ETF images
- `etf.rs` (84 cases) — `term_to_binary` bytes + `binary_to_term` round trips incl. local pid/ref
  identity; the `[local]` `LOCAL_EXT` image with the per-VM 4-byte node hash masked
  (`etf/term_to_binary_local_masks_the_node_hash`, `…_replaces_the_local_node_atom`); the
  `{minor_version,0}` legacy `FLOAT_EXT` dialect pinned byte-exactly, standalone and nested; and the
  **`[compressed]` family** — whole images byte-for-byte (a 16 000-byte repetitive binary, a
  2000-character string, `{compressed,1}`, `{compressed,9}`), both sides of the encoder's savings
  **threshold** (a small body stays verbatim), the 32-bit big-endian uncompressed-size field, and
  `binary_to_term(Image, [used])`'s consumed-byte count over a compressed image.
- `wire_ids.rs` (A.2/A.5) — local pid/port id images `NEW_PID_EXT`/`V4_PORT_EXT` + node identity
  `node/0,1`, `nodes/0,1`, `set_cookie/2`, and `wire_ids/internal_port_round_trip_identity` (a live
  port decoded back to the very port and re-encoded to the same image, both reduced to booleans so no
  per-VM-start id/creation byte reaches a golden).
## A.3 — NIF C ABI
- `nif.rs` — `erlang:nif_error/1,2` reason shapes.
- `nif_so.rs` (45 cases) — `load_nif/2` of a real `cc`-built `.so`, the `load_failed`/`bad_lib`/reload
  shapes (integer-classified) and the load **refusal matrix** (`load/1` veto, `ErlNifEntry`
  module-name mismatch, `ERL_NIF_MINOR_VERSION` too old/too new, `vm_variant` mismatch, the `LoadInfo`
  term reaching the callback, a non-string path as a catchable `badarg`).
  Plus the 30-case `nif_rt/…` block over a second embedded module with its `NIF_RT_C` /
  `NIF_RT_RAISE_C` fixtures:
  * real data through the ABI — a >32-key HAMT map built by `enif_make_map_from_arrays` and by 40
    `enif_make_map_put`s and walked by `enif_map_iterator_*`, a deep mixed iolist flattened by
    `enif_inspect_iolist_as_binary`, a 40-element cons-list walk, `enif_make_list_from_array`, the ETF
    round trip against `term_to_binary/1`, the `load`-callback-raise rule;
  * the **stateful** families — `enif_send` in both the `NULL`-env and the `msg_env` form (to a
    waiting process, to the caller, to a dead pid), `enif_monitor_process`/`enif_demonitor_process`
    with exactly one `down` carrying the armed pid and the dead-pid refusal,
    `enif_compare_monitors`/`enif_make_monitor_term`, `enif_select` on a real pipe fd (one
    `ready_input`), a term-held resource's destructor on the last reference after a forced GC,
    `enif_make_map_from_arrays`'s duplicate-key refusal, `enif_binary_to_term`'s consumed-byte report
    over trailing bytes, and a local fun's `NEW_FUN_EXT` image.
  Every live side effect is **waited for** under a deadline (a deadlined `receive`, a bounded poll)
  and each sub-block has its own `classify/1` table.
- `nif_res.rs` (30 cases) — the resource type + `enif_make/get_resource`, `enif_ioq_*`, the
  `erl_drv_thread`/sync + extra-env families, the destructor firing on the last reference after a
  forced GC, `enif_keep_resource`, `enif_get_resource`'s non-resource refusal and the
  `enif_open_resource_type` flag matrix.
- `nif_state.rs` (9 cases, `nif_state/…`) — the stateful family's **behaviour**, one embedded module
  (`nif_state:classify/1`) + one `NIF_STATE_C` fixture: the `enif_monitor_process` round trip (arm →
  DOWN → an `enif_demonitor_process` the C can no longer identify) and the `enif_set_pid_undefined`
  refusal; `enif_select`/`enif_select_x` on real `pipe(2)` descriptors (an armed READ, the
  `ERL_NIF_SELECT_STOP` and `SELECT_CANCEL|READ` codes, `CUSTOM_MSG` delivery, the WRITE direction →
  `{select,_,_,ready_output}`, and an interest's reference holding a released object alive until
  STOP); resource identity across a forced collection (`enif_compare`/`enif_is_identical`/
  `enif_get_resource` payload stability) and `enif_keep_resource` refcounting outliving the term.
  The resource type's `stop`-callback *invocation count* is the one unpinnable item (oracle calls it
  per STOP, the runtime never does — `divergences.md` §A.3).
- `nif_thread.rs` (8 cases, `nif_thread/…`) — the **threading / scheduling / cross-process** half,
  one embedded module (`nif_thread:classify/1`) + one `NIF_THREAD_C` fixture: two
  `enif_thread_create`s over an `enif_mutex`-guarded counter (plus an overlap counter and
  `enif_thread_join`'s per-entry value), `enif_thread_self`/`enif_equal_tids`/`enif_thread_type`
  inside a created thread, `enif_thread_name` (named and `"unknown"`), `enif_mutex_trylock`'s `EBUSY`
  on the owning thread and a blocking `enif_mutex_lock` completing only after the owner's unlock, the
  `enif_getenv` **success** path, `enif_whereis_pid`+`enif_send(env,&pid,NULL,msg)` reaching a live
  mailbox, and `enif_make_new_binary`/`enif_make_sub_binary`/`enif_realloc_binary` byte windows.
  `enif_schedule_nif`'s success path and `enif_send` from a NIF-created thread are the two unpinnable
  items (`divergences.md` §A.3).
- `nif_abi.rs` (166 cases) — one shared `.so` whose `probe/1` integer selector covers the `enif_*`
  surface, incl. the port/monitor/loader/scheduler families.
- `nif_abi_extra.rs` (131 cases, `nif_extra/…`) — a second `.so` over the boundary/edge/refusal +
  map/IOQ/thread families.
## A.4 — driver C ABI and ports
- `driver_ports.rs` — `open_port/2` open-error shapes.
- `driver.rs` — `os:cmd/1`, the spawn-driver read path, `port_command/2,3`, `port_info/1,2` (+ after
  `port_close`), `ports/0`, and the port-ownership family: the static `spawn_driver` namespace incl.
  the `efile`/`fd` refusals and that a refused name consumes no port id, `port_connect/2` + the
  `erts_internal:` form, the `erts_internal:` port-*value* shapes (`port_close/1`/`port_info/1`
  `badarg` vs `port_info/2` `badtype`, an unknown item's `badarg`, an unnamed port's `[]`
  `registered_name`, a closed port's `undefined`), `register/2` of a port, `port_info(P,memory)`/
  `queue_size`, the close signalling (`{'EXIT',P,normal}` to a trapping owner,
  `{'DOWN',Ref,port,P,normal}` to a port monitor), and the `spawn_executable` close-both-fds teardown
  pinned as the **order-free pair** `{data,{noeol,_}}`+`{exit_status,0}`
  (`driver/spawn_executable_close_both_fds_flushes_the_frame_and_exit_status`; which lands first is a
  ledger entry, not a pin).
- `driver_c.rs` (54 cases) — a real C driver `.so`: lifecycle, `port_info`, the echo round trip, the
  registry fixtures, the **port-closing family** (`driver_failure`/`_atom`/`_posix`, `driver_exit`,
  each reason to a trapping owner and to a port monitor, plus the post-failure `port_info`/
  `port_close` refusals), the `control` callback behind `port_control/3` (reply buffer / `NULL` reply
  / negative result / `PORT_CONTROL_FLAG_BINARY` / output beside the reply / absent slot / a verb
  beyond `UINT_MAX`), `erl_drv_output_term`'s `ERL_DRV_PID` operand, `remove_driver_entry` of an
  unlisted entry, `set_busy_port` + `[nosuspend]`, the `{input,_}`/`{output,_}` counters,
  `erl_drv_set_os_pid/2` through `port_info(P,os_pid)`, and the `erl_drv_putenv/2`+`erl_drv_getenv`
  round trip on a present key.
  Fixtures: `SPLIT_DRIVER_C` (`driver_output2`'s header/body, `NULL`-body and empty-body replies as
  three structurally different **terms**, plus a 1000-byte refc binary through `control` and
  `output`), `MONITOR_DRIVER_C` (the only entry declaring `process_exit` — the monitor **success**
  path and its exactly-once firing), `ASYNC_DRIVER_C`/`ASYNC_NO_READY_DRIVER_C` (the **async pool** end
  to end: a submission answering a positive id, `async_invoke` running, `ready_async` firing exactly
  once with the armed `async_data`, and `async_free` instead where the slot is `NULL`), and
  `STATE_DRIVER_C`.
- `driver_abi.rs` (90 cases) — one shared driver `.so` dispatched by a one-byte selector.
- `driver_abi_extra.rs` (78 cases, `driver_extra/…`) — `driver_get_now/1`, timeslice, port-queue,
  `driver_pdl_*`, the `erl_drv_thread_exit`/rwlock/cond families, the `tcp_inet` built-in.
- `udp.rs` (32) / `ram_file.rs` (21) / `inet_control.rs` (15) / `inet_write.rs` (11) — the **built-in
  drivers**; see the "built-in drivers" section below.
## A.5 — distribution wire / live nodes
- `wire.rs` — the ETF/distribution tag codec: canonical flatmap key order, legacy→modern id upgrades,
  `EXPORT_EXT`/`NEW_FUN_EXT`, `compressed`, version framing.
- `wire_extra.rs` (443 cases) — tag-level byte goldens + round trips, framing + `[safe]`/`[used]`,
  `DOP_*` control tuples + `ERL_DIST_HEADER`, the `term_to_binary/2` minor-version matrix reduced to
  per-VM-free tag bytes, latin-1↔UTF-8 atom re-encoding, every non-byte-aligned bitstring width,
  ±2^128 `SMALL_BIG_EXT`, seven `binary_to_term` `badarg` refusals, the send-path `term_to_iovec/1,2`
  family pinned by its relation to the `term_to_binary` image (never by a segment split), truncation
  refusals over a real image, and the large/refc payload identities.
- `dist.rs` (5 `CaseKind::Command` cases) — four over the `dist_probe` helper (handshake ping, node
  visibility, term delivery, non-Latin-1 atom round trip), plus the **two-VM self-pair**
  `dist/live_self_pair_handshake_and_message`, whose `PeerRun` leg is a second VM of *that side's own*
  runtime and whose client line pins `pong`, a published (not hidden) node, one message each way
  twice, and `{nodedown,_}`. `{nodeup,_}` is deliberately unpinned (the oracle delivers no `nodeup`
  with that shape, so pinning it would pin an OTP-version-dependent absence). Cross-runtime traffic
  cannot be a case (the two legs never coexist) — see `divergences.md` §A.5.
- `dist_extra.rs` (15 `CaseKind::Command` cases, `dist_extra/…`) — the widened **two-VM self-pair**
  surface: `net_kernel:connect_node/1`, a ping of a missing node (`pang`), a **cookie-mismatch
  refusal**, a foreign pid's `{node,N}` tag (far-side `node/1` compared locally ⇒ a boolean, no node
  name in the golden), `erlang:send` to a remote pid *and* to a remote registered name with the
  arrival order preserved, five ordered round trips, atom / list / binary / 100-element-tuple /
  40-level-nested payloads whose identity is pinned **relationally** (far-side `erlang:phash2/1` and
  `byte_size(term_to_binary/1)` against the sender's own — never a hash constant), a send to a
  missing remote name and to a dead remote pid (the client must survive). Two measured boundaries
  fence it: the receiver dies above ≈1536 B of binary / ≈110 list elements / a 20-message burst of
  64-byte binaries (an accumulated-heap limit, `beam_dist::fragment::DIST_FRAGMENT_SIZE` is 64 KiB so
  a genuinely fragmented frame is unreachable live), and the whole **signal family** — `DOP_LINK`,
  `DOP_UNLINK`, `DOP_EXIT`, `DOP_MONITOR_P`/`DOP_MONITOR_P_EXIT`, pid identity, `DOP_ALIAS_SEND` — is
  silent in **both** directions on the Rust side (root cause: `crates/beam_process/src/ptab.rs`
  `terminate` drops `remote_watchers` without emitting `DOP_MONITOR_P_EXIT`), so none is pinnable.
  Plain messaging (`DOP_REG_SEND`, reply-to-pid) is what the landed cases ride.
## A.6 — CLI flags and environment
- `flags.rs` — `init:get_argument/1` for `-pa`/`-pz` multiplicity + interleaved occurrence order,
  repeated/bare/empty-valued flags, the value-list boundary at the next switch (incl. a `+`-flag), the
  `-setcookie`/`-sname`/`-name`/`-hidden`/`-remsh`/`-connect_all`/`-smp`/`-start_epmd`/`-mode`/`-boot`
  switches, app-env flags, flag-name spelling (`=` kept, dotted keys, case-sensitive), the
  `-s`/`-run`/`-eval` Start instructions (consumed before `init`), the emulator `+`-flags incl. the
  `+P`/`+Q`/`+S` minima and the `+Q N`/`+S N:online` matrices, the `+t`/`+e`/`+B`/`+sbt` effects,
  `-env` (last-value-wins, the `-env NAME=VALUE V` split), the `--` boundary, `.` on `code:get_path/0`,
  the `ERL_*FLAGS` merge incl. an env-carried `-env`/`-eval` being *recorded*, the `+hmax*` family and
  `BEAM_RT_GC_STRATEGY`.
- `flags_apply.rs` — A.6 *application*: the `flags_apply/erl_*` `EvalEnv` cases pinning each merge
  source's *effect* (`+P`/`+Q`/`+A`/`+S`/`-pa`/`-pz`/`ERL_MAX_PORTS`, incl. one `ERL_FLAGS` value
  carrying two switches) + the `ModuleEnv` `-pa`- and `-pz`-via-`ERL_AFLAGS` cases (the `-pz` one
  pins that an appended directory does NOT shadow a stdlib module, `code:which/1` reduced to an
  integer, because the `-pa` mirror is unpinnable: a winning shadow is loaded during the oracle's boot
  and crashes it).
## A.7 — BIFs, ETS, printing, math, re, zlib
- `bifs.rs` — `length`, `integer_to_binary`, `lists:*` incl. `lists:concat/1`, `string:concat/2`,
  `maps:get`, `code:ensure_modules_loaded/1`, the quoted-atom `-eval` argument, binary ops,
  `io_lib:format`, the `timer` family, process-control, code-loading, `erts_debug:size/1`/`same/2`/
  `flat_size/1`, the `interpreter_size`/`disassemble`/`lcnt_*` build constants and the whole
  `erl_debugger` support guard, `erts_internal:cmp_term/2`, `erlang:send/3`, `beam_lib:md5/1`, and the
  code-store removal surface (`erlang:delete_module/1`, `code:purge/1`/`code:soft_purge/1`,
  `erts_internal:purge_module/2`).
- `ets.rs` — the plain `ets:*` surface + table types/options; `ets_access.rs` — the access-control
  matrix (the *denied* cells are a divergence, not pinned); `ets_spec.rs` (35) —
  `select`/`match`/`match_object` + continuations + delete/replace/count, `match_spec_*` incl. the
  `specs/`-prefixed `match_spec_compile/1` shapes, the `ordered_set` arithmetic-key rule.
- `maps.rs` — the fun-taking `maps` BIFs (`fold/3`/`map/2`/`filter/2`/`foreach/2`/`to_list/1`) over a
  map *and* over every `maps:iterator/1,2` order incl. a custom 2-arity comparator, `maps:next/1`'s
  look-ahead and terminal `none`, the >32-key HAMT chunk boundary, the `{badmap, Arg}` refusal vs a
  bad fun's plain `badarg`.
- `phash.rs`, `binmatch.rs`, `math_bifs.rs`, `printing.rs`.
- `re.rs` (141) — `re:run/2,3`, `re:compile/1,2`, `re:replace/3,4` + the run/compile options (promoted
  from the live-oracle source `crates/beam_bifs/src/re/oracle.rs`); payload-bearing `re:compile`/
  `re:inspect` results are projected payload-free *inside the expression*, and malformed patterns pin
  the byte-comparable `{error,{Msg,Pos}}`.
- `format_limit.rs` (21) — `io_lib:format/3`'s `~ts` + `{chars_limit,N}` grapheme eliding.
- `ddll.rs` (21) — `erl_ddll:loaded_drivers/0`/`try_load/3`/`try_unload/2`/`info/2` (agreeing
  shapes/reasons + the statically linked builtin set, pinned order-insensitively).
- `decode_packet.rs` (43) — `erlang:decode_packet/3` across the 4 integer + 12 named packet types.
- `proc_signals.rs`, `proc_control.rs`.
- `records.rs` (A.1/A.2/A.7) — native-record runs + `records:*`.
- `zlib.rs` (36) — DEFLATE **byte identity** with the oracle throughout (the encoder is libz-exact):
  byte-exact images for the one-shot `zlib:zip/1` (raw DEFLATE), `zlib:compress/1` (zlib-wrapped) and
  `zlib:gzip/1` (gzip container) over repetitive, structured, tiny and large bodies (an 8 KiB zero
  body compressed to **17 bytes**, a 1 KiB zero body, a 100-byte sequence, a 16 000-byte repetitive
  text, `<<"hello">>` as a fixed-Huffman block, `<<>>` and the stored blocks); the **stream API**
  byte-exactly too — `deflateInit` at levels 9/6/1/0, `deflate`, a `deflateParams` level change
  mid-stream, `inflateInit`/`inflate` (whole stream and split across two chunks), `deflateEnd`/`close`;
  the `unzip`/`uncompress`/`gunzip` round trips; and the `data_error`/`badarg` error matrix.
## A.8 — introspection
- `system_info.rs` — `system_info/1` shapes + `badarg` on an unknown key, incl. the wider build/state
  keys (`multi_scheduling`, `emu_type` — pinned by its profile-invariant shape/relation,
  `lock_checking`, `scheduler_bind_type`, `min_heap_size`, `dist`, `{allocator, _}`, …),
  `erlang:memory/0,1` structural goldens, bulk `process_info/1` + the `process_info/2` items (the
  spawned-process `{registered_name,links,trap_exit}` trio and the position items
  `{current_function,initial_call,current_location,catchlevel}`), the `erlang:fun_info/1,2` family
  under the same contract (the **external**-fun key set + key count,
  `fun_info/external_fun_info_{length,keys}`), the `erlang:statistics/1` shape and the tracing surface
  `trace/3`/`trace_info/2`/`seq_trace:*`/`halt/1`.
## A.1/A.7 — bit strings and the GC
- `bits.rs` (37) — the `-eval` bit-string surface: two's-complement integer segments of any width in
  both endiannesses, bignum segments, `float` `:16`/`:32`/`:64` with half-to-even rounding,
  `utf8`/`utf16`/`utf32` construction and decoding, string-literal and unsized `binary`/`bitstring`
  segments, the non-byte-aligned sizes, and the absolute rule that an invalid/truncated/out-of-range
  **pattern** field is a match failure, never a raise.
- `gc.rs` (38) — post-collection value/identity preservation over big lists, flat/HAMT maps,
  refc/sub binaries, refs/pids/funs; collector-stressing folds; the `erlang:garbage_collect/0,1,2`
  return value + the `/2` option form and the `process_info(memory|garbage_collection)` surface; five
  `erlc`-compiled module-runs. Comparability limits in `GC_COMPARABILITY.md`.
## A.4 (built-in drivers)
- `udp.rs` (32) — `udp_inet`'s port surface: `open_port({spawn_driver,…})`, `port_info/1,2`
  name/`connected`/`os_pid`/`links`/byte counters/`id`/`queue_size`/`memory`, `ports/0` membership
  across close, the double-close `badarg`, the closed-port `undefined`, `list_to_port`/`port_to_list`;
  plus the `port_control/3` named-verb half (`INET_REQ_OPEN`/`BIND`/`NAME`/`CLOSE`/`GETTYPE`/
  `GETSTATUS`, the SETOPTS/GETOPTS store-and-readback incl. `{active,N}` counter accumulation, the
  `[0]einval`/`[0]exbadport` refusals, the `INET_REQ_FDOPEN` pre-adoption refusal ladder) and the
  three `port_command/2` rows (a bare payload on a socket-less port answers `true` then tears the port
  down through `driver_failure_posix(EINVAL)`; a caller-ref-prefixed payload answers `true` and leaves
  it alive; a closed port is `badarg`).
- `ram_file.rs` (21) — `ram_file_drv`'s name (incl. the full `spawn_driver` string), `os_pid`, the byte
  counters, all 13 accepted command opcodes, the close/`badarg`/`undefined` refusals, and the **reply
  matrix** (`open`/`write`/`read`/`lseek`/`truncate`/`size` as they reach the owner, the
  `NUMBER`/`OK`/`ERROR` proper-list shapes, `port_set_data/2`+`port_get_data/1`, `port_control/3`'s
  `badarg` for every verb). Reply bytes are pinned through `iolist_to_binary/1` because the oracle
  splits a DATA reply into an improper list — see `divergences.md` §A.4.
- `inet_control.rs` (15) — the inet drivers through `gen_udp`/`gen_tcp`/`inet`: open + option store
  round trip, the bare refusal matrix for both families, a loopback `gen_tcp:listen/2`'s `sockname/1`
  shape with the `{active,_}`/`{packet,_}` store and the unknown-option `{error,einval}`, the static
  `spawn_driver` namespace incl. the `efile`/`fd` `badarg`, the `{packet,N}` option store on both
  sides, the **framed read** in all three block modes (`{packet,4}`, `{packet,2}`, `line`, plus
  framing inherited from `gen_tcp:listen/2` and buffering across segments), and the **data path**
  (`gen_tcp:send/2` on a listening vs a connected socket, active-mode TCP delivery + peer-close
  teardown, a datagram delivered between two `gen_udp` sockets, a passive `gen_udp:recv/3`). The
  framed **write** — a block framing mode on the *sending* socket — writes no length header on
  `beam_rt` and is therefore a ledger row, never a case (`divergences.md` §A.4).
- `inet_write.rs` (11) — the **write** half of the same data path, all `Eval`+`Probe::Value` with a
  loopback `port 0` listener and a length-draining bounded `receive` (so TCP coalescing cannot change
  the answer): an iolist vs a flat binary arriving as the concatenation, segmented `send`s, byte
  transparency (`0..255`), the `{error,closed}`/`{error,enotconn}` answer matrix plus the send after
  the peer-close notification, an empty payload as a successful no-op, the write-side option store
  (`send_timeout`/`send_timeout_close`/`packet_size`/`nodelay`/`delay_send` + `einval` on an unknown
  option), a 64 KiB write fully received, both directions of one stream, `{packet, line}` on a sender
  adding **no** prefix, and the datagram write path. Its guards reject `gen_tcp:recv(` and
  `gen_tcp:shutdown(` (both are the `process blocked` exit-3 gap), `inet:getstat(` (the send-side
  counters stay `{0,0}` on `beam_rt`), any `{packet, 1|2|4}` (the framed write — `line` is the one
  framing that agrees), and a bare-integer payload (the oracle writes byte N, the runtime drops it).
## A.9 — boot, preloaded, code server, in-process compile
- `boot.rs` — version banner, code-server + `application:*` observability, boot-time process surface.
- `boot_prereq.rs` (72) — `erlang:pre_loaded/0`, `which_applications/0`, `code:is_loaded(init)`,
  `register/2`+`whereis/1`, the `-boot` arg, `proc_lib:start_link/4`, and four observations under the
  `-boot start` flag (the kernel registered-process names plus three independent boot-completion
  signals, collected as `BOOT_COMPLETION_CASES`: `boot/kernel_processes_registered_under_boot_flag`,
  `boot/init_get_status_boot_progress_is_started_under_boot_flag`,
  `boot/which_applications_includes_kernel_and_stdlib_under_boot_flag`,
  `boot/kernel_sup_children_are_live_under_boot_flag`).
- `boot_extra.rs` (275) — the frozen preloaded set, the code-server view, the `function_exported/3`
  floor, the kernel registered-name manifest incl. `whereis(erl_prim_loader)`, `init`/boot status, the
  preloaded `persistent_term`/`atomics`/`counters` floors, the installation hierarchy relations
  (`code:root_dir`/`lib_dir`), app-resource value shapes, release-stable constants, the "installation
  view".
- `selfhost.rs` (26, all agree) — the six in-process `compile:file/2` module-run routes, the seven
  expression cases (the `timer.erl` export table and six *relations* of the file-free `[binary]`
  route), and the thirteen-case **artifact** block (the produced `Code` chunk and its
  `beam_lib:md5/1` image, the `debug_info`/`attributes`/`Docs`/chunk-set structure,
  `compile:forms/2` built-loaded-executed, a real `ordsets.erl` compile-and-call, the diagnostic
  surface, and the two `{outdir, Dir}`/`{d, …}` module-run routes). Each pins a relation, never a hash
  value or a release-only constant.
- `selfhost_chain.rs` (7, all agree) — the **staged** chain, i.e. a stage the runtime compiled being
  *used* to compile the next: `epp:parse_file/2` → `erl_lint:module/1` → `erl_scan:string/1` →
  `compile:forms/2` against the `compile:file/2` shortcut (equal `Code` chunk, equal export table,
  equal md5-significant chunk list **minus `Meta`**), that route's compile → `code:load_binary/3` →
  **call** hand-off, the compiler compiling one of its own passes (`lib/compiler/src/erl_bifs.erl`,
  renamed) and then being asked both polarities, a compiled `lib/stdlib/src/erl_scan.erl` tokenizing
  the next module (→ `erl_parse:parse_form/1` → `compile:forms/2` → load → call), the three-hop
  A-compiles-B-and-runs-it chain, a `-moduledoc({file, …})` source through the staged route, and the
  `compile:forms/2`/`compile:noenv_forms/2` option-list tuple shapes. Every loading case **renames**
  the module attribute (loading under a stdlib name is `{error, sticky_directory}`). The expensive
  routes (`erl_lint.erl` ≈164 s, `erl_scan.erl` ≈36 s on the Rust side) are ledger entries, not cases
  — see `divergences.md` §A.9.
## A.2/A.7 — module runs
- `modules.rs` (71) — embedded `erlc`-compiled wrappers + `erl_error:format_exception/3` regressions +
  a zero-length-binary case + the `diff_trace_info` source's two compiled-code cases over the
  synthesized callee-head `undef` stacktrace (`modules/undef_head_item_{shape,identity}_in_compiled_code`,
  reached through the three-argument `try C:R:St ->` form that real bytecode carries and the `-eval`
  evaluator cannot express) and its external-`fun_info` case
  (`modules/fun_info_external_fun_created_in_compiled_code` — the five-key external proplist
  `arity`/`env`/`module`/`name`/`type`).
- `mexec.rs` (tiny wrappers calling a real system module end-to-end) and `mexec_extra.rs` (60 — a
  second block of real-system-module wrappers over a broad stdlib slice).
## Registry (`src/corpus/mod.rs`)
`Case`/`CaseKind` (`Eval`/`EvalEnv`/`Module`/`ModuleEnv`/`Command`)/`ModuleRun`/`CommandRun` (with
`peer: Option<PeerRun>`)/`CommandSpec`/`CommandProgram`/`EmbeddedModule`/`Contract`/`Probe`/`Golden`
(+ `DIR_PLACEHOLDER`/`expand_env`, `PEER_NONCE_PLACEHOLDER`/`expand_nonce`), the registry
(`cases`/`find_case`/`probe_expr`) and the wiring/well-formedness tests that fail the build on a
malformed case.
## Pinning traps (easy to get wrong)
- An expression containing a `case` or a `<<…>>` **pattern** must be wrapped in an
  immediately-applied fun — the **oracle's** `erl_eval` cannot evaluate it inside the probe's `try`
  body.
- A probe expression must not bind `V`, `C` or `R` (the wrapper binds them) — use `Reason`/`Msg`,
  else the *oracle* fails to compile the wrapped expression and the case is a guaranteed false
  mismatch.
- **Do not use `Pid ! Msg` as the last statement before a blocking call in a `-eval` script** —
  measured, a bare `!` *after* a `timer:sleep/1` or a bounded `receive` fails the whole `-eval` on the
  Rust side with `beam_rt: term not found` (exit 1), while `erlang:send(Pid, Msg)` works. Every
  distribution script therefore uses `erlang:send/2`.
- Never assert an order-unstable golden: a multi-key-map `~w`, a per-VM-start pid/port/ref/creation
  value, or a temp-dir-bearing `load_nif` message. Assert a shape / `maps:size` / sorted keys, or
  classify into an integer.
- The `udp.rs`, `inet_control.rs` and `inet_write.rs` group tests still fence their modules: `udp`
  rejects `gen_udp:send`/`gen_udp:recv`/`prim_inet` (those belong to `inet_control`), `inet_control`
  rejects `port_command`/`prim_inet`/`os:cmd` (an external-injection route would make the golden
  host-dependent) **and** a `{packet, N}` in a `gen_tcp:connect/3` option list (the framed *write*,
  a measured divergence — a framing mode on a *receiving* socket is free), and each pins its group
  size (32 / 15 / 11) — a case lands only together with the matching guard/size edit in the same
  change. `inet_write` additionally rejects `gen_tcp:recv(`, `gen_tcp:shutdown(`, `inet:getstat(` and
  a bare-integer payload (see its section above).
- A NIF/driver fixture may reference only `enif_*`/`driver_*` symbols the Rust runtime really exports
  (both runtimes `dlopen(RTLD_NOW)` it): an unexported symbol makes the *Rust* load fail while the
  oracle loads — a mismatch, not a gap.
- A `PeerRun`-carrying `Command` case must derive **every** node name from `{nonce}` (the harness
  substitutes it per invocation) and must derive the peer's name from the client's own `node()` — a
  fixed `-sname` collides on `epmd`'s machine-global name space with other worktrees and other
  agents' parallel sweeps.
