# diameter — Documentation

## Intent
Source inputs for the published `diameter` application documentation (HTML via ExDoc + man pages), plus repo-only reference material (RFC texts, release notes, legacy XML-doc helpers).
Only the ExDoc extras (`guides/`, `references/`), `notes.md` and the `-moduledoc` comments in `../src/**/*.erl` feed the published docs.

## API Surface / Build
- `Makefile` — includes `$(ERL_TOP)/make/doc.mk` with `APPLICATION=diameter` and `VSN=$(DIAMETER_VSN)` (from `../vsn.mk`); defines no local targets.
- `docs.exs` — local ExDoc config: `extras` order (`guides/diameter_intro.md`, `guides/diameter_using.md`, `guides/diameter_examples.md`, `guides/diameter_soc.md`, `references/diameterc_cmd.md`, `references/diameter_dict.md`) and `skip_code_autolink_to: ["diameter_traffic:request/1"]`.
- Build targets come from the OTP-wide `make/doc.mk`: `docs`/`html` (ExDoc via `make/ex_doc_wrapper`), `chunks`, `man`, `release_docs_spec`.
- man3 pages are generated from `../src/**/*.erl` files that do NOT contain `moduledoc false.`: `diameter.erl`, `diameter_app.erl`, `diameter_codec.erl`, `diameter_make.erl`, `diameter_transport.erl`, `diameter_tcp.erl`, `diameter_sctp.erl` (7 modules → `diameter(3)`, `diameter_app(3)`, `diameter_codec(3)`, `diameter_make(3)`, `diameter_transport(3)`, `diameter_tcp(3)`, `diameter_sctp(3)`).
- man4 page `diameter_dict(4)` ← `references/diameter_dict.md`; man1 page `diameterc(1)` ← `references/diameterc_cmd.md`.
- No `man6` page: there is no `<app>_app.md` application-overview file (unlike `kernel`/`ssh`), so the app description ("Diameter protocol") comes only from `../src/diameter.app.src`.
- `make/ex_doc.exs` auto-adds `Path.wildcard("*.md")` to `extras` (so `notes.md` is published as "Release Notes" even though it is not listed in `docs.exs`) and groups guides → "User's Guides", `references/*_cmd.md` → "Command Line Tools", `references/` → References.
- `src/` — legacy helpers of the retired XML/edoc doc pipeline: `depend.sed`, `seehere.sed`, `seealso.ent`; none is referenced by any current build file.
- `standard/` — RFC texts used as the normative reference for the guides, with `.license` SPDX sidecars where the license is not Apache-2.0.

## Constraints
- Reference documentation for the API modules lives in `-moduledoc`/edoc comments in `../src/**/*.erl` — edit it there, never duplicate it into `doc/`.
- `seealso.ent` entities must be updated in the same change as any renamed/removed module, function or type they reference (only matters if the XML doc build is revived).
- Build output (`html/`, `man*/`, `pdf/`, and `src/depend.mk`/`src/seehere.ent`) is git-ignored via `doc/.gitignore` and `src/.gitignore`.

## Known Issues / Gotchas
- `notes.md` (~74 KB) is a large, hand-maintained "Release Notes" document (per-version Fixed Bugs/Improvements plus a long historical "Known Issues/Limitations" tail) — keep edits manual and append-only at the top.
- `guides/diameter_soc.md` (~89 KB) is by far the largest guide (RFC 6733 standards-compliance tables); `guides/diameter_using.md` and `guides/diameter_examples.md` are near-empty stubs ("To be written.").
- `doc/src/depend.sed`, `seehere.sed` and `seealso.ent` are dead legacy artifacts — there are no `src/*.md` sources, and no man3 page is generated from `src/`.
- `standard/` RFC texts (including the RFC 3588/4005/4006/4072/4740/3539/3124 set with no sidecar) ARE git-tracked (`git ls-files lib/diameter/doc` lists them); `lib/diameter/.gitignore` is empty, so nothing here is ignored — but they are repo-only: `STANDARDS` is unset in `Makefile`, so they are NOT installed into the release.
- `diameter_traffic` (`../src/base/diameter_traffic.erl`, `moduledoc false`) is undocumented, hence the `skip_code_autolink_to` entry in `docs.exs`.

## Routing Table
| Path | Contents |
| --- | --- |
| `docs.exs` | Local ExDoc config (extras order, autolink skips) |
| `Makefile` | Doc build driver; includes `make/doc.mk` (ExDoc + man targets) |
| `notes.md` | Hand-maintained diameter Release Notes (auto-included ExDoc extra) |
| `guides/` | ExDoc guide pages: `diameter_intro.md`, `diameter_using.md`, `diameter_examples.md`, `diameter_soc.md` |
| `references/` | Man/extra reference sources: `diameterc_cmd.md` (→ `diameterc(1)`), `diameter_dict.md` (→ `diameter_dict(4)`) |
| `src/` | Legacy XML-doc helpers (`depend.sed`, `seehere.sed`, `seealso.ent`) — unused by the current build |
| `standard/` | Repo-only RFC texts (`rfc*.txt` + `.license`), not built or installed |

## See Also
- `../src/` — Erlang sources; the `-moduledoc`/edoc comments here are the source of the man3 reference pages.
- `../examples/` — example code referenced by `guides/diameter_examples.md`.
- `../../make/doc.mk`, `../../make/ex_doc.exs` — the OTP-wide documentation build.
