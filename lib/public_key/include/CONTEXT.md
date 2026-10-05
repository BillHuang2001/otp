# public_key — Public Include Headers

## Intent
The public Erlang header files shipped with the `public_key` application and exported to the rest of OTP.
These headers define the record shapes for all ASN.1/X.509 data that crosses the `public_key` API boundary (certificates, keys, CRLs, OCSP, PKCS, CMP/CRMF), plus the OID macros used to name algorithms, curves and extensions.
They are the *interface* of `public_key`: consumers pattern-match against these records and compare against these macros.

## API Surface
Four headers are listed as `HRL_FILES` in `../src/Makefile` and therefore installed into `$(RELSYSDIR)/include` (i.e. they are the supported public API):
- `public_key.hrl` (~1553 lines) — the primary header. Wrapped in `-ifndef(public_key)` / `-endif`; defines ~159 records and several hundred OID `-define`s. Contents: guards/limits (`'ubMax'` etc.), RSA (`'RSAPublicKey'`, `'RSAPrivateKey'`, `'RSASSA-PSS-params'`, `'RSAES-OAEP-params'`), post-quantum keys (`'ML-DSAPrivateKey'`/`'ML-DSAPublicKey'`, `'ML-KEMPrivateKey'`/`'ML-KEMPublicKey'`, `'SLH-DSAPrivateKey'`/`'SLH-DSAPublicKey'`), DSA (`'DSAPrivateKey'`, `'Dss-Parms'`, `'Dss-Sig-Value'`), EC (`'ECPrivateKey'`, `'ECParameters'`, `'Curve'`, `'ECPoint'`, `'ECDSA-Sig-Value'`), certificates (`'Certificate'`, `'TBSCertificate'`, `'SubjectPublicKeyInfo'`, `'AlgorithmIdentifier'`, `'Validity'`, `'AttributeTypeAndValue'`), PKIX common types (`'Extension'`, `'AttributeSet'`), extension records (`'AuthorityKeyIdentifier'`, `'BasicConstraints'`, `'NameConstraints'`, `'PolicyConstraints'`, `'DistributionPoint'`, `'AccessDescription'`, ...), OTP *relaxed* alternates (`'OTPCertificate'`, `'OTPTBSCertificate'`, `'OTPSubjectPublicKeyInfo'`, `'SignatureAlgorithm'`, `'PublicKeyAlgorithm'`, and the `-record(cert, {der, otp})` pair), CRLs (`'CertificateList'`, `'TBSCertList'`, `'IssuingDistributionPoint'`), PKCS#10 (`'CertificationRequest'`), OCSP (`'OCSPRequest'`/`'OCSPResponse'` family), PKCS#8/OneAsymmetricKey (`'PrivateKeyInfo'`, `'EncryptedPrivateKeyInfo'`, `'OneAsymmetricKey'`), PBES/PBKDF2 (`'PBES2-params'`, `'PBKDF2-params'`), CMS (`'ContentInfo'`, `'SignedData'`, `'SignerInfo'`, `'EnvelopedData'`, recipients), and X.400 OR-address records.
- `OTP-PUB-KEY.hrl` (23 lines) — backward-compatibility shim: its whole body is `-include("public_key.hrl").`; kept so old `-include_lib("public_key/include/OTP-PUB-KEY.hrl")` users still compile. Do not add content here.
- `PKIXCMP.hrl` (~292 lines) — CMP (RFC 4210/9480) message records: `'PKIMessage'`, `'PKIHeader'`, `'ProtectedPart'`, `'PBMParameter'`/`'DHBMParameter'`/`'KemBMParameter'` (PBM families), `'PKIStatusInfo'`, `'CertRepMessage'`/`'CertResponse'`, `'RevReqContent'`/`'RevRepContent'`, `'CertReqTemplateContent'`. Guarded by `-ifndef(_PKIXCMP_HRL_)`; re-includes nothing but has commented-out records that already live in `public_key.hrl` (`'AttributeTypeAndValue'`, `'PrivateKeyInfo'`, ...) to avoid duplicate definitions.
- `PKIXCRMF.hrl` (~203 lines) — CRMF (RFC 4211) message records: `'CertReqMsg'`, `'CertRequest'`, `'CertTemplate'`, `'OptionalValidity'`, `'POPOSigningKey'`/`'POPOSigningKeyInput'`, `'PKMACValue'`, `'EncKeyWithID'`, `'EncryptedValue'`, `'CertId'`. Guarded by `-ifndef(_PKIXCRMF_HRL_)` with the same commented-out overlap with `public_key.hrl`.

