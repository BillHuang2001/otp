# tools — Sources (`lib/tools/src`)

## Intent
Erlang sources for the OTP `tools` application — a collection of mostly-independent developer/analysis tools (coverage, profilers, cross-reference, Emakefile make, TAGS generation, crash-dump decryption).
This is a FLAT directory: every module lives directly here, there are no child directories.
Public documentation is written as edoc/ExDoc Markdown inside the `-moduledoc`/`-doc` attributes of the `*.erl` files themselves (the sibling `../doc/` tree only holds the ExDoc config `docs.exs`, guides, references and `.fig` figures — no per-module doc sources).

## API Surface (module inventory)
Public API modules (documented, `-moduledoc` present):
- `cover.erl` — line coverage analysis tool (3119 lines, largest module); cover-compiles modules and counts executable-line executions. Uses `counters` and, when available, native runtime coverage (OTP 27+).
- `cprof.erl` — call-count profiler using BEAM breakpoints with counters; no compilation of targets needed.
- `tprof.erl` — process-tracing profiler (`call_count` / `call_time` / `call_memory`), `since OTP 27.0`; explicitly aims to replace `eprof` + `cprof`; experimental, offers ad-hoc and server-aided modes.
- `eprof.erl` — time profiler via trace BIFs (`gen_server`).
- `fprof.erl` — trace-to-file time profiler (own vs. accumulated time; traces to file for low overhead).
- `lcnt.erl` — runtime-system lock-profiling tool (`gen_server`), `since OTP R13B04`; needs an emulator built with lock counting.
- `crashdump.erl` — decrypts encrypted Erlang crash dumps, `since OTP 29.0`; uses `public_key` + `crypto`.
- `make.erl` — Emakefile-based Make utility (`all/0,1`, `files/1,2`, `all_or_nothing/0`).
- `tags.erl` — generates Emacs `TAGS` files (`file/1,2`, `files/1,2`, `dir/1,2`, `root/0,1`).
- `xref.erl` — cross-reference analysis API (`gen_server`, API surface: `start/1,2`, `stop/1`, `m/1`, `d/1`, index/query functions, `format_error/1`).

Internal/private implementation modules (`-moduledoc false`, not public API):
- `xref_base.erl` — xref state/record handling and query engine internals.
- `xref_compiler.erl` — compiles raw module data into the xref graph.
- `xref_reader.erl` — reads module data from BEAM files.
- `xref_scanner.erl` — lexical scanner for the xref query language (`scan/1`).
- `xref_utils.erl` — shared helpers (`xset/2`, `is_directory/1`, path/name helpers).

Generated / build inputs in this directory:
- `xref_parser.yrl` — Yecc grammar for the xref query language; `xref_parser.erl` is GENERATED from it at build time (do NOT hand-edit the generated file).
- `tools.app.src` / `tools.appup.src` — application resource templates with `%VSN%` placeholder.
- `xref.hrl` — shared xref record definitions (`#xref`, `#xref_mod`, `#xref_app`, `#xref_rel`, `#xref_lib`, `#xref_var`). Header comment: "meant to be included by `xref_*` only".
- `Makefile` — build rules for this app's sources.

## Constraints
- `xref.hrl` is included by `xref.erl`, `xref_base.erl`, `xref_compiler.erl`, `xref_reader.erl`, `xref_scanner.erl` (the `xref_*` family) — keep it internal to that group.
- Never edit the generated `xref_parser.erl`; change `xref_parser.yrl` instead.
- Never edit `tools.app`/`tools.appup`; they are produced by sed-substituting `%VSN%` (from `../vsn.mk`, `TOOLS_VSN`) into the `.src` templates (see `Makefile`).
- `MODULES` in `Makefile` must list every public module shipped in `ebin`; `xref_parser` is built from `YRL_FILE`.
- Sources compile with `ERL_COMPILE_FLAGS += -Werror +nowarn_deprecated_catch` — warnings are fatal.
- Under `ERL_DETERMINISTIC=yes` the build skips dependency generation for the generated `xref_parser.erl` (`DEP_SKIP_FILES`).
- `eprof.erl` and `fprof.erl` carry `-compile([{nowarn_deprecated_function, [{erlang,exit,2}]}])`.

## Internal Module Dependencies (within `src/`)
- `xref.erl` → `xref_base`, `xref_utils`.
- `xref_base.erl` → `xref_compiler`, `xref_reader`, `xref_utils`.
- `xref_compiler.erl` → `xref_scanner`, `xref_utils`.
- `xref_reader.erl` → `xref_utils`.
- `crashdump.erl` → `public_key` (+ `crypto`) via `-include_lib("public_key/include/public_key.hrl")`.
- The coverage/profiler/make/tags modules are self-contained (no cross-references among them).

## Runtime / Environment Requirements (agent-relevant)
- `cover`: target modules must be cover-compiled (`cover:compile*`), or the runtime must provide native coverage (OTP 27+) for the low-overhead path; modules built without debug/abstract info cannot be coverage-mapped back to source.
- `xref`: reads `debug_info` from BEAM files (or abstract code from source) — the analyzed code must be compiled with `debug_info`; it is the analyzing OTP version that decides what counts as a BIF.
- `cprof`/`eprof`/`fprof`/`tprof`: require a live node and the trace facilities; no special compilation of targets needed (except fprof traces to file).
- `lcnt`: requires an emulator built with lock-counting enabled.
- `crashdump`: decryption only works on dumps produced by an emulator built with `--enable-encrypted-crash-dumps`.

## Notes for Agents
- Do not look for `lc`, `erl_tidy` or `instrument` here — `instrument` was moved to `runtime_tools`, and `lc`/`erl_tidy` no longer exist under `lib/`.
- `tools.app.src` declares `applications = [kernel, stdlib]` and `runtime_dependencies` including `runtime_tools`, `compiler`, `public_key`, `crypto`.
- The `priv/` directory holds `styles.css` (used by `cover`'s HTML output) and is installed to `$(RELEASE_PATH)/lib/tools-$(VSN)/priv`.
- Release build installs `src` (`*.erl` + `xref_parser.yrl` + `xref.hrl`) and `ebin` outputs to `$(RELEASE_PATH)/lib/tools-$(VSN)`.
- The app's test suites live in the sibling `../test/` directory (one `*_SUITE.erl` per tool) — read-only from here; escalate writes there to the parent node.

## Routing Table
This node is flat; entries map tool areas to their owning source files:
- Coverage analysis → `cover.erl`
- Call profilers → `cprof.erl`, `tprof.erl`, `eprof.erl`, `fprof.erl`
- Runtime lock profiling → `lcnt.erl`
- Crash-dump decryption → `crashdump.erl`
- Cross-reference tool (public API) → `xref.erl`
- Cross-reference internals → `xref_base.erl`, `xref_compiler.erl`, `xref_reader.erl`, `xref_scanner.erl`, `xref_utils.erl`, `xref_parser.yrl` (generated), `xref.hrl`
- Build/dev helpers → `make.erl`, `tags.erl`
- Application resource + build → `tools.app.src`, `tools.appup.src`, `Makefile`

## See Also
- `../CONTEXT.md` — the `tools` application as a whole (doc/ structure, test/, build layout).
- `../../CONTEXT.md` — OTP library applications overview.
