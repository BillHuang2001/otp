# eldap — Erlang LDAP Client

## Intent
OTP application providing a client API to the Lightweight Directory Access Protocol (LDAP v3, RFC 4510–4519, plus RFC 2830 for StartTLS).
Pure-Erlang client layered over TCP/TLS (`gen_tcp` / `ssl`); the LDAP wire protocol is encoded/decoded by an ASN.1 (BER) module generated from `asn1/ELDAPv3.asn1`.
Unlike most OTP apps, `eldap` is not a supervision tree: `eldap:open/2` spawns a plain linked process that owns the socket and runs a `receive` loop, and the opaque `handle` returned is that process's pid.

## API Surface
Single public module `eldap` (`src/eldap.erl`, ~2050 lines) with its documentation in `-moduledoc`/`-doc` attributes (no `doc/src/`; ExDoc reads the module):
- Connection: `open/1,2`, `close/1`, `info/1`, `controlling_process/2`, `start_tls/2,3,4`.
- Auth: `simple_bind/3,4`, `modify_password/3,4,5` (RFC 3062 password-modify extended op).
- Directory ops: `search/2,3`, `add/3,4`, `delete/2,3`, `modify/3,4`, `modify_dn/5`.
- Filters/scopes/aliases: `'and'/1`, `'or'/1`, `'not'/1`, `equalityMatch/2`, `greaterOrEqual/2`, `lessOrEqual/2`, `approxMatch/2`, `present/1`, `substrings/2`, `extensibleMatch/2`, `baseObject/0`, `singleLevel/0`, `wholeSubtree/0`, `neverDerefAliases/0`, `derefInSearching/0`, `derefFindingBaseObj/0`, `derefAlways/0`.
- Helpers: `parse_dn/1`, `parse_ldap_url/1`, `mod_add/2`, `mod_delete/2`, `mod_replace/2`, `getopts/2`, `paged_result_control/1,2`, `paged_result_cookie/1` (RFC 2696 paged results).
- Exported types: `scope/0`, `dereference/0` (`-export_type`); also `handle/0`, `filter/0`, `attribute/0`, `modify_op/0`, `connection_info/0`, `referrals/0`.
- Referral-aware calls return `{ok, {referral, UrlList}}` in addition to `ok`/`{ok, Result}`/`{error, Reason}`.
- Public records in `include/eldap.hrl`: `#eldap_search{}` (search request options), `#eldap_search_result{}` (entries/referrals/controls), `#eldap_entry{}` (DN + attribute list).
- The private `#eldap{}` connection record lives in `src/eldap.erl`.

## Constraints
- The LDAP ASN.1 encoder `ELDAPv3` is generated at build time — never hand-edit it.
  `src/Makefile` compiles `asn1/ELDAPv3.asn1` with `erlc -bber` (BER, `+legacy_erlang_types`) into `ebin/ELDAPv3.erl` + `ebin/ELDAPv3.hrl` (both git-ignored; `.gitignore` also ignores `*.asn1db` and `*.beam`).
  `eldap.erl` includes `ELDAPv3.hrl` from `ebin/` (`-I../ebin`), so the app must be built once before `eldap.erl` compiles.
- Modules list in `src/eldap.app.src` is `[eldap, 'ELDAPv3']` — keep in sync if new generated modules appear.
- Never edit generated `ebin/*` (`*.app`, `*.appup`, `ELDAPv3.*`); edit `src/eldap.app.src`, `src/eldap.appup.src` (both `%VSN%`-substituted) and the ASN.1 source instead.
- `-Werror` is set for `src/`; warnings fail the build.
- The app does not verify server certificates by default (see `doc/notes.md` history); TLS options must be supplied explicitly.

## Dependencies
From `src/eldap.app.src`: `{applications, [kernel, stdlib, asn1]}`; `{runtime_dependencies, [...]}` also names `ssl-5.3.4`, `kernel-3.0`, `erts-6.0`, `asn1-3.0`.
`ssl` is required at runtime for LDAPS (`{ssl, true}` option) and StartTLS, though listed only in `runtime_dependencies` (soft dependency — plain LDAP works without it).
Dialyzer PLT apps: `asn` (via `DIA_PLT_APPS` in the top `Makefile`) and `ssl`.

## Notes for Agents
- `ELDAP_VSN` is defined in `vsn.mk` (currently `1.3.1`); app versions are tracked repo-wide in `otp_versions.table`.
- Debug tracing has a macro `?PRINT(S,A)` compiled out (`-define(PRINT(S,A), true)`); `?elog(S,A)` logs via `error_logger`.
- `loop/2` is exported only "for upgrades" (hot code upgrade via appup).
- `example/` is intentionally singular (a single sample script, not OTP's usual `examples/`).

## Test Strategy
- Common Test suites in `test/`: `eldap_basic_SUITE` (main API suite — encode/decode, connection, bind, add/delete/modify, search filters, referrals, paging, TLS/StartTLS, timeouts, IPv4/IPv6), `eldap_collect_labmachine_info_SUITE` (helper, `all() -> []`, used to gather lab-machine data), and `make_certs` (cert generation helper).
- `test/eldap.cfg` points the suite at `{"localhost",389}`; the suite needs a **running LDAP server**.
  `test/run_server.sh` starts `bitnami/openldap:2.5` via Docker on ports 9876 (ldap) / 9877 (ldaps) and auto-generates certs; a system `slapd` with `ldap_server/slapd.conf` is the alternative (needs apparmor tweaks — see `test/README`).
- Certs are generated with `make_certs:all("/dev/null", "eldap_basic_SUITE_data/certs")` (server cert, CA, client certs); `eldap_basic_SUITE_data/RAND` seeds them.
- Tests run against the app's own `ebin` (`-pa $(ERL_TOP)/lib/eldap/ebin`); `eldap.spec` (`{suites,"../eldap_test",all}`) and `eldap.cover` (excludes generated `ELDAPv3`) drive ct_run/cover.
- `+nowarn_missing_spec_documented` and removal of `+deterministic` for test compilation (see `test/Makefile`).

## Routing Table
- `src/` → the `eldap` client module, `eldap.app.src`, `eldap.appup.src` and the per-dir `Makefile` (incl. ASN.1 generation rule)
- `asn1/` → `ELDAPv3.asn1`, the LDAP v3 ASN.1 spec (RFC 4511) compiled to the BER codec
- `include/` → public `eldap.hrl` (`#eldap_search{}`, `#eldap_search_result{}`, `#eldap_entry{}`)
- `test/` → Common Test suites, LDAP-server/cert setup scripts and config
- `doc/` → `docs.exs` (ExDoc config), `notes.md` (release notes), doc build `Makefile`
- `example/` → `ad_expiring_account.erl`, sample Active Directory query script
- `ebin/` → build output only (git-ignored except `.gitignore`)

## See Also
- `../asn1/` — the ASN.1 compiler that generates `ELDAPv3` (BER backend)
- `../ssl/` — TLS/StartTLS transport; `../public_key/` — cert handling for LDAPS
- Parent node `../CONTEXT.md` for OTP application conventions and `lib/Makefile` build order
