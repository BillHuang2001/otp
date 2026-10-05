# diameter_tls_SUITE_data — legacy TLS cert helper

## Intent
Holds only `Makefile.ca`, retained as a manual/reference recipe for generating the self-signed CAs (`*_key.pem`, `*_ca.pem`) that `diameter_tls_SUITE` needs.

## Notes for Agents
- The suite does **not** use this directory: since the certificates are now produced by the suite itself (`make_certs/1` runs `openssl genrsa`/`openssl req` into the case's `priv_dir`), no key material is committed.
- The directory is *not* listed in `DATA` in `../modules.mk`, so it is not installed into the released test tree — do not put runtime data here expecting it to be present during `make test`.
- Requires the `openssl` CLI if used by hand; the suite skips with `no_certs` when certificate generation fails.
