# parsetools — Generator Sources

## Intent
Erlang sources of the two code generators shipped with OTP: `yecc` (yacc-like LALR(1) parser generator, `.yrl` → `.erl`) and `leex` (lex/flex-like regex scanner generator, `.xrl` → `.erl`).
Also holds yecc's own auxiliary modules: the meta-scanner `yeccscan.erl`, the metagrammar of `.yrl` files `yeccgramm.yrl`, and the parser `yeccparser.erl` that yecc generates from it.
App metadata (`parsetools.app.src`, `parsetools.appup.src`) and the per-app `Makefile` live here too.
App version is `PARSETOOLS_VSN` in `../vsn.mk`.

## API Surface
- `yecc.erl` (3321 lines) — the LALR(1) parser generator.
  - Exports `file/1`, `file/2`, `format_error/1`, and `compile/3` (the `erlc`/`erl_compile` hook, `-doc false`); legacy `yecc/2,3,4` kept for R10B compatibility.
  - Exports types `option/0`, `yecc_ret/0`, `error_info/0`.
  - Its long `-moduledoc` (lines ~28–488) is the published user reference for the `.yrl` format, precedence declarations, and all options — update it for user-visible behaviour, there is no separate doc file.
  - Options (validated by `all_options/0` + `check_options/3`): `{error_location, column|line}` (default `column`), `{file_attributes, boolean}` (default `true`), `{includefile, F}`, `{parserfile, F}`, `report_errors`/`report_warnings`/`report` (default `true`), `return_errors`/`return_warnings`/`return` (default `false`), `{time, boolean}` (undocumented per-pass timing), `verbose`, `warnings_as_errors`, `deterministic`.
  - `ERL_COMPILER_OPTIONS` is parsed by `env_default_opts/0` and filtered to recognized names by `select_recognized_opts/1`.
  - `file/2` does the work in a spawned linked process (`infile/3`) and restores the caller's `trap_exit` flag; internal failures `throw(#yecc{})`.
  - Pipeline in `generate/1`, driven by `?PASS(P)` over the `#yecc{}` state: `parse_grammar` → `check_grammar` → `states_and_goto_table` → `parse_actions` → `action_conflicts` → `write_file`; the fold aborts as soon as `#yecc.errors` is non-empty.
  - `compile/3` maps the `#options{warning, verbose, includes, specific}` record to yecc options (`includes` first entry → `includefile`) and returns `ok | error`.
- `leex.erl` (2332 lines) — the scanner generator.
  - Exports `file/1`, `file/2`, `format_error/1`, `compile/3`.
  - Also fake-exports `string/1,2`, `token/2,3`, `tokens/2,3` whose bodies are `erlang:nif_error(undef)`: they exist only so `-doc`/`-spec` can document the API of the *generated* scanner (group `<<"Generated Scanner Exports">>`).
  - Options: `dfa_graph`, `{includefile, F}`, `{scannerfile, F}`, `report_errors`/`report_warnings`/`report`, `return_errors`/`return_warnings`/`return`, `verbose`, `warnings_as_errors`, `deterministic`, `{error_location, line|column}` (default `line`), `{tab_size, N}` (default `8`).
  - Pipeline in `file/2`: `filenames/3` (derives `.xrl`/`.erl`/`.dot` names and the module atom) → `parse_file/1` → `make_dfa/2` → `out_file/5`, optionally `out_dfa_graph/3` for `dfa_graph`; aborts by `throw(#leex{})`.
- `yeccscan.erl` (78 lines) — `scan/1`, `scan/3`: yecc's tokenizer for grammar files.
  - Wraps `io:scan_erl_form/5` with `[text, return]`, reclassifies bare atoms/operators via `erl_scan:reserved_word/1` into `reserved_word`/`reserved_symbol`, and returns both the full and the white-space/comment-filtered token lists.
  - Used only by yecc; leex tokenizes with `erl_scan` directly.
