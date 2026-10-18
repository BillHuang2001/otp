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

//! Path-included tests for [`super`] (`driver::drv::port`) — the port/host-dependent half of the
//! `driver_*` / `erl_drv_*` C ABI.
//!
//! Kept in a sibling file so `port.rs` stays under the workspace's ~1000-line convention; included
//! via `#[path]` as a **child** of `port`, so `super::*` still reaches the module's items and
//! `super::super::*` the shared `drv` helpers (`binary_payload`, `driver_api_table`).

use super::*;
use std::ffi::CString;

use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{
    atom_val, is_atom, is_tuple, make_atom, make_small, tuple_element, tuple_size,
};

use crate::driver::host::{DriverEnv, DriverHost};
use crate::driver::types::{term_data, ERL_DRV_ATOM, ERL_DRV_INT, ERL_DRV_READ, ERL_DRV_TUPLE};

use super::super::mem::{driver_alloc_binary, driver_free_binary};
use super::super::{binary_payload, driver_api_table};

/// A recording [`DriverHost`] over a bump heap and an atom table.
///
/// Every host call is captured, so a test can assert exactly what reached the port (`output`,
/// `sent`, `selected`, `timers_*`, `async_calls`); the optional methods return recognizable
/// values instead of the trait's `unsupported` defaults.
struct TestDriverHost {
    heap: Heap,
    atoms: AtomTable,
    output: Vec<u8>,
    /// How many times [`DriverHost::output`] was called (`output` itself only concatenates, so the
    /// call *count* is what distinguishes one joined delivery from several).
    calls: usize,
    sent: Vec<(Option<Eterm>, Eterm)>,
    selected: Vec<(usize, c_int, c_int)>,
    timers_set: Vec<c_ulong>,
    timers_cancelled: usize,
    timer_left: Option<c_ulong>,
    async_calls: Vec<(bool, usize)>,
    connected: Option<Eterm>,
    caller: Option<Eterm>,
}

impl TestDriverHost {
    fn new() -> TestDriverHost {
        TestDriverHost {
            heap: Heap::new(1024).expect("a small test heap is allocatable"),
            atoms: AtomTable::new(),
            output: Vec::new(),
            calls: 0,
            sent: Vec::new(),
            selected: Vec::new(),
            timers_set: Vec::new(),
            timers_cancelled: 0,
            timer_left: None,
            async_calls: Vec::new(),
            connected: None,
            caller: None,
        }
    }

    /// The name behind an atom term, through this host's own atom table.
    fn atom_name(&self, term: Eterm) -> Option<&str> {
        if !is_atom(term) {
            return None;
        }
        self.atoms.name(atom_val(term))
    }
}

impl DriverHost for TestDriverHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
        self.calls += 1;
        self.output.extend_from_slice(data);
        0
    }

    fn send_term(&mut self, _port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int {
        self.sent.push((to, term));
        0
    }

    fn select(
        &mut self,
        _port: ErlDrvPort,
        event: ErlDrvEvent,
        mode: c_int,
        on: c_int,
    ) -> c_int {
        self.selected.push((event as usize, mode, on));
        0
    }

    fn set_timer(&mut self, _port: ErlDrvPort, ms: c_ulong) -> c_int {
        self.timers_set.push(ms);
        0
    }

    fn cancel_timer(&mut self, _port: ErlDrvPort) -> c_int {
        self.timers_cancelled += 1;
        0
    }

    fn read_timer(&mut self, _port: ErlDrvPort) -> Option<c_ulong> {
        self.timer_left
    }

    fn r#async(
        &mut self,
        _port: ErlDrvPort,
        _key: *mut c_uint,
        invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
        _free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_long {
        self.async_calls.push((invoke.is_some(), data as usize));
        5 // A recognizable "queued" handle.
    }

    fn connected_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        self.connected
    }

    fn caller_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        self.caller
    }
}

/// The `{atom, value}` shape the term-data tests send: the atom's name and the second element,
/// or `None` when `term` is not a 2-tuple.
///
/// A comparison helper rather than a raw-word one: every `*_output_term` call decodes the spec
/// afresh, so equal-content tuples live in *different* heap boxes.
fn atom_int_pair(host: &TestDriverHost, term: Eterm) -> Option<(Option<&str>, Eterm)> {
    // SAFETY: `term` is a live term word.
    if !unsafe { is_tuple(term) } {
        return None;
    }
    // SAFETY: `term` is a tuple (checked above).
    if unsafe { tuple_size(term) } != 2 {
        return None;
    }
    // SAFETY: both indices are inside a 2-tuple (checked above).
    let (first, second) = unsafe { (tuple_element(term, 0), tuple_element(term, 1)) };
    Some((host.atom_name(first), second))
}

