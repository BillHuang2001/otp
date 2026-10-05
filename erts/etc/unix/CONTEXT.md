# erts/etc/unix — Unix launchers, `run_erl`/`to_erl` daemons, ETP debug toolbox

## Intent
Unix-specific "etc" material for ERTS: the `/bin/sh` templates for the `erl`/`start`/`start_erl` commands and the release `Install` script, the C sources of the `run_erl`/`to_erl` logging-pty daemons (plus their `safe_string` helpers), the Erlang-Term-Parser (ETP) gdb/lldb/rr debug toolbox for core dumps, and a few standalone auxiliary scripts.
This directory contains **sources/templates only** — almost nothing is built here; the C programs are compiled and everything is installed by the sibling `../common/Makefile.in` (`../Makefile` picks `unix` as a subdirectory when `TARGET != win32`).

## API Surface
Only one generated artifact is produced in this directory: `etp-commands` (git-ignored via `erts/.gitignore:/etc/unix/etp-commands`).

| source | generated / installed as | substitutions | performed by |
|---|---|---|---|
| `etp-commands.in` | `erts/etc/unix/etp-commands` | `@ERL_TOP@`, `@TARGET@` | this dir's `Makefile` target `etc` |
| `erl.src.src` | `$(ERL_TOP)/bin/erl` (in-tree) | `%FINAL_ROOTDIR%`, `erts-.*/bin`→`bin/$(TARGET)`, `EMU=.*`→`EMU=beam$(TYPEMARKER)`, `%DYN_ERL_PATH%` (no-op), `%DYN_ROOTDIR_BASE_EXT%` (no-op) | `erts/Makefile` target `local_setup` |
| `erl.src.src` | `$(RELEASE_PATH)/erts-$(VSN)/bin/erl.src` | `%EMULATOR%`, `%EMULATOR_NUMBER%`, `%VSN%` | `../common/Makefile.in` rule `erl.src` |
| `cerl.src` | `$(ERL_TOP)/bin/cerl` (in-tree only) | `%SRC_ROOTDIR%`, `%TARGET%`, `%VSN%`, `%DYN_ERL_PATH%` (no-op) | `erts/Makefile` target `local_setup` |
| `Install.src` | `Install` → `$(RELEASE_PATH)/Install` (`INSTALL_TOP`) | `%I_VSN%`, `%EMULATOR%`, `%EMULATOR_NUMBER%`, `%I_SYSTEM_VSN%` | `../common/Makefile.in` rule `Install` |
| `start.src`, `start_erl.src` | `$(RELEASE_PATH)/erts-$(VSN)/bin/{start,start_erl}.src` (`INSTALL_EMBEDDED_DATA`) | none at build; substituted *at install time* by the generated `Install` | `../common/Makefile.in` |
| `run_erl.c`, `to_erl.c`, `safe_string.c`, `safe_string.h`, `run_erl.h` | `$(BINDIR)/run_erl`, `$(BINDIR)/to_erl` (`INSTALL_EMBEDDED_PROGS`) | — | `../common/Makefile.in` |
| `format_man_pages` | `$(RELEASE_PATH)/misc/format_man_pages` (`INSTALL_MISC`) | — | `../common/Makefile.in` |
| `setuid_socket_wrap.c` | `$(RELEASE_PATH)/erts-$(VSN)/src/setuid_socket_wrap.c` (`INSTALL_SRC`, "delivered as an example") | — | `../common/Makefile.in` |
| `etp.py`, `etp-thr.py`, `etp-rr-run-until-beam.py`, `etp_commands.erl`, `etp_commands.mk` | not installed | — | — |
| `gcov-gen-html` | not installed, not referenced by any Makefile | — | — |
| `README`, `RELNOTES` | not installed | — | — |

Two-stage shell generation contract (important):
1. **Build stage**: `Install` (from `Install.src`) is baked with `%I_VSN%`/`%EMULATOR%`/`%EMULATOR_NUMBER%`/`%I_SYSTEM_VSN%` and shipped in the release root; `erl.src` (from `erl.src.src`) is shipped as `erts-$(VSN)/bin/erl.src` with `%EMULATOR%`/`%EMULATOR_NUMBER%`/`%VSN%` baked in.
2. **Install stage** (run by the user against an untarred release): `Install <ERL_ROOT>` seds `%FINAL_ROOTDIR%` into `erl` and `start`, symlinks `bin/epmd`, copies the embedded programs, writes `releases/start_erl.data` and `releases/RELEASES`, copies `start.boot`/`start.script` from `releases/%I_SYSTEM_VSN%/`, and calls `misc/format_man_pages` when `$ERL_ROOT/man` exists.

