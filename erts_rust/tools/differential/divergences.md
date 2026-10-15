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
- `enif_thread_type()`'s **exact value** differs (oracle `1`, rust `0`) — only the documented-set
  *membership* shape (one of the four thread kinds) is pinned, never the value itself.
- An `ErlNifMonitor`-based probe that hands the runtime a **zeroed** monitor is a **probe hazard**:
  `enif_demonitor_process(env, NULL, &mon)` over one crashes the oracle VM (an exit signal with
  empty stdout), so it is deliberately not pinned — such a case would be a harness artifact, never
  a runtime difference. The zeroed-monitor reads that *do* survive the oracle are pinned instead
  (`enif_compare_monitors` ordering two equal payloads, `enif_make_monitor_term`'s not-a-ref
  answer).

## A.4 — driver C ABI (owner: `beam_rt` / `beam_io`; `beam_abi` for the timer units)
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
- A `{'DOWN', …}` for a monitored **port** is not delivered after `port_close/1`: the oracle
  sends `{'EXIT',Port,normal}` (the link) then `{'DOWN',Ref,port,Port,normal}`, the Rust
  runtime sends nothing. The `unlink/1` needed to isolate it raises `error:badarg` in the Rust
  runtime (the port's link is not removable). The monitor **creation** agrees and is pinned.

## A.7 — program result / CLI (owner: `beam_rt` / `beam_engine`, `beam_bifs`)
- A bare (unqualified) name in the `-eval` path resolves differently: `phash2(T)` is `error:undef`
  under the oracle's `-eval` (a local call) but `ok …` under the rust runtime (it resolves the
  `erlang:phash2/1` BIF). The `erlang:phash2/1`-qualified shapes agree and are pinned in
  `phash.rs`. Owner: `beam_rt` (`-eval` evaluator).

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
- `whereis(code_server|erl_prim_loader|kernel_sup)` → oracle a pid, rust `undefined`.
- Further surfaces found while probing: `code:get_path/0` (differing path lists — and every entry
  is absolute in the rust path list (`filename:absname(S) =:= S` holds for all of them) but not in
  the oracle's),
  `init:script_id/0` (rust `beam_rt: unsupported: init:script_id/0 (no boot script has been
  loaded)`, exit 3; the oracle `{"Erlang/OTP","29"}`),
  `erl_prim_loader:get_path/0` (`badarg` vs `{ok,Paths}` — the oracle's `erl_prim_loader` is a
  test-only module with no `get_path/0`, while the runtime serves it),
  `erlang:system_info(logical_processors)` (16 vs 14),
  `erlang:system_info(heap_sizes)` (the rust list has extra trailing entries),
  `erlang:module_loaded(lists|kernel)` (`true` vs `false`).
- `erlang:system_info(emu_type)` and `erlang:system_info(debug_compiled)` report the runtime's
  **own build profile**, so their values track the artifact the harness resolved rather than the
  release tag: a **debug** build answers `debug`/`true` where the oracle (and the release build the
  harness prefers) answers `opt`/`false`. Only the type/shape (`is_atom`/`is_boolean`) is pinnable —
  a case asserting the value would be profile-dependent. `erlang:system_info(compat_rel)`
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
- The `{fd,…}` port grammar is disjoint; `{line,N}` returns raw bytes; `eof` is never delivered;
  a non-iodata `port_command/2` is accepted; `port_info(P,registered_name)` is `badarg`; only the
  oracle writes a missing-`{spawn,Cmd}` shell stderr line.

## A.6 — CLI flags & environment (owner: `beam_rt` / the `-eval` evaluator)
- Plain `-extra`/`--` arguments are unusable through the harness; a repeated `-eval` is recorded
  verbatim where `init:get_argument(eval)` -> `error`; an unbound `-eval` variable is a loud error
  vs silent.
- `+hmax` overflow: the oracle **kills the process** where the rust evaluator raises
  `error:system_limit`; only the tiny bounded-allocation case is pinned.
- A large `-eval` iolist raises `error:out_of_memory` in the rust evaluator, so `iolist_size/1`'s
  bignum promotion is not observable and has no case.
- The `ERL_*FLAGS` merge **order** IS pinned (`flags/env_*`); `ERL_OTP<sysver>_FLAGS` (29 vs 30) and
  an `-eval` carried by `ERL_FLAGS`/`ERL_AFLAGS` are deliberately not.
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
  `flags/env_switch_value_is_not_an_init_argument`). Owner: `beam_rt`.

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
- `code:is_loaded/1` answers out of the code server's **load state** in both runtimes — the
  `{file, Path}` pair for a loaded module, the atom `false` for one that exists on the code path but
  was never loaded (`code:is_loaded(dets)` and `code:is_loaded(lists)` are both `false` in the rust
  runtime, and `dets` is `false` in the oracle too) — so the answers differ only where the **boot
  set** does: the oracle boots the whole stdlib, so `code:is_loaded(lists)` / `code:is_loaded(kernel)`
  are `{file, …}` there and `false` in a runtime whose `-eval`/`run` path loads only the 22 preloaded
  modules. The affected corpus cases (`bifs/module_loaded_lists_shape`,
  `boot_extra/code_is_loaded_lists_is_a_file_pair`, `boot_extra/code_is_loaded_kernel_is_file`) are
  therefore ensure-load-explicit and pin the loaded answer. Owner: `beam_rt`.
