# xmerl — XML parser/generator (OTP application)

## Intent
`xmerl` ("XML Erlang") is the OTP application providing XML parsing, generation and query tooling: a DOM-style single-pass XML 1.0 scanner, a SAX2 callback parser, XPath 1.0 search, XSLT-like export rules, XML Schema (XSD) validation, and SGML/HTML/`otp_sgml` helpers.
It is a pure-Erlang library application (no C code, no autoconf, no ports) depending only on `kernel` and `stdlib`.
Build order: `lib/Makefile` lists `xmerl` under `ALL_ERLANG_APPLICATIONS`, so it is only built with `BUILD_ALL`; `lib/xmerl/Makefile` recurses into `SUB_DIRECTORIES = src doc`.

## API Surface
- `xmerl_scan` — the main XML parser: `string/1,2`, `file/1,2` return a DOM tree of `#xmlElement{}`/`#xmlDocument{}` (or `#xmerl_scanner{}` state in hook mode); customization state is read/written through `user_state/{1,2}`, `event_state/{1,2}`, `hook_state/{1,2}`, `rules_state/{1,2}`, `fetch_state/{1,2}`, `cont_state/{1,2}`.
- `xmerl_sax_parser` — SAX2 API: `file/2`, `stream/2`, events delivered to `event_fun` with `event_state`, continuations via `continuation_fun`/`default_continuation_cb/1`.
- `xmerl` — export engine: `export/2,3`, `export_content/2`, `export_element/2,3`, `export_simple/2,3`, `export_simple_element/2`, `export_simple_content/2`, `callbacks/1`.
- `xmerl_xpath` — XPath 1.0: `string/2,3,5`, helpers `eval_path/3`, `axis/3,4`, debug `write_node/1`.
- `xmerl_xs` — XSLT-like transforms over the DOM tree: `xslapply/2`, `value_of/1`, `select/2`, `built_in_rules/2`.
- `xmerl_xsd` — XSD (2nd ed., 2004-10-28) validation: `validate/2,3`, `process_validate/2,3`, `process_schema/1,2`, `process_schemas/1,2`, `state2file/1,2`, `file2state/1`, `format_error/1`, `print_table/1`.
- `xmerl_eventp` — event/stream front-ends to `xmerl_scan`: `stream/2` (DOM style), `stream_sax/4`, `file_sax/4`, `string_sax/4` (SAX style); it installs its own `continuation_fun`/`acc_fun`/`fetch_fun`/`rules`/`close_fun`, so those options cannot be overridden by the caller.
- Export back ends (tag-callback modules): `xmerl_xml`, `xmerl_xml_indent`, `xmerl_html`, `xmerl_sgml`, `xmerl_otpsgml` (inherits `xmerl_sgml`), `xmerl_text`; plus the `xmerl_xlate` `file/3`,`string/3` front-end.
- Undocumented support modules (`-moduledoc false`): `xmerl_lib`, `xmerl_validate` (DTD validation, invoked by `xmerl_scan` for `{validation,dtd}`), `xmerl_ucs`, `xmerl_uri`, `xmerl_simple`, `xmerl_b64Bin`+`xmerl_b64Bin_scan` (used only for XSD `base64Binary`), `xmerl_xpath_lib`, `xmerl_xpath_pred`, `xmerl_xpath_scan`, `xmerl_sax_old_dom`, `xmerl_sax_simple_dom`, `xmerl_xsd_type`, `xmerl_xsd_re`.
- Version/quota: `vsn.mk` → `XMERL_VSN = 2.2`; `src/xmerl.app.src` declares `{applications,[kernel,stdlib]}` and `{runtime_dependencies,["stdlib-2.5","kernel-8.4","erts-6.0"]}`.
- Public headers: `include/xmerl.hrl` (DOM + scanner/event records), `include/xmerl_xpath.hrl`, `include/xmerl_xsd.hrl` — see the `include/` node for the record contract.

## Constraints
- Generated sources are never hand-edited and are deleted by `make clean`:
  - `xmerl_xpath_parse.erl` ← `xmerl_xpath_parse.yrl`, `xmerl_b64Bin.erl` ← `xmerl_b64Bin.yrl`, `xmerl_xsd_re_parse.erl` ← `xmerl_xsd_re_parse.yrl` (yecc),
  - `xmerl_sax_parser_{list,latin1,utf8,utf16be,utf16le}.erl` ← `cat <encoding>.erlsrc xmerl_sax_parser_base.erlsrc`.
- A module addition/removal must be reflected in `MODULES` in `src/Makefile` **and** the hand-maintained `modules` list in `src/xmerl.app.src`.
- `include/xmerl.hrl` and `include/xmerl_xsd.hrl` are the public API contract: record fields are matched/constructed directly by consumers, so layout changes are breaking; extend by appending fields with defaults.
- `xmerl_scan`, `xmerl_xpath` and `xmerl_xsd_type` create atoms from document content via `list_to_atom/1` (annotated `-unsafe(...)` / `nowarn_possibly_unsafe_function`); untrusted XML can grow the atom table without bound.
- Compile flags: `-I ../include +warn_unused_vars +warn_obsolete_bool_op`; `ERL_DETERMINISTIC=yes` adds `+deterministic` plus `DEP_SKIP_FILES` for the generated files.

