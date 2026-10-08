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

//! The consolidated compile-time layout freeze for the frozen NIF/driver C ABIs (LP64).
//!
//! # This file IS the freeze
//!
//! Every type the NIF C ABI (contract **A.3**) and the driver C ABI (contract **A.4**) expose has
//! its `size_of`, `align_of` and (for structs) its field `offset_of`s pinned here by
//! `const`-evaluated `assert!`s. These are evaluated **during compilation**: if a Rust declaration
//! ever drifts from the legacy header, the crate fails to compile — there is no runtime escape.
//!
//! The assertions use `core::mem::{size_of, offset_of}` (both `const` and stable on the 1.80
//! toolchain), so no external assertion crate is needed.
//!
//! # The fix is always to correct the Rust declaration
//!
//! Ground truth is the *unmodified* legacy headers (`erts/emulator/beam/erl_nif.h`,
//! `erl_nif_api_funcs.h`, `erl_drv_nif.h`, `erl_driver.h`). If one of these numbers ever fails,
//! **fix the Rust declaration to match the header** — never weaken, relax, delete or `#[ignore]`
//! the assert. A mismatch here *is* an ABI break.
//!
//! # Numbers are for LP64
//!
//! The target is 64-bit LP64 (`SIZEOF_LONG == 8`, `SIZEOF_VOID_P == 8`; pointer/`size_t`/
//! `unsigned long` = 8, `int` = 4). Every number below is the LP64 header layout. (`SysIOVec`
//! aliases `libc::iovec`, guaranteeing `readv`/`writev` compatibility.)
//!
//! The per-file size asserts already present in [`crate::common`], [`crate::nif::types`],
//! [`crate::nif::api`], [`crate::driver::types`] and [`crate::driver::api`] may stay; this module
//! is the *consolidated* freeze of every size, align and offset in one place.

use crate::common::{
    ErlDirtyJobFlags, ErlDrvMonitor, ErlDrvSysInfo, ErlDrvThreadOpts, ErlNapiSInt, ErlNapiSInt64,
    ErlNapiUInt, ErlNapiUInt64, ErlNifSelectFlags, SysIOVec, ERL_DIRTY_JOB_CPU_BOUND,
    ERL_DIRTY_JOB_IO_BOUND, ERL_NIF_SELECT_CANCEL, ERL_NIF_SELECT_CUSTOM_MSG, ERL_NIF_SELECT_ERROR,
    ERL_NIF_SELECT_ERROR_CANCELLED, ERL_NIF_SELECT_FAILED, ERL_NIF_SELECT_INVALID_EVENT,
    ERL_NIF_SELECT_NOTSUP, ERL_NIF_SELECT_READ, ERL_NIF_SELECT_READ_CANCELLED, ERL_NIF_SELECT_STOP,
    ERL_NIF_SELECT_STOP_CALLED, ERL_NIF_SELECT_STOP_SCHEDULED, ERL_NIF_SELECT_WRITE,
    ERL_NIF_SELECT_WRITE_CANCELLED, ERTS_NAPI_MSEC, ERTS_NAPI_NSEC, ERTS_NAPI_SEC,
    ERTS_NAPI_TIME_ERROR, ERTS_NAPI_USEC,
};
use crate::driver::api::ErlDrvApi;
use crate::driver::types::{
    ErlDrvBinary, ErlDrvCond, ErlDrvCond_, ErlDrvData, ErlDrvData_, ErlDrvEntry, ErlDrvEvent,
    ErlDrvEvent_, ErlDrvMutex, ErlDrvMutex_, ErlDrvNowData, ErlDrvPDL, ErlDrvPort, ErlDrvPort_,
    ErlDrvPortDataLock, ErlDrvRWLock, ErlDrvRWLock_, ErlDrvSSizeT, ErlDrvSInt, ErlDrvSInt64,
    ErlDrvSizeT, ErlDrvTSDKey, ErlDrvTermData, ErlDrvThreadData, ErlDrvTid, ErlDrvTid_, ErlDrvTime,
    ErlDrvTimeUnit, ErlDrvUInt, ErlDrvUInt64, ErlIOVec, DRIVER_CALL_KEEP_BUFFER,
    ERL_DRV_ATOM, ERL_DRV_BINARY, ERL_DRV_BUF2BINARY, ERL_DRV_BUSY_MSGQ_DISABLED,
    ERL_DRV_BUSY_MSGQ_LIM_MAX, ERL_DRV_BUSY_MSGQ_LIM_MIN, ERL_DRV_BUSY_MSGQ_READ_ONLY,
    ERL_DRV_EXT2TERM, ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MARKER,
    ERL_DRV_EXTENDED_MINOR_VERSION, ERL_DRV_FLAG_NO_BUSY_MSGQ, ERL_DRV_FLAG_SOFT_BUSY,
    ERL_DRV_FLAG_USE_INIT_ACK, ERL_DRV_FLAG_USE_PORT_LOCKING, ERL_DRV_FLOAT, ERL_DRV_INT,
    ERL_DRV_INT64, ERL_DRV_LIST, ERL_DRV_MAP, ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD,
    ERL_DRV_NIL, ERL_DRV_PID, ERL_DRV_PORT, ERL_DRV_READ, ERL_DRV_STRING, ERL_DRV_STRING_CONS,
    ERL_DRV_TIME_ERROR, ERL_DRV_TUPLE, ERL_DRV_UINT, ERL_DRV_UINT64, ERL_DRV_USE,
    ERL_DRV_USE_NO_CALLBACK, ERL_DRV_WRITE, PORT_CONTROL_FLAG_BINARY, PORT_CONTROL_FLAG_HEAVY,
    PORT_FLAG_BINARY, PORT_FLAG_LINE,
};
use crate::nif::api::ErlNifApi;
use crate::nif::term::{ERL_NIF_TERM, ERL_NIF_UINT};
use crate::nif::types::{
    ErlNifBinary, ErlNifBinaryToTerm, ErlNifCharEncoding, ErlNifCond, ErlNifDirtyTaskFlags,
    ErlNifEntry, ErlNifEnv, ErlNifEvent, ErlNifFunc, ErlNifHash, ErlNifIOQueue, ErlNifIOQueueOpts,
    ErlNifIOVec, ErlNifMapIterator, ErlNifMapIteratorEntry, ErlNifMapIteratorFlat,
    ErlNifMapIteratorHash, ErlNifMapIteratorUnion, ErlNifMonitor, ErlNifMutex,
    ErlNifOnHaltCallback, ErlNifOnUnloadThreadCallback, ErlNifOption, ErlNifPid, ErlNifPort,
    ErlNifRWLock, ErlNifResourceDown, ErlNifResourceDtor, ErlNifResourceDynCall,
    ErlNifResourceFlags, ErlNifResourceStop, ErlNifResourceType, ErlNifResourceTypeInit, ErlNifSInt,
    ErlNifSInt64, ErlNifSysInfo, ErlNifTSDKey, ErlNifTermType, ErlNifThreadOpts, ErlNifTid,
    ErlNifTime, ErlNifTimeUnit, ErlNifUInt, ErlNifUInt64, ErlNifUniqueInteger, ErtsDynamicWStack,
    VaList, VaListTag, ERL_NIF_IOVEC_SIZE, ERL_NIF_THR_DIRTY_CPU_SCHEDULER,
    ERL_NIF_THR_DIRTY_IO_SCHEDULER, ERL_NIF_THR_NORMAL_SCHEDULER, ERL_NIF_THR_UNDEFINED,
    ERL_NIF_TIME_ERROR,
};
use crate::{
    ERL_NIF_MAJOR_VERSION, ERL_NIF_MIN_ERTS_VERSION, ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD,
    ERL_NIF_MINOR_VERSION, ERL_NIF_VM_VARIANT,
};
use beam_term::Eterm;
use libc::c_int;

