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

//! End-to-end proof of the **safe async-job runner**: a real C driver's `async_invoke`/`async_free`
//! callbacks — reached through the exported `driver_async` slot and handed over as raw pointers —
//! are run through [`AsyncJob`], whose construction is the only `unsafe` in the path.
//!
//! # What this adds over the unit tests
//!
//! The unit tests in `src/driver/async_job.rs` pin the handle's own semantics with Rust callbacks.
//! This test pins the *whole* chain with the operands that a genuinly **foreign** caller produces:
//! a C driver `.so` (compiled by the system `cc` against the system `erl_driver.h`, `dlopen`ed
//! through [`beam_abi::driver::loader`]) calls `driver_async(port, NULL, t_async_invoke, &job_data,
//! t_async_free)`; [`beam_abi::DriverHost::r#async`] receives those **C function pointers** and runs
//! them through an [`AsyncJob`] (the C's own inline/no-pool path). The driver then reports, through a
//! second `port_control` call, how many times each callback ran and in which order — so a translation
//! mistake anywhere (a swapped callback, a double free, a data pointer that does not round-trip)
//! shows up as a wrong value, not as a silently passing test.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system OTP include directory. When either is
//! missing the test prints a loud `SKIP:` banner and returns (via `common::compile_so`); when both
//! are present every assertion is hard.

mod common;

use std::ffi::CString;
use std::ptr;

use beam_abi::driver::loader::load;
use beam_abi::driver::types::ErlDrvPort;use beam_abi::{AsyncJob, DriverEnv, DriverHost};
use beam_term::atom::AtomTable;
use beam_term::prelude::{make_atom, Eterm};
use libc::{c_char, c_int, c_long, c_uint, c_void};

use common::compile_so;

/// A minimal C driver that hands the emulator one async job from `port_control(port, 1, …)` and
/// reports the callbacks' effect from `port_control(port, 2, …)`.
///
/// The two callbacks are file-static C functions, i.e. exactly the "live C function pointers" the
/// `AsyncJob::new` contract talks about, and they receive a **real C object** (`&job_data`) as their
/// async data. The reply of command 2 packs the evidence into four 8-bit fields, so one assertion
/// covers counts *and* ordering:
///
/// `[invokes][frees][invoke_order][free_order]` — `0x01_01_01_02` for a correct single inline run.
/// A mismatched data pointer poisons its counter by +100, which cannot fit in the field.
const FIXTURE_C: &str = r#"
#include <erl_driver.h>

/* The async data: a real object the callbacks verify they got back verbatim. */
static char job_data = 'D';

/* Evidence reported through control(2): counters plus the order each callback ran in (0 = never). */
static unsigned int sequence = 0;
static unsigned int invokes = 0;
static unsigned int frees = 0;
static unsigned int invoke_order = 0;
static unsigned int free_order = 0;

static void t_async_invoke(void *data) {
    invokes++;
    if (data != (void *) &job_data) {
        invokes += 100;             /* a wrong data pointer must not keep the reply clean */
        return;
    }
    invoke_order = ++sequence;
}

static void t_async_free(void *data) {
    frees++;
    if (data != (void *) &job_data) {
        frees += 100;
        return;
    }
    free_order = ++sequence;
}

static ErlDrvData t_start(ErlDrvPort port, char *command) {
    (void) command;
    return (ErlDrvData) port;       /* the port IS all the state this fixture needs */
}

static void t_stop(ErlDrvData drv_data) {
    (void) drv_data;
}

static ErlDrvSSizeT t_control(ErlDrvData drv_data, unsigned int command,
                              char *buf, ErlDrvSizeT len,
                              char **rbuf, ErlDrvSizeT rlen) {
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;
    switch (command) {
    case 1:
        /* Hand the emulator an async job. A host with no pool runs it inline (erl_async.c:604). */
        return (ErlDrvSSizeT) driver_async((ErlDrvPort) drv_data, NULL, t_async_invoke,
                                           (void *) &job_data, t_async_free);
    case 2:
        /* Pack the evidence: one byte per count/order field. */
        return (ErlDrvSSizeT) ((invokes << 24) | (frees << 16) | (invoke_order << 8) | free_order);
    default:
        return 0;
    }
}

static ErlDrvEntry t_async_driver_entry = {
    .start = t_start,
    .stop = t_stop,
    .driver_name = "tasync",
    .control = t_control,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
};

DRIVER_INIT(tasync) {
    return &t_async_driver_entry;
}
"#;

/// A [`DriverHost`] whose `r#async` runs the job **inline through [`AsyncJob`]** — the C's own
/// no-pool path — and which is otherwise inert.
///
/// This is the shape the objective prescribes for a host: the only `unsafe` in the whole
/// implementation is the single [`AsyncJob::new`] call, whose operands came straight from
/// `driver_async`'s arguments.
struct InlineAsyncHost {
    heap: beam_alloc::Heap,
    atoms: AtomTable,
    /// How many jobs the host was asked to run (so a test can tell "ran" from "was never asked").
    jobs: usize,
}

