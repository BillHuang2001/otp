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
//! `ram_file_drv` is a built-in driver, so its replies are harvested by the port-task round the
//! write itself runs ([`PortRegistry::write_command`]) rather than by a poller wakeup: a reply that
//! round drops is a **lost reply**, and the owner's `receive` times out while
//! `port_info(Port, input)` stays `0`. A per-opcode unit test would not catch that (every opcode's
//! encoding can be right); what catches it is driving the *same* sequence under both runtimes and
//! comparing what the owner actually received.
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
//! `control` slot is `NULL`, which this driver answers through `Driver::has_control`), so
//! `beam_io`'s named-verb `Driver::control` transport is a Rust-only extension with no oracle
//! counterpart: nothing here drives it. Everything goes through `port_command`, which is the only
//! transport the oracle has.
//!
//! # Honest scope
//!
//! Nothing here fabricates a reply or papers a difference over: a row that disagrees with the
//! oracle fails on the session string (with the oracle's live output in the failure message).
//! The `[binary]` stream's *term shape* is pinned per reply kind by its own test, which also pins
//! the runtime's **builder** reproducing every shape and the runtime's **end-to-end** delivery of
//! the whole session — header lists and the split `DATA` tail alike.
use super::tests::{
    counter, data_payload, erl_bytes, fixture, skip_oracle_check, text_of, Fixture, PayloadShape,
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
    /// The `binary` option: the reply's *bytes* are the same, its *term shape* follows the driver
    /// output call that carried them — see
    /// [`a_binary_stream_port_delivers_the_oracles_header_lists`].
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

/// That session as the oracle renders it on a `[binary]` port, one `~w` line per row plus the
/// closing counter line (erts-17.1, and [`BINARY_SESSION`]'s own bytes).
///
/// A `DATA` reply is the **improper** `[header|<<data>>]` the C's `driver_output_binary` renders;
/// every other reply is a proper header list, because `rfile_command` answers `OK`/`NUMBER`/`ERROR`
/// through `driver_output2(hbuf, len, NULL, 0)` — a header with no data part at all.
const BINARY_SESSION_GOLDEN: &str = "{open_fresh,[3,0,0,0,0],5,8}\n\
     {read_data,[2,0,0,0,2|<<97,98>>],12,13}\n\
     {size,[3,0,0,0,3],17,14}\n\
     {truncate_seek,[3,0,0,0,1],22,23}\n\
     {truncate,[0],23,24}\n\
     {get,[2,0,0,0,1|<<97>>],29,25}\n\
     {final,29,25}";

/// A live runtime with a `{spawn_driver, "ram_file_drv"}` port opened on the runtime's **own**
/// registry, so the reply the driver writes during the write's task round is the very one
/// [`RuntimeContext::poll_port_messages`] (the `-eval` route's port half) reports.
///
/// The session rendering it accumulates is what gets compared: `lines` holds one
/// `{row, Reply, Input, Output}` per row plus the closing `{final, Input, Output}`, where `Reply`
/// is the payload's **actual** `~w` rendering (see [`Matrix::render`]).
struct Matrix {
    runtime: Runtime,
    fx: Fixture,
    hook: RuntimePorts,
    port: PortId,
    stream: Stream,
    lines: Vec<String>,
    rows: HashMap<&'static str, RowRecord>,
    /// The running totals: Σ delivered reply bytes and Σ `port_command` payload bytes.
    input: i64,
    output: i64,
}

/// One row's received payload, as the owner actually got it.
struct RowRecord {
    /// The payload's byte values, flattened out of whichever [`PayloadShape`] it arrived in.
    bytes: Vec<i64>,
    /// The shape the payload arrived in — the producing `driver_output*` call's, per
    /// [`PayloadShape`].
    shape: PayloadShape,
    /// The payload's `~w` rendering by the runtime's own printer ([`Runtime::format_term_w`]),
    /// which is what the session line prints.
    rendered: String,
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
            rows: HashMap::new(),
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
    /// The session line prints the payload's **actual** rendering ([`Matrix::render`]), so the
    /// comparison with the oracle covers the term's shape and not only its bytes.
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
        let message = self.take_reply(row.name);
        let (bytes, shape) = data_payload(message);
        let rendered = self.render(data_payload_term(message));
        self.input += i64::try_from(bytes.len()).expect("a reply length fits an i64");
        self.output += i64::try_from(row.command.len()).expect("a command length fits an i64");
        assert_eq!(
            counter(&mut self.fx, &mut self.hook, self.port, "input"),
            self.input,
            "row {:?}: `port_info(Port, input)` must count the {} reply bytes exactly once",
            row.name,
            bytes.len()
        );
        assert_eq!(
            counter(&mut self.fx, &mut self.hook, self.port, "output"),
            self.output,
            "row {:?}: `port_info(Port, output)` must count the {} command bytes",
            row.name,
            row.command.len()
        );
        self.lines.push(format!(
            "{{{},{rendered},{},{}}}",
            row.name, self.input, self.output
        ));
        self.rows
            .insert(row.name, RowRecord { bytes, shape, rendered });
    }

    /// Polls the runtime's own poller until `name`'s reply arrives, returning the whole
    /// `{Port, {data, Payload}}` message.
    ///
    /// The message must come from **this** port and there must be exactly one: one command is one
    /// reply, so a second message — or a foreign port's — fails the row.
    fn take_reply(&mut self, name: &'static str) -> Eterm {
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
        message
    }

    /// `term` as the registered `~w` printer renders it — the oracle's own control, so a session
    /// line's payload text is comparable with the oracle's character for character.
    fn render(&mut self, term: Eterm) -> String {
        self.runtime
            .format_term_w(term, None)
            .unwrap_or_else(|error| panic!("the `~w` printer refused a port payload: {error}"))
            .trim_end_matches('\n')
            .to_string()
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

    /// A row's captured reply, as byte values flattened out of its [`PayloadShape`].
    fn reply(&self, name: &str) -> Vec<i64> {
        self.record(name).bytes.clone()
    }

    /// A row's captured reply, as the `~w` printer rendered it.
    fn rendered(&self, name: &str) -> &str {
        &self.record(name).rendered
    }

    /// The [`PayloadShape`] a row's payload arrived in.
    fn shape(&self, name: &str) -> PayloadShape {
        self.record(name).shape
    }

    /// A row's record, or a loud failure naming the session it is missing from.
    fn record(&self, name: &str) -> &RowRecord {
        self.rows
            .get(name)
            .unwrap_or_else(|| panic!("no {name:?} row in this session"))
    }

    /// Asserts every row of `rows` arrived as the flat proper char list the `list` stream produces
    /// (`PayloadShape::ProperList`) — one byte per element, the header/body split invisible.
    fn assert_every_row_is_a_proper_list(&self, rows: &[Row]) {
        for row in rows {
            assert_eq!(
                self.shape(row.name),
                PayloadShape::ProperList,
                "row {:?}: the default `list` stream renders every reply byte as a list element",
                row.name
            );
        }
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

/// The `Data` payload term of a `{Port, {data, Data}}` message.
///
/// [`data_payload`] reads the same slot as bytes and shape; the term itself is what the session
/// renders through the `~w` printer.
fn data_payload_term(message: Eterm) -> Eterm {
    let inner = terms::tuple_element(message, 1).expect("the `{data, Payload}` pair");
    terms::tuple_element(inner, 1).expect("the payload")
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
pub(super) fn oracle_output(expr: &str) -> Option<String> {
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

/// The `Data` payload [`RuntimeContext::port_data_message_shaped`] builds for `read` on `stream`
/// with `shape`, rendered by the registered `~w` printer — the oracle's own control, so the two
/// renderings are compared as text (`[2,0,0,0,2|<<97,98>>]`).
///
/// `read` is one driver output call's bytes with the C's header/body split intact, so the goldens
/// below are the same terms they always were: only the driving input changed (a `ReadMessage`
/// instead of a `(bytes, clamp)` guess).
///
/// The runtime owns the heap the term is built on, so it is borrowed for the whole read.
fn built_reply(rt: &mut Runtime, read: &ReadMessage, stream: StreamType, shape: StreamShape) -> String {
    let port = PortId::new(9001);
    let message = {
        let mut ctx = RuntimeContext::new(&mut *rt);
        ctx.port_data_message_shaped(port, read, stream, shape)
            .unwrap_or_else(|error| panic!("a message for {read:?} ({shape:?}): {error}"))
    };
    let inner = terms::tuple_element(message, 1).expect("the `{data, …}` tuple");
    let payload = terms::tuple_element(inner, 1).expect("the data payload");
    let rendered = rt
        .format_term_w(payload, None)
        .unwrap_or_else(|error| panic!("the `~w` printer refused a built message: {error}"));
    rendered.trim_end_matches('\n').to_string()
}

/// A `driver_output2(hbuf, hlen, NULL, 0)`-shaped read: the whole reply is the header and there is
/// **no** body at all (the C's `NULL` `buf`), which is what renders the proper header list.
fn header_only(bytes: &[u8]) -> ReadMessage {
    ReadMessage {
        header: bytes.to_vec(),
        body: None,
    }
}

/// A `driver_output_binary(hbuf, hlen, bin, off, len)`-shaped read: `header_len` leading bytes are
/// the header, the rest is the data body (`Some`, empty or not — the C's non-`NULL` `buf`).
fn header_then_body(bytes: &[u8], header_len: usize) -> ReadMessage {
    assert!(header_len <= bytes.len(), "a header longer than the reply");
    ReadMessage {
        header: bytes[..header_len].to_vec(),
        body: Some(bytes[header_len..].to_vec()),
    }
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
/// in order, and print one `{row, Reply, Input, Output}` line per row — the shape
/// [`Matrix::step`] renders on this side — then `tail` (if any) and the closing counter line.
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
    // The default stream is a flat proper char list — the oracle's own `{data,[3,0,0,0,0]}` shape,
    // with the driver's header/body split invisible.
    session.assert_every_row_is_a_proper_list(MAIN_SESSION);
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

    // Every row of a default-stream session is the flat proper char list, so the driver's
    // header/body split never changes the term the owner sees.
    session.assert_every_row_is_a_proper_list(EXTENDED_SESSION);
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

    session.assert_every_row_is_a_proper_list(REFUSAL_SESSION);
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
    let reply = session.rendered("reply").to_string();
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
        "{{reply,{reply},{input},{output}}}\n\
         {{close,true}}\n\
         {{info_after_close,undefined}}\n\
         {{command_after_close,badarg}}"
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

/// The `[binary]` stream's **term shape**, pinned per reply kind: the oracle's measured terms, and
/// the runtime's message builder reproducing every one of them.
///
/// Measured (erts-17.1), one row per reply kind — the shape is a function of the driver output call
/// that carried the bytes, never of the bytes alone (`driver_output2` hands the header to the list
/// stream, `driver_output_binary` hands the header to the list and the payload on as a binary tail):
///
/// | reply (`ram_file_drv.c`) | call | `[binary]` port's `{data, D}` | default `list` port |
/// |---|---|---|---|
/// | `NUMBER` (`numeric_reply`, `:197`) | `driver_output2(buf, len, NULL, 0)` | `[3,0,0,0,0]` — a **proper** list | `[3,0,0,0,0]` |
/// | `OK` (`reply`, `:181`) | the same | `[0]` | `[0]` |
/// | `ERROR` (`error_reply`, `:222`) | the same | `[1,101,105,110,118,97,108]` | the same |
/// | `DATA` (`:448`) | `driver_output_binary(port, hdr, 5, bin, 0, n)`, `n >= 1` | `[2,0,0,0,2\|<<97,98>>]` — **improper** | `[2,0,0,0,2,97,98]` |
/// | `DATA` at end of file | the same, `n == 0` | `[2,0,0,0,0\|<<>>]` | `[2,0,0,0,0]` |
///
/// Every one of those terms is asserted below through [`built_reply`] — the runtime's **own**
/// builder ([`RuntimeContext::port_data_message_shaped`]) fed the bytes this session really
/// received, shaped by the driver call that produced them, and rendered by the registered `~w`
/// printer (the oracle's own control).
///
/// # End to end
///
/// A **built-in driver port** reports [`StreamShape::DriverOutput`], which renders a
/// **header-less** read as list elements on **either** stream — the C's header rendering — and
/// leaves a read that carries a header to its own split.
/// `beam_io`'s in-tree `ram_file` driver hands every reply up as one read carrying the C's
/// header/body split (`driver_output2(hbuf, len, NULL, 0)` for `OK`/`NUMBER`/`ERROR`,
/// `ram_file_drv.c:181-230`; `driver_output_binary(hdr, 5, bin, n)` for `DATA`, `:448-489`), so a
/// `NUMBER`/`OK`/`ERROR` reply arrives as a header with **no** body and renders the oracle's proper
/// list, and a `DATA` reply — empty or not — arrives as a five-byte header plus a body and renders
/// the oracle's improper `[hdr|<<data>>]`.
/// The whole `[binary]` session is asserted against the measured text below, per-row shape and both
/// `port_info` counters included, and the **builder** half feeds
/// [`RuntimeContext::port_data_message_shaped`] the same reads directly.
#[test]
fn a_binary_stream_port_delivers_the_oracles_header_lists() {
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
    // … and each reply's term shape is the producing `driver_output*` call's: a header-only reply
    // is a proper header list, a `DATA` reply the improper `[header|<<data>>]` with a five-byte
    // header. Neither is ever a bare binary — no reply here is header-less.
    let row_shapes = [
        ("open_fresh", PayloadShape::ProperList),
        ("read_data", PayloadShape::ImproperList { header_len: 5 }),
        ("size", PayloadShape::ProperList),
        ("truncate_seek", PayloadShape::ProperList),
        ("truncate", PayloadShape::ProperList),
        ("get", PayloadShape::ImproperList { header_len: 5 }),
    ];
    for (name, shape) in row_shapes {
        assert_eq!(
            session.shape(name),
            shape,
            "row {name:?}: a `[binary]` port renders the reply's own header/body split"
        );
    }
    // … and the counters count the same bytes the oracle counts, header and body alike.
    assert_eq!(session.input, 5 + 7 + 5 + 5 + 1 + 6);
    assert_eq!(session.output, 8 + 5 + 1 + 9 + 1 + 1);

    // The measured matrix, whole session: the oracle's own rendering of these opcodes on a
    // `[binary]` port, counters included. The golden is unconditional — it is a measurement, not a
    // toolchain probe.
    assert_eq!(
        session.session(),
        BINARY_SESSION_GOLDEN,
        "the `[binary]` stream's measured reply shapes (erts-17.1)"
    );
    // …and the live oracle, where there is one, prints exactly that. A toolchain-less host is a
    // loud skip, never a pass.
    match oracle_output(&oracle_session_expr("[binary]", BINARY_SESSION, "")) {
        Some(oracle) => assert_eq!(
            oracle,
            session.session(),
            "the `[binary]` stream's session must match the live oracle, rendering and counters"
        ),
        None => skip_oracle_check("the `[binary]` stream's measured reply shapes"),
    }
    // ...and the runtime's **builder** renders each of those terms, from the bytes this session
    // really received, shaped by the driver output call that carried them.
    let shapes: [(&str, ReadMessage, &str, &str); 6] = [
        // `numeric_reply` — `driver_output2(buf, len, NULL, 0)`: the whole reply is the header.
        (
            "open_fresh",
            header_only(&[3, 0, 0, 0, 0]),
            "[3,0,0,0,0]",
            "[3,0,0,0,0]",
        ),
        // `READ` — `driver_output_binary(port, hdr, 5, bin, 0, n)`: a 5-byte header, a data body.
        (
            "read_data",
            header_then_body(&[2, 0, 0, 0, 2, 97, 98], 5),
            "[2,0,0,0,2|<<97,98>>]",
            "[2,0,0,0,2,97,98]",
        ),
        (
            "size",
            header_only(&[3, 0, 0, 0, 3]),
            "[3,0,0,0,3]",
            "[3,0,0,0,3]",
        ),
        (
            "truncate_seek",
            header_only(&[3, 0, 0, 0, 1]),
            "[3,0,0,0,1]",
            "[3,0,0,0,1]",
        ),
        // `reply` — the bare `OK` byte.
        ("truncate", header_only(&[0]), "[0]", "[0]"),
        (
            "get",
            header_then_body(&[2, 0, 0, 0, 1, 97], 5),
            "[2,0,0,0,1|<<97>>]",
            "[2,0,0,0,1,97]",
        ),
    ];
    for (name, read, binary_term, list_term) in shapes {
        let bytes: Vec<u8> = session
            .reply(name)
            .into_iter()
            .map(|byte| u8::try_from(byte).expect("a reply byte"))
            .collect();
        assert_eq!(
            read.flattened(),
            bytes,
            "row {name:?}: the shaped read carries exactly the bytes the session received"
        );
        assert_eq!(
            built_reply(&mut session.runtime, &read, StreamType::Binary, StreamShape::Data),
            binary_term,
            "row {name:?}: a `[binary]` port renders the read's own split as the oracle's term"
        );
        assert_eq!(
            built_reply(&mut session.runtime, &read, StreamType::List, StreamShape::Data),
            list_term,
            "row {name:?}: the default `list` stream renders the same bytes without a binary tail"
        );
    }
    // The two shapes no session row reaches: a zero-length `DATA` (a read at end of file) and an
    // `ERROR` reply — both measured above, both the builder's to reproduce.
    for (read, oracle_term) in [
        (
            header_then_body(&[2u8, 0, 0, 0, 0], 5),
            "[2,0,0,0,0|<<>>]",
        ),
        (
            header_only(&[1u8, 101, 105, 110, 118, 97, 108]),
            "[1,101,105,110,118,97,108]",
        ),
    ] {
        assert_eq!(
            built_reply(&mut session.runtime, &read, StreamType::Binary, StreamShape::Data),
            oracle_term,
            "{read:?} on a `[binary]` port renders the oracle's term"
        );
    }
    // The default stream's own measured form of a `DATA` reply: every byte a list element.
    assert_eq!(
        built_reply(
            &mut session.runtime,
            &header_then_body(&[2, 0, 0, 0, 2, 97, 98], 5),
            StreamType::List,
            StreamShape::Data,
        ),
        "[2,0,0,0,2,97,98]",
        "the default `list` stream is a proper list of the header and the data"
    );

}

// ---------------------------------------------------------------------------------------------
// The **built-in** driver's row of the C's `port_command/2,3` decision table
// ---------------------------------------------------------------------------------------------

/// `ram_file_drv`'s reply to [`OPEN_ABC`]: the `NUMBER` reply code (`3`) plus a big-endian `i32`
/// zero (the C's `numeric_reply(f, 0)` — the `open` result slot, "0 is not used").
const OPEN_NUMBER_REPLY: [u8; 5] = [3, 0, 0, 0, 0];

/// A **built-in** driver port is idle, takes a plain command (`true`, and the bytes really reach the
/// driver) and refuses `[force]` with `error:notsup` — because a built-in driver has no
/// `ErlDrvEntry` flag word and so can never carry `ERL_DRV_FLAG_SOFT_BUSY`.
///
/// Measured on the oracle (OTP 29.1.1 / erts-17.1) for
/// `P = open_port({spawn_driver,"ram_file_drv"},[])`:
///
/// | call | oracle |
/// |---|---|
/// | `port_command(P, <<1,0,0,0,3,97,98,99>>)` | `true` |
/// | `port_command(P, <<1,0,0,0,3,97,98,99>>, [nosuspend])` | `true` |
/// | `port_command(P, <<1,0,0,0,3,97,98,99>>, [force])` | `error:notsup` |
/// | `port_command(P, <<1,0,0,0,3,97,98,99>>, [force,nosuspend])` | `error:notsup` |
///
/// The refusal is decided from the driver's **flag word, never from the busy state**
/// (`erl_bif_port.c:195-198`, read *before* `erts_port_output`), which is exactly why an **idle**
/// built-in port refuses it: a built-in driver is the C's "never busy, no `SOFT_BUSY`" case that
/// [`beam_io::Driver::is_busy`]/[`beam_io::Driver::flags`]'s trait defaults describe. That is the
/// one row of the table the sibling `driver/port_attributes_tests.rs` cannot state — its fixtures are
/// `dlopen`ed drivers with a real entry to carry (or omit) the bit.
///
/// Driven on `beam_io`'s registry directly, because that is where the decision table lives:
/// [`crate::PortRegistry`] exposes no `port_command_with` (the runtime's `port_command` floor
/// re-derives the same arms in `crate::native::port`). The row is hermetic — no child, no oracle
/// round trip, and no wait: a `ram_file` port owns no descriptor, so the poller is only a signature.
#[test]
fn a_builtin_driver_port_takes_a_plain_command_and_refuses_force() {
    let mut poller = Poller::new().expect("a poll set");
    let drivers = DriverRegistry::with_static_spawn_drivers();
    let mut ports = IoPortRegistry::new();
    let id = ports
        .open_named(&mut poller, &drivers, "ram_file_drv")
        .expect("the built-in ram_file_drv opens");
    assert!(
        !ports.is_busy(id).expect("a live port"),
        "a built-in driver never marks its port busy (no `set_busy_port`)"
    );

    // `[force]`: the refusal, on the idle port. Nothing is queued, so nothing can be written.
    let err = ports
        .port_command_with(
            &mut poller,
            id,
            OPEN_ABC,
            PortCommandOptions::EMPTY.with_force(true),
        )
        .expect_err("a built-in driver has no SOFT_BUSY flag word, so `[force]` is notsup");
    assert_eq!(
        err.as_atom(),
        Some("notsup"),
        "the oracle raises `error:notsup` for `[force]` on a `{{spawn_driver,\"ram_file_drv\"}}` port"
    );
    let err = ports
        .port_command_with(
            &mut poller,
            id,
            OPEN_ABC,
            PortCommandOptions::EMPTY
                .with_force(true)
                .with_nosuspend(true),
        )
        .expect_err("`nosuspend` does not soften the capability refusal");
    assert_eq!(
        err.as_atom(),
        Some("notsup"),
        "the oracle's same `error:notsup`"
    );
    let port = ports.get(id).expect("a live port");
    assert_eq!(
        (port.output_len(), port.input_len()),
        (0, 0),
        "a refused forced command queues no bytes and the driver answers nothing"
    );

    // The idle, non-forced rows: both are the oracle's `true`, and the command really reaches the
    // driver — its five-byte `NUMBER 0` reply is what the round harvests.
    assert_eq!(
        ports
            .port_command_with(&mut poller, id, OPEN_ABC, PortCommandOptions::EMPTY)
            .expect("an idle port takes a plain command"),
        beam_io::PortCommandOutcome::Done,
        "the oracle answers `true` for a plain `port_command/2` on the idle port"
    );
    assert_eq!(
        ports.get(id).expect("a live port").output_len(),
        OPEN_ABC.len(),
        "the whole command is queued for the write path"
    );
    let round = ports
        .execute_port_tasks(&mut poller, 1024)
        .expect("the output task runs");
    assert_eq!(
        round.notifications,
        vec![PortNotification::Data {
            port: id,
            // `numeric_reply` answers through `driver_output2(hbuf, len, NULL, 0)`, so the reply
            // travels up as the read's **header** with no data part at all — the split the owner
            // later renders (`ram_file_drv.c:197`).
            read: ReadMessage {
                header: OPEN_NUMBER_REPLY.to_vec(),
                body: None,
            },
        }],
        "the command reached the driver: exactly one `{{data, …}}` notification carrying its reply"
    );
    assert_eq!(
        ports
            .port_command_with(
                &mut poller,
                id,
                OPEN_ABC,
                PortCommandOptions::EMPTY.with_nosuspend(true)
            )
            .expect("an idle port takes a `[nosuspend]` command too"),
        beam_io::PortCommandOutcome::Done,
        "the oracle answers `true` for `[nosuspend]` on an idle port (the option only matters when busy)"
    );
}