/// `const`-evaluable `&str` ↔ byte-string comparison, used for the string-valued version macros
/// (`ERL_NIF_VM_VARIANT`, `ERL_NIF_MIN_ERTS_VERSION`). `str` equality is not `const`-stable, so the
/// comparison is done byte-by-byte.
const fn str_matches(s: &str, expected: &[u8]) -> bool {
    let s = s.as_bytes();
    if s.len() != expected.len() {
        return false;
    }
    let mut i = 0;
    while i < s.len() {
        if s[i] != expected[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ---------------------------------------------------------------------------------------------
// NIF structs (`erl_nif.h`)
// ---------------------------------------------------------------------------------------------

// ErlNifFunc: size 32 / align 8; name 0, arity 8, fptr 16, flags 24.
const _: () = {
    assert!(core::mem::size_of::<ErlNifFunc>() == 32);
    assert!(core::mem::align_of::<ErlNifFunc>() == 8);
    assert!(core::mem::offset_of!(ErlNifFunc, name) == 0);
    assert!(core::mem::offset_of!(ErlNifFunc, arity) == 8);
    assert!(core::mem::offset_of!(ErlNifFunc, fptr) == 16);
    assert!(core::mem::offset_of!(ErlNifFunc, flags) == 24);
};

// ErlNifEntry: size 96 / align 8; major 0, minor 4, name 8, num_of_funcs 16, funcs 24, load 32,
// reload 40, upgrade 48, unload 56, vm_variant 64, options 72, sizeof_ErlNifResourceTypeInit 80,
// min_erts 88.
const _: () = {
    assert!(core::mem::size_of::<ErlNifEntry>() == 96);
    assert!(core::mem::align_of::<ErlNifEntry>() == 8);
    assert!(core::mem::offset_of!(ErlNifEntry, major) == 0);
    assert!(core::mem::offset_of!(ErlNifEntry, minor) == 4);
    assert!(core::mem::offset_of!(ErlNifEntry, name) == 8);
    assert!(core::mem::offset_of!(ErlNifEntry, num_of_funcs) == 16);
    assert!(core::mem::offset_of!(ErlNifEntry, funcs) == 24);
    assert!(core::mem::offset_of!(ErlNifEntry, load) == 32);
    assert!(core::mem::offset_of!(ErlNifEntry, reload) == 40);
    assert!(core::mem::offset_of!(ErlNifEntry, upgrade) == 48);
    assert!(core::mem::offset_of!(ErlNifEntry, unload) == 56);
    assert!(core::mem::offset_of!(ErlNifEntry, vm_variant) == 64);
    assert!(core::mem::offset_of!(ErlNifEntry, options) == 72);
    assert!(core::mem::offset_of!(ErlNifEntry, sizeof_ErlNifResourceTypeInit) == 80);
    assert!(core::mem::offset_of!(ErlNifEntry, min_erts) == 88);
};

// ErlNifBinary: size 40 / align 8; size 0, data 8, ref_bin 16, __spare__ 24.
const _: () = {
    assert!(core::mem::size_of::<ErlNifBinary>() == 40);
    assert!(core::mem::align_of::<ErlNifBinary>() == 8);
    assert!(core::mem::offset_of!(ErlNifBinary, size) == 0);
    assert!(core::mem::offset_of!(ErlNifBinary, data) == 8);
    assert!(core::mem::offset_of!(ErlNifBinary, ref_bin) == 16);
    assert!(core::mem::offset_of!(ErlNifBinary, __spare__) == 24);
};

// ErlNifPid / ErlNifPort: size 8 / align 8; pid 0 / port_id 0.
const _: () = {
    assert!(core::mem::size_of::<ErlNifPid>() == 8);
    assert!(core::mem::align_of::<ErlNifPid>() == 8);
    assert!(core::mem::offset_of!(ErlNifPid, pid) == 0);
    assert!(core::mem::size_of::<ErlNifPort>() == 8);
    assert!(core::mem::align_of::<ErlNifPort>() == 8);
    assert!(core::mem::offset_of!(ErlNifPort, port_id) == 0);
};

// ErlNifResourceTypeInit: size 40 / align 8; dtor 0, stop 8, down 16, members 24, dyncall 32.
const _: () = {
    assert!(core::mem::size_of::<ErlNifResourceTypeInit>() == 40);
    assert!(core::mem::align_of::<ErlNifResourceTypeInit>() == 8);
    assert!(core::mem::offset_of!(ErlNifResourceTypeInit, dtor) == 0);
    assert!(core::mem::offset_of!(ErlNifResourceTypeInit, stop) == 8);
    assert!(core::mem::offset_of!(ErlNifResourceTypeInit, down) == 16);
    assert!(core::mem::offset_of!(ErlNifResourceTypeInit, members) == 24);
    assert!(core::mem::offset_of!(ErlNifResourceTypeInit, dyncall) == 32);
};

// ErlNifMapIterator: size 56 / align 8; map 0, size 8, idx 16, u 24, __spare__ 40.
const _: () = {
    assert!(core::mem::size_of::<ErlNifMapIterator>() == 56);
    assert!(core::mem::align_of::<ErlNifMapIterator>() == 8);
    assert!(core::mem::offset_of!(ErlNifMapIterator, map) == 0);
    assert!(core::mem::offset_of!(ErlNifMapIterator, size) == 8);
    assert!(core::mem::offset_of!(ErlNifMapIterator, idx) == 16);
    assert!(core::mem::offset_of!(ErlNifMapIterator, u) == 24);
    assert!(core::mem::offset_of!(ErlNifMapIterator, __spare__) == 40);
};

// `ErlNifMapIterator::u` — the anonymous union interior (`erl_nif.h:250-259`).
// `flat { ERL_NIF_TERM *ks; ERL_NIF_TERM *vs; }` and `hash { struct ErtsDynamicWStack_ *wstack;
// ERL_NIF_TERM *kv; }` are each two pointers; the union itself is 16/8. The C union is anonymous,
// so it is modelled by the `ErlNifMapIteratorUnion` union of the two named arms.
const _: () = {
    assert!(core::mem::size_of::<ErlNifMapIteratorFlat>() == 16);
    assert!(core::mem::align_of::<ErlNifMapIteratorFlat>() == 8);
    assert!(core::mem::offset_of!(ErlNifMapIteratorFlat, ks) == 0);
    assert!(core::mem::offset_of!(ErlNifMapIteratorFlat, vs) == 8);
    assert!(core::mem::size_of::<ErlNifMapIteratorHash>() == 16);
    assert!(core::mem::align_of::<ErlNifMapIteratorHash>() == 8);
    assert!(core::mem::offset_of!(ErlNifMapIteratorHash, wstack) == 0);
    assert!(core::mem::offset_of!(ErlNifMapIteratorHash, kv) == 8);
    assert!(core::mem::size_of::<ErlNifMapIteratorUnion>() == 16);
    assert!(core::mem::align_of::<ErlNifMapIteratorUnion>() == 8);
};

// ErlNifIOVec: size 424 / align 8; iovcnt 0, size 8, iov 16, ref_bins 24, flags 32, small_iov 40,
// small_ref_bin 296.
const _: () = {
    assert!(core::mem::size_of::<ErlNifIOVec>() == 424);
    assert!(core::mem::align_of::<ErlNifIOVec>() == 8);
    assert!(core::mem::offset_of!(ErlNifIOVec, iovcnt) == 0);
    assert!(core::mem::offset_of!(ErlNifIOVec, size) == 8);
    assert!(core::mem::offset_of!(ErlNifIOVec, iov) == 16);
    assert!(core::mem::offset_of!(ErlNifIOVec, ref_bins) == 24);
    assert!(core::mem::offset_of!(ErlNifIOVec, flags) == 32);
    assert!(core::mem::offset_of!(ErlNifIOVec, small_iov) == 40);
    assert!(core::mem::offset_of!(ErlNifIOVec, small_ref_bin) == 296);
};

// ---------------------------------------------------------------------------------------------
// The function tables (`erl_nif_api_funcs.h`, `erl_driver.h`)
// ---------------------------------------------------------------------------------------------

// ErlNifApi: the header-exact LP64 set of 176 `enif_*` entries, one pointer each = 176 * 8 = 1408.
const _: () = {
    assert!(core::mem::size_of::<ErlNifApi>() == 176 * 8);
    assert!(core::mem::size_of::<ErlNifApi>() == 1408);
    assert!(core::mem::align_of::<ErlNifApi>() == 8);
};

// ErlDrvApi: the header's `driver_*`/`erl_drv_*` surface, one pointer each = 111 * 8 = 888.
const _: () = {
    assert!(core::mem::size_of::<ErlDrvApi>() == 111 * 8);
    assert!(core::mem::size_of::<ErlDrvApi>() == 888);
    assert!(core::mem::align_of::<ErlDrvApi>() == 8);
};

// ---------------------------------------------------------------------------------------------
// Shared (`erl_drv_nif.h`) structs
// ---------------------------------------------------------------------------------------------

// ErlDrvSysInfo (= ErlNifSysInfo): size 56 / align 8; driver_major_version 0,
// driver_minor_version 4, erts_version 8, otp_release 16, thread_support 24, smp_support 28,
// async_threads 32, scheduler_threads 36, nif_major_version 40, nif_minor_version 44,
// dirty_scheduler_support 48.
const _: () = {
    assert!(core::mem::size_of::<ErlDrvSysInfo>() == 56);
    assert!(core::mem::align_of::<ErlDrvSysInfo>() == 8);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, driver_major_version) == 0);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, driver_minor_version) == 4);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, erts_version) == 8);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, otp_release) == 16);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, thread_support) == 24);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, smp_support) == 28);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, async_threads) == 32);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, scheduler_threads) == 36);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, nif_major_version) == 40);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, nif_minor_version) == 44);
    assert!(core::mem::offset_of!(ErlDrvSysInfo, dirty_scheduler_support) == 48);
};

