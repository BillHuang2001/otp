# sasl — Source Modules (`lib/sasl/src`)

## Intent
Erlang sources of the SASL ("System Architecture Support Libraries") application: release handling (build-time `.script`/`.boot`/`tar`/`relup` generation and runtime unpack/install/upgrade of releases), the alarm handler, the legacy `error_logger`-era SASL report/logging support (incl. the RB report browser), and the Windows service helper.
The whole application is only 17 modules in this one flat directory — there are no meaningful subdirectories here (all of `src/` is this node).

## API Surface
Only four modules carry a `-moduledoc` string and are therefore the public, documented API rendered by ExDoc: `alarm_handler`, `rb`, `release_handler`, `systools`.
Every other module is `-moduledoc false.` (internal) — including all `systools_*`, `release_handler_1`, `sasl`, `sasl_report*`, `format_lib_supp`, `misc_supp`, `rb_format_supp`, `erlsrv`.
Exported entry points by area (public API first):

**High-level release tools — `systools.erl` (public facade, delegates to the internal modules)**
- `make_script/1,2` → `systools_make:make_script/1,2` (`Name.rel` → `Name.script` + `Name.boot`).
- `make_tar/1,2` → `systools_make:make_tar/1,2` (`Name.rel` → compressed release package `Name.tar.gz`).
- `make_relup/3,4` → `systools_relup:mk_relup/3,4` (generates the `relup` file).
- `script2boot/1,3`, `compile_rel/3` → `systools_lib:file_term2binary/2` (`.script` → binary `.boot`).
- Error formatting for callers: `systools_make:format_error/1`, `format_warning/1`; `systools_relup:format_error/1`, `format_warning/1`.

**Script/boot/tar generation — `systools_make.erl` (biggest module after `release_handler`)**
- `make_script/1,2,3`, `make_tar/1,2`, `format_error/1`, `format_warning/1`.
- Reused internally by other apps: `read_release/2`, `get_release/2,3`, `pack_app/1`, `read_application/4`, `make_hybrid_boot/4` (hybrid old+new boot used for emulator upgrade), `preloaded/0` (exported only for testing).
- Optionally runs `xref` in the `tools` app for the `exref` flag; `-define(XREF_SERVER, systools_make)`.

**`relup` generation — `systools_relup.erl`**
- `mk_relup/3,4` (top `.rel` + base up/down `.rel` lists → `relup`), `format_error/1`, `format_warning/1`.
- `appup_search_for_version/2` — exported for `release_handler:find_script/4` and for kernel/stdlib/sasl tests.

**Instruction translation — `systools_rc.erl`**
- `translate_scripts/3,4`, `format_error/1` — translates high-level `appup` instructions (`{update,Mod,Change,...}`, `{load_module,...}`, `{restart_application,...}`, …) into the low-level `script` instructions (`point_of_no_return`, `{load,...}`, `{suspend,...}`, `restart_new_emulator`, …); `-define(DEP_INSTRS, [update, load_module, add_module, delete_module])` drives dependency ordering.
- Called at build time by `systools_relup` and at runtime by `release_handler` (`upgrade_app`/`downgrade_app` paths).

**Release handler server — `release_handler.erl` (largest module)**
- `start_link/0` (registered `gen_server` process `release_handler`).
- `create_RELEASES/1,2,3,4`, `unpack_release/1`, `check_install_release/1,2`, `install_release/1,2`, `new_emulator_upgrade/2`, `remove_release/1`, `which_releases/0,1`, `make_permanent/1`, `reboot_old_release/1`, `set_unpacked/2`, `set_removed/1`, `install_file/2`.
- Test/introspection helpers (explicitly documented as not safe to combine with `install_release/1,2`): `upgrade_app/2`, `downgrade_app/2,3`, `upgrade_script/2`, `downgrade_script/3`, `eval_appup_script/4`.
- Client-callable file-op exports used by a "client" `release_handler` and by `rpc` from master nodes: `do_write_release/3`, `do_copy_file/2`, `do_copy_files/1,2`, `do_rename_files/1`, `do_remove_files/1`, `remove_file/1`, `do_write_file/2,3`, `do_ensure_RELEASES/1`, `consult/2`, and the `root_dir_relative_{read_file_info,read_file,rename_file,make_dir,ensure_dir}` helpers.
- Runtime evaluation is delegated: `release_handler_1:check_script/2`, `release_handler_1:eval_script/5`, `release_handler_1:check_old_processes/2`; `systools_rc:translate_scripts/2`; `systools_relup:appup_search_for_version/2`; `systools_make:read_application/4`, `make_hybrid_boot/4`.

**Low-level script evaluation — `release_handler_1.erl`**
- `eval_script/1`, `eval_script/5`, `check_script/2`, `check_old_processes/2`; `get_current_vsn/1`, `get_supervised_procs/0` exported only because test cases use them.
- Holds `#eval_state{}` tracking bins, stopped/suspended module pids, libdirs, unpurged modules and old→new vsns (for downgrade `code_change`).

