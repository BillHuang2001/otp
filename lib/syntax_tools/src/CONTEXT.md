# syntax_tools/src — Application Sources

## Intent
Erlang sources of the `syntax_tools` application (app version `SYNTAX_TOOLS_VSN = 4.1.1`, see `../vsn.mk`).
Provides the `erl_syntax` abstract syntax-tree ADT plus parsing (tolerant), pretty-printing, comment re-attachment and merl metaprogramming.
This is a **library-only** application: no processes, no application callback module, no `priv` runtime assets (`../priv/` holds only `.gitignore`).
Nine modules are public API/runtime; `merl_tests.erl` is a test-only module living here (see Test Strategy).

## API Surface
Grouped by module (exports listed by role; see inline `-moduledoc`/`-doc` for details — do not duplicate here):

- `erl_syntax.erl` (8655 lines — by far the largest) — the core ADT, also the hub referenced by all other modules.
  - Introspection/identity: `type/1` (node type atom), `is_leaf/1`, `is_form/1`, `is_literal/1`, `annotated_type/2`, `subtrees/1`.
  - Conversion to/from `erl_parse`: `abstract/1` (term → tree), `concrete/1` (tree → term), `revert/1`, `revert_forms/1`.
  - Annotations/metadata: `get_ann/1`/`set_ann/2`/`add_ann/2`/`copy_ann/2`, `meta/1`, positions `get_pos/1`/`set_pos/2`/`copy_pos/2`, comments `get_precomments/*`, `get_postcomments/*`, `remove_comments/1`, `join_comments/2`.
  - Generic tree ops: `make_tree/2`, `update_tree/2`, `get_attrs/1`/`set_attrs/2`/`copy_attrs/2`, list helpers `cons/2`, `list_head/1`, `list_tail/1`, `list_elements/1`, `normalize_list/1`, `compact_list/1`.
  - Node constructors + accessors: one `Name(...)`/`name_field(...)` pair per syntax node type (application, atom/char/integer/float/string, attribute, binary/binary_field/binary_comp, block_expr, case_expr, clause, comment, conjunction/disjunction, function, fun_expr, if_expr, list/list_comp, match_expr, module_qualifier, record_expr/record_field, tuple, ...). `atom/1`, `is_atom/2` etc. are guarded by `nowarn_possibly_unsafe_function` for `list_to_atom/1`.
  - Exports types `forms/0`, `syntaxTree/0`, `syntaxTreeAttributes/0`, `padding/0`, `annotation_or_location/0`.
- `erl_syntax_lib.erl` — higher-level utilities over `erl_syntax` trees.
  - Traversal: `map/2`, `map_subtrees/2`, `mapfold/3`, `mapfold_subtrees/3`, `fold/3`, `fold_subtrees/3`, `mapfoldl_listlist/3`.
  - Analysis: `analyze_form/1`, `analyze_forms/1`, `analyze_application/1`, `analyze_attribute/1`, `analyze_export_attribute/1`, `analyze_function/1`, `analyze_record_expr/1`, `analyze_type_application/1`, `analyze_wild_attribute/1`, ... (each returns `{Key, Value}` `info_pair()` lists).
  - Variable handling: `annotate_bindings/1,2`, `variables/1`, `new_variable_name/1,2`, `new_variable_names/2,3`, `function_name_expansions/1`.
  - Comment/view helpers: `strip_comments/1`, `to_comment/1,2,3`, `is_fail_expr/1`, `limit/2,3`. Exports `info_pair/0`.
- `erl_prettypr.erl` — pretty-printer front end for `erl_syntax` trees, built on `prettypr`.
  - Main: `format/1,2` (tree → text), `best/1,2` (tree → `prettypr` document), `layout/1,2`.
  - Context hooks (`ctxt`): `get_ctxt_precedence/1`/`set_ctxt_precedence/2`, `get_ctxt_paperwidth`/`set_ctxt_paperwidth`, `get_ctxt_linewidth`/`set_ctxt_linewidth`, `get_ctxt_hook`/`set_ctxt_hook`, `get_ctxt_user`/`set_ctxt_user`.
  - `-import`s `prettypr` combinators and `erl_parse` precedence helpers (`preop_prec/1`, `inop_prec/1`, `func_prec/0`).
- `prettypr.erl` — generic Hughes-style pretty-printing document library, **independent of Erlang ASTs** (usable standalone).
  - Constructors: `text/1`, `null_text/1`, `text_par/1,2`, `above/2`, `beside/2`, `nest/2`, `sep/1`, `par/1,2`, `break/1`, `floating/1,3`, `follow/2,3`, `empty/0`.
  - Layout: `format/1,2,3`, `best/3`. Exports `document/0`.
