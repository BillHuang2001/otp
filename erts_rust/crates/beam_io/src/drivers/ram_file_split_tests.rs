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

//! The `ram_file_drv` **header/body split**, pinned per reply shape.
//!
//! A reply is not one flat byte string: the C sends `OK`/`NUMBER`/`ERROR` through
//! `driver_output2(port, response, len, NULL, 0)` — *header only*, `buf == NULL` — and a `DATA`
//! reply through `driver_output_binary(port, header, 5, bin, 0, n)`, the five-byte
//! `[RAM_FILE_RESP_DATA, n:i32be]` header plus the `n` content bytes as a **present** body
//! (`ram_file_drv.c:182`-`:230`, the `READ`/`PREAD`/`GET` arms at `:448`/`:461`/`:488`).
//!
//! That split is observable at a `[binary]` port owner, which is why it must survive to
//! [`crate::ReadMessage`] rather than being flattened into the port's input queue: the owner sees
//! the improper list `[2,0,0,0,5|<<104,101,108,108,111>>]` for a `DATA` reply but the **proper**
//! list `[3,0,0,0,0]` for a `NUMBER` one. [`ReadMessage::body`] is `None` for the latter (no
//! binary tail at all) and `Some` for the former — including `Some(vec![])`, the `n == 0` DATA
//! reply of a read at end of file, which the C still sends as `driver_output_binary(hdr, 5, bin,
//! 0, 0)` (measured: `[2,0,0,0,0|<<>>]`, **not** the proper list `[2,0,0,0,0]`).
//!
//! [`RamFileDriver::output`](super::RamFileDriver) hands the pair up through
//! [`DriverContext::deliver_read`] as one read (one `driver_output*` call, never two); this suite
//! pins that decision per reply shape and, as the compatibility gate, byte-compares the rendered
//! `[binary]`-port term against the real system `ram_file_drv` through `erl` — skipping loudly,
//! never failing, when `erl` is absent.
//!
//! Split out of `ram_file_tests.rs` only because that file is the driver's whole matrix suite;
//! the tests are still the driver's own in-module suite (`super` is the `ram_file` module).

use super::{
    RamFileDriver, RAM_FILE_ADVISE, RAM_FILE_ALLOCATE, RAM_FILE_FDATASYNC, RAM_FILE_FSYNC,
    RAM_FILE_GET, RAM_FILE_LSEEK, RAM_FILE_MODE_READ, RAM_FILE_MODE_READ_WRITE, RAM_FILE_OPEN,
    RAM_FILE_PREAD, RAM_FILE_PWRITE, RAM_FILE_READ, RAM_FILE_RESP_DATA, RAM_FILE_RESP_ERROR,
    RAM_FILE_RESP_NUMBER, RAM_FILE_RESP_OK, RAM_FILE_SEEK_END, RAM_FILE_SEEK_SET, RAM_FILE_SIZE,
    RAM_FILE_TRUNCATE, RAM_FILE_WRITE,
};
use crate::driver::{Driver, DriverContext, PortArms};
use crate::poller::Poller;
use crate::port_task::ReadMessage;
use crate::queue::ByteQueue;
use beam_process::PortId;
use std::collections::VecDeque;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

/// Wall-clock budget for the `erl` oracle before the affected cross-check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(60);

/// A driver plus the port-side buffers a [`DriverContext`] needs, so one command can be driven
/// through the `output` transport and the reads it hands up collected **with the split intact**.
///
/// A deliberate local copy of the shape `ram_file_tests.rs` keeps: a `#[cfg(test)]` module's
/// private helpers are not reachable across `mod` boundaries, and this suite's whole point is the
/// split, which that harness's `RamReply` flattening would hide.
struct Harness {
    poller: Poller,
    input_q: ByteQueue,
    output_q: ByteQueue,
    arms: PortArms,
    reads: VecDeque<ReadMessage>,
    driver: RamFileDriver,
}

impl Harness {
    fn new() -> Harness {
        Harness {
            poller: Poller::new().expect("a poll set"),
            input_q: ByteQueue::new(),
            output_q: ByteQueue::new(),
            arms: PortArms::default(),
            reads: VecDeque::new(),
            driver: RamFileDriver::new(),
        }
    }

