# `beam_rt/src` — runtime source root

## Intent
The crate's implementation root: the `main.rs` shim, the `Runtime` façade (`runtime.rs`, `runtime/`),
the CLI (`cli.rs`, `cli/`), the concrete `ExecutionContext` glue (`context/`), the runtime-native BIF
floor (`native.rs`, `native/`), the A.4 port layer (`port.rs`, `port/`), the A.8 introspection surface
(`info.rs`, `info/`), the distribution node (`dist.rs`, `dist/`, `dist_client.rs`), and the
observability modules (`trace.rs`, `seq_trace.rs`, `crash_dump.rs`, `perf_map.rs`, `debug_trace.rs`).
The crate-level API surface (CLI table, exit codes, `system_info` keys, boot status, gaps) lives in
`../CONTEXT.md`; the per-module detail lives in the child `CONTEXT.md`s below.

## Version / boot-script / system-root resolution (cross-cutting seam)
Binding facts for anyone touching A.6/A.7/A.9 (CLI, boot, introspection); verified against the code
and this host. These pieces are spread across `codeserver.rs`, `cli.rs`, `boot*.rs`, `info/system.rs`
and `native/{code,init}.rs`, so they are recorded here rather than in any one child node.

- **The version constants are compile-time.** `beam_common::{OTP_VERSION = "30.0-rc0",
  ERTS_VERSION = "17.1"}` (`../beam_common/src/version.rs:39,42`). `erlang:system_info(otp_release /
  version / system_version / compat_rel)`, the NIF `ErlDrvSysInfo` strings and the `-version` banner
  are all served from them — `info/system.rs:116,117,126-130,182`, `boot.rs:317-319` (banner, printed
  by `cli.rs:1217`), `nif.rs:476,480`.
- **The runtime has NO notion of its own root/release.** The code path/root is resolved **only from
  the filesystem**, preferring the `erl` on `$PATH` (`codeserver.rs::native_roots:660-683` — seed
  order: `erl`-on-`$PATH` ancestors, then `current_exe` ancestors, then `$ERL_TOP`, then
  `STANDARD_OTP_ROOTS`; each tried as itself and as `<seed>/lib/erlang`). The first root passing the
  `SYSTEM_APPS` probe (`stdlib`/`lists.beam` is the hard requirement) wins
  (`native_system_ebins:644-646`, `system_ebins_under:693-713`, `SYSTEM_APPS:825`). `bootstrap/` is
  never a candidate root. So with an `erl` on `$PATH`, the consumed
  root is ALWAYS that system install's root (`CodeServer::lib_root` at `codeserver.rs:335-340`), and
  that root then yields the whole release's ebin list through `codeserver/path.rs`
  (`release_ebins`/`user_lib_ebins`/`system_path`, the `code_server.erl` port), which reproduces the
  oracle's `code:get_path/0` exactly (`../../tests/code_path.rs`).
- **The default route loads THIS runtime's own release's boot script.** With no `-boot`,
  `cli.rs`'s `boot_script_to_drive` prefers `CodeServer::own_bootstrap_boot_script()` — the committed
  `bootstrap/bin/start.boot` of the source tree the binary runs from — so the default route drives the
  OTP-30 primary bootstrap (header `{"Erlang/OTP","30"}`) and `init:script_id/0` agrees with
  `erlang:system_info(otp_release)` (`"30"`). Only when no source tree is reachable (an installed
  deployment) does it fall back to `DEFAULT_BOOT_SCRIPT = "start"` → `resolve_boot_file` (candidate
  `<value>.boot`, else the `<Root>/bin/<value>.boot` string-concat fallback) → `<systemRoot>/bin/start.boot`.
  An **explicit** `-boot FILE` is untouched and still resolves exactly as `init`'s `get_boot/2` does.
- **`init:script_id/0`** (`native/init.rs`) reads `Runtime::boot_script_id`, set from the
  loaded `BootScript`'s `{Name,Vsn}` header at `runtime.rs` (`boot_from_script`) and
  `runtime.rs` (`boot_from_script_reporting`). With no loaded
  script it is the typed `unsupported: init:script_id/0 (no boot script has been loaded)` gap.
- **`CodeServer::lib_root()` stays the consumed SYSTEM install's `<lib>`** (it feeds
  `bindir`/`app_dir`/system-ebin resolution/`$ROOT` expansion), so it is never repointed at the source
  tree: `own_bootstrap_boot_script` is a **separate** discovery used only to pick the default script.
