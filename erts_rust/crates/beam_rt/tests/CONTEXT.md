# `beam_rt/tests/` — runtime integration suites (the executable + the `Runtime` façade)

## Intent
Black-box and glue-level end-to-end tests for the `beam_rt` runtime binary and its `Runtime`/`RuntimeContext` façade.
Two drivers are used throughout and they are NOT interchangeable:
* `Runtime::eval` / the CLI `-eval` action (`beam_rt -noshell -eval '<expr>'`) — the **single-pass** evaluator route; no scheduler slice.
* the CLI `run <file.beam> <mod>:<fun>/<arity> [args]` action (`Runtime::run_scheduled`) — a **scheduled process** on the real scheduler/poller/runner; also `runtime.run_function`/`run_module` (single-pass, `context.run_export`, caller-owned output sink).
Most suites run **both** runtimes (this one and the system `erl`/`erlc` oracle) and compare observable output; the oracle is the expected value.

## Running
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt [--test <suite>] [--no-fail-fast]` (the sandbox `$HOME` is read-only — always go through the wrapper; the workspace manifest is `erts_rust/Cargo.toml`, never the repo root).
A missing **toolchain** (`erl`, `erlc`, `cc`, `erl_driver.h`) is always a **loud skip** plus a `return`, never a silent pass (`support/mod.rs`'s `skip_loudly`/`require_oracle`).
A missing **committed fixture** is the other class — a repository defect, not a toolchain absence — and **fails** (`mailbox_rescan.rs`'s `Scratch::new`, `bif_entry_spawn.rs`'s staging, `port_failure.rs`'s `require_probe_beam`/`the_probe_image_is_committed`, `live_inbound.rs`'s `the_fixture_beam_is_present`), because a skip there would report green rows that compared nothing.
**Opt-in suites** are `#[ignore]`d and need `-- --ignored --nocapture`: the four two-VM files `dist_two_vm.rs` (15 tests), `dist_two_rust.rs` (7), `dist_two_vm_wire.rs` (1) and `dist_payload_matrix.rs` (9, the large-payload `HeapFragment` matrix), plus the corpus/measurement harnesses (`selfhost_breadth.rs`, `selfhost_bootstrap.rs`, `src/context/calls/tests.rs`, `src/context/gc_walk.rs`) and the one residual case in `inet_nested.rs`.
An `#[ignore]`d *worker* test that a non-ignored parent re-runs in a child process (`ref_id_space.rs`, `nif_resource_gc.rs`, `nif_resource_nested.rs`) is **not** an opt-in: it is driven by its parent and must not be run through `--ignored` beside the parent.
Run the pair suites **serially** (`--test-threads=1`): each test owns a private `epmd` and two VMs, so serial execution keeps a heavily loaded host from stretching the per-step bounds.

