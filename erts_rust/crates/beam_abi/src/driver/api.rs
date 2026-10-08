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

//! `ErlDrvApi` — the frozen `driver_*` / `erl_drv_*` function table of the driver C ABI.
//!
//! This table declares every function the **runtime provides to drivers** by signature, in the
//! exact order of `erts/emulator/beam/erl_driver.h` (from the first `EXTERN` of the
//! `!ERL_DRIVER_TYPES_ONLY` block at the `erl_drv_busy_msgq_limits` declaration through
//! `erl_drv_set_os_pid`). Together with [`crate::driver::types`] it is the driver half of
//! compatibility contract **A.4** (driver C ABI) and the link-time frozen-header seam of
//! `ERTS_RUST_REWRITE_DEEPDIVE.md` §C.
//!
//! # Exported free functions vs. one table (documented deviation)
//!
//! The legacy header exposes these as *exported free functions* (`EXTERN`), which on Unix a driver
//! resolves at load time via `dlsym` — the header only wraps them into a callback struct
//! (`TWinDynDriverCallbacks`) on Windows. Modelling them as a single `#[repr(C)]` function-pointer
//! table is how the Rust runtime will **export/consume** the surface, and it makes the frozen
//! signature list explicit and compile-checked. This is the crate's documented deviation (see the
//! `beam_abi` `CONTEXT.md`, Design Decisions, and the identical choice for `ErlNifApi`); it does
//! not change any individual function's ABI.
//!
//! The declared pointers are **never called** in this crate — the functions they name are
//! implemented by the driver subsystem (`beam_io` / `beam_rt`) in a later pass.
//!
//! # Length
//!
//! This file is a legitimately long, **generated-style table**: one documented field per header
//! declaration (~110 fields), mirroring the header's own order. It is deliberately not split.
//!
//! Every function-pointer field is `Option<unsafe extern "C" fn …>` (null-pointer optimized, same
//! size/align as a raw pointer) — see the crate `CONTEXT.md`. `__DARWIN__`-only declarations
//! (`erl_drv_stolen_main_thread_join`, `erl_drv_steal_main_thread`) are **not** part of the frozen
//! surface on the supported LP64 Linux/AArch64 targets and are intentionally omitted.

use crate::common::SysIOVec;
use crate::driver::types::{
    ErlDrvBinary, ErlDrvCond, ErlDrvData, ErlDrvEntry, ErlDrvEvent, ErlDrvMonitor, ErlDrvMutex,
    ErlDrvNowData, ErlDrvPDL, ErlDrvPort, ErlDrvRWLock, ErlDrvSInt, ErlDrvSizeT, ErlDrvSysInfo,
    ErlDrvTermData, ErlDrvThreadOpts, ErlDrvTid, ErlDrvTime, ErlDrvTimeUnit, ErlDrvTSDKey, ErlIOVec,
};
use libc::{c_char, c_int, c_uint, c_ulong, c_void, size_t};

// ---------------------------------------------------------------------------------------------
// Queue / port / output control
// ---------------------------------------------------------------------------------------------

