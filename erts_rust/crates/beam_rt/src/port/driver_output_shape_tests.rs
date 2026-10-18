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
//! A **built-in driver port**'s `{Port, {data, Data}}` **term shape**, end to end and against the
//! live oracle.
//!
//! The C renders a driver output call's **header** bytes as list *elements* and only its **data**
//! bytes through the port's stream bit (`deliver_read_message`, `erts/emulator/beam/io.c:3363-3448`,
//! `deliver_vec_message`, `:3501-3617`), so a `[binary]` port does **not** always deliver a binary:
//! a driver reply whose call was header-only arrives as a proper list of byte integers. `ram_file_drv`
//! answers exactly that way — `numeric_reply`/`reply`/`error_reply` all go through
//! `driver_output2(port, buf, len, NULL, 0)` (`erts/emulator/drivers/common/ram_file_drv.c:181-228`).
//!
//! # What this suite pins, and where it differs from its two siblings
//!
//! [`super::stream_shape_tests`] pins the **builder** (given a [`ReadMessage`], the runtime renders
//! the oracle's term) and [`super::ram_file_matrix_tests`] pins the reply **bytes** and the two
//! `port_info/2` counters row by row. This suite pins the last seam those two cannot see: the
//! *producer* — a real `{spawn_driver, "ram_file_drv"}` port opened on the **runtime's own**
//! registry, driven through `port_command/2`, read back through the runtime's own poll route
//! ([`RuntimeContext::poll_port_messages`]) and rendered by the registered `~w` printer. The whole
//! rendered matrix — every row's term plus the closing byte counters — is then compared with the
//! live oracle's output for the very same command sequence, in **both** stream modes.
//!
//! [`a_split_read_reaches_the_owner_as_the_cs_terms`] then pins the split itself end to end: a
//! driver registered on the runtime's own port table hands up `driver_output2`-shaped reads
//! ([`beam_io::DriverContext::deliver_read`]) through a **real** port-task round, and every one of
//! the C's four shapes — plus a line frame's header prefix — reaches the owner unchanged.
//!
//! # The one residual, named loudly
//!
//! A **built-in driver port**'s reply still arrives through [`beam_io::DriverContext::input`], a
//! flat byte queue that carries **no** header/data split, so the port layer reports
//! [`StreamShape::DriverOutput`] — "the C's header rendering, split unknown". That is *exact* for a
//! header-only reply and loses the binary tail of a `DATA` reply (`driver_output_binary`,
//! `ram_file_drv.c:448`): the oracle delivers `[2,0,0,0,2|<<97,98>>]`, this runtime
//! `[2,0,0,0,2,97,98]` — the same bytes, as list elements. [`report_residual`] prints those two
//! rows on every run, and the matrix comparison below is aligned on exactly those two rows (never
//! silently).
use super::ram_file_matrix_tests::oracle_output;
use super::tests::{counter, erl_bytes, fixture, skip_oracle_check, Fixture};
use super::*;
use crate::config::RuntimeConfig;

/// How long the owner waits for a row's reply before the row fails loudly.
///
/// The reply is already harvested by the time `port_command/2` returns, so this bound exists only
/// to turn a lost reply into a failure instead of a hang — it is not a timing assumption.
const RECEIVE_BOUND: Duration = Duration::from_secs(5);

/// How long one poll round may block. The retained reply is reported by that round's very first
/// harvest, so no round ever waits this slice out for one; it is the round's own ceiling.
const POLL_ROUND: Duration = Duration::from_millis(50);

/// How long a **child** port may take to write its bytes: a real OS child is asynchronous, so this
/// one is a genuine (generous) bound rather than a formality.
const CHILD_BOUND: Duration = Duration::from_secs(10);

/// The C's error reply for a control request the driver's own `case` does not match: `[0] ++
/// "exbadport"` (`inet_drv.c`'s `ctl_error`, measured on the oracle through
/// `erlang:port_control(P, 200, <<>>)`).
const EXBADPORT: &[u8] = &[0, 101, 120, 98, 97, 100, 112, 111, 114, 116];

/// One matrix row: the reply's provoking command, its bytes, and the two renderings.
struct Row {
    /// The row label both sides print for it.
    name: &'static str,
    /// The `rfile_command` wire payload (opcode byte ++ operands).
    command: &'static [u8],
    /// The reply bytes exactly as the driver emits them.
    bytes: &'static [u8],
    /// The `[binary]` port's rendering in this runtime — the C's header rule, on **either** stream:
    /// every delivered byte a list element, because the driver answered header-only.
    rendered: &'static str,
    /// The oracle's own `[binary]` rendering where it differs — the improper `[header|<<data>>]`
    /// form of a `DATA` reply, whose binary tail the `beam_io` seam does not carry. `None` means
    /// "the oracle renders `rendered` too", i.e. that row is *exact*.
    oracle_binary: Option<&'static str>,
}

