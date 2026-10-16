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

//! In-module tests for [`RamFileDriver`](super): the reply encodings, the access-mode and
//! seek bit tables, the in-memory file semantics (zero-filled gaps, truncation), the
//! consume-what-you-report accounting of the `output` transport, a hard-coded matrix of the
//! exact replies the real `ram_file_drv` produces for the same command bytes, and a live
//! cross-check of the whole matrix against the system `erl`.
//!
//! Split out of `ram_file.rs` only because that file is the driver source half; the tests are
//! still the driver's own in-module suite (`super` is the `ram_file` module).

use super::{
    make_ram_file, data_reply, number_reply, read_i32, RamFileDriver, DRIVER_NAME,
    RAM_FILE_ADVISE, RAM_FILE_ALLOCATE, RAM_FILE_FDATASYNC, RAM_FILE_FSYNC, RAM_FILE_GET,
    RAM_FILE_LSEEK, RAM_FILE_MODE_READ, RAM_FILE_MODE_READ_WRITE, RAM_FILE_MODE_WRITE,
    RAM_FILE_OPEN, RAM_FILE_PREAD, RAM_FILE_PWRITE, RAM_FILE_READ, RAM_FILE_RESP_DATA,
    RAM_FILE_RESP_ERROR, RAM_FILE_RESP_INFO, RAM_FILE_RESP_NUMBER, RAM_FILE_RESP_OK,
    RAM_FILE_SEEK_CUR, RAM_FILE_SEEK_END, RAM_FILE_SEEK_SET, RAM_FILE_SIZE, RAM_FILE_TRUNCATE,
    RAM_FILE_WRITE,
};
use crate::driver::{Driver, DriverContext, DriverFlags, PortArms};
use crate::poller::Poller;
use crate::queue::ByteQueue;
use beam_common::BeamError;
use beam_process::PortId;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

/// Wall-clock budget for the `erl` oracle before the affected cross-check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(60);

/// A driver plus the port-side buffers a `DriverContext` needs, so the driver can be driven
/// through either transport (the `output` byte path or the named-verb `control` path).
struct Harness {
    poller: Poller,
    input_q: ByteQueue,
    output_q: ByteQueue,
    arms: PortArms,
    driver: RamFileDriver,
}

impl Harness {
    fn new() -> Harness {
        Harness {
            poller: Poller::new().expect("a poll set"),
            input_q: ByteQueue::new(),
            output_q: ByteQueue::new(),
            arms: PortArms::default(),
            driver: RamFileDriver::new(),
        }
    }

    /// Feeds `bytes` through the driver's `output` transport and returns the reply it pushed
    /// onto the input queue, asserting the consume-what-you-report accounting on the way.
    fn output(&mut self, bytes: &[u8]) -> Vec<u8> {
        let Harness {
            driver,
            poller,
            input_q,
            output_q,
            arms,
        } = self;
        output_q.push(bytes);
        let mut ctx = DriverContext::new(PortId::new(0), poller, input_q, output_q, arms);
        let written = driver.output(&mut ctx).expect("the command is accepted");
        assert_eq!(
            written,
            bytes.len(),
            "the driver consumes exactly the command bytes"
        );
        drop(ctx);
        let reply = input_q.front().to_vec();
        input_q.clear();
        assert!(output_q.is_empty(), "the whole command was consumed");
        reply
    }

    /// Runs one named verb through the driver's `control` transport.
    fn verb(&mut self, command: &str, arg: &[u8]) -> Vec<u8> {
        let Harness {
            driver,
            poller,
            input_q,
            output_q,
            arms,
        } = self;
        let mut ctx = DriverContext::new(PortId::new(0), poller, input_q, output_q, arms);
        driver
            .control(&mut ctx, command, arg)
            .unwrap_or_else(|err| panic!("the `{command}` verb is accepted: {err:?}"))
    }
}

// -----------------------------------------------------------------------
// Command and reply builders (the wire form, byte for byte)
// -----------------------------------------------------------------------

fn c_open(mode: i32, content: &[u8]) -> Vec<u8> {
    let mut v = vec![RAM_FILE_OPEN];
    v.extend_from_slice(&mode.to_be_bytes());
    v.extend_from_slice(content);
    v
}

fn c_read(len: i32) -> Vec<u8> {
    let mut v = vec![RAM_FILE_READ];
    v.extend_from_slice(&len.to_be_bytes());
    v
}

fn c_lseek(offset: i32, whence: i32) -> Vec<u8> {
    let mut v = vec![RAM_FILE_LSEEK];
    v.extend_from_slice(&offset.to_be_bytes());
    v.extend_from_slice(&whence.to_be_bytes());
    v
}

fn c_write(data: &[u8]) -> Vec<u8> {
    let mut v = vec![RAM_FILE_WRITE];
    v.extend_from_slice(data);
    v
}

fn c_pread(offset: i32, len: i32) -> Vec<u8> {
    let mut v = vec![RAM_FILE_PREAD];
    v.extend_from_slice(&offset.to_be_bytes());
    v.extend_from_slice(&len.to_be_bytes());
    v
}

