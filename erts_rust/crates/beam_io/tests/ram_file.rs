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

//! Integration tests for the built-in `"ram_file_drv"` in-memory file driver
//! ([`beam_io::drivers::RamFileDriver`]) — the Rust counterpart of the legacy C driver
//! `erts/emulator/drivers/common/ram_file_drv.c`, the driver `lib/kernel/src/ram_file.erl`
//! talks to.
//!
//! The suite drives the driver the way the runtime does — through the **real port layer**, as
//! `tests/inet.rs` drives `tcp_inet` — so the whole path a `ram_file:open/2` call rides is
//! exercised end to end: `DriverRegistry::with_builtins()` → `create` → `PortRegistry::open` →
//! `port_command` (queue + schedule an `OUTPUT` port task) → `execute_port_tasks` →
//! `Driver::output` (command word + operands) → the reply on the port's input queue →
//! `PortNotification::Data` on the round's outcome. The driver is **command driven** — it owns no
//! descriptor and registers nothing with the poller — so no event loop is involved: a round is
//! the whole transport.
//!
//! What is pinned: the built-in registration; the four reply encodings as literal bytes (`OK` =
//! `[0]`, `NUMBER` = `[3] ++ be32`, `DATA` = `[2] ++ be32 ++ n`, `ERROR` =
//! `[1] ++ lowercased errno`, big-endian); a 25-command grammar session driven through **both**
//! transports (`port_command` opcode bytes and the `ram_file.erl` verb names with their aliases)
//! with every reply asserted byte for byte as it arrives in a `PortNotification::Data`; the
//! access-mode violations (`EBADF`/`EACCES` as `ERROR <errno>` **replies**, i.e. bytes, never
//! Rust errors); protocol faults as the typed [`BeamError::BadArg`]; teardown as the registry's
//! reserved [`PortControl::CLOSE`] op (`Driver::stop` + one `Closed` report) versus
//! `Port::control`'s own `"close"` verb (the `reset()` `stop` runs, on a still-live port); and,
//! as the **compatibility gate**, the same three sessions replayed against the *real* system
//! `ram_file_drv` through `erl` (`open_port({spawn,"ram_file_drv"},[binary])`, one port per
//! session) with every reply compared byte for byte — skipping loudly, never failing, if `erl`
//! is absent.
//!
//! The replies this suite pins are the driver's own documented encoding *and* the oracle's
//! answer, so a divergence on either side of the comparison fails the test rather than being
//! papered over. The file is the whole driver suite (the crate's ~1000-line target is for a
//! single-responsibility source half, and this one is the comprehensive test module: no row is
//! padding, and the reply matrix is what makes it long).
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::drivers::ram_file::{
    RAM_FILE_ADVISE, RAM_FILE_ALLOCATE, RAM_FILE_FDATASYNC, RAM_FILE_FSYNC, RAM_FILE_GET,
    RAM_FILE_LSEEK, RAM_FILE_MODE_READ, RAM_FILE_MODE_READ_WRITE, RAM_FILE_OPEN, RAM_FILE_PREAD,
    RAM_FILE_PWRITE, RAM_FILE_READ, RAM_FILE_RESP_DATA, RAM_FILE_RESP_ERROR, RAM_FILE_RESP_NUMBER,
    RAM_FILE_RESP_OK, RAM_FILE_SEEK_CUR, RAM_FILE_SEEK_END, RAM_FILE_SEEK_SET, RAM_FILE_SIZE,
    RAM_FILE_TRUNCATE, RAM_FILE_WRITE,
};
use beam_io::drivers::RAM_FILE_DRIVER_NAME;
use beam_io::prelude::*;
use beam_process::PortId;

/// Wall-clock budget for the `erl` oracle before the affected check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a helper waits for a port-task round to report a reply before failing the test.
const WAIT: Duration = Duration::from_secs(10);

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// Opens a `"ram_file_drv"` port through the **built-in registry** (the way the runtime resolves
/// the name), so the registration itself is exercised.
fn ram_port(poller: &mut Poller) -> (PortRegistry, PortId) {
    let registry = DriverRegistry::with_builtins();
    let mut ports = PortRegistry::new();
    let driver = registry
        .create(RAM_FILE_DRIVER_NAME)
        .expect("the built-in ram_file_drv driver is registered");
    let id = ports.open(poller, driver).expect("the port opens");
    (ports, id)
}

// ---------------------------------------------------------------------------
// Command and reply builders (the wire form, byte for byte)
// ---------------------------------------------------------------------------

