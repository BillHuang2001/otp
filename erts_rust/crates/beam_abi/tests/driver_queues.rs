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

//! End-to-end proof of the **port-queue family** of the driver C ABI (workspace contract **A.4**):
//! a **real C driver shared object** — compiled with the system `cc` against the system
//! `erl_driver.h` — is `dlopen`ed, validated and driven through the Rust loader, with its
//! `driver_enq`/`driver_deq`/`driver_peekq*`/… imports resolving out of *this test binary's* dynamic
//! symbol table.
//!
//! # Why this is the load-bearing test
//!
//! `tests/fixtures/driver_queues.c` is an ordinary driver: it `#include`s `erl_driver.h` and calls
//! `driver_enq`, `driver_pushq`, `driver_deq`, `driver_sizeq`, `driver_peekq`, `driver_peekqv`,
//! `driver_enq_bin`, `driver_pushq_bin`, `driver_enqv`, `driver_pushqv` and `driver_vec_to_buf`,
//! whose definitions live in *this* crate (`beam_abi::driver::drv::queues`). The linker leaves those
//! calls **undefined** in the `.so`, so `dlopen(..., RTLD_NOW)` inside
//! [`beam_abi::driver::loader::load`] can only succeed if the *loading process* exports them as
//! dynamic symbols — which `build.rs` arranges with `-Wl,--export-dynamic` on the test targets, and
//! which `driver_api_table()` forces into the link. A successful load is therefore the empirical
//! proof that the Rust queue functions bind a real C driver.
//!
//! # What is asserted
//!
//! Each fixture command publishes a one-line report through `driver_output(st->port, …)` — i.e. it
//! drives [`DriverHost::output`] — describing what the eleven queue functions returned and what they
//! left in the queue. The test asserts, per command:
//!
//! 1. **the report text** the host observed (the ABI's return values / filled structures), against
//!    the string measured on the real system ERTS (OTP 29.1.1 / erts-17.1) with this same fixture;
//! 2. **the queue traffic** the host observed (every `enq`/`pushq` payload with its `is_binary`
//!    flag, every `deq` length, and the resulting queue size after each op) — so a function that
//!    returned the right number while queueing the wrong bytes fails here;
//! 3. that every command **drains** the queue, and that the report never goes through the `control`
//!    reply buffer.
//!
//! # Documented divergences from the oracle (both witnesses of the same deviation)
//!
//! The seam only **reads** the caller's `ErlIOVec`, where the C's `skip()` (`erl_io_queue.c:194`)
//! rewrites it *in place* (`iov_base += skip`, `iov_len -= skip`). The fixture deliberately reuses
//! one `ErlIOVec`/`SysIOVec` array across calls, so the two runtimes report different bytes there:
//!
//! * **command 2** — the fixture hands `driver_pushqv` and `driver_vec_to_buf` fresh vectors, so
//!   the oracle and this runtime agree byte for byte: `vec4=4 bytes4=abcd vecbig=6 bytesbig=abcdef
//!   veczero=0`.
//! * **command 3** — the oracle reports `ev_iov0_len=4 … vector_moved=1`; this runtime reports
//!   `ev_iov0_len=6 … vector_moved=0` (the base still points at `"abcdef"`). Both agree on the
//!   queue's own readings (`enqv=0 size=4 vlen=1 entry=4 ev_size=6`).
//!
//! Commands 1 and 4 agree with the oracle **byte for byte**. Both divergences are asserted as
//! *measured* here, with the oracle's value recorded in the comment next to each expectation.
//!
//! # Skip-vs-fail policy
//!
//! Building the fixture needs a C compiler and the system OTP include directory (which holds
//! `erl_driver.h`). When either is missing the test prints a loud `SKIP:` banner and returns; when
//! the toolchain **is** present every assertion is hard (a mismatch fails, it never silently passes).

mod common;

use std::ffi::CString;
use std::path::Path;
use std::ptr;

use beam_abi::driver::api::ErlDrvApi;
use beam_abi::driver::drv::driver_api_table;
use beam_abi::driver::loader::load;
use beam_abi::driver::types::{ErlDrvPort, ErlDrvSizeT, ErlIOVec};
use beam_abi::{DriverEnv, DriverHost, SysIOVec};
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{make_atom, Eterm};
use libc::{c_char, c_int, c_void};

use common::{compile_so, skip};

