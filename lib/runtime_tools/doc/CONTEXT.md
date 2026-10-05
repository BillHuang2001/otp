# runtime_tools — Documentation Sources

## Intent
Holds the documentation *extras* (user's guides, application chapter, release notes) for the `runtime_tools` OTP application.
Built with ExDoc driven by `$(ERL_TOP)/make/doc.mk`; output (HTML + man pages) is generated, never stored here.
There is NO `doc/src/` in this app: the module/function reference documentation lives as `-moduledoc`/`-doc` attributes inside `../src/*.erl` (see Constraints).

## API Surface
- `Makefile` — includes `$(ERL_TOP)/make/target.mk`, `$(ERL_TOP)/make/$(TARGET)/otp.mk`, `../vsn.mk` (`VSN=$(RUNTIME_TOOLS_VSN)`), sets `APPLICATION=runtime_tools`, then includes `$(ERL_TOP)/make/doc.mk`; provides the `docs`/`html`/`man`/`clean_docs`/`release_*` targets.
- `docs.exs` — ExDoc local config, only `:extras`; the list order defines the published page order: `guides/lttng.md`, `guides/dtrace.md`, `guides/systemtap.md`, `runtime_tools_app.md`.
- `runtime_tools_app.md` — the "Runtime tools Application" chapter (description, "no configuration parameters", SEE ALSO); matched by doc.mk's `*_app.md` rule so it also becomes the man6 page `runtime_tools_app.6`.
- `notes.md` — release notes for the whole application, ~42 KB, newest release first (`## Runtime_Tools 2.5`, …); hand-authored markdown checked into git, appended by the "Prepare release" process (no generator exists in the build system).
- `guides/dbg_guide.md` — "Tracing in Erlang with dbg" user's guide (~33 KB): quick start (`dbg:c/3`), tracing complex systems, match specs, trace sessions, remote nodes, port tracing, sequential tracing, avoiding overloads/deadlocks.
- `guides/lttng.md` — "LTTng and Erlang/OTP": building with `--with-dynamic-trace=lttng`, reference of all `org_erlang_dyntrace` tracepoints, BEAM tracepoints, process-tracing example.
- `guides/dtrace.md` — "DTrace and Erlang/OTP": history/goals/supported platforms, `--with-dynamic-trace=dtrace`, probe specs in `erts/emulator/beam/erlang_dtrace.d`, example scripts in `../examples/`.
- `guides/systemtap.md` — "SystemTap and Erlang/OTP": requisites, `--with-dynamic-trace=systemtap`, listing/attaching probes with `stap` against `../examples/*.systemtap`.

## Constraints
- Never add module reference docs here — every documented module in `../src/*.erl` carries `-moduledoc`/`-doc` (EEP-48) doc chunks; 9 of 15 modules use `-moduledoc false.` and are hidden.
- ExDoc reads the compiled doc chunks from `../ebin/*.beam` (`HTML_DEPS` in `make/doc.mk`), so the application must be compiled before `make docs`; `make/check_ex_doc` downloads `ex_doc` when the `html` target is used.
- `make/ex_doc.exs` merges the local `docs.exs` `:extras` with the wildcards `*.md` and `{guides,references,internal_docs}/*.md`, then `Enum.uniq`: files not listed in `docs.exs` are still published but appear after the explicitly listed ones (`guides/dbg_guide.md` and `notes.md` currently rely on this).
- Reordering published extras or adding one at a chosen position requires editing `:extras` in `docs.exs`; this is the only per-app ExDoc knob here (`:groups_for_extras` defaults classify `guides/` as "User's Guides").
- Guides and extras in this directory are markdown only — no `.erl` doc sources, no `references/`, `internal_docs/`, `assets/` or `html/` subdirectories.
- Man pages are produced by `$(ERL_TOP)/make/markdown_to_man.escript`: `man3/*.3` from the `../src/*.erl` files that do not say `-moduledoc false.`, and `man6/runtime_tools_app.6` from `runtime_tools_app.md`.
- `notes.md` is exempted from undefined-reference warnings (`skip_undefined_reference_warnings_on: ["notes.md"]` in `make/ex_doc.exs`).
- Build outputs `html/` and `man1..man7/` are git-ignored (root `.gitignore`: `/lib/*/doc/html`, `*.3`, …); never commit generated doc artifacts.
- Cross-app links use the `e:<app>:<page>.md` form, e.g. `dbg.erl` links to `` `e:runtime_tools:dbg_guide.md` ``; intra-app links are plain relative links (`` `[release notes](notes.md)` ``).

## Routing Table
| Doc file | Documents |
| --- | --- |
| `runtime_tools_app.md` | The `runtime_tools` application as a whole (chapter + man6) |
| `guides/dbg_guide.md` | `m:dbg` (the `../src/dbg.erl` moduledoc links here) and the `erlang:trace` BIFs it wraps |
| `guides/lttng.md` | `m:dyntrace` as a `{tracer,dyntrace,[]}` backend; LTTng/`org_erlang_dyntrace` tracepoints |
| `guides/dtrace.md` | DTrace provider; probe layout in `../../erts/emulator/beam/erlang_dtrace.d` |
| `guides/systemtap.md` | SystemTap Linux compatibility path for the same probes |
| `notes.md` | Release notes for all `runtime_tools` modules/releases |
| `../src/*.erl` | Actual module reference docs (read-only from here; `-moduledoc`/`-doc` attributes) |
| `../examples/` | `.d` / `.systemtap` probe scripts referenced by `dtrace.md`/`systemtap.md` (read-only, sibling) |
| `../test/dyntrace_SUITE.erl`, `../test/dyntrace_lttng_SUITE.erl`, `../test/dbg_SUITE.erl` | Tests exercising the tracing features these guides document (read-only, sibling) |

## Notes for Agents
- Editing prose about a module (behaviour, examples) usually means editing the guide here AND the `-moduledoc`/`-doc` in `../src/<module>.erl`; the two are separate sources.
- `make docs` in this directory needs `ERL_TOP` set and only works from an in-tree build (the Makefile includes `../vsn.mk` and `$(ERL_TOP)/make/*`).
- Only 13 of the 36 `lib/*/doc` directories have a `doc/src` (asn1, common_test, diameter, inets, kernel, megaco, mnesia, odbc, sasl, snmp, ssl, stdlib, tools); the other 22, like this one, rely solely on in-source docs plus extras.

## See Also
- `../CONTEXT.md` — the `runtime_tools` application (src layout, modules, tests).
- `../../make/doc.mk`, `../../make/ex_doc.exs` — the generic doc build and ExDoc defaults.
- `../../system/doc/reference_manual/documentation.md` — OTP's authoring rules for `-moduledoc`/`-doc` and extra-page links.
