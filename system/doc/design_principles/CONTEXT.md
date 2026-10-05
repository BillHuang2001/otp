# OTP Design Principles (guide)

## Intent
Source of the **OTP Design Principles** guide (`system/doc/`, built into the "Erlang System Documentation" app doc set): how to structure Erlang code into processes, behaviours, applications and releases. Prose/tutorial markdown only — no code lives here; the behaviours documented here are implemented in `lib/stdlib` (`gen_server`, `gen_statem`, `gen_event`, `supervisor`, `sys`, `proc_lib`) and released/upgraded by `lib/sasl`.

## API Surface (page inventory — `h1` = sidebar title, order = `docs.exs` order)
- `design_principles.md` — "Overview", landing page: supervision trees, behaviours (incl. the classic `ch1`/`server`/`ch2` channels example), applications, releases, release handling.
- `gen_server_concepts.md` — "gen_server Behaviour": client-server model, callback module, `start`/`call`/`cast`/`stop`, supervision-tree vs. standalone start. Uses setext headings and a `[](){: #gen_server }`-prefixed `h1`.
- `statem.md` — "gen_statem Behaviour": by far the most detailed behaviour chapter (event-driven state machines, callback modes `state_functions`/`handle_event_function`, time-outs, state-enter calls, postpone/inserted events, hibernation, fuzzy state diagrams).
- `events.md` — "gen_event Behaviour": event manager process, handlers list, add/notify/delete handler, stopping.
- `sup_princ.md` — "Supervisor Behaviour": supervision principles, flags, restart strategies (`one_for_one`/`one_for_all`/`rest_for_one`/`simple_one_for_one`), max restart intensity, `auto_shutdown`, child specs, adding/stopping children, simplified one_for_one supervisors.
- `spec_proc.md` — "sys and proc_lib": simple debugging via `m:sys`, special processes (start/loop, debug, system-message handling), user-defined behaviours (`-callback`).
- `applications.md` — "Applications": application concept, application callback module (`-behaviour(application)`), `.app` resource-file keys, development vs. released directory structure, application controller/master, loading/unloading, starting/stopping, configuring (`env`), start types.
- `included_applications.md` — "Included Applications": `included` key, primary vs. included applications, start-up synchronisation.
- `distributed_applications.md` — "Distributed Applications": distributed application controller, specification, failover/takeover (illustrated by `dist1..dist5` figures).
- `release_structure.md` — "Releases": release concept, `.rel` file, generating boot scripts (`systools`), release packages, released directory structure, disk-less/read-only clients.
- `release_handling.md` — "Release Handling": workflow, `appup`/`relup`, release-handling instructions (`load_module`, `update`, `add_module`/`delete_module`, application instructions, `apply`, `restart_new_emulator`, `restart_emulator`), appup/relup file contents, installing a release, updating application specs.
- `appup_cookbook.md` — "Appup Cookbook": per-change recipes for writing `.appup` files (functional/residence/callback modules, internal state, `code_change`, module dependencies, supervisors, added/removed modules, included applications, non-Erlang code, runtime restart).
- `secure_coding.md` — "Secure Coding Guidelines": large (≈2100 lines, 90 KB) and **self-contained**: threat model, Erlang-specific pitfalls, per-application guidelines (`crypto`, `ssl`, `ssh`, `public_key`, `xmerl`), 30 numbered rules (`STL-*`, `DEP-*`, `DSG-*`, `LNG-*`, `MSC-*`), Top 40 CWEs / OWASP Top 10 / OWASP API Top 10 tables, plus 110 explicit anchors (`#cwe-*`, `#owasp-a*`, `#h-*`, `#rule-*`). It links to NO sibling page here and is presented as its own guide (see Known Issues).