/// A host that overrides **nothing** but the required methods: every optional method keeps its
/// `unsupported` default, so the C failure code must come back.
struct MinimalDriverHost {
    heap: Heap,
}

impl MinimalDriverHost {
    fn new() -> MinimalDriverHost {
        MinimalDriverHost {
            heap: Heap::new(1024).expect("a small test heap is allocatable"),
        }
    }
}

impl DriverHost for MinimalDriverHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, _name: &str) -> Option<Eterm> {
        None
    }

    fn output(&mut self, _port: ErlDrvPort, _data: &[u8]) -> c_int {
        -1
    }

    fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
        -1
    }
}

/// A host that overrides the **split** seam ([`DriverHost::output2`]) and records every reply as
/// `(header, body)`; anything that reached the *unsplit* hook is recorded separately, so a test can
/// assert the reply was never flattened.
struct SplitHost {
    heap: Heap,
    splits: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    flattened: Vec<Vec<u8>>,
}

impl SplitHost {
    fn new() -> SplitHost {
        SplitHost {
            heap: Heap::new(1024).expect("a small test heap is allocatable"),
            splits: Vec::new(),
            flattened: Vec::new(),
        }
    }
}

impl DriverHost for SplitHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, _name: &str) -> Option<Eterm> {
        None
    }

    fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
        self.flattened.push(data.to_vec());
        0
    }

    fn output2(&mut self, _port: ErlDrvPort, header: &[u8], body: Option<&[u8]>) -> c_int {
        self.splits
            .push((header.to_vec(), body.map(<[u8]>::to_vec)));
        0
    }

    fn send_term(&mut self, _port: ErlDrvPort, _to: Option<Eterm>, _term: Eterm) -> c_int {
        -1
    }
}

#[test]
fn driver_output_reaches_the_host_with_the_exact_bytes() {
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    assert!(!port.is_null(), "the port pointer is never null");

    let mut body = *b"hello";
    // SAFETY: `port` is a live env handle and `body` declares its own length.
    let rc = unsafe { driver_output(port, body.as_mut_ptr() as *mut c_char, body.len()) };
    assert_eq!(rc, 0);
    assert_eq!(host.output, b"hello");

    // `driver_output2` puts the header in front of the body.
    host.output.clear();
    let mut head = *b"H";
    // SAFETY: `port` is a live handle; both buffers declare their lengths.
    let rc = unsafe {
        driver_output2(
            port,
            head.as_mut_ptr() as *mut c_char,
            head.len(),
            body.as_mut_ptr() as *mut c_char,
            body.len(),
        )
    };
    assert_eq!(rc, 0);
    assert_eq!(host.output, b"Hhello");

    // An empty body still delivers the header.
    host.output.clear();
    // SAFETY: `port` is live; a zero length with a valid pointer is well formed.
    let rc = unsafe {
        driver_output2(
            port,
            head.as_mut_ptr() as *mut c_char,
            head.len(),
            body.as_mut_ptr() as *mut c_char,
            0,
        )
    };
    assert_eq!(rc, 0);
    assert_eq!(host.output, b"H");
}

#[test]
fn driver_output_binary_slices_the_payload_and_refuses_an_over_read() {
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    // SAFETY: the driver-binary allocator is a malloc wrapper; `bin` is freed below.
    let bin = unsafe { driver_alloc_binary(8) };
    binary_payload(bin, 8).copy_from_slice(b"abcdefgh");
    let mut head = *b"H";

    // SAFETY: `bin` is live and `2 + 3 <= orig_size`.
    let rc =
        unsafe { driver_output_binary(port, head.as_mut_ptr() as *mut c_char, 1, bin, 2, 3) };
    assert_eq!(rc, 0);
    assert_eq!(host.output, b"Hcde");

    // A sub-range past the payload is refused (the C would read past it).
    host.output.clear();
    // SAFETY: `bin` is live; the request deliberately runs past its payload.
    let rc =
        unsafe { driver_output_binary(port, head.as_mut_ptr() as *mut c_char, 1, bin, 6, 3) };
    assert_eq!(rc, -1);
    assert!(host.output.is_empty());

    // The empty sub-range at the very end is still well formed.
    // SAFETY: `bin` is live and `8 + 0 <= orig_size`.
    let rc =
        unsafe { driver_output_binary(port, head.as_mut_ptr() as *mut c_char, 1, bin, 8, 0) };
    assert_eq!(rc, 0);
    assert_eq!(host.output, b"H");

    // A null binary is refused.
    // SAFETY: the null binary is checked before any read.
    let rc = unsafe {
        driver_output_binary(
            port,
            head.as_mut_ptr() as *mut c_char,
            1,
            core::ptr::null_mut(),
            0,
            0,
        )
    };
    assert_eq!(rc, -1);

    // SAFETY: `bin` came from `driver_alloc_binary` and is freed exactly once.
    unsafe { driver_free_binary(bin) };
}

