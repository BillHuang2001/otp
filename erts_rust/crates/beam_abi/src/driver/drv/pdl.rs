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

//! **Port data lock (PDL), driver monitors & the deprecated wall clock** family of the driver host
//! layer (part of the `driver_*` / `erl_drv_*` C ABI, workspace contract **A.4**).
//!
//! # Slots owned by this file
//!
//! * port data lock — `driver_pdl_create`, `driver_pdl_lock`, `driver_pdl_unlock`,
//!   `driver_pdl_get_refc`, `driver_pdl_inc_refc`, `driver_pdl_dec_refc`
//! * monitors — `driver_monitor_process`, `driver_demonitor_process`,
//!   `driver_get_monitored_process`, `driver_compare_monitors`
//! * deprecated — `driver_get_now` (`ERL_DRV_DEPRECATED_FUNC`)
//!
//! # The binary refcount trio lives in `mem`
//!
//! `driver_binary_get_refc`/`inc_refc`/`dec_refc` (`io.c:6537-6558`) are **owned by
//! [`super::mem`]**, not by this file: they operate on the emulator's **private** `Binary` header
//! (`ErlDrvBinary2Binary(dbp)` → `bp->intern.refc`), which sits *immediately before* the frozen
//! `ErlDrvBinary`, so they belong next to the allocator that now places that header —
//! [`super::mem::driver_alloc_binary`]. See that file's *Driver binaries* section for the block
//! layout, the alignment argument and the measured oracle values.
//!
//! # The port data lock
//!
//! Ground truth `erts/emulator/beam/io.c:6717-6787`. The C's PDL is an `erts_alloc`ated
//! `struct erl_drv_port_data_lock` — a **mutex plus a reference count**, with the port's
//! `pp->port_data_lock` slot as the "has one already" flag — so the object (not a host service) is
//! what the C functions operate on. This file therefore owns the object: [`PdlState`], a real
//! mutual-exclusion lock plus an atomic `ErlDrvSInt` reference count, held in a process-wide
//! registry keyed by the opaque [`ErlDrvPDL`] handle. The exact C sequences are reproduced:
//!
//! * `driver_pdl_create` — a valid port admits one PDL; a second attempt returns `NULL`
//!   (the C's `pp->port_data_lock` check). The handle itself is asked of the host
//!   ([`DriverHost::pdl_create`](crate::driver::host::DriverHost::pdl_create), the one reachable
//!   hook of this family — see *Deviations*), whose `None` is the C's "cannot create" (`NULL`).
//! * `driver_pdl_lock` — `pdl_inc_refc(pdl); erts_mtx_lock(&pdl->mtx);` (refc **then** lock, so the
//!   count reads one higher for the duration of the critical section).
//! * `driver_pdl_unlock` — `erts_mtx_unlock(&pdl->mtx); refc = pdl_dectest_refc(pdl);` — unlock
//!   **then** decrement; a count that reaches zero releases the PDL.
//! * `driver_pdl_get_refc` — a plain read; `inc_refc`/`dec_refc` return the value *after* the
//!   change (`pdl_inctest_refc`/`pdl_dectest_refc`). The C has no failure value (it dereferences
//!   the handle), so a `NULL`/foreign handle is answered with [`PDL_INVALID_REFCOUNT`] (`-1`) —
//!   never a crash, never a fabricated count.
//!
//! # The monitor family
//!
//! Ground truth `erts/emulator/beam/io.c:6994-7147`. `ErlDrvMonitor` is an opaque 32-byte holder
//! whose first `ERTS_REF_THING_SIZE` (`= 3`, `erl_term.h:990`) `Eterm`-sized words are an internal
//! reference thing: `driver_monitor_process` fills it, `driver_demonitor_process` and
//! `driver_get_monitored_process` consume it, and `driver_compare_monitors` is a **pure** `memcmp`
//! over exactly those 24 bytes (`io.c:7142`), needing no port and no host.
//! [`ERL_DRV_MONITOR_OK`] / [`ERL_DRV_MONITOR_NOT_ALIVE`] / [`ERL_DRV_MONITOR_ERROR`] reproduce the
//! C's `0` / `> 0` / `< 0` reason codes (`erl_driver.md`, `driver_monitor_process`: *"Returns `0`
//! on success, `< 0` if no callback is provided, and `> 0` if the process is no longer alive"*;
//! `driver_demonitor_process`: *"Returns `0` if a monitor was removed and `> 0` if the monitor no
//! longer exists"*). The header defines **no** `ERL_DRV_MONITOR_*` constants, so these are this
//! crate's names for the C's literal values.
//!
//! [`monitor_identity`] / [`monitor_from_identity`] are the **safe** half of the same family. The
//! runtime behind the seam (the sibling `beam_rt`/`beam_io`, which is `#![deny(unsafe_code)]`) must
//! key its per-port monitor tree on a monitor's identity, but a `DriverHost` monitor hook only ever
//! receives a raw `*const ErlDrvMonitor`. These two functions move exactly the 24 reference-thing
//! bytes across that boundary — the read side and its write-side counterpart — so the host never
//! has to spell an `unsafe` read of the opaque holder. The bytes stay **opaque**: no encoding is
//! invented here (see the function docs).
//!
//! # The deprecated wall clock
//!
//! `driver_get_now(ErlDrvNowData *now)` (`io.c:6957`): `-1` for a `NULL` out-parameter, else the
//! three-field breakdown of "now" and `0`. Filled from `clock_gettime(CLOCK_REALTIME)`; the
//! `ErlDrvNowData` epoch is documented as arbitrary (`erl_driver.md`: *"three values measured from
//! some arbitrary point in the past"*), so the split — `megasecs = secs / 1_000_000`,
//! `secs = secs % 1_000_000`, `microsecs = nsec / 1000` — carries no epoch promise.
//!
//! # Deviations from the oracle
//!
//! * The seam hooks `pdl_lock`/`pdl_unlock`/`pdl_get_refc`/`pdl_inc_refc`/`pdl_dec_refc`/`now` are
//!   **unreachable from these C entry points**: none of them takes an `ErlDrvPort`, and a bare
//!   `ErlDrvPDL` carries no reference to the host that produced it (reaching one would need a
//!   process-wide handle→host table whose entries outlive the host and alias it from a driver
//!   thread — neither sound). The observable contract is therefore implemented here, which is also
//!   what the C does: the lock and the reference count live in the PDL object itself. Only
//!   `driver_pdl_create` delegates (handle identity + the host's one-PDL-per-port rule).
//! * `driver_monitor_process` reaches the host through the **three-way**
//!   [`DriverHost::monitor_process_result`](crate::driver::host::DriverHost::monitor_process_result)
//!   hook, so all three C outcomes are expressible: [`MonitorResult::Ok`] ⇒ `0` (the holder is
//!   filled), [`MonitorResult::NotAlive`] ⇒ the positive [`ERL_DRV_MONITOR_NOT_ALIVE`] (`1`), and
//!   [`MonitorResult::Error`] ⇒ the negative [`ERL_DRV_MONITOR_ERROR`] (`-1`). A `process` operand
//!   that cannot name a process at all (neither an internal nor an external pid term — which the C
//!   would hand straight to its monitor-signal machinery) is likewise the positive
//!   [`ERL_DRV_MONITOR_NOT_ALIVE`] code. The two-way
//!   [`DriverHost::monitor_process`](crate::driver::host::DriverHost::monitor_process) hook remains
//!   the compatibility subset the three-way one defaults to (`Some` ⇒ `Ok`, `None` ⇒ `Error`).
//! * `driver_compare_monitors` with a `NULL` operand is ordered (`NULL` first) where the C's
//!   `memcmp` would fault; `driver_demonitor_process`/`driver_get_monitored_process` with a `NULL`
//!   monitor answer the C's port-failure values (`-1` / `driver_term_nil`) instead of faulting.
//! * The registry is process-wide and keyed by the handle's *address*; the handle is never
//!   dereferenced (the C's `pdl->mtx`/`pdl->refc` live in this crate's object instead).

