# erl_interface Programs (`prog/`)

## Intent
Contains the two hand-written C programs that `../Makefile.in` compiles on top of the `libei` library: `erl_call.c` (the `erl_call` command-line utility) and `ei_fake_prog.c` (a never-executed header/link smoke test).
These are *programs*, not library sources — hence the `prog/` directory and the `PROG_CFLAGS`/`-Iglobal` build flags.
Only the `ei` API is used; the legacy `erl_*` / `liberl_interface` API no longer exists in this tree.

## API Surface
`erl_call.c` — one self-contained file, `int main(int argc, char *argv[])`, no exported header.
Its CLI is the app's external API (documented in `../../doc/references/erl_call_cmd.md`):

Single-letter switches (must be exactly 2 chars, `-<x>`):
- `-s` — start a distributed Erlang node if the node is not running.
- `-q` — halt the target node (overrides `-s`); also makes a failed connect exit 0.
- `-v` — verbose output on stderr.
- `-d` — debug output redirected to `$HOME/.erl_start.out.<alive>`.
- `-r` — random hidden client-node name (`c<time(NULL)%997>` instead of `c17`).
- `-R` — dynamic client-node name (hidden name left NULL; `ei_connect_xinit` with NULL nodename).
- `-e` — evaluate the Erlang expressions read from stdin via `erl_eval:eval_str`.
- `-m` — read an Erlang module from stdin, write and compile it on the remote node.
- `-c Cookie` — cookie for the connection.
- `-n Node` — node name; sets `use_long_name` (i.e. `-name`/FQDN semantics, *not* short names).
- `-h HiddenName` — name of the client (hidden) node.
- `-x ErlScript` — alternative Erlang start script used by `-s` (default `erl`).
- `-a [Mod [Fun [Args]]]` — apply request; missing parts default to `start` / `[]`.
- `-?` — print usage (exits 0).

Word switches:
- `-sname Node` (short name), `-name Node` (long name), `-n Node` (long name).
- `-address [HOSTNAME:]PORT` — connect straight to an address/port (no epmd lookup); mutually exclusive with `-s`, `-n`, `-name`, `-sname`.
- `-timeout Secs` — accepted range 1..(2^20-1); installed as `alarm()` on Unix or a timer thread that calls `exit(1)` on win32.
- `-fetch_stdout` — run the `-a`/`-e` code in a process whose group leader forwards stdout back and print it locally.
- `-no_result_term` — suppress printing the result term.
- `-__uh_test__` — undocumented test hook forcing the "cannot resolve own hostname" localhost fallback path.

Request/response behaviour:
- `-a` builds a `{Mod,Fun,Args}` call by parsing `Args` with `ei_x_format_wo_ver`, then `ei_rpc` (or the internal `rpc_print_node_stdout`) against the remote node.
- Result terms are printed with `ei_print_term(stdout, reply.buff, &i)` unless `-no_result_term` was given.
- `-fetch_stdout` uses `ei_xrpc_to(..., EI_RPC_FETCH_STDOUT)` and loops over `ei_rpc_from`, printing `{rex_stdout, Binary}` payloads until the final `{rex, Result}` arrives.
- `-q` sends `ei_rpc(..., "erlang", "halt", [])` and exits 0.
- `-m` encodes `[Fname, ModuleBin]` for `file:write_file`, then `c:c(Mod)`; the compiler result is not inspected.
- Exit status: 0 normally; 1 for usage argument/illegal-argument errors, hostname/nodename resolution failures, connect/start failures and eval failures; -1 (255) when the `-a` apply or its argument parsing fails.

Node-name resolution:
- The target hostname comes from `-address` (or is the local host if `-address` has no host part / the node has no `@`).
- Short-node mode truncates the hostname at the first `.`; long-name mode keeps the FQDN.
- If the local hostname cannot be resolved (`ei_gethostbyname`), it falls back to `localhost` and skips canonical name resolution.

`-s` node start (`do_connect` + `erl_start_sys`, `exec_erlang` / `spawn_erlang_epmd`):
- First tries `ei_connect`; on failure starts `erl -noinput (-name|-sname) <alive> -s erl_reply reply <myip> <port> <pid>` (plus `-setcookie <cookie>`) via `sh -c` + `execvp` on Unix (`fork`) or `CreateProcess` on win32.
- Binds a temporary socket and waits for the `erl_reply` sign-on message for a hardcoded 10 s; on timeout the child is killed (SIGINT, then SIGKILL) / `TerminateProcess` is used on win32.
- The started node must provide `erl_reply` (see `../../../kernel/src/erl_reply.erl`).

