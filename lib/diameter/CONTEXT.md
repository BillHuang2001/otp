# diameter — Diameter protocol stack

## Intent
OTP application implementing the Diameter base protocol and applications (RFC 6733, previously RFC 3588) — an authentication/authorization/accounting (AAA) peer-to-peer protocol.
It provides a configurable Diameter *node*: a service (one or more applications), a set of transports to peers, and dictionaries that define the AVPs/messages of each Diameter application.
Application version is `DIAMETER_VSN = 2.7.2` (`vsn.mk`); the OTP app callback is `diameter_app` (`{mod, {diameter_app, []}}`).
One OTP application only — `subdirs.mk` is `SUB_DIRS = src doc`; the subdirectories of `src/` are module groupings, not nested applications.

## API Surface
- `diameter` module (`src/base/diameter.erl`) — the user API: `start/0`, `stop/0`, `start_service/2`, `stop_service/1`, `add_transport/2`, `remove_transport/2`, `call/3,4`, `session_id/1`, `origin_state_id/0`, `subscribe/1`, `services/0`, `service_info/2`, `peer_info/1`, and the `diameter:*` AVP data types used by dictionaries and callbacks.
- `diameter_app` (`src/base/diameter_app.erl`) — the callback behaviour users implement per Diameter application (`peer_up/3`, `peer_down/3`, `pick_peer/4`, `prepare_request/3`, `handle_request/3`, `handle_answer/4`, `handle_error/4`, `prepare_retransmit/3`); the same module is the OTP application callback.
- `diameter_transport` (`src/transport/diameter_transport.erl`) — the behaviour a transport process implementation must follow (`start/3` plus the `{diameter, Term}` message protocol); shipped transports are TCP (+TLS), SCTP and `diameter_etcp` (Erlang distribution).
- `diameter_codec` (`src/base/diameter_codec.erl`) — encode/decode interface (`encode/2,3`, `decode/2..4`, `collect_avps/1`, `message()`, `packet()`, `avp()`, `header()` types) used between the stack and generated dictionary modules.
- Dictionary DSL and its compiler: `.dia` sources in `src/dict/`, the `diameter_make` module (`src/compiler/`, `codec/1,2`, `flatten/1`, `format/1`, `format_error/1`) and the `diameterc` escript front-end in `bin/`.
  Compiling a dictionary yields `diameter_gen_<dict>.erl`/`.hrl` encode-decode + record-schema modules (generated into `src/gen/`, not checked in); runtime dictionaries shipped by the app are `diameter_gen_base_rfc3588`, `diameter_gen_base_rfc6733`, `diameter_gen_base_accounting`, `diameter_gen_acct_rfc6733`, `diameter_gen_doic_rfc7683`, `diameter_gen_relay`.
- `include/diameter.hrl` — public records (`#diameter_packet{}`, `#diameter_header{}`, `#diameter_avp{}`, `#diameter_service{}`, `#diameter_app{}`, `#diameter_caps{}`, `#diameter_event{}`, …) and app-id macros; `include/diameter_gen.hrl` — the fragment every generated dictionary module includes.
- `src/base/diameter_info.erl` / `src/base`… `src/info/diameter_dbg.erl` — debug/introspection helpers (`diameter_dbg` exposes table dumps, `dbg` tracing and a raw AVP decoder); excluded from the `.app` `modules` list.
- Reference manual pages: `diameter(3)`, `diameter_app(3)`, `diameter_codec(3)`, `diameter_make(3)`, `diameter_transport(3)`, `diameter_tcp(3)`, `diameter_sctp(3)`, `diameter_dict(4)`, `diameterc(1)`.

## Constraints
- Build with the standard app flow: `make` in `lib/diameter/` (delegating to `src/` and `doc/` through `SUB_DIRS`), plus `make app`/`make dict`/`make depend`/`make strict` in `src/`.
- Never edit generated files: `src/gen/**`, `ebin/**`, `ebin/diameter.app`, `ebin/diameter.appup`, the man pages and the generated `doc` output.
- A module list change goes in `src/modules.mk`; a dictionary addition goes in `src/modules.mk` `DICTS` **and** must be placed in `src/dict/`.
- The app declares only `stdlib` and `kernel` as required applications; `ssl`, `syntax_tools` and `runtime_tools` are optional (TLS transports / dictionary compiler / debug modules) and must not be added as hard deps.
- Public API changes in `diameter`, `diameter_app`, `diameter_codec`, `diameter_transport`, `diameter_tcp`, `diameter_sctp`, `diameter_make` are user-visible and must keep their `-doc`/`-since` metadata and reference pages in sync.
- Dictionary `.dia` files are the source of truth for wire formats; generated records use the dict's `@prefix` verbatim, so renaming a dictionary breaks users.

## Design Decisions
- Layering: users configure *services* (`diameter:start_service/2`) holding applications + `#diameter_app{}` entries; each service connects to peers over *transports*; each connection is governed by an RFC 3539 watchdog and a connection FSM that performs the CER/CEA capabilities exchange, after which application traffic is routed by `diameter_traffic`.
  See `src/base/CONTEXT.md` for the process/supervision tree.
