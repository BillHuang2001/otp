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

//! The driver `#[repr(C)]` types from `erts/emulator/beam/erl_driver.h`.
//!
//! Field order, integer widths and structure sizes reproduce the OTP header exactly for the LP64
//! target (`SIZEOF_LONG == 8`, `SIZEOF_VOID_P == 8`). Only the normal (`!ERL_SYS_DRV`) `start`
//! signature is modelled; the Windows-specific parts (`erl_win_dyn_driver.h`, the
//! `ERL_SYS_DRV`/`SysDriverOpts` variant) are out of scope. Every struct is pinned by a
//! compile-time size/align assertion at the bottom of the file; the exhaustive field-offset
//! freeze lives in [`crate::layout`].
//!
//! Types shared with the NIF ABI (`ErlDrvSysInfo`, `ErlDrvThreadOpts`, `ErlDirtyJobFlags`,
//! `ErlDrvMonitor`) are defined once in [`crate::common`] and re-exported here.

use crate::common::SysIOVec;
use libc::{c_char, c_int, c_uint, c_void, size_t};

// Types defined in `erl_drv_nif.h` (shared with the NIF ABI) are re-exported so that
// `driver::types::ErlDrvSysInfo` &c. resolve.
pub use crate::common::{
    ErlDirtyJobFlags, ErlDrvMonitor, ErlDrvSysInfo, ErlDrvThreadOpts,
    ERL_DIRTY_JOB_CPU_BOUND, ERL_DIRTY_JOB_IO_BOUND,
};

// ---------------------------------------------------------------------------------------------
// Integer typedefs
// ---------------------------------------------------------------------------------------------

/// `ErlDrvUInt64` (`erl_driver.h`) — `ErlNapiUInt64`.
pub type ErlDrvUInt64 = crate::common::ErlNapiUInt64;
/// `ErlDrvSInt64` (`erl_driver.h`) — `ErlNapiSInt64`.
pub type ErlDrvSInt64 = crate::common::ErlNapiSInt64;
/// `ErlDrvUInt` (`erl_driver.h`) — `ErlNapiUInt`.
pub type ErlDrvUInt = crate::common::ErlNapiUInt;
/// `ErlDrvSInt` (`erl_driver.h`) — `ErlNapiSInt`.
pub type ErlDrvSInt = crate::common::ErlNapiSInt;
/// `ErlDrvTermData` — an encoded term handed to `erl_drv_output_term`/`erl_drv_send_term`.
pub type ErlDrvTermData = crate::common::ErlNapiUInt;
/// `ErlDrvSizeT` — `size_t` on Unix.
pub type ErlDrvSizeT = size_t;
/// `ErlDrvSSizeT` — `ssize_t` on Unix.
pub type ErlDrvSSizeT = libc::ssize_t;
/// `ErlDrvTime` — a signed 64-bit time value.
pub type ErlDrvTime = crate::common::ErlNapiSInt64;
/// `ErlDrvTSDKey` — thread-specific-data key.
pub type ErlDrvTSDKey = c_int;

// ---------------------------------------------------------------------------------------------
// Opaque handles (incomplete C types — used behind pointers / by reference, never dereferenced)
// ---------------------------------------------------------------------------------------------

/// `struct ErlDrvTid_` — a driver thread identifier.
#[repr(C)]
pub struct ErlDrvTid_ {
    _private: [u8; 0],
}

/// `struct ErlDrvMutex_` — a driver mutex (used behind a `*mut`).
#[repr(C)]
pub struct ErlDrvMutex_ {
    _private: [u8; 0],
}

/// `struct ErlDrvCond_` — a driver condition variable (used behind a `*mut`).
#[repr(C)]
pub struct ErlDrvCond_ {
    _private: [u8; 0],
}

/// `struct ErlDrvRWLock_` — a driver read/write lock (used behind a `*mut`).
#[repr(C)]
pub struct ErlDrvRWLock_ {
    _private: [u8; 0],
}

