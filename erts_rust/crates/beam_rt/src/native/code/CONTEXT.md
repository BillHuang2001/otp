# `beam_rt/src/native/code/` — the `code:*` floor (readers + the one authoritative code path)

## Intent
The `code:*` runtime-native floor: `code.rs` (the path/load readers, the mode and the installation
directories) and `path.rs` (the path **mutators**), answering from this runtime's own
[`CodeServer`](crate::CodeServer) and the preloaded-module registry — the state this runtime really
searches.
`code.erl`'s bodies are *clients* of the live `code_server` process, which keeps its **own** path copy,
so entering them would answer from a second store; the floor owns these keys instead.
The dispatch is `../native.rs`'s (the one dispatch table — this node adds no second list); the
per-entry ground truth is each module's rustdoc.

## API Surface
### `code.rs` — the readers
`get_path/0` ⇒ the code path, one directory string per entry, in search order (resolved against the
release layout by `../codeserver/path.rs`, reproducing the oracle's `code:get_path/0` element for
element — `../../tests/code_path.rs`).
`where_is_file/1` is the code server's **non-module** path search (`code_server.erl:1469-1486`);
`which/1` and `get_object_code/1` are **path**-based (load state first — the recorded file, or
`preloaded` — then a code-path search, so an unloaded-but-findable module such as `dets` still has
one); `get_object_code/1` ⇒ `{Module,Binary,Filename}`, else the atom `error`; `is_loaded/1` is
**load-state**-based (`false` unless preloaded or really loaded here, else `{file,Beam}`), and
`ensure_loaded/1` closes the difference.
`load_file/1` really loads (`{module,M}`) with the oracle's `{error,sticky_directory|nofile|badfile}`
vocabulary, and a `<Name>.beam` whose **object code names another module** is that `badfile` on
**every** file route — the loader's expected-name check lives in the one `CodeServer::load`, so
`load_file/1` ⇒ `{error,badfile}`, `ensure_loaded/1` ⇒ `{error,badfile}` and
`ensure_modules_loaded/1` ⇒ `{error,[{Name,badfile}]}`, with the image loaded under neither name
(`which/1` still answers the path; oracle-verified with a `bogus.beam` really holding `realname`, and
only the oracle's own `beam_load.c` error report is omitted here).
`load_binary/3` reproduces `code.erl:667-688` clause for clause: the guard fails as the **bare-atom
`function_clause`** raise, never this floor's usual `badarg` (the oracle's `is_list/1` accepts the
**empty** list, which `beam_term::is_list` does not, so `is_nil` covers it); `modp(File)` is
`{error,badarg}` for a proper list with a non-integer element and `function_clause` for an
**improper** one; `erlang:prepare_loading/2` is `{error,badfile}` for an empty/garbage image **or one
whose own module name is not `Mod`**; the sticky check (after the image validates) is
`{error,sticky_directory}`; success registers the image under `Mod` with **`File` recorded verbatim**
(`CodeServer::insert_loaded`/`recorded_name`: an atom filename is reported as the atom —
`which/1` ⇒ `cover_compiled` — and the empty list as `[]`).
`ensure_modules_loaded/1` is the whole-list form: `prepare_ensure/2` demands a **proper list of
atoms** (a non-list, an improper list or a non-atom element raises the bare-atom `function_clause`);
the names neither preloaded nor already loaded are `lists:usort`-ed, each located by the same
`module_file` lookup and loaded through `CodeServer::load`; only failures are listed — `ok`, else
`{error,[{Name,nofile|badfile}]}` in **reverse** of the sorted order (the oracle's `do_par_recv/3`
prepends each result as it arrives); `badfile` is a `.beam` found on the path that fails to load — a
corrupt image or one whose **object code names another module**, the two shapes the oracle reports
identically.
**Accepted residual:** for either, the oracle also logs an `=ERROR REPORT==== …` line (the corrupt
image's `corrupt file header`, the name mismatch's `beam_load.c … module name in object code is
<Real>`), and this runtime emits only the value, never an invented report.
`ensure_modules_loaded/1` is served through the floor's own `try_native` arm **and** is listed in
`floor_keys("code")`, so it is a floor key on **both** routes — a **compiled** module's `call_ext` is
refused the in-machine `code.beam` body (which reaches the live `code_server` process and its second
path store, where a module that store cannot find crashes the server) and reaches this implementation
instead; pinned end to end on the `run` route by `../../tests/code_ensure_modules_loaded.rs`.
Also served: `get_mode/0` (`interactive`/`embedded`, from the `-mode` init argument), `lib_dir/0,1,2`
(`priv_dir/1` = the joined `lib_dir(App, priv)`, never `stat`ed, `{error,bad_name}` for an unknown
name), `root_dir/0`, `is_sticky/1,2`, `purge/1`, `soft_purge/1`; a non-list `File` raises the
bare-atom `function_clause`.

