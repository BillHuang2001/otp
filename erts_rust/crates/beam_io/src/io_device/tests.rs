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

//! The device-loop suite: a scripted [`ByteSource`] and a scripted [`Continuation`] pin
//! every branch of `get_chars/5` → `get_chars_apply/7`.
//!
//! The expected values are taken from `lib/kernel/src/file_io_server.erl` and
//! `lib/stdlib/src/io_lib.erl` (the line numbers are in the parent module's docs).

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

use beam_common::Result;

use super::{ByteSource, ContData, Continuation, Encoding, IoDevice, Leftover, NoEngineContinuation};
use crate::drivers::efile::FileDataMode;
use crate::Applied;
use beam_common::BeamError;

/// A source that hands out one pre-scripted chunk per read and counts its reads.
struct ScriptedSource {
    chunks: VecDeque<Vec<u8>>,
    reads: Rc<Cell<usize>>,
}

impl ScriptedSource {
    fn new(chunks: &[&[u8]]) -> (Self, Rc<Cell<usize>>) {
        let reads = Rc::new(Cell::new(0));
        let source = Self {
            chunks: chunks.iter().map(|chunk| chunk.to_vec()).collect(),
            reads: Rc::clone(&reads),
        };
        (source, reads)
    }
}

impl ByteSource for ScriptedSource {
    fn read_chunk(&mut self, _size: usize) -> Result<Option<Vec<u8>>> {
        self.reads.set(self.reads.get() + 1);
        Ok(self.chunks.pop_front())
    }
}

/// A source whose reads always fail.
struct BrokenSource;

impl ByteSource for BrokenSource {
    fn read_chunk(&mut self, _size: usize) -> Result<Option<Vec<u8>>> {
        Err(BeamError::Io(std::io::Error::from_raw_os_error(libc::EIO)))
    }
}

/// The reply type of the mock continuation.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Reply {
    List(Vec<u32>),
    Binary(Vec<u8>),
}

/// One scripted continuation outcome.
#[derive(Clone, Debug)]
enum Step {
    More,
    Done {
        reply: Reply,
        leftover: Leftover,
    },
    /// The continuation itself fails (`Mod:Func` raising) with `badmatch`.
    Fail,
}

/// A continuation that returns a scripted sequence of outcomes and records every
/// application's data.
struct Mock {
    seen: Vec<ContData>,
    steps: VecDeque<Step>,
}

impl Mock {
    fn new(steps: Vec<Step>) -> Self {
        Self {
            seen: Vec::new(),
            steps: steps.into(),
        }
    }

    /// The mock's default `cast_reply`: `io_lib`'s cast of a list reply when the first data
    /// of the request was a binary.
    fn cast(reply: Reply, is_data_binary: bool, encoding: Encoding) -> Reply {
        match (reply, is_data_binary) {
            (Reply::List(chars), true) => Reply::Binary(if encoding == Encoding::Unicode {
                // `unicode:characters_to_binary(Result, unicode, unicode)`.
                let mut text = String::new();
                for cp in chars {
                    if let Some(ch) = char::from_u32(cp) {
                        text.push(ch);
                    }
                }
                text.into_bytes()
            } else {
                // `erlang:iolist_to_binary(Result)`.
                chars.iter().map(|&c| c as u8).collect()
            }),
            (other, _) => other,
        }
    }
}

impl Continuation for Mock {
    type Cont = Vec<u32>;
    type Reply = Reply;

    fn apply(
        &mut self,
        mut cont: Vec<u32>,
        data: ContData,
        _encoding: Encoding,
    ) -> Result<Applied<Vec<u32>, Reply>> {
        if let ContData::Chars(chars) = &data {
            cont.extend_from_slice(chars);
        }
        self.seen.push(data);
        match self.steps.pop_front() {
            Some(Step::More) => Ok(Applied::More(cont)),
            Some(Step::Done { reply, leftover }) => Ok(Applied::Done { reply, leftover }),
            Some(Step::Fail) => Err(BeamError::BadMatch),
            // The script ran out: answer with everything accumulated so far.
            None => Ok(Applied::Done {
                reply: Reply::List(cont),
                leftover: Leftover::None,
            }),
        }
    }

