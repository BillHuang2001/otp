# `beam_rt/tests/` — runtime integration suites (the executable + the `Runtime` façade)

## Intent
Black-box and glue-level end-to-end tests for the `beam_rt` binary and its `Runtime`/`RuntimeContext` façade.
Two drivers, NOT interchangeable: `Runtime::eval` / the CLI `-eval` action (the **single-pass** evaluator route — no scheduler slice) and the CLI `run <file.beam> <mod>:<fun>/<arity> [args]` action (`Runtime::run_scheduled`: a real scheduled process over the scheduler/poller/runner; `run_function`/`run_module` are the single-pass equivalents with a caller-owned output sink).
Where a suite has an oracle it runs the system `erl`/`erlc` and the oracle is the expected value.
Harness plumbing — the toolchain probe, the bounded oracle probe, the loud-skip helpers, the two live-node pair harnesses — is [`support/CONTEXT.md`](support/CONTEXT.md).

## Running
A missing **toolchain** (`erl`, `erlc`, `cc`, `erl_driver.h`) is a **loud skip** plus a `return`, never a silent pass.
A missing **committed fixture** is the other class — a repository defect, not a toolchain absence — and **fails** (`mailbox_rescan.rs`'s `Scratch::new`, `bif_entry_spawn.rs`'s staging, `port_failure.rs`'s `require_probe_beam`/`the_probe_image_is_committed`, `live_inbound.rs`'s `the_fixture_beam_is_present`), because a skip there would report green rows that compared nothing.
**Opt-in suites** are `#[ignore]`d and need `-- --ignored --nocapture`: `dist_two_vm.rs` (24 tests), `dist_two_rust.rs` (12), `dist_two_vm_wire.rs` (1, hermetic byte-level), `dist_payload_matrix.rs` (9), the corpus/measurement harnesses (`selfhost_breadth.rs`, `selfhost_bootstrap.rs`, `src/context/calls/tests.rs`, `src/context/gc_walk.rs`) and the one residual case in `inet_nested.rs`.
An `#[ignore]`d *worker* test a non-ignored parent re-runs in a child process (`ref_id_space.rs`, `nif_resource_gc.rs`, `nif_resource_nested.rs`) is **not** an opt-in: it is driven by its parent and must not be run through `--ignored` beside the parent.
- whole crate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt`
- one suite: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --test <name>`
- the pair suites, serially (each test owns a private `epmd` and two VMs, so a loaded host does not stretch the per-step bounds): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --test dist_two_vm --test dist_two_rust --test dist_two_vm_wire -- --ignored --test-threads=1`
- wall-clock on a shared 8-core host, both VMs booting per case so the figures move with load: `dist_two_vm` ≈52–110 s, `dist_two_rust` ≈26–34 s, `dist_two_vm_wire` ≈0.4 s
- the compatibility gate — authoritative, **re-run it**, never quote it from memory: `erts_rust/tools/differential/gate.sh`
- the gate-equivalent full-corpus sweep and the long-workload recipe: [`../TEST_STRATEGY.md`](../TEST_STRATEGY.md)

## Cross-cutting conventions
* **A suite compiled into its own scratch directory is the only `-pa` either runtime gets.** The shared `tests/fixtures/` directory is never named as one; each fixture is compiled or copied into a per-case directory holding only that module.
* **Scratch dirs come from `crate::context::test_support::scratch_dir(label)`.** A hand-rolled `temp_dir().join(format!("…_{pid}"))` collides with a concurrent test *process*.
* **Three harness shapes, not one.** (a) a suite comparing against the system `erl` spawns it through `support::{require_oracle, resolve_oracle, oracle_eval, run_bounded}` or a file-local copy (the copies exist where the child's **exit code** must survive); (b) a suite that only needs `erlc` builds a fixture and compares against a pinned golden; (c) a suite with no toolchain drives `env!("CARGO_BIN_EXE_beam_rt")` or the in-process `Runtime`/`RuntimeContext` against hard-coded expected text.
* **Every oracle probe must terminate and be bounded** — the standard probe is `erl -noshell [-pa DIR] -eval EXPR -s init stop` through `support::{resolve_oracle, oracle_eval, run_bounded}` (`/dev/null` stdin, `PROBE_TIMEOUT`, killed and reaped), or a file-local copy of the same shape where the child's **exit code** must survive. `resolve_oracle` probes the **filesystem** and never runs an `erl` to ask whether one exists.
* **Byte-exact stdout probes pass `-noshell`** — except on the `run` action, which is **banner-free by construction**: `src/cli.rs`'s `parse_run_file` records the `noshell` init argument.
* **`code:ensure_modules_loaded/1` is parallel in OTP**, so its failure pairs are asserted as a **set**, never in order.
* **A `beam_rt` node cannot render a remote identifier.** `pid_to_list/1` and `~p`/`~w` of an external pid or reference are typed, **uncatchable** capability gaps (exit 3), so the two-VM suites assert **term identity** (`=:=`, `is_pid`, a bound receive pattern) and `dist_two_vm_wire.rs` prints only ids it made up.
* **No suite compares live GC counters, `process_info(_, garbage_collection)` or `erlang:memory/0,1` value-for-value with the oracle.** The two oracle comparisons are `system_info(garbage_collection)` by value (`system_info_oracle.rs`) and `statistics(garbage_collection)` by shape (`runtime_hooks.rs`); every other GC suite reads the runtime's own `Runtime::gc_stats()`/`erlang:memory/1`.
* **The four `zlib` suites share one convention: six probe shapes, one golden** — `-eval` and the scheduled `run` route on both tiers, plus the oracle in both of its own shapes; each suite is self-contained (its own file-local oracle helper, so the probe's exit code and exact stdout survive) and compiles its source-only fixture into a per-fixture scratch directory.
* **A GC-accounting suite derives its bounds**, never a pinned capacity — through the policy arithmetic (`young_target_words`) or an allocation product, so a `beam_gc` sizing change moves it only through that derivation.
* **The two-VM fixture invariants are load-bearing, not defensive**: every `receive` is deadline-bounded (a bare `receive` empties the run queue, the port set and the timer heap, so `run_scheduled` reports its honest `blocked`, exit 3, before the peer's term can arrive); the process that must die is a `spawn`ed child, never the run action's target (if the target exits, the runtime exits and the outbound `DOP_EXIT`/`DOP_MONITOR_P_EXIT` it just routed never reaches the socket); and our side never speaks first (an outbound send does not dial — `DistLinks::route_*` answers `no_link_error`), so the peer pings and greets on a bounded retry schedule because a `DOP_REG_SEND` to a not-yet-registered name is silently dropped, by the oracle too.
* **The outbound monitor-down is always the plain `DOP_MONITOR_P_EXIT` (21) form**, never `DOP_PAYLOAD_MONITOR_P_EXIT`, even when the peer advertises `DFLAG_EXIT_PAYLOAD` (`src/dist_client/client.rs::encode_signal`, the C's `erts_dsig_send_m_exit`); the payload form is therefore deliberately unexercised by the wire suite.
* The pair harness itself — `Harness`/`PrivateEpmd`/`ChildVm`/`Pair`, the `setsid -w` session isolation, the group `TERM`-then-`KILL` reap, the private `epmd` on a private port, the load-bearing drop order — is [`support/CONTEXT.md`](support/CONTEXT.md)'s.
* Interfaces every suite shares: `env!("CARGO_BIN_EXE_beam_rt")` and `support::{run_bounded, oracle_eval, require_oracle, skip_loudly, …}`.

## Routing Table
One line per suite file: what it pins, and what it needs — `erl` = the system oracle (or a live peer), `erlc`/`cc` = a compiler, none = just the built binary.
The two-VM fixture invariants the dist suites depend on are in **Cross-cutting conventions** above; the harness itself (probes, skip helpers, the pair harness) is `support/` → [`support/CONTEXT.md`](support/CONTEXT.md).

### Core execution / vertical slice
* `slice.rs` — the library slice: load a real `.beam` and run an export through `Runtime::run_function`/`run_module` (none).
* `cli.rs` — the built binary runs the slice and pins the usage/exit-code contract (`0`/`1`/`2`/`3`) (none).
* `mailbox.rs` — the receive/message slice over the real `RuntimeContext` (`fixtures/mailbox.beam`) (none).
* `mailbox_rescan.rs` — the receive scan position across a suspend/resume, six oracle-cross-checked exports (plain, marker'd, marker-behind-junk, two receives, a fired `after`) (`erl`).
* `scheduler.rs` — several processes, one blocked in `receive`, over the real scheduler (`fixtures/sched.beam`) (none).
* `preempt.rs` — the reduction budget pre-empts a loop (`fixtures/preempt.beam`) (none).
* `multimodule.rs` — on-demand cross-module load of real stdlib (`lists`/`maps`/`ordsets`/`binary`) from a compiled fixture (`erl`).
* `proc_lib_start.rs` — `proc_lib:start[_link]/3,4` across a module boundary (`plstart.erl`/`plwait.erl`) (`erlc`).
* `fun_entry_receive.rs` — a fun-bodied process entry that blocks in `receive`, inline and one `call_ext` away (`erlc`).
* `scheduled_apply_seam.rs` — the `call_fun`/`call_fun2` path: a scheduled process enters a blocking closure in-machine (`erl`).
* `spawn_from_slice.rs` — every spawn form from inside a scheduler slice, a nested spawn and a rejected option list included (`erlc`).
* `spawn_monitor.rs` — `spawn_monitor/1,3` and the `{'DOWN',…}` it owes (`erlc`).
* `spawn_opts.rs` — `spawn_opt/2,3,4`'s heap options reach the child's `HeapPolicy` and answer through `process_info/2`'s heap items (`erlc`).
* `spawn_opt_heap.rs` — the `min_heap_size`/`max_heap_size` **acceptance** rule (`ok` vs `badarg`), order-irrelevant (`erl`).
* `min_heap_size.rs` — `HeapPolicy::initial_heap_words` is the one authority behind `+hms`/`+hmbs` and `spawn_opt`'s `min_heap_size`, floored at the global policy (`erl`).
* `bif_entry_spawn.rs` — an entry naming a **host-served** MFA is dispatched to the BIF, not entered as `erlang.beam`'s stub, plus the `-eval` `receive … after T` timer wait (`erl`).
* `signal_self_delivery.rs` — a `'DOWN'`/`'EXIT'` a process queues on **itself** is delivered before it sleeps (`erlc`).
* `monitor.rs` — local `monitor/2,3`/`demonitor/1,2` and the DOWN the runtime owes (`erl`).
* `alias.rs` — reference aliases as destinations: `alias/1`/`unalias/1`, `monitor/3`'s `{alias,Mode}`, the compiled `Alias ! Msg`, `erlang:send/2`, `gen:reply/2` and real `gen_server` calls (`erl`).
* `exit.rs` — local exit signals: trapping, `kill`, `normal` (`erlc`).
* `group_leader.rs` — group-leader inheritance, `group_leader/2`, and the io-device default that follows it (`erl`).
* `timers.rs` — the runtime timer heap behind `send_after`/`start_timer` (relative and `{abs,true}`) and `cancel_timer` (`erl`).
* `read_timer.rs` — `erlang:read_timer/1,2` end to end: a live timer's remaining ms, `false` after it fires, a read that is not a cancellation (`erl`).
* `proc_seams.rs` — the `ProcessHook` seam behind eleven process/name/global-state BIFs, nine served (`erl`).
* `action_identity.rs` — the action body's own identity: freshly spawned, unregistered (`whereis(init) =:= self()` is `false`) (`erlc`).
* `action_result.rs` — the action's returned term survives the action child's teardown, and the action leaves no process behind (`erlc`).
* `run_route_sleep.rs` — the `run` route's pass wait: no blocking wait while the round still ran slices, the anti-spin half for a parked `after T` (`erl`).
* `boot_repeat.rs` — a second in-process `Runtime` boot must not run stale baseline-JIT compiled blocks (none).

### CLI / configuration (A.6)
* `cli_flags.rs` — the `ERL_OTP<vsn>_FLAGS` → `ERL_AFLAGS` → cmdline → `ERL_FLAGS` → `ERL_ZFLAGS` merge (`erl`).
* `cli_env.rs` — the `ERL_*` environment variables and the init arguments they record (`erl`).
* `cli_plain_args.rs` — the plain-argument surface: `-extra`/`--` select `init:get_plain_arguments/0`, are never init arguments, `-extra` terminates flag parsing, and `parse`'s `RuntimeConfig::plain_args` split (nine oracle-checked rows) (`erl`).
* `cli_compact_flags.rs` — the attached `+`-flag forms (`+S1`, `+A0`, `+P65536`, `+hms233`, `+sbtu`, …) against their separated twins, the `erlc` front-end's driven figures, the numeric switches' C digit-prefix agreement, the `+sbwt*` value refusals and `+` switches never being init arguments (`erl`).
* `perf_bif.rs` — the perf harness's CLI contract: `run` prints the returned value and requires one argument per declared arity (exit 2 otherwise) (none).
* `perf_call.rs` — drives the built `perf_call` example: every claimed row prints under its own block (`erlc`; loud skip and exit 0 without it).
* `tier_selection.rs` — the `BEAM_RT_TIER` surface: a feature-off `cranelift` request is refused at the CLI boundary before any boot work (exit 3), `-version` still answers, the unset/`auto`/`baseline`/`interpreter` rows still print `55`, and the three-tier equivalence row (`slice:fib/1 10`, `recv_after:after20/0`, `pingpong:go/1 50`) (none).
* `binary_append_tier_equivalence.rs` — the writable-accumulator binary append (`bs_init_writable` + a first-position `private_append`; the accumulator is a `[BinRef][ErlSubBits]` pair a collection may split) agreeing across every served tier, `run/1 32` ⇒ `2097152` (none).

### Boot / preloaded / native floor / code store (A.7)
* `boot.rs` — the real `bootstrap/bin/start.boot` driven to completion (22 directives, ending at index 21), the CLI `-boot` route, and the live `logger_server` registered as `logger` (`erl`).
* `boot_code_path.rs` — `-pa`/`-pz` take effect on the `-boot` route before the script's first directive, with the oracle's `-pa`-before-`-pz` ordering (`erl`).
* `native_init.rs` — the native `init` floor: `get_argument`, `script_id`, `restart/0`, `boot/1`, `stop/0,1` (`erl`).
* `a9_native_floor.rs` — the A.9 floor surface, incl. `init:script_id/0`'s **loaded** script header on the CLI routes, and the preloaded set (`erl`).
* `fetch_loaded.rs` — the boot-time `{From,fetch_loaded}` list is drained and cleared at boot, so every post-boot fetch answers the empty list (`erl`).
* `code_load_file.rs` — `code:load_file/1`/`code:ensure_modules_loaded/1`'s error vocabulary and the expected-name refusal, on the file routes and the call-like routes (`erl`).
* `code_ensure_modules_loaded.rs` — the floor key that makes `resolve_external_call` refuse to enter `code.beam`'s own body, so `{error,[{Name,nofile}]}`/`ok` agree (`erl`).
* `code_delete_live.rs` — `code:delete/1`, `erlang:delete_module/1`, `code:purge/1`, `code:soft_purge/1` through the live `code_server` and the native floor, on both routes (`erl`).
* `code_add_path.rs` — the code-path mutators (`add_patha|add_pathz|add_path|del_path/1,2`) really move the loader (`erl`).
* `code_path.rs` — the **whole** `code:get_path/0` list element-wise in order against the oracle, plus the `$ERL_LIBS` shape and a blank entry (`erl`).
* `code_load_binary.rs` — `code:load_binary/3`'s guard/`modp`/`badfile`/sticky/valid-image shapes (`erl`).
* `prim_load_atoms.rs` — only the pre-`{kernel_load_completed}` `{primLoad, …}` batch loads, so `list_to_existing_atom/1` agrees (`erl`).
* `module_loaded.rs` — `erlang:module_loaded/1` over the preloaded set plus the live code store (`erl`).
* `module_info.rs` — `Module:module_info/0,1` and `get_module_info/1,2` through the `ModuleInfo` seam; documents its own `exports`-order divergence (`erl`).
* `records.rs` — the native-record opcode family, `put_record/6`'s create form included; the colocated `src/context/records.rs` units pin the **foreign** owner on both routes plus `{badrecord, Id}` (none).
* `runtime_hooks.rs` — the three served `erlang:*` `RuntimeHooks` keys plus the `statistics(garbage_collection)` shape (`erl`).
* `dict.rs` — the process dictionary through the `DictHook` seam, and the `undef` reason producers (`erl`).
* `persistent_term.rs` — the persistent-term store, incl. a stored value surviving a process heap collection (`pt_gc.beam`) (`erl`).
* `prim_tty.rs` — the built-in `prim_tty` NIF library: `load/0`, the served keys, the opaque `tty_create/1` round trip, the typed stubs, the no-`HAVE_TERMCAP` deviation (`erl`).
* `zlib_stream_levels.rs` — `zlib`'s stream API through the real preloaded `zlib.beam`: per-level 0–9 byte-count vectors, flush modes, `deflateInit/1,2,6`, `deflateReset/1`, `deflateParams/3`, round trips (`erl`+`erlc`).
* `zlib_stream_dict.rs` — `deflateParams/3`'s stash path, the `need_dictionary` protocol with its Adler digest, `safeInflate/2` chunking, the reset/dictionary accessors, a truncated stream only `inflateEnd/1` catches (`erl`+`erlc`).
* `zlib_guards.rs` — the wrapper's refusals (`data_error`, `stream_error`, `not_initialized`, `already_initialized`, `badarg`, `not_on_controlling_process`), `set_controlling_process/2` in every state, `close/1` idempotence, a reason atom absent from the raising module's table (`erl`+`erlc`).
* `zlib_one_shot.rs` — the six convenience functions' fast paths, the public stream wrapper byte-identical to `compress/1`/`zip/1`/`gzip/1`, `gunzip/1` over concatenated members, `zlib:compress/1`'s libz byte-exactness (`erl`+`erlc`).

### Inbound `-eval` route
The action's own process never becomes a scheduler slice: its `receive … after T` waits through a mailbox scan, a non-blocking pump bounded by `RECEIVE_PUMP_SLICES = 64`, one port-poll slice and `Runtime::wait_for_timer`.
* `eval.rs` — the `-eval` evaluator against the differential corpus's `Eval` probes, pinning the golden values the corpus asserts (none).
* `eval_order.rs` — sub-expression evaluation order (`erl`).
* `eval_bits.rs` — binary construction and matching in `-eval`: little-endian non-byte-aligned segments, `float`, `utf8/16/32`, wider-than-64-bit patterns, `unit:` (`erl`).
* `bignum_bit_segment.rs` — both `<<…>>` implementations mask a bignum at **every** width: the `-eval` bit-buffer builder and the engine's `bs_create_bin` (`erl`).
* `eval_region.rs` — the per-statement scratch window: a value held across twenty throwaway builds is never reclaimed (a `persistent_term` value, an environment binding, a mailbox message, the family the descent cannot enumerate) (none).
* `eval_pool_result.rs` — the BIF-result copy-out and pool rewind on the table-less route: a container used later prints correctly, a thousand calls stay at one call's high-water under `+hmax`, one retained across sub-expressions survives (none).
* `eval_off_heap_reclaim.rs` — the escaped-payload store is pruned at each statement boundary, so an expression building `N` off-heap payloads holds `O(1)` (none).
* `eval_port_receive.rs` — a port frame drained by `-eval`'s `receive … after` lands in the owning mailbox, so non-matching frames survive for the next receive (`erl`).
* `eval_spawn.rs` — a spawned `-eval` closure with recorded source is compiled into a synthetic `beam_rt_eval_fun_<index>` module, loaded with `code:load_binary/3` and run as a real `ProcessEntry::Export`, so it parks in a bare `receive` and in `timer:sleep/1` and resumes on a message (none).
* `eval_exempt_receiver_signal.rs` — a `'DOWN'`/`'EXIT'` control signal aimed at the `-eval` route's relocation-exempt root, with a roomy and a saturated nursery (none).
* `send.rs` — `erlang:send/2`/`erlang:'!'/2` over the `SendHook` seam (local, remote, port and alias destinations) (`erl`).
* `empty_binary_result.rs` — the `run` route prints an empty binary from both representations (on-heap `<<>>`, off-heap refc accumulator) (none).
* `engine_off_heap_transport.rs` — a scheduled process's result and exit reason are copied off its dying heap with the refc payload handle resolved (none).

### Ports, drivers and sockets (A.4)
* `driver.rs` — a real C `.so` driver: `driver_select` + `driver_set_timer`, `output` and `outputv`-only entries, `erl_drv_output_term`/`erl_drv_send_term`, and the port-less registry hooks (`event_driver.c`, the inline `reg.c`/`reg_recv.erl` pair) (`cc`).
* `port_failure.rs` — a driver that fails its **own** port from `output`: the `driver_failure*`/`driver_exit` rows on both routes (`port_failure_driver.c` + committed `port_failure.beam`) (`cc`).
* `port_busy.rs` — the `port_command/2,3` busy/`[force]` table over a real busy driver in both `driver_flags` variants, and the BIF-suspend park (`busy_driver.c`) (`cc`).
* `port_parity.rs` — `open_port`/`port_info`/`port_command` shapes against the oracle (`erl`).
* `port_info.rs` — `port_info/1`'s C roster plus the derived `/2` items (`queue_size`, `memory`, `monitored_by`, `locking`, `parallelism`), with no `/2`-only item leaking into `/1` (`erl`).
* `port_bif.rs` — the port data BIFs (`port_set_data/2`, `port_get_data/1`, `port_connect/2`, …) under their fully qualified spellings, byte-identical to the oracle (`erl`).
* `port_exit.rs` — the port `'EXIT'`/`'DOWN'` a close owes trapping, monitoring and `port_connect/2`-ed owners (`port_exit.beam`) (`erl`+`erlc`).
* `port_stream.rs` — the port-stream read at the library boundary: the `{Port,{data,Data}}` term, `binary` vs charlist rendering (`erl`).
* `port_wake.rs` — a receiver parked on a spawn-driver port's stream is woken by the poller, never by another process's send (`port_recv.beam`) (`erlc`).
* `port_monitor_cleanup.rs` — an incoming port monitor's cleanup when the **watcher** dies first (`port_watch.erl`) (`erlc`).
* `inet_active.rs` — the `{active,true|N|once}` socket-input channel over real loopback traffic, seven oracle-cross-checked cases (`inet_active.erl`) (`erl`+`erlc`).
* `inet_completion.rs` — the driver→owner completion channel on the `run` route: a parked UDP recv completed by a later datagram, an accepted descriptor adopted into a live `tcp_inet` port, a non-blocking `gen_udp` round trip (`erlc`).
* `inet_nested.rs` — the nested route (`-eval`, `-s`, boot) really waits for the driver's `{inet_async, …}` (`wait_for_port_event`), incl. the typed port-event deadline; one `#[ignore]`d residual (`erlc`).