/// `ErlDrvApi` — the frozen table of `driver_*` / `erl_drv_*` functions the runtime provides.
///
/// One field per declaration in `erl_driver.h`, in header order; every field is
/// `Option<unsafe extern "C" fn …>` (the C function pointer, null-optimized). The field count is
/// pinned by the compile-time assertion at the bottom of the file (`N == 111`, i.e. 888 bytes on
/// LP64).
///
/// Use [`ErlDrvApi::empty`] to build an all-`None` table (e.g. for tests or for a runtime that has
/// not yet wired up a function).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ErlDrvApi {
    /// `void erl_drv_busy_msgq_limits(ErlDrvPort port, ErlDrvSizeT *low, ErlDrvSizeT *high);` —
    /// query the busy message-queue limits of `port`.
    pub erl_drv_busy_msgq_limits:
        Option<unsafe extern "C" fn(ErlDrvPort, *mut ErlDrvSizeT, *mut ErlDrvSizeT)>,
    /// `int driver_select(ErlDrvPort port, ErlDrvEvent event, int mode, int on);` — register /
    /// unregister interest in an event handle (`ERL_DRV_READ`/`WRITE`/`USE`).
    pub driver_select: Option<unsafe extern "C" fn(ErlDrvPort, ErlDrvEvent, c_int, c_int) -> c_int>,
    /// `int driver_output(ErlDrvPort port, char *buf, ErlDrvSizeT len);` — send data to the port
    /// owner.
    pub driver_output: Option<unsafe extern "C" fn(ErlDrvPort, *mut c_char, ErlDrvSizeT) -> c_int>,
    /// `int driver_output2(ErlDrvPort port, char *hbuf, ErlDrvSizeT hlen, char *buf, ErlDrvSizeT
    /// len);` — send a header followed by body data in one message.
    pub driver_output2:
        Option<unsafe extern "C" fn(ErlDrvPort, *mut c_char, ErlDrvSizeT, *mut c_char, ErlDrvSizeT) -> c_int>,
    /// `int driver_output_binary(ErlDrvPort port, char *hbuf, ErlDrvSizeT hlen, ErlDrvBinary *bin,
    /// ErlDrvSizeT offset, ErlDrvSizeT len);` — send a header plus a (sub-range of a) driver
    /// binary.
    pub driver_output_binary: Option<
        unsafe extern "C" fn(
            ErlDrvPort,
            *mut c_char,
            ErlDrvSizeT,
            *mut ErlDrvBinary,
            ErlDrvSizeT,
            ErlDrvSizeT,
        ) -> c_int,
    >,
    /// `int driver_outputv(ErlDrvPort port, char *hbuf, ErlDrvSizeT hlen, ErlIOVec *ev, ErlDrvSizeT
    /// skip);` — send a header plus a scatter/gather vector.
    pub driver_outputv: Option<
        unsafe extern "C" fn(ErlDrvPort, *mut c_char, ErlDrvSizeT, *mut ErlIOVec, ErlDrvSizeT) -> c_int,
    >,
    /// `ErlDrvSizeT driver_vec_to_buf(ErlIOVec *ev, char *buf, ErlDrvSizeT len);` — linearize a
    /// vector into `buf`, returning the number of bytes copied.
    pub driver_vec_to_buf:
        Option<unsafe extern "C" fn(*mut ErlIOVec, *mut c_char, ErlDrvSizeT) -> ErlDrvSizeT>,
    /// `int driver_set_timer(ErlDrvPort port, unsigned long time);` — schedule the `timeout`
    /// callback after `time` milliseconds.
    pub driver_set_timer: Option<unsafe extern "C" fn(ErlDrvPort, c_ulong) -> c_int>,
    /// `int driver_cancel_timer(ErlDrvPort port);` — cancel a pending timer.
    pub driver_cancel_timer: Option<unsafe extern "C" fn(ErlDrvPort) -> c_int>,
    /// `int driver_read_timer(ErlDrvPort port, unsigned long *time_left);` — read the time left on
    /// the port timer.
    pub driver_read_timer: Option<unsafe extern "C" fn(ErlDrvPort, *mut c_ulong) -> c_int>,
    /// `int erl_drv_consume_timeslice(ErlDrvPort port, int percent);` — inform the runtime about
    /// lengthy work done for the port's process.
    pub erl_drv_consume_timeslice: Option<unsafe extern "C" fn(ErlDrvPort, c_int) -> c_int>,
    /// `char *erl_errno_id(int error);` — the plain-text name of an errno value.
    pub erl_errno_id: Option<unsafe extern "C" fn(c_int) -> *mut c_char>,

    // -----------------------------------------------------------------------------------------
    // Initiating port close from the driver
    // -----------------------------------------------------------------------------------------

    /// `int driver_failure_eof(ErlDrvPort port);` — close the port with an eof reason.
    pub driver_failure_eof: Option<unsafe extern "C" fn(ErlDrvPort) -> c_int>,
    /// `int driver_failure_atom(ErlDrvPort port, char *string);` — close the port with the atom
    /// `string` as reason.
    pub driver_failure_atom: Option<unsafe extern "C" fn(ErlDrvPort, *mut c_char) -> c_int>,
    /// `int driver_failure_posix(ErlDrvPort port, int error);` — close the port with a POSIX errno
    /// reason.
    pub driver_failure_posix: Option<unsafe extern "C" fn(ErlDrvPort, c_int) -> c_int>,
    /// `int driver_failure(ErlDrvPort port, int error);` — close the port with an error tuple
    /// reason.
    pub driver_failure: Option<unsafe extern "C" fn(ErlDrvPort, c_int) -> c_int>,
    /// `int driver_exit(ErlDrvPort port, int err);` — immediately terminate the port's connected
    /// process.
    pub driver_exit: Option<unsafe extern "C" fn(ErlDrvPort, c_int) -> c_int>,

    // -----------------------------------------------------------------------------------------
    // Port data lock
    // -----------------------------------------------------------------------------------------

    /// `ErlDrvPDL driver_pdl_create(ErlDrvPort port);` — create a port data lock.
    pub driver_pdl_create: Option<unsafe extern "C" fn(ErlDrvPort) -> ErlDrvPDL>,
    /// `void driver_pdl_lock(ErlDrvPDL pdl);` — take the port data lock.
    pub driver_pdl_lock: Option<unsafe extern "C" fn(ErlDrvPDL)>,
    /// `void driver_pdl_unlock(ErlDrvPDL pdl);` — release the port data lock.
    pub driver_pdl_unlock: Option<unsafe extern "C" fn(ErlDrvPDL)>,
    /// `ErlDrvSInt driver_pdl_get_refc(ErlDrvPDL pdl);` — read the port data lock's reference
    /// count.
    pub driver_pdl_get_refc: Option<unsafe extern "C" fn(ErlDrvPDL) -> ErlDrvSInt>,
    /// `ErlDrvSInt driver_pdl_inc_refc(ErlDrvPDL pdl);` — increment the reference count, returning
    /// the new value.
    pub driver_pdl_inc_refc: Option<unsafe extern "C" fn(ErlDrvPDL) -> ErlDrvSInt>,
    /// `ErlDrvSInt driver_pdl_dec_refc(ErlDrvPDL pdl);` — decrement the reference count, returning
    /// the new value.
    pub driver_pdl_dec_refc: Option<unsafe extern "C" fn(ErlDrvPDL) -> ErlDrvSInt>,

    // -----------------------------------------------------------------------------------------
    // Process monitors
    // -----------------------------------------------------------------------------------------

    /// `int driver_monitor_process(ErlDrvPort port, ErlDrvTermData process, ErlDrvMonitor
    /// *monitor);` — monitor a process from the driver.
    pub driver_monitor_process:
        Option<unsafe extern "C" fn(ErlDrvPort, ErlDrvTermData, *mut ErlDrvMonitor) -> c_int>,
    /// `int driver_demonitor_process(ErlDrvPort port, const ErlDrvMonitor *monitor);` — cancel a
    /// driver monitor.
    pub driver_demonitor_process:
        Option<unsafe extern "C" fn(ErlDrvPort, *const ErlDrvMonitor) -> c_int>,
    /// `ErlDrvTermData driver_get_monitored_process(ErlDrvPort port, const ErlDrvMonitor
    /// *monitor);` — the monitored process's term data.
    pub driver_get_monitored_process:
        Option<unsafe extern "C" fn(ErlDrvPort, *const ErlDrvMonitor) -> ErlDrvTermData>,
    /// `int driver_compare_monitors(const ErlDrvMonitor *monitor1, const ErlDrvMonitor
    /// *monitor2);` — compare two driver monitors.
    pub driver_compare_monitors:
        Option<unsafe extern "C" fn(*const ErlDrvMonitor, *const ErlDrvMonitor) -> c_int>,

    // -----------------------------------------------------------------------------------------
    // Port attributes
    // -----------------------------------------------------------------------------------------

    /// `void set_busy_port(ErlDrvPort port, int on);` — set / clear the port's busy state.
    pub set_busy_port: Option<unsafe extern "C" fn(ErlDrvPort, c_int)>,
    /// `void set_port_control_flags(ErlDrvPort port, int flags);` — set the port control flags
    /// (`PORT_CONTROL_FLAG_*`).
    pub set_port_control_flags: Option<unsafe extern "C" fn(ErlDrvPort, c_int)>,
    /// `int get_port_flags(ErlDrvPort port);` — read the port flags (`PORT_FLAG_*`).
    pub get_port_flags: Option<unsafe extern "C" fn(ErlDrvPort) -> c_int>,

    // -----------------------------------------------------------------------------------------
    // Binary interface
    // -----------------------------------------------------------------------------------------

    /// `void driver_free_binary(ErlDrvBinary *bin);` — free a driver binary.
    pub driver_free_binary: Option<unsafe extern "C" fn(*mut ErlDrvBinary)>,
    /// `ErlDrvBinary *driver_alloc_binary(ErlDrvSizeT size);` — allocate a driver binary.
    pub driver_alloc_binary: Option<unsafe extern "C" fn(ErlDrvSizeT) -> *mut ErlDrvBinary>,
    /// `ErlDrvBinary *driver_realloc_binary(ErlDrvBinary *bin, ErlDrvSizeT size);` — resize a
    /// driver binary.
    pub driver_realloc_binary:
        Option<unsafe extern "C" fn(*mut ErlDrvBinary, ErlDrvSizeT) -> *mut ErlDrvBinary>,
    /// `ErlDrvSInt driver_binary_get_refc(ErlDrvBinary *dbp);` — read a binary's reference count.
    pub driver_binary_get_refc: Option<unsafe extern "C" fn(*mut ErlDrvBinary) -> ErlDrvSInt>,
    /// `ErlDrvSInt driver_binary_inc_refc(ErlDrvBinary *dbp);` — increment a binary's reference
    /// count, returning the new value.
    pub driver_binary_inc_refc: Option<unsafe extern "C" fn(*mut ErlDrvBinary) -> ErlDrvSInt>,
    /// `ErlDrvSInt driver_binary_dec_refc(ErlDrvBinary *dbp);` — decrement a binary's reference
    /// count, returning the new value.
    pub driver_binary_dec_refc: Option<unsafe extern "C" fn(*mut ErlDrvBinary) -> ErlDrvSInt>,

    // -----------------------------------------------------------------------------------------
    // Allocation interface
    // -----------------------------------------------------------------------------------------

    /// `void driver_free(void *ptr);` — free driver-allocated memory.
    pub driver_free: Option<unsafe extern "C" fn(*mut c_void)>,
    /// `void *driver_alloc(ErlDrvSizeT size);` — allocate driver memory.
    pub driver_alloc: Option<unsafe extern "C" fn(ErlDrvSizeT) -> *mut c_void>,
    /// `void *driver_realloc(void *ptr, ErlDrvSizeT size);` — resize driver memory.
    pub driver_realloc: Option<unsafe extern "C" fn(*mut c_void, ErlDrvSizeT) -> *mut c_void>,

    // -----------------------------------------------------------------------------------------
    // Queue interface
    // -----------------------------------------------------------------------------------------

    /// `int driver_enq(ErlDrvPort port, char *buf, ErlDrvSizeT len);` — enqueue data at the tail of
    /// the port queue.
    pub driver_enq: Option<unsafe extern "C" fn(ErlDrvPort, *mut c_char, ErlDrvSizeT) -> c_int>,
    /// `int driver_pushq(ErlDrvPort port, char *buf, ErlDrvSizeT len);` — push data at the head of
    /// the port queue.
    pub driver_pushq: Option<unsafe extern "C" fn(ErlDrvPort, *mut c_char, ErlDrvSizeT) -> c_int>,
    /// `ErlDrvSizeT driver_deq(ErlDrvPort port, ErlDrvSizeT size);` — drop `size` bytes from the
    /// head of the port queue.
    pub driver_deq: Option<unsafe extern "C" fn(ErlDrvPort, ErlDrvSizeT) -> ErlDrvSizeT>,
    /// `ErlDrvSizeT driver_sizeq(ErlDrvPort port);` — the number of bytes queued on the port.
    pub driver_sizeq: Option<unsafe extern "C" fn(ErlDrvPort) -> ErlDrvSizeT>,
    /// `int driver_enq_bin(ErlDrvPort port, ErlDrvBinary *bin, ErlDrvSizeT offset, ErlDrvSizeT
    /// len);` — enqueue a binary (sub-range) at the tail of the port queue.
    pub driver_enq_bin:
        Option<unsafe extern "C" fn(ErlDrvPort, *mut ErlDrvBinary, ErlDrvSizeT, ErlDrvSizeT) -> c_int>,
    /// `int driver_pushq_bin(ErlDrvPort port, ErlDrvBinary *bin, ErlDrvSizeT offset, ErlDrvSizeT
    /// len);` — push a binary (sub-range) at the head of the port queue.
    pub driver_pushq_bin:
        Option<unsafe extern "C" fn(ErlDrvPort, *mut ErlDrvBinary, ErlDrvSizeT, ErlDrvSizeT) -> c_int>,
    /// `ErlDrvSizeT driver_peekqv(ErlDrvPort port, ErlIOVec *ev);` — fill `ev` from the head of the
    /// port queue without removing anything.
    pub driver_peekqv: Option<unsafe extern "C" fn(ErlDrvPort, *mut ErlIOVec) -> ErlDrvSizeT>,
    /// `SysIOVec *driver_peekq(ErlDrvPort port, int *vlen);` — a `SysIOVec` view of the head of the
    /// port queue.
    pub driver_peekq: Option<unsafe extern "C" fn(ErlDrvPort, *mut c_int) -> *mut SysIOVec>,
    /// `int driver_enqv(ErlDrvPort port, ErlIOVec *ev, ErlDrvSizeT skip);` — enqueue a vector
    /// (skipping `skip` bytes) at the tail of the port queue.
    pub driver_enqv: Option<unsafe extern "C" fn(ErlDrvPort, *mut ErlIOVec, ErlDrvSizeT) -> c_int>,
    /// `int driver_pushqv(ErlDrvPort port, ErlIOVec *ev, ErlDrvSizeT skip);` — push a vector
    /// (skipping `skip` bytes) at the head of the port queue.
    pub driver_pushqv: Option<unsafe extern "C" fn(ErlDrvPort, *mut ErlIOVec, ErlDrvSizeT) -> c_int>,

    // -----------------------------------------------------------------------------------------
    // Driver entries and system info
    // -----------------------------------------------------------------------------------------

    /// `void add_driver_entry(ErlDrvEntry *de);` — make a statically linked driver known to the
    /// emulator.
    pub add_driver_entry: Option<unsafe extern "C" fn(*mut ErlDrvEntry)>,
    /// `int remove_driver_entry(ErlDrvEntry *de);` — remove a driver entry added by
    /// `add_driver_entry`.
    pub remove_driver_entry: Option<unsafe extern "C" fn(*mut ErlDrvEntry) -> c_int>,
    /// `void driver_system_info(ErlDrvSysInfo *sip, size_t si_size);` — fill in the driver system
    /// info structure.
    pub driver_system_info: Option<unsafe extern "C" fn(*mut ErlDrvSysInfo, size_t)>,

    // -----------------------------------------------------------------------------------------
    // Mutex family
    // -----------------------------------------------------------------------------------------

    /// `void erl_drv_mutex_destroy(ErlDrvMutex *mtx);` — destroy a driver mutex.
    pub erl_drv_mutex_destroy: Option<unsafe extern "C" fn(*mut ErlDrvMutex)>,
    /// `ErlDrvMutex *erl_drv_mutex_create(char *name);` — create a driver mutex.
    pub erl_drv_mutex_create: Option<unsafe extern "C" fn(*mut c_char) -> *mut ErlDrvMutex>,
    /// `int erl_drv_mutex_trylock(ErlDrvMutex *mtx);` — try to take a driver mutex (non-blocking).
    pub erl_drv_mutex_trylock: Option<unsafe extern "C" fn(*mut ErlDrvMutex) -> c_int>,
    /// `void erl_drv_mutex_lock(ErlDrvMutex *mtx);` — take a driver mutex.
    pub erl_drv_mutex_lock: Option<unsafe extern "C" fn(*mut ErlDrvMutex)>,
    /// `void erl_drv_mutex_unlock(ErlDrvMutex *mtx);` — release a driver mutex.
    pub erl_drv_mutex_unlock: Option<unsafe extern "C" fn(*mut ErlDrvMutex)>,

    // -----------------------------------------------------------------------------------------
    // Condition variable family
    // -----------------------------------------------------------------------------------------

    /// `void erl_drv_cond_destroy(ErlDrvCond *cnd);` — destroy a driver condition variable.
    pub erl_drv_cond_destroy: Option<unsafe extern "C" fn(*mut ErlDrvCond)>,
    /// `ErlDrvCond *erl_drv_cond_create(char *name);` — create a driver condition variable.
    pub erl_drv_cond_create: Option<unsafe extern "C" fn(*mut c_char) -> *mut ErlDrvCond>,
    /// `void erl_drv_cond_signal(ErlDrvCond *cnd);` — wake one waiter.
    pub erl_drv_cond_signal: Option<unsafe extern "C" fn(*mut ErlDrvCond)>,
    /// `void erl_drv_cond_broadcast(ErlDrvCond *cnd);` — wake all waiters.
    pub erl_drv_cond_broadcast: Option<unsafe extern "C" fn(*mut ErlDrvCond)>,
    /// `void erl_drv_cond_wait(ErlDrvCond *cnd, ErlDrvMutex *mtx);` — wait on a condition variable
    /// (releasing `mtx` while waiting).
    ///
    /// Note: `erl_driver.h` has **no** timed driver-cond wait — `enif_cond_wait_timed` is a NIF
    /// (`erl_nif.h`) entry only, so no `erl_drv_cond_wait_timed` field exists here.
    pub erl_drv_cond_wait: Option<unsafe extern "C" fn(*mut ErlDrvCond, *mut ErlDrvMutex)>,

    // -----------------------------------------------------------------------------------------
    // Read/write lock family (the header spells the write ops `*rwlock`/`*rwunlock`)
    // -----------------------------------------------------------------------------------------

    /// `void erl_drv_rwlock_destroy(ErlDrvRWLock *rwlck);` — destroy a driver read/write lock.
    pub erl_drv_rwlock_destroy: Option<unsafe extern "C" fn(*mut ErlDrvRWLock)>,
    /// `ErlDrvRWLock *erl_drv_rwlock_create(char *name);` — create a driver read/write lock.
    pub erl_drv_rwlock_create: Option<unsafe extern "C" fn(*mut c_char) -> *mut ErlDrvRWLock>,
    /// `int erl_drv_rwlock_tryrlock(ErlDrvRWLock *rwlck);` — try to take the read lock.
    pub erl_drv_rwlock_tryrlock: Option<unsafe extern "C" fn(*mut ErlDrvRWLock) -> c_int>,
    /// `void erl_drv_rwlock_rlock(ErlDrvRWLock *rwlck);` — take the read lock.
    pub erl_drv_rwlock_rlock: Option<unsafe extern "C" fn(*mut ErlDrvRWLock)>,
    /// `void erl_drv_rwlock_runlock(ErlDrvRWLock *rwlck);` — release the read lock.
    pub erl_drv_rwlock_runlock: Option<unsafe extern "C" fn(*mut ErlDrvRWLock)>,
    /// `int erl_drv_rwlock_tryrwlock(ErlDrvRWLock *rwlck);` — try to take the write lock.
    pub erl_drv_rwlock_tryrwlock: Option<unsafe extern "C" fn(*mut ErlDrvRWLock) -> c_int>,
    /// `void erl_drv_rwlock_rwlock(ErlDrvRWLock *rwlck);` — take the write lock.
    pub erl_drv_rwlock_rwlock: Option<unsafe extern "C" fn(*mut ErlDrvRWLock)>,
    /// `void erl_drv_rwlock_rwunlock(ErlDrvRWLock *rwlck);` — release the write lock.
    pub erl_drv_rwlock_rwunlock: Option<unsafe extern "C" fn(*mut ErlDrvRWLock)>,

    // -----------------------------------------------------------------------------------------
    // Thread-specific data family
    // -----------------------------------------------------------------------------------------

    /// `int erl_drv_tsd_key_create(char *name, ErlDrvTSDKey *key);` — reserve a thread-specific
    /// data key.
    pub erl_drv_tsd_key_create:
        Option<unsafe extern "C" fn(*mut c_char, *mut ErlDrvTSDKey) -> c_int>,
    /// `void erl_drv_tsd_key_destroy(ErlDrvTSDKey key);` — release a thread-specific data key.
    pub erl_drv_tsd_key_destroy: Option<unsafe extern "C" fn(ErlDrvTSDKey)>,
    /// `void erl_drv_tsd_set(ErlDrvTSDKey key, void *data);` — set the calling thread's value.
    pub erl_drv_tsd_set: Option<unsafe extern "C" fn(ErlDrvTSDKey, *mut c_void)>,
    /// `void *erl_drv_tsd_get(ErlDrvTSDKey key);` — get the calling thread's value.
    pub erl_drv_tsd_get: Option<unsafe extern "C" fn(ErlDrvTSDKey) -> *mut c_void>,

    // -----------------------------------------------------------------------------------------
    // Thread family
    // -----------------------------------------------------------------------------------------

    /// `void erl_drv_thread_opts_destroy(ErlDrvThreadOpts *opts);` — free a thread options struct.
    pub erl_drv_thread_opts_destroy: Option<unsafe extern "C" fn(*mut ErlDrvThreadOpts)>,
    /// `ErlDrvThreadOpts *erl_drv_thread_opts_create(char *name);` — allocate a thread options
    /// struct.
    pub erl_drv_thread_opts_create:
        Option<unsafe extern "C" fn(*mut c_char) -> *mut ErlDrvThreadOpts>,
    /// `int erl_drv_thread_create(char *name, ErlDrvTid *tid, void *(*func)(void *), void *args,
    /// ErlDrvThreadOpts *opts);` — start a driver thread running `func(args)`.
    pub erl_drv_thread_create: Option<
        unsafe extern "C" fn(
            *mut c_char,
            *mut ErlDrvTid,
            Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
            *mut c_void,
            *mut ErlDrvThreadOpts,
        ) -> c_int,
    >,
    /// `ErlDrvTid erl_drv_thread_self(void);` — the calling thread's driver thread id.
    pub erl_drv_thread_self: Option<unsafe extern "C" fn() -> ErlDrvTid>,
    /// `int erl_drv_equal_tids(ErlDrvTid tid1, ErlDrvTid tid2);` — compare two driver thread ids.
    pub erl_drv_equal_tids: Option<unsafe extern "C" fn(ErlDrvTid, ErlDrvTid) -> c_int>,
    /// `void erl_drv_thread_exit(void *resp);` — exit the calling driver thread with result `resp`.
    pub erl_drv_thread_exit: Option<unsafe extern "C" fn(*mut c_void)>,
    /// `int erl_drv_thread_join(ErlDrvTid tid, void **respp);` — wait for a driver thread to exit.
    pub erl_drv_thread_join: Option<unsafe extern "C" fn(ErlDrvTid, *mut *mut c_void) -> c_int>,

    // -----------------------------------------------------------------------------------------
    // Debug names
    // -----------------------------------------------------------------------------------------

    /// `char *erl_drv_mutex_name(ErlDrvMutex *mtx);` — the mutex's debug name.
    pub erl_drv_mutex_name: Option<unsafe extern "C" fn(*mut ErlDrvMutex) -> *mut c_char>,
    /// `char *erl_drv_cond_name(ErlDrvCond *cnd);` — the condition variable's debug name.
    pub erl_drv_cond_name: Option<unsafe extern "C" fn(*mut ErlDrvCond) -> *mut c_char>,
    /// `char *erl_drv_rwlock_name(ErlDrvRWLock *rwlck);` — the read/write lock's debug name.
    pub erl_drv_rwlock_name: Option<unsafe extern "C" fn(*mut ErlDrvRWLock) -> *mut c_char>,
    /// `char *erl_drv_thread_name(ErlDrvTid tid);` — the driver thread's debug name.
    pub erl_drv_thread_name: Option<unsafe extern "C" fn(ErlDrvTid) -> *mut c_char>,

    /// `int null_func(void);` — the header's do-nothing placeholder function.
    pub null_func: Option<unsafe extern "C" fn() -> c_int>,

    // -----------------------------------------------------------------------------------------
    // Term building (`ErlDrvTermData`), deprecated term output, async I/O, misc.
    // -----------------------------------------------------------------------------------------

    /// `ErlDrvTermData driver_mk_atom(char *string);` — intern a text string into a driver term
    /// atom.
    pub driver_mk_atom: Option<unsafe extern "C" fn(*mut c_char) -> ErlDrvTermData>,
    /// `ErlDrvTermData driver_mk_port(ErlDrvPort port);` — the term data for a port.
    pub driver_mk_port: Option<unsafe extern "C" fn(ErlDrvPort) -> ErlDrvTermData>,
    /// `ErlDrvTermData driver_connected(ErlDrvPort port);` — the term data of the port's connected
    /// process.
    pub driver_connected: Option<unsafe extern "C" fn(ErlDrvPort) -> ErlDrvTermData>,
    /// `ErlDrvTermData driver_caller(ErlDrvPort port);` — the term data of the process making the
    /// current call.
    pub driver_caller: Option<unsafe extern "C" fn(ErlDrvPort) -> ErlDrvTermData>,
    /// `ErlDrvTermData driver_mk_term_nil(void);` — the driver term data for the empty list.
    ///
    /// The header also exports a *variable* `const ErlDrvTermData driver_term_nil;`; a data symbol
    /// is not part of this function-pointer table (it is a constant, not a runtime provided
    /// function) and is therefore not declared here.
    pub driver_mk_term_nil: Option<unsafe extern "C" fn() -> ErlDrvTermData>,
    /// `ErlDrvPort driver_create_port(ErlDrvPort creator_port, ErlDrvTermData connected, char
    /// *name, ErlDrvData drv_data);` — create a port owned by a driver at runtime.
    pub driver_create_port: Option<
        unsafe extern "C" fn(ErlDrvPort, ErlDrvTermData, *mut c_char, ErlDrvData) -> ErlDrvPort,
    >,
    /// `int driver_output_term(ErlDrvPort ix, ErlDrvTermData *data, int len);` — **deprecated**
    /// (`ERL_DRV_DEPRECATED_FUNC`); use `erl_drv_output_term`.
    pub driver_output_term:
        Option<unsafe extern "C" fn(ErlDrvPort, *mut ErlDrvTermData, c_int) -> c_int>,
    /// `int driver_send_term(ErlDrvPort ix, ErlDrvTermData to, ErlDrvTermData *data, int len);` —
    /// **deprecated** (`ERL_DRV_DEPRECATED_FUNC`); use `erl_drv_send_term`.
    pub driver_send_term: Option<
        unsafe extern "C" fn(ErlDrvPort, ErlDrvTermData, *mut ErlDrvTermData, c_int) -> c_int,
    >,
    /// `int erl_drv_output_term(ErlDrvTermData port, ErlDrvTermData *data, int len);` — output term
    /// data to the port owner.
    pub erl_drv_output_term:
        Option<unsafe extern "C" fn(ErlDrvTermData, *mut ErlDrvTermData, c_int) -> c_int>,
    /// `int erl_drv_send_term(ErlDrvTermData port, ErlDrvTermData to, ErlDrvTermData *data, int
    /// len);` — output term data to a specific process.
    pub erl_drv_send_term: Option<
        unsafe extern "C" fn(ErlDrvTermData, ErlDrvTermData, *mut ErlDrvTermData, c_int) -> c_int,
    >,
    /// `unsigned int driver_async_port_key(ErlDrvPort port);` — the port's async key (to pass to
    /// `driver_async` from an async thread).
    pub driver_async_port_key: Option<unsafe extern "C" fn(ErlDrvPort) -> c_uint>,
    /// `long driver_async(ErlDrvPort ix, unsigned int *key, void (*async_invoke)(void *), void
    /// *async_data, void (*async_free)(void *));` — schedule an async job on an async thread.
    pub driver_async: Option<
        unsafe extern "C" fn(
            ErlDrvPort,
            *mut c_uint,
            Option<unsafe extern "C" fn(*mut c_void)>,
            *mut c_void,
            Option<unsafe extern "C" fn(*mut c_void)>,
        ) -> libc::c_long,
    >,
    /// `int driver_lock_driver(ErlDrvPort ix);` — lock the driver in the machine forever.
    pub driver_lock_driver: Option<unsafe extern "C" fn(ErlDrvPort) -> c_int>,
    /// `int driver_get_now(ErlDrvNowData *now);` — **deprecated** (`ERL_DRV_DEPRECATED_FUNC`);
    /// wall-clock "now" (analogue of the removed `erlang:now/0`).
    pub driver_get_now: Option<unsafe extern "C" fn(*mut ErlDrvNowData) -> c_int>,

    // -----------------------------------------------------------------------------------------
    // Erlang time API
    // -----------------------------------------------------------------------------------------

    /// `ErlDrvTime erl_drv_monotonic_time(ErlDrvTimeUnit time_unit);` — Erlang monotonic time.
    pub erl_drv_monotonic_time: Option<unsafe extern "C" fn(ErlDrvTimeUnit) -> ErlDrvTime>,
    /// `ErlDrvTime erl_drv_time_offset(ErlDrvTimeUnit time_unit);` — offset between monotonic and
    /// system time.
    pub erl_drv_time_offset: Option<unsafe extern "C" fn(ErlDrvTimeUnit) -> ErlDrvTime>,
    /// `ErlDrvTime erl_drv_convert_time_unit(ErlDrvTime val, ErlDrvTimeUnit from, ErlDrvTimeUnit
    /// to);` — time-unit conversion.
    pub erl_drv_convert_time_unit:
        Option<unsafe extern "C" fn(ErlDrvTime, ErlDrvTimeUnit, ErlDrvTimeUnit) -> ErlDrvTime>,

    // -----------------------------------------------------------------------------------------
    // Dynamic loading, environment, port start-up
    // -----------------------------------------------------------------------------------------

    /// `void *driver_dl_open(char *path);` — open a dynamic library.
    pub driver_dl_open: Option<unsafe extern "C" fn(*mut c_char) -> *mut c_void>,
    /// `void *driver_dl_sym(void *handle, char *name);` — look up a symbol in a dynamic library.
    pub driver_dl_sym: Option<unsafe extern "C" fn(*mut c_void, *mut c_char) -> *mut c_void>,
    /// `int driver_dl_close(void *handle);` — close a dynamic library.
    pub driver_dl_close: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    /// `char *driver_dl_error(void);` — the most recent dynamic-loader error message.
    pub driver_dl_error: Option<unsafe extern "C" fn() -> *mut c_char>,
    /// `int erl_drv_putenv(const char *key, char *value);` — set an environment variable.
    pub erl_drv_putenv: Option<unsafe extern "C" fn(*const c_char, *mut c_char) -> c_int>,
    /// `int erl_drv_getenv(const char *key, char *value, size_t *value_size);` — read an
    /// environment variable.
    pub erl_drv_getenv:
        Option<unsafe extern "C" fn(*const c_char, *mut c_char, *mut size_t) -> c_int>,
    /// `void erl_drv_init_ack(ErlDrvPort ix, ErlDrvData res);` — acknowledge an asynchronous
    /// `start` (with `ERL_DRV_FLAG_USE_INIT_ACK`).
    pub erl_drv_init_ack: Option<unsafe extern "C" fn(ErlDrvPort, ErlDrvData)>,
    /// `void erl_drv_set_os_pid(ErlDrvPort ix, ErlDrvSInt pid);` — set the OS pid reported in
    /// `port_info`.
    pub erl_drv_set_os_pid: Option<unsafe extern "C" fn(ErlDrvPort, ErlDrvSInt)>,
}