    /// A [`Harness`] whose driver has been opened with `mode` over `content`.
    fn opened(mode: i32, content: &[u8]) -> Harness {
        let mut harness = Harness::new();
        let reply = harness.reply(&c_open(mode, content));
        assert_eq!(
            split_of(&reply),
            (number_header(0), None),
            "`open` answers `NUMBER 0`, header only"
        );
        harness
    }

    /// Feeds `bytes` through the driver's `output` transport and returns the one read it handed
    /// up, header/body split preserved, along with the consume-what-you-report accounting.
    fn reply(&mut self, bytes: &[u8]) -> ReadMessage {
        let mut replies = self.replies(bytes);
        assert_eq!(replies.len(), 1, "one read per command: {replies:?}");
        replies.remove(0)
    }

    /// Feeds `bytes` through the driver's `output` transport and returns **every** read it handed
    /// up, in delivery order (the C delivers one message per `driver_output*` call).
    fn replies(&mut self, bytes: &[u8]) -> Vec<ReadMessage> {
        let Harness {
            driver,
            poller,
            input_q,
            output_q,
            arms,
            reads,
        } = self;
        output_q.push(bytes);
        let mut ctx = DriverContext::new(PortId::new(0), poller, input_q, output_q, arms)
            .with_reads(reads);
        let written = driver.output(&mut ctx).expect("the command is accepted");
        assert_eq!(
            written,
            bytes.len(),
            "the driver consumes exactly the command bytes"
        );
        drop(ctx);
        assert!(output_q.is_empty(), "the whole command was consumed");
        assert!(
            input_q.is_empty(),
            "the reply rides the completed-read channel, never the input queue"
        );
        reads.drain(..).collect()
    }
}

// -----------------------------------------------------------------------
// Command builders (the wire form, byte for byte)
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

// -----------------------------------------------------------------------
// Reply builders and the (header, body) view
// -----------------------------------------------------------------------

/// The `OK` reply's header: the bare response code.
fn ok_header() -> Vec<u8> {
    vec![RAM_FILE_RESP_OK]
}

/// The `NUMBER` reply's header: the response code plus the value big-endian.
fn number_header(value: i32) -> Vec<u8> {
    let mut out = vec![RAM_FILE_RESP_NUMBER];
    out.extend_from_slice(&value.to_be_bytes());
    out
}

/// The `DATA` reply's header: the response code plus the **body** length big-endian — the C's
/// five-byte `driver_output_binary` header, which carries the length of the body that follows it.
fn data_header(len: usize) -> Vec<u8> {
    let mut out = vec![RAM_FILE_RESP_DATA];
    out.extend_from_slice(&(len as i32).to_be_bytes());
    out
}

/// The `ERROR` reply's header: the response code plus the lowercase errno name.
fn error_header(name: &str) -> Vec<u8> {
    let mut out = vec![RAM_FILE_RESP_ERROR];
    out.extend_from_slice(name.as_bytes());
    out
}

/// The `(header, body)` pair a read carries, cloned out of the [`ReadMessage`].
fn split_of(read: &ReadMessage) -> (Vec<u8>, Option<Vec<u8>>) {
    (read.header.clone(), read.body.clone())
}

/// Asserts `read` is a header-only reply carrying exactly `header` — the C's
/// `driver_output2(port, header, len, NULL, 0)` — and returns the header for chaining.
///
/// `body == None` is the assertion that matters: `Some(vec![])` would be the *other* message
/// (`[hdr|<<>>]` at a `[binary]` port), so a driver that sent an empty body where the C sent none
/// is caught here rather than at the owner.
#[track_caller]
fn assert_header_only(read: &ReadMessage, header: Vec<u8>, what: &str) -> Vec<u8> {
    assert_eq!(
        split_of(read),
        (header.clone(), None),
        "{what} is header only (`driver_output2(hdr, None)`): {read:?}"
    );
    assert!(
        !read.header.is_empty(),
        "{what} carries its response code in the header: {read:?}"
    );
    header
}