// ErlDrvThreadOpts: size 4 / align 4; suggested_stack_size 0.
const _: () = {
    assert!(core::mem::size_of::<ErlDrvThreadOpts>() == 4);
    assert!(core::mem::align_of::<ErlDrvThreadOpts>() == 4);
    assert!(core::mem::offset_of!(ErlDrvThreadOpts, suggested_stack_size) == 0);
};

// ErlDrvMonitor: size 32 / align 1; opaque `data` byte array at 0.
const _: () = {
    assert!(core::mem::size_of::<ErlDrvMonitor>() == 32);
    assert!(core::mem::align_of::<ErlDrvMonitor>() == 1);
    assert!(core::mem::offset_of!(ErlDrvMonitor, data) == 0);
};

// SysIOVec (= libc::iovec): size 16 / align 8; iov_base 0, iov_len 8.
const _: () = {
    assert!(core::mem::size_of::<SysIOVec>() == 16);
    assert!(core::mem::align_of::<SysIOVec>() == 8);
    assert!(core::mem::offset_of!(SysIOVec, iov_base) == 0);
    assert!(core::mem::offset_of!(SysIOVec, iov_len) == 8);
};

// ---------------------------------------------------------------------------------------------
// Driver structs (`erl_driver.h`)
// ---------------------------------------------------------------------------------------------