/// The seven-row matrix: a fresh `OPEN` of `"abc"`, a two-byte `READ`, the `SIZE`, the whole-file
/// `GET`, a `TRUNCATE` at the position the reads left, and the two refusals.
///
/// `OPEN`/`SIZE`/`TRUNCATE`/`PREAD`/`LSEEK` are header-only replies (`numeric_reply`, `reply`,
/// `error_reply`), which is what makes them *exact* below; `READ`/`GET` carry a data part.
#[rustfmt::skip] // one row per line: the matrix is read as a table, not as code.
const MATRIX: &[Row] = &[
    Row {
        name: "open_fresh",
        command: &[1, 0, 0, 0, 3, 97, 98, 99],
        bytes: &[3, 0, 0, 0, 0],
        rendered: "[3,0,0,0,0]",
        oracle_binary: None,
    },
    Row {
        name: "read_data",
        command: &[2, 0, 0, 0, 2],
        bytes: &[2, 0, 0, 0, 2, 97, 98],
        rendered: "[2,0,0,0,2,97,98]",
        oracle_binary: Some("[2,0,0,0,2|<<97,98>>]"),
    },
    Row {
        name: "size",
        command: &[37],
        bytes: &[3, 0, 0, 0, 3],
        rendered: "[3,0,0,0,3]",
        oracle_binary: None,
    },
    Row {
        name: "get",
        command: &[30],
        bytes: &[2, 0, 0, 0, 3, 97, 98, 99],
        rendered: "[2,0,0,0,3,97,98,99]",
        oracle_binary: Some("[2,0,0,0,3|<<97,98,99>>]"),
    },
    Row {
        name: "truncate",
        command: &[14],
        bytes: &[0],
        rendered: "[0]",
        oracle_binary: None,
    },
    Row {
        name: "pread_neg",
        command: &[17, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 3],
        bytes: &[1, 101, 105, 110, 118, 97, 108],
        rendered: "[1,101,105,110,118,97,108]",
        oracle_binary: None,
    },
    Row {
        name: "lseek_bad_whence",
        command: &[3, 0, 0, 0, 0, 0, 0, 0, 9],
        bytes: &[1, 101, 105, 110, 118, 97, 108],
        rendered: "[1,101,105,110,118,97,108]",
        oracle_binary: None,
    },
];

/// A live session: a `{spawn_driver, "ram_file_drv"}` port opened on the **runtime's own** port
/// registry, so the reply the driver queues during the write's task round is the very one
/// [`RuntimeContext::poll_port_messages`] reports.
struct Session {
    /// The runtime whose registry, poller and printer the session uses.
    runtime: Runtime,
    /// The `open_port/2` term vocabulary.
    fx: Fixture,
    /// The provider the port was opened through (read back for `port_info/2`).
    hook: RuntimePorts,
    /// The port under test.
    port: PortId,
    /// The stream option the port was opened with.
    stream: StreamType,
    /// One `{row, Payload}` line per row, plus the closing counter line.
    lines: Vec<String>,
    /// Σ delivered reply bytes, and Σ `port_command/2` payload bytes.
    input: i64,
    output: i64,
}

impl Session {
    /// Opens `{spawn_driver, "ram_file_drv"}` over `stream` on a fresh runtime.
    fn new(stream: StreamType) -> Session {
        let mut fx = fixture();
        let runtime = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let owner = runtime.self_pid();
        let mut hook = fx.hook_over(Rc::clone(&runtime.ports), owner);
        let options = match stream {
            StreamType::List => NIL,
            StreamType::Binary => {
                let binary = fx.atom("binary");
                fx.list(&[binary])
            }
        };
        let port = fx.open_ram_file_with(&mut hook, options);
        Session {
            runtime,
            fx,
            hook,
            port,
            stream,
            lines: Vec::new(),
            input: 0,
            output: 0,
        }
    }

    /// Sends every row's command in order, recording each reply.
    fn run(&mut self) {
        for row in MATRIX {
            self.step(row);
        }
        self.lines
            .push(format!("{{final,{},{}}}", self.input, self.output));
    }