### Distribution (A.5)
* `dist.rs` — a `-sname`/`-name` node binds a listener and really publishes it to `epmd` (the port `beam_dist::epmd::lookup_port` resolves), and a real `erl` peer's `net_kernel:connect_node/1` completes the `'N'` handshake, in-process and through the binary (`erl`).
* `dist_delivery.rs` — the inbound delivery matrix hermetically over a hand-shuttled `ConnectionDriver` pair (32 tests): message families by delivered effect, the per-link checked drain, the route-free receipt families by installed local state, `every_routed_inbound_family_routes_nothing_outbound`, and the honest `Unsupported` rows (none).
* `dist_payload_matrix.rs` — **(opt-in, 9)** the *size* a link carries: every leg arrives through `LocalDeliverySink::enqueue_fragmented` → `beam_message::send_fragmented` → the fragment **read** path in `loop_rec` (`erl`).
* `dist_client.rs` — the connecting half against a real `erl -sname` peer: `epmd` resolution, the `'N'` handshake to `Established`, a real `DOP_REG_SEND` asserted on the peer's stdout (`erl`).
* `exit_remote.rs` — remote `erlang:exit/2` hermetically (5 tests): `kill` untrappable, `normal` sparing a non-trapping target, a trapping target getting the message, a dead target answering `NoSuchProcess` (none).
* `live_inbound.rs` — a real `erl` peer `DOP_REG_SEND`s into a **running local process**, in-process and over the CLI (3 tests) (`erl`).
* `live_signals.rs` — the outbound link/exit pair and the inbound monitor direction against a real peer, each with its byte-identical negative control (5 tests) (`erl`).
* `live_remote_families.rs` — outbound `unlink` (`DOP_UNLINK_ID`) and inbound `DOP_ALIAS_SEND`, each with its negative control (4 tests) (`erl`).
* `live_remote_exit.rs` — remote `erlang:exit/2` (`DOP_EXIT2`, not the link exit) against a real peer (2 tests) (`erl`).
* `net_adm.rs` — the connector BIFs reached over a live peer (`erl`).
* `remote_send.rs` — remote and name destinations: the non-node capability gaps, the three-routes-agree check (compiled `!`, `erlang:send/2`, the `-eval` `!`), the same-node arm and the live `{Name,PeerNode} ! Msg` (8 tests) (`erl`).
* `node_cookie.rs` — the cookie and node-name BIFs through the real binary (`erl`).
* `pid_roundtrip.rs` — a pid/port/reference wire round trip and the identity it must preserve (`erl`).
* `dist_two_vm.rs` — **(opt-in, 24)** rust ↔ real `erl` on one private `epmd`: exchange and node monitors, the peer acting on our process, us acting on a peer process, both alias directions, the pid identity round trip (`erl`).
* `dist_two_rust.rs` — **(opt-in, 12)** two `beam_rt` nodes, asserting **term identity** (neither peer can render a remote identifier) (none).
* `dist_two_vm_wire.rs` — **(opt-in, 1, hermetic)** the outbound monitor-down decoded byte-level: opcode 21, five elements, the **watched** local pid first, the peer-minted reference echoed, plus the negotiated `DFLAG` set (none).
* `support/` — the one two-VM harness (`Harness`/`PrivateEpmd`/`ChildVm`/`Pair`, the shared `two_vm` fixture, the payload-matrix harness): [`support/CONTEXT.md`](support/CONTEXT.md).