- `yeccparser.erl` (930 lines) — GENERATED parser for `yeccgramm.yrl`; exports `parse/1`, `parse_and_scan/1`, `format_error/1`.
- `yeccgramm.yrl` (82 lines) — the metagrammar (syntax of `.yrl` files) that yecc parses itself with; declares nonterminals `grammar declaration rule head symbol symbols attached_code token tokens`, and terminals `atom float integer reserved_symbol reserved_word string char var '->' ':' dot`, `Rootsymbol grammar`.
  - Its `Erlang code.` section defines `-record(symbol, {anno = none, name})` plus `symbol/1`, `value_of/1`, `anno_of/1`, which `yecc:parse_grammar/2` relies on.
- `parsetools.app.src` / `parsetools.appup.src` — `%VSN%` is substituted with `$(VSN)` by `sed` in the `Makefile`; the `modules` list duplicates the Makefile's `MODULES`.
- `Makefile` — `MODULES = leex yecc yeccparser yeccscan`; `HRL_FILES = ../include/yeccpre.hrl ../include/leexinc.hrl`; `regenerate:` runs `erlc +deterministic -o yeccparser.erl yeccgramm.yrl`; `release_spec` installs `src/`, `ebin/` and the two headers into `$(RELEASE_PATH)/lib/parsetools-$(VSN)`.

### Grammar-file → parser data flow (yecc)
`.yrl` → `yeccscan:scan/3` (per-line tokens) → `yeccparser:parse/1` (metagrammar pass) → `#yecc{}` state update in `parse_grammar/2` (`rule`, `prec`, declarations `Nonterminals`/`Terminals`/`Rootsymbol`/`Endsymbol`/`Expect`/`States`, `Header`, `Erlang code.`) → `check_grammar/1` (adds `?ACCEPT`/`'$empty'`, resolves rootsymbol/endsymbol, precedences, rules) → `create_symbol_table/1` + `compute_states/1` (symbols coded to integers, LALR states and goto table) → `compute_parse_actions/3` → `find_action_conflicts/1` → `write_file/1` (emits code).

### Scanner-file → scanner data flow (leex)
`.xrl` → `parse_head` (blank/comment lines) → `parse_defs` (the `Definitions.` section: `NAME = regexp` macro lines) → `parse_rules` (the `Rules.` section: `regexp : Action.` per line, body scanned with `erl_scan:tokens/3` via `collect_action/4`) → `parse_code` (the `Erlang code.` section recorded only as `{Line, FilePos, NCodeLines}`, copied later) → macro substitution of `{NAME}` in regexps (`parse_rule_regexp/3`) → regexp parser (`re_parse/2`: alt/seq/repeat/char-class/escapes) → `make_dfa/2` → `out_file/5`.

### What the generated output looks like
- yecc emits, in order: optional `Header` strings, `-module(M)`, `-file` directives, `-export([parse/1, parse_and_scan/1, format_error/1])`, the user's `Erlang code.` copied verbatim, then the contents of `../include/yeccpre.hrl` verbatim, then `yeccpars2*`, `yeccgoto_*` and the inlined action functions.
  - The parser runtime (`yeccpars0/1,5`, `yeccpars1/5,7`, `yeccerror/1`, `return_error/2`, `yecc_end/1`, token-to-string helpers) lives entirely in `yeccpre.hrl`; the module doc for `parse/1`/`parse_and_scan/1` is injected by the user via `?YECC_PARSE_DOC`/`?YECC_PARSE_AND_SCAN_DOC`/`?YECC_FORMAT_ERROR_DOC`.
- leex copies `../include/leexinc.hrl` line by line and substitutes marker lines recognized by their first 5 characters: `##module`, `##code`, `##str`, `##tkn`, `##tks`, `##tab`, `##dfa`, `##act` (everything else is emitted verbatim).
  - The emitted scanner defines `yystate/0` (start state) + one `yystate/7` clause per DFA state group, `yyaction_N/…` per rule action, and the public wrappers `string/1,2`, `token/2,3`, `tokens/2,3`, `format_error/1`, plus `tab_size/0` when `tab_size` is set.
