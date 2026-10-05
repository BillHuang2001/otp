# SSH Application — Documentation

## Intent
Source of the published `ssh` documentation pages: the application overview page, the release notes, and the user's-guide chapters.
Contains only `.md` sources (plus the ExDoc config and doc Makefile) — the page bodies are rendered to HTML/man by the shared OTP docs build (`../../make/doc.mk`) driven by ExDoc.
The module/function reference documentation is NOT here — it is generated from `-moduledoc`/`-doc` attributes in `../src/*.erl` and pulled in at doc-build time.

## API Surface
- `docs.exs` — ExDoc local config.
  - `extras:` lists the guide files and `ssh_app.md` in *display order* (`introduction`, `using_ssh`, `terminology`, `configurations`, `configure_algos`, `hardening`, `ssh_app.md`).
  - `annotations_for_docs:` callback that adds a `[md[:rfc]]` "RFC" annotation label when a doc block carries `rfc:` metadata.
  - No `groups_for_extras` here — grouping is applied by `../../make/ex_doc.exs`, which maps any path matching `~r/guides/` to the "User's Guides" group.
  - `notes.md` is deliberately absent from `extras`; it is auto-included by `../../make/ex_doc.exs` via `Path.wildcard("*.md")`.
- `Makefile` — includes `../../make/target.mk`, `../../make/$(TARGET)/otp.mk`, `../vsn.mk` (`VSN=$(SSH_VSN)`), sets `APPLICATION=ssh`, then includes `../../make/doc.mk`.
  - Targets come from `doc.mk`: `docs` (default; html + man + chunks), `html`, `man`, `chunks`, `clean`/`clean_docs`, and release targets `release_html_spec` / `release_chunks_spec` / `release_man_spec`.
  - `html` runs `../../make/ex_doc_wrapper <formats> ssh $(DOC_VSN) ../ebin -o html -c ../../make/ex_doc.exs`.
- `ssh_app.md` — the SSH application overview page (description, dependencies, config file list, public/host keys, supported RFCs). Also renders to the `ssh(6)` man page (`MAN6_DEPS := $(wildcard *_app.md)` in `doc.mk`).
- `notes.md` — SSH release notes, one `## Ssh <vers>` section per release (currently headed by the in-development/current version); referenced from module docs as `[release notes](notes.md)`.
- `guides/` — the six user's-guide chapters (see routing table).

## Constraints
- Every markdown file must begin with the Apache-2.0 SPDX `%CopyrightBegin% … %CopyrightEnd%` HTML comment; the header is enforced by `../../scripts/license-header.es`.
- To add a guide: create `guides/<name>.md` and register it in `docs.exs` `extras` (list order = display order); its group is picked up automatically from the `guides/` path.
- Use ExDoc autolink syntax in prose (`m:module`, `t:type/0`, `f:fun/1`, `e:app:page`, `c:callback/0`) — plain module names are not linked.
- `notes.md` is hand-authored by developers as part of the release/change process — it is NOT auto-generated; the `Prepare release` commit prepends the new `## Ssh <vers>` heading (and bumps `../vsn.mk` / `../../make/otp_version_tickets`). Do not regenerate it from commit messages.
- Do not add images/binaries here beyond what the assets mechanism supports; guide bodies are plain Markdown (mermaid code blocks are rendered client-side by the ex_doc config).

## Notes for Agents
- Module-level documentation lives in `../src/*.erl` (`-moduledoc`/`-doc`), not in this directory — edit source, not doc pages, when changing API descriptions.
- `notes.md` entries follow the format conventions in `../../HOWTO/RELEASE-NOTES.md` (Symptom/Trigger/Version for bugs, `Own Id: OTP-NNNNN Aux Id: PR-…, GH-…` trailer linking).
- The build regenerates `html/` and `man*/` from these sources; those outputs are build artifacts, not edited by hand.

## Routing Table
| Path | Description |
| --- | --- |
| `guides/introduction.md` | SSH protocol introduction — transport/authentication/connection layers, scope and purpose |
| `guides/using_ssh.md` | Usage examples/tutorial — Erlang terminal client, running a daemon, one-time exec, SFTP client/server, subsystems |
| `guides/terminology.md` | Glossary — "user" in OpenSSH vs Erlang/OTP, auth vs authorization, connection/channel/session |
| `guides/configurations.md` | Configuration reference — where options are set, precedence, `ssh:connect`/`ssh:daemon` options, cross-level algorithm config |
| `guides/configure_algos.md` | Algorithm configuration — negotiation, defaults, `preferred_algorithms`, `modify_algorithms` |
| `guides/hardening.md` | Security hardening recommendations — DoS resilience, attack-surface reduction, SFTP/key/network guidance |
| `ssh_app.md` | Application overview page (also the ssh(6) man page) |
| `notes.md` | SSH release notes, one section per released version |
| `docs.exs` | ExDoc local configuration (extras order, RFC annotations) |
| `Makefile` | Doc build driver (includes `../../make/doc.mk`) |
| `../internal_doc/` | Internal, non-published docs (`ssh_notes.md`: supervision-tree diagrams) — sibling, read-only |
| `../examples/` | Example SSH client/server Erlang code (`ssh_sample_cli.erl`, `ssh_device.erl`) — sibling, read-only |
| `../src/` | Erlang sources; module/function reference docs generated from `-moduledoc`/`-doc` |
| `../../system/` | System-level docs build (ExDoc driver, assets, shared guides) |
| `../../HOWTO/RELEASE-NOTES.md` | Conventions for writing release-note entries |

## See Also
- `../../make/doc.mk` and `../../make/ex_doc.exs` — the shared doc targets and default ExDoc config this directory plugs into.
- `../../system/` — the top-level documentation build and installed-doc layout.
- `../CONTEXT.md` — the `ssh` application itself (src, test, internal_doc, examples).

## Status
Complete.
