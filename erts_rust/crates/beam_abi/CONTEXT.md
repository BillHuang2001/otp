# `beam_abi` — Layout-Frozen NIF & Driver C ABI

## Intent
Owns the **frozen foreign-function C ABIs** Rust ERTS exposes to native code: the NIF C ABI (contract **A.3**) and the driver C ABI (**A.4**) — the legacy headers' exact struct layouts, enum widths, fn-pointer signatures and constants, so a NIF/driver `.so`/`.o` built against the *unmodified* OTP headers links against Rust ERTS and behaves identically.
Part II §C's **link-time frozen-header seam**: a layout mismatch is an ABI break.
The crate freezes the boundary's *shape* — `src/nif/{term,types,api}.rs`, `src/driver/{types,api}.rs`, `src/common.rs`, `src/layout.rs`, `src/sync/` — and **implements both host sides**, each owned by its own child node: the **NIF host layer** (`src/nif/`, contract A.3) and the **driver host layer** (`src/driver/`, contract A.4) (*Routing Table*).

Ground truth (read-only, never modify): `erts/emulator/beam/erl_nif.h`, `erl_nif_api_funcs.h`, `erl_drv_nif.h`, `erl_driver.h` — the only stable ABI headers (paths in this crate are cited repo-root relative). Per-OS `driver_int.h` and the Windows-specific paths are **out of scope**.

## ABI version targeted (record this — it is the compatibility claim)
- `ERL_NIF_MAJOR_VERSION` = **2**, `ERL_NIF_MINOR_VERSION` = **18** (`erl_nif.h`)
- `ERL_NIF_MIN_ERTS_VERSION` = `"erts-14.0"`, `ERL_NIF_VM_VARIANT` = `"beam.vanilla"`
- `ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD` = 2
- `ERL_DRV_EXTENDED_MARKER` = `0xfeeeeeed`, `ERL_DRV_EXTENDED_MAJOR_VERSION` = **3**, `ERL_DRV_EXTENDED_MINOR_VERSION` = **3**, `ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD` = 2
- Target platform: **64-bit LP64** (Linux x86-64 / AArch64): pointer/`size_t`/`unsigned long` = 8, `int` = 4. Every layout number below is for LP64.

