# `beam_abi` — Layout-Frozen NIF & Driver C ABI

## Intent
Owns the **frozen foreign-function C ABIs** the Rust ERTS exposes to native code: the NIF C ABI
(part of workspace compatibility contract **A.3**) and the driver C ABI (**A.4**). It reproduces the
exact in-memory **struct layouts, enum widths, function-pointer signatures and constants** of the
legacy headers so that a NIF/driver `.so` or `.o` compiled against the *unmodified* OTP headers
links against Rust ERTS and behaves identically. This is the **link-time frozen-header seam** of
Part II §C — the struct layouts must never change; a mismatch here is an ABI break.

The crate freezes the *shape* of the boundary (the types, the layouts, the term transmute, the
function-pointer tables `ErlNifApi`/`ErlDrvApi`) **and implements both host sides**: the NIF host
layer (`nif::host`, the `enif_*` functions, the `dlopen`-based NIF loader) and the **driver host
layer** (`driver::host`, the `driver_*`/`erl_drv_*` functions in `driver::drv`, and the
`dlopen`-based driver loader in `driver::loader` — `dlsym "driver_init"` plus the oracle's
`erlang:load_driver/2` validation order). The remaining `enif_*` bodies (resources, threads, I/O
queues, selects, monitors, distribution, `priv_data`) and the driver port-queue/lifecycle/threads
surfaces belong to `beam_rt`/`beam_bifs`/`beam_io`.

Ground truth (read-only, never modify): `../../erts/emulator/beam/erl_nif.h`,
`erl_nif_api_funcs.h`, `erl_drv_nif.h`, `erl_driver.h`. These are the only stable ABI headers.
Per-OS `driver_int.h` and the Windows-specific paths are **out of scope**.

## ABI version targeted (record this — it is the compatibility claim)
- `ERL_NIF_MAJOR_VERSION` = **2**, `ERL_NIF_MINOR_VERSION` = **18** (`erl_nif.h`)
- `ERL_NIF_MIN_ERTS_VERSION` = `"erts-14.0"`, `ERL_NIF_VM_VARIANT` = `"beam.vanilla"`
- `ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD` = 2
- `ERL_DRV_EXTENDED_MARKER` = `0xfeeeeeed`, `ERL_DRV_EXTENDED_MAJOR_VERSION` = **3**,
  `ERL_DRV_EXTENDED_MINOR_VERSION` = **3**, `ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD` = 2
- Target platform: **64-bit LP64** (Linux x86-64 / AArch64): pointer/`size_t`/`unsigned long` = 8,
  `int` = 4. Every layout number below is for LP64.

## API Surface
### The term seam (`nif::term`)
- `pub struct ERL_NIF_TERM(usize)` — `#[repr(transparent)]`, `Copy + Clone + PartialEq + Eq + Hash +
  Debug + Default`. ABI-identical to `beam_term::Eterm` and to the C `ERL_NIF_TERM`
  (`unsigned long` on LP64, one machine word).
- `pub type ERL_NIF_UINT = ERL_NIF_TERM` (the header's alias).
- `pub const fn eterm_from_nif(t: ERL_NIF_TERM) -> Eterm` — NIF term → internal term.
- `pub const fn nif_from_eterm(t: Eterm) -> ERL_NIF_TERM` — internal term → NIF term.
- `pub const fn ERL_NIF_TERM::raw(self) -> usize` / `pub const fn ERL_NIF_TERM::from_raw(usize)`.
  Both conversions are **safe `const fn`s built on `Eterm::raw()`/`Eterm::from_raw()`** — no
  `mem::transmute` is used or needed (both types are transparent over `usize`; Level-1 requires the
  identity mapping, documented in the module).

### NIF types (`nif::types`) — `#[repr(C)]`, field order exactly as `erl_nif.h`
`ErlNifFunc`, `ErlNifEntry`, `ErlNifBinary`, `ErlNifPid`, `ErlNifPort`, `ErlNifResourceTypeInit`,
`ErlNifMapIterator` (+ `ErlNifMapIteratorFlat`/`ErlNifMapIteratorHash`/`ErlNifMapIteratorUnion`),
`ErlNifIOVec`, `ErlNifSysInfo` (= `ErlDrvSysInfo`); opaque handles `ErlNifEnv`,
`ErlNifResourceType`, `ErlNifIOQueue`, `ErlNifTid`, `ErlNifMutex`, `ErlNifCond`, `ErlNifRWLock`;
scalars `ErlNifUInt64`/`ErlNifSInt64`/`ErlNifUInt`/`ErlNifSInt`, `ErlNifTime`, `ErlNifEvent`,
`ErlNifTSDKey`, `ErlNifThreadOpts`, `ErlNifMonitor`; function types `ErlNifResourceDtor`/`Stop`/
`Down`/`DynCall`, `ErlNifOnHaltCallback`, `ErlNifOnUnloadThreadCallback`; enums `ErlNifCharEncoding`,
`ErlNifTimeUnit`, `ErlNifMapIteratorEntry`, `ErlNifHash`, `ErlNifTermType`, `ErlNifOption`,
`ErlNifIOQueueOpts`; flag types `ErlNifResourceFlags`, `ErlNifSelectFlags`, `ErlNifUniqueInteger`,
`ErlNifBinaryToTerm`, `ErlNifDirtyTaskFlags`.
### `ErlNifApi` (`nif::api`) — the NIF function table (the frozen `enif_*` surface)
A `#[repr(C)]` struct of the **176** `enif_*` function pointers that exist on LP64, in exact
`erl_nif_api_funcs.h` declaration order. Derivation: the token `ERL_NIF_API_FUNC_DECL` occurs 185
times but only 180 are declarations (5 are guard/comment lines); the 4 `#if SIZEOF_LONG != 8`
entries (`enif_get_int64`/`enif_get_uint64`/`enif_make_int64`/`enif_make_uint64`) do not exist on
LP64 (the header `#define`s them onto their `long`/`ulong` siblings), so 180 − 4 = **176**; the
macro-only `enif_ioq_enq` (present in the `ERL_NIF_API_FUNC_MACRO` list only, with no
`DECL`/signature/definition) is excluded. This is the Rust analogue of the header's Windows-only
`TWinDynNifCallbacks` and defines the surface the runtime must implement.
`size_of::<ErlNifApi>() == 176 * 8 == 1408` is asserted at compile time.

### Driver types (`driver::types`) — `#[repr(C)]`, exactly as `erl_driver.h`
`ErlDrvEntry`, `ErlDrvBinary`, `ErlIOVec`, `ErlDrvNowData`; opaque handles `ErlDrvPort`,
`ErlDrvEvent`, `ErlDrvData`, `ErlDrvThreadData`, `ErlDrvPDL`, `ErlDrvTid`/`ErlDrvMutex`/`ErlDrvCond`/
`ErlDrvRWLock` (+ `_` marker structs); scalars `ErlDrvUInt64`/`ErlDrvSInt64`/`ErlDrvUInt`/`ErlDrvSInt`,
`ErlDrvTermData`, `ErlDrvSizeT`, `ErlDrvSSizeT`, `ErlDrvTime`, `ErlDrvTSDKey`; enum `ErlDrvTimeUnit`;
flag newtype `ErlDirtyJobFlags`; constants `ERL_DRV_*` (term-data tags, `ERL_DRV_FLAG_*`,
`PORT_*`, `ERL_DRV_READ/WRITE/USE*`, `DRIVER_CALL_KEEP_BUFFER`, `ERL_DRV_ERROR_*`,
`ERL_DRV_BUSY_MSGQ_*`).
### `ErlDrvApi` (`driver::api`) — the driver function table (the frozen `driver_*`/`erl_drv_*` surface)
A `#[repr(C)]` struct of function pointers for every function the *runtime provides* to drivers
(`driver_output`, `driver_select`, `erl_drv_mutex_*`, `erl_drv_thread_*`, `driver_alloc*`,
`driver_enq*`, monitor/PDL/term-data functions, …). The legacy header exposes these as exported
free functions; bundling them into one table is the Rust runtime's export shape (documented
deviation, see Design Decisions). Fields are `Option<unsafe extern "C" fn ...>` (null-optimized) in
`erl_driver.h` order.
### Shared (`common`) — `erl_drv_nif.h`
`ErlNapiUInt64`/`ErlNapiSInt64`/`ErlNapiUInt`/`ErlNapiSInt`, `SysIOVec`, `ErlDrvSysInfo`,
`ErlDrvThreadOpts`, `ErlDirtyJobFlags`, `ErlNifSelectFlags`, `ErlDrvMonitor`, the `ERTS_NAPI_*` time
constants, and `ERL_NIF_SELECT_*` / `ErlNifSelectFlags` selector bits.
### The host seam (`nif::host`) — the runtime services behind the `enif_*` layer
Not part of the C ABI; it is the *internal* boundary the future `enif_*` implementations are written
against (so this crate keeps its "declaration layer" role and pulls in no ETF/scheduler code).
- `pub trait NifHost` — `heap() -> &mut beam_alloc::Heap`, `intern_atom(&str) -> Option<Eterm>`,
  `atom_name(Eterm) -> Option<String>`, plus **defaulted** `self_pid`, `term_to_binary`,
  `binary_to_term`, `make_ref` (all `None` = unsupported). The ETF pair exists so `beam_abi` needs
  **no** `beam_codec` dependency.