    fn cast_reply(&self, reply: Reply, is_data_binary: bool, encoding: Encoding) -> Result<Reply> {
        Ok(Mock::cast(reply, is_data_binary, encoding))
    }
}

/// The code points of an ASCII literal.
fn chars(text: &str) -> Vec<u32> {
    text.chars().map(|ch| ch as u32).collect()
}

fn done(reply: Reply) -> Step {
    Step::Done {
        reply,
        leftover: Leftover::None,
    }
}

#[test]
fn read_size_matches_the_reference() {
    // `?READ_SIZE_BINARY` / `?READ_SIZE_LIST` (`file_io_server.erl:39-40`).
    assert_eq!(super::READ_SIZE_BINARY, 8 * 1024);
    assert_eq!(super::READ_SIZE_LIST, 128);
    assert_eq!(super::read_size(FileDataMode::Binary), 8 * 1024);
    assert_eq!(super::read_size(FileDataMode::List), 128);
}

#[test]
fn more_loops_and_reads_further_chunks() {
    let (source, reads) = ScriptedSource::new(&[b"ab", b"cd"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::More, done(Reply::Binary(b"done".to_vec()))]);

    let reply = device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");

    assert_eq!(reply, Reply::Binary(b"done".to_vec()));
    assert_eq!(reads.get(), 2, "the {{more,…}} loop reads a second chunk");
    assert_eq!(
        cont.seen,
        vec![
            ContData::Chars(chars("ab")),
            ContData::Chars(chars("cd")),
        ],
        "each application sees the chunk the loop read"
    );
    assert!(device.buffer().is_empty(), "no leftover was returned");
}

#[test]
fn a_more_continuation_accumulates_across_reads() {
    // The continuation's own state carries the data across the loop (the device does not
    // re-send earlier chunks), so the default `Done` reply holds everything.
    let (source, _) = ScriptedSource::new(&[b"one", b"two", b"three"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::List);
    let mut cont = Mock::new(vec![Step::More, Step::More]);

    let reply = device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");

    // `list` read mode did not cast the (list) reply.
    assert_eq!(reply, Reply::List(chars("onetwothree")));
}

#[test]
fn done_returns_the_reply_and_stashes_the_leftover() {
    let (source, _) = ScriptedSource::new(&[b"form\nrest"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::Done {
        reply: Reply::Binary(b"the-form".to_vec()),
        leftover: Leftover::Binary(b"rest".to_vec()),
    }]);

    let reply = device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");

    assert_eq!(reply, Reply::Binary(b"the-form".to_vec()));
    assert_eq!(device.buffer(), b"rest", "the leftover is the new device buffer");
    assert!(device.has_stashed_buffer());
}

#[test]
fn a_stashed_leftover_is_fed_before_a_fresh_read() {
    let (source, reads) = ScriptedSource::new(&[b"fresh"]);
    let mut device =
        IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary).with_buffer(b"stashed".to_vec());
    let mut cont = Mock::new(vec![done(Reply::Binary(b"answer".to_vec()))]);

    let reply = device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");

    assert_eq!(reply, Reply::Binary(b"answer".to_vec()));
    assert_eq!(reads.get(), 0, "the stash is applied without reading");
    assert_eq!(cont.seen, vec![ContData::Chars(chars("stashed"))]);
    assert!(device.buffer().is_empty(), "the stash was consumed");
}