// ErlDrvBinary: size 16 / align 8; orig_size 0, orig_bytes 8 (flexible array member).
const _: () = {
    assert!(core::mem::size_of::<ErlDrvBinary>() == 16);
    assert!(core::mem::align_of::<ErlDrvBinary>() == 8);
    assert!(core::mem::offset_of!(ErlDrvBinary, orig_size) == 0);
    assert!(core::mem::offset_of!(ErlDrvBinary, orig_bytes) == 8);
};

// ErlIOVec: size 32 / align 8; vsize 0, size 8, iov 16, binv 24.
const _: () = {
    assert!(core::mem::size_of::<ErlIOVec>() == 32);
    assert!(core::mem::align_of::<ErlIOVec>() == 8);
    assert!(core::mem::offset_of!(ErlIOVec, vsize) == 0);
    assert!(core::mem::offset_of!(ErlIOVec, size) == 8);
    assert!(core::mem::offset_of!(ErlIOVec, iov) == 16);
    assert!(core::mem::offset_of!(ErlIOVec, binv) == 24);
};

// ErlDrvEntry: size 176 / align 8; 16 leading function pointers (init 0 … unused_event_callback
// 120), then extended_marker 128, major_version 132, minor_version 136, driver_flags 140,
// handle2 144, process_exit 152, stop_select 160, emergency_close 168.
const _: () = {
    assert!(core::mem::size_of::<ErlDrvEntry>() == 176);
    assert!(core::mem::align_of::<ErlDrvEntry>() == 8);
    assert!(core::mem::offset_of!(ErlDrvEntry, init) == 0);
    assert!(core::mem::offset_of!(ErlDrvEntry, start) == 8);
    assert!(core::mem::offset_of!(ErlDrvEntry, stop) == 16);
    assert!(core::mem::offset_of!(ErlDrvEntry, output) == 24);
    assert!(core::mem::offset_of!(ErlDrvEntry, ready_input) == 32);
    assert!(core::mem::offset_of!(ErlDrvEntry, ready_output) == 40);
    assert!(core::mem::offset_of!(ErlDrvEntry, driver_name) == 48);
    assert!(core::mem::offset_of!(ErlDrvEntry, finish) == 56);
    assert!(core::mem::offset_of!(ErlDrvEntry, handle) == 64);
    assert!(core::mem::offset_of!(ErlDrvEntry, control) == 72);
    assert!(core::mem::offset_of!(ErlDrvEntry, timeout) == 80);
    assert!(core::mem::offset_of!(ErlDrvEntry, outputv) == 88);
    assert!(core::mem::offset_of!(ErlDrvEntry, ready_async) == 96);
    assert!(core::mem::offset_of!(ErlDrvEntry, flush) == 104);
    assert!(core::mem::offset_of!(ErlDrvEntry, call) == 112);
    assert!(core::mem::offset_of!(ErlDrvEntry, unused_event_callback) == 120);
    assert!(core::mem::offset_of!(ErlDrvEntry, extended_marker) == 128);
    assert!(core::mem::offset_of!(ErlDrvEntry, major_version) == 132);
    assert!(core::mem::offset_of!(ErlDrvEntry, minor_version) == 136);
    assert!(core::mem::offset_of!(ErlDrvEntry, driver_flags) == 140);
    assert!(core::mem::offset_of!(ErlDrvEntry, handle2) == 144);
    assert!(core::mem::offset_of!(ErlDrvEntry, process_exit) == 152);
    assert!(core::mem::offset_of!(ErlDrvEntry, stop_select) == 160);
    assert!(core::mem::offset_of!(ErlDrvEntry, emergency_close) == 168);
};

// ErlDrvNowData: size 24 / align 8; megasecs 0, secs 8, microsecs 16.
const _: () = {
    assert!(core::mem::size_of::<ErlDrvNowData>() == 24);
    assert!(core::mem::align_of::<ErlDrvNowData>() == 8);
    assert!(core::mem::offset_of!(ErlDrvNowData, megasecs) == 0);
    assert!(core::mem::offset_of!(ErlDrvNowData, secs) == 8);
    assert!(core::mem::offset_of!(ErlDrvNowData, microsecs) == 16);
};

// ---------------------------------------------------------------------------------------------
// Enumerations with values — every one is a 4-byte `#[repr(C)]` enum
// ---------------------------------------------------------------------------------------------

const _: () = {
    assert!(core::mem::size_of::<ErlNifCharEncoding>() == 4);
    assert!(core::mem::size_of::<ErlNifTimeUnit>() == 4);
    assert!(core::mem::size_of::<ErlNifMapIteratorEntry>() == 4);
    assert!(core::mem::size_of::<ErlNifHash>() == 4);
    assert!(core::mem::size_of::<ErlNifTermType>() == 4);
    assert!(core::mem::size_of::<ErlNifOption>() == 4);
    assert!(core::mem::size_of::<ErlNifIOQueueOpts>() == 4);
    assert!(core::mem::size_of::<ErlDrvTimeUnit>() == 4);
};

