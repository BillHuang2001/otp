# diameter — Examples

## Intent
Sample code and sample dictionaries shipped with the `diameter` application (RFC 6733 Diameter stack).
Two kinds of material: runnable reference implementations of Diameter nodes (client, server, relay agent, redirect agent) and RFC application dictionaries meant as starting points for users.
This is tutorial/reference material — nothing here is required at runtime by the `diameter` application itself.

## API Surface
- `code/` — runnable Erlang examples: an interface module (`client`, `server`, `relay`, `redirect`) plus its `diameter_app` callback module (`*_cb`), a standalone `sctp.erl` `gen_sctp` demo, `GNUmakefile` (`erlc -W`, `all`/`clean`) and `README` (manual walkthrough + recommended service options).
- `dict/` — six example `.dia` dictionaries (`rfc4004_mip`, `rfc4005_nas`, `rfc4006_cc`, `rfc4072_eap`, `rfc4590_digest`, `rfc4740_sip`), `GNUmakefile` (compiles them with `diameterc` + `erlc`) and `depend.sed` (generates `depend.mk` inter-dictionary build order).
- The four examples use only the app's built-in generated dictionaries (`diameter_gen_base_rfc6733` for client/server, `diameter_gen_relay` for relay/redirect); the `dict/` examples are unused by `code/`.

## Constraints
- The examples are a test contract, not just samples: `../test/diameter_examples_SUITE.erl` compiles *every* `*.erl` under `code/` with `warnings_as_errors`, compiles *every* `*.dia` under `dict/`, and runs the client/server pair over TCP (and SCTP when available).
- They must therefore keep compiling and running against the current public `diameter` API; a signature change in `diameter` breaks this suite.
- Adding/removing a file requires updating `EXAMPLES` in `../src/modules.mk` (only the listed files are installed).
- Never hand-edit generated artifacts (`*.beam`, and the `.erl`/`.hrl` produced from `.dia`).

## Design Decisions
- Examples come in an interface module / callback module pair with a shared `<name>_cb` naming convention, so the same interface functions (`start`, `stop`, `listen`, `connect`) look identical across node roles.
- Transport convenience: `client.erl` and `server.erl` map simple terms (`tcp`, `sctp`, `any`, `{tcp, loopback, 3868}`) onto `{transport_module, transport_config}` pairs, making the examples usable from a bare Erlang shell; `relay.erl`/`redirect.erl` reuse those helpers by delegating to `server:listen/2` and `client:connect/2`.
- The examples deliberately set non-default options recommended for real services (`decode_format`, `restrict_connections`, `string_decode`, `strict_mbit`, `call_mutates_state`); `code/README` explains why.
- `dict/` exists to document/validate the dictionary language against real RFCs; it is built out-of-tree as a convenience and is not wired into the app build.
- Examples are released under `doc/examples/`, not a top-level `examples/` directory.

## Known Issues / Notes for Agents
- The *released* file set is narrower than the tracked file set: `EXAMPLES` in `../src/modules.mk` excludes `code/README`, `code/sctp.erl`, `dict/GNUmakefile`, `dict/depend.sed` and the `.gitignore` files — only `code/GNUmakefile` + the eight example `*.erl` + the six `.dia` land in `$(RELSYSDIR)/doc/examples/{code,dict}/`.
- The *test release* differs: `../test/Makefile` `release_tests_spec` tars the whole `examples/` tree into `$(RELSYSDIR)/examples`, so the suite does see the unreleased files (and thus also compiles `code/sctp.erl`).
- `diameter_examples_SUITE` executes only the server and client examples, and only via `start/0`, `listen/1`, `connect/1`, `call/0` on 127.0.0.1:3868; `relay` and `redirect` are compiled but never run by the suite.
- Default ports/addresses (3868, 127.0.0.1) are hard-coded in the examples; running client and server on one host therefore conflicts unless ports are overridden via the option tuples.
- SCTP variants need OS/lib support; the suite degrades to TCP only when SCTP is unavailable.
- The example dictionaries are compiled with `diameterc`, which means the compile requires a working `diameterc` (and `erlc`) on `PATH` for the manual `GNUmakefile` route.
- `../doc/guides/diameter_examples.md` is a stub pointing at this directory; `code/README` is the actual walkthrough.

## Test Strategy
- `../test/diameter_examples_SUITE.erl` is the only automated coverage: testcase `dict` compiles all example dictionaries (`diameterc`, order derived from the `@inherits` graph), testcase `code` compiles all example modules with `warnings_as_errors` and then drives 100 client requests to the server over `tcp`/`sctp`.
- Both testcases require distribution (peer nodes) and a temp dir; the `dict` case skips itself on slow machines.
- No unit tests exist for `relay`/`redirect` behaviour in this directory — regressions there are only caught by compilation.

## See Also
- `../CONTEXT.md` — the `diameter` application (stack, compiler, `diameterc`, built-in dictionaries under `../src/dict/`).
- `../doc/guides/` — user guides; `../doc/references/diameter_dict.md` for the dictionary language, `diameterc_cmd.md` for the compiler CLI.
- `../src/modules.mk` — `EXAMPLES` list that defines what is installed; `../test/Makefile` — how `examples/` reaches the test release.

## Routing Table
- `code/` → runnable example nodes (client/server/relay/redirect + callbacks), `sctp.erl` demo, `GNUmakefile`, `README`
- `dict/` → example RFC application dictionaries (`.dia`) and their compile-order helper files