Runtime contracts:
- `erl` (`erl.src.src`): `find_rootdir` walks parent dirs from `$0` looking for a `erts-<vsn>` directory, falling back to the baked `%FINAL_ROOTDIR%`; `$ERL_ROOTDIR` overrides. Sets/exports `ROOTDIR`, `BINDIR=$ROOTDIR/erts-%VSN%/bin` (in-tree: `bin/$(TARGET)`), `EMU=beam<marker>`, `PROGNAME`, then `exec "$BINDIR/erlexec"`.
- `cerl` (`cerl.src`): dev-only. Defaults `ROOTDIR` from `%SRC_ROOTDIR%`, `BINDIR=$ROOTDIR/bin/<config.guess>` (fallback `$ROOTDIR/bin/%TARGET%`), and gets the emulator name from `erlexec -emu_name_exit [-emu_type T] [-emu_flavor F]`. Options: `-debug -gdb -rgdb -lldb -break F -dump CORE -core CORE -rcore CORE -gcov -valgrind -asan -lcnt -icount -gprof -frmptr -rr [replay|eplay|ps] -nox -display D -rootdir DIR -target TGT`; it can download and build the external gdb-tools.
- `start` (`start.src`): embedded-system launcher. Picks `LOGDIR=/var/log` if writable, else `$XDG_STATE_HOME/erlang` (or `$HOME/.local/state/erlang`), then runs `$ROOTDIR/bin/run_erl -daemon /tmp/ $LOGDIR "exec $ROOTDIR/bin/start_erl $ROOTDIR $RELDIR $START_ERL_DATA"`.
- `start_erl` (`start_erl.src`): called by `run_erl`; reads `<erts_vsn> <vsn>` from a `start_erl.data` file and `exec`s `$BINDIR/erlexec -boot $RELDIR/$VSN/start -config $RELDIR/$VSN/sys`.
- `run_erl <pipe_name|pipe_dir/> <log_dir> "<command>"`: sets up a master pty for the child, two FIFOs `<pipe>.r`/`<pipe>.w` (perm 0600; a `pipe_dir/` argument makes it allocate the next free `erlang.pipe.N`), splits child output into stdout + rotated logs `<log_dir>/erlang.log.<1..N>` (perm 0664), and writes a status file `<log_dir>/run_erl.log`. Tuning env vars: `RUN_ERL_LOG_GENERATIONS` (2–1000, default 5), `RUN_ERL_LOG_MAXSIZE` (default 100000), `RUN_ERL_LOG_ALIVE_MINUTES` (default 15), `RUN_ERL_LOG_ACTIVITY_MINUTES` (default 5), `RUN_ERL_LOG_ALIVE_FORMAT` (default `%a %b %e %T %Z %Y`), `RUN_ERL_LOG_ALIVE_IN_UTC`. Options: `-daemon` (daemonize), `-sleepy-child` (test-only).
- `to_erl [-h|-F] [pipe_name|pipe_dir/]`: client that attaches stdin/stdout to the FIFO pair; `-F` forces attaching when the pipe is locked by another `to_erl`. Protocol version handshake with `run_erl` via `run_erl.h` (`RUN_ERL_HI_VER 1` / `RUN_ERL_LO_VER 0`).
- ETP: `etp-commands` is a gdb command file (commands prefixed `etp-`/`etpf-`, "erts-term-print" / "flat"; help via `etp-help`) sourced by `cerl -gdb`/`-rgdb`/`-egdb`/`-dump`/`-rr replay`; `etp.py` is the lldb counterpart imported by `cerl -lldb` (`command script import`); `etp-thr.py` names BEAM threads in gdb; `etp-rr-run-until-beam.py` is an rr replay helper driven by `$etp_beam_executable`/`$etp_rr_run_until_beam`. Some ETP commands shell out to the Erlang support module `etp_commands.erl`, which needs `erl` and `erlc` in `PATH` and reads an `ETP_DATA` file (`etp-commands.bin`).

