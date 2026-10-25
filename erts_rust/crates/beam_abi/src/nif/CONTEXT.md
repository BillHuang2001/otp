# `src/nif/` — NIF host layer (contract A.3)

## Intent
The **NIF C ABI host half**: every `enif_*` function a NIF `.so` imports, the `NifHost`/`NifEnv` seam those bodies are written against, the dynamic NIF loader and the `erlang:load_nif/2` load-time validation contract — over the frozen `nif::term`/`nif::types`/`nif::api` shapes owned by the crate root (`../CONTEXT.md` → *Frozen layout table*).
The boundary's *shape* is the parent's contract; its *behaviour* is this node's. Per-family behaviour and the A.3 host-extension seams live one level down in `enif/CONTEXT.md`.

Ground truth (read-only, never modify): `erts/emulator/beam/erl_nif.h`, `erl_nif_api_funcs.h`, `erl_drv_nif.h`, `erts/emulator/beam/erl_nif.c`, and `erts/lib_src/common/erl_printf_format.c` for the `*printf` subset (paths are repo-root relative).

## The aliasing contract (A.3) — the reason this node exists
A NIF callback is C. It reaches the host through the env pointer it was handed *while the Rust frame that started the callback is still on the stack*. This is not a style rule; it is the one property that makes the whole A.3 layer correct, and violating it produces a **silent** wrong answer, never a crash.
- **An env handed to C takes `&self` accessors with interior mutability.** `NifEnv` has no `&mut self` accessor at all: `host()`/`with_host(f)`/`heap()`/`as_ptr()` are `&self`, `from_opaque` returns a **shared** `&NifEnv` (as `DriverEnv::from_port` does), and the two pieces of per-call state it stores into are interior-mutable (`pending_exception: Cell<Option<Eterm>>`, `recorded_atoms: Rc<RefCell<…>>`).
- **Never hold a Rust `&`/`&mut` to a host across a call into foreign (NIF) code.** A live caller reference lets LLVM emit `noalias`/`dereferenceable` and *legally drop* the foreign store made through the env's pointer. `NifEnv::new(&mut dyn NifHost)` is **the last time the host may be touched through a Rust reference**; any callback-published state must be interior-mutable.
- **`NifHost` is `&self`-receiver only.** No `&mut self` method exists; a host's callback-visible fields are `Cell`/`RefCell`-backed, which is also what keeps the type non-`Freeze`.
- **This defect is invisible in an unoptimised build, and only in `--release`-class optimisation.** Measured for this crate by `src/nif/host_aliasing_tests.rs` (a `#[inline(never)]` `enif_make_atom` callback frame under an `#[inline(never)]` caller frame holding `&Host`): a **plain-field** host reports the store **lost** at `opt-level = 1` (this workspace's `dev` profile) and in `--release`, and only survives at `opt-level = 0`; the shipped `RefCell` host reports it kept in all three. Reverting an accessor to `&mut self`, or giving a host a plain callback-visible field, is an **A.3 correctness regression**.
- **The guard must be run in both profiles.** `erts_rust/scripts/cargo.sh test --release --manifest-path erts_rust/Cargo.toml -p beam_abi --no-fail-fast` is the gate for any change to a host seam or an env accessor: repository CI (`.github/workflows/erts_rust.yml`) runs the **debug** profile only, so it cannot be the whole gate.
- **Recovering a concrete host is `NifEnv::new_typed` + `host_as`/`with_host_as`, not an `AsAnyHost` supertrait.** `NifHost: AsAnyHost` **cannot** be adopted: the blanket impl needs `'static`, and the runtime's per-call host is `beam_rt`'s borrowing `NifHostImpl<'a>`, so it is not.

## The host seam (`nif::host`) — the runtime services behind the `enif_*` layer
The *internal* boundary the `enif_*` bodies are written against (not part of the C ABI; `nif::host.rs`).
- `pub trait NifHost` — **required** `heap() -> &mut beam_alloc::Heap`, `intern_atom(&str) -> Option<Eterm>`, `atom_name(Eterm) -> Option<String>`; **defaulted** (default = unsupported): `self_pid`, `term_to_binary`, `binary_to_term`, `binary_to_term_used`, `make_ref`, `existing_atom`, `atom_table`, `priv_data`, `system_info`, `unique_integer`, `consume_timeslice`, `is_process_alive`, `is_port_alive`, `is_current_process_alive`, `send`, `port_command`, `whereis_pid`, `whereis_port`, `monitor_process`, `monitor_process_result`, `demonitor_process`, `select`, `nif_yield_point`, `alloc_env`, `free_env`, `make_copy`, `dynamic_resource_call`, and `schedule_nif`.
  Every seam is defaulted/additive, so no implementor has to change to stay compiling; a default answers the typed "unsupported" and the `enif_*` body then reproduces the C's failure shape. The ETF hooks give `beam_abi` **no** `beam_codec` edge.
  - `binary_to_term_used(&mut self, bytes: &[u8]) -> Option<(Eterm, usize)>` — the consumed-byte decode: the term plus the byte count `enif_binary_to_term` answers. Its **default forwards to `binary_to_term` and reports `bytes.len()`**, so a host that cannot report a count keeps the strict-whole-slice behaviour; `enif_binary_to_term` prefers it.
  - `monitor_process_result(&mut self, pid: Eterm) -> Option<NifMonitorResult>` — the three-way arming outcome (`Armed(Eterm)` with the internal reference, `NotAlive`, `NoDownCallback`); `None` means unsupported and `enif_monitor_process` falls back to the legacy `monitor_process` (whose `Some` counts as `Armed`).
  - `schedule_nif` is a **legacy** hook `enif_schedule_nif` no longer consults (it takes no `argv`, so it structurally cannot run a continuation; kept for source compatibility only) — the trampoline is in-crate (`enif/CONTEXT.md`).
- `pub struct NifEnv` — the env behind the C-facing opaque `*mut ErlNifEnv`: `new(&mut dyn NifHost)`, `new_typed::<H: NifHost + 'static>` (+ `host_as`/`with_host_as`), the `&self`-receiver accessors above, `unsafe from_opaque(*mut ErlNifEnv) -> Option<&NifEnv>`, the pending-exception slot (`pending_exception`/`set_exception`/`clear`), and a **bounded per-call atom record** (`record_atom`; read back by `recorded_atom_name`/`recorded_atom`; `MAX_RECORDED_ATOMS = 256`, first-wins dedupe) — atoms commit lazily, so `NifHost::atom_name` (committed table only) cannot see one interned earlier in the *same* call.
  `enif_clear_env` does **not** touch the exception slot (as the C, `erl_nif.c:700-723`). Re-exported as `beam_abi::{NifEnv, NifHost}`.
- **Thread-local current-host stack + per-call atom record:** `NifEnv::new` pushes the host and its `Rc<RefCell<Vec<(Eterm, String)>>>` record; `Drop` pops exactly its own entry (an out-of-order drop restores the enclosing host). Readers `with_current_host<R>(f) -> Option<R>` (behind env-less calls) and `current_recorded_atom_name(atom) -> Option<String>` (innermost env wins; behind every env-less name resolution). Sound: `NifEnv` is `!Clone`/`!Send` (raw host pointer, never moved across threads), and the `Rc<RefCell<…>>` record stays readable while the env is borrowed — no `unsafe`, no `&mut` aliasing.

### The thread-env factory (`thread_env.rs`) — the off-call-path env
How a thread that is **not** inside a NIF call (an `enif_thread_create`d worker) reaches a real host.
`pub trait ThreadEnvHostFactory: Send + Sync + 'static { fn open(&self) -> Option<Box<dyn NifHost>>; }` in a `Mutex<Option<…>>` (last-install-wins; a **poisoned lock yields `None`/`false` and never panics**).
Plus a **thread-local cache** of the host `open()` returned (a `NifHost` is `!Send`, so each thread opens its own; cross-thread sharing goes through the factory's own `Send + Sync` handles) stamped with a **global generation** bumped by every `install`/`clear`, so a superseded factory's host is dropped and re-opened instead of leaking into the next test.
`with_thread_env_host<R>(f: impl FnOnce(&mut dyn NifHost) -> R) -> Option<R>` is the accessor (`None` with no factory, or when `open()` declines); `clear_thread_env_factory()` is the explicit teardown.
**Limits of a factory-opened host** (the honest cost of the seam): such a host owns a **private append-only atom space** (`atom_table` is `None`), so its env-less `enif_compare`/`is_identical`/`hash` order a nested atom by raw index; `select`, monitors, liveness, `whereis_pid`, `make_ref`, `system_info`, ETF and `port_command` are unsupported there, a remote pid destination answers `false`, and a send is queued at `enif_send` and delivered at the next drain.
The reachability order is fixed: current call host → factory-opened thread host → `NULL`; never a crate-owned env, whose private atom table would hand a NIF a *wrong* atom index.

## The `enif_*` surface (`enif/`)
**176 slots** — the frozen `ErlNifApi` table built by one `pub(super) const fn populate` per family (`mem`, `get`, `make`, `map`, `pred`, `proc`, `process`, `exception`, `threads`, `ioq`, `mapiter`, `time`, `resources`, `select`, `envs`, `variadic`), chained innermost→outermost in `enif/mod.rs` into the `#[used] static ENIF_API` that `enif_api_table()` publishes.
All 176 are populated; the seven C-variadic ones only with the C shim compiled (else `None`, a loud `dlopen` failure, never a wrong result).
**The code is the authority for the slot list** (`erts/emulator/beam/erl_nif_api_funcs.h`; `nif::api` mirrors it field for field, and `tests/abi_conformance.rs` re-derives both). The invariants a change must not break:
- **A family's slots live in that family's file.** Extending a family edits that family's `populate`, never `mod.rs`.
- **No new `#[no_mangle]` `enif_*` symbol.** `enif_make_pid`/`enif_compare_pids` are LP64 header macros, not slots; a helper seam is a plain `pub fn`.
- **A resource term's identity is its reference, not its heap word.** `TermKey::Ref(internal_ref_id)` for a frozen internal reference, `TermKey::Word` otherwise, so a `copy_struct` copy of a minted resource term still resolves (the C's pointer-identified magic ref).
- **Never dereference a pointer this crate did not allocate** — a typed `false`/`None` refusal where the oracle SIGSEGVs.
- **`enif_schedule_nif` runs its continuation in-crate**, over an iterative trampoline that is constant in Rust stack depth; `nif_yield_point` is an **honest no-op** (fairness, never correctness).
- **`enif_select` forwards the documented flag set bit-identically** and the returned `STOP` verdicts drive the resource `stop` callback, whose `STOP_SCHEDULED` half is drained through the pending-stop table.
- **`enif_alloc_env` never fabricates an env** — three-step reachability (call host → factory → `NULL`).

Per-family behaviour, the family roster with the measured `Some`/total split, the resource seams (`resources_keep`/`stop`/`monitors`/`release`/`term`), the `schedule`/`select`/`envs` detail and the family-local gaps: `enif/CONTEXT.md`.

### The seams this node exposes for the runtime
The runtime-facing API that is not an `enif_*` slot (each is a `pub fn` or a defaulted `NifHost` hook, so `ErlNifApi` stays at 176):
- `NifHost::{binary_to_term_used, monitor_process_result, nif_yield_point}` — the three defaulted hooks behind `enif_binary_to_term`, `enif_monitor_process` (arming; the runtime reports the minted internal reference here, which is what records the monitor binding) and the trampoline's per-iteration preemption point.
- `nif::enif::{resource_terms, release_resource_terms}` — resource-term reclamation from a caller that owns the reachability decision (the predicate *is* the soundness assertion; a false "unreachable" is a use-after-free, a false "reachable" only a leak).
- `nif::enif::resource_term(host, obj) -> Option<Eterm>` — mints the resource **term** naming the bare `*mut c_void` a `NifHost::select` request carries (the C's `erts_mk_magic_ref` inside the `{select,ResourceTerm,Ref,Event}` tuple); the runtime mints it at delivery, then drops its reference through the prune.
- `nif::enif::{deliver_resource_down, deliver_resource_down_id, resource_monitors, MonitorId}` — the `'DOWN'`-delivery point: run the resource type's `down` callback with the `NifMonitorResult::Armed(term)` reference, or with the holder when only the 32-byte `ErlNifMonitor` block was kept (`MonitorId::from_holder`).
- `nif::enif::{keep_resource, unkeep_resource}` — the env-free keep/unkeep pair the `enif_select` route drives per interest (arm ⇒ keep, stop/cancel ⇒ unkeep), because that route holds the object alive with no environment and no `unsafe`.
- `nif::enif::run_pending_stops()` — the drain a host whose poller really defers a stop calls from its poll loop; after it, `enif_select`'s `STOP_SCHEDULED` answer is faithful (`beam_rt`'s store de-registers immediately, so it answers `STOP_CALLED` instead).
- `nif::{ThreadEnvHostFactory, install_thread_env_factory, clear_thread_env_factory}` — installs the off-call-path host; `open()` must reach the **live** `AtomTable` and inject sends thread-safely (`beam_abi::driver::async_pool` is the existing model for the latter).

## Dynamic loader (`loader.rs`)
`load(path, module, host, load_info) -> Result<NifLibrary, NifLoadError>` does `dlopen(path+".so", RTLD_NOW|RTLD_LOCAL)`, `dlsym("nif_init")` (fallback `_nif_init`), validates in the oracle's exact order (the table below) and invokes `entry.load`.
`NifLibrary::call(env, name, arity, args)` looks the function up by `(name, arity)`, calls `fptr(env.as_ptr(), arity, args.as_ptr())`, and turns a pending env exception into `NifCallError::Raised`; for the call's duration it publishes the library's `priv_data` on a crate-private thread-local stack with the same push/pop-token discipline as the host stack — the only source of `enif_priv_data`'s `env->mod_nif->priv_data`.
`Drop` runs `entry.unload` then `dlclose`. `Reload`/`Upgrade`/`OldCode` exist for surface completeness but `load()` never produces them (no module-instance / old-code state here).

## Symbol-export mechanism (load-bearing)
A NIF `.so` leaves `enif_*` (and a driver `.so` leaves `driver_*`/`erl_drv_*`) **undefined**; the dynamic linker binds them, at `dlopen(RTLD_NOW)`, against the **loading process's** dynamic symbol table, so `build.rs` emits `cargo::rustc-link-arg-tests=-Wl,--export-dynamic` for each `beam_abi` test binary.
Proven: `nm -D --undefined-only test_nif.so` lists the used `enif_*` as `U` and the same names appear `T` in the test binary's `.dynsym`; loading that `.so` from a process that does **not** export them fails with `undefined symbol: enif_make_int`.
The `cargo::rustc-link-arg-bins` counterpart must **not** be emitted: this package has no `[[bin]]` target and cargo hard-errors on it.

## The C-variadic shim (mechanism)
- The seven slots (`enif_make_tuple`, `enif_make_list`, `enif_set_option`, `enif_fprintf`, `enif_snprintf`, `enif_vfprintf`, `enif_vsnprintf`) live in `csrc/nif_variadic.c`.
- Rust cannot read a `va_list`, so it *drives* the extraction: for the `*printf` family the shim calls `beam_abi_enif_fmt_analyze` (a Rust format-string parser), then pulls exactly the varargs the parser reported into a flat `u64` slot array and calls `beam_abi_enif_fmt_{snprintf,fprintf}`, which render the string — parser and extractor share one grammar, so they cannot disagree.
- The supported `*printf` subset is a faithful port of `erts/lib_src/common/erl_printf_format.c`; anything outside it returns a **negative error** (`-EINVAL`), never a wrong string.
- `build.rs` emits `cargo::rustc-check-cfg=cfg(beam_abi_variadic_shim)` unconditionally (so the cfg is known on the degraded path) and sets it only when the shim compiled.

## Oracle behavior: `erlang:load_nif/2` load-time contract
Ground truth `erts/emulator/beam/erl_nif.c` (`erts_load_nif`, line 4729) — the checks validating an `ErlNifEntry` (the fields this crate declares), reproduced by `loader.rs` and to be reproduced by the runtime. Verified empirically against the system OTP 29.1.1 / erts-17.1 (scratch NIF in `$TMPDIR`, read-only C).
- **Not a plain BIF.** Registered in `bif.tab:527` as `erlang:load_nif/2`; the compiler rewrites a `call_ext` to it into the loader-only instruction `i_load_nif` (`emu/ops.tab:854-865`, JIT `jit/x86/ops.tab:561-569`). The BIF body `load_nif_2` (`erl_nif.c:4643`) is only a stub rejecting `apply(erlang, load_nif, ...)`: `{error,{bad_lib,"load_nif/2 must be explicitly called from the NIF module. It cannot be called through apply/3."}}`. **No `erts_internal` variant exists.**
- **Calling module derivation** (`erl_nif.c:4764-4770`): `caller = erts_find_function_from_pc(I)` (`I` = the PC of the `i_load_nif` instruction), then `mod_atom = caller->module` — a per-code-range binary search over the module's `BeamCodeHeader` function table (`beam_ranges.c:245-305`), *not* a stack-frame scan. The module is whichever module's code contains the call site.
- **`nif_init` symbol** (`erl_nif.h:407-409`; `ERL_NIF_INIT` 422-444): dynamic NIF exports `ErlNifEntry* nif_init(void)` (fallback `_nif_init`, `erl_unix_sys_ddll.c:230-242`); `STATIC_ERLANG_NIF` uses `<module>_nif_init`. The filename argument gets `FILE_EXT` (`.so` on Linux, `erl_unix_sys_ddll.c:48`) appended by `erts_sys_ddll_open`.
- **Validation order & exact return shapes** — success is the atom `ok`; every failure is `{error,{Reason,Text}}` (built by `load_nif_error`, `erl_nif.c:4551`):
  | # | check (`erl_nif.c`) | `Reason` | `Text` |
  |---|---|---|---|
  | 0 | call site in old code (4781) | `old_code` | `Calling load_nif from old module '<M>' not allowed` |
  | 1 | `this_mi->nif != NULL` — reload (4795) | `reload` | `NIF library already loaded (reload disallowed since OTP 20).` |
  | 2 | `dlopen` fails (4799) | `load_failed` | `Failed to load NIF library: '<...>...'` (or `Failed to load NIF library <name>: '<...>'`) |
  | 3 | `dlsym nif_init` fails (4810) | `bad_lib` | `Failed to find library init function: '<...>'` |
  | 4 | `nif_init()` == NULL (4815) | `bad_lib` | `Library init-call unsuccessful` |
  | 5 | `major>2 \|\| (major==2 && minor>18)` (4820) | `bad_lib` | `That '<M>' NIF library needs <min_erts> or newer. Either try to recompile the NIF lib or use a newer erts runtime.` |
  | 6 | `major<2 \|\| (major==2 && minor==5)` (4827) | `bad_lib` | `That old NIF library (<maj>.<min>) is not compatible with this erts runtime (2.18). Try recompile the NIF lib.` |
  | 7 | `vm_variant != "beam.vanilla"` (≥2.1) (4835) | `bad_lib` | `Library (<variant>) not compiled for this vm variant (beam.vanilla).` |
  | 8 | `entry->name` != calling module (4841) | `bad_lib` | `Library module name '<name>' does not match calling module '<M>'` |
  | 9 | `ErlNifFunc` name/arity not found (4874) | `bad_lib` | `Function not found <M>:<name>/<arity>` |
  | 10 | func exists but not `nif_start`-marked (4883) | `bad_lib` | `Function not declared as nif <M>:<name>/<arity>` |
  | 11 | `flags` ∉ {0, DIRTY_IO_BOUND, DIRTY_CPU_BOUND} (4889) | `bad_lib` | `Illegal flags field value <n> for NIF <M>:<name>/<arity>` |
  | 12 | duplicate NIF entry (4917) | `bad_lib` | `Duplicate NIF entry for <M>:<name>/<arity>` |
  | 13 | `load` callback veto ≠ 0 (5005) | `load` | `Library load-call unsuccessful (<veto>).` |
  | 14 | upgrade with NULL `upgrade` (4979) | `upgrade` | `Upgrade not supported by this NIF library.` |
  | 15 | `upgrade` veto ≠ 0 (4996) | `upgrade` | `Library upgrade-call unsuccessful (<veto>).` |
- **`check_is_nifable`** (`erl_nif.c:4721`): `!mod_code->are_nifs || mod_code->are_nifs[func_ix]`, `are_nifs` built from the `op_nif_start` opcode (`emu_load.c:1414`). Compiler rule (`v3_core.erl:335-339`): a module calling `load_nif/2` gets `nif_start` inserted into **every** function **unless** it has an explicit `-nifs([...])` attribute (then only the listed functions are marked) — so check #10 is reachable only via `-nifs`.
- **Version constants** (`erl_nif.h:67-91`): major 2 / minor 18 / min-required-major-on-load 2 / `ERL_NIF_MIN_ERTS_VERSION` `"erts-14.0"` / `ERL_NIF_VM_VARIANT` `"beam.vanilla"`. `AT_LEAST_VERSION` (`erl_nif.c:4580`) compares `major*0x100+minor`; `create_lib` (`erl_nif.c:4586`) makes a "modern" copy defaulting absent fields (`vm_variant`, `"beam.vanilla"`, `min_erts`, `"erts-?"`, `sizeof_ErlNifResourceTypeInit`, `options`, `funcs[].flags`).
- Empirical raw outputs (module `m`, NIF entry name `m`): success `ok`; a missing path and a text file both `{error,{load_failed,"Failed to load NIF library: '...'…"}}`; major 99 → `{error,{bad_lib,"That 'm' NIF library needs erts-99.0 or newer. …"}}`; no `nif_init` → `{error,{bad_lib,"Failed to find library init function: '.../emptysym.so: undefined symbol: _nif_init'"}}`; reload → `{error,{reload,"NIF library already loaded (reload disallowed since OTP 20)."}}`; `-on_load` calling `load_nif` succeeds and the module's NIF is callable; `apply/3` → the stub error.
- In this Nix-packaged runtime `code:lib_dir(erts,include)` points at a non-existent path; the real header is `.../lib/erlang/erts-17.1/include/erl_nif.h`.

## Known gaps (A.3 residuals — honest, never a wrong result)
The driver-side gaps are `../driver/CONTEXT.md`'s; the cross-cutting ones (undeclared header names, the malformed-term guard) are `../CONTEXT.md`'s; the family-local ones are `enif/CONTEXT.md`'s.
- **`enif_alloc_env` answers `NULL`** when **no host is reachable**: no live `NifEnv` on the thread (outside a NIF call) **and** no `ThreadEnvHostFactory` installed, or the current host's `alloc_env` is unsupported and no factory answers either.
  With a factory installed, a NIF-created thread opens its own host and gets a real message env — within that host's documented limits (private append-only atom space, no `select`/monitors/liveness, a send queued to the next drain).
  `beam_rt`'s `NifHostImpl` implements `alloc_env`/`free_env`/`make_copy` (`beam_rt/src/nif.rs`) over the one shared pooled BIF heap, so the runtime's message-env idiom works on the call path and `make_copy` is the identity; `beam_rt` also supplies a factory (`Runtime::install_nif_thread_env`, a process-global opt-in), so a runtime that has not installed one still answers `NULL` off the call path.
- **NIF host hooks the runtime does not implement.** `beam_rt`'s `NifHostImpl` answers every hook whose state it owns (atoms/atom table, ETF, refs, liveness, sends, `port_command`, monitors, `select`, message envs, `unique_integer`, `system_info`, `self_pid`, `nif_yield_point`); the rest sit at their trait defaults — `priv_data`, `consume_timeslice`, `whereis_port`, `dynamic_resource_call`, plus the two legacy hooks (`monitor_process`, superseded by `monitor_process_result`; `schedule_nif`, which `beam_abi` never consults).
  Each remaining default and its exact reason are documented in `beam_rt/src/nif.rs` (*The remaining hooks — and exactly why*); `beam_abi`'s side is complete. The resource-term seam's caller half (`release_resource_terms` driven from the GC/park point with the engine's root set as the predicate) is likewise `beam_rt`'s.
- **No scheduler threads.** `enif_thread_type` reports only `ERL_NIF_THR_UNDEFINED` (foreign thread) or `ERL_NIF_THR_OTHER` (`sync::thread_create`), never a scheduler/dirty-scheduler type; `*_name` reproduces the C's `no_name` = `"unknown"` for a live NULL-named handle and `NULL` for a foreign or already-joined tid (`erl_drv_thread.c:99,135,664-684`; `enif_*` forwards `erl_nif.c:2438-2441`).
- **`sync::tsd_*` passes the key straight to pthreads unchecked** (no `used_tsd_keys[]` validation); an invalid/destroyed key makes `set`/`get` a safe no-op (`NULL`) where the C aborts.
- **A dirty-flagged continuation runs on the calling thread** (only `enif_thread_type`'s reported type is the oracle's), and `nif_yield_point` is an honest no-op — a fairness residual, never a correctness one.
- **`enif_send` throughout this tree copies** where the C transfers the message-env fragment chain; a `stop`-less resource type answers the host's code and runs no callback (the oracle SIGSEGVs on that shape).
- **`enif_time_offset/1` has no `erl`-oracle pin** (its monotonic-time relation is pinned by unit tests only, `enif/time.rs`).

## Constraints
- **The C ABI shape is frozen by the parent.** `nif::{term,types,api}` are `../CONTEXT.md`'s; nothing here may change a struct layout, an enum width, a fn-pointer signature or the 176-entry table width.
- **The aliasing contract above is not style.** No `&mut self` accessor on `NifEnv`, no plain (non-interior-mutable) callback-visible host field, no live `&`/`&mut` to a host across a call into foreign code.
- **No new `#[no_mangle]` `enif_*` symbol**; a family's slots live in that family's file (extend its `populate`, never `mod.rs`); a host-extension helper is a plain `pub fn`.
- **No `beam_codec` dependency edge.** ETF goes through `NifHost`; the crate's dependencies are `beam_common`, `beam_term`, `beam_alloc` and `libc` (the C-variadic shim additionally wants `$CC`/`$AR`, both optional — absence degrades, never fails the build).
- **`unsafe` discipline** — every FFI declaration documented, every `unsafe` block preceded by a `// SAFETY:` rationale, `unsafe_op_in_unsafe_fn` clean; the raw-pointer ABI wrappers carry a scoped `allow(clippy::not_unsafe_ptr_arg_deref)` with the rationale inline.
- **Both profiles are the gate** for anything touching a host seam or an env accessor (`--release` too; CI is debug-only).
- **Licensing:** every new `*.rs` file here carries the OTP Apache-2.0 `%CopyrightBegin% … %CopyrightEnd%` header block, byte-exactly as `scripts/license-header.es` enforces.
- Files target ~1000 lines (this node's `host.rs` is just over, and `api.rs` is a legitimately long generated table); split by responsibility.

## Routing Table
- `mod.rs` → module docs, the `term`/`types`/`api`/`host`/`enif`/`loader` wiring (re-exported through `src/lib.rs`)
- `term.rs` → `ERL_NIF_TERM`/`ERL_NIF_UINT`, the `Eterm ↔ ERL_NIF_TERM` seam
- `types.rs` → NIF `#[repr(C)]` structs/enums/flags/fn-type aliases (from `erl_nif.h`), incl. the opaque `ErlNifEnv`
- `api.rs` → `ErlNifApi` (176-entry `enif_*` function table + `ERL_NIF_API_FIELD_COUNT`; generated, header-ordered — legitimately long, and the authority for the slot list)
- `host.rs` → the host seam: `NifHost` (required + defaulted hooks), `NifEnv` (`new`/`new_typed`/`host_as`, the `&self` accessors, `from_opaque`, the exception slot, the per-call atom record, `Drop`), the current-host stack and the `pub(crate)` `with_current_host`/`current_recorded_atom_name` readers
- `host_aliasing_tests.rs` → the **optimised-profile aliasing guard** (the plain-field-vs-`RefCell` store-loss matrix); must run in both profiles
- `thread_env.rs` → the thread-env factory seam (`ThreadEnvHostFactory::open`, `install_thread_env_factory`, `clear_thread_env_factory`, `with_thread_env_host`): the `Mutex<Option<…>>` factory, the per-thread host cache, the generation counter that supersedes a stale host, and the reasoning for answering `NULL` rather than a crate-owned env; `thread_env_tests.rs` is its test child
- `thread_env_tests.rs` → the factory seam's tests (`no_factory_means_alloc_env_is_still_null_on_a_fresh_thread`, `a_factory_installed_thread_gets_a_usable_env_and_hands_it_back`, `enif_send_with_a_null_env_resolves_the_host_from_the_message_env`, `the_current_call_host_wins_over_the_factory`, `a_current_host_without_alloc_env_falls_through_to_the_factory`, `a_new_install_supersedes_the_cached_host`, `a_poisoned_factory_lock_refuses_instead_of_panicking`, `a_factory_that_declines_yields_no_host`)
- `loader.rs` → the dynamic NIF loader: `NifLibrary`, `NifLoadError`/`NifCallError`, `load()` (dlopen/dlsym + oracle-order validation), `NifLibrary::call` (also the `priv_data` publication behind `enif_priv_data`), `Drop` (unload + dlclose)
- `enif/` → the per-family `enif_*` implementations (one file per family, each owning its `populate`), the `ENIF_API` chain, and the six A.3 host-extension seams (`schedule` trampoline, `resources_keep`/`resources_stop`+`stop`/`resources_monitors`/`resources_release`/`resources_term`) — see `enif/CONTEXT.md`
- `../driver/CONTEXT.md` → the sibling driver host layer (A.4) (sibling node — read-only, escalate writes to `..`)
- `../../tests/` → the crate-level NIF integration tests + their real-C fixtures (sibling of this node — read-only, escalate writes to `..`)
- `erts/emulator/beam/` → the ground-truth C (`erl_nif.c`, `erl_nif.h`, `erl_nif_api_funcs.h`) — read-only, repo-root relative

## Test Strategy
- **Per-family end-to-end tests** (`../../tests/nif_*.rs`, 18 files) — each compiles a real C fixture at test time with the system `cc` against the system OTP headers and `dlopen`s it (`RTLD_NOW`) through this crate's own loader, so a single unresolved `enif_*` import fails the load; `nif_variadic.rs` is gated on `cfg(beam_abi_variadic_shim)` and `nif_threads.rs` carries the extended `*_name` matrix (`"unknown"`).
- **Oracle cross-checks that really run** (they skip loudly without `erl`/`erlc`/`cc`): `nif_threads.rs` loads the *same* fixture `.so` from the system Erlang and compares the printed tuple; `nif_mapiter.rs`'s `the_oracle_agrees_on_the_flat_map_key_order` runs the real `erl` over `maps:from_list/1` → `maps:keys/1` and requires the fixture's C-side walk order to equal it; `nif_resource.rs`'s expected tuples are the measured oracle values (`post_release/0` = `{1,42,0}`), and its `dtor_probe/0` + `dtor_state/0` are **Rust-only** (the oracle's C `enif_release_resource(NULL)`/`enif_keep_resource(NULL)` dereference the operand and segfault); `nif_compare.rs`, `nif_priv_data.rs` and `nif_map_arrays.rs` carry the same shape (`nif_map_arrays.rs` pins the measured duplicate-key table: unique 3 → `{1,3}`, hash arm 40 → `{1,40}`, `1`+`1.0` → `{1,2}`, `7` twice → `{0,overwritten}`, 34-pair hash arm with `5` twice → `{0,overwritten}`, `=:=`-equal tuples → `{0,overwritten}`).
- **The three A.3 seam drivers** (`../../tests/nif_bin2term.rs`, `nif_monitor_codes.rs`, `nif_resource_release.rs`) pair a Rust-side assertion with a `the_same_fixture_agrees_*` oracle half over the **same** `.so`; their measured answers are in `enif/CONTEXT.md`.
- **Unit tests** — `src/nif/host_aliasing_tests.rs` (the aliasing guard), `src/nif/thread_env_tests.rs` (the factory), and the per-family in-module suites in `enif/` (roster in `enif/CONTEXT.md`). No fixture, no oracle.
- **Term-lifetime pitfall (host arena).** A NIF's result term lives on the **host arena** behind the call's `NifEnv`, readable only while that arena is alive (`tuple_size == 0`); `nif_variadic.rs`'s `Session` therefore owns the host (boxed), the loaded library and the term.
- **Loud `SKIP:`, never a silent pass** — a test that cannot execute (no `cc`, no OTP headers, no `erl`) prints a `SKIP:` banner; the oracle half must really execute.
- **Run in `--release` too** for anything touching a host seam or an env accessor (`erts_rust/scripts/cargo.sh test --release --manifest-path erts_rust/Cargo.toml -p beam_abi --no-fail-fast`); CI is debug-only.
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_abi`; behavioural cross-checking against real ERTS is `erts_rust/tools/differential`'s job (read-only, escalate writes to `..`).

## Notes for Agents
- `erts/emulator/beam/global.h` line ~57 holds the real `struct enif_environment_t` body — emulator-internal and **not** to be replicated: `ErlNifEnv` stays opaque here, its Rust analogue for a live call being `nif::host::NifEnv`, reached only through `as_ptr`/`from_opaque`.
- A NIF fixture must reference **only** `enif_*` slots the crate populates (`Some`), or `dlopen(RTLD_NOW)` fails on the unresolved import.
- **A NIF-loading test binary must export its symbols.** The fixture `.so` leaves `enif_*` undefined and the dynamic linker binds them against the *loading process*, which only works because `build.rs` emits `-Wl,--export-dynamic` for this crate's test targets. Any reproduction outside this package (a scratch crate, a `rustc` one-off) needs the same link argument, or every fixture load fails with `undefined symbol: enif_inspect_binary`.
- **Term lifetime:** a term produced by a NIF lives on the host arena; keep the host (and the loaded library) alive while inspecting it, or read a use-after-free (see *Test Strategy*).
- The seven C-variadic slots need `csrc/nif_variadic.c` compiled (`build.rs` sets `cfg(beam_abi_variadic_shim)`); without a C compiler they are `None` and a `dlopen` that imports one fails loudly.
- **Do not probe `enif_demonitor_process(env, NULL, &zeroed)` under the oracle**: the system `erl` dereferences the NULL pid and crashes. The measured demonitor values come from the legitimate shapes (`{0,1}` after the target died, `1` for an already-gone reference).
- Building: **never `cd`**; use `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_abi` (the sandbox `$HOME` is read-only, and the wrapper picks a writable `CARGO_HOME`).
- A `.rs` file added here needs the OTP Apache-2.0 header block; a family file earns a `populate` (see `enif/CONTEXT.md` → *Constraints*).

## See Also
- `../CONTEXT.md` → the crate root: the frozen layout table, the ABI version constants, the `ErlNifApi` table/`ERL_NIF_API_FIELD_COUNT`, dependencies, `unsafe` discipline.
- `enif/CONTEXT.md` → the per-family implementations, the six A.3 host-extension seams, the family roster, the family-local gaps.
- `../../CONTEXT.md` → the crate-layer contracts (`beam_abi` is wave 0) and the seam types it must expose.
- `../../../CONTEXT.md` §1 → workspace contract A.3 (NIF C ABI); §2 → the frozen term layout the `Eterm ↔ ERL_NIF_TERM` seam sits on.
- `../driver/CONTEXT.md` → the sibling driver host layer (A.4).
- `../../beam_term/` → the `Eterm` this node converts to/from; `../../beam_alloc/` → the `Heap` behind `NifHost::heap`.
- `../../beam_rt/src/nif.rs` → the production `NifHost` implementor (`NifHostImpl`) and the NIF call interception at `call_ext`.