#[test]
fn driver_outputv_gathers_the_vectors_and_drops_skip_bytes() {
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    let mut first = *b"abcd";
    let mut second = *b"efgh";
    let mut iov = [
        libc::iovec {
            iov_base: first.as_mut_ptr() as *mut c_void,
            iov_len: first.len(),
        },
        libc::iovec {
            iov_base: second.as_mut_ptr() as *mut c_void,
            iov_len: second.len(),
        },
    ];
    let mut ev = ErlIOVec {
        vsize: iov.len() as c_int,
        size: 8,
        iov: iov.as_mut_ptr(),
        binv: core::ptr::null_mut(),
    };
    let mut head = *b"H";

    // SAFETY: `ev` describes the two live buffers and `skip` is inside `size`.
    let rc = unsafe { driver_outputv(port, head.as_mut_ptr() as *mut c_char, 1, &mut ev, 5) };
    assert_eq!(rc, 0);
    assert_eq!(host.output, b"Hfgh", "skip drops the first 5 bytes");

    // `skip == size`: the header alone (the C's early return).
    host.output.clear();
    // SAFETY: `ev` is live and `skip == size`.
    let rc = unsafe { driver_outputv(port, head.as_mut_ptr() as *mut c_char, 1, &mut ev, 8) };
    assert_eq!(rc, 0);
    assert_eq!(host.output, b"H");

    // A `skip` past the vectors' declared lengths is malformed.
    let mut short_iov = [libc::iovec {
        iov_base: first.as_mut_ptr() as *mut c_void,
        iov_len: first.len(),
    }];
    let mut short_ev = ErlIOVec {
        vsize: 1,
        size: 8,
        iov: short_iov.as_mut_ptr(),
        binv: core::ptr::null_mut(),
    };
    host.output.clear();
    // SAFETY: `short_ev` is live; its single entry is shorter than the declared `size`.
    let rc =
        unsafe { driver_outputv(port, head.as_mut_ptr() as *mut c_char, 1, &mut short_ev, 5) };
    assert_eq!(rc, -1);
    assert!(host.output.is_empty());

    // A null vector, and a null port, are refused.
    // SAFETY: the null pointers are checked before any read.
    unsafe {
        assert_eq!(
            driver_outputv(port, core::ptr::null_mut(), 0, core::ptr::null_mut(), 0),
            -1
        );
        assert_eq!(
            driver_outputv(core::ptr::null_mut(), core::ptr::null_mut(), 0, &mut ev, 0),
            -1
        );
    }
}