## Constraints
- Templates use `%NAME%` placeholders; `etp-commands.in` is the exception and uses `@ERL_TOP@`/`@TARGET@`. Never edit generated `etp-commands` (and never assume `erts/etc/common/Install` or `erts/etc/common/erl.src` are sources — they are generated from `Install.src` / `erl.src.src`).
- Everything here is `/bin/sh` (POSIX): no bashisms, no `[[ ]]`, no arrays. Android builds rewrite `/bin/sh`→`/system/bin/sh` in the generated `erl`, `cerl`, `start` and `start_erl` variants.
- `run_erl.c`/`to_erl.c` must do all string handling through `safe_string.{c,h}` (`strn_cpy`, `strn_cat`, `strn_catf`, `sn_printf`, `vsn_printf`, `find_str`), which abort the process on overflow; fixed `FILENAME_BUFSIZ`=250 paths plus the `sn_printf`-based status line are relied upon. Both must include `run_erl.h` so their protocol versions stay in sync.
- `setuid_socket_wrap.c` is a privileged-socket helper copied into installed releases as an *example* (`erl_interface`/`inets` use the same code); changes here are the upstream for `lib/inets/test/inets_socketwrap_SUITE_data/setuid_socket_wrap.c`.
- The C sources include `config.h` under `HAVE_CONFIG_H` (the `config.h` of the `erts` build); nothing here compiles without a configured tree.

## Design Decisions
- `erl` is a shell script, not C: `ROOTDIR` must be derivable from the path of the invoked command at runtime (relocatable installs, `$ERL_ROOTDIR` overrides, symlinked/copied releases) without recompiling, and `%FINAL_ROOTDIR%`/Android-`/bin/sh` tweaks are trivial sed edits.
- `erlexec` (a real program, `../common/erlexec.c`) is kept separate from `erl`: it consumes the exported `ROOTDIR`/`BINDIR`/`EMU`/`PROGNAME`, synthesizes emulator arguments, and is the single place that also serves `cerl` (`-emu_name_exit`, `-emu_args_exit`) and Windows.
- The root-dir discovery walk (`absolute_parent` + `find_rootdir`) exists so a copied/moved release still finds its `erts-<vsn>` directory, silently falling back to the build-time root if the walk fails.
- `run_erl` puts the child on a master/slave pty so the VM believes it has a terminal; the two-FIFO design lets an operator attach to a running embedded system (`to_erl`), detach, and re-attach later without stopping the VM, while the same output is also written to rotating log files.
- Log rotation is size- *and* activity-based with a periodic "alive" heartbeat line, so a silent system still produces visible progress in `erlang.log.N`.
- ETP is distributed as a generated gdb script (`etp-commands`) rather than a gdb Python module because the large body of commands predates and outlives gdb's Python API; `etp.py` provides the newer lldb implementation.

## Known Issues
- `README` and `RELNOTES` are stale 1990s texts (SunOS/Interactive UNIX, `%VERSION%` substitution, "bundles", Erlang 4.3.1) — do **not** treat them as current documentation.
- `etp_commands.mk` is not included by any Makefile in the repository; `MAKE_AND_EXECUTE_ETP_COMMANDS`/`ETP_DATA` are conventions for out-of-tree use of the ETP heap/overlap dump analysis.
- The `etp-` process-flag decoder in `etp-commands.in` mirrors the flags in `../../emulator/beam/erl_process.h`; that header asks to update `etp-proc-state-int` there when flags change. `erl_etp.c` is the emulator-side producer of the data these commands consume.
- `format_man_pages` only runs when the installed release has a `man/` directory and hardcodes SunOS/Linux(old)/Darwin(old)/OpenBSD target detection; it is vestigial relative to the current HTML/EPUB doc builds.
- `gcov-gen-html` is invoked manually (see `HOWTO/DEVELOPMENT.md`), expects `lcov`'s `geninfo`/`genhtml` in `PATH`, and is not installed into releases.
- `etp-commands.in` is ~158 KB / 5665 lines — grep it, do not read it whole.
- `%DYN_ERL_PATH%` is still sed-substituted by `erts/Makefile` but the placeholder no longer exists in `erl.src.src`/`cerl.src` (leftover no-op).
- `erts/Makefile` is tracked yet matched by `erts/.gitignore:/Makefile`, so ripgrep skips it by default — use `rg --no-ignore` when searching for `local_setup`-related substitutions.

## Notes for Agents
- `erts/etc/unix/gdb-tools/` does **not** exist in the repo: it is cloned by `./otp_build download_gdb_tools` (repo pinned in `make/gdb_tools_vsn`) and built from `../common`; `../common/Makefile.in` optionally `-include`s `../unix/gdb-tools/jit-reader.mk`.
- To regenerate the gdb toolbox after editing `etp-commands.in`, just run make in this directory (or `make` from `erts/`).
- `start.src`/`start_erl.src` are *not* what runs in a source-tree build; for an in-tree embedded test the release layout created by `Install` is what uses them (see `system/doc/system_principles/create_target.md`).
- Debug/test docs worth reading before touching this code: `../../doc/references/run_erl_cmd.md`, `start_cmd.md`, `start_erl_cmd.md`; `../../../system/doc/embedded/embedded.md`; `../../../system/doc/tutorial/debugging.md`; `../../../HOWTO/DEVELOPMENT.md` (sections `cerl`, gcov).
- `../common/` is where the Makefile rules, the C launchers (`erlexec`, `erlc`, `escript`, `heart`, `inet_gethost`) and the optional gdb-tools build live; nothing in this directory is built without it.

