# `beam_rt/tests/` — runtime integration suites (the executable + the `Runtime` façade)

## Intent
Black-box and glue-level end-to-end tests for the `beam_rt` runtime binary and its `Runtime`/`RuntimeContext` façade.
Two drivers are used throughout, and they are NOT interchangeable:
* `Runtime::eval` / the CLI `-eval` action (`beam_rt -noshell -eval '<expr>'`) — the **single-pass** evaluator route; no scheduler slice.
* the CLI `run <file.beam> <mod>:<fun>/<arity> [args]` action (`Runtime::run_scheduled`) — a **scheduled process** on the real scheduler/poller/runner; also `runtime.run_function`/`run_module` (single-pass, `context.run_export`, caller-owned output sink).

Most suites run **both** runtimes (this one and the system `erl`/`erlc` oracle) and compare observable output; the oracle is the expected value. A missing toolchain is always a **loud skip**, never a silent pass (`support/mod.rs`).

## Running
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt [--test <suite>] [--no-fail-fast]`
(the sandbox `$HOME` is read-only — always go through the wrapper; the workspace manifest is `erts_rust/Cargo.toml`, never the repo root).
Suites that need the oracle skip **loudly** when `erl`/`erlc` are absent. `tests/fixtures/` must **never** be passed to the oracle as a single `-pa` (it holds a shadowing `lists.beam` — see below).

## Routing Table
* Core execution / vertical slice → `slice.rs` (library slice: load a real `.beam` and run it), `cli.rs` (spawn the binary, `run` the slice, usage/exit codes), `mailbox.rs`, `scheduler.rs` (several processes, one blocks in `receive`), `preempt.rs` (reduction budget), `multimodule.rs` (on-demand cross-module load), `proc_lib_start.rs` (`proc_lib:start/3,4` across a module boundary), `fun_entry_receive.rs` (fun-bodied process suspends), `scheduled_apply_seam.rs` (the `call_fun`/`call_fun2` closure-apply path: a scheduled process enters a blocking closure in-machine and answers the oracle)
* A.6 CLI/config → `cli.rs`, `cli_flags.rs` (`ERL_*FLAGS`), `cli_env.rs` (`ERL_*` env vars), `perf_bif.rs` (the perf-harness CLI contract)
* A.7 boot / preloaded / native floor → `boot.rs`, `native_init.rs`, `a9_native_floor.rs`, `code_load_file.rs`, `module_loaded.rs`, `module_info.rs`, `records.rs`, `runtime_hooks.rs`, `dict.rs`, `persistent_term.rs`
* A.8 introspection / tracing → `system_info_oracle.rs`, `process_info_items.rs`, `position.rs` (scheduler-boundary execution position), `trace.rs`, `trace_call.rs`, `trace_pattern.rs`, `trace_send_event.rs`, `perf_map.rs` (`+JPperf`), `crash_dump.rs`
* A.4 ports / drivers → `driver.rs` (real C `.so`), `port_parity.rs` (`open_port`/`port_info`/`port_command`), `port_stream.rs`, `port_wake.rs`, `port_monitor_cleanup.rs`
* A.5 distribution → `dist.rs`, `dist_delivery.rs`, `node_cookie.rs`, `pid_roundtrip.rs`
* A.3 NIF / off-heap → `nif.rs` (real C NIF), `etf_roundtrip_pid_ref.rs`, `engine_off_heap_transport.rs`
* Processes / spawn / monitor / signals → `spawn_monitor.rs`, `spawn_from_slice.rs`, `spawn_opts.rs`, `monitor.rs`, `alias.rs` (reference aliases as `send` destinations: `alias/1`, `unalias/1`, `monitor/3`'s `{alias, Mode}`, the compiled `Alias ! Msg` and the `erlang:send/2` route, `gen:reply/2` and real `gen_server` calls), `exit.rs`, `group_leader.rs`, `timers.rs`, `proc_seams.rs`
* GC / heap → `gc_collect.rs`, `gc_run.rs`, `heap_growth.rs`, `heap_limits.rs`, `garbage_collect.rs`, `gc_strategy.rs` (the runtime's own spawn `HeapPolicy` selects the mature-generation strategy through the automatic rule), `gc_immix_payload.rs` (the measured `Copying`-vs-`Immix` runtime workload + live-payload survival), `eval_exempt_receiver_signal.rs` (a `'DOWN'`/`'EXIT'` control signal aimed at the `-eval` route's relocation-exempt root, with a roomy and with a saturated nursery)
* ETS → `ets_owner.rs`
* Errors / terms → `structured_errors.rs`, `function_clause.rs`, `send.rs`, `eval.rs` (the `-eval` evaluator + the differential corpus's `Eval` probes), `eval_order.rs` (the `-eval` evaluator's sub-expression evaluation order, oracle-cross-checked)
* A.9 build/bootstrapping → `compile_file.rs` (`compile:forms/2` and `compile:file/2` driven in-process, with the artifact differentially compared to the oracle's)
* File I/O / io → `file_server.rs`, `io_scan.rs` (the library io-device read path, incl. the buffered-read sequences — `file:read/2` interleaved with `file:read_line/1`/`file:position/2`, asserted three-way equal to a pinned value on the scheduled `run` route, the `-eval` route and the oracle), `scheduled_io_device.rs` (scheduled io-device round trips: `io:getopts/1`, `io:setopts/2`, `io:scan_erl_form/4`), `empty_binary_result.rs`
* Harness + fixtures → `support/mod.rs` (bounded child runner, oracle discovery, `skip_loudly`), `fixtures/` (committed `.beam`/`.erl`/`.c` + `fixtures/README.md` — what each fixture is and how each is (re)built)

## File I/O coverage (current state)
The **read** path is exercised on both routes; the **write** path is exercised in-process:
* `file_server.rs` (`Runtime::eval`, single-pass `-eval` route): `file:read_file/1` returns the bytes (`reading_an_existing_file_returns_its_bytes`), `file:open(P,[read])` on a missing path ⇒ `{error,enoent}` / on a directory ⇒ `{error,eisdir}`, `file:list_dir/1`, `filelib:is_dir|is_file|is_regular/wildcard` (absent path ⇒ `false`/`[]` agree with the oracle; an **existing** path ⇒ `true` via `beam_io::fs_access`).
* `io_scan.rs`: the `-eval`-route reads (`file:read/2`, `io:scan_erl_exprs/4`, `io:parse_erl_exprs/4`, the `dflag_unicode_io/1` probe) **and** the **buffered-read sequences** — a test-time-compiled `ioseq` fixture whose `file:read/2` is interleaved with `file:read_line/1` and `file:position/2` (bare `cur` and `{cur, N}`), plus an `eof` control. Each sequence line is asserted three-way equal to a pinned value on the **scheduled** `run` route, the **pumped** `-eval` route and the live oracle, so a divergence confined to one route is a mismatch.
* `scheduled_io_device.rs`: a **scheduled** process completes io-device round trips (`io:getopts/1`, `io:setopts/2`, `io:scan_erl_form/4`), each line oracle-cross-checked.
* The **write path** runs in-process through `compile_file.rs`: `compile:file/2` writes its artifact via the native `file_server_2` (write-mode `file:open/2` + the `{put_chars,Enc,Chars}` io-device request reaching `std::fs::File::write_all`). A **direct** write-mode open / `file:write/2` probe is not driven separately.

## Application-level call chains (current state)
`compile_file.rs` drives the real `compiler` application in-process (`compile:forms/2` and `compile:file/2`); no test drives `syntax_tools`, `eunit`, `xref`, `dialyzer` or `edoc`. The other deepest chains exercised are:
* `proc_lib_start.rs` — `run` action, `proc_lib:start[_link]/4` → `proc_lib:sync_start/2` (a different kernel module), oracle `{ok,pid}`.
* `multimodule.rs` — single-pass `run_function`/`run_module`: a compiled fixture calling `lists:reverse/1`, `maps:get/2`, `ordsets:from_list|to_list/1` (a real stdlib module loaded on demand) and `binary:part/3`.
* `io_scan.rs` — the `{get_until,…}` io-device request drives the real `erl_scan:tokens/4` (and `erl_parse` via `io:parse_erl_exprs/4`) on the engine.
* `boot.rs` — the real `bootstrap/bin/start.boot` chain, whose instruction 20 `{apply,{c,erlangrc,[]}}` loads `c.beam` and reaches `file:open($HOME/.erlang,[read])` through the native `file_server_2`.

## Notes for Agents
* `scheduled_apply_seam.rs::a_scheduled_process_applies_a_blocking_closure_and_answers_the_oracle`, `boot.rs::the_cli_boot_registers_a_live_logger_process` and `eval_exempt_receiver_signal.rs::a_down_signal_reaches_the_eval_root_once_its_nursery_is_full` are **active** positive pins (the engine's `call_fun`/`call_ext erlang:apply/3` seams are closed, and `beam_process` carries a control signal to a relocation-exempt receiver through a heap fragment).
* Interfaces that all suites share: `env!("CARGO_BIN_EXE_beam_rt")` (the built binary) and `support::{run_bounded, oracle_eval, require_oracle, skip_loudly, ...}`.
