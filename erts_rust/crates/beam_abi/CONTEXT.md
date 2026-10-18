# `beam_abi` — Layout-Frozen NIF & Driver C ABI

## Intent
Owns the **frozen foreign-function C ABIs** Rust ERTS exposes to native code: the NIF C ABI (contract
**A.3**) and the driver C ABI (**A.4**) — the legacy headers' exact struct layouts, enum widths,
fn-pointer signatures and constants, so a NIF/driver `.so`/`.o` built against the *unmodified* OTP
headers links against Rust ERTS and behaves identically. Part II §C's **link-time frozen-header seam**:
a layout mismatch is an ABI break. The crate freezes the boundary's *shape* and **implements both host
sides**: the **NIF host layer** (`src/nif/enif/`, `enif_*`, `nif::host`, `nif::loader`) and the **driver host layer** (`src/driver/drv/`, `driver_*`, `erl_drv_*`, `driver::host`, `driver::loader`), over the shared `src/sync/` pthread facade
(*Routing Table*).

Ground truth (read-only, never modify): `../../erts/emulator/beam/erl_nif.h`, `erl_nif_api_funcs.h`,
`erl_drv_nif.h`, `erl_driver.h` — the only stable ABI headers. Per-OS `driver_int.h` and the
Windows-specific paths are **out of scope**.

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
  Debug + Default`; ABI-identical to `beam_term::Eterm` and to the C `ERL_NIF_TERM` (`unsigned long`
  on LP64, one machine word). `pub type ERL_NIF_UINT = ERL_NIF_TERM` (the header's alias).
- `eterm_from_nif(ERL_NIF_TERM) -> Eterm` / `nif_from_eterm(Eterm) -> ERL_NIF_TERM` /
  `ERL_NIF_TERM::raw(self) -> usize` / `from_raw(usize)` — all `pub const fn`, safe, over
  `Eterm::raw()`/`Eterm::from_raw()`; **no `mem::transmute`** (`usize`).

### NIF types (`nif::types`) — `#[repr(C)]`, field order exactly as `erl_nif.h`
`ErlNifFunc`, `ErlNifEntry`, `ErlNifBinary`, `ErlNifPid`, `ErlNifPort`, `ErlNifResourceTypeInit`,
`ErlNifMapIterator` (`ErlNifMapIteratorFlat`, `ErlNifMapIteratorHash`, `ErlNifMapIteratorUnion`),
`ErlNifIOVec`, `ErlNifSysInfo` (= `ErlDrvSysInfo`); opaque handles `ErlNifEnv`,
`ErlNifResourceType`, `ErlNifIOQueue`, `ErlNifTid`, `ErlNifMutex`, `ErlNifCond`, `ErlNifRWLock`;
scalars `ErlNifUInt64`/`ErlNifSInt64`/`ErlNifUInt`/`ErlNifSInt`, `ErlNifTime`, `ErlNifEvent`,
`ErlNifTSDKey`, `ErlNifThreadOpts`, `ErlNifMonitor`; function types `ErlNifResourceDtor`/`Stop`/
`Down`/`DynCall`, `ErlNifOnHaltCallback`, `ErlNifOnUnloadThreadCallback`; enums `ErlNifCharEncoding`,
`ErlNifTimeUnit`, `ErlNifMapIteratorEntry`, `ErlNifHash`, `ErlNifTermType`, `ErlNifOption`,
`ErlNifIOQueueOpts`; flag types `ErlNifResourceFlags`, `ErlNifSelectFlags`, `ErlNifUniqueInteger`,
`ErlNifBinaryToTerm`, `ErlNifDirtyTaskFlags`.

### `ErlNifApi` (`nif::api`) — the NIF function table (the frozen `enif_*` surface)
A `#[repr(C)]` struct of the **176** `enif_*` fn pointers that exist on LP64, in exact
`erl_nif_api_funcs.h` declaration order — the Rust analogue of the header's Windows-only
`TWinDynNifCallbacks`. Derivation: `ERL_NIF_API_FUNC_DECL` occurs 185× but only 180 are declarations
(5 guard/comment lines); the 4 `#if SIZEOF_LONG != 8` entries
(`enif_get_int64`, `enif_get_uint64`, `enif_make_int64`, `enif_make_uint64`) do not exist on LP64 (`#define`, `long`, `ulong`) ⇒ 180 − 4 = **176**; macro-only
`enif_ioq_enq` (`ERL_NIF_API_FUNC_MACRO` only, no `DECL`/signature/definition) is excluded.
`size_of::<ErlNifApi>() == 176 * 8 == 1408` asserted at compile time; `ERL_NIF_API_FIELD_COUNT ==
176` in `nif::enif`.

### Driver types (`driver::types`) — `#[repr(C)]`, exactly as `erl_driver.h`
`ErlDrvEntry`, `ErlDrvBinary`, `ErlIOVec`, `ErlDrvNowData`; opaque handles `ErlDrvPort`,
`ErlDrvEvent`, `ErlDrvData`, `ErlDrvThreadData`, `ErlDrvPDL`, `ErlDrvTid`/`ErlDrvMutex`/`ErlDrvCond`/
`ErlDrvRWLock` (+ `_` marker structs); scalars `ErlDrvUInt64`/`ErlDrvSInt64`/`ErlDrvUInt`/`ErlDrvSInt`,
`ErlDrvTermData`, `ErlDrvSizeT`, `ErlDrvSSizeT`, `ErlDrvTime`, `ErlDrvTSDKey`; enum `ErlDrvTimeUnit`;
flag newtype `ErlDirtyJobFlags`; constants `ERL_DRV_*` (term-data tags, `ERL_DRV_FLAG_*`,
`PORT_*`, `ERL_DRV_READ/WRITE/USE*`, `DRIVER_CALL_KEEP_BUFFER`, `ERL_DRV_ERROR_*`,
`ERL_DRV_BUSY_MSGQ_*`).

### `ErlDrvApi` (`driver::api`) — the driver function table (the frozen `driver_*`/`erl_drv_*` surface)
A `#[repr(C)]` struct of the **111** fn pointers for every function the *runtime provides* to drivers
(`driver_output`, `driver_select`, `erl_drv_mutex_*`, `erl_drv_thread_*`, `driver_alloc*`, `driver_enq*`), fields `Option<unsafe extern "C" fn ...>`
(null-optimized) in `erl_driver.h` order (single-table export shape: *Design Decisions*).

### Shared (`common`) — `erl_drv_nif.h`
`ErlNapiUInt64`/`ErlNapiSInt64`/`ErlNapiUInt`/`ErlNapiSInt`, `SysIOVec`, `ErlDrvSysInfo`,
`ErlDrvThreadOpts`, `ErlDirtyJobFlags`, `ErlNifSelectFlags`, `ErlDrvMonitor`, the `ERTS_NAPI_*` time
constants, and `ERL_NIF_SELECT_*` / `ErlNifSelectFlags` selector bits.

### Per-family implementation status (MEASURED at this node's HEAD — `Some`/total slots)
`ErlNifApi` **176/176** populated, `ErlDrvApi` **111/111** (no `None` on any LP64 slot);
`tests/abi_conformance.rs` re-derives both counts from the headers.

**NIF families** (`src/nif/enif/*.rs`):
| family file | slots it owns | `Some`/total |
|---|---|---|
| `mem` | alloc, binaries, ETF | 13/13 |
| `get` | `enif_get_*` accessors | 12/12 |
| `make` | `enif_make_*` constructors (maps excepted) | 18/18 |
| `map` | map accessors/constructors | 7/7 |
| `pred` | `enif_is_*`, `compare`, `is_identical`, `hash`, `term_type`, `term_size` | 17/17 |
| `proc` | self/pid/port, atom-cache queries | 7/7 |
| `process` | monitors, send, port command, whereis, `schedule_nif` | 12/12 |
| `exception` | badarg/raise/pending/clear | 4/4 |
| `threads` | mutex/cond/rwlock/tsd/thread | 34/34 |
| `ioq` | I/O queue + I/O vector | 10/10 |
| `mapiter` | `enif_map_iterator_*` | 7/7 |
| `time` | time, env, system info, unique integers, timeslice, `dl*` | 11/11 |
| `resources` | resource types + allocations | 11/11 |
| `select` | `enif_select`/`enif_select_x` | 2/2 |
| `envs` | `enif_alloc_env`/`enif_free_env`/`enif_make_copy`/`enif_priv_data` | 4/4 |
| `variadic` | the seven C-variadic slots (cfg `beam_abi_variadic_shim`) | 7/7 |

The seven C-variadic slots are `Some` only with the `csrc/nif_variadic.c` shim compiled (else `None`,
a loud `dlopen` failure, never a wrong result).

**Driver families** (`src/driver/drv/*.rs`):
| family file | slots it owns | `Some`/total |
|---|---|---|
| `mem` | host-free alloc/time/`dl*`/errno/`driver_mk_term_nil` + the driver-binary family (incl. the refcount trio) | 23/23 |
| `port` | output, term-data, port queries, select/timer/async | 17/17 |
| `queues` | the port-queue family | 11/11 |
| `threads` | the `erl_drv_*` thread/synchronisation family (over `crate::sync`) | 33/33 |
| `pdl` | port data lock + driver monitors + `driver_get_now` | 11/11 |
| `lifecycle` | failures/exit/attributes/asynchronous init | 12/12 |
| `registry` | the driver-registry slots (`add_driver_entry`, `remove_driver_entry`, `driver_lock_driver`, `driver_create_port`) | 4/4 |

The three `driver_binary_*_refc` slots read a **hidden per-block refcount header** (*Design
Decisions*).

### Module layout
`src/nif/enif/` and `src/driver/drv/` are **directory modules, one file per family**: each family file
owns `pub(super) const fn populate(api) -> api` filling in **only its own** slots, and the `mod.rs`
chains the `populate`s **innermost→outermost** into the `#[used] static ENIF_API`/`DRIVER_API` published
by `enif_api_table()`/`driver_api_table()` — extending a family edits that family's file, never the
`mod.rs` (a family's slots live in that family's file). `src/sync/` is the **pthread facade**
(`erl_drv_thread.c`'s equivalent) both thread families delegate to (deviations: `sync/mod.rs`, *Known
gaps*); `csrc/nif_variadic.c` is the C-variadic shim compiled by `build.rs`.

