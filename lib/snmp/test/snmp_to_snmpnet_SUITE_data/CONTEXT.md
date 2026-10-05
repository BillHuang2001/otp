# snmp_to_snmpnet_SUITE_data — Net-SNMP interop fixtures

## Intent
Data directory (`data_dir`) of `../snmp_to_snmpnet_SUITE.erl`, the Erlang SNMP ↔ Net-SNMP interoperability suite.
Holds the MIBs, Net-SNMP config files and the process wrapper the suite needs to drive the external Net-SNMP programs (`snmpget`, `snmpd`, `snmptrapd`, `mib2c`, `net-snmp-config`).

## API Surface (checked-in files)
- `OTP-C64-MIB.mib` — test MIB defining `otpC64Num1..4` (`Counter64`, values `18446744073709551615`, `9223372036854775807`, `9223372036854775808`, `0`) under `otpExpr` from `OTP-REG`; the only file here MIB-compiled by the build.
- `OTP-C64-MIB.bin`, `OTP-C64-MIB.hrl` — **build output, not checked in**: generated from `OTP-C64-MIB.mib` by `../Makefile` (it is in `NETSNMP_MIB_FILES` in `../modules.mk`); root `.gitignore` ignores `[A-Z]*.bin` / `[A-Z]*.hrl` in this directory.
  The `.hrl` is `-include`d by the suite (provides `?otpC64Num*_instance`, `?default_otpC64Num*`); the `.bin` is loaded by the Erlang agent via `snmpa:load_mib(..., join(MibDir, "OTP-C64-MIB"))`.
- `TestTrapv2.mib` — notification MIB (`testTrapv21`, `testTrapv22`, `linkUp`); byte-identical duplicate of `../snmp_test_data/TestTrapv2.mib`, but **not** in this dir's `NETSNMP_MIB_FILES`, so its compiled form is committed instead.
- `TestTrapv2.bin` + `TestTrapv2.bin.license` — pre-compiled (prebuilt) version of `TestTrapv2.mib` loaded by the Erlang agent in `erlang_agent_netsnmp_inform`; the build does not regenerate it here.
- `snmpd.conf` — Net-SNMP config passed to `snmpd` via `-c`; sets `sysLocation`/`sysContact`, a `createUser myinternaluser SHA dropdead` + `agentSecName myinternaluser`, `master agentx`, and disables persistent storage (`noPersistentLoad/Save yes`).
- `mib2c.c64.conf` — Net-SNMP `mib2c` template used to generate a C subagent (`otpC64MIB.c/.h`) exposing the `Counter64` objects; only generates code for Counter64 nodes.
- `start_stop_wrapper` — POSIX `/bin/sh` helper (executable bit required) that starts a program in the background and kills it when `stop` is read on stdin; the suite always launches external programs as `start_stop_wrapper <prog> <args...>`.

## Constraints
- The suite must never write into this directory: `mib2c`/`net-snmp-config` output and the compiled subagent land in the test run's cwd, not here.
- Do not hand-edit or `git add` `OTP-C64-MIB.bin`/`.hrl`; regenerate with the test Makefile (`make mib` / `make tests` in `../`).
- `start_stop_wrapper` must keep its executable mode (`test -x "$1"` / direct exec), otherwise every program-launching case fails.

## Known Issues / Gotchas
- External Net-SNMP tooling is required: `snmpd`, `snmptrapd`, `snmpget` (Ubuntu packages `snmp` and `snmpd`), plus `mib2c` and `net-snmp-config` for the manager-vs-netsnmp-subagent cases.
- The suite skips rather than fails when tools are missing: `netsnmp_init/1` (called from `init_per_suite/1`) skips with `"No NetSNMP"` if `snmpd -v` prints no `NET-SNMP`, and with `"Buggy NetSNMP"` for versions matching `5.4|5.6.2.1`; `find_executable/2` skips when a program is not on `PATH` (it also probes `/usr/local/sbin`, `/usr/sbin`, `/sbin`); `init_per_testcase(erlang_manager_netsnmp_get, _)` skips with `"Required mib2c not found"` and `has_netsnmpconfig/0` skips with `"No net-snmp-config available"`.
- Programs are located once per group and cached in the CT config under their own atom key (`snmpget`, `snmpd`, `snmptrapd`), so a missing binary only skips the affected group.
- Default ports used by the suite (not by these fixtures): Erlang agent `4000`, Erlang manager `8989`; the AgentX socket port is picked per group via `mk_port_number/0` and passed as `--agentXSocket=tcp:localhost:<port>`.
- Failure mode to recognise: if the OTP MIBs are not resolvable, requests return `noSuchObject` and the suite turns that into a skip (`verify_oid/3`), not a failure — an empty response value is not necessarily a code bug.
- `snmpd` is started with `-m SNMPv2-SMI:OTP-REG:OTP-C64-MIB` and `-M $(code:lib_dir(snmp))/mibs:$(mib_dir)`, i.e. `OTP-REG.mib` comes from `../../mibs/`, while `OTP-C64-MIB` resolves through this directory (the `.bin` for the Erlang agent, the `.mib` text for Net-SNMP).

## Notes for Agents
- `snmpget` is invoked with `-m :` (numeric OIDs only) and `-Cf`, so its output is compared verbatim against strings built by `expected/2` in the suite.
- `snmptrapd` is started with `-M <snmp>/mibs:<data_dir>` so it can decode `TestTrapv2` traps; Net-SNMP reads the `.mib` text here, never the `.bin`.
- Net-SNMP on SunOS uses `-a` instead of `--disableAuthorization=yes` — relevant only if these fixtures gain new trapd arguments.

## Test Strategy
- Run via Common Test: `make test`/`ct_run` in `../` (the suite is listed in `../modules.mk` `MODULES`), or indirectly through `lib/snmp` `make test`.
- The whole directory is shipped with the release test tree: `release_tests_spec` in `../Makefile` tars `*_SUITE_data` into `$(RELEASE_PATH)/snmp_test`.
- Coverage gap: nothing here is unit-tested — all coverage is via the interop suite, so the fixtures are only exercised where the Net-SNMP tools exist (skipped on most CI runs).

## See Also
- `../snmp_to_snmpnet_SUITE.erl` — the suite that consumes every file here (read-only from this node; escalate changes to `../`).
- `../snmp_test_data/` — regular (non-interop) suite data: compiled `*.bin`/`*.hrl` outputs plus the duplicate `TestTrapv2.mib`.
- `../../mibs/` — OTP mibs installed with the app (`OTP-REG.mib`, `STANDARD-MIB`, …).
