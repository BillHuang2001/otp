// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The NIF `#[repr(C)]` types from `erts/emulator/beam/erl_nif.h`.
//!
//! Field order, integer widths and structure sizes reproduce the OTP header exactly for the LP64
//! target (`SIZEOF_LONG == 8`, `SIZEOF_VOID_P == 8`); Windows-only branches (e.g. the
//! `WinDynNifCallbacks` struct) are intentionally not modelled. Every struct in this module is
//! pinned by a compile-time size/align assertion at the bottom of the file; the exhaustive
//! field-offset freeze lives in [`crate::layout`].
//!
//! Pure C enumerations (never OR-combined) become `#[repr(C)]` field-less Rust enums; C flag sets
//! (`ErlNifResourceFlags`, `ErlNifUniqueInteger`, …) become `#[repr(transparent)]` newtypes over
//! `c_int`/`c_uint` because callers OR the values together and a Rust enum cannot hold such a
//! value (crate `CONTEXT.md`, Design Decisions).

use crate::common::{ErlDrvMonitor, ErlDrvSysInfo, ErlDrvThreadOpts, SysIOVec};
use crate::nif::term::{ERL_NIF_TERM, ERL_NIF_UINT};

use core::ptr;
use libc::{c_char, c_int, c_uint, c_uchar, c_void, size_t};

// ---------------------------------------------------------------------------------------------
// Scalar typedefs / aliases
// ---------------------------------------------------------------------------------------------

/// `ErlNifUInt64` (`erl_nif.h`) — `ErlNapiUInt64`.
pub type ErlNifUInt64 = crate::common::ErlNapiUInt64;
/// `ErlNifSInt64` (`erl_nif.h`) — `ErlNapiSInt64`.
pub type ErlNifSInt64 = crate::common::ErlNapiSInt64;
/// `ErlNifUInt` (`erl_nif.h`) — `ErlNapiUInt`.
pub type ErlNifUInt = crate::common::ErlNapiUInt;
/// `ErlNifSInt` (`erl_nif.h`) — `ErlNapiSInt`.
pub type ErlNifSInt = crate::common::ErlNapiSInt;

/// `ErlNifTime` — a signed 64-bit time value (Erlang monotonic/system time).
pub type ErlNifTime = ErlNifSInt64;

/// `ErlNifEvent` — a selectable event handle (an `int` file descriptor on Unix).
pub type ErlNifEvent = c_int;

/// `ErlNifTSDKey` — thread-specific-data key.
pub type ErlNifTSDKey = c_int;

/// `ErlNifThreadOpts` — alias of `ErlDrvThreadOpts`.
pub type ErlNifThreadOpts = ErlDrvThreadOpts;

/// `ErlNifSysInfo` — alias of `ErlDrvSysInfo` (`typedef ErlDrvSysInfo ErlNifSysInfo;`).
pub type ErlNifSysInfo = ErlDrvSysInfo;

/// `ErlNifMonitor` — alias of `ErlDrvMonitor` (`typedef ErlDrvMonitor ErlNifMonitor;`).
pub type ErlNifMonitor = ErlDrvMonitor;

// ---------------------------------------------------------------------------------------------
// Opaque handles (incomplete C types — used behind pointers, never by value in this crate)
// ---------------------------------------------------------------------------------------------

/// `ErlNifEnv` — `struct enif_environment_t`, the per-call NIF environment.
///
/// Emulator-internal (`erts/emulator/beam/global.h`); deliberately **opaque** here — never
/// dereferenced, never instantiated.
#[repr(C)]
pub struct ErlNifEnv {
    _private: [u8; 0],
}

/// `ErlNifResourceType` — `struct enif_resource_type_t`, a NIF resource type handle.
#[repr(C)]
pub struct ErlNifResourceType {
    _private: [u8; 0],
}

/// `ErlNifIOQueue` — `struct erts_io_queue`, the NIF I/O queue handle.
#[repr(C)]
pub struct ErlNifIOQueue {
    _private: [u8; 0],
}

