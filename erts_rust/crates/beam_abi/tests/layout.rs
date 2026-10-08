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

//! Runtime re-assertion of the frozen LP64 layout table.
//!
//! This mirrors the compile-time freeze in `beam_abi::layout` (the authoritative check) at
//! **runtime**, so a mismatch also fails the test suite with a readable, per-field message. Every
//! size, `align_of` and field `offset_of` of the frozen table is re-checked with `assert_eq!`/
//! `assert!`.
//!
//! The fix for any failure is to correct the Rust declaration to match the legacy header
//! (`erts/emulator/beam/erl_nif.h`, `erl_nif_api_funcs.h`, `erl_drv_nif.h`, `erl_driver.h`) — never
//! to weaken this test.

use core::mem::{align_of, offset_of, size_of};

use beam_abi::common::{
    ErlDirtyJobFlags, ErlDrvMonitor, ErlDrvSysInfo, ErlDrvThreadOpts, ErlNapiSInt, ErlNapiSInt64,
    ErlNapiUInt, ErlNapiUInt64, ErlNifSelectFlags, SysIOVec, ERL_DIRTY_JOB_CPU_BOUND,
    ERL_DIRTY_JOB_IO_BOUND, ERL_NIF_SELECT_CANCEL, ERL_NIF_SELECT_CUSTOM_MSG, ERL_NIF_SELECT_ERROR,
    ERL_NIF_SELECT_ERROR_CANCELLED, ERL_NIF_SELECT_FAILED, ERL_NIF_SELECT_INVALID_EVENT,
    ERL_NIF_SELECT_NOTSUP, ERL_NIF_SELECT_READ, ERL_NIF_SELECT_READ_CANCELLED, ERL_NIF_SELECT_STOP,
    ERL_NIF_SELECT_STOP_CALLED, ERL_NIF_SELECT_STOP_SCHEDULED, ERL_NIF_SELECT_WRITE,
    ERL_NIF_SELECT_WRITE_CANCELLED, ERTS_NAPI_MSEC, ERTS_NAPI_NSEC, ERTS_NAPI_SEC,
    ERTS_NAPI_TIME_ERROR, ERTS_NAPI_USEC,
};
use beam_abi::driver::api::ErlDrvApi;
use beam_abi::driver::types::{
    ErlDrvBinary, ErlDrvCond, ErlDrvCond_, ErlDrvData, ErlDrvData_, ErlDrvEntry, ErlDrvEvent,
    ErlDrvEvent_, ErlDrvMutex, ErlDrvMutex_, ErlDrvNowData, ErlDrvPDL, ErlDrvPort, ErlDrvPort_,
    ErlDrvPortDataLock, ErlDrvRWLock, ErlDrvRWLock_, ErlDrvSSizeT, ErlDrvSInt, ErlDrvSInt64,
    ErlDrvSizeT, ErlDrvTSDKey, ErlDrvTermData, ErlDrvThreadData, ErlDrvTid, ErlDrvTid_, ErlDrvTime,
    ErlDrvTimeUnit, ErlDrvUInt, ErlDrvUInt64, ErlIOVec, DRIVER_CALL_KEEP_BUFFER,
    ERL_DRV_ATOM, ERL_DRV_BINARY, ERL_DRV_BUF2BINARY, ERL_DRV_BUSY_MSGQ_DISABLED,
    ERL_DRV_BUSY_MSGQ_LIM_MAX, ERL_DRV_BUSY_MSGQ_LIM_MIN, ERL_DRV_BUSY_MSGQ_READ_ONLY,
    ERL_DRV_ERROR_BADARG, ERL_DRV_ERROR_ERRNO, ERL_DRV_ERROR_GENERAL, ERL_DRV_EXT2TERM,
    ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MARKER, ERL_DRV_EXTENDED_MINOR_VERSION,
    ERL_DRV_FLAG_NO_BUSY_MSGQ, ERL_DRV_FLAG_SOFT_BUSY, ERL_DRV_FLAG_USE_INIT_ACK,
    ERL_DRV_FLAG_USE_PORT_LOCKING, ERL_DRV_FLOAT, ERL_DRV_INT, ERL_DRV_INT64, ERL_DRV_LIST,
    ERL_DRV_MAP, ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD, ERL_DRV_NIL, ERL_DRV_PID, ERL_DRV_PORT,
    ERL_DRV_READ, ERL_DRV_STRING, ERL_DRV_STRING_CONS, ERL_DRV_TIME_ERROR, ERL_DRV_TUPLE,
    ERL_DRV_UINT, ERL_DRV_UINT64, ERL_DRV_USE, ERL_DRV_USE_NO_CALLBACK, ERL_DRV_WRITE,
    ERTS_INVALID_ERL_DRV_PORT, PORT_CONTROL_FLAG_BINARY, PORT_CONTROL_FLAG_HEAVY, PORT_FLAG_BINARY,
    PORT_FLAG_LINE,
};
use beam_abi::nif::api::ErlNifApi;
use beam_abi::nif::term::{ERL_NIF_TERM, ERL_NIF_UINT};
use beam_abi::nif::types::{
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
use beam_abi::{
    ERL_NIF_MAJOR_VERSION, ERL_NIF_MIN_ERTS_VERSION, ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD,
    ERL_NIF_MINOR_VERSION, ERL_NIF_VM_VARIANT,
};
use beam_term::Eterm;

#[test]
fn nif_structs_match_the_frozen_layout() {
    assert_eq!(size_of::<ErlNifFunc>(), 32, "ErlNifFunc size");
    assert_eq!(align_of::<ErlNifFunc>(), 8, "ErlNifFunc align");
    assert_eq!(offset_of!(ErlNifFunc, name), 0);
    assert_eq!(offset_of!(ErlNifFunc, arity), 8);
    assert_eq!(offset_of!(ErlNifFunc, fptr), 16);
    assert_eq!(offset_of!(ErlNifFunc, flags), 24);

    assert_eq!(size_of::<ErlNifEntry>(), 96, "ErlNifEntry size");
    assert_eq!(align_of::<ErlNifEntry>(), 8, "ErlNifEntry align");
    assert_eq!(offset_of!(ErlNifEntry, major), 0);
    assert_eq!(offset_of!(ErlNifEntry, minor), 4);
    assert_eq!(offset_of!(ErlNifEntry, name), 8);
    assert_eq!(offset_of!(ErlNifEntry, num_of_funcs), 16);
    assert_eq!(offset_of!(ErlNifEntry, funcs), 24);
    assert_eq!(offset_of!(ErlNifEntry, load), 32);
    assert_eq!(offset_of!(ErlNifEntry, reload), 40);
    assert_eq!(offset_of!(ErlNifEntry, upgrade), 48);
    assert_eq!(offset_of!(ErlNifEntry, unload), 56);
    assert_eq!(offset_of!(ErlNifEntry, vm_variant), 64);
    assert_eq!(offset_of!(ErlNifEntry, options), 72);
    assert_eq!(offset_of!(ErlNifEntry, sizeof_ErlNifResourceTypeInit), 80);
    assert_eq!(offset_of!(ErlNifEntry, min_erts), 88);

    assert_eq!(size_of::<ErlNifBinary>(), 40, "ErlNifBinary size");
    assert_eq!(align_of::<ErlNifBinary>(), 8, "ErlNifBinary align");
    assert_eq!(offset_of!(ErlNifBinary, size), 0);
    assert_eq!(offset_of!(ErlNifBinary, data), 8);
    assert_eq!(offset_of!(ErlNifBinary, ref_bin), 16);
    assert_eq!(offset_of!(ErlNifBinary, __spare__), 24);

    assert_eq!(size_of::<ErlNifPid>(), 8, "ErlNifPid size");
    assert_eq!(align_of::<ErlNifPid>(), 8, "ErlNifPid align");
    assert_eq!(offset_of!(ErlNifPid, pid), 0);
    assert_eq!(size_of::<ErlNifPort>(), 8, "ErlNifPort size");
    assert_eq!(align_of::<ErlNifPort>(), 8, "ErlNifPort align");
    assert_eq!(offset_of!(ErlNifPort, port_id), 0);

    assert_eq!(size_of::<ErlNifResourceTypeInit>(), 40, "ErlNifResourceTypeInit size");
    assert_eq!(align_of::<ErlNifResourceTypeInit>(), 8, "ErlNifResourceTypeInit align");
    assert_eq!(offset_of!(ErlNifResourceTypeInit, dtor), 0);
    assert_eq!(offset_of!(ErlNifResourceTypeInit, stop), 8);
    assert_eq!(offset_of!(ErlNifResourceTypeInit, down), 16);
    assert_eq!(offset_of!(ErlNifResourceTypeInit, members), 24);
    assert_eq!(offset_of!(ErlNifResourceTypeInit, dyncall), 32);

    assert_eq!(size_of::<ErlNifMapIterator>(), 56, "ErlNifMapIterator size");
    assert_eq!(align_of::<ErlNifMapIterator>(), 8, "ErlNifMapIterator align");
    assert_eq!(offset_of!(ErlNifMapIterator, map), 0);
    assert_eq!(offset_of!(ErlNifMapIterator, size), 8);
    assert_eq!(offset_of!(ErlNifMapIterator, idx), 16);
    assert_eq!(offset_of!(ErlNifMapIterator, u), 24);
    assert_eq!(offset_of!(ErlNifMapIterator, __spare__), 40);

    assert_eq!(size_of::<ErlNifIOVec>(), 424, "ErlNifIOVec size");
    assert_eq!(align_of::<ErlNifIOVec>(), 8, "ErlNifIOVec align");
    assert_eq!(offset_of!(ErlNifIOVec, iovcnt), 0);
    assert_eq!(offset_of!(ErlNifIOVec, size), 8);
    assert_eq!(offset_of!(ErlNifIOVec, iov), 16);
    assert_eq!(offset_of!(ErlNifIOVec, ref_bins), 24);
    assert_eq!(offset_of!(ErlNifIOVec, flags), 32);
    assert_eq!(offset_of!(ErlNifIOVec, small_iov), 40);
    assert_eq!(offset_of!(ErlNifIOVec, small_ref_bin), 296);
}

#[test]
fn function_tables_are_one_pointer_per_header_entry() {
    // The header-exact LP64 NIF table: 176 `enif_*` entries, one pointer each.
    assert_eq!(size_of::<ErlNifApi>(), 176 * 8, "ErlNifApi is 176 pointers");
    assert_eq!(size_of::<ErlNifApi>(), 1408, "ErlNifApi size");
    assert_eq!(align_of::<ErlNifApi>(), 8, "ErlNifApi align");

    // The driver table: 111 `driver_*`/`erl_drv_*` entries, one pointer each.
    assert_eq!(size_of::<ErlDrvApi>(), 111 * 8, "ErlDrvApi is 111 pointers");
    assert_eq!(size_of::<ErlDrvApi>(), 888, "ErlDrvApi size");
    assert_eq!(align_of::<ErlDrvApi>(), 8, "ErlDrvApi align");
}

#[test]
fn shared_structs_match_the_frozen_layout() {
    assert_eq!(size_of::<ErlDrvSysInfo>(), 56, "ErlDrvSysInfo size");
    assert_eq!(align_of::<ErlDrvSysInfo>(), 8, "ErlDrvSysInfo align");
    assert_eq!(offset_of!(ErlDrvSysInfo, driver_major_version), 0);
    assert_eq!(offset_of!(ErlDrvSysInfo, driver_minor_version), 4);
    assert_eq!(offset_of!(ErlDrvSysInfo, erts_version), 8);
    assert_eq!(offset_of!(ErlDrvSysInfo, otp_release), 16);
    assert_eq!(offset_of!(ErlDrvSysInfo, thread_support), 24);
    assert_eq!(offset_of!(ErlDrvSysInfo, smp_support), 28);
    assert_eq!(offset_of!(ErlDrvSysInfo, async_threads), 32);
    assert_eq!(offset_of!(ErlDrvSysInfo, scheduler_threads), 36);
    assert_eq!(offset_of!(ErlDrvSysInfo, nif_major_version), 40);
    assert_eq!(offset_of!(ErlDrvSysInfo, nif_minor_version), 44);
    assert_eq!(offset_of!(ErlDrvSysInfo, dirty_scheduler_support), 48);

    assert_eq!(size_of::<ErlDrvThreadOpts>(), 4, "ErlDrvThreadOpts size");
    assert_eq!(align_of::<ErlDrvThreadOpts>(), 4, "ErlDrvThreadOpts align");
    assert_eq!(offset_of!(ErlDrvThreadOpts, suggested_stack_size), 0);

    assert_eq!(size_of::<ErlDrvMonitor>(), 32, "ErlDrvMonitor size");
    assert_eq!(align_of::<ErlDrvMonitor>(), 1, "ErlDrvMonitor align");
    assert_eq!(offset_of!(ErlDrvMonitor, data), 0);

    assert_eq!(size_of::<SysIOVec>(), 16, "SysIOVec size");
    assert_eq!(align_of::<SysIOVec>(), 8, "SysIOVec align");
    assert_eq!(offset_of!(SysIOVec, iov_base), 0);
    assert_eq!(offset_of!(SysIOVec, iov_len), 8);
}

#[test]
fn driver_structs_match_the_frozen_layout() {
    assert_eq!(size_of::<ErlDrvBinary>(), 16, "ErlDrvBinary size");
    assert_eq!(align_of::<ErlDrvBinary>(), 8, "ErlDrvBinary align");
    assert_eq!(offset_of!(ErlDrvBinary, orig_size), 0);
    assert_eq!(offset_of!(ErlDrvBinary, orig_bytes), 8);

    assert_eq!(size_of::<ErlIOVec>(), 32, "ErlIOVec size");
    assert_eq!(align_of::<ErlIOVec>(), 8, "ErlIOVec align");
    assert_eq!(offset_of!(ErlIOVec, vsize), 0);
    assert_eq!(offset_of!(ErlIOVec, size), 8);
    assert_eq!(offset_of!(ErlIOVec, iov), 16);
    assert_eq!(offset_of!(ErlIOVec, binv), 24);

    assert_eq!(size_of::<ErlDrvEntry>(), 176, "ErlDrvEntry size");
    assert_eq!(align_of::<ErlDrvEntry>(), 8, "ErlDrvEntry align");
    assert_eq!(offset_of!(ErlDrvEntry, init), 0);
    assert_eq!(offset_of!(ErlDrvEntry, start), 8);
    assert_eq!(offset_of!(ErlDrvEntry, stop), 16);
    assert_eq!(offset_of!(ErlDrvEntry, output), 24);
    assert_eq!(offset_of!(ErlDrvEntry, ready_input), 32);
    assert_eq!(offset_of!(ErlDrvEntry, ready_output), 40);
    assert_eq!(offset_of!(ErlDrvEntry, driver_name), 48);
    assert_eq!(offset_of!(ErlDrvEntry, finish), 56);
    assert_eq!(offset_of!(ErlDrvEntry, handle), 64);
    assert_eq!(offset_of!(ErlDrvEntry, control), 72);
    assert_eq!(offset_of!(ErlDrvEntry, timeout), 80);
    assert_eq!(offset_of!(ErlDrvEntry, outputv), 88);
    assert_eq!(offset_of!(ErlDrvEntry, ready_async), 96);
    assert_eq!(offset_of!(ErlDrvEntry, flush), 104);
    assert_eq!(offset_of!(ErlDrvEntry, call), 112);
    assert_eq!(offset_of!(ErlDrvEntry, unused_event_callback), 120);
    assert_eq!(offset_of!(ErlDrvEntry, extended_marker), 128);
    assert_eq!(offset_of!(ErlDrvEntry, major_version), 132);
    assert_eq!(offset_of!(ErlDrvEntry, minor_version), 136);
    assert_eq!(offset_of!(ErlDrvEntry, driver_flags), 140);
    assert_eq!(offset_of!(ErlDrvEntry, handle2), 144);
    assert_eq!(offset_of!(ErlDrvEntry, process_exit), 152);
    assert_eq!(offset_of!(ErlDrvEntry, stop_select), 160);
    assert_eq!(offset_of!(ErlDrvEntry, emergency_close), 168);

    assert_eq!(size_of::<ErlDrvNowData>(), 24, "ErlDrvNowData size");
    assert_eq!(align_of::<ErlDrvNowData>(), 8, "ErlDrvNowData align");
    assert_eq!(offset_of!(ErlDrvNowData, megasecs), 0);
    assert_eq!(offset_of!(ErlDrvNowData, secs), 8);
    assert_eq!(offset_of!(ErlDrvNowData, microsecs), 16);
}

#[test]
fn enums_and_flags_are_four_bytes() {
    // Pure `#[repr(C)]` enums with values.
    assert_eq!(size_of::<ErlNifCharEncoding>(), 4);
    assert_eq!(size_of::<ErlNifTimeUnit>(), 4);
    assert_eq!(size_of::<ErlNifMapIteratorEntry>(), 4);
    assert_eq!(size_of::<ErlNifHash>(), 4);
    assert_eq!(size_of::<ErlNifTermType>(), 4);
    assert_eq!(size_of::<ErlNifOption>(), 4);
    assert_eq!(size_of::<ErlNifIOQueueOpts>(), 4);
    assert_eq!(size_of::<ErlDrvTimeUnit>(), 4);

    // `#[repr(transparent)]` flag newtypes.
    assert_eq!(size_of::<ErlNifResourceFlags>(), 4);
    assert_eq!(size_of::<ErlNifUniqueInteger>(), 4);
    assert_eq!(size_of::<ErlNifDirtyTaskFlags>(), 4);
    assert_eq!(size_of::<ErlNifBinaryToTerm>(), 4);
    assert_eq!(size_of::<ErlDirtyJobFlags>(), 4);
    assert_eq!(size_of::<ErlNifSelectFlags>(), 4);
}

#[test]
fn the_term_seam_is_one_machine_word() {
    assert_eq!(size_of::<ERL_NIF_TERM>(), 8);
    assert_eq!(size_of::<ERL_NIF_UINT>(), 8, "ERL_NIF_UINT aliases ERL_NIF_TERM");
    assert_eq!(size_of::<Eterm>(), 8);
    assert_eq!(size_of::<usize>(), 8);
    assert_eq!(align_of::<ERL_NIF_TERM>(), align_of::<usize>());
    assert_eq!(align_of::<ERL_NIF_TERM>(), align_of::<Eterm>());
}

#[test]
fn callback_function_types_are_one_code_pointer() {
    // `erl_nif.h:212-219` — `typedef void F(...)`; each is modelled as a Rust fn pointer (8 bytes).
    assert_eq!(size_of::<ErlNifOnHaltCallback>(), 8);
    assert_eq!(size_of::<ErlNifOnUnloadThreadCallback>(), 8);
    assert_eq!(size_of::<ErlNifResourceDtor>(), 8);
    assert_eq!(size_of::<ErlNifResourceStop>(), 8);
    assert_eq!(size_of::<ErlNifResourceDown>(), 8);
    assert_eq!(size_of::<ErlNifResourceDynCall>(), 8);
}

#[test]
fn map_iterator_union_interior_matches_the_frozen_layout() {
    // `erl_nif.h:250-259` — `flat { ERL_NIF_TERM *ks; ERL_NIF_TERM *vs; }`,
    // `hash { struct ErtsDynamicWStack_ *wstack; ERL_NIF_TERM *kv; }` and the anonymous `u`.
    assert_eq!(size_of::<ErlNifMapIteratorFlat>(), 16, "flat arm size");
    assert_eq!(align_of::<ErlNifMapIteratorFlat>(), 8, "flat arm align");
    assert_eq!(offset_of!(ErlNifMapIteratorFlat, ks), 0);
    assert_eq!(offset_of!(ErlNifMapIteratorFlat, vs), 8);
    assert_eq!(size_of::<ErlNifMapIteratorHash>(), 16, "hash arm size");
    assert_eq!(align_of::<ErlNifMapIteratorHash>(), 8, "hash arm align");
    assert_eq!(offset_of!(ErlNifMapIteratorHash, wstack), 0);
    assert_eq!(offset_of!(ErlNifMapIteratorHash, kv), 8);
    assert_eq!(size_of::<ErlNifMapIteratorUnion>(), 16, "union size");
    assert_eq!(align_of::<ErlNifMapIteratorUnion>(), 8, "union align");
}

#[test]
fn scalar_typedef_widths_match_the_lp64_header() {
    // `erl_drv_nif.h:115-116,129-130`.
    assert_eq!(size_of::<ErlNapiUInt64>(), 8);
    assert_eq!(size_of::<ErlNapiSInt64>(), 8);
    assert_eq!(size_of::<ErlNapiUInt>(), 8);
    assert_eq!(size_of::<ErlNapiSInt>(), 8);

    // `erl_nif.h:102-105,112` — the NIF-side aliases.
    assert_eq!(size_of::<ErlNifUInt64>(), 8);
    assert_eq!(size_of::<ErlNifSInt64>(), 8);
    assert_eq!(size_of::<ErlNifUInt>(), 8);
    assert_eq!(size_of::<ErlNifSInt>(), 8, "ErlNifSInt is 64-bit on LP64");
    assert_eq!(size_of::<ErlNifTime>(), 8);
    assert_eq!(align_of::<ErlNifTime>(), 8);
    // `erl_nif.h:175` — `typedef int ErlNifEvent` on the Unix branch.
    assert_eq!(size_of::<ErlNifEvent>(), 4, "ErlNifEvent is `int` on Unix");
    // `erl_nif.h:235` — `typedef int ErlNifTSDKey`.
    assert_eq!(size_of::<ErlNifTSDKey>(), 4);
    // `erl_nif.h:210,229,237` — the aliased shared structs.
    assert_eq!(size_of::<ErlNifSysInfo>(), 56);
    assert_eq!(size_of::<ErlNifSysInfo>(), size_of::<ErlDrvSysInfo>());
    assert_eq!(size_of::<ErlNifThreadOpts>(), 4);
    assert_eq!(align_of::<ErlNifThreadOpts>(), 4);
    assert_eq!(size_of::<ErlNifThreadOpts>(), size_of::<ErlDrvThreadOpts>());
    assert_eq!(size_of::<ErlNifMonitor>(), 32);
    assert_eq!(size_of::<ErlNifMonitor>(), size_of::<ErlDrvMonitor>());

    // `erl_driver.h:116-120,123-124,158,208`.
    assert_eq!(size_of::<ErlDrvUInt64>(), 8);
    assert_eq!(size_of::<ErlDrvSInt64>(), 8);
    assert_eq!(size_of::<ErlDrvUInt>(), 8);
    assert_eq!(size_of::<ErlDrvSInt>(), 8, "ErlDrvSInt is 64-bit on LP64");
    assert_eq!(size_of::<ErlDrvTermData>(), 8);
    assert_eq!(size_of::<ErlDrvSizeT>(), 8);
    assert_eq!(size_of::<ErlDrvSSizeT>(), 8);
    assert_eq!(size_of::<ErlDrvTime>(), 8);
    assert_eq!(size_of::<ErlDrvTSDKey>(), 4);
}

#[test]
fn opaque_handles_are_abi_inert() {
    // Incomplete handle structs — `erl_nif.h:123,215,256,302` and `erl_driver.h:50-96,204-213` —
    // modelled as zero-sized `#[repr(C)]` placeholders (always used behind a pointer).
    assert_eq!((size_of::<ErlNifEnv>(), align_of::<ErlNifEnv>()), (0, 1));
    assert_eq!((size_of::<ErlNifResourceType>(), align_of::<ErlNifResourceType>()), (0, 1));
    assert_eq!((size_of::<ErlNifIOQueue>(), align_of::<ErlNifIOQueue>()), (0, 1));
    assert_eq!((size_of::<ErtsDynamicWStack>(), align_of::<ErtsDynamicWStack>()), (0, 1));
    assert_eq!((size_of::<ErlDrvTid_>(), align_of::<ErlDrvTid_>()), (0, 1));
    assert_eq!((size_of::<ErlDrvMutex_>(), align_of::<ErlDrvMutex_>()), (0, 1));
    assert_eq!((size_of::<ErlDrvCond_>(), align_of::<ErlDrvCond_>()), (0, 1));
    assert_eq!((size_of::<ErlDrvRWLock_>(), align_of::<ErlDrvRWLock_>()), (0, 1));
    assert_eq!((size_of::<ErlDrvPort_>(), align_of::<ErlDrvPort_>()), (0, 1));
    assert_eq!((size_of::<ErlDrvEvent_>(), align_of::<ErlDrvEvent_>()), (0, 1));
    assert_eq!((size_of::<ErlDrvData_>(), align_of::<ErlDrvData_>()), (0, 1));
    assert_eq!((size_of::<ErlDrvPortDataLock>(), align_of::<ErlDrvPortDataLock>()), (0, 1));
    // The NIF-side struct typedefs alias the driver ones (`erl_nif.h:232-234`).
    assert_eq!((size_of::<ErlNifMutex>(), align_of::<ErlNifMutex>()), (0, 1));
    assert_eq!((size_of::<ErlNifCond>(), align_of::<ErlNifCond>()), (0, 1));
    assert_eq!((size_of::<ErlNifRWLock>(), align_of::<ErlNifRWLock>()), (0, 1));
    assert_eq!(size_of::<ErlDrvMutex>(), 0);
    assert_eq!(size_of::<ErlDrvCond>(), 0);
    assert_eq!(size_of::<ErlDrvRWLock>(), 0);
    // The crate's `va_list` stand-in.
    assert_eq!((size_of::<VaListTag>(), align_of::<VaListTag>()), (0, 1));
    assert_eq!(size_of::<VaList>(), size_of::<*mut core::ffi::c_void>());

    // The pointer typedefs — one machine word (`erl_nif.h:231`, `erl_driver.h:145-150,204,213`).
    assert_eq!(size_of::<ErlNifTid>(), 8);
    assert_eq!(size_of::<ErlDrvData>(), 8);
    assert_eq!(size_of::<ErlDrvEvent>(), 8);
    assert_eq!(size_of::<ErlDrvPort>(), 8);
    assert_eq!(size_of::<ErlDrvThreadData>(), 8);
    assert_eq!(size_of::<ErlDrvTid>(), 8);
    assert_eq!(size_of::<ErlDrvPDL>(), 8);
}

#[test]
fn enum_discriminants_match_the_header() {
    // `erl_nif.h:116-121,194-198,263-270,281-284,304-306,308-326,339-343`.
    assert_eq!(ErlNifTimeUnit::Sec as i32, 0);
    assert_eq!(ErlNifTimeUnit::Msec as i32, 1);
    assert_eq!(ErlNifTimeUnit::Usec as i32, 2);
    assert_eq!(ErlNifTimeUnit::Nsec as i32, 3);
    assert_eq!(ErlNifCharEncoding::Latin1 as i32, 1);
    assert_eq!(ErlNifCharEncoding::Utf8 as i32, 2);
    assert_eq!(ErlNifMapIteratorEntry::First as i32, 1);
    assert_eq!(ErlNifMapIteratorEntry::Last as i32, 2);
    assert_eq!(ErlNifMapIteratorEntry::HEAD as i32, 1);
    assert_eq!(ErlNifMapIteratorEntry::TAIL as i32, 2);
    assert_eq!(ErlNifHash::InternalHash as i32, 1);
    assert_eq!(ErlNifHash::Phash2 as i32, 2);
    assert_eq!(ErlNifTermType::Atom as i32, 1);
    assert_eq!(ErlNifTermType::Bitstring as i32, 2);
    assert_eq!(ErlNifTermType::Float as i32, 3);
    assert_eq!(ErlNifTermType::Fun as i32, 4);
    assert_eq!(ErlNifTermType::Integer as i32, 5);
    assert_eq!(ErlNifTermType::List as i32, 6);
    assert_eq!(ErlNifTermType::Map as i32, 7);
    assert_eq!(ErlNifTermType::Pid as i32, 8);
    assert_eq!(ErlNifTermType::Port as i32, 9);
    assert_eq!(ErlNifTermType::Reference as i32, 10);
    assert_eq!(ErlNifTermType::Tuple as i32, 11);
    assert_eq!(ErlNifTermType::MissingDefaultCaseReadTheManual as i32, -1);
    assert_eq!(ErlNifOption::DelayHalt as i32, 1);
    assert_eq!(ErlNifOption::OnHalt as i32, 2);
    assert_eq!(ErlNifOption::OnUnloadThread as i32, 3);
    assert_eq!(ErlNifIOQueueOpts::Normal as i32, 1);
    // `erl_driver.h:162-167`.
    assert_eq!(ErlDrvTimeUnit::Sec as i32, 0);
    assert_eq!(ErlDrvTimeUnit::Msec as i32, 1);
    assert_eq!(ErlDrvTimeUnit::Usec as i32, 2);
    assert_eq!(ErlDrvTimeUnit::Nsec as i32, 3);
}

#[test]
fn flag_values_match_the_header() {
    // `erl_nif.h:188-192,239-243,272-275,277-279`.
    assert_eq!(ErlNifResourceFlags::CREATE.bits(), 1);
    assert_eq!(ErlNifResourceFlags::TAKEOVER.bits(), 2);
    assert_eq!(ErlNifUniqueInteger::POSITIVE.bits(), 1 << 0);
    assert_eq!(ErlNifUniqueInteger::MONOTONIC.bits(), 1 << 1);
    assert_eq!(ErlNifDirtyTaskFlags::CPU_BOUND.bits(), 1);
    assert_eq!(ErlNifDirtyTaskFlags::IO_BOUND.bits(), 2);
    assert_eq!(ErlNifBinaryToTerm::SAFE.bits(), 0x2000_0000);
    // `erl_drv_nif.h:49-52,55-62`.
    assert_eq!(ErlDirtyJobFlags::CPU_BOUND.bits(), ERL_DIRTY_JOB_CPU_BOUND);
    assert_eq!(ErlDirtyJobFlags::IO_BOUND.bits(), ERL_DIRTY_JOB_IO_BOUND);
    assert_eq!(ErlNifSelectFlags::READ.bits(), 1 << 0);
    assert_eq!(ErlNifSelectFlags::WRITE.bits(), 1 << 1);
    assert_eq!(ErlNifSelectFlags::STOP.bits(), 1 << 2);
    assert_eq!(ErlNifSelectFlags::CANCEL.bits(), 1 << 3);
    assert_eq!(ErlNifSelectFlags::CUSTOM_MSG.bits(), 1 << 4);
    assert_eq!(ErlNifSelectFlags::ERROR.bits(), 1 << 5);
}

#[test]
fn version_and_macro_constants_match_the_header() {
    // `erl_nif.h:67-68,79,91,107`; `erl_driver.h:70-72,88`.
    assert_eq!(ERL_NIF_MAJOR_VERSION, 2);
    assert_eq!(ERL_NIF_MINOR_VERSION, 18);
    assert_eq!(ERL_NIF_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD, 2);
    assert_eq!(ERL_NIF_VM_VARIANT, "beam.vanilla");
    assert_eq!(ERL_NIF_MIN_ERTS_VERSION, "erts-14.0");
    assert_eq!(ERL_DRV_EXTENDED_MARKER, 0xfeeeeeedu32 as i32);
    assert_eq!(ERL_DRV_EXTENDED_MAJOR_VERSION, 3);
    assert_eq!(ERL_DRV_EXTENDED_MINOR_VERSION, 3);
    assert_eq!(ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD, 2);

    // `erl_drv_nif.h:145-150`; `erl_nif.h:114`; `erl_driver.h:160`.
    assert_eq!(ERTS_NAPI_SEC, 0);
    assert_eq!(ERTS_NAPI_MSEC, 1);
    assert_eq!(ERTS_NAPI_USEC, 2);
    assert_eq!(ERTS_NAPI_NSEC, 3);
    assert_eq!(ERTS_NAPI_TIME_ERROR, i64::MIN);
    assert_eq!(ERL_NIF_TIME_ERROR, i64::MIN);
    assert_eq!(ERL_DRV_TIME_ERROR, i64::MIN);

    // `erl_drv_nif.h:55-62` — select mode bits.
    assert_eq!(ERL_NIF_SELECT_READ, 1 << 0);
    assert_eq!(ERL_NIF_SELECT_WRITE, 1 << 1);
    assert_eq!(ERL_NIF_SELECT_STOP, 1 << 2);
    assert_eq!(ERL_NIF_SELECT_CANCEL, 1 << 3);
    assert_eq!(ERL_NIF_SELECT_CUSTOM_MSG, 1 << 4);
    assert_eq!(ERL_NIF_SELECT_ERROR, 1 << 5);

    // `erl_nif.h:179-186` — `enif_select` return bits.
    assert_eq!(ERL_NIF_SELECT_STOP_CALLED, 1 << 0);
    assert_eq!(ERL_NIF_SELECT_STOP_SCHEDULED, 1 << 1);
    assert_eq!(ERL_NIF_SELECT_INVALID_EVENT, 1 << 2);
    assert_eq!(ERL_NIF_SELECT_FAILED, 1 << 3);
    assert_eq!(ERL_NIF_SELECT_READ_CANCELLED, 1 << 4);
    assert_eq!(ERL_NIF_SELECT_WRITE_CANCELLED, 1 << 5);
    assert_eq!(ERL_NIF_SELECT_ERROR_CANCELLED, 1 << 6);
    assert_eq!(ERL_NIF_SELECT_NOTSUP, 1 << 7);

    // `erl_nif.h:334-337` — `ERL_NIF_THR_*`; `erl_nif.h:286` — `ERL_NIF_IOVEC_SIZE`.
    assert_eq!(ERL_NIF_THR_UNDEFINED, 0);
    assert_eq!(ERL_NIF_THR_NORMAL_SCHEDULER, 1);
    assert_eq!(ERL_NIF_THR_DIRTY_CPU_SCHEDULER, 2);
    assert_eq!(ERL_NIF_THR_DIRTY_IO_SCHEDULER, 3);
    assert_eq!(ERL_NIF_IOVEC_SIZE, 16);
}

#[test]
fn driver_constant_values_match_the_header() {
    // `erl_driver.h:61-64`.
    assert_eq!(ERL_DRV_READ, 1);
    assert_eq!(ERL_DRV_WRITE, 2);
    assert_eq!(ERL_DRV_USE, 4);
    assert_eq!(ERL_DRV_USE_NO_CALLBACK, 12);

    // `erl_driver.h:98-99,103-104,107-110`.
    assert_eq!(PORT_CONTROL_FLAG_BINARY, 1 << 0);
    assert_eq!(PORT_CONTROL_FLAG_HEAVY, 1 << 1);
    assert_eq!(PORT_FLAG_BINARY, 1 << 0);
    assert_eq!(PORT_FLAG_LINE, 1 << 1);
    assert_eq!(ERL_DRV_FLAG_USE_PORT_LOCKING, 1 << 0);
    assert_eq!(ERL_DRV_FLAG_SOFT_BUSY, 1 << 1);
    assert_eq!(ERL_DRV_FLAG_NO_BUSY_MSGQ, 1 << 2);
    assert_eq!(ERL_DRV_FLAG_USE_INIT_ACK, 1 << 3);

    // `erl_driver.h:515` — `DRIVER_CALL_KEEP_BUFFER`.
    assert_eq!(DRIVER_CALL_KEEP_BUFFER, 0x1);

    // `erl_driver.h:176-191` — the start-failure codes. These are `*mut` values, so rustc forbids
    // pinning them during const evaluation (rust-lang/rust#53020); the runtime check is here.
    // `ERL_DRV_ERROR_GENERAL_INT_` = -1, `ERL_DRV_ERROR_ERRNO_INT_` = -2,
    // `ERL_DRV_ERROR_BADARG_INT_` = -3.
    assert_eq!(ERL_DRV_ERROR_GENERAL as isize, -1);
    assert_eq!(ERL_DRV_ERROR_ERRNO as isize, -2);
    assert_eq!(ERL_DRV_ERROR_BADARG as isize, -3);

    // `erl_driver.h:323-326` — `ERL_DRV_BUSY_MSGQ_*` (`ErlDrvSizeT` = `size_t`).
    assert_eq!(ERL_DRV_BUSY_MSGQ_DISABLED, usize::MAX);
    assert_eq!(ERL_DRV_BUSY_MSGQ_READ_ONLY, 0);
    assert_eq!(ERL_DRV_BUSY_MSGQ_LIM_MAX, usize::MAX - 1);
    assert_eq!(ERL_DRV_BUSY_MSGQ_LIM_MIN, 1);

    // `erl_driver.h:535-555` — the term-data tags.
    assert_eq!(ERL_DRV_NIL, 1);
    assert_eq!(ERL_DRV_ATOM, 2);
    assert_eq!(ERL_DRV_INT, 3);
    assert_eq!(ERL_DRV_PORT, 4);
    assert_eq!(ERL_DRV_BINARY, 5);
    assert_eq!(ERL_DRV_STRING, 6);
    assert_eq!(ERL_DRV_TUPLE, 7);
    assert_eq!(ERL_DRV_LIST, 8);
    assert_eq!(ERL_DRV_STRING_CONS, 9);
    assert_eq!(ERL_DRV_PID, 10);
    assert_eq!(ERL_DRV_FLOAT, 11);
    assert_eq!(ERL_DRV_EXT2TERM, 12);
    assert_eq!(ERL_DRV_UINT, 13);
    assert_eq!(ERL_DRV_BUF2BINARY, 14);
    assert_eq!(ERL_DRV_INT64, 15);
    assert_eq!(ERL_DRV_UINT64, 16);
    assert_eq!(ERL_DRV_MAP, 17);

    // `erl_port.h:67` — the invalid-port sentinel is the pointer `-1`, never `NULL`.
    assert_eq!(ERTS_INVALID_ERL_DRV_PORT as isize, -1);
}