#[test]
fn erl_drv_output_term_decodes_and_forwards_a_tuple() {
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let name = CString::new("a").expect("no interior NUL");

    // `{a, 7}`
    let spec = [
        ERL_DRV_ATOM,
        term_data(name.as_ptr() as usize),
        ERL_DRV_INT,
        7,
        ERL_DRV_TUPLE,
        2,
    ];
    // SAFETY: `port` is a live env handle (only compared and pointer-encoded).
    let port_id = unsafe { driver_mk_port(port) };
    assert_ne!(
        port_id, driver_term_nil,
        "a live port has a term-data handle"
    );

    // SAFETY: `port_id` encodes the live `port` and `spec` is a live six-word array.
    let rc = unsafe { erl_drv_output_term(port_id, spec.as_ptr() as *mut ErlDrvTermData, 6) };
    assert_eq!(rc, 0);
    assert_eq!(host.sent.len(), 1);
    let (to, term) = host.sent[0];
    assert_eq!(to, None, "the `_output_term` forms target the port owner");
    // SAFETY: `term` is a live 2-tuple (asserted below).
    unsafe {
        assert!(is_tuple(term));
        assert_eq!(tuple_size(term), 2);
        assert_eq!(tuple_element(term, 1), make_small(7));
    }
    // SAFETY: element 0 of a 2-tuple.
    let first = unsafe { tuple_element(term, 0) };
    assert_eq!(host.atom_name(first), Some("a"));

    // The deprecated `driver_output_term` form behaves identically. Each call decodes the spec
    // afresh, so the tuple is a *new* heap box: the comparison is structural, not raw-word.
    host.sent.clear();
    // SAFETY: `port` is live and `spec` is the same live array.
    let rc = unsafe { driver_output_term(port, spec.as_ptr() as *mut ErlDrvTermData, 6) };
    assert_eq!(rc, 0);
    assert_eq!(host.sent.len(), 1);
    assert_eq!(host.sent[0].0, None);
    assert_eq!(
        atom_int_pair(&host, host.sent[0].1),
        atom_int_pair(&host, term)
    );

    // The `_send_term` forms pass the destination term through.
    host.sent.clear();
    let dest = make_small(9);
    // SAFETY: `port_id` encodes the live `port`; `dest` is a term word (the C's `Eterm to`).
    let rc = unsafe {
        erl_drv_send_term(
            port_id,
            dest.raw() as ErlDrvTermData,
            spec.as_ptr() as *mut ErlDrvTermData,
            6,
        )
    };
    assert_eq!(rc, 0);
    assert_eq!(host.sent.len(), 1);
    assert_eq!(host.sent[0].0, Some(dest));
    assert_eq!(
        atom_int_pair(&host, host.sent[0].1),
        Some((Some("a"), make_small(7)))
    );
    host.sent.clear();
    // SAFETY: as above, through the deprecated `ErlDrvPort` form.
    let rc = unsafe {
        driver_send_term(
            port,
            dest.raw() as ErlDrvTermData,
            spec.as_ptr() as *mut ErlDrvTermData,
            6,
        )
    };
    assert_eq!(rc, 0);
    assert_eq!(host.sent.len(), 1);
    assert_eq!(host.sent[0].0, Some(dest));
    assert_eq!(
        atom_int_pair(&host, host.sent[0].1),
        Some((Some("a"), make_small(7)))
    );

    // A malformed spec never reaches the host.
    host.sent.clear();
    // Two values left on the stack.
    let bad = [ERL_DRV_INT, 1, ERL_DRV_INT, 2];
    // SAFETY: `bad` is a live four-word array.
    let rc = unsafe { erl_drv_output_term(port_id, bad.as_ptr() as *mut ErlDrvTermData, 4) };
    assert_eq!(rc, -1);
    assert!(host.sent.is_empty());

    // A non-empty spec behind a null array is refused, not dereferenced.
    // SAFETY: the null array is checked before any read.
    let rc = unsafe { erl_drv_output_term(port_id, core::ptr::null_mut(), 2) };
    assert_eq!(rc, -1);
    // A host without an atom table cannot intern, so the spec cannot be built.
    let mut bare = MinimalDriverHost::new();
    let mut bare_env = DriverEnv::new(&mut bare);
    let bare_port = bare_env.as_port();
    // SAFETY: `bare_env` is live and `spec` is a live six-word array.
    let rc = unsafe {
        erl_drv_output_term(
            driver_mk_port(bare_port),
            spec.as_ptr() as *mut ErlDrvTermData,
            6,
        )
    };
    assert_eq!(rc, -1, "an uninternable atom fails the spec");
}

#[test]
fn driver_connected_and_caller_return_term_words_or_nil() {
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    // A host with no known pid reports `nil`: the C's "no process" answer.
    // SAFETY: `port` is live; neither call takes another pointer.
    unsafe {
        assert_eq!(driver_connected(port), driver_term_nil);
        assert_eq!(driver_caller(port), driver_term_nil);
    }

    // The host's pids come back as raw term words.
    let connected = make_small(11);
    let caller = make_small(12);
    host.connected = Some(connected);
    host.caller = Some(caller);
    // SAFETY: as above.
    unsafe {
        assert_eq!(driver_connected(port), connected.raw() as ErlDrvTermData);
        assert_eq!(driver_caller(port), caller.raw() as ErlDrvTermData);
    }
}

#[test]
fn driver_mk_atom_returns_a_pointer_the_atom_spec_interns() {
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let name = CString::new("ok").expect("no interior NUL");

    // SAFETY: `name` is a NUL-terminated C string.
    let td = unsafe { driver_mk_atom(name.as_ptr() as *mut c_char) };
    assert_eq!(td, name.as_ptr() as usize as ErlDrvTermData);
    // That is exactly the `ERL_DRV_ATOM` operand `decode_term_spec` interns.
    let term = decode_term_spec(&mut env, &[ERL_DRV_ATOM, td]).expect("a valid atom spec");
    assert_eq!(host.atom_name(term), Some("ok"));
}

