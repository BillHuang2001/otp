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
  a call with a different pair is derived on the spot and cached nowhere. It returns an
  **owned `Vec<PathBuf>` clone** of the cached list, so every call allocates 36 `PathBuf`s.

## Cost shape (measured — do not re-derive)
The whole-run floor for `beam_rt -noshell -eval 'halt().'` on this tree is **1.9807 G
`cpu_core/instructions:u`** (`taskset -c 2`, spread ±0.05 % over 5 runs).
Attribution by exact sample counting over
`perf record -e instructions:u -c 2000000 --call-graph dwarf,16384` (each sample = 2 M
instructions):

| item | share | instructions | owner |
|---|---|---|---|
| `CodeServer::load_bytes` | 47.02 % | 930 M | — |
| · `beam_loader::lower::build_module` | 17.49 % | 346 M | `beam_loader` |
| · `<AtomTable as Clone>::clone` (the explicit `.clone()` in `load_bytes`) | 15.07 % | 298 M | `beam_term` fork seam |
| · `beam_loader::decode::decode_stream` | 9.00 % | 178 M | `beam_loader` |
| · drop of the decoded `Vec<RawInstr>` | 4.04 % | 80 M | `beam_loader`'s data shape |
| · `<AtomTable>::intern` (the `wire` pre-pass + merge-back) | 1.31 % | 26 M | `codeserver.rs` |
| `CodeServer::add_system_paths` | **2.63 %** | **52 M** | `codeserver.rs` |
| `CodeServer::find_file` | 0.10 % | 2 M | `codeserver.rs` |
| **everything in this module (`codeserver::path`)** | **0.00 %** | **0** | — |

- **This module is not on the hot path.** `cached_system_path` derives the layout **once per
  process**; its only recurring cost is the 36-`PathBuf` clone on a cache hit (~7 k
  instructions, ≈0.014 % of the floor). No sample lands inside `codeserver::path::*`.
- **The recurring cost is the caller's re-insert loop.** `CodeServer::add_system_paths` costs
  **1.32 M instructions per call** — measured from a `code:get_path/0` loop (300 calls cost
  396 M, and 78 % of the BIF's own 1.69 M/call) — and a boot makes ~**39** such calls (52 M /
  1.32 M) for **52 M ≈ 2.63 % of the floor**, re-inserting the *same* 36 directories each time.
  Nearly all of it is two `Vec<PathBuf>::contains` scans per directory (`Path::eq` is
  component-wise, `Components::eq`/`parse_next_component_back`, ~1000 instructions per compare),
  i.e. ~1 300 component walks per call.
- `find_file` builds one `PathBuf` per candidate and `stat`s it, but only 0.10 % of the run — a
  minimal run opens 99 `.beam` files and each is found in the first few entries.

## Constraints
- **Pure filesystem function, never a fabricated path.** A missing/unreadable dir, a bundle with
  no `ebin`, or a version that does not parse contributes nothing; no error is raised. `base` is
  only ever the set a resolution *proved*, never a guess.
- **No hard-coded application names or counts** beyond `PRIM_APPS`.
- **A one-shot "already installed" short-circuit cannot live here.** A process-global
  "return the list only once" flag in `cached_system_path` would starve every *second*
  `CodeServer` in the same process of its system path — and the crate's tests build one server
  per case. Duplicate-install suppression is **per-`CodeServer` state** (`codeserver.rs`), never
  a module-level cache.
- Deliberate divergences, each documented at its function: **no `.ez`/archive support** (an
  `.ez`-only tree contributes no entry where the oracle expands it) and **dedup by exact path,
  not by application name** (the only visible difference is an `$ERL_LIBS` second version of an
  installed application that this runtime adds and the oracle drops).
- This directory **holds no state and keeps no `Module`**: module cache, retention and code-path
  mutation live in the sibling file `../codeserver.rs` (`modules: HashMap<String, Arc<Module>>`).
- Files ~1000 lines / Apache-2.0 header / `#![warn(missing_docs)]` per the crate constraints.

## Known Issues
- **The system half of the code path is rebuilt on every `CodeServer::add_system_paths` call**
  (~39 per boot, 1.32 M instructions each, 2.63 % of the startup floor) although its two inputs
  are process-constant. The duplicate work is `add_default_path`'s
  `default_paths.contains(&dir)` + `add_path`'s `paths.contains(&dir)` pair — O(n) component
  comparisons over the 36-entry list — plus the 36-`PathBuf` clone this module returns.
  The fix belongs in `../codeserver.rs` and is two independent changes: (a) a per-server
  one-shot guard recording how many `tail_paths` the last successful install appended
  (`Option<usize>`, so a `set_configured_paths` arriving later still appends its extra `-pz`
  directories, and a failed first attempt still retries), and (b) replacing both `Vec::contains`
  duplicate checks with a `HashSet<PathBuf>` kept in lockstep with `paths`/`default_paths`
  (`Path`'s `Hash` normalises the same redundant separators `Eq` ignores, so membership answers
  identically). (a) alone removes ~99 % of the 52 M.
