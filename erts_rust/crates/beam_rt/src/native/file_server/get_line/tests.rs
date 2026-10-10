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

//! Unit tests for the `{get_line, …}` io-device request ([`super`]).
//!
//! The collect rule ([`LineContinuation`]) and the reply cast ([`cast`]) are pinned directly,
//! and the request is driven through the **real** dispatch arm
//! ([`Runtime::serve_device_request`]) over real temporary files — the very code path the
//! `file_server_2` service runs.
//!
//! Every oracle value quoted below was verified against the system `erl` (OTP 29 / erts-17.1)
//! by the [`the_replies_match_the_live_oracle`] cross-check, which re-runs the same cases under
//! the live oracle on every test run; a test whose prerequisite is a real system Erlang skips
//! **loudly** (`eprintln!` + early return), never silently passing.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use beam_engine::terms;

use super::super::tests::{build, runtime};
use super::super::Device;
use super::*;

// -- fixtures -------------------------------------------------------------------------

/// Registers `file` as a fresh io-device — the shape [`super::super::Runtime::reply_open`]
/// builds — and returns the pid it is keyed by.
///
/// The devices map is keyed by pid and only ever read by pid, so the request path is exercised
/// without a process-table entry (which `Runtime` exposes no mutable accessor for); the pid is
/// deliberately outside the table's number range.
fn open_device(runtime: &mut Runtime, file: File, encoding: Encoding, binary: bool) -> ProcessId {
    let pid = ProcessId::new(900_000, 1);
    runtime.file_services.devices.insert(
        pid,
        Device {
            file,
            binary,
            encoding,
            buf: Vec::new(),
        },
    );
    pid
}

/// Runs `f` over a unique scratch file under `std::env::temp_dir()` holding `contents`,
/// removing it afterwards.
///
/// The contents are **bytes**: several cases below are deliberately not valid UTF-8 (a
/// `latin1` device's `caf\xe9`).
fn with_scratch<R>(name: &str, contents: &[u8], f: impl FnOnce(&Path) -> R) -> R {
    let mut path = std::env::temp_dir();
    path.push(format!("beam_rt_get_line_{}_{name}.bin", std::process::id()));
    let mut file = File::create(&path).expect("a scratch file");
    file.write_all(contents).expect("writing the scratch file");
    drop(file);
    let result = f(&path);
    let _ = std::fs::remove_file(&path);
    result
}

/// The `~w` rendering of `term`, without the printer's trailing newline — the oracle's own
/// `io_lib:format("~w", [Term])`, which is what the values below are compared against.
fn render(runtime: &mut Runtime, term: Eterm) -> String {
    let rendered = runtime.format_term_w(term, None).expect("a rendered term");
    rendered
        .strip_suffix('\n')
        .map(str::to_owned)
        .unwrap_or(rendered)
}

/// The `~w` form of a list of small integers.
fn int_list(values: &[u32]) -> String {
    let inner: Vec<String> = values.iter().map(u32::to_string).collect();
    format!("[{}]", inner.join(","))
}

/// The `~w` form of a byte-aligned binary.
fn byte_binary(bytes: &[u8]) -> String {
    let inner: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("<<{}>>", inner.join(","))
}

/// `n` copies of `value`.
fn repeat(value: u32, n: usize) -> Vec<u32> {
    vec![value; n]
}

/// `n` bytes of `value`.
fn repeat_bytes(value: u8, n: usize) -> Vec<u8> {
    vec![value; n]
}

/// A `{get_line, Enc, Prompt}` request with the empty prompt `''`.
fn line_request(runtime: &mut Runtime, enc: &str) -> Eterm {
    build(runtime, |ctx| {
        let get_line = ctx.atom("get_line").unwrap();
        let encoding = ctx.atom(enc).unwrap();
        let prompt = terms::make_list(ctx, &[]).unwrap();
        terms::make_tuple(ctx, &[get_line, encoding, prompt]).unwrap()
    })
}

/// The legacy `{get_line, Prompt}` request, whose encoding defaults to `latin1`
/// (`file_io_server.erl:412-413`).
fn legacy_line_request(runtime: &mut Runtime) -> Eterm {
    build(runtime, |ctx| {
        let get_line = ctx.atom("get_line").unwrap();
        let prompt = terms::make_list(ctx, &[]).unwrap();
        terms::make_tuple(ctx, &[get_line, prompt]).unwrap()
    })
}