- `epp_dodger.erl` — tokenize/parse Erlang **without expanding macros or preprocessor directives** (tolerant; yields `erl_syntax` trees).
  - Two strictness families: `parse_file/1,2`, `parse/1,2,3`, `parse_form/2,3` (raise on unsupported constructs) vs `quick_parse_file/1,2`, `quick_parse/1,2,3`, `quick_parse_form/2,3` (invent placeholders/atoms for unexpanded macros).
  - `format_error/1` is `-doc false` (internal); `tokens_to_string/1` is exported for tests/introspection.
  - Uses sentinel tokens `'? <macro> ('`, `" ?"`/`" ?,"` prefixes and `'?preprocessor declaration?'` to represent macro applications/directives.
- `erl_comment_scan.erl` — scans `%`-comment lines from source into `{Line, Column, Indent, Text}` records; exports `comment/0`.
  - Exports: `file/1`, `string/1`, `scan_lines/1`, `join_lines/1`.
- `erl_recomment.erl` — attaches `erl_comment_scan` comments back onto syntax trees.
  - Exports: `recomment_forms/2`, `quick_recomment_forms/2`, `recomment_tree/2` (the `quick_*` variant tolerates comment/token mismatches, mirroring `epp_dodger`).
- `merl.erl` — metaprogramming/quasi-quotation API over `erl_syntax`; `-include_lib`s `syntax_tools/include/merl.hrl`.
  - Quoting: `quote/1`, `quote/2`, `qquote/2`, `qquote/3` (with environment substitution).
  - Templates/matching: `template/1`, `tree/1`, `subst/2`, `tsubst/2`, `alpha/2`, `match/2`, `switch/2`, `template_vars/1`, `meta_template/1`, `match_spec_run/2`.
  - Codegen helpers: `compile/1,2`, `compile_and_load/1,2`, `term/1`, `var/1`, `print/1`, `show/1`.
- `merl_transform.erl` — parse transform enabling `?Q` quasi-quotes and metavariable syntax; `-export([parse_transform/2])`.
  - Enabled implicitly by including `merl.hrl`, unless `MERL_NO_TRANSFORM` is defined before the include.
  - It partially evaluates `merl` calls at compile time; `parse_transform/2` is `-doc false`.
- `merl_tests.erl` — EUnit tests for `merl`/`merl_transform`; `-moduledoc false`, not in the app `modules` list (compiled on load), see Test Strategy.

## Syntax Tree Representation Contract
- `erl_syntax` trees are backwards-compatible with `erl_parse` "abstract format" (`syntaxTree() :: tree() | wrapper() | erl_parse()`); all `erl_parse` trees are valid syntax trees, but the reverse is not true.
- Internally a tree is a list/record carrying a `type` field plus attributes/annotations; `revert/1` maps a correct tree back to `erl_parse`.
- Guaranteed invariants (only exception to opacity): no syntax tree is a single atom (e.g. `none`), a list cons `[X | Y]`, or the empty list `[]`.
- The internal representation is deliberately **not documented and subject to change**; users must go through the ADT functions, not pattern-match the raw structure.

## Constraints / Conventions
- Treat every module in the app `modules` list as public API; write OD-consistent inline `-moduledoc`/`-doc`/`-spec` for new public functions (docs are built ExDoc-style, `../doc/`).
- Do **not** document the internal tree representation outside `erl_syntax` (see the contract above); `prettypr` is a documented public, AST-independent library — not private.
- Private/undocumented surfaces: `merl_transform:parse_transform/2` and `epp_dodger:format_error/1` (`-doc false`); `merl_tests` (`-moduledoc false`).
- Compile flags for all modules (`Makefile` `ERL_COMPILE_FLAGS`): `-Werror +warn_deprecated_catch +warn_obsolete_bool_op +warn_unused_import`. Warnings are errors — do not add newly-deprecated constructs.
- `-compile([{nowarn_possibly_unsafe_function, {erlang, list_to_atom, 1}}])` is intentional in modules that build atoms from parsed source (erl_syntax/erl_syntax_lib/epp_dodger/erl_prettypr/merl/merl_transform); do not remove without auditing.
- `nowarn_deprecated_catch` (erl_syntax, erl_syntax_lib, epp_dodger, erl_prettypr, erl_comment_scan) guards legacy `catch Expr` usage exercised by existing tests.
- `merl_transform.erl` must stay compilable with the transform **disabled** (self-hosted bootstrap) — no inline metavariables or transform features may be used in it.
- Generated files: `syntax_tools.app` and `syntax_tools.appup` are produced in `../ebin` from `*.app.src`/`*.appup.src` via `sed 's;%VSN%;$(VSN);'`; edit the `.src` files, never the generated ones. Appup restarts the whole application on any version change.

