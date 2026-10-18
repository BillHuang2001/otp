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
  never a candidate root nor on the resolved code path. So with an `erl` on `$PATH`, the consumed
  root is ALWAYS that system install's root (`CodeServer::lib_root` at `codeserver.rs:335-340`).
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

## File-size overages
The `../CONTEXT.md` *Files ~1000 lines* constraint is not met by the pre-existing oversize below, which are **not** restructured (line counts measured with `wc -l`; a "production" count excludes the file's own inline `#[cfg(test)]` modules).
- `src/port.rs` 3 948 production lines of 4 004 (unit suite lives in `port/tests.rs`, 3 824 lines, plus the sibling `port/{ram_file_matrix,monitor,spawn_driver,control_capability}_tests.rs` and `port/test_support.rs`).
- `src/runtime.rs` 3 980 lines — 3 116 of production, the rest the colocated unit test module (from line 3 117).
- `src/context/calls.rs` 1 551 lines of production of 1 597 (the BIF-dispatch + BIF-result-copy-out machinery; the tail is the test-only dispatch notes plus the suite in `context/calls/tests.rs`, 866 lines).
- `src/native/file_server.rs` 1 207 lines of production of 1 209 (the io-device request dispatch table; suite in `native/file_server/tests.rs`, 1 970 lines).
- `src/runtime/scheduler.rs` 1 396 lines of production of 1 518, plus a small inline borrow/quiescence test module (suite in `runtime/scheduler/tests.rs`, 1 225 lines).
- `src/context/gc.rs` 3 211 lines, 1 802 of production — the remainder is the test-only counters modules (`valve_counters`/`prefix_counters`) and the inline suites; its unit suite **belongs** in `context/gc/tests.rs` (still owed; the `port.rs`/`port/tests.rs` pattern).
- `src/context/gc_walk.rs` 1 854 lines: 857 of production (the valved reachability descent, whose module docs carry the soundness argument and the latch-clearing refutation), the test-only `seed_counters` module and the root-set pinning suite, which **must** stay colocated because it drives the module's private `Walk`/`WalkScratch` types.
- `src/context/runtime_hooks.rs` 1 823 lines of production with no inline tests — the `RuntimeHooks` bridge, whose per-arm rustdoc blocks carry the oracle columns and the served/fixed/gap reasoning (suites `context/tests.rs` and `tests/runtime_hooks.rs`).
- `src/codeserver.rs` 1 879 lines, 1 329 of production: the shared `Rc<AtomTable>`, the module cache with both location maps, the code path, the file-load routes and the store's one removal op, plus its colocated unit suite (550 lines).
- `src/native/code.rs` 2 781 lines, 981 of production: the `code:*` floor (the path/load/name readers and the path mutators) with its oracle-cross-checked colocated suite in the remaining 1 800 lines.

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