#[test]
fn driver_mk_port_encodes_the_handle_and_nil_stays_nil() {
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    // SAFETY: `port` is a live handle; a null handle is only compared.
    unsafe {
        assert_eq!(driver_mk_port(port), port as usize as ErlDrvTermData);
        assert_eq!(driver_mk_port(core::ptr::null_mut()), driver_term_nil);
    }
}

#[test]
fn timers_select_and_async_delegate_to_the_host() {
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    // SAFETY: `port` is live; no other pointer is dereferenced by these two calls.
    unsafe {
        assert_eq!(driver_set_timer(port, 250), 0);
        assert_eq!(host.timers_set, vec![250]);
        assert_eq!(driver_cancel_timer(port), 0);
        assert_eq!(host.timers_cancelled, 1);
    }

    let mut left: c_ulong = 0;
    // SAFETY: `port` is live and `left` is a writable `unsigned long`.
    unsafe {
        // No timer pending: the host's `None` becomes the C's failure code.
        assert_eq!(driver_read_timer(port, &mut left), -1);
        assert_eq!(left, 0, "a failed read leaves the out-parameter alone");
    }
    host.timer_left = Some(123);
    // SAFETY: as above.
    unsafe {
        assert_eq!(driver_read_timer(port, &mut left), 0);
        assert_eq!(left, 123);
        // A null out-parameter is refused instead of dereferenced.
        assert_eq!(driver_read_timer(port, core::ptr::null_mut()), -1);
    }

    // The event is an opaque handle the host only records.
    let event: ErlDrvEvent = 8usize as *mut c_void as ErlDrvEvent;
    // SAFETY: `port` is live and `event` is only compared/stored by the host.
    unsafe {
        assert_eq!(driver_select(port, event, ERL_DRV_READ, 1), 0);
        assert_eq!(driver_select(port, event, ERL_DRV_READ, 0), 0);
    }
    assert_eq!(
        host.selected,
        vec![(8, ERL_DRV_READ, 1), (8, ERL_DRV_READ, 0)]
    );

    let mut key: c_uint = 0;
    // SAFETY: `port` is live; the job arguments are passed straight through to the host.
    let rc = unsafe { driver_async(port, &mut key, None, 0x1234usize as *mut c_void, None) };
    assert_eq!(rc, 5, "the host's queue handle comes back verbatim");
    assert_eq!(host.async_calls, vec![(false, 0x1234)]);
    assert_eq!(key, 0, "the key operand is the host's, not ours");
}

#[test]
fn defaulted_host_methods_report_the_c_failure_code() {
    let mut host = MinimalDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let mut left: c_ulong = 0;
    // SAFETY: `port` is live; every other operand is null or a local out-parameter.
    unsafe {
        assert_eq!(
            driver_select(port, core::ptr::null_mut(), ERL_DRV_READ, 1),
            -1
        );
        assert_eq!(driver_set_timer(port, 10), -1);
        assert_eq!(driver_cancel_timer(port), -1);
        assert_eq!(driver_read_timer(port, &mut left), -1);
        assert_eq!(left, 0);
        assert_eq!(
            driver_async(
                port,
                core::ptr::null_mut(),
                None,
                core::ptr::null_mut(),
                None
            ),
            -1
        );
        assert_eq!(driver_connected(port), driver_term_nil);
        assert_eq!(driver_caller(port), driver_term_nil);
        assert_eq!(driver_output(port, core::ptr::null_mut(), 0), -1);
    }
}

