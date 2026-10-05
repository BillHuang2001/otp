# Common Test — Internal Developer Notes

## Intent
Non-published design notes about Common Test internals, written for OTP maintainers rather than users.
`ct_notes.md` collects Mermaid diagrams and tables explaining how a CT run executes and where output/measurements come from.

## API Surface
- `ct_notes.md` — the single document. It covers:
  - Time measurement categories: `TestTime` (suite/users code) vs `FrameworkTime` (hooks) vs `ElapsedTime`, and the fact that the `timetrap` option operates on `TestTime` only; a flow diagram maps the `pre/post_init_per_suite`, `init_per_suite`, testcase and `end_per_suite` phases onto those categories.
  - A call-flow diagram of the testcase execution path inside `test_server`: `run_test_case_apply/1,6` → `run_test_case_eval_fun/9` → `run_test_case_eval/9` → `do_init_tc_call/4` / `run_test_case_eval1/6` / `do_end_tc_call/4`, plus `ct_util mark_process/0` and `run_test_case_msgloop/1`.
  - Process structure: the group-leader "job" process that captures output vs the "case" process that runs the suite (which communicate by messages).
  - A mind-map of `test_server` interfaces and initialisation (`target_info`, `memory_checker`).
  - A table of which log row `ct:comment/1,2` output lands in for every `ct_hooks` callback, depending on whether the hook was installed via `ct_run`/`ct:run_test`/test spec or from `suite/0`/`init_per_suite/1`/`init_per_group/2`.

## Constraints
- These notes are **not** part of the published documentation: the ExDoc extras wildcard covers `internal_docs/*.md` (plural), while this directory is `internal_doc/` (singular), so `ct_notes.md` is never shipped in the docs.
- Diagrams are Mermaid inside markdown — keep the syntax valid when editing.

## Notes for Agents
- The most valuable part for code archaeology is the hook/`ct:comment` mapping table and the `test_server` testcase call-flow chain, which answer "where does this log line come from?" without reading `../src/test_server.erl` end to end.
- Some entries are open questions left by the authors (e.g. why `call_end_conf` is not called when `init/end_per_testcase` are undefined) — they are not bugs to fix here.

## See Also
- `../src/test_server.erl`, `../src/test_server_ctrl.erl`, `../src/ct_hooks.erl` — the code these notes describe (read-only siblings).
- `../doc/` — the published documentation (read-only sibling).

## Routing Table
Flat directory, no subdirectories.
- `ct_notes.md` → internal design notes: time categories, test_server call flow, process structure, hook/`ct:comment` mapping