- `pub struct NifEnv` — the concrete per-call environment behind the C-facing opaque
  `*mut ErlNifEnv`: `new(&mut dyn NifHost)`, `host()`, `heap()`, `as_ptr()`,
  `unsafe from_opaque(*mut ErlNifEnv) -> Option<&mut NifEnv>`, and the pending-exception slot
  (`pending_exception`/`set_exception`/`clear`, the C `enif_raise_exception`/`enif_clear_env`
  semantics). Re-exported at the crate root as `beam_abi::{NifEnv, NifHost}`.
- `nif::enif` implements the `enif_*` host functions over this seam; `nif::loader` implements the
  dynamic NIF loader / `ErlNifEntry` load contract. See **"NIF host layer (implemented)"** below.
### The driver host seam (`driver::host`) — the runtime services behind the `driver_*` layer
Not part of the C ABI; the *internal* boundary the `driver_*`/`erl_drv_*` implementations are written
against (so this crate pulls in no scheduler/poller/timer code).
- `pub trait DriverHost` — `heap() -> &mut beam_alloc::Heap`, `intern_atom(&str) -> Option<Eterm>`,
  and the **required** `output(port, &[u8]) -> c_int` / `send_term(port, Option<Eterm>, Eterm) ->
  c_int`, plus **defaulted** `select`/`set_timer`/`cancel_timer`/`read_timer`/`r#async`/
  `connected_pid`/`caller_pid` (the defaults report "unsupported" as `-1`/`None`).
- `pub struct DriverEnv` — the concrete per-port environment behind the C-facing opaque
  `*mut ErlDrvPort_`: `new(&mut dyn DriverHost)`, `host()`, `heap()`, `as_port()`,
  `unsafe from_port(*mut ErlDrvPort_) -> Option<&mut DriverEnv>`. **The `ErlDrvPort` a runtime passes
  to a driver callback *is* the `DriverEnv::as_port` pointer** (the analogue of `NifEnv`).
- `pub fn decode_term_spec(&mut DriverEnv, &[ErlDrvTermData]) -> Option<Eterm>` — the reverse-Polish
  term-data decoder behind `erl_drv_output_term`/`erl_drv_send_term`.
- `driver::drv` implements the `driver_*` host functions and publishes them through
  `driver_api_table()`; `driver::loader` implements the dynamic driver loader / `ErlDrvEntry` load
  contract. See **"Driver host layer (implemented)"** below.

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
| `ErlDrvThreadOpts` | 4 | 4 | suggested_stack_size 0 |
| `ErlDrvMonitor` | 32 | 1 | data 0 |
| `ErlDrvBinary` | 16 | 8 | orig_size 0, orig_bytes 8 |
| `ErlIOVec` | 32 | 8 | vsize 0, size 8, iov 16, binv 24 |
| `ErlDrvEntry` | 176 | 8 | init 0, start 8, stop 16, output 24, ready_input 32, ready_output 40, driver_name 48, finish 56, handle 64, control 72, timeout 80, outputv 88, ready_async 96, flush 104, call 112, unused_event_callback 120, extended_marker 128, major_version 132, minor_version 136, driver_flags 140, handle2 144, process_exit 152, stop_select 160, emergency_close 168 |
| `ErlDrvNowData` | 24 | 8 | megasecs 0, secs 8, microsecs 16 |
| `SysIOVec` (= `libc::iovec`) | 16 | 8 | iov_base 0, iov_len 8 |

