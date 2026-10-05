# diameter — Public Include Directory

## Intent
Public Erlang include surface of the `diameter` application (OTP 30.0-rc0, app vsn 2.7.2), installed into `$(RELSYSDIR)/include` for users and for code generated from `.dia` dictionaries.
Consumers reach both headers via `-include_lib("diameter/include/…")`; internal OTP code (`src/base/*.erl`, `src/info/*.erl`, `examples/code/*.erl`) uses exactly that form.
Private headers are NOT here: they live in `../src/base/` and `../src/compiler/` and are released per-subdirectory as `INTERNAL_HRLS`.

## API Surface
Files (exactly two, both listed as `EXTERNAL_HRLS` in `../src/modules.mk`):
- `diameter.hrl` — guarded by `-ifndef(diameter_hrl)`; app-id macros, dictionary-module macros, and the public records.
- `diameter_gen.hrl` — an unguarded *code fragment* `-include`d by dictionary-generated modules; defines functions, not just macros/records.

`diameter.hrl` macros:
- `?DIAMETER_APP_ID_COMMON` (0), `?DIAMETER_APP_ID_ACCOUNTING` (3), `?DIAMETER_APP_ID_RELAY` (16#FFFFFFFF) — RFC 3588, section 2.4 application identifiers.
- `?DIAMETER_DICT_COMMON` (`diameter_gen_base_rfc3588`), `?DIAMETER_DICT_ACCOUNTING` (`diameter_gen_base_accounting`), `?DIAMETER_DICT_RELAY` (`diameter_gen_relay`) — all deprecated (see Known Issues).

`diameter.hrl` records (field order is the ABI users compile against):
- `#diameter_event{service, info}` — delivered to `diameter:subscribe/1` subscribers.
- `#diameter_packet{header, avps, msg, bin, errors, transport_data}` — passed through the configured dictionary module's encode/decode callbacks.
- `#diameter_header{}` — wire header: version, length, cmd_code, application_id, hop_by_hop_id, end_to_end_id, and the R/P/E/T flag booleans.
- `#diameter_avp{code, vendor_id, is_mandatory, need_encryption, data, name, value, type, index}` — `data` is the encoded binary, `value` the decoded term.
- `#diameter_caps{}` — capabilities exchanged with a peer; in callbacks each field is a `{Host, Peer}` 2-tuple.
- `#diameter_uri{type, fqdn, port, transport, protocol}` — encoding of DiameterURI AVPs (IPFilterRule/QoSFilterRule AVPs are plain OctetStrings).
- `#diameter_callback{}` — replaces a plain callback module to selectively receive/re-shape callbacks (peer_up/down, pick_peer, prepare_request/retransmit, handle_request/answer/error, default, extra).
- `#diameter_service{pid, capabilities, applications}` and `#diameter_app{alias, dictionary, module, init_state, id, mutable, options}` — passed to a transport process at start, normally consulted only for `host_ip_address`/`capabilities`.

`diameter_gen.hrl` functions (all thin forwarders to the runtime module `../src/base/diameter_gen`):
- `encode_avps/3`, `decode_avps/2` — pass `#{module => ?MODULE}` through to `diameter_gen`.
- `avp/5` — dispatches `Mod:avp/4` (the generated module's own function); `grouped_avp/4`, `empty_group/2`, `empty/2` — forward to `diameter_gen`.

## Constraints
- These two files are installed, exported API: reordering/renaming/removing record fields breaks compiled user code and generated dictionaries; add fields at new positions with defaults only as a deliberate compatibility decision.
- `diameter_gen.hrl` must NOT gain an include guard or be `-include`d outside dictionary-generated modules: it defines functions, so double inclusion in one module would be a redefinition error, and it has no standalone meaning.
- Generated dictionaries depend on both headers at beam-build time (`../src/Makefile` dependency on `$(INCDIR)/diameter.hrl` and `$(INCDIR)/diameter_gen.hrl`), so editing either forces regeneration/recompilation of the `diameter_gen_*` modules.
- Adding a file here is not enough: it must be added to `EXTERNAL_HRLS` in `../src/modules.mk` to be installed by the `release_spec` target of `../src/Makefile`; anything meant for internal use goes to `INTERNAL_HRLS` instead.
- Do not add generated content here — the `gen/diameter_gen_*.hrl` files are generated into `../src/gen/` and installed from there (`DICT_HRLS`).

## Design Decisions
- App-id macros (RFC values) are kept separate from dictionary-module macros so the numeric application identifier can be used independently of which dictionary implements it.
- `diameter_gen.hrl` exists purely to keep `diameterc`-generated output small and stable: generated modules only carry the `#diameter_*` records and per-message/per-AVP functions, while the AVPs encode/decode machinery lives centrally in `diameter_gen.erl` and can evolve without regenerating dictionaries.
- The generated `#diameter_avp` record is pulled in by `-include_lib` of `diameter.hrl` (see `../src/compiler/diameter_codegen.erl`), so generated code and handwritten runtime share one AVP representation.
- `diameter_codegen` can emit raw Erlang forms instead of a `.erl` file; in that mode it inlines `diameter_gen.hrl` by extracting the corresponding forms from the pre-built `diameter_gen_relay` beam (only the `#diameter_avp` record and non-`#`-prefixed functions are retained), so the fragment's content is also an implicit build dependency of the forms path.

## Known Issues / Notes for Agents
- `?DIAMETER_DICT_COMMON`, `?DIAMETER_DICT_ACCOUNTING` and `?DIAMETER_DICT_RELAY` are deprecated: RFC 6733 dictionaries (`diameter_gen_base_rfc6733`, `diameter_gen_acct_rfc6733`) are not strictly backwards compatible with the RFC 3588 ones the macros point at, so new code must name the dictionary module explicitly.
- `#diameter_uri.transport` defaults to `sctp` and `#diameter_uri.port` to 3868; callers relying on defaults inherit those wire assumptions.
- Both files carry the standard `%CopyrightBegin%` / SPDX `Apache-2.0` header; keep it intact when editing.
- The full user-facing contract (dictionary format, `diameterc`, callback semantics) is documented in `../doc/`, not here.

## See Also
- `../src/modules.mk` — `EXTERNAL_HRLS`/`INTERNAL_HRLS` lists that decide what is installed.
- `../src/Makefile` — `-I $(INCDIR)`, `-pa $(ABS_EBIN)` (required so `include_lib` resolves with the application name in the path), `release_spec` install layout, and header dependencies of the generated dictionaries.
- `../src/compiler/diameter_codegen.erl` — emits the `include_lib` attributes for `diameter.hrl`/`diameter_gen.hrl` and the forms-mode inlining path.
- `../src/base/diameter_gen.erl` — runtime module all `diameter_gen.hrl` functions delegate to.
- `../src/base/diameter.hrl`-consuming modules (`diameter_app.erl`, `diameter_traffic.erl`, `diameter_caps.erl`, `diameter_service.erl`, …) — the handwritten users of these records.