### The host seam (`nif::host`) — the runtime services behind the `enif_*` layer
The *internal* boundary the `enif_*` bodies are written against (not part of the C ABI; `nif::host.rs`).
- `pub trait NifHost` — **required** `heap() -> &mut beam_alloc::Heap`, `intern_atom(&str) ->
  Option<Eterm>`, `atom_name(Eterm) -> Option<String>`; **defaulted** (default = unsupported):
  `self_pid`, `term_to_binary`, `binary_to_term`, `make_ref`, `existing_atom`, `priv_data`,
  `system_info`, `unique_integer`, `consume_timeslice`, `is_process_alive`, `is_port_alive`,
  `is_current_process_alive`, `send`, `port_command`, `whereis_pid`, `whereis_port`,
  `monitor_process`, `demonitor_process`, `select`, `schedule_nif`, `alloc_env`, `free_env`,
  `make_copy`, `dynamic_resource_call`. The ETF hooks give `beam_abi` **no** `beam_codec` edge.
- `pub struct NifEnv` — the env behind the C-facing opaque `*mut ErlNifEnv`: `new(&mut dyn NifHost)`,
  `host()`, `heap()`, `as_ptr()`, `unsafe from_opaque(*mut ErlNifEnv) -> Option<&mut NifEnv>`, the
  pending-exception slot (`pending_exception`/`set_exception`/`clear`), and a **bounded per-call atom
  record** (`record_atom`; read back by `recorded_atom_name`/`recorded_atom`; `MAX_RECORDED_ATOMS =
  256`, first-wins dedupe) — atoms commit lazily, so `NifHost::atom_name` (committed table only)
  cannot see one interned earlier in the *same* call. Re-exported as `beam_abi::{NifEnv, NifHost}`.
  `enif_clear_env` does **not** touch the exception slot (as the C, `erl_nif.c:700-723`).
- **Thread-local current-host stack + per-call atom record:** `NifEnv::new` pushes the host and its
  `Rc<RefCell<Vec<(Eterm, String)>>>` record; `Drop` pops exactly its own entry (an out-of-order drop
  restores the enclosing host). Readers `with_current_host<R>(f) -> Option<R>` (behind env-less
  `enif_alloc_env`) and `current_recorded_atom_name(atom) -> Option<String>` (innermost env wins;
  behind every env-less name resolution). Sound: `NifEnv` is `!Clone`/`!Send` (raw host pointer, never
  moved across threads), and the `Rc<RefCell<…>>` record stays readable while the env is borrowed — no
  `unsafe`, no `&mut` aliasing.

### The driver host seam (`driver::host`) — the runtime services behind the `driver_*` layer
The *internal* boundary the `driver_*` bodies are written against (source `src/driver/host.rs`).
- `pub trait DriverHost` — **required** `heap() -> &mut beam_alloc::Heap`, `intern_atom(&str) ->
  Option<Eterm>`, `output(port, &[u8]) -> c_int`, `send_term(port, Option<Eterm>, Eterm) -> c_int`;
  **defaulted** (`-1`, `None`): `output2(port, header: &[u8], body: Option<&[u8]>) -> c_int` (the
  split-reply hook every `driver_output*` thunk routes through — see *The split hook*),
  `select`, `set_timer`, `cancel_timer`, `read_timer`, `r#async`,
  `connected_pid`, `caller_pid`, `binary_to_term` (the ETF decode behind `ERL_DRV_EXT2TERM`), the
  port-queue hooks `enq`/`pushq`/`deq`/`sizeq`/`peekq`, the attribute hooks
  `set_busy_port`/`set_control_flags`/`get_port_flags`, the failure/lifecycle hooks
  `failure`/`port_exit`/`busy_msgq_limits`/`init_ack`/`set_os_pid`/`async_port_key`, the monitor hooks
  `monitor_process`/`monitor_process_result`/`demonitor_process`/`get_monitored_process`, the PDL hooks
  `pdl_create`/`pdl_lock`/`pdl_unlock`/`pdl_get_refc`/`pdl_inc_refc`/`pdl_dec_refc`, the
  driver-registry hooks `add_driver_entry`/`remove_driver_entry`/`lock_driver`/`create_port`, `now`.
- **The split hook (`output2`).** The C never hands the port owner one flat byte string: every
  `driver_output2`/`driver_output_binary`/`driver_outputv` reply is `{Port,{data,Reply}}` with
  `Reply = [header | Body]` — the `hbuf`/`hlen` bytes as a list prefix and the `buf`/`len` bytes as
  the tail (`deliver_read_message`, `io.c:3363-3442`; `deliver_vec_message`, `:3502-3621`), which is
  observable to Erlang code (`[h…|<<tail>>]` vs a flattened list). `output2(port, header, body)`
  carries that boundary: `body: None` is the C's `buf == NULL` (**no** tail — a proper header-only
  list) and `body: Some(&[])` a **present, empty** tail (`<<>>` in `[binary]` port mode) — different
  terms. The C tests the **pointer**, never `len != 0`.
  **Default = byte-identical to the pre-hook behaviour:** exactly one `output` call carrying
  header-then-body (`None` counted as empty), no allocation when either side is empty, so a host
  that overrides only `output` keeps the old flattened result. Overriding `output2` is what
  preserves the split. Additive only — no `#[repr(C)]`/table/assert moved.
