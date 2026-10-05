# public_key ASN.1 Specifications

## Intent
Source of the ASN.1 modules (RFC 5912 / PKIX / PKCS / CMS / OCSP / post-quantum) that are compiled into the Erlang encode/decode modules backing the `public_key` application.
This directory contains only inputs and the build rules; the generated Erlang code lives in `../src` and the one exported header in `../include`.
`../Makefile` builds `asn1` before `src`, so these modules must exist before the Erlang part of the app is compiled.

## API Surface
- `Makefile` — drives `erlc` over the specs; sets `ESRC = EGEN = ../src`, `EBIN = ../ebin`, `INCLUDE = ../include`.
- Toolchain flags: `ASN_FLAGS = -bber +der +noobj +asn1config`, plus `+deterministic` when `ERL_DETERMINISTIC=yes`; `ERL_COMPILE_FLAGS += +nowarn_deprecated_catch`.
- `ASN_TOP = PKCS-FRAME $(ASN_MODULES)`; only `PKCS-FRAME.hrl` is exported (`ASN_HRLS`), copied from `../src/PKCS-FRAME.hrl` to `../include/PKCS-FRAME.hrl`; every other generated `.hrl` stays in `../src`.
- Per spec, `erlc -o ../src` emits `../src/<Module>.erl`, `../src/<Module>.hrl` (when records exist) and `../src/<Module>.asn1db` (intermediate format for cross-module `IMPORT`).
- `PKCS-FRAME.set.asn` — ASN.1 "set file" (built by the `%.set.asn` rule); it lists `AsymmetricKeyPackageModuleV1.asn1` and `PKCS5v2-0.asn1`, which are compiled into the single `PKCS-FRAME` module used by `../src/public_key.erl` and `../src/pubkey_pbe.erl` (PKCS#8 `OneAsymmetricKey` + PKCS#5 PBE).
- `ASN_CONFIGS` = `OTP-PKIX.asn1config`, `OTP-PKIX-Relaxed.asn1config`, `PKIX1Explicit-2009.asn1config`; each holds one `exclusive_decode` instruction (`decode_TBSCert_exclusive` on `OTPCertificate.tbsCertificate`, `decode_TBSCertList_exclusive` on `CertificateList.toBeSigned`) and requires `+asn1config`; `../src/pubkey_cert.erl` calls `'OTP-PKIX':decode_TBSCert_exclusive/1`.
- X.509 / PKIX core: `OTP-PKIX.asn1`, `OTP-PKIX-Relaxed.asn1`, `PKIX1Explicit-2009.asn1`, `PKIX1Implicit-2009.asn1`, `PKIX-CommonTypes-2009.asn1`, `PKIX-X400Address-2009.asn1`, `PKIXAlgs-2009.asn1`, `PKIX1-PSS-OAEP-Algorithms-2009.asn1`, `AlgorithmInformation-2009.asn1`, `PKIXAttributeCertificate-2009.asn1`, `AttributeCertificateVersion1-2009.asn1`.
- PKCS: `PKCS-1.asn1` (RSA), `PKCS-3.asn1` (DH), `PKCS-10.asn1` (CSR); `PKCS5v2-0.asn1` and `AsymmetricKeyPackageModuleV1.asn1` only via `PKCS-FRAME.set.asn`.
- CMS / S-MIME / CMP / CRMF / OCSP: `CryptographicMessageSyntax-2009.asn1`, `CryptographicMessageSyntaxAlgorithms-2009.asn1`, `CMSAesRsaesOaep-2009.asn1`, `SecureMimeMessageV3dot1-2009.asn1`, `EnrollmentMessageSyntax-2009.asn1`, `PKIXCRMF-2009.asn1`, `PKIXCMP-2023.asn1`, `OCSP-2024-08.asn1`.
- Keys / curves: `DSS.asn1`, `ECPrivateKey.asn1`, `RFC5639.asn1` (Brainpool curves), `Safecurves-pkix-18.asn1` (Ed25519/Ed448, X25519/X448).
- Post-quantum (final 2024/2025 standards): `KEMAlgorithmInformation-2023.asn1`, `X509-ML-DSA-2025.asn1`, `X509-ML-KEM-2025.asn1`, `SLH-DSA-Module-2024.asn1`.
- Release install (`release_spec`): the `*.asn1` sources and the three `*.asn1config` files into `$(RELSYSDIR)/asn1`, generated `.erl` into `$(RELSYSDIR)/src`, generated beams into `$(RELSYSDIR)/ebin`, exported header into `$(RELSYSDIR)/include`.

## Constraints
- Never hand-edit generated artifacts in `../src` / `../include` (`OTP-PKIX.erl`, `PKCS-FRAME.hrl`, `*.asn1db`, …); they are build output and are listed in `../.gitignore`.
- Erlang module names equal the ASN.1 module name declared inside the file, which is usually but not always the file name (`OTP-PKIX.asn1` → `OTP-PKIX.erl`; the file-name-only modules like `PKCS-1` also define `PKCS-1`).
- A new spec must be added to `ASN_MODULES` in this Makefile and, if it should ship as an Erlang module, to the `modules` list of `../src/public_key.app.src`.
- Keep `.NOTPARALLEL:` in place (see Design Decisions) — removing it makes generated atom names non-reproducible under `make -j`.
- `OTP-PKIX-Relaxed.asn1` must stay in sync with `OTP-PKIX.asn1` for everything it aliases.

## Design Decisions
- `.NOTPARALLEL:` serializes compilation: the RFC 5912 modules import each other cyclically, and the ASN.1 compiler numbers anonymous parameterized-type/object-set arguments (`internal_object_set_argument_N`) with a global counter whose value depends on whether the imported siblings' checked `../src/*.asn1db` already exist; with `make -jN` that is a race producing different atoms between builds (reproducible-builds issue #4417).
- Output is written into `../src` rather than a private gen dir, so generated modules are ordinary app sources that the normal app build compiles and installs.
- `+noobj` only stops `erlc` from compiling the generated `.erl`; the beams are produced afterwards by the shared rule `$(EBIN)/%.beam: $(ESRC)/%.erl` in `make/otp.mk.in`.
- `-bber +der` selects the BER backend and additionally generates DER encode/decode functions — what `public_key:der_encode/2` and `der_decode/2` dispatch to via `get_asn1_module/1` in `../src/public_key.erl`.
- `OTP-PKIX.asn1` deliberately deviates from RFC 5912 so real-world certificates decode: `OTP-X520CommonName` accepts CNs longer than `ub-common-name` (255), `OTP-X520countryName` also accepts `UTF8String`, `OTP-DomainComponent` also accepts `PrintableString`, `OTP-emailAddress` also accepts `UTF8String`, and `OTP-DSA-Params-Or-NULL` tolerates absent DSA params; it also adds `OTPSignatureAlgorithms` for legacy MD2/MD5 signature algorithms and the `Hash*` types used by `public_key:short_name_hash/1`.
- `OTP-PKIX-Relaxed.asn1` exists for the `relaxed` decode mode of `public_key:pkix_decode_cert/2` (empty/zero-length RDNs, `OTPDirectoryString` CHOICE over Teletex/Printable/BMP/Universal/UTF8 with `SIZE (0)`) while re-using `OTP-PKIX` definitions by qualified reference (`OTP-PKIX.AlgorithmIdentifier`, `OTP-PKIX.Extensions`, …).

## Notes for Agents
- These generated modules form the ASN.1 backend of `public_key`; other apps (ssl, ssh, inets, ftp, …) go through the `public_key` API and the records in `../include/public_key.hrl`, not through these modules directly.
- `../include/OTP-PUB-KEY.hrl` is a hand-written legacy compatibility header that just `-include`s `public_key.hrl`; nothing generates an `OTP-PUB-KEY` module, so the `OTP-PUB-KEY.*` entries in `../.gitignore` and `../test/public_key.cover` are stale.
- `.asn1db` caches are written to `../src` (erlc `-o ../src`, and the compiler prepends its outdir to the import search path), so `make clean` here (which removes bare `*.asn1db` in this directory) does not delete them; stale `../src/*.asn1db` still steer the compiler and must be removed manually to force a full re-check.
- Some `IMPORT`s point at modules absent from this directory — `PKIXCMP-2023` imports from `PKCS-9`, `PKIXCRMF-2009` from `OCSP-2009` — copies exist only in `../../asn1/test/asn1_SUITE_data/rfcs/`; compilation still succeeds.
- Rebuild just this directory with `make -C lib/public_key/asn1` (needs a configured `ERL_TOP` and a usable `asn1` app); `make -C lib/public_key` builds the whole app in the right order.

## Test Strategy
- No test suite lives in this directory; ASN.1 decoding behaviour (including `relaxed` mode and long CNs) is covered by `../test/public_key_SUITE.erl` and the cert fixtures under `../test/`.
- The `asn1` application carries copies of some of these specs for compiler testing: `../../asn1/test/testSSLspecs.erl` with `../../asn1/test/asn1_SUITE_data/OTP-PKIX.asn1config`.
- Nothing checks that committed `.asn1` sources match the generated modules other than the normal build; a wrong spec shows up as changed `.asn1db`/atom names or failing public_key tests.

## See Also
- `../src/` — Erlang code of `public_key`, including the generator target of these specs and `get_asn1_module/1` dispatch.
- `../include/` — shipped headers (`OTP-PKIX.hrl` is generated, `public_key.hrl` is hand-written).
- `../../asn1/` — the ASN.1 compiler that consumes these files; see its docs for `noobj`, `asn1config`, `deterministic`, `.set.asn`.
- `../../ssl/`, `../../ssh/` — main downstream consumers via the `public_key` API.