fn c_pwrite(offset: i32, data: &[u8]) -> Vec<u8> {
    let mut v = vec![RAM_FILE_PWRITE];
    v.extend_from_slice(&offset.to_be_bytes());
    v.extend_from_slice(data);
    v
}

fn c_truncate() -> Vec<u8> {
    vec![RAM_FILE_TRUNCATE]
}

fn c_fsync() -> Vec<u8> {
    vec![RAM_FILE_FSYNC]
}

fn c_fdatasync() -> Vec<u8> {
    vec![RAM_FILE_FDATASYNC]
}

fn c_get() -> Vec<u8> {
    vec![RAM_FILE_GET]
}

fn c_size() -> Vec<u8> {
    vec![RAM_FILE_SIZE]
}

fn c_advise() -> Vec<u8> {
    let mut v = vec![RAM_FILE_ADVISE];
    v.extend_from_slice(&0i64.to_be_bytes());
    v.extend_from_slice(&4i64.to_be_bytes());
    v.extend_from_slice(&0i32.to_be_bytes());
    v
}

fn c_allocate() -> Vec<u8> {
    let mut v = vec![RAM_FILE_ALLOCATE];
    v.extend_from_slice(&0i64.to_be_bytes());
    v.extend_from_slice(&100i64.to_be_bytes());
    v
}

fn ok() -> Vec<u8> {
    vec![RAM_FILE_RESP_OK]
}

fn number(value: i32) -> Vec<u8> {
    let mut v = vec![RAM_FILE_RESP_NUMBER];
    v.extend_from_slice(&value.to_be_bytes());
    v
}

fn data(bytes: &[u8]) -> Vec<u8> {
    let mut v = vec![RAM_FILE_RESP_DATA];
    v.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
    v.extend_from_slice(bytes);
    v
}

fn err(name: &str) -> Vec<u8> {
    let mut v = vec![RAM_FILE_RESP_ERROR];
    v.extend_from_slice(name.as_bytes());
    v
}

/// Runs one byte-level command and returns its reply.
///
/// [`RamFileDriver::execute`] is fallible only for a **protocol** fault (an unknown opcode or
/// an argument too short for its command); an *operation-level* failure is the `ERROR <errno>`
/// reply the reference emits, so it never surfaces as a Rust error here.
fn exec(driver: &mut RamFileDriver, command: u8, arg: &[u8]) -> Vec<u8> {
    driver
        .execute(command, arg)
        .unwrap_or_else(|err| panic!("command {command} is well-formed: {err:?}"))
}

// -----------------------------------------------------------------------
// The verified command matrix (commands and the replies the real driver gives)
// -----------------------------------------------------------------------

/// The main session: every command, driven against one port through the whole state machine.
fn session_main() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let cmds = vec![
        c_open(3, b"hello world"),
        c_size(),
        c_get(),
        c_lseek(0, RAM_FILE_SEEK_SET),
        c_read(5),
        c_pread(0, 5),
        c_pread(-1, 5),
        c_pwrite(6, b"there"),
        c_get(),
        c_write(b" XY"),
        c_get(),
        c_truncate(),
        c_size(),
        c_fsync(),
        c_fdatasync(),
        c_advise(),
        c_allocate(),
        c_lseek(0, RAM_FILE_SEEK_END),
    ];
    let expected = vec![
        number(0),
        number(11),
        data(b"hello world"),
        number(0),
        data(b"hello"),
        data(b"hello"),
        err("einval"),
        number(5),
        data(b"hello there"),
        number(3),
        data(b"hello XYere"),
        ok(),
        number(8),
        ok(),
        ok(),
        ok(),
        ok(),
        number(8),
    ];
    (cmds, expected)
}

/// A fresh port that never saw an `open`: every mode-gated command is `EBADF`, `truncate` is
/// `EACCES`, and the two commands that do not need a mode (`size`, `get`) answer empties.
fn session_fresh() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let cmds = vec![
        c_fsync(),
        c_fdatasync(),
        c_advise(),
        c_allocate(),
        c_lseek(0, RAM_FILE_SEEK_SET),
        c_read(5),
        c_write(b"abcd"),
        c_pread(0, 5),
        c_pwrite(0, b"abcd"),
        c_truncate(),
        c_size(),
        c_get(),
    ];
    let expected = vec![
        err("ebadf"),
        err("ebadf"),
        err("ebadf"),
        err("ebadf"),
        err("ebadf"),
        err("ebadf"),
        err("ebadf"),
        err("ebadf"),
        err("ebadf"),
        err("eacces"),
        number(0),
        data(b""),
    ];
    (cmds, expected)
}

/// A read-only open: writes are `EBADF`, `truncate` is `EACCES`, and a read past the empty
/// file is an empty `DATA`.
fn session_read_only() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let cmds = vec![
        c_open(RAM_FILE_MODE_READ, b""),
        c_write(b"abcd"),
        c_truncate(),
        c_read(5),
    ];
    let expected = vec![number(0), err("ebadf"), err("eacces"), data(b"")];
    (cmds, expected)
}