/// A request whose `Enc` is `term` — used for the typed capability gap.
fn request_with_enc(runtime: &mut Runtime, term: Eterm) -> Eterm {
    build(runtime, |ctx| {
        let get_line = ctx.atom("get_line").unwrap();
        let prompt = terms::make_list(ctx, &[]).unwrap();
        terms::make_tuple(ctx, &[get_line, term, prompt]).unwrap()
    })
}

/// Serves one request through the real dispatch arm, asserting it is a plain reply (the device
/// is not retired) and returning its `~w` rendering.
fn read_line(runtime: &mut Runtime, pid: ProcessId, enc: &str) -> String {
    let request = line_request(runtime, enc);
    let (reply, retire) = runtime
        .serve_device_request(pid, request)
        .expect("a served request");
    assert!(!retire, "a plain line read never retires the device");
    render(runtime, reply)
}

/// The reason atom of a typed capability gap.
fn gap_reason(error: beam_common::BeamError) -> String {
    let raise = error
        .into_raise()
        .expect("a capability gap is an explicit raise");
    raise
        .reason
        .as_atom()
        .expect("an atom reason")
        .to_string()
}

/// The stashed device buffer (`State#state.buf`) of `pid`.
fn device_buffer(runtime: &Runtime, pid: ProcessId) -> Vec<u8> {
    runtime.file_services.devices[&pid].buf.clone()
}

// -- the collect rule (`io_lib:collect_line/4`) ---------------------------------------

/// Apples the continuation once to one chunk of code points.
fn apply(cont: Option<Vec<u32>>, data: ContData) -> Applied<Option<Vec<u32>>, LineReply> {
    LineContinuation
        .apply(cont, data, Encoding::Latin1)
        .expect("the collect rule never fails")
}

fn chars(text: &[u8]) -> ContData {
    ContData::Chars(text.iter().map(|&byte| u32::from(byte)).collect())
}

/// The LF terminates the line and the LF is kept.
#[test]
fn the_collect_rule_terminates_on_lf() {
    let done = apply(None, chars(b"hello\nrest"));
    assert_eq!(
        done,
        Applied::Done {
            reply: LineReply::Line(vec![104, 101, 108, 108, 111, 10]),
            leftover: Leftover::Chars(vec![114, 101, 115, 116]),
        }
    );
}

/// A CR immediately before the LF is dropped — `collect_line_list([$\n|T], [$\r|Stack])`.
#[test]
fn the_collect_rule_drops_a_cr_before_the_lf() {
    let done = apply(None, chars(b"a\r\nb"));
    assert_eq!(
        done,
        Applied::Done {
            reply: LineReply::Line(vec![97, 10]),
            leftover: Leftover::Chars(vec![98]),
        }
    );
}

/// A CR not followed by an LF is kept.
#[test]
fn the_collect_rule_keeps_a_lone_cr() {
    let done = apply(None, chars(b"a\rb\n"));
    assert_eq!(
        done,
        Applied::Done {
            reply: LineReply::Line(vec![97, 13, 98, 10]),
            leftover: Leftover::None,
        }
    );
}

/// A CR that ends one chunk and an LF that begins the next collapse too — the binary
/// `N =:= 1` clause (`io_lib.erl:1803-1813`).
#[test]
fn the_collect_rule_collapses_a_cr_lf_split_across_chunks() {
    let more = apply(None, chars(b"abc\r"));
    assert_eq!(more, Applied::More(Some(vec![97, 98, 99, 13])));
    let done = apply(Some(vec![97, 98, 99, 13]), chars(b"\nrest"));
    assert_eq!(
        done,
        Applied::Done {
            reply: LineReply::Line(vec![97, 98, 99, 10]),
            leftover: Leftover::Chars(vec![114, 101, 115, 116]),
        }
    );
}

/// The same chunk-final CR followed by a **non-LF** keeps the CR (the oracle's `N =/= 1`
/// fallback, `io_lib.erl:1811-1812`).
#[test]
fn the_collect_rule_keeps_a_chunk_final_cr_when_the_next_byte_is_not_lf() {
    let done = apply(Some(vec![97, 114, 13]), chars(b"y\n"));
    assert_eq!(
        done,
        Applied::Done {
            reply: LineReply::Line(vec![97, 114, 13, 121, 10]),
            leftover: Leftover::None,
        }
    );
}