## Dependencies
- POSIX/Unix: `mkfifo`, pty (`openpty`/`forkpty`, `HAVE_WORKING_POSIX_OPENPT` from `config.h`), `termios`/`ioctl`, `syslog`, `select`.
- `etp-commands`/`etp.py` require gdb (unix), or lldb; rr and emacs are optional (for `-rr`, `-gdb`); the Erlang support module needs `erl` and `erlc` in `PATH`.
- `gcov-gen-html` requires perl plus `lcov` (`geninfo`, `genhtml`).
- `format_man_pages` requires the system's `nroff`/`troff`-family tools for the target platform.

## Test Strategy
- `erts/test/run_erl_SUITE` is the only automated coverage for this directory: it drives `run_erl` + `to_erl` (`run_erl_SUITE_data/run_erl_test.pl` as the child, `to_erl <pipe>` as a `port`) in cases `basic`, `sleepy_child`, `heavy`, `heavier`, `defunct`; all cases `skip` on non-Unix. It is part of `erts/test/Makefile` `MODULES`, hence included by `system.spec` (`{suites,"../system_test",all}`) and the root `TEST_DIRS` run.
- There is **no** automated test for `etp-commands.in`, `etp.py`, `etp-thr.py` or `etp-rr-run-until-beam.py` — they are validated manually through `cerl -gdb|-rgdb|-lldb|-dump|-rr [-core <file>]`. The Erlang half can be exercised standalone with `erl -noshell -run etp_commands file <etp-data-file>` (see `etp_commands.mk`).
- `start`/`start_erl`/`Install` are covered indirectly by release-install tests (`lib/sasl` installer tests, `erts/test/upgrade_SUITE`, `lib/reltool` examples); note `erts/test/upgrade_SUITE_data/start.src` is a test fixture copy, not this file.
- `cerl -asan`/`-valgrind` wire in the suppression files from `../../emulator/asan/` and `../../emulator/valgrind/`, so those flavors are the intended way to exercise this C code under sanitizers.

## Routing Table
No subdirectories (nothing to delegate to). Contents by area:
- Launcher templates → `erl.src.src`, `start.src`, `start_erl.src`, `Install.src` (install-time substitution rules live in `../common/Makefile.in` and `erts/Makefile` `local_setup`)
- Dev launcher / debug runner → `cerl.src`
- `run_erl`/`to_erl` daemons → `run_erl.c`, `run_erl.h`, `to_erl.c`, `safe_string.c`, `safe_string.h`
- ETP toolbox → `etp-commands.in` (→ generated `etp-commands`), `etp.py`, `etp-thr.py`, `etp-rr-run-until-beam.py`, `etp_commands.erl`, `etp_commands.mk`; emulator side: `../../emulator/beam/erl_etp.c`
- Auxiliary scripts → `format_man_pages`, `gcov-gen-html`
- Privileged-socket example → `setuid_socket_wrap.c` (duplicate in `../../../lib/inets/test/inets_socketwrap_SUITE_data/` — sibling, read-only, escalate writes to the parent)
- Stale legacy prose → `README`, `RELNOTES` (do not rely on them)

Sibling cross-references (read-only from here): `../common/` (build + install of everything above, `erlexec`/`erlc`/`escript`/`heart`/`inet_gethost`, optional gdb-tools build), `../win32/` (Windows counterparts: `Install.c`, `start_erl.c`, `erl.c`), `../../emulator/beam/` (VM internals the ETP commands decode), `../../test/run_erl_SUITE.erl` + `../../test/run_erl_SUITE_data/` (the only test suite).

## See Also
- `../CONTEXT.md` (erts/etc) and `../../CONTEXT.md` (erts) for how this directory is configured and released.
- `../common/Makefile.in` — authoritative build/install rules and `-include ../unix/gdb-tools/jit-reader.mk`.
- `erts/Makefile` (`local_setup`) — in-tree generation of `bin/erl` and `bin/cerl`.
- `../../doc/references/run_erl_cmd.md`, `start_cmd.md`, `start_erl_cmd.md` — user-facing documentation of the scripts generated here.