## Cross-cutting conventions
* **A suite compiled into its own scratch directory is the only `-pa` either runtime gets.** The shared `tests/fixtures/` directory is never named as one, and each fixture is compiled or copied into a per-case directory holding only that module.
* **Byte-exact stdout probes pass `-noshell`** — except on the `run` action, which is **banner-free by construction**: `src/cli.rs`'s `parse_run_file` records the `noshell` init argument.
* **Three harness shapes, not one.** (a) a suite that compares against the system `erl` spawns it through `support::{require_oracle, resolve_oracle, oracle_eval, run_bounded}` or a file-local copy (the copies exist where the child's **exit code** must survive); (b) a suite that only needs `erlc` builds a fixture and compares against a pinned golden; (c) a suite with no toolchain drives `env!("CARGO_BIN_EXE_beam_rt")` or the in-process `Runtime`.
* **No suite compares live GC counters, `process_info(_, garbage_collection)` or `erlang:memory/0,1` value-for-value with the oracle.** The two oracle comparisons are `system_info_oracle.rs` (`system_info(garbage_collection)`, by value) and `runtime_hooks.rs` (`statistics(garbage_collection)`, by shape); every other GC suite reads the runtime's own `Runtime::gc_stats()`/`erlang:memory/1`.
* **A `beam_rt` node cannot render a remote identifier.** `pid_to_list/1` and `~p`/`~w` of an external pid or reference are typed capability gaps (exit 3, uncatchable), so the two-VM suites assert **term identity** (`=:=`, `is_pid`, a bound receive pattern), and `dist_two_vm_wire.rs` prints only ids it made up.
* **Two-VM fixture invariants are load-bearing**: every `receive` is deadline-bounded (a bare `receive` empties the run queue, the port set and the timer heap, so `run_scheduled` reports its honest `blocked`, exit 3, before the peer's term can arrive); the process that must die is a `spawn`ed child, never the run action's target; and our side never speaks first (an outbound send does not dial, `DistLinks::route_*` answers `no_link_error`).
* **The outbound monitor-down is always the plain `DOP_MONITOR_P_EXIT` (21)** form, never `DOP_PAYLOAD_MONITOR_P_EXIT`, even when the peer advertises `DFLAG_EXIT_PAYLOAD`.
* **The four `zlib` suites share one convention: six probe shapes, one golden** — `-eval` and the scheduled `run` route on both tiers, plus the oracle in both of its own shapes. Each is self-contained (a file-local oracle helper, so the probe's exit code and exact stdout survive) and compiles its source-only fixture into a per-fixture scratch directory.
* Interfaces every suite shares: `env!("CARGO_BIN_EXE_beam_rt")` (the built binary) and `support::{run_bounded, oracle_eval, require_oracle, skip_loudly, …}`.
* **An oracle probe must terminate**: the standard probe is `erl -noshell [-pa DIR] -eval EXPR -s init stop`, bounded through `support::run_bounded`/`support::PROBE_TIMEOUT` where the suite uses it.
* Scratch temp dirs come from `crate::context::test_support::scratch_dir(label)`; a hand-rolled `temp_dir().join(format!("…_{pid}"))` collides with a concurrent test *process*.

## Routing Table
One line per suite file: what it pins, and whether it needs the oracle (`erl`), a compiler (`erlc`/`cc`), or nothing beyond the built binary.

### Core execution / vertical slice
* `slice.rs` — the library slice: load a real `.beam` and run an export through `Runtime::run_function`/`run_module` (no toolchain).
* `cli.rs` — spawns the built binary, runs the slice, and pins the usage/exit-code contract (`0`/`1`/`2`/`3`).
* `mailbox.rs` — the receive/message slice over the real `RuntimeContext` (`fixtures/mailbox.beam`).
* `mailbox_rescan.rs` — the receive scan position across a suspend/resume (`fixtures/rescan.beam`: plain, marker'd, marker-behind-junk, two receives, a fired `after` and its cancellation — six oracle-cross-checked exports).
* `scheduler.rs` — several processes, one blocked in `receive`, over the real scheduler (`fixtures/sched.beam`).
* `preempt.rs` — the reduction budget pre-empts a loop (`fixtures/preempt.beam`).
* `multimodule.rs` — on-demand cross-module load of real stdlib modules (`lists`, `maps`, `ordsets`, `binary`) from a compiled fixture.
* `proc_lib_start.rs` — `proc_lib:start[_link]/3,4` across a module boundary (`plstart.erl`/`plwait.erl`).
* `fun_entry_receive.rs` — a fun-bodied process entry that blocks in a `receive`, inline and one `call_ext` away (`funrecv.erl`/`funrecvw.erl`).
* `scheduled_apply_seam.rs` — the `call_fun`/`call_fun2` closure-apply path: a scheduled process enters a blocking closure in-machine.
* `spawn_from_slice.rs` — every spawn form from inside a scheduler slice, a nested spawn and a rejected option list included (`spawnslice.erl`).
* `spawn_monitor.rs` — `spawn_monitor/1,3` and the `{'DOWN',…}` it owes (`spawnmon.erl`).
* `spawn_opts.rs` — `spawn_opt/2,3,4`'s heap-size options reach the child's `HeapPolicy` and answer through `process_info/2`'s heap items.
* `spawn_opt_heap.rs` — the `min_heap_size`/`max_heap_size` **acceptance** rule (`ok` vs `badarg`), option order irrelevant, oracle-cross-checked.
* `min_heap_size.rs` — `beam_gc::HeapPolicy::initial_heap_words` is the single authority behind `+hms`/`+hmbs` and `spawn_opt`'s `min_heap_size`, which is floored at the global policy (oracle-cross-checked figures).
* `bif_entry_spawn.rs` — a process entry naming a **host-served** MFA is dispatched to the BIF instead of entered as `erlang.beam`'s stub (`fixtures/bifent.beam`/`tmr.beam`), plus the `-eval` `receive … after T` timer wait.
* `signal_self_delivery.rs` — a `'DOWN'`/`'EXIT'` a process queues on **itself** is delivered before it sleeps (`sigself.erl`).
* `monitor.rs` — local `monitor/2,3`/`demonitor/1,2` and the DOWN the runtime owes.
* `alias.rs` — reference aliases as send destinations: `alias/1`, `unalias/1`, `monitor/3`'s `{alias,Mode}`, the compiled `Alias ! Msg`, `erlang:send/2`, `gen:reply/2` and real `gen_server` calls.
* `exit.rs` — local exit signals: trapping, `kill`, `normal`.
* `group_leader.rs` — group-leader inheritance, `group_leader/2` and the io-device default that follows it.
* `timers.rs` — the runtime timer heap behind `send_after`/`start_timer` (relative and `{abs,true}`) and `cancel_timer`.
* `read_timer.rs` — `erlang:read_timer/1,2` end to end (the hook `inet:timeout/1` needs): a live timer's remaining ms, `false` after it fires, and a read that is not a cancellation.
* `proc_seams.rs` — the `ProcessHook` seam behind the eleven process/name/global-state BIFs (`whereis`, `register`, `unregister`, `registered`, `is_process_alive`, `processes`, `ports`, `process_info`, `process_flag`, `group_leader`), nine of them served.
* `action_identity.rs` — the action body's own process identity: freshly spawned, unregistered (`whereis(init) =:= self()` is `false`, `registered_name` is `[]`) (`action_identity.erl`).
* `action_result.rs` — the action's returned term survives the action child's teardown, and the action leaves no process behind.
* `run_route_sleep.rs` — the `run` route's pass wait: no blocking wait while the round still ran slices, the anti-spin half for a parked `after T` (`pingpong.beam`).
* `boot_repeat.rs` — a second in-process `Runtime` boot must not run stale baseline-JIT compiled blocks.

### CLI / configuration (A.6)
* `cli_flags.rs` — the `ERL_OTP<vsn>_FLAGS` → `ERL_AFLAGS` → cmdline → `ERL_FLAGS` → `ERL_ZFLAGS` merge.
* `cli_env.rs` — the `ERL_*` environment variables and the init arguments they record.
* `perf_bif.rs` — the perf harness's CLI contract: `run` prints the returned value and requires one argument per declared arity (exit 2 otherwise).
* `perf_call.rs` — drives the built `perf_call` example: every claimed row is printed under its own block, and a missing `erlc` is a loud skip that exits 0.

### Boot / preloaded / native floor / code store (A.7)
* `boot.rs` — the real `bootstrap/bin/start.boot` driven to completion (22 directives, ending at index 21), the CLI `-boot` route, and the live `logger_server` registered as `logger`.
* `boot_code_path.rs` — `-pa`/`-pz` take effect on the **`-boot`** route, before the script's first directive, with the oracle's `-pa`-before-`-pz` ordering.
* `native_init.rs` — the native `init` floor: `get_argument`, `script_id`, `restart/0`, `boot/1`, `stop/0,1`.
* `a9_native_floor.rs` — the A.9 floor surface, incl. `init:script_id/0`'s **loaded** script header on the CLI routes and the preloaded set.
* `fetch_loaded.rs` — the boot-time `{From,fetch_loaded}` list is drained and cleared at boot, so every post-boot fetch answers the empty list.
* `code_load_file.rs` — `code:load_file/1`/`code:ensure_modules_loaded/1`'s error vocabulary and the expected-name refusal, on the file routes and on the call-like routes (`-s`/`-run`, `spawn(M,F,A)`).
* `code_ensure_modules_loaded.rs` — the floor key that makes `resolve_external_call` refuse to enter `code.beam`'s own body, so `{error,[{Name,nofile}]}`/`ok` agree with the oracle (oracle-cross-checked).
* `code_delete_live.rs` — `code:delete/1`, `erlang:delete_module/1`, `code:purge/1` and `code:soft_purge/1` through the live `code_server` and the native floor, on both routes.
* `code_add_path.rs` — the code-path mutators (`add_patha|add_pathz|add_path|del_path/1,2`) really move the loader.
* `code_path.rs` — the **whole** `code:get_path/0` list compared to the oracle's, element-wise and in exact order, plus the `$ERL_LIBS` shape and a blank entry (the `src/codeserver/path.rs` release-layout derivation).
* `code_load_binary.rs` — `code:load_binary/3`'s guard/`modp`/`badfile`/sticky/valid-image shapes, oracle-cross-checked.
* `prim_load_atoms.rs` — only the pre-`{kernel_load_completed}` `{primLoad, …}` batch loads, so `list_to_existing_atom/1` agrees with the oracle.
* `module_loaded.rs` — `erlang:module_loaded/1` over the preloaded set plus the live code store, oracle-cross-checked.
* `module_info.rs` — `Module:module_info/0,1` and `get_module_info/1,2` through the `ModuleInfo` seam (the suite documents its own `exports`-order divergence).
* `records.rs` — the OTP-29 native-record opcode family, `put_record/6`'s own-record create form included.
* `runtime_hooks.rs` — the three `erlang:*` `RuntimeHooks` keys `beam_rt` serves, plus the `statistics(garbage_collection)` shape.
* `dict.rs` — the process dictionary through the `DictHook` seam, and the `undef` reason producers.
* `persistent_term.rs` — the persistent-term store, incl. a stored value surviving a process heap collection (`pt_gc.beam`).
* `prim_tty.rs` — the built-in `prim_tty` NIF library: `load/0`, the served keys, the opaque `tty_create/1` round trip, the typed stubs and the no-`HAVE_TERMCAP` deviation.
* `zlib_stream_levels.rs` — `zlib`'s stream API through the real preloaded `zlib.beam`: per-level 0–9 transcripts with byte-count vectors, the flush modes, the `deflateInit/1,2,6` forms, `deflateReset/1`/`deflateParams/3` and round trips.
* `zlib_stream_dict.rs` — `deflateParams/3`'s stash path, the `need_dictionary` protocol with its Adler digest, `safeInflate/2` chunking, the reset/dictionary accessors, and a truncated stream only `inflateEnd/1` catches.
* `zlib_guards.rs` — the wrapper's refusals (`data_error`, `stream_error`, `not_initialized`, `already_initialized`, `badarg`, `not_on_controlling_process`), `set_controlling_process/2` in every state, `close/1` idempotence, and a reason atom absent from the raising module's table.
* `zlib_one_shot.rs` — the six convenience functions' fast paths, the public stream wrapper byte-identical to `compress/1`/`zip/1`/`gzip/1`, `gunzip/1` over concatenated members, and `zlib:compress/1`'s libz byte-exactness.

### Inbound `-eval` route
The action's own process never becomes a scheduler slice: its `receive … after T` really waits through a mailbox scan, a non-blocking pump bounded by `RECEIVE_PUMP_SLICES = 64`, one port-poll slice and `Runtime::wait_for_timer`.
* `eval.rs` — the `-eval` evaluator against the differential corpus's `Eval` probes, pinning the golden values the corpus asserts.
* `eval_order.rs` — sub-expression evaluation order, oracle-cross-checked.
* `eval_bits.rs` — binary construction and matching in `-eval`: little-endian non-byte-aligned segments, `float`, `utf8/16/32`, wider-than-64-bit patterns and `unit:`.
* `bignum_bit_segment.rs` — both `<<…>>` implementations mask a bignum at **every** width: the `-eval` bit-buffer builder and the engine's `bs_create_bin`.
* `eval_region.rs` — the per-statement scratch window: a value held across twenty throwaway builds is never reclaimed (a `persistent_term` value, an environment binding, a mailbox message, and the family the descent cannot enumerate).
* `eval_pool_result.rs` — the BIF-result copy-out and pool rewind on the table-less route: a container used later prints correctly, a thousand calls stay at one call's high-water under `+hmax`, and one retained across sub-expressions survives.
* `eval_off_heap_reclaim.rs` — the escaped-payload store is pruned at each statement boundary, so an expression building `N` off-heap payloads holds `O(1)`.
* `eval_port_receive.rs` — a port frame drained by `-eval`'s `receive … after` lands in the owning mailbox rather than in the matcher, so non-matching frames survive for the next receive.
* `eval_spawn.rs` — a spawned `-eval` closure whose source was recorded is compiled into a synthetic `beam_rt_eval_fun_<index>` module, loaded with `code:load_binary/3` and run as a real `ProcessEntry::Export`, so it parks in a bare `receive` and in a nested `timer:sleep/1` and resumes on a message.
* `eval_exempt_receiver_signal.rs` — a `'DOWN'`/`'EXIT'` control signal aimed at the `-eval` route's relocation-exempt root, with a roomy and with a saturated nursery.
* `send.rs` — `erlang:send/2`/`erlang:'!'/2` over the `SendHook` seam (local, remote, port and alias destinations).
* `empty_binary_result.rs` — the `run` route prints an empty binary from both representations (an on-heap `<<>>` and an off-heap refc accumulator).
* `engine_off_heap_transport.rs` — a scheduled process's result and exit reason are copied off its dying heap with the refc payload handle resolved.

### Ports, drivers and sockets (A.4)
* `driver.rs` — a real C `.so` driver: `driver_select` + `driver_set_timer`, `output` and `outputv`-only entries, `erl_drv_output_term`/`erl_drv_send_term`, and the port-less registry hooks (`event_driver.c`, the inline `reg.c`/`reg_recv.erl` pair).
* `port_failure.rs` — a driver that fails its **own** port from `output`: the `driver_failure*`/`driver_exit` rows on both routes (`port_failure_driver.c` + committed `port_failure.beam`).
* `port_busy.rs` — the `erlang:port_command/2,3` busy/`[force]` table over a real busy driver in both `driver_flags` variants, and the BIF-suspend park (`busy_driver.c`).
* `port_parity.rs` — `open_port`/`port_info`/`port_command` shapes against the oracle.
* `port_info.rs` — `port_info/1`'s C roster plus the derived `/2` items (`queue_size`, `memory`, `monitored_by`, `locking`, `parallelism`), with no `/2`-only item leaking into `/1`.
* `port_bif.rs` — the port data BIFs (`port_set_data/2`, `port_get_data/1`, `port_connect/2`, …) under their fully qualified spellings, byte-identical to the oracle.
* `port_exit.rs` — the port `'EXIT'`/`'DOWN'` a close owes trapping, monitoring and `port_connect/2`-ed owners (`port_exit.beam`, normalized shapes).
* `port_stream.rs` — the port-stream read at the library boundary: the `{Port,{data,Data}}` term, `binary` vs charlist rendering.
* `port_wake.rs` — a receiver parked on a spawn-driver port's stream is woken by the poller, never by another process's send (`port_recv.beam`).
* `port_monitor_cleanup.rs` — an incoming port monitor's cleanup when the **watcher** dies first (`port_watch.erl`).
* `inet_active.rs` — the `{active,true|N|once}` socket-input channel over real loopback traffic, seven oracle-cross-checked cases (`inet_active.erl`).
* `inet_completion.rs` — the driver→owner completion channel on the `run` route: a parked UDP recv completed by a later datagram, an accepted descriptor adopted into a live `tcp_inet` port, a non-blocking `gen_udp` round trip (`inet_completion_*.erl`).
* `inet_nested.rs` — the **nested** route (`-eval`, `-s`, boot) really waits for the driver's `{inet_async, …}` (`wait_for_port_event`), incl. the typed port-event deadline; one `#[ignore]`d residual (an accepted socket's already-buffered payload raises no read event).

### Distribution (A.5)
* `dist.rs` — a `-sname`/`-name` node binds a listener and really publishes it to `epmd` (the same port `beam_dist::epmd::lookup_port` resolves), and a real `erl` peer's `net_kernel:connect_node/1` completes the `'N'` handshake against it, in-process and through the binary.
* `dist_delivery.rs` — the inbound delivery matrix, hermetic over a hand-shuttled `ConnectionDriver` pair (31 tests): message families by delivered effect, the per-link checked drain, the route-free receipt families by installed local state, `every_routed_inbound_family_routes_nothing_outbound`, and the honest `Unsupported` rows.
* `dist_payload_matrix.rs` — **(opt-in, 9 tests)** the *size* a link carries: every leg arrives through `LocalDeliverySink::enqueue_fragmented` → `beam_message::send_fragmented` → the fragment **read** path in `loop_rec`.
* `dist_client.rs` — the connecting half against a real `erl -sname` peer: `epmd` resolution, the `'N'` handshake to `Established`, and a real `DOP_REG_SEND` asserted on the peer's own stdout.
* `exit_remote.rs` — remote `erlang:exit/2` end to end, hermetically (5 tests: `kill` terminates and is untrappable, `normal` leaves a non-trapping target alive, a trapping target gets the message, a dead target is `NoSuchProcess`).
* `live_inbound.rs` — a real `erl` peer `DOP_REG_SEND`s into a **running local process**, in-process and over the CLI (3 tests).
* `live_signals.rs` — the outbound link/exit pair and the inbound monitor direction against a real peer, each with its byte-identical negative control (5 tests).
* `live_remote_families.rs` — outbound `unlink` (`DOP_UNLINK_ID`) and inbound `DOP_ALIAS_SEND`, each with its negative control (4 tests).
* `live_remote_exit.rs` — remote `erlang:exit/2` (`DOP_EXIT2`, not the link exit) against a real peer (2 live tests).
* `net_adm.rs` — the connector BIFs reached over a live peer.
* `remote_send.rs` — remote and name destinations: the non-node capability gaps, the three-routes-agree check (compiled `!`, `erlang:send/2`, the `-eval` `!`), the same-node arm and the live `{Name,PeerNode} ! Msg` (8 tests).
* `node_cookie.rs` — the cookie and node-name BIFs through the real binary, oracle-cross-checked.
* `pid_roundtrip.rs` — a pid/port/reference wire round trip and the identity it must preserve.
* `dist_two_vm.rs` — **(opt-in, 15 `#[ignore]`d tests)** rust ↔ real `erl` on one private `epmd`: exchange and node monitors, the peer acting on our process, us acting on a peer process, both alias directions and the pid identity round trip.
* `dist_two_rust.rs` — **(opt-in, 7 `#[ignore]`d tests)** two `beam_rt` nodes, asserting **term identity** (a `beam_rt` peer cannot render a remote identifier).
* `dist_two_vm_wire.rs` — **(opt-in, 1 hermetic `#[ignore]`d test)** the outbound monitor-down decoded byte-level: opcode 21, five elements, the **watched** local pid first, the peer-minted reference echoed, plus the negotiated `DFLAG` set.
* `support/{dist,pair,fixture}.rs` — the **one** two-VM harness (`Harness`/`PrivateEpmd`/`ChildVm`/`Scratch`, the responder/actor session, the one `two_vm` fixture both peer kinds run); `support/two_vm.rs` is the second harness `dist_payload_matrix.rs` uses. Every child is `setsid -w`-isolated, `TERM`-then-`KILL`-reaped, and a suite holding VMs declares the `Harness` **last** (the drop order is load-bearing).

### NIF (A.3)
* `nif.rs` — a real C NIF: loading, dispatch and the `enif_*` surface (incl. the inline `env_nif`/`env_caller` `enif_alloc_env` pair).
* `nif_send.rs` — `enif_send` really delivers, `NULL`-env and `msg_env`/`enif_make_copy` forms, and the C's `0` for a dead pid.
* `nif_send_load.rs` — a `load` callback's `enif_send` is queued on the load host and drained at callback exit, read by a self-receiver and a parked child.
* `nif_monitor.rs` — the stateful monitor family: the resource's `down` fires exactly once with the armed pid, `enif_demonitor_process` answers `0`/`1`, dead and undefined targets answer `1` (`nif_monitor.{c,erl}` + `nif_monitor_caller.erl`).
* `nif_binary_to_term_used.rs` — `enif_binary_to_term`'s consumed-byte count: a trailing-bytes image decodes its first term, a truncated one answers `0`.
* `nif_term_image.rs` — `enif_term_to_binary` of a local closure emits `NEW_FUN_EXT`, compared structurally (module/uniq/arity) rather than to a byte image.
* `nif_select.rs` — `enif_select` on real descriptors: delivery, the `STOP`/`CANCEL` bit answers, `CUSTOM_MSG`, the combined masks, the error-interest path and the per-interest reference family (10 cases).
* `nif_a3_thread_env.rs` — the NIF-created-thread host (`ForeignHost` over its private atom space) end to end, oracle-cross-checked.
* `nif_a3_schedule.rs` — the `enif_schedule_nif` trampoline: a reschedule chain, a nested schedule, the dirty flag and a continuation's raise, oracle-cross-checked.
* `nif_a3_resource_stop.rs` — `enif_select`'s `STOP` arm running the resource type's own `stop` callback (direct, scheduled, and a type opened without one), oracle-cross-checked.
* `nif_resource_gc.rs` — the caller-owned reachability prune of resource terms at the runner's park arm, observed through the destructor's own counts.
* `nif_resource_nested.rs` — the nested-site half of the same prune on the `-eval` route, where the nested run's epilogue is the only reclamation site.
* `etf_roundtrip_pid_ref.rs` — pid and reference ETF round trips keep identity in both directions.

### GC / heap
* `gc_collect.rs` — the collector at the engine's heap-admission point (`gc_loop`/`gc_ints`/`gc_floats`/`preempt.beam`).
* `gc_run.rs` — the root-set fix end to end: `erl_error:format_exception/3` moves the heap mid-body and then reads the argument it was handed across the collection.
* `gc_strategy.rs` — the runtime's own spawn `HeapPolicy` selects the mature-generation strategy through the automatic rule (`gc_payload`/`heap_growth.beam`).
* `gc_immix_payload.rs` — the measured `Copying`-vs-`Immix` runtime workload and live-payload survival (`gc_payload.beam`).
* `heap_growth.rs` — a scheduled process's heap really grows to fit a request the nursery cannot hold (`heap_growth`/`reds_boundary.beam`).
* `heap_limits.rs` — an allocation-heavy loop completes and a workload past the term-heap limit raises `error:system_limit`, never a host allocation diagnostic.
* `garbage_collect.rs` — `erlang:garbage_collect/0,1,2`'s observable contract (return values and the other-process forms), oracle-cross-checked.
* `off_heap_reclaim.rs` — the slice-boundary off-heap refc-binary reclamation on the `run` route: `erlang:memory(binary)` is flat across a 10× iteration step and a retained 64 KiB payload survives thousands of prunes byte-exactly.
* `rss_bound.rs` — the `churn.beam` allocate-and-abandon loop: peak `VmHWM` does not scale with the loop count, and the loop really collects.
* `foreign_root_loop.rs` — the `vp` loop holding one registered-BIF tuple live across its whole run: the exact fill law `(N + 4) / 13` on the process's own scanned young heap.
* `max_heap_enforce.rs` — `max_heap_size` enforcement: a breach kills with the bare atom `killed`, `kill => false` logs the ERTS report, plus the `violates_floor` matrix and the capacity-not-occupancy rule (`maxheap.beam`).
* `max_heap_flag.rs` — `process_flag(max_heap_size, …)`'s previous-value map, the map form's overrides, and `process_info(self(), max_heap_size)`'s key order.

### ETS
* `ets_owner.rs` — ETS owner tracking and owner-death reclamation at the executable boundary, heir transfer included.

### Errors, terms, atoms, references
* `structured_errors.rs` — structured raise reasons (`{badkey,_}`, `{badmap,_}`, `{case_clause,_}`, `{badrecord,_}`) through compiled code and `Runtime::process_exit`, asserted structurally (`raise_structured.beam`).
* `function_clause.rs` — a failed head clause reaches `func_info` and raises the bare `function_clause`, not a capability gap (`function_clause_fixture.beam`).
* `atom_namespace.rs` — the `ExecutionContext::atoms` override: a compiled guard over an atom interned **after** load, through both `run_function` and `run_scheduled`, plus the `sofs` repro.
* `safe_binary_to_term.rs` — the `[safe]` active-export gate on `erlang:binary_to_term/2` across its whole route (accepted and refused exports, and the flag-free form).
* `reference_ids.rs` — one reference-id space (`beam_common::next_ref_id`) and provenance-based destination classification, never the id alone.
* `ref_id_space.rs` — the cross-producer collision-freedom proof: five producer entry points, interleaved round-robin, oracle-cross-checked; its per-producer worker legs are `#[ignore]`d and run in child processes of their parents.
* `atom_table_commit_alloc.rs` — the append-only atom-intern commit allocates `O(names)`, not `O(table)`, with an `AtomTable::clone` sensitivity control.
* `decode_scratch_alloc.rs` — the memoized ETF decode scratch allocates `O(term)`, independent of the shared table's size, with positive controls.

### Build / bootstrapping (A.9)
* `compile_file.rs` — `compile:forms/2` and `compile:file/2` driven in process, the artifact differentially compared (incl. a real stdlib module), plus the scheduled-route driver (`sched_compile_driver.erl`/`sched_compile_input.erl`).
* `selfhost_chain.rs` — the A.9 chain in one `-eval` invocation each: compile in process → `code:add_patha/1` → `code:load_file/1` → run, plus the code-store probe.
* `selfhost_breadth.rs` — **(opt-in)** the widened corpus sweep over every `lib/*/src/*.erl`, chunked by `BEAM_RT_BREADTH_APPS`/`_SHARD=i/n`/`_LIMIT`, with `BEAM_RT_BREADTH_SCHEDULED=1` selecting the spawned-compiler route.
* `selfhost_bootstrap.rs` — **(opt-in)** the staged self-hosted chain: one invocation rebuilds `epp` → `erl_scan` → `erl_lint` → `compile` and then compiles a stdlib module with the rebuilt compiler.

### File I/O and io devices
* `file_server.rs` — `file:`/`filelib:` reads over the native `file_server_2` floor (`read_file`, `open`, `list_dir`, `is_dir|is_file|is_regular`, wildcards).
* `io_scan.rs` — the io-device read path (`file:read/2`, `io:scan_erl_*`, `dflag_unicode_io/1`) incl. buffered-read sequences asserted three-way equal to a pinned value on the `run` route, the `-eval` route and the oracle.
* `scheduled_io_device.rs` — scheduled io-device round trips (`io:getopts/1`, `io:setopts/2`, `io:scan_erl_form/4`), each line oracle-cross-checked.
* `io_device.rs` — the native io floor's request wrappers, so an installed compiler `catch` handler's diagnostic is not replaced by `error:calling_self`.
* `io_diagnostic_unmask.rs` — that unmasking end to end: the compiler's own `Function: ~w/~w` report still prints before it re-raises.

### Introspection and tracing (A.8)
* `system_info_oracle.rs` — every served `system_info/1` key against the oracle, plus the guard that fails if an unclassified oracle-accepted key appears.
* `system_info_retained.rs` — a `system_info/1` value kept while a second call materializes survives (the scratch-lifetime pin).
* `process_info_items.rs` — `process_info/1,2`'s per-process control items against a parked real `.beam`, oracle-cross-checked.
* `process_info_binary.rs` — the `process_info(P, binary)` roster, **shape-only** (a payload pointer is an address), with the three measured divergences (counting, a released payload while parked, a copy-engine payload).
* `position.rs` — the four execution-position items from the **in-machine** BIF door (`recv.erl`/`pos_self.erl`), self query included.
* `trace.rs` — the trace-flag state and `erlang:trace_info/2`/`trace_delivered/1` at the BIF boundary.
* `trace_call.rs` — the `call`/`return_from`/`return_to` execution-trace events from a real `.beam` through the engine's seam.
* `trace_pattern.rs` — `erlang:trace_pattern/2,3` module/function/local matching and count/flag semantics.
* `trace_send_event.rs` — the `{trace, …}`/`{trace_ts, …}` send event delivered to the tracer's mailbox.
* `trace_timestamp.rs` — the three timestamp flavours report one clock: the monotonic pair agrees with `erlang:monotonic_time/0`'s origin.
* `perf_map.rs` — `+JPperf map`/`dump` through the built binary: the file name, path and line formatter the oracle produces.
* `crash_dump.rs` — `erlang:halt/0,1,2` and `ERL_CRASH_DUMP`: which shapes terminate with a status, an abort or a dump.

## Asserted residuals (current state, pinned by the suites above)
* The one `#[ignore]`d `inet_nested.rs` case: a `beam_io` accepted socket whose payload was buffered **before** the read armed raises no event, so this runtime answers `{error,closed}` where the oracle answers `{ok, <<"hi">>}`.
* `process_info(P, binary)`'s three shape differences (per-distinct-payload counting, a released payload while the child stays parked, a copy-engine payload that nothing registers).
* `enif_thread_type()` answers `ERL_NIF_THR_UNDEFINED` on the calling thread, a non-dirty continuation and the `stop` thread where the oracle answers the scheduler thread; `enif_schedule_nif`'s raw return word is not reproduced.
* The `-eval` route's spawn residuals: an in-engine `spawn` keeps the tree-walking entry, a closure the compile declines keeps its typed gaps, and `receive … after infinity` parks.
* `+hmax` does not bound the grown nursery **chain**.
* The `send`/inline-`!` route cannot park on a busy port (only `port_command` suspends); the busy wait is bounded by `port::busy_wait::BUSY_PORT_DEADLINE`.
* No self-hosted build: the runtime compiles every `lib/*/src/*.erl` source in process but does not compile or run itself.

## Application-level call chains (current state)
`compile_file.rs` drives the real `compiler` application in-process; no test drives `syntax_tools`, `eunit`, `xref`, `dialyzer` or `edoc`.
The other deepest chains are `proc_lib_start.rs` (`proc_lib:start[_link]/4` → `proc_lib:sync_start/2`), `multimodule.rs` (real stdlib modules loaded on demand), `io_scan.rs` (the `{get_until,…}` request driving the real `erl_scan`/`erl_parse`) and `boot.rs` (all 22 boot directives to completion, `{apply,{c,erlangrc,[]}}` included).
**A real stdlib module does not complete a compile on the scheduled `run` route** (`ordsets.erl` does, an order of magnitude slower than the `-eval` route; `calendar.erl`/`lists.erl` spend the whole window inside one `Runtime::scheduler_pass`, which the between-pass `RUN_DEADLINE` never sees) — the scheduled route's limit there is its own throughput.

## See Also
* [`fixtures/README.md`](fixtures/README.md) — the fixture index, the committed `.beam` images, the `## Regeneration` recipes and the `## Oracle values` table.
* [`fixtures/ERL_SOURCE_FIXTURES.md`](fixtures/ERL_SOURCE_FIXTURES.md) — the `.erl` fixtures compiled at test time.
* [`fixtures/PORT_AND_DRIVER_FIXTURES.md`](fixtures/PORT_AND_DRIVER_FIXTURES.md) — the C driver/NIF fixtures and the driver-port fixtures.
* [`../TEST_STRATEGY.md`](../TEST_STRATEGY.md) — the crate's test strategy; [`../CONTEXT.md`](../CONTEXT.md) — the suite→node map.