/// A chunk without an LF accumulates; `eof` answers what was accumulated (or `eof` when
/// nothing was).
#[test]
fn the_collect_rule_accumulates_without_lf_and_answers_eof() {
    let more = apply(None, chars(b"abc"));
    assert_eq!(more, Applied::More(Some(vec![97, 98, 99])));
    let done = apply(Some(vec![97, 98, 99]), ContData::Eof);
    assert_eq!(
        done,
        Applied::Done {
            reply: LineReply::Line(vec![97, 98, 99]),
            leftover: Leftover::None,
        }
    );
    let empty = apply(None, ContData::Eof);
    assert_eq!(
        empty,
        Applied::Done {
            reply: LineReply::Eof,
            leftover: Leftover::None,
        }
    );
}

/// The line is terminated by the **first** LF, and the tail carries over into the next chunk.
#[test]
fn the_collect_rule_reports_the_first_line_and_the_tail() {
    // Two chunks, the first of which holds a complete line.
    let more = apply(None, chars(b"one\ntwo"));
    assert_eq!(
        more,
        Applied::Done {
            reply: LineReply::Line(vec![111, 110, 101, 10]),
            leftover: Leftover::Chars(vec![116, 119, 111]),
        }
    );
}

/// `\r\r\n` collapses only the CR next to the LF.
#[test]
fn the_collect_rule_collapses_one_cr() {
    assert_eq!(
        apply(None, chars(b"\r\r\n")),
        Applied::Done {
            reply: LineReply::Line(vec![13, 10]),
            leftover: Leftover::None,
        }
    );
}

// -- the reply cast (`cast/4`) --------------------------------------------------------

/// Renders one `cast` outcome — `Err(())` for the oracle's `no_translation` exit.
#[allow(clippy::result_unit_err)]
fn casted(
    runtime: &mut Runtime,
    line: &LineReply,
    mode: FileDataMode,
    stream: Encoding,
    out: Encoding,
) -> std::result::Result<String, ()> {
    match cast(runtime, line, mode, stream, out).expect("a cast") {
        Casted::Reply(term) => Ok(render(runtime, term)),
        Casted::NoTranslation => Err(()),
    }
}

/// `cast(eof, _, _, _) -> eof` (`file_io_server.erl:782-783`).
#[test]
fn the_cast_answers_eof_for_every_mode() {
    let mut runtime = runtime();
    for mode in [FileDataMode::List, FileDataMode::Binary] {
        for stream in [Encoding::Latin1, Encoding::Unicode] {
            for out in [Encoding::Latin1, Encoding::Unicode] {
                assert_eq!(
                    casted(&mut runtime, &LineReply::Eof, mode, stream, out),
                    Ok("eof".to_string())
                );
            }
        }
    }
}

/// `cast(B, list, latin1, _) -> binary_to_list(B)` — the request encoding is ignored.
#[test]
fn the_cast_ignores_the_request_encoding_for_a_latin1_list_device() {
    let mut runtime = runtime();
    let line = LineReply::Line(vec![104, 105, 10]);
    assert_eq!(
        casted(&mut runtime, &line, FileDataMode::List, Encoding::Latin1, Encoding::Latin1),
        Ok(int_list(&[104, 105, 10]))
    );
    assert_eq!(
        casted(&mut runtime, &line, FileDataMode::List, Encoding::Latin1, Encoding::Unicode),
        Ok(int_list(&[104, 105, 10]))
    );
}

/// `cast(B, binary, latin1, latin1) -> B`, and `cast(B, binary, latin1, unicode)` UTF-8
/// encodes those bytes.
#[test]
fn the_cast_encodes_a_latin1_binary_device() {
    let mut runtime = runtime();
    // `caf\xe9\n` — U+00E9 is two bytes in UTF-8.
    let line = LineReply::Line(vec![99, 97, 102, 233, 10]);
    assert_eq!(
        casted(&mut runtime, &line, FileDataMode::Binary, Encoding::Latin1, Encoding::Latin1),
        Ok(byte_binary(&[99, 97, 102, 233, 10]))
    );
    assert_eq!(
        casted(&mut runtime, &line, FileDataMode::Binary, Encoding::Latin1, Encoding::Unicode),
        Ok(byte_binary(&[99, 97, 102, 195, 169, 10]))
    );
    // The list mode never validates: `binary_to_list(B)` keeps the raw byte.
    assert_eq!(
        casted(&mut runtime, &line, FileDataMode::List, Encoding::Latin1, Encoding::Unicode),
        Ok(int_list(&[99, 97, 102, 233, 10]))
    );
}