/// Asserts `read` is a `DATA` reply — the five-byte header plus a **present** body — and returns
/// that body.
#[track_caller]
fn assert_data(read: &ReadMessage, payload: &[u8], what: &str) -> Vec<u8> {
    assert_eq!(
        read.header,
        data_header(payload.len()),
        "{what}'s header is `[DATA, len:i32be]`: {read:?}"
    );
    assert_eq!(
        read.body.as_deref(),
        Some(payload),
        "{what}'s body is the content, present even when empty (`driver_output_binary`): {read:?}"
    );
    assert!(
        read.has_body(),
        "{what} has a body slot at all — the binary tail exists: {read:?}"
    );
    payload.to_vec()
}

// -----------------------------------------------------------------------
// The split, per reply shape
// -----------------------------------------------------------------------

/// Every **header-only** reply: `open`/`lseek`/`pwrite`/`write` answer `NUMBER`, `size` answers
/// `NUMBER`, and `fsync`/`fdatasync`/`truncate`/`advise`/`allocate` answer `OK`. All of them are
/// the C's `driver_output2(header, len, NULL, 0)` — one header, no body.
#[test]
fn every_header_only_reply_carries_no_body() {
    let mut h = Harness::new();
    let read = h.reply(&c_open(RAM_FILE_MODE_READ_WRITE, b"hello"));
    assert_header_only(&read, number_header(0), "`open`");

    let read = h.reply(&c_lseek(0, RAM_FILE_SEEK_SET));
    assert_header_only(&read, number_header(0), "`lseek`");

    let read = h.reply(&c_lseek(0, RAM_FILE_SEEK_END));
    assert_header_only(&read, number_header(5), "`lseek` from the end");

    let read = h.reply(&c_write(b"xy"));
    assert_header_only(&read, number_header(2), "`write`");

    let read = h.reply(&c_pwrite(0, b"z"));
    assert_header_only(&read, number_header(1), "`pwrite`");

    let read = h.reply(&c_size());
    assert_header_only(&read, number_header(7), "`size`");

    let read = h.reply(&c_fsync());
    assert_header_only(&read, ok_header(), "`fsync`");

    let read = h.reply(&c_fdatasync());
    assert_header_only(&read, ok_header(), "`fdatasync`");

    let read = h.reply(&c_advise());
    assert_header_only(&read, ok_header(), "`advise`");

    let read = h.reply(&c_allocate());
    assert_header_only(&read, ok_header(), "`allocate`");

    let read = h.reply(&c_truncate());
    assert_header_only(&read, ok_header(), "`truncate`");

    // An `ERROR` reply is header only too — the response code plus the errno name, no body.
    let mut read_only = Harness::opened(RAM_FILE_MODE_READ, b"hello");
    let read = read_only.reply(&c_write(b"x"));
    assert_header_only(&read, error_header("ebadf"), "`write` on a read-only file");

    // ... and so is a `NUMBER` refused nothing at all: an unopened driver's `size`.
    let mut fresh = Harness::new();
    let read = fresh.reply(&c_size());
    assert_header_only(&read, number_header(0), "an unopened driver's `size`");
}

/// Every **`DATA`** reply: the five-byte `[DATA, len:i32be]` header plus the content bytes as the
/// body (`driver_output_binary(port, header, 5, bin, 0, n)`).
#[test]
fn every_data_reply_carries_the_header_and_the_payload_as_its_body() {
    let mut h = Harness::opened(RAM_FILE_MODE_READ_WRITE, b"hello world");

    // `read` at the current position.
    let read = h.reply(&c_read(5));
    assert_eq!(
        assert_data(&read, b"hello", "`read`"),
        b"hello".to_vec(),
        "the payload is the body"
    );

    // `pread` at an explicit offset.
    let read = h.reply(&c_pread(6, 5));
    assert_data(&read, b"world", "`pread`");

    // `get` — the whole logical file.
    let read = h.reply(&c_get());
    assert_data(&read, b"hello world", "`get`");

    // The header's length word is the **body** length: an 11-byte file's `get` says 11 while its
    // header is five bytes, i.e. the header length and the body length are different quantities.
    let read = h.reply(&c_get());
    assert_eq!(
        read.header,
        vec![RAM_FILE_RESP_DATA, 0, 0, 0, 11],
        "the header's length word counts the body, not the header itself: {read:?}"
    );
    assert_eq!(read.body.as_deref(), Some(b"hello world".as_slice()));
    assert_eq!(
        read.len(),
        16,
        "the read carries header + body, the C's `bytes_in` for the call"
    );
    assert!(!read.is_empty(), "a DATA reply is never the empty read");
}

