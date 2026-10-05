# uc_spec — Unicode Character Database inputs (stdlib)

## Intent
Vendored, verbatim Unicode Character Database (UCD) files that are the source of truth for the `stdlib` Unicode tables.
Nothing here is compiled into a release directly: the data feeds `gen_unicode_mod.escript`, which generates `../src/unicode_util.erl` and the `unicode_util_SUITE` fixture.
Currently tracks **Unicode 17.0.0** (see `README-UPDATE.txt` and `vendor.info`).
Flat directory — no subdirectories (an explicit README-UPDATE requirement).

## API Surface
### Vendored data consumed by the generator
- `UnicodeData.txt` — per-code-point name, general category, canonical combining class, bidi, decomposition (canonical vs. `<compat>` tagged) and simple upper/lower/title case; also `<..., First>` / `<..., Last>` ranges that get expanded.
- `SpecialCasing.txt` — full multi-code-point lower/title/upper mappings; parsing **stops at the `03A3` line**, so conditional casing is unsupported.
- `CaseFolding.txt` — only `F` (full) foldings; `T` (Turkic) and `S` (simple) lines are ignored.
- `CompositionExclusions.txt` — code points excluded from NFC composition.
- `emoji-data.txt` — only the `Extended_Pictographic` property is kept (the other emoji properties are parsed and discarded).
- `GraphemeBreakProperty.txt` — `CR, LF, Control, Prepend, L, V, T, LV, LVT, Regional_Indicator, ZWJ, Extend, SpacingMark` for grapheme clustering (`gc/1`) and Hangul fast paths.
- `PropList.txt` — `White_Space, Pattern_White_Space, Pattern_Syntax, Other_ID_Start, Other_ID_Continue` → `is_whitespace/1`, `pattern_whitespace/0`, `is_letter_not_pattern_syntax/1`, `is_other_id_start/1`, `is_other_id_continue/1`.
- `IndicSyllabicCategory.txt` — `Consonant, Vowel_Independent, Virama, Invisible_Stacker` → Indic conjunct-break support (`is_indic_consonant/1`, `is_indic_linker/1`).
- `EastAsianWidth.txt` — reduced to "wide" ranges only (`W`/`F` plus default-wide CJK ranges hardcoded in `is_default_width/2`); narrow code points are deliberately not represented.
- `DerivedCoreProperties.txt` — **not read by the generator**; vendored only so `unicode_SUITE` can cross-check `unicode:is_id_start/1` / `unicode:is_id_continue/1`.

### Generator and metadata (OTP-authored, Apache-2.0 headers, not listed in `vendor.info`)
- `gen_unicode_mod.escript` — the generator (1533 lines; `-mode(compile)`, `+A0`); see Notes for Agents.
- `README-UPDATE.txt` — step-by-step Unicode version-update procedure (17.0.0, downloaded from unicode.org `ucd/`, `ucd/auxiliary/`, `ucd/emoji/`).
- `vendor.info` — OTP SBOM/ORT metadata: `licenseDeclared: Unicode-3.0`, `versionInfo: 17.0.0`, `copyrightText`, download URL, and the `path` array listing every vendored file.
- `UnicodeData.txt.license`, `EastAsianWidth.txt.license` — 7-line SPDX `Unicode-3.0` header sidecars for exactly those two files.

### Outputs / consumers
- `../src/unicode_util.erl` — generated during the build; **git-ignored** (repo-root `.gitignore` has `/lib/stdlib/src/unicode_util.erl`) and therefore *not* committed.
  Rule in `../src/Makefile`: `unicode_util.erl: ../uc_spec/*` → `escript ../uc_spec/gen_unicode_mod.escript`.
- `../test/unicode_util_SUITE_data/unicode_table.bin` — generated fixture (escript `update_tests` mode); this one **is** committed.
- `../test/Makefile` installs `PropList.txt` and `DerivedCoreProperties.txt` verbatim into the test-release `unicode_SUITE_data/` directory.