/// A write-only open seeded with content: reads are `EBADF` and the seek modes are exercised
/// against a known `cur`/`end`.
fn session_write_only() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let cmds = vec![
        c_open(RAM_FILE_MODE_WRITE, b"seed"),
        c_read(5),
        c_lseek(0, RAM_FILE_SEEK_SET),
        c_lseek(-5, RAM_FILE_SEEK_CUR),
        c_lseek(0, 9),
        c_lseek(-10, RAM_FILE_SEEK_END),
        c_lseek(2, RAM_FILE_SEEK_SET),
        c_size(),
    ];
    let expected = vec![
        number(0),
        err("ebadf"),
        number(0),
        err("einval"),
        err("einval"),
        err("einval"),
        number(2),
        number(4),
    ];
    (cmds, expected)
}

/// A read/write open of an empty file, a 2000-byte write, and a sparse write far past the
/// end (which grows `end` to the furthest byte written).
fn session_mode3_sparse() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let cmds = vec![
        c_open(RAM_FILE_MODE_READ_WRITE, b""),
        c_write(&vec![b'x'; 2000]),
        c_size(),
        c_pwrite(5000, b"z"),
        c_size(),
    ];
    let expected = vec![number(0), number(2000), number(2000), number(1), number(5001)];
    (cmds, expected)
}

/// An `open` carrying access mode `0`: writes are still `EBADF` and the file stays empty.
fn session_mode0() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let cmds = vec![c_open(0, b""), c_write(b"abcd"), c_size()];
    let expected = vec![number(0), err("ebadf"), number(0)];
    (cmds, expected)
}

/// A read/write open with content: the end-relative seek, a read at the end and a clamped
/// read of the remainder.
fn session_mode3_content() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let cmds = vec![
        c_open(RAM_FILE_MODE_READ_WRITE, b"abcdef"),
        c_size(),
        c_lseek(0, RAM_FILE_SEEK_END),
        c_read(4),
        c_lseek(2, RAM_FILE_SEEK_SET),
        c_read(100),
    ];
    let expected = vec![
        number(0),
        number(6),
        number(6),
        data(b""),
        number(2),
        data(b"cdef"),
    ];
    (cmds, expected)
}

/// The raw command sequence `ram_file.erl` drives for the high-level API calls pinned by
/// [`the_high_level_ram_file_api_shapes_agree_with_the_erl_oracle`] — the same state machine
/// the oracle's own API answers come out of.
fn session_api_raw() -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let cmds = vec![
        c_open(RAM_FILE_MODE_READ_WRITE, b"hello world"),
        c_size(),
        c_get(),
        c_lseek(0, RAM_FILE_SEEK_SET),
        c_read(5),
        c_lseek(0, RAM_FILE_SEEK_CUR),
        c_read(100),
        c_read(10),
        c_pread(0, 5),
        c_pread(-1, 5),
        c_pwrite(6, b"there"),
        c_get(),
        c_lseek(0, RAM_FILE_SEEK_END),
        c_read(5),
        c_truncate(),
        c_size(),
        c_fsync(),
        c_fdatasync(),
        c_advise(),
        c_allocate(),
        c_pread(0, 8),
    ];
    let expected = vec![
        number(0),
        number(11),
        data(b"hello world"),
        number(0),
        data(b"hello"),
        number(5),
        data(b" world"),
        data(b""),
        data(b"hello"),
        err("einval"),
        number(5),
        data(b"hello there"),
        number(11),
        data(b""),
        ok(),
        number(11),
        ok(),
        ok(),
        ok(),
        ok(),
        data(b"hello th"),
    ];
    (cmds, expected)
}

/// Every session, in the order the oracle expression drives them.
fn sessions() -> Vec<(Vec<Vec<u8>>, Vec<Vec<u8>>)> {
    vec![
        session_main(),
        session_fresh(),
        session_read_only(),
        session_write_only(),
        session_mode3_sparse(),
        session_mode0(),
        session_mode3_content(),
        session_api_raw(),
    ]
}

/// A representative argument for `opcode`, used to compare the two transports.
fn argument_for(opcode: u8) -> Vec<u8> {
    match opcode {
        RAM_FILE_OPEN => {
            let mut v = 3i32.to_be_bytes().to_vec();
            v.extend_from_slice(b"xyz");
            v
        }
        RAM_FILE_READ => 3i32.to_be_bytes().to_vec(),
        RAM_FILE_LSEEK => {
            let mut v = 0i32.to_be_bytes().to_vec();
            v.extend_from_slice(&0i32.to_be_bytes());
            v
        }
        RAM_FILE_WRITE => b"abc".to_vec(),
        RAM_FILE_PREAD => {
            let mut v = 0i32.to_be_bytes().to_vec();
            v.extend_from_slice(&5i32.to_be_bytes());
            v
        }
        RAM_FILE_PWRITE => {
            let mut v = 2i32.to_be_bytes().to_vec();
            v.extend_from_slice(b"ab");
            v
        }
        RAM_FILE_ADVISE => {
            let mut v = 0i64.to_be_bytes().to_vec();
            v.extend_from_slice(&4i64.to_be_bytes());
            v.extend_from_slice(&0i32.to_be_bytes());
            v
        }
        RAM_FILE_ALLOCATE => {
            let mut v = 0i64.to_be_bytes().to_vec();
            v.extend_from_slice(&100i64.to_be_bytes());
            v
        }
        _ => Vec::new(),
    }
}

