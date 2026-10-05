# SNMP Examples

## Intent
Worked examples for the Erlang/OTP **snmp** application, published into the release documentation tree rather than linked into any runtime application.
They back the user-guide chapters "Agent Implementation Example" and "Manager Implementation Example" (`../doc/guides/snmp_impl_example_agent.md`, `../doc/guides/snmp_impl_example_manager.md`) and are shipped so users can copy a working starting point.
This is example/teaching code — not a library, not part of `snmp`'s `ebin/`.

## API Surface
- `Makefile` — coordinator; includes `$(ERL_TOP)/make/target.mk`, `make/$(TARGET)/otp.mk`, `subdirs.mk`, then `make/otp_subdir.mk` (recurses into `SUB_DIRECTORIES`).
- `subdirs.mk` — `SUB_DIRECTORIES = ex1 ex2`; the only place the built example subdirs are listed.
- `ex1/` — the example SNMP **agent** (`ex1.erl`) plus the `EX1-MIB` and `EX1-MIBv2` MIB + instrumentation (`*.funcs`) files.
- `ex2/` — the example SNMP **manager** (`snmp_ex2_manager.erl`) plus `snmp_ex2_simple_standard_test.erl` (example agent-testing driver).
- Each subdir's `Makefile` installs its sources and compiled artifacts into `$(RELEASE_PATH)/lib/snmp-$(VSN)/doc/examples/<subdir>` via its `release_docs_spec` target.
- Per-subdir `VSN` is taken from `../vsn.mk` (`SNMP_VSN`).

## Constraints
- These directories are documentation examples; keep the sources copy-pasteable and self-contained, and keep them consistent with the guide text (`../doc/guides/`).
- Never hand-edit or commit build outputs (`*.beam`, `EX1-MIB.bin`) — they are per-subdir build artifacts.
- Adding a built example subdirectory requires adding it to `subdirs.mk` (`SUB_DIRECTORIES`); a subdirectory not listed there is present but not built.

## Known Issues
- `mcli/` exists but is not in `SUB_DIRECTORIES`, so it is never built; it currently contains only a tracked empty `.gitignore` (no sources).

## Dependencies
- Build-time: `make/target.mk`, `make/$(TARGET)/otp.mk`, `make/otp_subdir.mk`, `make/otp_release_targets.mk`, `../vsn.mk`, the built `snmp` MIB compiler, and `stdlib`.
- The examples compile with the `sys_pre_attributes` parse transform and `-I../include` (a directory that does not exist here — harmless no-op; the examples use no `.hrl`).

## Notes for Agents
- `-Dversion="$(VSN)$(PRE_VSN)"` is passed by both subdir Makefiles but never referenced by the example sources.
- The examples are not exercised by the repo-wide Dialyzer run (`scripts/run-dialyzer` is per-application); `ex2/` has its own local Dialyzer targets.
- SNMP feature testing lives in `../test/`, not here.

## Routing Table
| Dir | Description |
| --- | --- |
| `ex1/` | Example SNMP agent: `ex1.erl` + `EX1-MIB`/`EX1-MIBv2` MIB and `.funcs` instrumentation files |
| `ex2/` | Example SNMP manager: `snmp_ex2_manager.erl` + `snmp_ex2_simple_standard_test.erl` (with local Dialyzer targets) |
| `mcli/` | Present but unbuilt/empty (only a tracked `.gitignore`) — not in `subdirs.mk` |

## See Also
- `../doc/guides/snmp_impl_example_agent.md`, `../doc/guides/snmp_impl_example_manager.md` — the guides these examples illustrate.
- `../CONTEXT.md` — the `snmp` application node; `../test/` — the real SNMP test suites (read-only sibling).
