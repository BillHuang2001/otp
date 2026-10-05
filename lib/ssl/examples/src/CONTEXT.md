# ssl examples source

## Intent
Sources for the `ssl` application's user-visible example: a self-contained TLS client/server demo that uses the bundled example certificates.
Built as part of the `ssl` app (`SUB_DIRECTORIES = src doc examples/certs examples/src` in `lib/ssl/Makefile`) but shipped only via `make release_docs`, not `make release`.

## API Surface
- `client_server.erl` — `-module(client_server)`, exports `start/0` (run the demo: starts `ssl`, listens on port 0, spawns the client, accepts, handshakes, decodes peer cert with `public_key:pkix_decode_cert(Cert, otp)`, sends `"hello"`, closes, stops the apps it started) and `init_connect/1` (client half, `ssl:connect/3` to `inet:gethostname()`:Port).
  - Internal `mk_opts/1` maps role atoms `listen`/`connect` to `"server"`/`"client"` and builds `[{active,false},{verify,2},{depth,2},{server_name_indication,disable},{cacertfile,...},{certfile,...},{keyfile,...}]`.
- `Makefile` — `MODULES = client_server`; `TARGET_FILES = $(MODULES:%=$(EBIN)/%.beam)` with `EBIN = ../ebin` (from `make/otp.mk.in`, `EMULATOR = beam`), so `make`/`opt` compiles `client_server.beam` into `lib/ssl/examples/ebin`.
  - `include ../../vsn.mk`, `VSN = $(SSL_VSN)`; `RELSYSDIR = $(RELEASE_PATH)/lib/ssl-$(VSN)`.
  - `EXTRA_ERLC_FLAGS = +warn_unused_vars +nowarn_missing_doc +nowarn_missing_spec_documented`.
  - Targets: default/`$(TYPES)` compile, `clean`, `docs:` (empty), `release_spec: opt` (nothing extra), `release_docs_spec:` (creates `$(RELSYSDIR)/doc/examples/{src,ebin}` and pipes `tar cf - src ebin` from `..` into it, then `chmod -R ug+w`).

## Constraints
- Do not edit `../ebin/*.beam`; it is build output (masked by the root `.gitignore`'s `*.beam`; only an empty `../ebin/.gitignore` is tracked to keep the directory).
- Keep the module dependency-free apart from `ssl`/`public_key` so the example stays copy-pasteable.

## Notes for Agents
- `mk_opts/1` resolves the certificates as `filename:join([code:lib_dir(ssl), "examples", "certs", "etc"])`, which is correct only in a source tree.
  In an installed release the examples live under `<lib_dir>/ssl-<vsn>/doc/examples/`, so the PEMs are actually at `<lib_dir>/ssl-<vsn>/doc/examples/certs/etc` and the hardcoded path misses them.
- The committed example certs (`../certs/etc/{client,server,erlangCA,otpCA}/*.pem`) are valid 2010-09-01 .. 2025-08-28 and are therefore expired; with `{verify, 2}` the handshake now fails certificate verification unless the certs are regenerated (see `../certs/src/make_certs.erl`, which needs `erl_make_certs` from `lib/ssl/test`).
- The example is not referenced by any doc source or test suite in the repo; grep for `client_server` outside these two files finds only unrelated matches.
- `../certs/` owns cert generation and the PEM tree; changes to cert paths or contents must be coordinated with it.

## See Also
- `../CONTEXT.md` — the `examples/` tree (build output `../ebin/`, certs).
- `../../CONTEXT.md` — the `ssl` application (API, tests, docs).
- `../../test/` — where `erl_make_certs` lives, used by `../certs/src/make_certs.erl`.