All C enum-with-values types (`ErlNifCharEncoding`, `ErlNifTimeUnit`, `ErlNifMapIteratorEntry`,
`ErlNifHash`, `ErlNifTermType`, `ErlNifOption`, `ErlNifIOQueueOpts`, `ErlDrvTimeUnit`) are 4-byte
`#[repr(C)]` enums (asserted). Flag types are 4-byte `#[repr(transparent)]` newtypes over
`c_int`/`c_uint` (asserted).

## Constraints
- **Layout is FROZEN.** Every `#[repr(C)]` struct/enum and every constant must match the legacy
  header. Changes require a `../../../CHANGE.md` entry and a note here.
- **Compile-time asserts are mandatory** for every type in the table above, expressed as
  `const _: () = assert!(core::mem::size_of::<T>() == N);` and
  `const _: () = assert!(core::mem::offset_of!(T, f) == N);` (both `core::mem` items are const and
  stable on the 1.80 toolchain — no external `static_assertions` dependency is added). A mismatch is
  a **compile error**, not a test failure.
- **Dependencies:** `beam_common`, `beam_term`, **`beam_alloc`** (wave-0 sibling: the production bump
  heap the host seam hands out via `NifHost::heap`), `libc`. No external crate beyond `libc`
  (deliberately: **no `beam_codec`** — ETF goes through `NifHost`). `#[repr(transparent)]` on the
  term aliases; `#[repr(C)]` on every ABI struct and enum-with-values.
- **`build.rs` exists solely to link the test/bin links with `-Wl,--export-dynamic`**, so a loaded NIF
  `.so` can resolve the runtime's `enif_*` dynamic symbols (see Design Decisions).
- **`unsafe` discipline:** this crate declares FFI types, so `unsafe extern "C" fn` appears
  throughout; every such declaration is documented, and every `unsafe` block (term/pointer
  transmutes, the `NifEnv` opaque-pointer recover path) carries a preceding `// SAFETY:` rationale.
  `unsafe_op_in_unsafe_fn` stays clean (the deref in `NifEnv::from_opaque` is in an explicit block).
  Function *pointers* are declared but never called here — no call sites exist in this crate.
- **Nullability:** C function-pointer fields are `Option<unsafe extern "C" fn …>` (null-pointer
  optimized, same size/align as a raw pointer). Data pointers stay raw `*mut T`/`*const T`.
- **Opaque handles** are `#[repr(C)] pub struct X { _private: [u8; 0] }` (incomplete C type), used
  behind pointers — never by value, never dereferenced in this crate.
- Files target ~1000 lines, split by responsibility; `nif::api` is a legitimately large
  **generated table** (see its top-of-file note).

## Design Decisions
- **`ERL_NIF_TERM` is a `#[repr(transparent)]` newtype over `usize`, and conversions are *safe*.** The
  header defines it as `unsigned long` (`ErlNapiUInt`), which on LP64 is bit-identical to `usize`.
  Rather than `mem::transmute`, the seam is `Eterm::raw()`/`Eterm::from_raw()` (both `const fn`), so
  the mapping is total, safe, and explicit; a layout test pins the size/align equality to `usize`
  and to `Eterm`. (Requirement A.3: the internal term is exactly the NIF term.)
- **Flag sets are `#[repr(transparent)]` newtypes, not `#[repr(C)]` enums.** The C types
  `ErlNifResourceFlags` (`CREATE|TAKEOVER`), `ErlNifUniqueInteger`, `ErlNifSelectFlags`,
  `ErlNifDirtyTaskFlags`, `ErlDirtyJobFlags`, `ErlNifBinaryToTerm` are bit-wise OR-combined by
  callers; a Rust field-less enum cannot hold such a value without UB. A `c_int`/`c_uint` newtype is
  ABI-identical (4 bytes, asserted) and carries the named constants as associated `const`s. *Pure*
  enumerations (never combined) keep the `#[repr(C)]` enum form the rule requires.
- **`va_list` parameters are modelled as `*mut VaListTag`** (an opaque marker). Rust has no stable
  `va_list`; on the supported ABIs a `va_list` *parameter* is passed indirectly (pointer to the list
  state), so a pointer-sized opaque type is ABI-correct for `enif_vfprintf`/`enif_vsnprintf`. Only
  those two table entries are affected; documented at the definition site.
- **`ErlNifApi` / `ErlDrvApi` are declared tables, not C structs.** On Linux the headers expose these
  as *exported free functions* resolved via `dlsym` (only Windows wraps them in the
  `TWinDynNifCallbacks` struct). Modelling them as `#[repr(C)]` function-pointer tables is how the
  Rust runtime will export/consume the surface, and it makes the frozen signature list explicit and
  compile-checked. `ErlNifApi` is a faithful 176-entry LP64 mirror of the `erl_nif_api_funcs.h`
  declaration order;
  `ErlDrvApi` mirrors the `erl_driver.h` free-function list.
- **`ErlDrvSysInfo` is the single definition; `ErlNifSysInfo` is a `pub use` alias** (the header does
  `typedef ErlDrvSysInfo ErlNifSysInfo;`), preventing duplication.
- **`SysIOVec` aliases `libc::iovec`** on Unix (the header uses `struct iovec` from `<sys/uio.h>`),
  guaranteeing `writev`/`readv` compatibility; a layout assert pins size 16 / offsets 0, 8.
- **Target = LP64 only.** Windows-specific layout branches (`TWinDynNifCallbacks`, `ERL_SYS_DRV`
  `start` signature, `WinDynNifCallbacks`) are intentionally not modelled; the layout asserts make
  this explicit (they would fail on a non-LP64 target).
