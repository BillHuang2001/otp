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
- **The default `-boot` therefore resolves to the SYSTEM install's script.** `DEFAULT_BOOT_SCRIPT =
  "start"` (`cli.rs:1292`) → `resolve_boot_file` (`cli.rs:1469-1498`): candidate `<value>.boot`
  (CWD-relative), else the `<Root>/bin/<value>.boot` string-concat fallback (line 1489) →
  `<systemRoot>/bin/start.boot`. On this host that is the Nix OTP-29 install's script
  (header `{"Erlang/OTP","29"}`), NOT the repo's `bootstrap/bin/start.boot` (header
  `{"Erlang/OTP","30"}`). This is why `init:script_id/0` reports `{"Erlang/OTP","29"}` while
  `otp_release` reports the compile-time `"30"`.
- **`init:script_id/0`** (`native/init.rs:404-417`) reads `Runtime::boot_script_id`, set from the
  loaded `BootScript`'s `{Name,Vsn}` header at `runtime.rs:1698` (`boot_from_script`) and
  `runtime.rs:1739` (`boot_from_script_reporting`); accessor `runtime.rs:885-887`. With no loaded
  script it is the typed `unsupported: init:script_id/0 (no boot script has been loaded)` gap.
- **The runtime does not know where its own repo `bootstrap/` is at runtime.** An explicit
  `-boot bootstrap/bin/start` resolves **CWD-relative** (`cli.rs:1480`), so it only finds the
  committed script when the process CWD is the repo root. Only tests hardcode the repo path
  (`boot/bootscript.rs:736`, via `env!("CARGO_MANIFEST_DIR")`).
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
  state (`runtime/CONTEXT.md`).
- `runner/` → the `beam_process::ProcessRunner` (`runner/CONTEXT.md`).
- `dist.rs` + `dist/` + `dist_client.rs` → the distributed node, accepting and connecting halves
  (`dist/CONTEXT.md`).
- `eval/` → the `-eval` evaluator (AST/parser/`Flow`) (`eval/CONTEXT.md`).
- `codeserver.rs`, `error.rs`, `config.rs`, `format.rs`, `mailbox.rs`, `timers.rs`, `nif.rs`, the
  observability modules and `main.rs` → documented in `../CONTEXT.md` (the crate routing table).

## See Also
- `../CONTEXT.md` → the crate-level API surface, CLI table, exit codes, `system_info` keys and gaps.
- `../M1_GAPS.md` · `../DESIGN_DECISIONS.md` · `../TRACE_AND_PERF.md` · `../TEST_STRATEGY.md` ·
  `../PERF.md` → companion docs.