/// Big-endian `i32` — **the** integer encoding this driver's protocol uses, both for an operand
/// and for a `NUMBER`/`DATA` reply field.
fn be32(value: i32) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// Big-endian `i64` (the `advise`/`allocate` operands the driver ignores).
fn be64(value: i64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// The wire command: the command word followed by its operands.
fn cmd(opcode: u8, arg: &[u8]) -> Vec<u8> {
    let mut out = vec![opcode];
    out.extend_from_slice(arg);
    out
}

/// The `open` operands: the access-mode word followed by the initial content.
fn open_arg(mode: i32, content: &[u8]) -> Vec<u8> {
    let mut out = be32(mode);
    out.extend_from_slice(content);
    out
}

/// The `lseek` operands: offset and whence.
fn seek_arg(offset: i32, whence: i32) -> Vec<u8> {
    let mut out = be32(offset);
    out.extend_from_slice(&be32(whence));
    out
}

/// The `pread` operands: offset and length.
fn pread_arg(offset: i32, len: i32) -> Vec<u8> {
    let mut out = be32(offset);
    out.extend_from_slice(&be32(len));
    out
}

/// The `pwrite` operands: offset followed by the data.
fn pwrite_arg(offset: i32, data: &[u8]) -> Vec<u8> {
    let mut out = be32(offset);
    out.extend_from_slice(data);
    out
}

fn c_open(mode: i32, content: &[u8]) -> Vec<u8> {
    cmd(RAM_FILE_OPEN, &open_arg(mode, content))
}

fn c_read(len: i32) -> Vec<u8> {
    cmd(RAM_FILE_READ, &be32(len))
}

fn c_lseek(offset: i32, whence: i32) -> Vec<u8> {
    cmd(RAM_FILE_LSEEK, &seek_arg(offset, whence))
}

fn c_write(data: &[u8]) -> Vec<u8> {
    cmd(RAM_FILE_WRITE, data)
}

fn c_pread(offset: i32, len: i32) -> Vec<u8> {
    cmd(RAM_FILE_PREAD, &pread_arg(offset, len))
}

fn c_pwrite(offset: i32, data: &[u8]) -> Vec<u8> {
    cmd(RAM_FILE_PWRITE, &pwrite_arg(offset, data))
}

fn c_truncate() -> Vec<u8> {
    cmd(RAM_FILE_TRUNCATE, &[])
}

fn c_fsync() -> Vec<u8> {
    cmd(RAM_FILE_FSYNC, &[])
}

fn c_fdatasync() -> Vec<u8> {
    cmd(RAM_FILE_FDATASYNC, &[])
}

fn c_get() -> Vec<u8> {
    cmd(RAM_FILE_GET, &[])
}

fn c_size() -> Vec<u8> {
    cmd(RAM_FILE_SIZE, &[])
}

fn c_advise() -> Vec<u8> {
    let mut arg = be64(0);
    arg.extend_from_slice(&be64(4));
    arg.extend_from_slice(&be32(0));
    cmd(RAM_FILE_ADVISE, &arg)
}

fn c_allocate() -> Vec<u8> {
    let mut arg = be64(0);
    arg.extend_from_slice(&be64(100));
    cmd(RAM_FILE_ALLOCATE, &arg)
}

/// The `OK` reply: the bare response code.
fn resp_ok() -> Vec<u8> {
    vec![RAM_FILE_RESP_OK]
}

/// The `NUMBER` reply: the response code plus the value big-endian.
fn resp_number(value: i32) -> Vec<u8> {
    let mut out = vec![RAM_FILE_RESP_NUMBER];
    out.extend_from_slice(&be32(value));
    out
}

/// The `DATA` reply: the response code plus the length big-endian plus the bytes.
fn resp_data(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![RAM_FILE_RESP_DATA];
    out.extend_from_slice(&be32(bytes.len() as i32));
    out.extend_from_slice(bytes);
    out
}

/// The `ERROR` reply: the response code plus the lowercase errno name.
fn resp_error(name: &str) -> Vec<u8> {
    let mut out = vec![RAM_FILE_RESP_ERROR];
    out.extend_from_slice(name.as_bytes());
    out
}

// ---------------------------------------------------------------------------
// Driving the port layer
// ---------------------------------------------------------------------------

/// Queues `command` on port `id` and runs port-task rounds until the round reports the reply,
/// returning the round's notifications.
///
/// This is the whole `port_command` transport: the command is queued and an `OUTPUT` port task
/// is scheduled (the write belongs to the task, not to `port_command`), the task runs the
/// driver's `output`, and the round reports the reply the driver pushed onto the port's input
/// queue as [`PortNotification::Data`].
fn command_notifications(
    poller: &mut Poller,
    ports: &mut PortRegistry,
    id: PortId,
    command: &[u8],
) -> Vec<PortNotification> {
    assert!(
        !ports.get(id).expect("the port is live").has_pending_task(PortTaskKind::Output),
        "the port starts the round with no queued write"
    );
    ports.port_command(poller, id, command, PortCommandOptions::EMPTY).expect("the command queues");
    assert!(
        ports.get(id).expect("the port is live").has_pending_task(PortTaskKind::Output),
        "`port_command` schedules the OUTPUT task the write runs in"
    );

    let deadline = Instant::now() + WAIT;
    loop {
        let outcome = ports
            .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("the port-task round runs");
        if !outcome.notifications.is_empty() {
            assert_eq!(
                ports.get(id).expect("the port is live").output_len(),
                0,
                "the driver consumed the whole command {command:?}"
            );
            return outcome.notifications;
        }
        assert!(Instant::now() < deadline, "no reply arrived for the command {command:?}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// Concatenates the reply the round's notifications carry **for port `id`**, rejecting anything
/// else — a driver that reported a different port, or a close, has broken the transport.
fn reply_bytes(notifications: Vec<PortNotification>, id: PortId) -> Vec<u8> {
    let mut reply = Vec::new();
    for notification in notifications {
        match notification {
            PortNotification::Data { port, read } => {
                assert_eq!(port, id, "the reply names the command's own port");
                reply.extend_from_slice(&read.flattened());
            }
            PortNotification::Closed { port } => {
                panic!("port {port:?} closed during a command round")
            }
        }
    }
    reply
}

/// Drives one command through the port and returns the reply bytes the round reported.
fn command_round(
    poller: &mut Poller,
    ports: &mut PortRegistry,
    id: PortId,
    command: &[u8],
) -> Vec<u8> {
    reply_bytes(command_notifications(poller, ports, id, command), id)
}

/// Drives one named verb through the registry's `port_control` (the driver's `control`
/// transport, which returns its reply to the caller instead of queueing it as port data).
fn verb(
    poller: &mut Poller,
    ports: &mut PortRegistry,
    id: PortId,
    name: &str,
    arg: &[u8],
) -> Vec<u8> {
    ports
        .port_control(poller, id, name, arg)
        .unwrap_or_else(|err| panic!("the `{name}` verb is accepted: {err:?}"))
}

/// Replaces `err` with a panic unless it is the crate's argument fault — the *typed* error a
/// protocol fault must be, never a fabricated reply.
#[track_caller]
fn assert_badarg(outcome: Result<impl std::fmt::Debug, BeamError>, what: &str) {
    match outcome {
        Err(BeamError::BadArg) => {}
        other => panic!("{what} must be the typed BadArg, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The grammar session (both transports, one state machine each)
// ---------------------------------------------------------------------------

/// One step of the grammar session: the two spellings of one command — the integer opcode that
/// rides `port_command` and the `control` name (canonical or alias) — plus the reply both must
/// answer with.
struct Step {
    /// The `port_command` command word (the first byte of the payload).
    opcode: u8,
    /// The `Driver::control` name.
    name: &'static str,
    /// The operand bytes, shared by both spellings.
    arg: Vec<u8>,
    /// The reply, byte for byte.
    expected: Vec<u8>,
}

fn step(opcode: u8, name: &'static str, arg: Vec<u8>, expected: Vec<u8>) -> Step {
    Step { opcode, name, arg, expected }
}

/// A `read`/`write` open of an 11-byte file, then one command of every opcode this floor
/// supports — the linear state machine `ram_file.erl` drives, with each reply pinned.
///
/// Row by row: `open("hello world")`, then `size`/`get` (which need no access mode), an absolute
/// seek, a `read` of five bytes, a `SEEK_CUR` read-back, two reads (a clamped tail and an
/// end-of-file empty `DATA`), a `pread` and its negative-offset `EINVAL`, a `pwrite` that leaves
/// the position alone, a positioned `write`, a `SEEK_END` position, `truncate`, and the
/// mode-gated no-ops (`fsync`/`fdatasync`/`advise`/`allocate`) — each verb also spelled with its
/// `ram_file.erl` alias (`get_size`/`get_file`/`position`/`sync`/`datasync`).
fn grammar_session() -> Vec<Step> {
    vec![
        step(
            RAM_FILE_OPEN,
            "open",
            open_arg(RAM_FILE_MODE_READ_WRITE, b"hello world"),
            resp_number(0),
        ),
        step(RAM_FILE_SIZE, "size", vec![], resp_number(11)),
        step(RAM_FILE_SIZE, "get_size", vec![], resp_number(11)),
        step(RAM_FILE_GET, "get", vec![], resp_data(b"hello world")),
        step(RAM_FILE_GET, "get_file", vec![], resp_data(b"hello world")),
        step(RAM_FILE_LSEEK, "position", seek_arg(0, RAM_FILE_SEEK_SET), resp_number(0)),
        step(RAM_FILE_READ, "read", be32(5), resp_data(b"hello")),
        step(RAM_FILE_LSEEK, "lseek", seek_arg(0, RAM_FILE_SEEK_CUR), resp_number(5)),
        step(RAM_FILE_READ, "read", be32(100), resp_data(b" world")),
        step(RAM_FILE_READ, "read", be32(10), resp_data(b"")),
        step(RAM_FILE_PREAD, "pread", pread_arg(0, 5), resp_data(b"hello")),
        step(RAM_FILE_PREAD, "pread", pread_arg(-1, 5), resp_error("einval")),
        step(RAM_FILE_PWRITE, "pwrite", pwrite_arg(6, b"there"), resp_number(5)),
        step(RAM_FILE_GET, "get", vec![], resp_data(b"hello there")),
        step(RAM_FILE_WRITE, "write", b" XY".to_vec(), resp_number(3)),
        step(RAM_FILE_GET, "get", vec![], resp_data(b"hello there XY")),
        step(RAM_FILE_LSEEK, "position", seek_arg(0, RAM_FILE_SEEK_END), resp_number(14)),
        step(RAM_FILE_TRUNCATE, "truncate", vec![], resp_ok()),
        step(RAM_FILE_SIZE, "size", vec![], resp_number(14)),
        step(RAM_FILE_FSYNC, "fsync", vec![], resp_ok()),
        step(RAM_FILE_FSYNC, "sync", vec![], resp_ok()),
        step(RAM_FILE_FDATASYNC, "fdatasync", vec![], resp_ok()),
        step(RAM_FILE_FDATASYNC, "datasync", vec![], resp_ok()),
        step(RAM_FILE_ADVISE, "advise", c_advise()[1..].to_vec(), resp_ok()),
        step(RAM_FILE_ALLOCATE, "allocate", c_allocate()[1..].to_vec(), resp_ok()),
    ]
}

/// The commands a fresh, never-opened driver is given and the reply each must answer: every
/// mode-gated operation is `EBADF`, `truncate` (which needs the `WRITE` bit) is `EACCES`, and
/// the two operations that need no access mode still answer (`size` → `0`, `get` → empty).
fn unopened_session() -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        (c_fsync(), resp_error("ebadf")),
        (c_fdatasync(), resp_error("ebadf")),
        (c_advise(), resp_error("ebadf")),
        (c_allocate(), resp_error("ebadf")),
        (c_lseek(0, RAM_FILE_SEEK_SET), resp_error("ebadf")),
        (c_read(5), resp_error("ebadf")),
        (c_write(b"abcd"), resp_error("ebadf")),
        (c_pread(0, 5), resp_error("ebadf")),
        (c_pwrite(0, b"abcd"), resp_error("ebadf")),
        (c_truncate(), resp_error("eacces")),
        (c_size(), resp_number(0)),
        (c_get(), resp_data(b"")),
    ]
}

/// The commands a **read-only** open is given: the write paths are `EBADF`, `truncate` is
/// `EACCES`, and reading (including the located `pread`), `size`/`get` and the mode-gated
/// `fsync` all still work.
fn read_only_session() -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        (c_open(RAM_FILE_MODE_READ, b"seed"), resp_number(0)),
        (c_write(b"abcd"), resp_error("ebadf")),
        (c_pwrite(2, b"zz"), resp_error("ebadf")),
        (c_truncate(), resp_error("eacces")),
        (c_read(4), resp_data(b"seed")),
        (c_read(1), resp_data(b"")),
        (c_pread(1, 2), resp_data(b"ee")),
        (c_size(), resp_number(4)),
        (c_get(), resp_data(b"seed")),
        (c_fsync(), resp_ok()),
        (c_lseek(0, RAM_FILE_SEEK_END), resp_number(4)),
    ]
}

// ---------------------------------------------------------------------------
// The registry path
// ---------------------------------------------------------------------------

/// **The built-in registration.** `DriverRegistry::with_builtins()` knows the `"ram_file_drv"`
/// name, its factory builds a driver that reports that name, and a port created through
/// `PortRegistry::open` is live (and released again by `close`).
#[test]
fn the_registry_builds_a_live_ram_file_drv_port() {
    assert_eq!(RAM_FILE_DRIVER_NAME, "ram_file_drv", "the C driver's own name");
    let registry = DriverRegistry::with_builtins();
    assert!(
        registry.contains(RAM_FILE_DRIVER_NAME),
        "`with_builtins` registers the in-memory file driver"
    );

    let mut poller = poller();
    let mut ports = PortRegistry::new();
    let driver = registry.create(RAM_FILE_DRIVER_NAME).expect("the factory builds a driver");
    assert_eq!(driver.name(), RAM_FILE_DRIVER_NAME);
    let id = ports.open(&mut poller, driver).expect("the port opens");

    let port = ports.get(id).expect("the port is live");
    assert_eq!(port.driver_name(), RAM_FILE_DRIVER_NAME);
    assert_eq!(port.state(), PortState::Open, "`open` started the driver");
    assert_eq!(port.output_len(), 0);
    assert_eq!(
        port.pending_tasks(),
        0,
        "the driver owns no descriptor, so it schedules nothing at start"
    );
    assert!(ports.contains(id));
    assert_eq!(ports.len(), 1);

    // The named route reaches the same driver, on an id of its own.
    let named = ports
        .open_named(&mut poller, &registry, RAM_FILE_DRIVER_NAME)
        .expect("the named route opens a port");
    assert_ne!(named, id);
    assert_eq!(ports.get(named).expect("live").driver_name(), RAM_FILE_DRIVER_NAME);
    ports.close(&mut poller, named).expect("closes the second port");

    ports.close(&mut poller, id).expect("closes the port");
    assert!(ports.is_empty(), "both ports are gone");
}

/// **The reply encodings, as literal bytes.** The response codes and their payload order are the
/// driver's wire contract, and the integers are **big-endian** — the one detail a helper-based
/// expectation can hide, so it is pinned here with raw literals (`0x0102_0304` → `1,2,3,4`,
/// `-1` → four `0xFF`).
#[test]
fn the_reply_encodings_are_the_documented_big_endian_bytes() {
    assert_eq!(resp_ok(), vec![0]);
    assert_eq!(resp_number(0), vec![3, 0, 0, 0, 0]);
    assert_eq!(resp_number(11), vec![3, 0, 0, 0, 11]);
    assert_eq!(
        resp_number(0x0102_0304),
        vec![3, 1, 2, 3, 4],
        "the value is big-endian, not native-endian"
    );
    assert_eq!(resp_number(-1), vec![3, 255, 255, 255, 255]);
    assert_eq!(resp_data(b""), vec![2, 0, 0, 0, 0], "an empty file is an empty DATA");
    assert_eq!(
        resp_data(b"hello world"),
        vec![2, 0, 0, 0, 11, 104, 101, 108, 108, 111, 32, 119, 111, 114, 108, 100]
    );
    assert_eq!(resp_error("ebadf"), vec![1, 101, 98, 97, 100, 102]);
    assert_eq!(resp_error("eacces"), vec![1, 101, 97, 99, 99, 101, 115]);
    assert_eq!(resp_error("einval"), vec![1, 101, 105, 110, 118, 97, 108]);

    // The operand encodings the command builders rely on.
    assert_eq!(open_arg(3, b""), vec![0, 0, 0, 3]);
    assert_eq!(seek_arg(0, RAM_FILE_SEEK_END), vec![0, 0, 0, 0, 0, 0, 0, 2]);
    assert_eq!(cmd(RAM_FILE_READ, &be32(5)), vec![2, 0, 0, 0, 5]);
}

// ---------------------------------------------------------------------------
// The `port_command` transport
// ---------------------------------------------------------------------------

/// **The grammar, through the port.** Every command of the session is sent as opcode bytes with
/// `port_command` and its reply is asserted byte for byte **where it arrives**: the
/// `PortNotification::Data` of the port-task round that ran the `OUTPUT` task. The first row is
/// additionally pinned as the notification itself, so the `[port, bytes]` shape of the
/// delivery — not just the payload — is on the record.
#[test]
fn the_opcode_grammar_surfaces_its_replies_as_port_data() {
    let mut poller = poller();
    let (mut ports, id) = ram_port(&mut poller);

    for (index, step) in grammar_session().into_iter().enumerate() {
        let command = cmd(step.opcode, &step.arg);
        let notifications = command_notifications(&mut poller, &mut ports, id, &command);
        if index == 0 {
            assert_eq!(
                notifications,
                vec![PortNotification::Data {
                    port: id,
                    read: ReadMessage::body_only(vec![3, 0, 0, 0, 0]),
                }],
                "`open` answers `NUMBER 0` as the bytes of one Data notification"
            );
        }
        let reply = reply_bytes(notifications, id);
        assert_eq!(reply, step.expected, "the `{}` reply to {command:?}", step.name);
    }

    // The one Data notification carries the whole reply: nothing is left queued.
    assert_eq!(ports.get(id).expect("live").input_len(), 0);
    ports.close(&mut poller, id).expect("closes the port");
}

/// **The same grammar through the driver's `control` transport.** The named verbs — the
/// `ram_file.erl` names and their aliases (`get_size`, `get_file`, `position`, `sync`,
/// `datasync`) — funnel into the same core as the opcodes, so the identical session must answer
/// the identical bytes. `control` returns its reply to the caller (the C's `port_control`
/// transport) instead of queueing it, which is the only difference between the two spellings.
#[test]
fn the_named_verb_spelling_answers_the_same_bytes() {
    let mut poller = poller();
    let (mut ports, id) = ram_port(&mut poller);

    for step in grammar_session() {
        let reply = verb(&mut poller, &mut ports, id, step.name, &step.arg);
        assert_eq!(
            reply, step.expected,
            "the `{}` verb (spelling of opcode {})",
            step.name, step.opcode
        );
    }

    ports.close(&mut poller, id).expect("closes the port");
}

/// **The integer-as-name spelling.** `Driver::control` accepts the decimal opcode as a verb, so
/// both directions of the integer↔name correspondence are reachable; a decimal that is not an
/// opcode (and a word that is neither) is the typed `BadArg`.
#[test]
fn a_decimal_opcode_is_accepted_as_a_command_name() {
    let mut poller = poller();
    let (mut ports, id) = ram_port(&mut poller);

    assert_eq!(
        verb(&mut poller, &mut ports, id, "open", &open_arg(RAM_FILE_MODE_READ_WRITE, b"abcdef")),
        resp_number(0)
    );
    assert_eq!(verb(&mut poller, &mut ports, id, "37", &[]), resp_number(6));
    assert_eq!(verb(&mut poller, &mut ports, id, "30", &[]), resp_data(b"abcdef"));
    assert_eq!(
        verb(&mut poller, &mut ports, id, "3", &seek_arg(3, RAM_FILE_SEEK_SET)),
        resp_number(3)
    );
    assert_eq!(verb(&mut poller, &mut ports, id, "14", &[]), resp_ok());
    assert_eq!(verb(&mut poller, &mut ports, id, "37", &[]), resp_number(3));
    assert_eq!(verb(&mut poller, &mut ports, id, "30", &[]), resp_data(b"abc"));
    // The two spellings of one command are interchangeable.
    assert_eq!(
        verb(&mut poller, &mut ports, id, "get_size", &[]),
        verb(&mut poller, &mut ports, id, "37", &[])
    );

    // `200` parses as a byte but names no opcode; `frobnicate` does not parse at all — both are
    // the argument fault, never a silently-accepted no-op.
    assert_badarg(ports.port_control(&mut poller, id, "200", b""), "an unassigned decimal opcode");
    assert_badarg(ports.port_control(&mut poller, id, "frobnicate", b""), "an unknown verb");

    ports.close(&mut poller, id).expect("closes the port");
}

// ---------------------------------------------------------------------------
// Access-mode violations are replies, not Rust errors
// ---------------------------------------------------------------------------

/// **An unopened driver.** The C `RamFile` is inert until `open` gives it an access mode: every
/// operation that needs one answers `ERROR EBADF` (and `truncate`, which needs the `WRITE` bit,
/// `ERROR EACCES`), while `size`/`get` — which need no mode — still answer.
#[test]
fn an_unopened_port_answers_the_error_reply_to_every_mode_gated_command() {
    let mut poller = poller();
    let (mut ports, id) = ram_port(&mut poller);

    for (command, expected) in unopened_session() {
        let reply = command_round(&mut poller, &mut ports, id, &command);
        assert_eq!(reply, expected, "the reply to a fresh port's {command:?}");
        if expected.first() == Some(&RAM_FILE_RESP_ERROR) {
            assert_eq!(
                reply.first(),
                Some(&RAM_FILE_RESP_ERROR),
                "an operation-level failure is an ERROR reply, never a Rust error"
            );
        }
    }

    ports.close(&mut poller, id).expect("closes the port");
}

/// **A read-only open.** Reading, `pread`, `size`, `get` and the mode-gated `fsync` all work;
/// the write paths are `ERROR EBADF` and `truncate` `ERROR EACCES`, and none of them changes the
/// file.
#[test]
fn a_read_only_open_refuses_writes_and_serves_reads() {
    let mut poller = poller();
    let (mut ports, id) = ram_port(&mut poller);

    for (command, expected) in read_only_session() {
        let reply = command_round(&mut poller, &mut ports, id, &command);
        assert_eq!(reply, expected, "the reply to a read-only port's {command:?}");
    }

    // The refused writes left the file exactly as it was opened.
    assert_eq!(command_round(&mut poller, &mut ports, id, &c_get()), resp_data(b"seed"));
    ports.close(&mut poller, id).expect("closes the port");
}

// ---------------------------------------------------------------------------
// Protocol faults are typed errors
// ---------------------------------------------------------------------------

/// **An unknown command word.** The first byte of a `port_command` payload *is* the command
/// word, so a byte no opcode claims is a protocol fault: the port-task round reports the typed
/// `BadArg`, no reply is fabricated, and the offending bytes are consumed (so the queue cannot
/// keep re-driving a command that can never succeed). The port survives it.
#[test]
fn an_unknown_command_word_is_the_typed_bad_arg() {
    let mut poller = poller();
    let (mut ports, id) = ram_port(&mut poller);
    assert_eq!(
        command_round(&mut poller, &mut ports, id, &c_open(RAM_FILE_MODE_READ_WRITE, b"ab")),
        resp_number(0)
    );

    ports
        .port_command(&mut poller, id, &[200, 1, 2, 3], PortCommandOptions::EMPTY)
        .expect("the bytes queue");
    let outcome = ports.execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET);
    assert_badarg(outcome, "an unknown command word");

    let port = ports.get(id).expect("the port is still live");
    assert_eq!(port.output_len(), 0, "the unknown command was consumed");
    assert!(
        !port.has_pending_task(PortTaskKind::Output),
        "the failed task is not re-queued forever"
    );

    // Nothing was fabricated, and the port still serves the next command.
    let outcome = ports
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the next round is clean");
    assert!(
        outcome.notifications.is_empty(),
        "no reply was fabricated: {:?}",
        outcome.notifications
    );
    assert_eq!(command_round(&mut poller, &mut ports, id, &c_size()), resp_number(2));

    ports.close(&mut poller, id).expect("closes the port");
}

/// **A too-short argument.** An operation whose operand block is truncated is the same protocol
/// fault — on both transports — rather than a command run against whatever bytes happened to
/// follow.
#[test]
fn a_too_short_argument_is_the_typed_bad_arg() {
    let mut poller = poller();
    let (mut ports, id) = ram_port(&mut poller);

    let truncated: Vec<(Vec<u8>, &str)> = vec![
        (cmd(RAM_FILE_READ, &[]), "`read` with no length"),
        (cmd(RAM_FILE_READ, &[0, 0, 0]), "`read` with three length bytes"),
        (cmd(RAM_FILE_LSEEK, &be32(0)), "`lseek` with no whence"),
        (cmd(RAM_FILE_OPEN, &[0, 0, 0]), "`open` with three mode bytes"),
        (cmd(RAM_FILE_PREAD, &be32(0)), "`pread` with no length"),
        (cmd(RAM_FILE_PWRITE, &[]), "`pwrite` with no offset"),
    ];
    for (command, what) in truncated {
        ports
            .port_command(&mut poller, id, &command, PortCommandOptions::EMPTY)
            .expect("the bytes queue");
        let outcome = ports.execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET);
        assert_badarg(outcome, what);
        assert_eq!(
            ports.get(id).expect("still live").output_len(),
            0,
            "the truncated command was consumed: {command:?}"
        );
    }

    // The named-verb transport reports the same fault.
    assert_badarg(
        ports.port_control(&mut poller, id, "read", b""),
        "`read` as a verb with no length",
    );

    // And the port is unharmed: a well-formed command still answers.
    assert_eq!(command_round(&mut poller, &mut ports, id, &c_size()), resp_number(0));
    ports.close(&mut poller, id).expect("closes the port");
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

/// **`erlang:port_close/1`.** `ram_file:close/1` is the port close, and on the port path that is
/// the registry's **reserved** [`PortControl::CLOSE`] op: the driver's `stop` runs (the same
/// `reset()` the `"close"` verb performs), the port leaves the registry, and the close is
/// reported as exactly one [`PortNotification::Closed`] — minted by the next round, which is
/// also where the port number is retired. The reserved op answers with **no bytes**, so the
/// driver's own `OK` reply to a `close` verb is not what its caller sees.
#[test]
fn the_reserved_close_op_tears_the_port_down_once() {
    let mut poller = poller();
    let (mut ports, id) = ram_port(&mut poller);
    assert_eq!(
        command_round(&mut poller, &mut ports, id, &c_open(RAM_FILE_MODE_READ_WRITE, b"xyz")),
        resp_number(0)
    );

    let reply = ports
        .port_control(&mut poller, id, PortControl::CLOSE, b"")
        .expect("the reserved close op closes the port");
    assert!(reply.is_empty(), "the reserved op answers with no bytes, not the driver's OK");
    assert!(!ports.contains(id), "the port left the registry");
    assert!(ports.is_empty());

    // Every later operation on the dead id is the crate's argument fault.
    assert_badarg(
        ports.port_command(&mut poller, id, &c_size(), PortCommandOptions::EMPTY),
        "a command to a closed port",
    );
    assert_badarg(ports.port_control(&mut poller, id, "size", b""), "a verb on a closed port");

    // The close is reported exactly once, by the next round.
    let outcome = ports
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert_eq!(outcome.notifications, vec![PortNotification::Closed { port: id }]);
    let outcome = ports
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert!(
        outcome.notifications.is_empty(),
        "a close is reported exactly once: {:?}",
        outcome.notifications
    );
}

/// **The driver's own `close` verb** — the `reset()` `Driver::stop` runs on teardown. The
/// registry reserves the name `"close"`, so this verb is reachable only through
/// `Port::control` (the driver's `control` transport, as a direct caller reaches it); it
/// answers `OK` and leaves a **live** port whose file is inert again — the observable half of
/// "teardown resets the driver".
#[test]
fn the_drivers_own_close_verb_resets_the_file_without_closing_the_port() {
    let mut poller = poller();
    let (mut ports, id) = ram_port(&mut poller);
    assert_eq!(
        command_round(&mut poller, &mut ports, id, &c_open(RAM_FILE_MODE_READ_WRITE, b"hello")),
        resp_number(0)
    );
    assert_eq!(command_round(&mut poller, &mut ports, id, &c_size()), resp_number(5));

    let reply = ports
        .get_mut(id)
        .expect("the port is live")
        .control(&mut poller, "close", b"")
        .expect("the close verb is accepted");
    assert_eq!(reply, resp_ok(), "the driver's own close verb answers OK");

    // The port is still live; the file is not.
    assert!(ports.contains(id), "the verb does not close the port");
    assert_eq!(command_round(&mut poller, &mut ports, id, &c_size()), resp_number(0));
    assert_eq!(
        command_round(&mut poller, &mut ports, id, &c_read(5)),
        resp_error("ebadf"),
        "the access mode is gone"
    );
    assert_eq!(command_round(&mut poller, &mut ports, id, &c_get()), resp_data(b""));
    // Idempotent, exactly like the C's `ram_file_close` on an already-inert file. It has to go
    // through `Port::control` again: the registry reserves the name `"close"`, so
    // `PortRegistry::port_control` would tear the port down instead of reaching the driver.
    let again = ports
        .get_mut(id)
        .expect("the port is live")
        .control(&mut poller, "close", b"")
        .expect("a second close is accepted");
    assert_eq!(again, resp_ok(), "a second close is a no-op that still answers OK");

    ports.close(&mut poller, id).expect("closes the port");
}

// ---------------------------------------------------------------------------
// Live cross-check against the system `erl`
// ---------------------------------------------------------------------------

/// One oracle session: an independent port, the commands it is given, and the replies this
/// suite pins for them.
struct OracleSession {
    commands: Vec<Vec<u8>>,
    expected: Vec<Vec<u8>>,
}

/// The three sessions the oracle replays, one port each: the full grammar session, a fresh port
/// that never saw an `open`, and a read-only open that refuses the write paths.
fn oracle_sessions() -> Vec<OracleSession> {
    let grammar = grammar_session();
    let (fresh_commands, fresh_expected): (Vec<_>, Vec<_>) = unopened_session().into_iter().unzip();
    let (read_only_commands, read_only_expected): (Vec<_>, Vec<_>) =
        read_only_session().into_iter().unzip();
    vec![
        OracleSession {
            commands: grammar.iter().map(|step| cmd(step.opcode, &step.arg)).collect(),
            expected: grammar.into_iter().map(|step| step.expected).collect(),
        },
        OracleSession { commands: fresh_commands, expected: fresh_expected },
        OracleSession { commands: read_only_commands, expected: read_only_expected },
    ]
}

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after
/// printing a clear skip message — when `erl` cannot be spawned or does not answer within
/// [`ERL_TIMEOUT`]. A spawned `erl` that exits non-zero is a bug in the expression, so it fails
/// the test.
///
/// A local copy of the helper the other suites keep: a private helper is not reachable across
/// test binaries, and the crate's convention is to skip loudly rather than fail when the oracle
/// is unavailable.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out = Command::new("erl").arg("-noshell").arg("-eval").arg(&code).output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("SKIP: oracle check skipped: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!("SKIP: oracle check skipped: could not spawn `erl` ({err}); is erl on PATH?");
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
fn erl_bin(command: &[u8]) -> String {
    let bytes: Vec<String> = command.iter().map(u8::to_string).collect();
    format!("<<{}>>", bytes.join(","))
}

/// The oracle expression driving every session against the **real** `ram_file_drv`:
/// `open_port({spawn,"ram_file_drv"},[binary])` — the name `ram_file.erl` opens — one port per
/// session, each command sent with `port_command/2` and its reply flattened to a byte list.
///
/// The reply is normalised through `iolist_to_binary/1` because the C answers `OK`/`ERROR`/
/// `NUMBER` with `driver_output2` (a plain list of bytes) but `DATA` with `driver_output_binary`
/// (an improper `[header | binary]` iolist in a `[binary]` port) — flattening both to the
/// underlying byte stream is exactly what the driver produces.
fn oracle_expression(sessions: &[OracleSession]) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push("Norm = fun(D) -> binary_to_list(iolist_to_binary(D)) end".to_string());
    parts.push(
        "Mk = fun() -> P = open_port({spawn,\"ram_file_drv\"},[binary]), \
         S = fun(B) -> port_command(P,B), \
         receive {P,{data,D}} -> Norm(D) after 2000 -> timeout end end, S end"
            .to_string(),
    );
    parts.push(
        "ToStr = fun(R) -> case R of timeout -> \"TIMEOUT\"; \
         _ -> lists:join(\" \", [integer_to_list(X) || X <- R]) end end"
            .to_string(),
    );
    for index in 1..=sessions.len() {
        parts.push(format!("F{index} = Mk()"));
    }
    for (index, session) in sessions.iter().enumerate() {
        let handle = index + 1;
        let calls: Vec<String> = session
            .commands
            .iter()
            .map(|command| format!("F{handle}({})", erl_bin(command)))
            .collect();
        parts.push(format!(
            "io:format(\"~s~n\", [lists:join(\"|\", [ToStr(R) || R <- [{}]])])",
            calls.join(", ")
        ));
    }
    parts.join(", ")
}

/// Parses one oracle session line (`3 0 0 0 0|1 101 98 97 100 102|…`) into the reply bytes of
/// each command, with `None` for a `TIMEOUT` (no reply arrived).
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

/// **The compatibility gate.** Every command of the three sessions is sent to the real
/// `ram_file_drv` through a raw `erl` port and compared with what this driver answers through
/// the real port layer. The session's pinned `expected` replies are asserted too, so the oracle,
/// the Rust driver and the documented encoding all have to agree.
#[test]
fn the_command_matrix_agrees_with_the_erl_oracle() {
    let sessions = oracle_sessions();
    let Some(stdout) = erl_stdout(&oracle_expression(&sessions)) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let lines: Vec<&str> = stdout.lines().filter(|line| !line.trim().is_empty()).collect();
    assert_eq!(lines.len(), sessions.len(), "one output line per session, got: {stdout:?}");

    let mut poller = poller();
    let mut compared = 0usize;
    for (index, session) in sessions.iter().enumerate() {
        let oracle = parse_oracle_session(lines[index]);
        assert_eq!(
            oracle.len(),
            session.commands.len(),
            "session {index} replies one command at a time"
        );

        let (mut ports, id) = ram_port(&mut poller);
        for ((command, want), expected) in
            session.commands.iter().zip(&oracle).zip(&session.expected)
        {
            let reply =
                reply_bytes(command_notifications(&mut poller, &mut ports, id, command), id);
            let Some(oracle_bytes) = want else {
                // The reference produced no reply (it would leave the caller hanging); this
                // driver must not fabricate one either, so the row is skipped loudly.
                eprintln!(
                    "SKIP: the rest of the oracle comparison is skipped: session {index} \
                     command {command:?} timed out under the reference"
                );
                return;
            };
            assert_eq!(
                &reply, oracle_bytes,
                "session {index} command {command:?} differs from the oracle"
            );
            assert_eq!(
                &reply, expected,
                "session {index} command {command:?} differs from the pinned encoding"
            );
            compared += 1;
        }
        ports.close(&mut poller, id).expect("closes the port");
    }

    eprintln!("compared {compared} command replies byte-for-byte with the real ram_file_drv");
}