/// `struct ErtsDynamicWStack_` — the emulator's dynamic (heap-allocated) term stack.
///
/// Referenced by [`ErlNifMapIteratorHash`]; opaque and never dereferenced here.
#[repr(C)]
pub struct ErtsDynamicWStack {
    _private: [u8; 0],
}

// ---------------------------------------------------------------------------------------------
// Thread/mutex handles shared with the driver ABI (`erl_nif.h` aliases the `ErlDrv*` types)
// ---------------------------------------------------------------------------------------------

/// `ErlNifTid` — `typedef struct ErlDrvTid_ *ErlNifTid;`.
pub type ErlNifTid = crate::driver::types::ErlDrvTid;
/// `ErlNifMutex` — `typedef struct ErlDrvMutex_ ErlNifMutex;` (used behind a pointer).
pub type ErlNifMutex = crate::driver::types::ErlDrvMutex;
/// `ErlNifCond` — `typedef struct ErlDrvCond_ ErlNifCond;` (used behind a pointer).
pub type ErlNifCond = crate::driver::types::ErlDrvCond;
/// `ErlNifRWLock` — `typedef struct ErlDrvRWLock_ ErlNifRWLock;` (used behind a pointer).
pub type ErlNifRWLock = crate::driver::types::ErlDrvRWLock;

// ---------------------------------------------------------------------------------------------
// `va_list` stand-in
// ---------------------------------------------------------------------------------------------

/// Opaque marker for the C `va_list` *parameter* of `enif_vfprintf`/`enif_vsnprintf`.
///
/// Rust has no stable `va_list`. On the supported ABIs (x86-64 System V, AArch64 AAPCS) a
/// `va_list` passed as a function *argument* is passed indirectly — i.e. as a pointer to the list
/// state — so a pointer-sized opaque type is ABI-correct for those two table entries. It is never
/// dereferenced.
#[repr(C)]
pub struct VaListTag {
    _private: [u8; 0],
}

/// The modelled C `va_list` (see [`VaListTag`]).
pub type VaList = *mut VaListTag;

// ---------------------------------------------------------------------------------------------
// Function-type aliases
// ---------------------------------------------------------------------------------------------

/// `ErlNifResourceDtor` — a resource destructor (called when the last reference is dropped).
pub type ErlNifResourceDtor = unsafe extern "C" fn(*mut ErlNifEnv, *mut c_void);

/// `ErlNifResourceStop` — `ERL_NIF_SELECT_STOP` callback for a resource.
pub type ErlNifResourceStop =
    unsafe extern "C" fn(*mut ErlNifEnv, *mut c_void, ErlNifEvent, c_int);

/// `ErlNifResourceDown` — `enif_monitor_process` "down" callback for a resource.
pub type ErlNifResourceDown =
    unsafe extern "C" fn(*mut ErlNifEnv, *mut c_void, *mut ErlNifPid, *mut ErlNifMonitor);

/// `ErlNifResourceDynCall` — callback for `enif_dynamic_resource_call`.
pub type ErlNifResourceDynCall = unsafe extern "C" fn(*mut ErlNifEnv, *mut c_void, *mut c_void);

/// `ErlNifOnHaltCallback` — `ERL_NIF_OPT_ON_HALT` callback.
pub type ErlNifOnHaltCallback = unsafe extern "C" fn(*mut c_void);

/// `ErlNifOnUnloadThreadCallback` — `ERL_NIF_OPT_ON_UNLOAD_THREAD` callback.
pub type ErlNifOnUnloadThreadCallback = unsafe extern "C" fn(*mut c_void);

// ---------------------------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------------------------

/// `ErlNifFunc` — one entry of a NIF library's function table.
///
/// Layout (32/8): `name` 0, `arity` 8, `fptr` 16, `flags` 24.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlNifFunc {
    /// The Erlang name the function is exported as.
    pub name: *const c_char,
    /// Arity of the exported function.
    pub arity: c_uint,
    /// The native implementation.
    pub fptr: Option<unsafe extern "C" fn(*mut ErlNifEnv, c_int, *const ERL_NIF_TERM) -> ERL_NIF_TERM>,
    /// `ERL_NIF_DIRTY_JOB_*` flags (0 = normal scheduler).
    pub flags: c_uint,
}