use core::mem::size_of;
use core::sync::atomic::{AtomicIsize, Ordering};
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use libc::{c_int, c_ulong};

use beam_term::prelude::{is_boxed, is_external_pid, is_internal_pid, Eterm};

use crate::driver::api::ErlDrvApi;
use crate::driver::host::MonitorResult;
use crate::driver::types::{
    ErlDrvMonitor, ErlDrvNowData, ErlDrvPDL, ErlDrvPort, ErlDrvSInt, ErlDrvTermData,
};

use super::{driver_term_nil, env_for};

// ---------------------------------------------------------------------------------------------
// Reason codes (`io.c`'s literal returns; the header defines no `ERL_DRV_MONITOR_*` constants)
// ---------------------------------------------------------------------------------------------

/// `driver_monitor_process` success (`io.c:7037`).
pub const ERL_DRV_MONITOR_OK: c_int = 0;
/// The process is no longer alive / the monitor no longer exists (`io.c:7031`, `io.c:7075`).
pub const ERL_DRV_MONITOR_NOT_ALIVE: c_int = 1;
/// Invalid port, or a driver without a `process_exit` callback (`io.c:7021`, `io.c:7055`).
pub const ERL_DRV_MONITOR_ERROR: c_int = -1;

// The C distinguishes the three outcomes by their *sign* (`erl_driver.md`, `driver_monitor_process`:
// "Returns `0` on success, `< 0` if no callback is provided, and `> 0` if the process is no longer
// alive"). Pin those signs at compile time so a future edit cannot silently collapse two of them
// onto one side of zero — which is exactly the bug the three-way [`MonitorResult`] closes.
const _: () = assert!(ERL_DRV_MONITOR_OK == 0);
const _: () = assert!(ERL_DRV_MONITOR_NOT_ALIVE > 0);
const _: () = assert!(ERL_DRV_MONITOR_ERROR < 0);

/// The count reported for a `NULL`/foreign PDL handle.
///
/// The C dereferences the handle (so it has no failure value); `-1` is this crate's documented
/// "not one of mine" answer — a reference count is never negative.
pub const PDL_INVALID_REFCOUNT: ErlDrvSInt = -1;

/// The width of the internal **reference thing** a monitor carries:
/// `ERTS_REF_THING_SIZE * sizeof(Eterm)` (`io.c:6996`/`io.c:7146`) = `3 * 8` = 24 bytes on LP64.
///
/// It is the range `driver_monitor_process` writes into the holder (`erts_ref_to_driver_monitor`,
/// `io.c:6998-6999`), the range `driver_compare_monitors` compares (`io.c:7145-7146`) and the range
/// the two safe accessors ([`monitor_identity`] / [`monitor_from_identity`]) expose — one constant,
/// one width, no duplication.
const REF_THING_COMPARE_BYTES: usize = 3 * size_of::<Eterm>();

// ---------------------------------------------------------------------------------------------
// The PDL object and its registry
// ---------------------------------------------------------------------------------------------

/// One port data lock — the C's `struct erl_drv_port_data_lock` (`mtx` + `refc`) as real Rust
/// state: a genuine mutual-exclusion lock plus an atomic reference count starting at `1`
/// (`pdl_init_refc`, `io.c:6726`).
struct PdlState {
    /// The `erts_mtx_t` analogue: `true` while a driver thread holds the lock.
    held: Mutex<bool>,
    /// Signalled when the lock is released.
    released: Condvar,
    /// The `refc` analogue.
    refcount: AtomicIsize,
}

impl PdlState {
    /// A fresh PDL: unlocked, reference count `1`.
    fn new() -> PdlState {
        PdlState {
            held: Mutex::new(false),
            released: Condvar::new(),
            refcount: AtomicIsize::new(1),
        }
    }

    /// Take the lock, blocking until it is free (the C's `erts_mtx_lock`).
    ///
    /// A poisoned guard is recovered (`unwrap_or_else(|e| e.into_inner())`): the guarded `bool` is
    /// only ever toggled, so a poisoning panic cannot leave it inconsistent, and a library function
    /// must not panic into its C caller (workspace `CONTEXT.md` §3).
    fn lock(&self) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while *held {
            held = self.released.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        *held = true;
    }

    /// Release the lock (the C's `erts_mtx_unlock`).
    fn unlock(&self) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        *held = false;
        self.released.notify_one();
    }
}

/// The live PDLs, keyed by the handle's address.
///
/// The C keeps the object reachable from the port (`pp->port_data_lock`); this crate keeps it here
/// because none of the `driver_pdl_*` entry points except `create` takes an `ErlDrvPort`.
type Registry = Mutex<BTreeMap<usize, Arc<PdlState>>>;

/// The process-wide PDL registry.
static PDLS: OnceLock<Registry> = OnceLock::new();

/// The registry, initialised on first use.
fn registry() -> &'static Registry {
    PDLS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// The registry guard (poisoning recovered, never panicked — see [`PdlState::lock`]).
fn registry_lock() -> MutexGuard<'static, BTreeMap<usize, Arc<PdlState>>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// The state behind `pdl`, or `None` for a `NULL`/foreign handle (never dereferences `pdl`).
fn state_for(pdl: ErlDrvPDL) -> Option<Arc<PdlState>> {
    if pdl.is_null() {
        return None;
    }
    registry_lock().get(&(pdl as usize)).cloned()
}

/// Release the PDL `pdl` (the C's `pdl_destroy`), i.e. forget its state.
fn destroy_pdl(pdl: ErlDrvPDL) {
    registry_lock().remove(&(pdl as usize));
}

// ---------------------------------------------------------------------------------------------
// Port data lock
// ---------------------------------------------------------------------------------------------

/// `ErlDrvPDL driver_pdl_create(ErlDrvPort port);` — create the port's data lock (`io.c:6717`).
///
/// Returns the PDL's opaque handle, or `NULL` when the port is invalid or already has one. The
/// handle is asked of [`DriverHost::pdl_create`](crate::driver::host::DriverHost::pdl_create) (the
/// host owns handle identity and the C's per-port rule); this crate then attaches the lock and the
/// reference count to it (see the module docs).
///
/// # Safety
///
/// `port` must be null or a live port handle.
#[no_mangle]
pub unsafe extern "C" fn driver_pdl_create(port: ErlDrvPort) -> ErlDrvPDL {
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return core::ptr::null_mut();
    };
    let Some(handle) = env.host().pdl_create() else {
        return core::ptr::null_mut();
    };
    if handle.is_null() {
        return core::ptr::null_mut();
    }
    let mut pdls = registry_lock();
    if pdls.contains_key(&(handle as usize)) {
        // The C's `pp->port_data_lock` check: a port admits exactly one PDL.
        return core::ptr::null_mut();
    }
    pdls.insert(handle as usize, Arc::new(PdlState::new()));
    handle
}