### NIF (A.3)
* `nif.rs` — a real C NIF: loading, dispatch and the `enif_*` surface (incl. the inline `env_nif`/`env_caller` `enif_alloc_env` pair) (`cc`).
* `nif_send.rs` — `enif_send` really delivers: the `NULL`-env and `msg_env`/`enif_make_copy` forms, and the C's `0` for a dead pid (`cc`).
* `nif_send_load.rs` — a `load` callback's `enif_send` is queued on the load host and drained at callback exit, read by a self-receiver and a parked child (`cc`).
* `nif_monitor.rs` — the stateful monitor family: the resource's `down` fires exactly once with the armed pid, `enif_demonitor_process` answers `0`/`1`, dead and undefined targets answer `1` (`cc`).
* `nif_binary_to_term_used.rs` — `enif_binary_to_term`'s consumed-byte count: a trailing-bytes image decodes its first term, a truncated one answers `0` (`cc`).
* `nif_term_image.rs` — `enif_term_to_binary` of a local closure emits `NEW_FUN_EXT`, compared structurally (module/uniq/arity) rather than byte-for-byte (`cc`).
* `nif_select.rs` — `enif_select` on real descriptors: delivery, the `STOP`/`CANCEL` bit answers, `CUSTOM_MSG`, the combined masks, the error-interest path and the per-interest reference family (10 cases) (`cc`).
* `nif_a3_thread_env.rs` — the NIF-created-thread host (`ForeignHost` over its private atom space) end to end (`cc`+`erl`).
* `nif_a3_schedule.rs` — the `enif_schedule_nif` trampoline: a reschedule chain, a nested schedule, the dirty flag, a continuation's raise (`cc`+`erl`).
* `nif_a3_resource_stop.rs` — `enif_select`'s `STOP` arm running the resource type's own `stop` callback (direct, scheduled, and a type opened without one) (`cc`+`erl`).
* `nif_resource_gc.rs` — the caller-owned reachability prune of resource terms at the runner's park arm, observed through the destructor's own counts (`cc`).
* `nif_resource_nested.rs` — the nested-site half of the same prune on the `-eval` route, where the nested run's epilogue is the only reclamation site (`cc`).
* `etf_roundtrip_pid_ref.rs` — pid and reference ETF round trips keep identity in both directions (`erl`).