impl Default for ErlNifFunc {
    fn default() -> Self {
        Self {
            name: ptr::null(),
            arity: 0,
            fptr: None,
            flags: 0,
        }
    }
}

/// `ErlNifEntry` — the `nif_init()` result describing a NIF library.
///
/// Layout (96/8): `major` 0, `minor` 4, `name` 8, `num_of_funcs` 16, `funcs` 24, `load` 32,
/// `reload` 40, `upgrade` 48, `unload` 56, `vm_variant` 64, `options` 72,
/// `sizeof_ErlNifResourceTypeInit` 80, `min_erts` 88.
///
/// The field names mirror `erl_nif.h` verbatim (ABI/field-order fidelity — callers and tests use
/// `offset_of!(ErlNifEntry, sizeof_ErlNifResourceTypeInit)`), so `non_snake_case` is allowed here.
#[repr(C)]
#[allow(non_snake_case)]
#[derive(Copy, Clone, Debug)]
pub struct ErlNifEntry {
    /// `ERL_NIF_MAJOR_VERSION` the library was built against.
    pub major: c_int,
    /// `ERL_NIF_MINOR_VERSION` the library was built against.
    pub minor: c_int,
    /// The Erlang module name the library implements.
    pub name: *const c_char,
    /// Number of entries in `funcs`.
    pub num_of_funcs: c_int,
    /// The NIF function table.
    pub funcs: *mut ErlNifFunc,
    /// Called when the library is loaded.
    pub load:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut *mut c_void, ERL_NIF_TERM) -> c_int>,
    /// Called instead of `load` when a library is reloaded.
    pub reload:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut *mut c_void, ERL_NIF_TERM) -> c_int>,
    /// Called on module upgrade.
    pub upgrade: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *mut *mut c_void, *mut *mut c_void, ERL_NIF_TERM) -> c_int,
    >,
    /// Called on module unload.
    pub unload: Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut c_void)>,
    /// `ERL_NIF_VM_VARIANT` the library was built for.
    pub vm_variant: *const c_char,
    /// Library options (unused; 0 or 1).
    pub options: c_uint,
    /// `sizeof(ErlNifResourceTypeInit)` as the library saw it.
    pub sizeof_ErlNifResourceTypeInit: size_t,
    /// Minimum ERTS version required (`ERL_NIF_MIN_ERTS_VERSION`).
    pub min_erts: *const c_char,
}

impl Default for ErlNifEntry {
    fn default() -> Self {
        Self {
            major: 0,
            minor: 0,
            name: ptr::null(),
            num_of_funcs: 0,
            funcs: ptr::null_mut(),
            load: None,
            reload: None,
            upgrade: None,
            unload: None,
            vm_variant: ptr::null(),
            options: 0,
            sizeof_ErlNifResourceTypeInit: 0,
            min_erts: ptr::null(),
        }
    }
}

/// `ErlNifBinary` — a NIF binary (immutable byte buffer, possibly a refcounted sub-binary).
///
/// Layout (40/8): `size` 0, `data` 8, `ref_bin` 16, `__spare__` 24.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlNifBinary {
    /// Size in bytes.
    pub size: size_t,
    /// The (read-only by contract) bytes.
    pub data: *mut c_uchar,
    /// Internals (refcounted binary or sub-binary).
    pub ref_bin: *mut c_void,
    /// Reserved, for ABI-compatible future additions.
    pub __spare__: [*mut c_void; 2],
}

impl Default for ErlNifBinary {
    fn default() -> Self {
        Self {
            size: 0,
            data: ptr::null_mut(),
            ref_bin: ptr::null_mut(),
            __spare__: [ptr::null_mut(); 2],
        }
    }
}

/// `ErlNifPid` — a NIF process identifier (internal representation, may change).
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct ErlNifPid {
    /// The encoded pid.
    pub pid: ERL_NIF_TERM,
}

/// `ErlNifPort` — a NIF port identifier (internal representation, may change).
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct ErlNifPort {
    /// The encoded port id.
    pub port_id: ERL_NIF_TERM,
}

