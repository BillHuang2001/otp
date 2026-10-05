# eunit — Sources

## Intent
Erlang sources of the `eunit` OTP application (EUnit, the lightweight unit-test framework), version 2.11 (`EUNIT_VSN`).
Only `eunit` and `eunit_surefire` are documented modules; the real public API is the `eunit` module plus the macros in `../include/eunit.hrl`.
Every other module here is internal (`-moduledoc false`) and may change without notice.
The app is built in place; `Makefile` produces `../ebin/*.beam` and the release payload `<app>/ebin`, `<app>/src`, `<app>/include`.

## API Surface
Documented modules (`-moduledoc` present, docs come from the attributes):
- `eunit` — user entry point. Official: `start/0`, `stop/0`, `test/1`, `test/2`. Experimental (own `-export` block, "may be removed or relocated"): `start/1`, `stop/1`, `test/3`, `submit/1..3`, `watch/1..3`, `watch_path/1..3`, `watch_regexp/1..3`, `watch_app/1..3`.
- `eunit_surefire` — documented listener emitting Maven/Bamboo Surefire XML; `start/0`, `start/1`.

Internal modules (all `-moduledoc false`):
- `eunit_server` — `start/1`, `stop/1`, `start_test/4`, `watch/3`, `watch_path/3`, `watch_regexp/3`, plus `main/1` (private, entered via the `?MODULE:main/1` trick on the server loop). Registered server process (default name `?SERVER` = `eunit_server`); holds the job table, the enqueue queue and the watched module/path/regexp sets.
- `eunit_serial` — `start/1`. One serializer process per test run; guarantees listeners see coherent, ordered `begin`/`end`/`cancel` events even when tests run in parallel.
- `eunit_proc` — `start/5`, `get_output/0`, `group_leader_process/1` (must stay exported), `-export_type(abort_exception/0)`. Builds the test process tree ("insulator" + child), applies timeouts, captures stdout, emits status messages to the supervisor.
- `eunit_test` — `run_testfun/1`, `mf_wrapper/2`, `enter_context/4`, `multi_setup/1`, `-export_type(wrapper_error/0)`. Runs test funs, prunes eunit frames from stacktraces, guarantees cleanup, folds a `{Tag,Setup[,Cleanup]}` list into a single setup/cleanup pair.
- `eunit_data` — `iter_init/3`, `iter_next/1`, `iter_prev/1`, `iter_id/1`, `enter_context/3`, `get_module_tests/2`, `parse_command_line/2` (exported only for unit testing), `-export_type` for the whole `tests()` grammar. Parses the symbolic test representation into `#test{}`/`#group{}` and provides the lazy iterator.
- `eunit_lib` — utilities: `dlist_next/1`, `uniq/1`, `is_string/1`, `command/1..3`, trie (`trie_new/0`, `trie_store/2`, `trie_match/2`), `split_node/1`, `consult_file/1`, `list_dir/1`, `is_not_test/1`, and the error-descriptor formatters `format_exception/1,2`, `format_error/1,2`, `format_stacktrace/1`, `format_exit_term/1`.
- `eunit_listener` — generic listener behaviour + `start/1,2`; owns the pass/fail/skip/cancel counters and drives the callbacks.
- `eunit_tty` — default text frontend listener; always installed and the only listener that replies with the final result (see Design Decisions).
- `eunit_autoexport` — parse transform exporting `*_test/0`, `*_test_/0`, `*_exported_/0` and `eunit_wrapper_/1`.
- `eunit_striptests` — parse transform removing those functions in non-test builds.
- `eunit_tests` — external self-tests of the `eunit.hrl` macros; compiled into the app like a normal source.

Headers / other files:
- `eunit_internal.hrl` — internal records `#test{}`, `#group{}`, `#context{}`, `?SERVER`, suffix constants (`_test`, `_test_`, `_exported_`, `_tests`), `DEFAULT_TEST_TIMEOUT` (5000), `DEFAULT_GROUP_TIMEOUT` (infinity), `DEFAULT_MODULE_WRAPPER_NAME`; also installed to `<app>/src` in releases. The public header lives in `../include/eunit.hrl`.
- `eunit.app.src` / `eunit.appup.src` — templates; `%VSN%` is substituted with `EUNIT_VSN` by `sed` at build time.

