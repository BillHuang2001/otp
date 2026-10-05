# erl_interface — Documentation

## Intent
Documentation sources for the `erl_interface` application, which is a C/Java-facing library (`../src/` is C, the app declares `{modules, []}`) — the audience is C programmers writing `ei`-based C nodes and port programs, plus users of the `erl_call` tool.
Content is one user's guide, four hand-written API reference pages and the app's release notes; there are no XML/DTD sources and no page generator — the `.md` files here are the single source of truth.
Everything published for this app comes from the extras in this directory (no module/function API pages are generated because the app has no Erlang modules).

## API Surface
- `docs.exs` — ExDoc config: `main: "ei_users_guide"` (landing page), `api_reference: false`, and `extras` in published order: `guides/ei_users_guide.md`, `references/ei.md`, `references/ei_connect.md`, `references/ei_global.md`, `references/erl_call_cmd.md`.
- `guides/ei_users_guide.md` — "Erl_Interface User's Guide", the ExDoc `main` page and the narrative tutorial (see `guides/CONTEXT.md`).
- `references/ei.md`, `references/ei_connect.md`, `references/ei_global.md`, `references/erl_call_cmd.md` — per-function reference pages, each documenting one public header / tool; they double as the `ei(3)`, `ei_connect(3)`, `ei_global(3)` and `erl_call(1)` man-page sources (see `references/CONTEXT.md`).
- `Makefile` — 36 lines: includes `$(ERL_TOP)/make/target.mk`, `make/$(TARGET)/otp.mk`, `../vsn.mk` (`VSN=$(ERL_INTERFACE_VSN)`, i.e. `EI_VSN`) and finally `$(ERL_TOP)/make/doc.mk` with `APPLICATION=erl_interface`.
- `notes.md` — app release notes (`# Erl_interface Release Notes`, 100 `## Erl_Interface <vsn>` sections, newest first, down to `3.4.1`); not listed in `docs.exs`.