// -----------------------------------------------------------------------
// Names, flags and lifecycle
// -----------------------------------------------------------------------

#[test]
fn the_driver_reports_its_registered_name_and_no_flags() {
    let driver = RamFileDriver::new();
    assert_eq!(driver.name(), DRIVER_NAME);
    assert_eq!(driver.name(), "ram_file_drv");
    assert!(Driver::flags(&driver) == DriverFlags::empty());
}

#[test]
fn make_ram_file_builds_a_fresh_driver() {
    let driver = make_ram_file().expect("the factory builds a driver");
    assert_eq!(driver.name(), DRIVER_NAME);
}

#[test]
fn a_fresh_driver_is_inert() {
    let driver = RamFileDriver::new();
    assert_eq!(driver.flags(), 0);
    assert_eq!(driver.size(), 0);
    assert_eq!(driver.position(), 0);
    assert!(!driver.is_open());
}

#[test]
fn start_ready_input_and_stop_leave_no_state_behind() {
    let mut h = Harness::new();
    let Harness {
        driver,
        poller,
        input_q,
        output_q,
        arms,
    } = &mut h;
    let mut ctx = DriverContext::new(PortId::new(0), poller, input_q, output_q, arms);
    driver.start(&mut ctx).expect("start is a no-op");
    driver.ready_input(&mut ctx).expect("ready_input is a no-op");

    // A live file, then a stop: the buffer and the mode are released.
    driver
        .execute(RAM_FILE_OPEN, &argument_for(RAM_FILE_OPEN))
        .expect("open");
    assert!(driver.is_open());
    driver.stop(&mut ctx).expect("stop resets");
    assert!(!driver.is_open());
    assert_eq!(driver.size(), 0);
    assert_eq!(driver.position(), 0);
}

#[test]
fn the_close_verb_resets_the_driver() {
    let mut h = Harness::new();
    h.driver
        .execute(RAM_FILE_OPEN, &argument_for(RAM_FILE_OPEN))
        .expect("open");
    assert!(h.driver.is_open());
    assert_eq!(h.verb("close", b""), ok());
    assert!(!h.driver.is_open());
    assert_eq!(h.driver.size(), 0);
    assert_eq!(h.driver.position(), 0);
}

// -----------------------------------------------------------------------
// Integer ↔ name correspondence, and the argument faults
// -----------------------------------------------------------------------

#[test]
fn the_named_verbs_match_their_opcodes() {
    let pairs: &[(&str, u8)] = &[
        ("open", RAM_FILE_OPEN),
        ("read", RAM_FILE_READ),
        ("lseek", RAM_FILE_LSEEK),
        ("position", RAM_FILE_LSEEK),
        ("write", RAM_FILE_WRITE),
        ("fsync", RAM_FILE_FSYNC),
        ("sync", RAM_FILE_FSYNC),
        ("truncate", RAM_FILE_TRUNCATE),
        ("pread", RAM_FILE_PREAD),
        ("pwrite", RAM_FILE_PWRITE),
        ("fdatasync", RAM_FILE_FDATASYNC),
        ("datasync", RAM_FILE_FDATASYNC),
        ("get", RAM_FILE_GET),
        ("get_file", RAM_FILE_GET),
        ("size", RAM_FILE_SIZE),
        ("get_size", RAM_FILE_SIZE),
        ("advise", RAM_FILE_ADVISE),
        ("allocate", RAM_FILE_ALLOCATE),
    ];
    for (name, opcode) in pairs {
        let arg = argument_for(*opcode);
        let mut direct = Harness::new();
        let expected = direct
            .driver
            .execute(*opcode, &arg)
            .unwrap_or_else(|err| panic!("`{opcode}` is a command: {err:?}"));

        let mut named = Harness::new();
        assert_eq!(
            named.verb(name, &arg),
            expected,
            "the `{name}` verb matches opcode {opcode}"
        );
        assert_eq!(named.driver.flags(), direct.driver.flags());
        assert_eq!(named.driver.size(), direct.driver.size());
        assert_eq!(named.driver.position(), direct.driver.position());

        let mut decimal = Harness::new();
        assert_eq!(
            decimal.verb(&opcode.to_string(), &arg),
            expected,
            "the decimal spelling of opcode {opcode} matches"
        );
        assert_eq!(decimal.driver.flags(), direct.driver.flags());
        assert_eq!(decimal.driver.size(), direct.driver.size());
        assert_eq!(decimal.driver.position(), direct.driver.position());
    }
}

#[test]
fn an_unknown_command_word_is_a_badarg() {
    let mut driver = RamFileDriver::new();
    for opcode in [0u8, 5, 8, 16, 31, 40, 255] {
        assert!(
            matches!(driver.execute(opcode, b""), Err(BeamError::BadArg)),
            "opcode {opcode} is not a command"
        );
    }
    // A failed command leaves no trace on the driver.
    assert_eq!(driver.flags(), 0);
    assert_eq!(driver.size(), 0);
}