### GC / heap
* `gc_collect.rs` — the collector at the engine's heap-admission point (`gc_loop`/`gc_ints`/`gc_floats`/`preempt.beam`); its bounds derive from the policy (none).
* `gc_run.rs` — the root-set fix end to end: `erl_error:format_exception/3` moves the heap mid-body and then reads the argument it was handed (`erl`).
* `gc_strategy.rs` — a spawned `HeapPolicy` selects the mature-generation strategy through the automatic rule (`gc_payload`/`heap_growth.beam`) (none).
* `gc_immix_payload.rs` — the measured `Copying`-vs-`Immix` payload workload and live-payload survival, compared through the allocation product (`collections × mean nursery capacity`) (`erlc`).
* `gc_strategy_classes.rs` — the measured `Copying`-vs-`Immix` A/B on messaging/copy-bound (`send_recv`, `ets_copy`) and in-process-compile classes, arms one configured word apart at `IMMIX_BREAK_EVEN_WORDS`, the intended `MatureKind` asserted **inside** the measured process, interleaved paired medians with the noise floor; record in [`../GC_STRATEGY_MEASURED.md`](../GC_STRATEGY_MEASURED.md) (`erlc`).
* `heap_growth.rs` — a scheduled process's heap really grows to fit a request the nursery cannot hold (`heap_growth`/`reds_boundary.beam`) (none).
* `heap_limits.rs` — an allocation-heavy loop completes and a workload past the term-heap limit raises `error:system_limit`, never a host allocation diagnostic (none).
* `garbage_collect.rs` — `erlang:garbage_collect/0,1,2`'s observable contract (return values and the other-process forms) (`erl`).
* `off_heap_reclaim.rs` — the slice-boundary off-heap refc-binary reclamation on the `run` route: the **phase-immune dense envelope**, the live-reference regression guard, the flat single-read sibling and the anti-dangle half — mechanism and figures in [`../M1_GAPS_GC.md`](../M1_GAPS_GC.md), the module docs (`erl`).
* `rss_bound.rs` — the `churn.beam` allocate-and-abandon loop: peak `VmHWM` does not scale with the loop count, and the loop really collects (none).
* `foreign_root_loop.rs` — the `vp` loop holding one registered-BIF tuple live across its whole run: the exact fill law `(N + 4) / 13` on the process's own scanned young heap (none).
* `nested_run_allocation_heap.rs` — the profile-independent structural pin behind the nested-run heap fix: a container a nested run's engine block builds lies in the running process's **scanned** young-heap range with no arena fallback, survives a growth inside the relocation-exempt window, then moves with its children under a real `Process::collect` (none).
* `parked_exit_reclaim.rs` — a process that dies while its engine sits parked gives its off-heap payloads back (`RunnerState::prune_dead` at the scheduler-pass boundary), witnessed on `binary_alloc`'s live bytes (none).
* `max_heap_enforce.rs` — `max_heap_size` enforcement: a breach kills with the bare atom `killed`, `kill => false` logs the ERTS report, plus the `violates_floor` matrix and the capacity-not-occupancy rule (`maxheap.beam`) (none).
* `max_heap_flag.rs` — `process_flag(max_heap_size, …)`'s previous-value map, the map form's overrides, `process_info(self(), max_heap_size)`'s key order (none).

