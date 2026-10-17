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
//! The port-stream **term shape** of a driver-produced reply, against the live oracle.
//!
//! A `binary` port does not always deliver a binary: the shape is a function of the driver
//! output call that produced the bytes, never of the bytes themselves
//! ([`StreamShape`]'s table; `deliver_read_message`, `erts/emulator/beam/io.c:3363`, and
//! `deliver_vec_message`, `:3501`). The legacy C renders the call's **header** bytes as list
//! elements and its **data** bytes as the stream asks, so:
//!
//! | the driver called | a `[binary]` port's `{data, D}` | measured (erts-17.1) |
//! |---|---|---|
//! | `driver_output2(hbuf, hlen, NULL, 0)` (a `ram_file_drv` `OK`/`NUMBER`/`ERROR` reply) | `[H1..Hh]` | `[3,0,0,0,0]`, `[0]`, `[1,101,105,110,118,97,108]` |
//! | `driver_output_binary(hbuf, hlen, bin, 0, n)` (a `DATA` reply, `n >= 1`) | `[H1..Hh\|<<Data>>]` | `[2,0,0,0,2\|<<97,98>>]` |
//! | the same, `n == 0` (a read at end-of-file) | `[H1..Hh\|<<>>]` | `[2,0,0,0,0\|<<>>]` |
//! | the default `list` stream, **any** of the above | a proper list of every byte | `[2,0,0,0,2,97,98]` |
//!
//! Every row below is one `ram_file_drv` reply, driven through the runtime's **own** message
//! builder ([`RuntimeContext::port_data_message_shaped`]) with the [`StreamShape`] of the output
//! call that produced it, and compared with the system `erl`'s rendering of the very same
//! command sequence. The oracle half is a **loud skip** when the toolchain is absent, never a
//! silent pass.
//!
//! # What this suite does and does not prove
//!
//! It proves the *builder*: given the shape, the runtime renders the oracle's exact term, on both
//! streams. It cannot prove the *producer*: [`beam_io`] folds every driver output call into one
//! flat byte queue (`beam_io::DriverContext::input`), so the port layer is handed bytes without
//! the header/data split and reports [`StreamShape::Data`]. That residual — the `[binary]` stream
//! delivering one binary where the oracle delivers a list — is pinned by
//! [`super::ram_file_matrix_tests`], and the exact `beam_io` change that closes it is recorded in
//! this node's `CONTEXT.md` (*Handoffs*).
use super::ram_file_matrix_tests::oracle_output;
use super::tests::text_of;
use super::*;
use crate::config::RuntimeConfig;
/// One measured reply: the `port_command/2` payload that provokes it, the reply bytes the driver
/// answers with, the shape of the output call that carried them, and the oracle's rendering of the
/// payload on a `[binary]` port.
struct Reply {
    /// The row's label, printed by both sides.
    name: &'static str,
    /// The `rfile_command` wire payload (`opcode` ++ operands).
    command: &'static [u8],
    /// The reply bytes, exactly as the driver emits them.
    bytes: &'static [u8],
    /// The output call's header/data split.
    shape: StreamShape,
    /// The oracle's `~w` rendering of the payload on a `[binary]` port.
    binary_term: &'static str,
    /// The oracle's `~w` rendering on the default (`list`) stream.
    list_term: &'static str,
}
/// `Header`: `numeric_reply`/`reply`/`error_reply` all go through
/// `driver_output2(port, buf, len, NULL, 0)` (`ram_file_drv.c:181-228`), so a `binary` port gets a
/// **proper** list of the reply's bytes.
const OPEN: Reply = Reply {
    name: "open",
    command: &[1, 0, 0, 0, 3, 97, 98, 99],
    bytes: &[3, 0, 0, 0, 0],
    shape: StreamShape::Header,
    binary_term: "[3,0,0,0,0]",
    list_term: "[3,0,0,0,0]",
};
/// `HeaderThenData`: `READ` answers through `driver_output_binary(port, header, 5, bin, 0, n)`
/// (`ram_file_drv.c:448`) — the five header bytes are list elements, the payload the binary tail.
const READ_DATA: Reply = Reply {
    name: "read_data",
    command: &[2, 0, 0, 0, 2],
    bytes: &[2, 0, 0, 0, 2, 97, 98],
    shape: StreamShape::HeaderThenData { len: 5 },
    binary_term: "[2,0,0,0,2|<<97,98>>]",
    list_term: "[2,0,0,0,2,97,98]",
};
/// The clamping read: one byte left in the file.
const READ_CLAMPED: Reply = Reply {
    name: "read_clamped",
    command: &[2, 0, 0, 0, 2],
    bytes: &[2, 0, 0, 0, 1, 99],
    shape: StreamShape::HeaderThenData { len: 5 },
    binary_term: "[2,0,0,0,1|<<99>>]",
    list_term: "[2,0,0,0,1,99]",
};
/// A read at end-of-file: a **zero-length** `DATA` reply still carries a data part, so the term is
/// `[H1..H5|<<>>]` — an improper list ending in the empty binary, never a bare header list.
const READ_EOF: Reply = Reply {
    name: "read_eof",
    command: &[2, 0, 0, 0, 2],
    bytes: &[2, 0, 0, 0, 0],
    shape: StreamShape::HeaderThenData { len: 5 },
    binary_term: "[2,0,0,0,0|<<>>]",
    list_term: "[2,0,0,0,0]",
};
/// The bare `OK` reply — a one-byte header and no data part.
const TRUNCATE: Reply = Reply {
    name: "truncate",
    command: &[14],
    bytes: &[0],
    shape: StreamShape::Header,
    binary_term: "[0]",
    list_term: "[0]",
};
/// An `ERROR <errno>` refusal (`einval`), again header-only.
const PREAD_NEG: Reply = Reply {
    name: "pread_neg",
    command: &[17, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 3],
    bytes: &[1, 101, 105, 110, 118, 97, 108],
    shape: StreamShape::Header,
    binary_term: "[1,101,105,110,118,97,108]",
    list_term: "[1,101,105,110,118,97,108]",
};
/// The session, in an order where every reply is load-bearing: the file is opened at 3 bytes, two
/// 2-byte reads exhaust it (the second clamped to 1 byte), a third read is the end-of-file zero
/// `DATA`, then the header-only `OK` and `ERROR` replies.
#[rustfmt::skip] // one row per line: the matrix is read as a table, not as code.
const SESSION: &[Reply] = &[
    OPEN, READ_DATA, READ_CLAMPED, READ_EOF, TRUNCATE, PREAD_NEG,
];
/// Builds one port message through the runtime's builder and returns its `Data` payload.
fn payload(bytes: &[u8], stream: StreamType, shape: StreamShape) -> Eterm {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let port = PortId::new(1);
    let mut ctx = RuntimeContext::new(&mut rt);
    let message = ctx
        .port_data_message_shaped(port, bytes, stream, shape)
        .unwrap_or_else(|error| panic!("a message for {bytes:?} ({shape:?}): {error}"));
    let inner = terms::tuple_element(message, 1).expect("the `{data, …}` tuple");
    terms::tuple_element(inner, 1).expect("the data payload")
}
/// The oracle expression for [`SESSION`]: open `ram_file_drv` with `options`, send every row in
/// order and print one `{name, Reply}` line per row.
fn session_expr(options: &str) -> String {
    let mut expr = format!(
        "begin \
         P = open_port({{spawn_driver,\"ram_file_drv\"}},{options}), \
         C = fun(Cmd) -> true = port_command(P, Cmd), \
                         receive {{P,{{data,D}}}} -> D after 2000 -> timeout end end, \
         F = fun(N,Cmd) -> io:format(\"~w~n\",[{{N,C(Cmd)}}]) end, "
    );
    for reply in SESSION {
        let rendered: Vec<String> = reply.command.iter().map(u8::to_string).collect();
        expr.push_str(&format!(
            "F({},<<{}>>),",
            reply.name,
            rendered.join(",")
        ));
    }
    expr.push_str("ok end");
    expr
}
/// The whole session on a `[binary]` port: every reply kind renders the oracle's shape — a proper
/// list for `OK`/`NUMBER`/`ERROR`, an improper `[header|binary]` for `DATA` (empty or not) — and
/// the `list` stream renders the same bytes as a proper list whatever the split.
#[test]
fn every_reply_kind_renders_the_oracles_stream_shape() {
    let mut our_binary = Vec::new();
    let mut our_list = Vec::new();
    for reply in SESSION {
        let binary = text_of(payload(reply.bytes, StreamType::Binary, reply.shape));
        assert_eq!(
            binary, reply.binary_term,
            "{}: a `[binary]` port renders the C's header/data split",
            reply.name
        );
        our_binary.push(format!("{{{},{binary}}}", reply.name));
        // The `list` stream is insensitive to the split: header and data are both list elements.
        let list = text_of(payload(reply.bytes, StreamType::List, reply.shape));
        assert_eq!(
            list, reply.list_term,
            "{}: the default stream is a proper list of every byte, whatever the split",
            reply.name
        );
        our_list.push(format!("{{{},{}}}", reply.name, reply.list_term));
    }
    // ...and the oracle really renders those terms for this very command sequence.
    for (options, ours, what) in [
        ("[binary]", &our_binary, "the `[binary]` stream"),
        ("[]", &our_list, "the default `list` stream"),
    ] {
        match oracle_output(&session_expr(options)) {
            Some(oracle) => {
                assert_eq!(
                    ours.join("\n"),
                    oracle,
                    "{what}: every reply kind must render the oracle's term"
                );
                oracle_agreed += 1;
            }
            None => skip_oracle_check(what),
        }
    }
}
/// A list-stream payload is a **proper** list of every byte, for every shape — the split may not
/// leak a stray `ImproperList` into the default stream.
#[test]
fn the_list_stream_ignores_the_header_split() {
    for shape in [
        StreamShape::Data,
        StreamShape::Header,
        StreamShape::HeaderThenData { len: 5 },
        // A header longer than the payload is clamped, never a dropped byte.
        StreamShape::HeaderThenData { len: 99 },
    ] {
        let term = payload(&[2, 0, 0, 0, 2, 97, 98], StreamType::List, shape);
        assert_eq!(
            text_of(term),
            "[2,0,0,0,2,97,98]",
            "{shape:?}: the list stream is every byte as list elements"
        );
        assert!(
            terms::list_elements(term).is_ok(),
            "{shape:?}: the list-stream payload is a proper list"
        );
    }
}
/// A header-less call ([`StreamShape::Data`]) stays the binary a byte-stream read has always
/// been — the shape machinery must not change `driver_output`'s own rendering.
#[test]
fn a_headerless_call_is_still_a_binary() {
    let term = payload(b"abc", StreamType::Binary, StreamShape::Data);
    assert_eq!(
        terms::binary_bytes(term).expect("a header-less binary-stream read is a binary"),
        b"abc"
    );
    // ...and an empty header on a header-plus-data call degenerates to the same bare binary.
    let term = payload(b"abc", StreamType::Binary, StreamShape::HeaderThenData { len: 0 });
    assert_eq!(
        terms::binary_bytes(term).expect("an empty header leaves the data binary"),
        b"abc"
    );
}
/// A **line-framed** frame renders its payload by the stream bit inside the `{eol|noeol, …}`
/// wrapper, exactly as an unframed read renders it: a binary for a `binary` port, a char list for
/// the default one. (The wrapper itself is the C's `deliver_linebuf_message`, `io.c:3454`; a frame
/// carries no header/data split of its own — its bytes come from [`beam_io`]'s line accumulator,
/// not from one driver output call.)
#[test]
fn a_line_frame_wraps_the_stream_shaped_payload() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let port = PortId::new(1);
    let mut fx = super::tests::fixture();
    let eol = fx.atom("eol");
    let noeol = fx.atom("noeol");
    for (eol_flag, tag) in [(true, eol), (false, noeol)] {
        let binary = {
            let mut ctx = RuntimeContext::new(&mut rt);
            let message = ctx
                .port_line_message(port, b"ab", eol_flag, StreamType::Binary)
                .expect("a binary-stream line frame");
            terms::tuple_element(
                terms::tuple_element(message, 1).expect("the `{data, …}` pair"),
                1,
            )
            .expect("the frame")
        };
        let framed = terms::tuple_element(binary, 0).expect("the `{eol|noeol, Data}` tuple");
        assert_eq!(framed, make_atom(tag), "the frame's tag");
        let data = terms::tuple_element(binary, 1).expect("the frame's data");
        assert_eq!(
            terms::binary_bytes(data).expect("a binary-stream frame's data is a binary"),
            b"ab"
        );
        let list = {
            let mut ctx = RuntimeContext::new(&mut rt);
            let message = ctx
                .port_line_message(port, b"ab", eol_flag, StreamType::List)
                .expect("a list-stream line frame");
            terms::tuple_element(
                terms::tuple_element(message, 1).expect("the `{data, …}` pair"),
                1,
            )
            .expect("the frame")
        };
        let data = terms::tuple_element(list, 1).expect("the frame's data");
        assert_eq!(text_of(data), "[97,98]", "a list-stream frame is a char list");
    }
}