- **`NifEnv` erases its host borrow's lifetime at construction.** `NifEnv::new(&mut dyn NifHost)` must
  keep the spec's signature, but the C-facing env has no lifetime parameter — and `host as *mut dyn
  NifHost` is rejected because a raw pointer to a trait object is invariant and defaults to `'static`
  (raw-pointer *casts* may not extend a trait-object lifetime either, rust#141402). So `new` does one
  documented `mem::transmute` of the layout-identical `&mut dyn NifHost` → `*mut dyn NifHost` (only
  the erased lifetime is reinterpreted); the caller-owned borrow is reproduced exactly by `host`, and
  the safety invariant ("keep the host alive while the env is used") is discharged by the same caller
  contract as `from_opaque`. `NifEnv` is not `Clone`, so there is exactly one handle.
- **`DriverEnv` erases its host borrow's lifetime the same way.** `DriverEnv::new(&mut dyn
  DriverHost)` does one documented `mem::transmute` of the layout-identical borrow (only the erased
  lifetime is reinterpreted, rust#141402), because the C-facing `*mut ErlDrvPort_` has no lifetime
  parameter; `DriverEnv::host` reproduces the caller's borrow and `DriverEnv::from_port` recovers the
  env from the opaque port pointer under the same "keep the host alive" contract. `DriverEnv` is not
  `Clone`, so a port has exactly one environment.
- **The test link exports its dynamic symbols (`build.rs`).** A NIF `.so` resolves `enif_*` and a
  driver `.so` resolves `driver_*` at load time out of the loading process, so the test targets are
  linked with `-Wl,--export-dynamic` (`cargo::rustc-link-arg-tests`). The `*-bins` counterpart is
  **not** emitted: this package has no `[[bin]]` target and Cargo hard-errors on a `*-bins`
  instruction when there is none.

## Test Strategy
- `tests/layout.rs` — re-asserts every size/align/offset from the frozen table at **runtime** (so a
  mismatch also fails the suite with a readable message, complementing the compile-time asserts):
  all struct sizes, `ErlNifEntry`/`ErlDrvEntry`/`ErlNifIOVec`/`ErlNifMapIterator` field offsets, and
  the 4-byte width of every enum/flag type.
- `tests/term_abi.rs` — `size_of::<ERL_NIF_TERM>() == size_of::<usize>() == size_of::<Eterm>()`,
  matching `align_of`, and `nif_from_eterm(eterm_from_nif(t)) == t` round-trips over boundary raw
  values (`NIL`, `make_small(0)`, max small, atom tag) — the A.3 guarantee.
- `tests/api_surface.rs` — `size_of::<ErlNifApi>() == 176 * 8` and `size_of::<ErlDrvApi>() == 111 * 8`
  (every column is one pointer) — the strongest available freeze of the table shape — plus a smoke
  test that an all-`None` table round-trips through `size_of_val`.
- `tests/entry.rs` — build an `ErlNifFunc`/`ErlNifEntry` (the `nif_init` shape) with null callbacks,
  read back `major`/`minor`/`num_of_funcs`/`vm_variant`/`options`, and confirm the `#[repr(C)]` field
  order via `offset_of!` on a constructed value; likewise a `ErlDrvEntry` with all-null callbacks.
- `tests/host_seam.rs` + `tests/common/mod.rs` — the host seam through the **public** API. `common/`
  lives in a *subdirectory* so Cargo does not treat it as an integration-test target; test files pull
  it in with `mod common;` (it starts with `#![allow(dead_code)]`, the same convention as
  `beam_ets/tests/common/mod.rs`). It provides `erl_include_dir()`, `cc_available()`, `compile_so()`,
  `skip()`, a `TestHost` (`beam_alloc::Heap` + `beam_term::atom::AtomTable`) implementing `NifHost`,
  and a `TestDriverHost` (same arena + atom table) implementing `DriverHost`, recording every
  output/term/select/timer/async call with accessors for assertions. `host_seam.rs` also compiles a
  trivial `erl_nif.h`-including probe `.so`, proving the include-directory resolution and the
  `cc -shared -fPIC` invocation work. **Skip loudly, never fail silently**: a missing C compiler or
  OTP include directory prints a `SKIP:` banner and returns.
- `tests/nif_end_to_end.rs` + `tests/fixtures/test_nif.c` — a **real C NIF** compiled at test time
  with the system `cc` against the system `erl_nif.h`: it is `dlopen`ed and its NIFs are called, its
  `enif_*` imports resolving out of the test binary; plus the loader's `bad_lib` rejection paths.
- `tests/driver_end_to_end.rs` + `tests/fixtures/test_driver.c` — a **real C driver** (contract A.4)
  compiled the same way against the system `erl_driver.h`: the fixture defines a `DRIVER_INIT(t)`
  entry and drives `driver_alloc`/`driver_free`/`driver_output`/`driver_set_timer`/
  `driver_cancel_timer`/`driver_select`/`driver_async`/`driver_mk_atom`/`driver_mk_port`/
  `erl_drv_output_term`/`driver_system_info`. The test loads it, asserts the validated entry fields,
  runs `start`, then drives `output` (the bytes must reach the recording host through the `.so`'s
  unresolved `driver_output`), `control` (commands 0–7: the `min(len, rlen)` echo, the timer/select/
  cancel/async host traffic, the `{t, 1}` term-data round-trip and the `driver_system_info` major
  version), `timeout` and `stop`; it also checks the `.so`'s imports are undefined and the test
  binary exports them. Three negative tests pin the loader's typed failures
  (`no_driver_init`/`bad_driver_name`/`driver_incorrect_version`) plus a missing object
  (`unspecified_error`) — never a crash.
- In-module `#[cfg(test)]` unit tests cover the seam's unsafe surface: the
  `as_ptr` → `from_opaque` round-trip (a set exception survives), `from_opaque(null) == None`,
  `clear()`, the `as_port` → `from_port` round-trip, and `decode_term_spec` over the tag set.
- Gate: `../scripts/cargo.sh test -p beam_abi` GREEN and `../scripts/cargo.sh build --workspace`
  GREEN (warning-free). Cross-checking behaviour against real ERTS is `../../tools/differential`'s job
  (sibling — read-only, escalate writes to `..`).

