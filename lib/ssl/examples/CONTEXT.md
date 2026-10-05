# ssl examples (`lib/ssl/examples`)

## Intent
Standalone, runnable demonstration of the `ssl` application's basic TLS client/server usage, plus the static PKI material it needs.
It is the only "example program" shipped with the `ssl` app; it is documentation material, not part of the shipped runtime code or the test suites.
Sibling `src/`, `doc/`, `test/` of the app are documented elsewhere — see the routing table.

## API Surface (tracked files)
- `src/client_server.erl` — `-module(client_server)`, exports `start/0` and `init_connect/1`: starts `ssl`, listens on port 0, spawns the client, `ssl:handshake/1`, prints the peer cert (decoded with `public_key:pkix_decode_cert/2`), exchanges `"hello"`, closes and stops the apps it started.
- `src/Makefile` — `MODULES = client_server`, compiles `client_server.beam` into `../ebin` (`EBIN = ../ebin` from `make/otp.mk.in`).
- `certs/Makefile` — trivial wrapper (`TARGET_FILES=` empty, no `MODULES`); only installs the PEM tree at release-docs time.
- `certs/src/make_certs.erl` — 49-line script (`-module(make_certs)`, `all/0`) that only *logs* how the example certs were created via `erl_make_certs`; not compiled by any Makefile.
- `certs/etc/{erlangCA,otpCA,client,server}/` — committed PEM material: CA certs, plus `cert.pem`/`key.pem`/`cacerts.pem` for client and server (nested chain `erlangCA` → `otpCA` → `{client,server}`).
- `ebin/` — build output directory, kept in git only by an empty `.gitignore`; the compiled `*.beam` is masked by the repo-root `.gitignore`.

## How to build and run
- Build: `make` in `lib/ssl/` recurses into `examples/certs` and `examples/src` (`SUB_DIRECTORIES = src doc examples/certs examples/src`), producing `lib/ssl/examples/ebin/client_server.beam`.
- Run (source tree): from `$ERL_TOP/lib/ssl`, start the emulator with `examples/ebin` on the code path (e.g. `erl -pa examples/ebin`) and call `client_server:start().`.
- There is no script, test suite or doc command line that drives this example; the run instructions above are the only supported way.
- The example is not referenced from the app's doc sources (`doc/guides/using_ssl.md` shows shell-based snippets only) nor from `doc/notes.md`, and no test suite exercises it.

## Constraints
- Installed as documentation only: both child `Makefile`s have an intentionally empty `release_spec:`, so examples are excluded from `make release`; `release_docs_spec:` copies them into `$(RELEASE_PATH)/lib/ssl-$(SSL_VSN)/doc/examples/{src,ebin,certs}`.
- The PEM files under `certs/etc/` are committed data generated once by hand; never regenerate them as part of a build, and never hand-edit them.
- Keep `client_server.erl` dependency-free apart from `ssl`/`public_key`/`inet` so it stays copy-pasteable as documentation.
- Do not add this directory to `DIA_PLT_APPS`/test targets in `lib/ssl/Makefile`; it is neither analysed nor tested.

## Notes for Agents
- **The example cannot complete a handshake as-is**: the committed certs are valid `2010-09-01 .. 2025-08-28` (expired), and `client_server.erl` verifies with `{verify, 2}`.
- The certs are also 1024-bit RSA with `sha1WithRSAEncryption`, which hosts configured at OpenSSL security level ≥ 2 reject — a second, expiry-independent failure mode.
- **Cert path mismatch in installed releases**: `mk_opts/1` resolves certs as `filename:join([code:lib_dir(ssl), "examples", "certs", "etc"])`, which matches the source tree but not the released layout `<lib_dir>/ssl-<vsn>/doc/examples/certs/etc`.
- The example cert generator calls `erl_make_certs`, which does **not** live in `lib/ssl/test/` but in `lib/public_key/test/erl_make_certs.erl`; it is not on the default code path.
- `lib/ssl/test/make_certs.erl` is a separate, much larger (696-line, `-compile([export_all])`) test-cert generator used by the Common Test suites — do not conflate it with `certs/src/make_certs.erl`.
- The generated PEMs are only inspectable with `python3 -c "import ssl; print(ssl._ssl._test_decode_cert(path))"` — `openssl` is not available in this environment.
- No doc-file (`docs.exs`) build touches this tree; only the `release_docs` tar copy does.

## Routing Table
- `src/` → the example program (`client_server.erl`) and its compile-only Makefile → see `src/CONTEXT.md`
- `certs/` → example PKI material (`etc/**/*.pem`), the cert-logging script, and the release-docs install rule → see `certs/CONTEXT.md`
- `ebin/` → git-ignored build output for `client_server.beam` (only an empty `.gitignore` is tracked)
- `../test/` → `ssl` Common Test suites and the unrelated, larger test-cert generator `../test/make_certs.erl` (sibling — read-only)
- `../doc/guides/using_ssl.md` → the user guide's shell-based usage examples (the documentation counterpart of this example; sibling — read-only)
- `../../public_key/test/erl_make_certs.erl` → the `erl_make_certs` generator library referenced by `certs/src/make_certs.erl` (other app — read-only)
- `../Makefile` → app-level build driver (`SUB_DIRECTORIES`), the place to escalate changes to how examples are built or installed