/// A `unicode` device narrows to the request's encoding: `unicode` keeps the code points,
/// `latin1` drops out on a code point that does not fit a byte.
#[test]
fn the_cast_narrows_a_unicode_device() {
    let mut runtime = runtime();
    // `a€b\n` — U+20AC is above 255.
    let wide = LineReply::Line(vec![97, 8364, 98, 10]);
    assert_eq!(
        casted(&mut runtime, &wide, FileDataMode::List, Encoding::Unicode, Encoding::Unicode),
        Ok(int_list(&[97, 8364, 98, 10]))
    );
    assert_eq!(
        casted(&mut runtime, &wide, FileDataMode::Binary, Encoding::Unicode, Encoding::Unicode),
        Ok(byte_binary(&[97, 226, 130, 172, 98, 10]))
    );
    assert_eq!(
        casted(&mut runtime, &wide, FileDataMode::List, Encoding::Unicode, Encoding::Latin1),
        Err(())
    );
    assert_eq!(
        casted(&mut runtime, &wide, FileDataMode::Binary, Encoding::Unicode, Encoding::Latin1),
        Err(())
    );

    // A code point that fits a byte is fine in every combination.
    let narrow = LineReply::Line(vec![104, 233, 10]);
    assert_eq!(
        casted(&mut runtime, &narrow, FileDataMode::List, Encoding::Unicode, Encoding::Latin1),
        Ok(int_list(&[104, 233, 10]))
    );
    assert_eq!(
        casted(&mut runtime, &narrow, FileDataMode::Binary, Encoding::Unicode, Encoding::Latin1),
        Ok(byte_binary(&[104, 233, 10]))
    );
}

/// The two encoding helpers: latin1 is a byte-range check, UTF-8 is the scalar-value rule.
#[test]
fn the_encoding_helpers_report_what_does_not_fit() {
    assert_eq!(latin1_bytes(&[0, 255]), Some(vec![0, 255]));
    assert_eq!(latin1_bytes(&[0, 256]), None);
    assert_eq!(utf8_bytes(&[104, 233]), Some(vec![104, 195, 169]));
    assert_eq!(utf8_bytes(&[0xD800]), None, "a lone surrogate is not a code point");
    assert_eq!(utf8_bytes(&[0x11_0000]), None);
}

// -- the request end to end (`Runtime::serve_device_request`) --------------------------

/// The canonical oracle sequence: `printf 'hello\nworld\n' > f; file:read_line(f)` three times
/// answers `{ok,"hello\n"}`, `{ok,"world\n"}`, `eof` — a level-by-level list device gives
/// `[104,101,108,108,111,10]`, `[119,111,114,108,100,10]`, `eof`.
#[test]
fn a_latin1_list_device_reads_lines_and_then_eof() {
    let mut runtime = runtime();
    with_scratch("lf_list", b"hello\nworld\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[104, 101, 108, 108, 111, 10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[119, 111, 114, 108, 100, 10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
        assert!(device_buffer(&runtime, pid).is_empty());
    });
}

