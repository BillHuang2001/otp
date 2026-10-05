# public_key — C Sources (NIF)

## Intent
Native (NIF) code for the `public_key` application, used only by `pubkey_os_cacerts` to read the OS trust store.
Currently a single NIF that fetches the Windows certificate store; on non-Windows platforms the module has a pure-Erlang fallback and this directory builds nothing.

## API Surface
- `public_key.c` — defines the NIF for module `pubkey_os_cacerts` (`ERL_NIF_INIT(pubkey_os_cacerts, ...)`), exposing exactly `os_cacerts/0`.
  - On Windows (`#ifdef WINVER`) it opens the `ROOT` system store (`CertOpenStore(CERT_SYSTEM_STORE_CURRENT_USER, ...)`) and returns a list of `{Encoding::atom(), Cert::binary()}` where the atom is `x509_asn_encoding` | `pkcs_7_asn_encoding` | `unknown` and the binary is the DER-encoded certificate.
  - The whole function body is guarded by `#ifdef WINVER`; on other targets the NIF exports only stubs (load/upgrade/unload), and `pubkey_os_cacerts` never loads the NIF (`erlang:load_nif/2` is only reached on `{win32, _}`).
- `Makefile` — builds the shared library only for `TARGET=win32`: `PUBKEY_LIB = $(LIBDIR)/public_key$(TYPEMARKER).$(DED_EXT)` under `../priv/lib/$(TARGET)`, objects under `../priv/obj/$(TARGET)`.
  - On non-Windows targets `DIRS`/`PUBKEY_LIB` are empty and `release_spec` is a no-op.

## Constraints
- Uses the dedicated emulator build macros (`$(DED_CC)`/`$(DED_CFLAGS)` etc. from `make/$(TARGET)/otp_ded.mk`) and includes `$(ERL_TOP)/erts/emulator/$(TARGET)`; do not replace with the ordinary app C flags.
- The NIF must remain named `pubkey_os_cacerts` to match `-nifs([os_cacerts/0])` and the `erlang:load_nif` call site in `../src/pubkey_os_cacerts.erl`.
- `../priv/lib/` and `../priv/obj/` are build output (git-ignored) — never commit them.

## Notes for Agents
- This NIF is Windows-only in practice; the code path is not exercised on Linux/macOS. The Erlang side (`../src/pubkey_os_cacerts.erl`) is where the cross-platform `cacerts_get/load/clear` API and the PEM-file fallback live.
- Returned encodings are filtered by the Erlang side; a certificate the module ignores yields the `unknown` atom here.

## See Also
- `../src/pubkey_os_cacerts.erl` — the module that loads this NIF and implements the public `cacerts_*` API.
- `../priv/` — build output directory (`priv/lib/`, `priv/obj/`) and runtime data.