/// `ErlDrvDriver` — documented compatibility alias for [`ErlDrvApi`].
///
/// The legacy header has no single name for "the table of functions the runtime provides to
/// drivers"; this alias exists so that drivers modelled against the header can refer to the
/// driver-facing function surface by a driver-flavoured name. It is the *same* type: no
/// conversion, no distinct layout.
pub type ErlDrvDriver = ErlDrvApi;

impl ErlDrvApi {
    /// Build an all-`None` table.
    ///
    /// This is the starting point for a runtime that has not yet wired up a function pointer, and
    /// the shape the layout tests build against (an all-`None` table is still exactly
    /// `N * size_of::<usize>()` bytes).
    #[allow(clippy::too_many_lines)]
    pub fn empty() -> Self {
        Self {
            erl_drv_busy_msgq_limits: None,
            driver_select: None,
            driver_output: None,
            driver_output2: None,
            driver_output_binary: None,
            driver_outputv: None,
            driver_vec_to_buf: None,
            driver_set_timer: None,
            driver_cancel_timer: None,
            driver_read_timer: None,
            erl_drv_consume_timeslice: None,
            erl_errno_id: None,
            driver_failure_eof: None,
            driver_failure_atom: None,
            driver_failure_posix: None,
            driver_failure: None,
            driver_exit: None,
            driver_pdl_create: None,
            driver_pdl_lock: None,
            driver_pdl_unlock: None,
            driver_pdl_get_refc: None,
            driver_pdl_inc_refc: None,
            driver_pdl_dec_refc: None,
            driver_monitor_process: None,
            driver_demonitor_process: None,
            driver_get_monitored_process: None,
            driver_compare_monitors: None,
            set_busy_port: None,
            set_port_control_flags: None,
            get_port_flags: None,
            driver_free_binary: None,
            driver_alloc_binary: None,
            driver_realloc_binary: None,
            driver_binary_get_refc: None,
            driver_binary_inc_refc: None,
            driver_binary_dec_refc: None,
            driver_free: None,
            driver_alloc: None,
            driver_realloc: None,
            driver_enq: None,
            driver_pushq: None,
            driver_deq: None,
            driver_sizeq: None,
            driver_enq_bin: None,
            driver_pushq_bin: None,
            driver_peekqv: None,
            driver_peekq: None,
            driver_enqv: None,
            driver_pushqv: None,
            add_driver_entry: None,
            remove_driver_entry: None,
            driver_system_info: None,
            erl_drv_mutex_destroy: None,
            erl_drv_mutex_create: None,
            erl_drv_mutex_trylock: None,
            erl_drv_mutex_lock: None,
            erl_drv_mutex_unlock: None,
            erl_drv_cond_destroy: None,
            erl_drv_cond_create: None,
            erl_drv_cond_signal: None,
            erl_drv_cond_broadcast: None,
            erl_drv_cond_wait: None,
            erl_drv_rwlock_destroy: None,
            erl_drv_rwlock_create: None,
            erl_drv_rwlock_tryrlock: None,
            erl_drv_rwlock_rlock: None,
            erl_drv_rwlock_runlock: None,
            erl_drv_rwlock_tryrwlock: None,
            erl_drv_rwlock_rwlock: None,
            erl_drv_rwlock_rwunlock: None,
            erl_drv_tsd_key_create: None,
            erl_drv_tsd_key_destroy: None,
            erl_drv_tsd_set: None,
            erl_drv_tsd_get: None,
            erl_drv_thread_opts_destroy: None,
            erl_drv_thread_opts_create: None,
            erl_drv_thread_create: None,
            erl_drv_thread_self: None,
            erl_drv_equal_tids: None,
            erl_drv_thread_exit: None,
            erl_drv_thread_join: None,
            erl_drv_mutex_name: None,
            erl_drv_cond_name: None,
            erl_drv_rwlock_name: None,
            erl_drv_thread_name: None,
            null_func: None,
            driver_mk_atom: None,
            driver_mk_port: None,
            driver_connected: None,
            driver_caller: None,
            driver_mk_term_nil: None,
            driver_create_port: None,
            driver_output_term: None,
            driver_send_term: None,
            erl_drv_output_term: None,
            erl_drv_send_term: None,
            driver_async_port_key: None,
            driver_async: None,
            driver_lock_driver: None,
            driver_get_now: None,
            erl_drv_monotonic_time: None,
            erl_drv_time_offset: None,
            erl_drv_convert_time_unit: None,
            driver_dl_open: None,
            driver_dl_sym: None,
            driver_dl_close: None,
            driver_dl_error: None,
            erl_drv_putenv: None,
            erl_drv_getenv: None,
            erl_drv_init_ack: None,
            erl_drv_set_os_pid: None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Compile-time layout freeze (LP64)
// ---------------------------------------------------------------------------------------------

/// `N` — the exact number of function-pointer fields in [`ErlDrvApi`], i.e. the number of
/// `driver_*` / `erl_drv_*` functions the runtime provides to drivers on LP64 (every declaration
/// in `erl_driver.h` from `erl_drv_busy_msgq_limits` through `erl_drv_set_os_pid`, excluding the
/// two `__DARWIN__`-only entries).
///
/// Exposed so dependent code and the sibling `tests/api_surface.rs` can assert the table shape
/// without re-deriving it.
pub const ERL_DRV_API_FIELD_COUNT: usize = 111;

const _: () = {
    // Every column is one pointer, so the table is exactly N machine words on LP64.
    assert!(core::mem::size_of::<ErlDrvApi>() == ERL_DRV_API_FIELD_COUNT * core::mem::size_of::<usize>());
    assert!(core::mem::size_of::<ErlDrvApi>() == 888);
    assert!(core::mem::align_of::<ErlDrvApi>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_shape_matches_the_field_count() {
        assert_eq!(ERL_DRV_API_FIELD_COUNT, 111);
        assert_eq!(
            core::mem::size_of::<ErlDrvApi>(),
            ERL_DRV_API_FIELD_COUNT * core::mem::size_of::<usize>()
        );
        assert_eq!(core::mem::size_of::<ErlDrvApi>(), 888);
    }

    #[test]
    fn empty_table_is_all_none() {
        let api = ErlDrvApi::empty();
        // Copy + Clone round-trip (the table is a plain aggregate of pointers).
        let copy = api;
        assert!(copy.driver_select.is_none());
        assert!(copy.erl_drv_mutex_lock.is_none());
        assert!(copy.driver_async.is_none());
        assert!(copy.erl_drv_set_os_pid.is_none());
        assert_eq!(core::mem::size_of_val(&api), 888);
    }

    #[test]
    fn alias_is_the_same_type() {
        // `ErlDrvDriver` must be a pure alias: same size, and values flow between the two names.
        let api: ErlDrvApi = ErlDrvDriver::empty();
        let same: ErlDrvDriver = api;
        assert_eq!(core::mem::size_of::<ErlDrvDriver>(), 888);
        assert!(same.null_func.is_none());
    }
}