- `erlang:trace_info/2`: the `all`/`traced` selectors differ (`badarg` vs a value list); only
  `flags`/`tracer` agree.
- `erlang:exit(self(), normal)`: the oracle ignores it (`true`), the rust runtime raises `exit:normal`.
- `erlang:memory/0,1` is pinned **structurally** (A.8): the 9-key order/count,
  `is_integer(memory(total))`, the selection shape and the unknown-key `badarg`; the volatile byte
  totals are never asserted.
- `+Q N` / `ERL_MAX_PORTS` -> `system_info(port_limit)` **is** pinned (the `next_power_of_two` sizing
  matrix); `+Q legacy` / no `+Q` -> 65536.

## A.7 — erl_ddll / decode_packet (owner: `beam_bifs` / `beam_io`)
- `erl_ddll:loaded_drivers/0`'s **list contents** differ: the oracle lists its statically linked
  `["tcp_inet","udp_inet","ram_file_drv"]` (the entries are *strings*, not atoms), the Rust runtime
  answers `[]`. The members are therefore unpinnable; `ddll.rs` pins only order-stable shapes of
  the result (`{ok, List}`, the 2-tuple, `is_list/1`, an all-strings fold and a filtered
  projection that is empty on both).
- `erl_ddll:try_load(".", "tcp_inet", [])` answers `{error,linked_in_driver}` in the oracle but the
  generic `{error,{open_error,-10}}` in the Rust runtime.
- `erl_ddll:try_unload("tcp_inet", [])` answers `{error,linked_in_driver}` in the oracle but
  `{error,not_loaded}` in the Rust runtime.
  (The missing-object / missing-directory / non-string-name `{error,{open_error,-10}}` and the
  not-loaded `{error,not_loaded}` shapes, plus every `info/2` `error:badarg` refusal, DO agree and
  are pinned in `ddll.rs`.)
- `erlang:decode_packet/3` shows **no observed divergence**: the four integer forms (`0`/`raw`,
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

- `init:script_id/0` and `erlang:system_info(otp_release)` disagree: the runtime's `init:script_id/0`
  reports the **consumed boot script's** header (`{"Erlang/OTP","29"}` — the oracle install's script)
  while `system_info(otp_release)` reports the runtime's **compile-time target** (`"30"`); the oracle is
  self-consistent (both `"29"`). This is documented runtime behaviour (`../crates/beam_rt/src/boot/CONTEXT.md`,
  "the consumed install's release" vs "the runtime's compile-time target"). The corpus case
  `boot_extra/init_script_id_names_the_running_otp_release` pins the self-consistency invariant
  `init:script_id() =:= {"Erlang/OTP", erlang:system_info(otp_release)}` and therefore reports a
  **mismatch** (`ok true` vs `ok false`) on a plain `-eval` run, which loads a boot script. It is
  **pinned and failing** (the gate exits 1), and it is deliberately not in `known-gaps.txt` (a
  `mismatch` is never allowlistable): the owning
  crate (`beam_rt`) must either make the two values consistent, or the case must be deliberately
  unpinned like the other ledger entries.