Generated (not shipped as source, git-ignored, produced in place here):
- `PKCS-FRAME.hrl` — copied from `../src/PKCS-FRAME.hrl` (which is generated from `../asn1/*.asn1` by the `../asn1/Makefile` rule `$(INCLUDE)/%.hrl: $(ESRC)/%.hrl`). `.gitignore` ignores both `include/PKCS-FRAME.hrl` and `src/PKCS-FRAME.hrl`.

## Constraints
- `public_key.hrl` is the cross-application contract: it is included by `lib/ssl` (src + tests), `lib/ssh`, `lib/inets` tests, `lib/tools`, `lib/ftp`, and `lib/compiler` test data, typically via `-include_lib("public_key/include/public_key.hrl")`. Changing or removing an existing record field, or renaming a record/OID macro, is a **backward-incompatible API change** across OTP — prefer adding new fields with `asn1_NOVALUE` defaults or new records/macros.
- Records in these headers must keep the `asn1_NOVALUE` / `asn1_DEFAULT` defaults: they must round-trip with the ASN.1-generated encoders in `../src` (`OTP-PUB-KEY`, `PKCS-FRAME`, ...).
- `PKCS-FRAME.hrl` is generated — never hand-edit; regenerate via the `../asn1/` build.
- `public_key.hrl` has no `-include` of its own: it must remain self-contained (only its `-ifdef(public_key)` guard).
- The internal (non-public) headers `pubkey_moduli.hrl` and `public_key_internal.hrl` live in `../src/`, **not** here — do not add them to `include/`.
- Anything added here must also be added to `HRL_FILES` in `../src/Makefile` to be installed.

## Notes for Agents
- The OID `-define`s use the literal ASN.1 names (e.g. `'id-ce-subjectAltName'`, `'secp256r1'`, `'id-Ed25519'`, `'id-ml-kem-512'`). They are quoted atoms — usable directly as `? 'id-ce-basicConstraints'` in pattern matches against `#'Extension'{extnID=...}`.
- Two certificate representations coexist: the strict ASN.1 form (`'Certificate'`, `'TBSCertificate'`, ...) produced/consumed by the generated modules, and the OTP "relaxed" form (`'OTPCertificate'`, `'OTPTBSCertificate'`) used by `pubkey_cert_records`/`public_key`'s public API. `-record(cert, ...)` bundles the DER and its decoded `'OTPCertificate'` for validated certs.
- `PKIXCMP.hrl` / `PKIXCRMF.hrl` are hand-maintained (not build-generated): they mirror the record layout of the generated modules `PKIXCMP-2023` and `PKIXCRMF-2009`, assembled so consumers don't need the generated `*.hrl`. When the corresponding `.asn1` modules change, these must be updated by hand and kept in sync.
- Post-quantum records were added recently (`ML-DSA`, `ML-KEM`, `SLH-DSA`); earlier commits added `ExtensionRequest` support — these are the newest parts of the API.
- Current `pkix` consumers in-tree: `lib/public_key/test/public_key_SUITE.erl` and `lib/public_key/test/property_test/pkix.erl` both `-include_lib("public_key/include/PKIXCMP.hrl")` and `PKIXCRMF.hrl`.

## See Also
- `../src/` → the implementation that consumes these headers (`public_key.erl`, `pubkey_*`) and holds `HRL_FILES` / `INTERNAL_HRL_FILES` definitions in its `Makefile`.
- `../asn1/` → generates the ASN.1 `.erl`/`.hrl` modules and copies `PKCS-FRAME.hrl` into this directory.
- `../CONTEXT.md` (parent) → the `public_key` application as a whole.
- `../../ssl/` → the largest external consumer of `public_key.hrl`.