## Internal cross-reference structure
Page-relative links (`page.md#anchor`), validated by ex_doc against `docs.exs` extras. Referenced anchors act as frozen contracts between chapters:
- `design_principles.md`: `#ch1` (channels example) ← `gen_server_concepts.md`, `spec_proc.md`; `#channels-implementation` ← `spec_proc.md`; `#behaviours` (h2, implicit anchor) ← **outside** this guide: `system/doc/reference_manual/modules.md`.
- `gen_server_concepts.md#ex` ← `appup_cookbook.md`, `sup_princ.md`.
- `spec_proc.md#ex` ← `appup_cookbook.md`; `spec_proc.md#debug`/`#msg` are the guide's own sys-debug contract.
- `sup_princ.md#ex` ← `applications.md`, `appup_cookbook.md`; `sup_princ.md#shutdown` ← `events.md`, `gen_server_concepts.md`, `statem.md`; `sup_princ.md#spec` ← `appup_cookbook.md`.
- `release_handling.md#sys` ← `applications.md`; `#appup` ← `appup_cookbook.md`; `#req` ← `release_structure.md`; `#restart_new_emulator_instr` / `#restart_emulator_instr` ← `appup_cookbook.md`.
- `release_structure.md#ch_rel` ← `release_handling.md`; `applications.md#ch_app` / `#app_dir` ← `release_structure.md`; `appup_cookbook.md#int_state` / `#sup` ← `release_handling.md`; `events.md#mgr` ← `sup_princ.md`.
- `statem.md#example` ← `spec_proc.md` (this one relies on the **implicit** `h2` heading anchor "Example", not an explicit `{: #example }` marker).
- The landing page links only 7 of the 13 pages (all four behaviour chapters, applications, releases, release handling). `spec_proc.md`, `appup_cookbook.md`, `included_applications.md`, `distributed_applications.md` and `secure_coding.md` are reachable only via the sidebar — fine, but worth remembering when reordering/moving pages.
- Guide-local figure assets: `statem.md` → `assets/ballpoint-pen.svg`; `distributed_applications.md` → `assets/dist1..dist5.gif`.
- Outbound cross-guide references (autolink `e:app:page`): `e:kernel:app.md`, `kernel:config.md`, `kernel:kernel_app.md`, `e:sasl:appup(.md)`, `sasl:relup.md`, `sasl:rel.md`, `sasl:script.md`, `sasl:sasl_app.md`, `e:system:create_target.md`, `system:typespec.md`, `erts:erl_cmd.md`, plus `secure_coding.md`'s many `m:`/`t:`/`e:` links into `crypto`, `ssl`, `ssh`, `public_key`, `xmerl`, `erts` (`erl_nif`, `erl_driver`, `erl_dist_protocol`), `mnesia`, `kernel`, `system`. `secure_coding.md` mixes `.md` and explicit `.html` suffixes in `e:` links; both work (`convert_extra_extension` only rewrites `.md`/`.txt`/`.livemd`/`.cheatmd`).