### ETS
* `ets_owner.rs` — ETS owner tracking and owner-death reclamation at the executable boundary, heir transfer included (none).

### Errors, terms, atoms, references
* `structured_errors.rs` — structured raise reasons (`{badkey,_}`, `{badmap,_}`, `{case_clause,_}`, `{badrecord,_}`) through compiled code and `Runtime::process_exit`, asserted structurally (`raise_structured.beam`) (none).
* `structured_reason_lifetime.rs` — an uncaught raise's structured reason renders as its real term on both escape routes **after** the heap that built it is gone, so every case runs the built binary as a subprocess and cross-checks the oracle's `~w` (`erl`+`erlc`).
* `decoded_fun_badfun.rs` — applying a decoded `NEW_FUN_EXT` closure on the default and the interpreter tier: the genuine descriptor applies, an identity naming no lambda is `{badfun, F}` with `F =:= G`, a module with no loadable code is `error:undef` whose synthesized innermost trace item names the applied fun and fabricates nothing (`erl`+`erlc`).
* `function_clause.rs` — a failed head clause reaches `func_info` and raises the bare `function_clause`, not a capability gap (none).
* `atom_namespace.rs` — the `ExecutionContext::atoms` override: a compiled guard over an atom interned **after** load, through both `run_function` and `run_scheduled`, plus the `sofs` repro (none).
* `safe_binary_to_term.rs` — the `[safe]` active-export gate on `erlang:binary_to_term/2` across its whole route (accepted and refused exports, and the flag-free form) (`erl`).
* `reference_ids.rs` — one reference-id space (`beam_common::next_ref_id`) and provenance-based destination classification, never the id alone (none).
* `ref_id_space.rs` — the cross-producer collision-freedom proof: five producer entry points, interleaved round-robin, oracle-cross-checked; its per-producer worker legs are `#[ignore]`d and run in child processes of their parents (`erl`).
* `atom_table_commit_alloc.rs` — the append-only atom-intern commit allocates `O(names)`, not `O(table)`, with an `AtomTable::clone` sensitivity control (none).
* `decode_scratch_alloc.rs` — the memoized ETF decode scratch allocates `O(term)`, independent of the shared table's size, with positive controls (none).