## API Surface
### The term seam (`nif::term`)
- `pub struct ERL_NIF_TERM(usize)` — `#[repr(transparent)]`, `Copy + Clone + PartialEq + Eq + Hash + Debug + Default`; ABI-identical to `beam_term::Eterm` and to the C `ERL_NIF_TERM` (`unsigned long` on LP64, one machine word). `pub type ERL_NIF_UINT = ERL_NIF_TERM` (the header's alias).
- `eterm_from_nif(ERL_NIF_TERM) -> Eterm` / `nif_from_eterm(Eterm) -> ERL_NIF_TERM` / `ERL_NIF_TERM::raw(self) -> usize` / `from_raw(usize)` — all `pub const fn`, safe, over `Eterm::raw()`/`Eterm::from_raw()`; **no `mem::transmute`** (`usize`).

### NIF types (`nif::types`) — `#[repr(C)]`, field order exactly as `erl_nif.h`
`ErlNifFunc`, `ErlNifEntry`, `ErlNifBinary`, `ErlNifPid`, `ErlNifPort`, `ErlNifResourceTypeInit`, `ErlNifMapIterator` (`ErlNifMapIteratorFlat`, `ErlNifMapIteratorHash`, `ErlNifMapIteratorUnion`), `ErlNifIOVec`, `ErlNifSysInfo` (= `ErlDrvSysInfo`); opaque handles `ErlNifEnv`, `ErlNifResourceType`, `ErlNifIOQueue`, `ErlNifTid`, `ErlNifMutex`, `ErlNifCond`, `ErlNifRWLock`; scalars `ErlNifUInt64`/`ErlNifSInt64`/`ErlNifUInt`/`ErlNifSInt`, `ErlNifTime`, `ErlNifEvent`, `ErlNifTSDKey`, `ErlNifThreadOpts`, `ErlNifMonitor`; function types `ErlNifResourceDtor`/`Stop`/`Down`/`DynCall`, `ErlNifOnHaltCallback`, `ErlNifOnUnloadThreadCallback`; enums `ErlNifCharEncoding`, `ErlNifTimeUnit`, `ErlNifMapIteratorEntry`, `ErlNifHash`, `ErlNifTermType`, `ErlNifOption`, `ErlNifIOQueueOpts`; flag types `ErlNifResourceFlags`, `ErlNifSelectFlags`, `ErlNifUniqueInteger`, `ErlNifBinaryToTerm`, `ErlNifDirtyTaskFlags`.

### `ErlNifApi` (`nif::api`) — the NIF function table (the frozen `enif_*` surface)
A `#[repr(C)]` struct of the **176** `enif_*` fn pointers that exist on LP64, in exact `erl_nif_api_funcs.h` declaration order — the Rust analogue of the header's Windows-only `TWinDynNifCallbacks`. Derivation: `ERL_NIF_API_FUNC_DECL` occurs 185× but only 180 are declarations (5 guard/comment lines); the 4 `#if SIZEOF_LONG != 8` entries (`enif_get_int64`, `enif_get_uint64`, `enif_make_int64`, `enif_make_uint64`) do not exist on LP64 (`#define`, `long`, `ulong`) ⇒ 180 − 4 = **176**; macro-only `enif_ioq_enq` (`ERL_NIF_API_FUNC_MACRO` only, no `DECL`/signature/definition) is excluded. `size_of::<ErlNifApi>() == 176 * 8 == 1408` asserted at compile time; `ERL_NIF_API_FIELD_COUNT == 176` in `nif::enif`.

### Driver types (`driver::types`) — `#[repr(C)]`, exactly as `erl_driver.h`
`ErlDrvEntry`, `ErlDrvBinary`, `ErlIOVec`, `ErlDrvNowData`; opaque handles `ErlDrvPort`, `ErlDrvEvent`, `ErlDrvData`, `ErlDrvThreadData`, `ErlDrvPDL`, `ErlDrvTid`/`ErlDrvMutex`/`ErlDrvCond`/`ErlDrvRWLock` (+ `_` marker structs); scalars `ErlDrvUInt64`/`ErlDrvSInt64`/`ErlDrvUInt`/`ErlDrvSInt`, `ErlDrvTermData`, `ErlDrvSizeT`, `ErlDrvSSizeT`, `ErlDrvTime`, `ErlDrvTSDKey`; enum `ErlDrvTimeUnit`; flag newtype `ErlDirtyJobFlags`; constants `ERL_DRV_*` (term-data tags, `ERL_DRV_FLAG_*`, `PORT_*`, `ERL_DRV_READ/WRITE/USE*`, `DRIVER_CALL_KEEP_BUFFER`, `ERL_DRV_ERROR_*`, `ERL_DRV_BUSY_MSGQ_*`).

### `ErlDrvApi` (`driver::api`) — the driver function table (the frozen `driver_*`/`erl_drv_*` surface)
A `#[repr(C)]` struct of the **111** fn pointers for every function the *runtime provides* to drivers (`driver_output`, `driver_select`, `erl_drv_mutex_*`, `erl_drv_thread_*`, `driver_alloc*`, `driver_enq*`), fields `Option<unsafe extern "C" fn ...>` (null-optimized) in `erl_driver.h` order (single-table export shape: *Design Decisions*).

### Shared (`common`) — `erl_drv_nif.h`
`ErlNapiUInt64`/`ErlNapiSInt64`/`ErlNapiUInt`/`ErlNapiSInt`, `SysIOVec`, `ErlDrvSysInfo`, `ErlDrvThreadOpts`, `ErlDirtyJobFlags`, `ErlNifSelectFlags`, `ErlDrvMonitor`, the `ERTS_NAPI_*` time constants, and `ERL_NIF_SELECT_*` / `ErlNifSelectFlags` selector bits.

### Populated slots (MEASURED — no `None` on any LP64 slot)
`ErlNifApi` **176/176** populated, `ErlDrvApi` **111/111**; `tests/abi_conformance.rs` re-derives both counts from the headers, and each child node tabulates its families' `Some`/total split (`src/nif/CONTEXT.md`, `src/driver/CONTEXT.md`).
The seven C-variadic slots are `Some` only with the `csrc/nif_variadic.c` shim compiled (else `None`, a loud `dlopen` failure, never a wrong result).

### Module layout
`src/nif/enif/` and `src/driver/drv/` are **directory modules, one file per family**: each family file owns `pub(super) const fn populate(api) -> api` filling in **only its own** slots, and the `mod.rs` chains the `populate`s **innermost→outermost** into the `#[used] static ENIF_API`/`DRIVER_API` published by `enif_api_table()`/`driver_api_table()` — extending a family edits that family's file, never the `mod.rs` (a family's slots live in that family's file). `src/sync/` is the **pthread facade** (`erl_drv_thread.c`'s equivalent) both thread families delegate to (deviations: `src/sync/mod.rs`, `src/nif/CONTEXT.md` → *Known gaps*); `csrc/nif_variadic.c` is the C-variadic shim compiled by `build.rs`.