/// The committed C fixture — a real driver exercising the whole port-queue family.
const FIXTURE_C: &str = include_str!("fixtures/driver_queues.c");

// ---------------------------------------------------------------------------------------------
// The reports measured on the oracle (and, for commands 2/3, the Rust-side value)
// ---------------------------------------------------------------------------------------------

/// Command 1 — the enq / size / peek / deq semantics. **Identical on the oracle.**
const CASE1_REPORT: &str = "enq=0,0 size=10 peekq_null=0 vlen=2 first=5 sum=10 peekqv=10 \
ev.vsize=2 ev.size=10 deq3=7 size2=7 deq_over=18446744073709551615 size3=7 deq_exact=0 size4=0 \
peekq_empty_null=1 vlen_empty=0 peekqv_empty=0 ev2.vsize=0";

/// Command 2 — the head/tail variants, the binary forms, `skip` and `vec_to_buf`.
///
/// **Oracle agreement:** `driver_pushqv` and `driver_vec_to_buf` are given fresh vectors by the
/// fixture, so the oracle reports exactly the string below (its in-place `skip()` can only
/// rewrite a vector a `*_v` call was actually handed — see command 3).
const CASE2_REPORT: &str = "push=0,0 size=4 enqbin=0 pushbin=0 zero=0 size2=8 enqv=0 size_mid=12 \
pushqv=0 size3=14 vec4=4 bytes4=abcd vecbig=6 bytesbig=abcdef veczero=0";

/// Command 3 — does `driver_enqv` advance the caller's vector in place?
///
/// **Divergence:** the oracle reports `ev_iov0_len=4 … vector_moved=1` (its `skip()` advances the
/// caller's `iov[0]` in place); the values below are this runtime's, which only reads it. The queue
/// readings (`enqv=0 size=4 vlen=1 entry=4 ev_size=6`) agree on both.
const CASE3_REPORT: &str =
    "enqv=0 size=4 vlen=1 entry=4 ev_iov0_len=6 ev_size=6 vector_moved=0";

/// Command 4 — the empty-queue readings on a fresh port. **Identical on the oracle.**
const CASE4_REPORT: &str = "size=0 peekq_null=1 vlen=0 peekqv=0 ev.vsize=0 ev.size=0 ev.iov_null=1 \
ev.binv_null=1 deq0=0 deq1=18446744073709551615";

// ---------------------------------------------------------------------------------------------
// The test host: a real in-memory port FIFO behind the `DriverHost` seam
// ---------------------------------------------------------------------------------------------

/// One port-queue operation the fixture drove through the [`DriverHost`] seam, with the queue's
/// total size *after* it — the observable queue traffic behind the ABI's return values.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    /// `DriverHost::enq` (from `driver_enq`/`driver_enq_bin`/`driver_enqv`).
    Enq {
        /// The bytes the seam handed the host (already linearized out of any binary window).
        bytes: Vec<u8>,
        /// Whether the driver presented them as an `ErlDrvBinary` window.
        is_binary: bool,
        /// The queue's total byte count after the append.
        size: usize,
    },
    /// `DriverHost::pushq` (from `driver_pushq`/`driver_pushq_bin`/`driver_pushqv`).
    Pushq {
        /// The bytes the seam handed the host.
        bytes: Vec<u8>,
        /// Whether the driver presented them as an `ErlDrvBinary` window.
        is_binary: bool,
        /// The queue's total byte count after the prepend.
        size: usize,
    },
    /// `DriverHost::deq`, with the queue's total byte count after the drop.
    Deq {
        /// The number of bytes the ABI asked to drop.
        len: usize,
        /// The queue's total byte count after the drop.
        size: usize,
    },
}

/// A [`DriverHost`] with a **real port queue**: a FIFO of owned entries, one per `enq`/`pushq`
/// (which is what makes `driver_peekq`'s `*vlen` observable as `2` after two enqueues), plus the
/// recording `output` sink the fixture's reports land in and a trace of every queue operation.
///
/// The queue hooks are implemented here rather than in `tests/common/mod.rs`: the shared
/// `TestDriverHost` deliberately leaves them at their unsupported defaults, and this test needs a
/// real FIFO to exercise the family end to end.
struct QueueHost {
    heap: Heap,
    atoms: AtomTable,
    /// The port queue: one owned entry per enqueue, in order. Empty entries are impossible
    /// (`driver_enq*` refuse `len == 0` before reaching the host).
    queue: Vec<Vec<u8>>,
    /// Every [`DriverHost::output`] payload, in call order — where the fixture's reports land.
    outputs: Vec<Vec<u8>>,
    /// Every queue operation, in call order.
    trace: Vec<Op>,
}

