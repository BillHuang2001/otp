# OTP Design Principles — Figure Assets

## Intent
Editable diagram sources plus their exported figures for the OTP Design Principles guide (`system/doc/design_principles/`).
Only the exported files (`.svg`/`.gif`) are consumed by the ExDoc build; the sources (`.odg`/`.fig`) exist so the figures can be regenerated later.
All 12 files are tracked in git.

## API Surface (files + referencing page)
| File | What it is | Referenced from |
|---|---|---|
| `ballpoint-pen.odg` | LibreOffice Draw 7.3.7.2 source (ODF drawing, mimetype `application/vnd.oasis.opendocument.graphics`, one page `page1`, created 2024-06-12). No build reference. | — |
| `ballpoint-pen.svg` | LibreOffice SVG export (`svg` 1.2 namespace, `xmlns:ooo`), 210.06mm × 50.8mm. All labels are baked into vector `<path>` elements — the file has no `<text>` elements. | `statem.md:75` → `assets/ballpoint-pen.svg` ("Ballpoint Pen") |
| `dist1.fig` … `dist5.fig` | XFig editable sources of the distributed-application failover/takeover figures. Draw `myapp` plus nodes `cp1`/`cp2`/`cp3`; `dist2`/`dist3` carry a "5 secs." timer label; `dist5` contains takeover shell commands (e.g. `cp1: application:takeover(myapp, permanent)`). Encodings: `dist1.fig`/`dist4.fig` are `#FIG 3.1`; `dist2/3/5.fig` are `#FIG 3.2 Produced by xfig version 3.2.8b`. | — |
| `dist1.gif` … `dist5.gif` | GIF87a static single-frame exports (2.2–7 KB, no animation, no NETSCAPE loop extension). | `distributed_applications.md:137,175,182,206,215` → `assets/distN.gif` (Situations 1–5) |

## Constraints
- Reference convention: markdown always uses the **flat** form `assets/<basename>` (relative to the guide page URL), never a path into this directory.
- An exported file must keep the same basename as its source (`dist1.fig` → `dist1.gif`).
- Only `.gif`, `.png` and `.svg` are ever copied: `ASSETS_SRC` in `system/doc/Makefile` wildcards exactly those three extensions per guide. Adding another format requires editing that wildcard.
- The copy target `assets.copy` does `cp -f ... assets/` — everything is **flattened** into `system/doc/assets/`. Basenames therefore must be unique across all guides; a collision silently overwrites another guide's asset.
- `design_principles` must stay in the `system/doc/guides` list, otherwise this directory is never scanned and ExDoc reports the references as missing assets (fatal when `EX_DOC_WARNINGS_AS_ERRORS=true`).
- The copied files are build outputs and are **not** committed: `system/doc/assets/.gitignore` ignores `*.png`, `*.gif`, `*.svg`. The originals in this directory are the tracked copies.
- `.gif`/`.svg` are generated artifacts — never hand-edit them; change the `.fig`/`.odg` source and re-export.
- `.fig`/`.odg` are never referenced by markdown and never copied into the built docs; keep them alongside the exports.

## Notes for Agents
- Export is manual and out-of-band: no makefile, script or CI job in the tree invokes xfig/`fig2dev`/`transfig` or LibreOffice for these files. (`lib/mnesia/doc/misc/Makefile` and `lib/tools/doc/src/venn*.fig` are unrelated legacy doc leftovers.)
- `.fig` → `.gif`: xfig / `fig2dev -L gif`. `.odg` → `.svg`: LibreOffice Draw "Export as SVG"; the exact export options used (notably "text as curves") are **not** recorded anywhere in the repo.
- The ballpoint-pen figure is complemented in `statem.md` by an inline `mermaid` `stateDiagram-v2` block showing the same state machine — keep the SVG and the mermaid text in sync when editing.

## See Also
- `../CONTEXT.md` — the OTP Design Principles guide itself (page list, build wiring)
- `../../CONTEXT.md` — `system/doc/` build (`Makefile` `assets.copy` target, `guides` list, `make/ex_doc.exs` `assets:` mapping)
