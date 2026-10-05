# `beam_abi` — Layout-Frozen NIF & Driver C ABI

## Intent
Owns the **frozen foreign-function C ABIs** the Rust ERTS exposes to native code: the NIF C ABI
(part of workspace compatibility contract **A.3**) and the driver C ABI (**A.4**). It reproduces the
exact in-memory **struct layouts, enum widths, function-pointer signatures and constants** of the
legacy headers so that a NIF/driver `.so` or `.o` compiled against the *unmodified* OTP headers
links against Rust ERTS and behaves identically. This is the **link-time frozen-header seam** of
Part II §C — the struct layouts must never change; a mismatch here is an ABI break.

This crate **declares types only**. It provides no `enif_*`/`erl_drv_*` *implementations* — those
belong to the NIF/driver subsystem (`beam_rt`/`beam_bifs`/`beam_io`, later passes). What is frozen
now is the *shape* of the boundary: the types, the layouts, the term transmute, and the
function-pointer tables (`ErlNifApi`, `ErlDrvApi`) the runtime must implement and export.

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
A `#[repr(C)]` struct of the **181** `enif_*` function pointers that exist on LP64, in exact
`erl_nif_api_funcs.h` line order (the 4 `#if SIZEOF_LONG != 8` entries are excluded). This is the
Rust analogue of the header's Windows-only `TWinDynNifCallbacks` and defines the surface the runtime
must implement. `size_of::<ErlNifApi>() == 181 * 8 == 1448` is asserted at compile time.

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
| `ErlNifApi` | 1448 | 8 | (181 fn ptrs) |
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
- **Dependencies:** `beam_common`, `beam_term`, `libc` only. `#[repr(transparent)]` on the term
  aliases; `#[repr(C)]` on every ABI struct and enum-with-values.
- **`unsafe` discipline:** this crate declares FFI types, so `unsafe extern "C" fn` appears
  throughout; every such declaration is documented, and every `unsafe` block (transmute/precondition
  helpers, if any) carries a preceding `// SAFETY:` rationale. Function pointers are declared but
  never *called* here — no call sites exist in this crate.
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
  compile-checked. `ErlNifApi` is a faithful 181-entry mirror of `erl_nif_api_funcs.h` order;
  `ErlDrvApi` mirrors the `erl_driver.h` free-function list.
- **`ErlDrvSysInfo` is the single definition; `ErlNifSysInfo` is a `pub use` alias** (the header does
  `typedef ErlDrvSysInfo ErlNifSysInfo;`), preventing duplication.
- **`SysIOVec` aliases `libc::iovec`** on Unix (the header uses `struct iovec` from `<sys/uio.h>`),
  guaranteeing `writev`/`readv` compatibility; a layout assert pins size 16 / offsets 0, 8.
- **Target = LP64 only.** Windows-specific layout branches (`TWinDynNifCallbacks`, `ERL_SYS_DRV`
  `start` signature, `WinDynNifCallbacks`) are intentionally not modelled; the layout asserts make
  this explicit (they would fail on a non-LP64 target).

## Test Strategy
- `tests/layout.rs` — re-asserts every size/align/offset from the frozen table at **runtime** (so a
  mismatch also fails the suite with a readable message, complementing the compile-time asserts):
  all struct sizes, `ErlNifEntry`/`ErlDrvEntry`/`ErlNifIOVec`/`ErlNifMapIterator` field offsets, and
  the 4-byte width of every enum/flag type.
- `tests/term_abi.rs` — `size_of::<ERL_NIF_TERM>() == size_of::<usize>() == size_of::<Eterm>()`,
  matching `align_of`, and `nif_from_eterm(eterm_from_nif(t)) == t` round-trips over boundary raw
  values (`NIL`, `make_small(0)`, max small, atom tag) — the A.3 guarantee.
- `tests/api_surface.rs` — `size_of::<ErlNifApi>() == 181 * 8` and `size_of::<ErlDrvApi>() == <n> * 8`
  (every column is one pointer) — the strongest available freeze of the table shape — plus a smoke
  test that an all-`None` table round-trips through `size_of_val`.
