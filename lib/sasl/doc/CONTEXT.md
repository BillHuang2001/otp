# SASL Application Documentation

## Intent
Documentation sources for the `sasl` (System Architecture Support Libraries) application.
Built by ExDoc into HTML pages plus man pages; `make docs` here delegates entirely to `$(ERL_TOP)/make/doc.mk`.

## API Surface
- `Makefile` — thin wrapper: includes `make/target.mk`, `make/$(TARGET)/otp.mk`, `../vsn.mk` (`VSN=$(SASL_VSN)`, `APPLICATION=sasl`), then `$(ERL_TOP)/make/doc.mk`. No app-specific rules.
- `docs.exs` — ExDoc config: `extras` (in publication order) and `skip_code_autolink_to: ["release_handler:mbj_func/1"]`.
- `sasl_app.md` — application description (man section 6): services, config parameters (`start_prg`, `masters`, `client_directory`, `static_emulator`, `releases_dir`) and the deprecated `error_logger`-handlers/`sasl_error_logger`/`errlog_type`/`utc_log`/`log_mf_h` section.
- `notes.md` — SASL release notes (per-version change list, `Own Id`/`Aux Id` format); regenerated at release time.
- `references/` — normative *file-format* specs, published as man section 4 pages.
- `guides/` — narrative guides, published as HTML extras.
- `src/rel/` — example Erlang modules backing the release-handling docs / Appup Cookbook.

## Build & Publishing
- Published page set = `docs.exs` `extras` ∪ `make/ex_doc.exs` auto-globs `doc/*.md` and `doc/{guides,references,internal_docs}/*.md`.
- `notes.md` is picked up by that auto-glob (it is NOT listed in `docs.exs`); `docs.exs` order wins for items listed there.
- Module docs come from the compiled `../ebin/*.beam`s (i.e. `-moduledoc` in `../src/*.erl`); only modules with a *real* `-moduledoc` are published: `systools`, `alarm_handler`, `release_handler`, `rb`. Modules marked `-moduledoc false.` are excluded.
- `references/rel.md`, `script.md`, `appup.md`, `relup.md` are MAN4 (section 4); `sasl_app.md` is MAN6; documented modules are MAN3 (see `MAN4_DEPS`/`MAN6_DEPS` in `../make/doc.mk`… actually `$(ERL_TOP)/make/doc.mk`).
- HTML built via `make/ex_doc_wrapper` (needs ex_doc, see `make/check_ex_doc`); man pages via `make/markdown_to_man.escript`.

## Constraints
- These markdown/`.erl` sources are the source of truth for the `.rel`/`.script`/`.appup`/`relup` formats — they must match what `systools` and `release_handler` actually produce/consume.
- Do not hand-edit generated output (`html/`, man pages, chunks); edit sources here and rebuild.
- `docs.exs` `extras` order defines published page order; keep it intentional.
- Cross-references use the ExDoc custom-anchor syntax `[](){: #anchor }` (see `sasl_app.md`, `guides/error_logging.md`).

## Known Issues
- `src/rel/*.erl` are referenced by no Makefile and are not published by ExDoc; they are reference example code consumed conceptually by the Appup Cookbook in `system/doc/design_principles/appup_cookbook.md` (e.g. the `portc` port-upgrade example). Keep them consistent with the cookbook text.

## Routing Table
- `references/` → normative `.rel`/`.script`/`.appup`/`relup` file-format specs (man 4); `appup.md` also documents high-/low-level upgrade instructions for `release_handler`
- `guides/` → narrative guides: `sasl_intro.md` (scope), `error_logging.md` (deprecated OTP-21-era SASL error logging + Report Browser)
- `src/rel/` → example Erlang modules (`gs1`/`gs2`/`sp`/`sup`/`bar`/`lists2`/`ge_h`/`portc`, versioned `Mod.Vsn.erl`) for the release/appup cookbook
- `sasl_app.md` → application description + configuration parameters (man 6)
- `notes.md` → release notes for the `sasl` app

## See Also
- `../CONTEXT.md` — the `sasl` application (sources in `../src/`).
- `../examples/` — `target_system.erl` sample (sibling — read-only from here).
- `../../system/doc/design_principles/appup_cookbook.md` and `../../system/doc/system_principles/create_target.md` — the System docs that consume these example modules.
- `../../make/doc.mk`, `../../make/ex_doc.exs` — the shared doc build machinery.