/// `ErlNifResourceTypeInit` — the resource-type descriptors passed to `enif_init_resource_type`.
///
/// Layout (40/8): `dtor` 0, `stop` 8, `down` 16, `members` 24, `dyncall` 32.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct ErlNifResourceTypeInit {
    /// Destructor.
    pub dtor: Option<ErlNifResourceDtor>,
    /// `ERL_NIF_SELECT_STOP` callback.
    pub stop: Option<ErlNifResourceStop>,
    /// `enif_monitor_process` "down" callback.
    pub down: Option<ErlNifResourceDown>,
    /// The number of trailing members this struct was initialised with.
    pub members: c_int,
    /// `enif_dynamic_resource_call` callback.
    pub dyncall: Option<ErlNifResourceDynCall>,
}

/// `ErlNifMapIteratorFlat` — the flat-map (`ks`/`vs` array) arm of the map-iterator union.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlNifMapIteratorFlat {
    /// Key array.
    pub ks: *mut ERL_NIF_TERM,
    /// Value array.
    pub vs: *mut ERL_NIF_TERM,
}

impl Default for ErlNifMapIteratorFlat {
    fn default() -> Self {
        Self {
            ks: ptr::null_mut(),
            vs: ptr::null_mut(),
        }
    }
}

/// `ErlNifMapIteratorHash` — the hashed-map arm of the map-iterator union.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlNifMapIteratorHash {
    /// Emulator-internal work stack.
    pub wstack: *mut ErtsDynamicWStack,
    /// Emulator-internal key/value area.
    pub kv: *mut ERL_NIF_TERM,
}

impl Default for ErlNifMapIteratorHash {
    fn default() -> Self {
        Self {
            wstack: ptr::null_mut(),
            kv: ptr::null_mut(),
        }
    }
}

/// `ErlNifMapIterator`'s anonymous `u` union (both arms are 16 bytes, align 8).
#[repr(C)]
#[derive(Copy, Clone)]
pub union ErlNifMapIteratorUnion {
    /// Flat-map arm.
    pub flat: ErlNifMapIteratorFlat,
    /// Hashed-map arm.
    pub hash: ErlNifMapIteratorHash,
}

impl core::fmt::Debug for ErlNifMapIteratorUnion {
    /// A union cannot be read without knowing the active arm, so this prints the union opaquely
    /// (the same information `derive(Debug)` could safely provide).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ErlNifMapIteratorUnion { .. }")
    }
}

/// `ErlNifMapIterator` — the state of an in-progress map iteration.
///
/// Layout (56/8): `map` 0, `size` 8, `idx` 16, `u` 24, `__spare__` 40. All fields are internal
/// and may change.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlNifMapIterator {
    /// The map being iterated.
    pub map: ERL_NIF_TERM,
    /// Number of key/value pairs.
    pub size: ERL_NIF_UINT,
    /// Current iteration index.
    pub idx: ERL_NIF_UINT,
    /// Flat/hashed implementation detail.
    pub u: ErlNifMapIteratorUnion,
    /// Reserved, for ABI-compatible future additions.
    pub __spare__: [*mut c_void; 2],
}

impl Default for ErlNifMapIterator {
    fn default() -> Self {
        Self {
            map: ERL_NIF_TERM::default(),
            size: ERL_NIF_UINT::default(),
            idx: ERL_NIF_UINT::default(),
            u: ErlNifMapIteratorUnion {
                flat: ErlNifMapIteratorFlat::default(),
            },
            __spare__: [ptr::null_mut(); 2],
        }
    }
}

/// `ERL_NIF_IOVEC_SIZE` — the number of stack-allocated vectors in [`ErlNifIOVec`].
pub const ERL_NIF_IOVEC_SIZE: usize = 16;