/// `struct _erl_drv_port` — a port descriptor.
#[repr(C)]
pub struct ErlDrvPort_ {
    _private: [u8; 0],
}

/// `struct _erl_drv_event` — a selectable event handle.
#[repr(C)]
pub struct ErlDrvEvent_ {
    _private: [u8; 0],
}

/// `struct _erl_drv_data` — driver-private data.
#[repr(C)]
pub struct ErlDrvData_ {
    _private: [u8; 0],
}

/// `struct erl_drv_port_data_lock` — a port data lock.
#[repr(C)]
pub struct ErlDrvPortDataLock {
    _private: [u8; 0],
}

/// `ErlDrvData` — driver-private data (`typedef struct _erl_drv_data* ErlDrvData;`).
pub type ErlDrvData = *mut ErlDrvData_;
/// `ErlDrvEvent` — an event to be selected on.
pub type ErlDrvEvent = *mut ErlDrvEvent_;
/// `ErlDrvPort` — a port descriptor.
pub type ErlDrvPort = *mut ErlDrvPort_;
/// `ErlDrvThreadData` — the thread data passed to `ready_async`.
pub type ErlDrvThreadData = *mut ErlDrvPort_;
/// `ErlDrvPDL` — a port data lock handle.
pub type ErlDrvPDL = *mut ErlDrvPortDataLock;
/// `ErlDrvTid` — a driver thread identifier (used behind a `*mut`).
pub type ErlDrvTid = *mut ErlDrvTid_;
/// `ErlDrvMutex` — a driver mutex (the C name is the *struct*, used behind a `*mut`).
pub type ErlDrvMutex = ErlDrvMutex_;
/// `ErlDrvCond` — a driver condition variable (used behind a `*mut`).
pub type ErlDrvCond = ErlDrvCond_;
/// `ErlDrvRWLock` — a driver read/write lock (used behind a `*mut`).
pub type ErlDrvRWLock = ErlDrvRWLock_;

// ---------------------------------------------------------------------------------------------
// enums
// ---------------------------------------------------------------------------------------------

/// `ErlDrvTimeUnit` — a time unit for the driver time API.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ErlDrvTimeUnit {
    /// `ERL_DRV_SEC`.
    Sec = 0,
    /// `ERL_DRV_MSEC`.
    Msec = 1,
    /// `ERL_DRV_USEC`.
    Usec = 2,
    /// `ERL_DRV_NSEC`.
    Nsec = 3,
}

// ---------------------------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------------------------

/// `ErlDrvBinary` — a binary as seen from a driver. Never mutate once sent to Erlang.
///
/// Layout (16/8): `orig_size` 0, `orig_bytes` 8 (a 1-element flexible array member).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlDrvBinary {
    /// Total length of the binary in bytes.
    pub orig_size: ErlDrvSInt,
    /// The data (`char` rather than `byte`, as in the header).
    pub orig_bytes: [c_char; 1],
}

impl Default for ErlDrvBinary {
    fn default() -> Self {
        Self {
            orig_size: 0,
            orig_bytes: [0],
        }
    }
}

/// `ErlIOVec` — a driver scatter/gather vector (ErlDrvBinary-based analogue of `ErlNifIOVec`).
///
/// Layout (32/8): `vsize` 0, `size` 8, `iov` 16, `binv` 24.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlIOVec {
    /// Number of valid vectors.
    pub vsize: c_int,
    /// Total size in bytes.
    pub size: ErlDrvSizeT,
    /// The vector array.
    pub iov: *mut SysIOVec,
    /// The `ErlDrvBinary*` array backing the vectors.
    pub binv: *mut *mut ErlDrvBinary,
}

impl Default for ErlIOVec {
    fn default() -> Self {
        Self {
            vsize: 0,
            size: 0,
            iov: core::ptr::null_mut(),
            binv: core::ptr::null_mut(),
        }
    }
}