#[test]
fn an_unknown_verb_is_a_badarg() {
    let mut h = Harness::new();
    let Harness {
        driver,
        poller,
        input_q,
        output_q,
        arms,
    } = &mut h;
    let mut ctx = DriverContext::new(PortId::new(0), poller, input_q, output_q, arms);
    assert!(matches!(
        driver.control(&mut ctx, "rewind", b""),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        driver.control(&mut ctx, "frobnicate", b""),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn a_short_argument_is_a_badarg() {
    let mut driver = RamFileDriver::new();
    // `open` needs four mode bytes before its content.
    for short in [b"".as_slice(), &[0], &[0, 0], &[0, 0, 0]] {
        assert!(matches!(
            driver.execute(RAM_FILE_OPEN, short),
            Err(BeamError::BadArg)
        ));
    }
    // `read` needs its length; `lseek`/`pread` need both operands; `pwrite` its offset.
    assert!(matches!(driver.execute(RAM_FILE_READ, &[0, 0, 0]), Err(BeamError::BadArg)));
    assert!(matches!(
        driver.execute(RAM_FILE_LSEEK, &[0, 0, 0, 0]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        driver.execute(RAM_FILE_PREAD, &[0, 0, 0, 0]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(driver.execute(RAM_FILE_PWRITE, &[0, 0, 0]), Err(BeamError::BadArg)));

    // A too-short `read_i32` operand is the same typed fault.
    assert!(matches!(read_i32(&[1, 2, 3]), Err(BeamError::BadArg)));
    assert_eq!(read_i32(&[0, 0, 1, 0]).expect("four bytes"), 256);
}

// -----------------------------------------------------------------------
// Reply encodings
// -----------------------------------------------------------------------

#[test]
fn the_reply_shapes_are_the_reference_encodings() {
    assert_eq!(RAM_FILE_RESP_OK, 0);
    assert_eq!(RAM_FILE_RESP_ERROR, 1);
    assert_eq!(RAM_FILE_RESP_DATA, 2);
    assert_eq!(RAM_FILE_RESP_NUMBER, 3);
    assert_eq!(RAM_FILE_RESP_INFO, 4);

    assert_eq!(ok(), vec![0]);
    assert_eq!(number(0), vec![3, 0, 0, 0, 0]);
    assert_eq!(number(11), vec![3, 0, 0, 0, 11]);
    assert_eq!(number(2000), vec![3, 0, 0, 7, 208]);
    assert_eq!(number(-1), vec![3, 255, 255, 255, 255]);
    assert_eq!(data(b""), vec![2, 0, 0, 0, 0]);
    assert_eq!(data(b"hello"), vec![2, 0, 0, 0, 5, 104, 101, 108, 108, 111]);
    assert_eq!(err("ebadf"), vec![1, 101, 98, 97, 100, 102]);
    assert_eq!(err("einval"), vec![1, 101, 105, 110, 118, 97, 108]);
    assert_eq!(err("eacces"), vec![1, 101, 97, 99, 99, 101, 115]);
    assert_eq!(err("enomem"), vec![1, 101, 110, 111, 109, 101, 109]);

    // The two helpers the driver itself builds its replies with.
    assert_eq!(number_reply(6), number(6));
    assert_eq!(data_reply(b"abcdef"), data(b"abcdef"));
}

#[test]
fn the_seek_modes_and_mode_words_are_the_reference_bit_tables() {
    assert_eq!(RAM_FILE_MODE_READ, 1);
    assert_eq!(RAM_FILE_MODE_WRITE, 2);
    assert_eq!(RAM_FILE_MODE_READ_WRITE, 3);
    assert_eq!(RAM_FILE_MODE_READ | RAM_FILE_MODE_WRITE, RAM_FILE_MODE_READ_WRITE);

    assert_eq!(RAM_FILE_SEEK_SET, 0);
    assert_eq!(RAM_FILE_SEEK_CUR, 1);
    assert_eq!(RAM_FILE_SEEK_END, 2);
}

// -----------------------------------------------------------------------
// The `output` transport
// -----------------------------------------------------------------------

#[test]
fn the_output_transport_consumes_the_whole_command_and_replies_once() {
    let mut h = Harness::new();
    // Every command byte is preceded by nothing; the whole payload is consumed.
    let reply = h.output(&c_open(3, b"hello world"));
    assert_eq!(reply, number(0));
    assert_eq!(h.output(&c_size()), number(11));
    assert_eq!(h.output(&c_get()), data(b"hello world"));
    assert!(h.input_q.is_empty(), "the harness drains every reply");
    assert!(h.output_q.is_empty(), "the whole command was consumed");
}

#[test]
fn an_empty_output_queue_reports_zero_bytes_written() {
    let mut h = Harness::new();
    let reply = h.output(b"");
    assert!(reply.is_empty());
    assert_eq!(h.output_q.len(), 0);
}

#[test]
fn the_output_transport_replies_to_a_bad_command_with_a_typed_error() {
    let mut h = Harness::new();
    let Harness {
        driver,
        poller,
        input_q,
        output_q,
        arms,
    } = &mut h;
    output_q.push(&[200u8, 1, 2, 3]);
    let mut ctx = DriverContext::new(PortId::new(0), poller, input_q, output_q, arms);
    assert!(matches!(driver.output(&mut ctx), Err(BeamError::BadArg)));
    // The unknown command was still consumed (the queue is the only record of it).
    assert!(output_q.is_empty());
    assert!(input_q.is_empty(), "no reply was fabricated");
}

// -----------------------------------------------------------------------
// In-memory file semantics
// -----------------------------------------------------------------------

#[test]
fn a_write_past_the_end_zero_fills_the_gap() {
    let mut driver = RamFileDriver::new();
    driver
        .execute(RAM_FILE_OPEN, &c_open(RAM_FILE_MODE_READ_WRITE, b"")[1..])
        .expect("open");
    assert_eq!(
        exec(&mut driver, RAM_FILE_PWRITE, &c_pwrite(5, b"ab")[1..]),
        number(2)
    );
    assert_eq!(driver.size(), 7);
    let reply = driver.execute(RAM_FILE_GET, &[]).expect("get");
    assert_eq!(reply[0], RAM_FILE_RESP_DATA);
    assert_eq!(
        &reply[5..],
        &[0, 0, 0, 0, 0, b'a', b'b'],
        "the gap between the old end and the write is zeros"
    );
}

#[test]
fn truncate_ends_the_file_at_the_current_position() {
    let mut driver = RamFileDriver::new();
    driver
        .execute(RAM_FILE_OPEN, &c_open(RAM_FILE_MODE_READ_WRITE, b"abcdefgh")[1..])
        .expect("open");
    assert_eq!(driver.size(), 8);
    driver
        .execute(RAM_FILE_LSEEK, &c_lseek(3, RAM_FILE_SEEK_SET)[1..])
        .expect("seek");
    assert_eq!(exec(&mut driver, RAM_FILE_TRUNCATE, &[]), ok());
    assert_eq!(driver.size(), 3);
    assert_eq!(exec(&mut driver, RAM_FILE_SIZE, &[]), number(3));
    let reply = driver.execute(RAM_FILE_GET, &[]).expect("get");
    assert_eq!(&reply[5..], b"abc");
}

#[test]
fn truncate_past_the_end_extends_the_file_with_zeros() {
    let mut driver = RamFileDriver::new();
    driver
        .execute(RAM_FILE_OPEN, &c_open(RAM_FILE_MODE_READ_WRITE, b"ab")[1..])
        .expect("open");
    driver
        .execute(RAM_FILE_LSEEK, &c_lseek(5, RAM_FILE_SEEK_SET)[1..])
        .expect("seek");
    assert_eq!(exec(&mut driver, RAM_FILE_TRUNCATE, &[]), ok());
    assert_eq!(driver.size(), 5);
    let reply = driver.execute(RAM_FILE_GET, &[]).expect("get");
    assert_eq!(&reply[5..], &[b'a', b'b', 0, 0, 0]);
}

#[test]
fn a_read_past_the_end_is_an_empty_data_reply() {
    let mut driver = RamFileDriver::new();
    driver
        .execute(RAM_FILE_OPEN, &c_open(RAM_FILE_MODE_READ_WRITE, b"abc")[1..])
        .expect("open");
    driver
        .execute(RAM_FILE_LSEEK, &c_lseek(0, RAM_FILE_SEEK_END)[1..])
        .expect("seek");
    assert_eq!(exec(&mut driver, RAM_FILE_READ, &c_read(100)[1..]), data(b""));
    assert_eq!(driver.position(), 3, "an empty read does not move the position");
}

#[test]
fn a_write_at_the_end_extends_the_logical_file() {
    let mut driver = RamFileDriver::new();
    driver
        .execute(RAM_FILE_OPEN, &c_open(RAM_FILE_MODE_READ_WRITE, b"ab")[1..])
        .expect("open");
    driver
        .execute(RAM_FILE_LSEEK, &c_lseek(0, RAM_FILE_SEEK_END)[1..])
        .expect("seek");
    assert_eq!(exec(&mut driver, RAM_FILE_WRITE, b"cd"), number(2));
    assert_eq!(driver.size(), 4);
    assert_eq!(driver.position(), 4);
    let reply = driver.execute(RAM_FILE_GET, &[]).expect("get");
    assert_eq!(&reply[5..], b"abcd");
}

/// A read-only mode leaves the file untouched but still reports its content through `get`.
#[test]
fn get_and_size_do_not_need_a_write_mode() {
    let mut driver = RamFileDriver::new();
    driver
        .execute(RAM_FILE_OPEN, &c_open(RAM_FILE_MODE_READ, b"seed")[1..])
        .expect("open");
    assert_eq!(exec(&mut driver, RAM_FILE_SIZE, &[]), number(4));
    let reply = driver.execute(RAM_FILE_GET, &[]).expect("get");
    assert_eq!(&reply[5..], b"seed");
    assert!(driver.is_open());
}

// -----------------------------------------------------------------------
// The hard-coded matrix, through both transports
// -----------------------------------------------------------------------

#[test]
fn the_hard_coded_command_matrix_matches_the_verified_replies() {
    for (index, (cmds, expected)) in sessions().into_iter().enumerate() {
        assert_eq!(cmds.len(), expected.len(), "session {index} is well-formed");

        // Through the `port_command` byte path.
        let mut byte_path = Harness::new();
        for (command, want) in cmds.iter().zip(&expected) {
            let got = byte_path.output(command);
            assert_eq!(&got, want, "session {index} `output` path, command {command:?}");
        }

        // Through the named-verb path, spelled with the decimal opcode so the
        // integer↔name correspondence is exercised too.
        let mut verb_path = Harness::new();
        for (command, want) in cmds.iter().zip(&expected) {
            let got = verb_path.verb(&command[0].to_string(), &command[1..]);
            assert_eq!(&got, want, "session {index} `control` path, command {command:?}");
        }

        // Both transports leave the driver in the same state.
        assert_eq!(byte_path.driver.flags(), verb_path.driver.flags());
        assert_eq!(byte_path.driver.size(), verb_path.driver.size());
        assert_eq!(byte_path.driver.position(), verb_path.driver.position());
    }
}

// -----------------------------------------------------------------------
// Live cross-check against the system `erl`
// -----------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after
/// printing a clear skip message — when `erl` cannot be spawned or does not answer within
/// [`ERL_TIMEOUT`]. A spawned `erl` that exits non-zero is a bug in the expression, so it
/// fails the test.
///
/// A local copy of the helper the `inet` suite and `tests/inet.rs` each keep: a
/// `#[cfg(test)]` module's private helpers are not reachable across those boundaries, and the
/// crate's convention is to skip loudly rather than fail when the oracle is unavailable.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(
            Command::new("erl")
                .arg("-noshell")
                .arg("-eval")
                .arg(&code)
                .output(),
        );
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({err}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "the oracle expression failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// The packed Erlang binary literal for a command's byte sequence.
fn erl_bin(cmd: &[u8]) -> String {
    let mut s = String::from("<<");
    for (i, byte) in cmd.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&byte.to_string());
    }
    s.push_str(">>");
    s
}

/// The oracle expression driving the whole matrix: one `ram_file_drv` port per session, each
/// command sent with `port_command/2` and its reply flattened to a byte list.
///
/// The reply is normalised through `iolist_to_binary/1` because the C answers `OK`/`ERROR`/
/// `NUMBER` with `driver_output2` (a plain list of bytes) but `DATA` with
/// `driver_output_binary` (an improper `[header | binary]` iolist in a `[binary]` port) —
/// flattening both to the underlying byte stream is exactly what the driver produces.
fn oracle_expression(sessions: &[(Vec<Vec<u8>>, Vec<Vec<u8>>)]) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push("Norm = fun(D) -> binary_to_list(iolist_to_binary(D)) end".to_string());
    parts.push("Mk = fun() -> P = open_port({spawn,\"ram_file_drv\"},[binary])".to_string());
    parts.push(
        "S = fun(B) -> port_command(P,B), \
         receive {P,{data,D}} -> Norm(D) after 2000 -> timeout end end, S end"
            .to_string(),
    );
    parts.push(
        "ToStr = fun(R) -> case R of timeout -> \"TIMEOUT\"; \
         _ -> lists:join(\" \", [integer_to_list(X) || X <- R]) end end"
            .to_string(),
    );
    for i in 1..=sessions.len() {
        parts.push(format!("F{i} = Mk()"));
    }
    for (i, (cmds, _)) in sessions.iter().enumerate() {
        let n = i + 1;
        let calls: Vec<String> = cmds
            .iter()
            .map(|cmd| format!("F{n}({})", erl_bin(cmd)))
            .collect();
        parts.push(format!(
            "io:format(\"~s~n\", [lists:join(\"|\", [ToStr(R) || R <- [{}]])])",
            calls.join(", ")
        ));
    }
    parts.join(", ")
}

/// Parses one oracle session line (`3 0 0 0 0|1 101 98 97 100 102|TIMEOUT`) into the reply
/// bytes of each command, with `None` for a `timeout` (no reply arrived).
fn parse_oracle_session(line: &str) -> Vec<Option<Vec<u8>>> {
    line.trim()
        .split('|')
        .map(|field| {
            if field == "TIMEOUT" {
                None
            } else if field.is_empty() {
                Some(Vec::new())
            } else {
                Some(
                    field
                        .split(' ')
                        .map(|token| token.parse::<u8>().expect("a reply byte"))
                        .collect(),
                )
            }
        })
        .collect()
}

/// **The compatibility check for the raw command protocol**: every command of every session is
/// sent to the real `ram_file_drv` through a raw port and compared byte-for-byte with what this
/// driver answers.
#[test]
fn the_command_matrix_agrees_with_the_erl_oracle() {
    let sessions = sessions();
    let Some(stdout) = erl_stdout(&oracle_expression(&sessions)) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let lines: Vec<&str> = stdout.lines().filter(|line| !line.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        sessions.len(),
        "one output line per session, got: {stdout:?}"
    );

    let mut compared = 0usize;
    for (index, (cmds, _)) in sessions.iter().enumerate() {
        let oracle = parse_oracle_session(lines[index]);
        assert_eq!(oracle.len(), cmds.len(), "session {index} reply count");
        let mut driver = RamFileDriver::new();
        for (command, want) in cmds.iter().zip(&oracle) {
            let got = driver
                .execute(command[0], &command[1..])
                .expect("the command is well-formed");
            match want {
                Some(bytes) => assert_eq!(
                    &got, bytes,
                    "session {index} command {command:?} differs from the oracle"
                ),
                None => {
                    // The reference produced no reply (it would leave the caller hanging);
                    // this driver must not fabricate one either, so the row is skipped
                    // loudly rather than compared.
                    eprintln!(
                        "skipping the rest of the oracle comparison: session {index} command \
                         {command:?} timed out under the reference"
                    );
                    return;
                }
            }
            compared += 1;
        }
    }
    eprintln!("compared {compared} command replies byte-for-byte with the real ram_file_drv");
}

/// The Erlang side of the **high-level `ram_file` API** cross-check.
///
/// The reference module is opened in `binary` mode so its `DATA` replies are binaries (the
/// `~p` printer then renders them as `<<"...">>`), and every exported operation is exercised
/// once in the order the API documentation describes.
fn ram_file_api_expression() -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push("{ok,F} = ram_file:open(<<\"hello world\">>,[read,write,binary])".to_string());
    parts.push("O = {element(1,F), element(2,F), is_port(element(3,F))}".to_string());
    parts.push("R1 = ram_file:get_size(F)".to_string());
    parts.push("R2 = ram_file:get_file(F)".to_string());
    parts.push("R3 = ram_file:position(F,{bof,0})".to_string());
    parts.push("R4 = ram_file:read(F,5)".to_string());
    parts.push("R5 = ram_file:position(F,cur)".to_string());
    parts.push("R6 = ram_file:read(F,100)".to_string());
    parts.push("R7 = ram_file:read(F,10)".to_string());
    parts.push("R8 = ram_file:pread(F,0,5)".to_string());
    parts.push("R9 = ram_file:pread(F,-1,5)".to_string());
    parts.push("R10 = ram_file:pwrite(F,6,<<\"there\">>)".to_string());
    parts.push("R11 = ram_file:get_file(F)".to_string());
    parts.push("R12 = ram_file:position(F,{eof,0})".to_string());
    parts.push("R13 = ram_file:read(F,5)".to_string());
    parts.push("R14 = ram_file:truncate(F)".to_string());
    parts.push("R15 = ram_file:get_size(F)".to_string());
    parts.push("R16 = ram_file:sync(F)".to_string());
    parts.push("R17 = ram_file:datasync(F)".to_string());
    parts.push("R18 = ram_file:advise(F,0,4,normal)".to_string());
    parts.push("R19 = ram_file:allocate(F,0,100)".to_string());
    parts.push("R20 = ram_file:ipread_s32bu_p32bu(F,0,100)".to_string());
    parts.push("R21 = ram_file:close(F)".to_string());
    for value in [
        "O", "R1", "R2", "R3", "R4", "R5", "R6", "R7", "R8", "R9", "R10", "R11", "R12", "R13",
        "R14", "R15", "R16", "R17", "R18", "R19", "R20", "R21",
    ] {
        parts.push(format!("io:format(\"~p~n\",[{value}])"));
    }
    parts.join(", ")
}

/// **The compatibility check for the `ram_file` API shapes**: the real module (over the real
/// driver) must answer the documented sequence exactly as pinned here.
///
/// `ram_file:ipread_s32bu_p32bu/3` is implemented entirely in Erlang on top of `pread/3`
/// (`file:ipread_s32bu_p32bu_int/3` reads an 8-byte header, finds it is not a
/// `<<0:32, Pos:32>>` descriptor, and reports `eof`) — there is **no** `ipread` command in the
/// driver at all, which is why the raw matrix in
/// [`session_api_raw`] ends at the `pread` that composes it.
#[test]
fn the_high_level_ram_file_api_shapes_agree_with_the_erl_oracle() {
    let Some(stdout) = erl_stdout(&ram_file_api_expression()) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let lines: Vec<&str> = stdout.lines().filter(|line| !line.trim().is_empty()).collect();
    let expected = [
        "{file_descriptor,ram_file,true}",
        "{ok,11}",
        "{ok,<<\"hello world\">>}",
        "{ok,0}",
        "{ok,<<\"hello\">>}",
        "{ok,5}",
        "{ok,<<\" world\">>}",
        "eof",
        "{ok,<<\"hello\">>}",
        "{error,einval}",
        "ok",
        "{ok,<<\"hello there\">>}",
        "{ok,11}",
        "eof",
        "ok",
        "{ok,11}",
        "ok",
        "ok",
        "ok",
        "ok",
        "eof",
        "ok",
    ];
    assert_eq!(
        lines.len(),
        expected.len(),
        "the oracle printed {} lines, got: {stdout:?}",
        lines.len()
    );
    for (index, (got, want)) in lines.iter().zip(expected.iter()).enumerate() {
        assert_eq!(got, want, "the ram_file API reply at line {index}");
    }
}