/// `void driver_pdl_lock(ErlDrvPDL pdl);` — take the port data lock (`io.c:6736`).
///
/// The C is `pdl_inc_refc(pdl); erts_mtx_lock(&pdl->mtx);` — the count is raised **before** the
/// lock is taken. A `NULL`/foreign handle is a no-op (the C would fault; a library function must
/// not).
///
/// # Safety
///
/// `pdl` must be null or a handle returned by [`driver_pdl_create`].
#[no_mangle]
pub unsafe extern "C" fn driver_pdl_lock(pdl: ErlDrvPDL) {
    let Some(state) = state_for(pdl) else {
        return;
    };
    state.refcount.fetch_add(1, Ordering::SeqCst);
    state.lock();
}

/// `void driver_pdl_unlock(ErlDrvPDL pdl);` — release the port data lock (`io.c:6746`).
///
/// The C is `erts_mtx_unlock(&pdl->mtx); refc = pdl_dectest_refc(pdl); if (!refc) pdl_destroy(pdl);`
/// — the lock is released **before** the count is lowered, and a count reaching zero releases the
/// PDL. A `NULL`/foreign handle is a no-op.
///
/// # Safety
///
/// `pdl` must be null or a handle returned by [`driver_pdl_create`] and currently locked by the
/// caller.
#[no_mangle]
pub unsafe extern "C" fn driver_pdl_unlock(pdl: ErlDrvPDL) {
    let Some(state) = state_for(pdl) else {
        return;
    };
    state.unlock();
    if state.refcount.fetch_sub(1, Ordering::SeqCst) == 1 {
        destroy_pdl(pdl);
    }
}

/// `ErlDrvSInt driver_pdl_get_refc(ErlDrvPDL pdl);` — the port data lock's reference count
/// (`io.c:6759`, `pdl_read_refc`). [`PDL_INVALID_REFCOUNT`] for a `NULL`/foreign handle.
///
/// # Safety
///
/// `pdl` must be null or a handle returned by [`driver_pdl_create`].
#[no_mangle]
pub unsafe extern "C" fn driver_pdl_get_refc(pdl: ErlDrvPDL) -> ErlDrvSInt {
    match state_for(pdl) {
        Some(state) => state.refcount.load(Ordering::SeqCst) as ErlDrvSInt,
        None => PDL_INVALID_REFCOUNT,
    }
}

/// `ErlDrvSInt driver_pdl_inc_refc(ErlDrvPDL pdl);` — raise the reference count, returning the
/// value reached (`io.c:6765`, `pdl_inctest_refc`). [`PDL_INVALID_REFCOUNT`] for a bad handle.
///
/// # Safety
///
/// `pdl` must be null or a handle returned by [`driver_pdl_create`].
#[no_mangle]
pub unsafe extern "C" fn driver_pdl_inc_refc(pdl: ErlDrvPDL) -> ErlDrvSInt {
    match state_for(pdl) {
        Some(state) => state.refcount.fetch_add(1, Ordering::SeqCst) as ErlDrvSInt + 1,
        None => PDL_INVALID_REFCOUNT,
    }
}

/// `ErlDrvSInt driver_pdl_dec_refc(ErlDrvPDL pdl);` — lower the reference count, returning the
/// value reached; a count reaching zero releases the PDL (`io.c:6776`, `pdl_dectest_refc` plus the
/// `pdl_destroy` below it). [`PDL_INVALID_REFCOUNT`] for a bad handle.
///
/// # Safety
///
/// `pdl` must be null or a handle returned by [`driver_pdl_create`].
#[no_mangle]
pub unsafe extern "C" fn driver_pdl_dec_refc(pdl: ErlDrvPDL) -> ErlDrvSInt {
    let Some(state) = state_for(pdl) else {
        return PDL_INVALID_REFCOUNT;
    };
    let left = state.refcount.fetch_sub(1, Ordering::SeqCst) - 1;
    if left == 0 {
        destroy_pdl(pdl);
    }
    left as ErlDrvSInt
}

// ---------------------------------------------------------------------------------------------
// Process monitors
// ---------------------------------------------------------------------------------------------

/// Can `term` name a process the driver may monitor?
///
/// The C hands the operand straight to its monitor-signal machinery, which accepts a pid. A term
/// that is neither an internal (local) pid nor an external (remote) pid cannot name a live process;
/// answering `false` for it keeps the `process` operand from being interpreted as anything else.
fn names_a_process(term: Eterm) -> bool {
    if is_internal_pid(term) {
        return true;
    }
    if !is_boxed(term) {
        return false;
    }
    // SAFETY: `term` is boxed, so its first word is a term word. The C entry point's contract is
    // that its operands are live terms — the same contract every boxed-term read in this crate
    // relies on (a forged pointer word is undetectable from the word alone, exactly as in the C).
    unsafe { is_external_pid(term) }
}

/// `int driver_monitor_process(ErlDrvPort port, ErlDrvTermData process, ErlDrvMonitor *monitor);`
/// — monitor `process` from the driver (`io.c:7012-7063`).
///
/// Returns [`ERL_DRV_MONITOR_OK`] (`0`) on success with `*monitor` filled, otherwise a non-zero
/// reason code (see the module docs for the exact mapping).
///
/// # Safety
///
/// `port` must be null or a live port handle; `monitor` must be null or point at a writable
/// `ErlDrvMonitor`.
#[no_mangle]
pub unsafe extern "C" fn driver_monitor_process(
    port: ErlDrvPort,
    process: ErlDrvTermData,
    monitor: *mut ErlDrvMonitor,
) -> c_int {
    if monitor.is_null() {
        return ERL_DRV_MONITOR_ERROR;
    }
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return ERL_DRV_MONITOR_ERROR;
    };
    let pid = Eterm::from_raw(process as usize);
    if !names_a_process(pid) {
        return ERL_DRV_MONITOR_NOT_ALIVE;
    }
    match env.host().monitor_process_result(pid) {
        MonitorResult::Ok(mon) => {
            // SAFETY: `monitor` is non-null and writable (checked above).
            unsafe { core::ptr::write(monitor, mon) };
            ERL_DRV_MONITOR_OK
        }
        MonitorResult::NotAlive => ERL_DRV_MONITOR_NOT_ALIVE,
        MonitorResult::Error => ERL_DRV_MONITOR_ERROR,
    }
}

/// `int driver_demonitor_process(ErlDrvPort port, const ErlDrvMonitor *monitor);` — cancel a driver
/// monitor (`io.c:7065-7102`): `0` when a monitor was removed, `1` when it no longer exists, `-1`
/// for an invalid port.
///
/// # Safety
///
/// `port` must be null or a live port handle; `monitor` must be null or point at a readable
/// `ErlDrvMonitor`.
#[no_mangle]
pub unsafe extern "C" fn driver_demonitor_process(
    port: ErlDrvPort,
    monitor: *const ErlDrvMonitor,
) -> c_int {
    if monitor.is_null() {
        return ERL_DRV_MONITOR_ERROR;
    }
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return ERL_DRV_MONITOR_ERROR;
    };
    // SAFETY: `monitor` is non-null and readable (contract above).
    env.host().demonitor_process(monitor)
}

