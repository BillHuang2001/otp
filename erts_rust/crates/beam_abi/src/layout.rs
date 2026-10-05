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

use crate::common::{ErlDirtyJobFlags, ErlDrvMonitor, ErlDrvSysInfo, ErlDrvThreadOpts, ErlNifSelectFlags, SysIOVec};
use crate::driver::api::ErlDrvApi;
use crate::driver::types::{ErlDrvBinary, ErlDrvEntry, ErlDrvNowData, ErlDrvTimeUnit, ErlIOVec};
use crate::nif::api::ErlNifApi;
use crate::nif::term::ERL_NIF_TERM;
use crate::nif::types::{
    ErlNifBinary, ErlNifBinaryToTerm, ErlNifCharEncoding, ErlNifDirtyTaskFlags, ErlNifEntry,
    ErlNifFunc, ErlNifHash, ErlNifIOQueueOpts, ErlNifIOVec, ErlNifMapIterator,
    ErlNifMapIteratorEntry, ErlNifOption, ErlNifPid, ErlNifPort, ErlNifResourceFlags,
    ErlNifResourceTypeInit, ErlNifTermType, ErlNifTimeUnit, ErlNifUniqueInteger,
};
use beam_term::Eterm;

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
// The term seam — `ERL_NIF_TERM`, the internal `Eterm`, and the raw machine word are one type
// (compatibility contract A.3: the internal term *is* the NIF term)
// ---------------------------------------------------------------------------------------------

const _: () = {
    assert!(core::mem::size_of::<ERL_NIF_TERM>() == 8);
    assert!(core::mem::size_of::<Eterm>() == 8);
    assert!(core::mem::size_of::<usize>() == 8);
    assert!(core::mem::align_of::<ERL_NIF_TERM>() == core::mem::align_of::<usize>());
    assert!(core::mem::align_of::<ERL_NIF_TERM>() == core::mem::align_of::<Eterm>());
};
