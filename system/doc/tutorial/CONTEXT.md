# Interoperability Tutorial (`system/doc/tutorial`)

## Intent
Guide published in the "Interoperability Tutorial" sidebar group: how Erlang interoperates with other languages (mainly C) from the Erlang programmer's perspective.
All mechanisms (port, port driver, NIF, Erl_Interface, C node) are presented as competing solutions to one running example (`complex.c` with `foo/1`/`bar/1`), so the pages deliberately mirror each other's structure (Erlang program → C program → "Running the Example").
`debugging.md` is the odd one out: it is about debugging native code (debug emulator, ASan, Valgrind, rr), not about an interop mechanism.

## Page Inventory (API Surface)
All nine pages are registered in `system/doc/docs.exs` `extras:` as `"tutorial/<name>.md": []`; that list order is the sidebar order.
- `tutorial.md` — landing page "Introduction": purpose, prerequisites, note that examples omit error handling; defines anchor `#interoperability-tutorial`.
- `overview.md` — taxonomy: built-in mechanisms (distributed Erlang, ports + linked-in drivers, NIFs), C/Java libraries (Erl_Interface, C nodes, Jinterface), standard protocols/sockets, IC/CORBA; anchors `#distributed-erlang`, `#ports-and-linked-in-drivers`, `#native-implemented-functions-nifs`, `#c-nodes`, `#sockets` are linked to from other pages.
- `example.md` — "Problem Example": the `complex.c` functions and the promise that `complex.erl` will be implemented by each mechanism.
- `c_port.md` — **Ports**: Erlang `complex1` + C `erl_comm.c`/`port.c`; contains a ```mermaid diagram.
- `erl_interface.md` — **Erl_Interface**: Erlang `complex2` + C `ei.c` (+ `erl_comm.c`); requires the ports page as a prerequisite.
- `c_portdriver.md` — **Port Drivers** (linked-in driver): Erlang `complex5` + C `port_driver.c`; contains a ```mermaid diagram.
- `cnode.md` — 27-line stub; only redirects to `e:erl_interface:ei_users_guide.md`. No C-node example lives in this guide any more.
- `nif.md` — **NIFs**: Erlang `complex6` + C `complex6_nif.c`.
- `debugging.md` — **Debugging NIFs and Port Drivers**: debug emulator, AddressSanitizer, Valgrind, rr; anchors `#debug`, `#rr` are explicit `[](){: #debug }` markers, `#asan` explicit, `#valgrind` comes from the heading.

## Companion Sources (C / Erlang files in this directory)
Referenced by the prose (listed inline in the pages below):
- `complex.c` → `example.md`
- `erl_comm.c`, `port.c`, `complex1.erl` → `c_port.md`
- `ei.c`, `complex2.erl` → `erl_interface.md`
- `port_driver.c`, `complex5.erl` → `c_portdriver.md`
- `complex6_nif.c`, `complex6.erl` → `nif.md`

Orphaned (no page in this guide or elsewhere in the repo references them):
- `cnode_c.c`, `cnode_s.c`, `cnode_s2.c`, `complex3.erl`, `complex4.erl` — leftovers of the removed C-node example (`complex3.erl`/`complex4.erl` still hardcode stale hostnames `c1@idril` / `cnode@idril.du.uab.ericsson.se`). C-node documentation now lives in `lib/erl_interface/doc/guides/ei_users_guide.md`.
- `erl_comm.c` (1 line), `port.c` (2 lines) and `port_driver.c` (8 lines) still carry trailing whitespace that the corresponding markdown listings no longer have; `complex6_nif.c` indents two `return enif_make_badarg(env);` lines with a TAB where `nif.md` now uses spaces; `ei.c` differs from its listing only by a trailing blank line.

## Code-Listing Mechanism (most important fact)
ex_doc **never reads** the `.c`/`.erl` files. Every listing is a **verbatim copy** inside the markdown as a ```c / ```erlang fenced block, prefixed by a comment line naming the file (`/* erl_comm.c */`; the `.erl` blocks have no such marker).
There is no `include`/`reuse` mechanism, no `{: .file }` annotation, and no link from any page to the companion files — the file names appear only inside the code comments and in the `gcc ...` command lines.
Consequence: prose and source are two independent copies; editing one does **not** update the other, and nothing in the build or CI detects drift. Any content change must be applied to **both** the `.md` listing and the `.c`/`.erl` file by hand.