/// A **zero-length `DATA`** reply keeps a **present, empty body**: the C calls
/// `driver_output_binary(port, hdr, 5, bin, 0, 0)` there, whose owner-visible message is the
/// improper `[2,0,0,0,0|<<>>]`, never the proper list `[2,0,0,0,0]` a `NULL` body would give.
#[test]
fn a_zero_length_data_reply_has_a_present_empty_body() {
    let mut h = Harness::opened(RAM_FILE_MODE_READ_WRITE, b"hello");

    // A `read` at the exact end of the file.
    let read = h.reply(&c_read(0));
    assert_eq!(
        split_of(&read),
        (data_header(0), Some(Vec::new())),
        "a zero-length `read` is a present, empty body: {read:?}"
    );
    // ... and after consuming the whole file, so the read really is at EOF.
    let read = h.reply(&c_read(5));
    assert_data(&read, b"hello", "the read that reaches EOF");
    let read = h.reply(&c_read(5));
    assert_eq!(
        split_of(&read),
        (data_header(0), Some(Vec::new())),
        "a `read` past EOF is a present, empty body: {read:?}"
    );
    assert!(
        read.has_body(),
        "the binary tail exists even though it is empty: {read:?}"
    );

    // A `pread` of length 0 — the C's `driver_output_binary(hdr, 5, bin, offs, 0)`.
    let read = h.reply(&c_pread(0, 0));
    assert_eq!(
        split_of(&read),
        (data_header(0), Some(Vec::new())),
        "a zero-length `pread` is a present, empty body: {read:?}"
    );

    // A `pread` past the end.
    let read = h.reply(&c_pread(5, 5));
    assert_eq!(
        split_of(&read),
        (data_header(0), Some(Vec::new())),
        "a `pread` past EOF is a present, empty body: {read:?}"
    );

    // `get` of a logically empty file (truncated at position 0).
    let mut empty = Harness::opened(RAM_FILE_MODE_READ_WRITE, b"");
    let read = empty.reply(&c_get());
    assert_eq!(
        split_of(&read),
        (data_header(0), Some(Vec::new())),
        "an empty file's `get` is a present, empty body: {read:?}"
    );
    // ... and an unopened driver's `get` (the C serves it with no mode at all).
    let mut fresh = Harness::new();
    let read = fresh.reply(&c_get());
    assert_eq!(
        split_of(&read),
        (data_header(0), Some(Vec::new())),
        "an unopened driver's `get` is a present, empty body: {read:?}"
    );
}

/// The two empty shapes are **different** messages: a `NUMBER 0` (`body: None`) and a zero-length
/// `DATA` (`body: Some(vec![])`) never collapse into each other.
#[test]
fn an_empty_number_and_a_zero_length_data_reply_are_different_messages() {
    let mut h = Harness::opened(RAM_FILE_MODE_READ_WRITE, b"");
    let number = h.reply(&c_size());
    let data = h.reply(&c_get());
    assert_eq!(
        split_of(&number),
        (number_header(0), None),
        "`size` of an empty file"
    );
    assert_eq!(
        split_of(&data),
        (data_header(0), Some(Vec::new())),
        "`get` of an empty file"
    );
    assert_ne!(
        number.body, data.body,
        "`None` (the C's `NULL` body) and `Some(vec![])` (a present empty body) differ"
    );
    // Both are one byte of `flattened` payload apart from their headers, and neither is the
    // empty read — that one is `driver_output2(NULL, 0, NULL, 0)` and no reply here is it.
    assert_eq!(number.flattened(), vec![RAM_FILE_RESP_NUMBER, 0, 0, 0, 0]);
    assert_eq!(data.flattened(), vec![RAM_FILE_RESP_DATA, 0, 0, 0, 0]);
    assert!(!number.is_empty(), "a NUMBER reply carries its header");
    assert!(!data.is_empty(), "a DATA reply carries its header");
}