## Constraints
- Register every page in `system/doc/docs.exs` `extras:` (path **and** position). A page not listed is not built; the listed order is the sidebar order inside the group (extras are only stably sorted by group index).
- Keep the `%CopyrightBegin%` … `%CopyrightEnd%` Apache-2.0 HTML-comment header (lines 1–21) at the top of every `.md`; `h1` text becomes the page title/nav label.
- Reference figures as flat `assets/<file>` (resolved against `system/doc/assets/`, where `system/doc/Makefile`'s `assets.copy` flattens `*/assets/*.gif|png|svg`; the directory name `design_principles` must stay in `system/doc/guides` for the copy to happen). Do not hand-edit the exported `.gif`/`.svg`; the `.fig`/`.odg` sources are next to them (see `assets/CONTEXT.md`).
- Anchor conventions used here: explicit `[](){: #name }` (plus `#name-cased-with-dashes` for rule/CWE anchors in `secure_coding.md`), reference-style links (`[Foo]: #anchor`) inside `secure_coding.md`, mermaid diagrams as fenced ```mermaid``` blocks (rendered by the hook in `make/ex_doc.exs`; used in `design_principles.md`, `gen_server_concepts.md`, `included_applications.md`, `statem.md`, `sup_princ.md`), autolinks `m:`, `t:`, `f:`, `e:app:page`.
- Prefer `m:`/`t:` autolinks over hard-coded URLs so links survive module moves; they are the mechanism by which these chapters stay in sync with `lib/stdlib` and `lib/sasl`.

## Known Issues
- **`secure_coding.md` is not in a "Secure Coding Guidelines" group.** `system/doc/guides` has `secure_coding:Secure Coding Guidelines`, which becomes the regex `~r/secure_coding/` in the `groups_for_extras` of `system/doc/docs.exs`, but the extra's match input is its path `design_principles/secure_coding.md`, which already matches the earlier entry `design_principles:OTP Design Principles` (`~r/design_principles/`). ex_doc picks the **first** matching pattern (`ExDoc.Config.match_group_patterns` → `Enum.find_value`, ex_doc 0.40.4), so the page renders under the group **"OTP Design Principles"** and no "Secure Coding Guidelines" group exists at all (verified against the generated OTP docs). The `guides` line currently only affects the Makefile `GUIDES` variable (asset scan, `secure_coding/assets/` does not exist) — it has no grouping effect. To actually separate the guide, either move the `secure_coding:` line **above** `design_principles:` in `guides`, or move/rename the page out of `design_principles/`.
- Anchor edits are silent breakage: intra-guide `.md#anchor` links are not checked for anchor existence by ex_doc, and cross-guide `e:app:page.md#anchor` links are neither validated nor rewritten (the page name is simply appended to the target app's doc URL). Renaming the `## Behaviours` heading in `design_principles.md` breaks `e:system:design_principles.md#behaviours` in `system/doc/reference_manual/modules.md`; changing the "Example" `h2` in `statem.md` breaks `spec_proc.md#example`.
- Unmatched `assets/<file>` links *are* checked (against `system/doc/assets/`), so a missing/renamed figure surfaces as an ex_doc warning (build failure with `EX_DOC_WARNINGS_AS_ERRORS=true`); anchors do not.
- Mixed heading styles: `gen_server_concepts.md` and `statem.md` use setext (`===`/`---`) headings while the rest use ATX (`#`). ex_doc only assigns implicit ids to `h2`/`h3`, so `h4`-level headings (several in `secure_coding.md`) cannot be linked to by heading slug.

## Notes for Agents
- Editing one chapter usually means editing the sibling it links to: `release_handling.md` ↔ `appup_cookbook.md` and `release_structure.md`, `sup_princ.md` ↔ all behaviour chapters, `applications.md` ↔ `sup_princ.md`/`release_structure.md`/`distributed_applications.md`. Check the chapter's inbound links (list above) before touching an `h2` heading or an explicit anchor.
- These chapters are prose contracts for behaviour callbacks and `.app`/`.appup`/`.rel` file formats; the normative reference lives in another doc set — verify behaviour semantics against `m:gen_server`, `m:gen_statem`, `m:gen_event`, `m:supervisor`, `m:sys`, `m:proc_lib` (STDLIB) and `m:release_handler`, `m:systools`, `m:appup`, `m:relup` (SASL) rather than trusting the tutorial text alone.
- To regenerate docs: `make` in `system/doc` (ex_doc via `make/doc.mk`, global config `make/ex_doc.exs`); requires `ERL_TOP` set from the `system/doc` Makefile's perspective and `EX_DOC_VERSION` in the environment. Never edit `configure`/`Makefile`; the only file in this directory to change for build wiring is `docs.exs`'s sibling `extras:` list.
- `warnings_as_errors` is enabled during OTP doc builds, so a stale link fails CI, not just the local build.

## Routing Table
- `assets/` → figure sources and exported images for this guide (`.fig`/`.odg` sources, `.gif`/`.svg` exports; flat `assets/<file>` reference convention, `assets.copy`/gitignore rules)

## See Also
- `../system_principles/` (sibling — read-only, escalate writes to `../`): boot scripts, `create_target.md`, `upgrade.md`, error logging for the system view of what these chapters describe.
- `../reference_manual/` (sibling — read-only): `modules.md` (behaviour attribute, links here), `code_loading.md` (code replacement), `ref_man_processes.md` (exit signals, process dictionary, links here).
- `../../lib/stdlib/` — `gen_server`, `gen_statem`, `gen_event`, `supervisor`, `sys`, `proc_lib` reference manuals (callback contracts documented here).
- `../../lib/sasl/` — `appup`, `relup`, `script`, `rel` file formats, `systools`/`release_handler` (the tooling behind `release_handling.md` and `appup_cookbook.md`).
- `../../lib/kernel/` — `application` module and `kernel_app`/`config` references for `applications.md`.
- `../../system/doc/README.md` — index that links `design_principles/design_principles.md`.