## Frozen layout table (LP64, asserted at compile time in `layout.rs`)
| type | size | align | asserted field offsets |
|---|---|---|---|
| `ErlNifFunc` | 32 | 8 | name 0, arity 8, fptr 16, flags 24 |
| `ErlNifEntry` | 96 | 8 | major 0, minor 4, name 8, num_of_funcs 16, funcs 24, load 32, reload 40, upgrade 48, unload 56, vm_variant 64, options 72, sizeof_ErlNifResourceTypeInit 80, min_erts 88 |
| `ErlNifBinary` | 40 | 8 | size 0, data 8, ref_bin 16, `__spare__` 24 |
| `ErlNifPid` / `ErlNifPort` | 8 | 8 | pid 0 / port_id 0 |
| `ErlNifResourceTypeInit` | 40 | 8 | dtor 0, stop 8, down 16, members 24, dyncall 32 |
| `ErlNifMapIterator` | 56 | 8 | map 0, size 8, idx 16, u 24, `__spare__` 40 |
| `ErlNifIOVec` | 424 | 8 | iovcnt 0, size 8, iov 16, ref_bins 24, flags 32, small_iov 40, small_ref_bin 296 |
| `ErlNifApi` | 1408 | 8 | (176 fn ptrs) |
| `ErlDrvSysInfo` (= `ErlNifSysInfo`) | 56 | 8 | driver_major_version 0, erts_version 8, otp_release 16, thread_support 24, … scheduler_threads 36, …, dirty_scheduler_support 48 |
| `ErlDrvThreadOpts` | 4 | 4 | suggested_stack_size 0 (in **kilowords**, `-1` = platform default) |
| `ErlDrvMonitor` | 32 | 1 | data 0 |
| `ErlDrvBinary` | 16 | 8 | orig_size 0, orig_bytes 8 |
| `ErlIOVec` | 32 | 8 | vsize 0, size 8, iov 16, binv 24 |
| `ErlDrvEntry` | 176 | 8 | init 0, start 8, stop 16, output 24, ready_input 32, ready_output 40, driver_name 48, finish 56, handle 64, control 72, timeout 80, outputv 88, ready_async 96, flush 104, call 112, unused_event_callback 120, extended_marker 128, major_version 132, minor_version 136, driver_flags 140, handle2 144, process_exit 152, stop_select 160, emergency_close 168 |
| `ErlDrvNowData` | 24 | 8 | megasecs 0, secs 8, microsecs 16 |
| `SysIOVec` (= `libc::iovec`) | 16 | 8 | iov_base 0, iov_len 8 |

Every C enum-with-values type (`ErlNifCharEncoding`, `ErlNifTimeUnit`, `ErlNifMapIteratorEntry`, `ErlNifHash`, `ErlNifTermType`, `ErlNifOption`, `ErlNifIOQueueOpts`, `ErlDrvTimeUnit`) is a 4-byte `#[repr(C)]` enum, every flag type a 4-byte `#[repr(transparent)]` newtype over `c_int`/`c_uint` (both asserted).

