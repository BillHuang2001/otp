# erl_interface — Guides

## Intent
Hand-written tutorial markdown for the `ei` C library, aimed at C programmers writing C nodes/ports that talk to Erlang.
Holds exactly one file, `ei_users_guide.md` ("Erl_Interface User's Guide", ~440 lines).
This file is the ExDoc `main` page of the `erl_interface` docs, so it doubles as the app's documentation front page.

## API Surface
`ei_users_guide.md` is a linear tutorial; sections and the `ei` function families they teach:
- `## Introduction` (incl. `### Scope`, `### Prerequisites`) — library capabilities, release-compatibility note pointing at `ei.md#ei_set_compat_rel`.
- `## Compiling and Linking Your Code` — includes `ei.h`, `-I$OTPROOT/lib/erl_interface-$EIVSN/include`, links `libei.a`/`-lei`, thread flags (`-D_REENTRANT`, `STHREADS`/`PTHREADS`).
- `## Initializing the Library` — `ei_init()`, exactly once (see `../../include/ei.h`).
- `## Encoding, Decoding, and Sending Erlang Terms` — `ei_x_buff`, `ei_x_new`, `ei_x_encode_*`.
- `## Building Terms` — `ei_x_format_wo_ver` format directives, `ei_print_term`, `ei_x_free`; carries hidden anchor `building_terms_and_patterns`.
- `## Connecting to a Distributed Erlang Node` — `ei_cnode`, `ei_connect_init*`, `ei_connect` (header `../../include/ei_connect.h`).
- `## Using EPMD` — `ei_publish`, node registration/`epmd` lifecycle.
- `## Sending and Receiving Erlang Messages` (subsections `### Example of Sending Messages`, `### Example of Receiving Messages`) — `ei_send`/`ei_reg_send`, `ei_receive`/`ei_receive_msg`/`ei_xreceive_msg`, `ERL_TICK`, `erl_ticks`.
- `## Remote Procedure Calls` — `ei_rpc`, `ei_rpc_to`, `ei_rpc_from`.
- `## Using Global Names` — `ei_global_names`, `ei_global_whereis`, `ei_global_register`, `ei_global_unregister` (header `../../include/ei_global.h`).
Implementations of everything taught here live in `../../src/` (`encode/`, `decode/`, `connect/`, `global/`, `epmd/`).

## Constraints
- File name `ei_users_guide.md` is a public link target: other apps reference it as `` `e:erl_interface:ei_users_guide.md` `` (e.g. `../../../../system/doc/reference_manual/distributed.md`, `../../../../system/doc/tutorial/cnode.md`) — do not rename or move it.
- `../docs.exs` lists this file first in `extras` and sets `main: "ei_users_guide"`; the shared driver `../../../../make/ex_doc.exs` also auto-globs `guides/*.md` and groups them under "User's Guides" via the regex `~r/guides/`.
- Cross-links to sibling reference pages use explicit anchors that must exist as headings in `../references/` (e.g. `ei.md#ei_set_compat_rel`, `ei.md#ei_x_format_wo_ver`, `ei.md#ei_x_free`, `ei_connect.md#ei_connect_init`, `ei_connect.md#ei_send`, `ei_connect.md#ei_reg_send`, `ei_connect.md#ei_receive`, `ei_connect.md#ei_receive_msg`, `ei_connect.md#ei_accept`, `ei_global.md#ei_global_names`).
- Autoref links out of the app (`` `m:mnesia` ``, `` `m:global` ``, `` `e:erts:epmd_cmd.md` ``) only resolve in the release-wide doc build, not in an isolated app build.
- All C samples use ```c fenced code blocks; keep the Apache-2.0 `%CopyrightBegin%` header intact.
- No `.erl`/`docs.exs` sources belong in this directory.

## Design Decisions
- The guide stays narrative and example-driven: function-by-function detail lives in `../references/` and is only linked to, not duplicated.
- Samples are deliberately minimal and occasionally incomplete (e.g. the receiving example uses `x`/`buf` loosely, the linking example line-wraps oddly) to keep the prose short — they are illustrative, not compilable programs.

## Known Issues
- `### Scope` still advertises a "Pattern matching" topic that has no corresponding section; the hidden `building_terms_and_patterns` anchor is the only remnant of the merged former "Building terms and patterns" heading and is currently unreferenced inside the repo.

## Notes for Agents
- Add new tutorial prose/examples here; add per-function signatures, argument and return-value detail to `../references/ei.md`, `ei_connect.md`, `ei_global.md`, `erl_call_cmd.md`.
- Overlaps conceptually with `../../../../system/doc/tutorial/erl_interface.md` (the release-wide "port + Erl_Interface" walkthrough); that document is a task-level tutorial with example programs, this one is the app's `ei` API guide — do not merge the two.
- The executable counterpart of the C samples is `../../src/prog/erl_call.c` (a real `ei` client) and the `../../../../system/doc/tutorial/` C sources (`../../src/prog` is not affected by edits here).

## See Also
- `../references/` — per-function reference pages (`ei.md`, `ei_connect.md`, `ei_global.md`, `erl_call_cmd.md`).
- `../docs.exs` — ExDoc config deciding `main`/`extras` order.
- `../../include/ei.h`, `../../include/ei_connect.h`, `../../include/ei_global.h`, `../../include/eicode.h` — public C headers the guide explains.
- `../../../../make/ex_doc.exs` — shared ExDoc driver (extras globbing, sidebar groups).
- `../../../../system/doc/tutorial/erl_interface.md`, `../../../../system/doc/tutorial/cnode.md` — release-wide tutorial documents that link back into this guide.