/// `ErlDrvNowData` — the (deprecated) `driver_get_now` wall-clock breakdown.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct ErlDrvNowData {
    /// Mega-seconds since the epoch.
    pub megasecs: libc::c_ulong,
    /// Seconds within the mega-second.
    pub secs: libc::c_ulong,
    /// Micro-seconds within the second.
    pub microsecs: libc::c_ulong,
}

/// `ErlDrvEntry` — the driver description returned by `driver_init`.
///
/// Layout (176/8): 16 leading function pointers (`init` 0 … `unused_event_callback` 120), then
/// `extended_marker` 128, `major_version` 132, `minor_version` 136, `driver_flags` 140,
/// `handle2` 144, `process_exit` 152, `stop_select` 160, `emergency_close` 168.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ErlDrvEntry {
    /// Called at system start-up (static drivers) or after load (dynamic drivers).
    pub init: Option<unsafe extern "C" fn() -> c_int>,
    /// Called when `open_port/2` is invoked; returning `-1` means failure.
    pub start: Option<unsafe extern "C" fn(ErlDrvPort, *mut c_char) -> ErlDrvData>,
    /// Called when the port is closed and when the emulator halts.
    pub stop: Option<unsafe extern "C" fn(ErlDrvData)>,
    /// Called when there is output from Erlang to the port.
    pub output: Option<unsafe extern "C" fn(ErlDrvData, *mut c_char, ErlDrvSizeT)>,
    /// Called when input arrives on one of the driver's handles.
    pub ready_input: Option<unsafe extern "C" fn(ErlDrvData, ErlDrvEvent)>,
    /// Called when output is possible to one of the driver's handles.
    pub ready_output: Option<unsafe extern "C" fn(ErlDrvData, ErlDrvEvent)>,
    /// The name supplied as the command in `open_port({spawn, Name}, ...)`.
    pub driver_name: *mut c_char,
    /// Called before unloading the driver (dynamic drivers only).
    pub finish: Option<unsafe extern "C" fn()>,
    /// Reserved — used by the emulator internally.
    pub handle: *mut c_void,
    /// The driver "ioctl", invoked by `port_control/3`.
    pub control: Option<
        unsafe extern "C" fn(
            ErlDrvData,
            c_uint,
            *mut c_char,
            ErlDrvSizeT,
            *mut *mut c_char,
            ErlDrvSizeT,
        ) -> ErlDrvSSizeT,
    >,
    /// Timeout handler.
    pub timeout: Option<unsafe extern "C" fn(ErlDrvData)>,
    /// Called when there is output from Erlang to the port (vector form).
    pub outputv: Option<unsafe extern "C" fn(ErlDrvData, *mut ErlIOVec)>,
    /// Called when an async job has completed.
    pub ready_async: Option<unsafe extern "C" fn(ErlDrvData, ErlDrvThreadData)>,
    /// Called when the port is about to be closed but queued data must be flushed first.
    pub flush: Option<unsafe extern "C" fn(ErlDrvData)>,
    /// A synchronous call into the driver (works mostly like `control`).
    pub call: Option<
        unsafe extern "C" fn(
            ErlDrvData,
            c_uint,
            *mut c_char,
            ErlDrvSizeT,
            *mut *mut c_char,
            ErlDrvSizeT,
            *mut c_uint,
        ) -> ErlDrvSSizeT,
    >,
    /// Reserved.
    pub unused_event_callback: Option<unsafe extern "C" fn()>,
    /// `ERL_DRV_EXTENDED_MARKER`.
    pub extended_marker: c_int,
    /// `ERL_DRV_EXTENDED_MAJOR_VERSION`.
    pub major_version: c_int,
    /// `ERL_DRV_EXTENDED_MINOR_VERSION`.
    pub minor_version: c_int,
    /// `ERL_DRV_FLAG_*`.
    pub driver_flags: c_int,
    /// Reserved — used by the emulator internally.
    pub handle2: *mut c_void,
    /// Called when a process monitor fires.
    pub process_exit: Option<unsafe extern "C" fn(ErlDrvData, *mut ErlDrvMonitor)>,
    /// Called on behalf of `driver_select` when it is safe to release `event`.
    pub stop_select: Option<unsafe extern "C" fn(ErlDrvEvent, *mut c_void)>,
    /// Called when the port is closed abruptly (e.g. by `erl_crash_dump`).
    pub emergency_close: Option<unsafe extern "C" fn(ErlDrvData)>,
}