## Listener Protocol
`eunit_listener` spawns a process per `{Module, Options}` and calls, in its own process:
- `init(Options) -> SubState` — the listener must `receive {start, Reference}` inside `init` (all bundled listeners do) before returning.
- `handle_begin(Type, Data, SubState)` where `Type :: test | group`; `Data` includes `{id, Id}` plus `{desc,_},{source,_},{line,_}` (test) or `{desc,_},{spawn,_},{order,_}` (group).
- `handle_end(Type, Data, SubState)`; test `Data` has `{status, Status}` and `{time, ms},{output, binary()}`, group `Data` has `{size, Count}` as status.
- `handle_cancel(Type, Data, SubState)`; `Data` has `{reason, Reason}`.
- `terminate({ok, [{pass,N},{fail,N},{skip,N},{cancel,N}]} | {error, Reason}, SubState)` — return value is ignored; `eunit_listener` counts results itself from `handle_end`/`handle_cancel`.
`eunit_listener:start(Callback, Options)` spawns with `proplists:get_all_values(spawn, Options)` passed to `spawn_opt`.

Raw events fed to the serializer by `eunit_proc` (documented in `eunit_proc.erl` header):
- `{status, Id, {progress, 'begin', {test|group, Data}}}`
- `{status, Id, {progress, 'end', {Status, Data}}}` with `Status = ok | {error, Exception} | {skipped, Cause} | integer()` (integer = subitem count for a group)
- `{status, Id, {cancel, Descriptor}}` with `Descriptor = timeout | {timeout, #{stacktrace := _}} | {blame, Id} | {abort, Cause} | {exit, Reason} | {startup, Reason}`
`eunit_serial` re-orders/buffers these (trie of cancelled ids, `dict` of pending messages) so each listener sees at most one `end` XOR one `cancel` per `begin`, and no event with a cancelled id as prefix.

Plugging in a custom listener: pass `{report, {Module, Options}}` in the `test/2` option list; `eunit:listeners/1` collects all `report` values and starts each with `Mod:start(Opts)`. It also accepts bare pids/atoms, prepends `{eunit_tty, Options}` unconditionally, and `{event_log, File|true}` adds a debug logger writing raw events to `eunit-events.log`.