`ei_fake_prog.c`:
- A "fake program" that is compiled and linked but never run; its `main/0` references every documented `ei`/`ei_connect` type, function and constant (`ei_connect*`, `ei_publish`/`ei_listen`, encode/decode, `ei_x_*`, `ei_reg_*`, `ei_rpc*`, error codes, `MAXATOMLEN`, ...) so the compiler/linker validates that `../../include/ei.h` plus the built library are complete.
- `ei_decode_bignum`/`ei_encode_bignum`/`ei_x_encode_bignum` are only referenced under `HAVE_GMP_H && HAVE_LIBGMP`; the undocumented block is guarded by `USE_EI_UNDOCUMENTED`.

## Constraints
- `prog/` sources `#include "config.h"` directly — an explicit exception to the library rule that all sources must include `eidef.h` first.
- Compiled with `PROG_CFLAGS` (`@CFLAGS@ $(WARNFLAGS) $(INCFLAGS) $(TYPE_FLAGS) -Iglobal`).
- `$(ERL_CALL)` = `$(BINDIR)/erl_call$(EXE)` = `lib/erl_interface/bin/$(TARGET)/erl_call`, linked against the just-built library (`-lei` and `-MD/-MT/-MDD` flag variants; `-lei_st`/`-lei_r` variants in the THR/multi-threaded branches). `bin/` is git-ignored.
- The `check` target must be run from `src/` (`make check`); it only builds the fake programs, it does not run them.
- Anything kept in `ei_fake_prog.c` must stay in sync with `ei.h` and the documentation.

## Known Issues
- `prog/erl_fake_prog.c` does not exist (removed together with the `erl_*` API), yet `../Makefile.in` still lists `erl_fake_prog_{st,mt,md,mdd}[_cxx]` in `FAKE_TARGETS` and has rules depending on `prog/erl_fake_prog.c`; those rules link `-lerl_interface`/`-lerl_interface_st`/`-lerl_interface_r`, libraries no longer built, so `make check` in `src/` cannot satisfy those targets.
- `../README.internal` still instructs keeping `prog/erl_fake_prog.c` in sync and notes the check "(FIXME this check should be rewritten to a test case)"; nothing in CI runs `make check`.
- `ERL_START_TIME` (10000 ms) is defined but unused — the wait-for-erl_reply timeout is hardcoded to 10 s and is independent of `-timeout`.
- The usage text is inaccurate: `-d` is described as writing to `~/.erl_call.out.<Nodename>` while the actual file is `$HOME/.erl_start.out.<alive>`; the usage line omits `-R`, `-e` output details and the `-address` incompatibilities.
- `usage()` exits with status 0, so invalid option combinations invoked through it (e.g. `-m` with `-e`, `-address` with `-s`/`-n`) look like success; only `usage_arg`/`usage_error` exit 1.
- `-a` RPC failure exits with -1 and prints no error message (FIXME in the code); `-m` ignores compiler errors.
- `-m` detects the module name with `strstr(buf, "-module(")`, i.e. only the first occurrence in the stdin text.

## Dependencies
- Link time: the `libei` library and its headers (`../../include/ei.h`, `ei_connect.h`); `prog/` also needs `-Iglobal`.
- Run time: a reachable distributed node plus `epmd` for the `-n`/`-name`/`-sname` paths; `-address HOST:PORT` connects directly without epmd; `-s` additionally requires an `erl` (or `-x`) executable and `erl_reply` on the started node.

## Test Strategy
- `../../test/erl_call_SUITE.erl` (Common Test) is the functional test: it locates the binary at `code:lib_dir(erl_interface)/bin/erl_call[.exe]` or `bin/<system_architecture>/erl_call` and drives it with `open_port({spawn_executable, ...})`.
- Suites: `smoke`, `random_cnode_name`, `test_connect_to_host_port`, `unresolvable_hostname` (exercises the `-__uh_test__` fallback path), `timeout`, `test_fetch_stdout`.
- Nothing tests the fake-program build; `ei_fake_prog.c` is only compiled/linked by `make check` in `src/`, which is not part of CI.

## See Also
- `../Makefile.in` — build rules for `erl_call`, the `check` target and `FAKE_TARGETS`; declares `ERLCALL = prog/erl_call.c`.
- `../README.internal` — `src/` layout and the "keep documentation/headers/fake prog in sync" rule.
- `../connect/`, `../decode/`, `../encode/`, `../misc/`, `../epmd/`, `../global/` — the `libei` sources this program consumes.
- `../../include/ei.h`, `../../include/ei_connect.h` — the headers `ei_fake_prog.c` validates.
- `../../test/erl_call_SUITE.erl` — functional test suite for the binary.
- `../../doc/references/erl_call_cmd.md` — user-facing reference for the CLI.
- `../../../../erts/etc/common/Makefile.in` — copies `lib/erl_interface/bin/$(TARGET)/erl_call` into the release `bin` dir.
- `../../../kernel/src/erl_reply.erl` — the module used to signal readiness when `-s` starts a node.