## Constraints
- Data files must stay verbatim copies of the unicode.org originals: same file names, no reformatting, no subdirectories (README-UPDATE step 1).
- All ten data files must come from the same Unicode version; the targeted version is recorded in `README-UPDATE.txt`, `vendor.info` (`17.0.0`) and the generated `spec_version/0` literal inside the escript.
- Every vendored file must be listed in `vendor.info`'s `path` array: that list feeds the SBOM and the `is_vendored` exemption in `scripts/license-header.es` (a vendored file carrying no `%CopyrightBegin%` header is tolerated only while listed).
- `gen_unicode_mod.escript` and `README-UPDATE.txt` are OTP-authored — they need normal Apache-2.0 headers and must *not* be added to `vendor.info`.
- `../src/unicode_util.erl` is generated: never hand-edit it and never commit it; behavior changes belong in the escript, data changes in the `.txt` files.

## Notes for Agents
- The escript hardcodes relative paths: it reads `../uc_spec/<file>.txt` and writes `unicode_util.erl` **into the current directory**, so the build runs it with cwd `lib/stdlib/src`.
- README-UPDATE step 6 suggests running it from `lib/stdlib/uc_spec` (with `update_tests`): the `../uc_spec/...` reads still resolve, but it leaves a stray untracked `unicode_util.erl` in `uc_spec/` — prefer running from `../src`, or delete the stray file.
- `spec_version/0` in the generated module comes from an inline literal in `gen_static/1` (`io:put_chars(Fd, "spec_version() -> {17,0}.\n\n\n")`), not from a function — README-UPDATE step 3 ("update the spec_version() function") means this literal.
- The escript needs a working OTP to run (`escript`, plus `sofs`/`string` from the very stdlib it is generating for, hence the +A0/bootstrap context); no other external tool or system library.
- Version-bump procedure: replace the 10 data files and the 3 test files under `../test/unicode_util_SUITE_data/`, edit the `spec_version` literal, update `README-UPDATE.txt` + `vendor.info`, regenerate the fixture, then run the Unicode suites.
- The data files are large (UnicodeData.txt ~2.2 MB, DerivedCoreProperties.txt ~1.1 MB) — grep/`head` them instead of reading whole.
- `parse_properties/2` keys per code point on `string:lowercase/1` of the UCD property name, so UCD names map to atoms such as `pattern_white_space`, `extended_pictographic`, `regional_indicator`, `vowel_independent`.

## Dependencies
- Unicode Character Database 17.0.0 from `https://www.unicode.org/Public/17.0.0/ucd/` (plus `auxiliary/` and `emoji/`), licensed `Unicode-3.0`.
- Downstream: SBOM/ORT tooling and `scripts/license-header.es` read `vendor.info` (see `HOWTO/SBOM.md`).

## Test Strategy
- No tests live in this directory; the data is exercised through `../test/unicode_util_SUITE.erl` (exhaustive comparison against the committed `unicode_util_SUITE_data/unicode_table.bin`) and `../test/unicode_SUITE.erl` (whitespace and `is_id_*` checks against the installed `PropList.txt` / `DerivedCoreProperties.txt`).
- Normal builds do **not** refresh the fixture — only `escript gen_unicode_mod.escript update_tests` does; changing data without regenerating it fails `unicode_util_SUITE`.
- After any change here run both Unicode suites (README-UPDATE step 7: `otp_build all -a && otp_build tests`, then `ts:run(stdlib, unicode_SUITE, [batch])`).

## See Also
- `../src/unicode_util.erl` — the generated module (build artifact), `../src/unicode.erl` — the public API layered on it.
- `../src/Makefile` — generation rule; `../test/Makefile` — installs `PropList.txt`/`DerivedCoreProperties.txt` for tests.
- `../test/unicode_util_SUITE.erl`, `../test/unicode_util_SUITE_data/`, `../test/unicode_SUITE.erl`.
- `../doc/notes.md` — release notes state which Unicode standard the `unicode` module supports.