/// `ErlNifIOVec` — a stack (or heap) allocated scatter/gather buffer used by the I/O BIFs.
///
/// Layout (424/8): `iovcnt` 0, `size` 8, `iov` 16, `ref_bins` 24, `flags` 32, `small_iov` 40,
/// `small_ref_bin` 296.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlNifIOVec {
    /// Number of valid vectors in `iov`.
    pub iovcnt: c_int,
    /// Total size in bytes.
    pub size: size_t,
    /// The vector array (either `small_iov` or heap-allocated).
    pub iov: *mut SysIOVec,
    /// Internals: the `ErlNifBinary*` array backing the vectors.
    pub ref_bins: *mut *mut c_void,
    /// Internal flags.
    pub flags: c_int,
    /// Stack-allocated vectors.
    pub small_iov: [SysIOVec; ERL_NIF_IOVEC_SIZE],
    /// Stack-allocated binary references.
    pub small_ref_bin: [*mut c_void; ERL_NIF_IOVEC_SIZE],
}

// ---------------------------------------------------------------------------------------------
// Pure enumerations (`#[repr(C)]`, 4 bytes — never OR-combined, so a real Rust enum is correct)
// ---------------------------------------------------------------------------------------------

/// `ErlNifTimeUnit` — a time unit for the NIF time API.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ErlNifTimeUnit {
    /// `ERL_NIF_SEC`.
    Sec = 0,
    /// `ERL_NIF_MSEC`.
    Msec = 1,
    /// `ERL_NIF_USEC`.
    Usec = 2,
    /// `ERL_NIF_NSEC`.
    Nsec = 3,
}

/// `ErlNifCharEncoding` — the character encoding of a NIF string.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ErlNifCharEncoding {
    /// `ERL_NIF_LATIN1`.
    Latin1 = 1,
    /// `ERL_NIF_UTF8`.
    Utf8 = 2,
}

/// `ErlNifMapIteratorEntry` — where a map iteration starts.
///
/// The header also defines the deprecated synonyms `ERL_NIF_MAP_ITERATOR_HEAD`/`_TAIL`; Rust
/// forbids two variants with the same discriminant, so they are exposed as associated constants
/// ([`ErlNifMapIteratorEntry::HEAD`] / [`ErlNifMapIteratorEntry::TAIL`]).
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ErlNifMapIteratorEntry {
    /// `ERL_NIF_MAP_ITERATOR_FIRST` (deprecated synonym: `HEAD`).
    First = 1,
    /// `ERL_NIF_MAP_ITERATOR_LAST` (deprecated synonym: `TAIL`).
    Last = 2,
}

impl ErlNifMapIteratorEntry {
    /// Deprecated synonym for [`ErlNifMapIteratorEntry::First`]
    /// (`ERL_NIF_MAP_ITERATOR_HEAD`).
    pub const HEAD: Self = Self::First;
    /// Deprecated synonym for [`ErlNifMapIteratorEntry::Last`]
    /// (`ERL_NIF_MAP_ITERATOR_TAIL`).
    pub const TAIL: Self = Self::Last;
}

/// `ErlNifHash` — the hash algorithm used by `enif_hash`.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ErlNifHash {
    /// `ERL_NIF_INTERNAL_HASH`.
    InternalHash = 1,
    /// `ERL_NIF_PHASH2`.
    Phash2 = 2,
}

/// `ErlNifTermType` — the result of `enif_term_type`.
///
/// The negative `MissingDefaultCaseReadTheManual` variant exists to force callers to handle a
/// `default` case, exactly as in the header.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ErlNifTermType {
    /// `ERL_NIF_TERM_TYPE_ATOM`.
    Atom = 1,
    /// `ERL_NIF_TERM_TYPE_BITSTRING`.
    Bitstring = 2,
    /// `ERL_NIF_TERM_TYPE_FLOAT`.
    Float = 3,
    /// `ERL_NIF_TERM_TYPE_FUN`.
    Fun = 4,
    /// `ERL_NIF_TERM_TYPE_INTEGER`.
    Integer = 5,
    /// `ERL_NIF_TERM_TYPE_LIST`.
    List = 6,
    /// `ERL_NIF_TERM_TYPE_MAP`.
    Map = 7,
    /// `ERL_NIF_TERM_TYPE_PID`.
    Pid = 8,
    /// `ERL_NIF_TERM_TYPE_PORT`.
    Port = 9,
    /// `ERL_NIF_TERM_TYPE_REFERENCE`.
    Reference = 10,
    /// `ERL_NIF_TERM_TYPE_TUPLE`.
    Tuple = 11,
    /// `ERL_NIF_TERM_TYPE__MISSING_DEFAULT_CASE__READ_THE_MANUAL` — a dummy value that forces
    /// callers to have a `default` case.
    MissingDefaultCaseReadTheManual = -1,
}

