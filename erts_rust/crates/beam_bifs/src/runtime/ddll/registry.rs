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

//! The loaded-driver registry — the process-global table of the drivers
//! `erl_ddll:try_load/3` has `dlopen`ed.
//!
//! Ground truth (read-only): `erts/emulator/beam/erl_bif_ddll.c` (the `erts_ddll_driver_t`
//! list `erts_ddll_driver_loaded`/`erts_ddll_try_load`/`erts_ddll_try_unload` walk,
//! `:1830-1980`) and `erts/emulator/sys/unix/erl_unix_sys_ddll.c` (the `dlopen` handle +
//! `ErlDrvEntry` a load produces).
//!
//! Each entry is keyed by the driver **name** and holds the open
//! [`DriverLibrary`](beam_abi::driver::loader::DriverLibrary) — so the entry owns the
//! `dlopen` handle and, when dropped, runs the driver's `finish` callback and `dlclose`s it
//! (the C's `do_unload_driver_entry`, `erl_bif_ddll.c:1559-1564`).
//!
//! # Why a `thread_local!`
//!
//! `DriverLibrary` holds raw `*mut c_void`/`*mut ErlDrvEntry` (it is `!Send`), and this crate is
//! `#![deny(unsafe_code)]`, so the registry cannot live in a `static OnceLock<Mutex<…>>` — that
//! would require an `unsafe impl Send`. A `thread_local!` is the correct scope and needs no
//! `unsafe`, exactly as `beam_rt`'s own driver-entry list does
//! (`crates/beam_rt/src/port/driver.rs`, `DRIVER_ENTRIES`).
//!
//! The consequence is that the registry is **per-thread**: a driver loaded on one thread is
//! invisible to another. That is faithful for this runtime today — every BIF dispatch happens on
//! the single runtime thread — and it is the same limitation `beam_rt`'s driver-entry list already
//! carries.
//!
//! # The statically linked built-ins
//!
//! The emulator links three drivers into the binary — [`STATIC_DRIVERS`] (`tcp_inet`, `udp_inet`,
//! `ram_file_drv`). They carry no `dlopen` handle, so they cannot be registry entries (an entry owns
//! a [`DriverLibrary`]); instead they are a pure constant the loading keys and `info/2` consult
//! ([`is_static`]/[`static_names`]) to answer the C's `handle == NULL` branch. `loaded_drivers/0`
//! reports them first, ahead of the dynamically loaded drivers.
//!
//! # Honest divergences from the C
//!
//! * **Unload is immediate.** The C's `try_unload` is a *pending* state machine (a reload/unload is
//!   staged and completed by the scheduler); here the entry is removed at once and the
//!   `DriverLibrary` drops synchronously.
//! * **Single-process bookkeeping only.** The C's entry also tracks reload state, monitor lists and
//!   the loading process for `not_loaded_by_this_process`; this registry records the loading pid but
//!   does not yet enforce it.

// Several fields (`path`, `options`, `kill_ports`, `loader`) are recorded for the host bridge /
// diagnostics and read by this module's tests and by a future `monitor/2`/`info/2` implementation;
// the loading BIFs themselves only need `library`'s Drop. `get`/`len` are the inspectors for that
// state. Silence the module's dead-code lint rather than deleting the honest bookkeeping.
#![allow(dead_code)]

use std::cell::RefCell;

use beam_abi::driver::loader::DriverLibrary;
use beam_process::ProcessId;

/// The three drivers the emulator links statically, in the oracle's list order
/// (`tcp_inet`, `udp_inet`, `ram_file_drv`).
///
/// These are the `driver_list` entries whose `handle == NULL`
/// (`erl_bif_ddll.c:282,545,728,743,752,775`). `erts_add_driver_entry`
/// (`io.c:7650-7665`) prepends to `driver_list` and `erl_ddll_loaded_drivers_0`
/// (`:666-687`) walks it front→back CONSing each name onto the front, so this
/// insertion order is the order `loaded_drivers/0` reports: the three built-ins
/// first, then the dynamically loaded drivers in load order.
pub(crate) const STATIC_DRIVERS: [&str; 3] = ["tcp_inet", "udp_inet", "ram_file_drv"];

/// Whether `name` names one of the statically linked built-in drivers.
pub(crate) fn is_static(name: &str) -> bool {
    STATIC_DRIVERS.contains(&name)
}

/// The statically linked driver names, in the oracle's list order.
pub(crate) fn static_names() -> &'static [&'static str] {
    &STATIC_DRIVERS
}

/// One successfully loaded driver.
///
/// The `library` owns the `dlopen` handle: dropping the entry runs the driver's `finish` callback
/// and closes the object (the C's `do_unload_driver_entry`).
pub(crate) struct Loaded {
    /// The directory the driver was loaded from (`try_load`'s `Path` argument).
    pub(crate) path: String,
    /// The open shared object, validated against `driver_name`.
    pub(crate) library: DriverLibrary,
    /// The recognised option names (e.g. `kill_ports`, `monitor`) of the load.
    pub(crate) options: Vec<String>,
    /// Whether the `kill_ports` option was given.
    pub(crate) kill_ports: bool,
    /// The process that performed the load (the C's `loader` pid).
    pub(crate) loader: ProcessId,
}

thread_local! {
    /// The process-global driver list, insertion-ordered (a `Vec` so `names/0` reports load order
    /// like the C's list walk). See the module docs for the thread-local rationale.
    static LOADED_DRIVERS: RefCell<Vec<(String, Loaded)>> = const { RefCell::new(Vec::new()) };
}

/// Records `name` as loaded (replacing any existing entry of the same name).
///
/// The caller checks [`contains`] first for the `already_loaded` arm, so a replace only happens when
/// a host deliberately re-registers.
pub(crate) fn insert(name: String, loaded: Loaded) {
    LOADED_DRIVERS.with(|drivers| {
        let mut drivers = drivers.borrow_mut();
        match drivers.iter_mut().find(|(key, _)| *key == name) {
            Some(slot) => slot.1 = loaded,
            None => drivers.push((name, loaded)),
        }
    });
}

/// Removes and returns the entry for `name`, dropping the `DriverLibrary` (which runs the driver's
/// `finish` callback and `dlclose`s the object) at the caller's drop point.
pub(crate) fn remove(name: &str) -> Option<Loaded> {
    LOADED_DRIVERS.with(|drivers| {
        let mut drivers = drivers.borrow_mut();
        let index = drivers.iter().position(|(key, _)| key == name)?;
        Some(drivers.remove(index).1)
    })
}

/// Whether `name` is currently loaded.
pub(crate) fn contains(name: &str) -> bool {
    LOADED_DRIVERS.with(|drivers| drivers.borrow().iter().any(|(key, _)| key == name))
}

/// Every loaded driver's name, in load order.
pub(crate) fn names() -> Vec<String> {
    LOADED_DRIVERS.with(|drivers| {
        drivers
            .borrow()
            .iter()
            .map(|(key, _)| key.clone())
            .collect()
    })
}

/// Runs `f` over the entry for `name`, or `None` when it is not loaded.
pub(crate) fn get<R>(name: &str, f: impl FnOnce(&Loaded) -> R) -> Option<R> {
    LOADED_DRIVERS.with(|drivers| {
        let drivers = drivers.borrow();
        drivers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, loaded)| f(loaded))
    })
}

/// The number of loaded drivers.
pub(crate) fn len() -> usize {
    LOADED_DRIVERS.with(|drivers| drivers.borrow().len())
}
