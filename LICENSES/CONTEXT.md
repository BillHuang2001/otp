# LICENSES

## Intent
Flat directory of third-party license **texts** used by Erlang/OTP.
Two consumers:
1. `../scripts/license-header.es` loads every `LICENSES/*.txt` as a template keyed by its basename (the flattened SPDX id) and requires the license block in a file's header to match that text exactly.
2. `../.github/scripts/otp-compliance.es` uses the `LicenseRef*.txt` files as the license text attached to custom `LicenseRef-*` ids in the generated SPDX SBOM.
There are no subdirectories and no code here — only static license text.

## File inventory (basename = SPDX id with spaces replaced by `-`)
| File | Referenced from (headers/sidecars) |
| --- | --- |
| `BSD-2-Clause.txt` | `lib/parsetools/src/leex.erl`, `lib/parsetools/include/leexinc.hrl`, `lib/public_key/priv/moduli`, `lib/public_key/src/pubkey_moduli.hrl`; compound id in `lib/diameter/doc/standard/rfc7075.txt.license` |
| `BSD-3-Clause.txt` | ASN.1/RFC-derived files: `lib/public_key/asn1/*.asn1`, `lib/asn1/test/asn1_SUITE_data/rfcs/*`, `lib/public_key/test/public_key_SUITE_data/*`, `lib/ssl/test/*_SUITE_data/*`, `lib/eldap/asn1/*` (PCRE is vendored and excluded, it has its own `LICENCE`) |
| `MIT.txt` | `lib/eldap/src/eldap.erl`, `lib/eldap/include/eldap.hrl`, `lib/eldap/src/eldap.app.src`, `lib/eldap/vsn.mk`, `lib/eldap/README`; jQuery sidecars `lib/common_test/priv/jquery-*.js.license` |
| `LicenseRef-IETF-MIB.txt` | `lib/snmp/mibs/*.mib` (21), `lib/snmp/test/test-mibs/*.mib` (13), `lib/snmp/test/snmp_test_data/*.mib` (3) |
| `LicenseRef-RSA-PKCS3.txt` | `lib/public_key/asn1/PKCS-3.asn1` |
| `LicenseRef-RSA-PKCS5v2-0.txt` | `lib/public_key/asn1/PKCS5v2-0.asn1` |
| `LicenseRef-scancode-ietf-trust.txt` | `lib/diameter/doc/standard/rfc5447.txt.license`, `rfc6733.txt.license`, and (as the first half of the compound id) `rfc7075.txt.license` |
| `LicenseRef-scancode-ietf-trust-AND-BSD-2-Clause.txt` | `lib/diameter/doc/standard/rfc7075.txt.license` (concatenation of the IETF-trust text + BSD-2 text, as the compound id `...-AND-...`) |
| `LicenseRef-scancode-wxwindows-free-doc-3.txt` | second half of the compound id used by `lib/wx/src/wx.erl` and `lib/wx/src/gen/*.erl` (~236 files); those files actually match `../FILE-HEADERS/Apache-2.0-AND-LicenseRef-scancode-wxwindows-free-doc-3.txt` |
| `Unicode-3.0.txt` | sidecars `lib/stdlib/uc_spec/UnicodeData.txt.license`, `lib/stdlib/uc_spec/EastAsianWidth.txt.license` |
| `SGI-B-2.0.txt`, `W3C-19980720.txt`, `LicenseRef-scancode-ietf.txt`, `LicenseRef-scancode-w3c-docs-19990405.txt` | no header currently references these ids (see Known Issues) |

## Naming / API Surface rules
- A template's basename **is** the SPDX id: spaces in compound expressions become `-`, so `A AND B` → `A-AND-B.txt` (see `check_license/6`: `string:replace(Spdx, " ", "-", all)`).
- Canonical SPDX ids (`BSD-2-Clause`, `MIT`, `Unicode-3.0`, …) keep their official spelling; ids not on the SPDX list use the `LicenseRef-` prefix.
- `LicenseRef-scancode-*` ids mirror the ScanCode toolkit's ids for licenses the SPDX list does not carry; `LicenseRef-IETF-MIB`, `LicenseRef-RSA-PKCS3`, `LicenseRef-RSA-PKCS5v2-0` are OTP-specific ids.
- File content is the bare license text: no SPDX header, no copyright line, no wrapping/indentation.