- Both generators let the user override the prologue via `{includefile, F}`; yecc detects the "new" (`{1,4}`) vs old (`{1,1}`) prologue by preprocessing it (see Known Issues).

## Constraints
- `yeccparser.erl` is generated: edit `yeccgramm.yrl` and run `make regenerate` (`erlc +deterministic -o yeccparser.erl yeccgramm.yrl`) — never hand-edit it.
  It is also listed in `DEP_SKIP_FILES` so `make/dep.mk` skips dependency generation for it.
- The emitted-code version string is coupled: `?CODE_VERSION` is `"1.4"` in both `yecc.erl` and `../include/yeccpre.hrl` — change them together.
  A parser generated by yecc "1.4" running against a mismatching `yeccpre.hrl` surfaces as `{yecc_bug, ?CODE_VERSION, Desc}` because `yeccpars0/5` re-raises unexpected errors with the include file's version.
- `-Werror` plus `+warn_obsolete_bool_op +nowarn_deprecated_catch` are set in `ERL_COMPILE_FLAGS` (with `-I$(ERL_TOP)/lib/stdlib/include`); a new warning fails the build of this directory.
- Calls that the compiler considers unsafe are explicitly allowlisted per module: `-compile([{nowarn_possibly_unsafe_function, {erlang, list_to_atom, 1}}, ...])` (yecc also allows `os:cmd/1`).
- `'$empty'`, `'$end'`, `'$undefined'` are reserved symbol names (rejected with `{reserved, N}`); `?ACCEPT` is the tuple `{}` and is neither a terminal nor a non-terminal.
- Invalid options raise `erlang:error(badarg, ...)`; formattable failures come back as `{error, Errors, Warnings}` / `error` and each descriptor must have a `format_error/1` clause.
- Never edit generated build output or the `.app`/`.appup` files locally: `%VSN%` substitution happens in the Makefile.

## Design Decisions
- `parse_grammar/2` clauses are keyed off the `#symbol{}` records produced by `yeccgramm.yrl`'s Erlang code; the metagrammar deliberately uses that record so yecc's own parser and the generator share one representation.
- Lookahead sets are represented as integer bit masks during state computation (`set_add/2`, `set_union/2`, …), with terminals coded to indices and `'$empty'` fixed at mask 1; `?SYMBOLS_AS_CODES` selects this fast path and the `-ifdef`/`-else` pair at the bottom of `yecc.erl` keeps an equivalent symbolic implementation for debugging.
- Non-terminals are coded to negative integers whose absolute value indexes a tuple of rule lists (`?ACCEPT` handling included); the coded-only `#yecc{}` state (`StC`) is discarded after the pass.
- LALR states are stored in a plain `{}` keyed by state number (`first_state/0` is 0), with `ets` tables `yecc_state_id` (state id → state number) and `yecc_goto` (bag) for the goto relation; the goto table is decoded from symbol codes to names at the end of `compute_states/1`, and `ets` is cleared between passes (`erase/0`).
- Generated code size is reduced by two state-sharing passes: `find_identical_shift_states/1` maps states with identical shift-action sets onto one representative (other states emit no clauses, only a `%% see yeccpars2_N` comment), and `find_partial_shift_states/2` extracts clause groups shared between states into `cont_N/7` functions described by `{jump_some,To}` / `{jump_all,To}` records.
  `select_parts/1` is explicitly documented as greedy and non-optimal — it minimizes size, not optimally.