/// `ErlDrvTermData driver_get_monitored_process(ErlDrvPort port, const ErlDrvMonitor *monitor);`
/// — the monitored process's term data (`io.c:7104-7140`), or `driver_term_nil` when the monitor is
/// unknown or the port is invalid.
///
/// # Safety
///
/// `port` must be null or a live port handle; `monitor` must be null or point at a readable
/// `ErlDrvMonitor`.
#[no_mangle]
pub unsafe extern "C" fn driver_get_monitored_process(
    port: ErlDrvPort,
    monitor: *const ErlDrvMonitor,
) -> ErlDrvTermData {
    if monitor.is_null() {
        return driver_term_nil;
    }
    // SAFETY: `port` is null or a live handle (contract above).
    let Some(env) = (unsafe { env_for(port) }) else {
        return driver_term_nil;
    };
    match env.host().get_monitored_process(monitor) {
        Some(pid) => pid.raw() as ErlDrvTermData,
        None => driver_term_nil,
    }
}

/// `int driver_compare_monitors(const ErlDrvMonitor *monitor1, const ErlDrvMonitor *monitor2);` —
/// compare two monitors (`io.c:7142`): a **pure** `memcmp` of the first
/// `ERTS_REF_THING_SIZE * sizeof(Eterm)` (24) bytes, i.e. the internal reference thing itself.
/// No port, no host state.
///
/// # Safety
///
/// Each operand must be null or point at a readable `ErlDrvMonitor`.
#[no_mangle]
pub unsafe extern "C" fn driver_compare_monitors(
    monitor1: *const ErlDrvMonitor,
    monitor2: *const ErlDrvMonitor,
) -> c_int {
    match (monitor1.is_null(), monitor2.is_null()) {
        (true, true) => 0,
        // The C's `memcmp` would fault; a `NULL` monitor is ordered first instead.
        (true, false) => -1,
        (false, true) => 1,
        // SAFETY: both operands are non-null and readable (contract above).
        (false, false) => unsafe { compare_monitor_payloads(monitor1, monitor2) },
    }
}

/// The **identity** of the monitor at `mon` — its first [`REF_THING_COMPARE_BYTES`] (24) bytes, the
/// exact range [`driver_compare_monitors`] compares (`io.c:7142-7147`) and `driver_monitor_process`
/// writes (`erts_ref_to_driver_monitor`, `io.c:6998-6999`); `None` for a `NULL` pointer.
///
/// This is the one read a `#![deny(unsafe_code)]` runtime cannot spell itself: the monitor hooks of
/// [`DriverHost`](crate::driver::host::DriverHost) receive a raw `*const ErlDrvMonitor`, and a host
/// (the sibling `beam_rt`/`beam_io`) must key its per-port monitor registry on those bytes without
/// writing `unsafe`. The bytes are **opaque** — they are the emulator's internal reference thing,
/// never an encoding this crate invents — so the identity is exposed verbatim and the last
/// `size_of::<ErlDrvMonitor>() - 24` bytes of the holder (which the C never writes) are excluded.
///
/// The pointer contract is the caller's, exactly as for the other raw-pointer ABI wrappers in this
/// crate: a non-null `mon` must point at a readable `ErlDrvMonitor`, and the returned array borrows
/// nothing — it is a copy, so the caller may drop the monitor immediately.
///
/// The counterpart for the write side is [`monitor_from_identity`].
//
// The pointer-validity contract is the caller's, exactly as for the other raw-pointer ABI wrappers
// of this crate (`crate::driver::loader` allows the same lint with the same rationale): a `NULL`
// `mon` is accepted and answered with `None`, while a non-null operand must point at a live
// `ErlDrvMonitor` — the same operand the C's `memcmp`/`sys_memcpy` dereferences (`io.c:6998`,
// `io.c:7142`). The whole point of this accessor is that a `#![deny(unsafe_code)]` host
// (`beam_rt`/`beam_io`) can read the identity without writing `unsafe`, so it must stay a safe
// `pub fn`; the lint is allowed instead of marking it `unsafe fn`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[must_use]
pub fn monitor_identity(mon: *const ErlDrvMonitor) -> Option<[u8; REF_THING_COMPARE_BYTES]> {
    if mon.is_null() {
        // The C has no failure value here (it dereferences the operand); `None` is this safe
        // accessor's documented answer for a missing monitor, never a fabricated identity.
        return None;
    }
    // SAFETY: `mon` is non-null and points at a readable `ErlDrvMonitor` (caller contract).
    Some(unsafe { identity_bytes(mon) })
}

/// The counterpart of [`monitor_identity`]: build a monitor whose first [`REF_THING_COMPARE_BYTES`]
/// (24) bytes are `identity` and whose trailing `size_of::<ErlDrvMonitor>() - 24` bytes are zero.
///
/// The C's write path (`erts_ref_to_driver_monitor`, `io.c:6998-6999`) `sys_memcpy`s exactly the
/// reference thing into the caller's holder and leaves the trailing bytes **untouched**; a fresh
/// caller buffer is zero there. Zeroing them here is the deterministic spelling of that fact (a
/// `compare`/`identity` round-trip is unaffected either way — see [`monitor_identity`]).
///
/// The bytes are opaque: this constructor neither derives nor validates them, it only places them
/// where the emulator places them. It exists so a `DriverHost` can produce the holder
/// `driver_monitor_process` hands back (`DriverHost::monitor_process`) from an identity it chose,
/// without writing `unsafe`.
#[must_use]
pub const fn monitor_from_identity(identity: [u8; REF_THING_COMPARE_BYTES]) -> ErlDrvMonitor {
    let mut data = [0u8; size_of::<ErlDrvMonitor>()];
    let mut i = 0;
    while i < REF_THING_COMPARE_BYTES {
        data[i] = identity[i];
        i += 1;
    }
    ErlDrvMonitor { data }
}

/// The first [`REF_THING_COMPARE_BYTES`] bytes of `mon` — a monitor's internal reference thing.
///
/// # Safety
///
/// `mon` must be non-null and point at a readable `ErlDrvMonitor`.
unsafe fn identity_bytes(mon: *const ErlDrvMonitor) -> [u8; REF_THING_COMPARE_BYTES] {
    let mut identity = [0u8; REF_THING_COMPARE_BYTES];
    // SAFETY: `mon` is non-null and readable for the whole holder (contract); the reference thing is
    // its first `REF_THING_COMPARE_BYTES` bytes (`io.c:6996`), well inside the 32-byte payload, so
    // this reads `REF_THING_COMPARE_BYTES` initialized bytes from the start of the struct.
    unsafe {
        core::ptr::copy_nonoverlapping(
            mon as *const u8,
            identity.as_mut_ptr(),
            REF_THING_COMPARE_BYTES,
        );
    }
    identity
}