## Constraints
- The basename (flattened SPDX id) must match the `SPDX-License-Identifier:` string used in headers, otherwise `license-header.es` throws `Could not find <id>.txt in LICENSES or LICENSES/HEADERS`.
- Template text is compared to the file's header license block after `string:trim/1`; do not rewrap, re-indent or "beautify" the text — any content change breaks the scan for every file using that license (only trailing whitespace/indent differences are tolerated).
- Every SPDX id that appears in a scanned header must have a template in `LICENSES/` (or `../FILE-HEADERS/`); this includes ids used only in `<file>.license` sidecars (e.g. the diameter RFC sidecars).
- Resolution order is `FILE-HEADERS/` **first**, then `LICENSES/` — on a duplicate basename the FILE-HEADERS version wins.
- `LICENSES/*` are deliberately exempt from the header check (`is_ignored/1` matches `^LICENSES/.*`) and must keep no license header of their own.
- Adding a new `LicenseRef-*` id also requires: a `<id>.txt` file here, registration in `../.ort/config/license-classifications.yml` (category `review` for the existing `LicenseRef-*`/W3C/Unicode entries), and review of `../.ort.yml` curations/`../.ort/config/evaluator.rules.kts`.

## Design Decisions
- Split from the former `LICENSE-HEADERS/`: texts that are copied *verbatim* into file headers live in `LICENSES/` so they can double as SBOM license text, while `../FILE-HEADERS/` keeps header snippets for licenses whose header quotes only part of the license (e.g. `MPL-1.1.txt`, the Apache-2.0 boilerplate).
- Compound ids get one flattened file containing the concatenated texts rather than per-part files, because the header check looks up templates by the whole id string.
- `../.ort.yml` curates `LICENSES/*` as `concluded_license: "NOASSERTION"`; `otp-compliance.es` additionally emits `comment: "license file", licenseInfoInFiles: ["NOASSERTION"]` for these paths.

## Known Issues
- Four files are not referenced by any header id: `SGI-B-2.0.txt` (the `lib/wx/src/gen/gl.erl`/`glu.erl` header uses the compound `Apache-2.0 AND SGI-B-2.0`, resolved from `FILE-HEADERS/`), `W3C-19980720.txt`, `LicenseRef-scancode-ietf.txt` and `LicenseRef-scancode-w3c-docs-19990405.txt` (their ids appear only in `../.ort/config/license-classifications.yml`). Do not delete them without checking the SBOM tests: `otp-compliance.es:test_has_extracted_licenses/1` asserts a 1:1 correspondence between `LICENSES/LicenseRef*.txt` and the `LicenseRef-*` ids used in the SBOM (`fix_has_extracted_license_info/2` seeds `hasExtractedLicensingInfos` from `LICENSES/LicenseRef*.txt` only).
- `LicenseRef-scancode-ietf.txt` and `LicenseRef-RSA-PKCS3.txt` are byte-identical (same IETF RFC boilerplate) — the duplication is intentional (one SPDX-listed look-alike id per use case) and must be kept in sync when either changes.
- Vendored trees (`erts/emulator/{pcre,zlib,zstd,ryu,asmjit}`, `lib/wx/src/gen`, `make/autoconf`, …) are skipped by the scanner via `vendor.info` and declare their licenses there (`Zlib`, `BSD-3-Clause`, `BSD-3-Clause OR GPL-2.0-only`, …) — those ids do **not** get templates here.
- Trailing newline presence varies between these files (e.g. `BSD-2-Clause.txt`, `MIT.txt` and `LicenseRef-scancode-wxwindows-free-doc-3.txt` have none) and is irrelevant because templates are trimmed; do not add or remove one gratuitously — the text is meant to mirror the header content byte-for-byte.

## Consumers (sibling paths — read-only, escalate writes to the repo root)
- `../scripts/license-header.es` — `get_license_templates/1`: loads `LICENSES/*.txt` + `FILE-HEADERS/*.txt` into the SPDX-id → template map.
- `../.github/scripts/otp-compliance.es` — `extracted_license_info/0` (`filelib:wildcard("LICENSES/LicenseRef*.txt")`), `generate_spdx_otp_licenses/2` (LICENSES + FILE-HEADERS become the `license-headers` SBOM package), `test_has_extracted_licenses/1`.
- `../.ort.yml`, `../.ort/config/license-classifications.yml`, `../.ort/config/evaluator.rules.kts` — ORT/ScanCode curations and per-license approval categories.
- `../HOWTO/FILE-HEADERS.md` — authoritative rules for when a whole license goes into `LICENSES/` vs. a snippet into `FILE-HEADERS/`.

## Test Strategy
- CI: `../.github/workflows/license-scanner.yaml` runs `./scripts/license-header.es ci` over the whole tree (twice, to verify that `update` is a no-op). A new/changed template here is validated by that job; there is no unit test for individual license files.
- SBOM-side validation lives in `../.github/scripts/otp-compliance.es` test functions (run from the SBOM workflows `reusable-sbom.yaml`).
- Practical check after editing: grep the id across the tree (`git grep -n "SPDX-License-Identifier: <id>"`) and confirm each hit's header text still matches the template byte-for-byte after trimming.

## See Also
- `../FILE-HEADERS/CONTEXT.md` — header snippets, wins on duplicate basenames.
- `../CONTEXT.md` — root routing table (license/CI tooling overview).
- `../lib/snmp/test/test-mibs/CONTEXT.md` — example of a consumer of `LicenseRef-IETF-MIB`.