- **The three-way monitor hook.** `monitor_process(&mut self, pid) -> Option<ErlDrvMonitor>` is the
  **two-way compatibility subset** (`Some` ⇒ the C's `0`, `None` ⇒ `-1`) kept for source
  compatibility; the full three-way hook is `monitor_process_result(&mut self, pid) -> MonitorResult`
  and its **default delegates** to `monitor_process` (`Some(mon)` ⇒ `MonitorResult::Ok(mon)`, `None`
  ⇒ `MonitorResult::Error`), so an existing two-way implementor is unchanged. `pub enum MonitorResult
  { Ok(ErlDrvMonitor), NotAlive, Error }` (`#[derive(Debug, Clone, Copy)]` — `ErlDrvMonitor` has no
  `PartialEq`) is the driver A.4 monitor result — distinct from the NIF monitor family
  (`NifHost::monitor_process`/`enif_monitor_process`) — and mirrors the C's `0` / `> 0` / `< 0`
  reason codes (`io.c:7012-7038`): `Ok` ⇒ `0` (`ERL_DRV_MONITOR_OK`), `NotAlive` ⇒ `1`
  (`ERL_DRV_MONITOR_NOT_ALIVE`, `io.c:7031`), `Error` ⇒ `-1` (`ERL_DRV_MONITOR_ERROR`,
  `io.c:7020-7021`). `driver_monitor_process` maps the enum onto those three codes (and pins their
  signs with `const _: () = assert!(…)` in `src/driver/drv/pdl.rs`). Re-exported at the crate root
  (`beam_abi::MonitorResult`).
- `pub struct DriverEnv` — the env behind the C-facing opaque `*mut ErlDrvPort_`: `new(&mut dyn
  DriverHost)`, `host()`, `heap()`, `as_port()`, `unsafe from_port(*mut ErlDrvPort_) -> Option<&mut
  DriverEnv>`; **the `ErlDrvPort` a runtime passes to a driver callback *is* the `DriverEnv::as_port`
  pointer** (the `NifEnv` analogue); plus `pub fn decode_term_spec(&mut DriverEnv, &[ErlDrvTermData])
  -> Option<Eterm>`, the reverse-Polish term-data decoder behind `erl_drv_output_term`/
  `erl_drv_send_term`.
- **Current-driver-host publication (`driver::current_host`).** `add_driver_entry`/`remove_driver_entry`
  carry no port operand, so `DriverEnv::from_port` cannot recover a host for them (they are meaningful
  only inside a driver callback); `beam_io` publishes the host for one callback via `pub fn
  with_current_host<R>(host: &mut dyn DriverHost, f: impl FnOnce() -> R) -> R` — a scoped guard in a
  thread-local `Cell<Option<*mut dyn DriverHost>>` that **always restores the previous host on exit**
  (`f`); with no host published the two
  entry-list slots answer `-1`/`0` without dereferencing their operand.
- `driver::drv` implements the `driver_*`/`erl_drv_*` functions and publishes them through
  `driver_api_table()`; `driver::loader` the dynamic loader / `ErlDrvEntry` contract (**Driver host
  layer**).

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

Every C enum-with-values type (`ErlNifCharEncoding`, `ErlNifTimeUnit`, `ErlNifMapIteratorEntry`, `ErlNifHash`, `ErlNifTermType`, `ErlNifOption`, `ErlNifIOQueueOpts`, `ErlDrvTimeUnit`) is a 4-byte
`#[repr(C)]` enum, every flag type a 4-byte `#[repr(transparent)]` newtype over `c_int`/`c_uint`
(both asserted).

## Constraints
- **Layout is FROZEN.** Every `#[repr(C)]` struct/enum and constant matches the legacy header; a change
  needs a `../../../CHANGE.md` entry and a note here.
- **Compile-time asserts are mandatory** for every type above: `const _: () =
  assert!(core::mem::size_of::<T>() == N);` + `assert!(core::mem::offset_of!(T, f) == N);` (`core::mem`, `static_assertions`). A
  mismatch is a **compile error**, not a test failure.
- **Dependencies:** `beam_common`, `beam_term`, **`beam_alloc`** (`NifHost::heap`), **`libc`** — deliberately **no `beam_codec`** (ETF
  goes through `NifHost`). The C-variadic shim additionally needs a **C compiler** (`$CC`, default `cc`)
  and an archiver (`$AR`, default `ar`), both **optional** (absence degrades, it does not fail the
  build).
- **`build.rs`:** (1) `cargo::rustc-link-arg-tests=-Wl,--export-dynamic` (*Symbol-export mechanism*);
  (2) compiles `csrc/nif_variadic.c` and emits `cfg(beam_abi_variadic_shim)` +
  `rustc-link-search`/`rustc-link-lib=static`.
- **`unsafe` discipline:** every FFI declaration is documented, and every `unsafe` block
  (`NifEnv`, `DriverEnv`) carries a preceding `// SAFETY:`
  rationale; `unsafe_op_in_unsafe_fn` stays clean.
- **The raw-pointer ABI wrappers intentionally `allow(clippy::not_unsafe_ptr_arg_deref)`**: the
  `DriverLibrary` callback wrappers (`src/driver/loader.rs`, `start`, `control`, `call`, `stop`, `output`, `outputv`, `timeout`, `ready_input`, `ready_output`, `ready_async`, `flush`, `process_exit`, `stop_select`, `emergency_close`) and `sync::thread_exit` are
  **safe** public fns taking the C `ErlDrvEntry`/pthread raw pointers, forwarded verbatim (pointer
  validity is the caller's contract, as in the C); the rationale sits in a module-scoped attribute
  there and a function-scoped one on `thread_exit`, since `unsafe fn` would break the frozen
  ABI-mirroring API `beam_rt`/`beam_io` consume. `cargo clippy -p beam_abi --all-targets --no-deps` is
  error-free.
- **Nullability:** C fn-pointer fields are `Option<unsafe extern "C" fn …>` (null-pointer optimized,
  same size/align as a raw pointer); data pointers stay raw `*mut T`/`*const T`.
- **Opaque handles** are `#[repr(C)] pub struct X { _private: [u8; 0] }` (incomplete C type), used
  behind pointers — never by value, never dereferenced in this crate.
- Files target ~1000 lines, split by responsibility; `nif::api`/`driver::api` are legitimately large
  **generated tables** (see their top-of-file notes), and `src/nif/enif/tests.rs`, `src/sync/tests.rs`,
  `tests/common/mod.rs` are test-only modules.

## Design Decisions
- **`ERL_NIF_TERM` is a `#[repr(transparent)]` newtype over `usize`, conversions *safe*.** The header
  defines it as `unsigned long` (`ErlNapiUInt`), bit-identical to `usize` on LP64; the seam is
  `Eterm::raw()`/`Eterm::from_raw()` (`const fn`) rather than `mem::transmute`, so the mapping is total,
  safe and explicit, and a layout test pins size/align equality to `usize` and `Eterm` (A.3: the
  internal term is exactly the NIF term).
- **Flag sets are `#[repr(transparent)]` newtypes, not `#[repr(C)]` enums.** `ErlNifResourceFlags`
  (`CREATE|TAKEOVER`), `ErlNifUniqueInteger`, `ErlNifSelectFlags`, `ErlNifDirtyTaskFlags`,
  `ErlDirtyJobFlags`, `ErlNifBinaryToTerm` are bit-wise OR-combined, which a field-less Rust enum cannot
  hold without UB; a `c_int`/`c_uint` newtype is ABI-identical (4 bytes, asserted) and carries the named
  constants as associated `const`s. *Pure* enumerations keep the `#[repr(C)]` form.
- **`va_list` parameters are modelled as `*mut VaListTag`** (opaque marker): no stable Rust `va_list`,
  and a `va_list` *parameter* is passed indirectly on the supported ABIs, so a pointer-sized opaque type
  is ABI-correct for `enif_vfprintf`/`enif_vsnprintf` (the only two affected entries).
- **`ErlNifApi`/`ErlDrvApi` are declared tables, not C structs.** On Linux the headers expose these as
  *exported free functions* resolved via `dlsym` (only Windows wraps them in `TWinDynNifCallbacks`);
  `#[repr(C)]` fn-pointer tables are the Rust export/consume shape and make the frozen signature list
  explicit and compile-checked.
- **The C-variadic `enif_*` slots live in a small C shim**  — *The C-variadic shim*.
- **Single definitions, no duplication:** `ErlDrvSysInfo` is the one definition, `ErlNifSysInfo` a
  `pub use` alias (the header's `typedef ErlDrvSysInfo ErlNifSysInfo;`); `SysIOVec` aliases `libc::iovec`
  (the header uses `struct iovec` from `<sys/uio.h>`) for `writev`/`readv` compatibility,
  layout-asserted at size 16 / offsets 0, 8.
- **A driver binary carries a hidden refcount header, exactly as ERTS does.** The frozen `ErlDrvBinary`
  is only `{ orig_size, orig_bytes }`, but real ERTS reaches the count through an emulator-internal
  `Binary` header *preceding* it (`Binary2ErlDrvBinary`, `ErlDrvBinary2Binary`, `erl_binary.h:173-176`),
  so `driver_alloc_binary` allocates an 8-byte `AtomicI64` refcount word before the public struct
  (`DRIVER_BINARY_PREFIX == 16`), keeping the returned pointer 8-aligned (`ERTS_CHK_BIN_ALIGNMENT`) with
  the frozen layout untouched. Alloc refc = 1; `inc`/`dec` return the NEW value and `dec` never frees
  (only `driver_free_binary` releases, at zero) — oracle-pinned by `tests/driver_binary_refc.rs`.
- **Target = LP64 only.** The Windows-specific layout branches (`TWinDynNifCallbacks`, `ERL_SYS_DRV`
  `start` signature, `WinDynNifCallbacks`) are intentionally not modelled (the layout asserts would fail
  on a non-LP64 target).
- **No shared low-level `dlopen` wrapper — every dynamic-loading site calls `libc::dl*` directly.**
  `nif::loader` and `driver::loader` each own a **private** `dl_last_error`/`dl_sym`/`OpenHandle` trio
  (deliberate mirrors, `fn` not `pub fn`); the frozen slots forward to `libc` too —
  `driver_dl_open`/`_sym`/`_close`/`_error` (`driver/drv/mem.rs`), `enif_dlopen`/`enif_dlsym`
  (`nif/enif/time.rs`, `dlsym_checked`, `last_dl_error`). No `ffi`/`sys`/dynlib module and
  no *public* Rust `dlopen` helper; only `beam_rt` consumes the safe loaders
  (`port::driver::load_driver`, `driver::loader::load`, `NifBridge`, `nif::loader::load`).
- **`NifEnv`/`DriverEnv` erase their host borrow's lifetime at construction.** `new(&mut dyn
  NifHost)`/`new(&mut dyn DriverHost)` must keep the spec's signature, but the C-facing env has no
  lifetime parameter — a raw pointer to a trait object is invariant and defaults to `'static`
  (rust#141402) — so `new` does one documented `mem::transmute` of the layout-identical borrow (only the
  erased lifetime is reinterpreted); `host` reproduces the caller-owned borrow exactly, and "keep the
  host alive while the env is used" is the same caller contract as `from_opaque`/`from_port`. Neither
  env type is `Clone`.
- **The created-thread name registry is first-wins.** `thread_create` and the new thread's own
  `thread_start` both register the same tid's name via `register_thread_name`, which keeps the first
  entry and never replaces it, so a buffer handed out by `thread_name` is never freed early.

## The C-variadic shim (mechanism)
- The seven slots (`enif_make_tuple`, `enif_make_list`, `enif_set_option`, `enif_fprintf`, `enif_snprintf`, `enif_vfprintf`, `enif_vsnprintf`) live in `csrc/nif_variadic.c`.
- Rust cannot read a `va_list`, so it *drives* the extraction: for the `*printf` family the shim calls
  `beam_abi_enif_fmt_analyze` (a Rust format-string parser), then pulls exactly the varargs the parser
  reported into a flat `u64` slot array and calls `beam_abi_enif_fmt_{snprintf,fprintf}`, which render
  the string — parser and extractor share one grammar, so they cannot disagree.
- The supported `*printf` subset is a faithful port of `erts/lib_src/common/erl_printf_format.c`;
  anything outside it returns a **negative error** (`-EINVAL`), never a wrong string.
- `build.rs` emits `cargo::rustc-check-cfg=cfg(beam_abi_variadic_shim)` unconditionally (so the cfg is
  known on the degraded path) and sets it only when the shim compiled.

## Test Strategy
- `tests/layout.rs` — the runtime mirror of the `src/layout.rs` freeze: every size/align/offset of the
  Frozen layout table, the `ErlNifMapIterator` **union interior**
  (`ErlNifMapIteratorUnion`/`Flat`/`Hash`), every scalar-typedef width, every opaque handle, and the
  enum-discriminant / flag-bit / versioned-constant surface (`ERL_NIF_*`, `ERL_DRV_*`, `PORT_*`,
  `ERTS_NAPI_*`). `tests/term_abi.rs` — `size_of::<ERL_NIF_TERM>() == size_of::<usize>() ==
  size_of::<Eterm>()`, matching `align_of`, and `nif_from_eterm(eterm_from_nif(t)) == t` over boundary
  raw values (`NIL`, `make_small(0)`, max small, atom tag) — the A.3 guarantee. `tests/api_surface.rs` —
  `size_of::<ErlNifApi>() == 176 * 8` / `size_of::<ErlDrvApi>() == 111 * 8` + an all-`None` table through
  `size_of_val`. `tests/entry.rs` — an `ErlNifFunc`/`ErlNifEntry`/`ErlDrvEntry` (the `nif_init` shape,
  null callbacks) read back via `major`/`minor`/`num_of_funcs`/`vm_variant`/`options` and `offset_of!`
  field order.
- `tests/host_seam.rs` + `tests/common/mod.rs` — the host seam through the **public** API; `common/` also
  compiles a trivial `erl_nif.h`-including probe `.so` (`cc -shared -fPIC`) — its helper list is in the
  Routing Table.
- **Per-family end-to-end tests** (`tests/nif_*.rs`/`driver_*.rs`) — each compiles a real C fixture at
  test time with the system `cc` against the system OTP headers and `dlopen`s it (`RTLD_NOW`) through the
  crate's own loader, so a single unresolved `enif_*`/`driver_*` import fails the load; `nif_variadic.rs`
  is gated on `cfg(beam_abi_variadic_shim)`, `driver_binary_refc.rs` uses a dual-`driver_init`/`nif_init`
  fixture shared with the oracle run, and `nif_threads.rs`/`driver_threads.rs` carry the extended
  `*_name`-matrix (`"unknown"`) cases.
- **`tests/abi_conformance.rs` — the mechanical gap guard.** Parses the real headers at test time
  (`erl_nif_api_funcs.h`'s `ERL_NIF_API_FUNC_DECL` list, `erl_driver.h`'s `EXTERN` declarations) with the
  headers' own gating (the four `#if SIZEOF_LONG != 8` `enif_*64` entries do not exist on LP64; the
  `erl_drv_steal_main_thread*` pair is `__DARWIN__`-only) and asserts **set equality** against the crate's
  `#[no_mangle]` definitions plus `csrc/nif_variadic.c`, that `ErlNifApi`/`ErlDrvApi` **field order and
  counts equal the header declaration order** (a transposed slot is an ABI break), and that every word of
  `enif_api_table()`/`driver_api_table()` is populated; a header declaration with no crate definition
  FAILS, and `KNOWN_UNIMPLEMENTED` is an explicit (currently empty) allowlist. Headers come from the
  **in-tree legacy** `erts/emulator/beam/` (`beam_dir()` =
  `CARGO_MANIFEST_DIR/../../../erts/emulator/beam`); absent ⇒ a loud `SKIP`, never a silent pass.
- **Oracle cross-checks that really run** (they skip loudly without `erl`/`erlc`/`cc`). `nif_threads.rs`
  loads the *same* fixture `.so` from the system Erlang and compares the printed tuple (`thread_type/0`); `nif_mapiter.rs`'s
  `the_oracle_agrees_on_the_flat_map_key_order` runs the real `erl` over `maps:from_list/1` →
  `maps:keys/1` and requires the fixture's C-side walk order to equal it. `nif_resource.rs`'s expected
  `load_status`/`roundtrip`/`roundtrip_value`/`post_release` tuples are the **measured oracle values**
  (OTP 29.1.1 / erts-17.1, `post_release/0` = `{1,42,0}`), and `dtor_probe/0` + `dtor_state/0` are
  **Rust-only** (the oracle's C `enif_release_resource(NULL)`/`enif_keep_resource(NULL)` dereference the
  operand and segfault). `nif_compare.rs`'s `the_same_fixture_agrees_under_the_erl_oracle` requires the
  Rust order points to equal the oracle's for the *same* fixture `.so`; `nif_priv_data.rs`/
  `driver_monitors.rs` cross-check under `erl`/`erlc`, but their **driver** halves print a loud `SKIP:`
  in this sandbox (its ERTS exports `erlang:load_nif/2` but not `erlang:load_driver/2`), so the driver
  monitor expectations are pinned on the Rust side + the `io.c` ground truth.
- **Loud `SKIP:`, never a silent pass:** `common::compile_so` prints a `SKIP:` banner and returns `None`
  when the C compiler or the OTP include directory is missing; every test then skips.
- **Term-lifetime pitfall (host arena).** A NIF's result term lives on the **host arena** behind the
  call's `NifEnv`, readable only while that arena is alive (`tuple_size == 0`); `nif_variadic.rs`'s
  `Session` therefore owns the host (boxed), the loaded library and the term.
- In-module `#[cfg(test)]` unit tests: `src/nif/enif/tests.rs`, `src/nif/enif/process_tests.rs`,
  `src/nif/enif/mapiter_tests.rs`, `src/nif/enif/resources_tests.rs`, `src/driver/drv/port_tests.rs`,
  `src/sync/tests.rs`. `port_tests.rs` carries both halves of the `output2` contract: a `SplitHost`
  (overrides `output2`, records `(header, body)`; its `output` records a "must stay empty" list) pins
  the split per entry point — `driver_output2` (three body shapes + a refused null-with-length + two
  replies staying two records), `driver_output` (null vs empty buffer), `driver_output_binary`
  (`H`+`cde`, zero-length range, over-read refusal), `driver_outputv` (last vector as tail, `skip`
  0/5/8, a trailing zero-length vector ⇒ `Some(&[])`) — while `TestDriverHost` (no `output2`
  override, an `output` call counter) pins the **default** as byte-identical: exactly one `output`
  call carrying header-then-body for every shape.
- Gate: `../scripts/cargo.sh test -p beam_abi` and `../scripts/cargo.sh build --workspace` GREEN
  (warning-free); behavioural cross-checking against real ERTS is `../../tools/differential`'s job
  (sibling — read-only, escalate writes to `..`).

## Oracle behavior: `erlang:load_nif/2` load-time contract
Ground truth `../../erts/emulator/beam/erl_nif.c` (`erts_load_nif`, line 4729) — the checks validating an `ErlNifEntry` (the fields this crate declares), to be reproduced by the runtime. Verified empirically against the system OTP 29.1.1 / erts-17.1 (scratch NIF in `$TMPDIR`, read-only C).
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
- Empirical raw outputs (module `m`, NIF entry name `m`): success `ok`; missing path and a text file both `{error,{load_failed,"Failed to load NIF library: '...'…"}}`; major 99 → `{error,{bad_lib,"That 'm' NIF library needs erts-99.0 or newer. …"}}`; no `nif_init` → `{error,{bad_lib,"Failed to find library init function: '.../emptysym.so: undefined symbol: _nif_init'"}}`; reload → `{error,{reload,"NIF library already loaded (reload disallowed since OTP 20)."}}`; `-on_load` calling `load_nif` succeeds and the module's NIF is callable; `apply/3` → the stub error.
- (Note: in this Nix-packaged runtime `code:lib_dir(erts,include)` points at a non-existent path; the real header is `.../lib/erlang/erts-17.1/include/erl_nif.h`.)

## NIF host layer
### `enif_*` functions (`src/nif/enif/`)
Every one is `#[no_mangle] pub unsafe extern "C"` with the exact signature of its matching `nif::api`
field, driving the `NifEnv`/`NifHost` seam (`beam_term`, `beam_alloc::Heap`, `NifHost::intern_atom`). By family:
- **`mem`** — libc-backed `enif_alloc`/`enif_free`/`enif_realloc`; the binary family
  (`enif_alloc_binary`, `enif_realloc_binary`, `enif_release_binary`, `enif_make_new_binary`, `enif_make_binary`, `enif_make_sub_binary`, `enif_inspect_binary`, `enif_inspect_iolist_as_binary`);
  the ETF pair `enif_term_to_binary`/`enif_binary_to_term` via `NifHost` (the C's failure `0` without
  the hook). Binary I/O shapes are **oracle-exact** (`erl_nif.c`, `erts_inspect_iolist_as_binary`, `erts_copy_bits_fwd`, `erts_build_sub_bitstring`):
  - `enif_inspect_iolist_as_binary` — a byte integer only in head (CAR) position, a proper list, a
    byte-sized binary/bitstring, or the empty list; `[]`/`nil` **succeeds** (`size = 0`, non-NULL dummy
    `data` = `(unsigned char*)&bin->data`, `erl_nif.c:1400`); a bare integer, a non-nil non-list tail
    (`[1|2]`), a non-byte integer (`256`, `-1`) or a non-byte-sized bitstring (`<<1:7>>`) are
    **failures → `0`** (`size = 0`, `data = -1`).
  - `enif_inspect_binary` — a bit-offset, byte-sized bitstring ok (`TAIL_BITS(size) == 0` ⇒ `1`); an
    unaligned start is copied by `copy_bits_fwd` and handed back **owned** (null `__spare__[0]`, freed
    once by `enif_release_binary`).
  - `enif_make_sub_binary` — windows **any bitstring by bits**: offset `source_bit_offset + pos*8`,
    span `size` bits; `size == 0` ⇒ `<<>>` for any `pos`; ≤ `ERL_ONHEAP_BITS_LIMIT` (512) bits copies
    into an `ErlHeapBits`, above it an `ErlSubBits` window; `badarg` only for a non-bitstring source or
    `pos*8 + size*8 > source_bit_size` with `size > 0` (`checked_mul`).
- **`get`** — `enif_get_int/uint/long/ulong/double/list_cell/tuple/list_length/string/string_length/atom/atom_length`;
  the `ERL_NIF_UTF8` arm of `enif_get_string` is **byte-faithful** to `erts_unicode_list_to_buf`
  (`utils.c:2862-2942`): byte budget `len-1`, char budget `len`, an incompletely fitting multibyte char
  dropped **whole** — `buf[written] = '\0'` + negative `-(written+1)`, else `buf[0] = '\0'` + `0`;
  `enif_get_string_length` agrees.
- **`make`** — `enif_make_int/uint/long/ulong/double/atom(_len)/new_atom(_len)/existing_atom(_len)/string(_len)/list_cell/reverse_list/ref/tuple_from_array/list_from_array`;
  the four make-atom entry points **record** each interned atom on the env, `enif_make_existing_atom(_len)`
  consults that record first then `NifHost::existing_atom` (else `0` with `*atom` untouched);
  `enif_make_ref` raises `badarg` when `NifHost::make_ref` is absent.
- **`map`** — `enif_get_map_size`, `enif_get_map_value`, `enif_make_new_map`, `enif_make_map_from_arrays`, `enif_make_map_put`, `enif_make_map_update`, `enif_make_map_remove`
- **`pred`** — `enif_is_*`, `enif_compare`, `enif_is_identical`, `enif_hash`, `enif_term_type`,
  `enif_term_size`. `enif_compare` is the C's `CMP` = `erts_cmp(a, b, 0, 0)` (**arithmetic** mode:
  `enif_compare(1, 1.0) == 0`) and orders an atom pair by **name** (`erts_cmp_atoms`, `erl_utils.h:193-215`), each name resolved *current env atom
  record → `NifHost::atom_name` → raw atom index* — which is what makes in-call `enif_make_atom` atoms
  resolvable. `enif_is_identical` = `EQ` (exact mode); `enif_hash` = PHASH2 over the term word.
  Atom pairs are resolved at **top level** only — see *Known gaps*.
