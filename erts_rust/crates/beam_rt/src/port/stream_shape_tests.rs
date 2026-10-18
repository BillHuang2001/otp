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
//! ([`ReadMessage`]'s table; `deliver_read_message`, `erts/emulator/beam/io.c:3363`, and
//! `deliver_vec_message`, `:3501`). The legacy C renders the call's **header** bytes as list
//! elements and its **data** bytes as the stream asks, so with `HDR` as the header:
//!
//! | the driver called | a `[binary]` port's `{data, D}` | the default `list` stream |
//! |---|---|---|
//! | `driver_output2(hbuf, 3, NULL, 0)` (a `ram_file_drv` `OK`/`NUMBER`/`ERROR` reply) | `[72,68,82]` | `[72,68,82]` |
//! | `driver_output2(hbuf, 3, buf, n)`, `n >= 1` (a `DATA` reply) | `[72,68,82\|<<Data>>]` | `[72,68,82,D1..Dn]` |
//! | the same, `n == 0` (a read at end-of-file) | `[72,68,82\|<<>>]` | `[72,68,82]` |
//! | `driver_output(buf, n)` (no header at all) | `<<Data>>` | `[D1..Dn]` |
//! | `driver_output2(NULL, 0, NULL, 0)` | `[]` | `[]` |
//!
//! Every row of [`SESSION`] is one `ram_file_drv` reply, driven through the runtime's **own**
//! message builder ([`RuntimeContext::port_data_message_shaped`]) with the [`ReadMessage`] of the
//! output call that produced it, and compared with the system `erl`'s rendering of the very same
//! command sequence. The oracle half is a **loud skip** when the toolchain is absent, never a
//! silent pass.
//!
//! # What this suite does and does not prove
//!
//! It proves the *builder*: given a read's header/body split, the runtime renders the oracle's
//! exact term, on both streams. It cannot prove the *producer*: whether a real driver's
//! `driver_output2` reaches the builder with the split intact is pinned end to end by
//! [`super::driver_output_shape_tests`] (a registered driver staging split reads through a real
//! port-task round). The one producer shape no read can carry — a **built-in** driver port's flat
//! byte queue — stays [`StreamShape::DriverOutput`]'s, pinned by
//! [`super::ram_file_matrix_tests`]; the exact `beam_io` change that closes it is recorded in this
//! node's `CONTEXT.md` (*Handoffs*).
use super::ram_file_matrix_tests::oracle_output;
use super::tests::{skip_oracle_check, text_of};
use super::*;
use crate::config::RuntimeConfig;
/// The **output call** that produced a row's bytes: how the driver split them.
///
/// This is the C's `driver_output2` argument pair — the leading bytes it passed as
/// `hbuf`/`hlen`, and whether it passed a data part at all (`body: false` is a `NULL` `buf`,
/// which the C renders as a proper list rather than as a list with an empty binary tail).
#[derive(Clone, Copy)]
struct Split {
    /// How many of the row's leading bytes were the call's **header**.
    header: usize,
    /// Whether the call passed a data part (the C's `buf != NULL`).
    body: bool,
}
impl Split {
    /// The [`ReadMessage`] this call hands up for `bytes`.
    fn read(self, bytes: &[u8]) -> ReadMessage {
        let (header, rest) = bytes.split_at(self.header.min(bytes.len()));
        ReadMessage {
            header: header.to_vec(),
            body: self.body.then(|| rest.to_vec()),
        }
    }
}
/// One measured reply: the `port_command/2` payload that provokes it, the reply bytes the driver
/// answers with, the split of the output call that carried them, and the oracle's rendering of the
/// payload on each stream.
struct Reply {
    /// The row's label, printed by both sides.
    name: &'static str,
    /// The `rfile_command` wire payload (`opcode` ++ operands).
    command: &'static [u8],
    /// The reply bytes, exactly as the driver emits them.
    bytes: &'static [u8],
    /// The output call's header/data split.
    split: Split,
    /// The oracle's `~w` rendering of the payload on a `[binary]` port.
    binary_term: &'static str,
    /// The oracle's `~w` rendering on the default (`list`) stream.
    list_term: &'static str,
}
/// `Header`: `numeric_reply`/`reply`/`error_reply` all go through
/// `driver_output2(port, buf, len, NULL, 0)` (`ram_file_drv.c:181-228`), so a `binary` port gets a
/// **proper** list of the reply's bytes — the whole reply is the call's header and there is no
/// data part at all.
const OPEN: Reply = Reply {
    name: "open",
    command: &[1, 0, 0, 0, 3, 97, 98, 99],
    bytes: &[3, 0, 0, 0, 0],
    split: Split {
        header: 5,
        body: false,
    },
    binary_term: "[3,0,0,0,0]",
    list_term: "[3,0,0,0,0]",
};
/// `HeaderThenData`: `READ` answers through `driver_output_binary(port, header, 5, bin, 0, n)`
/// (`ram_file_drv.c:448`) — the five header bytes are list elements, the payload the binary tail.
const READ_DATA: Reply = Reply {
    name: "read_data",
    command: &[2, 0, 0, 0, 2],
    bytes: &[2, 0, 0, 0, 2, 97, 98],
    split: Split {
        header: 5,
        body: true,
    },
    binary_term: "[2,0,0,0,2|<<97,98>>]",
    list_term: "[2,0,0,0,2,97,98]",
};
/// The clamping read: one byte left in the file.
const READ_CLAMPED: Reply = Reply {
    name: "read_clamped",
    command: &[2, 0, 0, 0, 2],
    bytes: &[2, 0, 0, 0, 1, 99],
    split: Split {
        header: 5,
        body: true,
    },
    binary_term: "[2,0,0,0,1|<<99>>]",
    list_term: "[2,0,0,0,1,99]",
};
/// A read at end-of-file: a **zero-length** `DATA` reply still carries a data part, so the term is
/// `[H1..H5|<<>>]` — an improper list ending in the empty binary, never a bare header list.
const READ_EOF: Reply = Reply {
    name: "read_eof",
    command: &[2, 0, 0, 0, 2],
    bytes: &[2, 0, 0, 0, 0],
    split: Split {
        header: 5,
        body: true,
    },
    binary_term: "[2,0,0,0,0|<<>>]",
    list_term: "[2,0,0,0,0]",
};
/// The bare `OK` reply — a one-byte header and no data part.
const TRUNCATE: Reply = Reply {
    name: "truncate",
    command: &[14],
    bytes: &[0],
    split: Split {
        header: 1,
        body: false,
    },
    binary_term: "[0]",
    list_term: "[0]",
};
/// An `ERROR <errno>` refusal (`einval`), again header-only.
const PREAD_NEG: Reply = Reply {
    name: "pread_neg",
    command: &[17, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 3],
    bytes: &[1, 101, 105, 110, 118, 97, 108],
    split: Split {
        header: 7,
        body: false,
    },
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
/// `bytes` as the `ReadMessage` of a `driver_output2(header, bytes)`: `header` leading bytes as the
/// call's header, the rest as its data part.
fn header_then_body(header: &[u8], bytes: &[u8]) -> ReadMessage {
    ReadMessage {
        header: header.to_vec(),
        body: Some(bytes.to_vec()),
    }
}
/// `bytes` as the `ReadMessage` of a **header-only** call — `driver_output2(buf, len, NULL, 0)`.
fn header_only(bytes: &[u8]) -> ReadMessage {
    ReadMessage {
        header: bytes.to_vec(),
        body: None,
    }
}
/// Builds one port message through the runtime's **own** message builder and renders it the way
/// the oracle's `~w` prints it.
///
/// The runtime outlives every read: a term is only meaningful while the heap it was built on is
/// alive, so the builder is created once per test and borrowed for each message.
struct Builder {
    /// The runtime whose heap the messages are built on and whose printer renders them.
    rt: Runtime,
    /// The port the messages are addressed to (never compared: only `element(2, Msg)` is read).
    port: PortId,
}

impl Builder {
    /// A builder over a fresh default runtime.
    fn new() -> Builder {
        Builder {
            rt: Runtime::new(RuntimeConfig::default()).expect("the default runtime"),
            port: PortId::new(1),
        }
    }

    /// The `Data` payload of the `{Port, {data, Data}}` message
    /// [`RuntimeContext::port_data_message_shaped`] builds for `read` on `stream` with `shape`.
    fn payload(&mut self, read: &ReadMessage, stream: StreamType, shape: StreamShape) -> Eterm {
        let port = self.port;
        let mut ctx = RuntimeContext::new(&mut self.rt);
        let message = ctx
            .port_data_message_shaped(port, read, stream, shape)
            .unwrap_or_else(|error| panic!("a message for {read:?} ({shape:?}): {error}"));
        let inner = terms::tuple_element(message, 1).expect("the `{data, …}` tuple");
        terms::tuple_element(inner, 1).expect("the data payload")
    }

    /// `term` rendered by the registered `~w` printer — the very control the oracle's expression
    /// uses, so the two renderings are compared as text.
    fn render(&mut self, term: Eterm) -> String {
        let rendered = self
            .rt
            .format_term_w(term, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a built message: {error}"));
        rendered.trim_end_matches('\n').to_string()
    }

    /// [`Self::payload`] rendered with [`Self::render`].
    fn rendered_payload(
        &mut self,
        read: &ReadMessage,
        stream: StreamType,
        shape: StreamShape,
    ) -> String {
        let term = self.payload(read, stream, shape);
        self.render(term)
    }
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
    let mut builder = Builder::new();
    let mut our_binary = Vec::new();
    let mut our_list = Vec::new();
    for reply in SESSION {
        let read = reply.split.read(reply.bytes);
        let binary = builder.rendered_payload(&read, StreamType::Binary, StreamShape::Data);
        assert_eq!(
            binary, reply.binary_term,
            "{}: a `[binary]` port renders the C's header/data split",
            reply.name
        );
        our_binary.push(format!("{{{},{binary}}}", reply.name));
        // The `list` stream is insensitive to the split: header and data are both list elements.
        let list = builder.rendered_payload(&read, StreamType::List, StreamShape::Data);
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
            Some(oracle) => assert_eq!(
                ours.join("\n"),
                oracle,
                "{what}: every reply kind must render the oracle's term"
            ),
            None => skip_oracle_check(what),
        }
    }
}
/// The **structural kind** of a shaped payload: what the C's rendering *is*, so a test can assert
/// the fact the rendering implies rather than only its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// A proper list (`is_list/1` true, `length/1` the element count).
    Proper,
    /// An improper list ending in a binary (`is_list/1` true, `length/1` `badarg`) — the header
    /// bytes with a data part consed on.
    Improper,
    /// A bare binary (a header-less read's data part).
    Binary,
}
/// **The four oracle shapes** of a `[binary]` port — the C's table, one row per `driver_output*`
/// call shape, with the structural facts the row's rendering implies.
///
/// Measured on the oracle (`ReadMessage`'s table): a non-empty header with a data part is an
/// **improper** list whose tail is the data binary (`is_list/1` true, `length/1` `badarg`); a
/// non-empty header with **no** data part is a **proper** list (the C adds no binary tail at all);
/// an **empty** header with a data part is that binary bare; and a read of nothing at all is the
/// empty proper list `[]`, never `<<>>`.
#[test]
fn the_binary_stream_renders_the_cs_four_shapes() {
    let mut builder = Builder::new();
    for (label, read, binary_term, list_term, kind) in [
        (
            "header_then_body",
            header_then_body(b"HDR", b"abc"),
            "[72,68,82|<<97,98,99>>]",
            "[72,68,82,97,98,99]",
            Kind::Improper,
        ),
        (
            "header_only",
            header_only(b"HDR"),
            "[72,68,82]",
            "[72,68,82]",
            Kind::Proper,
        ),
        (
            "header_then_empty_body",
            header_then_body(b"HDR", b""),
            "[72,68,82|<<>>]",
            "[72,68,82]",
            Kind::Improper,
        ),
        (
            "body_only",
            ReadMessage::body_only(b"abc".to_vec()),
            "<<97,98,99>>",
            "[97,98,99]",
            Kind::Binary,
        ),
        (
            "nothing_at_all",
            ReadMessage::default(),
            "[]",
            "[]",
            Kind::Proper,
        ),
    ] {
        let term = builder.payload(&read, StreamType::Binary, StreamShape::Data);
        assert_eq!(
            builder.render(term),
            binary_term,
            "{label}: a `[binary]` port's `{{data, D}}` for {read:?}"
        );
        // The structural fact behind the rendering: only a header **with** a data part leaves an
        // improper list, and only a header-less read leaves a bare binary.
        match kind {
            Kind::Proper => assert!(
                terms::list_elements(term).is_ok(),
                "{label}: {binary_term} must be a proper list"
            ),
            Kind::Improper => assert!(
                terms::list_elements(term).is_err(),
                "{label}: {binary_term} must be an improper list (a binary tail)"
            ),
            Kind::Binary => assert!(
                terms::binary_bytes(term).is_ok(),
                "{label}: {binary_term} must be a bare binary"
            ),
        }
        // The `list` stream is every byte as list elements, whatever the split: never a binary
        // and never improper.
        let term = builder.payload(&read, StreamType::List, StreamShape::Data);
        assert_eq!(
            builder.render(term),
            list_term,
            "{label}: the default stream's payload for {read:?}"
        );
        assert!(
            terms::list_elements(term).is_ok(),
            "{label}: the `list` stream is always a proper list"
        );
    }
}
/// A list-stream payload is a **proper** list of every byte, for every shape — the split may not
/// leak a stray `ImproperList` into the default stream.
#[test]
fn the_list_stream_ignores_the_header_split() {
    let mut builder = Builder::new();
    for read in [
        ReadMessage::body_only(vec![2, 0, 0, 0, 2, 97, 98]),
        header_then_body(&[2, 0, 0, 0, 2], &[97, 98]),
        header_only(&[2, 0, 0, 0, 2, 97, 98]),
        // An empty header and an empty body: a present-but-empty data part is still a body.
        header_then_body(&[], &[]),
        ReadMessage::default(),
    ] {
        let term = builder.payload(&read, StreamType::List, StreamShape::Data);
        assert!(
            terms::list_elements(term).is_ok(),
            "{read:?}: the list-stream payload is a proper list"
        );
        assert_eq!(
            builder.render(term),
            if read.is_empty() {
                "[]".to_string()
            } else {
                "[2,0,0,0,2,97,98]".to_string()
            },
            "{read:?}: the list stream is every byte as list elements"
        );
    }
}
/// A header-less call ([`StreamShape::Data`]) stays the binary a byte-stream read has always
/// been — the split machinery must not change `driver_output`'s own rendering.
#[test]
fn a_headerless_call_is_still_a_binary() {
    let mut builder = Builder::new();
    let read = ReadMessage::body_only(b"abc".to_vec());
    let term = builder.payload(&read, StreamType::Binary, StreamShape::Data);
    assert_eq!(
        terms::binary_bytes(term).expect("a header-less binary-stream read is a binary"),
        b"abc"
    );
    // ...and so is a body that carries an (empty) header: the header conses nothing.
    let read = header_then_body(&[], b"abc");
    let term = builder.payload(&read, StreamType::Binary, StreamShape::Data);
    assert_eq!(
        terms::binary_bytes(term).expect("an empty header leaves the data binary"),
        b"abc"
    );
    // ...and a present-but-empty body is the bare `<<>>`, never `[]`.
    let read = header_then_body(&[], b"");
    let term = builder.payload(&read, StreamType::Binary, StreamShape::Data);
    assert_eq!(
        terms::binary_bytes(term).expect("an empty body is the empty binary"),
        b""
    );
}
/// A **built-in driver port**'s header-less read ([`StreamShape::DriverOutput`]) is list elements
/// on **either** stream: its flat byte queue cannot carry a split, so the C's header rendering is
/// the honest answer.
///
/// A read that *does* carry a header keeps its own split even for such a port — the shape may
/// never hide bytes a producer really delivered as a header.
#[test]
fn a_builtin_driver_port_renders_a_headerless_read_as_list_elements() {
    let mut builder = Builder::new();
    let read = ReadMessage::body_only(vec![2, 0, 0, 0, 2, 97, 98]);
    for stream in [StreamType::Binary, StreamType::List] {
        let term = builder.payload(&read, stream, StreamShape::DriverOutput);
        assert_eq!(
            builder.render(term),
            "[2,0,0,0,2,97,98]",
            "{stream:?}: a built-in driver port's payload is the C's header list"
        );
    }
    let read = header_then_body(b"HDR", b"abc");
    let term = builder.payload(&read, StreamType::Binary, StreamShape::DriverOutput);
    assert_eq!(
        builder.render(term),
        "[72,68,82|<<97,98,99>>]",
        "a read that carries a header keeps its own split"
    );
}
/// The `Data` payload of one `{eol|noeol, Data}` frame built by
/// [`RuntimeContext::port_line_message`], and the frame's marker atom name.
fn line_frame(
    builder: &mut Builder,
    header: &[u8],
    bytes: &[u8],
    eol: bool,
    stream: StreamType,
) -> (Option<String>, Eterm) {
    let port = builder.port;
    let (tag, data) = {
        let mut ctx = RuntimeContext::new(&mut builder.rt);
        let message = ctx
            .port_line_message(port, header, bytes, eol, stream)
            .unwrap_or_else(|error| panic!("a {stream:?} line frame: {error}"));
        let framed = terms::tuple_element(
            terms::tuple_element(message, 1).expect("the `{data, …}` pair"),
            1,
        )
        .expect("the frame");
        (
            terms::tuple_element(framed, 0).expect("the `{eol|noeol, Data}` tuple"),
            terms::tuple_element(framed, 1).expect("the frame's data"),
        )
    };
    let marker = builder.rt.atom_name_of(tag).map(str::to_string);
    (marker, data)
}
/// A **line-framed** frame renders its payload by the stream bit inside the `{eol|noeol, …}`
/// wrapper, exactly as an unframed read renders it: a binary for a `binary` port, a char list for
/// the default one — and it carries the **header of the read that produced it** as the same cons
/// prefix (`deliver_linebuf_message` → `deliver_read_message`, `io.c:3454`, `:3416-3428`).
#[test]
fn a_line_frame_wraps_the_stream_shaped_payload() {
    let mut builder = Builder::new();
    for (eol_flag, marker) in [(true, "eol"), (false, "noeol")] {
        let (tag, data) = line_frame(&mut builder, &[], b"ab", eol_flag, StreamType::Binary);
        assert_eq!(tag.as_deref(), Some(marker), "a frame's marker");
        assert_eq!(
            terms::binary_bytes(data).expect("a binary-stream frame's data is a binary"),
            b"ab"
        );
        let (tag, data) = line_frame(&mut builder, &[], b"ab", eol_flag, StreamType::List);
        assert_eq!(tag.as_deref(), Some(marker), "the list stream's marker");
        assert!(
            terms::list_elements(data).is_ok(),
            "a list-stream frame's data is a proper char list"
        );
        assert_eq!(
            text_of(data),
            "ab",
            "a list-stream frame carries the delivered bytes as char integers"
        );
    }
}
/// A frame's **header prefix**: `driver_output2("HDR", 3, "a\nb\n", 4)` on a `[binary]` port
/// yields two frames, each `{eol, [72,68,82 | <<"a">>]}` — the read's header consed on every
/// frame it produced, exactly as the C does.
#[test]
fn a_line_frame_carries_the_headers_of_the_read_that_produced_it() {
    let mut builder = Builder::new();
    let (tag, data) = line_frame(&mut builder, b"HDR", b"a", true, StreamType::Binary);
    assert_eq!(tag.as_deref(), Some("eol"), "the frames are newline-terminated");
    assert_eq!(
        builder.render(data),
        "[72,68,82|<<97>>]",
        "a frame's payload is the read's header consed on the frame's own bytes"
    );
    // The `list` stream spells the same prefix as list elements.
    let (tag, data) = line_frame(&mut builder, b"HDR", b"a", true, StreamType::List);
    assert_eq!(tag.as_deref(), Some("eol"));
    assert_eq!(builder.render(data), "[72,68,82,97]");
    // An empty frame (a blank line) keeps the prefix and an empty binary body.
    let (_, data) = line_frame(&mut builder, b"HDR", b"", true, StreamType::Binary);
    assert_eq!(builder.render(data), "[72,68,82|<<>>]");
    // ...and a header-less frame (an end-of-file flush, a plain read) is the bare payload.
    let (_, data) = line_frame(&mut builder, &[], b"a", false, StreamType::Binary);
    assert_eq!(
        terms::binary_bytes(data).expect("a header-less frame's data is a binary"),
        b"a"
    );
}
