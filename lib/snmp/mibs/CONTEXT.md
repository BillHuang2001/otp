# snmp — MIB Sources

## Intent
Ship the ASN.1 MIB modules that the OTP `snmp` application needs at runtime plus the standard SMI base modules: the RFC MIBs, the IETF SNMPv3 framework MIBs and the OTP-specific MIBs.
At build time every entry of `MIBS` is compiled by the snmp MIB compiler (`snmpc`, invoked as `erlc -pa ../ebin +version`) into `<name>.bin` and `<name>.hrl`; the `.mib` sources and `.funcs` files are also installed for users who want to recompile or reference them.
This directory is one build subdirectory (`mibs`) of the snmp app — see `../subdirs.mk` (`SUB_DIRECTORIES = src/compile mibs src examples priv/conf doc`).

## Installed layout
- `.mib` + `$(FUNCS_FILES)` + `$(STD_v2_MIB_FILES)` → `$(RELSYSDIR)/mibs`.
- `$(STD_v1_MIB_FILES)` + generated `v1/OTP-SNMPEA-MIB.mib.v1` → `$(RELSYSDIR)/mibs/v1`.
- generated `.bin` → `$(RELSYSDIR)/priv/mibs`; generated `.hrl` (incl. `SNMPv2-TC.hrl`) → `$(RELSYSDIR)/include`.

## API Surface (MIB lists in `Makefile.in`)
- `OTP_MIBS` = `OTP-REG` (OTP enterprise OID root, `otpModules`/`otpApplications`), `OTP-TC` (OTP textual conventions), `OTP-SNMPEA-MIB` (legacy OTP SNMPEA objects; most objects obsolete, kept for its OID assignments).
- `MIBS` = the standard set compiled to `.bin`+`.hrl`: `RFC1213-MIB`, `STANDARD-MIB`, `SNMPv2-TM`, `SNMPv2-MIB`, `SNMP-FRAMEWORK-MIB`, `SNMP-MPD-MIB`, `SNMP-TARGET-MIB`, `SNMP-NOTIFICATION-MIB`, `SNMP-COMMUNITY-MIB`, `SNMP-USER-BASED-SM-MIB`, `SNMP-VIEW-BASED-ACM-MIB`, `SNMP-USM-AES-MIB`, `INET-ADDRESS-MIB`, `TRANSPORT-ADDRESS-MIB`, `SNMP-USM-HMAC-SHA2-MIB`, plus `$(OTP_MIBS)`.
- `STD_v1_MIB_FILES` = `RFC1155-SMI.mib`, `RFC-1212.mib`, `RFC-1215.mib` — SMIv1 base modules, installed only (never compiled).
- `STD_v2_MIB_FILES` = `SNMPv2-SMI.mib`, `SNMPv2-TC.mib`, `SNMPv2-CONF.mib` — SMIv2 base modules, installed only.
- `FUNCS_FILES` = `STANDARD-MIB.funcs`, `SNMPv2-MIB.funcs`, `SNMP-NOTIFICATION-MIB.funcs`, `SNMP-TARGET-MIB.funcs` — the `.funcs` files installed into the release.
- `V1_MIB_FILES` = `v1/OTP-SNMPEA-MIB.mib.v1`, generated from `OTP-SNMPEA-MIB.mib` by `../bin/snmp-v2tov1` (built from `../bin/snmp-v2tov1.src` with `@PERL@`).

## `.funcs` files
Erlang terms mapping a MIB object name to its implementation callback, e.g. `{sysDescr, {snmp_generic, variable_func, [{sysDescr, persistent}]}}`.
`snmpc` reads `<name>.funcs` next to `<name>.mib` at compile time (unless the `+module` option is given — see `../src/compile/snmpc.erl`, `snmpc_lib:read_funcs`); the mapping is baked into the generated `.hrl`.
Nine `.funcs` files are present in this directory but only the four in `FUNCS_FILES` are installed into the release.

## Constraints
- `Makefile` is generated from `Makefile.in` by autoconf (`AC_CONFIG_FILES([mibs/Makefile:mibs/Makefile.in])`); edit `Makefile.in` only and regenerate via the top-level `./otp_build update_configure`.
- `.bin`, `.hrl` (under `../priv/mibs` and `../include`) and `v1/*.mib.v1` are build artifacts — never edited or committed by hand (the v1 output is git-ignored from the repo-root `.gitignore`).
- Parallel make relies on explicit prerequisites for MIB imports, so add a `$(SNMP_BIN_TARGET_DIR)/<mib>.bin: ...` edge here whenever a new MIB IMPORTS from another.
  Current edges: `STANDARD-MIB` → `RFC1213-MIB`; `OTP-TC` and `OTP-SNMPEA-MIB` → `OTP-REG`; `SNMP-TARGET-MIB` → `SNMP-FRAMEWORK-MIB`; `SNMP-NOTIFICATION-MIB` and `SNMP-COMMUNITY-MIB` → `SNMP-FRAMEWORK-MIB` + `SNMP-TARGET-MIB`; `SNMP-USER-BASED-SM-MIB`, `SNMP-USM-HMAC-SHA2-MIB`, `SNMP-VIEW-BASED-ACM-MIB`, `SNMP-USM-AES-MIB` → `SNMP-FRAMEWORK-MIB`.
- A new MIB ships only if its base name is added to `MIBS` (or to one of the `STD_*_MIB_FILES` lists for install-only files).
- `../prebuild.skip` lists `Makefile`, excluding it from prebuilt-bootstrap handling.

## Design Decisions
- Upstream/IETF MIB text is kept verbatim apart from an OTP SPDX copyright header; all transformation happens in `snmpc` at build time.
- SMIv1 base modules and the v1-converted `OTP-SNMPEA-MIB` are installed but not compiled: they exist for tools/agents that must understand v1 information modules, while the runtime uses the SMIv2 modules.

## Notes for Agents
- Compiling requires the snmp app's compiled compiler in `../ebin`; the app-level `make` builds `src/compile` before `mibs`.
- `make info` prints the resolved `MIBS`, `BIN_TARGETS`, `HRL_TARGETS`, `SNMP_FLAGS`, etc.
- Env knobs add snmpc flags: `MIBS_VERBOSITY`, `MIBS_REFERENCE=true`, `MIBS_OPTIONS=true`, `MIBS_MC=true`, `MIBS_AC=true` (`+version` is always passed).
- `make clean` removes the generated targets (`TARGET_FILES`); `make conf` delegates to the parent app.

## Routing Table
- `v1/` → generated SMIv1 MIB conversions installed to `mibs/v1`; only build output (currently just the ignored `OTP-SNMPEA-MIB.mib.v1`), no sources to document here.
- See also `../src/compile/` (the `snmpc` MIB compiler) and `../priv/mibs/`, `../include/` (generated `.bin`/`.hrl` targets) — read-only from here.