- **`proc`** — `enif_self`, `enif_get_local_pid`, `enif_get_local_port`, `enif_set_pid_undefined`, `enif_is_pid_undefined`, `enif_get_atom_cache_index`, `enif_max_atom_cache_index`
- **`process`** — `enif_monitor_process`/`demonitor_process`/`compare_monitors`/`make_monitor_term`,
  `enif_is_process_alive`/`is_current_process_alive`/`is_port_alive`, `enif_send`, `enif_port_command`,
  `enif_whereis_pid`/`whereis_port` (0 when the `NifHost` hook is absent), `enif_schedule_nif`; all
  drive existing hooks, no new state. Monitor payload = the C's 3-word reference block in the 32-byte
  `ErlNifMonitor` (bytes 24..32 zeroed).
- **`exception`** — `enif_make_badarg`, `enif_raise_exception`, `enif_has_pending_exception`,
  `enif_clear_env`.
- **`threads`** — the 34 mutex/cond/rwlock/tsd/thread slots: cast-and-forward facades over
  `crate::sync`; only `enif_thread_type` and `enif_thread_name`/`erl_drv_thread_name` (the `*_name`
  accessors) do more.
- **`ioq`** — `enif_ioq_create`/`destroy`/`enq_binary`/`enqv`/`size`/`deq`/`peek`/`peek_head` +
  `enif_inspect_iovec`/`enif_free_iovec`; a `VecDeque` of buffers preserving the vector *count* rule
  (one `SysIOVec` per non-empty enqueued buffer).
- **`mapiter`** — the seven `enif_map_iterator_*` slots over the frozen 56-byte `ErlNifMapIterator`, in
  the map's **storage order** (`hashmap_iterator_next`). The hash arm's `ErtsDynamicWStack` is a boxed pair list; `get_pair` answers the same
  sequence.
- **`time`** — `enif_monotonic_time`/`enif_time_offset`/`enif_convert_time_unit`/`enif_now_time`/`enif_cpu_time`/`enif_system_info`/`enif_getenv`/`enif_make_unique_integer`/`enif_consume_timeslice`/`enif_dlopen`/`enif_dlsym`;
  the monotonic-time / time-offset relation is reproduced and covered by Rust unit tests in
  `src/nif/enif/time.rs` (`monotonic_time_is_monotone_and_unit_consistent`, `monotonic_plus_offset_is_wall_time`)
  — there is no `erl`-oracle pin for `enif_time_offset/1`;
  `enif_system_info` (no env) reaches the host via `with_current_host` and prefers
  `NifHost::system_info`; `enif_make_unique_integer` prefers `NifHost::unique_integer`.