### `path.rs` — the mutators (and why they are floor entries)
`add_patha/1,2`, `add_pathz/1,2`, `add_path/1,2` and `del_path/1` edit the **same** store the readers
search. The wrapper bodies are clients of the live `code_server` process, whose own path copy is read
by nothing here, so entering one would land the directory in a store this runtime never searches —
`add_patha(Dir), code:get_path()` and a later `load_file/1` would disagree. The floor owns the keys so
there is **one** authoritative store.
Half the semantics is the `code.erl` guard + normalisation and half the `code_server.erl` edit:
`add_patha`/`add_pathz`/`add_path` guard `is_list(Dir), ?is_cache(Cache)` and hand a **normalised**
directory to the server's `{add_path, first|last, Dir, Cache}` (`code.erl:1069-1130`);
`add_path/1,2` is `add_pathz`; `check_path/1` decides `{error,bad_directory}`
(`code_server.erl:730-761`); `do_add(first, …)` `keytake`s then conses, so a directory already on the
path is **moved** to the head, while `do_add(last, …)` `keyreplace`s it in place; `del_path/1` matches
either the directory as written or its `get_name/1` application name (`code_server.erl:884-912`).
A failed `code.erl` guard — a non-list `Dir`, a `Cache` that is neither `cache` nor `nocache` — is the
bare-atom `function_clause` raise the oracle produces.

## Constraints
- `#![deny(unsafe_code)]` (crate-wide); terms through `beam_engine::terms` only; return-based errors
  (`Option<Result<Eterm>>` from the arm, `behavior` unchanged).
- **`code:*` behaviour must not change** — `src/native/CONTEXT.md` *Constraints* makes it binding: the
  floor's `CodeServer` owns the ONE authoritative code path, and one `code/path.rs` mutator list is
  the only writer.
- No `Eterm` at rest: `CodeServer` holds `Rc<AtomTable>`, `HashMap<String, Arc<Module>>`, two
  `HashMap<String, PathBuf>`, `HashMap<String, RecordedName>` and the path vectors, so every reply is
  **rebuilt per call** (`charlist`/`string_list` → `terms::make_*` → `alloc_words`: the process young
  heap in a scheduled slice, the pointer-stable arena otherwise).
- The one **mixed-region** reply is `get_object_code/1`: the object-code binary is an on-heap
  `ErlHeapBits` block on `Runtime::eval_heap` while the 3-tuple around it is built through
  `alloc_words` — sound because a copying collection classifies any pointer outside its from-space as
  `Target::Foreign` and leaves it verbatim, and every nursery chain space is evacuated before
  `collapse` (`src/native/CONTEXT.md` *Reply-term regions*).

## Test Strategy
- `code.rs`'s colocated suite, oracle-cross-checked through `oracle_probe` (`$ERTS_ORACLE` else `$PATH`,
  `.output()`, argv `-noshell -eval … -s init stop`, loud skip via `skip_oracle_loudly()`, gated first
  by `system_runtime()`); `code/path.rs`'s own colocated suite pins every mutator shape.
- Cross-crate: `../../tests/code_path.rs` (the 36-entry list vs the oracle), `../../tests/code_add_path.rs`
  (the mutators on the `-eval`/scheduled routes), `../../tests/boot_code_path.rs`,
  `../../tests/code_load_file.rs`, `../../tests/code_load_binary.rs` (guard/`modp`/`badfile`/sticky and
  the recorded-filename `which`/`is_loaded`/`all_loaded` observations), `../../tests/code_delete_live.rs`
  (`delete/1`'s live-server round trip), `../../tests/code_ensure_modules_loaded.rs`.

## Routing Table
- `../code.rs` → the readers listed above, `STICKY_APPS`, and the `which`/`is_loaded`/`get_object_code`
  base rules.
- `path.rs` → the four mutators, normalisation, the `function_clause` guard, the first/last edit rules
  and the app-name `del_path/1` form.
- `../../codeserver.rs` → `CodeServer` (the ONE store), `normalize_dir`, `RecordedName`,
  `insert_loaded`, `load`, `forget_module`.
- `../../codeserver/path.rs` → the release-layout derivation behind `get_path/0`.
- `../native.rs` → the `try_native` `code` arms and `floor_keys("code")`.

## See Also
- `../CONTEXT.md` → the floor's dispatch order, the constraints and the not-implemented index.
- `../../../M1_GAPS.md` *The code store* → the divergence ledger (the live server's second store, the
  omitted oracle reports).
- `../../../../../erts/emulator/beam/beam_bif_load.c` and `../../../../../lib/kernel/src/{code,code_server}.erl`
  → the C/Erlang ground truth (read-only).