- `tests/entry.rs` — build an `ErlNifFunc`/`ErlNifEntry` (the `nif_init` shape) with null callbacks,
  read back `major`/`minor`/`num_of_funcs`/`vm_variant`/`options`, and confirm the `#[repr(C)]` field
  order via `offset_of!` on a constructed value; likewise a `ErlDrvEntry` with all-null callbacks.
- Per-module `#[cfg(test)]` unit tests only where they add value (constants); the substantive checks
  live in `tests/`.
- Gate: `../scripts/cargo.sh test -p beam_abi` GREEN and `../scripts/cargo.sh build --workspace`
  GREEN. Cross-checking behaviour against real ERTS is `../../tools/differential`'s job (sibling —
  read-only, escalate writes to `..`).

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, the ABI **version constants** (`ERL_NIF_*`, `ERL_DRV_EXTENDED_*`, `ERL_NIF_VM_VARIANT`, `ERL_NIF_MIN_ERTS_VERSION`)
- `src/common.rs` → `erl_drv_nif.h` shared types: integer typedefs, `SysIOVec`, `ErlDrvSysInfo`, `ErlDrvThreadOpts`, `ErlDirtyJobFlags`, `ErlNifSelectFlags` + `ERL_NIF_SELECT_*`, `ErlDrvMonitor`, time constants
- `src/nif/term.rs` → `ERL_NIF_TERM`/`ERL_NIF_UINT`, `Eterm ↔ ERL_NIF_TERM` transmute seam
- `src/nif/types.rs` → NIF `#[repr(C)]` structs/enums/flags/fn-type aliases (from `erl_nif.h`)
- `src/nif/api.rs` → `ErlNifApi` (181-entry `enif_*` function table; generated, header-ordered)
- `src/driver/types.rs` → driver `#[repr(C)]` structs/enums/flags + `ErlDrvEntry` (from `erl_driver.h`)
- `src/driver/api.rs` → `ErlDrvApi` (`driver_*`/`erl_drv_*` function table)
- `src/layout.rs` → the compile-time layout freeze (all `const _: () = assert!(...)` for the table above)
- `tests/` → crate-level integration tests (`layout.rs`, `term_abi.rs`, `api_surface.rs`, `entry.rs`)

## Notes for Agents
- **Read the legacy headers before touching a layout**: `../../erts/emulator/beam/erl_nif.h`,
  `erl_nif_api_funcs.h`, `erl_drv_nif.h`, `erl_driver.h`. Field order and integer widths must match
  exactly; the asserts are the enforcement. Cite the header line in a comment where a value is
  non-obvious.
- `../../../erts/emulator/beam/global.h` line ~57 holds the real `struct enif_environment_t` body —
  it is emulator-internal and must **not** be replicated: `ErlNifEnv` stays opaque here.
- `erl_int_sizes_config.h` (included by `erl_drv_nif.h`) fixes `SIZEOF_LONG == 8` on this target,
  which is why the 4 `enif_*64` table entries are excluded from `ErlNifApi`.
- Building: **never `cd`**; use `../scripts/cargo.sh build -p beam_abi` /
  `../scripts/cargo.sh test -p beam_abi` (the sandbox `$HOME` is read-only; the wrapper picks a
  writable `CARGO_HOME`).
- Do not implement any `enif_*`/`erl_drv_*` *function body* — out of scope for this pass. Do not
  touch `../../tools/`.

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, layering waves (§0 places `beam_abi` in wave 0).
- `../../CONTEXT.md` → workspace contracts: §1 (A.3 NIF C ABI, A.4 driver C ABI), §2 frozen term layout, §3 error strategy.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.3/A.4; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §C (the frozen-header seam).
- `../beam_term/` → the internal `Eterm` this crate transmutes to/from (`nif::term`).
- `../beam_rt/`, `../beam_bifs/`, `../beam_io/` → the future implementors of `ErlNifApi`/`ErlDrvApi`.
