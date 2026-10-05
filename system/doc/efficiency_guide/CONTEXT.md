# Efficiency Guide (`system/doc/efficiency_guide`)

## Intent
The "Efficiency Guide" chapter series: how to write efficient Erlang code, which language/VM constructs are traps, and how to measure (benchmark/profiling) instead of guessing.
It is the docs group whose sidebar title comes from `efficiency_guide:Efficiency Guide` in `system/doc/guides`.
Everything here is prose (Markdown) plus unbuilt example sources — nothing in this directory is compiled, tested or executed by the build.
Published as pages of the single `system/doc` ex_doc site (`system/doc/html`, installed as `doc/system`).

## API Surface

### Pages (13 Markdown chapters; sidebar order == order in `system/doc/docs.exs`)
1. `efficiency_guide.md` — landing page: Purpose (Knuth quote) and Prerequisites only. H1 is `# Introduction`, so the page/sidebar title is "Introduction".
2. `commoncaveats.md` — constructs to watch out for: `++`, the `timer` module, accidental copying / loss of sharing, `list_to_atom`/`binary_to_atom`, `length/1`, `setelement/3`, NIFs.
3. `binaryhandling.md` — constructing and matching binaries, refc/heap/sub binaries, match context, circumstances that force copying, option `bin_opt_info`.
4. `maps.md` — records vs maps, maps as records/dictionaries/sets, how small/large maps are implemented, map syntax vs `maps` functions, then one section per `maps` function.
5. `listhandling.md` — creating lists, list comprehensions, deep vs flat lists, recursive list functions.
6. `eff_guide_functions.md` — pattern matching and the function-call cost hierarchy.
7. `tablesdatabases.md` — ETS/DETS/Mnesia: select/match, delete, fetch, `tab2list`, `ordered_set`, keys, secondary index, transactions.
8. `eff_guide_processes.md` — process creation and initial heap size, sending/fetching messages, option `recv_opt_info`, literal pool, loss of sharing, SMP.
9. `drivers.md` — writing efficient linked-in drivers; avoiding binary copying between driver and emulator.
10. `memory.md` — memory footprint of data types and operations (one large raw HTML table).
11. `system_limits.md` — documented implementation limits (processes, PIDs, ports, atoms, binaries, references, unique integers, timers, ...).
12. `profiling.md` — "never guess about bottlenecks", memory profiling, large systems, what to look for, tools (fprof/eprof/cprof/`m:tprof`, dbg, lcnt, observer/etop/msacc) and the "Tool Summary" table.
13. `benchmarking.md` — benchmarking with `erlperf` (with sample output), plus `timer:tc/3` vs `statistics(runtime)` advice.

### Support / example files (none of them are part of the build)
- `efficiency_guide.erl` (282 lines, `-compile([export_all,nowarn_export_all])`, no module doc) — a bag of example functions (`naive_reverse`, `after_zero`, `count1..3`, `simple_receive`, `optimized_receive`, `kilo_byte`, `#state{}` records, ...). Two pages quote compiler output obtained by compiling this module: `binaryhandling.md` (`erlc +bin_opt_info efficiency_guide.erl`) and `eff_guide_processes.md` (`erlc +recv_opt_info efficiency_guide.erl`). Readers compile it themselves; no make target or test in the repo compiles or references it.
- `README` — describes the legacy benchmark framework below (callback contract, `?repN` macros, pitfalls, generated files).
- `bench.erl`, `bench.hrl`, `all.erl`, `call_bm.erl`, `call_result.html` — a standalone pre-GitHub benchmark framework: `bench:run/0` compiles and runs all `<name>_bm.erl` callback modules in the cwd and writes `*.bmres` + an `index.html` report; `all:run/0` runs the benchmarks across several installed Erlang releases; `bench.hrl` holds the `?rep5/?rep10/?rep20/?rep40/?rep80` repetition macros; `call_bm.erl` is the only example callback module (local vs external vs fun vs `apply` call costs); `call_result.html` is a checked-in sample report comparing a single "5.4 BEAM" column.
  - **DEAD**: no page links to any of these, they are not listed in `docs.exs`, not compiled by any Makefile, and not exercised by any test/suite. Unchanged (apart from license-text edits) since the R13B03 import; they predate the `erlperf`-based `benchmarking.md`, which replaced them with no link. Treat as historical artifacts — do not extend or "fix" them without a decision from the docs owner.

