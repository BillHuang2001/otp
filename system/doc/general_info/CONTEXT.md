# general_info — Deprecations & Removals Bookkeeping

## Intent
Owns the system-level reference pages that track deprecated / scheduled-for-removal / removed functionality in Erlang/OTP, plus the single hand-maintained registry (`DEPRECATIONS`) of every `-deprecated`/`-removed`/`-unsafe` module attribute in OTP.
Three of the four pages are *generated at build time*; nothing here is a "guide" (see Known Issues).

## File roles
| Kind | Files | Notes |
|---|---|---|
| Data (authoritative) | `DEPRECATIONS` | hand-maintained registry, feeds code generation AND the doc pages |
| Per-release prose fragments | `deprecations_12/18/19/22/23/24/27/28/29.md`, `scheduled_for_removal_30.md`, `removed_23/24/26/27/30.md` | naming contract `<type>_<otp-major>.md`; optional |
| Page preamble templates | `deprecations_head.md`, `scheduled_for_removal_head.md`, `removed_head.md` | `# <Title>` + `## Introduction` + link to `../system_principles/misc.md`; never a release |
| **Generated at build time** | `deprecations.md`, `scheduled_for_removal.md`, `removed.md` | listed in `.gitignore`; must never be edited or committed |
| Hand-written page | `upcoming_incompatibilities.md` | "Upcoming Potential Incompatibilities"; not generated |

## `DEPRECATIONS` format
```
<module>:<function>/<arity>  (since=<N> | unsafe=<N>)*  (remove=<N>)?
erlang:now/0 since=18
filename:safe_relative_path/1 since=23 remove=25
erlang:binary_to_atom/_ unsafe=29
slave:_/_ since=25 remove=31
```
- `<arity>` is an integer or `_`; `<function>` may be `_` (matches every exported function, as produced by `-deprecated([{'_','_',Desc}])`).
- Keys: `since=<major>` (release deprecated), `unsafe=<major>`, `remove=<major>` (scheduled removal). Whitespace-separated; extra blank lines/whitespace are fine.
- Blank lines and lines starting with `#` are comments (section headers `# Added in OTP <N>.`).
- Only bare atoms are expressible: the regex is `^(\w+):(\w+)/([\d_]+)$` — no quoted atoms (`'ssh-rsa'` etc.).
- `mfa=<MFA>` is accepted by the parser but used nowhere in the file today; don't rely on it.
- An unknown key or a part without `=` aborts the escript with a `function_clause` (no friendly error).
- Duplicate MFA lines: last one wins (`maps:from_list/1`).

## Generation contract
`lib/stdlib/scripts/update_deprecations make_markdown <type> <ERL_TOP> <outfile>`, type ∈ `deprecations | scheduled_for_removal | removed` (invoked by the three rules in `../Makefile`).
- Needs a *full* build: it scans `erts/ebin` and every `lib/*/ebin` beam and halts if any of those dirs contains no `.beam`.
- Joins `DEPRECATIONS` with the module attributes. `deprecations` and `scheduled_for_removal` read `-deprecated` text, grouped by `since=` and `remove=` respectively; `removed` reads `-removed` text grouped by `remove=`.
- An MFA appears in a list only if it has **both** a `DEPRECATIONS` line **and** the matching attribute in a built beam (so a stale line silently drops out).
- Output = trimmed `<type>_head.md`, then for each release in ascending order: `## OTP <N>`, the `<type>_<N>.md` fragment body verbatim if that file exists, then `### Functions Deprecated in OTP <N>` / `### Functions Scheduled for Removal in OTP <N>` / `### Functions Removed in OTP <N>` followed by `-   \`m:f/a\` (<text>)` bullets.
- Fragment discovery: `collect_template_rels/2` picks up every `<type>_<integer>.md` in the output dir (new release = new file, no code change). Anything else — notably `<type>_head.md` — is silently ignored, *not* treated as a release. The `_head.md` file is the page preamble only.
- `removed` guard: aborts if an MFA that actually carries a `-removed` attribute is listed with `remove=N` where `N > major(OTP_VERSION)`. Fragments/values naming future releases are fine as long as no `-removed` attribute exists yet.
- Heading anchor contract: `## OTP 24` ⇒ `#otp-24`; other apps' docs link here (`general_info:deprecations.md#otp-24`, `upcoming_incompatibilities.md#float_matching`, …). Do not change heading spelling/format.

## Authoring workflow
Add a deprecation:
1. add the attribute to the module(s) (`-deprecated/...`, following `HOWTO/DEPRECATE.md`);
2. add/update the MFA line in `DEPRECATIONS` (mandatory — `./otp_build update_deprecations` fails otherwise);
3. run `./otp_build update_deprecations` (regenerates `lib/stdlib/src/otp_internal.erl` + `bootstrap/lib/stdlib/ebin/otp_internal.beam`, commits both);
4. optionally add `deprecations_<major>.md` prose for the release section (plain `###` sub-headings, no `## OTP N` heading — the generator adds it).
Mark a removal: keep the `DEPRECATIONS` line, make `remove=<N>` ≤ current OTP major, add the `-removed` attribute, and optionally move prose into `removed_<N>.md`.
Runtime effect of the data: `since=` alone ⇒ "`m:f/a` is deprecated; <text>"; plus `remove=` ⇒ "... will be removed in OTP <N>"; `unsafe=` ⇒ unsafe-function warnings. `unsafe=` lines never appear on the three pages.