    /// Sends one row's command and records the payload its owner received.
    ///
    /// Every row asserts its own three facts, so a divergence names the row instead of only the
    /// session's last line: the payload is a **proper list** of the reply's byte integers, its
    /// rendered form is the C's header rendering, and both byte counters (`port_info(P, input)` and
    /// the node-global `erlang:statistics(io)`) charge the reply exactly once.
    fn step(&mut self, row: &Row) {
        self.runtime
            .ports
            .borrow_mut()
            .write_command(self.port, row.command)
            .unwrap_or_else(|error| {
                panic!(
                    "row {:?}: port_command({:?}) failed: {error}",
                    row.name, row.command
                )
            });
        let payload = self.take_payload(row.name);
        let bytes: Vec<i64> = terms::list_elements(payload)
            .unwrap_or_else(|error| {
                panic!(
                    "row {:?}: a built-in driver port delivers the C's header as list elements, \
                     got {} ({error})",
                    row.name,
                    self.render(payload)
                )
            })
            .into_iter()
            .map(signed_value)
            .collect();
        let expected: Vec<i64> = row.bytes.iter().map(|byte| i64::from(*byte)).collect();
        assert_eq!(bytes, expected, "row {:?}: the reply's bytes", row.name);
        assert_eq!(
            self.render(payload),
            row.rendered,
            "row {:?}: a built-in driver port renders a driver output call's header as list \
             elements on either stream",
            row.name
        );
        self.input += i64::try_from(row.bytes.len()).expect("a reply length fits an i64");
        self.output += i64::try_from(row.command.len()).expect("a command length fits an i64");
        assert_eq!(
            counter(&mut self.fx, &mut self.hook, self.port, "input"),
            self.input,
            "row {:?}: `port_info(Port, input)` counts the {} reply bytes exactly once",
            row.name,
            row.bytes.len()
        );
        assert_eq!(
            counter(&mut self.fx, &mut self.hook, self.port, "output"),
            self.output,
            "row {:?}: `port_info(Port, output)` counts the {} command bytes",
            row.name,
            row.command.len()
        );
        // The node-global counters behind `erlang:statistics(io)` count the same traffic: this
        // runtime opened exactly one port, so its totals are this session's totals.
        assert_eq!(
            self.runtime.io_statistics(),
            (
                u64::try_from(self.input).expect("a byte count"),
                u64::try_from(self.output).expect("a byte count"),
            ),
            "row {:?}: `erlang:statistics(io)` counts the same bytes",
            row.name
        );
        self.lines
            .push(format!("{{{},{}}}", row.name, row.rendered));
    }

    /// Polls the runtime's own poll route until `name`'s reply arrives, returning its payload term.
    ///
    /// The message must be `{Port, {data, Payload}}` from **this** port, and there must be exactly
    /// one: one command is one reply, so a second message — or a foreign port's — fails the row.
    fn take_payload(&mut self, name: &'static str) -> Eterm {
        let data_atom = make_atom(self.runtime.code_server_atoms_mut().intern("data"));
        let deadline = Instant::now() + RECEIVE_BOUND;
        let mut messages = Vec::new();
        while messages.is_empty() {
            assert!(
                Instant::now() < deadline,
                "row {name:?}: the driver's reply never reached its owner within {RECEIVE_BOUND:?}"
            );
            let round = {
                let mut ctx = RuntimeContext::new(&mut self.runtime);
                ctx.poll_port_messages(POLL_ROUND)
                    .expect("the port poll must not fail")
            };
            messages.extend(round);
        }
        assert_eq!(
            messages.len(),
            1,
            "row {name:?}: one command yields exactly one message: {messages:?}"
        );
        let message = messages[0];
        assert_eq!(
            terms::tuple_element(message, 0).expect("the `Port` element"),
            self.port.to_eterm(),
            "row {name:?}: the message must come from this port"
        );
        let inner = terms::tuple_element(message, 1).expect("the `{{data, _}}` element");
        assert_eq!(
            terms::tuple_element(inner, 0).expect("the inner tag"),
            data_atom,
            "row {name:?}: the message is `{{Port, {{data, Payload}}}}`"
        );
        terms::tuple_element(inner, 1).expect("the payload")
    }

    /// `term` rendered by the registered `~w` printer — the very control the oracle's own
    /// expression prints with, so the two renderings are compared as text.
    fn render(&mut self, term: Eterm) -> String {
        self.runtime
            .format_term_w(term, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a payload: {error}"))
            .trim_end_matches('\n')
            .to_string()
    }

    /// The stream option as the oracle expression spells it.
    fn options(&self) -> &'static str {
        match self.stream {
            StreamType::List => "[]",
            StreamType::Binary => "[binary]",
        }
    }

    /// The whole rendered session, one line per row plus the closing counter line.
    fn rendered(&self) -> String {
        self.lines.join("\n")
    }
}

/// `bytes` as an Erlang binary literal: `<<1,0,0,0,3,97,98,99>>`.
fn erl_binary(bytes: &[u8]) -> String {
    let rendered: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("<<{}>>", rendered.join(","))
}

/// The oracle expression for the whole matrix: open `ram_file_drv` with `options`, send every row
/// in order and print one `{row, Reply}` line per row, then the closing `{final, Input, Output}`.
///
/// `receive … after 2000 -> timeout end` is the oracle's own bound: a reply that never arrives
/// prints `timeout` in that row's place, so a comparison fails on that row instead of hanging.
fn oracle_matrix_expr(options: &str) -> String {
    let mut expr = format!(
        "begin \
         P = open_port({{spawn_driver,\"ram_file_drv\"}},{options}), \
         C = fun(Cmd) -> true = port_command(P, Cmd), \
                         receive {{P,{{data,D}}}} -> D after 2000 -> timeout end end, \
         I = fun(Item) -> {{Item,V}} = erlang:port_info(P, Item), V end, \
         F = fun(N,Cmd) -> io:format(\"~w~n\",[{{N,C(Cmd)}}]) end, "
    );
    for row in MATRIX {
        expr.push_str(&format!("F({},{}),", row.name, erl_binary(row.command)));
    }
    expr.push_str("io:format(\"~w~n\",[{final,I(input),I(output)}]), erlang:port_close(P), ok end");
    expr
}