## Build & Publication
Built by ex_doc through `system/doc/Makefile` → `make/doc.mk` (`docs` → `html`), config `system/doc/docs.exs` + global `make/ex_doc.exs`.
- `system/doc/Makefile` sets `APP_DIR=../../lib/erl_interface`, so `make/doc.mk`'s `ERL_FILES0` (which globs `$(APP_DIR)/src/**/*.erl` for man-page generation) never picks up `tutorial/*.erl`; the guides are HTML-only (`HTML_DEPS=$(wildcard $(guide)/*.md)`).
- New/renamed `.md` must be added to `system/doc/docs.exs` `extras:` or it is not published; the path must contain `tutorial/` to land in this sidebar group (group title comes from `system/doc/guides` line `tutorial:Interoperability Tutorial`, matched by the `~r/tutorial/` regex built in `docs.exs`).
- `EX_DOC_WARNINGS_AS_ERRORS=default` (= true, see `make/ex_doc.exs`) means unresolved `` `m:mod` ``, `` `fun/2` ``, `` `t:type/0` `` autolinks and bad `` `e:app:page.md#anchor` `` references fail the docs build. Post-build, `scripts/otp_check_html_links.exs` validates the generated HTML links.

## Constraints
- These C/Erlang files are **not compiled, tested, linted, or packaged anywhere**: no Makefile in `system/doc/` or `make/` touches them, no test suite copies or compiles them, and repo-wide grep finds no other file referencing their names (the only external mentions are two erts internal docs that link to the *published URL* `http://erlang.org/doc/tutorial/nif.html`). They are reference copies shipped in the source tree only.
- Each `.md` must start with the Apache-2.0 `<!--` / `%CopyrightBegin%` … `%CopyrightEnd%` / `-->` HTML-comment header.
- The header-less `.c`/`.erl` files here are not a license violation: `scripts/license-header.es` lists `**/doc/**` in `NoWarnAllFiles`, so missing headers under this directory are never reported.
- Keep the per-page structure (Erlang program → C program → Running the Example) when adding a mechanism; the pages cross-reference each other freely (`example.md`, `c_port.md`, `erl_interface.md`).
- Mermaid diagrams are rendered client-side only in the HTML output (mermaid.js is injected by `make/ex_doc.exs` and rewrites ```mermaid blocks); they carry no information in other formats.

## Known Issues
- Five orphan sources (`cnode_c.c`, `cnode_s.c`, `cnode_s2.c`, `complex3.erl`, `complex4.erl`) are dead weight from the removed C-node example page and cannot be validated by reading the guide.
- Two `.erl` orphans pin obsolete host names (`c1@idril`, `cnode@idril.du.uab.ericsson.se`) that no longer resolve anywhere.
- Source/markdown whitespace drift already exists (see Companion Sources): the checked-in C files are the pre-clean-up versions.
- Stale decorative versions: `erl_interface.md` shows `erl_interface-3.9.2` in the `gcc` example while the following paragraph says the version is `3.2.1`, and it still explains R4B/R5B layout; `nif.md`'s shell transcript is from `R13B04 (erts-5.7.5)` while `c_port.md`/`c_portdriver.md`/`erl_interface.md` use OTP 26/erts-14.2. `nif.md`'s transcript also has a typo `comlpex6:foo`.
- `debugging.md` cross-links `` `e:system:install.md#advanced-configuration-...` ``, while the system app registers that page in `docs.exs` as `installation_guide/INSTALL.md`; double-check that target before relying on it.
- `cnode.md` is a redirect stub but the sidebar shows it as a full page; its only content links to `e:erl_interface:ei_users_guide.md`.

## Notes for Agents
- Do not expect `make` to verify anything in this directory; the only automatic check is the ex_doc link/autolink resolution (warnings as errors) and `scripts/otp_check_html_links.exs` on the built HTML.
- If code and prose disagree, the markdown is what users read and what is published; fix both, but treat the `.md` as the source of truth for wording/formatting and the `.c`/`.erl` files as the compilable copies.
- Anchors are written as `[](){: #anchor }` (explicit) or derived from headings; link to a page as `[text](page.md)` (relative) or `[text](`e:app:page.md#anchor`)` (cross-app).
- Admonitions use blockquote + attribute syntax: `> #### Note {: .info }`, `{: .warning }`.
- Verified page/source parity (diff of the fenced block against the file): `complex.c`, `complex1.erl`, `complex2.erl`, `complex5.erl`, `complex6.erl` match byte-for-byte; `erl_comm.c`, `port.c`, `port_driver.c`, `ei.c`, `complex6_nif.c` differ only in whitespace (trailing blanks / tabs / a trailing blank line).

## See Also
- `../reference_manual/ports.md` — language-level ports and `open_port/2`.
- `../efficiency_guide/drivers.md` — performance trade-offs of ports/drivers/NIFs.
- `lib/erl_interface/doc/guides/ei_users_guide.md` — C node and `ei` API reference (the page `cnode.md` defers to); `lib/erl_interface/doc/references/ei.md`, `ei_connect.md`, `ei_global.md`.
- `erts/doc/references/erl_nif.md`, `erl_driver.md`, `erl_cmd.md` — NIF/port-driver API and `-emu_type` referenced by `nif.md`/`debugging.md`.
- `../installation_guide/INSTALL.md` — building a debug/asan/valgrind emulator, referenced from `debugging.md`.