// -----------------------------------------------------------------------
// Live cross-check against the system `erl`
// -----------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after
/// printing a clear skip message — when `erl` cannot be spawned or does not answer within
/// [`ERL_TIMEOUT`]. A spawned `erl` that exits non-zero is a bug in the expression, so it fails
/// the test.
///
/// A local copy of the helper the `inet` suite and `ram_file_tests.rs` each keep: a `#[cfg(test)]`
/// module's private helpers are not reachable across those boundaries, and the crate's convention
/// is to skip loudly rather than fail when the oracle is unavailable.
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
    let bytes: Vec<String> = cmd.iter().map(u8::to_string).collect();
    format!("<<{}>>", bytes.join(","))
}

/// The sequence this suite drives through both sides: one `[binary]` `ram_file_drv` port, one
/// command per row, covering every reply shape — `NUMBER`, a payload `DATA`, a zero-length `DATA`,
/// `OK`, and an `ERROR`.
///
/// The commands are replayed in this exact order on both sides (a `read` at the position `open`
/// leaves, then an EOF read, then a `pread` of length 0), so the rows are comparable one for one.
fn split_sequence() -> Vec<Vec<u8>> {
    vec![
        // NUMBER 0 — `open`, its reply's unused result slot.
        c_open(RAM_FILE_MODE_READ_WRITE, b"hello"),
        // DATA 5 — a `read` of the whole file, at the position `open` left.
        c_read(5),
        // DATA 0 — a `read` at EOF: the zero-length `driver_output_binary`.
        c_read(5),
        // NUMBER 5 — `size`.
        c_size(),
        // DATA 5 — `get`, the whole logical file.
        c_get(),
        // DATA 0 — a `pread` of length 0.
        c_pread(0, 0),
        // OK — `fsync`.
        c_fsync(),
        // NUMBER 0 — `lseek` back to the start.
        c_lseek(0, RAM_FILE_SEEK_SET),
        // NUMBER 1 — `write` of one byte.
        c_write(b"!"),
        // ERROR ebadf — a `write` on a file re-opened read-only.
        c_open(RAM_FILE_MODE_READ, b"hello"),
        c_write(b"x"),
    ]
}

/// The oracle expression: one `[binary]` `ram_file_drv` port, every row of
/// [`split_sequence`] sent with `port_command/2`, and each reply printed **as the term the owner
/// receives** (`~w`, so the improper list of a `DATA` reply shows its binary tail:
/// `[2,0,0,0,5|<<104,101,108,108,111>>]`).
fn oracle_expression() -> String {
    let calls: Vec<String> = split_sequence()
        .iter()
        .map(|cmd| format!("S({})", erl_bin(cmd)))
        .collect();
    format!(
        "P = open_port({{spawn,\"ram_file_drv\"}},[binary]), \
         S = fun(B) -> port_command(P,B), \
                 receive {{P,{{data,D}}}} -> D after 2000 -> timeout end end, \
         [io:format(\"~w~n\", [R]) || R <- [{}]]",
        calls.join(", ")
    )
}

/// Renders one read the way a **`[binary]`** port's owner receives it, in `~w`'s own notation, so
/// the split is compared and not merely described:
///
/// * `body: None` ⇒ the **proper** list of the header bytes (`[3,0,0,0,0]`) — the C adds no
///   binary tail for a `NULL` body;
/// * `body: Some(b)` ⇒ the improper `[h1,…,hn|<<b1,…,bm>>]`, the header as one cons cell per byte
///   and the body as the tail's binary — with `<<>>` for an empty body;
/// * an empty header (no reply here produces one) ⇒ the bare binary, or `[]` with no body at all.
///
/// This mirrors [`ReadMessage`]'s pinned shape table (the C's `deliver_read_message`,
/// `io.c:3363-3442`); `~w` prints a binary's bytes rather than its characters, which is why the
/// tail is `<<104,…>>` and not `<<"hello">>`.
fn rendered_binary_term(read: &ReadMessage) -> String {
    let header = read
        .header
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let body = read.body.as_ref().map(|body| {
        let bytes = body.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
        format!("<<{bytes}>>")
    });
    match (read.header.is_empty(), body) {
        (true, None) => "[]".to_string(),
        (true, Some(body)) => body,
        (false, None) => format!("[{header}]"),
        (false, Some(body)) => format!("[{header}|{body}]"),
    }
}