#[test]
fn null_ports_return_the_failure_code_without_panicking() {
    // SAFETY: every pointer here is null or empty, which the functions check before use.
    unsafe {
        let null_port: ErlDrvPort = core::ptr::null_mut();
        assert_eq!(
            driver_output(null_port, core::ptr::null_mut(), 0),
            -1,
            "driver_output"
        );
        assert_eq!(
            driver_output2(
                null_port,
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
                0
            ),
            -1,
            "driver_output2"
        );
        assert_eq!(
            driver_output_binary(
                null_port,
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
                0,
                0
            ),
            -1,
            "driver_output_binary"
        );
        assert_eq!(
            driver_outputv(
                null_port,
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
                0
            ),
            -1,
            "driver_outputv"
        );
        assert_eq!(
            driver_output_term(null_port, core::ptr::null_mut(), 0),
            -1,
            "driver_output_term"
        );
        assert_eq!(
            driver_send_term(null_port, driver_term_nil, core::ptr::null_mut(), 0),
            -1,
            "driver_send_term"
        );
        assert_eq!(
            erl_drv_output_term(0, core::ptr::null_mut(), 0),
            -1,
            "erl_drv_output_term"
        );
        assert_eq!(
            erl_drv_send_term(0, driver_term_nil, core::ptr::null_mut(), 0),
            -1,
            "erl_drv_send_term"
        );
        assert_eq!(driver_connected(null_port), driver_term_nil);
        assert_eq!(driver_caller(null_port), driver_term_nil);
        assert_eq!(driver_mk_port(null_port), driver_term_nil);
        assert_eq!(driver_set_timer(null_port, 0), -1, "driver_set_timer");
        assert_eq!(driver_cancel_timer(null_port), -1, "driver_cancel_timer");
        assert_eq!(
            driver_read_timer(null_port, core::ptr::null_mut()),
            -1,
            "driver_read_timer"
        );
        assert_eq!(
            driver_select(null_port, core::ptr::null_mut(), ERL_DRV_READ, 1),
            -1,
            "driver_select"
        );
        assert_eq!(
            driver_async(
                null_port,
                core::ptr::null_mut(),
                None,
                core::ptr::null_mut(),
                None
            ),
            -1,
            "driver_async"
        );
        // A negative length is malformed even without a port.
        assert_eq!(
            driver_output_term(null_port, core::ptr::null_mut(), -1),
            -1,
            "a negative spec length"
        );
    }
}

#[test]
fn driver_term_data_round_trips_through_the_table() {
    // The table's port-carrying slots are the very functions the tests above call: driving one
    // through the table proves the frozen table and the `#[no_mangle]` symbol agree.
    let t = driver_api_table();
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();

    let output = t.driver_output.expect("driver_output is populated");
    let mut body = *b"via the table";
    // SAFETY: the table slot is `driver_output` itself; `port`/`body` are live.
    let rc = unsafe { output(port, body.as_mut_ptr() as *mut c_char, body.len()) };
    assert_eq!(rc, 0);
    assert_eq!(host.output, b"via the table");

    let mk_port = t.driver_mk_port.expect("driver_mk_port is populated");
    // SAFETY: `port` is a live handle.
    let port_id = unsafe { mk_port(port) };
    let name = CString::new("x").expect("no interior NUL");
    let spec = [ERL_DRV_ATOM, term_data(name.as_ptr() as usize)];
    let output_term = t
        .erl_drv_output_term
        .expect("erl_drv_output_term is populated");
    // SAFETY: `port_id` encodes the live port and `spec` is a live two-word array.
    let rc = unsafe { output_term(port_id, spec.as_ptr() as *mut ErlDrvTermData, 2) };
    assert_eq!(rc, 0);
    assert_eq!(host.sent, vec![(None, make_atom(host.atoms.intern("x")))]);
}
// The split seam (`DriverHost::output2`): a header prefix plus a *distinguishable* body tail.

/// The C's `(buf, len)` operand pair — `None` is a **null** `buf` with `len == 0`, the case the seam
/// must keep distinct from an empty one.
fn body_operand(body: Option<&mut [u8]>) -> (*mut c_char, ErlDrvSizeT) {
    match body {
        Some(bytes) => (bytes.as_mut_ptr() as *mut c_char, bytes.len()),
        None => (core::ptr::null_mut(), 0),
    }
}

/// `driver_output(port, buf, len)` over the operand pair.
fn call_output(port: ErlDrvPort, body: Option<&mut [u8]>) -> c_int {
    let (ptr, len) = body_operand(body);
    // SAFETY: `port` is a live env handle and the buffer, when present, declares its own length.
    unsafe { driver_output(port, ptr, len) }
}

/// `driver_output2(port, hbuf, hlen, buf, len)` over the operand pairs.
fn call_output2(port: ErlDrvPort, header: &mut [u8], body: Option<&mut [u8]>) -> c_int {
    let (ptr, len) = body_operand(body);
    // SAFETY: `port` is live and both buffers declare their own lengths.
    unsafe { driver_output2(port, header.as_mut_ptr() as *mut c_char, header.len(), ptr, len) }
}

/// `driver_output_binary(port, hbuf, hlen, bin, offs, len)` over the operand pairs.
fn call_output_binary(
    port: ErlDrvPort,
    header: &mut [u8],
    bin: *mut ErlDrvBinary,
    offs: ErlDrvSizeT,
    len: ErlDrvSizeT,
) -> c_int {
    let head = (header.as_mut_ptr() as *mut c_char, header.len());
    // SAFETY: `port` is live, `header` declares its own length and `bin` covers `offs + len`.
    unsafe { driver_output_binary(port, head.0, head.1, bin, offs, len) }
}