#[test]
fn a_leftover_feeds_the_next_request_before_the_source() {
    // Request 1 reads "abc" and leaves "bc"; request 2 must see "bc" first.
    let (source, reads) = ScriptedSource::new(&[b"abc", b"def"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);

    let mut first = Mock::new(vec![Step::Done {
        reply: Reply::Binary(vec![]),
        leftover: Leftover::Binary(b"bc".to_vec()),
    }]);
    device
        .get_until(&mut first, Encoding::Latin1)
        .expect("first reply");
    assert_eq!(device.buffer(), b"bc");
    assert_eq!(reads.get(), 1);

    let mut second = Mock::new(vec![done(Reply::Binary(vec![]))]);
    device
        .get_until(&mut second, Encoding::Latin1)
        .expect("second reply");
    assert_eq!(
        second.seen,
        vec![ContData::Chars(chars("bc"))],
        "the stashed buffer is applied before any read"
    );
    assert_eq!(reads.get(), 1, "no read was needed for the stashed buffer");
}

#[test]
fn end_of_file_is_delivered_as_eof() {
    let (source, reads) = ScriptedSource::new(&[]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![done(Reply::Binary(b"eof-reply".to_vec()))]);

    let reply = device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");

    assert_eq!(reply, Reply::Binary(b"eof-reply".to_vec()));
    assert_eq!(cont.seen, vec![ContData::Eof], "the atom `eof` is applied");
    assert_eq!(reads.get(), 1, "the end of file is sticky but read once");
}

#[test]
fn eof_after_data_is_delivered_as_eof_too() {
    let (source, _) = ScriptedSource::new(&[b"head"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::More, done(Reply::Binary(vec![]))]);

    device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");

    assert_eq!(
        cont.seen,
        vec![ContData::Chars(chars("head")), ContData::Eof]
    );
}

#[test]
fn binary_read_mode_casts_a_list_reply_but_list_read_mode_does_not() {
    // Binary read mode: the first data was a binary, so `io_lib` casts the list reply to a
    // binary (through `iolist_to_binary/1` under `latin1`).
    let (source, _) = ScriptedSource::new(&[b"hi"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![done(Reply::List(chars("hi")))]);
    assert_eq!(
        device.get_until(&mut cont, Encoding::Latin1).expect("reply"),
        Reply::Binary(b"hi".to_vec())
    );

    // List read mode: the first data was a charlist, so no cast happens.
    let (source, _) = ScriptedSource::new(&[b"hi"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::List);
    let mut cont = Mock::new(vec![done(Reply::List(chars("hi")))]);
    assert_eq!(
        device.get_until(&mut cont, Encoding::Latin1).expect("reply"),
        Reply::List(chars("hi"))
    );
}

#[test]
fn a_unicode_request_casts_a_list_reply_to_utf8() {
    let (source, _) = ScriptedSource::new(&["é".as_bytes()]);
    let mut device = IoDevice::new(source, Encoding::Unicode, FileDataMode::Binary);
    let mut cont = Mock::new(vec![done(Reply::List(vec![0xE9]))]);

    assert_eq!(
        device.get_until(&mut cont, Encoding::Unicode).expect("reply"),
        // U+00E9 encodes to two UTF-8 bytes.
        Reply::Binary(vec![0xC3, 0xA9])
    );
}

#[test]
fn a_latin1_stream_read_for_a_unicode_request_is_transcoded() {
    // The device is latin1 (`State#state.unic`), the request asks for `unicode`: a latin1
    // byte 0xE9 becomes the code point U+00E9.
    let (source, _) = ScriptedSource::new(&[&[0xE9]]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![done(Reply::Binary(vec![]))]);

    device
        .get_until(&mut cont, Encoding::Unicode)
        .expect("a reply");

    assert_eq!(cont.seen, vec![ContData::Chars(vec![0xE9])]);
}

#[test]
fn a_latin1_request_on_a_unicode_device_takes_the_general_clause() {
    // `file_io_server.erl:628-691`: the `latin1` clause matches only when the request's `Enc`
    // is `latin1` **and** `State#state.unic` is `latin1`. Here `Enc = latin1` but the device is
    // `unicode`, so the general clause runs `unicode:characters_to_binary(Data0, unicode, latin1)`:
    // the UTF-8 `é` (`0xC3 0xA9`) collapses to the single latin1 byte `0xE9` before `io_lib`
    // narrows it with `binary_to_list/1`.
    let (source, _) = ScriptedSource::new(&[&[0xC3, 0xA9]]);
    let mut device = IoDevice::new(source, Encoding::Unicode, FileDataMode::Binary);
    let mut cont = Mock::new(vec![done(Reply::Binary(vec![]))]);

    let reply = device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");

    assert_eq!(cont.seen, vec![ContData::Chars(vec![0xE9])]);
    assert_eq!(reply, Reply::Binary(vec![]));
}

#[test]
fn the_unicode_branch_carries_an_incomplete_utf8_tail_across_reads() {
    // "a" + the first byte of "é", then the second byte of "é" + "b".
    let (source, _) = ScriptedSource::new(&[&[0x61, 0xC3], &[0xA9, 0x62]]);
    let mut device = IoDevice::new(source, Encoding::Unicode, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::More, done(Reply::Binary(vec![]))]);

    device
        .get_until(&mut cont, Encoding::Unicode)
        .expect("a reply");

    assert_eq!(
        cont.seen,
        vec![
            ContData::Chars(vec![0x61]),
            // The incomplete tail `C3` was re-fed and completed to U+00E9.
            ContData::Chars(vec![0xE9, 0x62]),
        ]
    );
    assert!(device.buffer().is_empty());
}

#[test]
fn the_unicode_list_read_mode_carries_the_incomplete_tail_too() {
    let (source, _) = ScriptedSource::new(&[&[0x61, 0xC3], &[0xA9, 0x62]]);
    let mut device = IoDevice::new(source, Encoding::Unicode, FileDataMode::List);
    let mut cont = Mock::new(vec![Step::More, done(Reply::Binary(vec![]))]);

    device
        .get_until(&mut cont, Encoding::Unicode)
        .expect("a reply");

    assert_eq!(
        cont.seen,
        vec![
            ContData::Chars(vec![0x61]),
            ContData::Chars(vec![0xE9, 0x62]),
        ]
    );
}

#[test]
fn an_incomplete_utf8_tail_at_eof_is_invalid_unicode() {
    let (source, _) = ScriptedSource::new(&[&[0x61, 0xC3]]);
    let mut device = IoDevice::new(source, Encoding::Unicode, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::More]);

    let error = device
        .get_until(&mut cont, Encoding::Unicode)
        .expect_err("the sequence spans the end of the file");

    assert_eq!(
        error.into_raise().expect("a raise").reason.as_atom(),
        Some("invalid_unicode"),
        "`get_chars_notempty`'s `{{stop, invalid_unicode, …}}`"
    );
    assert_eq!(
        device.buffer(),
        &[0xC3],
        "the oracle keeps the pending tail in the device buffer on that stop"
    );
}

#[test]
fn an_invalid_utf8_sequence_with_an_empty_prefix_is_invalid_unicode() {
    let (source, _) = ScriptedSource::new(&[&[0xFF, 0x41]]);
    let mut device = IoDevice::new(source, Encoding::Unicode, FileDataMode::Binary);
    let mut cont = Mock::new(vec![done(Reply::Binary(vec![]))]);

    let error = device
        .get_until(&mut cont, Encoding::Unicode)
        .expect_err("`{error, <<>>, _}` exits invalid_unicode");

    assert_eq!(
        error.into_raise().expect("a raise").reason.as_atom(),
        Some("invalid_unicode")
    );
    // The continuation was never applied.
    assert!(cont.seen.is_empty());
}

#[test]
fn an_invalid_utf8_sequence_after_a_decoded_prefix_still_applies() {
    // A decoded prefix arms the `{error, Decoded, Rest}` clause: the prefix is applied and
    // the rest becomes the incomplete tail.
    let (source, _) = ScriptedSource::new(&[&[0x61, 0xFF]]);
    let mut device = IoDevice::new(source, Encoding::Unicode, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::More]);

    let error = device
        .get_until(&mut cont, Encoding::Unicode)
        .expect_err("the {{more,…}} loop reaches eof with a pending tail");

    assert_eq!(cont.seen, vec![ContData::Chars(vec![0x61])]);
    assert_eq!(
        error.into_raise().expect("a raise").reason.as_atom(),
        Some("invalid_unicode")
    );
}

