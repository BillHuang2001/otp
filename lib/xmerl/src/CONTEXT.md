# xmerl/src — Erlang sources of the XML application

## Intent
All Erlang sources of the `xmerl` application: two independent XML front ends (DOM scanner, SAX2 parser), the XPath 1.0 engine, the XSD validator, the export/back-end family, and the internal helper modules they share.
`src/Makefile` is the authority for the module list, generated-file rules, compile flags and the release (`release_spec`) contents.

## API Surface
Documented (EDOC) modules — `EDOC_MODULES` in `src/Makefile`, and the only modules with public `-moduledoc`:
- `xmerl_scan` — single-pass XML 1.0 scanner; `string/1,2`, `file/1,2` return `#xmlElement{}`/`#xmlDocument{}` (or `#xmerl_scanner{}` state in hook mode). Also exported: the customization-function state accessors `user_state/{1,2}`, `event_state/{1,2}`, `hook_state/{1,2}`, `rules_state/{1,2}`, `fetch_state/{1,2}`, `cont_state/{1,2}`, plus `accumulate_whitespace/4` (exported only for `xmerl_xpath_pred`).
- `xmerl` — export engine: `export/2,3`, `export_content/2`, `export_element/2,3`, `export_simple/2,3`, `export_simple_element/2`, `export_simple_content/2`, `callbacks/1`; also the `-export_type`s for all public records.
- `xmerl_xpath` — XPath 1.0: `string/2,3,5`, helpers `eval_path/3`, `axis/3,4`, debug `write_node/1`.
- `xmerl_xs` — XSLT-like transforms over the DOM tree: `xslapply/2`, `value_of/1`, `select/2`, `built_in_rules/2`.
- `xmerl_xsd` — XML Schema (2nd ed., 2004-10-28) validator/interface: `validate/2,3`, `process_validate/2,3`, `process_schema/1,2`, `process_schemas/1,2`, `state2file/1,2`, `file2state/1`, `format_error/1`, `print_table/1`.
- `xmerl_eventp` — event/stream front-ends to `xmerl_scan`: `stream/2` (DOM style), `stream_sax/4`, `file_sax/4`, `string_sax/4` (SAX style). It installs `continuation_fun`/`acc_fun`/`fetch_fun`/`rules`/`close_fun` itself, so the caller cannot override those options.
- `xmerl_sax_parser` — the public SAX2 API: `file/2`, `stream/2` and the internal `default_continuation_cb/1`; events are delivered to `event_fun` with `event_state`, continuations via `continuation_fun`.

Undocumented (`-moduledoc false`) support modules, grouped by role — see Module Map below.

## Module Map (Routing)
- DOM scanner core → `xmerl_scan.erl` (largest source, 4.3k lines), `xmerl_lib.erl` (shared record/normalization/name-char/DTD helpers), `xmerl_validate.erl` (DTD validation called from `xmerl_scan` when `{validation,dtd}`)
- SAX family → `xmerl_sax_parser.erl` (API + encoding dispatch + option/state handling), `xmerl_sax_parser_base.erlsrc` (the shared 5k-line state machine), `xmerl_sax_parser_{list,latin1,utf8,utf16be,utf16le}.erlsrc` (per-encoding heads), `xmerl_sax_old_dom.erl` + `xmerl_sax_simple_dom.erl` (optional SAX→DOM event-callback modules, referenced only by `../test/xmerl_sax_SUITE`)
- XPath family → `xmerl_xpath.erl` (API + evaluator driver), `xmerl_xpath_scan.erl` (lexer, `tokens/1`), `xmerl_xpath_parse.yrl` (yecc grammar), `xmerl_xpath_pred.erl` (XPath core function library + predicate evaluation, `eval/2`), `xmerl_xpath_lib.erl` (`eval/3` helper)
- XSD family → `xmerl_xsd.erl` (interface + whole validation engine, 5.7k lines), `xmerl_xsd_type.erl` (simple-type facets/constraints, 1.6k lines), `xmerl_xsd_re.erl` (`map/1`, XSD regexp engine for the `pattern` facet) + `xmerl_xsd_re_parse.yrl` (regex grammar)
- Export back ends (tag-callback modules) → `xmerl_xml.erl`, `xmerl_xml_indent.erl`, `xmerl_html.erl`, `xmerl_sgml.erl`, `xmerl_otpsgml.erl` (inherits `xmerl_sgml`), `xmerl_text.erl`, `xmerl_xlate.erl` (small `file/3`,`string/3` front-end, not a callback module)
- Encoding/format helpers → `xmerl_ucs.erl` (UCS-2/4, UTF-8/16, ISO-8859-1/9 conversions), `xmerl_uri.erl` (RFC 2396 URI parse/resolve, used by `xmerl_scan` for xmlbase/SystemId and by `xmerl_xsd_type` for `anyURI`), `xmerl_b64Bin.yrl` + `xmerl_b64Bin_scan.erl` (base64 binary lexer/parser, used only by `xmerl_xsd_type` for XSD `base64Binary`), `xmerl_simple.erl` (unused-in-OTP front-end returning simple-form content via an `event_fun`)
- Internal headers → `xmerl_internal.hrl` (debug macros `?dbg`/`?DBG` only, no records), `xmerl_sax_parser.hrl` (`#xmerl_sax_parser_state{}`), `xmerl_sax_old_dom.hrl` (private DOM record set used by `xmerl_sax_old_dom` and `xmerl_sax_simple_dom`)
- App metadata → `xmerl.app.src` (`modules` list is maintained by hand, not generated), `xmerl.appup.src` — both get `%VSN%` → `XMERL_VSN` substitution at build time