impl QueueHost {
    fn new() -> Self {
        QueueHost {
            heap: Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            queue: Vec::new(),
            outputs: Vec::new(),
            trace: Vec::new(),
        }
    }

    /// The queued byte count, as `driver_sizeq` reports it.
    fn size(&self) -> usize {
        self.queue.iter().map(Vec::len).sum()
    }

    /// Every [`DriverHost::output`] payload, in call order.
    fn outputs(&self) -> &[Vec<u8>] {
        &self.outputs
    }

    /// Every queue operation, in call order.
    fn trace(&self) -> &[Op] {
        &self.trace
    }

    /// True when the port queue holds no bytes — the fixture drains before returning, so every
    /// command must leave it empty (a non-empty queue at close makes the real emulator dump core).
    fn queue_is_empty(&self) -> bool {
        self.queue.is_empty() && self.size() == 0
    }
}

impl DriverHost for QueueHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// The atom table is unbounded, so interning always succeeds.
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
        self.outputs.push(data.to_vec());
        0
    }

    fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
        0
    }

    fn enq(&mut self, data: &[u8], is_binary: bool) -> c_int {
        self.queue.push(data.to_vec());
        self.trace.push(Op::Enq {
            bytes: data.to_vec(),
            is_binary,
            size: self.size(),
        });
        0
    }

    fn pushq(&mut self, data: &[u8], is_binary: bool) -> c_int {
        self.queue.insert(0, data.to_vec());
        self.trace.push(Op::Pushq {
            bytes: data.to_vec(),
            is_binary,
            size: self.size(),
        });
        0
    }

    /// Drop up to `len` bytes from the head, trimming a partially covered entry, and report the
    /// number of bytes actually dropped. `driver_deq` refuses an over-long request *before* calling
    /// this hook, so it never sees `len` above the queued total.
    fn deq(&mut self, len: c_int) -> c_int {
        if len < 0 {
            return -1;
        }
        let mut left = len as usize;
        let mut dropped = 0usize;
        while left > 0 {
            let Some(front) = self.queue.first_mut() else {
                break;
            };
            if front.len() <= left {
                left -= front.len();
                dropped += front.len();
                self.queue.remove(0);
            } else {
                front.drain(..left);
                dropped += left;
                left = 0;
            }
        }
        self.trace.push(Op::Deq {
            len: dropped,
            size: self.size(),
        });
        dropped as c_int
    }

    fn sizeq(&mut self) -> c_int {
        self.size() as c_int
    }

    /// One `SysIOVec` per queued entry — the shape the C publishes (`*vlen` counts entries, not
    /// bytes), which is what makes `driver_peekq`'s `*vlen` observable as `2` after two enqueues.
    ///
    /// `None` for an empty queue: the seam reports "empty" and "unsupported" the same way, and
    /// `driver_peekq` tells them apart through `sizeq` (see `queues.rs::peekq_len_when_empty`).
    fn peekq(&mut self) -> Option<Vec<SysIOVec>> {
        if self.queue.is_empty() {
            return None;
        }
        Some(
            self.queue
                .iter()
                .map(|entry| SysIOVec {
                    iov_base: entry.as_ptr() as *mut c_void,
                    iov_len: entry.len(),
                })
                .collect(),
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The frozen `driver_*` table the queue functions are published through.
fn api() -> &'static ErlDrvApi {
    driver_api_table()
}

/// `None` (after a loud skip) when the compiled `.so` path is not valid UTF-8.
fn so_path(so: &Path) -> Option<String> {
    match so.to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("compiled .so path is not valid UTF-8");
            None
        }
    }
}

/// The report the fixture published for the command just driven (it is ASCII text).
fn last_report(host: &QueueHost) -> String {
    let bytes = host
        .outputs()
        .last()
        .expect("the fixture must publish a report through driver_output");
    String::from_utf8(bytes.clone()).expect("the report is ASCII text")
}

/// `Op::Enq` shorthand for the expected-trace literals.
fn enq(bytes: &[u8], is_binary: bool, size: usize) -> Op {
    Op::Enq {
        bytes: bytes.to_vec(),
        is_binary,
        size,
    }
}

