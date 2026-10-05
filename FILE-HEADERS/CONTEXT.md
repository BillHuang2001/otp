# `FILE-HEADERS/` — SPDX license-header template registry

## Intent
Flat registry of the exact license-header *bodies* that files in Erlang/OTP are allowed to carry.
One file per distinct `SPDX-License-Identifier` expression; the file name is the flattened, hyphenated form of that expression (`<flattened-expression>.txt`).
The body is the literal text that must appear in the file's `%CopyrightBegin% … %CopyrightEnd%` block *below* the `SPDX-License-Identifier:` line.
Consumed by `../scripts/license-header.es` as the authoritative text for validation — the scanner compares, it does not generate from here.
The normative rules live in `../HOWTO/FILE-HEADERS.md`.

## API Surface (the templates)
| Template | `SPDX-License-Identifier` it matches | Used by |
|---|---|---|
| `Apache-2.0.txt` | `Apache-2.0` | the default — virtually every OTP source file |
| `Apache-2.0-OR-LGPL-2.1-or-later.txt` | `Apache-2.0 OR LGPL-2.1-or-later` | ~85 legacy dual-licensed files, mostly `lib/syntax_tools/` |
| `Apache-2.0-AND-LicenseRef-scancode-wxwindows-free-doc-3.txt` | `Apache-2.0 AND LicenseRef-scancode-wxwindows-free-doc-3` | ~236 files, almost all of `lib/wx/src/gen/` |
| `Apache-2.0-AND-SGI-B-2.0.txt` | `Apache-2.0 AND SGI-B-2.0` | `lib/wx/src/gen/gl.erl`, `lib/wx/src/gen/glu.erl` |
| `MPL-1.1.txt` | `MPL-1.1` | vendored wrangler/ewgi fixtures under `lib/dialyzer/test/opaque_SUITE_data{,2}/` |

`README.md` is a one-paragraph pointer to `../HOWTO/FILE-HEADERS.md`.

## Resolution rule (how a file's license text is determined)
1. The scanner reads the file's `SPDX-License-Identifier:` value and flattens it by replacing every space with `-` (the `AND`/`OR` operators stay verbatim and uppercase, so they become `-AND-`/`-OR-` in the name).
2. It looks the flattened name up in a single map built from `../LICENSES/*.txt` first and `FILE-HEADERS/*.txt` second — so **`FILE-HEADERS/` wins over `LICENSES/` on identical flattened names** (currently no duplicates exist).
3. No entry ⇒ the hard warning `Could not find <flattened>.txt in LICENSES or LICENSES/HEADERS`.
4. Match is byte-exact: the header's license lines (comment prefix stripped, joined with `\n`) must equal `string:trim(Template) ++ "\n"`. A trailing newline/whitespace in the template is insignificant; internal blank lines are significant. `scan -v` prints the diff when it fails.

## Constraints
- A template contains the license body **only**: no `%CopyrightBegin%`/`%CopyrightEnd%`, no `SPDX-License-Identifier:` line, no copyright lines, and no comment prefix (`%%`, `##`, …) — the prefix is stripped by the scanner before comparison.
- Templates are shared across all comment syntaxes; one file serves `%%`, `##`, `/* */`-style headers alike.
- A composite (`AND`/`OR`) expression needs its own file whose name is the *whole* expression; the individual ids are not concatenated.
- The body holds only the text that files actually carry inline. For `Apache-2.0 AND SGI-B-2.0` that is just the Apache text (identical content to `Apache-2.0.txt`, separate filename) because the SGI terms are delivered by id via `../LICENSES/SGI-B-2.0.txt`; the wxwindows composite by contrast inlines the full wxWindows Free Documentation Licence text.
- Never rewrap or reformat: the check is exact, so a whitespace change here becomes the enforced text repo-wide.
- `LicenseRef-*` ids used in template names must resolve to a text in `../LICENSES/` (REUSE convention).
- Adding or changing a template goes together with updating `../HOWTO/FILE-HEADERS.md`.

## Exclusion from the header check
`is_ignored/1` in `../scripts/license-header.es` skips `^FILE-HEADERS/.*`, so these `.txt` files legitimately carry no `%CopyrightBegin%` block — do not add one.

## Consumers
- `../scripts/license-header.es` — `scan`/`ci`/`update`; `../scripts/otp_build_check` and `../.github/workflows/license-scanner.yaml` run `license-header.es ci` (the workflow also runs `update` then `ci` as an idempotence check).
- `../.github/scripts/otp-compliance.es` — SBOM generation: adds the SPDX `comment` `"license file"` to every `FILE-HEADERS/*` except `README.md`, and groups `LICENSES/` + `FILE-HEADERS/` into the `license-headers` SBOM package.
- `../.ort.yml` — ORT curations mark `FILE-HEADERS/*` as `NOASSERTION` so the ORT scanner does not report the template text as project license findings.

## Notes for Agents
- Leaf directory: 6 files, no subdirectories, nothing generated, nothing compiled or installed — safe to edit in place.
- To add a license variant: create `<flattened-expression>.txt` here, use exactly that expression as the file's `SPDX-License-Identifier`, and document it in `../HOWTO/FILE-HEADERS.md`. Remember the name is the flattened expression, not the license id.
- Local verification: `./scripts/license-header.es scan --path <file|dir>` (add `-v` to see the template diff).
- A typo here is not caught by the CI job that enforces the headers (this directory is skipped by the scanner), so it silently changes the enforced text.

## Test Strategy
No tests of this directory exist. Correctness is enforced indirectly: `./scripts/otp_build_check` (`--no-license-header` to skip) and the `Check license header` GitHub workflow fail if any file's header no longer matches its template.

## Status
Complete — all 5 templates and their consumers documented; flat leaf, no delegation targets.

## Routing Table
No subdirectories. Cross-references (read-only; escalate writes to the parent node):
- `../HOWTO/` → `FILE-HEADERS.md` — normative header rules, copyright-statement forms, short-file/vendored exemptions (sibling — read-only)
- `../LICENSES/` → single-license texts, lower precedence than this directory (sibling — read-only)
- `../scripts/` → `license-header.es`, the scanner that resolves these templates (sibling — read-only)
- `../.github/` → `workflows/license-scanner.yaml`, `scripts/otp-compliance.es` (sibling — read-only)