## Design Decisions
- Test task = insulator process + child process: the insulator traps exits, is the child's group leader, and is the only one that sends `{done, Reference, Pid}` to the parent, so the framework never relies on test code trapping exits or on links.
- `group_leader_process/1` must be spawned with `spawn/3` (explicit module+function): `?UNDER_EUNIT` inspects `process_info(group_leader(), current_function)` and a fun-spawned group leader would appear as `erlang:apply/2`.
- `{done, Reference, Pid}` messages are tagged with the job's unique reference so that arbitrary test-code messages cannot be pulled out of the framework's mailbox.
- `eunit_tty` is always a listener because `eunit:test/2` blocks on `{result, Reference, Result}`, which only `eunit_tty:sync_end/1` produces.
- `eunit_data` is a lazy iterator over a possibly improper "deep list" (`eunit_lib:dlist_next/1`) plus look-ahead folding in `group/1` (collapses `{String, Test}` into the test's `desc`, pushes timeout/order into inner groups when joinable).
- Watches (`watch*`, `watch_app`) just register targets in `eunit_server`; `watch_app/3` resolves `code:lib_dir(Name)/ebin` and delegates to `watch_path`.
- `submit/1..3` starts a test with a throw-away `devnull` supervisor so the caller gets an async job instead of a run-to-completion call.
- `eunit_data` `{node, N, Args, Tests}` builds a `{setup, ...}` that starts a `peer` node (via `peer:start_link`), optionally starting `net_kernel` when running on `nonode@nohost`; `Args` accept both a single command-line string and a list of strings (`parse_command_line/2` / `parse_peer_args/1`).

## Constraints
- Compiled with `-Werror` plus `+warn_deprecated_catch +warn_obsolete_bool_op +warn_unused_import` (see `Makefile`): any warning fails the build.
- `eunit_autoexport` is listed as `PARSE_TRANSFORM` and every object depends on it, so it is always built first; keep it that way when adding sources.
- `eunit.app.src` `modules` is a literal list — adding/removing a module here requires updating that list.
- The `{status, Id, Info}` / event message shapes are an implicit contract between `eunit_proc`, `eunit_serial` and `eunit_listener`; changing them means touching all three.
- `eunit` depends only on `kernel` + `stdlib`; do not introduce `tools`/`compiler` (or any other) runtime dependency.
- Keep `-doc`/`-moduledoc` attributes in `eunit.erl` and `eunit_surefire.erl` consistent with `../doc/` (ExDoc renders the module attributes; `../doc/docs.exs` only lists `guides/chapter.md` as an extra).
- Do not add `-moduledoc` to internal modules to "expose" types: despite `-export_type` in `eunit_data`/`eunit_lib`/`eunit_test`/`eunit_proc`, those types are not part of the documented API.

## Known Issues
- The module-watch / auto-test-on-load feature is inert: `eunit_server:main/1` handles `{code_monitor, {loaded, M, _Time}}` messages, but the `code_monitor:monitor/1` call that would produce them is commented out and no `code_monitor` module exists anywhere in the tree, so `watch/watch_path/watch_regexp/watch_app` only register targets and never trigger a run.
- `-ifdef(TEST)` self-tests in `eunit_data.erl` reference the relative path `examples/tests.txt`, so running them requires cwd = the app root (`lib/eunit`).
- `eunit_data:extract_module_tests/3` auto-includes a sibling `Name_tests` module unless `{exact_execution, true}` is set, which can surprise when iterating a directory that holds both plain and `_tests` modules.

## Notes for Agents
- `eunit_internal.hrl` records are shared by `eunit_data`, `eunit_proc`, `eunit_test`, `eunit_serial` and `eunit_server`; changing a record field ripples across all of them.
- The `-ifdef(TEST)` blocks in `eunit_proc.erl`, `eunit_test.erl`, `eunit_data.erl` and all of `eunit_tests.erl` ARE compiled in normal builds, because `../include/eunit.hrl` defines `TEST` unless `NOTEST` is set.
- `?capturedOutput` / `?UNDER_EUNIT` macros (in `../include/eunit.hrl`) are implemented by `eunit_proc`'s per-test group leader and `eunit_proc:get_output/0` — do not move the group-leader logic.
- `eunit_lib:command/1..3` is the generic external-process helper (used by tests and `eunit_lib` itself); `split_node/1` splits `name@host`.
- `eunit_server` `main/1` is invoked through `?MODULE:main(St)` (not a plain local call) even though it looks like a normal function.

## Test Strategy
- Self-tests: `eunit_tests.erl` (external tests of the `eunit.hrl` macros: `?UNDER_EUNIT`, `?LET`, `?IF`, `?MATCHES`, `?capturedOutput`) plus the `-ifdef(TEST)` suites inside `eunit_proc.erl` (io/get_output), `eunit_test.erl` (assertion/macro/error-wrapper tests) and `eunit_data.erl` (representation/generator/node tests incl. the `{node, test@localhost, ...}` case).
- Integration: `../test/eunit_SUITE.erl` (Common Test) and `../test/eunit_test_listener.erl`, a custom listener exercising the `{report, {Mod, Opts}}` plug-in path.
- Parser edge cases for `parse_command_line/2` are covered by exporting it "for unit testing".

## Routing Table
- No subdirectories — this is a flat source directory; all modules listed under API Surface above live here.
- `../include/eunit.hrl` — public header (macros, `TEST`/`EUNIT` switches) — read-only from this node, escalate changes to `../`.
- `../test/` — Common Test suites and helper listeners — read-only from this node.
- `../doc/` — ExDoc config and the `chapter.md` overview/doc source — read-only from this node.