#[test]
fn the_latin1_leftover_shapes_are_cast_binary() {
    // A binary leftover is used verbatim.
    let (source, _) = ScriptedSource::new(&[b"x"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::Done {
        reply: Reply::Binary(vec![]),
        leftover: Leftover::Binary(b"tail".to_vec()),
    }]);
    device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");
    assert_eq!(device.buffer(), b"tail");

    // A charlist leftover becomes bytes (`list_to_binary/1`).
    let (source, _) = ScriptedSource::new(&[b"x"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::Done {
        reply: Reply::Binary(vec![]),
        leftover: Leftover::Chars(chars("hi")),
    }]);
    device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");
    assert_eq!(device.buffer(), b"hi");

    // No leftover clears the buffer.
    let (source, _) = ScriptedSource::new(&[b"x"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary).with_buffer(b"old".to_vec());
    let mut cont = Mock::new(vec![done(Reply::Binary(vec![]))]);
    device
        .get_until(&mut cont, Encoding::Latin1)
        .expect("a reply");
    assert!(device.buffer().is_empty());
}

#[test]
fn a_charlist_leftover_that_is_not_latin1_is_badarg() {
    let (source, _) = ScriptedSource::new(&[b"x"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::Done {
        reply: Reply::Binary(vec![]),
        leftover: Leftover::Chars(vec![0x0100]),
    }]);

    assert!(matches!(
        device
            .get_until(&mut cont, Encoding::Latin1)
            .expect_err("0x100 does not fit latin1"),
        BeamError::BadArg
    ));
}

#[test]
fn a_unicode_leftover_is_converted_and_joined_with_the_incomplete_tail() {
    // "a" + a truncated "é": the charlist leftover is UTF-8 encoded and the pending tail is
    // appended (`file_io_server.erl:672-680`).
    let (source, _) = ScriptedSource::new(&[&[0x61, 0xC3]]);
    let mut device = IoDevice::new(source, Encoding::Unicode, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::Done {
        reply: Reply::Binary(vec![]),
        leftover: Leftover::Chars(vec![0x62, 0xE9]),
    }]);

    device
        .get_until(&mut cont, Encoding::Unicode)
        .expect("a reply");

    let mut expected = vec![0x62u8];
    expected.extend_from_slice(&[0xC3, 0xA9]); // U+00E9
    expected.push(0xC3); // the pending incomplete tail
    assert_eq!(device.buffer(), expected.as_slice());
}