/// `driver_outputv(port, hbuf, hlen, ev, skip)` over the operand pair and a live vector.
fn call_outputv(port: ErlDrvPort, header: &mut [u8], ev: &mut ErlIOVec, skip: ErlDrvSizeT) -> c_int {
    // SAFETY: `port` is live, `header` declares its own length and `ev` describes live buffers.
    unsafe { driver_outputv(port, header.as_mut_ptr() as *mut c_char, header.len(), ev, skip) }
}

/// An [`ErlIOVec`] over `iov`, declaring `size` bytes in total (the C's `vec->size`).
fn iovec(iov: &mut [libc::iovec], size: ErlDrvSizeT) -> ErlIOVec {
    ErlIOVec {
        vsize: iov.len() as c_int,
        size,
        iov: iov.as_mut_ptr(),
        binv: core::ptr::null_mut(),
    }
}

/// The C's `[hbuf… | body]` boundary, which only `output2` can carry, survives `driver_output2`: the
/// header stays a prefix, a null body buffer stays "no tail at all" and a non-null empty one stays a
/// present, empty tail (`<<>>` in `[binary]` port mode) — three different terms.
#[test]
fn driver_output2_keeps_the_header_and_body_split() {
    let mut host = SplitHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let mut head = *b"HDR";
    let mut body = *b"abc";

    assert_eq!(call_output2(port, &mut head, Some(&mut body[..])), 0);
    assert_eq!(host.splits, vec![(b"HDR".to_vec(), Some(b"abc".to_vec()))]);
    assert!(host.flattened.is_empty(), "a split-aware host never joins");

    // A null body buffer is the C's `buf == NULL`: a header and **no** tail.
    host.splits.clear();
    let mut head = *b"HH";
    assert_eq!(call_output2(port, &mut head, None), 0);
    assert_eq!(host.splits, vec![(b"HH".to_vec(), None)]);

    // A non-null buffer with a zero length is a **present, empty** tail.
    host.splits.clear();
    assert_eq!(call_output2(port, &mut head, Some(&mut body[..0])), 0);
    assert_eq!(host.splits, vec![(b"HH".to_vec(), Some(Vec::new()))]);

    // A null body buffer with a non-zero length is refused, never dereferenced.
    host.splits.clear();
    let (null, three) = (core::ptr::null_mut(), 3);
    // SAFETY: the null buffer is checked before any read.
    let rc =
        unsafe { driver_output2(port, head.as_mut_ptr() as *mut c_char, head.len(), null, three) };
    assert_eq!(rc, -1);
    assert!(host.splits.is_empty());

    // Two replies in one callback are two records, never merged.
    let mut more = *b"H2";
    assert_eq!(call_output2(port, &mut head, Some(&mut body[..1])), 0);
    assert_eq!(call_output2(port, &mut more, Some(&mut body[..1])), 0);
    assert_eq!(
        host.splits,
        vec![
            (b"HH".to_vec(), Some(b"a".to_vec())),
            (b"H2".to_vec(), Some(b"a".to_vec())),
        ]
    );
}

/// `driver_output` is `driver_output2(ix, NULL, 0, buf, len)` (`io.c:6442`): no header, and the buf
/// **pointer** — never `len` — decides whether a tail exists.
#[test]
fn driver_output_distinguishes_a_null_buffer_from_an_empty_one() {
    let mut host = SplitHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let mut body = *b"abc";

    assert_eq!(call_output(port, Some(&mut body[..])), 0);
    assert_eq!(host.splits, vec![(Vec::new(), Some(b"abc".to_vec()))]);

    // `driver_output(port, NULL, 0)`: no header, no tail — the proper empty list.
    host.splits.clear();
    assert_eq!(call_output(port, None), 0);
    assert_eq!(host.splits, vec![(Vec::new(), None)]);

    // Non-null with a zero length: a present, empty tail — a *different* reply.
    host.splits.clear();
    assert_eq!(call_output(port, Some(&mut body[..0])), 0);
    assert_eq!(host.splits, vec![(Vec::new(), Some(Vec::new()))]);
}

