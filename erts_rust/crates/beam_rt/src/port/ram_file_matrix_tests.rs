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
//! The `ram_file_drv` **command/reply matrix** against the live oracle.
//!
//! One row per wire opcode: the `port_command/2` payload, the reply it produces and the two
//! `port_info/2` byte counters — driven through the runtime's **own** port registry and compared,
//! whole-session, with the system `erl`'s output for the very same command sequence.
//!
//! # Why a matrix, and why a whole *session*
//!
//! `ram_file_drv` is the built-in driver whose reply path had no coverage before the fix this
//! suite guards: its replies are harvested by the port-task round the write itself runs
//! ([`PortRegistry::write_command`]), so the defect was a **lost reply** rather than a wrong one —
//! the owner's `receive` timed out and `port_info(Port, input)` stayed `0`. A per-opcode unit test
//! would not have caught it (every opcode's encoding was right); what catches it is driving the
//! *same* sequence under both runtimes and comparing what the owner actually received.
//!
//! Each test therefore runs one grouped session expression per side and compares the whole
//! rendered session (one `~w` line per row, the way `frames_w` compares a frame sequence), plus
//! per-row counter assertions: the counters are checked against the running totals of the reply
//! and command byte lengths inside [`Matrix::step`], so a row whose reply is harvested twice or
//! not at all fails on *its own* line rather than at the end of the session.
//!
//! # The wire protocol (measured on the oracle, and the C's own `rfile_command`)
//!
//! One command per `port_command(P, Payload)`; the payload is the opcode byte followed by its
//! operands. `OPEN=1` (`flags:i32` ++ the file's **initial content** — not a name: `OPEN` neither
//! truncates nor appends, it *replaces* content and position), `READ=2` (`len`), `LSEEK=3`
//! (`off`,`whence` 0/1/2), `WRITE=4` (the bytes), `FSYNC=9`, `TRUNCATE=14`, `PREAD=17` (`off`,`len`),
//! `PWRITE=18` (`off`, bytes), `FDATASYNC=19`, `GET=30` (the whole logical file), `SIZE=37`,
//! `ADVISE=38`, `ALLOCATE=39`. Replies: `OK` `[0]`, `NUMBER` `[3] ++ be32(i32)`, `DATA`
//! `[2] ++ be32(n) ++ bytes`, `ERROR` `[1] ++ lowercased errno`.
//!
//! `RAM_FILE_RESP_INFO=4` is declared by the reference but **never emitted** (the C's
//! `rfile_command` has no arm that produces it), so this driver has no `read_file_info`-shaped
//! reply at all: a whole-file `GET` dump is the closest thing it has, which is why the matrix
//! covers `GET` as that row.
//!
//! `erlang:port_control/3` is `error:badarg` for **every** opcode on this port (the driver entry's
//! `control` slot is `NULL`), so `beam_io`'s named-verb `Driver::control` transport is a Rust-only
//! extension with no oracle counterpart: nothing here drives it. Everything goes through
//! `port_command`, which is the only transport the oracle has.
//!
//! # Honest scope
//!
//! Nothing here fabricates a reply or papers a difference over: a row that disagrees with the
//! oracle fails on the session string (with the oracle's live output in the failure message), and
//! the one measured divergence this suite found — the `[binary]` stream's *term shape* — has its
//! own loud test rather than a relaxed assertion
//! ([`the_binary_stream_option_diverges_in_term_shape_only`]).
use super::tests::{
    counter, data_payload_bytes, erl_bytes, fixture, skip_oracle_check, text_of, Fixture,
};
use super::*;
use crate::config::RuntimeConfig;

/// How long the owner waits for a row's reply before the row fails loudly.
///
/// The reply is already harvested by the time `port_command` returns, so this bound exists only to
/// turn a lost reply into a failure instead of a hang — it is not a timing assumption.
const RECEIVE_BOUND: Duration = Duration::from_secs(5);

/// How long one poll round may block. The retained reply is reported by that round's very first
/// harvest, so no round ever waits this slice out for one; it is the round's own ceiling.
const POLL_ROUND: Duration = Duration::from_millis(50);

/// One matrix row: the label both sides print for it, and the exact `port_command/2` payload.
struct Row {
    /// The row label — the same text the oracle's expression prints (`F(open_fresh, …)`), so the
    /// two renderings are line-for-line comparable.
    name: &'static str,
    /// The opcode byte followed by its operands (the `rfile_command` wire form).
    command: &'static [u8],
}

/// The `open_port/2` stream a session is driven over.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// The default: the reply arrives as a char list (the oracle's `{data,[3,0,0,0,0]}`).
    List,
    /// The `binary` option, whose reply *shape* diverges — see
    /// [`the_binary_stream_option_diverges_in_term_shape_only`].
    Binary,
}

