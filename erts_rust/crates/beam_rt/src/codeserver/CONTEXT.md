# `beam_rt/src/codeserver/` — release-layout code-path derivation (A.9)

## Intent
One module, `path.rs` (675 lines): the **release-layout** half of `code:get_path/0` — the `<lib>`
bundle scan the oracle's `code_server.erl` performs at start-up (`init/2` `:96-125`,
`make_path/2` `:380-392`, `choose_bundles` `:388-392`, `create_bundle`/`vsn_to_num`/`split`/
`split_base` `:394-432`, `get_user_lib_dirs/0,1` `:127-159`, `add_loader_path/2` `:513-546`).
It is what turns "a handful of ebin dirs" into the oracle's **36** entries, so `compiler`,
`eunit`, `syntax_tools`, … become resolvable.

Everything name-dependent is limited to `PRIM_APPS = ["kernel", "stdlib"]` (the boot script's
`primLoad` pair, kernel first) and `CURRENT_DIR = "."`; every other entry comes from the
filesystem. `-pa`/`-pz` and the configured path are **not** handled here — `CodeServer::
set_configured_paths`/`add_system_paths` own them and append the tail after this module's value.

## API Surface (`pub(crate)`, `path.rs`)
- `release_ebins(lib_dir: &Path) -> Vec<PathBuf>` (`:99`) — one `ebin` per application under
  `<lib>`, **highest version wins**, ordered **descending** by application name (`make_path/3`
  cons-accumulates, hence the `reverse()`). A nonexistent/unreadable dir contributes nothing.
- `user_lib_ebins(erl_libs: Option<&str>) -> Vec<PathBuf>` (`:276`) — the same scan per
  `$ERL_LIBS` element, keeping only basenames equal to `ebin`, in `$ERL_LIBS` order.
- `system_path(lib_root: Option<&Path>, base: &[PathBuf]) -> Vec<PathBuf>` (`:322`) — the
  oracle's order: prim pair (from `base`) → `$ERL_LIBS` → `release_ebins(lib_root)` (prim pair
  not repeated) → **source-tree fallback** (a `base` dir in neither the prim pair nor the
  release layout, so `erts` preloaded modules stay resolvable) → `.`. `lib_root == None` (a
  resolution that proved only bare `ebin` dirs) yields exactly `base` + `.`.
- Internal helpers: `choose_bundles` (`:137`), `Bundle` (derived `Ord` **is** the oracle's
  `lists:sort/1` tuple order: name, version component list, full name), `vsn_to_num` (`:209`),
  `resolve_ebin` (`:246`), `app_name`, `push_unique`, and the caching wrapper
  `cached_system_path` (`:379`).
- `cached_system_path` uses a `OnceLock` that **remembers the input `(lib_root, base)` pair**;
  a call with a different pair is derived on the spot and cached nowhere.

## Constraints
- **Pure filesystem function, never a fabricated path.** A missing/unreadable dir, a bundle with
  no `ebin`, or a version that does not parse contributes nothing; no error is raised. `base` is
  only ever the set a resolution *proved*, never a guess.
- **No hard-coded application names or counts** beyond `PRIM_APPS`.
- Deliberate divergences, each documented at its function: **no `.ez`/archive support** (an
  `.ez`-only tree contributes no entry where the oracle expands it) and **dedup by exact path,
  not by application name** (the only visible difference is an `$ERL_LIBS` second version of an
  installed application that this runtime adds and the oracle drops).
- This directory **holds no state and keeps no `Module`**: module cache, retention and code-path
  mutation live in the sibling file `../codeserver.rs` (`modules: HashMap<String, Arc<Module>>`).
- Files ~1000 lines / Apache-2.0 header / `#![warn(missing_docs)]` per the crate constraints.

## Notes for Agents
- **This node is irrelevant to literal-area lifetime**: it only derives directory strings. A
  module's `LiteralArea` lives as long as its `Arc<Module>`, which `../codeserver.rs` retains
  (`modules`, `forget_module`) — see the literal section of `../CONTEXT.md`.
- Unit tests are colocated at the tail of `path.rs` (`vsn_to_num_accepts_only_ascii_digit_
  components`, `release_ebins_is_descending_by_application_name`,
  `user_lib_ebins_scans_every_erl_libs_element_and_keeps_only_ebin`,
  `system_path_is_prim_then_erl_libs_then_release_then_dot`). The end-to-end oracle cross-check
  of the whole path is `../tests/code_path.rs`.
- `system_path` reads the **ambient** `$ERL_LIBS`, so a test that manipulates it must be
  serialized by the caller (the crate's env discipline).

## See Also
- `../codeserver.rs` → the `CodeServer` façade (atom table, module cache, load routes, code path,
  `forget_module`); `../CONTEXT.md` → the root/system resolution seam (which roots are probed and
  in what order) and the literal-area facts.