- Every rule action with user code is emitted as its own inlined function `yeccpars2_State_Term_N/1` so compiler errors/warnings point at the right `.yrl` line; the reduce clause then calls it to build `NewStack`.
- Two micro-optimizations in the emitted code: a reduce action whose body is exactly the pseudo-variable `'___1'` with one daughter reuses `Stack` instead of calling an action function, and the call to `yeccpars2/7` is short-circuited straight to `yeccpars2_Repr/7` when the target state is known.
- `sort_parse_actions1/1` orders action kinds as accept, `#shift{}`, `#reduce{}`, `nonassoc`; the dummy field `unused` in `-record(reduce, …)` exists only to make `#reduce{}` compare before `#shift{}` in that ordering.
- leex builds a Thompson NFA then a subset-construction DFA over *sets of character ranges* rather than characters, which supports arbitrary Unicode ranges (`.` is `[{0,9},{11,maxchar}]`) without assuming a charset size; `maxchar` is just an upper-bound sentinel.
  `pack_crs/1`/`pack_trans/1` merge adjacent or contained ranges/transitions to shrink both the DFA and the emitted code, and `minimise_dfa/2` runs a simple state-merging pass.
- leex's `error_location` is a code-generation switch, not a runtime one: it decides whether the emitted wrappers pass/return `{Line, Col}` or `Line`, and `TokenLoc` can still be requested by an action independently.
- Both modules share the same CLI-compatible shape: `format_error/1`, `ERL_COMPILER_OPTIONS` handling, `shorten_filename/1`, `assure_extension/2`, and a `compile/3` that adapts stdlib's `#options{}` (`-include("erl_compile.hrl")` from `lib/stdlib/include`).
- leex lives in `parsetools` (`?LEEXLIB`), and the default prologue `leexinc.hrl` is looked up via `code:lib_dir(parsetools)`; the comments retain the old standalone-`leex` alternative.
- `output_file_directive/…` emits `-file(...)` attributes both to relate messages to the `.yrl`/`.xrl` source and to keep the generated file's own line numbering consistent; `deterministic` restricts those paths to basenames.

## Known Issues
- `#leex{}` has planned-but-unused structure: `posix` is commented out in the record, and every regexp clause guarded on `#leex{posix = true}` (POSIX character classes, `{m,n}` interval support) is commented out — POSIX classes are simply not implemented.
- Because parser generation runs in a linked child process, an internal crash aborts the caller differently from an ordinary error; the `receive after 1` in `file/2` exists to let the child's exit signal arrive before restoring `trap_exit`.
- `includefile_version/1` decides compatibility by preprocessing the custom include file and looking for `yeccpars1/7`; a file without it silently selects the legacy `{1,1}` code paths (`output_call_to_includefile/2`, `output_goto_fini/3`, `yeccpars1/5`), so a hand-written include file that merely fails `epp` parsing falls back to the old API instead of erroring.
- `-include("ms_transform.hrl")` in `yecc.erl` is required by `check_usage/1`, which uses `ets:fun2ms` to find unused symbols.
- `list_to_atom` derives module names from filenames and creates atoms for generated function/continuation names (`cont_N`, `yeccpars2_*`, `yeccgoto_*`, `yyaction_N`) — these atoms are per generated file, hence the `nowarn_possibly_unsafe_function` attributes.
- `'->'` and `':'` are simultaneously metagrammar symbols and terminals, so `yeccgramm.yrl` converts them explicitly; `attached_code -> '$empty'` synthesizes a token whose annotation text is `"'$undefined'"`.
- The pseudo-variable prefix is `"___"` and `'___1'` is rewritten during `parse_grammar` (`subst_pseudo_vars/3`), so real Erlang variables named `___N` in actions would collide.
- `declaration -> symbol strings dot` in the metagrammar is the shape that yields the `{bad_symbol, String}` error for e.g. `Nonterminals "foo"`.
- Absolute input paths are shortened relative to the current working directory by `shorten_filename/1`, so messages and `-file` attributes depend on the invocation cwd.
- `timeit/3` calls `os:cmd/1` only on SunOS and `erts_debug:flat_size/1` always (which is not allowed under `-mode(interactive)` restrictions in some test setups); it must stay behind the `time` option.
- Sections in leex are detected by matching the literal strings `Definitions.`, `Rules.`, `Erlang code.` and by a macro-definition regexp; a typo in a section header is reported as `missing_defs`/`missing_rules`/`missing_code`, and the code section is detected by position (so the file must be seekable).

