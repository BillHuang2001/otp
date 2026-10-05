# xmerl — XML Parser/Generator (OTP application)

## Intent
`xmerl` ("XML Erlang") is the OTP application providing an XML parser, generator and query toolbox: a DOM-style single-pass XML 1.0 scanner, a SAX2 callback parser, XPath 1.0 search, XSLT-like export templates, XML Schema (XSD) validation, and SGML/HTML/`otp_sgml` helpers.
It is a pure-Erlang library application (no C code, no autoconf) that sits above `kernel`/`stdlib` only.

## API Surface
- `src/xmerl_scan` — the documented public XML parser entry point: `string/1,2`, `file/1,2` return the DOM-tree of `#xmlElement{}`/`#xmlDocument{}` records defined in `include/xmerl.hrl`.
  Customization hooks are passed as options and read back with `user_state/1,2`, `event_state/1,2`, `hook_state/1,2`, `rules_state/1,2`, `fetch_state/1,2`, `cont_state/1,2`.
- `src/xmerl_sax_parser` — SAX2-style event parser: `file/2`, `stream/2` with an `event_fun`/`event_state` callback interface, continuations via `default_continuation_cb/1`.
- `src/xmerl` — export engine: `export/2` plus record constructors/accessors (`#xmlElement{}`-family builders) used by all backends.
- `src/xmerl_xpath` — XPath 1.0 search: `string/2,3,5`, helper `eval_path/3`, `axis/3,4`, debug `write_node/1`.
- `src/xmerl_xs` — XSLT-like template/apply rules (`xslapply/2`, `value_of/1`, `select/2`, `built_in_rules/2`) layered on the DOM tree.
- `src/xmerl_xsd` — XML Schema validation: `validate/2,3`, `process_validate/2,3`, `process_schema/2,3`, `print_table/1`.
- `src/xmerl_validate` — DTD/`xmerl_sgml`-era validation (`validate/2`, internal, `-moduledoc false`).
- `src/xmerl_eventp` — convenience "export as event" wrapper (`file_sax/3,4`, `string/2`, `stream/2`) around `xmerl_scan`/`xmerl_sax_parser`.
- Export backends (used via `xmerl:export/2` or their `export/2`): `xmerl_xml` (plain XML), `xmerl_xml_indent` (pretty-printed XML), `xmerl_html`, `xmerl_otpsgml`, `xmerl_sgml`, `xmerl_text`, `xmerl_xlate`.
- Internal support modules (`-moduledoc false`): `xmerl_lib` (record/accessor + DTD helpers), `xmerl_ucs` (Unicode/UCS code conversion), `xmerl_uri` (URI parsing, RFC 2396 style), `xmerl_simple`, `xmerl_b64Bin`/`xmerl_b64Bin_scan` (base64/binary to text conversion), `xmerl_xpath_lib`, `xmerl_xpath_pred`, `xmerl_xpath_scan`, `xmerl_sax_simple_dom`, `xmerl_sax_old_dom`, `xmerl_xsd_type`, `xmerl_xsd_re`.
- `include/xmerl.hrl` — the public record contract: `#xmlElement{}`, `#xmlAttribute{}`, `#xmlText{}`, `#xmlComment{}`, `#xmlPI{}`, `#xmlDocument{}`, `#xmlNamespace{}`, `#xmlDecl{}`, `#xmlContext{}`, plus scanner/state records; also `include/xmerl_xpath.hrl` and `include/xmerl_xsd.hrl` (XSD schema records).
- Public modules documented in `doc/` (per `src/Makefile` `EDOC_MODULES`): `xmerl_scan`, `xmerl`, `xmerl_xs`, `xmerl_eventp`, `xmerl_xpath`, `xmerl_xsd`; `xmerl_sax_parser` is documented via its `-moduledoc`.
- Version: `vsn.mk` → `XMERL_VSN = 2.2`; app resource `src/xmerl.app.src` declares `{applications,[kernel,stdlib]}` and `{runtime_dependencies,["stdlib-2.5","kernel-8.4","erts-6.0"]}`.

## Constraints
- Never hand-edit generated sources — they are produced at build time and removed by `make clean`:
  - `xmerl_xpath_parse.erl` ← `xmerl_xpath_parse.yrl` (yecc),
  - `xmerl_b64Bin.erl` ← `xmerl_b64Bin.yrl` (yecc),
  - `xmerl_xsd_re_parse.erl` ← `xmerl_xsd_re_parse.yrl` (yecc),
  - `xmerl_sax_parser_{list,latin1,utf8,utf16be,utf16le}.erl` ← the encoding-specific `*.erlsrc` concatenated with the shared `xmerl_sax_parser_base.erlsrc` (edit the `.erlsrc` halves, never the generated concatenation).