**Low-level file/term IO — `systools_lib.erl`**
- `file_term2binary/2`, `read_term/1`, `read_term_from_stream/2`, `get_dirs/1`, `get_path/1`, `werror/2` (turns warnings into errors for `-Werror`-style option handling).

**Application + supervisor — `sasl.erl`**
- `start/2`, `stop/1` (application behaviour), `init/1` (supervisor behaviour).
- The module doubles as the supervisor: `sasl_sup` (one_for_one) has fixed children `sasl_safe_sup` and `release_handler`; `init(safe)` starts `sasl_safe_sup` which supervises `alarm_handler` so that the node reboots if `release_handler` dies.
- At start it installs the SASL logger only when `kernel`'s `logger_sasl_compatible` is true: a `logger_std_h` handler named `sasl` (level from `sasl` env `errlog_type`, domain filter for `[otp,sasl]`, `legacy_header` formatter), and optionally the STDLIB `log_mf_h` error-logger handler driven by `error_logger_mf_dir`/`maxbytes`/`maxfiles` (used by `rb`).

**Alarm handling — `alarm_handler.erl`**
- `set_alarm/1`, `clear_alarm/1`, `get_alarms/0` (all public/documented) and `start_link/0`, `add_alarm_handler/1,2`, `delete_alarm_handler/1` (exported but `-doc false`).
- A `gen_event` manager registered as `alarm_handler`, with a default simple handler that logs each alarm via `error_logger:info_report/1` and keeps the alarm list; custom handlers swap in via `gen_event:swap_handler/3` and receive `{set_alarm, {Id,Descr}}` / `{clear_alarm, Id}`.

**Report browser — `rb.erl` (+ `rb_format_supp.erl`)**
- `start/0,1`, `stop/0`, `rescan/0,1`, `list/0,1`, `log_list/0,1`, `show/0,1`, `grep/1`, `filter/1,2`, `start_log/1`, `stop_log/0`, `h/0`, `help/0`.
- `start/1` registers a dynamic child `rb_server` under `sasl_sup` via `supervisor:start_child/2`; `rb` is a `gen_server` reading the STDLIB `log_mf_h` error-logger files; `rb_format_supp:print/3` does the report pretty-printing.

**Legacy `error_logger` SASL report support**
- `sasl_report.erl`: shared formatter, `write_report/3`, `format_report/3`.
- `sasl_report_tty_h.erl` / `sasl_report_file_h.erl`: `error_logger` event handlers writing SASL reports to stdout / to a file.
- `format_lib_supp.erl` (`print_info/2,3`) and `misc_supp.erl` (`format_pdict/3`, `format_tuples/2`, `assq/2`, `passq/2`, `is_string/1`, `multi_map/2`, `modifier/1`) — generic "SUPPORT tools" formatting helpers; `misc_supp:format_pdict/3` is called from `init`, `memsup`, `disksup` and `init`-style `format_status_info` callbacks in other apps.

**Windows service control — `erlsrv.erl`**
- `get_all_services/0`, `get_service/1,2`, `store_service/1,2`, `new_service/3,4`, `disable_service/1,2`, `enable_service/1,2`, `remove_service/1`, `rename_service/2,3`, `erlsrv/1`.
- Windows-only: shells out to `erts-<Vsn>/bin/erlsrv.exe` via `open_port({spawn,...})`; no-op/unusable on non-Windows.

**Data / build files in this directory**
- `systools.hrl` — internal records `#release{}` and `#application{}`; only included by `systools_make`, `systools_rc`, `systools_relup` (and the test suite `systools_rc_SUITE` via `-include_lib("sasl/src/systools.hrl")`). Declared as `INTERNAL_HRL_FILES` in the Makefile.
- `sasl.app.src` / `sasl.appup.src` — templates; `Makefile` sed-substitutes `%VSN%` with `$(SASL_VSN)` into `ebin/sasl.app` and `ebin/sasl.appup`. `sasl.appup.src` expresses up/downgrade rules as regex version patterns with `restart_new_emulator` instructions.
- `Makefile` — explicit `MODULES` list (17 modules), `HRL_FILES=` empty, `INTERNAL_HRL_FILES=systools.hrl`, app/appup generation rules, and release install rules (`src/` gets `.erl` files + `systools.hrl`).