## How These Docs Are Built and Published
- `make` in this directory builds `docs`, i.e. `check_ex_doc` → `html` (ExDoc) → `man` (whenever reference markdown exists).
- HTML is produced by ExDoc driven by `make/ex_doc.exs` merged with the local `docs.exs`, with the program spec merged from `make/ex_doc_wrapper.in`; output goes to `html/`, linking sibling apps via their `../<app>/doc/html` directories (so building docs of one app is normally done after the other apps are built).
- `make/ex_doc.exs` computes extras as local `docs.exs` extras ++ `Path.wildcard("*.md")` ++ `Path.wildcard("{guides,references,internal_docs}/*.md")`, then `Enum.uniq()`: listing a page in `docs.exs` fixes sidebar order, and unlisted top-level `*.md` (here `notes.md`) is still published, appended after the listed pages.
- Sidebar groups come from `groups_for_extras` in `make/ex_doc.exs`: `guides/...` → "User's Guides", `references/*_cmd.md` → "Command Line Tools", other `references/...` → "References".
- Man pages are derived from the same markdown by `make/doc.mk` via `make/markdown_to_man.escript` (stdlib's `man_docs`): `*/*_cmd.md` → `man1/erl_call.1`, `references/*.md` → `man3/{ei,ei_connect,ei_global}.3`. There is no `MAN4`/`MAN6` input here (no `app.md`/`*_app.md`), and `CHUNK_FILES` is unset, so no chunks are produced.
- `release_docs` installs to `$(RELEASE_PATH)/lib/erl_interface-$(VSN)/doc/html` (`release_html_spec`, followed by `make/fixup_doc_links.sh`) and to `$(RELEASE_PATH)/man/man[13]`; the fixup script has an `erl_interface` special case because this app's version variable in `../vsn.mk` is `EI_VSN`, not `ERL_INTERFACE_VSN`.
- Release-notes entries are a release-process artifact: sections are added at the top of `notes.md` together with `../vsn.mk` in "Prepare release" commits, not as part of ordinary code changes.

## Constraints
- Hand-written markdown only; nothing in this directory is generated, so there is no source/regeneration step to prefer over editing these files.
- File names, page headings, `## <function>()` headings and `{: #... }` anchors are public link targets used from other applications (`erts/doc/guides/driver.md` → `e:erl_interface:ei.md#ei_x_encode_string`; `system/doc/reference_manual/distributed.md`, `system/doc/tutorial/cnode.md` → `e:erl_interface:ei_users_guide.md`; `system/doc/embedded/embedded.md` → `e:erl_interface:erl_call_cmd.md`) — renaming a file/heading silently breaks them and also renames the generated man pages.
- Reference pages must stay in sync with the public headers `../include/ei.h`, `ei_connect.h`, `ei_global.h` and with the compile-only smoke test `../src/prog/ei_fake_prog.c` (rule stated in `../src/README.internal`, whose other file inventory is stale).
- Autoref links out of the app (`` `m:global` ``, `` `e:erts:epmd_cmd.md` ``, and inbound `` `e:erl_interface:...` `` links) only resolve in the release-wide doc build; ExDoc warnings-as-errors is disabled automatically by `make/ex_doc_wrapper` when any `lib/*/ebin/*.app` is missing.
- `html/` (ignored by the root `.gitignore` rule `/lib/*/doc/html`) and `man1/`, `man3/` (`*.3`) are build artifacts — never edit or commit them.
- Keep the Apache-2.0 `%CopyrightBegin%` header (HTML comment in markdown) intact in these files.

## Notes for Agents
- To document a newly exported `ei` function, touch three places: the header in `../include/`, a call in `../src/prog/ei_fake_prog.c`, and a `## <name>()` section in the matching page of `references/`.
- `notes.md` uses link-reference definitions (`[PR-11492]: https://…`) and link targets that ExDoc cannot resolve; `make/ex_doc.exs` therefore sets `skip_undefined_reference_warnings_on: ["notes.md"]`.
- Legacy release-note sections use the spelling `## Erl_interface <vsn>` while recent ones use `## Erl_Interface <vsn>`; a new section follows the newer spelling.
- `../../../system/doc/tutorial/erl_interface.md` and the `cnode*.c` examples are the separate release-wide port/C-node tutorial (legacy `erl_interface.h`-based) — not part of this app's doc build, do not merge with it.

## Test Strategy
- No tests here: correctness is enforced at doc-build time (ExDoc unresolved-reference/link warnings, `make docs` / `make html` in this directory).
- The docs ↔ header consistency rule is only probed indirectly by compiling `../src/prog/ei_fake_prog.c` (`make check` in `../src/`), which verifies the headers/libraries, not the prose.

## Routing Table
- `guides/` → User's guide / ExDoc landing page (`ei_users_guide.md`) — see `guides/CONTEXT.md`
- `references/` → Per-function reference pages and man-page sources (`ei.md`, `ei_connect.md`, `ei_global.md`, `erl_call_cmd.md`) — see `references/CONTEXT.md`
- `docs.exs` → ExDoc entry point (main page, extras order, `api_reference: false`)
- `notes.md` → App release notes, published as an extra by the shared wildcard
- `Makefile` → `make/doc.mk` wiring (VSN, `APPLICATION=erl_interface`)

## See Also
- `../src/` — C implementations of everything documented here (`encode/`, `decode/`, `connect/`, `global/`, `epmd/`, `misc/`); `../src/prog/ei_fake_prog.c`, `../src/prog/erl_call.c`; `../src/README.internal` internal rules.
- `../include/` — public headers `ei.h`, `ei_connect.h`, `ei_global.h`, `eicode.h` documented by the reference pages.
- `../../system/doc/` — release-wide documentation build (`docs.exs`, `top/`), the port/C-node tutorial and the docs conventions in `reference_manual/documentation.md`.
- `../../../make/doc.mk`, `../../../make/ex_doc.exs`, `../../../make/markdown_to_man.escript`, `../../../make/fixup_doc_links.sh` — the build machinery used by this directory.