## Oracle behavior: `erlang:load_nif/2` load-time contract
Ground truth `../../erts/emulator/beam/erl_nif.c` (`erts_load_nif`, line 4729) — the checks that validate an `ErlNifEntry` (the very fields this crate declares), to be reproduced by the runtime. Verified empirically against the system OTP 29.1.1 / erts-17.1 (scratch NIF in `$TMPDIR`, read-only C).
- **Not a plain BIF.** Registered in `bif.tab:527` as `erlang:load_nif/2`; the compiler rewrites a `call_ext` to it into the loader-only instruction `i_load_nif` (`emu/ops.tab:854-865`, JIT `jit/x86/ops.tab:561-569`). The BIF body `load_nif_2` (`erl_nif.c:4643`) is only a stub rejecting `apply(erlang, load_nif, ...)`: `{error,{bad_lib,"load_nif/2 must be explicitly called from the NIF module. It cannot be called through apply/3."}}`. **No `erts_internal` variant exists.**
- **Calling module derivation** (`erl_nif.c:4764-4770`): `caller = erts_find_function_from_pc(I)` where `I` is the PC of the `i_load_nif` instruction, then `mod_atom = caller->module` — a per-code-range binary search over the module's `BeamCodeHeader` function table (`beam_ranges.c:245-305`), *not* a stack-frame scan. The module is whichever module's code contains the call site.
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
- **`check_is_nifable`** (`erl_nif.c:4721`): `!mod_code->are_nifs || mod_code->are_nifs[func_ix]`; `are_nifs` is built from the `op_nif_start` opcode (`emu_load.c:1414`). Compiler rule (`v3_core.erl:335-339`): a module that calls `load_nif/2` gets `nif_start` inserted into **every** function **unless** it has an explicit `-nifs([...])` attribute (then only the listed functions are marked). So check #10 is reachable only via `-nifs`.
- **Version constants** (`erl_nif.h:67-91`): major 2 / minor 18 / min-required-major-on-load 2 / `ERL_NIF_MIN_ERTS_VERSION` `"erts-14.0"` / `ERL_NIF_VM_VARIANT` `"beam.vanilla"`. `AT_LEAST_VERSION` (`erl_nif.c:4580`) compares `major*0x100+minor`; `create_lib` (`erl_nif.c:4586`) makes a "modern" copy defaulting absent fields (`vm_variant`→`"beam.vanilla"`, `min_erts`→`"erts-?"`, `sizeof_ErlNifResourceTypeInit`→0, `options`→0, `funcs[].flags`→0).
- Empirical raw outputs (module `m`, NIF entry name `m`): success `ok`; missing path → `{error,{load_failed,"Failed to load NIF library: '.../nope.so: cannot open shared object file: No such file or directory'"}}`; text file → `{error,{load_failed,"Failed to load NIF library: '.../nota.so: file too short'"}}`; major 99 → `{error,{bad_lib,"That 'm' NIF library needs erts-99.0 or newer. ..."}}`; no `nif_init` → `{error,{bad_lib,"Failed to find library init function: '.../emptysym.so: undefined symbol: _nif_init'"}}`; reload → `{error,{reload,"NIF library already loaded (reload disallowed since OTP 20)."}}`; `-on_load` calling `load_nif` succeeds and the module's NIF is callable; `apply/3` → the stub error. Return type is the atom `ok` / `{error,_}`; callable from `-on_load/0` (must return `ok`) or an ordinary function.
- (Note: in this Nix-packaged runtime `code:lib_dir(erts,include)` points at a non-existent path; the real header is `.../lib/erlang/erts-17.1/include/erl_nif.h`.)

## NIF host layer (implemented)
### `enif_*` functions (`src/nif/enif.rs`)
Every one is `#[no_mangle] pub unsafe extern "C"` with the exact signature of its matching `nif::api`
field. Implemented:
- **allocation** `enif_alloc`/`enif_free`/`enif_realloc` (libc-backed), `enif_alloc_binary`/`enif_realloc_binary`/`enif_release_binary`;
- **binaries/ETF** `enif_inspect_binary`, `enif_inspect_iolist_as_binary`, `enif_make_new_binary`, `enif_make_binary`, `enif_make_sub_binary`, `enif_term_to_binary`, `enif_binary_to_term`;
- **accessors** `enif_get_int/uint/long/ulong/double`, `enif_get_atom`, `enif_get_atom_length`, `enif_get_string`, `enif_get_string_length`, `enif_get_tuple`, `enif_get_list_cell`, `enif_get_list_length`, `enif_get_map_size`, `enif_get_map_value`;
- **predicates/compare** `enif_is_atom/binary/ref/fun/pid/port/empty_list/list/tuple/map/number/exception`, `enif_is_identical`, `enif_compare`, `enif_term_type`, `enif_term_size`, `enif_hash`, `enif_get_atom_cache_index`, `enif_max_atom_cache_index`;
- **constructors** `enif_make_int/uint/long/ulong/double`, `enif_make_atom(_len)`, `enif_make_new_atom(_len)`, `enif_make_tuple_from_array`, `enif_make_list_cell`, `enif_make_list_from_array`, `enif_make_reverse_list`, `enif_make_string(_len)`, `enif_make_ref`;
- **maps** `enif_make_new_map`, `enif_make_map_put`, `enif_make_map_update`, `enif_make_map_remove`, `enif_make_map_from_arrays`;
- **exceptions/misc** `enif_make_badarg`, `enif_raise_exception`, `enif_has_pending_exception`, `enif_clear_env`, `enif_self`, `enif_get_local_pid`, `enif_get_local_port`, `enif_set_pid_undefined`, `enif_is_pid_undefined`; plus the non-table LP64 aliases `enif_get_int64/uint64` + `enif_make_int64/uint64`.

`pub fn enif_api_table() -> &'static ErlNifApi` returns a `#[used] static` populated with `Some` for every implemented slot (built with `..ErlNifApi::empty()`, now a `const fn`). The `#[used]` static is what keeps the `#[no_mangle]` symbols in the final link — a test that calls `enif_api_table()` also forces the symbol-bearing codegen unit into the link.

**Intentionally `None`:** the C-variadic entries `enif_make_tuple`, `enif_make_list`, `enif_set_option`, `enif_fprintf`, `enif_snprintf`, `enif_vfprintf`, `enif_vsnprintf` (stable Rust 1.98 cannot *define* a C-variadic `fn`; the `_from_array` variants cover tuple/list building), plus the resource / thread / ioq / select / monitor / time / `enif_send` / `enif_schedule_nif` / `whereis_*` / `priv_data` / `alloc_env` / map-iterator / `make_existing_atom` surfaces (owned by subsystems not yet wired through the `NifHost` seam).