/// `oracle`'s `[binary]` matrix with the two declared `DATA` tails folded to the proper-list form
/// this runtime renders, so the whole session — counters included — can be compared line for line.
///
/// The substitution is *row-scoped and counted*: only a row that [MATRIX] declares as a residual
/// may change, and every declared residual must actually have been present (a `DATA` reply that
/// started arriving as the oracle's improper list would make this fail rather than silently align).
fn aligned_with_this_runtime(oracle: &str) -> String {
    let mut aligned = oracle.to_string();
    for row in MATRIX {
        let Some(improper) = row.oracle_binary else {
            continue;
        };
        let expected = format!("{{{},{}", row.name, improper);
        assert!(
            aligned.contains(&expected),
            "the oracle must still answer {expected:?} for this row:\n{aligned}"
        );
        aligned = aligned.replace(
            &format!("{{{},{improper}}}", row.name),
            &format!("{{{},{}}}", row.name, row.rendered),
        );
    }
    aligned
}

/// Prints the residual this suite cannot close, naming every row it covers.
///
/// An **in-tree driver** hands its bytes up through [`beam_io::DriverContext::input`], a flat byte
/// queue with no split attached, so a `DATA` reply's binary tail is unreachable from this crate;
/// the bytes and both byte counters agree exactly.
fn report_residual() {
    println!("================================================================");
    println!(
        "RESIDUAL (reported, not papered over): a built-in driver port's `DATA` reply term shape."
    );
    for row in MATRIX {
        if let Some(improper) = row.oracle_binary {
            println!("  row {}:", row.name);
            println!("    oracle:       {improper} — header list ++ a binary tail");
            println!(
                "    this runtime: {} — the same bytes as one proper list",
                row.rendered
            );
        }
    }
    println!(
        "  cause: an in-tree driver hands its bytes up through `beam_io`'s flat byte queue \
         (`DriverContext::input`), which carries no split; owner: `beam_io`, bytes and counters \
         agree."
    );
    println!("================================================================");
}

/// A `[binary]` **built-in driver** port delivers the C's header list, end to end.
///
/// The seven-row matrix through a real `{spawn_driver, "ram_file_drv"}` port: every reply is a
/// proper list of byte integers (never a binary), both byte counters are exact, and the whole
/// rendered session agrees with the live oracle's for the same command sequence — except the two
/// declared `DATA` rows this runtime's seam cannot split, which [`report_residual`] prints.
#[test]
fn a_binary_stream_builtin_driver_reply_is_the_cs_header_list() {
    let mut session = Session::new(StreamType::Binary);
    session.run();
    assert_eq!(session.rendered().lines().count(), MATRIX.len() + 1);
    let ours = session.rendered();

    match oracle_output(&oracle_matrix_expr(session.options())) {
        Some(oracle) => {
            // The oracle's own matrix, spelled out: the header-only rows are exactly what this
            // runtime delivers (which is the fix), and the two `DATA` rows are its improper form.
            assert_eq!(
                oracle,
                "{open_fresh,[3,0,0,0,0]}\n\
                 {read_data,[2,0,0,0,2|<<97,98>>]}\n\
                 {size,[3,0,0,0,3]}\n\
                 {get,[2,0,0,0,3|<<97,98,99>>]}\n\
                 {truncate,[0]}\n\
                 {pread_neg,[1,101,105,110,118,97,108]}\n\
                 {lseek_bad_whence,[1,101,105,110,118,97,108]}\n\
                 {final,40,34}",
                "the `[binary]` stream's measured matrix (erts-17.1)"
            );
            // ... and the whole session — every row's term and both counters — is the oracle's,
            // aligned on exactly the two rows [MATRIX] declares as residuals.
            assert_eq!(
                ours,
                aligned_with_this_runtime(&oracle),
                "the `[binary]` matrix must agree with the oracle apart from the declared `DATA` \
                 rows"
            );
        }
        None => skip_oracle_check("a `[binary]` built-in driver port's reply shapes"),
    }
    report_residual();
}

/// The default `[]` stream renders the same bytes as a proper list — unchanged by this shape.
///
/// The default stream is insensitive to the split (header and data are both list elements there),
/// so a list-stream session agrees with the oracle **whole**: every row and both counters.
#[test]
fn the_default_stream_renders_the_same_bytes_as_a_proper_list() {
    let mut session = Session::new(StreamType::List);
    session.run();
    let ours = session.rendered();
    match oracle_output(&oracle_matrix_expr(session.options())) {
        Some(oracle) => assert_eq!(
            ours, oracle,
            "the default `list` stream's whole session must match the oracle exactly"
        ),
        None => skip_oracle_check("the default stream's built-in driver reply shapes"),
    }
    assert_eq!(
        (session.input, session.output),
        (40, 34),
        "the same bytes are counted on either stream"
    );
}

