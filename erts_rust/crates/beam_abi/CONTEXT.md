# `beam_abi` — Layout-Frozen NIF & Driver C ABI

## Intent
Owns the **frozen foreign-function C ABIs** the Rust ERTS exposes to native code: the NIF C ABI
(part of workspace compatibility contract **A.3**) and the driver C ABI (**A.4**).
It reproduces the exact in-memory **struct layouts, enum widths, function-pointer signatures and
constants** of the legacy headers so that a NIF/driver `.so` or `.o` compiled against the
*unmodified* OTP headers links against Rust ERTS and behaves identically.
This is the **link-time frozen-header seam** of Part II §C — the struct layouts must never change; a
mismatch here is an ABI break.

The crate freezes the *shape* of the boundary (the types, the layouts, the term transmute, the
`ErlNifApi`/`ErlDrvApi` function tables) **and implements both host sides**:
- the **NIF host layer** — the `enif_*` bodies under `src/nif/enif/` (one file per family, **all 176**
  LP64 slots populated), the `NifHost` seam (`nif::host`) and the `dlopen`-based NIF loader
  (`nif::loader`);
- the **driver host layer** — the `driver_*`/`erl_drv_*` bodies under `src/driver/drv/` (**108 of the
  111** slots), the `DriverHost` seam (`driver::host`) and the `dlopen`-based driver loader
  (`driver::loader`, `dlsym "driver_init"` plus the oracle's `erlang:load_driver/2` validation order).
- `src/sync/` holds the **shared pthread facade** both thread families are thin facades over.

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
`size_of::<ErlNifApi>() == 176 * 8 == 1408` is asserted at compile time; `ERL_NIF_API_FIELD_COUNT ==
176` is asserted in `nif::enif`.

### Driver types (`driver::types`) — `#[repr(C)]`, exactly as `erl_driver.h`
`ErlDrvEntry`, `ErlDrvBinary`, `ErlIOVec`, `ErlDrvNowData`; opaque handles `ErlDrvPort`,
`ErlDrvEvent`, `ErlDrvData`, `ErlDrvThreadData`, `ErlDrvPDL`, `ErlDrvTid`/`ErlDrvMutex`/`ErlDrvCond`/
`ErlDrvRWLock` (+ `_` marker structs); scalars `ErlDrvUInt64`/`ErlDrvSInt64`/`ErlDrvUInt`/`ErlDrvSInt`,
`ErlDrvTermData`, `ErlDrvSizeT`, `ErlDrvSSizeT`, `ErlDrvTime`, `ErlDrvTSDKey`; enum `ErlDrvTimeUnit`;
flag newtype `ErlDirtyJobFlags`; constants `ERL_DRV_*` (term-data tags, `ERL_DRV_FLAG_*`,
`PORT_*`, `ERL_DRV_READ/WRITE/USE*`, `DRIVER_CALL_KEEP_BUFFER`, `ERL_DRV_ERROR_*`,
`ERL_DRV_BUSY_MSGQ_*`).

### `ErlDrvApi` (`driver::api`) — the driver function table (the frozen `driver_*`/`erl_drv_*` surface)
A `#[repr(C)]` struct of the **111** function pointers for every function the *runtime provides* to
drivers (`driver_output`, `driver_select`, `erl_drv_mutex_*`, `erl_drv_thread_*`, `driver_alloc*`,
`driver_enq*`, monitor/PDL/term-data functions, …). The legacy header exposes these as exported
free functions; bundling them into one table is the Rust runtime's export shape (documented
deviation, see Design Decisions). Fields are `Option<unsafe extern "C" fn ...>` (null-optimized) in
`erl_driver.h` order.

### Shared (`common`) — `erl_drv_nif.h`
`ErlNapiUInt64`/`ErlNapiSInt64`/`ErlNapiUInt`/`ErlNapiSInt`, `SysIOVec`, `ErlDrvSysInfo`,
`ErlDrvThreadOpts`, `ErlDirtyJobFlags`, `ErlNifSelectFlags`, `ErlDrvMonitor`, the `ERTS_NAPI_*` time
constants, and `ERL_NIF_SELECT_*` / `ErlNifSelectFlags` selector bits.

### Per-family implementation status (MEASURED at this node's HEAD — `Some`/total slots)
`ErlNifApi` — **176/176** populated (the seven C-variadic slots only when `build.rs` compiled the
`csrc/nif_variadic.c` shim — cfg `beam_abi_variadic_shim`). `ErlDrvApi` — **111/111** populated: every
LP64 slot, no `None` remaining. A mechanical test (`tests/abi_conformance.rs`) re-derives both
counts from the headers and fails if a header declaration ever lacks a crate definition.

**NIF families** (`src/nif/enif/*.rs`):
| family file | slots it owns | `Some`/total |
|---|---|---|
| `mem` | allocation, the binary family, the ETF pair | 13/13 |
| `get` | the `enif_get_*` accessors | 12/12 |
| `make` | the `enif_make_*` constructors (maps excepted) | 18/18 |
| `map` | the map accessors/constructors | 7/7 |
| `pred` | `enif_is_*`, `compare`, `is_identical`, `hash`, `term_type`, `term_size` | 17/17 |
| `proc` | self/pid/port accessors + atom-cache queries | 7/7 |
| `process` | monitors, send, port command, whereis, `schedule_nif` | 12/12 |
| `exception` | badarg/raise/pending/clear | 4/4 |
| `threads` | mutex/cond/rwlock/tsd/thread | 34/34 |
| `ioq` | I/O queue + I/O vector | 10/10 |
| `mapiter` | the `enif_map_iterator_*` family | 7/7 |
| `time` | time, env, system info, unique integers, timeslice, `dl*` | 11/11 |
| `resources` | resource types + allocations | 11/11 |
| `select` | `enif_select`/`enif_select_x` | 2/2 |
| `envs` | `enif_alloc_env`/`enif_free_env`/`enif_make_copy`/`enif_priv_data` | 4/4 |
| `variadic` | the seven C-variadic slots (cfg `beam_abi_variadic_shim`) | 7/7 |

Every LP64 `enif_*` slot is populated; the seven C-variadic slots are `Some` only when `build.rs`
compiled the `csrc/nif_variadic.c` shim (otherwise `None`, a loud `dlopen` failure, never a wrong
result).

**Driver families** (`src/driver/drv/*.rs`):
| family file | slots it owns | `Some`/total |
|---|---|---|
| `mem` | host-free alloc/time/`dl*`/errno/`driver_mk_term_nil` + the driver-binary family (incl. the refcount trio) | 23/23 |
| `port` | output, term-data, port queries, select/timer/async | 17/17 |
| `queues` | the port-queue family | 11/11 |
| `threads` | the `erl_drv_*` thread/synchronisation family (over `crate::sync`) | 33/33 |
| `pdl` | port data lock + driver monitors + `driver_get_now` | 11/11 |
| `lifecycle` | failures/exit/attributes/asynchronous init | 12/12 |
| `registry` | the driver-registry slots (`add_driver_entry`/`remove_driver_entry`/`driver_lock_driver`/`driver_create_port`) | 4/4 |