## Effects elsewhere (why the data must stay consistent)
- `make deprecations` (`Makefile.in`) runs the script's `update` mode → `lib/stdlib/src/otp_internal.erl`, whose `obsolete/3`, `obsolete_type/3`, `obsolete_callback/3` are consumed by `lib/stdlib/src/erl_lint.erl` (compile warnings) and `lib/edoc/src/edoc_data.erl`. `otp_internal.hrl` documents the tuple shapes.
- `lib/stdlib/scripts/update_deprecations` reads `$(ERL_TOP)/system/doc/general_info/DEPRECATIONS` by hard-coded relative path.

## Constraints
- NEVER edit or commit `deprecations.md`, `scheduled_for_removal.md`, `removed.md` — generated and gitignored; likewise never hand-edit `lib/stdlib/src/otp_internal.erl`.
- Release fragments MUST be named `<type>_<integer>.md`; a typo'd name is dropped silently (no error).
- `deprecations_head.md` / `scheduled_for_removal_head.md` / `removed_head.md` are fixed names; do not add release numbers to them.
- Every markdown file starts with an HTML-comment Apache-2.0 copyright block (`%CopyrightBegin%` … `%CopyrightEnd%`, license scanner requirement). Two styles coexist (`%%`-prefixed and `SPDX-FileCopyrightText:`); either is accepted. Fragment headers are copied verbatim into the generated page, so they must stay HTML comments.
- Do NOT add this directory to `../guides` — it is not a guide.

## Build wiring
- `../Makefile`: `DEPRECATIONS_SCRIPT`, `GENERATED_FILES`, one generation rule per type (prereqs `DEPRECATIONS` + `$(wildcard <type>_*.md)` + script), and the three files are extra prerequisites of `$(HTMLDIR)/index.html`.
- `../top/docs.exs` registers `../general_info/{deprecations,removed,scheduled_for_removal,upcoming_incompatibilities}.md` as `extras` of the top-level "Erlang/OTP" doc set, which renders them to `doc/general_info/*.html`; `make/ex_doc.exs` maps the `general_info:` link prefix to that directory. The `*_<N>.md` fragments, `*_head.md` and `DEPRECATIONS` are deliberately NOT extras.
- Top-level `make docs` order guarantees correctness: `lib/` → `erts/` → `system/doc` (generate + guides html) → `system/doc/top` (renders the extras). `../top/Makefile` itself does not depend on the generated files.
- Authoring reference: `HOWTO/DEPRECATE.md`.

## Known Issues
- No `general_info` entry exists in `../guides`, and `../docs.exs` `groups_for_extras` therefore does not cover these pages; the pages are extras of the top-level doc set only.
- `deprecations_22.md`, `deprecations_23.md`, `deprecations_24.md` and `removed_26.md` still contain hand-written `### Functions Deprecated/Removed in OTP <N>` lists that the generator also emits ⇒ duplicated heading and list in the rendered pages.
- No `deprecations_30.md` exists although `DEPRECATIONS` has an `Added in OTP 30` block (`erlang:exit/2,3`); fragments are optional, so this is only a missing prose section.
- `upcoming_incompatibilities.md` sections are out of order (OTP 30, 27, 28, 29) and the OTP 27/28/29 sections still describe past releases in future tense; they are kept because other apps' release notes link to their anchors (`#maybe_expr`, `#float_matching`, `#new_re_engine`, `#fun_creator_pid`).
- `upcoming_incompatibilities.md:269` uses `[]{}{: #max_connections_open }` (typo for `[](){: … }`), so that explicit anchor is not emitted.
- `remove=` values may point to past releases (e.g. `os_mon_mib:_/_ since=13 remove=22`) or to future ones (`slave:_/_`, `ct_slave:_/_` `remove=31` while OTP_VERSION is 30.0-rc0). Both are tolerated; only the `removed` page generation aborts, and only for entries that really carry a `-removed` attribute.
- `Parsed but unused` `mfa=` key in `DEPRECATIONS` (dead grammar).

## Notes for Agents
- The three generated pages are usually absent in a fresh checkout; do not grep for them and do not `git add` them.
- Changing `DEPRECATIONS` alone does not update `otp_internal` — `./otp_build update_deprecations` must be run (it needs a built tree).
- Editing a fragment/head file does not require touching `../Makefile`; the wildcard prerequisites pick up new files.

## Test Strategy
- `./otp_build update_deprecations` (or `make deprecations`) is the consistency gate: it fails listing MFAs that have `-deprecated`/`-removed`/`-unsafe` attributes but no `DEPRECATIONS` line.
- `erts/test/otp_SUITE.erl` `deprecated_not_in_obsolete/1` and `obsolete_but_not_deprecated/1` (xref-based) keep attributes ↔ `otp_internal` in sync; `call_to_deprecated/1` only reports.
- Page rendering is exercised only by a doc build (`make docs`), which requires ~all apps built.

## See Also
- `../../lib/stdlib/scripts/update_deprecations` — generator (`update` + `make_markdown` modes).
- `../../lib/stdlib/src/otp_internal.erl`, `otp_internal.hrl` — generated runtime registry.
- `../../HOWTO/DEPRECATE.md` — authoring howto for attributes and `DEPRECATIONS`.
- `../system_principles/misc.md` — prose explanation of the deprecation/removal strategy, linked from the page templates.
- `../top/docs.exs`, `../../make/doc.mk`, `../Makefile` — doc wiring. Sibling dirs are read-only from here; escalate writes to `../`.

## Routing Table
- This directory is flat — no child nodes. Related areas (read-only from here):
- `../../lib/stdlib/scripts/` → the generator script `update_deprecations`
- `../../lib/stdlib/src/` → generated `otp_internal.erl` and its consumer `erl_lint.erl`
- `../` (`system/doc`) → generation rules in `Makefile`, `guides`, `docs.exs`
- `../top/` → the ExDoc doc set that renders these pages
- `../../erts/test/otp_SUITE.erl` → consistency test suite