### Build / bootstrapping (A.9)
* `compile_file.rs` — `compile:forms/2` and `compile:file/2` driven in process, the artifact differentially compared (incl. a real stdlib module), plus the scheduled-route driver (`sched_compile_driver.erl`/`sched_compile_input.erl`) (`erl`+`erlc`).
* `compile_scheduled_gate.rs` — the **scheduled** in-process compile of a real `lib/compiler/src/*.erl` module into a pre-populated (dirty) output directory: clean, and it takes the debt-gated in-machine route (`erlc`).
* `selfhost_chain.rs` — the A.9 chain in one `-eval` invocation each: compile in process → `code:add_patha/1` → `code:load_file/1` → run, plus the code-store probe (none).
* `selfhost_breadth.rs` — **(opt-in)** the widened corpus sweep over every `lib/*/src/*.erl`, chunked by `BEAM_RT_BREADTH_APPS`/`_SHARD=i/n`/`_LIMIT`, with `_SCHEDULED=1`, `_PA=a:b:c`, `_COMPARE=artifact`, `_MAX_BYTES` and `_DEADLINE_S` — recipes in [`../TEST_STRATEGY.md`](../TEST_STRATEGY.md) (`erlc`).
* `selfhost_bootstrap.rs` — **(opt-in)** the staged self-hosted chain: one invocation rebuilds `epp` → `erl_scan` → `erl_lint` → `compile` and then compiles a stdlib module with the rebuilt compiler (none).
* `erlc_emulator.rs` — the `ERLC_EMULATOR=<beam_rt> erlc …` route: a plain and an option-carrying compile each exit 0 with empty stdout/stderr and an artifact byte-identical to the system `erlc`'s, both load under the oracle, and a warning/error source agrees byte for byte (`erl`+`erlc`).
* `path_length_independence.rs` — the address-layout-sensitive in-process compile abort: the real `run <dir>/selfhost_docfile.beam …` route over 18 **absolute** staging-directory lengths (the crashing `58` included) must exit 0 with empty stderr and identical stdout at every one (`erl`+`erlc`).

