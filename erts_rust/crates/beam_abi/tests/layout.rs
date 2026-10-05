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
    ErlDirtyJobFlags, ErlDrvMonitor, ErlDrvSysInfo, ErlDrvThreadOpts, ErlNifSelectFlags, SysIOVec,
};
use beam_abi::driver::api::ErlDrvApi;
use beam_abi::driver::types::{ErlDrvBinary, ErlDrvEntry, ErlDrvNowData, ErlDrvTimeUnit, ErlIOVec};
use beam_abi::nif::api::ErlNifApi;
use beam_abi::nif::term::ERL_NIF_TERM;
use beam_abi::nif::types::{
    ErlNifBinary, ErlNifBinaryToTerm, ErlNifCharEncoding, ErlNifDirtyTaskFlags, ErlNifEntry,
    ErlNifFunc, ErlNifHash, ErlNifIOQueueOpts, ErlNifIOVec, ErlNifMapIterator,
    ErlNifMapIteratorEntry, ErlNifOption, ErlNifPid, ErlNifPort, ErlNifResourceFlags,
    ErlNifResourceTypeInit, ErlNifTermType, ErlNifTimeUnit, ErlNifUniqueInteger,
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
    assert_eq!(size_of::<Eterm>(), 8);
    assert_eq!(size_of::<usize>(), 8);
    assert_eq!(align_of::<ERL_NIF_TERM>(), align_of::<usize>());
    assert_eq!(align_of::<ERL_NIF_TERM>(), align_of::<Eterm>());
}