- Dictionaries are compiled to Erlang modules rather than interpreted, so encode/decode is plain module dispatch; `include/diameter_gen.hrl` keeps generated modules thin by forwarding into the handwritten `diameter_gen` runtime.
- Prefix/dictionary separation (`diameter_gen_base_rfc3588` vs `diameter_gen_base_rfc6733`) exists because RFC 6733 is not strictly backwards compatible with RFC 3588; the RFC 3588 dictionaries are kept for compatibility.
- The RFC texts referenced by the implementation live in `doc/standard/` (git-tracked, excluded from the source tarball via `TAR.exclude` and not installed into a release).

## Dependencies
- Runtime: `stdlib`, `kernel`. Optional: `ssl` (TLS/DTLS over TCP transports; must be started by the user), `syntax_tools` (dictionary compiler at runtime), `runtime_tools` (`diameter_dbg`).
- OS: SCTP kernel support for `diameter_sctp`; `openssl` CLI to generate TLS test certificates.
- Erlang distribution must be running when a transport option references a remote node (notably `diameter_etcp` and distributed traffic handling).

## Known Issues / Notes for Agents
- `src/dict/capup_rfc6737.dia` exists but is deliberately NOT in `DICTS` — it is an unsupported reference dictionary, never built, installed or referenced; do not just add it to `DICTS`.
- `src/transport/diameter_etcp.erl` is undocumented, untested and appears non-functional (its `sockname/1` return shape conflicts with `diameter_tcp`'s expectations) — treat it as a demo.
- `src/compiler/diameter_dict_scanner.erl` is a handwritten tokenizer, not a leex-generated scanner.
- There is no declared `diameter_codec` behaviour; it documents an implementation interface of generated dictionary modules (potential new dictionary modules must still be passed as `{dictionary, Mod}`).
- `doc/src/` holds a legacy XML/edoc helper set (`seealso.ent`, `seehere.sed`, `depend.sed`) that no current build file references; docs are ExDoc-generated from `-moduledoc`/`-doc` comments.
- `doc/notes.md` (~74 KB) is hand-maintained and historical; `doc/guides/diameter_soc.md` (~89 KB) is the conceptual design guide.
- `src/dict/CONTEXT.md` records further dictionary-level gotchas (e.g. `doic_rfc7683` has no `@id`, so `id/0` raises `badarg`).

## Test Strategy
- Common Test suites live in `test/` (`*_SUITE.erl`); `test/Makefile` runs them either with `common_test` (`make ct`, `make <suite>`) or directly (`make run`, `make _<suite>`), and `make cover` produces coverage from `test/diameter.cover`.
- `lib/diameter/Makefile` sets `TEST_NEEDS_RELEASE=true` (tests run against an installed release) and `DIA_PLT_APPS=public_key ssl runtime_tools syntax_tools` for Dialyzer.
- Test helpers `test/diameter_util.erl` + `test/diameter_util.hrl` are shared by nearly every suite; slow hosts skip heavy cases via a computed `dia_factor`.
- See `test/CONTEXT.md` for the suite → protocol-area mapping and prerequisites (SCTP, distributed nodes, TLS certs).

## Routing Table
- `src/` → all sources and the build driver (`Makefile`, `modules.mk`, `diameter.app.src`, `diameter.appup.src`); routes on to `base/`, `compiler/`, `transport/`, `info/`, `dict/`, `gen/` (generated).
- `include/` → the two exported headers, `diameter.hrl` (public records/macros) and `diameter_gen.hrl` (generated-codec fragment).
- `test/` → Common Test suites, shared test helpers, test data (`diameter_codec_SUITE_data/`, `diameter_tls_SUITE_data/`), ct specs and coverage config.
- `doc/` → ExDoc config, guides, DSL/man reference sources, `notes.md` and the RFC texts in `standard/`.
- `examples/` → runnable reference nodes (`code/`: client, server, relay, redirect, sctp demo) and reference dictionaries (`dict/`); compiled by `test/diameter_examples_SUITE`.
- `bin/diameterc` → escript that compiles a `.dia` dictionary to `.erl`/`.hrl` (thin wrapper over `diameter_make:codec/2`).
- `vsn.mk` → `DIAMETER_VSN` / `APP_VSN`; `subdirs.mk` → `SUB_DIRS = src doc`; `AUTHORS`, `TAR.exclude` (`diameter/doc/standard`).

## See Also
- `../CONTEXT.md` — OTP library applications layout and build conventions; `../../make/` — shared OTP makefile infrastructure.
- Sibling applications used or served by the stack: `../crypto/`, `../public_key/`, `../ssl/` (TLS and certificate handling), `../runtime_tools/`, `../syntax_tools/` (optional deps), `../common_test/` (test framework) — read-only, escalate writes to the parent.