/// `ErlNifOption` — options for `enif_set_option`.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ErlNifOption {
    /// `ERL_NIF_OPT_DELAY_HALT`.
    DelayHalt = 1,
    /// `ERL_NIF_OPT_ON_HALT`.
    OnHalt = 2,
    /// `ERL_NIF_OPT_ON_UNLOAD_THREAD`.
    OnUnloadThread = 3,
}

/// `ErlNifIOQueueOpts` — options for `enif_ioq_create`.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ErlNifIOQueueOpts {
    /// `ERL_NIF_IOQ_NORMAL`.
    Normal = 1,
}

// ---------------------------------------------------------------------------------------------
// Flag sets (`#[repr(transparent)]` newtypes — the values are OR-combined, so not Rust enums)
// ---------------------------------------------------------------------------------------------

/// `ErlNifResourceFlags` — flags for `enif_open_resource_type_x`/`enif_init_resource_type`.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ErlNifResourceFlags(c_int);

impl ErlNifResourceFlags {
    /// `ERL_NIF_RT_CREATE`.
    pub const CREATE: Self = Self(1);
    /// `ERL_NIF_RT_TAKEOVER`.
    pub const TAKEOVER: Self = Self(2);

    /// The raw C value.
    #[inline]
    pub const fn bits(self) -> c_int {
        self.0
    }

    /// Wrap a raw C value.
    #[inline]
    pub const fn from_bits(bits: c_int) -> Self {
        Self(bits)
    }
}

/// `ErlNifUniqueInteger` — flags for `enif_make_unique_integer`.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ErlNifUniqueInteger(c_int);

impl ErlNifUniqueInteger {
    /// `ERL_NIF_UNIQUE_POSITIVE`.
    pub const POSITIVE: Self = Self(1 << 0);
    /// `ERL_NIF_UNIQUE_MONOTONIC`.
    pub const MONOTONIC: Self = Self(1 << 1);

    /// The raw C value.
    #[inline]
    pub const fn bits(self) -> c_int {
        self.0
    }

    /// Wrap a raw C value.
    #[inline]
    pub const fn from_bits(bits: c_int) -> Self {
        Self(bits)
    }
}

/// `ErlNifDirtyTaskFlags` — flags for `enif_schedule_nif` (`ERL_NIF_DIRTY_JOB_*`).
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ErlNifDirtyTaskFlags(c_int);

impl ErlNifDirtyTaskFlags {
    /// `ERL_NIF_DIRTY_JOB_CPU_BOUND`.
    pub const CPU_BOUND: Self = Self(crate::common::ERL_DIRTY_JOB_CPU_BOUND);
    /// `ERL_NIF_DIRTY_JOB_IO_BOUND`.
    pub const IO_BOUND: Self = Self(crate::common::ERL_DIRTY_JOB_IO_BOUND);

    /// The raw C value.
    #[inline]
    pub const fn bits(self) -> c_int {
        self.0
    }

    /// Wrap a raw C value.
    #[inline]
    pub const fn from_bits(bits: c_int) -> Self {
        Self(bits)
    }
}

/// `ErlNifBinaryToTerm` — flags for `enif_binary_to_term` (note: `unsigned`, not `int`).
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct ErlNifBinaryToTerm(c_uint);

impl ErlNifBinaryToTerm {
    /// `ERL_NIF_BIN2TERM_SAFE`.
    pub const SAFE: Self = Self(0x2000_0000);

    /// The raw C value.
    #[inline]
    pub const fn bits(self) -> c_uint {
        self.0
    }

    /// Wrap a raw C value.
    #[inline]
    pub const fn from_bits(bits: c_uint) -> Self {
        Self(bits)
    }
}

// ---------------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_TIME_ERROR` — the invalid-time sentinel returned by the NIF time API.
pub const ERL_NIF_TIME_ERROR: ErlNifSInt64 = crate::common::ERTS_NAPI_TIME_ERROR;