// ---------------------------------------------------------------------------------------------
// Flag sets — bit-combinable values, modelled as 4-byte `#[repr(transparent)]` newtypes
// ---------------------------------------------------------------------------------------------

const _: () = {
    assert!(core::mem::size_of::<ErlNifResourceFlags>() == 4);
    assert!(core::mem::size_of::<ErlNifUniqueInteger>() == 4);
    assert!(core::mem::size_of::<ErlNifDirtyTaskFlags>() == 4);
    assert!(core::mem::size_of::<ErlNifBinaryToTerm>() == 4);
    assert!(core::mem::size_of::<ErlDirtyJobFlags>() == 4);
    assert!(core::mem::size_of::<ErlNifSelectFlags>() == 4);
};

// ---------------------------------------------------------------------------------------------
// Scalar typedef widths (`erl_nif.h`, `erl_driver.h`, `erl_drv_nif.h`)
//
// These are the atomic integer typedefs the two ABIs are built from. They are *not* `#[repr(C)]`
// structs, so only their width/alignment is assertable; every one is derived in the header from
// `SIZEOF_LONG == 8` / `SIZEOF_VOID_P == 8` (the LP64 branch of `erl_drv_nif.h:114-143`).
// ---------------------------------------------------------------------------------------------

const _: () = {
    // `erl_drv_nif.h:115-116,129-130` — `SIZEOF_LONG == 8`, `SIZEOF_VOID_P == 8`.
    assert!(core::mem::size_of::<ErlNapiUInt64>() == 8);
    assert!(core::mem::size_of::<ErlNapiSInt64>() == 8);
    assert!(core::mem::size_of::<ErlNapiUInt>() == 8);
    assert!(core::mem::size_of::<ErlNapiSInt>() == 8);

    // `erl_nif.h:102-105,112` — the NIF-side aliases.
    assert!(core::mem::size_of::<ErlNifUInt64>() == 8);
    assert!(core::mem::size_of::<ErlNifSInt64>() == 8);
    assert!(core::mem::size_of::<ErlNifUInt>() == 8);
    assert!(core::mem::size_of::<ErlNifSInt>() == 8);
    assert!(core::mem::size_of::<ErlNifTime>() == 8);
    assert!(core::mem::align_of::<ErlNifTime>() == 8);
    // `erl_nif.h:175` — `typedef int ErlNifEvent` on the non-Windows (Unix) branch.
    assert!(core::mem::size_of::<ErlNifEvent>() == 4);
    // `erl_nif.h:235` — `typedef int ErlNifTSDKey`.
    assert!(core::mem::size_of::<ErlNifTSDKey>() == 4);
    // `erl_nif.h:229,237` — the two shared/aliased struct typedefs.
    assert!(core::mem::size_of::<ErlNifSysInfo>() == 56);
    assert!(core::mem::size_of::<ErlNifSysInfo>() == core::mem::size_of::<ErlDrvSysInfo>());
    assert!(core::mem::size_of::<ErlNifThreadOpts>() == 4);
    assert!(core::mem::align_of::<ErlNifThreadOpts>() == 4);
    assert!(core::mem::size_of::<ErlNifThreadOpts>() == core::mem::size_of::<ErlDrvThreadOpts>());
    // `erl_nif.h:210` — `typedef ErlDrvMonitor ErlNifMonitor;`.
    assert!(core::mem::size_of::<ErlNifMonitor>() == 32);
    assert!(core::mem::size_of::<ErlNifMonitor>() == core::mem::size_of::<ErlDrvMonitor>());

    // `erl_driver.h:116-120,123-124,158` — the driver-side aliases (LP64 `size_t`/`ssize_t`).
    assert!(core::mem::size_of::<ErlDrvUInt64>() == 8);
    assert!(core::mem::size_of::<ErlDrvSInt64>() == 8);
    assert!(core::mem::size_of::<ErlDrvUInt>() == 8);
    assert!(core::mem::size_of::<ErlDrvSInt>() == 8);
    assert!(core::mem::size_of::<ErlDrvTermData>() == 8);
    assert!(core::mem::size_of::<ErlDrvSizeT>() == 8);
    assert!(core::mem::size_of::<ErlDrvSSizeT>() == 8);
    assert!(core::mem::size_of::<ErlDrvTime>() == 8);
    // `erl_driver.h:208` — `typedef int ErlDrvTSDKey`.
    assert!(core::mem::size_of::<ErlDrvTSDKey>() == 4);
};

// ---------------------------------------------------------------------------------------------
// Opaque handles (`erl_nif.h`, `erl_driver.h`)
//
// The legacy headers declare these as *incomplete* C types (`struct enif_environment_t;`,
// `struct _erl_drv_port*` …): `sizeof` does not even compile on the bare struct, and every use is
// behind a pointer. The Rust analogue is a `#[repr(C)] struct X { _private: [u8; 0] }` (size 0,
// align 1) — an ABI-inert placeholder that only ever appears behind the pointers below.
// ---------------------------------------------------------------------------------------------