impl InlineAsyncHost {
    fn new() -> Self {
        InlineAsyncHost {
            heap: beam_alloc::Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            jobs: 0,
        }
    }
}

impl DriverHost for InlineAsyncHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    /// The fixture's job never outputs through the port, so this must never be reached.
    fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
        panic!("the async fixture must not write through the port");
    }

    fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
        -1
    }

    /// The C's no-pool `driver_async`: run the job in the calling thread and release it there.
    ///
    /// `0` is the C's "the job completed in this call" answer.
    fn r#async(
        &mut self,
        _port: ErlDrvPort,
        _key: *mut c_uint,
        invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_long {
        // SAFETY: these are `driver_async`'s own operands — a live C async job (`invoke`/`free` are
        // the driver's callbacks and `data` is the object it handed them).
        let Some(job) = (unsafe { AsyncJob::new(invoke, free, data) }) else {
            return -1;
        };
        self.jobs += 1;
        let _tdata = job.invoke();
        job.finish();
        0
    }
}

#[test]
fn a_real_c_driver_async_job_runs_through_the_safe_runner() {
    let Some(so) = compile_so("driver_async_job", FIXTURE_C) else {
        return; // no C compiler / no OTP include dir: `compile_so` printed the SKIP banner
    };
    let path = so.to_str().expect("the scratch path is UTF-8");
    let lib = load(path, "tasync").expect("the fixture driver must load and validate");

    let mut host = InlineAsyncHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");

    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; `command` is NUL-terminated.
    let data = lib
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(!data.is_null(), "start must return the private state, not a failure code");

    // `control(1, …)` calls `driver_async`, so the host's safe runner runs the C callbacks inline.
    let mut reply: *mut c_char = ptr::null_mut();
    // SAFETY: `data` is the live state from `start`; the async command reads no buffer.
    let code = lib
        .control(data, 1, ptr::null_mut(), 0, &mut reply, 0)
        .expect("the fixture implements control");
    assert_eq!(code, 0, "the host reports an inline completion, as the C does");
    assert_eq!(host.jobs, 1, "`driver_async` must reach `DriverHost::r#async` exactly once");

    // `control(2, …)` reports what the C callbacks observed: counts and ordering, one byte each.
    // SAFETY: `data` is live; the report command reads no buffer.
    let reported = lib
        .control(data, 2, ptr::null_mut(), 0, &mut reply, 0)
        .expect("the fixture implements control");
    assert_eq!(
        reported, 0x0101_0102_i64 as libc::ssize_t,
        "each C callback must run exactly once (invoke, then free) on the data it was given; \
         reported {reported:#x}"
    );

    // SAFETY: `data` is the live state from `start` and `stop` has not run yet.
    lib.stop(data).expect("the fixture implements stop");
}

/// The fixture's `driver_async` with a `NULL` `async_invoke` is refused by the runner (and the host
/// answers the C's failure code) rather than calling anything through a null pointer.
///
/// This pins the one branch [`AsyncJob::new`] can take without a job — the same guard the unit test
/// covers, exercised here through the frozen `driver_async` slot and a host that panics if any other
/// service is touched.
#[test]
fn the_frozen_async_slot_with_a_null_invoke_is_refused() {
    let mut host = NullJobHost;
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let mut key: c_uint = 7;

    // SAFETY: `port` is the live `env`'s own handle; the job has no callbacks and no data.
    let code =
        unsafe { beam_abi::driver::drv::driver_async(port, &mut key, None, ptr::null_mut(), None) };
    assert_eq!(code, -1, "a job with no `async_invoke` is refused, never run");
}

/// A host that proves the refusal path touches nothing else: every other seam method panics, and
/// `r#async` forwards to [`AsyncJob::new`] and answers the C's failure code when it finds no job.
struct NullJobHost;

impl DriverHost for NullJobHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        panic!("refusing an async job must not allocate");
    }

    fn intern_atom(&mut self, _name: &str) -> Option<Eterm> {
        panic!("refusing an async job must not intern an atom");
    }

    fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
        panic!("refusing an async job must not output");
    }

    fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
        panic!("refusing an async job must not send a term");
    }

    fn r#async(
        &mut self,
        _port: ErlDrvPort,
        _key: *mut c_uint,
        invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_long {
        // SAFETY: the operands are whatever the caller passed; with `invoke == None` nothing runs.
        match unsafe { AsyncJob::new(invoke, free, data) } {
            Some(_) => panic!("a `NULL` async_invoke must not produce a job"),
            None => -1,
        }
    }
}