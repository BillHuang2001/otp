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

* `src/codeserver.rs` → `CodeServer`: the **one** shared `Rc<AtomTable>` (copy-on-write on intern), module cache, the **one authoritative** code-path store, `module_handle`, `bindir()` and the *installation* resolution (`system_ebins`/`resolved_lib_root`, `$ERTS_STDLIB_DIR`, `erl_system_ebins`; `SYSTEM_APPS` is only that probe); its child `src/codeserver/path.rs` is the **release-layout derivation** (`choose_bundles`/`create_bundle`/`split_base`/`vsn_to_num`, `$ERL_LIBS`, the prim pair, `cached_system_path`) pinned element-wise and in order against the oracle by `tests/code_path.rs` — [`src/boot/CONTEXT.md`](src/boot/CONTEXT.md).
  The module cache is `modules: HashMap<String, Arc<Module>>` (`codeserver.rs:124`) and it **retains every loaded module for the runtime's whole life** — nothing unloads code, and `forget_module` (`:820`, `erlang:delete_module/1`) is the only removal — so `module(name)`/`module_handle(name)` (`:610`/`:622`) are the LCA for any *cross-module* metadata, and an `Arc<Module>` is also the handle `ExecutionContext::resolve_external_call` hands the engine (so a cross-module frame keeps its module alive).
  Two consumers read a **foreign** module's `Recs` chunk off that cache: `src/context/bridges.rs`'s `RecordBridge` (`records:get_definition/2`, `:722-761`, name-resolving through the module's own atom table) and `ExecutionContext::foreign_record_definition` (`src/context/records.rs`), the native-record instruction path for a foreign `put_record/6` create id.


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
| · `beam_loader::decode::decode_stream` | 9.00 % | 178 M | `beam_loader` |
| · drop of the decoded `Vec<RawInstr>` | 4.04 % | 80 M | `beam_loader`'s data shape |
| · `<AtomTable>::intern` (the `wire` pre-pass + merge-back) | 1.31 % | 26 M | `codeserver.rs` |
| `CodeServer::add_system_paths` | **2.63 %** | **52 M** | `codeserver.rs` |
| `CodeServer::find_file` | 0.10 % | 2 M | `codeserver.rs` |
| **everything in this module (`codeserver::path`)** | **0.00 %** | **0** | — |

- **This module is not on the hot path.** `cached_system_path` derives the layout **once per
  process**; its only recurring cost is the 36-`PathBuf` clone on a cache hit (~7 k
  instructions, ≈0.014 % of the floor). No sample lands inside `codeserver::path::*`.
- **The recurring cost WAS the caller's re-insert loop — now one-shot** (`../codeserver.rs`'s
  `installed_tail` marker, see *The system half is installed once* below). Before it,
  `CodeServer::add_system_paths` cost **1.32 M instructions per call** — measured from a
  `code:get_path/0` loop (300 calls cost 396 M, and 78 % of the BIF's own 1.69 M/call) — and a
  boot made ~**39** such calls, **52 M ≈ 2.63 % of the floor**, re-inserting the *same* 36
  directories each time. Nearly all of it was two `Vec<PathBuf>::contains` scans per directory
  (`Path::eq` is component-wise, `Components::eq`/`parse_next_component_back`, ~1000
  instructions per compare), i.e. ~1 300 component walks per call.
- **After the marker** (same recipe, `taskset -c 2`, 5 alternating control/new rounds): the floor
  **1.9807 G → 1.9331 G instructions (−47.6 M, −2.40 %)**; the perf census puts
  `<CodeServer>::add_system_paths` at **52 M → 4 M** (2.60 % → 0.20 % — what is left is the one
  real install and its callees). The per-`code:get_path/0` slope falls **1.685 M → 0.529 M**
  (N=0 vs N=300 loop probe): the install is gone from the query, and the residual is the BIF's
  own list building and copy-out. `cached_system_path`'s 36-`PathBuf` clone is now paid once.
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
  per case. Duplicate-install suppression is **per-`CodeServer` state** (`codeserver.rs`'s
  `installed_tail`), never a module-level cache.
- Deliberate divergences, each documented at its function: **no `.ez`/archive support** (an
  `.ez`-only tree contributes no entry where the oracle expands it) and **dedup by exact path,
  not by application name** (the only visible difference is an `$ERL_LIBS` second version of an
  installed application that this runtime adds and the oracle drops).
- This directory **holds no state and keeps no `Module`**: module cache, retention and code-path
  mutation live in the sibling file `../codeserver.rs` (`modules: HashMap<String, Arc<Module>>`).
- Files ~1000 lines / Apache-2.0 header / `#![warn(missing_docs)]` per the crate constraints.

## The system half is installed once (`../codeserver.rs`)
`CodeServer::add_system_paths` is a **one-shot install**, per server: the field
`installed_tail: Option<usize>` holds the `tail_paths` length the last successful install
covered, a hit at the top of the method returns `Ok(())` immediately, and a success sets it.
`code:get_path/0` calls it first (`src/native/code.rs:73`) and so does every other `code:*`
reader, so the guard is what makes those cheap.

- **A count, not a bool.** `tail_paths` grows only in `set_configured_paths`, so a `-pz`
  directory configured after the first install still gets appended (after `.`, the oracle's
  `add_pa_pz/3` order) — pinned by
  `a_pz_directory_configured_after_the_install_is_still_appended`.
- **A failed first attempt retries.** The `?` on `system_ebins()` leaves the field `None`.
- **The body's inputs are exactly three**, and each keeps the marker sound: `system_lib_root`
  (written only by the install itself, from the cached `system_ebins()`), `default_paths`
  (monotone, `add_default_path` only — `set_configured_paths`' `-pa` group and the install), and
  `tail_paths`. `reset_paths_to_defaults` restores from `default_paths`, which already holds the
  system half, so a post-`init:restart/0` skip is still correct.
- **Per-`CodeServer`, never process-global** (a global flag would starve every second server in
  the process; the crate builds one per test case).
- **The `HashSet<PathBuf>` duplicate-check idea is therefore closed as unnecessary.** It was the
  second half of the original two-part plan (replacing `add_default_path`'s
  `default_paths.contains` + `add_path`'s `paths.contains` with a set kept in lockstep); the
  marker alone removed **48 of the 52 M** (the rest is the single real install), so the two
  component-wise scans no longer run on any query path.
- **`code:del_path/1` on a system `ebin` now persists**, which is the oracle's behaviour: the
  entry is dropped from `paths` (`remove_path_where`, `src/native/code/path.rs:121`) and nothing
  puts it back; the oracle restores its default path only on `init:restart/0`, here
  `reset_paths_to_defaults`. Pinned by
  `a_system_ebin_deleted_after_the_install_stays_deleted_until_a_restart`, and pinned against the
  oracle end to end by `../tests/code_add_path.rs` (a fixture directory, unaffected by the
  marker). No differential case exercises `del_path` (verified: none of the 3 439 case ids names
  it) — adding one is the sibling `../../../tools/differential/` node's write, not this one's.

## Known Issues
- **`load_bytes`'s two interning loops call `self.atoms_mut()` once per atom** (`codeserver.rs`
  `:847-849`, `:864-868`), i.e. two `Rc::get_mut` calls per atom where one hoisted borrow would
  do. It is a micro: `Rc`'s strong count is a non-atomic `Cell`, and the whole
  `AtomTable::intern` share is ~26 M (1.31 %, and below the ±20 M sampling noise of the census),
  dominated by the name→index hash lookup itself.
  Both loops are also **load-bearing and not removable**: the `wire` pre-pass must intern every
  module atom into the *shared* table because `build_module`'s fork has to start with an
  identical prefix for the wire indices to mean anything, and the merge-back must intern the
  literal-only atoms at exactly `base..` so the shared indices stay identical to the module's.
  The merge-back's lookups are the only avoidable half — the names are appended in index order,
  so an append-only push would do — but `AtomTable::intern` is that table's **only** mutating
  API (`beam_term/src/atom.rs`: a name-keyed `index.get` fast path, one `Arc<str>` per new
  name), so removing them needs a `beam_term` API, i.e. a **sibling node's** surface. Not done
  here.
- **The whole-table copy the load hands `lower::build_module` is `AtomTable::fork`**
  (`codeserver.rs:921`): it shares the `Arc<str>` name handles and preserves every index and the
  generation, so it costs two allocations (one `Vec` of handles, one index bucket array) and
  nothing per atom — it has no attributable share of its own in the census above. This is only a
  *copy* change: every index of the module's table names exactly what a deep clone named, because
  a fork starts at the same length and appends in index order, which is the protocol the
  merge-back loop rides (`beam_term`'s `a_fork_serves_the_lower_then_merge_back_protocol`). The
  fork's **fresh instance** is deliberate and unobserved here: `appended_since` is `false` in both
  directions between a fork and its parent, and no memo in this crate witnesses a module's own
  table (the two decode memos — `../nif.rs`'s `with_decode_scratch` and
  `beam_bifs`'s — witness the **shared** table only).

## Notes for Agents
- **Do not look for wins inside this module.** The measured share of `codeserver::path::*` is
  0 samples out of 989; a change here cannot move the startup floor. What used to be the per-call
  money — `add_system_paths`'s re-insert loop — is now a one-shot install (above), and the
  load's whole-table copy is `AtomTable::fork` (the `load_bytes` bullet above), so nothing here
  pays per atom any more.
- Measurement recipe that produced the table above (keep it identical when comparing):
  `perf record -e instructions:u -c 2000000 --call-graph dwarf,16384 -o X.data -- taskset -c 2
  erts_rust/target/release/beam_rt -noshell -eval 'halt().'`, then exact inclusive shares by
  splitting `perf script -i X.data --no-inline` on blank lines and counting the samples whose
  stack names a symbol (each sample = 2 M instructions).
  `perf probe` is unavailable in this sandbox (`/sys/kernel/tracing` is not readable), so call
  counts come from **loop probes** instead: `-eval 'lists:foreach(fun(_) ->
  code:get_path() end, lists:seq(1,N)), halt().'` is exactly linear — **1.685 M/call before the
  marker, 0.529 M/call after** (N=0 = the floor) — and the slope prices a per-call path
  operation.
- **This node is irrelevant to literal-area lifetime**: it only derives directory strings. A
  module's `LiteralArea` lives as long as its `Arc<Module>`, which `../codeserver.rs` retains
  (`modules`, `forget_module`) — see the literal section of `../CONTEXT.md`.
- Unit tests are colocated at the tail of `path.rs` (`vsn_to_num_accepts_only_ascii_digit_
  components`, `release_ebins_is_descending_by_application_name`,
  `user_lib_ebins_scans_every_erl_libs_element_and_keeps_only_ebin`,
  `system_path_is_prim_then_erl_libs_then_release_then_dot`), and the `CodeServer`-level ones
  (the path install/marker) at the tail of `../codeserver.rs`
  (`a_system_ebin_deleted_after_the_install_stays_deleted_until_a_restart`,
  `a_pz_directory_configured_after_the_install_is_still_appended`), plus
  `a_loaded_module_shares_the_shared_tables_name_allocations`, which pins that the load's copy is
  a fork (every atom the shared table already held is the *same* name allocation in the module's
  table, at the same index). The end-to-end oracle
  cross-check of the whole path is `../tests/code_path.rs` (plus `../tests/boot_code_path.rs`
  and `../tests/code_add_path.rs` for the mutators); the boot gates are
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --test boot`
  and `beam_rt -noshell -boot bootstrap/bin/start` (exit 0, empty stdout/stderr).
- **The full crate gate is `-p beam_rt --lib --no-fail-fast`**: 1981 passed / 6 failed / 7 ignored.
  Four are the inventorial `src/runner/` *measured-table* pins
  (`a_process_never_hands_out_off_heap_slots_across_slices`, `boundary_cost`, `reds_door`,
  `slice_charging`) — number-drift pins owned by that node, not this one. The other two are
  `context::calls`'s collection-cadence pins
  (`a_collecting_default_process_copies_every_result_without_refusing`,
  `without_the_learned_hint_the_same_cadence_refuses_again`), which reproduce identically on an
  unmodified tree at this commit and are *not* in `../TEST_STRATEGY.md`'s known-red inventory
  (same class — they assert `collections * 4 > OPS` for a heap-policy-driven cadence). The
  `codeserver::` slice is green on its own (`--lib -- codeserver`, 35 passed, the marker's two
  cases included).
- `system_path` reads the **ambient** `$ERL_LIBS`, so a test that manipulates it must be
  serialized by the caller (the crate's env discipline).
- **The cheapest end-to-end parity check of this module's output** (seconds, no differential
  harness): compile `-module(gp). -export([go/0]). go() -> code:get_path().` with `erlc`, then
  compare `erl -noshell -eval 'io:format("~w~n",[code:get_path()]),halt().'` with
  `beam_rt run gp.beam gp:go/0` — the two `~w` renderings are **byte-identical** (36 entries,
  11 140 bytes on this host). Any code-path change must keep that `cmp` clean; a path whose
  *content* is right but whose order moved fails it.

## See Also
- `../codeserver.rs` → the `CodeServer` façade (atom table, module cache, load routes, code path,
  `forget_module`); `../CONTEXT.md` → the root/system resolution seam (which roots are probed and
  in what order) and the literal-area facts.