#[test]
fn a_later_more_iteration_failure_keeps_the_pending_tail_in_the_device_state() {
    // `get_chars_apply` is entered from `get_chars_notempty` with the pending incomplete tail
    // *as the device buffer* (`file_io_server.erl:611-615`), so a failure raised inside it —
    // the continuation's own `Mod:Func` raising included — is caught with that tail still in
    // `State#state{buf=B}` (`:686-691`).
    //
    // `read` #1 yields "a" plus the first two bytes of U+2022 (`E2 82`) and returns
    // `{more, S1}` with the tail `E2 82`; `read` #2 completes "•b" and returns `{more, S2}`
    // with the tail `E2 82` again; `read` #3 completes "•" but the continuation then raises.
    let (source, reads) =
        ScriptedSource::new(&[&[0x61, 0xE2, 0x82], &[0xA2, 0x62, 0xE2, 0x82], &[0xA2]]);
    let mut device = IoDevice::new(source, Encoding::Unicode, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::More, Step::More, Step::Fail]);

    let error = device
        .get_until(&mut cont, Encoding::Unicode)
        .expect_err("the continuation failed in the second {{more,…}} iteration");

    assert!(matches!(error, BeamError::BadMatch), "{error:?}");
    assert_eq!(reads.get(), 3, "the loop read a third chunk before failing");
    assert_eq!(
        cont.seen.len(),
        3,
        "the continuation was applied once per read"
    );
    assert_eq!(
        device.buffer(),
        &[0xE2, 0x82],
        "the failing iteration's pending incomplete tail is kept as the device buffer"
    );
}