### File I/O and io devices
* `file_server.rs` — `file:`/`filelib:` reads over the native `file_server_2` floor (`read_file`, `open`, `list_dir`, `is_dir|is_file|is_regular`, wildcards) (`erl`).
* `io_scan.rs` — the io-device read path (`file:read/2`, `io:scan_erl_*`, `dflag_unicode_io/1`) incl. buffered-read sequences asserted three-way equal on the `run` route, the `-eval` route and the oracle (`erl`).
* `scheduled_io_device.rs` — scheduled io-device round trips (`io:getopts/1`, `io:setopts/2`, `io:scan_erl_form/4`), each line oracle-cross-checked (`erl`).
* `io_device.rs` — the native io floor's request wrappers, so an installed compiler `catch` handler's diagnostic is not replaced by `error:calling_self` (`erl`).
* `io_diagnostic_unmask.rs` — that unmasking end to end: the compiler's own `Function: ~w/~w` report still prints before it re-raises (`erl`+`erlc`).
* `display_oracle.rs` — `erlang:display/1` byte-for-byte against the live `erl`: the `%T` renderer's record/map/float/atom/printable-list shapes and the `\r\n` terminator, over a compiled fixture and over `-eval` literals (`erl`+`erlc`).

### Introspection and tracing (A.8)
* `system_info_oracle.rs` — every served `system_info/1` key against the oracle, plus the guard that fails if an unclassified oracle-accepted key appears (`erl`).
* `system_info_retained.rs` — a `system_info/1` value kept while a second call materializes survives (the scratch-lifetime pin) (none).
* `term_to_string_oracle.rs` — `erts_internal:term_to_string/1,2` byte for byte against the live `erl`: the `%T` renderer (`#{a=>1}`, `%e` floats, a printable list as `"hello"`) and the C's argument rule, through the native floor (`erl`).
* `process_info_items.rs` — `process_info/1,2`'s per-process control items against a parked real `.beam` (`erl`).
* `process_info_binary.rs` — the `process_info(P, binary)` roster, **shape-only** (a payload pointer is an address), for the self query (atom and list form), the empty row and the other-pid row (`erl`).
* `position.rs` — the four execution-position items from the **in-machine** BIF door (`recv.erl`/`pos_self.erl`), self query included (`erl`).
* `trace.rs` — the trace-flag state and `erlang:trace_info/2`/`trace_delivered/1` at the BIF boundary (`erl`).
* `trace_call.rs` — the `call`/`return_from`/`return_to` execution-trace events from a real `.beam` through the engine's seam (none).
* `trace_pattern.rs` — `erlang:trace_pattern/2,3` module/function/local matching and count/flag semantics (`erl`).
* `trace_send_event.rs` — the `{trace, …}`/`{trace_ts, …}` send event delivered to the tracer's mailbox (`erl`).
* `trace_timestamp.rs` — the three timestamp flavours report one clock: the monotonic pair agrees with `erlang:monotonic_time/0`'s origin (none).
* `perf_map.rs` — `+JPperf map`/`dump` through the built binary: the file name, path and line formatter the oracle produces (`erl`).
* `crash_dump.rs` — `erlang:halt/0,1,2` and `ERL_CRASH_DUMP`: which shapes terminate with a status, an abort or a dump (none).

### Fixture hygiene
* `fixtures_are_path_free.rs` — the path-free guard over every committed `.beam` in `fixtures/`, enumerated **recursively** at run time: fails on any absolute-path marker (`/home/`, `/tmp/`, `/Source/`, `/Users/`, `/private/`, `/nix/store/`, `.genesis/`) or Windows drive-letter path anywhere in the raw image, and on one hiding in a zlib-framed chunk payload after inflating it; non-vacuous, with synthetic positives and relative-path negative controls (none).

## Fixtures
`fixtures/` holds the committed `.beam`/`.erl`/`.c` fixtures; each one's recipe and provenance is that directory's own docs ([`fixtures/README.md`](fixtures/README.md), [`fixtures/ERL_SOURCE_FIXTURES.md`](fixtures/ERL_SOURCE_FIXTURES.md), [`fixtures/PORT_AND_DRIVER_FIXTURES.md`](fixtures/PORT_AND_DRIVER_FIXTURES.md)) — never restated here.
A source-only fixture is compiled at test time by the system `erlc`/`cc` into a per-case scratch directory; a committed `.beam` is used where a suite must run with no toolchain at all, and a missing one of those **fails** rather than skipping.
`fixtures_are_path_free.rs` is the guard over every committed image.

## Asserted residuals (current state, pinned by the suites above)
* **A fixed-offset read of `erlang:memory(binary)` samples a slice phase, not retention** — the park releases the *whole* reservoir, so a rung legitimately ladders; the suites assert the phase-immune dense envelope (`{min, max, trough, parks}`, peak = one pre-emption window, trough = one payload) instead, and the one shape the loop genuinely does not bound (a tail-call-only body charges no reduction → 0 parks) is measured and documented, not asserted. Mechanism and figures: [`../M1_GAPS_GC.md`](../M1_GAPS_GC.md), `off_heap_reclaim.rs`'s module docs.
* The one `#[ignore]`d `inet_nested.rs` case: a `beam_io` accepted socket whose payload was buffered **before** the read armed raises no event, so this runtime answers `{error,closed}` where the oracle answers `{ok, <<"hi">>}`.
* No suite covers the `process_info(P, binary)` **parked-child** shapes: a payload a `beam_bifs` BIF built (`binary:copy/2`, `list_to_binary/1`) and one the target received through the copy engine are missing from the roster, so such a child counts fewer entries than the oracle; a *released* engine-built payload is listed by **both** sides, because a parked child runs neither a collection nor the `Yield` arm's prune.
* `enif_thread_type()` answers `ERL_NIF_THR_UNDEFINED` on the calling thread, a non-dirty continuation and the `stop` thread where the oracle answers the scheduler thread; `enif_schedule_nif`'s raw return word is not reproduced.
* The `-eval` route's spawn residuals: an in-engine `spawn` keeps the tree-walking entry, a closure the compile declines keeps its typed gaps, and `receive … after infinity` parks.
* `+hmax`'s nursery-chain ceiling holds inside `beam_gc::Nursery::grow`, but the two `beam_rt` growth seams serve a refused growth from the arena instead of raising `system_limit` ([`../HANDOFF.md`](../HANDOFF.md), [`../M1_GAPS_GC.md`](../M1_GAPS_GC.md) *Heap switches*).
* The `send`/inline-`!` route cannot park on a busy port (only `port_command` suspends); the busy wait is bounded by `port::busy_wait::BUSY_PORT_DEADLINE`.
* No self-hosted build: the runtime compiles every `lib/*/src/*.erl` source in process but does not compile or run itself.
* `module_info.rs` carries one divergence of its own (`exports` order); `gc_strategy_classes.rs` and `gc_immix_payload.rs` are self-comparison arms, never an oracle comparison.

