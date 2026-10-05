# erl_interface Reference Pages

## Intent
Hand-written Markdown reference pages for the `ei` C library and for the `erl_call` command-line tool shipped with the `erl_interface` application.
Each page is a complete manual page: it is both the ExDoc source page and the input for a generated Unix man page.
There is no XML DTD and no generator for the page bodies — these `.md` files are the single source of truth for `ei` documentation.

## API Surface
Flat directory, four `.md` files, no subdirectories.
Every file starts with an `%CopyrightBegin%` HTML comment, then `# <name>` + a one-line summary, then `## Description`.

### `ei.md` (1245 lines) — "# ei / Routines for handling the Erlang binary term format"
Documents the public header `../../include/ei.h`.
- Section `## Data Types` with explicit anchors: `ei_term` (`#ei_term`), `ei_x_buff` (`#ei_x_buff`), `erlang_char_encoding`, `erlang_fun`, `erlang_pid`, `erlang_port`, `erlang_ref`, `erlang_trace`.
- Decode family (`buf` + `int *index` cursor): `ei_decode_atom`, `_atom_as`, `_bignum`, `_binary`, `_bitstring`, `_boolean`, `_char`, `_double`, `_ei_term`, `_fun` (+ `free_fun`), `_iodata`, `_list_header`, `_long`, `_longlong`, `_map_header`, `_pid`, `_port`, `_ref`, `_string`, `_trace`, `_tuple_header`, `_ulong`, `_ulonglong`, `_version`.
- Encode family, each function paired with an `ei_x_*` dynamic-buffer twin: `ei_encode_*` / `ei_x_encode_*` for `atom`/`atom_len`/`atom_as`/`atom_len_as`, `bignum`, `binary`, `bitstring`, `boolean`, `char`, `double`, `empty_list`, `fun`, `list_header`, `long`, `longlong`, `map_header`, `pid`, `port`, `ref`, `string`/`string_len`, `trace`, `tuple_header`, `ulong`, `ulonglong`, `version`.
- `ei_x_*` buffer lifecycle/formatting: `ei_x_new`, `ei_x_new_with_version`, `ei_x_append`, `ei_x_append_buf`, `ei_x_format`, `ei_x_format_wo_ver`, `ei_x_free` (documents the `~a ~c ~s ~i ~l ~u ~f ~d ~p` format specifiers).
- Miscellaneous: `ei_cmp_pids`, `ei_cmp_ports`, `ei_cmp_refs`, `ei_get_type`, `ei_init`, `ei_print_term`, `ei_s_print_term`, `ei_set_compat_rel`, `ei_skip_term`.
- Closes with `## Debug Information`.

### `ei_connect.md` (1188 lines) — "# ei_connect / Communicate with distributed Erlang"
Documents the public header `../../include/ei_connect.h`.
- Prologue sections `## Time-Out Functions` (the `_tmo` convention) and `## User Supplied Socket Implementation` (anchor `#ussi`, field docs at `#ei_socket_callbacks_fields`).
- Section `## Data Types`: `ei_cnode`, `ei_socket_callbacks`, `ErlConnect`, `Erl_IpAddr`, `erlang_msg` — all with `{: #... }` anchors.
- Connect/publish/lifecycle: `ei_connect`, `ei_xconnect`, `ei_connect_host_port`, `ei_xconnect_host_port`, plus `_tmo` variants; `ei_connect_init`, `ei_connect_xinit` and their `_ussi` variants; `ei_listen`/`ei_xlisten`, `ei_publish`/`ei_publish_tmo`, `ei_unpublish`/`ei_unpublish_tmo`, `ei_accept`/`ei_accept_tmo`, `ei_close_connection`.
- Messaging: `ei_send`/`ei_send_tmo`, `ei_send_encoded`/`ei_send_encoded_tmo`, `ei_send_reg_encoded`/`ei_send_reg_encoded_tmo`, `ei_receive`/`ei_receive_tmo`, `ei_receive_msg`/`ei_xreceive_msg` and their `_tmo` forms, `ei_receive_encoded`/`ei_receive_encoded_tmo`, `ei_reg_send`/`ei_reg_send_tmo`, `ei_rpc`, `ei_rpc_to`, `ei_xrpc_to`, `ei_rpc_from`.
- Identity and helpers: `ei_self`, `ei_thisnodename`, `ei_thishostname`, `ei_thisalivename`, `ei_make_pid`, `ei_make_ref`, `ei_gethostbyaddr`/`_r`, `ei_gethostbyname`/`_r`, `ei_get_tracelevel`, `ei_set_tracelevel`.
- Closes with `[](){: #debug_information }` + `## Debug Information` (documents `erl_errno` values, `EI_TRACELEVEL`, trace levels 1–5).