impl Default for ErlDrvEntry {
    fn default() -> Self {
        Self {
            init: None,
            start: None,
            stop: None,
            output: None,
            ready_input: None,
            ready_output: None,
            driver_name: core::ptr::null_mut(),
            finish: None,
            handle: core::ptr::null_mut(),
            control: None,
            timeout: None,
            outputv: None,
            ready_async: None,
            flush: None,
            call: None,
            unused_event_callback: None,
            extended_marker: 0,
            major_version: 0,
            minor_version: 0,
            driver_flags: 0,
            handle2: core::ptr::null_mut(),
            process_exit: None,
            stop_select: None,
            emergency_close: None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------------

// ABI version constants are owned by the crate root (`src/lib.rs`, the authoritative location per
// the CONTEXT.md routing table) and re-exported here so `driver::types::ERL_DRV_EXTENDED_*`
// resolves too.
pub use crate::{
    ERL_DRV_EXTENDED_MAJOR_VERSION, ERL_DRV_EXTENDED_MARKER, ERL_DRV_EXTENDED_MINOR_VERSION,
    ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD,
};

/// `ERL_DRV_READ` — select for readability (`ERL_NIF_SELECT_READ`).
pub const ERL_DRV_READ: c_int = crate::common::ErlNifSelectFlags::READ.bits();
/// `ERL_DRV_WRITE` — select for writability (`ERL_NIF_SELECT_WRITE`).
pub const ERL_DRV_WRITE: c_int = crate::common::ErlNifSelectFlags::WRITE.bits();
/// `ERL_DRV_USE` — select for "safe to release" (`ERL_NIF_SELECT_STOP`).
pub const ERL_DRV_USE: c_int = crate::common::ErlNifSelectFlags::STOP.bits();
/// `ERL_DRV_USE_NO_CALLBACK` — `ERL_DRV_USE | (ERL_DRV_USE << 1)`.
pub const ERL_DRV_USE_NO_CALLBACK: c_int = ERL_DRV_USE | (ERL_DRV_USE << 1);

/// `PORT_CONTROL_FLAG_BINARY` — binary-mode port on `port_control`.
pub const PORT_CONTROL_FLAG_BINARY: c_int = 1 << 0;
/// `PORT_CONTROL_FLAG_HEAVY` — heavy-workload hint.
pub const PORT_CONTROL_FLAG_HEAVY: c_int = 1 << 1;

/// `PORT_FLAG_BINARY` — the port is in binary mode.
pub const PORT_FLAG_BINARY: c_int = 1 << 0;
/// `PORT_FLAG_LINE` — the port is in line mode.
pub const PORT_FLAG_LINE: c_int = 1 << 1;

/// `ERTS_INVALID_ERL_DRV_PORT` — the invalid-port sentinel (`erts/emulator/beam/erl_port.h:67`):
/// the C's `((struct _erl_drv_port *) ((SWord) -1))`, i.e. the pointer `-1`.
///
/// It is what `driver_create_port` returns for any failure (`io.c:786`). Its comment explains why it
/// is not `NULL`: in pre-R16 systems `0` was a valid port handle, so `-1` had to be the invalid one.
pub const ERTS_INVALID_ERL_DRV_PORT: ErlDrvPort = -1isize as ErlDrvPort;

/// `ERL_DRV_FLAG_USE_PORT_LOCKING` — the emulator locks the port before calls.
pub const ERL_DRV_FLAG_USE_PORT_LOCKING: c_int = 1 << 0;
/// `ERL_DRV_FLAG_SOFT_BUSY` — busy state is not scheduled automatically.
pub const ERL_DRV_FLAG_SOFT_BUSY: c_int = 1 << 1;
/// `ERL_DRV_FLAG_NO_BUSY_MSGQ` — disable the busy message queue.
pub const ERL_DRV_FLAG_NO_BUSY_MSGQ: c_int = 1 << 2;
/// `ERL_DRV_FLAG_USE_INIT_ACK` — `start` is acknowledged asynchronously.
pub const ERL_DRV_FLAG_USE_INIT_ACK: c_int = 1 << 3;

/// `DRIVER_CALL_KEEP_BUFFER` — a `call` return flag telling the emulator to keep the buffer.
pub const DRIVER_CALL_KEEP_BUFFER: c_int = 0x1;

/// `ERL_DRV_ERROR_GENERAL` — `start` failure → `{'EXIT',{einval,Where}}`.
pub const ERL_DRV_ERROR_GENERAL: ErlDrvData = -1isize as ErlDrvData;
/// `ERL_DRV_ERROR_ERRNO` — `start` failure → `{'EXIT',{Errno,Where}}`.
pub const ERL_DRV_ERROR_ERRNO: ErlDrvData = -2isize as ErlDrvData;
/// `ERL_DRV_ERROR_BADARG` — `start` failure → `{'EXIT',{badarg,Where}}`.
pub const ERL_DRV_ERROR_BADARG: ErlDrvData = -3isize as ErlDrvData;

/// `ERL_DRV_BUSY_MSGQ_DISABLED` — the busy message queue is disabled (`~0`).
pub const ERL_DRV_BUSY_MSGQ_DISABLED: ErlDrvSizeT = !0;
/// `ERL_DRV_BUSY_MSGQ_READ_ONLY` — the queue refuses writes.
pub const ERL_DRV_BUSY_MSGQ_READ_ONLY: ErlDrvSizeT = 0;
/// `ERL_DRV_BUSY_MSGQ_LIM_MAX` — the largest usable queue limit.
pub const ERL_DRV_BUSY_MSGQ_LIM_MAX: ErlDrvSizeT = ERL_DRV_BUSY_MSGQ_DISABLED - 1;
/// `ERL_DRV_BUSY_MSGQ_LIM_MIN` — the smallest busy message queue limit.
pub const ERL_DRV_BUSY_MSGQ_LIM_MIN: ErlDrvSizeT = 1;

/// `ERL_DRV_TIME_ERROR` — the invalid-time sentinel returned by the driver time API.
pub const ERL_DRV_TIME_ERROR: ErlDrvTime = crate::common::ERTS_NAPI_TIME_ERROR;

// Term-data tags for `erl_drv_output_term` / `erl_drv_send_term`.
/// `ERL_DRV_NIL` — no argument.
pub const ERL_DRV_NIL: ErlDrvTermData = 1;
/// `ERL_DRV_ATOM` — argument: `driver_mk_atom(string)`.
pub const ERL_DRV_ATOM: ErlDrvTermData = 2;
/// `ERL_DRV_INT` — argument: `ErlDrvSInt`.
pub const ERL_DRV_INT: ErlDrvTermData = 3;
/// `ERL_DRV_PORT` — argument: `driver_mk_port(ix)`.
pub const ERL_DRV_PORT: ErlDrvTermData = 4;
/// `ERL_DRV_BINARY` — arguments: `ErlDrvBinary*`, size, offset.
pub const ERL_DRV_BINARY: ErlDrvTermData = 5;
/// `ERL_DRV_STRING` — arguments: `char*`, length.
pub const ERL_DRV_STRING: ErlDrvTermData = 6;
/// `ERL_DRV_TUPLE` — argument: arity.
pub const ERL_DRV_TUPLE: ErlDrvTermData = 7;
/// `ERL_DRV_LIST` — argument: length.
pub const ERL_DRV_LIST: ErlDrvTermData = 8;
/// `ERL_DRV_STRING_CONS` — arguments: `char*`, length.
pub const ERL_DRV_STRING_CONS: ErlDrvTermData = 9;
/// `ERL_DRV_PID` — argument: pid term data.
pub const ERL_DRV_PID: ErlDrvTermData = 10;
/// `ERL_DRV_FLOAT` — argument: `double *`.
pub const ERL_DRV_FLOAT: ErlDrvTermData = 11;
/// `ERL_DRV_EXT2TERM` — arguments: `char *`, length.
pub const ERL_DRV_EXT2TERM: ErlDrvTermData = 12;
/// `ERL_DRV_UINT` — argument: `ErlDrvUInt`.
pub const ERL_DRV_UINT: ErlDrvTermData = 13;
/// `ERL_DRV_BUF2BINARY` — arguments: `char *`, length.
pub const ERL_DRV_BUF2BINARY: ErlDrvTermData = 14;
/// `ERL_DRV_INT64` — argument: `ErlDrvSInt64 *`.
pub const ERL_DRV_INT64: ErlDrvTermData = 15;
/// `ERL_DRV_UINT64` — argument: `ErlDrvUInt64 *`.
pub const ERL_DRV_UINT64: ErlDrvTermData = 16;
/// `ERL_DRV_MAP` — argument: number of key/value pairs.
pub const ERL_DRV_MAP: ErlDrvTermData = 17;

/// `TERM_DATA(x)` — the header's cast helper for building term-data arrays.
///
/// C performs an implicit conversion here (`#define TERM_DATA(x) ((ErlDrvTermData) (x))`); Rust
/// has no implicit conversions, so the helper takes the address-sized integer form callers use
/// when feeding pointers/lengths into the term-data constructors.
#[inline]
pub const fn term_data(x: usize) -> ErlDrvTermData {
    x as ErlDrvTermData
}

// ---------------------------------------------------------------------------------------------
// Compile-time layout freeze (LP64)
// ---------------------------------------------------------------------------------------------

const _: () = {
    assert!(core::mem::size_of::<ErlDrvBinary>() == 16);
    assert!(core::mem::align_of::<ErlDrvBinary>() == 8);
    assert!(core::mem::offset_of!(ErlDrvBinary, orig_size) == 0);
    assert!(core::mem::offset_of!(ErlDrvBinary, orig_bytes) == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlIOVec>() == 32);
    assert!(core::mem::align_of::<ErlIOVec>() == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlDrvNowData>() == 24);
    assert!(core::mem::align_of::<ErlDrvNowData>() == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlDrvEntry>() == 176);
    assert!(core::mem::align_of::<ErlDrvEntry>() == 8);
};

const _: () = {
    assert!(core::mem::size_of::<ErlDrvTimeUnit>() == 4);
};

const _: () = {
    assert!(core::mem::size_of::<ErlDrvTid_>() == 0);
    assert!(core::mem::size_of::<ErlDrvMutex_>() == 0);
    assert!(core::mem::size_of::<ErlDrvCond_>() == 0);
    assert!(core::mem::size_of::<ErlDrvRWLock_>() == 0);
    assert!(core::mem::size_of::<ErlDrvPort_>() == 0);
    assert!(core::mem::size_of::<ErlDrvEvent_>() == 0);
    assert!(core::mem::size_of::<ErlDrvData_>() == 0);
    assert!(core::mem::size_of::<ErlDrvPortDataLock>() == 0);
    assert!(core::mem::size_of::<ErlDrvData>() == core::mem::size_of::<*mut c_void>());
    assert!(core::mem::size_of::<ErlDrvEvent>() == core::mem::size_of::<*mut c_void>());
    assert!(core::mem::size_of::<ErlDrvPort>() == core::mem::size_of::<*mut c_void>());
    assert!(core::mem::size_of::<ErlDrvTid>() == core::mem::size_of::<*mut c_void>());
    assert!(core::mem::size_of::<ErlDrvSizeT>() == 8);
    assert!(core::mem::size_of::<ErlDrvSSizeT>() == 8);
    assert!(core::mem::size_of::<ErlDrvTermData>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_and_driver_flag_values() {
        assert_eq!(ERL_DRV_READ, 1);
        assert_eq!(ERL_DRV_WRITE, 2);
        assert_eq!(ERL_DRV_USE, 4);
        assert_eq!(ERL_DRV_USE_NO_CALLBACK, 12);
        assert_eq!(PORT_CONTROL_FLAG_BINARY, 1);
        assert_eq!(PORT_CONTROL_FLAG_HEAVY, 2);
        assert_eq!(PORT_FLAG_BINARY, 1);
        assert_eq!(PORT_FLAG_LINE, 2);
        assert_eq!(ERL_DRV_FLAG_USE_PORT_LOCKING, 1);
        assert_eq!(ERL_DRV_FLAG_SOFT_BUSY, 2);
        assert_eq!(ERL_DRV_FLAG_NO_BUSY_MSGQ, 4);
        assert_eq!(ERL_DRV_FLAG_USE_INIT_ACK, 8);
        assert_eq!(DRIVER_CALL_KEEP_BUFFER, 0x1);
        // The invalid-port sentinel is the pointer `-1` (`erl_port.h:67`), never `NULL`.
        assert_eq!(ERTS_INVALID_ERL_DRV_PORT as isize, -1);
    }

    #[test]
    fn error_and_busy_message_queue_constants() {
        assert_eq!(ERL_DRV_ERROR_GENERAL as isize, -1);
        assert_eq!(ERL_DRV_ERROR_ERRNO as isize, -2);
        assert_eq!(ERL_DRV_ERROR_BADARG as isize, -3);
        assert_eq!(ERL_DRV_BUSY_MSGQ_DISABLED, !0);
        assert_eq!(ERL_DRV_BUSY_MSGQ_READ_ONLY, 0);
        assert_eq!(ERL_DRV_BUSY_MSGQ_LIM_MAX, !0 - 1);
        assert_eq!(ERL_DRV_BUSY_MSGQ_LIM_MIN, 1);
        assert_eq!(ERL_DRV_TIME_ERROR, i64::MIN);
    }

    #[test]
    fn term_data_tags_and_helper() {
        assert_eq!(
            [
                ERL_DRV_NIL,
                ERL_DRV_ATOM,
                ERL_DRV_INT,
                ERL_DRV_PORT,
                ERL_DRV_BINARY,
                ERL_DRV_STRING,
                ERL_DRV_TUPLE,
                ERL_DRV_LIST,
                ERL_DRV_STRING_CONS,
                ERL_DRV_PID,
                ERL_DRV_FLOAT,
                ERL_DRV_EXT2TERM,
                ERL_DRV_UINT,
                ERL_DRV_BUF2BINARY,
                ERL_DRV_INT64,
                ERL_DRV_UINT64,
                ERL_DRV_MAP,
            ],
            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
        );
        assert_eq!(term_data(42), 42);
    }

    #[test]
    fn default_entry_is_all_null() {
        let e = ErlDrvEntry::default();
        assert!(e.init.is_none());
        assert!(e.start.is_none());
        assert!(e.driver_name.is_null());
        assert!(e.handle.is_null());
        assert!(e.handle2.is_null());
        assert_eq!(e.extended_marker, 0);
        assert!(e.emergency_close.is_none());
    }

    #[test]
    fn version_constants_are_reachable_through_the_driver_module() {
        assert_eq!(ERL_DRV_EXTENDED_MARKER, 0xfeeeeeedu32 as c_int);
        assert_eq!(ERL_DRV_EXTENDED_MAJOR_VERSION, 3);
        assert_eq!(ERL_DRV_EXTENDED_MINOR_VERSION, 3);
        assert_eq!(ERL_DRV_MIN_REQUIRED_MAJOR_VERSION_ON_LOAD, 2);
        assert_eq!(ErlDrvTimeUnit::Nsec as c_int, 3);
    }
}