## Application-level call chains (current state)
`compile_file.rs` drives the real `compiler` application in process; no test drives `syntax_tools`, `eunit`, `xref`, `dialyzer` or `edoc`.
The deepest chains are `proc_lib_start.rs` (`proc_lib:start[_link]/4` → `proc_lib:sync_start/2`), `multimodule.rs` (real stdlib loaded on demand), `io_scan.rs` (the `{get_until,…}` request driving the real `erl_scan`/`erl_parse`) and `boot.rs` (all 22 boot directives, `{apply,{c,erlangrc,[]}}` included).
A real stdlib module does not complete a compile on the scheduled `run` route (`ordsets.erl` does, an order of magnitude slower than the `-eval` route; `calendar.erl`/`lists.erl` spend the whole window inside one `Runtime::scheduler_pass`, which the between-pass `RUN_DEADLINE` never sees) — the scheduled route's limit there is its own throughput.

## Distribution (A.5) evidence
The full two-VM evidence — per-case transcripts, the four inbound families, the control-ordering rule and the wire-level assertion — is [`DISTRIBUTION_EVIDENCE.md`](DISTRIBUTION_EVIDENCE.md); the suite roster is the routing table above, the harness is [`support/CONTEXT.md`](support/CONTEXT.md), and the recipe is the `-- --ignored --test-threads=1` pair command in **Running**.

## The off-heap refc-binary reclamation measurements (`off_heap_reclaim.rs`)
The invariant: the reclamation fires at the **pre-emption** cadence (the runner's `Yield` arm, gated on `!is_relocation_exempt()`), so a payload-building loop's reading is bounded by the bin-vheap budget and is flat in `N`, while a fun-loop body's payloads live in the escaped-payload store and are pruned at its own two points.
What the suite asserts is the bounded **envelope**, never exact bytes: `dense(N)` folded to `{min, max, trough, parks}` with the peak at one pre-emption window and the trough at one payload, both unchanged across an 8× `N` step, against a tail-call-only twin that is bounded only under `BEAM_RT_TIER=interpreter`; plus the anti-dangle half (a retained 64 KiB payload byte-exact and still counted across ~6 000 prunes).
Every figure (payload bytes, park counts, `VmHWM`, the derived `BOUND_BYTES`/`SMALL_BYTES` bounds and the paired `/proc/loadavg` discipline) lives in [`../M1_GAPS_GC.md`](../M1_GAPS_GC.md) and the suite's module docs — read them there rather than from here.

## Notes for Agents
* **`beam_rt: error undef` is overloaded — four causes**: (a) `<module>.beam` on no code-path directory, (b) the export missing, (c) an **arity mismatch** in `run_export`, (d) the library body being the stub `erlang:nif_error(undef)`, which dominates — such a key **raises** (exit 1) for *every* argument, and registering it only changes exit 1 → 3.
* **Probing dispatch empirically**: `-eval` prints nothing on success (exit 0), `beam_rt: error <Reason>` (exit 1) or `beam_rt: unsupported: <Reason>` (exit 3) otherwise, so `beam_rt -eval '<M>:<F>(<args>)'` plus the exit code asks what the NIF → floor → registry → module-load order does today.
* **A capability gap is uncatchable**, so a fixture's own `try … catch` cannot rescue a remote-identifier render or an unimplemented `erlang` BIF; assert such cases through the **exit code**, not through printed output.
* **A missing committed fixture fails**; only a missing *toolchain* skips — never invert the two.
* **The `-eval` route's own process never becomes a slice**; the `run` route's does. A suite that needs the scheduler, the poller or a port round trip uses `run`.
* **A GC-counter comparison against the live oracle exists in exactly two places** (`system_info_oracle.rs` by value, `runtime_hooks.rs` by shape); do not add a third.
* **The runtime-level collector instrument lives in `../src/`**, not here: `src/runner/tests.rs::run_growth_counted` and `src/runner/tests/boundary_cost.rs::run_counted` are the reuse-worthy `GrowthCounters` harnesses (they build their runtime through `Runtime::new`, so `BEAM_RT_GC_STRATEGY` reaches them), and their own `MEASURED` rows are exact assertions that fail under a non-default mature strategy by construction.
* **Measure in retired instructions, not wall clock**, and read `/proc/loadavg` around a run — the host is shared; quote no figure without reading the owning record's measurement discipline ([`../PERF.md`](../PERF.md), [`../src/runner/MEASURED.md`](../src/runner/MEASURED.md)).
* **Fixture recipes are the fixture directory's own docs** ([`fixtures/README.md`](fixtures/README.md), [`fixtures/ERL_SOURCE_FIXTURES.md`](fixtures/ERL_SOURCE_FIXTURES.md), [`fixtures/PORT_AND_DRIVER_FIXTURES.md`](fixtures/PORT_AND_DRIVER_FIXTURES.md)); this file does not restate them.
* Active passing pins worth knowing before touching the engine's call seams: `scheduled_apply_seam.rs::a_scheduled_process_applies_a_blocking_closure_and_answers_the_oracle`, `eval_exempt_receiver_signal.rs::a_down_signal_reaches_the_eval_root_once_its_nursery_is_full`, `boot.rs::the_cli_boot_registers_a_live_logger_process`.