## Build Notes (special rules)
- `ERL_FILES` compiles 9 modules into `$(EBIN)` (`../ebin`) with `-o`; `INCLUDE_FILES = merl.hrl` is installed to `../include`.
- Self-applying transform bootstrap: `./merl_transform.beam` is first built from `merl_transform.erl` with `-DMERL_NO_TRANSFORM`, then `$(EBIN)/merl_transform.beam` is produced by recompiling with the transform applied (so the transform optimizes itself). `clean` removes `./merl_transform.beam`. `EXTRA_DEP_DEPENDENCIES=$(EBIN)/merl_transform.beam` feeds `$(ERL_TOP)/make/dep.mk`.
- `ERL_COMPILE_FLAGS` passes `-pa $(EBIN) -pa ./` so the bootstrap beam and `merl.hrl` resolve during compilation.

## Known Issues / Gotchas
- `epp_dodger` does not yet process feature attributes correctly: it temporarily treats `else` and `maybe` as always-keyword via a hardcoded `reserved_word/1` (see the `#7266` comment in the source). Removing these lines requires fixing feature-attribute handling first.
- The tolerant family (`quick_parse_*`, `quick_recomment_forms`) fabricates placeholder nodes instead of failing — callers must not assume well-formed trees; use the strict family when validation matters.
- `merl_tests.erl` compiles on load and depends on `merl.hrl` being resolvable at runtime; it carries no `-compile` transform flags of its own.
- Large files: `erl_syntax.erl` (8655 lines) and `erl_prettypr.erl`, `erl_syntax_lib.erl`, `merl.erl` are monolithic by design (one function per node type/field); avoid mechanical splitting that breaks the generated-style accessor ordering or doc grouping.

## Dependencies
- `{applications, [stdlib]}` (see `syntax_tools.app.src`); `{runtime_dependencies, ["compiler-9.0","erts-16.0","kernel-10.3","stdlib-8.0"]}`.
- Runtime libs used: `erl_parse`/`erl_scan` (stdlib) for parsing/`revert` compatibility, `prettypr` (in-app), `eunit` via `-include_lib` in `merl_tests.erl` only.
- No OS/external libraries; no autoconf `configure` in this app (it is pure Erlang).

## Test Strategy
- Common Test suites live in the sibling `../test/` (owner of `syntax_tools_SUITE.erl` and `merl_SUITE.erl`, plus `syntax_tools_SUITE_data/`); run with `make test` from `../` (test `Makefile` compiles suites with `+nowarn_missing_spec_documented`).
- Unit tests for `merl`/`merl_transform` also live here (`merl_tests.erl`, EUnit): it is not in the app `modules` list nor in `Makefile` `ERL_FILES`, so it is not built by `make` and not run by the CT suites — run it separately (e.g. `eunit:test(merl_tests)`) after compiling it; the `../examples/merl` tree has its own standalone EUnit programs.
- When changing `merl_transform` or the tree contract, exercise both the strict and `quick_*` (`epp_dodger`/`erl_recomment`) paths.

## See Also
- `../CONTEXT.md` — app-level layout (`Makefile`, `vsn.mk`, `include/merl.hrl`, `examples/`, `doc/`).
- `../include/merl.hrl` — the `?Q` macro header consumers include.
- `../test/` — Common Test suites (sibling; read-only from here, escalate write changes to the app root).
- `../ebin/` — build output (`*.beam`, generated `syntax_tools.app`/`.appup`).
- `../../edoc/`, `../../dialyzer/` (`cerl_prettypr`, `dialyzer_dataflow`), `../../common_test/` (`ct_doctest`, `erl2html2`) and `../../diameter/src/compiler/` — the in-tree source consumers of `erl_syntax`/`erl_prettypr`/`epp_dodger`.
- `../../compiler/` does **not** depend on `syntax_tools` in its runtime path (`compiler.app.src` lists only `[kernel, stdlib]`; no reference in `compiler/src/`) — only its `test/` suites `-include_lib` `merl.hrl`. `../../stdlib/` provides `erl_parse`/`erl_scan`/`unicode`/`file`.