/// `Op::Pushq` shorthand for the expected-trace literals.
fn pushq(bytes: &[u8], is_binary: bool, size: usize) -> Op {
    Op::Pushq {
        bytes: bytes.to_vec(),
        is_binary,
        size,
    }
}

/// `Op::Deq` shorthand: the bytes dropped (the ABI's request, always satisfied here) and the
/// resulting queue size.
fn deq(len: usize, size: usize) -> Op {
    Op::Deq { len, size }
}

// ---------------------------------------------------------------------------------------------
// The frozen table
// ---------------------------------------------------------------------------------------------

#[test]
fn the_frozen_table_populates_the_whole_port_queue_family() {
    let table = api();
    let populated = [
        table.driver_enq.is_some(),
        table.driver_pushq.is_some(),
        table.driver_enq_bin.is_some(),
        table.driver_pushq_bin.is_some(),
        table.driver_enqv.is_some(),
        table.driver_pushqv.is_some(),
        table.driver_deq.is_some(),
        table.driver_sizeq.is_some(),
        table.driver_peekq.is_some(),
        table.driver_peekqv.is_some(),
        table.driver_vec_to_buf.is_some(),
    ];
    assert!(
        populated.iter().all(|&p| p),
        "every port-queue slot must be populated: {populated:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// End-to-end: load a real driver `.so` and drive its queue family
// ---------------------------------------------------------------------------------------------

#[test]
fn drives_a_real_c_driver_through_the_port_queue_family() {
    // Load-bearing: this pins the family's slots in the frozen table AND forces the
    // `driver_*`-carrying codegen unit into this test binary's link, so the fixture's undefined
    // queue imports resolve under `dlopen(RTLD_NOW)`.
    let table = api();
    let populated = [
        table.driver_enq.is_some(),
        table.driver_pushq.is_some(),
        table.driver_enq_bin.is_some(),
        table.driver_pushq_bin.is_some(),
        table.driver_enqv.is_some(),
        table.driver_pushqv.is_some(),
        table.driver_deq.is_some(),
        table.driver_sizeq.is_some(),
        table.driver_peekq.is_some(),
        table.driver_peekqv.is_some(),
        table.driver_vec_to_buf.is_some(),
    ];
    assert!(populated.iter().all(|&p| p), "{populated:?}");

    let Some(so) = compile_so("driver_queues", FIXTURE_C) else {
        return;
    };
    let Some(path) = so_path(&so) else {
        return;
    };

    // --- dlopen(RTLD_NOW) + the `ErlDrvEntry` validation ---------------------------------------
    let lib = load(&path, "qd")
        .expect("a driver whose queue imports resolve from this test binary must load");
    assert_eq!(lib.name(), "qd");

    // --- the port/state and the control commands ----------------------------------------------
    let mut host = QueueHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let command = CString::new("start").expect("no interior NUL");
    // SAFETY: `port` came from `env.as_port()` on the still-live `env`; `command` is NUL-terminated.
    let data = lib
        .start(port, command.as_ptr() as *mut c_char)
        .expect("the fixture implements start");
    assert!(
        !data.is_null(),
        "start must return the private state, not a failure code"
    );

    // The reply buffer a `control` call is handed. The fixture publishes through
    // `driver_output`, so this buffer must come back untouched (the emulator's own reply buffer
    // truncates at `ERL_ONHEAP_BINARY_LIMIT` = 64 bytes, which is why the reports moved there).
    let mut reply = [0u8; 64];
    let mut reply_ptr = reply.as_mut_ptr() as *mut c_char;

    // (command, the report the host must observe, the queue traffic the host must observe)
    let cases: [(u32, &str, Vec<Op>); 4] = [
        (
            1,
            CASE1_REPORT,
            vec![
                enq(b"hello", false, 5),
                enq(b"world", false, 10),
                // `driver_deq(3)`: a partial drop, reported as the remaining 7 bytes.
                deq(3, 7),
                // `driver_deq(100)` on a 7-byte queue is refused by `driver_deq` itself (the C's
                // `(ErlDrvSizeT)-1`), so it never reaches the host; `driver_deq(7)` drains exactly.
                deq(7, 0),
            ],
        ),
        (
            2,
            CASE2_REPORT,
            vec![
                pushq(b"cd", false, 2),
                pushq(b"ab", false, 4),
                // `driver_enq_bin(bin, 1, 2)` → the `"wxyz"[1..3]` window, as a binary.
                enq(b"xy", true, 6),
                // `driver_pushq_bin(bin, 0, 2)` → the `"wxyz"[0..2]` window, as a binary.
                pushq(b"wx", true, 8),
                // `driver_enqv(ev(6), 2)` → the 4 bytes after the skip.
                enq(b"cdef", false, 12),
                // `driver_pushqv(ev(6), 4)` → the 2 bytes after the skip. Note `driver_enq("x", 0)`
                // is the C's early return: it must never reach the host.
                pushq(b"ef", false, 14),
                // The fixture's drain.
                deq(14, 0),
            ],
        ),
        (
            3,
            CASE3_REPORT,
            vec![
                enq(b"cdef", false, 4),
                deq(4, 0),
            ],
        ),
        (
            4,
            CASE4_REPORT,
            vec![
                // `driver_deq(0)` is the no-op that *does* reach the host (and reports the
                // unchanged size); the following `driver_deq(1)` on an empty queue is refused by
                // `driver_deq` itself (the C's `(ErlDrvSizeT)-1`), so it stops there.
                deq(0, 0),
            ],
        ),
    ];

    for (cmd, expected_report, expected_trace) in cases {
        let ops_before = host.trace().len();
        // SAFETY: `data` is the live state from `start`; the commands read no input buffer; the
        // reply pointer heads 64 writable bytes and `rlen` reports that capacity.
        let n = lib
            .control(
                data,
                cmd,
                ptr::null_mut(),
                0,
                &mut reply_ptr,
                reply.len() as ErlDrvSizeT,
            )
            .expect("the fixture implements control");

        let report = last_report(&host);
        assert_eq!(report, expected_report, "the control command {cmd} report");
        assert_eq!(
            n as usize,
            expected_report.len(),
            "control must return the length of the report it published for command {cmd}"
        );
        assert_eq!(
            &host.trace()[ops_before..],
            &expected_trace[..],
            "the queue traffic for control command {cmd}"
        );
        assert!(
            host.queue_is_empty(),
            "control command {cmd} must drain the port queue before returning"
        );
    }

    assert_eq!(
        host.outputs().len(),
        4,
        "every command publishes exactly one report"
    );
    assert!(
        reply.iter().all(|&b| b == 0),
        "the reports must be published through driver_output, not written into the reply buffer"
    );

    // `stop` frees the private state (once; no double free).
    // SAFETY: `data` is the live state from `start` and `stop` has not run yet.
    lib.stop(data).expect("the fixture implements stop");

    // Dropping the library runs the `finish` callback (absent here) then `dlclose`s the object.
    drop(lib);
}

// ---------------------------------------------------------------------------------------------
// Refusals where the C would fault
// ---------------------------------------------------------------------------------------------

#[test]
fn refuses_null_operands_with_the_c_failure_codes() {
    let t = api();
    let mut data = *b"abc";
    let buf = data.as_mut_ptr() as *mut c_char;
    let mut vlen = 5;
    let mut ev = ErlIOVec::default();

    // SAFETY: every call below passes a null port (or a null vector) and, where a buffer is taken,
    // a live 3-byte one; the functions must report the C's failure code instead of dereferencing.
    unsafe {
        assert_eq!((t.driver_sizeq.unwrap())(ptr::null_mut()), ErlDrvSizeT::MAX);
        assert_eq!((t.driver_deq.unwrap())(ptr::null_mut(), 1), ErlDrvSizeT::MAX);
        assert_eq!((t.driver_enq.unwrap())(ptr::null_mut(), buf, 3), -1);
        assert_eq!((t.driver_pushq.unwrap())(ptr::null_mut(), buf, 3), -1);
        assert_eq!(
            (t.driver_enqv.unwrap())(ptr::null_mut(), &mut ev, 0),
            -1
        );
        assert_eq!(
            (t.driver_peekqv.unwrap())(ptr::null_mut(), &mut ev),
            ErlDrvSizeT::MAX
        );
        assert!((t.driver_peekq.unwrap())(ptr::null_mut(), &mut vlen).is_null());
        assert_eq!(vlen, -1, "a null port is the C's null-queue path (`*vlen = -1`)");
        assert_eq!(
            (t.driver_vec_to_buf.unwrap())(ptr::null_mut(), buf, 4),
            0,
            "`driver_vec_to_buf` has no failure code: it reports `0` (copied nothing)"
        );
    }
}