### Dynamic loader (`src/nif/loader.rs`)
`load(path, module, host, load_info) -> Result<NifLibrary, NifLoadError>` does `dlopen(path+".so", RTLD_NOW|RTLD_LOCAL)`, `dlsym("nif_init")` (fallback `_nif_init`), then validates in the oracle's exact order (the table above) and invokes `entry.load`. `NifLibrary::call(env, name, arity, args)` looks the function up by `(name, arity)`, calls `fptr(env.as_ptr(), arity, args.as_ptr())`, and turns a pending env exception into `NifCallError::Raised`. `Drop` runs `entry.unload` then `dlclose`. `Reload`/`Upgrade`/`OldCode` variants exist for surface completeness but `load()` never produces them (no module-instance / old-code state here).

### Symbol-export mechanism (load-bearing)
A NIF `.so` leaves `enif_*` and a driver `.so` leaves `driver_*`/`erl_drv_*` **undefined**, and the dynamic linker binds them, at `dlopen(RTLD_NOW)`, against the **loading process's** dynamic symbol table. `build.rs` emits `cargo::rustc-link-arg-tests=-Wl,--export-dynamic` so each `beam_abi` test binary exports its globals. Proven: `nm -D --undefined-only test_nif.so` lists the six used `enif_*` as `U` and `test_driver.so` lists its eleven `driver_*`/`erl_drv_*` as `U`; the same names appear `T` (defined) in the test binary's `.dynsym`; and loading the same `.so` from a process that does **not** export them fails with `undefined symbol: enif_make_int`. (`cargo::rustc-link-arg-bins` must NOT be emitted — this package has no `[[bin]]` and cargo hard-errors.)

### Known gaps
- `enif_make_map_*` / `enif_compare` / `enif_is_identical` / `enif_hash` take no env, hence no host/atom table: identical terms take a raw-word fast path and distinct **atoms** fall back to `beam_term`'s index order (exact for non-atom keys, the common NIF case).
- Integers: small ints + one-limb bignums; a wider value makes the accessor return `0` rather than truncate. `enif_make_binary` builds an **on-heap** binary (off-heap refc binaries are not exposed by the seam). `enif_inspect_binary` refuses a *bit*-offset base instead of fabricating an unaligned pointer.
- Malformed-term guard: a bare `THE_NON_VALUE` / `HEADER`-tagged word is rejected at the term-inspecting entry points (the C would deref it); a *forged* pointer-tagged word is still undetectable from the word alone, exactly as in the C.

## Driver host layer (implemented)
### `driver_*` / `erl_drv_*` functions (`src/driver/drv/{mod.rs, mem.rs, port.rs}`)
This is a directory module split along the C ABI's own seam: `mod.rs` holds the module docs, the shared `env_for` helper, the exported `driver_term_nil` data symbol and the `#[used] static DRIVER_API` table (so the signature check against the one frozen table lives there); `mem.rs` is part 1 (host-free) and `port.rs` is part 2 (port/host-dependent).
The port half's tests live in the sibling `port_tests.rs`, declared at the end of `port.rs` as `#[cfg(test)] #[path = "port_tests.rs"] mod tests;` — a path-included **child** of `port`, so `super::*` still reaches `port.rs`'s items and the tests exercise the `#[no_mangle]` functions directly.
It is a sibling file only so nothing in the directory grows past the workspace's ~1000-line target; `port_tests.rs` carries the bulk of the test traffic (port-pointer recovery, term-data round-trips through the populated table, the output/select/timer host calls).
Every one is `#[no_mangle] pub unsafe extern "C"` with the exact signature of its matching
`driver::api` field, and lands in the `#[used] static DRIVER_API` published through
`pub fn driver_api_table() -> &'static ErlDrvApi`.
Two groups:
- **host-free** — `driver_alloc`/`driver_free`/`driver_realloc`, `driver_alloc_binary`/
  `driver_realloc_binary`/`driver_free_binary`, `driver_system_info`, `null_func`, `erl_errno_id`,
  `erl_drv_consume_timeslice`, `erl_drv_monotonic_time`/`erl_drv_time_offset`/
  `erl_drv_convert_time_unit`, `driver_dl_open`/`driver_dl_sym`/`driver_dl_close`/`driver_dl_error`,
  `erl_drv_putenv`/`erl_drv_getenv`, `driver_mk_term_nil` (plus the `driver_term_nil` data symbol).
- **port/host-dependent** (the `ErlDrvPort` operand **is** the `DriverEnv::as_port` pointer, recovered
  by `DriverEnv::from_port`, and each delegates to the `DriverHost` seam) — the output family
  (`driver_output`/`driver_output2`/`driver_output_binary`/`driver_outputv`), the term-data family
  (`driver_output_term`/`driver_send_term`/`erl_drv_output_term`/`erl_drv_send_term`), the port
  queries (`driver_mk_atom`/`driver_mk_port`/`driver_connected`/`driver_caller`), and the
  select/timer/async group (`driver_select`/`driver_set_timer`/`driver_cancel_timer`/
  `driver_read_timer`/`driver_async`).
**Slots populated (`Some`): 37 of the 111** `ErlDrvApi` fields — exactly the functions listed above.
**Slots left `None`** (each needs runtime structure this seam does not expose): the port queues
(`driver_vec_to_buf` + the `driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq*`/
`driver_enqv`/`driver_pushqv` family), port lifecycle & attributes (`driver_create_port`,
`add_driver_entry`, `remove_driver_entry`, `driver_lock_driver`, `erl_drv_init_ack`,
`erl_drv_set_os_pid`, `erl_drv_busy_msgq_limits`, the `driver_failure*`/`driver_exit` family,
`set_busy_port`, `set_port_control_flags`, `get_port_flags`), the `driver_monitor_*` family and the
whole `driver_pdl_*` family, `driver_async_port_key`, the whole thread family
(`erl_drv_mutex_*`/`erl_drv_cond_*`/`erl_drv_rwlock_*`/`erl_drv_tsd_*`/`erl_drv_thread_*`/
`erl_drv_*_name`), the deprecated `driver_get_now`, and the binary refcount trio
(`driver_binary_get_refc`/`inc_refc`/`dec_refc`) — the frozen `ErlDrvBinary` is exactly
`{ orig_size, orig_bytes }` with **no refcount word**, so there is nothing to report or adjust.
`EMPTY_DRIVER_API` is `unsafe { core::mem::zeroed() }` — the const-evaluable, all-`None` base table
(`ErlDrvApi::empty()` is a plain `fn` and cannot initialise a `static`); every pointer field is
null-optimized, so the all-zero struct **is** the all-`None` table (a test pins that equality).

