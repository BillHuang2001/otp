# snmp_test_data — SNMP test MIB fixtures

## Intent
Shared data directory (`data_dir`) of the `snmp` application's Common Test suites: checked-in MIB sources plus their MIB-function association (`.funcs`) files, used as both the source dir for build-time MIB compilation and the runtime `mib_dir` the suites load compiled MIBs from.
The suite reaches it via its `data_dir` config, which `snmp_test_lib:fix_data_dir/1` rewrites to `<test>/snmp_test_data` because Common Test trashes `data_dir`; the string is also passed at compile time as `-Dsnmp_test_data=snmp_test_data` (used as `?snmp_test_data`).
On release install the whole directory is copied into `$(RELEASE_PATH)/snmp_test/snmp_test_data` (`tar cf - snmp_test_data` in `test/Makefile`).

## Contents / API Surface

### Precompiled at build time — must be listed in `MIB_FILES` in `test/modules.mk`
`test/Makefile` compiles each `MIB_FILES` entry with the snmp MIB compiler (`erlc -pa ../ebin +version +'{group_check,false}' -I<this dir> -I../priv/mibs`, plus `+'{description,true}'` when `SNMP_DESC=true`) producing `<name>.bin` and `<name>.hrl` in this same directory.
- Agent/sub-agent MIBs: `Klas1.mib`, `Klas1-v2.mib`, `Klas2.mib`, `Klas3.mib`, `Klas4.mib`, `SA-MIB.mib`, `OLD-SNMPEA-MIB.mib`, `OLD-SNMPEA-MIB-v2.mib`.
- Trap/notification MIBs: `TestTrap.mib`, `TestTrapv2.mib`.
- Generic test MIBs: `Test1.mib`, `Test2.mib`, `Test3.mib`.
- Compiler-suite fixtures: `AC-TEST-MIB.mib`, `MC-TEST-MIB.mib`.
- `EX1-MIB.mib` — precompiled but only referenced from a commented-out VACM view and from `snmp_config`/`snmpa_conf` generated-config templates.

### Compiled on demand by suites — deliberately NOT in `MIB_FILES`
- `snmp_compiler_SUITE` uses this dir as `mib_dir` and compiles these itself, each one a regression fixture named after its OTP ticket: `OTP8574-MIB.mib`, `OTP8595-MIB.mib`, `OTP10799-MIB.mib`, `OTP10808-MIB.mib`, `OTP14145-MIB.mib`, `OTP14196-MIB.mib`, plus `ERICSSON-TOP-MIB.mib` and `Test-LLDP-MIB.mib`; it also re-compiles `Test2.mib`/`Test3.mib` and writes throw-away MIBs into its own case dir.
- Checked-in copies of "standard" MIBs kept as fixtures: `SNMPv2-MIB.mib`, `STANDARD-MIB.mib`, `RFC1213-MIB.mib`, and `PROXY.mib` (PROXY is not referenced by any current suite).

### `.funcs` MIB-function association files
Each `<name>.funcs` binds MIB objects/tables to Erlang MFAs; `snmpc` picks it up automatically when compiling `<name>.mib` from this directory (it looks for `<path-without-extension>.funcs`).
Present for `Klas2`, `Klas3`, `Klas4`, `SA-MIB`, `Test1`, `Test2`, `STANDARD-MIB`, `SNMPv2-MIB`.
The referenced modules live in `lib/snmp/test/` (`klas3`, `sa`, `test1`, `test2`) or in the app itself (`snmp_generic`, `snmp_standard_mib`).

### Generated artifacts
`*.bin` and `*.hrl` in this directory are build output and are git-ignored (`.gitignore` = `*.bin`, `*.hrl`); only `.mib` and `.funcs` are tracked.

## Constraints
- A MIB is only precompiled if its file is listed in `MIB_FILES` (`lib/snmp/test/modules.mk`); adding a fixture there is what makes `.bin`/`.hrl` available at test time.
- `COMPILER_MIB_FILES` in `modules.mk` (`OTP8574-MIB`) is not consumed by `test/Makefile` — the compiler suite compiles that MIB itself.
- Explicit build-order dependencies in `test/Makefile`: `Klas4.bin` ← `Klas3.bin`, `SA-MIB.bin` ← `OLD-SNMPEA-MIB.bin`, `Test3.bin` ← `Test2.bin`; any new MIB that imports another MIB in this list needs a matching rule.
- Import resolution needs the standard MIBs already built into `lib/snmp/priv/mibs` (via `-I../priv/mibs`, built from `lib/snmp/mibs/`): `Klas1/2/3`, `SA-MIB`, `EX1-MIB`, `OLD-SNMPEA-MIB` import `RFC1213-MIB`/`STANDARD-MIB`; `Test1` and `OTP8595-MIB` import `SNMPv2-MIB`; `MC-TEST-MIB` imports `OTP-REG`; `Klas4` imports `Klas3`.
- Never edit or commit `*.bin`/`*.hrl` here; regenerate via `make` in `lib/snmp/test/` (or a full `make` in `lib/snmp/`).
- Renaming a MIB file requires renaming its `.funcs` companion accordingly, otherwise the instrumentation silently falls back to the default implementation.

## Known Issues / Gotchas
- `Klas1.mib` (SMIv1) and `Klas1-v2.mib` (SMIv2, MIB name `Klas1-V2`) describe the same objects under different names — suites load them by file basename (`"Klas1"` / `"Klas1-v2"`), so keep both.
- The local `SNMPv2-MIB.mib` is the 1995 IETF version and differs from `lib/snmp/mibs/SNMPv2-MIB.mib`; the local `STANDARD-MIB.mib` matches `lib/snmp/mibs/STANDARD-MIB.mib`, and the local `RFC1213-MIB.mib` differs from it only in spelling (`dissasociates`). Do not "fix"/re-sync these fixture copies.
- `Test2.hrl` is the only generated header included directly by suites (`-include("snmp_test_data/Test2.hrl")` in `snmp_manager_SUITE` and `snmp_agent_mibs_SUITE`), so deleting/renaming `Test2.mib` breaks suite compilation.
- `snmp_agent_test_lib:mibs/2` builds the agent's MIB load list from this dir (`Klas1/2/3/4.bin`, `SA-MIB.bin`, `TestTrap.bin`, `Test1.bin`, `Test2.bin`, `TestTrapv2.bin`, `OLD-SNMPEA-MIB.bin`) plus `STANDARD-MIB.bin`/`SNMPv2-MIB.bin` from `priv/mibs`; a MIB removed from `MIB_FILES` breaks that list at test-run time, not at build time.
- `OTP*-MIB.mib` fixture names encode the ticket they regression-test (details in the file comments / git log).

## Notes for Agents
- `OTP-C64-MIB.mib` referenced by `NETSNMP_MIB_FILES` in `modules.mk` lives in the sibling `test/snmp_to_snmpnet_SUITE_data/`, not here.
- Do not document or reformat individual MIB object definitions; these files mirror upstream/legacy content on purpose.

## See Also
- `../CONTEXT.md` — the snmp CT suites and `snmp_test_lib` helpers that consume this dir (sibling/parent — read-only for this node).
- `test/modules.mk`, `test/Makefile` — `MIB_FILES`, compile flags and MIB build rules.
- `../test-mibs/` and `../test_config/` — other test fixture dirs (sibling, read-only).
- `lib/snmp/mibs/` and `lib/snmp/priv/mibs/` — canonical standard MIBs and their compiled `.bin`s used as import sources.
