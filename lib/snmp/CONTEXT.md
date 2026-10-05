# snmp — SNMP Agent & Manager Toolkit

## Intent
The `snmp` OTP application: an SNMP **agent** (v1/v2c/v3) and **manager** toolkit in Erlang, plus a **MIB compiler** (`snmpc`) that turns ASN.1 `.mib` sources into compiled `.bin` MIBs and generated `.hrl` headers.
One application covers both the agent and manager sides; the app adds `snmpa_supervisor`/`snmpm_supervisor` dynamically depending on the `agent`/`manager` app-env keys.
App version lives in `vsn.mk` (`SNMP_VSN`, currently `5.20.5`); no app here has a `Makefile.in` except the generated ones below.

## API Surface
- `snmp` (in `src/app/`) — umbrella interface to the toolkit (`start/stop`, `start_agent`, `start_manager`, version/date-time helpers).
- `snmpa` + `snmpa_conf` (in `src/agent/`) — the SNMP agent API and its config-file utility; the agent is extensible through 11 pluggable behaviours (`snmpa_network_interface`, `snmpa_mib_storage`, `snmpa_mib_data`, `snmpa_get_mechanism`, `snmpa_set_mechanism`, `snmpa_authentication_service`, `snmpa_error_report`, `snmpa_discovery_handler`, `snmpa_notification_filter`, `snmpa_notification_delivery_info_receiver`, `snmpa_network_interface_filter`).
- `snmpm` + `snmpm_conf` (in `src/manager/`) — the SNMP manager API and its config-file utility; extensible via the `snmpm_user` / `snmpm_network_interface` (+ filter) behaviours.
- `snmpc` (in `src/compile/`) — the MIB compiler (`compile/1..3`, `mib_to_hrl/1,3`, `is_consistent/1`); also shipped as an escript (`../bin/snmpc`, generated from `snmpc.src`).
- Application resource `src/app/snmp.app.src`; upgrade instructions `src/app/snmp.appup.src`; example node config `src/app/snmp.config`.
- `include/` — public user headers: `snmp_types.hrl` (core types/records), `snmp_tables.hrl` (agent MIB table records for Mnesia storage), `SNMPv2-TC.hrl` (auto-generated MIB header, checked in).
- `bin/` — `snmp-v2tov1` (built from `snmp-v2tov1.src` + `.pl`/`.sed`): converts a SMIv2 MIB to SMIv1; used to produce `mibs/v1/`.

## Constraints
- Generated code — never hand-edit the generated file, edit its source template instead:
  - `src/agent/Makefile` ← `src/agent/Makefile.in` (by `configure`).
  - `mibs/Makefile` ← `mibs/Makefile.in` (by `configure`).
  - `src/compile/snmpc_mib_gram.erl` ← `.yrl` (yecc); `src/misc/snmp_pdus_basic.erl` (+ `.asn1db`) ← the ASN.1 `snmp_pdus_basic.asn`/`.set.asn` via the `asn1` app.
  - `bin/snmpc` ← `snmpc.src`; `bin/snmp-v2tov1` ← `snmp-v2tov1.src`; `ebin/snmp.app`/`snmp.appup` ← the `.app.src`/`.appup.src` templates.
- The app has its **own autoconf**: `configure.ac` → `configure` generates `mibs/Makefile` and `src/agent/Makefile`, and checks for Perl (needed by `snmp-v2tov1`); it takes `--with-snmp-empty-pdu-size=SZ` (default/min 21, feeds `-Dempty_pdu_size`).
- Build order is fixed by the two `subdirs.mk` files: top-level `SUB_DIRECTORIES = src/compile mibs src examples priv/conf doc`; `src/subdirs.mk` = `compile app misc agent manager`.
- Children of `src/` are compiled with `-Werror` and `+warn_unused_vars` by default (disable via `SNMP_WARNING_AS_ERROR=false` / `WARN_UNUSED_VARS=false`).
- New MIBs/conf files/suites/example dirs must be registered in their directory's `Makefile`/`modules.mk`/`files.mk`/`subdirs.mk` to be built or installed.
- `ebin/` and `priv/mibs/`, `priv/plt/` are build output; Dialyzer PLT path is `priv/plt/snmp.plt` (`DIA_PLT` in `Makefile`).

