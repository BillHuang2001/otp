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

//! The `nif_init()` / `driver_init()` entry-point shapes.
//!
//! Builds an `ErlNifEntry` (the value a NIF library's `nif_init()` returns) and an `ErlDrvEntry`
//! (the value a driver's `driver_init()` returns) with the frozen version constants and null
//! callbacks, then reads the fields back. This exercises the `#[repr(C)]` field order the loader
//! relies on — the position of `major`/`minor`/`num_of_funcs`/`vm_variant` and of
//! `extended_marker`/`major_version`/`minor_version` in memory.

use core::ffi::c_char;
use core::mem::offset_of;

use beam_abi::driver::types::ErlDrvEntry;
use beam_abi::nif::types::{ErlNifEntry, ErlNifFunc};
use beam_abi::{
    ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MARKER, ERL_DRV_EXTENDED_MINOR_VERSION,
    ERL_NIF_MAJOR_VERSION, ERL_NIF_MINOR_VERSION, ERL_NIF_VM_VARIANT,
};

#[test]
fn nif_func_field_order_is_frozen() {
    assert_eq!(offset_of!(ErlNifFunc, name), 0);
    assert_eq!(offset_of!(ErlNifFunc, arity), 8);
    assert_eq!(offset_of!(ErlNifFunc, fptr), 16);
    assert_eq!(offset_of!(ErlNifFunc, flags), 24);
}

#[test]
fn nif_init_shape_reads_back() {
    let mut funcs = [ErlNifFunc::default()];

    let entry = ErlNifEntry {
        major: ERL_NIF_MAJOR_VERSION,
        minor: ERL_NIF_MINOR_VERSION,
        num_of_funcs: funcs.len() as core::ffi::c_int,
        funcs: funcs.as_mut_ptr(),
        vm_variant: ERL_NIF_VM_VARIANT.as_ptr().cast::<c_char>(),
        ..ErlNifEntry::default()
    };

    assert_eq!(entry.major, 2, "ERL_NIF_MAJOR_VERSION");
    assert_eq!(entry.minor, 18, "ERL_NIF_MINOR_VERSION");
    assert_eq!(entry.num_of_funcs, 1);
    assert_eq!(entry.funcs, funcs.as_mut_ptr());
    assert!(!entry.funcs.is_null());
    assert_eq!(
        entry.vm_variant,
        ERL_NIF_VM_VARIANT.as_ptr().cast::<c_char>(),
        "vm_variant points at ERL_NIF_VM_VARIANT"
    );
    // All callbacks default to null.
    assert!(entry.load.is_none());
    assert!(entry.reload.is_none());
    assert!(entry.upgrade.is_none());
    assert!(entry.unload.is_none());
}

#[test]
fn driver_init_shape_reads_back() {
    let mut name_buf = *b"tst_drv\0";

    let entry = ErlDrvEntry {
        driver_name: name_buf.as_mut_ptr().cast::<c_char>(),
        extended_marker: ERL_DRV_EXTENDED_MARKER,
        major_version: ERL_DRV_EXTENDED_MAJOR_VERSION,
        minor_version: ERL_DRV_EXTENDED_MINOR_VERSION,
        ..ErlDrvEntry::default()
    };

    assert_eq!(entry.extended_marker, 0xfeeeeeedu32 as core::ffi::c_int);
    assert_eq!(entry.major_version, 3, "ERL_DRV_EXTENDED_MAJOR_VERSION");
    assert_eq!(entry.minor_version, 3, "ERL_DRV_EXTENDED_MINOR_VERSION");
    assert_eq!(entry.driver_name, name_buf.as_mut_ptr().cast::<c_char>());
    // All callbacks default to null.
    assert!(entry.init.is_none());
    assert!(entry.start.is_none());
    assert!(entry.stop.is_none());
    assert!(entry.output.is_none());
    assert!(entry.process_exit.is_none());
    assert!(entry.emergency_close.is_none());
}