/// The `sys_memcmp` of two live monitors' reference things, as **unsigned** byte comparison
/// (`< 0`, `0`, `> 0` — `memcmp`'s contract).
///
/// # Safety
///
/// Both pointers must be non-null and point at `REF_THING_COMPARE_BYTES` readable bytes.
unsafe fn compare_monitor_payloads(
    monitor1: *const ErlDrvMonitor,
    monitor2: *const ErlDrvMonitor,
) -> c_int {
    // SAFETY: both pointers are non-null and readable for `REF_THING_COMPARE_BYTES` (contract).
    let (a, b) = unsafe { (identity_bytes(monitor1), identity_bytes(monitor2)) };
    for i in 0..REF_THING_COMPARE_BYTES {
        if a[i] != b[i] {
            return c_int::from(a[i]) - c_int::from(b[i]);
        }
    }
    0
}

// ---------------------------------------------------------------------------------------------
// Deprecated wall clock
// ---------------------------------------------------------------------------------------------

/// `int driver_get_now(ErlDrvNowData *now);` — **deprecated** (`ERL_DRV_DEPRECATED_FUNC`) wall clock
/// in the three-field `megasecs`/`secs`/`microsecs` breakdown (`io.c:6957`).
///
/// Returns `-1` for a `NULL` out-parameter or an unreadable clock, else `0` with `*now` filled from
/// `clock_gettime(CLOCK_REALTIME)`.
///
/// # Safety
///
/// `now_data` must be null or point at a writable `ErlDrvNowData`.
#[no_mangle]
pub unsafe extern "C" fn driver_get_now(now_data: *mut ErlDrvNowData) -> c_int {
    if now_data.is_null() {
        return -1;
    }
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live, writable `timespec` and `CLOCK_REALTIME` is a valid clock id.
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) } != 0 {
        return -1;
    }
    let (secs, nsec) = (ts.tv_sec, ts.tv_nsec);
    if secs < 0 || nsec < 0 {
        // Before the epoch / out of range: no breakdown to report.
        return -1;
    }
    // SAFETY: `now_data` is non-null and writable (checked above).
    unsafe {
        (*now_data).megasecs = (secs as u64 / 1_000_000) as c_ulong;
        (*now_data).secs = (secs as u64 % 1_000_000) as c_ulong;
        (*now_data).microsecs = (nsec as u64 / 1_000) as c_ulong;
    }
    0
}

// ---------------------------------------------------------------------------------------------
// Table assembly (the `populate` chain step owned by this file)
// ---------------------------------------------------------------------------------------------