/// A binary-mode device answers a binary: `{ok,<<"hello\n">>}`, `{ok,<<"world\n">>}`, `eof`.
#[test]
fn a_latin1_binary_device_reads_binary_lines() {
    let mut runtime = runtime();
    with_scratch("lf_binary", b"hello\nworld\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            true,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), byte_binary(b"hello\n"));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), byte_binary(b"world\n"));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// `printf 'a\r\nb\r\n'` — the CR is dropped and the LF kept, in both read modes.
#[test]
fn a_crlf_no_longer_leaks_the_cr() {
    let mut runtime = runtime();
    with_scratch("crlf_list", b"a\r\nb\r\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[97, 10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[98, 10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
    with_scratch("crlf_binary", b"a\r\nb\r\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            true,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), byte_binary(b"a\n"));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), byte_binary(b"b\n"));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// The CR sits at the 128-byte read boundary and the LF in the next chunk: `127 x` + `\r\n`
/// answers `127 x` ++ `[10]`, i.e. the split pair still collapses.
#[test]
fn a_cr_lf_split_across_the_read_boundary_collapses() {
    let mut runtime = runtime();
    let mut contents = repeat_bytes(b'x', 127);
    contents.extend_from_slice(b"\r\n");
    let mut expected = repeat(u32::from(b'x'), 127);
    expected.push(10);
    with_scratch("crlf_boundary", &contents, |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&expected));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// The same boundary with a byte that is **not** an LF after the CR keeps it:
/// `127 x` + `\r` + `y\n` answers `127 x` ++ `[13,121,10]`.
#[test]
fn a_chunk_final_cr_before_a_non_lf_is_kept() {
    let mut runtime = runtime();
    let mut contents = repeat_bytes(b'x', 127);
    contents.extend_from_slice(b"\ry\n");
    let mut expected = repeat(u32::from(b'x'), 127);
    expected.extend_from_slice(&[13, 121, 10]);
    with_scratch("cr_boundary_kept", &contents, |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&expected));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// `128 z` + `\r\ntail\n`: the first line is exactly 128 bytes plus the LF, and the tail is
/// carried over in the device buffer and served by the **second** request.
#[test]
fn a_full_chunk_line_leaves_the_tail_in_the_device_buffer() {
    let mut runtime = runtime();
    let mut contents = repeat_bytes(b'z', 128);
    contents.extend_from_slice(b"\r\ntail\n");
    let mut expected = repeat(u32::from(b'z'), 128);
    expected.push(10);
    with_scratch("full_chunk", &contents, |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&expected));
        assert_eq!(
            device_buffer(&runtime, pid),
            b"tail\n",
            "the unconsumed tail is the device's `State#state.buf`"
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[116, 97, 105, 108, 10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// A line longer than one read chunk is assembled across chunks: `200 a` + `\ntail\n`.
#[test]
fn a_line_spanning_many_chunks_is_assembled() {
    let mut runtime = runtime();
    let mut contents = repeat_bytes(b'a', 200);
    contents.extend_from_slice(b"\ntail\n");
    let mut expected = repeat(u32::from(b'a'), 200);
    expected.push(10);
    with_scratch("long_line", &contents, |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&expected));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[116, 97, 105, 108, 10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// A lone CR at the end of the file is **kept** (`printf 'abc\r'`).
#[test]
fn a_trailing_lone_cr_is_kept() {
    let mut runtime = runtime();
    with_scratch("lone_cr_eof", b"abc\r", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[97, 98, 99, 13]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// A final line with no newline is delivered as-is, then `eof`.
#[test]
fn a_final_line_without_a_newline_is_delivered() {
    let mut runtime = runtime();
    with_scratch("no_newline", b"abc", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[97, 98, 99]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// An empty file answers `eof` — twice, because the end of file is sticky.
#[test]
fn an_empty_file_answers_eof_twice() {
    let mut runtime = runtime();
    with_scratch("empty", b"", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// Bare newline / CRLF / CR files: `\n` and `\r\n` both answer `[10]`, a lone `\r` answers
/// `[13]` and then `eof`.
#[test]
fn the_newline_only_shapes() {
    let mut runtime = runtime();

    with_scratch("only_lf", b"\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });

    with_scratch("only_crlf", b"\r\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });

    with_scratch("only_cr", b"\r", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[13]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// `a\nb\n` over two requests proves the stashed leftover is fed **before** any fresh read
/// (the file itself is already at end of file on the second request).
#[test]
fn the_leftover_is_carried_between_requests() {
    let mut runtime = runtime();
    with_scratch("leftover", b"a\nb\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[97, 10]));
        assert_eq!(
            device_buffer(&runtime, pid),
            b"b\n",
            "the second line is stashed, not read again"
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[98, 10]));
        assert!(device_buffer(&runtime, pid).is_empty());
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
}

/// A `latin1` device with a non-ASCII byte: `caf\xe9\n` answers `[99,97,102,233,10]` (list)
/// and `<<99,97,102,233,10>>` (binary).
#[test]
fn a_latin1_device_keeps_its_bytes() {
    let mut runtime = runtime();
    with_scratch("latin1_bytes", b"caf\xe9\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[99, 97, 102, 233, 10]));
    });
    with_scratch("latin1_bytes_bin", b"caf\xe9\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            true,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), byte_binary(&[99, 97, 102, 233, 10]));
    });
}

/// `io:get_line/2` requests `unicode` on a `latin1` binary device, so the latin1 byte is
/// UTF-8 encoded: `caf\xe9\n` answers `<<99,97,102,195,169,10>>`.
#[test]
fn a_unicode_request_utf8_encodes_a_latin1_binary_device() {
    let mut runtime = runtime();
    with_scratch("latin1_binary_unicode", b"caf\xe9\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            true,
        );
        assert_eq!(read_line(&mut runtime, pid, "unicode"), byte_binary(&[99, 97, 102, 195, 169, 10]));
    });
}

/// A `unicode` device read with `{get_line, latin1, ''}` (what `file:read_line/1` sends)
/// answers the decoded code points as a **latin1** list: `héllo\nwörld\n` gives
/// `[104,233,108,108,111,10]` and `[119,246,114,108,100,10]`.
#[test]
fn a_unicode_device_read_as_latin1_answers_latin1_code_points() {
    let mut runtime = runtime();
    with_scratch("unicode_latin1", b"h\xc3\xa9llo\nw\xc3\xb6rld\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Unicode,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[104, 233, 108, 108, 111, 10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[119, 246, 114, 108, 100, 10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), "eof");
    });
    with_scratch("unicode_latin1_bin", b"h\xc3\xa9llo\nw\xc3\xb6rld\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Unicode,
            true,
        );
        assert_eq!(read_line(&mut runtime, pid, "latin1"), byte_binary(&[104, 233, 108, 108, 111, 10]));
        assert_eq!(read_line(&mut runtime, pid, "latin1"), byte_binary(&[119, 246, 114, 108, 100, 10]));
    });
}

/// `io:get_line/2` on a `unicode` device requests `unicode`: the code points are kept
/// (`a€b\n` ⇒ `[97,8364,98,10]`) or UTF-8 encoded for a binary device.
#[test]
fn a_unicode_device_read_as_unicode_keeps_the_code_points() {
    let mut runtime = runtime();
    with_scratch("unicode_unicode", b"a\xe2\x82\xacb\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Unicode,
            false,
        );
        assert_eq!(read_line(&mut runtime, pid, "unicode"), int_list(&[97, 8364, 98, 10]));
    });
    with_scratch("unicode_unicode_bin", b"a\xe2\x82\xacb\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Unicode,
            true,
        );
        assert_eq!(read_line(&mut runtime, pid, "unicode"), byte_binary(&[97, 226, 130, 172, 98, 10]));
    });
}

/// `file:read_line/1` on a `unicode` device asks for `latin1`, and a code point that does not
/// fit a byte is the oracle's `{error, {no_translation, unicode, latin1}}` **and kills the
/// device** (the reply is followed by a `{stop, …}`).
///
/// The second request therefore reaches a removed device; the oracle's caller would answer
/// `{error, terminated}`, this crate's "closed io device" typea gap — a documented divergence.
#[test]
fn a_code_point_that_does_not_fit_latin1_answers_no_translation_and_retires() {
    let mut runtime = runtime();
    with_scratch("no_translation", b"a\xe2\x82\xacb\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Unicode,
            false,
        );
        let request = line_request(&mut runtime, "latin1");
        let (reply, retire) = runtime
            .serve_device_request(pid, request)
            .expect("the request is served");
        assert!(retire, "the oracle replies and then dies: `{{stop, …}}`");
        assert_eq!(
            render(&mut runtime, reply),
            "{error,{no_translation,unicode,latin1}}"
        );

        // Serve the retire exactly as the `file_server_2` loop does.
        runtime.file_services.devices.remove(&pid);
        let second = line_request(&mut runtime, "latin1");
        let error = runtime
            .serve_device_request(pid, second)
            .expect_err("the device is gone");
        let reason = gap_reason(error);
        assert!(
            reason.contains("closed io device"),
            "the documented divergence: the oracle answers `{{error,terminated}}`: {reason}"
        );
    });
}

/// The legacy `{get_line, Prompt}` request behaves exactly as `{get_line, latin1, Prompt}`.
#[test]
fn the_legacy_request_defaults_the_encoding_to_latin1() {
    let mut runtime = runtime();
    with_scratch("legacy", b"ab\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        let request = legacy_line_request(&mut runtime);
        let (reply, retire) = runtime
            .serve_device_request(pid, request)
            .expect("a served request");
        assert!(!retire);
        assert_eq!(render(&mut runtime, reply), int_list(&[97, 98, 10]));
    });
}

/// A request encoding this runtime does not model — including a non-atom — is the typed
/// capability gap, never a guessed conversion (the oracle also serves `utf8`/`utf16`/`utf32`).
#[test]
fn an_unmodelled_request_encoding_is_the_typed_gap() {
    let mut runtime = runtime();
    with_scratch("bad_encoding", b"ab\n", |path| {
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Latin1,
            false,
        );
        let utf8 = build(&mut runtime, |ctx| ctx.atom("utf8").unwrap());
        let request = request_with_enc(&mut runtime, utf8);
        let error = runtime
            .serve_device_request(pid, request)
            .expect_err("`utf8` is not modelled");
        assert!(gap_reason(error).contains("{get_line"));

        // A non-atom `Enc` (the `{utf16,_}` tuple encoding) is the same gap.
        let tuple = build(&mut runtime, |ctx| {
            let utf16 = ctx.atom("utf16").unwrap();
            let binary = ctx.atom("binary").unwrap();
            terms::make_tuple(ctx, &[utf16, binary]).unwrap()
        });
        let request = request_with_enc(&mut runtime, tuple);
        let error = runtime
            .serve_device_request(pid, request)
            .expect_err("a tuple encoding is not modelled");
        assert!(gap_reason(error).contains("{get_line"));

        // ... and the device is still usable for a request it *does* serve.
        assert_eq!(read_line(&mut runtime, pid, "latin1"), int_list(&[97, 98, 10]));
    });
}

/// A request for a device the service no longer holds is the typed "closed io device" gap.
#[test]
fn a_request_for_an_unknown_device_is_the_typed_gap() {
    let mut runtime = runtime();
    let pid = ProcessId::new(900_001, 1);
    let request = line_request(&mut runtime, "latin1");
    let error = runtime
        .serve_device_request(pid, request)
        .expect_err("no such device");
    assert!(gap_reason(error).contains("closed io device"));
}

// -- the live-oracle cross-check ------------------------------------------------------

/// How long an oracle probe may run before it is killed and reaped.
const ORACLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `true` for an existing executable file.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The oracle `erl`: `$ERTS_ORACLE` when it is an executable, else `erl` on `$PATH`.
///
/// A **filesystem** probe only — the oracle is never *run* to locate it (a bare `erl` with no
/// terminator would never exit).
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        return is_executable(&program).then_some(program);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("erl"))
        .find(|candidate| is_executable(candidate))
}

/// Runs `erl -noshell -eval <eval> -s init stop`, **bounded** by [`ORACLE_TIMEOUT`].
///
/// The `-s init stop` makes the node terminate even when `eval` raises; the child is polled
/// and killed + reaped at the bound so a probe can never hang the suite.
fn run_oracle(oracle: &Path, eval: &str) -> Option<String> {
    use std::process::{Command, Stdio};

    let mut command = Command::new(oracle);
    command
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("warning: cannot spawn the oracle `{}`: {error}", oracle.display());
            return None;
        }
    };
    let deadline = std::time::Instant::now() + ORACLE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    eprintln!(
                        "warning: the oracle probe outlived the {}s bound and was killed",
                        ORACLE_TIMEOUT.as_secs()
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => {
                eprintln!("warning: cannot poll the oracle probe: {error}");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            eprintln!("warning: cannot collect the oracle probe's output: {error}");
            return None;
        }
    };
    if !output.status.success() {
        eprintln!(
            "warning: the oracle exited with {:?}\n--- stderr ---\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// The oracle's raw `{get_line, Enc, ''}` device replies for `count` successive requests —
/// one `~w` rendering per line.
///
/// The request is sent **directly** to the device pid, exactly the shape this runtime's
/// `serve_device_request` answers, so the comparison is byte-for-byte at the device boundary.
/// (`io:get_line/2` returns the same value: `net_kernel:dflag_unicode_io/1` is true for a
/// modern io server, so `io:bc_req/3` does not convert — `lib/stdlib/src/io.erl:1735-1739`.)
fn oracle_lines(
    oracle: &Path,
    path: &Path,
    open_opts: &str,
    enc: &str,
    count: usize,
) -> Option<Vec<String>> {
    let eval = format!(
        "{{ok,D}} = file:open(\"{}\",[{open_opts}]), \
         [begin M = make_ref(), D ! {{io_request,self(),M,{{get_line,{enc},''}}}}, \
          receive {{io_reply,M,X}} -> io:format(\"~w~n\",[X]) end end || _ <- lists:seq(1,{count})], \
         ok.",
        path.display()
    );
    let output = run_oracle(oracle, &eval)?;
    Some(output.lines().map(str::to_owned).collect())
}

/// The oracle's `io:get_line(D, "")` answers, one `~w` rendering per line.
fn oracle_get_line(oracle: &Path, path: &Path, open_opts: &str, count: usize) -> Option<Vec<String>> {
    let eval = format!(
        "{{ok,D}} = file:open(\"{}\",[{open_opts}]), \
         [io:format(\"~w~n\",[io:get_line(D,\"\")]) || _ <- lists:seq(1,{count})], \
         ok.",
        path.display()
    );
    let output = run_oracle(oracle, &eval)?;
    Some(output.lines().map(str::to_owned).collect())
}

/// Every pinned case re-run against the **live** oracle: the rendered device replies must
/// match byte-for-byte.
///
/// Skips **loudly** when no oracle is installed — a missing toolchain is never a silent pass.
#[test]
fn the_replies_match_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("=============== beam_rt get_line oracle SKIPPED ===============");
        eprintln!("no `erl` on $PATH (and no executable $ERTS_ORACLE): the oracle comparison");
        eprintln!("cannot run. Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
        eprintln!("==============================================================");
        eprintln!();
        return;
    };
    let mut runtime = runtime();

    // (name, contents, open options, request encoding, reads, read mode)
    let cases: [(&str, &[u8], &str, &str, usize, bool); 6] = [
        ("oracle_lf_list", b"hello\nworld\n", "read", "latin1", 3, false),
        ("oracle_crlf_list", b"a\r\nb\r\n", "read", "latin1", 3, false),
        ("oracle_no_newline", b"abc", "read", "latin1", 2, false),
        ("oracle_empty", b"", "read", "latin1", 2, false),
        ("oracle_lf_binary", b"hello\nworld\n", "read,binary", "latin1", 3, true),
        ("oracle_crlf_boundary", b"", "read", "latin1", 2, false),
    ];
    // The boundary case's contents are built below; only its slot is reserved here.
    let boundary_contents = {
        let mut contents = repeat_bytes(b'x', 127);
        contents.extend_from_slice(b"\r\n");
        contents
    };

    for (name, contents, open_opts, enc, reads, binary) in cases {
        let contents: &[u8] = if name == "oracle_crlf_boundary" {
            &boundary_contents
        } else {
            contents
        };
        let expected = with_scratch(name, contents, |path| {
            let Some(expected) = oracle_lines(&oracle, path, open_opts, enc, reads) else {
                eprintln!("warning: the oracle could not answer for `{name}` — skipped");
                return None;
            };
            let pid = open_device(
                &mut runtime,
                File::open(path).expect("the scratch file opens"),
                Encoding::Latin1,
                binary,
            );
            let actual: Vec<String> = (0..reads).map(|_| read_line(&mut runtime, pid, enc)).collect();
            assert_eq!(actual, expected, "the rendered replies for `{name}`");
            Some(())
        });
        if expected.is_none() {
            return;
        }
    }

    // The unicode case, through the library-level `io:get_line/2` (contract: `io:get_line`
    // requests `unicode`, and `dflag_unicode_io` makes the reply pass through unchanged).
    with_scratch("oracle_unicode_get_line", b"a\xe2\x82\xacb\n", |path| {
        let Some(expected) = oracle_get_line(&oracle, path, "read,unicode", 2) else {
            eprintln!("warning: the oracle could not answer for `oracle_unicode_get_line` — skipped");
            return;
        };
        let pid = open_device(
            &mut runtime,
            File::open(path).expect("the scratch file opens"),
            Encoding::Unicode,
            false,
        );
        let actual: Vec<String> = (0..2).map(|_| read_line(&mut runtime, pid, "unicode")).collect();
        assert_eq!(actual, expected, "the `io:get_line/2` replies");
    });
}