#[test]
fn a_binary_unicode_leftover_is_transcoded_back_to_the_stream_encoding() {
    // The request's output encoding is `unicode`; the device stream is `latin1`. A binary
    // leftover in the output encoding is transcoded back to the stream encoding.
    let (source, _) = ScriptedSource::new(&[b"x"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![Step::Done {
        reply: Reply::Binary(vec![]),
        leftover: Leftover::Binary(vec![0xC3, 0xA9]), // "é" in UTF-8
    }]);

    device
        .get_until(&mut cont, Encoding::Unicode)
        .expect("a reply");

    assert_eq!(device.buffer(), &[0xE9]);
}

#[test]
fn a_read_error_is_propagated() {
    let mut device = IoDevice::new(BrokenSource, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Mock::new(vec![done(Reply::Binary(vec![]))]);

    let error = device
        .get_until(&mut cont, Encoding::Latin1)
        .expect_err("the source failed");

    assert!(matches!(error, BeamError::Io(_)), "{error:?}");
    assert!(cont.seen.is_empty(), "the continuation is never applied");
}

#[test]
fn no_engine_continuation_is_the_typed_unsupported() {
    let (source, _) = ScriptedSource::new(&[b"data"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = NoEngineContinuation;

    let error = device
        .get_until(&mut cont, Encoding::Latin1)
        .expect_err("no continuation is installed");

    assert!(error.is_raise());
    let raise = error.into_raise().expect("a raise");
    let reason = raise.reason.as_atom().expect("an atom reason");
    assert!(reason.starts_with("unsupported: "), "{reason}");
    assert!(reason.contains("erl_scan:tokens/4"), "{reason}");
}

#[test]
fn a_continuation_that_raises_is_propagated_unchanged() {
    /// A continuation whose application always raises.
    struct Raising;
    impl Continuation for Raising {
        type Cont = ();
        type Reply = ();
        fn apply(
            &mut self,
            _cont: (),
            _data: ContData,
            _encoding: Encoding,
        ) -> Result<Applied<(), ()>> {
            Err(BeamError::BadMatch)
        }
    }

    let (source, _) = ScriptedSource::new(&[b"data"]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);
    let mut cont = Raising;

    assert!(matches!(
        device.get_until(&mut cont, Encoding::Latin1),
        Err(BeamError::BadMatch)
    ));
}

#[test]
fn the_device_state_is_observable_and_settable() {
    let (source, _) = ScriptedSource::new(&[]);
    let mut device = IoDevice::new(source, Encoding::Latin1, FileDataMode::Binary);

    assert_eq!(device.encoding(), Encoding::Latin1);
    assert_eq!(device.read_mode(), FileDataMode::Binary);
    assert!(device.buffer().is_empty());
    assert!(!device.has_stashed_buffer());

    device.set_encoding(Encoding::Unicode);
    device.set_read_mode(FileDataMode::List);
    device.set_buffer(b"stash".to_vec());
    assert_eq!(device.encoding(), Encoding::Unicode);
    assert_eq!(device.read_mode(), FileDataMode::List);
    assert_eq!(device.buffer(), b"stash");
    assert!(device.has_stashed_buffer());
    assert_eq!(device.take_buffer(), b"stash".to_vec());
    assert!(device.buffer().is_empty());
    assert!(device.source_mut().read_chunk(1).expect("read").is_none());
}