## Design Decisions
- Two XML front ends coexist deliberately: the older single-pass scanner (`xmerl_scan`, DOM records, pluggable `event_fun`/`hook_fun`/`acc_fun`/`fetch_fun`/`continuation_fun`/`rules_*_fun` held in the public `#xmerl_scanner{}` record) and the 2008 SAX2 parser (`xmerl_sax_parser`, callback events); `xmerl_eventp` bridges them and reuses `xmerl_scan` rather than duplicating a parser.
- The SAX state machine is generated per input encoding (shared base + per-encoding head) so the hot loop needs no runtime encoding dispatch.
- XPath is split into lexer (`xmerl_xpath_scan`) / regenerable yecc grammar (`xmerl_xpath_parse.yrl`) / predicate & core-function evaluator (`xmerl_xpath_pred`); `xmerl_xpath` is a thin driver.
- Export is a tag-callback protocol (`'#root#'/4`, `'#element#'/5`, `'#text#'/1`, … plus `'#xml-inheritance#'/0`), resolved by `xmerl:callbacks/1`; `xmerl_scan` runs DTD validation (`xmerl_validate`) or schema validation (`xmerl_xsd:process_validate/3`) after scanning.
- Documentation is attribute-driven: module pages come from `-moduledoc`/`-doc` in `src/*.erl` via the shared ExDoc build (`doc/docs.exs`); there is **no** `doc/src/` tree, and the legacy EDOC target in `src/Makefile` is inactive.

## Notes for Agents
- `ebin/` is build output (only `.gitignore` tracked).
- Commercially load-bearing in-OTP consumers: `lib/edoc` (uses `xmerl_html` as `DEFAULT_XML_EXPORT` and matches `#xmlElement{}`/`#xmlAttribute{}` in its doclets/layout/wiki), `lib/common_test` (`ct_config_xml`, `ct_netconfc`), `lib/wx/api_gen`, `lib/stdlib/src/otp_internal.erl` — treat those record layouts and `xmerl_html`'s output shape as frozen.
- `xmerl_xpath.hrl` is installed but effectively internal: its `#state{context,acc}` is included only by `xmerl_xpath_pred`, while `xmerl_xpath.erl` re-declares the same record privately; both copies must be kept identical by hand.
- Several modules (`xmerl_simple`, `xmerl_text`, `xmerl_sgml`/`xmerl_otpsgml`, `xmerl_xlate`, `xmerl_sax_old_dom`, `xmerl_sax_simple_dom`) are referenced only by tests or by no OTP code — they are user-facing legacy surface, not dead code.
- `doc/examples/xmerl_test.erl` is the master copy for the generated `test/xmerl_test.erl` — edit the doc example, never the generated copy.
- See each child CONTEXT.md for per-directory detail; the app-level `Known Issues` (stale test artifacts, duplicate corpora, hardcoded ports) are owned by the `test/` node.

## Test Strategy
- Common Test suites live in `test/` (`xmerl_SUITE`, `xmerl_std_SUITE`, `xmerl_sax_SUITE`, `xmerl_sax_std_SUITE`, `xmerl_sax_stream_SUITE`, `xmerl_xsd_SUITE` + MS/NIST/Sun XSD suites, `xmerl_app_test`/`xmerl_appup_test`), run with `make test` in `lib/xmerl` or `ct_run -dir lib/xmerl/test -suite <suite>`; they install to `$(RELEASE_PATH)/xmerl_test`.
- `xmerl_SUITE:doctests/1` executes the `erlang` code examples in the `-doc` blocks of `xmerl`, `xmerl_xpath` and `xmerl_xs`, so doc examples must stay runnable.
- W3C/IBM/OASIS/Sun/Japanese XML and MS/NIST/Sun XSD conformance corpora are vendored as tarballs under `test/*_SUITE_data/`; XSD suites compare results against `*_failed_cases.log` baselines.

## Routing Table
- `src/` → all Erlang sources, the three yecc `.yrl` grammars, the six `.erlsrc` SAX inputs, internal headers (`xmerl_internal.hrl`, `xmerl_sax_parser.hrl`, `xmerl_sax_old_dom.hrl`), `xmerl.app.src`/`xmerl.appup.src` and `src/Makefile` (module list + generated-file rules)
- `include/` → the installed public headers: `xmerl.hrl` (DOM + scanner/event records), `xmerl_xpath.hrl`, `xmerl_xsd.hrl` (write changes only via `HRL_FILES` in `../src/Makefile`)
- `test/` → Common Test suites, `xmerl_test_lib`/`xmerl_xsd_lib` helpers, `xmerl.spec`/`xmerl.cover`, and `*_SUITE_data/` conformance corpora
- `doc/` → ExDoc config (`docs.exs`), `notes.md` release notes, `guides/` (user guide + two tutorials), tutorial `assets/`, legacy standalone `examples/`
- `../edoc/` → primary in-OTP consumer of the record contract and of `xmerl_html` export (sibling — read-only, escalate writes to `lib/`)
- `../parsetools/` → provides `yecc`, which generates `xmerl_xpath_parse.erl`, `xmerl_b64Bin.erl` and `xmerl_xsd_re_parse.erl` at build time