/// The command payloads, written in decimal byte order exactly as the oracle was measured with —
/// `<<1,0,0,0,3,97,98,99>>` is `OPEN` with the access-mode flags `3` and the initial content
/// `"abc"`.
///
/// `LSEEK`'s operands are `offset:i32` then `whence:i32`, both big-endian, so its payload is nine
/// bytes: `[3, 0,0,0,<offset>, 0,0,0,<whence>]`.
const OPEN_ABC: &[u8] = &[1, 0, 0, 0, 3, 97, 98, 99];
/// `READ` 2 bytes at the current position.
const READ_2: &[u8] = &[2, 0, 0, 0, 2];
/// `READ` 5 bytes at the current position.
const READ_5: &[u8] = &[2, 0, 0, 0, 5];
/// `READ` 1 byte at the current position.
const READ_1: &[u8] = &[2, 0, 0, 0, 1];
/// `WRITE` the bytes `"XY"` at the current position.
const WRITE_XY: &[u8] = &[4, 88, 89];
/// `WRITE` the single byte `"X"`.
const WRITE_X: &[u8] = &[4, 88];
/// `TRUNCATE`: the logical file ends at the current position.
const TRUNCATE: &[u8] = &[14];
/// `SIZE`: the logical file length.
const SIZE: &[u8] = &[37];
/// `GET`: a copy of the whole logical file.
const GET: &[u8] = &[30];
/// `FSYNC`.
const FSYNC: &[u8] = &[9];
/// `FDATASYNC`.
const FDATASYNC: &[u8] = &[19];
/// `ADVISE` — its operands are dropped, as in the C.
const ADVISE: &[u8] = &[38];
/// `ALLOCATE` — its operands are dropped, as in the C.
const ALLOCATE: &[u8] = &[39];
/// `PREAD` at `-1` — the negative-offset refusal (`einval`), which the C checks *before* the
/// access mode.
const PREAD_NEG: &[u8] = &[17, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 3];
/// `PWRITE` at `-1` — the same refusal on the write side.
const PWRITE_NEG: &[u8] = &[18, 0xff, 0xff, 0xff, 0xff, 90];
/// `LSEEK` to `-1` — `ram_file_seek` rejects the resulting negative position.
const LSEEK_NEG: &[u8] = &[3, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0];
/// `LSEEK` with an unknown `whence` (`9`) — the C's `default:` arm.
const LSEEK_BAD_WHENCE: &[u8] = &[3, 0, 0, 0, 0, 0, 0, 0, 9];
/// `PREAD` 3 bytes at offset `0`; it must **not** move the current position.
const PREAD_0_3: &[u8] = &[17, 0, 0, 0, 0, 0, 0, 0, 3];
/// `PWRITE` the byte `"Z"` at offset `10`, which must leave a zero-filled gap.
const PWRITE_10_Z: &[u8] = &[18, 0, 0, 0, 10, 90];

/// The main session: one port driven through the whole file-operation set, in an order where every
/// reply is *load-bearing* — each read proves the position the previous row left, and the truncate
/// pair proves the shrink **and** the extend.
///
/// `read_data`/`read_clamped`/`read_eof` pin the clamping rule (a read never runs past the end, and
/// EOF is a zero-length `DATA`, never an error); `lseek_zero` + `read_whole` prove the seek took
/// effect; `get_after_extend` proves a truncate past the end zero-fills rather than resurrecting
/// the dropped tail.
#[rustfmt::skip] // one row per line: the matrix is read as a table, not as code.
const MAIN_SESSION: &[Row] = &[
    Row { name: "open_fresh", command: OPEN_ABC },
    Row { name: "read_data", command: READ_2 },
    Row { name: "read_clamped", command: READ_2 },
    Row { name: "read_eof", command: READ_2 },
    Row { name: "write_xy", command: WRITE_XY },
    Row { name: "size", command: SIZE },
    Row { name: "get", command: GET },
    Row { name: "lseek_zero", command: &[3, 0, 0, 0, 0, 0, 0, 0, 0] },
    Row { name: "read_whole", command: READ_5 },
    Row { name: "lseek_2", command: &[3, 0, 0, 0, 2, 0, 0, 0, 0] },
    Row { name: "truncate_shrink", command: TRUNCATE },
    Row { name: "get_after_shrink", command: GET },
    Row { name: "size_after_shrink", command: SIZE },
    Row { name: "lseek_6", command: &[3, 0, 0, 0, 6, 0, 0, 0, 0] },
    Row { name: "truncate_extend", command: TRUNCATE },
    Row { name: "size_after_extend", command: SIZE },
    Row { name: "get_after_extend", command: GET },
];