/// **The split, against the real driver.** Every row is rendered as the `[binary]` port's `Data`
/// term and compared line for line with what the system `ram_file_drv` answers for the same
/// bytes: the header-only replies are proper lists, the `DATA` replies improper lists, and the
/// zero-length `DATA` reply an improper list with an empty binary tail.
#[test]
fn the_binary_port_term_shapes_agree_with_the_erl_oracle() {
    let sequence = split_sequence();
    let Some(stdout) = erl_stdout(&oracle_expression()) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let lines: Vec<&str> = stdout.lines().filter(|line| !line.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        sequence.len(),
        "one output line per command, got: {stdout:?}"
    );

    let mut harness = Harness::new();
    let mut compared = 0usize;
    for (index, command) in sequence.iter().enumerate() {
        let read = harness.reply(command);
        assert_eq!(
            rendered_binary_term(&read),
            lines[index].trim(),
            "row {index} (command {command:?}) differs from the oracle"
        );
        compared += 1;
    }
    assert_eq!(compared, sequence.len(), "every row was compared");

    // The same rows, asserted structurally, so a renderer bug cannot hide a split bug: the
    // header-only rows carry no body and the `DATA` rows carry one (empty or not).
    let mut harness = Harness::new();
    let read = harness.reply(&sequence[0]);
    assert_header_only(&read, number_header(0), "`open`");
    let read = harness.reply(&sequence[1]);
    assert_data(&read, b"hello", "`read`");
    let read = harness.reply(&sequence[2]);
    assert_eq!(
        split_of(&read),
        (data_header(0), Some(Vec::new())),
        "the EOF `read` keeps a present, empty body"
    );
    let read = harness.reply(&sequence[3]);
    assert_header_only(&read, number_header(5), "`size`");
    let read = harness.reply(&sequence[4]);
    assert_data(&read, b"hello", "`get`");
    let read = harness.reply(&sequence[5]);
    assert_eq!(
        split_of(&read),
        (data_header(0), Some(Vec::new())),
        "the zero-length `pread` keeps a present, empty body"
    );
    let read = harness.reply(&sequence[6]);
    assert_header_only(&read, ok_header(), "`fsync`");
    let read = harness.reply(&sequence[7]);
    assert_header_only(&read, number_header(0), "`lseek`");
    let read = harness.reply(&sequence[8]);
    assert_header_only(&read, number_header(1), "`write`");
    let read = harness.reply(&sequence[9]);
    assert_header_only(&read, number_header(0), "the read-only `open`");
    let read = harness.reply(&sequence[10]);
    assert_header_only(&read, error_header("ebadf"), "the refused `write`");
}

/// The oracle's rows, pinned literally — the shapes the comparison above relies on, so a change
/// in what the system driver answers shows up as a failing literal rather than as a silently
/// re-baselined comparison.
#[test]
fn the_oracle_rows_are_the_documented_terms() {
    let Some(stdout) = erl_stdout(&oracle_expression()) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let lines: Vec<&str> = stdout.lines().filter(|line| !line.trim().is_empty()).collect();
    let expected = [
        "[3,0,0,0,0]",
        "[2,0,0,0,5|<<104,101,108,108,111>>]",
        "[2,0,0,0,0|<<>>]",
        "[3,0,0,0,5]",
        "[2,0,0,0,5|<<104,101,108,108,111>>]",
        "[2,0,0,0,0|<<>>]",
        "[0]",
        "[3,0,0,0,0]",
        "[3,0,0,0,1]",
        "[3,0,0,0,0]",
        "[1,101,98,97,100,102]",
    ];
    assert_eq!(
        lines.len(),
        expected.len(),
        "one line per row, got: {stdout:?}"
    );
    for (index, (got, want)) in lines.iter().zip(expected.iter()).enumerate() {
        assert_eq!(got.trim(), *want, "row {index} of the oracle's term shapes");
    }
}