## Constraints
- A page is published only if it is listed in `system/doc/docs.exs` `extras:`. Guide directories are NOT globbed by `make/ex_doc.exs` (`Path.wildcard` there only covers `*.md`, `{guides,references,internal_docs}/*.md`). List order == sidebar order; keep `efficiency_guide/efficiency_guide.md` first.
- Every `.md` file starts with the HTML-comment `%CopyrightBegin% ... %CopyrightEnd%` Apache-2.0 header (`Copyright Ericsson AB 2023-2025`).
- The first `#` heading becomes the page title and the sidebar entry (hence "Introduction" for the landing page) — titles are not taken from `docs.exs`.
- ex_doc autolinks are written inside backticks: `` `m:module` ``, `` `t:type/0` ``, `` `fun/arity` ``, and `` `e:app:file.md#anchor` `` for other applications' docs (e.g. `` `e:erts:erl_cmd.md#max_processes` ``, `` `e:erts:erl_nif.md#lengthy_work` ``). Anchors are either explicit `[](){: #anchor }` (heavily used in `system_limits.md` and for single list items, because otherwise the target has no heading) or GitHub-style slugs derived from the heading (`## Literal Pool` → `#literal-pool`, `## Loss of Sharing` → `#loss-of-sharing`). The repo also uses `m:foo()` as a placeholder for "some module" (e.g. in `eff_guide_functions.md`).
- Cross-page links are relative to the source files (`[Tools](profiling.md#profiling_tools)`, `[Benchmarking](benchmarking.md)`, `[small maps](maps.md#terminology)`); ex_doc rewrites them to the flat `.html` output. Broken links and undefined `m:`/`e:` references fail the build: warnings-as-errors is the default (`make/doc.mk` `EX_DOC_WARNINGS_AS_ERRORS?=default` → true in `make/ex_doc.exs`), only auto-downgraded when some `lib/*/ebin/*.app` is missing (`make/ex_doc_wrapper.in`); `make/fixup_doc_links.sh` rewrites link prefixes for the installed release copy.
- Do not assume an `assets/` directory exists — this guide has none and no page references an image. If images are ever added they must go in `system/doc/efficiency_guide/assets/`, because `ASSETS_SRC` in `system/doc/Makefile` globs `*/assets/*.gif|png|svg` and copies them **flat** into the git-ignored `system/doc/assets/`, so file names must be unique across all guides.
- The Markdown here is not a build input beyond rendering: `system/doc/Makefile` sets `HTML_DEPS` to all `$(guide)/*.md`, so editing the `.erl`/`.hrl`/`README` files does not even trigger an ex_doc re-run. `make/doc.mk` derives `ERL_FILES_WITH_DOC`/man sources from `APP_DIR/src`, which for this doc directory is `../../lib/erl_interface/src` — the local Erlang sources are invisible to it.
- Preserve the version-scoped wording: many claims are deliberately limited ("From Erlang/OTP 27", "Starting with Erlang/OTP 26", "Before OTP 26.0", "the sharing ... was introduced in OTP 26.0"). When behavior changes, update the scope wording instead of deleting the note.

## Known Issues
- `binaryhandling.md` (`### Option bin_opt_info`) is internally inconsistent about the message prefix: the quoted sample output says `./efficiency_guide.erl:60: Warning: NOT OPTIMIZED: binary is returned from the function`, while the annotation embedded in the `after_zero/1` example in the same section says `BINARY CREATED: binary is returned from the function`. The compiler prefixes every `binary_created` promotion with `BINARY CREATED: ` (`lib/compiler/src/beam_ssa_bsm.erl`, `format_opt_info/1`; the `#b_ret{}` case yields exactly "binary is returned from the function"), so the sample-output line is the wrong one. The `efficiency_guide.erl:60/62` line numbers themselves do match the current `after_zero/1` clauses.
- `eff_guide_processes.md` (`### Option recv_opt_info`) quotes warnings with `efficiency_guide.erl:<line>` numbers that are 2 lower than the current module: doc 194/200/206/208/218/219/222 vs actual `receive`/`monitor`/`make_ref` sites 196/202/210/208/220/221/224. Both the sample-output block and the `%% efficiency_guide.erl:NNN:` comments inserted into the examples carry the stale numbers. Re-derive them with `erlc +recv_opt_info` and update both places together.
- `memory.md` states its table shows "figures for the `erts-8.0` system in OTP 19.0" and has not been re-measured; it is explicitly approximate and must not be passed off as current (OTP 30) numbers.
- Because no page links to the legacy benchmark framework, `README`, `bench.erl`, `bench.hrl`, `all.erl`, `call_bm.erl` and `call_result.html` are unreachable from the published documentation; `call_result.html`'s own `href="call_bm.erl"` only resolves when browsing the source tree.

## Notes for Agents
- This is a text-only documentation directory: the only correctness checks that matter are (a) the page is listed in `system/doc/docs.exs`, (b) every link/anchor/autolink resolves. Validate by running `make docs` from `system/doc` with warnings-as-errors enabled (ex_doc is fetched via `make/check_ex_doc`/`otp_build download_ex_doc`).
- `efficiency_guide.erl` is the reference input for the compiler-output quotes in two pages; inserting or removing lines in it silently invalidates those quotes (see Known Issues).
- `system_limits.md` and `commoncaveats.md` are the pages most tightly coupled to ERTS internals (`erl(1)` `+P`/`+Q` flags, `erts_alloc` options, `+MIscs`, NIF responsiveness, `bin_opt_info`/`recv_opt_info`); cross-check the ERTS/compiler documentation before changing numbers or limits.

## See Also
- `../CONTEXT.md` (`system/doc`) — ex_doc configuration (`docs.exs`, `guides`, `Makefile`, `assets/`), how the guide list and sidebar groups are derived (read-only from here; escalate writes to the parent).
- `../reference_manual/` — language and `erlang` BIF reference this guide links into (`m:erlang#system_info_wordsize`, `m:erlang#process_info_memory`, ...).
- `../../erts/doc/` — ERTS user's guide targets used here (`e:erts:erl_cmd.md`, `erl_nif.md`, `erl_driver.md`, `time_correction.md`, `beamasm.md#linux-perf-support`, `erts_alloc.md#MIscs`).
- `../../../lib/compiler/src/` — `beam_ssa_bsm.erl` / `beam_ssa_recv.erl` produce the `bin_opt_info` / `recv_opt_info` messages quoted by pages here; `../../../lib/compiler/test/warnings_SUITE.erl` shows the exact expected warning terms.
- `../../../lib/runtime_tools/doc/`, `../../../lib/observer/doc/`, `../../../lib/tools/doc/` — manuals for the profilers the Profiling page summarizes (fprof, eprof, cprof, dbg, lcnt, msacc, observer, etop).

## Status
Complete.