/// The **builder** renders a [`StreamShape::DriverOutput`] payload as list elements on *both*
/// streams — the one shape whose rendering ignores the port's "binary" bit.
///
/// [`super::stream_shape_tests`] covers the header shapes; this is the split-unknown one the
/// producer really reports, pinned on the binary stream too (a driver reply is not a binary there).
#[test]
fn the_builder_renders_a_driver_output_payload_as_list_elements_on_either_stream() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let port = PortId::new(9001);
    for (stream, expected) in [
        (StreamType::Binary, "[2,0,0,0,2,97,98]"),
        (StreamType::List, "[2,0,0,0,2,97,98]"),
    ] {
        let message = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.port_data_message_shaped(
                port,
                &ReadMessage::body_only(vec![2, 0, 0, 0, 2, 97, 98]),
                stream,
                StreamShape::DriverOutput,
            )
            .unwrap_or_else(|error| panic!("a {stream:?} driver-output message: {error}"))
        };
        let inner = terms::tuple_element(message, 1).expect("the `{data, …}` tuple");
        let payload = terms::tuple_element(inner, 1).expect("the payload");
        let rendered = rt
            .format_term_w(payload, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a payload: {error}"))
            .trim_end_matches('\n')
            .to_string();
        assert_eq!(
            rendered, expected,
            "{stream:?}: a driver-output payload is the C's header list, never a binary"
        );
    }
}

/// The **child-port half does not move**: a `spawn` port's payload still follows the `binary`/`list`
/// option, because its bytes come from the port-task round with [`StreamShape::Data`].
///
/// Measured on the oracle: `{spawn,"cat"}` echoes `"ab"` as `<<97,98>>` on `[binary]` and
/// `[97,98]` on the default stream; `{spawn,"echo hi"}` writes `<<104,105,10>>` / `[104,105,10]`.
#[test]
fn a_child_ports_payload_still_follows_the_stream_option() {
    for (stream, echo_hi, cat) in [
        (StreamType::Binary, "<<104,105,10>>", "<<97,98>>"),
        (StreamType::List, "[104,105,10]", "[97,98]"),
    ] {
        // `echo hi`: the child writes its own bytes and exits.
        let hi = read_child_payload("echo hi", stream, None);
        assert_eq!(
            hi, echo_hi,
            "{stream:?}: a child's bytes honour the stream option (`echo hi`)"
        );
        // `cat`: the child echoes what `port_command/2` wrote to its stdin.
        let echoed = read_child_payload("cat", stream, Some(b"ab"));
        assert_eq!(
            echoed, cat,
            "{stream:?}: a child's echoed bytes honour the stream option (`cat`)"
        );
    }
}

/// Opens `{spawn, command}`, optionally writes `input` to its stdin, and returns the rendered first
/// `{Port, {data, Payload}}` the runtime's own poll route reports.
fn read_child_payload(command: &str, stream: StreamType, input: Option<&[u8]>) -> String {
    let mut fx = fixture();
    let command_line = fx.string(command);
    let name = fx.name("spawn", command_line);
    let options = match stream {
        StreamType::List => NIL,
        StreamType::Binary => {
            let binary = fx.atom("binary");
            fx.list(&[binary])
        }
    };
    let mut runtime = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let mut hook = fx.hook_over(Rc::clone(&runtime.ports), runtime.self_pid());
    let port = hook
        .open_port(name, options)
        .unwrap_or_else(|error| panic!("open_port({{spawn,{command:?}}}) failed: {error}"));
    if let Some(bytes) = input {
        runtime
            .ports
            .borrow_mut()
            .write_command(port, bytes)
            .unwrap_or_else(|error| panic!("port_command({command:?}, {bytes:?}): {error}"));
    }
    let deadline = Instant::now() + CHILD_BOUND;
    loop {
        assert!(
            Instant::now() < deadline,
            "`{command}` on {stream:?} produced no port data within {CHILD_BOUND:?}"
        );
        let round = {
            let mut ctx = RuntimeContext::new(&mut runtime);
            ctx.poll_port_messages(POLL_ROUND)
                .expect("the port poll must not fail")
        };
        for message in round {
            if terms::tuple_element(message, 0).expect("the `Port` element") != port.to_eterm() {
                continue;
            }
            let inner = terms::tuple_element(message, 1).expect("the `{{data, _}}` element");
            let payload = terms::tuple_element(inner, 1)
                .unwrap_or_else(|_| panic!("`{command}` reported a non-data message: {message:?}"));
            return runtime
                .format_term_w(payload, None)
                .unwrap_or_else(|error| panic!("the `~w` printer refused a payload: {error}"))
                .trim_end_matches('\n')
                .to_string();
        }
    }
}