- **The runtime DOES know where its own source tree is at runtime**, without a build-time absolute path:
  `CodeServer::own_bootstrap_boot_script` walks `current_exe()`'s ancestors (bounded, exactly as
  `native_roots` does) and then `$ERL_TOP`, checking each for `bootstrap/bin/start.boot`. Tests still use
  an explicit repo path (`env!("CARGO_MANIFEST_DIR")`), which a relocated binary could not report
  truthfully. An explicit `-boot bootstrap/bin/start` stays **CWD-relative** (as on the oracle), so it
  only finds the committed script when the process CWD is the repo root.
- **Consumers of the resolved root** all yield the system install's paths (never this runtime's own
  tree): `codeserver.rs::app_dir:349-371` (`code:lib_dir/1,2`), `codeserver.rs::bindir:385-400`
  (`init:get_argument(bindir)` = `<root>/erts-<vsn>/bin`), `native/code.rs::code_lib_dir_0:426-434`
  (`code:lib_dir/0`), `native/code.rs::code_root_dir:650-661` (`code:root_dir/0`),
  `native/init.rs:74,108,215` (`init:get_argument(root|bindir)`, `init:get_arguments/0`),
  `cli.rs::boot_file_root:1443-1446`, `boot/bootscript.rs::expand_root:418-435` (the script's own
  `$ROOT` expansion).
- **Stale doc comment to be aware of:** `native/init.rs:397-401` claims the boot script this runtime
  loads is the committed `bootstrap/bin/start.boot` with header `{"Erlang/OTP","30"}`. That is not
  what happens on a host with an `erl` on `$PATH` — the resolved default is the system install's
  script (see above).

