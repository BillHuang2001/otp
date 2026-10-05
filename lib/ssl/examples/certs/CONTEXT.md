# ssl example certificates (`lib/ssl/examples/certs`)

## Intent
Static example PKI material used by the SSL example program (`../src/client_server.erl`) and shipped as documentation (installed under `$(RELEASE_PATH)/lib/ssl-<VSN>/doc/examples/certs`).
Contains a nested/self CA chain: root `erlangCA` → intermediate `otpCA` → end-entity `client` and `server` certificates.

## API Surface
Tracked files (10 total; `etc/**/*.pem` are committed data, not build output):
- `etc/erlangCA/cert.pem` — self-signed root CA (`CN=erlangCA`, issuer = itself).
- `etc/otpCA/cert.pem` — intermediate CA (`CN=otpCA`, issued by `erlangCA`).
- `etc/client/cert.pem`, `etc/client/key.pem`, `etc/client/cacerts.pem` — client end-entity cert, its private key, and the trust chain.
- `etc/server/cert.pem`, `etc/server/key.pem`, `etc/server/cacerts.pem` — server end-entity cert, its private key, and the trust chain.
- `cacerts.pem` (both) = concatenation of `erlangCA/cert.pem` + `otpCA/cert.pem` (2 `BEGIN CERTIFICATE` blocks) — root-then-intermediate order.
- `Makefile` — trivial build wrapper (see below).
- `src/make_certs.erl` — 48-line script, `-module(make_certs)`, exports `all/0`; its header states it only *logs* how the example certs were created and that it requires `erl_make_certs` from the test directory.

Consumed by `../src/client_server.erl` line 76, which builds `filename:join([code:lib_dir(ssl), "examples", "certs", "etc"])` and passes `cacertfile`/`certfile`/`keyfile` from `etc/<Role>/` — that path matches this source-tree directory, but NOT the released location `doc/examples/certs/etc` (see Notes for Agents).

## Constraints
- PEM files here are **generated artifacts of a third-party tool run once by hand**; do not hand-edit them, and do not add a build step that regenerates them (`certs/Makefile` deliberately has `TARGET_FILES=` empty and no `MODULES`).
- `Makefile` includes `make/target.mk`, `make/$(TARGET)/otp.mk`, `../../vsn.mk` (`VSN = $(SSL_VSN)`), sets `RELSYSDIR = $(RELEASE_PATH)/lib/ssl-$(VSN)`, and is recursed into from `lib/ssl/Makefile` (`SUB_DIRECTORIES = src doc examples/certs examples/src`).
- `release_spec:` is intentionally empty: the certs are **not** part of `make release`; only `release_docs_spec:` copies them, via `tar cf - etc` into `$(RELSYSDIR)/doc/examples/certs/` followed by `chmod -R ug+rw`.
- No test suite in `lib/ssl/test/` reads this directory; nothing validates the certs automatically.

## Notes for Agents
- **The certificates are expired**: all four certs are valid `2010-09-01 .. 2025-08-28` (verified with `ssl._ssl._test_decode_cert`). `client_server.erl` verifies the peer with `{verify, 2}`, so the example no longer completes a handshake as-is. Regenerating means overwriting committed PEMs, not touching `Makefile`.
- **All certs and keys are 1024-bit RSA, signed with `sha1WithRSAEncryption`** (verified from DER). Hosts that configure an OpenSSL security level ≥ 2 reject them (RSA < 2048 bits, SHA-1) — the example may fail for that reason too, independently of expiry.
- Both CA keys were deliberately discarded: `src/make_certs.erl` deletes `erlangCA/cert_key.pem` and `otpCA/cert_key.pem` and renames `client/cert_key.pem`/`server/cert_key.pem` to `key.pem` (matches `erl_make_certs:write_pem/3`, which writes `<Name>.pem` + `<Name>_key.pem`). So the CA chain cannot be extended/re-signed without regenerating it entirely.
- Subject fields on every cert: `emailAddress=tester@erlang.org`, `localityName=Stockholm`, `countryName=SE`, `organizationName=erlang`, `organizationalUnitName=testing dep`; only the `commonName` differs (`erlangCA`, `otpCA`, `client`, `server`). Validity in the script is `{today, today+15*365}` days, which is how the 2010 window was produced.
- **Correction to a common assumption: `erl_make_certs` still exists** at `lib/public_key/test/erl_make_certs.erl` (module `erl_make_certs`, exports `make_cert/1` and `write_pem/3` — exactly the two functions used here). So `src/make_certs.erl` is runnable in principle if that module is on the code path (it is not on the default path); it is *not* the same module as `lib/ssl/test/make_certs.erl` (695 lines, `-compile([export_all])`, richer, unrelated/test-only).
- Nothing compiles or installs `src/make_certs.erl`: the `Makefile` here defines no `MODULES`/`TARGET_FILES` and there is no `src/` subdirectory build; `git log` shows the PEMs were checked in once (commit `e28a9a10da`, 2010-09-01, "Checkin example certs instead of generating them.") and the generator was reduced to a log-only script.
- Inspect the PEMs with `python3 -c "import ssl; print(ssl._ssl._test_decode_cert(...))"` — `openssl` is not on PATH in this environment.

## See Also
- `../` (`lib/ssl/examples/`) — example programs overview; `src/client_server.erl` is the sole consumer.
- `../../../public_key/test/erl_make_certs.erl` — the generator library referenced by `src/make_certs.erl`.
- `../../../ssl/test/make_certs.erl` — separate, larger test-cert generator (read-only sibling).
- `../../doc/notes.md` (~line 618) — OTP 29 note that docs/examples were moved out of `make release` into the doc package.
