# OTP Library Applications

## Intent
Holds the OTP library applications (`lib/<app>/`) that ship with Erlang/OTP — everything above the runtime system (`erts/`) and shared build infrastructure (`make/`).
Each subdirectory is one *OTP application* built in place; `lib/Makefile` coordinates building and releasing all of them.

## API Surface
- `lib/Makefile` — the build coordinator. Includes `$(ERL_TOP)/make/target.mk`, `make/$(TARGET)/otp.mk` and finally `make/otp_subdir.mk` (which recurses into `SUB_DIRECTORIES`).
  - `ERTS_APPLICATIONS = stdlib sasl kernel compiler` — must be built first (they are needed to build everything else).
  - `ERLANG_APPLICATIONS = tools common_test runtime_tools inets parsetools` — built next; under `TINY_BUILD` only `parsetools`.
  - `ALL_ERLANG_APPLICATIONS` (xmerl edoc snmp erl_interface asn1 jinterface wx debugger reltool mnesia crypto os_mon syntax_tools public_key ssl observer odbc diameter et megaco eunit ssh eldap dialyzer ftp tftp) — only built with `BUILD_ALL` (i.e. `make` in `lib/` or `otp_build -a`).
  - With `BUILD_ALL`, any extra `*/` dir not in the expected lists is auto-added to `EXTRA_APPLICATIONS`.
  - `BUILD_STATIC_LIBS` restricts `SUB_DIRECTORIES` to `asn1 crypto`.
  - `BOOTSTRAP` overrides `SUB_DIRECTORIES` with the bootstrap app list.
  - `SKIP-APPLICATIONS` (optional file) lists apps to exclude.
- `lib/.gitignore` — ignores per-app build outputs (`ebin/*`, generated files).