Every LP64 `driver_*`/`erl_drv_*` slot is populated. The three `driver_binary_*_refc` slots read a
**hidden per-block refcount header** (see Design Decisions).

### Module layout
- `src/nif/enif/` and `src/driver/drv/` are **directory modules split one file per family**.
  Each family file owns `pub(super) const fn populate(api) -> api` that fills in **only its own**
  slots; `src/nif/enif/mod.rs` / `src/driver/drv/mod.rs` chain the `populate`s **innermost→outermost**
  into the `#[used] static ENIF_API`/`DRIVER_API` published by `enif_api_table()`/`driver_api_table()`.
  Extending a family edits that family's file, never the `mod.rs`.
- `src/sync/` — the in-crate **pthread facade**: `Mutex`/`Cond`/`RwLock`/`TsdKey`/`Tid` plus
  `mutex_*`, `cond_*`, `rwlock_*`, `tsd_key_create`/`tsd_key_destroy`/`tsd_set`/`tsd_get`,
  `thread_opts_create`/`thread_opts_destroy`, `thread_create`/`thread_self_tid`/`thread_equal_tids`/
  `thread_exit`/`thread_join`/`thread_name`/`thread_type`. Both thread families delegate here (the
  Rust equivalent of the C's single `erl_drv_thread.c`); deviations are documented in `sync/mod.rs`
  and in *Known gaps* below.
- `csrc/nif_variadic.c` — the C-variadic shim, compiled by `build.rs` (see *The C-variadic shim*).

### The host seam (`nif::host`) — the runtime services behind the `enif_*` layer
Not part of the C ABI; the *internal* boundary the `enif_*` bodies are written against.
- `pub trait NifHost` — **required** `heap() -> &mut beam_alloc::Heap`, `intern_atom(&str) ->
  Option<Eterm>`, `atom_name(Eterm) -> Option<String>`; plus **defaulted** hooks, each `None`/no-op
  meaning "unsupported": `self_pid`, `term_to_binary`, `binary_to_term`, `make_ref`, `existing_atom`,
  `priv_data`, `system_info`, `unique_integer`, `consume_timeslice`, `is_process_alive`,
  `is_port_alive`, `is_current_process_alive`, `send`, `port_command`, `whereis_pid`, `whereis_port`,
  `monitor_process`, `demonitor_process`, `select`, `schedule_nif`, `alloc_env`, `free_env`,
  `make_copy`, `dynamic_resource_call`. The ETF hooks (`term_to_binary`/`binary_to_term`) exist so
  `beam_abi` needs **no** `beam_codec` dependency.
- `pub struct NifEnv` — the concrete per-call environment behind the C-facing opaque
  `*mut ErlNifEnv`: `new(&mut dyn NifHost)`, `host()`, `heap()`, `as_ptr()`,
  `unsafe from_opaque(*mut ErlNifEnv) -> Option<&mut NifEnv>`, the pending-exception slot
  (`pending_exception`/`set_exception`/`clear`), and a **bounded per-call record of the atoms
  interned through this env** (`record_atom`, read back by `recorded_atom_name`/`recorded_atom`;
  `MAX_RECORDED_ATOMS = 256`, first-wins dedupe) — the runtime commits a fresh atom lazily, so `NifHost::atom_name` (committed
  table only) cannot see one interned earlier in the *same* call. Re-exported at the crate root as
  `beam_abi::{NifEnv, NifHost}`.
  `enif_clear_env` does **not** touch the exception slot (matching the C, which frees temporary
  objects / rewinds the heap top but leaves `exception_thrown` set — `erl_nif.c:700-723`);
  `NifEnv::clear()` is the Rust-only reset helper, not the C `enif_clear_env`.
- **Current-NIF-host stack (host discovery for `enif_alloc_env`).** `NifEnv::new` — the single
  construction point for every real call env — publishes the host it was given onto a crate-private
  thread-local stack, and `NifEnv`'s `Drop` pops exactly its own entry (out-of-order drop restores
  the enclosing host). The read side is `pub(crate) fn with_current_host<R>(f: impl FnOnce(&mut dyn
  NifHost) -> R) -> Option<R>`; `enif_alloc_env` (which takes no argument) uses it. A thread-local is
  the sound store because `NifEnv` is `!Clone`/`!Send` (it holds a raw host pointer) and is never
  moved across threads.
- `nif::enif` implements the `enif_*` host functions over this seam; `nif::loader` implements the
  dynamic NIF loader / `ErlNifEntry` load contract. See **"NIF host layer"** below.

### The driver host seam (`driver::host`) — the runtime services behind the `driver_*` layer
Not part of the C ABI; the *internal* boundary the `driver_*`/`erl_drv_*` bodies are written against.
- `pub trait DriverHost` — **required** `heap() -> &mut beam_alloc::Heap`, `intern_atom(&str) ->
  Option<Eterm>`, `output(port, &[u8]) -> c_int`, `send_term(port, Option<Eterm>, Eterm) -> c_int`;
  plus **defaulted** hooks (defaults report "unsupported" as `-1`/`None`): `select`, `set_timer`,
  `cancel_timer`, `read_timer`, `r#async`, `connected_pid`, `caller_pid`, `binary_to_term` (the ETF
  decode behind `ERL_DRV_EXT2TERM`), the port-queue hooks
  `enq`/`pushq`/`deq`/`sizeq`/`peekq`, the attribute hooks `set_busy_port`/`set_control_flags`/
  `get_port_flags`, the failure/lifecycle hooks `failure`/`port_exit`/`busy_msgq_limits`/`init_ack`/
  `set_os_pid`/`async_port_key`, the monitor hooks `monitor_process`/`demonitor_process`/
  `get_monitored_process`, the PDL hooks `pdl_create`/`pdl_lock`/`pdl_unlock`/`pdl_get_refc`/
  `pdl_inc_refc`/`pdl_dec_refc`, the driver-registry hooks `add_driver_entry`/`remove_driver_entry`/
  `lock_driver`/`create_port`, and `now`. Each default is the C's own failure value (`-1`/`0`/the
  invalid-port sentinel) and dereferences no operand.
- **Current-driver-host publication (`driver::current_host`).** `add_driver_entry`/`remove_driver_entry`
  carry no port operand, so they cannot recover a host through `DriverEnv::from_port`; they are only
  meaningful inside a driver callback (C: "to be used by drivers"). The runtime (`beam_io`) publishes
  the host for the duration of one driver callback via
  `pub fn with_current_host<R>(host: &mut dyn DriverHost, f: impl FnOnce() -> R) -> R` — a scoped guard
  that stores the host in a thread-local `Cell<Option<*mut dyn DriverHost>>` and **always restores the
  previous host on exit** (nesting restores the outer host; a panic/unwind in `f` still restores). With
  no host published the two entry-list slots answer gracefully (`-1`/`0`) without dereferencing their
  operand.
- `pub struct DriverEnv` — the concrete per-port environment behind the C-facing opaque
  `*mut ErlDrvPort_`: `new(&mut dyn DriverHost)`, `host()`, `heap()`, `as_port()`,
  `unsafe from_port(*mut ErlDrvPort_) -> Option<&mut DriverEnv>`. **The `ErlDrvPort` a runtime passes
  to a driver callback *is* the `DriverEnv::as_port` pointer** (the analogue of `NifEnv`).