## Constraints
- Never hand-edit generated sources; `make clean` deletes and the Makefile rebuilds them:
  - `xmerl_xpath_parse.erl` ← `xmerl_xpath_parse.yrl`, `xmerl_b64Bin.erl` ← `xmerl_b64Bin.yrl`, `xmerl_xsd_re_parse.erl` ← `xmerl_xsd_re_parse.yrl` (all via `$(ERLC)`/yecc into `$(ESRC)`),
  - `xmerl_sax_parser_{list,latin1,utf8,utf16be,utf16le}.erl` ← `cat xmerl_sax_parser_<enc>.erlsrc xmerl_sax_parser_base.erlsrc`; edit the `.erlsrc` halves only, and keep the encoding head emitting the macros the base expects (`?STRING`, `?PARSE_XML_DECL`, `?BYTE_ORDER_MARK_*`, …).
- Adding/removing a module requires updating **both** `MODULES` in `src/Makefile` and the `modules` list in `xmerl.app.src` (nothing generates the latter); `EDOC_MODULES` applies to anything meant to appear in the public docs.
- Public headers are owned by the sibling `../include/` node (`xmerl.hrl`, `xmerl_xpath.hrl`, `xmerl_xsd.hrl`) and are reachable here only through `ERL_COMPILE_FLAGS += -I ../include`; record layout changes there are breaking for consumers (`edoc` etc.) — escalate such edits to `../include/`, do not add private duplicates.
- `ERL_COMPILE_FLAGS += -I ../include +warn_unused_vars +warn_obsolete_bool_op`; with `ERL_DETERMINISTIC=yes` the build adds `+deterministic` and lists the 8 generated files in `DEP_SKIP_FILES` (they are not reproducible-stable).
- All name-creating code paths go through `list_to_atom/1` and are therefore guarded with `-compile({nowarn_possibly_unsafe_function, {erlang, list_to_atom, 1}})` and/or `-unsafe([...])`; keep those annotations when touching the modules, they are a deliberate audit trail.
- `-doc hidden` + `-compile({nowarn_hidden_doc, ...})` mark exported-but-opaque types (`xmerl_scanner/0` in `xmerl_scan`, `xsd_state/0` in `xmerl_xsd`) so EDOC does not document them.

## Design Decisions
- Two front ends coexist: the older single-pass scanner (`xmerl_scan`, DOM records, customization functions) and the 2008 SAX2 parser (`xmerl_sax_parser`, callback events); `xmerl_eventp` bridges scanner-driven streaming and SAX-style processing without duplicating the scanner.
- The scanner is driven by a `#xmerl_scanner{}` record (public, in `../include/xmerl.hrl`) holding pluggable `event_fun`/`hook_fun`/`acc_fun`/`fetch_fun`/`close_fun`/`continuation_fun`/`rules_*_fun`, each with its own sub-state in `#xmerl_fun_states{}` reachable through the `*_state/1,2` accessors — this is what lets `xmerl_eventp`, `xmerl_simple` and `xmerl_xlate` reuse the parser for streaming/SAX/simple-form with a few lines each.
- The SAX parser is generated per encoding (head + shared base concatenated) so the hot state machine has no runtime encoding dispatch; `xmerl_sax_parser:parse_binary/3` pattern-matches on the detected `{encoding, …}` option and calls `xmerl_sax_parser_utf8:F/2` etc.
- XPath is split into lexer (`xmerl_xpath_scan`) / regenerable yecc grammar (`xmerl_xpath_parse.yrl`) / predicate & core-function evaluator (`xmerl_xpath_pred`) so `xmerl_xpath` stays a thin driver and the grammar can be regenerated (`yecc:yecc/4` on the `.yrl`).
- `xmerl_xpath_pred` exports its core functions (`concat/2`, `count/2`, `substring/2`, …) because the grammar dispatches them by name through `apply/3` via `core_function/1`; the unusual export list is not an API surface.
- Export is a callback-module protocol: a back end exports a function per tag (`'#root#'/4`, `'#element#'/5`, `'#text#'/1`, `'#cdata#'/1`, `'#comment#'/1`) plus `'#xml-inheritance#'/0`; `xmerl:callbacks/1` resolves inheritance (only `xmerl_otpsgml` → `xmerl_sgml` is non-empty) and `xmerl:tagdef/...` walks the chain, handling `{'#xml-alias#', NewTag}` and `{'#xml-redefine#', Content}` results.
- XSD validation lives entirely in two big modules: `xmerl_xsd` (schema processing + element validation, error accumulation via `format_error/1`) and `xmerl_xsd_type` (simple-type facets, XSD `duration`/`dateTime` arithmetic such as `fQuotient/2`, `modulo/2`, `compare_durations/2`), with `xmerl_xsd_re` for `pattern` regexps.
- `xmerl_scan` integrates both validators: with `{validation,dtd}` it calls `xmerl_validate:validate/2` after the document is scanned, and with `{validation,schema}` it resolves `schemaLocation` and calls `xmerl_xsd:process_validate/3`.