/// Populate this family's slots of the frozen [`ErlDrvApi`] (the `populate` chain in [`super`]
/// calls this). The binary refcount trio belongs to [`super::mem`] — see the module docs.
pub(super) const fn populate(mut api: ErlDrvApi) -> ErlDrvApi {
    api.driver_pdl_create = Some(driver_pdl_create);
    api.driver_pdl_lock = Some(driver_pdl_lock);
    api.driver_pdl_unlock = Some(driver_pdl_unlock);
    api.driver_pdl_get_refc = Some(driver_pdl_get_refc);
    api.driver_pdl_inc_refc = Some(driver_pdl_inc_refc);
    api.driver_pdl_dec_refc = Some(driver_pdl_dec_refc);
    api.driver_monitor_process = Some(driver_monitor_process);
    api.driver_demonitor_process = Some(driver_demonitor_process);
    api.driver_get_monitored_process = Some(driver_get_monitored_process);
    api.driver_compare_monitors = Some(driver_compare_monitors);
    api.driver_get_now = Some(driver_get_now);
    api
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use core::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::{make_atom, make_internal_pid, make_small, NIL};

    use crate::driver::host::{DriverEnv, DriverHost};

    /// The opaque `ErlDrvMonitor` payload size (`sizeof(void *) * 4`).
    const MONITOR_BYTES: usize = size_of::<*mut c_void>() * 4;

    /// A monitor whose payload encodes `id` in its first word (zero elsewhere).
    fn monitor(id: u64) -> ErlDrvMonitor {
        let mut data = [0u8; MONITOR_BYTES];
        data[..8].copy_from_slice(&id.to_ne_bytes());
        ErlDrvMonitor { data }
    }

    /// Does the monitor at `mon` hold `expected`'s payload?
    fn is_monitor(mon: *const ErlDrvMonitor, expected: &ErlDrvMonitor) -> bool {
        if mon.is_null() {
            return false;
        }
        // SAFETY: a `DriverHost` monitor hook is only ever handed a live `ErlDrvMonitor`.
        let bytes = unsafe { core::slice::from_raw_parts(mon as *const u8, MONITOR_BYTES) };
        bytes == &expected.data[..]
    }

    /// How [`TestHost`] answers the three-way monitor hook — the three C outcomes of
    /// `driver_monitor_process` (`io.c:7012-7038`).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum MonitorMode {
        /// Arm for real (the C's `0`): the host installs and records a monitor.
        Arm,
        /// The process is not alive (the C's `1`, `ERL_DRV_MONITOR_NOT_ALIVE`).
        NotAlive,
        /// The host cannot monitor (the C's `-1`, `ERL_DRV_MONITOR_ERROR`).
        Error,
    }

    /// A family-local [`DriverHost`]: a real arena/atom table, one PDL slot (the C's
    /// `pp->port_data_lock`), a monitor table, and a settable caller pid.
    struct TestHost {
        heap: Heap,
        atoms: AtomTable,
        pdls: Vec<Box<u64>>,
        pdl: Option<ErlDrvPDL>,
        /// Return the already-created handle again instead of refusing (to exercise the registry's
        /// own duplicate-handle guard).
        reuse_handle: bool,
        monitors: Vec<(ErlDrvMonitor, Eterm)>,
        next_monitor: u64,
        /// The outcome [`TestHost`]'s three-way monitor hook reports.
        monitor_mode: MonitorMode,
    }

    impl TestHost {
        fn new() -> TestHost {
            TestHost {
                heap: Heap::new(4096).expect("a test heap is allocatable"),
                atoms: AtomTable::new(),
                pdls: Vec::new(),
                pdl: None,
                reuse_handle: false,
                monitors: Vec::new(),
                next_monitor: 1,
                monitor_mode: MonitorMode::Arm,
            }
        }

        /// The internal pid the monitor tests use.
        fn pid() -> Eterm {
            make_internal_pid(1)
        }
    }

    impl DriverHost for TestHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(make_atom(self.atoms.intern(name)))
        }

        fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
            0
        }

        fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
            0
        }

        /// One PDL per port, like the C's `pp->port_data_lock`; the handle is a live boxed word
        /// (this crate uses only its address, never its contents).
        fn pdl_create(&mut self) -> Option<ErlDrvPDL> {
            if let Some(existing) = self.pdl {
                return if self.reuse_handle { Some(existing) } else { None };
            }
            let boxed = Box::new(0u64);
            let handle = (&*boxed) as *const u64 as *mut u64 as ErlDrvPDL;
            self.pdls.push(boxed);
            self.pdl = Some(handle);
            Some(handle)
        }

        fn monitor_process(&mut self, pid: Eterm) -> Option<ErlDrvMonitor> {
            let mon = monitor(self.next_monitor);
            self.next_monitor += 1;
            self.monitors.push((mon, pid));
            Some(mon)
        }

        /// The three-way hook: [`MonitorMode::Arm`] arms through the two-way
        /// [`monitor_process`](DriverHost::monitor_process), the other two report the C's positive
        /// "not alive" (`1`) and negative failure (`-1`) codes directly.
        fn monitor_process_result(&mut self, pid: Eterm) -> MonitorResult {
            match self.monitor_mode {
                MonitorMode::Arm => match self.monitor_process(pid) {
                    Some(mon) => MonitorResult::Ok(mon),
                    None => MonitorResult::Error,
                },
                MonitorMode::NotAlive => MonitorResult::NotAlive,
                MonitorMode::Error => MonitorResult::Error,
            }
        }
        fn demonitor_process(&mut self, mon: *const ErlDrvMonitor) -> c_int {
            match self.monitors.iter().position(|(m, _)| is_monitor(mon, m)) {
                Some(i) => {
                    self.monitors.remove(i);
                    ERL_DRV_MONITOR_OK
                }
                None => ERL_DRV_MONITOR_NOT_ALIVE,
            }
        }

        fn get_monitored_process(&mut self, mon: *const ErlDrvMonitor) -> Option<Eterm> {
            self.monitors
                .iter()
                .find(|(m, _)| is_monitor(mon, m))
                .map(|(_, pid)| *pid)
        }
    }

    /// Move a PDL's reference count to zero so its registry entry is released, keeping the tests
    /// independent of each other (they share the process-wide registry).
    fn release_pdl(pdl: ErlDrvPDL) {
        // SAFETY: `pdl` came from `driver_pdl_create` on a live test host.
        while unsafe { driver_pdl_dec_refc(pdl) } > 0 {}
    }

    #[test]
    fn pdl_refcount_follows_the_c_sequence() {
        let mut host = TestHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` came from `env.as_port()` on this live env.
        let pdl = unsafe { driver_pdl_create(port) };
        assert!(!pdl.is_null(), "a valid port admits a PDL");

        // A fresh PDL has exactly one reference (`pdl_init_refc`, io.c:6726).
        // SAFETY: `pdl` came from `driver_pdl_create`.
        assert_eq!(unsafe { driver_pdl_get_refc(pdl) }, 1);

        // `lock` raises the count before locking, `unlock` lowers it after unlocking (io.c:6736).
        // SAFETY: `pdl` came from `driver_pdl_create`.
        unsafe { driver_pdl_lock(pdl) };
        // SAFETY: as above.
        assert_eq!(unsafe { driver_pdl_get_refc(pdl) }, 2);
        // SAFETY: `pdl` is locked by this thread.
        unsafe { driver_pdl_unlock(pdl) };
        // SAFETY: as above.
        assert_eq!(unsafe { driver_pdl_get_refc(pdl) }, 1);

        // `inc_refc`/`dec_refc` report the value reached (`pdl_inctest_refc`/`pdl_dectest_refc`).
        // SAFETY: `pdl` came from `driver_pdl_create`.
        assert_eq!(unsafe { driver_pdl_inc_refc(pdl) }, 2);
        // SAFETY: as above.
        assert_eq!(unsafe { driver_pdl_dec_refc(pdl) }, 1);

        // A count of zero releases the PDL: the handle becomes foreign.
        // SAFETY: as above.
        assert_eq!(unsafe { driver_pdl_dec_refc(pdl) }, 0);
        // SAFETY: as above — the handle is now unknown, which must not crash.
        assert_eq!(unsafe { driver_pdl_get_refc(pdl) }, PDL_INVALID_REFCOUNT);
    }

    #[test]
    fn pdl_create_admits_one_pdl_per_port_and_rejects_a_duplicate_handle() {
        let mut host = TestHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` came from `env.as_port()` on this live env.
        let first = unsafe { driver_pdl_create(port) };
        assert!(!first.is_null());

        // A host that hands back the *same* handle again is refused by the registry (the C's
        // `pp->port_data_lock` check).
        host.reuse_handle = true;
        // SAFETY: as above.
        assert!(unsafe { driver_pdl_create(port) }.is_null(), "one PDL per port");

        // With the host applying the rule itself, the second create is refused there too.
        host.reuse_handle = false;
        // SAFETY: as above.
        assert!(unsafe { driver_pdl_create(port) }.is_null());

        // SAFETY: `first` came from `driver_pdl_create`.
        release_pdl(first);
        // SAFETY: as above — the host still reports the port as having a PDL.
        assert!(unsafe { driver_pdl_create(port) }.is_null());

        // An operand that names no port — `NULL` here, or the C's invalid-port sentinel
        // (`ERTS_INVALID_ERL_DRV_PORT`, the C's own early return) — creates nothing.
        // SAFETY: a null port handle is explicitly allowed.
        assert!(unsafe { driver_pdl_create(core::ptr::null_mut()) }.is_null());
    }

    #[test]
    fn a_pdl_rejects_null_and_foreign_handles_without_crashing() {
        // A handle this crate never issued.
        let foreign = 0xdead_beefusize as ErlDrvPDL;

        // SAFETY: every call below is documented as "null or a handle from `driver_pdl_create`";
        // the point of the test is that a foreign handle is refused rather than dereferenced.
        unsafe {
            assert_eq!(driver_pdl_get_refc(core::ptr::null_mut()), PDL_INVALID_REFCOUNT);
            assert_eq!(driver_pdl_get_refc(foreign), PDL_INVALID_REFCOUNT);
            assert_eq!(driver_pdl_inc_refc(foreign), PDL_INVALID_REFCOUNT);
            assert_eq!(driver_pdl_dec_refc(foreign), PDL_INVALID_REFCOUNT);
            // `lock`/`unlock` return nothing: they must simply do nothing.
            driver_pdl_lock(core::ptr::null_mut());
            driver_pdl_unlock(core::ptr::null_mut());
            driver_pdl_lock(foreign);
            driver_pdl_unlock(foreign);
        }

        // The refusal of `foreign` must not have registered any state.
        assert!(state_for(foreign).is_none());
    }

    #[test]
    fn a_pdl_serialises_two_contending_threads() {
        let mut host = TestHost::new();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // SAFETY: `port` came from `env.as_port()` on this live env.
        let pdl = unsafe { driver_pdl_create(port) };
        assert!(!pdl.is_null());
        // A raw pointer is not `Send`; the address is all the workers need.
        let key = pdl as usize;

        // The probe: how many threads are inside the critical section at once. It must never
        // exceed one if the PDL is a real lock.
        let inside = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));

        let mut workers = Vec::new();
        for _ in 0..2 {
            let inside = Arc::clone(&inside);
            let most = Arc::clone(&most);
            workers.push(thread::spawn(move || {
                let pdl = key as ErlDrvPDL;
                for _ in 0..20_000 {
                    // SAFETY: `key` is the handle the test thread registered above; only its
                    // address is used, and the registry keeps its state alive for the whole run.
                    unsafe { driver_pdl_lock(pdl) };
                    let now = inside.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                    most.fetch_max(now, AtomicOrdering::SeqCst);
                    thread::yield_now();
                    inside.fetch_sub(1, AtomicOrdering::SeqCst);
                    // SAFETY: as above — this thread took the lock just above.
                    unsafe { driver_pdl_unlock(pdl) };
                }
            }));
        }
        for worker in workers {
            worker.join().expect("a PDL worker must not panic");
        }

        assert_eq!(
            most.load(AtomicOrdering::SeqCst),
            1,
            "the port data lock must be real mutual exclusion between threads"
        );
        // Every lock was matched by an unlock, so the count is back to one.
        // SAFETY: `pdl` came from `driver_pdl_create` and is no longer contended.
        assert_eq!(unsafe { driver_pdl_get_refc(pdl) }, 1);
        // SAFETY: as above.
        release_pdl(pdl);
        // The host (and hence the handle's storage) outlives every use above.
        assert!(host.pdl.is_some());
    }

    #[test]
    fn driver_monitor_process_registers_and_reports_the_c_result_codes() {
        let mut host = TestHost::new();
        let pid = TestHost::pid();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let mut mon = monitor(0);
        let mut other = monitor(0);

        // Success: the host fills the monitor, the crate returns 0.
        // SAFETY: `port` is live and `mon` is writable.
        assert_eq!(
            unsafe { driver_monitor_process(port, pid.raw() as ErlDrvTermData, &mut mon) },
            ERL_DRV_MONITOR_OK
        );

        // An operand that cannot name a process (here the empty list, then a small integer) is the
        // positive "not alive" code — the C would hand it to the signal machinery.
        // SAFETY: `port` is live and `other` is writable.
        assert_eq!(
            unsafe { driver_monitor_process(port, NIL.raw() as ErlDrvTermData, &mut other) },
            ERL_DRV_MONITOR_NOT_ALIVE
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { driver_monitor_process(port, make_small(7).raw() as ErlDrvTermData, &mut other) },
            ERL_DRV_MONITOR_NOT_ALIVE
        );

        // Invalid port / null out-parameter are the C's `-1` outcomes.
        // SAFETY: a null port and a null monitor are explicitly allowed.
        unsafe {
            assert_eq!(
                driver_monitor_process(core::ptr::null_mut(), pid.raw() as ErlDrvTermData, &mut other),
                ERL_DRV_MONITOR_ERROR
            );
            assert_eq!(
                driver_monitor_process(port, pid.raw() as ErlDrvTermData, core::ptr::null_mut()),
                ERL_DRV_MONITOR_ERROR
            );
        }
    }

    /// The three C outcomes of `driver_monitor_process` are all reachable through the three-way
    /// [`DriverHost::monitor_process_result`] hook (`io.c:7012-7038`, `do_driver_monitor_process`):
    /// `0` armed with the holder filled, `1` not alive, `-1` failure. The C distinguishes them by
    /// **sign**, which is precisely why the earlier two-way `Option` hook could not express the
    /// middle one (`None` only ever meant `-1`).
    #[test]
    fn the_three_way_monitor_result_maps_onto_the_c_reason_codes() {
        let mut host = TestHost::new();
        let pid = TestHost::pid();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();

        // --- `MonitorResult::Ok` -> the C's `0`, holder filled (`io.c:7037`) ---------------------
        host.monitor_mode = MonitorMode::Arm;
        let mut mon = monitor(0);
        // SAFETY: `port` is live and `mon` is writable.
        assert_eq!(
            unsafe { driver_monitor_process(port, pid.raw() as ErlDrvTermData, &mut mon) },
            ERL_DRV_MONITOR_OK
        );
        assert!(
            is_monitor(&mon, &monitor(1)),
            "the armed path must write the host's monitor into the holder"
        );

        // --- `MonitorResult::NotAlive` -> the C's positive `1` (`io.c:7031`) ---------------------
        host.monitor_mode = MonitorMode::NotAlive;
        let mut not_mon = monitor(0);
        // SAFETY: `port` is live and `not_mon` is writable.
        assert_eq!(
            unsafe { driver_monitor_process(port, pid.raw() as ErlDrvTermData, &mut not_mon) },
            ERL_DRV_MONITOR_NOT_ALIVE
        );
        assert!(
            !is_monitor(&not_mon, &monitor(1)),
            "a not-alive result must not write the holder"
        );

        // --- `MonitorResult::Error` -> the C's negative `-1` (`io.c:7020-7021`) ------------------
        host.monitor_mode = MonitorMode::Error;
        let mut err_mon = monitor(0);
        // SAFETY: `port` is live and `err_mon` is writable.
        assert_eq!(
            unsafe { driver_monitor_process(port, pid.raw() as ErlDrvTermData, &mut err_mon) },
            ERL_DRV_MONITOR_ERROR
        );
        assert!(
            !is_monitor(&err_mon, &monitor(1)),
            "a failed arm must not write the holder"
        );

        // Only the armed case reached the host's two-way hook, so exactly one monitor is registered.
        assert_eq!(host.monitors.len(), 1, "only the armed case installs a monitor");
    }

    #[test]
    fn driver_get_monitored_process_and_demonitor_round_trip() {
        let mut host = TestHost::new();
        let pid = TestHost::pid();
        let mut env = DriverEnv::new(&mut host);
        let port = env.as_port();
        let mut mon = monitor(0);

        // SAFETY: `port` is live and `mon` is writable.
        assert_eq!(
            unsafe { driver_monitor_process(port, pid.raw() as ErlDrvTermData, &mut mon) },
            ERL_DRV_MONITOR_OK
        );

        // The monitor reports its process …
        // SAFETY: `port` is live and `mon` holds a monitor this host issued.
        assert_eq!(unsafe { driver_get_monitored_process(port, &mon) }, pid.raw() as ErlDrvTermData);
        // … an unknown monitor and an invalid port report `driver_term_nil`.
        let unknown = monitor(9999);
        // SAFETY: `port` is live; `unknown` is a readable monitor this host never issued.
        assert_eq!(unsafe { driver_get_monitored_process(port, &unknown) }, driver_term_nil);
        // SAFETY: `unknown` is readable; the port is null.
        assert_eq!(
            unsafe { driver_get_monitored_process(core::ptr::null_mut(), &unknown) },
            driver_term_nil
        );
        // SAFETY: a null monitor is explicitly allowed.
        assert_eq!(unsafe { driver_get_monitored_process(port, core::ptr::null()) }, driver_term_nil);

        // `demonitor` removes it once, then reports "no longer exists".
        // SAFETY: `port` is live and `mon` holds a monitor this host issued.
        assert_eq!(unsafe { driver_demonitor_process(port, &mon) }, ERL_DRV_MONITOR_OK);
        // SAFETY: as above — the monitor is gone now.
        assert_eq!(unsafe { driver_demonitor_process(port, &mon) }, ERL_DRV_MONITOR_NOT_ALIVE);
        // SAFETY: a null monitor / null port are the C's `-1`.
        unsafe {
            assert_eq!(driver_demonitor_process(port, core::ptr::null()), ERL_DRV_MONITOR_ERROR);
            assert_eq!(driver_demonitor_process(core::ptr::null_mut(), &mon), ERL_DRV_MONITOR_ERROR);
        }
    }

    #[test]
    fn driver_compare_monitors_is_a_pure_payload_comparison() {
        let a = monitor(1);
        let b = monitor(1);
        let c = monitor(2);

        // SAFETY: all three are live `ErlDrvMonitor` values.
        unsafe {
            assert_eq!(driver_compare_monitors(&a, &b), 0, "identical payloads compare equal");
            assert!(driver_compare_monitors(&a, &c) < 0, "the smaller first word sorts first");
            assert!(driver_compare_monitors(&c, &a) > 0, "…and the other way round");
            // Only the first `ERTS_REF_THING_SIZE * sizeof(Eterm)` bytes are consulted.
            let mut wide = a;
            wide.data[REF_THING_COMPARE_BYTES] = 0xff;
            assert_eq!(
                driver_compare_monitors(&a, &wide),
                0,
                "bytes past the reference thing are ignored"
            );
            // Null operands are ordered rather than dereferenced (the C would fault).
            assert_eq!(driver_compare_monitors(core::ptr::null(), core::ptr::null()), 0);
            assert!(driver_compare_monitors(core::ptr::null(), &a) < 0);
            assert!(driver_compare_monitors(&a, core::ptr::null()) > 0);
        }
        assert_eq!(REF_THING_COMPARE_BYTES, 24, "ERTS_REF_THING_SIZE(3) * sizeof(Eterm)(8)");
    }

    #[test]
    fn monitor_identity_reads_the_24_byte_reference_thing_and_ignores_the_tail() {
        // A missing monitor has no identity (the C would fault; `None` is the safe accessor's answer).
        assert_eq!(monitor_identity(core::ptr::null()), None);

        // A monitor whose first 24 bytes are set returns exactly those bytes.
        let mut mon = ErlDrvMonitor { data: [0u8; MONITOR_BYTES] };
        for (i, byte) in mon.data[..REF_THING_COMPARE_BYTES].iter_mut().enumerate() {
            *byte = (i as u8) + 1;
        }
        let identity =
            monitor_identity(&mon as *const ErlDrvMonitor).expect("a live monitor has one");
        assert_eq!(identity.len(), REF_THING_COMPARE_BYTES);
        assert_eq!(identity[..], mon.data[..REF_THING_COMPARE_BYTES]);

        // The trailing 8 bytes are not part of the identity: changing them changes nothing.
        let mut wide = mon;
        wide.data[REF_THING_COMPARE_BYTES..].fill(0xff);
        assert_eq!(monitor_identity(&wide as *const ErlDrvMonitor), Some(identity));
    }

    #[test]
    fn monitor_from_identity_is_the_write_side_counterpart() {
        let mut identity = [0u8; REF_THING_COMPARE_BYTES];
        for (i, byte) in identity.iter_mut().enumerate() {
            *byte = 0xa0u8.wrapping_add(i as u8);
        }
        let mon = monitor_from_identity(identity);
        assert_eq!(mon.data[..REF_THING_COMPARE_BYTES], identity[..]);
        assert_eq!(
            &mon.data[REF_THING_COMPARE_BYTES..],
            &[0u8; MONITOR_BYTES - REF_THING_COMPARE_BYTES],
            "the bytes the C never writes are zeroed"
        );
        // The round-trip through the safe accessor is the identity it was built from.
        assert_eq!(monitor_identity(&mon as *const ErlDrvMonitor), Some(identity));
    }

    #[test]
    fn monitors_agreeing_on_the_reference_thing_compare_equal_whatever_the_tail() {
        let a = monitor_from_identity([0x5au8; REF_THING_COMPARE_BYTES]);
        // Same identity, different trailing bytes: still equal under the pure 24-byte `memcmp`.
        let mut b = a;
        b.data[REF_THING_COMPARE_BYTES..].fill(0xff);
        // SAFETY: both are live `ErlDrvMonitor` values.
        assert_eq!(
            unsafe { driver_compare_monitors(&a, &b) },
            0,
            "only the first 24 bytes may be compared"
        );

        // One differing identity byte breaks both the comparison and the identity itself.
        let mut c = b;
        c.data[REF_THING_COMPARE_BYTES - 1] ^= 0x01;
        // SAFETY: `c` is a live `ErlDrvMonitor` value.
        assert_ne!(unsafe { driver_compare_monitors(&a, &c) }, 0);
        assert_ne!(
            monitor_identity(&c as *const ErlDrvMonitor),
            monitor_identity(&a as *const ErlDrvMonitor)
        );
    }
    #[test]
    fn driver_get_now_fills_the_three_field_breakdown() {
        let mut now = ErlDrvNowData::default();

        // A null out-parameter is the C's `-1` (`io.c:6962`).
        // SAFETY: a null out-parameter is explicitly allowed.
        assert_eq!(unsafe { driver_get_now(core::ptr::null_mut()) }, -1);

        // SAFETY: `now` is a live, writable `ErlDrvNowData`.
        assert_eq!(unsafe { driver_get_now(&mut now) }, 0);
        assert!(now.secs < 1_000_000, "secs is a within-megasecond remainder: {}", now.secs);
        assert!(
            now.microsecs < 1_000_000,
            "microsecs is a within-second remainder: {}",
            now.microsecs
        );

        // The split must reconstruct a wall clock close to `SystemTime::now()` (this crate's epoch
        // is the Unix epoch; the C's is only documented as "arbitrary").
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is after the epoch")
            .as_secs();
        let total = now.megasecs as u64 * 1_000_000 + now.secs as u64;
        assert!(
            total + 2 >= unix && total <= unix + 2,
            "the breakdown ({total}s) must be within 2s of SystemTime ({unix}s)"
        );

        // A second read must not go backwards.
        let mut later = ErlDrvNowData::default();
        // SAFETY: `later` is a live, writable `ErlDrvNowData`.
        assert_eq!(unsafe { driver_get_now(&mut later) }, 0);
        let total_later = later.megasecs as u64 * 1_000_000 + later.secs as u64;
        assert!(total_later >= total);
    }

    #[test]
    fn the_family_slots_are_wired_into_the_frozen_table() {
        let table = super::super::driver_api_table();
        for (name, populated) in [
            ("driver_pdl_create", table.driver_pdl_create.is_some()),
            ("driver_pdl_lock", table.driver_pdl_lock.is_some()),
            ("driver_pdl_unlock", table.driver_pdl_unlock.is_some()),
            ("driver_pdl_get_refc", table.driver_pdl_get_refc.is_some()),
            ("driver_pdl_inc_refc", table.driver_pdl_inc_refc.is_some()),
            ("driver_pdl_dec_refc", table.driver_pdl_dec_refc.is_some()),
            ("driver_monitor_process", table.driver_monitor_process.is_some()),
            ("driver_demonitor_process", table.driver_demonitor_process.is_some()),
            ("driver_get_monitored_process", table.driver_get_monitored_process.is_some()),
            ("driver_compare_monitors", table.driver_compare_monitors.is_some()),
            ("driver_get_now", table.driver_get_now.is_some()),
        ] {
            assert!(populated, "{name} must be populated");
        }
        // The binary refcount trio is owned by `mem`, which populates it from the same frozen
        // table; assert it here too, so this family's test notices if the chain ever drops it.
        assert!(table.driver_binary_get_refc.is_some());
        assert!(table.driver_binary_inc_refc.is_some());
        assert!(table.driver_binary_dec_refc.is_some());
    }
}
