# diameter — Dictionary Compiler

## Intent
Compiles a Diameter dictionary written in the `.dia` DSL into an Erlang encode/decode module `diameter_gen_<dict>` (a `.erl` plus a `.hrl`) that implements the interface the runtime needs.
This is build-time / offline tooling: the compiler modules run while dictionaries are being generated and are NOT part of a running Diameter node.
It is driven both by the build (`../Makefile` rule running `escript ../../bin/diameterc -o gen -i $(EBIN) dict/<dict>.dia`) and directly by users via `diameter_make:codec/2` or `diameterc(1)`.

## API Surface
Only `diameter_make` is public/stable API (has a real `-moduledoc`, documented in `../../doc/references/diameterc_cmd.md`):
- `diameter_make:codec/1,2` — compile a dictionary; input is a path, a literal dictionary, `{path,File}`, or an already-parsed `{dict,ParseD}`; a literal is recognized by the presence of NL/CR in the argument.
- `diameter_make:flatten/1` — return the parsed dictionary with `@inherits` folded in, i.e. all inherited AVP/grouped/enum/`@custom_types`/`@codecs` definitions copied locally.
- `diameter_make:format/1` — render a parsed dictionary back to `.dia` text.
- `diameter_make:format_error/1` — render a `{error,Reason}` from `codec/2` as a string (delegates to `diameter_dict_util`).
- `diameter_make:opt/0` — the option type: `{include|outdir|name|prefix|inherits,string()} | return | verbose | parse | forms | erl | hrl` (plus `indirect_inherits`).
Internal (all `-moduledoc false`, no stability guarantee; `CT_MODULES` in `../modules.mk`):
- `diameter_dict_util` — dictionary semantics: `parse/2` (the real pipeline entry), `format/1`, `format_error/1`; owns AVP/type inheritance, section explosion, validation and error messages.
- `diameter_dict_scanner` — handwritten tokenizer (`scan/1`, `is_name/1`, `format_error/1`), NOT a leex-generated scanner.
- `diameter_dict_parser` — the yecc grammar (`diameter_dict_parser.yrl` compiled into `../gen/diameter_dict_parser.erl`); a `DICT_YRL` member and therefore also a `COMPILER_MODULES` member.
- `diameter_codegen` — abstract-form generation: `from_dict/4` (the `parse|forms|erl|hrl` back end) and `is_printable_ascii/1`.
- `diameter_exprecs` — parse transform (`parse_transform/2`) that expands the `-export_records` attribute of generated modules into `'#new-'`, `'#get-'`, `'#set-'`, `'#info-'` record accessors.
Private headers (listed under `INTERNAL_HRLS` in `../modules.mk`, released only for private use):
- `diameter_forms.hrl` — macros (`?ATOM`, `?clause`, `?TERM`, `?BADARG`, …) for hand-building abstract forms; used by `diameter_codegen` and `diameter_exprecs`.
- `diameter_vsn.hrl` — `?VERSION` = `1`, the format version tag of the `dict/0` return value.
DSL sections the pipeline understands (grammar/scanner are the authority): `@name`, `@prefix`, `@id`, `@vendor`, `@inherits`, `@avp_types`, `@avp_vendor_id`, `@custom_types`, `@codecs`, `@grouped`, `@messages`, `@enum`, `@define`, `@end` (terminates parsing early).

## Compilation Pipeline
1. `diameter_make:codec/2` → `identify/1` splits path vs. literal and vs. `{dict,…}`, then calls `diameter_dict_util:parse/2`.
2. Read: `{path,Path}` is read from file; anything else is treated as an in-memory iolist/binary.
3. Scan: `diameter_dict_scanner:scan/1` → `{word,L,Name} | {number,L,N} | {Symbol,L} | '$end'`.
4. Parse: the generated yecc parser produces a nested `[Section | Body]` term.
5. Semantic model: `diameter_dict_util:make_dict/2` = `reset/2` (apply `name|prefix|inherits` options) → `pass1/1` (explode sections, semantic checks) → `pass2/1` (AVP uniqueness keyed on `{Code,VendorId}`) → `pass3/2` (`command_codes`, `import_inherits`, `import_avps`, `import_groups`, `import_enums`) → `examine/1` (referential sanity) → `make_orddict/1`, the orddict handed to codegen.
6. Inheritance: `enter` of `pass3` loads each `@inherits` module and calls `Mod:dict()`; this is why `@inherits` creates a BEAM (not erl/hrl) dependency.
7. Codegen: `diameter_codegen:from_dict/4` generates per requested mode — `parse` (`.D`, the `[?VERSION|ParseD]` term), `forms` (`.F`, preprocessed abstract forms ready for `compile:forms/2`), `erl`, `hrl` — writing to `outdir` or returning the values when the `return` option is given.
8. `.erl` is pretty-printed from abstract forms by `erl_prettypr`; `.hrl` contains message/grouped-AVP records plus enum/define macros.