### Dynamic loader (`src/driver/loader.rs`)
`load(path, name) -> Result<DriverLibrary, DriverLoadError>` does `dlopen(path+".so", RTLD_NOW |
RTLD_LOCAL)`, `dlsym("driver_init")` (fallback `_driver_init`), then validates the returned
`ErlDrvEntry` in the oracle's exact order and runs its `init` callback — reproducing
`do_load_driver_entry` (`erts/emulator/beam/erl_bif_ddll.c:1466-1533`),
`erts_sys_ddll_load_driver_init` (`sys/unix/erl_unix_sys_ddll.c:216-228`) and `init_driver`
(`io.c:7559-7621`).
`DriverLibrary` exposes the entry (`entry()`), `name()`, `init()`, `finish()` and one wrapper per
callback slot (`start`/`control`/`call`/`output`/`outputv`/`timeout`/`ready_input`/`ready_output`/
`ready_async`/`flush`/`process_exit`/`stop_select`/`emergency_close`/`stop`); a `NULL` slot is
reported as `DriverCallError::AbsentCallback { callback }` (never a crash), so the caller applies
whichever of the C's two behaviours matches the slot (a logging no-op, or a plain skip).
`Drop` runs the `finish` callback then `dlclose`s the object.
Failure variants and their reason atoms (`erl_bif_ddll.c:1709-1720`): `NoDriverInit` ⇒
`no_driver_init`, `DriverInitFailed` ⇒ `driver_init_failed` (a null entry **or** a vetoing `init`),
`BadDriverName` ⇒ `bad_driver_name`, `DriverIncorrectVersion` ⇒ `driver_incorrect_version`
(missing `ERL_DRV_EXTENDED_MARKER` or out-of-range version), `Unspecified` ⇒ `unspecified_error`;
`SymbolNotFound` exists for surface completeness but `load()` never produces it.

### Documented deviations from the oracle
- `driver_mk_atom` returns the **name pointer** (the `ERL_DRV_ATOM` operand `decode_term_spec`
  interns), where the C returns a pre-interned atom handle.
- `driver_mk_port` returns the **port handle pointer** (the `erl_drv_output_term`/`erl_drv_send_term`
  encoding), where the C returns the port's id term; each pair round-trips.
- `driver_output_term`/`driver_send_term`/`erl_drv_output_term`/`erl_drv_send_term` return the host's
  code (`0` = delivered), where the C returns `1`/`0`; the documented `< 0` idiom sees identical
  behaviour.
- `erl_errno_id` returns `strerror`'s descriptive message; the oracle returns the lowercase POSIX
  errno *name*.
- `erl_drv_time_offset` returns `0` by construction (monotonic time is anchored on the system epoch).
- `driver_system_info` returns without writing for a null `sip` or an `si_size` shorter than the first
  field group, where the C calls `erts_exit(ERTS_ERROR_EXIT, …)`.
- `erl_drv_consume_timeslice` always returns `0` (no port-timeslice accounting).
- A malformed/out-of-range request (`driver_output_binary`'s `offset + len > orig_size`, a
  `driver_outputv` `skip` past the vector, a null `time_left`/`ErlIOVec`) is refused with `-1` where
  the C would read past the caller's buffer.
- A `dlopen` failure lands in `Unspecified` carrying the dynamic loader's own `dlerror` text (the C
  renders an `{open_error, N}` code); the `.so` suffix is appended only when absent; a `NULL`
  `driver_name` is a typed `BadDriverName` (the C would fault); and no code-server/driver list or
  reload policy is consulted (only the *entry* validation).

### Known gaps
- The `ErlDrvApi` port-queue / lifecycle / monitor / PDL / thread slots are `None` (they need a real
  port registry, poller and scheduler — `beam_io`/`beam_rt`); a driver using one gets the C's failure
  code, never a crash.
- `driver_select`/`driver_set_timer`/`driver_cancel_timer`/`driver_read_timer`/`driver_async` forward
  to `DriverHost`, whose **defaults** report "unsupported" (`-1`/`None`): a host only records the
  request, so "the OS poller really woke the port" is a `beam_io`/`beam_rt` concern, outside this
  crate.

