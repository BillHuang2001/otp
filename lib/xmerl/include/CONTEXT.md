# xmerl/include — Public Headers (record & macro contract)

## Intent
Installed, hand-written headers of the `xmerl` application: they define the record and macro contract that xmerl itself and external consumers (`lib/edoc`, `lib/wx/api_gen`, `lib/common_test`) `-include` directly.
The three files are the `HRL_FILES` list of `../src/Makefile` and are the only xmerl sources installed into `$(RELSYSDIR)/include`; everything else in `../src/` is private to the application.

## API Surface
### `xmerl.hrl` — DOM + scanner/event contract (the main public header)
- DOM node records produced by `xmerl_scan:string/1,2` / `file/1,2` and consumed by `xmerl:export/2`: `#xmlElement{}`, `#xmlAttribute{}`, `#xmlText{}`, `#xmlComment{}`, `#xmlPI{}`, `#xmlNsNode{}`, `#xmlNamespace{}`, `#xmlDecl{}`, plus the root wrapper `#xmlDocument{}` (single field `content`).
- API-critical `#xmlElement{}` fields users pattern-match: `name`, `expanded_name`, `nsinfo`, `namespace`, `parents`, `pos`, `attributes`, `content`, `language`, `xmlbase`, `elementdef` (edoc's doclets/layout/wiki do exactly this).
- Other fields commonly matched by consumers: `#xmlAttribute{.name,.value,.nsinfo}`, `#xmlNsNode{.prefix,.uri}`, `#xmlText{.value,.type}` (`text` | `cdata`), `#xmlPI{.name,.value}`.
- XPath support records: `#xmlObj{}` (the `type`/`value` scalar-or-nodeset wrapper), `#xmlNode{}` (a node-set entry: `type`, `node`, `parents`, `pos`), `#xmlContext{}` (evaluation context: `context_node`, `context_position`, `nodeset`, `bindings`, `functions`, `namespace`, `axis_type`, `whole_document`) — used by `xmerl_xpath`, `xmerl_xpath_lib`, `xmerl_xpath_pred`.
- Scanner/event records: `#xmerl_scanner{}` (the state passed to and returned from every user callback) with its sub-record `#xmerl_fun_states{}`, and `#xmerl_event{}` (`event`, `line`, `col`, `pos`, `data`) passed to `event_fun` hooks.
- Macros: `?space`/`?cr`/`?lf`/`?tab` and `?whitespace/1` (used throughout xmerl and by callers), `?non_character/2`, `?non_ascii/1`, plus scanner-internal expansion macros `?strip1`–`?strip10`, `?condstrip1`–`?condstrip4`, `?bump_col/1`.

### `xmerl_xpath.hrl` — XPath evaluator state
- One record only: `#state{context = #xmlContext{}, acc = []}`, included by `../src/xmerl_xpath_pred.erl`.
- The same record is re-declared inline in `../src/xmerl_xpath.erl`, so this header is not the single source of truth for that record and is effectively internal despite being installed.

### `xmerl_xsd.hrl` — XML Schema record set
- Namespace macros `?XSD_NAMESPACE` / `?XSD_INSTANCE_NAMESPACE` (atoms) and `?debug/2` (activates only when `DEBUG` is defined).
- Validator state `#xsd_state{}`: schema name/version, `scope`, `table` (the ETS symbol table), `targetNamespace`, namespace-node lists, error accumulator, `fetch_fun`/`fetch_path`, `xml_options` — it is the public state of `xmerl_xsd:validate/3`, `process_validate/2,3` and is persisted by `state2file`/`file2state`.
- Schema component records: `#schema{}`, `#schema_element{}`, `#schema_simple_type{}`, `#schema_complex_type{}`, `#schema_attribute{}`, `#schema_attribute_group{}`, `#schema_anyAttribute{}`, `#schema_group{}`, `#schema_extension{}`, `#schema_restriction{}`, `#schema_list{}`.
- Constraint/content-model records: `#id_constraint{}` and the content-model pair `#chain{}` / `#alternative{}`.
- Consumed by `../src/xmerl_xsd.erl`, `../src/xmerl_xsd_type.erl`, `../src/xmerl_lib.erl` and the XSD conformance suites in `../test/`.

## Constraints
- This is the public API: adding, removing, renaming or reordering record fields breaks every consumer that `-include_lib("xmerl/include/xmerl.hrl")`; extend by appending fields with defaults.
- Field defaults are part of the contract — code constructs bare records and relies on defaults such as empty `content`/`attributes`/`parents`, `namespace = #xmlNamespace{}`, `elementdef = undeclared`, `#xmlText.type = text`.
- `#xmerl_scanner{}` is public only because callbacks receive and must return it; `user_state` (with `xmerl_scan:user_state/1,2`) is the supported place for callback-owned data, and the remaining fields are scanner internals.
- The `?stripN`/`?condstripN`/`?bump_col` macros expand to uses of `strip/2`, `condstrip/3`, `continuation_fun` and `?dbg` that exist only inside `../src/xmerl_scan.erl` (plus `xmerl_internal.hrl` for `?dbg`) — despite living in the public header they are usable only from that module.
- Internal-only headers are NOT installed and must stay in `../src/` (`xmerl_internal.hrl`, `xmerl_sax_parser.hrl`, `xmerl_sax_old_dom.hrl`); the SAX parser is self-contained and does not depend on `xmerl.hrl`.
- `xmerl_xpath.hrl` declares the very generic record name `#state{}`; a module defining its own `#state{}` (e.g. `../src/xmerl_simple.erl`, different fields) cannot include it.
- Keep the `%CopyrightBegin%` + `SPDX-License-Identifier: Apache-2.0` header block: repo license scanning checks these files.

## Design Decisions
- Records, not opaque types, are the interface: consumers are expected to pattern-match and construct DOM/XSD terms, and the `#xmlElement{}` tree is the interchange format between `xmerl_scan`, `xmerl` (export), `xmerl_xs`, `xmerl_xpath` and external tools such as `edoc`.
- The XPath object/context records live in `xmerl.hrl` (not `xmerl_xpath.hrl`) because the XPath evaluator was layered onto the existing DOM records.
- DOM records carry `parents`/`pos` provenance so downstream code (XPath, XSLT-style rules) can navigate without a separate index structure.

## Known Issues
- `#xmerl_scanner{}` mixes stable, callback-relevant fields (`user_state`, `fun_states`, `continuation_fun`) with many internal ones; there is no marker in the header distinguishing them.
- `xmerl_xpath.hrl` is installed but is only used by one module and duplicates a record that `xmerl_xpath.erl` declares privately — it is installed-by-convention rather than an intended external API.

## Notes for Agents
- Only three files here, all hand-written; no generator or build step writes into this directory.
- If a header file is added or renamed, update `HRL_FILES` in `../src/Makefile` first — installation of headers is driven solely by that list.
- `doc/docs.exs` sets `skip_code_autolink_to: ["t:xmerl_scanner/0", "t:xsd_state/0"]` because those types are `-doc hidden` in `xmerl_scan.erl` / `xmerl_xsd.erl`; do not "fix" documentation by un-hiding them here.
- Changing a record here requires checking all consumers: `../src/`, `../test/`, `../doc/examples/`, `../../edoc/src/`, `../../wx/api_gen/`, `../../common_test/src/ct_netconfc.erl`.
- SAX-parser work never touches this directory — SAX records live in `../src/xmerl_sax_parser.hrl`.

## Test Strategy
- No tests live here; header changes are exercised indirectly by the Common Test suites in `../test/` (notably `xmerl_xsd_*_SUITE`, which `-include_lib`s `xmerl/include/xmerl_xsd.hrl`, and `xmerl_SUITE`/`xmerl_std_SUITE` for the DOM records) and by `../../edoc` compilation.

## See Also
- `../src/CONTEXT.md` — the modules that produce/consume these records, plus yecc/generated-source rules.
- `../doc/CONTEXT.md` — which record types surface in the documentation (`docs.exs`).
- `../../edoc/src/CONTEXT.md` — primary in-OTP consumer of `xmerl.hrl`.