/// A `{spawn_driver, "udp_inet"}` `port_control/3` reply is **untouched**: an int list on either
/// stream, because a driver's `control` reply is `buf_to_intlist` unless the driver asked for
/// `PORT_CONTROL_FLAG_BINARY` (`write_port_control_result`, `io.c:4133`) — never the port's stream
/// bit, and never a `{Port, {data, …}}` message.
///
/// Measured on the oracle, both stream options: `[0,101,120,98,97,100,112,111,114,116]`.
#[test]
fn a_udp_inet_control_reply_is_an_int_list_on_either_stream() {
    for (stream, options) in [(StreamType::Binary, "[binary]"), (StreamType::List, "[]")] {
        let mut fx = fixture();
        let name = fx.string("udp_inet");
        let name = fx.name("spawn_driver", name);
        let options_term = match stream {
            StreamType::List => NIL,
            StreamType::Binary => {
                let binary = fx.atom("binary");
                fx.list(&[binary])
            }
        };
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let mut hook = fx.hook_over(Rc::clone(&runtime.ports), runtime.self_pid());
        let port = hook
            .open_port(name, options_term)
            .expect("open_port({spawn_driver,\"udp_inet\"})");
        let data = terms::make_heap_binary(&mut RuntimeContext::new(&mut runtime), &[])
            .expect("the empty operand binary");
        let reply = crate::native::test_support::call(
            &mut runtime,
            "erlang",
            "port_control",
            &[port.to_eterm(), make_small(200), data],
        )
        .unwrap_or_else(|error| panic!("{stream:?}: port_control/3 failed: {error}"));
        let rendered = runtime
            .format_term_w(reply, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a control reply: {error}"))
            .trim_end_matches('\n')
            .to_string();
        assert_eq!(
            rendered,
            erl_bytes(
                &EXBADPORT
                    .iter()
                    .map(|byte| i64::from(*byte))
                    .collect::<Vec<i64>>()
            ),
            "{stream:?}: a driver's control reply is an int list, never a stream-shaped binary"
        );
        let expr = format!(
            "begin P = open_port({{spawn_driver,\"udp_inet\"}},{options}), \
             R = erlang:port_control(P, 200, <<>>), \
             io:format(\"~w\",[R]), erlang:port_close(P), ok end"
        );
        match oracle_output(&expr) {
            Some(oracle) => assert_eq!(
                rendered, oracle,
                "{stream:?}: the oracle answers the same int list for the same control request"
            ),
            None => skip_oracle_check("a `udp_inet` control reply's shape"),
        }
        hook.close_port(port).expect("closing the driver port");
    }
}

// ---------------------------------------------------------------------------------------
// The split **end to end**: a real port-task round carries `driver_output2`'s two halves
// ---------------------------------------------------------------------------------------
use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use beam_io::Interest;

/// One scripted `driver_output*` call a [`SplitReadDriver`] hands up: the C's `hbuf`/`hlen` pair
/// and its `buf`/`len` pair (`body: None` is a `NULL` buf — no binary tail at all).
struct ScriptedRead {
    /// The row label the assertions name.
    name: &'static str,
    /// The call's header bytes.
    header: &'static [u8],
    /// The call's data part; `None` is the C's `buf == NULL`.
    body: Option<&'static [u8]>,
    /// The owner's rendered `{data, D}` on a `[binary]` port; `None` is **no message at all**.
    binary: Option<&'static str>,
    /// ...and on the default `[]` stream.
    list: Option<&'static str>,
}

impl ScriptedRead {
    /// This row's expected rendering on `stream` (`None` = the read produces no message).
    fn expected(&self, stream: StreamType) -> Option<&'static str> {
        match stream {
            StreamType::Binary => self.binary,
            StreamType::List => self.list,
        }
    }

    /// The bytes this call carries: the C's `hlen + len`, the `bytes_in` accounting.
    fn byte_count(&self) -> usize {
        self.header.len() + self.body.map_or(0, <[u8]>::len)
    }
}

/// The C's four shapes, one row per `driver_output*` call, as a **single** driver callback's script
/// — so the round below proves both the split and the C's one-message-per-call rule.
#[rustfmt::skip] // one row per line: the table is read as a table, not as code.
const SPLIT_READS: &[ScriptedRead] = &[
    ScriptedRead {
        name: "header_then_body",
        header: b"HDR", body: Some(b"abc"),
        binary: Some("[72,68,82|<<97,98,99>>]"), list: Some("[72,68,82,97,98,99]"),
    },
    ScriptedRead {
        name: "header_only",
        header: b"HDR", body: None,
        binary: Some("[72,68,82]"), list: Some("[72,68,82]"),
    },
    ScriptedRead {
        name: "header_then_empty_body",
        header: b"HDR", body: Some(b""),
        binary: Some("[72,68,82|<<>>]"), list: Some("[72,68,82]"),
    },
    ScriptedRead {
        name: "body_only",
        header: b"", body: Some(b"abc"),
        binary: Some("<<97,98,99>>"), list: Some("[97,98,99]"),
    },
    // A read of nothing at all is no message at all (the fold's zero-byte rule) — never `{data,[]}`.
    ScriptedRead { name: "nothing_at_all", header: b"", body: None, binary: None, list: None },
];