## Dependencies
- From `src/app/snmp.app.src`: `applications = [kernel, stdlib]`; `runtime_dependencies = asn1-5.4, stdlib-5.0, runtime_tools-1.8.14, mnesia-4.12, kernel-8.0, erts-12.0, crypto-4.6`.
- `crypto` must be started before `snmp` when SNMPv3 authentication/encryption is used; `mnesia` must be started first when MIB storage is configured as mnesia.
- `mibs/` compilation requires the built `snmpc` in `ebin/` (so `src/compile` builds before `mibs`).
- External (test-only): Net-SNMP CLI/daemon tools for `test/snmp_to_snmpnet_SUITE`.

## Test Strategy
- Common Test suites in `test/` (`snmp_agent_SUITE`, `snmp_manager_SUITE`, `snmp_compiler_SUITE`, `snmp_pdus_SUITE`, config/log/note-store/interop suites; see `test/CONTEXT.md`).
- Suites exercise the *installed* app and require the MIB fixtures to be precompiled to `.bin`/`.hrl` first (`make mib`).
- `make test` runs the legacy `snmp_test_server`-driven `agent` and `manager` targets; suites also run under `ct_run`/`test_server` with the `ts_install_cth` hook.
- MIB fixtures live in `test/snmp_test_data/`; interop fixtures `test/snmp_to_snmpnet_SUITE_data/` need external Net-SNMP tools (skipped where absent).

## Notes for Agents
- Two `subdirs.mk` quirks: the app-level one lists `src/compile` (not `src`) first, and `src/subdirs.mk` has a commented-out `proxy` entry.
- `src/agent/snmpa.erl` and the `snmp_*_mib` modules define types consumed by other agent modules, so they must be compiled first (`agent/Makefile` builds `BEHAVIOUR_MODULES` before `MODULES`).
- The standard MIB implementation modules (`snmp_*_mib`) live in `src/agent/` despite the shared `snmp_` prefix; `snmp_usm` (shared) is in `src/misc/` while `snmpa_usm`/`snmpm_usm` are the agent/manager wrappers.
- `../bin/snmpc` and the example `snmp-v2tov1` script are generated at build time and are git-ignored.

## Routing Table
- `src/` → Erlang sources, split into `compile/` (MIB compiler), `app/` (glue), `misc/` (shared), `agent/`, `manager/`
- `mibs/` → bundled ASN.1 MIB sources compiled by `snmpc` into `.bin`/`.hrl`
- `test/` → Common Test suites + MIB/config fixtures (`snmp_test_data/`, `test-mibs/`, `test_config/`, `snmp_to_snmpnet_SUITE_data/`)
- `doc/` → user guide + reference manual (ExDoc `docs.exs`, `guides/`, `references/`, `notes.md`)
- `include/` → public Erlang headers (`snmp_types.hrl`, `snmp_tables.hrl`, `SNMPv2-TC.hrl`)
- `examples/` → worked example agent (`ex1/`) and manager (`ex2/`)
- `priv/` → example agent/manager config files (`conf/`) and build-output dirs (`mibs/`, `plt/`)
- `bin/` → `snmp-v2tov1` SMIv2→SMIv1 converter (first-use build output; edit `.src`/`.pl`/`.sed`)
- `../snmp/src/app/` → application resource and umbrella `snmp` API (this app)

## See Also
- `../../lib/CONTEXT.md` — OTP application conventions; repo-root `CONTEXT.md` — top-level build/release.
- `../crypto/`, `../public_key/`, `../asn1/` — runtime dependencies of the SNMPv3/USM code paths (siblings — read-only).