/// `driver_output_binary`'s payload sub-range is always a present body, so a zero-length range is
/// `Some(&[])` (the C's `<<>>`), not a header-only reply; an out-of-range one stays refused.
#[test]
fn driver_output_binary_keeps_the_header_split() {
    let mut host = SplitHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    // SAFETY: the driver-binary allocator is a malloc wrapper; `bin` is freed below.
    let bin = unsafe { driver_alloc_binary(8) };
    binary_payload(bin, 8).copy_from_slice(b"abcdefgh");
    let mut head = *b"H";

    assert_eq!(call_output_binary(port, &mut head, bin, 2, 3), 0);
    assert_eq!(host.splits, vec![(b"H".to_vec(), Some(b"cde".to_vec()))]);

    host.splits.clear();
    assert_eq!(call_output_binary(port, &mut head, bin, 8, 0), 0);
    assert_eq!(host.splits, vec![(b"H".to_vec(), Some(Vec::new()))]);

    // A sub-range past the payload is refused (the C would read past it).
    host.splits.clear();
    assert_eq!(call_output_binary(port, &mut head, bin, 6, 3), -1);
    assert!(host.splits.is_empty());

    // SAFETY: `bin` came from `driver_alloc_binary` and is freed exactly once.
    unsafe { driver_free_binary(bin) };
}

/// `driver_outputv`'s **last** remaining vector is the improper tail (`deliver_vec_message`,
/// `io.c:3502-3621`) and every preceding one belongs to the header; `size <= skip` is the C's
/// header-only reply, *not* an empty tail.
#[test]
fn driver_outputv_keeps_the_last_vector_as_the_tail() {
    let mut host = SplitHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let mut head = *b"H";
    let mut first = *b"abcd";
    let mut second = *b"efgh";
    let mut iov = [
        libc::iovec { iov_base: first.as_mut_ptr() as *mut c_void, iov_len: 4 },
        libc::iovec { iov_base: second.as_mut_ptr() as *mut c_void, iov_len: 4 },
    ];
    let mut ev = iovec(&mut iov, 8);

    assert_eq!(call_outputv(port, &mut head, &mut ev, 0), 0);
    assert_eq!(
        host.splits,
        vec![(b"Habcd".to_vec(), Some(b"efgh".to_vec()))],
        "the last vector is the tail, the earlier one joins the header"
    );

    host.splits.clear();
    assert_eq!(call_outputv(port, &mut head, &mut ev, 5), 0);
    assert_eq!(host.splits, vec![(b"H".to_vec(), Some(b"fgh".to_vec()))]);

    // `skip == size`: the C's early `driver_output2(ix, hbuf, hlen, NULL, 0)` — no tail at all.
    host.splits.clear();
    assert_eq!(call_outputv(port, &mut head, &mut ev, 8), 0);
    assert_eq!(host.splits, vec![(b"H".to_vec(), None)]);

    // A trailing zero-length vector is a present, empty tail (the C's `<<>>`).
    let mut short = *b"ab";
    let mut iov = [
        libc::iovec { iov_base: short.as_mut_ptr() as *mut c_void, iov_len: 2 },
        libc::iovec { iov_base: short.as_mut_ptr() as *mut c_void, iov_len: 0 },
    ];
    let mut ev = iovec(&mut iov, 2);
    host.splits.clear();
    assert_eq!(call_outputv(port, &mut head, &mut ev, 0), 0);
    assert_eq!(host.splits, vec![(b"Hab".to_vec(), Some(Vec::new()))]);
}

/// The default [`DriverHost::output2`] is byte-identical to the pre-hook behaviour: for every
/// header/body shape it makes **exactly one** [`DriverHost::output`] call carrying header-then-body,
/// so a `driver_output*` thunk and a direct `output2` reply with the same single call.
#[test]
fn the_default_output2_makes_exactly_one_output_call() {
    let mut host = TestDriverHost::new();
    let mut env = DriverEnv::new(&mut host);
    let port = env.as_port();
    let mut head = *b"HDR";
    let mut body = *b"abc";

    assert_eq!(call_output2(port, &mut head, Some(&mut body[..])), 0);
    assert_eq!(host.output2(port, b"HDR", Some(b"abc")), 0);
    assert_eq!(host.calls, 2);
    assert_eq!(host.output, b"HDRabcHDRabc");

    // Each remaining shape is one call too, and an empty side never adds bytes.
    host.output.clear();
    host.calls = 0;
    assert_eq!(host.output2(port, b"", Some(b"b")), 0);
    assert_eq!(host.output2(port, b"HDR", None), 0);
    assert_eq!(host.output2(port, b"", None), 0);
    assert_eq!(host.output2(port, b"HDR", Some(b"")), 0);
    assert_eq!(host.calls, 4);
    assert_eq!(host.output, b"bHDRHDR");
}