- Any change to the record layouts in `include/xmerl.hrl` is a breaking API change for every consumer (notably `edoc`) — records are `-include`d directly by user code.
- Strict compiles: `-I ../include +warn_unused_vars +warn_obsolete_bool_op`; `ERL_DETERMINISTIC=yes` adds `+deterministic` and lists generated files in `DEP_SKIP_FILES` (they are not reproducible-stable).
- `xmerl_scan` and `xmerl_xpath` are annotated `-unsafe([{'_','_',possibly}])` and use `list_to_atom/1` with `nowarn_possibly_unsafe_function` — external input is not trusted to be atom-safe; keep that in mind when touching atom creation.
- Export/validation must keep the `#xmlElement{}` shape stable; `xmerl_xsd` works off `include/xmerl_xsd.hrl` records.

## Design Decisions
- Two independent XML front ends: the older single-pass scanner (`xmerl_scan`, event/hook/continuation customization functions) producing DOM records, and the newer SAX parser (`xmerl_sax_parser`, 2008) with a callback interface; both are kept, and `xmerl_eventp` bridges scanner-driven event streaming.
- XPath is split lexer (`xmerl_xpath_scan`) / yecc grammar (`xmerl_xpath_parse.yrl`) / evaluator (`xmerl_xpath_pred`) so the grammar is regenerable and `xmerl_xpath` stays a thin API.
- The SAX scanner is generated per input encoding by concatenating the shared state machine (`xmerl_sax_parser_base.erlsrc`) with a small per-encoding head, avoiding runtime encoding dispatch.
- Documentation for the app is built from the `.erl` `-moduledoc`/`-doc` attributes plus `doc/*.md` and the guides, driven by `doc/docs.exs`, not from a separate doc source tree (`doc/` has no `src/`).

## Known Issues
- `xmerl.spec`/`xmerl.cover`/`testcases.dtd` in `test/` are stale legacy references from the pre-Common-Test test harness kept only because `test/Makefile` installs them.

## Notes for Agents
- `ebin/` is build output (only `.gitignore` tracked) — ignore it.
- `src/Makefile` is the source of truth for the module list, `HRL_FILES`, generated-file rules and the `MODULES`/`EDOC_MODULES` split; when adding a module, update `src/Makefile` and `src/xmerl.app.src`.
- The yecc grammars for XPath and XSD regular expressions are *grammar files*, not code to patch by hand; grammar changes are regenerated by `erlc`.
- `doc/assets/*.txt|*.xml` are example inputs for the XML-extraction tutorial, not test fixtures; real fixtures live under `test/*_SUITE_data/`.
- Three consumers matter for compatibility: `lib/edoc` (doclet/layout/wiki use `xmerl_scan` + `xmerl` export + `xmerl.hrl` records), `lib/common_test` (`ct_config_xml`, `ct_netconfc`), `lib/wx/api_gen` (doc scraping); `lib/stdlib/src/otp_internal.erl` also reads XML.

## Test Strategy
- Common Test suites in `test/` run through `test/Makefile` (`MODULES` + `SUITE_FILES`), e.g. `xmerl_SUITE`, `xmerl_std_SUITE`, `xmerl_sax_SUITE`, `xmerl_sax_stream_SUITE`, `xmerl_xsd_SUITE`.
- Large conformance corpora are vendored as tarballs in `test/*_SUITE_data/` (W3C XML conformance `xmlts20130923.tar.gz`, IBM/OASIS/Sun/Japanese `*.tgz`, MS 2002-01-16 and NIST 2002-01-16 XSD suites); these make `xmerl_std_SUITE`, `xmerl_sax_std_SUITE` and the MS/NIST suites very large files.
- `xmerl_test.erl` is copied from `../doc/examples/xmerl_test.erl` by the `test/Makefile` (do not edit the generated copy).
- `xmerl.cover` deletes/ignores coverage for helper modules; keep it in sync with `MODULES` changes.

## Routing Table
- `src/` → all Erlang sources, yecc grammars (`.yrl`), generated-source inputs (`.erlsrc`), `xmerl.app.src`/`xmerl.appup.src`, and `src/Makefile` (module + generated-file rules)
- `include/` → public headers: `xmerl.hrl` (DOM/scanner records), `xmerl_xpath.hrl`, `xmerl_xsd.hrl` (XSD records)
- `test/` → Common Test suites, `xmerl_test_lib`/`xmerl_xsd_lib` helpers, `xmerl.spec`/`xmerl.cover`, and `*_SUITE_data/` conformance corpora (W3C/IBM/OASIS/Sun/MS/NIST)
- `doc/` → documentation config (`docs.exs`), `notes.md`, `guides/` (user guide + examples) and `assets/`/`examples/` sample XML and sample programs
- See also: `../edoc/` (primary in-OTP consumer of the records and export API), `../parsetools/` (provides `yecc` used to generate the parsers)