## Notes for Agents
- `yecc.erl` and `leex.erl` are the two big files: 3.3k and 2.3k lines. Navigate by the section banners (`%%% Exported functions`, `% Computing parse states …`, `% Code generation:`, `% Auxiliaries:`) rather than reading linearly.
- The comment block above `?CODE_VERSION` in `yecc.erl` is the only description of the generated-code version history — it is documentation, not a changelog to prune.
- `parsetools.app.src`'s trailing blank lines are original file content; leave them alone.
- `../doc/notes.md` and `../doc/docs.exs` hold the app's ExDoc extras if the public docs need changing.

## Dependencies
- stdlib/kernel modules: `erl_scan`, `erl_anno`, `erl_parse`, `io`, `io_lib`, `file`, `filename`, `epp`, `lists`, `ordsets`, `orddict`, `sofs`, `string`, `re`, `unicode`, `proplists`, `ets`, `code`, `os`, `statistics`, `erts_debug`.
- `erl_compile.hrl` from `lib/stdlib/include` (the `#options{}` record matched by `compile/3`); `ms_transform.hrl` from stdlib (yecc only).
- `../include/yeccpre.hrl` and `../include/leexinc.hrl` are copied verbatim into every generated parser/scanner, so both are a hard runtime dependency of generated user code (also installed into the release).
- Build-time: GNU make, `erlc` with `-I$(ERL_TOP)/lib/stdlib/include`, `sed` for `%VSN%`, and `make/otp_release_targets.mk`/`make/dep.mk` from `../../make/`.

## Test Strategy
- Common Test suites live in the sibling `../test/` (`yecc_SUITE.erl`, `leex_SUITE.erl`, `app_SUITE.erl`, `parsetools.spec`); this directory has no tests of its own.
- `make regenerate` here is the check that `yeccparser.erl` is up to date with `yeccgramm.yrl`; a non-empty diff after regeneration means the grammar or yecc changed.
- Generated code is exercised by the suites in the consuming apps (`stdlib` `erl_parse.yrl`, `compiler`, `edoc`, `megaco`, `snmp`, `xmerl`, `diameter`, `tools/xref`) as well as by `../test/`.

## Routing Table
This directory has no child directories; it is a leaf node of the context tree.
- `../include/` → generated-code templates `yeccpre.hrl` / `leexinc.hrl` (sibling — read-only here, escalate writes to `../`)
- `../test/` → Common Test suites for both generators (sibling — read-only)
- `../doc/` → ExDoc config and notes (sibling — read-only)
- `../vsn.mk`, `../Makefile` → app version and build coordination (parent node)
- `../../make/otp.mk` → compiles `%.yrl`/`%.xrl` via `compile/3` (read-only, separate node)
- `../../lib/stdlib/src/erl_compile.erl`, `../../lib/stdlib/src/c.erl` → compiler dispatch to `yecc:compile/3`/`leex:compile/3` and `c:y/1,2` → `yecc:file/1,2` (read-only, separate node)

## See Also
- `../include/` — the two prologue templates the generated modules include; `yeccpre.hrl` carries the parser runtime and its own `?CODE_VERSION`.
- `../../make/otp.mk` — the rule that turns `.yrl`/`.xrl` sources into `.erl` by invoking `erlc`, i.e. the `compile/3` entry points here.
- `../../lib/stdlib/include/erl_compile.hrl` — definition of the `#options{}` record consumed by `compile/3`.