- `pub fn decode_term_spec(&mut DriverEnv, &[ErlDrvTermData]) -> Option<Eterm>` — the reverse-Polish
  term-data decoder behind `erl_drv_output_term`/`erl_drv_send_term`.
- `driver::drv` implements the `driver_*`/`erl_drv_*` host functions and publishes them through
  `driver_api_table()`; `driver::loader` implements the dynamic driver loader / `ErlDrvEntry` load
  contract. See **"Driver host layer"** below.

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
  heap the host seam hands out via `NifHost::heap`), **`libc`**. No other external crate
  (deliberately: **no `beam_codec`** — ETF goes through `NifHost`). Building the C-variadic shim also
  needs a **C compiler** at build time (`$CC`, default `cc`) and an archiver (`$AR`, default `ar`),
  both **optional** — their absence degrades, it does not fail the build.
- **`build.rs`** does two things: (1) `cargo::rustc-link-arg-tests=-Wl,--export-dynamic` so a loaded
  NIF/driver `.so` resolves the runtime's `enif_*`/`driver_*` dynamic symbols (see Design Decisions);
  (2) compiles `csrc/nif_variadic.c` and emits `cfg(beam_abi_variadic_shim)` +
  `rustc-link-search`/`rustc-link-lib=static`.
- **`unsafe` discipline:** this crate declares FFI types and `#[no_mangle] pub unsafe extern "C"`
  bodies, so `unsafe` appears throughout; every declaration is documented and every `unsafe` block
  (term/pointer transmutes, the `NifEnv`/`DriverEnv` opaque-pointer recover paths) carries a
  preceding `// SAFETY:` rationale. `unsafe_op_in_unsafe_fn` stays clean.
- **The raw-pointer ABI wrappers intentionally `allow(clippy::not_unsafe_ptr_arg_deref)`.** The
  `DriverLibrary` callback wrappers (`src/driver/loader.rs`: `start`/`control`/`call`/`stop`/
  `output`/`outputv`/`timeout`/`ready_input`/`ready_output`/`ready_async`/`flush`/`process_exit`/
  `stop_select`/`emergency_close`) and `sync::thread_exit` are **safe** public fns whose parameters
  are the raw pointers of the C `ErlDrvEntry`/pthread signatures, forwarded verbatim to the driver's
  own callback (or `pthread_exit`): the pointer-validity contract belongs to the caller, exactly as
  in the C. The lint is allowed with a recorded rationale — a module-scoped inner attribute in
  `src/driver/loader.rs` and a function-scoped attribute on `thread_exit` — because making them
  `unsafe fn` would be a breaking change to the frozen ABI-mirroring public API that `beam_rt`/
  `beam_io` consume. `cargo clippy -p beam_abi --all-targets --no-deps` is error-free as a result
  (warnings remain; the workspace clippy step is non-blocking).
- **Nullability:** C function-pointer fields are `Option<unsafe extern "C" fn …>` (null-pointer
  optimized, same size/align as a raw pointer). Data pointers stay raw `*mut T`/`*const T`.
- **Opaque handles** are `#[repr(C)] pub struct X { _private: [u8; 0] }` (incomplete C type), used
  behind pointers — never by value, never dereferenced in this crate.
- Files target ~1000 lines, split by responsibility; `nif::api`/`driver::api` are legitimately large
  **generated tables** (see their top-of-file notes), and `src/nif/enif/tests.rs`,
  `src/sync/tests.rs`, `tests/common/mod.rs` are test-only modules.

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
  Rust runtime exports/consumes the surface, and it makes the frozen signature list explicit and
  compile-checked.