## Constraints
- No compile-time dependency on the running protocol stack: `diameter_dict_util` must not call `diameter_types` (it re-implements the Unsigned32 range check locally) because generated dictionaries may be compiled before the app is built.
- Compiler modules are compiled into `ebin/` but deliberately excluded from the generated `.app` `modules` list — see the commented `%,%COMPILER%` line in `../diameter.app.src`; only runtime dictionaries (`APP_MODULES = RT_MODULES + DICT_MODULES`) are listed.
- Never edit `../gen/diameter_dict_parser.erl` (generated); change `diameter_dict_parser.yrl` instead — it is compiled with `-Werror $(DETERMINISM_FLAG)` (`+deterministic` when `ERL_DETERMINISTIC=yes`).
- Adding or renaming a DSL section requires changing BOTH `diameter_dict_scanner:section/1` (its reserved-word list) and the `.yrl` terminals/nonterminals.
- `diameter_forms.hrl`.?TERM/1 encodes the printable-ASCII check via `fun diameter_codegen:is_printable_ascii/1`, so `diameter_exprecs` (which uses ?TERM) requires `diameter_codegen` to be loadable while the transform runs.
- Inheritance resolution requires the parent dictionary's beam to be on the code path: `{include,Dir}` (equivalently `-i dir`) calls `code:add_pathsa/1`; without it compilation fails with `not_loaded`/`no_dict`.
- The `?VERSION` tag in `diameter_vsn.hrl` must match the inherited beam's; a mismatch reports `recompile` (or raises `{version,{Vsn,?VERSION}}` for a passed-in parsed dict).

## Design Decisions
- Codegen builds abstract forms (via `diameter_forms.hrl`) rather than text, so the same representation serves the `forms` mode (`compile:forms/2`) and the printed `.erl`/`.hrl`.
- Generated modules carry `dict/0` returning the entire parsed orddict tagged with `?VERSION`, which turns dictionary inheritance into a module/beam dependency and lets the compiler re-derive a parent's definitions exactly.
- `diameter_exprecs` is generated into the produced module (`-compile({parse_transform, diameter_exprecs})` plus `-export_records`) so records can be exported without forcing consumers to `-include` the `.hrl`, i.e. to avoid compile-time dependencies on the dictionary.
- Generated modules delegate the hard work to the runtime: the emitted `.erl` includes `diameter/include/diameter_gen.hrl`, whose `encode_avps/3`/`decode_avps/3`/`grouped_avp/4` call `diameter_gen`, while the dictionary-specific data comes from generated `avp_name/2`, `avp_header/1`, `avp_arity/1,2`, `avp/4`, `msg_header/1`, `enumerated_avp/3`, `empty_value/2`, ….
- Base-type encoding is emitted as calls to `diameter_types:<Type>(…)` (`../base/diameter_types.erl`), with `DiamIdent`/`DiamURI` mapped to their dedicated functions.
- `forms` mode does its own include handling (`preprocess/2`): it pulls `include_lib` contents out of the compiled `diameter_gen_relay` beam's abstract code and renames `diameter_gen_relay` atoms, because `compile:forms/2` performs no preprocessing.
- `indirect_inherits` makes inheritance transitive: `get_all_inherits/1` walks the whole ancestor chain and `inherit_enums_for_defined_avps/4` merges enums for locally re-declared AVPs, so a dictionary need not list every ancestor.
- Imported AVPs take their vendor id from the *inheriting* dictionary's `@avp_vendor_id`/`@vendor` (or the inherited dictionary's `@vendor`); `@avp_vendor_id` in the inherited dictionary is deliberately ignored when importing.

## Known Issues / Notes for Agents
- Unrecognized options passed to `codec/2` are silently ignored (documented BUGS in the `diameter_make` moduledoc), so a typo'd option yields a silent behaviour change, not an error.
- `forms` mode needs a compiled `diameter_gen_relay` beam built with `debug_info` on the code path; otherwise it fails with `{error, no_abstract_code}`.
- `flatten/1` and `format/1` expect the `[?VERSION|Dict]` tagged list produced by `parse`/`codec` mode; anything else raises `{version,{Vsn,?VERSION}}`.
- Verbose reporting is kept in the process dictionary (`putr`/`getr`/`eraser` in both `diameter_dict_util` and `diameter_codegen`), set and cleared per top-level call.
- All compiler error terms flow through `diameter_dict_util:format_error/1`; adding a new error means adding a `fmt/1` clause there.
- `diameter_dict_util:parse/2` returns the orddict only after `examine/1`; a dictionary can parse cleanly and still fail reference checks (e.g. `avp_not_defined`, `grouped_avp_not_grouped`, `enumerated_avp_has_wrong_local_type`).
- `@end` is a scanner-level token that maps to the yecc `'$end'` symbol, so it truncates parsing rather than being a section.

## Test Strategy
Compiler behaviour is covered by Common Test suites in the sibling directory `../../test/`: `diameter_compiler_SUITE.erl` (format/replace/generate/flatten), `diameter_codegen_SUITE.erl` (generated-code correctness across inheritance chains), and `diameter_indirect_inherits_SUITE.erl` (the `indirect_inherits` option); `diameter_compiler_SUITE:run/0` and `diameter_codegen_SUITE:run/0` also run standalone without Common Test.

## See Also
- `../modules.mk` and `../Makefile` — module classification (`CT_MODULES`, `DICT_YRL`, `INTERNAL_HRLS`) and the dictionary/yecc build rules.
- `../gen/` — build output for the parser and for every generated `diameter_gen_*`.
- `../dict/*.dia` — the in-tree dictionaries compiled by this directory's codegen.
- `../base/diameter_gen.erl` — runtime encode/decode dispatch that the generated code calls.
- `../base/diameter_types.erl` — base AVP type codecs referenced from generated `avp/4` clauses.
- `../../include/diameter_gen.hrl` — code included by every generated module.
- `../../bin/diameterc` — escript front end mapping CLI flags onto `diameter_make:codec/2` options.
- `../../doc/references/diameter_dict.md` and `../../doc/references/diameterc_cmd.md` — user documentation of the DSL and CLI.