## Known Issues
- `xmerl_scan` (and `xmerl_xpath`/`xmerl_xsd_type`) create atoms from document content (`list_to_atom/1` on tag/attribute/namespace names): parsing attacker-controlled XML grows the atom table without bound and can exhaust the VM.
- The `clean` target does not remove `xmerl_xsd_re_parse.erl`, so that generated file can survive a `make clean` and be picked up by a stale build; remove it manually when the regex grammar changes.
- `release_spec` installs only `xmerl_xpath_parse.yrl` and `xmerl_b64Bin.yrl` (plus `.erl`, internal `.hrl`, `.app.src`, `.appup.src`); `xmerl_xsd_re_parse.yrl` and all `*.erlsrc` files are absent from an installed release, so the SAX parser and XSD regex never get rebuilt from a release tree.
- `xmerl_xpath.erl` re-declares `#state{context, acc}` locally instead of including the identically-defined `../include/xmerl_xpath.hrl`; that header is used only by `xmerl_xpath_pred.erl`, so the two copies must be kept manually identical.
- `xmerl_simple`, `xmerl_sax_old_dom`, `xmerl_sax_simple_dom`, `xmerl_html`, `xmerl_text`, `xmerl_sgml`/`xmerl_otpsgml` and `xmerl_xlate` are not referenced by any other OTP application (only `../test/xmerl_sax_SUITE` uses `xmerl_sax_old_dom`); they are user-facing legacy surface, so treat them as frozen rather than dead code.
- `xmerl_internal.hrl` defines only the `?dbg`/`?DBG` macros (a no-op unless `debug` is defined) despite its "Records" section header — it is included by most modules for those macros, not for types.

## Notes for Agents
- `ebin/` and the generated `*.erl` are build output — never edit or commit them; the tracks-in-git sources are the 27 hand-written `.erl` files, 6 `.erlsrc`, 3 `.yrl`, 3 internal `.hrl`, `Makefile`, `xmerl.app.src`, `xmerl.appup.src`.
- `xmerl.hrl` is found via `-I ../include`, so `-include("xmerl.hrl")` in a source here refers to the public header — do not "fix" those paths.
- The `.erlsrc` files have no licence header of their own beyond the base; the generated `.erl` inherits the head's header, which is why encoding heads start with a full copyright block.
- `xmerl_ucs` predates the `unicode` module and is still the conversion layer used by `xmerl_scan`/`xmerl_lib`; do not assume `unicode` is used for all charset handling.

## Dependencies
- Application dependencies are only `{applications,[kernel,stdlib]}` with `{runtime_dependencies,["stdlib-2.5","kernel-8.4","erts-6.0"]}` (`xmerl.app.src`); no C code, no autoconf, no ports.
- Build-time only: yecc/`erlc` (from `../parsetools`/`../compiler`) for the three `.yrl` grammars, and EDOC (`../../edoc`) with `syntax_tools` and `xmerl` itself on the code path (`EDOC_PATHS`) for the commented-out `$(DOCDIR)/%.html` rule.
- `xmerl_scan` and `xmerl_xsd` use `kernel/include/file.hrl` (file info for size/time checks during fetch).

## Test Strategy
- Tests live in the sibling `../test/` node (Common Test: `xmerl_SUITE`, `xmerl_std_SUITE`, `xmerl_sax_SUITE`, `xmerl_sax_stream_SUITE`, `xmerl_xsd_SUITE` + MS/NIST/Sun XSD suites) — read-only from here; escalate test changes to that node.
- `xmerl_SUITE:doctests/1` runs `ct_doctest:module/2` over `xmerl`, `xmerl_xpath` and `xmerl_xs`, so every ```` ```erlang ```` example inside their `-doc` blocks is executable and must stay correct.
- W3C/OASIS conformance corpora are vendored under `../test/*_SUITE_data/`; the `.erlsrc` and grammar regressions surface there, not in `src/`.

## See Also
- `../include/` — public headers (`xmerl.hrl` DOM/scanner records, `xmerl_xpath.hrl`, `xmerl_xsd.hrl`); the owner of the record contract, read-only from here.
- `../test/` — Common Test suites and conformance corpora (sibling — read-only, escalate test writes upwards).
- `../doc/` — `docs.exs`, guides and the XML tutorial these `-moduledoc`s reference.
- `../CONTEXT.md` — application-level overview (version `XMERL_VSN = 2.2`, consumers `edoc`/`common_test`/`wx`/`stdlib`).
