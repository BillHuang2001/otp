# megaco — Documentation

## Intent
User-facing documentation for the `megaco` application (Megaco/H.248 media-gateway-control protocol stack): the ExDoc build config, nine prose guides, per-release notes, the bundled protocol specifications and figure/asset sources.
Source-text for the module API reference does NOT live here — it comes from `-moduledoc`/`-doc` attributes in `../src/**.erl`.

## API Surface
- `Makefile` — doc build entry point; includes `../vsn.mk` (`VSN=$(MEGACO_VSN)`), sets `APPLICATION=megaco`, then includes `$(ERL_TOP)/make/doc.mk`.
  No `MAN*_DEPS` / `CHUNK_FILES` set, so only the default `html` target is built (no man pages, no chunks).
- `docs.exs` — ExDoc config; its only content is the ordered `extras:` list of guides.
  The list order determines how the guides appear in the generated docs.
- `../make/doc.mk` (shared) — `html` target runs `make/ex_doc_wrapper` with `-c ../make/ex_doc.exs` over `../ebin/*.beam` plus `*.md` and `*/**.md`; output goes to `html/`.

## Contents
- `guides/` — nine Markdown guides (all Apache-2.0), referenced in order by `docs.exs`.
- `notes.md` — "Megaco Release Notes": one `## Megaco <vsn>` section per release, newest first; each bullet carries `Own Id: OTP-xxxxx` and (often) `Aux Id: [PR-nnnnn]` with a link reference definition.
- `standard/` — verbatim protocol specs, each with a `.license` sidecar: `rfc3525.txt` (Megaco/H.248 v1), `rfc4234.txt` (ABNF), `rfc4566.txt` (SDP), `draft-ietf-megaco-h248v2-04.txt` (H.248 v2 draft), `implementors_guide_v10-13.pdf`.
- `src/` — figure/asset sources only (no `.erl`, no API `.md`): `MG-startup_flow_noMID.fig` (+`.license`), `mstone1.jpg` (+`.license`), `meas.log` (mstone measurement sample).
- `specs/.gitignore` — ignores generated `specs_*.xml`; the directory holds no tracked content.
- `html/` — generated ExDoc output (git-ignored via `../.gitignore` `/doc/html`).

## Guides (`guides/`, in `docs.exs` order)
- `megaco_intro.md` — protocol/API introduction and conceptual overview.
- `megaco_architecture.md` — internal architecture of the megaco stack.
- `megaco_run.md` — starting/running a megaco node, initial configuration (key: `initial_config`).
- `megaco_encode.md` — configuring Erlang-distribution, text and binary codecs.
- `megaco_transport_mechanisms.md` — transport behaviours and built-in transports.
- `megaco_examples.md` — usage examples.
- `megaco_mib.md` — how the Megaco MIB is handled.
- `megaco_performance.md` — performance notes / measurement.
- `megaco_debug.md` — mstone1/mstone2 measurement tools, message transformation and other debugging aids.

## Constraints
- The set and order of top-level guides is defined solely by `extras:` in `docs.exs` — a new guide must be added there to appear in the docs.
- The module API reference text is authored in `../src/**.erl` (`-moduledoc`/`-doc`), never in this directory.
- `html/` is generated — never edit it by hand.
- Any bundled non-OTP-licensed asset must have a matching `<file>.license` sidecar with an SPDX identifier (e.g. `NOASSERTION` for RFCs; `Apache-2.0` for Ericsson-authored `.fig`/`.jpg`).

## Notes for Agents
- A change to the public API normally requires (a) updating `-doc`/`-moduledoc` in `../src/`, (b) adding a `notes.md` entry under the current version, and possibly (c) a guide update.
- `notes.md` is append-at-top: add the new release section above the previous one; the newest listed version is currently `Megaco 4.9.2`.
- `src/*.fig`, `src/*.jpg` and `src/meas.log` are standalone source assets; the figures are not currently referenced from the Markdown guides.
- To build the docs run `make` in this directory (requires an ex_doc install / network; see `../make/doc.mk` `check_ex_doc`).

## Routing Table
- `./guides/` — the nine prose guides listed in `docs.exs` (edit here for user-guide changes).
- `./standard/` — bundled RFCs/draft/PDF specifications + `.license` sidecars (read-only reference material).
- `./src/` — figure/asset sources (`.fig`, `.jpg`, `meas.log`) contributing to the doc build.
- `./specs/` — generated `specs_*.xml` output (git-ignored; only `.gitignore` tracked).
- `./notes.md` — per-release changelog (newest-first).
- `./docs.exs` — ExDoc config: ordered `extras` guide list.
- `./Makefile` — doc build driver (via `$(ERL_TOP)/make/doc.mk`).
- `../src/` — megaco Erlang sources; authored API-reference text lives there (sibling app source, read-only from here — escalate writes to `../`).