const _: () = {
    // Incomplete handle structs, `erl_nif.h:123,215,302` and `erl_nif.h:256`.
    assert!(core::mem::size_of::<ErlNifEnv>() == 0);
    assert!(core::mem::align_of::<ErlNifEnv>() == 1);
    assert!(core::mem::size_of::<ErlNifResourceType>() == 0);
    assert!(core::mem::align_of::<ErlNifResourceType>() == 1);
    assert!(core::mem::size_of::<ErlNifIOQueue>() == 0);
    assert!(core::mem::align_of::<ErlNifIOQueue>() == 1);
    assert!(core::mem::size_of::<ErtsDynamicWStack>() == 0);
    assert!(core::mem::align_of::<ErtsDynamicWStack>() == 1);

    // Incomplete handle structs, `erl_driver.h:50-96,204-213`.
    assert!(core::mem::size_of::<ErlDrvTid_>() == 0);
    assert!(core::mem::align_of::<ErlDrvTid_>() == 1);
    assert!(core::mem::size_of::<ErlDrvMutex_>() == 0);
    assert!(core::mem::align_of::<ErlDrvMutex_>() == 1);
    assert!(core::mem::size_of::<ErlDrvCond_>() == 0);
    assert!(core::mem::align_of::<ErlDrvCond_>() == 1);
    assert!(core::mem::size_of::<ErlDrvRWLock_>() == 0);
    assert!(core::mem::align_of::<ErlDrvRWLock_>() == 1);
    assert!(core::mem::size_of::<ErlDrvPort_>() == 0);
    assert!(core::mem::align_of::<ErlDrvPort_>() == 1);
    assert!(core::mem::size_of::<ErlDrvEvent_>() == 0);
    assert!(core::mem::align_of::<ErlDrvEvent_>() == 1);
    assert!(core::mem::size_of::<ErlDrvData_>() == 0);
    assert!(core::mem::align_of::<ErlDrvData_>() == 1);
    assert!(core::mem::size_of::<ErlDrvPortDataLock>() == 0);
    assert!(core::mem::align_of::<ErlDrvPortDataLock>() == 1);

    // `ErlNifMutex`/`ErlNifCond`/`ErlNifRWLock` are *struct* typedefs (`erl_nif.h:232-234`): they
    // alias the `ErlDrv*` structs above (used behind a pointer), hence the same 0/1.
    assert!(core::mem::size_of::<ErlNifMutex>() == 0);
    assert!(core::mem::align_of::<ErlNifMutex>() == 1);
    assert!(core::mem::size_of::<ErlNifCond>() == 0);
    assert!(core::mem::align_of::<ErlNifCond>() == 1);
    assert!(core::mem::size_of::<ErlNifRWLock>() == 0);
    assert!(core::mem::align_of::<ErlNifRWLock>() == 1);
    // `ErlDrvMutex`/`ErlDrvCond`/`ErlDrvRWLock` are the same struct typedefs (`erl_driver.h:205-207`).
    assert!(core::mem::size_of::<ErlDrvMutex>() == 0);
    assert!(core::mem::size_of::<ErlDrvCond>() == 0);
    assert!(core::mem::size_of::<ErlDrvRWLock>() == 0);

    // `VaListTag` is the crate's `va_list` stand-in (no C type); `VaList` is a pointer to it.
    assert!(core::mem::size_of::<VaListTag>() == 0);
    assert!(core::mem::align_of::<VaListTag>() == 1);
    assert!(core::mem::size_of::<VaList>() == core::mem::size_of::<*mut core::ffi::c_void>());

    // The pointer typedefs — one machine word each, align 8 on LP64.
    assert!(core::mem::size_of::<ErlNifTid>() == 8); // `erl_nif.h:231`
    assert!(core::mem::align_of::<ErlNifTid>() == 8);
    assert!(core::mem::size_of::<ErlDrvData>() == 8); // `erl_driver.h:145`
    assert!(core::mem::size_of::<ErlDrvEvent>() == 8); // `erl_driver.h:147`
    assert!(core::mem::size_of::<ErlDrvPort>() == 8); // `erl_driver.h:149`
    assert!(core::mem::size_of::<ErlDrvThreadData>() == 8); // `erl_driver.h:150`
    assert!(core::mem::size_of::<ErlDrvTid>() == 8); // `erl_driver.h:204`
    assert!(core::mem::size_of::<ErlDrvPDL>() == 8); // `erl_driver.h:213`
};

// ---------------------------------------------------------------------------------------------
// Callback function types (`erl_nif.h:212-219`)
//
// The header declares these as `typedef void F(...)` — a function *type* — and every use is through
// an `F*`. A Rust `unsafe extern "C" fn(...)` is exactly that pointer (one code pointer, 8/8).
// ---------------------------------------------------------------------------------------------

const _: () = {
    assert!(core::mem::size_of::<ErlNifOnHaltCallback>() == 8);
    assert!(core::mem::size_of::<ErlNifOnUnloadThreadCallback>() == 8);
    assert!(core::mem::size_of::<ErlNifResourceDtor>() == 8);
    assert!(core::mem::size_of::<ErlNifResourceStop>() == 8);
    assert!(core::mem::size_of::<ErlNifResourceDown>() == 8);
    assert!(core::mem::size_of::<ErlNifResourceDynCall>() == 8);
};

// ---------------------------------------------------------------------------------------------
// Enumerations with values — the exact discriminants of every `#[repr(C)]` enum
// ---------------------------------------------------------------------------------------------

const _: () = {
    // `ErlNifTimeUnit` (`erl_nif.h:116-121`).
    assert!(ErlNifTimeUnit::Sec as c_int == 0);
    assert!(ErlNifTimeUnit::Msec as c_int == 1);
    assert!(ErlNifTimeUnit::Usec as c_int == 2);
    assert!(ErlNifTimeUnit::Nsec as c_int == 3);
    // `ErlNifCharEncoding` (`erl_nif.h:194-198`).
    assert!(ErlNifCharEncoding::Latin1 as c_int == 1);
    assert!(ErlNifCharEncoding::Utf8 as c_int == 2);
    // `ErlNifMapIteratorEntry` (`erl_nif.h:263-270`, incl. the HEAD/TAIL synonyms).
    assert!(ErlNifMapIteratorEntry::First as c_int == 1);
    assert!(ErlNifMapIteratorEntry::Last as c_int == 2);
    assert!(ErlNifMapIteratorEntry::HEAD as c_int == 1);
    assert!(ErlNifMapIteratorEntry::TAIL as c_int == 2);
    // `ErlNifHash` (`erl_nif.h:281-284`).
    assert!(ErlNifHash::InternalHash as c_int == 1);
    assert!(ErlNifHash::Phash2 as c_int == 2);
    // `ErlNifTermType` (`erl_nif.h:308-326`).
    assert!(ErlNifTermType::Atom as c_int == 1);
    assert!(ErlNifTermType::Bitstring as c_int == 2);
    assert!(ErlNifTermType::Float as c_int == 3);
    assert!(ErlNifTermType::Fun as c_int == 4);
    assert!(ErlNifTermType::Integer as c_int == 5);
    assert!(ErlNifTermType::List as c_int == 6);
    assert!(ErlNifTermType::Map as c_int == 7);
    assert!(ErlNifTermType::Pid as c_int == 8);
    assert!(ErlNifTermType::Port as c_int == 9);
    assert!(ErlNifTermType::Reference as c_int == 10);
    assert!(ErlNifTermType::Tuple as c_int == 11);
    assert!(ErlNifTermType::MissingDefaultCaseReadTheManual as c_int == -1);
    // `ErlNifOption` (`erl_nif.h:339-343`).
    assert!(ErlNifOption::DelayHalt as c_int == 1);
    assert!(ErlNifOption::OnHalt as c_int == 2);
    assert!(ErlNifOption::OnUnloadThread as c_int == 3);
    // `ErlNifIOQueueOpts` (`erl_nif.h:304-306`).
    assert!(ErlNifIOQueueOpts::Normal as c_int == 1);
    // `ErlDrvTimeUnit` (`erl_driver.h:162-167`).
    assert!(ErlDrvTimeUnit::Sec as c_int == 0);
    assert!(ErlDrvTimeUnit::Msec as c_int == 1);
    assert!(ErlDrvTimeUnit::Usec as c_int == 2);
    assert!(ErlDrvTimeUnit::Nsec as c_int == 3);
};