## Constraints
- **Layout is FROZEN.** Every `#[repr(C)]` struct/enum and constant matches the legacy header; a change needs a `../../../CHANGE.md` entry and a note here.
- **Compile-time asserts are mandatory** for every type above: `const _: () = assert!(core::mem::size_of::<T>() == N);` + `assert!(core::mem::offset_of!(T, f) == N);` (`core::mem`, `static_assertions`). A mismatch is a **compile error**, not a test failure.
- **Dependencies:** `beam_common`, `beam_term`, **`beam_alloc`** (`NifHost::heap`), **`libc`** — deliberately **no `beam_codec`** (ETF goes through `NifHost`). The C-variadic shim additionally needs a **C compiler** (`$CC`, default `cc`) and an archiver (`$AR`, default `ar`), both **optional** (absence degrades, it does not fail the build).
- **`build.rs`:** (1) `cargo::rustc-link-arg-tests=-Wl,--export-dynamic` (*Symbol-export mechanism*); (2) compiles `csrc/nif_variadic.c` and emits `cfg(beam_abi_variadic_shim)` + `rustc-link-search`/`rustc-link-lib=static`.
- **`unsafe` discipline:** every FFI declaration is documented, and every `unsafe` block (`NifEnv`, `DriverEnv`) carries a preceding `// SAFETY:` rationale; `unsafe_op_in_unsafe_fn` stays clean.
- **The raw-pointer ABI wrappers intentionally `allow(clippy::not_unsafe_ptr_arg_deref)`**: the `DriverLibrary` callback wrappers (`src/driver/loader.rs`, `start`, `control`, `call`, `stop`, `output`, `outputv`, `timeout`, `ready_input`, `ready_output`, `ready_async`, `flush`, `process_exit`, `stop_select`, `emergency_close`) and `sync::thread_exit` are **safe** public fns taking the C `ErlDrvEntry`/pthread raw pointers, forwarded verbatim (pointer validity is the caller's contract, as in the C); the rationale sits in a module-scoped attribute there and a function-scoped one on `thread_exit`, since `unsafe fn` would break the frozen ABI-mirroring API `beam_rt`/`beam_io` consume. `cargo clippy -p beam_abi --all-targets --no-deps` is error-free.
- **Nullability:** C fn-pointer fields are `Option<unsafe extern "C" fn …>` (null-pointer optimized, same size/align as a raw pointer); data pointers stay raw `*mut T`/`*const T`.
- **Opaque handles** are `#[repr(C)] pub struct X { _private: [u8; 0] }` (incomplete C type), used behind pointers — never by value, never dereferenced in this crate.
- Files target ~1000 lines, split by responsibility; `nif::api`/`driver::api` are legitimately large **generated tables** (see their top-of-file notes), and `src/nif/enif/tests.rs`, `src/sync/tests.rs`, `tests/common/mod.rs` are test-only modules.

## Design Decisions
- **`ERL_NIF_TERM` is a `#[repr(transparent)]` newtype over `usize`, conversions *safe*.** The header defines it as `unsigned long` (`ErlNapiUInt`), bit-identical to `usize` on LP64; the seam is `Eterm::raw()`/`Eterm::from_raw()` (`const fn`) rather than `mem::transmute`, so the mapping is total, safe and explicit, and a layout test pins size/align equality to `usize` and `Eterm` (A.3: the internal term is exactly the NIF term).
- **Flag sets are `#[repr(transparent)]` newtypes, not `#[repr(C)]` enums.** `ErlNifResourceFlags` (`CREATE|TAKEOVER`), `ErlNifUniqueInteger`, `ErlNifSelectFlags`, `ErlNifDirtyTaskFlags`, `ErlDirtyJobFlags`, `ErlNifBinaryToTerm` are bit-wise OR-combined, which a field-less Rust enum cannot hold without UB; a `c_int`/`c_uint` newtype is ABI-identical (4 bytes, asserted) and carries the named constants as associated `const`s. *Pure* enumerations keep the `#[repr(C)]` form.
- **`va_list` parameters are modelled as `*mut VaListTag`** (opaque marker): no stable Rust `va_list`, and a `va_list` *parameter* is passed indirectly on the supported ABIs, so a pointer-sized opaque type is ABI-correct for `enif_vfprintf`/`enif_vsnprintf` (the only two affected entries).
- **`ErlNifApi`/`ErlDrvApi` are declared tables, not C structs.** On Linux the headers expose these as *exported free functions* resolved via `dlsym` (only Windows wraps them in `TWinDynNifCallbacks`); `#[repr(C)]` fn-pointer tables are the Rust export/consume shape and make the frozen signature list explicit and compile-checked.
- **The C-variadic `enif_*` slots live in a small C shim** — `src/nif/CONTEXT.md` (*The C-variadic shim*).
- **Single definitions, no duplication:** `ErlDrvSysInfo` is the one definition, `ErlNifSysInfo` a `pub use` alias (the header's `typedef ErlDrvSysInfo ErlNifSysInfo;`); `SysIOVec` aliases `libc::iovec` (the header uses `struct iovec` from `<sys/uio.h>`) for `writev`/`readv` compatibility, layout-asserted at size 16 / offsets 0, 8.
- **A driver binary carries a hidden refcount header, exactly as ERTS does.** The frozen `ErlDrvBinary` is only `{ orig_size, orig_bytes }`, but real ERTS reaches the count through an emulator-internal `Binary` header *preceding* it (`Binary2ErlDrvBinary`, `ErlDrvBinary2Binary`, `erl_binary.h:173-176`), so `driver_alloc_binary` allocates an 8-byte `AtomicI64` refcount word before the public struct (`DRIVER_BINARY_PREFIX == 16`), keeping the returned pointer 8-aligned (`ERTS_CHK_BIN_ALIGNMENT`) with the frozen layout untouched. Alloc refc = 1; `inc`/`dec` return the NEW value and `dec` never frees (only `driver_free_binary` releases, at zero) — oracle-pinned by `tests/driver_binary_refc.rs`.
- **Target = LP64 only.** The Windows-specific layout branches (`TWinDynNifCallbacks`, `ERL_SYS_DRV` `start` signature, `WinDynNifCallbacks`) are intentionally not modelled (the layout asserts would fail on a non-LP64 target).
- **No shared low-level `dlopen` wrapper — every dynamic-loading site calls `libc::dl*` directly.** `nif::loader` and `driver::loader` each own a **private** `dl_last_error`/`dl_sym`/`OpenHandle` trio (deliberate mirrors, `fn` not `pub fn`); the frozen slots forward to `libc` too — `driver_dl_open`/`_sym`/`_close`/`_error` (`driver/drv/mem.rs`), `enif_dlopen`/`enif_dlsym` (`nif/enif/time.rs`, `dlsym_checked`, `last_dl_error`). No `ffi`/`sys`/dynlib module and no *public* Rust `dlopen` helper; only `beam_rt` consumes the safe loaders (`port::driver::load_driver`, `driver::loader::load`, `NifBridge`, `nif::loader::load`).
- **`NifEnv`/`DriverEnv` erase their host borrow's lifetime at construction.** `new(&mut dyn NifHost)`/`new(&mut dyn DriverHost)` must keep the spec's signature, but the C-facing env has no lifetime parameter — a raw pointer to a trait object is invariant and defaults to `'static` (rust#141402) — so `new` does one documented `mem::transmute` of the layout-identical borrow (only the erased lifetime is reinterpreted); `host` reproduces the caller-owned borrow exactly, and "keep the host alive while the env is used" is the same caller contract as `from_opaque`/`from_port`. Neither env type is `Clone`.
- **The created-thread name registry is first-wins.** `thread_create` and the new thread's own `thread_start` both register the same tid's name via `register_thread_name`, which keeps the first entry and never replaces it, so a buffer handed out by `thread_name` is never freed early.

## Known gaps (cross-cutting — the per-family ones are in the child nodes)
- **Header names deliberately not declared** (audited against the three headers): `DO_READ`/`DO_WRITE` (deprecated aliases of `ERL_DRV_READ`/`ERL_DRV_WRITE`), `ERL_DRV_ERROR_{GENERAL,ERRNO,BADARG}_INT_` (the pointer-cast form is declared instead, `layout.rs:595`), `ERL_NAPI_SINT64_{MAX,MIN}__` (only `ERTS_NAPI_TIME_ERROR = i64::MIN` is declared), and the C macro machinery `ERL_NIF_INIT*` / `ERL_NAPI_ATTR_*` / `ERL_NAPI_EXPORT` / `ERL_NIF_API_FUNC_{DECL,MACRO}` (no Rust analogue). The `enif_make_{tuple,list}1..9`, `enif_make_pid`, `enif_compare_pids` and `enif_select_{read,write,error}` macros expand onto declared slots, so they need no extra symbol.
- Malformed-term guard: a bare `THE_NON_VALUE` / `HEADER`-tagged word is rejected at the term-inspecting entry points (the C would deref it); a *forged* pointer-tagged word is still undetectable from the word alone, exactly as in the C.

## Test Strategy
- `tests/layout.rs` — the runtime mirror of the `src/layout.rs` freeze: every size/align/offset of the Frozen layout table, the `ErlNifMapIterator` **union interior** (`ErlNifMapIteratorUnion`/`Flat`/`Hash`), every scalar-typedef width, every opaque handle, and the enum-discriminant / flag-bit / versioned-constant surface (`ERL_NIF_*`, `ERL_DRV_*`, `PORT_*`, `ERTS_NAPI_*`). `tests/term_abi.rs` — `size_of::<ERL_NIF_TERM>() == size_of::<usize>() == size_of::<Eterm>()`, matching `align_of`, and `nif_from_eterm(eterm_from_nif(t)) == t` over boundary raw values (`NIL`, `make_small(0)`, max small, atom tag) — the A.3 guarantee. `tests/api_surface.rs` — `size_of::<ErlNifApi>() == 176 * 8` / `size_of::<ErlDrvApi>() == 111 * 8` + an all-`None` table through `size_of_val`. `tests/entry.rs` — an `ErlNifFunc`/`ErlNifEntry`/`ErlDrvEntry` (the `nif_init` shape, null callbacks) read back via `major`/`minor`/`num_of_funcs`/`vm_variant`/`options` and `offset_of!` field order.
- `tests/host_seam.rs` + `tests/common/mod.rs` — the host seam through the **public** API; `common/` also compiles a trivial `erl_nif.h`-including probe `.so` (`cc -shared -fPIC`) — its helper list is in the Routing Table.
- **Per-family end-to-end tests** (`tests/nif_*.rs`/`driver_*.rs`) — each compiles a real C fixture at test time with the system `cc` against the system OTP headers and `dlopen`s it (`RTLD_NOW`) through the crate's own loader, so a single unresolved `enif_*`/`driver_*` import fails the load; `nif_variadic.rs` is gated on `cfg(beam_abi_variadic_shim)`, `driver_binary_refc.rs` uses a dual-`driver_init`/`nif_init` fixture shared with the oracle run, and `nif_threads.rs`/`driver_threads.rs` carry the extended `*_name`-matrix (`"unknown"`) cases.
- **`tests/abi_conformance.rs` — the mechanical gap guard.** Parses the real headers at test time (`erl_nif_api_funcs.h`'s `ERL_NIF_API_FUNC_DECL` list, `erl_driver.h`'s `EXTERN` declarations) with the headers' own gating (the four `#if SIZEOF_LONG != 8` `enif_*64` entries do not exist on LP64; the `erl_drv_steal_main_thread*` pair is `__DARWIN__`-only) and asserts **set equality** against the crate's `#[no_mangle]` definitions plus `csrc/nif_variadic.c`, that `ErlNifApi`/`ErlDrvApi` **field order and counts equal the header declaration order** (a transposed slot is an ABI break), and that every word of `enif_api_table()`/`driver_api_table()` is populated; a header declaration with no crate definition FAILS, and `KNOWN_UNIMPLEMENTED` is an explicit (currently empty) allowlist. Headers come from the **in-tree legacy** `erts/emulator/beam/` (`beam_dir()` = `CARGO_MANIFEST_DIR/../../../erts/emulator/beam`); absent ⇒ a loud `SKIP`, never a silent pass.
- **Oracle cross-checks:** the per-family end-to-end tests re-run the *same* fixture `.so` under the system Erlang (`erlang:load_nif/2` / `erl_ddll:load_driver/2`) and compare the printed values with the measured oracle constants; they skip loudly without `erl`/`erlc`/`cc`, and each node's case list is in `src/nif/CONTEXT.md` / `src/driver/CONTEXT.md`.
- **Loud `SKIP:`, never a silent pass:** `common::compile_so` prints a `SKIP:` banner and returns `None` when the C compiler or the OTP include directory is missing; every test then skips.
- In-module `#[cfg(test)]` unit tests: `src/nif/enif/{tests,process_tests,mapiter_tests,resources_tests}.rs`, `src/driver/drv/port_tests.rs` (both halves of the `output2` contract — see `src/driver/CONTEXT.md`), `src/sync/tests.rs`.
- Gate: `../../scripts/cargo.sh test -p beam_abi` and `../../scripts/cargo.sh build --workspace` GREEN (warning-free); behavioural cross-checking against real ERTS is `../../tools/differential`'s job (sibling — read-only, escalate writes to `..`).

## Handoffs (pointers — the detail is in `src/driver/CONTEXT.md`)
- **`beam_rt` / `beam_io` own the remaining `DriverHost`/`NifHost` hosts**: `DriverHost::port_term` (the `ERL_DRV_PORT` term-data operand) and `DriverHost::failure`/`port_exit` (the port-closing family) are unimplemented there, so a loaded C driver written the C way delivers nothing on that operand and its `driver_failure_*` closes no port — the `beam_abi` halves are complete and oracle-pinned.
- **A tools-owned ledger needs correcting**: `erts_rust/tools/differential/{src/corpus/driver_c.rs, GAPS.md, divergences.md}` still lists a closed `erl_errno_id` divergence and calls the port-closing reason "not a VM-stable observable". Reported here; the tools owner edits it.

## Routing Table
- `build.rs` → the build script: `-Wl,--export-dynamic` on the test targets (`.so`, `enif_*`, `driver_*`) **plus** compiling `csrc/nif_variadic.c` (`$CC`/`$AR`, no `cc` crate) and emitting `cfg(beam_abi_variadic_shim)` + `rustc-link-search`/`rustc-link-lib=static`
- `csrc/nif_variadic.c` → the C-variadic `enif_*` shim (the only C in the crate): the `va_list` handling of the seven variadic slots, forwarding into Rust (`src/nif/`)
- `src/lib.rs` → crate docs, module wiring, root re-exports (`NifEnv`, `NifHost`, `DriverEnv`, `DriverHost`, `Eterm`, `MonitorResult`, `AsyncJob`, `AsyncCallback`, `monitor_identity`, `monitor_from_identity`), the ABI **version constants** (`ERL_NIF_*`, `ERL_DRV_EXTENDED_*`, `ERL_NIF_VM_VARIANT`, `ERL_NIF_MIN_ERTS_VERSION`)
- `src/common.rs` → `erl_drv_nif.h` shared types: integer typedefs, `SysIOVec`, `ErlDrvSysInfo`, `ErlDrvThreadOpts`, `ErlDirtyJobFlags`, `ErlNifSelectFlags` + `ERL_NIF_SELECT_*`, `ErlDrvMonitor`, time constants
- `src/sync/` → the pthread facade (`Mutex`, `Cond`, `RwLock`, `TsdKey`, `Tid`, `mutex_*`, `cond_*`, `rwlock_*`, `tsd_*`, `thread_opts_*`, `thread_*`) both thread families forward to, with the C's `no_name` = `"unknown"` string (`no_name()`) and the created-unnamed-tid test (`thread_created_unnamed`), plus `tests.rs`
- `src/layout.rs` → the compile-time layout freeze (all `const _: () = assert!(...)` for the table above)
- `src/nif/` → **NIF host layer (contract A.3)** — `term.rs`/`types.rs`/`api.rs` (the frozen shapes), `host.rs` (`NifHost`/`NifEnv`), `enif/` (one file per `enif_*` family), `loader.rs`; see `src/nif/CONTEXT.md`
- `src/driver/` → **driver host layer (contract A.4)** — `types.rs`/`api.rs`, `host.rs` (`DriverHost`/`DriverEnv`), `current_host.rs`, `async_job.rs`, `drv/` (one file per family), `loader.rs`; see `src/driver/CONTEXT.md`
- `tests/` → crate-level integration tests (`layout.rs`, `term_abi.rs`, `api_surface.rs`, `abi_conformance.rs`, `entry.rs`) plus the per-family end-to-end tests (`tests/nif_*.rs`, `tests/driver_*.rs` — itemised in the two child routing tables) and `tests/fixtures/*.c` (real C NIFs/drivers, compiled at test time with the system `cc` against the system OTP headers); the crate-level ones pin the ABI rather than a family: the layout freeze (`layout.rs`), `size_of::<ERL_NIF_TERM>() == size_of::<Eterm>()` (`term_abi.rs`), the two table widths (`api_surface.rs`), the header-derivation set equality (`abi_conformance.rs`), and the `nif_init`/`driver_init` entry shapes (`entry.rs`)
- `tests/common/mod.rs` → shared test helpers (`mod common;`): `erl_include_dir`, `cc_available`, `compile_so`, `skip`, `TestHost`, `TestDriverHost`

## Notes for Agents
- **Read the legacy headers before touching a layout**: `erts/emulator/beam/{erl_nif.h,erl_nif_api_funcs.h,erl_drv_nif.h,erl_driver.h}` — field order and integer widths must match exactly, and the asserts are the enforcement. Cite the header line where a value is non-obvious.
- `erl_int_sizes_config.h` (included by `erl_drv_nif.h`) fixes `SIZEOF_LONG == 8` on this target, which is why the 4 `enif_*64` table entries are excluded from `ErlNifApi`.
- **A family's slots live in that family's file** — to extend a family, edit the family file's body and add the slot to its `populate`; do not touch the `ENIF_API`/`DRIVER_API` chain in `mod.rs`.
- Building: **never `cd`**; use `../../scripts/cargo.sh build -p beam_abi` / `../../scripts/cargo.sh test -p beam_abi` (the sandbox `$HOME` is read-only; the wrapper picks a writable `CARGO_HOME`).
- `tests/common/mod.rs` is invisible to `cargo test` unless some `tests/*.rs` says `mod common;` — many files do, so keep at least one includer (`tests/common/mod.rs`).
- A fixture must reference **only** the `enif_*`/`driver_*` slots the crate populates (`Some`), or `dlopen(RTLD_NOW)` fails on the unresolved import.
- Adding a `cargo::rustc-link-arg-bins` instruction to `build.rs` **breaks the build** (no `[[bin]]` target in this package).
- Ground truth for the two loaders is different C code: `erts_load_nif` in `erts/emulator/beam/erl_nif.c` for the NIF side (*`src/nif/CONTEXT.md`*) and `do_load_driver_entry`/`init_driver` for the driver side (*`src/driver/CONTEXT.md`*). Do not modify `erts/` or `tools/`.

## Status
- **Both host layers are implemented**: `ErlNifApi` **176/176** and `ErlDrvApi` **111/111** slots populated, every `enif_*`/`driver_*` family carrying a real body over a host seam, and both dynamic loaders validating an entry in the oracle's exact order.
- **Behavioural parity is pinned per family** by the `tests/nif_*.rs`/`tests/driver_*.rs` fixtures and their oracle cross-checks; each child node's *Known gaps* states what is still a typed failure rather than a wrong result.
- **The runtime halves are outstanding**: `beam_rt`/`beam_io` must implement the `DriverHost` hooks listed under *Handoffs* and `NifHost`'s remaining hooks (`src/nif/CONTEXT.md` → *Known gaps*).

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, layering waves (§0 places `beam_abi` in wave 0).
- `../../CONTEXT.md` → workspace contracts: §1 (A.3 NIF C ABI, A.4 driver C ABI), §2 frozen term layout, §3 error strategy.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.3/A.4; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §C (the frozen-header seam).
- `src/nif/CONTEXT.md` → the NIF host layer (A.3): `enif_*` families, `NifHost`/`NifEnv`, the NIF loader, the `erlang:load_nif/2` contract, the C-variadic shim.
- `src/driver/CONTEXT.md` → the driver host layer (A.4): `driver_*`/`erl_drv_*` families, `DriverHost`/`DriverEnv`, `AsyncJob`, the driver loader, the port-closing family, the Handoffs.
- `../beam_term/` → the internal `Eterm` this crate converts to/from (`nif::term`); `../beam_alloc/` → the production bump `Heap` the host seams hand out (`NifHost::heap`/`DriverHost::heap`).
- `../beam_rt/`, `../beam_bifs/`, `../beam_io/` → the runtime owners of the `NifHost`/`DriverHost` seams (`../beam_rt/src/nif.rs`, `../beam_rt/src/port/driver.rs`, `../beam_io/src/abi/host.rs`).
