# kernel — OTP Kernel Application

## Intent
The `kernel` application is the lowest OTP application above the runtime system (`erts/`).
It provides everything needed to boot and run a node: application control, the code server, file & disk-log I/O, sockets/name resolution, distribution (`net_kernel`, `inet`), the `logger` subsystem, and OS/terminal services (`os`, `user_drv`, `prim_tty`).
It is built as part of `ERTS_APPLICATIONS = stdlib sasl kernel compiler` (see `../Makefile`), i.e. among the first apps built and started.
Boundary vs `stdlib`: anything required during early boot — before the code server, file server or logger exist — belongs in kernel; `stdlib` holds higher-level pure-Erlang libraries that may rely on kernel.
App version `KERNEL_VSN` (currently `11.0.4`) is defined in `vsn.mk`.

## API Surface
- `src/` — 104 Erlang modules + internal `.hrl` + `kernel.app.src`/`kernel.appup.src` + `Makefile`. The directory is FLAT; see `src/CONTEXT.md` for the full module map grouped by area.
  Public modules by area: application control (`application`, `kernel`), code loading (`code`, `erl_ddll`, `erl_boot_server`, `erl_debugger`), file I/O (`file`, `disk_log`, `wrap_log_reader`), logging (`logger`, `logger_std_h`, `logger_disk_log_h`, `logger_filters`, `logger_formatter`, `logger_handler`, `error_logger` legacy), distribution (`net_kernel`, `net`, `net_adm`, `auth`, `erl_epmd`, `global`, `global_group`, `pg`, `rpc`, `erpc`), sockets/DNS (`inet`, `inet_res`, `gen_tcp`, `gen_udp`, `gen_sctp`, `socket`), OS/terminal (`os`, `heart`, `data_publisher`), tracing (`seq_trace`, `trace`).
- `include/` — the 8 public `.hrl` headers exported to other apps: `file.hrl`, `inet.hrl`, `inet_sctp.hrl`, `logger.hrl`, `eep48.hrl`, `dist.hrl`, `dist_util.hrl`, `net_address.hrl`.
- `doc/` — ExDoc config (`docs.exs`) + hand-written guides/references/overview; per-module reference docs are generated from `@doc`/`-moduledoc` in `src/`. See `doc/CONTEXT.md`.
- `test/` — flat Common Test suite dir (~62 `*_SUITE.erl`), helper libs, test apps and `*_SUITE_data/` fixtures. See `test/CONTEXT.md`.
- `examples/` — standalone example distribution + socket programs shipped in the release docs (`gen_tcp_dist/`, `erl_uds_dist/`, `uds_dist/`, `socket_sctp/`); built by `examples/Makefile`.
- `Makefile` — includes `../make/target.mk`, `../make/$(TARGET)/otp.mk`, `vsn.mk`, `../make/otp_subdir.mk` and `../make/app_targets.mk`; sets `SUB_DIRECTORIES = src doc examples`.
- `src/kernel.app.src` — application resource template: `{applications, []}`, `{mod, {kernel, []}}`, explicit `modules`/`registered` lists, default `env`, and `runtime_dependencies`. `%VSN%` is substituted at build time from `vsn.mk`.
- `src/kernel.appup.src` — release upgrade/downgrade instructions (`.appup` generated from it).

## Constraints
- kernel declares `{applications, []}` and must start before any other application — it may use only `erts` plus already-available stdlib modules, never higher apps.
- Every module MUST be listed in BOTH `src/Makefile` (`MODULES`) and `src/kernel.app.src` (`modules`); the two lists are kept identical. New public modules also need a `-moduledoc` (public) vs `-moduledoc false` (internal) decision — that attribute is what selects whether the module is emitted into the docs.
- Internal `.hrl` live in `src/`; headers meant for other applications live in `include/` (and are listed in `HRL_FILES`).
- Never edit generated files: edit `kernel.app.src`/`kernel.appup.src` (not the generated `kernel.app`/`kernel.appup`); `inet_dns_record_adts.hrl` is generated from `inet_dns_record_adts.pl` by perl.
- Sources compile with `-Werror +warn_deprecated_catch +warn_obsolete_bool_op -I../include`.
- Where early-boot order matters, keep dependencies minimal (e.g. `kernel`/`application`/`error_handler` must work before most of kernel exists).