// ---------------------------------------------------------------------------------------------
// Flag-set values — the OR-combinable newtypes (values from the headers)
// ---------------------------------------------------------------------------------------------

const _: () = {
    // `ErlNifResourceFlags` (`erl_nif.h:188-192`).
    assert!(ErlNifResourceFlags::CREATE.bits() == 1);
    assert!(ErlNifResourceFlags::TAKEOVER.bits() == 2);
    // `ErlNifUniqueInteger` (`erl_nif.h:272-275`).
    assert!(ErlNifUniqueInteger::POSITIVE.bits() == 1 << 0);
    assert!(ErlNifUniqueInteger::MONOTONIC.bits() == 1 << 1);
    // `ErlNifDirtyTaskFlags` (`erl_nif.h:239-243`, `erl_drv_nif.h:49-52`).
    assert!(ErlNifDirtyTaskFlags::CPU_BOUND.bits() == 1);
    assert!(ErlNifDirtyTaskFlags::IO_BOUND.bits() == 2);
    // `ErlNifBinaryToTerm` (`erl_nif.h:277-279`).
    assert!(ErlNifBinaryToTerm::SAFE.bits() == 0x2000_0000);
    // `ErlDirtyJobFlags` (`erl_drv_nif.h:49-52`).
    assert!(ErlDirtyJobFlags::CPU_BOUND.bits() == ERL_DIRTY_JOB_CPU_BOUND);
    assert!(ErlDirtyJobFlags::IO_BOUND.bits() == ERL_DIRTY_JOB_IO_BOUND);
    // `ErlNifSelectFlags` (`erl_drv_nif.h:55-62`).
    assert!(ErlNifSelectFlags::READ.bits() == 1 << 0);
    assert!(ErlNifSelectFlags::WRITE.bits() == 1 << 1);
    assert!(ErlNifSelectFlags::STOP.bits() == 1 << 2);
    assert!(ErlNifSelectFlags::CANCEL.bits() == 1 << 3);
    assert!(ErlNifSelectFlags::CUSTOM_MSG.bits() == 1 << 4);
    assert!(ErlNifSelectFlags::ERROR.bits() == 1 << 5);
};

// ---------------------------------------------------------------------------------------------
// The versioned constant / macro surface (`erl_nif.h`, `erl_driver.h`, `erl_drv_nif.h`)
//
// Every value is the literal from the header (line cited per group). A drift here is an ABI break
// exactly as a struct drift is: `erlang:load_nif/2` and `erlang:load_driver/2` compare these.
// Where the header writes a helper macro (e.g. `ERL_DRV_ERROR_GENERAL` = `((ErlDrvData)
// ERL_DRV_ERROR_GENERAL_INT_)`) the crate exposes only the outer (pointer) form, whose value rustc
// refuses to cast or compare during const evaluation; those three are pinned at **runtime** in
// `tests/layout.rs` instead.
// ---------------------------------------------------------------------------------------------

const _: () = {
    // Interface versions (`erl_nif.h:67-68,79,91,107`; `erl_driver.h:70-72,88`).
    assert!(ERL_NIF_MAJOR_VERSION == 2);
    assert!(ERL_NIF_MINOR_VERSION == 18);
    assert!(ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD == 2);
    assert!(str_matches(ERL_NIF_VM_VARIANT, b"beam.vanilla"));
    assert!(str_matches(ERL_NIF_MIN_ERTS_VERSION, b"erts-14.0"));
    assert!(ERL_DRV_EXTENDED_MARKER == 0xfeee_eeed_u32 as c_int);
    assert!(ERL_DRV_EXTENDED_MAJOR_VERSION == 3);
    assert!(ERL_DRV_EXTENDED_MINOR_VERSION == 3);
    assert!(ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD == 2);

    // `ERTS_NAPI_*` time constants (`erl_drv_nif.h:145-150`).
    assert!(ERTS_NAPI_SEC == 0);
    assert!(ERTS_NAPI_MSEC == 1);
    assert!(ERTS_NAPI_USEC == 2);
    assert!(ERTS_NAPI_NSEC == 3);
    assert!(ERTS_NAPI_TIME_ERROR == i64::MIN);
    assert!(ERL_NIF_TIME_ERROR == i64::MIN); // `erl_nif.h:114`
    assert!(ERL_DRV_TIME_ERROR == i64::MIN); // `erl_driver.h:160`

    // `ERL_NIF_SELECT_*` mode bits (`erl_drv_nif.h:55-62`).
    assert!(ERL_NIF_SELECT_READ == 1 << 0);
    assert!(ERL_NIF_SELECT_WRITE == 1 << 1);
    assert!(ERL_NIF_SELECT_STOP == 1 << 2);
    assert!(ERL_NIF_SELECT_CANCEL == 1 << 3);
    assert!(ERL_NIF_SELECT_CUSTOM_MSG == 1 << 4);
    assert!(ERL_NIF_SELECT_ERROR == 1 << 5);

    // `ERL_NIF_SELECT_*` return bits from `enif_select` (`erl_nif.h:179-186`).
    assert!(ERL_NIF_SELECT_STOP_CALLED == 1 << 0);
    assert!(ERL_NIF_SELECT_STOP_SCHEDULED == 1 << 1);
    assert!(ERL_NIF_SELECT_INVALID_EVENT == 1 << 2);
    assert!(ERL_NIF_SELECT_FAILED == 1 << 3);
    assert!(ERL_NIF_SELECT_READ_CANCELLED == 1 << 4);
    assert!(ERL_NIF_SELECT_WRITE_CANCELLED == 1 << 5);
    assert!(ERL_NIF_SELECT_ERROR_CANCELLED == 1 << 6);
    assert!(ERL_NIF_SELECT_NOTSUP == 1 << 7);

    // `ERL_NIF_THR_*` thread types (`erl_nif.h:334-337`).
    assert!(ERL_NIF_THR_UNDEFINED == 0);
    assert!(ERL_NIF_THR_NORMAL_SCHEDULER == 1);
    assert!(ERL_NIF_THR_DIRTY_CPU_SCHEDULER == 2);
    assert!(ERL_NIF_THR_DIRTY_IO_SCHEDULER == 3);

    // `ERL_NIF_IOVEC_SIZE` (`erl_nif.h:286`).
    assert!(ERL_NIF_IOVEC_SIZE == 16);
};

