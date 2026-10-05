# snmp — Test Configuration Source

## Intent
Source of standalone agent / manager / system configuration for manually set-up snmp test trees.
Each config file has a hand-edited `*.src` template that the local `Makefile` expands (variable substitution only, no protocol semantics) into a real `.config` / `.conf` file.
Nothing in the repo invokes this directory: it is a manual utility, run by hand (`make release_tests_spec TESTROOT=<dir>`) to prepare a test tree.

## API Surface
- `modules.mk` defines the file lists and the compiled module:
  - `SYS_CONFIG_FILES` = `sys.config` `sys-agent.config` `sys-manager.config` (templates `*.config.src` in this dir).
  - `AGENT_CONFIG_FILES` = `agent/{agent,community,context,notify,standard,target_addr,target_params,usm,vacm}.conf` (templates in `agent/`).
  - `MANAGER_CONFIG_FILES` = `manager/{manager,usm}.conf` (templates in `manager/`).
  - `MODULES` = `snmp_test_config`.
- `snmp_test_config.erl` — tiny host-inspection helper: `ip_address/0` prints the host IP as a 4-element list (`[127,0,0,1]`), `ip_address2/0` prints it dotted (`127.0.0.1`). Used by the Makefile (and callable at runtime) to fill in addresses.
- `Makefile` targets: `tests`/`$(TYPES)` (default: build `snmp_test_config.beam` + all config files), `clean`, `info` (prints resolved vars/file lists), `release_tests_spec`, no-op `release_spec` / `release_docs_spec` / `docs`.
- `release_tests_spec` = `clean opt`, then creates `$(RELSYSDIR)`, `agent/{conf,db,log}`, `manager/{conf,db,log}` and installs the three sys configs to `$(RELSYSDIR)` and the agent/manager `.conf` files into `agent/conf` / `manager/conf`.

## Constraints
- Edit only the `.src` templates; the generated `.config` / `.conf` files are build output and are git-ignored.
- Substitution is literal:
  - `%.config` ← `%.config.src` via `perl -p s?%DIR%?$(RELSYSDIR)?` — the only variable available to sys configs.
  - `agent/%.conf` ← `agent/%.conf.src` and `manager/%.conf` ← `manager/%.conf.src` via `sed`, replacing `%ADDR%`, `%SYS_CONTACT%`, `%SYS_LOCATION%`, `%SYS_NAME%`, `%TARGET_NAME_PRE%`, `%ENGINE_ID%`, `%USM_ENGINE_ID%` (engine ids are quoted when inserted; manager templates use only `%ADDR%`, `%ENGINE_ID%`, `%USM_ENGINE_ID%`).
  - Use no other `%VAR%` markers in templates — they will be copied through verbatim.
- Every substitution variable is a Makefile variable with a default and can be overridden on the command line: `ADDR`, `TARGET_NAME_PRE`, `SYS_CONTACT` (`foo@bar.ericsson.se`), `SYS_LOCATION` (`Erlang/OTP`), `SYS_NAME` (`FOO`), `AGENT_ENGINE_ID` / `AGENT_USM_ENGINE_ID` (`Agent engine of $(USER)`), `MANAGER_ENGINE_ID` / `MANAGER_USM_ENGINE_ID` (`Manager engine of $(USER)`), `TESTROOT`.
- A new config file must be added to the matching list in `modules.mk` (and to `.gitignore`) or it will neither be generated nor installed.

## Gotchas / Known Behaviours
- `RELSYSDIR = $(TESTROOT)`, default `/tmp` — a bare `make` here would write into `/tmp`; `release_tests_spec` also `chmod -R u+w`s the tree first.
- `ADDR` and `TARGET_NAME_PRE` are computed at make time by shelling out to `erl -noshell -s snmp_test_config ...` (requires working `erl` + hostname resolution in `PATH`); the ip1 form produces an Erlang list, the dotted form a plain string.
- `snmp_test_config.beam` is compiled in place (`EBIN = .`) with the `sys_pre_attributes` parse transform and an injected `app_vsn` attribute; the repo-root `.gitignore` excludes `*.beam`, this dir's `.gitignore` excludes exactly the 14 generated config files.
- The generated sys configs contain `%DIR%`-anchored paths (`agent/db`, `agent/log`, `agent/conf`, `manager/...`), so they are only valid where the sibling directories created by `release_tests_spec` exist.
- Different `TESTROOT` values give independent agent/manager trees (separate conf/db/log) for parallel or repeated test runs.

## Dependencies
- `make`, `perl`, `sed`; `erl` in `PATH`; includes `$(ERL_TOP)/make/target.mk`, `$(ERL_TOP)/make/$(TARGET)/otp.mk`, `$(ERL_TOP)/make/otp_release_targets.mk` and `../../vsn.mk` (`SNMP_VSN` → `VSN`).

## Routing Table
- `agent/` → agent-side config templates (`*.conf.src`): engine/address, community, context, notify, standard (sysContact/sysLocation/sysName), target_addr, target_params, usm, vacm.
- `manager/` → manager-side config templates (`*.conf.src`): `manager.conf` (port/address/engine_id) and `usm.conf`.
- `../test_config/Makefile` / `modules.mk` (this dir) → build & variable-substitution rules.
- `../` (snmp `test/`) → the snmp Common Test suites; they generate their own run-time configs (into the CT `priv_dir`) and do NOT consume this directory's output (see `../CONTEXT.md`).