## Application Directory Conventions
Every `lib/<app>/` follows the same schema (some dirs optional per app):
- `src/` — Erlang sources; `src/<app>.app.src` is the application resource template (`vsn` set from `%VSN%`, `modules` filled at build time). A few apps keep it in a nested dir instead (`inets/src/inets_app/`, `megaco/src/app/`, `snmp/src/app/`, `jinterface/java_src/com/ericsson/otp/erlang/`).
- `include/` — public Erlang headers (`*.hrl`) exported to other applications.
- `ebin/` — build output (only `.gitignore` tracked).
- `test/` — test suites (`*_SUITE.erl`) run via Common Test; `test/Makefile` lists `MODULES` and `RELSYSDIR` (installed as `$(RELEASE_PATH)/<app>_test`).
- `doc/` — documentation: `docs.exs` (ExDoc config), `src/` (markdown+`.erl` doc sources), `references/`, `guides/`.
- `vsn.mk` — defines `<APP>_VSN` (the app's version); `Makefile` sets `VSN = $(<APP>_VSN)` and includes `make/app_targets.mk`. (35 of 36 apps have `vsn.mk`; `erl_interface` versions differently.)
- `Makefile` — includes `../make/target.mk`, `../make/$(TARGET)/otp.mk`, `vsn.mk`, then `make/app_targets.mk`; sets `SUB_DIRECTORIES`.
- `AUTHORS`, `priv/` (runtime data files), `examples/`, `c_src/` (NIF/linked-in-driver C code), `internal_doc/` (non-published docs) — present only where relevant.
- C/OS-dependent apps carry their own autoconf: `configure`+`configure.ac` exist in `common_test`, `crypto`, `erl_interface`, `megaco`, `odbc`, `snmp`, `wx`; the top-level `configure` runs these.
- `prebuild.skip` / `prebuild.keep` control prebuilt-artifact handling during bootstrap; `subdirs.mk` lists nested build subdirectories for a few apps.

## Constraints
- Build order is fixed by `lib/Makefile`: `stdlib`, `sasl`, `kernel`, `compiler` first. Dependencies declare `{applications, [...]}` and `{runtime_dependencies, [...]}` in `*.app.src`.
- `<app>/ebin/`, generated `*.app` files and doc output are build artifacts — not edited by hand.
- Any new application directory must be added to the appropriate list in `lib/Makefile` (or picked up automatically only under `BUILD_ALL`).

## Notes for Agents
- App versions live in `lib/<app>/vsn.mk` (`<APP>_VSN`); the release-wide mapping of OTP version → app versions is `otp_versions.table` at the repo root.
- `erl_interface` and `jinterface` ship C/Java (`jinterface` has `java_src/`), so their layout differs from the pure-Erlang apps.
- `debugger`, `observer`, `wx` are GUI apps with extra runtime deps; `odbc` needs ODBC system libs; `wx` needs wxWidgets.

## Test Strategy
- `make test` in an app dir runs its Common Test suites from `test/`; `TEST_NEEDS_RELEASE=true` (e.g. `kernel`) means tests run against an installed release.
- Cross-app behaviour is covered by suites in the highest-level app (e.g. `ssl` tests dist/TLS integration).

## Routing Table
| App dir | Description |
| --- | --- |
| `asn1/` | ASN.1 (BER/DER/PER) compiler and runtime library |
| `common_test/` | Common Test — automated test framework (`ct_run`, `test_server/`) |
| `compiler/` | Erlang compiler (Core → BEAM; `compile`, `beam_*` passes) |
| `crypto/` | Cryptography NIFs (OpenSSL-backed; ciphers, hashes, public-key primitives) |
| `debugger/` | Graphical/interactive Erlang debugger |
| `dialyzer/` | Discrepancy analyzer (static success-typing analysis of BEAM) |
| `diameter/` | Diameter authentication/authorization protocol stack |
| `edoc/` | Erlang documentation engine (generates docs from source) |
| `eldap/` | LDAP client (needs an ASN.1-generated encoder in `asn1/`) |
| `erl_interface/` | C library for Erlang↔C communication (`ei`, `erl_*`); builds `erl_call` |
| `et/` | Event Tracer (GUI tracing tool) |
| `eunit/` | EUnit lightweight unit-test framework |
| `ftp/` | FTP client |
| `inets/` | HTTP client (`httpc`) + HTTP server (`httpd`) and the `inets` service API (sources split across `src/{inets_app,http_lib,http_client,http_server}`) |
| `jinterface/` | Java classes for Jinterface (Java↔Erlang `OtpNode`/`OtpMbox`) |
| `kernel/` | Kernel — core OTP app (files, code loading, distribution, logger, inet) |
| `megaco/` | Megaco/H.248 media gateway control protocol stack |
| `mnesia/` | Distributed, transactional, soft-realtime database |
| `observer/` | GUI system observer (processes, memory, applications) |
| `odbc/` | ODBC database access (Erlang ODBC interface) |
| `os_mon/` | OS monitoring (cpu/mem/disk supervision, SNMP MIBs) |
| `parsetools/` | Parser-generator tools (Yecc LALR, Leex lexer) |
| `public_key/` | Public-key infrastructure API and cert handling (ASN.1-based) |
| `reltool/` | Release-management tool for building/analyzing release specs |
| `runtime_tools/` | Runtime tools (instrument, scheduler/memory inspection helpers) |
| `sasl/` | SASL — system architecture support (release handling, alarm handlers) |
| `snmp/` | SNMP agent/toolkit and MIB compiler |
| `ssh/` | SSH-2 client/server implementation |
| `ssl/` | TLS/DTLS (ssl API, over `crypto`/`public_key`) |
| `stdlib/` | Standard library (gen_*, lists, maps, string, io, ...) |
| `syntax_tools/` | Erlang syntax tree utilities (parsing/pretty-printing, `erl_syntax`) |
| `tftp/` | TFTP client/server |
| `tools/` | Development tools (cover, xref, profilers, `make`-style helpers, emacs mode) |
| `wx/` | wxWidgets-based GUI framework (Erlang/Elixir bindings) |
| `xmerl/` | XML parser/generator and XPath/XSLT helpers |

## See Also
- `../erts/` — the runtime system; `../make/` — build-system makefiles; `../system/` — docs build; repo-root `CONTEXT.md` for top-level build/release entry points.