const _: () = {
    // `ERL_DRV_READ`/`WRITE`/`USE`/`USE_NO_CALLBACK` (`erl_driver.h:61-64`).
    assert!(ERL_DRV_READ == 1);
    assert!(ERL_DRV_WRITE == 2);
    assert!(ERL_DRV_USE == 4);
    assert!(ERL_DRV_USE_NO_CALLBACK == 12);

    // `PORT_CONTROL_FLAG_*` (`erl_driver.h:98-99`) and `PORT_FLAG_*` (`erl_driver.h:103-104`).
    assert!(PORT_CONTROL_FLAG_BINARY == 1 << 0);
    assert!(PORT_CONTROL_FLAG_HEAVY == 1 << 1);
    assert!(PORT_FLAG_BINARY == 1 << 0);
    assert!(PORT_FLAG_LINE == 1 << 1);

    // `ERL_DRV_FLAG_*` (`erl_driver.h:107-110`).
    assert!(ERL_DRV_FLAG_USE_PORT_LOCKING == 1 << 0);
    assert!(ERL_DRV_FLAG_SOFT_BUSY == 1 << 1);
    assert!(ERL_DRV_FLAG_NO_BUSY_MSGQ == 1 << 2);
    assert!(ERL_DRV_FLAG_USE_INIT_ACK == 1 << 3);

    // `DRIVER_CALL_KEEP_BUFFER` (`erl_driver.h:515`).
    assert!(DRIVER_CALL_KEEP_BUFFER == 0x1);

    // `ERL_DRV_ERROR_*` start-failure codes (`erl_driver.h:176-191`) are `ErlDrvData` (`*mut`)
    // values built from the `ERL_DRV_ERROR_*_INT_` helper macros (`-1`/`-2`/`-3`). rustc forbids
    // casting or comparing raw pointers during const evaluation (rust-lang/rust#53020), so they —
    // like `ERTS_INVALID_ERL_DRV_PORT` (`erl_port.h:67`) — are pinned at **runtime** in
    // `tests/layout.rs` instead. The `*_INT_` helper values are exactly those integer literals.

    // `ERL_DRV_BUSY_MSGQ_*` (`erl_driver.h:323-326`).
    assert!(ERL_DRV_BUSY_MSGQ_DISABLED == usize::MAX);
    assert!(ERL_DRV_BUSY_MSGQ_READ_ONLY == 0);
    assert!(ERL_DRV_BUSY_MSGQ_LIM_MAX == usize::MAX - 1);
    assert!(ERL_DRV_BUSY_MSGQ_LIM_MIN == 1);

    // Term-data tags for `erl_drv_output_term`/`erl_drv_send_term` (`erl_driver.h:535-555`).
    assert!(ERL_DRV_NIL == 1);
    assert!(ERL_DRV_ATOM == 2);
    assert!(ERL_DRV_INT == 3);
    assert!(ERL_DRV_PORT == 4);
    assert!(ERL_DRV_BINARY == 5);
    assert!(ERL_DRV_STRING == 6);
    assert!(ERL_DRV_TUPLE == 7);
    assert!(ERL_DRV_LIST == 8);
    assert!(ERL_DRV_STRING_CONS == 9);
    assert!(ERL_DRV_PID == 10);
    assert!(ERL_DRV_FLOAT == 11);
    assert!(ERL_DRV_EXT2TERM == 12);
    assert!(ERL_DRV_UINT == 13);
    assert!(ERL_DRV_BUF2BINARY == 14);
    assert!(ERL_DRV_INT64 == 15);
    assert!(ERL_DRV_UINT64 == 16);
    assert!(ERL_DRV_MAP == 17);
};

// ---------------------------------------------------------------------------------------------
// The term seam — `ERL_NIF_TERM`, the internal `Eterm`, and the raw machine word are one type
// (compatibility contract A.3: the internal term *is* the NIF term)
// ---------------------------------------------------------------------------------------------

const _: () = {
    assert!(core::mem::size_of::<ERL_NIF_TERM>() == 8);
    assert!(core::mem::size_of::<ERL_NIF_UINT>() == 8); // `erl_nif.h:110` — `ERL_NIF_UINT` alias
    assert!(core::mem::size_of::<Eterm>() == 8);
    assert!(core::mem::size_of::<usize>() == 8);
    assert!(core::mem::align_of::<ERL_NIF_TERM>() == core::mem::align_of::<usize>());
    assert!(core::mem::align_of::<ERL_NIF_TERM>() == core::mem::align_of::<Eterm>());
};