### `ei_global.md` (119 lines) — "# ei_global / Access globally registered names"
Documents the public header `../../include/ei_global.h`; implementation lives in `../../src/global/`.
Exactly four functions: `ei_global_names`, `ei_global_register`, `ei_global_unregister`, `ei_global_whereis`.
Warns that these RPCs occupy the caller-supplied socket during the operation.

### `erl_call_cmd.md` (252 lines) — "# erl_call / Call/start a distributed Erlang node"
Documents the `erl_call` tool built from `../../src/prog/erl_call.c`.
`## erl_call <options>` lists every flag as one bullet: `-a`, `-address`, `-c`, `-d`, `-e`, `-fetch_stdout`, `-h`, `-m`, `-n`, `-name`, `-no_result_term`, `-q`, `-r`, `-R`, `-s`, `-sname`, `-timeout`, `-v`, `-x`.
`## Examples` closes the page.

## Design Decisions
- Every documented function is a `## <name>()` heading immediately followed by a ```c prototype block, then prose; grouped functions share one heading per name with two adjacent blocks (e.g. `## ei_x_append()` / `## ei_x_append_buf()`).
- Heading names are public anchors: `erts/doc/guides/driver.md` links `e:erl_interface:ei.md#ei_x_encode_string`, and `erts/doc/references/erl_driver.md`, `erts/doc/references/epmd_cmd.md`, `erts/preloaded/src/erlang.erl` and `system/doc/embedded/embedded.md` link the page roots — renaming or reordering headings breaks those references.
- Data Types entries carry explicit `{: #name }` anchors on the item text because the type name is inside a bullet, not a heading.
- Newer functions end with an `Available since OTP <version>` line.

## Constraints
- Keep the documented functions, variables and symbols in sync with `../../include/ei.h`, `ei_connect.h`, `ei_global.h` and with the compile-only smoke test `../../src/prog/ei_fake_prog.c` (rule stated in `../../src/README.internal`, whose other file inventory is stale).
- Do not rename `## <function>()` headings or the `{: #... }` Data-Types anchors — they are link targets from other applications and from `../notes.md`.
- Prose style: a `## Description` overview, then one section per function with an explicit parameter list and an explicit return-code sentence.

## Notes for Agents
- Adding a newly exported `ei` function requires: the header (`../../include/ei.h` etc.), an `ei_fake_prog.c` call, and a new `## <name>()` section here in the same style (signature block first, then parameters/returns, then `Available since OTP <version>`).
- `../docs.exs` explicitly lists all four pages in `extras`, which fixes their order in the sidebar; `../../../../make/ex_doc.exs` also globs `{guides,references,internal_docs}/*.md` and de-duplicates, so a new `references/*.md` page is picked up by ExDoc even if not listed — but its ordering/placement is only deterministic once added to `../docs.exs`.
- Sidebar group is assigned by filename pattern in `../../../../make/ex_doc.exs` (`groups_for_extras`): `references/*_cmd.md` → "Command Line Tools", other `references/*` → "References" — so the `_cmd` suffix on `erl_call_cmd.md` is load-bearing.
- Man pages are generated from these same files by `../../../../make/doc.mk` via `../../../../make/markdown_to_man.escript`: `references/%_cmd.md` → `man1/%.1` (hence `erl_call_cmd.md` → `man1/erl_call.1`) and `references/%.md` → `man3/%.3` (`ei.md` → `man3/ei.3`, `ei_connect.md` → `ei_connect.3`, `ei_global.md` → `ei_global.3`).

## See Also
- `../guides/ei_users_guide.md` — the narrative "Erlang/Interface" users guide (ExDoc `main` page); links into `ei.md`/`ei_connect.md`/`ei_global.md` anchors.
- `../docs.exs` — ExDoc configuration (extras order, `api_reference: false`).
- `../../include/` — the C headers these pages document.
- `../../src/prog/ei_fake_prog.c` — compile-only smoke test that must reference every documented function; `../../src/prog/erl_call.c` — the `erl_call` tool.
- `../../src/README.internal` — the "keep documentation, headers and fake prog in sync" rule.
- `../../../../make/doc.mk`, `../../../../make/markdown_to_man.escript`, `../../../../make/ex_doc.exs` — the generation machinery.