/// The second session: a re-`OPEN` of an already-opened port (this driver's "open an existing
/// file" — it replaces the content **and** resets the position, which `read_one` +
/// `size_after_reopen` prove), the located reads/writes that must leave the position alone, the
/// four `flags`-gated no-ops, and the two `einval` refusals.
#[rustfmt::skip] // one row per line: the matrix is read as a table, not as code.
const EXTENDED_SESSION: &[Row] = &[
    Row { name: "open_fresh", command: OPEN_ABC },
    Row { name: "reopen", command: OPEN_ABC },
    Row { name: "size_after_reopen", command: SIZE },
    Row { name: "read_one", command: READ_1 },
    Row { name: "pread_0_3", command: PREAD_0_3 },
    Row { name: "read_after_pread", command: READ_2 },
    Row { name: "pwrite_at_10", command: PWRITE_10_Z },
    Row { name: "get_after_pwrite", command: GET },
    Row { name: "fsync", command: FSYNC },
    Row { name: "fdatasync", command: FDATASYNC },
    Row { name: "advise", command: ADVISE },
    Row { name: "allocate", command: ALLOCATE },
    Row { name: "pread_neg", command: PREAD_NEG },
    Row { name: "pwrite_neg", command: PWRITE_NEG },
    Row { name: "lseek_neg", command: LSEEK_NEG },
    Row { name: "lseek_bad_whence", command: LSEEK_BAD_WHENCE },
];

/// The refusal session: every operation against a port that was **never** opened.
///
/// `size`/`get` are answered (`flags`-independent — they read the empty file), everything that
/// needs an access mode is `ebadf`, and `truncate` is `eacces` because its own gate is the
/// `RAM_FILE_MODE_WRITE` bit rather than `flags == 0`.
#[rustfmt::skip] // one row per line: the matrix is read as a table, not as code.
const REFUSAL_SESSION: &[Row] = &[
    Row { name: "size_unopened", command: SIZE },
    Row { name: "get_unopened", command: GET },
    Row { name: "read_unopened", command: READ_2 },
    Row { name: "write_unopened", command: WRITE_X },
    Row { name: "lseek_unopened", command: &[3, 0, 0, 0, 0, 0, 0, 0, 0] },
    Row { name: "fsync_unopened", command: FSYNC },
    Row { name: "fdatasync_unopened", command: FDATASYNC },
    Row { name: "advise_unopened", command: ADVISE },
    Row { name: "allocate_unopened", command: ALLOCATE },
    Row { name: "truncate_unopened", command: TRUNCATE },
];

/// The refusal session's tail: `port_info(Port, name)` — the driver name, rendered as the char
/// list the oracle's `~w` prints for it.
const NAME_TAIL: &str = "io:format(\"~w~n\",[{name,I(name)}]),";

/// The `[binary]` stream session — the same opcodes through a binary-stream port.
#[rustfmt::skip] // one row per line: the matrix is read as a table, not as code.
const BINARY_SESSION: &[Row] = &[
    Row { name: "open_fresh", command: OPEN_ABC },
    Row { name: "read_data", command: READ_2 },
    Row { name: "size", command: SIZE },
    Row { name: "truncate_seek", command: &[3, 0, 0, 0, 1, 0, 0, 0, 0] },
    Row { name: "truncate", command: TRUNCATE },
    Row { name: "get", command: GET },
];

/// A live runtime with a `{spawn_driver, "ram_file_drv"}` port opened on the runtime's **own**
/// registry, so the reply the driver writes during the write's task round is the very one
/// [`RuntimeContext::poll_port_messages`] (the `-eval` route's port half) reports — the path the
/// defect starved.
///
/// The session rendering it accumulates is what gets compared: `lines` holds one
/// `{row, Reply, Input, Output}` per row plus the closing `{final, Input, Output}`.
struct Matrix {
    runtime: Runtime,
    fx: Fixture,
    hook: RuntimePorts,
    port: PortId,
    stream: Stream,
    lines: Vec<String>,
    replies: HashMap<&'static str, Vec<i64>>,
    /// Whether a row's payload arrived as a **binary** rather than a char list.
    binary_payloads: HashMap<&'static str, bool>,
    /// The running totals: Σ delivered reply bytes and Σ `port_command` payload bytes.
    input: i64,
    output: i64,
}