## Constraints
- Do not hand-edit `ebin/sasl.app` / `ebin/sasl.appup` — they are generated from the `.src` templates by the Makefile; only `src/Makefile`'s `MODULES` list governs what gets compiled (new module files are ignored until added there).
- `systools.hrl` is internal: records may change without notice, and changes must keep `systools_make`/`systools_rc`/`systools_relup` in sync.
- Compile flags: `-I../../stdlib/include`, `-Werror`, `+nowarn_deprecated_catch` — sources still use legacy `catch` and will fail the build on any warning (`systools_make`/`systools_relup` include `systools.hrl`, `systools` includes stdlib's `erl_compile.hrl`).
- Public/private split is signalled solely by `-moduledoc`: adding `-moduledoc` to an internal module publishes it in the docs build; functions can be exported yet `-doc false` (e.g. `alarm_handler:add_alarm_handler/1,2`).
- Runtime file operations must go through `release_handler`'s `root_dir_relative_*` / `do_*` wrappers so that client-node and master-node (`Masters`) semantics are respected; do not call `file:*` directly on paths under `$ROOT/releases`.
- Pure-Erlang app: no `configure`, no NIFs, no `c_src/` here; but `systools_make` depends on the `tools` app (`xref`) at run time when the `exref` option is used (hence `DIA_PLT_APPS=tools` in the app Makefile).

## Design Decisions
- Two-layer release handling: high-level `appup` instructions are translated once — either at package build time (`systools_relup` + `systools_rc`) into a low-level `relup` script, or at runtime for direct `upgrade_app/downgrade_app` — and the low-level script is what `release_handler_1` actually evaluates.
- The `release_handler` child spec in `sasl.erl` deliberately uses `modules = []` so that release handling can never hot-upgrade `release_handler` itself (the code loading of this process is frozen).
- `alarm_handler` is a `gen_event` manager rather than a bespoke server so that users can swap in their own handler (e.g. an SNMP/MIB-based one) via `gen_event:swap_handler/3`.
- The legacy `error_logger` API and the `sasl_report*` handlers are retained for backward compatibility with `logger_sasl_compatible` mode, but the modern path is the `logger` handler installed by `sasl.erl` (OTP `logger` domain `[otp,sasl]`).
- `sasl_sup` is hosted by the application module `sasl` itself (`sasl:init/1`) to avoid an extra module.

## Known Issues
- `release_handler` is a single registered process owning the persistent `RELEASES` file; killing or bypassing it (e.g. mixing `install_release/1,2` with `upgrade_app/2` / `eval_appup_script/4`) leaves the node in an inconsistent state — the docs warn explicitly about this.
- Client/master mode: file operations are performed on all master nodes or none; a partially-reachable master leaves the release state inconsistent.
- `erlsrv.erl` is only functional on Windows and requires `erts-<Vsn>/bin/erlsrv.exe` to exist; on other platforms its calls fail at the `open_port` step.
- `alarm_handler`'s default handler logs via the deprecated `error_logger:info_report/1` and keeps accumulated alarms in memory (the alarm list can grow unbounded).
- `rb` depends on STDLIB `log_mf_h`; if `error_logger_mf_dir`/`maxbytes`/`maxfiles` are not configured, no report files exist and `rb` has nothing to show.

## Notes for Agents
- Entry point for build-time release work is `systools.erl`; entry point for runtime installation is `release_handler.erl`. Everything else in the release-handling area is internal plumbing called by those two.
- Companion test suites live in the sibling `../test/` directory (read-only from here; escalate writes to the parent) — `release_handler_SUITE`, `systools_SUITE`, `systools_rc_SUITE`, `alarm_handler_SUITE`, `rb_SUITE`, `sasl_SUITE`, `sasl_report_SUITE`. `TEST_NEEDS_RELEASE=true` for the app, so most suites run against an installed release.
- Public API docs configuration is in `../doc/docs.exs` (sibling) — it only renders `-moduledoc`'d modules, which is why `alarm_handler`, `rb`, `release_handler`, `systools` are the visible API.

## Routing Table
| Module group | Files |
| --- | --- |
| High-level release tools (public API) | `systools.erl` |
| Script/boot/tar creation | `systools_make.erl` |
| `relup` construction | `systools_relup.erl` |
| Script instruction translation (high→low level) | `systools_rc.erl` |
| Runtime release handler (`gen_server`, `RELEASES` file) | `release_handler.erl` |
| Low-level script evaluation / code loading | `release_handler_1.erl` |
| Low-level term/file IO helpers | `systools_lib.erl` |
| Internal release/application records | `systools.hrl` |
| Application module + `sasl_sup` supervisor + logger wiring | `sasl.erl` |
| Alarm handling (`gen_event` manager) | `alarm_handler.erl` |
| Report browser (RB tool) | `rb.erl`, `rb_format_supp.erl` |
| Legacy `error_logger` SASL report formatting/handlers | `sasl_report.erl`, `sasl_report_tty_h.erl`, `sasl_report_file_h.erl`, `format_lib_supp.erl`, `misc_supp.erl` |
| Windows service control | `erlsrv.erl` |
| App resources & build | `sasl.app.src`, `sasl.appup.src`, `Makefile` |
| Module test suites (sibling — read-only, escalate writes to parent) | `../test/` |
| Public API docs / ExDoc config (sibling — read-only) | `../doc/` |