## Dependencies
- `{applications, []}` — no application dependencies; only erts and already-loaded stdlib modules at runtime.
- `{runtime_dependencies, ["erts-17.0", "stdlib-@OTP-20412@", "sasl-3.0", "crypto-5.8"]}` (the `@OTP-…@` placeholder is resolved by `make/fixup_development_runtime_dependencies`; `crypto` is an optional dependency used by `inet_db` when present).
- Build: `src/Makefile` includes `../vsn.mk`, `erts/vsn.mk` and `erts/epmd/epmd.mk` to obtain EPMD port/dist flags; `perl` is needed for `inet_dns_record_adts.hrl`.
- `DIA_PLT_APPS = crypto compiler` (extra apps in the Dialyzer PLT for kernel).

## Notes for Agents
- `ebin/`, `priv/` and `doc/src/` contain only `.gitignore` (build outputs / generated docs). `priv/` is otherwise empty.
- Several modules normally thought of as kernel live in `erts/preloaded/src/` instead: `init`, `erlang`, `erl_prim_loader`, `prim_file`, `prim_socket`. Edits for those go to `erts/`, not here.
- Two socket backends coexist: the classic `inet_drv`-based `gen_tcp`/`gen_udp` and the NIF `socket`-based `gen_tcp_socket`/`gen_udp_socket`, selected via `{inet_backend, socket}`.
- `pg2` is a removed tombstone module; `pg` was reimplemented on `data_publisher` (OTP 29+).
- Default distribution module is `inet_tcp_dist` (`-proto_dist inet_tcp`); `inet_epmd_dist` is opt-in and reused by the ssl dist.

## Test Strategy
- Common Test suites under `test/`; `TEST_NEEDS_RELEASE=true` — `make test` builds and installs an OTP release, then runs `ct_run -spec` against it (`make/test_target_script.sh`).
- `../Makefile` sets `DIA_PLT_APPS=crypto compiler`; `test/` holds the spec files (`kernel.spec`, `kernel_smoke.spec`, `kernel_bench.spec`, `kernel_gh.spec`, `logger.spec`) and cover files.
- Suites are grouped by area (application/code, file/disk_log, distribution, sockets/inet, logger, os/terminal); the socket suites are the largest area. Details and per-suite data dirs are in `test/CONTEXT.md`.
- `test/Makefile` keeps a separate `SOCKET_MODULES` list; every compiled helper/suite must be listed there or in `MODULES` and in the `release_tests_spec` install list.

## Routing Table
| Path | Contents |
| --- | --- |
| `src/` | Kernel implementation — 104 modules grouped by area, app resource, build Makefile (see `src/CONTEXT.md`) |
| `test/` | Common Test suites, helper libs, test apps, `*_SUITE_data/` fixtures (see `test/CONTEXT.md`) |
| `doc/` | ExDoc config, guides, references, application overview and release notes (see `doc/CONTEXT.md`) |
| `include/` | 8 public headers exported to other applications |
| `examples/` | Example distribution/socket programs shipped in the release docs |
| `priv/` | Empty (build output; only `.gitignore`) |

## See Also
- `../Makefile` — the OTP app build coordinator that fixes build order (`stdlib sasl kernel compiler` first).
- `../../erts/` — the runtime system, including `erts/preloaded/src/` where a few kernel-named modules live, and `erts/epmd/` for the EPMD daemon.
- `../../make/` — shared build/docs machinery (`app_targets.mk`, `doc.mk`, `ex_doc.exs`, `test_target_script.sh`).
- `../../lib/stdlib/` — the next layer up; kernel must not depend on it beyond early-boot availability.