## Routing Table
- `build.rs` → the link-flag build script: `-Wl,--export-dynamic` on the test targets (a NIF/driver `.so` resolves `enif_*`/`driver_*` out of the loading process); nothing else
- `src/lib.rs` → crate docs, module wiring, root re-exports (`NifEnv`/`NifHost`, `DriverEnv`/`DriverHost`, `Eterm`), the ABI **version constants** (`ERL_NIF_*`, `ERL_DRV_EXTENDED_*`, `ERL_NIF_VM_VARIANT`, `ERL_NIF_MIN_ERTS_VERSION`)
- `src/common.rs` → `erl_drv_nif.h` shared types: integer typedefs, `SysIOVec`, `ErlDrvSysInfo`, `ErlDrvThreadOpts`, `ErlDirtyJobFlags`, `ErlNifSelectFlags` + `ERL_NIF_SELECT_*`, `ErlDrvMonitor`, time constants
- `src/nif/term.rs` → `ERL_NIF_TERM`/`ERL_NIF_UINT`, `Eterm ↔ ERL_NIF_TERM` transmute seam
- `src/nif/types.rs` → NIF `#[repr(C)]` structs/enums/flags/fn-type aliases (from `erl_nif.h`), incl. the opaque `ErlNifEnv`
- `src/nif/api.rs` → `ErlNifApi` (176-entry `enif_*` function table; generated, header-ordered)
- `src/nif/host.rs` → the host seam: `NifHost` (heap/atoms/pid/ETF/make_ref services) + `NifEnv` (per-call env behind the opaque `*mut ErlNifEnv`, exception slot, `as_ptr`/`from_opaque`)
- `src/nif/enif.rs` → the implemented `enif_*` host functions + `enif_api_table()` / `#[used] static ENIF_API` (the frozen-surface table; keeps the `#[no_mangle]` symbols linked)
- `src/nif/loader.rs` → the dynamic NIF loader: `NifLibrary`, `NifLoadError`/`NifCallError`, `load()` (dlopen/dlsym + oracle-order validation), `NifLibrary::call`, `Drop` (unload + dlclose)
- `src/driver/types.rs` → driver `#[repr(C)]` structs/enums/flags + `ErlDrvEntry` (from `erl_driver.h`)
- `src/driver/api.rs` → `ErlDrvApi` (111-entry `driver_*`/`erl_drv_*` function table) + `ErlDrvApi::empty()`
- `src/driver/host.rs` → the driver host seam: `DriverHost` (heap/atoms/output/send_term + defaulted select/timer/async/pid services) + `DriverEnv` (per-port env behind the opaque `*mut ErlDrvPort_`, `as_port`/`from_port`) + `decode_term_spec` (the reverse-Polish term-data decoder)
- `src/driver/drv/` → a directory module split along the C ABI's host-free/port-dependent seam: `mod.rs` (module docs + the shared `env_for` helper + `driver_api_table()` / `#[used] static DRIVER_API`, the frozen-surface table that keeps the `#[no_mangle]` symbols linked + the `driver_term_nil` data symbol + `binary_payload`), `mem.rs` (part 1, the implemented host-free `driver_*` host functions), `port.rs` (part 2, the implemented port/host-dependent `driver_*` host functions), `port_tests.rs` (the path-included `port::tests` module: the port-pointer recovery, term-data round-trips through the populated table, and the output/select/timer host traffic)
- `src/driver/loader.rs` → the dynamic driver loader: `DriverLibrary`, `DriverLoadError`/`DriverCallError`, `load()` (dlopen/dlsym `driver_init` + oracle-order `ErlDrvEntry` validation), the per-callback wrappers, `Drop` (finish + dlclose)
- `src/layout.rs` → the compile-time layout freeze (all `const _: () = assert!(...)` for the table above)
- `tests/common/mod.rs` → shared test helpers (NOT a test target: a subdirectory module pulled in with `mod common;`): `erl_include_dir` (resolves the dir holding `erl_nif.h` *and* `erl_driver.h`), `cc_available`, `compile_so`, `skip`, `TestHost`, `TestDriverHost`
- `tests/` → crate-level integration tests (`layout.rs`, `term_abi.rs`, `api_surface.rs`, `entry.rs`, `host_seam.rs`, `enif.rs`, `loader.rs`, `nif_end_to_end.rs`, `driver_end_to_end.rs`) + `tests/fixtures/test_nif.c` (a real C NIF) and `tests/fixtures/test_driver.c` (a real C driver), both compiled at test time with the system `cc` against the system OTP headers

## Notes for Agents
- **Read the legacy headers before touching a layout**: `../../erts/emulator/beam/erl_nif.h`,
  `erl_nif_api_funcs.h`, `erl_drv_nif.h`, `erl_driver.h`. Field order and integer widths must match
  exactly; the asserts are the enforcement. Cite the header line in a comment where a value is
  non-obvious.
- `../../../erts/emulator/beam/global.h` line ~57 holds the real `struct enif_environment_t` body —
  it is emulator-internal and must **not** be replicated: `ErlNifEnv` stays opaque here (its Rust
  analogue for a live call is `nif::host::NifEnv`, reached only through `as_ptr`/`from_opaque`).
- `erl_int_sizes_config.h` (included by `erl_drv_nif.h`) fixes `SIZEOF_LONG == 8` on this target,
  which is why the 4 `enif_*64` table entries are excluded from `ErlNifApi`.
- Building: **never `cd`**; use `../scripts/cargo.sh build -p beam_abi` /
  `../scripts/cargo.sh test -p beam_abi` (the sandbox `$HOME` is read-only; the wrapper picks a
  writable `CARGO_HOME`).
- `tests/common/mod.rs` is invisible to `cargo test` unless some `tests/*.rs` says `mod common;` —
  keep at least one includer (currently `host_seam.rs`, `loader.rs`, `nif_end_to_end.rs`,
  `driver_end_to_end.rs`) so it stays compiled; Cargo would in particular *not* report a
  `tests/common/mod.rs` that fails to compile.
- The driver loader's ground truth is a different C file from the NIF's: `do_load_driver_entry` in
  `../../erts/emulator/beam/erl_bif_ddll.c` (the `errcode_tab` reason atoms included),
  `erts_sys_ddll_load_driver_init` in `../../erts/emulator/sys/unix/erl_unix_sys_ddll.c`, and
  `init_driver` in `../../erts/emulator/beam/io.c`; the term-data decoder mirrors `io.c`'s
  `ERTS_DDT_*`. Do not touch `../../tools/` or `../../erts/`.
- A driver fixture must reference **only** `driver_*` slots the crate populates (`Some`), or
  `dlopen(RTLD_NOW)` fails on the unresolved import; the same rule applies to a NIF fixture and the
  `enif_*` slots.
- Adding a `cargo::rustc-link-arg-bins` instruction to `build.rs` **breaks the build** (no `[[bin]]`
  target in this package).

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, layering waves (§0 places `beam_abi` in wave 0).
- `../../CONTEXT.md` → workspace contracts: §1 (A.3 NIF C ABI, A.4 driver C ABI), §2 frozen term layout, §3 error strategy.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.3/A.4; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §C (the frozen-header seam).
- `../beam_term/` → the internal `Eterm` this crate transmutes to/from (`nif::term`).
- `../beam_alloc/` → the production bump `Heap` the host seam hands out (`NifHost::heap`).
- `../beam_rt/`, `../beam_bifs/`, `../beam_io/` → the runtime owners of the `NifHost`/`DriverHost` seams and the implementors of the remaining `ErlNifApi`/`ErlDrvApi` slots (resources, threads, I/O queues, port queues/lifecycle, monitors, scheduler).