/// `ERL_NIF_THR_UNDEFINED` — not a scheduler thread.
pub const ERL_NIF_THR_UNDEFINED: c_int = 0;
/// `ERL_NIF_THR_NORMAL_SCHEDULER` — a normal scheduler thread.
pub const ERL_NIF_THR_NORMAL_SCHEDULER: c_int = 1;
/// `ERL_NIF_THR_DIRTY_CPU_SCHEDULER` — a dirty CPU scheduler thread.
pub const ERL_NIF_THR_DIRTY_CPU_SCHEDULER: c_int = 2;
/// `ERL_NIF_THR_DIRTY_IO_SCHEDULER` — a dirty I/O scheduler thread.
pub const ERL_NIF_THR_DIRTY_IO_SCHEDULER: c_int = 3;

// The ABI version constants are owned by the crate root (see `src/lib.rs`, the authoritative
// location per the CONTEXT.md routing table) and re-exported here so `nif::types::ERL_NIF_*`
// resolves too.
pub use crate::{
    ERL_NIF_MAJOR_VERSION, ERL_NIF_MIN_ERTS_VERSION, ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD,
    ERL_NIF_MINOR_VERSION, ERL_NIF_VM_VARIANT,
};

// ---------------------------------------------------------------------------------------------
// Compile-time layout freeze (LP64)
// ---------------------------------------------------------------------------------------------

const _: () = {
    assert!(core::mem::size_of::<ErlNifFunc>() == 32);
    assert!(core::mem::align_of::<ErlNifFunc>() == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlNifEntry>() == 96);
    assert!(core::mem::align_of::<ErlNifEntry>() == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlNifBinary>() == 40);
    assert!(core::mem::align_of::<ErlNifBinary>() == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlNifPid>() == 8);
    assert!(core::mem::align_of::<ErlNifPid>() == 8);
    assert!(core::mem::size_of::<ErlNifPort>() == 8);
    assert!(core::mem::align_of::<ErlNifPort>() == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlNifResourceTypeInit>() == 40);
    assert!(core::mem::align_of::<ErlNifResourceTypeInit>() == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlNifMapIteratorFlat>() == 16);
    assert!(core::mem::size_of::<ErlNifMapIteratorHash>() == 16);
    assert!(core::mem::size_of::<ErlNifMapIteratorUnion>() == 16);
    assert!(core::mem::align_of::<ErlNifMapIteratorUnion>() == 8);
    assert!(core::mem::size_of::<ErlNifMapIterator>() == 56);
    assert!(core::mem::align_of::<ErlNifMapIterator>() == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlNifIOVec>() == 424);
    assert!(core::mem::align_of::<ErlNifIOVec>() == 8);
    assert!(ERL_NIF_IOVEC_SIZE == 16);
};

const _: () = {
    assert!(core::mem::size_of::<ErlNifTimeUnit>() == 4);
    assert!(core::mem::size_of::<ErlNifCharEncoding>() == 4);
    assert!(core::mem::size_of::<ErlNifMapIteratorEntry>() == 4);
    assert!(core::mem::size_of::<ErlNifHash>() == 4);
    assert!(core::mem::size_of::<ErlNifTermType>() == 4);
    assert!(core::mem::size_of::<ErlNifOption>() == 4);
    assert!(core::mem::size_of::<ErlNifIOQueueOpts>() == 4);
};

const _: () = {
    assert!(core::mem::size_of::<ErlNifResourceFlags>() == 4);
    assert!(core::mem::align_of::<ErlNifResourceFlags>() == 4);
    assert!(core::mem::size_of::<ErlNifUniqueInteger>() == 4);
    assert!(core::mem::align_of::<ErlNifUniqueInteger>() == 4);
    assert!(core::mem::size_of::<ErlNifDirtyTaskFlags>() == 4);
    assert!(core::mem::align_of::<ErlNifDirtyTaskFlags>() == 4);
    assert!(core::mem::size_of::<ErlNifBinaryToTerm>() == 4);
    assert!(core::mem::align_of::<ErlNifBinaryToTerm>() == 4);
};