- **`resources`** — the eleven slots over a crate-internal type registry + a real refcounted allocation
  (`enif_open_resource_type`, `_x`, `init_resource_type`, `alloc_resource`, `release_resource`, `keep_resource`, `make_resource`, `make_resource_binary`, `get_resource`, `sizeof_resource`, `dynamic_resource_call`). A resource *term* is a reference **minted here** (`make_ref`, `None`), recorded term-word → object. An exporting call
  (`enif_make_resource`/`enif_make_resource_binary`) takes **its own reference** as the C magic ref
  does, so a released-but-term-wrapped object stays alive and `enif_get_resource` keeps resolving it
  (oracle-verified). Deviations (no GC): that reference is never dropped, so an ever-exported object is
  retained (block + payload) for the process lifetime and its destructor never runs, while a
  never-exported object is destroyed on its last release with the destructor run **synchronously** (the
  C schedules it on a misc aux-work thread) — counts/ordering and the callback's environment/payload
  match the oracle, only the timing differs. The asynchronous form **cannot be reproduced safely** (`Send`, `*mut dyn NifHost`).
- **`select`** — `enif_select`/`enif_select_x`: the C's argument handling and return codes, handed to
  `NifHost::select` (the poller is `beam_io`'s); `-1` when no host can service the request.
- **`envs`** — `enif_alloc_env` (thread-local current-NIF-host → `NifHost::alloc_env()`, else `NULL`),
  `enif_free_env` (→ `NifHost::free_env`; `NULL` and foreign pointers are no-ops — this crate never
  frees memory it did not allocate), `enif_make_copy` (`NifHost::make_copy`; `THE_NON_VALUE` when
  unsupported), `enif_priv_data` (`priv_data`, `NifLibrary::call`, `NifHost::priv_data`). A use-after-`enif_free_env` is a caller-contract
  violation the raw-pointer ABI cannot detect.
- **`variadic`** — the seven C-variadic slots via the C shim (see *The C-variadic shim*).
`pub fn enif_api_table() -> &'static ErlNifApi` returns the `#[used] static ENIF_API` built by the
family `populate` chain from `ErlNifApi::empty()`; the `#[used]` static keeps the `#[no_mangle]`
symbols in the final link.
### Dynamic loader (`src/nif/loader.rs`)
`load(path, module, host, load_info) -> Result<NifLibrary, NifLoadError>` does `dlopen(path+".so", RTLD_NOW|RTLD_LOCAL)`, `dlsym("nif_init")` (fallback `_nif_init`), validates in the oracle's exact order (the table above) and invokes `entry.load`. `NifLibrary::call(env, name, arity, args)` looks the function up by `(name, arity)`, calls `fptr(env.as_ptr(), arity, args.as_ptr())`, and turns a pending env exception into `NifCallError::Raised`; for the call's duration it publishes the library's `priv_data` on a crate-private thread-local stack with the same push/pop-token discipline as the host stack — the only source of `enif_priv_data`'s `env->mod_nif->priv_data`. `Drop` runs `entry.unload` then `dlclose`. `Reload`/`Upgrade`/`OldCode` exist for surface completeness but `load()` never produces them (no module-instance / old-code state here).

### Symbol-export mechanism (load-bearing)
A NIF `.so` leaves `enif_*` and a driver `.so` leaves `driver_*`/`erl_drv_*` **undefined**; the dynamic linker binds them, at `dlopen(RTLD_NOW)`, against the **loading process's** dynamic symbol table, so `build.rs` emits `cargo::rustc-link-arg-tests=-Wl,--export-dynamic` for each `beam_abi` test binary. Proven: `nm -D --undefined-only test_nif.so` lists the used `enif_*` as `U` and `test_driver.so` lists its `driver_*`/`erl_drv_*` as `U`; the same names appear `T` in the test binary's `.dynsym`; loading the same `.so` from a process that does **not** export them fails with `undefined symbol: enif_make_int`. The `cargo::rustc-link-arg-bins` counterpart must **not** be emitted: this package has no `[[bin]]` target and cargo hard-errors on it.

## Driver host layer
### `driver_*` / `erl_drv_*` functions (`src/driver/drv/`)
Every one is `#[no_mangle] pub unsafe extern "C"` with the exact signature of its matching `driver::api` field, landing in the `#[used] static DRIVER_API` published through `pub fn driver_api_table() -> &'static ErlDrvApi`. `EMPTY_DRIVER_API` is `unsafe { core::mem::zeroed() }` (the const-evaluable all-`None` base; `ErlDrvApi::empty()` is a plain `fn` and cannot initialise a `static`) — every pointer field is null-optimized, so the all-zero struct **is** the all-`None` table (a test pins that equality). Each family's behaviour and measured return codes live in that family file's module docs (`src/driver/drv/*.rs`; ground truth `io.c`):
- **`mem`** (host-free) — `driver_alloc`, `driver_free`, `driver_realloc`, `driver_alloc_binary`, `driver_realloc_binary`, `driver_free_binary`, `driver_binary_get_refc`, `inc_refc`, `dec_refc`, `driver_system_info`, `null_func`, `erl_errno_id`, `erl_drv_consume_timeslice`, `erl_drv_monotonic_time`, `erl_drv_time_offset`, `erl_drv_convert_time_unit`, `driver_dl_open`, `driver_dl_sym`, `driver_dl_close`, `driver_dl_error`, `erl_drv_putenv`, `erl_drv_getenv`, `driver_mk_term_nil`, `driver_term_nil`
- **`port`** (`ErlDrvPort`, `DriverEnv::as_port`) — the output family (`driver_output`,
  `driver_output2`, `driver_output_binary`, `driver_outputv`), all routed through
  [`DriverHost::output2`](crate::driver::host::DriverHost::output2) with the C's `buf == NULL`
  distinction kept (`tail_bytes`; a null buffer with a non-zero length is refused `-1`, never
  dereferenced). `driver_outputv` reproduces `driver_outputv`'s own `do { … } while (skip > 0)`
  stripping (it also consumes one leading zero-length vector when `skip == 0`) and leaves the
  **last** remaining vector as the improper tail, every preceding one joining the header; the
  C's exact `vec->size <= skip` early return becomes `body: None`, not an empty tail. The
  term-data family (`driver_output_term`/`driver_send_term`/`erl_drv_output_term`/`erl_drv_send_term` over `decode_term_spec`, decoding **all 17 `ERL_DRV_*` tags** — `ERL_DRV_PID` accepts only an internal pid, `ERL_DRV_MAP` builds via `beam_term::map` (flat ≤ 32, else HAMT; duplicate keys refused), `ERL_DRV_EXT2TERM` routes through the defaulted `DriverHost::binary_to_term`); the port queries (`driver_mk_atom`, `driver_mk_port`, `driver_connected`, `driver_caller`); the select/timer/async group.
- **`queues`** — `driver_vec_to_buf`, `driver_enq`, `driver_pushq`, `driver_deq`, `driver_sizeq`, `driver_enq_bin`, `driver_pushq_bin`, `driver_peekq`, `driver_peekqv`, `driver_enqv`, `driver_pushqv`, `DriverHost` Oracle-measured return codes: `driver_deq` returns the **remaining** count, `(ErlDrvSizeT)-1` when over-dequeued; `driver_sizeq`'s `NOT_A_SIZE` sentinel **is** `(ErlDrvSizeT)-1` (`ErlDrvSizeT::MAX`, `queues.rs`), which the C returns only for a `NULL` queue — so an empty allocated queue must make the **host** answer `0` (`beam_rt`/`beam_io`).
- **`threads`** — `erl_drv_*`, `crate::sync`, `ErlDrvPort`, `DriverHost`
- **`pdl`** — `driver_pdl_create`, `lock`, `unlock`, `get_refc`, `inc_refc`, `dec_refc`, `driver_monitor_process`, `demonitor_process`, `get_monitored_process`, `compare_monitors`, `driver_get_now` `driver_compare_monitors` is **host-free and complete** (a 24-byte `memcmp` = `ERTS_REF_THING_SIZE(3)*8`, C `io.c:7142-7146`); the five refc slots never touch the host (crate-owned `PdlState`); only `driver_pdl_create` delegates — and the host must return a **per-port-stable** handle (C `pp->port_data_lock`; a second create on the same port ⇒ `NULL`). `driver_monitor_process` reaches the host through the three-way `DriverHost::monitor_process_result` and maps `MonitorResult::{Ok,NotAlive,Error}` onto the C's `0`/`1`/`-1` (its signs pinned by `const _: () = assert!(…)`); its `names_a_process` gate answers `1` for an operand that cannot name a process. It also exposes the **safe** accessors [`monitor_identity(*const ErlDrvMonitor) -> Option<[u8; 24]>`] / [`monitor_from_identity([u8; 24]) -> ErlDrvMonitor`] (crate-root re-exports) so a `#![deny(unsafe_code)]` host can key its monitor tree on a monitor's bytes; `REF_THING_COMPARE_BYTES` (`= ERTS_REF_THING_SIZE(3) * size_of::<Eterm>()` = 24) is the single width shared with `driver_compare_monitors`, the bytes stay opaque (no encoding invented), and `monitor_identity` answers `None` for a null pointer under the `src/driver/loader.rs` wrapper convention.
- **`lifecycle`** — `driver_failure_eof`, `_atom`, `_posix`, `driver_failure`, `driver_exit`, `DriverHost::failure`, `port_exit`, `set_busy_port`, `set_port_control_flags`, `get_port_flags`, `erl_drv_busy_msgq_limits`, `erl_drv_init_ack`, `erl_drv_set_os_pid`, `driver_async_port_key`
- **`registry`** — `add_driver_entry`, `remove_driver_entry`, `with_current_host`, `-1`, `0`, `driver_lock_driver`, `driver_create_port`, `env_for` Ground truth `io.c:7640`/`7698`/`7324`/`786`.

### The safe async-job runner (`src/driver/async_job.rs`)
`driver_async`'s last three operands (`async_invoke`, `async_data`, `async_free`, `erl_driver.h:605-607`) **are** a C async job. The driver-facing half is `driver_async` → `DriverHost::r#async`; the **host** half is `AsyncJob`, which closes the gap for a runtime that forbids `unsafe` (the sibling `beam_rt` is `#![deny(unsafe_code)]`). `pub type AsyncCallback = unsafe extern "C" fn(*mut c_void)`.
- `pub struct AsyncJob` — the single `unsafe` entry point `AsyncJob::new -> Option<AsyncJob>` (its `# Safety` section is the caller's promise that the three operands are a live C async job; `None` when `async_invoke` is `NULL` — nothing to run, the caller still owns the data). Everything after it is **safe**: `invoke(&self) -> *mut c_void` runs `(*async_invoke)(async_data)` and returns the data for `ready_async`; `finish(self)` runs `(*async_free)(async_data)` **exactly once** (the handle is consumed); `data()`/`has_invoke()`/`has_free()` inspect the operands without dereferencing them; `Drop` frees a still-held job **without** invoking it; `Debug` never touches the pointers.
- **Inert by default.** `DriverHost::r#async`, its signature and its `-1` default are **unchanged** — a host with no async pool keeps answering `-1` and opts in by constructing a job and running it (`+A 0`). Re-exported as `beam_abi::{AsyncJob, AsyncCallback}`. Owner of the *adoption* half (`beam_rt`): implement `DriverHost::r#async`, poll/drain `ready_async`, keep the driver object mapped while a job is outstanding, implement `driver_async_port_key`.

### Dynamic loader (`src/driver/loader.rs`)
`load(path, name) -> Result<DriverLibrary, DriverLoadError>` does `dlopen(path+".so", RTLD_NOW | RTLD_LOCAL)`, `dlsym("driver_init")` (fallback `_driver_init`), then validates the returned `ErlDrvEntry` in the oracle's exact order and runs its `init` callback — reproducing `do_load_driver_entry` (`erts/emulator/beam/erl_bif_ddll.c:1466-1533`), `erts_sys_ddll_load_driver_init` (`sys/unix/erl_unix_sys_ddll.c:216-228`) and `init_driver` (`io.c:7559-7621`). `DriverLibrary` exposes the entry (`entry()`), `name()`, `init()`, `finish()` and one wrapper per callback slot (`start`, `control`, `call`, `output`, `outputv`, `timeout`, `ready_input`, `ready_output`, `ready_async`, `flush`, `process_exit`, `stop_select`, `emergency_close`, `stop`); a `NULL` slot is `DriverCallError::AbsentCallback { callback }` (never a crash), so the caller applies whichever of the C's two behaviours matches the slot (a logging no-op, or a plain skip). `Drop` runs the `finish` callback then `dlclose`s the object. Failure variants and their reason atoms (`erl_bif_ddll.c:1709-1720`): `NoDriverInit` ⇒ `no_driver_init`, `DriverInitFailed` ⇒ `driver_init_failed` (a null entry **or** a vetoing `init`), `BadDriverName` ⇒ `bad_driver_name`, `DriverIncorrectVersion` ⇒ `driver_incorrect_version` (missing `ERL_DRV_EXTENDED_MARKER` or out-of-range version), `Unspecified` ⇒ `unspecified_error`; `SymbolNotFound` exists for surface completeness but `load()` never produces it.

### Documented deviations from the oracle
(each site's module docs carry the full rationale)
- `driver_mk_atom` returns the **name pointer** (the `ERL_DRV_ATOM` operand `decode_term_spec` interns), the C a pre-interned atom handle; `driver_mk_port` returns the **port handle pointer** (the `erl_drv_output_term`/`erl_drv_send_term` encoding), the C the port's id term (each pair round-trips).
- `driver_mk_port(NULL)` returns `driver_term_nil` (is safe); the oracle does **not** null-check the operand and its VM exits on the spot (exit signal, empty stdout) — a deliberate, retained divergence, since that behaviour is an unpinnable probe hazard (`src/driver/drv/port.rs`).
- `driver_output_term`/`driver_send_term`/`erl_drv_output_term`/`erl_drv_send_term` return the host's code (`0` = delivered), the C `1`/`0`; the documented `< 0` idiom sees identical behaviour.
- `erl_errno_id` returns `strerror`'s descriptive message (the oracle the lowercase POSIX errno *name*); `erl_drv_time_offset` returns `0` by construction (monotonic time is anchored on the system epoch); `erl_drv_consume_timeslice` always returns `0` (no port-timeslice accounting); `driver_system_info` returns without writing for a null `sip` or an `si_size` shorter than the first field group, where the C calls `erts_exit(ERTS_ERROR_EXIT, …)`.
- Refusals where the C would fault or over-read: a malformed/out-of-range request (`driver_output_binary`, `offset + len > orig_size`, `driver_outputv`, `skip`, `time_left`, `ErlIOVec`) refused with `-1`; a `NULL` `driver_name` is a typed `BadDriverName`; a `dlopen` failure lands in `Unspecified` carrying the loader's own `dlerror` text (the C renders an `{open_error, N}` code); the `.so` suffix is appended only when absent. No code-server/driver list or reload policy is consulted (only the *entry* validation).

## Oracle-measured: the port-closing family (`driver_failure*` / `driver_exit`)
Measured on OTP 29.1.1 / erts-17.1 with a `cc`-built driver `.so` (`erl_ddll:load_driver/2` +
`open_port({spawn_driver,Name},[])`, owner `process_flag(trap_exit,true)`), read from
`io.c:7195-7280`'s single `driver_failure_term(ix, term, eof)`:
| C entry point | `(term, eof)` | reason the owner gets | C return |
|---|---|---|---|
| `driver_failure_eof(P)` | `(NIL, 1)` | `normal` | `0` |
| `driver_failure_atom(P,"boom")` | `(atom, 0)` | `boom` (bare atom) | `0` |
| `driver_failure_posix(P,EACCES)` | `(atom("eacces"), 0)` | `eacces` (bare atom, **not** `{error,·}`) | `0` |
| `driver_failure(P,7)` | `(small(7), 0)` | `7` (bare int, **not** `{error,·}`) | `0` |
| `driver_failure(P,0)` | `(small(0), **1**)` | `normal` — **identical to `driver_failure_eof`** | `0` |
| `driver_exit(P,0)` | unlink first, then `(am_normal, 0)` | **no** exit message (link removed); a monitor sees `{'DOWN',…,normal}` | `0` |
| `driver_exit(P,EACCES)` | unlink first, then `(atom("eacces"), 0)` | no exit message; monitor reason `eacces` | `0` |
`eof=1` does three further things beyond forcing `am_normal`: it acks an async-open port
(`init_ack_send_reply`), **flushes the line buffer** (`flush_linebuf_messages`, `io.c:3475-3499` — the
only call site in the emulator; measured: a `[line]` port with a buffered partial line delivers
`{Port,{data,{noeol,"partial"}}}` *before* the exit), and takes the **soft-eof** branch on a port
opened with the `eof` option (`ERTS_PORT_SFLG_SOFT_EOF`, `erl_bif_port.c:996`): instead of closing it
delivers `{Port,eof}` and the port **stays alive** (measured `port_info` intact, `port_close/1` =
`true`). Every closed port afterwards answers `port_info(P)` = `undefined` and `port_close/1` raises
`badarg`. `erl_errno_id/1` yields the **lowercase POSIX name** (measured `eacces`/`enoent`/`eio`/
`einval`; `0` ⇒ `errno_0`, an out-of-range errno ⇒ `errno_9999`, `erl_errno_str.c:47`, `:1158-1180`) —
so this crate's `erl_errno_id` (which returns `strerror` text, a documented deviation) **cannot** be
used to build a `Posix` reason. `driver_failure_atom` interns the raw bytes as **Latin-1** (measured:
the single byte `0xE9` ⇒ `é`, the UTF-8 pair `0xC3 0xA9` ⇒ `'hÃ©llo'`; an empty name ⇒ `''`), i.e. the
seam's UTF-8-only `PortFailure::Atom(&str)` is a real, documented divergence.
### The invalid-port operand: `NULL` is a crash, `(void*)-1` is the `-1` path
`ERTS_INVALID_ERL_DRV_PORT` is **`(struct _erl_drv_port *)(SWord)-1`**, never `NULL`
(`erl_port.h:60-67`: "It would have been preferred to use NULL … not backward compatible").
Measured: `driver_failure_eof(NULL)`/`_atom`/`_posix`/`driver_failure(NULL,·)`/`driver_exit(NULL,·)`
each **SIGSEGV the VM deterministically (3/3)** — only `ERTS_INVALID_ERL_DRV_PORT (-1)` returns `-1`
(deterministic, port untouched). So this crate's `env_for`/`DriverEnv::from_port` mapping *null* to
"the C's invalid-port path" (and its `-1` return) is a deliberate **safety divergence**, not ABI
fidelity; the real sentinel `(void*)-1` is **not** recognized (`from_port` accepts it and dereferences
`-1`, i.e. UB). A stale handle from a fully-terminated port returns `-1` deterministically (3/3) when
called from a *different* live port's callback, but calling a second `driver_failure_*` on a
just-terminated port from the **same** callback **segfaults 3 of 4 runs** (`erl_port.h:735-747`
documents the `ERTS_PORT_SFLGS_INVALID_DRIVER_LOOKUP` guard, but the state word is read out of freed
memory) — both shapes are use-after-free and therefore **unpinnable**.
### Status of this crate's half
`lifecycle.rs` models every spelling onto `DriverHost::failure`/`port_exit` with the C's return codes
(`0`/`-1`) and never touches a null port. Four contract defects remain (all in *this* crate, no host
needed): (1) `PortFailure::Posix`/`Error`'s doc comments claim the reason is `{error,·}` — the oracle's
is the bare atom / bare int; (2) `driver_failure(P, 0)` reaches the seam as `PortFailure::Error(0)`,
losing the `eof` flag — it must map to `PortFailure::Eof` (behaviourally identical in the C); (3) the
`(void*)-1` sentinel must be rejected alongside `NULL`; (4) no production host implements
`failure`/`port_exit` (`beam_rt`'s `DriverPort` and `beam_io`'s `AbiHost` leave both trait defaults), so
a loaded driver's `driver_failure_*` answers `-1` and nothing closes — the `{'EXIT',Port,Reason}` /
`{'DOWN',…}` / `{Port,eof}` observables above are the contract that host must reproduce.
`port_info`-side note: a `control` callback returning `< 0` (the documented
`return ERL_DRV_ERROR_GENERAL` idiom) makes `port_control/3` raise `badarg` and leaves the port
**alive**; `output`/`ready_input` are `void`-returning in erts-17.1, so that idiom now applies only to
`control`/`call`.

## Handoffs (host halves owned by other nodes)
- **`DriverHost::output2` adoption — the only remaining step for a loaded C driver's split reply.**
  All four `driver_output*` thunks now hand the split to `output2`; a host that does not override it
  gets the default (header-then-body joined into one `output` call), i.e. today's flattened result.
  Two hosts must implement it:
  - `beam_io`'s `AbiHost` (`beam_io/src/abi/host.rs`): `output2` = `match
    self.with_ctx(|ctx| ctx.deliver_read(header, body)) { Some(Ok(())) => 0, _ => -1 }` — the same
    `DriverContext::deliver_read` seam the in-tree drivers use, so the reply becomes one
    `port_task::ReadMessage { header, body }` (its `body: None` / `Some(vec![])` distinction is
    exactly the hook's). Its current `output` (`ctx.input().push`, a flat body-only read) then only
    serves direct `DriverHost::output` callers.
  - `beam_rt`'s `DriverPort` (`beam_rt/src/port/driver.rs`, `impl DriverHost for DriverPort`): it
    records replies in `outputs: Vec<Vec<u8>>` and `DriverPortAdapter::collect_outputs` pushes them
    through `ctx.input()`, so it must record `(header, Option<body>)` instead and deliver each split
    through `ctx.deliver_read(header, body)` — otherwise the runtime's loaded-driver path keeps the
    flattened shape the differential note in `tools/differential/src/corpus/driver_c.rs` records
    (`[97,98|<<99,100>>]` vs a flat `<<97,98,99,100>>`).

## Known gaps (honest, never a wrong result)
- **`driver_outputv` with more than one remaining vector — the earlier vectors' nesting, never the
  bytes.** The seam carries one header byte stream and one tail, so only the **last** remaining
  vector can be the improper tail (exact, and every inet-driver read path is single-vector). In
  `[binary]` port mode the C makes each *preceding* vector its own binary cons cell
  (`[h… | <<v0>>, <<v1>>, …, <<vlast>>]`, `deliver_vec_message`, `io.c:3568-3614`) while this crate
  folds those bytes into the header's int list (`[h…, v0 bytes… | <<vlast>>`): the flattened byte
  sequence and `[list]` mode are identical, only the nesting of the earlier vectors differs. A
  malformed `ErlIOVec` (a declared `size` the vectors cannot satisfy) is refused `-1` instead of the
  C's over-read, and a null `buf` with a non-zero `len` is refused rather than dereferenced.
- **`enif_alloc_env` answers `NULL`** only with the host's `alloc_env` at its trait default, or with no
  `NifEnv` live (outside a NIF call). `beam_rt`'s `NifHostImpl` implements `alloc_env`/`free_env`/
  `make_copy` (`beam_rt/src/nif.rs`) over the one shared pooled BIF heap, so the runtime's message-env
  idiom works and `make_copy` is the identity (a crate-owned env would carry unusable atoms).
- **NIF host hooks the runtime does not implement.** `NifHostImpl` overrides only
  `heap`/`intern_atom`/`atom_name`/`self_pid`/`alloc_env`/`free_env`/`make_copy`; the rest sit at their
  default `None`/`false`/`null`, so the matching `enif_*` entries answer the C's failure (or raise
  `badarg`, for `enif_make_ref`) where the oracle succeeds — the hook list is in *The host seam*.
  Closing each is a `beam_rt` change: `term_to_binary`/`binary_to_term` over `beam_codec`; `make_ref`;
  `existing_atom` over the committed table **and** the in-flight `PendingInterner`; `whereis_pid` over
  the process registry; `system_info` with the real `erts_version`/`otp_release`/`scheduler_threads`/
  `dirty_scheduler_support` (the built-in fallback rounds the last to `0` where the C reports `1`).
  The `beam_abi` side is complete.
- **No scheduler threads.** `enif_thread_type` reports only `ERL_NIF_THR_UNDEFINED` (foreign thread) or
  `ERL_NIF_THR_OTHER` (`sync::thread_create`), never a scheduler/dirty-scheduler type; `*_name`
  reproduces the C's `no_name` = `"unknown"` for a **live** NULL-named handle (`sync::no_name()`/
  `sync::thread_created_unnamed`, driver `erl_drv_thread.c:99,135`; `enif_*` forwards
  `erl_nif.c:2438-2441`) and answers `NULL` for a **foreign or already-joined** tid (the C's
  `drv_thr == 0` path, `erl_drv_thread.c:664-684`).
- **`sync::tsd_*` passes the key straight to pthreads unchecked** (no `used_tsd_keys[]` validation); an
  invalid/destroyed key makes `set`/`get` a safe no-op (`NULL`) where the C aborts.
- **The driver-registry slots are reachable from `beam_rt`**: its per-port `DriverHost`
  (`beam_rt/src/port/driver.rs`) implements `add_driver_entry`/`remove_driver_entry` (`:705`/`:711`) and
  `lock_driver` (`:724`, `io.c:7337`) over its own
  `DRIVER_ENTRIES` list, wrapping its callback dispatch in `with_current_host` (the `publishing`
  helper, `:570`).
- **`driver_create_port` is not plumbed — a hard-coded failure sentinel.** No host supplies a live
  `PortRegistry`+`Poller`+driver-table reachable from a per-port `DriverHost` , so a
  `beam_rt` host returns the invalid-port sentinel (`beam_rt/src/port/driver.rs:741-749`).
- **The `driver_binary_*_refc` trio refuses a NULL/foreign operand with `-1`** where the C would recover
  a bogus `Binary*` and crash (the count itself is oracle-exact).
- **`enif_ioq_is_empty` does not exist in this OTP tree** (`enif_ioq_*`, `api.rs`, `ioq.rs`, `enif_inspect_iovec`, `enif_free_iovec`).
- `driver_select`/`driver_set_timer`/`driver_cancel_timer`/`driver_read_timer`/`driver_async` forward to
  `DriverHost`, whose **defaults** report "unsupported" (`-1`/`None`): a host only records the request,
  so "the OS poller really woke the port" is a `beam_io`/`beam_rt` concern.
- `enif_make_map_*`/`enif_compare`/`enif_is_identical`/`enif_hash` take no env; identical terms take a
  raw-word fast path. `enif_compare` resolves an atom pair by **name** — a *top-level* pair through
  `current_recorded_atom_name`/`NifHost::atom_name`, and a pair **nested** (tuple/list/map) inside a
  compound through the current host's atom table (`NifHost::atom_table`, a defaulted hook). Only with
  no host, or a host that exposes no table, does a nested pair fall back to `beam_term`'s raw
  atom-index order — pinned by `tests/nif_compare.rs::a_nested_atom_pair_orders_by_name`. Exact for
  non-atom keys. **Cross-crate need:** `beam_rt`'s `NifHostImpl` (`erts_rust/crates/beam_rt/src/nif.rs`,
  which holds `Rc<beam_term::AtomTable>`) must override `NifHost::atom_table()` to return
  `Some(&*self.atoms)` for the runtime path to benefit.
- Integers: small ints + one-limb bignums; a wider value makes the accessor return `0`, never truncate.
- **On-heap vs off-heap refc binaries (F5-class, audited — not closable inside `beam_abi`).**
  `enif_alloc_binary`/`enif_make_new_binary`/`enif_make_binary` always build an **on-heap** binary
  (`ref_bin == NULL`, `orig_size`/`orig_bytes` only), where the oracle keeps the payload off-heap with
  an `ERL_REFC_BITS_SIZE` 8-word refc pair (`ERL_REFC_BITS_SIZE == 8`, `copy.c:219-242`). Measured
  against OTP 29.1.1 / erts-17.1: **agree** for `size ≤ ERL_ONHEAP_BINARY_LIMIT` (64 B) — 2/3/4/10 words
  for 0/1/10/64 B — and differ above (65 B: oracle 8 words / `flat_size` 64 vs **11 words / 88** here;
  the oracle's `b.ref_bin` is non-NULL at every `enif_alloc_binary` size); both are valid binaries of
  the same size/bytes, only the representation — and with it `enif_term_size`/`erts_debug:flat_size` —
  differs. Not closable here (`beam_alloc::RefcBinary`, `BinRef.val`, `beam_process`): closure
  touches `src/nif/host.rs`, `src/nif/enif/mem.rs`, `beam_rt`'s `impl NifHost for NifHostImpl`
  (`beam_rt/src/nif.rs`, `beam_rt/src/context/mod.rs:1677-1694`) and `beam_gc::OffHeap`;
  adjacent, `enif_release_binary` differs from the oracle only under `#ifdef DEBUG`
  (`erl_nif.c:1451-1461`). Pinned by the `mem.rs` unit tests and
  `tests/nif_bitslice.rs`/`tests/nif_subbin.rs`.
- **`driver_monitor_process`/`driver_demonitor_process`/`driver_get_monitored_process`** — the A.4
  driver-monitor family. The **`beam_abi` half is complete**: all three C outcomes of
  `driver_monitor_process` are expressible (the three-way `DriverHost::monitor_process_result` /
  `MonitorResult` above — `0`/`1`/`-1`, `io.c:7012-7038`), the reason codes are honoured, the
  monitor's 24-byte identity is exposed safely (`monitor_identity`/`monitor_from_identity`), and
  `driver_compare_monitors` is a finished `memcmp`. The host half is owned by `beam_rt`/`beam_io`,
  which must implement `DriverHost::demonitor_process`/`get_monitored_process` and the monitor arming
  (registration, liveness, the per-port monitor tree) and fire `process_exit` (`io.c:6998-6999`,
  `:7065-7081`, `:7104-7118`, `:7150+`). **`beam_rt` should override `monitor_process_result`**
  instead of the two-way `monitor_process`: its `DriverPort` (`beam_rt/src/port/driver.rs:1112-1118`)
  currently returns `None` for both "no `process_exit`" and "the operand is not a local pid" — the
  first must stay `MonitorResult::Error` (`-1`, the C's `io.c:7055`) while the second must become
  `MonitorResult::NotAlive` (the C's `1`, `io.c:7031`), which the old `Option` hook could not express.
- **Other `beam_rt`/`beam_io`-owned halves of the A.4 driver surface** (the operand handling and
  return-code mapping here are oracle-exact): `driver_pdl_create` needs a **per-port-stable** host
  handle (a second create on the same port ⇒ `NULL`); `driver_sizeq` must make the host answer `0` on
  an empty-but-allocated queue; `driver_async` needs the host to run the `AsyncJob`. The A.4
  differential corpus exercises `beam_rt`'s `DriverPort` host, not `beam_io`'s `AbiHost`, so those
  fixes must land there.
- Malformed-term guard: a bare `THE_NON_VALUE` / `HEADER`-tagged word is rejected at the
  term-inspecting entry points (the C would deref it); a *forged* pointer-tagged word is still
  undetectable from the word alone, exactly as in the C.

## Routing Table
- `build.rs` → the build script: `-Wl,--export-dynamic` on the test targets (`.so`, `enif_*`, `driver_*`) **plus** compiling `csrc/nif_variadic.c` (`$CC`/`$AR`, no `cc` crate) and emitting `cfg(beam_abi_variadic_shim)` + `rustc-link-search`/`rustc-link-lib=static`
- `csrc/nif_variadic.c` → the C-variadic `enif_*` shim (the only C in the crate): the `va_list` handling of the seven variadic slots, forwarding into Rust
- `src/lib.rs` → crate docs, module wiring, root re-exports (`NifEnv`, `NifHost`, `DriverEnv`, `DriverHost`, `Eterm`, `monitor_identity`, `monitor_from_identity`), the ABI **version constants** (`ERL_NIF_*`, `ERL_DRV_EXTENDED_*`, `ERL_NIF_VM_VARIANT`, `ERL_NIF_MIN_ERTS_VERSION`)
- `src/common.rs` → `erl_drv_nif.h` shared types: integer typedefs, `SysIOVec`, `ErlDrvSysInfo`, `ErlDrvThreadOpts`, `ErlDirtyJobFlags`, `ErlNifSelectFlags` + `ERL_NIF_SELECT_*`, `ErlDrvMonitor`, time constants
- `src/sync/` → the pthread facade (`Mutex`, `Cond`, `RwLock`, `TsdKey`, `Tid`, `mutex_*`, `cond_*`, `rwlock_*`, `tsd_*`, `thread_opts_*`, `thread_*`) both thread families forward to, with the C's `no_name` = `"unknown"` string (`no_name()`) and the created-unnamed-tid test (`thread_created_unnamed`), plus `tests.rs`
- `src/nif/term.rs` → `ERL_NIF_TERM`/`ERL_NIF_UINT`, the `Eterm ↔ ERL_NIF_TERM` seam
- `src/nif/types.rs` → NIF `#[repr(C)]` structs/enums/flags/fn-type aliases (from `erl_nif.h`), incl. the opaque `ErlNifEnv`
- `src/nif/api.rs` → `ErlNifApi` (176-entry `enif_*` function table + `ERL_NIF_API_FIELD_COUNT`; generated, header-ordered)
- `src/nif/host.rs` → the host seam: `NifHost` + `NifEnv` (`*mut ErlNifEnv`, `as_ptr`, `from_opaque`, `new`, `Drop`) + the `pub(crate)` `with_current_host`/`current_recorded_atom_name` readers
- `src/nif/enif/` → **one file per `enif_*` family** (`mem`, `get`, `make`, `map`, `pred`, `proc`, `process`, `exception`, `threads`, `ioq`, `mapiter`, `time`, `resources`, `select`, `envs`, `variadic`), each owning its `pub(super) const fn populate`; `mod.rs` holds the shared helpers, the submodule wiring and the `ENIF_API` chain; `tests.rs`/`process_tests.rs`/`mapiter_tests.rs`/`resources_tests.rs` are test-only children
- `src/nif/loader.rs` → the dynamic NIF loader: `NifLibrary`, `NifLoadError`/`NifCallError`, `load()` (dlopen/dlsym + oracle-order validation), `NifLibrary::call` (also the `priv_data` publication behind `enif_priv_data`), `Drop` (unload + dlclose)
- `src/driver/types.rs` → driver `#[repr(C)]` structs/enums/flags + `ErlDrvEntry` (from `erl_driver.h`) + `ERTS_INVALID_ERL_DRV_PORT` (the invalid-port sentinel, pointer `-1`)
- `src/driver/api.rs` → `ErlDrvApi` (111-entry `driver_*`/`erl_drv_*` function table) + `ErlDrvApi::empty()`
- `src/driver/host.rs` → the driver host seam: `DriverHost` (incl. the defaulted `output2` split hook) + `DriverEnv` (`*mut ErlDrvPort_`, `as_port`, `from_port`) + the three-way `MonitorResult`/`monitor_process_result` monitor hook + `decode_term_spec` + the `with_current_host` re-export
- `src/driver/current_host.rs` → the thread-local **current-driver-host** publication: `with_current_host(host, f)` (scoped, restores the previous host on exit, nesting-safe) plus the `pub(crate)` reader the two port-free entry-list slots use
- `src/driver/async_job.rs` → the **safe async-job runner** (`AsyncJob`/`AsyncCallback`, crate-root re-exports): the single `unsafe` `new` constructor plus the safe `invoke`/`finish`/`data`/`has_invoke`/`has_free`; `Drop` frees a held job without invoking it, `Debug` never dereferences
- `src/driver/drv/` → **one file per driver family** (`mem`, `port`, `queues`, `threads`, `pdl`, `lifecycle`, `registry`), each owning its `pub(super) const fn populate`; `mod.rs` holds the module docs, the shared `env_for` helper, the `#[used] static DRIVER_API` table + `driver_api_table()`, the `driver_term_nil` data symbol and `binary_payload`; `port_tests.rs` is the path-included child of `port`
- `src/driver/loader.rs` → the dynamic driver loader: `DriverLibrary`, `DriverLoadError`/`DriverCallError`, `load()` (`driver_init`, `ErlDrvEntry`), the per-callback wrappers, `Drop` (finish + dlclose)
- `src/layout.rs` → the compile-time layout freeze (all `const _: () = assert!(...)` for the table above)
- `tests/common/mod.rs` → shared test helpers (`mod common;`): `erl_include_dir`, `cc_available`, `compile_so`, `skip`, `TestHost`, `TestDriverHost`
- `tests/` → crate-level integration tests (`layout.rs`, `term_abi.rs`, `api_surface.rs`, `abi_conformance.rs`, `entry.rs`, `host_seam.rs`, `enif.rs`, `loader.rs`, `nif_end_to_end.rs`, `nif_process.rs`, `nif_resource.rs`, `nif_env.rs`, `nif_ioq.rs`, `nif_mapiter.rs`, `nif_threads.rs`, `nif_variadic.rs`, `nif_atom_env.rs`, `nif_get_string.rs`, `nif_bitslice.rs`, `nif_subbin.rs`, `nif_compare.rs`, `nif_priv_data.rs`, `driver_end_to_end.rs`, `driver_loader.rs`, `driver_binary_refc.rs`, `driver_lifecycle.rs`, `driver_queues.rs`, `driver_pdl.rs`, `driver_monitors.rs`, `driver_threads.rs`, `driver_async_job.rs`) + `tests/fixtures/*.c` (real C NIFs/drivers: `fixtures/nif_iolist.c` for the iolist/binary-shape oracle cross-check, `fixtures/nif_get_string_utf8.c` for the `ERL_NIF_UTF8` truncation, `fixtures/nif_compare.c`/`fixtures/nif_priv_data.c`/`fixtures/driver_monitors.c` for the divergence-fix families), all compiled at test time with the system `cc` against the system OTP headers

## Notes for Agents
- **Read the legacy headers before touching a layout**: `../../erts/emulator/beam/erl_nif.h`, `erl_nif_api_funcs.h`, `erl_drv_nif.h`, `erl_driver.h` — field order and integer widths must match exactly, and the asserts are the enforcement. Cite the header line where a value is non-obvious.
- `../../../erts/emulator/beam/global.h` line ~57 holds the real `struct enif_environment_t` body — emulator-internal and **not** to be replicated: `ErlNifEnv` stays opaque here, its Rust analogue for a live call being `nif::host::NifEnv`, reached only through `as_ptr`/`from_opaque`.
- `erl_int_sizes_config.h` (included by `erl_drv_nif.h`) fixes `SIZEOF_LONG == 8` on this target, which is why the 4 `enif_*64` table entries are excluded from `ErlNifApi`.
- **A family's slots live in that family's file** — to extend a family, edit the family file's body and add the slot to its `populate`; do not touch the `ENIF_API`/`DRIVER_API` chain in `mod.rs`.
- Building: **never `cd`**; use `../scripts/cargo.sh build -p beam_abi` / `../scripts/cargo.sh test -p beam_abi` (the sandbox `$HOME` is read-only; the wrapper picks a writable `CARGO_HOME`).
- `tests/common/mod.rs` is invisible to `cargo test` unless some `tests/*.rs` says `mod common;` — many files do, so keep at least one includer (`tests/common/mod.rs`).
- The driver loader's ground truth is a different C file from the NIF's: `do_load_driver_entry` in `../../erts/emulator/beam/erl_bif_ddll.c` (the `errcode_tab` reason atoms included), `erts_sys_ddll_load_driver_init` in `../../erts/emulator/sys/unix/erl_unix_sys_ddll.c`, and `init_driver` in `../../erts/emulator/beam/io.c`; the term-data decoder mirrors `io.c`'s `ERTS_DDT_*`. Do not touch `../../tools/` or `../../erts/`.
- A driver fixture must reference **only** `driver_*` slots the crate populates (`Some`), or `dlopen(RTLD_NOW)` fails on the unresolved import; the same rule applies to a NIF fixture and the `enif_*` slots.
- Adding a `cargo::rustc-link-arg-bins` instruction to `build.rs` **breaks the build** (no `[[bin]]` target in this package).
- **Term lifetime:** a term produced by a NIF lives on the host arena; keep the host (and the loaded library) alive while inspecting it, or read a use-after-free (see the Test Strategy pitfall).

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, layering waves (§0 places `beam_abi` in wave 0).
- `../../CONTEXT.md` → workspace contracts: §1 (A.3 NIF C ABI, A.4 driver C ABI), §2 frozen term layout, §3 error strategy.
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.3/A.4; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §C (the frozen-header seam).
- `../beam_term/` → the internal `Eterm` this crate transmutes to/from (`nif::term`).
- `../beam_alloc/` → the production bump `Heap` the host seam hands out (`NifHost::heap`).
- `../beam_rt/`, `../beam_bifs/`, `../beam_io/` → the runtime owners of the `NifHost`/`DriverHost` seams and the implementors of the `DriverHost` behaviours behind the `driver_*` seams; `beam_io` implements the driver-registry hooks over its driver table / port registry and wraps each driver callback **it** routes in `beam_abi::with_current_host`; `beam_rt` uses its own per-port `DriverEnv`-backed host (`self_pid`, `alloc_env`, `free_env`, `make_copy`, `add_driver_entry`, `remove_driver_entry`, `lock_driver`) — *Known gaps*.