- **`code:del_path/1` on a system `ebin` is undone before it can be observed.** The deletion is
  `remove_path_where` (`src/native/code/path.rs:121`), which edits `paths` only; the next
  `add_system_paths` call re-installs the entry from the (still intact) `default_paths` — and
  `code:get_path/0` calls `add_system_paths` **first** (`src/native/code.rs:73`), so
  `code:del_path(Sys), code:get_path()` lists the directory again where the oracle drops it.
  The one-shot guard above moves this to the oracle's answer (`default_paths` is restored only by
  `reset_paths_to_defaults`, i.e. by `init:restart/0`), so it is a **behaviour change to declare**
  — and a differential case worth adding beside the guard.
- **`load_bytes`'s two interning loops call `self.atoms_mut()` once per atom** (`codeserver.rs`
  `:847-849`, `:864-868`), i.e. two `Rc::get_mut` calls per atom where one hoisted borrow would
  do. It is a micro: the whole `AtomTable::intern` share is 26 M (1.31 %), dominated by the
  name→index hash lookup itself.
  Both loops are also **load-bearing and not removable**: the `wire` pre-pass must intern every
  module atom into the *shared* table because `build_module`'s clone has to start with an
  identical prefix for the wire indices to mean anything, and the merge-back must intern the
  literal-only atoms at exactly `base..` so the shared indices stay identical to the module's.
  The merge-back's lookups are the only avoidable half — the names are appended in index order,
  so an append-only `AtomTable` push would do — but that surface is `beam_term`'s.
- **The single largest attributable item in the load path is not this file's to fix**:
  `<AtomTable as Clone>::clone` at `codeserver.rs:862` (`lower::build_module(&chunks, &raw,
  (*self.atoms).clone())`) is 298 M / 15.07 % of the floor, 7.38 % of it `Arc<str>::from(&str)`
  re-copying every atom name. The seam is `beam_term`'s `fork()` handle (a sibling node).

## Notes for Agents
- **Do not look for wins inside this module.** The measured share of `codeserver::path::*` is
  0 samples out of 989; a change here cannot move the startup floor. The per-call money is in
  `../codeserver.rs` (`add_system_paths`, and the `.clone()` handed to
  `lower::build_module` — a `beam_term` fork seam owned by another node).
- Measurement recipe that produced the table above (keep it identical when comparing):
  `perf record -e instructions:u -c 2000000 --call-graph dwarf,16384 -o X.data -- taskset -c 2
  erts_rust/target/release/beam_rt -noshell -eval 'halt().'`, then exact inclusive shares by
  splitting `perf script -i X.data --no-inline` on blank lines and counting the samples whose
  stack names a symbol (each sample = 2 M instructions).
  `perf probe` is unavailable in this sandbox (`/sys/kernel/tracing` is not readable), so call
  counts come from **loop probes** instead: `-eval 'lists:foreach(fun(_) ->
  code:get_path() end, lists:seq(1,N)), halt().'` is exactly linear (1.690 M/call at both
  N=100 and N=300; N=0 = the floor), and the slope prices a per-call path operation.
- **This node is irrelevant to literal-area lifetime**: it only derives directory strings. A
  module's `LiteralArea` lives as long as its `Arc<Module>`, which `../codeserver.rs` retains
  (`modules`, `forget_module`) — see the literal section of `../CONTEXT.md`.
- Unit tests are colocated at the tail of `path.rs` (`vsn_to_num_accepts_only_ascii_digit_
  components`, `release_ebins_is_descending_by_application_name`,
  `user_lib_ebins_scans_every_erl_libs_element_and_keeps_only_ebin`,
  `system_path_is_prim_then_erl_libs_then_release_then_dot`). The end-to-end oracle cross-check
  of the whole path is `../tests/code_path.rs`; the boot gates are
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --test boot`
  and `beam_rt -noshell -boot bootstrap/bin/start` (exit 0, empty stdout/stderr).
- **The full crate gate is `-p beam_rt --lib --no-fail-fast`**: 1978 passed / 4 failed / 7 ignored,
  and the four are the pre-existing `src/runner/` *measured-table* pins
  (`a_process_never_hands_out_off_heap_slots_across_slices`, `boundary_cost`, `reds_door`,
  `slice_charging`) — number-drift pins owned by that node, not this one. The `codeserver::` slice
  is green on its own (`--lib -- codeserver::`, 32 passed).
- `system_path` reads the **ambient** `$ERL_LIBS`, so a test that manipulates it must be
  serialized by the caller (the crate's env discipline).

## See Also
- `../codeserver.rs` → the `CodeServer` façade (atom table, module cache, load routes, code path,
  `forget_module`); `../CONTEXT.md` → the root/system resolution seam (which roots are probed and
  in what order) and the literal-area facts.