const _: () = {
    assert!(core::mem::size_of::<ErlNifEnv>() == 0);
    assert!(core::mem::size_of::<ErlNifResourceType>() == 0);
    assert!(core::mem::size_of::<ErlNifIOQueue>() == 0);
    assert!(core::mem::size_of::<ErtsDynamicWStack>() == 0);
    assert!(core::mem::size_of::<VaListTag>() == 0);
    assert!(core::mem::size_of::<VaList>() == core::mem::size_of::<*mut c_void>());
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_discriminants_match_the_header() {
        assert_eq!(ErlNifTimeUnit::Sec as c_int, 0);
        assert_eq!(ErlNifTimeUnit::Nsec as c_int, 3);
        assert_eq!(ErlNifCharEncoding::Latin1 as c_int, 1);
        assert_eq!(ErlNifCharEncoding::Utf8 as c_int, 2);
        assert_eq!(ErlNifMapIteratorEntry::First as c_int, 1);
        assert_eq!(ErlNifMapIteratorEntry::Last as c_int, 2);
        assert_eq!(ErlNifMapIteratorEntry::HEAD as c_int, 1);
        assert_eq!(ErlNifMapIteratorEntry::TAIL as c_int, 2);
        assert_eq!(ErlNifHash::InternalHash as c_int, 1);
        assert_eq!(ErlNifHash::Phash2 as c_int, 2);
        assert_eq!(ErlNifTermType::Atom as c_int, 1);
        assert_eq!(ErlNifTermType::Tuple as c_int, 11);
        assert_eq!(ErlNifTermType::MissingDefaultCaseReadTheManual as c_int, -1);
        assert_eq!(ErlNifOption::DelayHalt as c_int, 1);
        assert_eq!(ErlNifOption::OnUnloadThread as c_int, 3);
        assert_eq!(ErlNifIOQueueOpts::Normal as c_int, 1);
    }

    #[test]
    fn flag_values_match_the_header() {
        assert_eq!(ErlNifResourceFlags::CREATE.bits(), 1);
        assert_eq!(ErlNifResourceFlags::TAKEOVER.bits(), 2);
        assert_eq!(ErlNifUniqueInteger::POSITIVE.bits(), 1);
        assert_eq!(ErlNifUniqueInteger::MONOTONIC.bits(), 2);
        assert_eq!(ErlNifDirtyTaskFlags::CPU_BOUND.bits(), 1);
        assert_eq!(ErlNifDirtyTaskFlags::IO_BOUND.bits(), 2);
        assert_eq!(ErlNifBinaryToTerm::SAFE.bits(), 0x2000_0000);
        assert_eq!(
            ErlNifResourceFlags::from_bits(
                ErlNifResourceFlags::CREATE.bits() | ErlNifResourceFlags::TAKEOVER.bits()
            )
            .bits(),
            3
        );
    }

    #[test]
    fn default_library_entry_is_all_null() {
        let e = ErlNifEntry::default();
        assert_eq!(e.major, 0);
        assert_eq!(e.num_of_funcs, 0);
        assert!(e.funcs.is_null());
        assert!(e.load.is_none());
        assert!(e.unload.is_none());
        assert!(e.vm_variant.is_null());
        assert!(e.min_erts.is_null());

        let f = ErlNifFunc::default();
        assert!(f.name.is_null());
        assert!(f.fptr.is_none());

        let m = ErlNifMapIterator::default();
        assert_eq!(m.map, ERL_NIF_TERM::default());
        assert_eq!(m.size, ERL_NIF_UINT::default());
        assert_eq!(m.idx, ERL_NIF_UINT::default());
        assert!(m.__spare__[0].is_null());
    }

    #[test]
    fn version_constants_are_reachable_through_the_nif_module() {
        assert_eq!(ERL_NIF_MAJOR_VERSION, 2);
        assert_eq!(ERL_NIF_MINOR_VERSION, 18);
        assert_eq!(ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD, 2);
        assert_eq!(ERL_NIF_VM_VARIANT, "beam.vanilla");
        assert_eq!(ERL_NIF_MIN_ERTS_VERSION, "erts-14.0");
        assert_eq!(ERL_NIF_TIME_ERROR, i64::MIN);
    }
}