## The `enif_select` handoff (A.3 — two seams owned by other nodes)
`NifHostImpl::select` answers `-1`, `beam_abi`'s documented "not serviceable" refusal, so `enif_select` on a real fd takes the C's own `retval < 0` path instead of fabricating a registration.
`../tests/nif_select.rs` is the `#[ignore]`d reproducer: a real `pipe(2)`, `ERL_NIF_SELECT_READ` on the read end, a write from Erlang — rust measures `code=-1 written=1 outcome=timeout`, the oracle (OTP 29.1.1) `code=0 written=1 outcome={delivered,ready_input}`.
The C's shape: `prepare_select_msg` (`erts/emulator/sys/common/erl_check_io.c:699-800`) queues `{select, ResourceTerm, Ref, EventAtom}` with sender `am_system`, `EventAtom` ∈ `ready_input`/`ready_output`/`ready_error` (`:1451-1463`), clearing the interest as it is queued (one-shot, `:2135-2151`).
Two seams own the faithful version; the observable stays the typed `-1` until both land.
**`src/port.rs` + `src/port/tty.rs` (the port layer's owner).** The generic select is ~90 % present as `TtyWatch`, hardcoded to fd 0 / `Interest::READABLE` / the `ready_input` atom:
- `TtyRegistration` already carries `token`/`fd`/`owner`/`handle`/`reference` — add `event: Eterm` and make `arm` (`tty.rs:164`) take `fd: RawFd`, `interest: Interest` and `event: Eterm` instead of `TTY_IFD` (`tty.rs:85`, used at `:177`/`:183`); `take_ready` (`tty.rs:201`) returns `Vec<(ProcessId, Eterm, Eterm)>` and must carry the event atom as a fourth element.
- Add `stop(&mut self, poller: &mut Poller, fd: RawFd, handle: Eterm) -> Result<bool>` plus an `(fd, handle)`→key index: there is no fd→key route today, so `ERL_NIF_SELECT_STOP` has none.
- `PortRegistry::arm_tty_select` (`port.rs:1211`) becomes `arm_nif_select(&mut self, owner: ProcessId, fd: RawFd, interest: Interest, event: Eterm, handle: Eterm, reference: Eterm) -> Result<()>`, keeping `arm_tty_select` as a thin wrapper so `native/prim_tty.rs:341` is untouched.
- `PortEvent::Select` (`port.rs:1856`/`:3559`, its `port()` arm at `:903`) gains the `event` field, and `RuntimeContext::tty_select_message` (`port.rs:3996`) becomes `select_message(&mut self, handle: Eterm, reference: Eterm, event: Eterm)`.
- `port.rs:3707` (`PortEvent::Select { .. } => continue`) drops **every** select event unconditionally, even when the owner is the evaluator, so a readiness is consumed and lost — it must deliver.
- Key reservation needs no change: `TTY_KEY_TOP = u64::MAX - 2` … `TTY_KEY_FLOOR = 1 << 60` already sits below `watch::SIGNAL_KEY`/`WAKER_KEY` and above the port-number space; `beam_io::Interest` has **no** error bit (`crates/beam_io/src/poller.rs:122`), so `ERL_NIF_SELECT_ERROR` needs `beam_io` too — until then an errored select-armed fd reports as readiness.
**`beam_abi` (the ABI layer's owner).** `NifHost::select` (`crates/beam_abi/src/nif/host.rs:359`) hands the host a raw `*mut c_void`, and no *safe* route turns it into the C's resource term: `resources.rs` exposes only `resource_type_has_down` (`:818`) safely, its `terms: HashMap<usize, ResourceRef>` is forward-only (`bind_term` `:350`, `lookup_term` `:373`), and `objects` maps obj→*type*, not obj→term. Needed: `pub fn resource_term(host: &mut dyn NifHost, obj: *mut c_void) -> Option<Eterm>` beside `mint_resource_ref` (`resources.rs:857`) — body = `object_type(obj)?`, `mint_resource_ref`'s four-word ref write against `host.heap()`, `bind_term(reference.raw(), ResourceRef { ty, obj })`, `retain_object(obj)`; it never dereferences `obj`.
Once both land, the `beam_rt` half is one `NifHost::select` arm calling `arm_nif_select` through `NifHostServices` (the pattern `port_command` already uses), and `../tests/nif_select.rs` turns green unaided.

## File-size overages
The `../CONTEXT.md` *Files ~1000 lines* constraint is not met by the pre-existing oversize below, which are **not** restructured (line counts measured with `wc -l`; a "production" count excludes the file's own test region — everything from its first test-module declaration to EOF, whether that declaration opens an inline module or a sibling `#[path]` one).
- `src/port.rs` 4 076 lines, 4 020 of production (unit suite lives in `port/tests.rs`, 3 824 lines, plus the sibling `port/{driver_output_shape,inet_matrix,ram_file_matrix,completion,control_capability,monitor,spawn_driver,packet_framing,stream_shape}_tests.rs` and `port/test_support.rs`).
- `src/runtime.rs` 4 032 lines — 3 168 of production, the rest the colocated unit test module (from line 3 169).
- `src/port/driver.rs` 3 313 lines, 2 920 of production: the driver C-ABI host (`DriverPort`/`DriverPortAdapter` — the concrete `beam_abi::DriverHost` behind a driver's `ErlDrvPort`), with its suites as the sibling `port/driver/{fire,port_term,control,ddll,stop_select,ready_output,liveness,monitor,hooks}_tests.rs` modules declared from its tail.
- `src/context/gc.rs` 3 210 lines, 1 802 of production — the remainder is the test-only counters modules (`valve_counters`/`prefix_counters`) and the inline suites; its unit suite **belongs** in `context/gc/tests.rs` (still owed; the `port.rs`/`port/tests.rs` pattern).
- `src/native/code.rs` 2 902 lines, 1 033 of production: the `code:*` floor (the path/load/name readers and the path mutators) with its oracle-cross-checked colocated suite in the remaining 1 869 lines.
- `src/codeserver.rs` 1 879 lines, 1 329 of production: the shared `Rc<AtomTable>`, the module cache with both location maps, the code path, the file-load routes and the store's one removal op, plus its colocated unit suite (550 lines).
- `src/context/gc_walk.rs` 1 853 lines: 857 of production (the valved reachability descent, whose module docs carry the soundness argument and the latch-clearing refutation), the test-only `seed_counters` module and the root-set pinning suite, which **must** stay colocated because it drives the module's private `Walk`/`WalkScratch` types.
- `src/context/runtime_hooks.rs` 1 823 lines of production with no inline tests — the `RuntimeHooks` bridge, whose per-arm rustdoc blocks carry the oracle columns and the served/fixed/gap reasoning (suites `context/tests.rs` and `tests/runtime_hooks.rs`).
- `src/context/calls.rs` 1 604 lines, 1 558 of production (the BIF-dispatch + BIF-result-copy-out machinery; the tail is the test-only dispatch notes plus the suite in `context/calls/tests.rs`, 866 lines).
- `src/runtime/scheduler.rs` 1 518 lines, 1 396 of production (the pass family), the 122-line tail being two inline pins for the code they measure plus the declaration of the sibling suite (`runtime/scheduler/tests.rs`, 1 225 lines).
- `src/error.rs` 1 311 lines, 755 of production: the `pub(crate)` `BeamError` constructors, the halt helpers, `enforce_term_heap_limit`, `unsupported_reason`/`live_reason_term` and the `KNOWN_UNIMPLEMENTED_ERLANG_BIFS` set with its drift guard, plus its colocated suite (556 lines).
- `src/native/file_server.rs` 1 207 lines of production of 1 209 (the io-device request dispatch table; suite in `native/file_server/tests.rs`, 1 970 lines).
- `src/port/failure_e2e_tests.rs` 848 lines — a **test-only** suite (declared from `port/failure.rs`'s tail), so it is legitimately long: it compiles its own real C `.so` and drives one row per oracle opcode through the whole `port_command → output → FailureLatch → settle → signal_failure → drain_failures → push_ready` road, asserting each published `PortSignal` list whole.
- `src/port/driver/port_term_tests.rs` 761 lines — a **test-only** suite (declared from `port/driver.rs`'s tail) for the `ERL_DRV_PORT` hook: the stamped handle↔`PortId` round trip through the real term-spec decoder, the foreign/null/unstamped refusals, the gone bit after `clear_driver_state`, and an end-to-end row through a real C driver that emits `{ERL_DRV_PORT, driver_mk_port(port)}`.
- `src/port/failure.rs` 200 lines, 193 of production — **within target**; it is the driver-failure channel's runtime half (`signal_reason`, `PortRegistry::drain_failures`/`fail_port`, the ready-queue publication), declared inside `exit.rs`, and its two suites are `port/failure_tests.rs` (159, the reason-mapping units) and `port/failure_e2e_tests.rs` (above).
- **Other files past the target in production lines** (one responsibility each, no split prescribed; same measurement): `eval/mod.rs` 3 488 / 2 597, `cli.rs` 2 968 / 1 791, `native/prim_file.rs` 1 601 / 1 601, `runner/run.rs` 1 450 / 1 450, `native/erts_internal.rs` 2 839 / 1 273, `native/memory/mod.rs` 1 273 / 1 269, `eval/parse.rs` 1 593 / 1 235, `native/process.rs` 1 841 / 1 196, `native/prim.rs` 4 066 / 1 139, `trace.rs` 1 850 / 1 074, `context/bridges.rs` 1 167 / 1 022, `nif.rs` 2 385 / 1 160.

## Routing Table
- `boot.rs` + `boot/` → preloaded-module set (`PreloadedModules`/`CORE_ATOMS`), the boot banner, the
  boot-script reader/executor and the system code-path resolution (`boot/CONTEXT.md`).
- `cli.rs` + `cli/` → the A.6 CLI parser/driver, the four `EXIT_*` codes and the boot-script drive
  (`cli/CONTEXT.md`).
- `context/` → the concrete `ExecutionContext` glue, the host-seam bridges and the NIF-dispatch order
  (`context/CONTEXT.md`).
- `native.rs` + `native/` → the runtime-native BIF floor (`try_native`, before the registry), the
  native `file_server_2` service and the code/init/application/memory floors (`native/CONTEXT.md`).
- `info.rs` + `info/` → the A.8 `system_info`/`process_info` introspection surface, incl. this file's
  version producers (`info/CONTEXT.md`).
- `port.rs` + `port/` → the A.4 `PortHook` provider, the port registry/poller and the driver C-ABI
  host (`port/CONTEXT.md`).
- `runtime.rs` + `runtime/` → the `Runtime` façade, the scheduler-pass family and the GC/ETS-owner
  state (`runtime/CONTEXT.md`); its pid-keyed bookkeeping (`group_leaders`/`async_dist`/`monitor_nodes`/
  `timer_resumes`) and the `runtime/timer_wakes.rs` `TimerWakes` mirror are `crate::hasher` maps.
- `runner/` → the `beam_process::ProcessRunner` (`runner/CONTEXT.md`); `RunnerState`'s four pid-keyed
  maps (`engines`/`results`/`exits`/`errors`) are `crate::hasher::FastHashMap`.
- `dist.rs` + `dist/` + `dist_client.rs` → the distributed node, accepting and connecting halves
  (`dist/CONTEXT.md`).
- `eval/` → the `-eval` evaluator (AST/parser/`Flow`) (`eval/CONTEXT.md`).
- `codeserver.rs`, `error.rs`, `config.rs`, `format.rs`, `mailbox.rs`, `timers.rs` (a min-heap of
  absolute deadlines over `crate::hasher::FastHashMap` indexes, whose `prune_cancelled` reclaims
  non-head cancelled tombstones with a watermark heap rebuild), `nif.rs`, `hasher.rs`
  (the crate's one deterministic non-cryptographic `BuildHasher` — `FastHasher`/`FastBuildHasher`/
  `FastHashSet`/`FastHashMap` — used by the per-pid verdict-only structures, never a keyed-by-`Arc<str>`
  or security-sensitive map), the observability modules and `main.rs` → documented in `../CONTEXT.md` (the crate routing table).

## See Also
- `../CONTEXT.md` → the crate-level API surface, CLI table, exit codes, `system_info` keys and gaps.
- `../M1_GAPS.md` · `../DESIGN_DECISIONS.md` · `../TRACE_AND_PERF.md` · `../TEST_STRATEGY.md` ·
  `../PERF.md` → companion docs.
