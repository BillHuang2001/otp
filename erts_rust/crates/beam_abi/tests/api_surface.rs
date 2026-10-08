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

//! The function-table surface freeze: the strongest available check of the frozen table *shape*.
//!
//! `ErlNifApi` is the header-exact LP64 `enif_*` surface (176 entries) and `ErlDrvApi` the
//! `driver_*`/`erl_drv_*` surface (111 entries). Because every column is one machine word, the size
//! of each table is its field count times `size_of::<usize>()` — an exact, compile-checked freeze
//! of the number of foreign functions the runtime must export.

use core::mem::size_of;

use beam_abi::driver::api::{ErlDrvApi, ERL_DRV_API_FIELD_COUNT};
use beam_abi::nif::api::{ErlNifApi, ERL_NIF_API_FIELD_COUNT};

// Compile-time: each table is exactly its field count in machine words (a duplicate of the asserts
// in `driver/api.rs` / `nif/api.rs` is fine — this target ties the exported const to the type).
const _: () = {
    assert!(
        size_of::<ErlNifApi>() == ERL_NIF_API_FIELD_COUNT * size_of::<usize>()
    );
    assert!(
        size_of::<ErlDrvApi>() == ERL_DRV_API_FIELD_COUNT * size_of::<usize>()
    );
};

#[test]
fn nif_table_is_176_machine_words_on_lp64() {
    assert_eq!(ERL_NIF_API_FIELD_COUNT, 176, "header-exact LP64 enif_* count");
    assert_eq!(size_of::<ErlNifApi>(), 176 * 8);
    assert_eq!(
        size_of::<ErlNifApi>(),
        ERL_NIF_API_FIELD_COUNT * size_of::<usize>()
    );
}

#[test]
fn driver_table_is_111_machine_words_on_lp64() {
    assert_eq!(ERL_DRV_API_FIELD_COUNT, 111, "header-exact driver_* count");
    assert_eq!(size_of::<ErlDrvApi>(), 111 * 8);
    assert_eq!(
        size_of::<ErlDrvApi>(),
        ERL_DRV_API_FIELD_COUNT * size_of::<usize>()
    );
}

#[test]
fn empty_nif_table_is_all_none() {
    // Destructure representative fields (first, middle, last) plus a `..` for the rest.
    let ErlNifApi {
        enif_priv_data,
        enif_alloc,
        enif_send,
        enif_select,
        enif_set_option,
        enif_max_atom_cache_index,
        ..
    } = ErlNifApi::empty();

    assert!(enif_priv_data.is_none());
    assert!(enif_alloc.is_none());
    assert!(enif_send.is_none());
    assert!(enif_select.is_none());
    assert!(enif_set_option.is_none());
    assert!(enif_max_atom_cache_index.is_none());
    assert_eq!(size_of_val(&ErlNifApi::empty()), 176 * 8);
}

#[test]
fn empty_driver_table_is_all_none() {
    let ErlDrvApi {
        erl_drv_busy_msgq_limits,
        driver_select,
        erl_drv_mutex_lock,
        driver_async,
        erl_drv_set_os_pid,
        ..
    } = ErlDrvApi::empty();

    assert!(erl_drv_busy_msgq_limits.is_none());
    assert!(driver_select.is_none());
    assert!(erl_drv_mutex_lock.is_none());
    assert!(driver_async.is_none());
    assert!(erl_drv_set_os_pid.is_none());
    assert_eq!(size_of_val(&ErlDrvApi::empty()), 111 * 8);
}