/// A driver that hands up a script of `driver_output2`-shaped reads in its first `ready_input`.
///
/// Read readiness is **real**: [`Driver::start`] registers the read end of a socket pair with the
/// port's own poller and writes one byte from the peer, so the round that reports the reads is the
/// same port-task round a live driver's would be — nothing is injected behind [`beam_io`]'s back.
struct SplitReadDriver {
    /// The read end the poller watches (the driver owns it, as a real driver owns its descriptor).
    local: UnixStream,
    /// The peer's send end, held open so the read end stays readable for the port's lifetime.
    peer: UnixStream,
    /// The script of `driver_output*` calls this driver hands up, in arrival order.
    steps: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    /// Whether the script has been handed up (one callback, then silence).
    delivered: bool,
}

impl Driver for SplitReadDriver {
    fn name(&self) -> &str {
        "split-read"
    }

    fn start(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> Result<()> {
        ctx.register(self.local.as_raw_fd(), Interest::READABLE)?;
        self.peer.write_all(b"!")?;
        Ok(())
    }

    fn output(&mut self, _ctx: &mut beam_io::DriverContext<'_>) -> Result<usize> {
        Ok(0)
    }

    fn ready_input(&mut self, ctx: &mut beam_io::DriverContext<'_>) -> Result<()> {
        let mut sink = [0u8; 1];
        let mut reader = &self.local;
        let _drained = reader.read(&mut sink)?;
        if self.delivered {
            return Ok(());
        }
        self.delivered = true;
        for (header, body) in &self.steps {
            ctx.deliver_read(header, body.as_deref())?;
        }
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut beam_io::DriverContext<'_>) -> Result<()> {
        Ok(())
    }
}

/// The `port_info/1,2` metadata of a [`SplitReadDriver`]'s port: a `{spawn_driver, _}`-kind port
/// whose bytes are its own driver's, so the read's split — never a producer rule — shapes the term.
fn split_meta(stream: StreamType, line: Option<usize>, owner: ProcessId) -> PortMeta {
    PortMeta {
        command: "split-read".to_string(),
        kind: PortName::SpawnDriver,
        os_pid: None,
        stream,
        line,
        packet: None,
        notify_exit: false,
        monitors: Vec::new(),
        owner,
        driver_has_control: false,
        driver_terms: None,
        driver_atoms: None,
        driver_control_flags: None,
        driver_busy: None,
        driver_monitors: None,
        input: Cell::new(0),
        output: Cell::new(0),
        exit_status_taken: Cell::new(false),
        held_lines: VecDeque::new(),
        held_since: Cell::new(None),
    }
}

/// A live [`SplitReadDriver`] port on `runtime`'s own table, with `steps` as its script.
fn open_split_read_port(
    runtime: &mut Runtime,
    stream: StreamType,
    line: Option<usize>,
    steps: Vec<(Vec<u8>, Option<Vec<u8>>)>,
) -> PortId {
    let owner = runtime.self_pid();
    let (local, peer) = UnixStream::pair().expect("a socket pair");
    let driver = SplitReadDriver {
        local,
        peer,
        steps,
        delivered: false,
    };
    runtime
        .ports
        .borrow_mut()
        .register(Box::new(driver), split_meta(stream, line, owner))
        .expect("the split-read port registers")
}

/// Polls the runtime's own `-eval` route until it has collected `wanted` messages, returning them.
fn poll_messages(runtime: &mut Runtime, wanted: usize) -> Vec<Eterm> {
    let deadline = Instant::now() + CHILD_BOUND;
    let mut messages = Vec::new();
    while messages.len() < wanted {
        assert!(
            Instant::now() < deadline,
            "only {} of {wanted} port messages reached the owner within {CHILD_BOUND:?}: {messages:?}",
            messages.len()
        );
        let round = {
            let mut ctx = RuntimeContext::new(runtime);
            ctx.poll_port_messages(POLL_ROUND)
                .expect("the port poll must not fail")
        };
        messages.extend(round);
    }
    messages
}

/// `message`'s `element(2, …)` — the `{data, …}` pair — rendered by the registered `~w` printer.
fn rendered_data(runtime: &mut Runtime, message: Eterm) -> String {
    let inner = terms::tuple_element(message, 1).expect("the `{data, …}` element");
    runtime
        .format_term_w(inner, None)
        .unwrap_or_else(|error| panic!("the `~w` printer refused a payload: {error}"))
        .trim_end_matches('\n')
        .to_string()
}

/// **The split reaches the owner**: a driver's `driver_output2` header/body pair, handed up through
/// [`beam_io::DriverContext::deliver_read`] in a real port-task round, renders the C's four shapes —
/// and a read of nothing at all produces no message.
///
/// The rows are the oracle's table ([`ReadMessage`]'s rustdoc; measured on a real C driver against
/// erts-17.1 and, for the header-only and `DATA` rows, by
/// [`a_binary_stream_builtin_driver_reply_is_the_cs_header_list`]): `[72,68,82|<<97,98,99>>]` for a
/// header **with** a data part, the proper `[72,68,82]` for one **without**, `[72,68,82|<<>>]` for a
/// present-but-empty data part, the bare `<<97,98,99>>` for a header-less `driver_output`, and the
/// flat list form on the default stream. Accounting counts `hlen + len` on every carried read.
#[test]
fn a_split_read_reaches_the_owner_as_the_cs_terms() {
    for stream in [StreamType::Binary, StreamType::List] {
        let mut fx = fixture();
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let mut hook = fx.hook_over(Rc::clone(&runtime.ports), runtime.self_pid());
        let steps: Vec<(Vec<u8>, Option<Vec<u8>>)> = SPLIT_READS
            .iter()
            .map(|read| (read.header.to_vec(), read.body.map(<[u8]>::to_vec)))
            .collect();
        let port = open_split_read_port(&mut runtime, stream, None, steps);
        let wanted = SPLIT_READS
            .iter()
            .filter(|read| read.expected(stream).is_some())
            .count();
        let messages = poll_messages(&mut runtime, wanted);
        let rendered: Vec<String> = messages
            .iter()
            .map(|message| {
                assert_eq!(
                    terms::tuple_element(*message, 0).expect("the `Port` element"),
                    port.to_eterm(),
                    "{stream:?}: every message comes from this port"
                );
                rendered_data(&mut runtime, *message)
            })
            .collect();
        let expected: Vec<(&str, String)> = SPLIT_READS
            .iter()
            .filter_map(|read| {
                read.expected(stream)
                    .map(|term| (read.name, format!("{{data,{term}}}")))
            })
            .collect();
        assert_eq!(
            rendered.len(),
            expected.len(),
            "{stream:?}: one message per carried read"
        );
        for ((label, wanted), got) in expected.iter().zip(&rendered) {
            assert_eq!(
                got, wanted,
                "{stream:?} row {label:?}: the C's shape for the call that produced it"
            );
        }
        // A read of nothing at all is no message: the next round is silent.
        let extra = {
            let mut ctx = RuntimeContext::new(&mut runtime);
            ctx.poll_port_messages(POLL_ROUND)
                .expect("the quiet round must not fail")
        };
        assert!(
            extra.is_empty(),
            "{stream:?}: a zero-length read produces no message, got {extra:?}"
        );
        let counted: i64 = SPLIT_READS
            .iter()
            .filter(|read| read.expected(stream).is_some())
            .map(|read| i64::try_from(read.byte_count()).expect("a byte count"))
            .sum();
        assert_eq!(
            counter(&mut fx, &mut hook, port, "input"),
            counted,
            "{stream:?}: `port_info(Port, input)` counts hlen + len of every carried read"
        );
        hook.close_port(port).expect("closing the split-read port");
    }
}

/// A **line frame's header prefix** survives the round: `driver_output2("HDR", 3, "a\nb\n", 4)` on a
/// `[binary]` line port yields **two** frames, each `{eol, [72,68,82|<<"a">>]}`-shaped — the
/// producing read's header consed on every frame it completed (`deliver_linebuf_message` →
/// `deliver_read_message`, `io.c:3454`, `:3416-3428`), while a header-less read stamps none.
///
/// The frame's own bytes (not the header, which [`beam_io`] stamps on every frame of one call) are
/// what `port_info(Port, input)` charges.
#[test]
fn a_line_frames_header_prefix_survives_the_round() {
    let mut fx = fixture();
    let mut runtime = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let mut hook = fx.hook_over(Rc::clone(&runtime.ports), runtime.self_pid());
    let port = open_split_read_port(
        &mut runtime,
        StreamType::Binary,
        Some(512),
        vec![(b"HDR".to_vec(), Some(b"a\nb\n".to_vec()))],
    );
    let messages = poll_messages(&mut runtime, 2);
    let rendered: Vec<String> = messages
        .iter()
        .map(|message| rendered_data(&mut runtime, *message))
        .collect();
    assert_eq!(
        rendered,
        vec![
            "{data,{eol,[72,68,82|<<97>>]}}".to_string(),
            "{data,{eol,[72,68,82|<<98>>]}}".to_string(),
        ],
        "each frame carries the header of the read that produced it"
    );
    assert_eq!(
        counter(&mut fx, &mut hook, port, "input"),
        2,
        "the frames' own bytes are charged (the header is not re-charged per frame)"
    );
    hook.close_port(port).expect("closing the line-mode port");
}