- **The C-variadic `enif_*` slots are defined in a small C shim.** Stable Rust cannot give a body to
  a C-variadic function (rust-lang/rust#44930). `csrc/nif_variadic.c` is a *thin* file whose only job
  is to touch `va_list`; each of the seven forwards straight into a `#[no_mangle]` Rust helper in
  `src/nif/enif/variadic.rs` that owns all the logic. `build.rs` drives `$CC`/`$AR` with
  `std::process::Command` (deliberately **no `cc` crate**) and, on success, publishes the object as a
  static archive in `$OUT_DIR` via `rustc-link-search`/`rustc-link-lib=static` — those two *are*
  propagated through the dependency graph, so a dependent binary that pulls `enif_api_table()` (e.g.
  `beam_rt`) resolves the shim's `enif_*` symbols. Without a C compiler the cfg is unset and the seven
  slots stay `None` (honest degradation, never a wrong result).
- **`ErlDrvSysInfo` is the single definition; `ErlNifSysInfo` is a `pub use` alias** (the header does
  `typedef ErlDrvSysInfo ErlNifSysInfo;`), preventing duplication.
- **`SysIOVec` aliases `libc::iovec`** on Unix (the header uses `struct iovec` from `<sys/uio.h>`),
  guaranteeing `writev`/`readv` compatibility; a layout assert pins size 16 / offsets 0, 8.
- **A driver binary carries a hidden refcount header, exactly as ERTS does.** The frozen
  `ErlDrvBinary` is only `{ orig_size, orig_bytes }`, but real ERTS reaches the count through an
  emulator-internal `Binary` header *preceding* it (`Binary2ErlDrvBinary`/`ErlDrvBinary2Binary`,
  `erl_binary.h:173-176`). `driver_alloc_binary` therefore allocates a whole 8-byte `AtomicI64`
  refcount word before the public struct (`DRIVER_BINARY_PREFIX == 16`), keeping the returned pointer
  on `orig_size`, `orig_bytes` 8-aligned (the C's `ERTS_CHK_BIN_ALIGNMENT`) and the frozen layout
  untouched. Alloc refc = 1; `inc`/`dec` return the NEW value and `dec` never frees (only
  `driver_free_binary` releases, at zero) — all pinned against the oracle by
  `tests/driver_binary_refc.rs`.
- **Target = LP64 only.** Windows-specific layout branches (`TWinDynNifCallbacks`, `ERL_SYS_DRV`
  `start` signature, `WinDynNifCallbacks`) are intentionally not modelled; the layout asserts make
  this explicit (they would fail on a non-LP64 target).
- **There is no shared low-level `dlopen` wrapper — every dynamic-loading site calls `libc::dl*`
  directly.** `nif::loader` and `driver::loader` each own a **private** `dl_last_error`/`dl_sym`/
  `OpenHandle` trio (deliberate mirrors of one another, `fn` not `pub fn`, visible only inside their
  own file — see the driver loader's module doc lines "deliberately mirror [`crate::nif::loader`]'s").
  The frozen ABI slots forward straight to `libc` too: `driver_dl_open`/`_sym`/`_close`/`_error`
  (`driver/drv/mem.rs`, the host-free A.4 surface, re-exported by `driver/drv/mod.rs`) and
  `enif_dlopen`/`enif_dlsym` (`nif/enif/time.rs`, whose private `dlsym_checked`/`last_dl_error` mirror
  the loaders' helpers). There is **no** `ffi`/`sys`/dynlib module and no *public* Rust `dlopen`
  helper. The safe loaders are consumed only by `beam_rt` (`port::driver::load_driver` over
  `driver::loader::load`; the `NifBridge` over `nif::loader::load`). A future `erl_ddll`-style
  facility would have to introduce the single shared wrapper.
- **`NifEnv` / `DriverEnv` erase their host borrow's lifetime at construction.** `NifEnv::new(&mut dyn
  NifHost)` (and `DriverEnv::new(&mut dyn DriverHost)`) must keep the spec's signature, but the
  C-facing env has no lifetime parameter — and a raw pointer to a trait object is invariant and
  defaults to `'static` (rust#141402). So `new` does one documented `mem::transmute` of the
  layout-identical borrow (only the erased lifetime is reinterpreted); the caller-owned borrow is
  reproduced exactly by `host`, and the safety invariant ("keep the host alive while the env is
  used") is discharged by the same caller contract as `from_opaque`/`from_port`. Neither env type is
  `Clone`.
- **The test link exports its dynamic symbols (`build.rs`).** A NIF `.so` resolves `enif_*` and a
  driver `.so` resolves `driver_*` at load time out of the loading process, so the test targets are
  linked with `-Wl,--export-dynamic` (`cargo::rustc-link-arg-tests`). The `*-bins` counterpart is
  **not** emitted: this package has no `[[bin]]` target and Cargo hard-errors on a `*-bins`
  instruction when there is none.
- **The created-thread name registry is first-wins.** `thread_create` and the new thread's own
  `thread_start` both register the same tid's name via `register_thread_name`, which keeps the first
  entry and never replaces it, so a buffer handed out by `thread_name` is never freed early.

## The C-variadic shim (mechanism)
- The seven slots (`enif_make_tuple`, `enif_make_list`, `enif_set_option`, `enif_fprintf`,
  `enif_snprintf`, `enif_vfprintf`, `enif_vsnprintf`) live in `csrc/nif_variadic.c`.
- Rust cannot read a `va_list`, so it *drives* the extraction: for the `*printf` family the shim
  first calls `beam_abi_enif_fmt_analyze` (a Rust format-string parser), then pulls exactly the
  varargs the parser reported into a flat `u64` slot array and calls
  `beam_abi_enif_fmt_{snprintf,fprintf}`, which render the string. The grammar lives in Rust, so
  parser and extractor cannot disagree.
- The supported `*printf` subset is a faithful port of `erts/lib_src/common/erl_printf_format.c`;
  anything outside it returns a **negative error** (`-EINVAL`), never a wrong string.
- `build.rs` emits `cargo::rustc-check-cfg=cfg(beam_abi_variadic_shim)` unconditionally (so the cfg
  is known on the degraded path) and sets it only when the shim compiled.

## Test Strategy
- `tests/layout.rs` — the **runtime mirror** of the `src/layout.rs` compile-time freeze: every
  size/align/offset in the Frozen layout table below, the `ErlNifMapIterator` **union interior**
  (`ErlNifMapIteratorUnion`/`Flat`/`Hash`), every scalar-typedef width, every opaque handle, and the
  full enum-discriminant / flag-bit / versioned-constant surface (`ERL_NIF_*`, `ERL_DRV_*`, `PORT_*`,
  `ERTS_NAPI_*`).
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
  `beam_ets/tests/common/mod.rs`). It provides `erl_include_dir()` (resolves the dir holding both
  `erl_nif.h` and `erl_driver.h`), `cc_available()`, `compile_so()`, `skip()`, a `TestHost`
  (`beam_alloc::Heap` + `beam_term::atom::AtomTable`) implementing `NifHost`, and a `TestDriverHost`
  (same arena + atom table) implementing `DriverHost`, recording every output/term/select/timer/async
  call with accessors for assertions. `host_seam.rs` also compiles a trivial `erl_nif.h`-including
  probe `.so`, proving the include-directory resolution and the `cc -shared -fPIC` invocation work.
- **Per-family end-to-end tests** (the `tests/nif_*.rs` / `driver_*.rs` files in the Routing Table),
  each a real C fixture compiled at test time with the system `cc` against the system OTP headers and
  `dlopen`ed (`RTLD_NOW`) through the crate's own loader, so a single unresolved `enif_*`/`driver_*`
  import fails the load. `nif_variadic.rs` is gated on `cfg(beam_abi_variadic_shim)`;
  `driver_binary_refc.rs` uses a dual-`driver_init`/`nif_init` fixture shared with the oracle run.
- **`tests/abi_conformance.rs` — the mechanical gap guard (the frozen C-ABI surface).** At test time
  it parses the real headers (`erl_nif_api_funcs.h`'s `ERL_NIF_API_FUNC_DECL` list; `erl_driver.h`'s
  `EXTERN` declarations) applying the headers' own gating (the four `#if SIZEOF_LONG != 8` `enif_*64`
  entries do not exist on LP64; the `erl_drv_steal_main_thread*` pair is `__DARWIN__`-only), and
  asserts **set equality** against the crate's `#[no_mangle]` definitions plus `csrc/nif_variadic.c`.
  It further asserts that the `ErlNifApi`/`ErlDrvApi` **field order and counts equal the header
  declaration order** (a transposed slot is an ABI break) and that **every word of
  `enif_api_table()`/`driver_api_table()` is populated**. A new header declaration with no crate
  definition FAILS the test; `KNOWN_UNIMPLEMENTED` is an explicit (currently empty) allowlist so a
  deferral must be named with a reason, never silently accepted. It resolves those headers from the
  **in-tree legacy** `erts/emulator/beam/` (`beam_dir()` = `CARGO_MANIFEST_DIR/../../../erts/emulator/
  beam`) and prints a loud `SKIP` (a vacuous pass, never a silent pass) when they are absent — so
  deleting the legacy headers would silently disable this mechanical freeze guard.
- **Oracle cross-checks that really run** (they skip loudly if `erl`/`erlc`/`cc` are absent):
  `nif_threads.rs` loads the *same* fixture `.so` from the system Erlang and compares the printed
  tuple (excluding `thread_type/0`, the documented scheduler deviation); `nif_mapiter.rs`'s
  `the_oracle_agrees_on_the_flat_map_key_order` runs the real `erl` over a generated module
  (`maps:from_list/1` → `maps:keys/1`) and requires the fixture's C-side walk order to equal it.
  `nif_resource.rs`'s expected tuples for `load_status`/`roundtrip`/`roundtrip_value`/`post_release`
  are the **measured oracle values** (OTP 29.1.1 / erts-17.1) — including the post-release read
  (`post_release/0` = `{1,42,0}`) that pins the resource-term retention rule. `dtor_probe/0` +
  `dtor_state/0` are **Rust-only**: the oracle cannot run them (its C `enif_release_resource(NULL)`/
  `enif_keep_resource(NULL)` dereference the operand and segfault), so they pin the synchronous-dtor
  substitution on the Rust side alone.
- **Loud `SKIP:`, never a silent pass.** `common::compile_so` prints a `SKIP:` banner and returns
  `None` when the C compiler or the OTP include directory is missing; every test then skips rather
  than failing or silently passing.
- **Term-lifetime pitfall (host arena).** A NIF's result term is allocated on the **host arena**
  behind the call's `NifEnv`, so it is only readable while that arena is alive. A test that drops the
  host before inspecting the term reads a use-after-free (freed words come back zeroed — the
  `tuple_size == 0` / "not a list" symptom). `nif_variadic.rs`'s `Session` therefore owns the host
  (boxed), the loaded library and the term, and reads the term only through the session.
- In-module `#[cfg(test)]` unit tests cover the seam's unsafe surface and the new families:
  `src/nif/enif/tests.rs`, `src/nif/enif/process_tests.rs`, `src/nif/enif/mapiter_tests.rs`,
  `src/nif/enif/resources_tests.rs` (the resource-term retention rule and the unexported-object
  destructor), `src/driver/drv/port_tests.rs` (a path-included child of `port`), and
  `src/sync/tests.rs`.
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
- Empirical raw outputs (module `m`, NIF entry name `m`): success `ok`; missing path → `{error,{load_failed,"Failed to load NIF library: '.../nope.so: cannot open shared object file: No such file or directory'"}}`; text file → `{error,{load_failed,"Failed to load NIF library: '.../nota.so: file too short'"}}`; major 99 → `{error,{bad_lib,"That 'm' NIF library needs erts-99.0 or newer. ..."}}`; no `nif_init` → `{error,{bad_lib,"Failed to find library init function: '.../emptysym.so: undefined symbol: _nif_init'"}}`; reload → `{error,{reload,"NIF library already loaded (reload disallowed since OTP 20)."}}`; `-on_load` calling `load_nif` succeeds and the module's NIF is callable; `apply/3` → the stub error.
- (Note: in this Nix-packaged runtime `code:lib_dir(erts,include)` points at a non-existent path; the real header is `.../lib/erlang/erts-17.1/include/erl_nif.h`.)

## NIF host layer
### `enif_*` functions (`src/nif/enif/`)
Every one is `#[no_mangle] pub unsafe extern "C"` with the exact signature of its matching
`nif::api` field and drives the `NifEnv`/`NifHost` seam (term construction through `beam_term`
writers over the host's `beam_alloc::Heap`, atoms through `NifHost::intern_atom`). By family:
- **`mem`** — libc-backed `enif_alloc`/`enif_free`/`enif_realloc`; the binary family
  (`enif_alloc_binary`/`enif_realloc_binary`/`enif_release_binary`/`enif_make_new_binary`/
  `enif_make_binary`/`enif_make_sub_binary`/`enif_inspect_binary`/`enif_inspect_iolist_as_binary`);
  the ETF pair `enif_term_to_binary`/`enif_binary_to_term` through `NifHost` (each answers the C's
  failure `0` when the host hook is absent).
- **`get`** — `enif_get_int/uint/long/ulong/double/list_cell/tuple/list_length/string/
  string_length/atom/atom_length`.
- **`make`** — `enif_make_int/uint/long/ulong/double/atom(_len)/new_atom(_len)/existing_atom(_len)/
  string(_len)/list_cell/reverse_list/ref/tuple_from_array/list_from_array`. The four make-atom
  entry points **record** each interned atom on the env; `enif_make_existing_atom(_len)` consults
  that record first (so a just-interned name resolves) then `NifHost::existing_atom`, and answers
  `0` with `*atom` untouched on a miss. `enif_make_ref` raises `badarg` when `NifHost::make_ref` is
  absent (the C never fails).
- **`map`** — `enif_get_map_size`/`enif_get_map_value`/`enif_make_new_map`/`enif_make_map_from_arrays`/
  `enif_make_map_put`/`enif_make_map_update`/`enif_make_map_remove`.
- **`pred`** — the `enif_is_*` predicates, `enif_compare`, `enif_is_identical`, `enif_hash`,
  `enif_term_type`, `enif_term_size`.
- **`proc`** — `enif_self`, `enif_get_local_pid`/`enif_get_local_port`, `enif_set_pid_undefined`/
  `enif_is_pid_undefined`, `enif_get_atom_cache_index`/`enif_max_atom_cache_index`.
- **`process`** — monitors (`enif_monitor_process`/`demonitor_process`/`compare_monitors`/
  `make_monitor_term`), liveness (`enif_is_process_alive`/`is_current_process_alive`/`is_port_alive`),
  `enif_send`, `enif_port_command`, `enif_whereis_pid`/`whereis_port` (0 when `NifHost::whereis_pid`/
  `whereis_port` is absent), `enif_schedule_nif`. All drive
  existing `NifHost` hooks and add no runtime state. The monitor payload is the C's 3-word reference
  block copied into the 32-byte `ErlNifMonitor` (bytes 24..32 zeroed).
- **`exception`** — `enif_make_badarg`, `enif_raise_exception` (sets the env's pending exception),
  `enif_has_pending_exception`, `enif_clear_env` (**leaves the pending exception intact**, as the C
  does; the env owns no temporary objects of its own, so it is otherwise a validated no-op).
- **`threads`** — the 34 mutex/cond/rwlock/tsd/thread slots, thin cast-and-forward facades over
  `crate::sync`; `enif_thread_type` and the `*_name` accessors are the only non-forwarding ones.
- **`ioq`** — `enif_ioq_create`/`destroy`/`enq_binary`/`enqv`/`size`/`deq`/`peek`/`peek_head` plus
  `enif_inspect_iovec`/`enif_free_iovec`. The queue is a `VecDeque` of buffers; the observable vector
  *count* rule (one `SysIOVec` per non-empty enqueued buffer) is preserved exactly.
- **`mapiter`** — the seven `enif_map_iterator_*` slots over the frozen 56-byte `ErlNifMapIterator`,
  walking the map in its **storage order** (flat map: the map's own key/value arrays; hash map: the C
  `hashmap_iterator_next` DFS order). The hash arm's `ErtsDynamicWStack` slot is a boxed pair list (a
  documented representation substitution; `get_pair` answers the same sequence).
- **`time`** — `enif_monotonic_time`/`enif_time_offset`/`enif_convert_time_unit`/`enif_now_time`/
  `enif_cpu_time`/`enif_system_info`/`enif_getenv`/`enif_make_unique_integer`/
  `enif_consume_timeslice`/`enif_dlopen`/`enif_dlsym`. The Erlang monotonic-time / time-offset
  relation is reproduced; `enif_time_offset/1` is pinned against the oracle.
  `enif_system_info` takes no env, so it reaches the host via `with_current_host` and prefers
  `NifHost::system_info` (falling back to the crate's built-in values); `enif_make_unique_integer`
  prefers `NifHost::unique_integer` and otherwise serves real values from a crate-internal counter.
- **`resources`** — the eleven resource slots over a crate-internal resource-type registry plus a
  real refcounted allocation (`enif_open_resource_type`/`_x`/`init_resource_type`/`alloc_resource`/
  `release_resource`/`keep_resource`/`make_resource`/`make_resource_binary`/`get_resource`/
  `sizeof_resource`/`dynamic_resource_call`). A resource *term* is an internal reference **minted by
  this module** (not through the host's `make_ref` hook, whose default is `None` on the real runtime)
  and recorded term-word → object; the C's magic-ref has no `beam_term` analogue. An exporting call
  (`enif_make_resource`/`enif_make_resource_binary`) takes **its own reference** on the object —
  exactly the reference the C magic ref holds — so a released-but-term-wrapped object stays alive and
  `enif_get_resource` keeps resolving it (oracle-verified). **The one deviation:** there is no GC, so
  that reference is never dropped — an object ever exported as a term is retained (block + payload)
  for the process lifetime and its destructor never runs; an object never exported is destroyed on its
  last release, with the destructor run **synchronously** (the C schedules it on a misc aux-work
  thread). The count/ordering — and the environment/payload the callback receives — match the
  oracle; only the timing differs. The asynchronous form **cannot be reproduced safely** here: the
  type's host borrow is a non-`Send` `*mut dyn NifHost` that is not kept alive past the call.
- **`select`** — `enif_select`/`enif_select_x` reproduce the C's argument handling and return codes
  and hand the request to `NifHost::select` (the poller is `beam_io`'s); a request no host can
  service answers `-1`.
- **`envs`** — `enif_alloc_env` (reads the thread-local current-NIF-host and returns
  `NifHost::alloc_env()`, else `NULL`), `enif_free_env` (hands the env back to `NifHost::free_env`;
  `NULL` and a pointer the host does not own are no-ops — this crate never frees memory it did not
  allocate), `enif_make_copy` (`NifHost::make_copy`; `THE_NON_VALUE` when unsupported), `enif_priv_data`
  (`NifHost::priv_data`). A use-after-`enif_free_env` is a caller-contract violation the raw-pointer
  ABI cannot detect; this crate adds no ownership tracking it cannot enforce.
- **`variadic`** — the seven C-variadic slots via the C shim (see *The C-variadic shim*).

`pub fn enif_api_table() -> &'static ErlNifApi` returns the `#[used] static ENIF_API` built with the
family `populate` chain from `ErlNifApi::empty()`. The `#[used]` static is what keeps the
`#[no_mangle]` symbols in the final link.

### Dynamic loader (`src/nif/loader.rs`)
`load(path, module, host, load_info) -> Result<NifLibrary, NifLoadError>` does `dlopen(path+".so",
RTLD_NOW|RTLD_LOCAL)`, `dlsym("nif_init")` (fallback `_nif_init`), then validates in the oracle's
exact order (the table above) and invokes `entry.load`. `NifLibrary::call(env, name, arity, args)`
looks the function up by `(name, arity)`, calls `fptr(env.as_ptr(), arity, args.as_ptr())`, and turns
a pending env exception into `NifCallError::Raised`. `Drop` runs `entry.unload` then `dlclose`.
`Reload`/`Upgrade`/`OldCode` variants exist for surface completeness but `load()` never produces them
(no module-instance / old-code state here).

### Symbol-export mechanism (load-bearing)
A NIF `.so` leaves `enif_*` and a driver `.so` leaves `driver_*`/`erl_drv_*` **undefined**, and the
dynamic linker binds them, at `dlopen(RTLD_NOW)`, against the **loading process's** dynamic symbol
table. `build.rs` emits `cargo::rustc-link-arg-tests=-Wl,--export-dynamic` so each `beam_abi` test
binary exports its globals. Proven: `nm -D --undefined-only test_nif.so` lists the used `enif_*` as
`U` and `test_driver.so` lists its `driver_*`/`erl_drv_*` as `U`; the same names appear `T` (defined)
in the test binary's `.dynsym`; and loading the same `.so` from a process that does **not** export
them fails with `undefined symbol: enif_make_int`. (`cargo::rustc-link-arg-bins` must NOT be emitted —
this package has no `[[bin]]` and cargo hard-errors.)

## Driver host layer
### `driver_*` / `erl_drv_*` functions (`src/driver/drv/`)
Every one is `#[no_mangle] pub unsafe extern "C"` with the exact signature of its matching
`driver::api` field, and lands in the `#[used] static DRIVER_API` published through
`pub fn driver_api_table() -> &'static ErlDrvApi`. `EMPTY_DRIVER_API` is
`unsafe { core::mem::zeroed() }` (the const-evaluable all-`None` base; `ErlDrvApi::empty()` is a plain
`fn` and cannot initialise a `static`) — every pointer field is null-optimized, so the all-zero struct
**is** the all-`None` table (a test pins that equality). By family:
- **`mem`** (host-free) — `driver_alloc`/`driver_free`/`driver_realloc`, the driver-binary family
  (`driver_alloc_binary`/`driver_realloc_binary`/`driver_free_binary` + the refcount trio
  `driver_binary_get_refc`/`inc_refc`/`dec_refc`) over a **hidden 16-byte per-block refcount prefix**
  (see Design Decisions), `driver_system_info`, `null_func`, `erl_errno_id`,
  `erl_drv_consume_timeslice`, `erl_drv_monotonic_time`/`erl_drv_time_offset`/
  `erl_drv_convert_time_unit`, `driver_dl_open`/`driver_dl_sym`/`driver_dl_close`/`driver_dl_error`,
  `erl_drv_putenv`/`erl_drv_getenv`, `driver_mk_term_nil` (plus the `driver_term_nil` data symbol).
- **`port`** (port/host-dependent; the `ErlDrvPort` operand **is** the `DriverEnv::as_port` pointer,
  recovered by `DriverEnv::from_port`) — the output family, the term-data family
  (`driver_output_term`/`driver_send_term`/`erl_drv_output_term`/`erl_drv_send_term` over
  `decode_term_spec`, which decodes **all 17 `ERL_DRV_*` tags** — `ERL_DRV_PID` accepts only an
  internal pid, `ERL_DRV_MAP` builds via `beam_term::map` (flat ≤ 32, else HAMT; duplicate keys
  refused), `ERL_DRV_EXT2TERM` routes through the defaulted `DriverHost::binary_to_term`), the port
  queries (`driver_mk_atom`/`driver_mk_port`/`driver_connected`/`driver_caller`), and the
  select/timer/async group.
- **`queues`** — the port-queue family (`driver_vec_to_buf`, `driver_enq`/`driver_pushq`/
  `driver_deq`/`driver_sizeq`, `driver_enq_bin`/`driver_pushq_bin`, `driver_peekq`/`driver_peekqv`,
  `driver_enqv`/`driver_pushqv`), delegating to the `DriverHost` queue hooks. The return codes were
  measured on the oracle (e.g. `driver_deq` returns the **remaining** count, `(ErlDrvSizeT)-1` when
  over-dequeued).
- **`threads`** — the 33 `erl_drv_*` thread/synchronisation slots, a self-contained cast-and-forward
  facade over `crate::sync` (no `ErlDrvPort`, hence no `DriverHost` hook: a mutex/cond/rwlock/tid/tsd
  key is an object the runtime owns outright).
- **`pdl`** — the port data lock (`driver_pdl_create`/`lock`/`unlock`/`get_refc`/`inc_refc`/
  `dec_refc` over a real mutex+atomic refcount in a process-wide registry), the driver monitors
  (`driver_monitor_process`/`demonitor_process`/`get_monitored_process`/`compare_monitors`), and the
  deprecated `driver_get_now`.
- **`lifecycle`** — the failure/exit family (`driver_failure_eof`/`_atom`/`_posix`/`driver_failure`/
  `driver_exit` collapsing onto `DriverHost::failure`/`port_exit`), the attributes (`set_busy_port`/
  `set_port_control_flags`/`get_port_flags`, round-tripping through the seam), `erl_drv_busy_msgq_limits`,
  `erl_drv_init_ack`, `erl_drv_set_os_pid`, `driver_async_port_key`.
- **`registry`** — the driver-registry slots: `add_driver_entry`/`remove_driver_entry` (port-free;
  forward to whatever host `with_current_host` published, else `-1`/`0`), `driver_lock_driver` and
  `driver_create_port` (recover the host through the shared `env_for` port helper). Ground truth
  `io.c:7640`/`7698`/`7324`/`786`.

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

## Known gaps (honest, never a wrong result)
- **`enif_alloc_env` answers `NULL` only when the current host leaves `alloc_env` at the trait
  default, or when no `NifEnv` is live (outside a NIF call).** `beam_rt`'s `NifHostImpl` implements
  `alloc_env`/`free_env`/`make_copy` (`beam_rt/src/nif.rs`), so the real runtime's message-env idiom
  works: every message env is built over the one shared pooled BIF heap, making `make_copy` the
  identity. A crate-owned env is not an option (its atoms would be unusable ⇒ a *wrong term*).
- **The NIF host hooks the runtime does not yet implement.** `NifHost::{term_to_binary,
  binary_to_term, make_ref, existing_atom, whereis_pid, unique_integer, system_info}` are defaulted to
  "unsupported", and `beam_rt`'s `NifHostImpl` leaves them so — so the matching `enif_*` entry points
  answer the C's failure (or, for `enif_make_ref`, raise `badarg`) where the oracle succeeds. Closing
  these is a `beam_rt` change: `term_to_binary`/`binary_to_term` over `beam_codec`; `make_ref` over a
  fresh reference term; `existing_atom` over the committed table **and** the in-flight
  `PendingInterner`; `whereis_pid` over the process registry; `system_info` with the emulator's real
  `erts_version`/`otp_release`/`scheduler_threads`/`dirty_scheduler_support` (the built-in fallback
  rounds `dirty_scheduler_support` to `0` where the C reports `1`). The `beam_abi` side is complete
  (C-failure/edge behaviour + the per-call atom record).
- **No scheduler threads.** `enif_thread_type` can only report `ERL_NIF_THR_UNDEFINED` (a foreign
  thread) or `ERL_NIF_THR_OTHER` (a thread created through `sync::thread_create`) — never a
  scheduler/dirty-scheduler type. An **unnamed** handle's `*_name` accessor returns `NULL` where the
  C returns the static string `"unknown"`.
- **`sync::tsd_*` passes the key straight to pthreads unchecked** (no `used_tsd_keys[]` validation);
  an invalid/destroyed key makes `set`/`get` a safe no-op (`NULL`) where the C aborts.
- **The driver-registry slots are reachable from the running runtime (`beam_rt`).** `beam_rt`'s per-port
  `DriverHost` (`beam_rt/src/port/driver.rs`) implements the three port-reachable hooks over its own
  driver-entry list: `add_driver_entry`/`remove_driver_entry` (`:705`/`:711`, against its `DRIVER_ENTRIES`
  list) and `lock_driver` (`:724`, reflecting the port's **own** loaded driver, C `io.c:7337`). Its
  callback dispatch is wrapped in [`with_current_host`] (the `publishing` helper, `:570`), so the two
  port-free slots reach the host too.
- **`driver_create_port` is not plumbed — a hard-coded failure sentinel.** The hook needs a live
  `PortRegistry`+`Poller`+driver-table reachable from a per-port `DriverHost`, which no host supplies
  (the registry is owned elsewhere; a re-entrant open would alias the borrow the running callback was
  dispatched through), so a `beam_rt` host returns the invalid-port sentinel
  (`beam_rt/src/port/driver.rs:741-749`). A fabricated port handle would be a wrong value, so the C's
  own single failure answer is the honest one.
- **The `driver_binary_*_refc` trio refuses a NULL/foreign operand with `-1`** where the C would
  recover a bogus `Binary*` and crash — the crate's "refuse rather than fault" policy (the count
  itself is oracle-exact).
- **`enif_ioq_is_empty` does not exist in this OTP tree** (the `enif_ioq_*` slots in `api.rs` are the
  eight in `ioq.rs` plus `enif_inspect_iovec`/`enif_free_iovec`).
- `driver_select`/`driver_set_timer`/`driver_cancel_timer`/`driver_read_timer`/`driver_async` forward
  to `DriverHost`, whose **defaults** report "unsupported" (`-1`/`None`): a host only records the
  request, so "the OS poller really woke the port" is a `beam_io`/`beam_rt` concern, outside this
  crate.
- `enif_make_map_*`/`enif_compare`/`enif_is_identical`/`enif_hash` take no env, hence no host/atom
  table: identical terms take a raw-word fast path and distinct **atoms** fall back to `beam_term`'s
  index order (exact for non-atom keys, the common NIF case).
- Integers: small ints + one-limb bignums; a wider value makes the accessor return `0` rather than
  truncate. `enif_make_binary` builds an **on-heap** binary (off-heap refc binaries are not exposed by
  the seam). `enif_inspect_binary` refuses a *bit*-offset base instead of fabricating an unaligned
  pointer.
- Malformed-term guard: a bare `THE_NON_VALUE` / `HEADER`-tagged word is rejected at the
  term-inspecting entry points (the C would deref it); a *forged* pointer-tagged word is still
  undetectable from the word alone, exactly as in the C.

## Routing Table
- `build.rs` → the build script: `-Wl,--export-dynamic` on the test targets (a NIF/driver `.so` resolves `enif_*`/`driver_*` out of the loading process) **plus** compiling `csrc/nif_variadic.c` (`$CC`/`$AR`, no `cc` crate) and emitting `cfg(beam_abi_variadic_shim)` + `rustc-link-search`/`rustc-link-lib=static`
- `csrc/nif_variadic.c` → the C-variadic `enif_*` shim (the only C in the crate): owns the `va_list` handling of the seven variadic slots and forwards into Rust
- `src/lib.rs` → crate docs, module wiring, root re-exports (`NifEnv`/`NifHost`, `DriverEnv`/`DriverHost`, `Eterm`), the ABI **version constants** (`ERL_NIF_*`, `ERL_DRV_EXTENDED_*`, `ERL_NIF_VM_VARIANT`, `ERL_NIF_MIN_ERTS_VERSION`)
- `src/common.rs` → `erl_drv_nif.h` shared types: integer typedefs, `SysIOVec`, `ErlDrvSysInfo`, `ErlDrvThreadOpts`, `ErlDirtyJobFlags`, `ErlNifSelectFlags` + `ERL_NIF_SELECT_*`, `ErlDrvMonitor`, time constants
- `src/sync/` → the in-crate pthread facade (`Mutex`/`Cond`/`RwLock`/`TsdKey`/`Tid` + `mutex_*`/`cond_*`/`rwlock_*`/`tsd_*`/`thread_opts_*`/`thread_*`) both thread families forward to, plus `tests.rs`
- `src/nif/term.rs` → `ERL_NIF_TERM`/`ERL_NIF_UINT`, `Eterm ↔ ERL_NIF_TERM` transmute seam
- `src/nif/types.rs` → NIF `#[repr(C)]` structs/enums/flags/fn-type aliases (from `erl_nif.h`), incl. the opaque `ErlNifEnv`
- `src/nif/api.rs` → `ErlNifApi` (176-entry `enif_*` function table + `ERL_NIF_API_FIELD_COUNT`; generated, header-ordered)
- `src/nif/host.rs` → the host seam: `NifHost` (heap/atoms + the defaulted service hooks) + `NifEnv` (per-call env behind `*mut ErlNifEnv`, exception slot, `as_ptr`/`from_opaque`; `new` publishes the host on the thread-local current-host stack, `Drop` pops it) + the `pub(crate)` `with_current_host` reader behind `enif_alloc_env`
- `src/nif/enif/` → **one file per `enif_*` family** (`mem`, `get`, `make`, `map`, `pred`, `proc`, `process`, `exception`, `threads`, `ioq`, `mapiter`, `time`, `resources`, `select`, `envs`, `variadic`), each owning its `pub(super) const fn populate`; `mod.rs` holds the shared helpers, the submodule wiring and the `ENIF_API` chain; `tests.rs`/`process_tests.rs`/`mapiter_tests.rs`/`resources_tests.rs` are test-only children
- `src/nif/loader.rs` → the dynamic NIF loader: `NifLibrary`, `NifLoadError`/`NifCallError`, `load()` (dlopen/dlsym + oracle-order validation), `NifLibrary::call`, `Drop` (unload + dlclose)
- `src/driver/types.rs` → driver `#[repr(C)]` structs/enums/flags + `ErlDrvEntry` (from `erl_driver.h`) + `ERTS_INVALID_ERL_DRV_PORT` (the invalid-port sentinel, pointer `-1`)
- `src/driver/api.rs` → `ErlDrvApi` (111-entry `driver_*`/`erl_drv_*` function table) + `ErlDrvApi::empty()`
- `src/driver/host.rs` → the driver host seam: `DriverHost` (heap/atoms/output/send_term + the defaulted service hooks, incl. the driver-registry hooks `add_driver_entry`/`remove_driver_entry`/`lock_driver`/`create_port`) + `DriverEnv` (per-port env behind `*mut ErlDrvPort_`, `as_port`/`from_port`) + `decode_term_spec` (the reverse-Polish term-data decoder) + the `with_current_host` re-export
- `src/driver/current_host.rs` → the thread-local **current-driver-host** publication: `with_current_host(host, f)` (scoped, restores the previous host on exit, nesting-safe) plus the `pub(crate)` reader the two port-free entry-list slots use
- `src/driver/drv/` → **one file per driver family** (`mem`, `port`, `queues`, `threads`, `pdl`, `lifecycle`, `registry`), each owning its `pub(super) const fn populate`; `mod.rs` holds the module docs, the shared `env_for` helper, the `#[used] static DRIVER_API` table + `driver_api_table()`, the `driver_term_nil` data symbol and `binary_payload`; `port_tests.rs` is the path-included child of `port`
- `src/driver/loader.rs` → the dynamic driver loader: `DriverLibrary`, `DriverLoadError`/`DriverCallError`, `load()` (dlopen/dlsym `driver_init` + oracle-order `ErlDrvEntry` validation), the per-callback wrappers, `Drop` (finish + dlclose)
- `src/layout.rs` → the compile-time layout freeze (all `const _: () = assert!(...)` for the table above)
- `tests/common/mod.rs` → shared test helpers (NOT a test target: a subdirectory module pulled in with `mod common;`): `erl_include_dir`, `cc_available`, `compile_so`, `skip`, `TestHost`, `TestDriverHost`
- `tests/` → crate-level integration tests (`layout.rs`, `term_abi.rs`, `api_surface.rs`, `abi_conformance.rs`, `entry.rs`, `host_seam.rs`, `enif.rs`, `loader.rs`, `nif_end_to_end.rs`, `nif_process.rs`, `nif_resource.rs`, `nif_env.rs`, `nif_ioq.rs`, `nif_mapiter.rs`, `nif_threads.rs`, `nif_variadic.rs`, `nif_atom_env.rs`, `driver_end_to_end.rs`, `driver_loader.rs`, `driver_binary_refc.rs`, `driver_lifecycle.rs`, `driver_queues.rs`, `driver_pdl.rs`, `driver_threads.rs`) + `tests/fixtures/*.c` (real C NIFs/drivers), all compiled at test time with the system `cc` against the system OTP headers

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
- **A family's slots live in that family's file.** To extend a family, edit the family file's body and
  add the slot to its `populate`; do not touch the `ENIF_API`/`DRIVER_API` chain in `mod.rs`.
- Building: **never `cd`**; use `../scripts/cargo.sh build -p beam_abi` /
  `../scripts/cargo.sh test -p beam_abi` (the sandbox `$HOME` is read-only; the wrapper picks a
  writable `CARGO_HOME`).
- `tests/common/mod.rs` is invisible to `cargo test` unless some `tests/*.rs` says `mod common;` —
  many files do, so keep at least one includer; Cargo would in particular *not* report a
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
- **Term lifetime:** a term produced by a NIF lives on the host arena; keep the host (and the loaded
  library) alive while inspecting it, or read a use-after-free (see the Test Strategy pitfall).

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, layering waves (§0 places `beam_abi` in wave 0).
- `../../CONTEXT.md` → workspace contracts: §1 (A.3 NIF C ABI, A.4 driver C ABI), §2 frozen term layout, §3 error strategy.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.3/A.4; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §C (the frozen-header seam).
- `../beam_term/` → the internal `Eterm` this crate transmutes to/from (`nif::term`).
- `../beam_alloc/` → the production bump `Heap` the host seam hands out (`NifHost::heap`).
- `../beam_rt/`, `../beam_bifs/`, `../beam_io/` → the runtime owners of the `NifHost`/`DriverHost` seams and the implementors of the `DriverHost` behaviours behind the `driver_*` seams (the poller, ports and drivers). `beam_io` implements the driver-registry hooks over its driver table / port registry and wraps each driver callback **it** routes in `beam_abi::with_current_host`. `beam_rt` uses its own per-port `DriverEnv`-backed host, which implements `self_pid`/`alloc_env`/`free_env`/`make_copy` on the NIF side and `add_driver_entry`/`remove_driver_entry`/`lock_driver` on the driver side (only `create_port` and the remaining runtime-service NIF hooks stay at their defaults — see *Known gaps*).