impl Matrix {
    /// Opens `{spawn_driver, "ram_file_drv"}` over `stream` on a fresh runtime.
    fn new(stream: Stream) -> Matrix {
        let mut fx = fixture();
        let runtime = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
        let owner = runtime.self_pid();
        // The hook is built over the **runtime's** registry (a cheap `Rc` clone), so the port the
        // runtime's own poller sees is the one opened here.
        let mut hook = fx.hook_over(Rc::clone(&runtime.ports), owner);
        let options = match stream {
            Stream::List => NIL,
            Stream::Binary => {
                let binary = fx.atom("binary");
                fx.list(&[binary])
            }
        };
        let port = fx.open_ram_file_with(&mut hook, options);
        Matrix {
            runtime,
            fx,
            hook,
            port,
            stream,
            lines: Vec::new(),
            replies: HashMap::new(),
            binary_payloads: HashMap::new(),
            input: 0,
            output: 0,
        }
    }

    /// Runs every `row` in order.
    fn run(&mut self, rows: &[Row]) {
        for row in rows {
            self.step(row);
        }
    }

    /// Sends one row's command and records what its owner received.
    ///
    /// The reply is taken through the runtime's poll route and its byte count is charged to
    /// `port_info(Port, input)` — asserted per row against the running total, so a lost or doubled
    /// reply fails here rather than being masked by the session's own sum.
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
        let reply = self.take_reply(row.name);
        self.input += i64::try_from(reply.len()).expect("a reply length fits an i64");
        self.output += i64::try_from(row.command.len()).expect("a command length fits an i64");
        assert_eq!(
            counter(&mut self.fx, &mut self.hook, self.port, "input"),
            self.input,
            "row {:?}: `port_info(Port, input)` must count the {} reply bytes exactly once",
            row.name,
            reply.len()
        );
        assert_eq!(
            counter(&mut self.fx, &mut self.hook, self.port, "output"),
            self.output,
            "row {:?}: `port_info(Port, output)` must count the {} command bytes",
            row.name,
            row.command.len()
        );
        self.lines
            .push(row_w(row.name, &reply, self.input, self.output));
        self.replies.insert(row.name, reply);
    }

    /// Polls the runtime's own poller until `name`'s reply arrives, returning its byte values.
    ///
    /// The message must be `{Port, {data, Payload}}` from **this** port, and there must be exactly
    /// one: one command is one reply, so a second message — or a foreign port's — fails the row.
    fn take_reply(&mut self, name: &'static str) -> Vec<i64> {
        let data_atom = make_atom(self.runtime.code_server_atoms_mut().intern("data"));
        let deadline = Instant::now() + RECEIVE_BOUND;
        let mut messages = Vec::new();
        while messages.is_empty() {
            assert!(
                Instant::now() < deadline,
                "row {name:?}: the driver's reply never reached its owner within \
                 {RECEIVE_BOUND:?} — the write's task round harvested it and nothing reported it"
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
        let (bytes, binary) = reply_payload(message);
        self.binary_payloads.insert(name, binary);
        bytes
    }

    /// The `port_info/1` value of `item`, read through the session's **own** hook.
    ///
    /// [`Fixture::item`] is deliberately not used here: it answers from the fixture's *separate*
    /// registry, while this session's port lives on the runtime's (`hook_over`). Reading through the
    /// wrong hook reports an empty pair list — the closed-port answer — for a live port.
    fn port_info_item(&mut self, item: &str) -> Eterm {
        let wanted = self.fx.atom(item);
        let pairs = self
            .hook
            .port_info(self.port)
            .unwrap_or_else(|error| panic!("port_info({:?}, {item}) failed: {error}", self.port));
        pairs
            .iter()
            .find(|(key, _)| *key == wanted)
            .map(|(_, value)| *value)
            .unwrap_or_else(|| panic!("no {item:?} item in {pairs:?}"))
    }

    /// Appends a line the session's oracle expression printed verbatim (see [`NAME_TAIL`]).
    fn push_line(&mut self, line: String) {
        self.lines.push(line);
    }

    /// Appends the session's closing `{final, Input, Output}` line.
    fn finish(&mut self) {
        self.lines
            .push(format!("{{final,{},{}}}", self.input, self.output));
    }

    /// A row's captured reply.
    fn reply(&self, name: &str) -> Vec<i64> {
        self.replies
            .get(name)
            .unwrap_or_else(|| panic!("no {name:?} row in this session"))
            .clone()
    }

    /// Whether a row's payload arrived as a **binary** (the `[binary]` stream) rather than a char
    /// list (the default `list` stream).
    fn reply_is_binary(&self, name: &str) -> bool {
        *self
            .binary_payloads
            .get(name)
            .unwrap_or_else(|| panic!("no {name:?} row in this session"))
    }

    /// The whole rendered session, one line per row.
    fn session(&self) -> String {
        self.lines.join("\n")
    }

    /// Compares the whole rendered session with the oracle's own output for the same sequence.
    ///
    /// A missing toolchain is a **loud skip** (never a silent pass); otherwise the assertion
    /// carries the oracle's live output, so a divergence shows both sides at once.
    fn assert_session_matches_the_oracle(&self, rows: &[Row], tail: &str, what: &str) {
        let options = match self.stream {
            Stream::List => "[]",
            Stream::Binary => "[binary]",
        };
        match oracle_output(&oracle_session_expr(options, rows, tail)) {
            Some(oracle) => assert_eq!(
                self.session(),
                oracle,
                "{what}: the whole session — every reply and every `port_info` counter — must \
                 match the oracle"
            ),
            None => skip_oracle_check(what),
        }
    }
}

/// A `{Port, {data, Payload}}` message's payload as byte values, and whether it arrived as a
/// **binary**.
///
/// The default (`list`) stream shapes the payload as a char list (the oracle's own
/// `{data,[3,0,0,0,0]}`) and a `[binary]` port as a binary; both are read here, so one session
/// runner covers both streams.
fn reply_payload(message: Eterm) -> (Vec<i64>, bool) {
    let inner = terms::tuple_element(message, 1).expect("the `{data, Payload}` pair");
    let payload = terms::tuple_element(inner, 1).expect("the payload");
    match terms::binary_bytes(payload) {
        Ok(bytes) => (bytes.iter().map(|byte| i64::from(*byte)).collect(), true),
        // `data_payload_bytes` re-reads the same payload as the char list it must then be, and
        // fails loudly if it is neither shape.
        Err(_) => (data_payload_bytes(message), false),
    }
}

/// Runs `expr` under the host `erl` and returns its trimmed stdout.
///
/// [`erl_eval`] answers `None` for **any** non-success, which cannot tell "there is no toolchain
/// here" from "the oracle expression itself raised" — and a matrix whose oracle expression is
/// silently skipped is worth nothing, so a typo'd or failing expression must fail loudly. This
/// wrapper therefore skips only when the tool cannot be *spawned* at all (no `erl` on `PATH`) and
/// fails with the VM's own stderr otherwise.
///
/// Returns `None` when the toolchain is absent; the caller prints the loud skip banner.
fn oracle_output(expr: &str) -> Option<String> {
    match std::process::Command::new("erl")
        .args(["-noshell", "-eval", expr, "-s", "init", "stop"])
        .output()
    {
        // No `erl` on `PATH`: a loud skip, never a silent pass.
        Err(_) => None,
        Ok(output) => {
            assert!(
                output.status.success(),
                "the oracle expression failed (status {:?}): {}\n--- expression ---\n{expr}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
    }
}

/// A session row rendered the way the oracle's `~w` prints it: `{row, [Reply], Input, Output}`.
fn row_w(name: &str, reply: &[i64], input: i64, output: i64) -> String {
    format!("{{{name},{},{input},{output}}}", erl_bytes(reply))
}

/// `bytes` as an Erlang binary literal: `<<1,0,0,0,3,97,98,99>>`.
fn erl_binary(bytes: &[u8]) -> String {
    let rendered: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("<<{}>>", rendered.join(","))
}

/// `text` as the byte values `~w` prints for a char list.
fn ascii_bytes(text: &str) -> Vec<i64> {
    text.bytes().map(i64::from).collect()
}

/// The oracle expression for a whole session: open `ram_file_drv` with `options`, send every `row`
/// in order, and print one `{row, Reply, Input, Output}` line per row — the shape [`row_w`]
/// renders on this side — then `tail` (if any) and the closing counter line.
///
/// `receive … after 2000 -> timeout end` is the oracle's own bound: a reply that never arrives
/// prints `timeout` in that row's place, so the comparison fails on that row instead of hanging.
fn oracle_session_expr(options: &str, rows: &[Row], tail: &str) -> String {
    let mut expr = format!(
        "begin \
         P = open_port({{spawn_driver,\"ram_file_drv\"}},{options}), \
         C = fun(Cmd) -> true = port_command(P, Cmd), \
                         receive {{P,{{data,D}}}} -> D after 2000 -> timeout end end, \
         I = fun(Item) -> {{Item,V}} = erlang:port_info(P, Item), V end, \
         F = fun(N,Cmd) -> R = C(Cmd), io:format(\"~w~n\",[{{N,R,I(input),I(output)}}]) end, "
    );
    for row in rows {
        expr.push_str(&format!("F({},{}),", row.name, erl_binary(row.command)));
    }
    expr.push_str(tail);
    expr.push_str("io:format(\"~w~n\",[{final,I(input),I(output)}]), ok end");
    expr
}

/// The `ram_file_drv` command/reply matrix, row for row, against the live oracle.
///
/// Every row is one `port_command/2`: its reply reaches the owner exactly once, its length is what
/// `port_info(Port, input)` counts, and its payload bytes are the oracle's. The session totals are
/// asserted both per row (inside [`Matrix::step`]) and at the end (`{final, In, Out}`).
#[test]
fn the_ram_file_driver_matrix_matches_the_oracle() {
    let mut session = Matrix::new(Stream::List);
    session.run(MAIN_SESSION);
    session.finish();

    // The load-bearing rows, spelled out beyond the session comparison, so a divergence names the
    // opcode it belongs to instead of only the line it landed on.
    assert_eq!(
        session.reply("open_fresh"),
        vec![3, 0, 0, 0, 0],
        "OPEN answers NUMBER 0 (the C's 'unused' result slot)"
    );
    assert_eq!(
        session.reply("read_data"),
        vec![2, 0, 0, 0, 2, 97, 98],
        "READ answers `DATA 2` with the two bytes the file holds"
    );
    assert_eq!(
        session.reply("read_clamped"),
        vec![2, 0, 0, 0, 1, 99],
        "a read is clamped to what is left, never an error"
    );
    assert_eq!(
        session.reply("read_eof"),
        vec![2, 0, 0, 0, 0],
        "a read at EOF is a zero-length DATA, not an error"
    );
    assert_eq!(
        session.reply("write_xy"),
        vec![3, 0, 0, 0, 2],
        "WRITE answers NUMBER <bytes written>"
    );
    assert_eq!(
        session.reply("size"),
        vec![3, 0, 0, 0, 5],
        "SIZE reports the logical length after the write"
    );
    assert_eq!(
        session.reply("get"),
        vec![2, 0, 0, 0, 5, 97, 98, 99, 88, 89],
        "GET dumps the whole logical file (`abc` ++ the written `XY`)"
    );
    assert_eq!(
        session.reply("read_whole"),
        vec![2, 0, 0, 0, 5, 97, 98, 99, 88, 89],
        "the LSEEK to 0 really moved the position (the read starts at `a` again)"
    );
    assert_eq!(
        session.reply("truncate_shrink"),
        vec![0],
        "TRUNCATE answers the bare OK byte"
    );
    assert_eq!(
        session.reply("get_after_shrink"),
        vec![2, 0, 0, 0, 2, 97, 98],
        "truncating at position 2 drops the tail"
    );
    assert_eq!(
        session.reply("get_after_extend"),
        vec![2, 0, 0, 0, 6, 97, 98, 0, 0, 0, 0],
        "truncating past the end extends with zeros — the dropped `c`/`X`/`Y` do not come back"
    );
    // The default stream is a char list — the oracle's own `{data,[3,0,0,0,0]}` shape.
    assert!(
        !session.reply_is_binary("open_fresh"),
        "the default (list) stream delivers the reply as a char list"
    );

    session.assert_session_matches_the_oracle(
        MAIN_SESSION,
        "",
        "the `ram_file_drv` command/reply matrix",
    );
    // The session's closing counters, spelled out: Σ every delivered reply's bytes and Σ every
    // command's bytes, as the oracle reports them for this exact sequence.
    assert_eq!((session.input, session.output), (98, 66));
}
/// The positional and re-`OPEN` half of the matrix: a re-`OPEN` of a live port replaces content and
/// position, the located reads/writes leave the position alone, the four `flags`-gated no-ops
/// answer OK, and the two negative-offset plus two bad-`whence` forms are `einval`.
#[test]
fn the_ram_file_driver_positional_matrix_matches_the_oracle() {
    let mut session = Matrix::new(Stream::List);
    session.run(EXTENDED_SESSION);
    session.finish();

    assert_eq!(
        session.reply("reopen"),
        vec![3, 0, 0, 0, 0],
        "a re-OPEN of a live port answers NUMBER 0"
    );
    assert_eq!(
        session.reply("size_after_reopen"),
        vec![3, 0, 0, 0, 3],
        "a re-OPEN replaces the content (3 bytes): it neither truncates nor appends"
    );
    assert_eq!(
        session.reply("read_one"),
        vec![2, 0, 0, 0, 1, 97],
        "the re-OPEN reset the position, so the read starts at `a`"
    );
    assert_eq!(
        session.reply("pread_0_3"),
        vec![2, 0, 0, 0, 3, 97, 98, 99],
        "PREAD reads at its own offset"
    );
    assert_eq!(
        session.reply("read_after_pread"),
        vec![2, 0, 0, 0, 2, 98, 99],
        "…and left the position where `read_one` put it (1)"
    );
    assert_eq!(
        session.reply("pwrite_at_10"),
        vec![3, 0, 0, 0, 1],
        "PWRITE answers NUMBER <bytes written>"
    );
    assert_eq!(
        session.reply("get_after_pwrite"),
        vec![2, 0, 0, 0, 11, 97, 98, 99, 0, 0, 0, 0, 0, 0, 0, 90],
        "a write past the end leaves a zero-filled gap"
    );
    for row in ["fsync", "fdatasync", "advise", "allocate"] {
        assert_eq!(
            session.reply(row),
            vec![0],
            "{row} on an opened port answers the bare OK byte"
        );
    }
    let einval = vec![1, 101, 105, 110, 118, 97, 108];
    for row in ["pread_neg", "pwrite_neg", "lseek_neg", "lseek_bad_whence"] {
        assert_eq!(
            session.reply(row),
            einval,
            "{row} answers ERROR einval (the reply bytes spelled as ASCII)"
        );
    }

    session.assert_session_matches_the_oracle(
        EXTENDED_SESSION,
        "",
        "the `ram_file_drv` positional/re-OPEN matrix",
    );
    assert_eq!((session.input, session.output), (89, 80));
}
/// Every operation against a port that was never opened: `size`/`get` are answered from the
/// (empty) file, the access-mode-gated operations are `ebadf`, and `truncate` is `eacces`.
#[test]
fn the_ram_file_driver_refusal_matrix_matches_the_oracle() {
    let mut session = Matrix::new(Stream::List);
    session.run(REFUSAL_SESSION);
    // `port_info(Port, name)`: the driver name as the oracle's char list, taken from the
    // provider's own `port_info/1` answer.
    let name = text_of(session.port_info_item("name"));
    session.push_line(format!("{{name,{}}}", erl_bytes(&ascii_bytes(&name))));
    session.finish();
    assert_eq!(name, "ram_file_drv", "the port names its driver");

    assert_eq!(
        session.reply("size_unopened"),
        vec![3, 0, 0, 0, 0],
        "SIZE is `flags`-independent: an unopened driver reports the empty file"
    );
    assert_eq!(
        session.reply("get_unopened"),
        vec![2, 0, 0, 0, 0],
        "GET dumps the empty file with no access mode set"
    );
    let ebadf = vec![1, 101, 98, 97, 100, 102];
    for row in [
        "read_unopened",
        "write_unopened",
        "lseek_unopened",
        "fsync_unopened",
        "fdatasync_unopened",
        "advise_unopened",
        "allocate_unopened",
    ] {
        assert_eq!(
            session.reply(row),
            ebadf,
            "{row} answers ERROR ebadf (the reply bytes spelled as ASCII)"
        );
    }
    assert_eq!(
        session.reply("truncate_unopened"),
        vec![1, 101, 97, 99, 99, 101, 115],
        "TRUNCATE's gate is the WRITE mode bit, so an unopened port answers ERROR eacces"
    );

    session.assert_session_matches_the_oracle(
        REFUSAL_SESSION,
        NAME_TAIL,
        "the `ram_file_drv` refusal matrix",
    );
    assert_eq!((session.input, session.output), (59, 23));
}
/// Closing the port: `port_close/1` answers `true`, the port then reports `undefined` from
/// `port_info/1,2` (the provider's empty pair list, which is exactly the oracle's `undefined`), and
/// a command on it raises `error:badarg`.
#[test]
fn closing_a_ram_file_port_refuses_later_commands_like_the_oracle() {
    let mut session = Matrix::new(Stream::List);
    session.step(&Row {
        name: "reply",
        command: OPEN_ABC,
    });
    let reply = session.reply("reply");
    let (input, output) = (session.input, session.output);

    session
        .hook
        .close_port(session.port)
        .expect("port_close/1 on a built-in driver port is the oracle's `true`");
    let info_after_close = session
        .hook
        .port_info(session.port)
        .expect("port_info on a closed port is `undefined`, never an error");
    assert!(
        info_after_close.is_empty(),
        "a closed port has no items (the oracle's `undefined`): {info_after_close:?}"
    );
    let command_after_close = session
        .runtime
        .ports
        .borrow_mut()
        .write_command(session.port, OPEN_ABC);
    assert!(
        matches!(command_after_close, Err(BeamError::BadArg)),
        "port_command/2 on a closed port is `error:badarg`, got {command_after_close:?}"
    );

    // The four facts in the oracle's own shape, compared with the oracle's own output.
    let rendered = format!(
        "{}\n{{close,true}}\n{{info_after_close,undefined}}\n{{command_after_close,badarg}}",
        row_w("reply", &reply, input, output)
    );
    let expr = "begin \
                P = open_port({spawn_driver,\"ram_file_drv\"},[]), \
                C = fun(Cmd) -> true = port_command(P, Cmd), \
                                receive {P,{data,D}} -> D after 2000 -> timeout end end, \
                I = fun(Item) -> {Item,V} = erlang:port_info(P, Item), V end, \
                R = C(<<1,0,0,0,3,97,98,99>>), \
                io:format(\"~w~n\",[{reply,R,I(input),I(output)}]), \
                io:format(\"~w~n\",[{close,port_close(P)}]), \
                io:format(\"~w~n\",[{info_after_close,erlang:port_info(P,input)}]), \
                Cmd = case catch port_command(P, <<37>>) of \
                          {'EXIT',{badarg,_}} -> badarg; Other -> Other end, \
                io:format(\"~w~n\",[{command_after_close,Cmd}]), ok end";
    match oracle_output(expr) {
        Some(oracle) => assert_eq!(
            rendered, oracle,
            "the close row must agree with the oracle (the reply, the close, and both post-close \
             answers)"
        ),
        None => skip_oracle_check("closing a built-in driver port and its post-close refusals"),
    }
}

/// The `[binary]` stream option's **term shape** diverges while the reply *bytes* agree.
///
/// Measured (erts-17.1): the oracle prints this driver's `NUMBER`/`OK` replies as flat lists
/// (`[3,0,0,0,0]`) and its `DATA` replies as an **improper** list (`[2,0,0,0,2|<<97,98>>]`) —
/// `driver_output_binary` hands the five-byte header to the list stream and the payload on as a
/// binary tail. This runtime's message builder turns a binary-stream port's whole reply into one
/// binary (`<<2,0,0,0,2,97,98>>`).
///
/// The bytes and both counters agree, so nothing a caller *reads* is lost — but the term shape is
/// a real divergence, and it lives in the stream-message builder rather than in this driver. This
/// test pins **both** measured shapes, prints the divergence loudly, and asserts the byte equality,
/// rather than relaxing the comparison or hiding the difference.
#[test]
fn the_binary_stream_option_diverges_in_term_shape_only() {
    let mut session = Matrix::new(Stream::Binary);
    session.run(BINARY_SESSION);
    session.finish();

    // The bytes are the list-stream session's bytes for the same opcodes …
    assert_eq!(session.reply("open_fresh"), vec![3, 0, 0, 0, 0]);
    assert_eq!(session.reply("read_data"), vec![2, 0, 0, 0, 2, 97, 98]);
    assert_eq!(
        session.reply("get"),
        vec![2, 0, 0, 0, 1, 97],
        "GET after the truncate to 1 byte"
    );
    assert!(
        session.reply_is_binary("read_data"),
        "this runtime delivers a binary-stream payload as a binary"
    );
    // … and the counters count the same bytes the oracle counts.
    assert_eq!(session.input, 5 + 7 + 5 + 5 + 1 + 6);
    assert_eq!(session.output, 8 + 5 + 1 + 9 + 1 + 1);

    let expr = "begin \
                P = open_port({spawn_driver,\"ram_file_drv\"},[binary]), \
                C = fun(Cmd) -> true = port_command(P, Cmd), \
                                receive {P,{data,D}} -> D after 2000 -> timeout end end, \
                I = fun(Item) -> {Item,V} = erlang:port_info(P, Item), V end, \
                D1 = C(<<1,0,0,0,3,97,98,99>>), \
                D2 = C(<<2,0,0,0,2>>), \
                io:format(\"~w~n\",[{open,D1}]), \
                io:format(\"~w~n\",[{read,D2}]), \
                io:format(\"~w~n\",[{input,I(input),output,I(output)}]) end";
    match oracle_output(expr) {
        Some(oracle) => {
            // The oracle's measured shape: a flat list for NUMBER, an improper list for DATA.
            assert_eq!(
                oracle, "{open,[3,0,0,0,0]}\n{read,[2,0,0,0,2|<<97,98>>]}\n{input,12,output,13}",
                "the oracle's binary-stream reply shape (measured on erts-17.1)"
            );
            println!("================================================================");
            println!(
                "DIVERGENCE (reported, not papered over): the `[binary]` stream's term shape."
            );
            println!("  oracle: {oracle}");
            println!(
                "  this runtime: {{open,<<3,0,0,0,0>>}}, {{read,<<2,0,0,0,2,97,98>>}} — the same \
                 bytes, delivered as binaries"
            );
            println!("================================================================");
        }
        None => skip_oracle_check("the `[binary]` stream reply shape"),
    }
}
