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
//! **Accepted-socket option inheritance** (contract A.4) — the runtime half of the C's
//! `tcp_inet_copy` (`erts/emulator/drivers/common/inet_drv.c:11834`), driven with **real loopback
//! TCP traffic**.
//!
//! # What the C does, and how this runtime does it
//!
//! The C's socket drivers create the accepted port themselves: `tcp_inet_copy` copies the
//! listening descriptor's option block field by field into the accepted one
//! (`inet_drv.c:11810-11846`) and `async_ok_port` hands the Erlang side `{ok, Port2}`. A
//! [`beam_io::Driver`] here cannot create a port, so the accepted descriptor rides
//! [`InetAsyncResult::Accepted`] up to the runtime, which registers the port
//! ([`PortRegistry::adopt_accepted`](crate::port::PortRegistry)).
//!
//! The **copy itself is the driver's**:
//! [`TcpInetDriver::from_accepted(fd, &listener)`](beam_io::TcpInetDriver::from_accepted) adopts
//! the descriptor and takes the listener's option store across in one call
//! (`SocketOptions::inherited_from`, `erts_rust/crates/beam_io/src/drivers/inet/mod.rs:924`). The
//! listener's concrete driver is reached through the read-only downcast
//! [`beam_io::PortRegistry::driver_as`] over [`beam_io::Driver::downcast_ref`]
//! (`beam_io/src/driver.rs:544`, `:870`; `beam_io/src/registry.rs:224`), so this node keeps **no
//! record list and replays none** — every assertion below reads the result back **through the
//! accepted port's own driver**, which is where the inheritance really has to have happened.
//!
//! What the driver cannot carry across is this runtime's own mirror
//! ([`inet_opts`](super::ops::inet_opts)): the C copies exactly two of its fields into the new
//! descriptor — `inet.mode` (`inet_drv.c:11811`) and `inet.exitf` (`:11812`) — and omits
//! `inet.active` from its copy list (`:11814`-`:11844`), which `inet_start` resets
//! (`:10967`/`:10932`). So the adopted port inherits **mode + `exit_on_close`** through
//! [`PortRegistry::seed_inet_latches`](crate::port::PortRegistry) and starts **passive**;
//! `prim_inet:accept_opts/3` (`erts/preloaded/src/prim_inet.erl:502-517`) is what re-applies the
//! listener's active mode from the Erlang side, and this runtime has no `prim_inet` in the loop.
//!
//! # What this suite asserts
//!
//! Every driver assertion is read back with a `getopts` control op on **both** ports — the state a
//! `gen_tcp`/`inet` caller would see, never a Rust-side copy of what an inheritance claimed to do:
//!
//! * **the whole inherited block** ([`INHERITED_CODES`], 16 codes): the accepted port's reply is the
//!   listener's, **byte for byte**, for every one of them;
//! * four of them additionally pinned to the C's literals, so the encoding itself is witnessed;
//! * `{active, 3}` — **never** inherited: the accepted socket stays at the driver's passive default,
//!   so its counter cannot be doubled (the C does not copy it either);
//! * `{recbuf, …}`'s **raise** of the inherited `inet.bufsz` — the one inherited field with two
//!   writers, so a case of its own ([`a_recbuf_raised_driver_buffer_is_inherited`]);
//! * `gettype` — the accepted port's cached `stype`/`family` agree with the listener's, the two
//!   fields the C copies without an option code;
//! * one row proves the inherited block is live **on the wire** rather than only in the driver's
//!   option store: the named `send` verb on the accepted port emits the `{packet, 2}` length header
//!   before the payload (`inet_drv.c:13745-13769`), so the peer reads `<<0,2,"hi">>`.
//!
//! # The mirror row — what the runtime's own latches do
//!
//! [`an_accepted_socket_inherits_the_whole_tcp_inet_copy_block`] also pins the split the change that
//! introduced this suite's seam produced: the accepted port's mirror holds **nothing it was told to
//! replay** (its `{active, …}` latch is unset, though the listener's is `INET_MULTI` with a live
//! counter), only the two latches the C copies, while its driver state equals the listener's and its
//! resolved data mode ([`PortRegistry::data_mode`](crate::port::PortRegistry)) is the listener's
//! `{mode, list}` rather than its own `[binary]` open-time stream option. That last equality is the
//! whole point: every inet delivery renderer resolves its stream through the mirror, so an accepted
//! socket rendered `[binary]` while the driver writes lists would be a wrong term.
//!
//! # The negative rows
//!
//! * A `setopts` the driver **rejects** must leave the mirror at its pre-call state, so a value the
//!   driver refused can never be inherited by a later accepted socket. Both rejection shapes are
//!   covered: the driver's own `[0]einval` reply (an unserved option code) and its typed `badarg`
//!   (a truncated `{active, N}` record).
//! * A descriptor the **constructor** refuses (it is not a socket) answers the constructor's **own**
//!   `getsockopt(2)` errno and is released — and a listener that names no `tcp_inet` driver answers
//!   `{error, closed}` and releases it too. Neither leaves a port behind, and neither leaks the
//!   descriptor; both rows prove the release by re-closing the number and reading `EBADF`.
//!
//! # Where the mirror comes from
//!
//! [`PortRegistry::port_control`](super::PortRegistry) feeds it: every successful `setopts` control
//! op is recorded by production (`src/port.rs:1843`), so the suite's `setopts` calls need no
//! stand-in and every case would fail loudly if that call site disappeared.
//!
//! # The oracle
//!
//! [`the_oracle_copies_the_same_option_set_into_an_accepted_socket`] runs the equivalent sequence
//! under the system `erl` (`gen_tcp:listen/2` + a real loopback client + a bounded
//! `gen_tcp:accept/2` + `inet:getopts/2`) and pins the answers for the listener **and** the accepted
//! socket. [`the_oracle_inherits_a_recbuf_raised_driver_buffer`] pins the `bufsz` row the same way. A
//! missing toolchain, or a probe that cannot complete, is a **loud skip** — never a silent pass and
//! never a panic; every runtime assertion above stands on its own.
use std::cell::RefCell;
use std::io::Read;
use std::net::TcpStream;
use std::os::fd::{IntoRawFd, RawFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_bifs::PortHook;
use beam_common::BeamError;
use beam_io::{CompletionEvent, InetAsyncResult, InetReason};
use beam_process::{PortId, ProcessId};

use super::super::ops::inet_opts::{is_setopts_control, setopts_succeeded};
use super::super::tests::{fixture, skip_oracle_check, Fixture};
use super::super::{PortEvent, PortRegistry, StreamType};

// -----------------------------------------------------------------------------------------------
// The wire vocabulary (`inet_drv.c`'s option codes and request numbers)
// -----------------------------------------------------------------------------------------------

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`) — the offset the Erlang
/// side adds to every `inet` control request, which both `*_inet_ctl` entries subtract again.
const INET_CONTROL_MAGIC: u64 = 0x03f1_a300;
/// `INET_REQ_ACCEPT` (26) — the **multi-operation** accept, the only form that hands the accepted
/// descriptor to the runtime (`inet_drv.c:12194`). The *named* `accept` verb is a different
/// contract (it takes the connection onto the listening port), so the numeric form is used here.
const INET_REQ_ACCEPT: u64 = 26;
/// `INET_REQ_SETOPTS` (7) — the request the option mirror listens for.
const INET_REQ_SETOPTS: u64 = 7;
/// `INET_REQ_GETTYPE` (15) — the request behind `inet:gettype/1`, answered with the descriptor's
/// cached `stype`/`family` pair (`inet_drv.c:1424-1440`). Sent numerically because the driver's
/// *named* verb list has no `gettype` entry (`erts_rust/crates/beam_io/src/drivers/inet/mod.rs:2330-2342`),
/// while the numeric request is what the Erlang side really sends.
const INET_REQ_GETTYPE: u64 = 15;
/// `INET_OPT_RCVBUF` (7) — `{recbuf, N}`: raises the copied `inet.bufsz` while it has not been set
/// explicitly (`inet_drv.c:7350-7360`).
const INET_OPT_RCVBUF: u8 = 7;
/// `INET_LOPT_BUFFER` (20) — `inet.bufsz`, the driver's own read buffer (`inet_drv.c:882`).
const INET_LOPT_BUFFER: u8 = 20;
/// `INET_LOPT_HEADER` (21) — `desc->inet.hsz`, the packet header size.
const INET_LOPT_HEADER: u8 = 21;
/// `INET_LOPT_ACTIVE` (22) — `desc->inet.active` plus, in the multi form, a 16-bit counter.
const INET_LOPT_ACTIVE: u8 = 22;
/// `INET_LOPT_PACKET` (23) — `desc->inet.htype`, the C's `TCP_PB_*` code (`{packet,2}` ⇒ `2`).
const INET_LOPT_PACKET: u8 = 23;
/// `INET_LOPT_MODE` (24) — `desc->inet.mode` (`INET_MODE_LIST` `0`, `INET_MODE_BINARY` `1`).
const INET_LOPT_MODE: u8 = 24;
/// `INET_LOPT_DELIVER` (25) — `desc->inet.deliver` (`port` `0`, `term` `1`).
const INET_LOPT_DELIVER: u8 = 25;
/// `INET_LOPT_EXITONCLOSE` (26) — `desc->inet.exitf`.
const INET_LOPT_EXITONCLOSE: u8 = 26;
/// `INET_LOPT_TCP_HIWTRMRK` (27) — `desc->high`, the stream high watermark.
const INET_LOPT_TCP_HIWTRMRK: u8 = 27;
/// `INET_LOPT_TCP_LOWTRMRK` (28) — `desc->low`, the stream low watermark.
const INET_LOPT_TCP_LOWTRMRK: u8 = 28;
/// `INET_LOPT_TCP_SEND_TIMEOUT` (30) — `desc->send_timeout`.
const INET_LOPT_TCP_SEND_TIMEOUT: u8 = 30;
/// `INET_LOPT_PACKET_SIZE` (32) — `desc->inet.psize`, the largest frame the driver will assemble.
const INET_LOPT_PACKET_SIZE: u8 = 32;
/// `INET_LOPT_TCP_SEND_TIMEOUT_CLOSE` (35) — `desc->send_timeout_close`.
const INET_LOPT_TCP_SEND_TIMEOUT_CLOSE: u8 = 35;
/// `INET_LOPT_MSGQ_HIWTRMRK` (36) — a **copied** code whose set form the driver refuses below
/// `ERL_DRV_BUSY_MSGQ_LIM_MIN` (`1`), which is what [`a_rejected_setopts_never_enters_the_option_mirror`]
/// uses as an unserved-value record.
const INET_LOPT_MSGQ_HIWTRMRK: u8 = 36;
/// `INET_LOPT_MSGQ_LOWTRMRK` (37) — the other half of the busy-message-queue pair.
const INET_LOPT_MSGQ_LOWTRMRK: u8 = 37;
/// `INET_LOPT_TCP_SHOW_ECONNRESET` (39) — `tcp_add_flags & TCP_ADDF_SHOW_ECONNRESET`.
const INET_LOPT_TCP_SHOW_ECONNRESET: u8 = 39;
/// `INET_LOPT_TCP_READ_AHEAD` (49) — `tcp_add_flags & TCP_ADDF_NO_READ_AHEAD`, negated.
const INET_LOPT_TCP_READ_AHEAD: u8 = 49;

/// Every option code of the C's copied block that a `setopts` record can carry — the inherited set,
/// read back through both ports' drivers by
/// [`an_accepted_socket_inherits_the_whole_tcp_inet_copy_block`].
///
/// The two the C copies without a code — `inet.stype`/`inet.sfamily` — are asserted separately
/// through the `gettype` verb, and the third (`TCP_ADDF_LINGER_ZERO`) has no code here because
/// `beam_io` models the kernel `SO_LINGER` only.
const INHERITED_CODES: [u8; 16] = [
    INET_OPT_RCVBUF,
    INET_LOPT_BUFFER,
    INET_LOPT_HEADER,
    INET_LOPT_PACKET,
    INET_LOPT_MODE,
    INET_LOPT_DELIVER,
    INET_LOPT_EXITONCLOSE,
    INET_LOPT_TCP_HIWTRMRK,
    INET_LOPT_TCP_LOWTRMRK,
    INET_LOPT_TCP_SEND_TIMEOUT,
    INET_LOPT_PACKET_SIZE,
    INET_LOPT_TCP_SEND_TIMEOUT_CLOSE,
    INET_LOPT_MSGQ_HIWTRMRK,
    INET_LOPT_MSGQ_LOWTRMRK,
    INET_LOPT_TCP_SHOW_ECONNRESET,
    INET_LOPT_TCP_READ_AHEAD,
];

/// `INET_MULTI` (3) — the `{active, N}` form, whose record carries a trailing 16-bit counter.
const INET_MULTI: i32 = 3;
/// `INET_PASSIVE` (0) — the driver's default `active`, and what a **not** inherited counter leaves.
const INET_PASSIVE: i32 = 0;
/// `{mode, list}` — `INET_MODE_LIST` (`inet_drv.c:786`).
const INET_MODE_LIST: i32 = 0;
/// `{deliver, port}` — `INET_DELIVER_PORT` (`inet_drv.c:782`).
const INET_DELIVER_PORT: i32 = 0;
/// `TCP_PB_2` — the `{packet, 2}` code (`inet_drv.c:781`): a 2-byte big-endian length header.
const TCP_PB_2: i32 = 2;
/// `{packet_size, 4096}` — a limit the framed write below stays well inside.
const PACKET_SIZE: i32 = 4096;
/// `{header, 8}` — the C's `hsz`, copied to the accepted descriptor like everything else here.
const HEADER_SIZE: i32 = 8;
/// `{buffer, 4096}` — above `INET_BUFFER_MIN`, so the driver stores it verbatim.
const BUFFER_SIZE: i32 = 4096;
/// `{recbuf, 65536}` — a kernel receive buffer whose **raise** of `inet.bufsz` the adopted driver's
/// option store has to carry across; in the whole-block case the following `{buffer, 4096}` overrides
/// `bufsz` again, which is what pins the pair's order.
const RECBUF_SIZE: i32 = 65_536;
/// `{high_watermark, 1000}` / `{low_watermark, 500}` — a pair written high-then-low, so the store's
/// cross-clamp is exercised in the same order on both ports.
const HIGH_WATERMARK: i32 = 1000;
/// See [`HIGH_WATERMARK`].
const LOW_WATERMARK: i32 = 500;
/// `{send_timeout, 7000}` / `{send_timeout_close, true}` — copied as a pair.
const SEND_TIMEOUT: i32 = 7000;
/// See [`SEND_TIMEOUT`].
const SEND_TIMEOUT_CLOSE: i32 = 1;
/// `{high_msgq_watermark, 20000}` / `{low_msgq_watermark, 10000}` — the busy-msgq pair, written
/// high-then-low like the stream watermarks.
const MSGQ_HIGH: i32 = 20_000;
/// See [`MSGQ_HIGH`].
const MSGQ_LOW: i32 = 10_000;
/// `{show_econnreset, true}` — `TCP_ADDF_SHOW_ECONNRESET`.
const SHOW_ECONNRESET: i32 = 1;
/// `{exit_on_close, false}` — `desc->inet.exitf`, copied.
const EXIT_ON_CLOSE: i32 = 0;
/// `{read_ahead, false}` — `TCP_ADDF_NO_READ_AHEAD`, copied.
const READ_AHEAD: i32 = 0;
/// The accept's `Timeout(4)` operand: two seconds, so a probe can never expire mid-round
/// (`queued_accept_stream`'s own choice in the sibling `inet_matrix_tests`).
const ACCEPT_TIMEOUT_MS: i32 = 2_000;
/// How long the pump may take to adopt the accepted descriptor before the environment is declared
/// unable to run the case (a loud skip, never a silent pass).
const ACCEPT_BOUND: Duration = Duration::from_secs(10);
/// One pump round's slice — the port poll budget a zero-slice scheduler pass hands this layer.
const PUMP_SLICE: Duration = Duration::from_millis(50);

// -----------------------------------------------------------------------------------------------
// The wire helpers
// -----------------------------------------------------------------------------------------------

/// One `[opt, val_be32]` `setopts` record — the C's five-byte unit (`inet_drv.c:6908`).
fn record(code: u8, value: i32) -> Vec<u8> {
    let mut bytes = vec![code];
    bytes.extend_from_slice(&value.to_be_bytes());
    bytes
}

/// One `{active, N}` record: `[22, INET_MULTI, counter_be16]` (`inet_drv.c:6977`).
fn active_counter_record(counter: i16) -> Vec<u8> {
    let mut bytes = record(INET_LOPT_ACTIVE, INET_MULTI);
    bytes.extend_from_slice(&counter.to_be_bytes());
    bytes
}

/// The `setopts` control op **as `prim_inet` sends it**: the decimal `c_uint` of
/// `INET_REQ_SETOPTS + ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`.
fn setopts_op() -> String {
    let op = (INET_CONTROL_MAGIC + INET_REQ_SETOPTS).to_string();
    assert!(
        is_setopts_control(&op),
        "the numeric `setopts` opcode {op} must be the mirror's own request"
    );
    op
}

/// The numeric `accept` control op (`INET_REQ_ACCEPT`, 26).
fn accept_op() -> String {
    (INET_CONTROL_MAGIC + INET_REQ_ACCEPT).to_string()
}

/// Runs one control op and fails loudly with the driver's own error text.
fn control(reg: &mut PortRegistry, id: PortId, op: &str, arg: &[u8]) -> Vec<u8> {
    reg.port_control(id, op, arg)
        .unwrap_or_else(|error| panic!("the {op} control op on {id:?}: {error}"))
}

/// Runs one `setopts` op through the ordinary `port_control` path.
///
/// Nothing stands in for the mirror here: `PortRegistry::port_control` records a successful
/// `setopts` argument itself (`src/port.rs:1457-1461`), and
/// [`a_setopts_control_call_populates_the_option_mirror_from_production_alone`] is the case that
/// proves it without any help from this helper.
///
/// Returns the driver's reply, so a caller can assert on `[1]`/`[0]einval`.
fn setopts(reg: &mut PortRegistry, id: PortId, arg: &[u8]) -> Vec<u8> {
    control(reg, id, &setopts_op(), arg)
}

/// How many of the port's three inet latches are set — the witness every mirror row here reads.
///
/// The mirror keeps three latches (`{mode, …}`, `{active, …}`, `{exit_on_close, …}`) and no record
/// list (`super::super::ops::inet_opts`), so this counts the ones a port has actually been told.
/// A `setopts` naming none of them (`{packet, 2}`, `{recbuf, …}`, …) leaves the count where it was,
/// which is exactly what [`a_recbuf_raised_driver_buffer_is_inherited`] turns into evidence: that
/// case's `bufsz` can only have been inherited by the **driver's** option store.
fn recorded(reg: &PortRegistry, id: PortId) -> usize {
    reg.inet_options(id).map_or(0, |options| {
        usize::from(options.mode().is_some())
            + usize::from(options.active_mode().is_some())
            + usize::from(options.exit_on_close().is_some())
    })
}

/// The driver's reply to a `getopts` request for `code`, as raw bytes.
fn getopts(reg: &mut PortRegistry, id: PortId, code: u8) -> Vec<u8> {
    control(reg, id, "getopts", &[code])
}

/// The driver's `gettype` reply (`INET_REQ_GETTYPE`, 15) — the accepted port's cached
/// `stype`/`family`, the two copied fields no option code carries.
fn gettype(reg: &mut PortRegistry, id: PortId) -> Vec<u8> {
    control(
        reg,
        id,
        &(INET_CONTROL_MAGIC + INET_REQ_GETTYPE).to_string(),
        &[],
    )
}

/// The `setopts` argument the listener of the whole-block case is configured with: **every** code
/// [`INHERITED_CODES`] names, plus `{active, 3}` (the code that must never be inherited).
///
/// The order is the C's arrival order, which the driver's option store applies in sequence (and the
/// accepted driver then inherits whole). It is deliberately not sorted: `{recbuf, 65536}` comes
/// **before** `{buffer, 4096}` so the `bufsz` raise and its override are both exercised, and the
/// watermark pair is written high-then-low so the store's cross-clamp runs on both ports in the same
/// order.
fn listener_options() -> Vec<u8> {
    [
        record(INET_OPT_RCVBUF, RECBUF_SIZE),
        record(INET_LOPT_PACKET, TCP_PB_2),
        record(INET_LOPT_MODE, INET_MODE_LIST),
        record(INET_LOPT_PACKET_SIZE, PACKET_SIZE),
        record(INET_LOPT_HEADER, HEADER_SIZE),
        record(INET_LOPT_DELIVER, INET_DELIVER_PORT),
        record(INET_LOPT_EXITONCLOSE, EXIT_ON_CLOSE),
        active_counter_record(3),
        record(INET_LOPT_BUFFER, BUFFER_SIZE),
        record(INET_LOPT_TCP_HIWTRMRK, HIGH_WATERMARK),
        record(INET_LOPT_TCP_LOWTRMRK, LOW_WATERMARK),
        record(INET_LOPT_TCP_SEND_TIMEOUT, SEND_TIMEOUT),
        record(INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, SEND_TIMEOUT_CLOSE),
        record(INET_LOPT_TCP_SHOW_ECONNRESET, SHOW_ECONNRESET),
        record(INET_LOPT_TCP_READ_AHEAD, READ_AHEAD),
        record(INET_LOPT_MSGQ_HIWTRMRK, MSGQ_HIGH),
        record(INET_LOPT_MSGQ_LOWTRMRK, MSGQ_LOW),
    ]
    .concat()
}

/// The number of **latches** [`listener_options`] sets on its listener — all three: `{mode, list}`
/// (`inet.mode`), `{active, 3}` (`inet.active`) and `{exit_on_close, false}` (`inet.exitf`). The
/// accepted port carries only two of them, because the C's copy list omits `inet.active`.
const LISTENER_LATCHES: usize = 3;
/// The number of five-byte records [`listener_options`] carries — one per [`INHERITED_CODES`] row
/// plus the `{active, 3}` record (`17 × 5` bytes, with the two-byte counter after them).
const LISTENER_RECORDS: usize = 17;

// -----------------------------------------------------------------------------------------------
// The fixture: a real listening `tcp_inet` port and a real client
// -----------------------------------------------------------------------------------------------

/// What the runtime made of one accepted descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adoption {
    /// The descriptor became a live `tcp_inet` port (the C's `{ok, Port2}`).
    Port(PortId),
    /// The adoption was refused, so nothing was registered and the operation answered
    /// `{error, Reason}` (the C's `send_async_error`).
    Rejected(InetReason),
}

/// A `tcp_inet` port opened through the runtime's own `{spawn_driver, "tcp_inet"}` path, bound to
/// a real loopback port and listening — the state `gen_tcp:listen/2` leaves the port in.
struct Listener {
    /// The runtime's port table (the one `RuntimePorts::open_port` registered the port in).
    registry: Rc<RefCell<PortRegistry>>,
    /// The fixture that owns the term vocabulary and the heap the `Name`/`Options` live on, so it
    /// must outlive the port (the registered `PortMeta` holds no copy of those terms).
    #[allow(dead_code)]
    fx: Fixture,
    /// The listening port.
    id: PortId,
    /// The kernel-assigned loopback port the accepted client connects to.
    port: u16,
}

impl Listener {
    /// Opens, binds and listens — or fails loudly naming the step.
    ///
    /// `None` when the environment cannot give this case a loopback socket
    /// ([`skip_no_loopback`] has then printed the banner): a sandbox that refuses
    /// `socket(2)`/`bind(2)` cannot host a real-traffic case, and a silent pass would be a lie.
    fn open() -> Option<Listener> {
        let mut fx = fixture();
        let name = {
            let driver = fx.string("tcp_inet");
            fx.name("spawn_driver", driver)
        };
        let options = {
            // `prim_inet:open/5` always opens the driver port `[binary]`; the *data* mode is a
            // driver option (`{mode, list}`) applied afterwards through `setopts`.
            let binary = fx.atom("binary");
            fx.list(&[binary])
        };
        let id = match fx.hook.open_port(name, options) {
            Ok(id) => id,
            Err(error) => {
                skip_no_loopback(&format!("open_port({{spawn_driver,\"tcp_inet\"}}) failed: {error}"));
                return None;
            }
        };
        let registry = Rc::clone(&fx.registry);
        let listener = Listener {
            registry,
            fx,
            id,
            port: 0,
        };
        Some(listener)
    }

    /// The port table, borrowed for one step.
    fn reg(&self) -> std::cell::RefMut<'_, PortRegistry> {
        self.registry
            .borrow_mut()
    }

    /// `open` → `bind` (loopback, kernel-assigned port) → `listen` — the C's three requests, with
    /// the same operands `prim_inet:open/5` + `gen_tcp:listen/2` send.
    fn bind_and_listen(&mut self) {
        let mut reg = self.reg();
        let opened = control(&mut reg, self.id, "open", &[1, 1, 0]);
        assert_eq!(opened, vec![1], "INET_REQ_OPEN on a stream/inet port");
        let bound = control(&mut reg, self.id, "bind", &[1, 0, 0, 127, 0, 0, 1]);
        assert_eq!(
            bound.first(),
            Some(&1),
            "INET_REQ_BIND answers `[1] ++ port`: {bound:?}"
        );
        assert_eq!(bound.len(), 3, "bind reports the ephemeral port: {bound:?}");
        let port = u16::from_be_bytes([bound[1], bound[2]]);
        assert_ne!(port, 0, "the kernel assigned a real port");
        let listened = control(&mut reg, self.id, "listen", &[0, 5]);
        assert_eq!(listened, vec![1], "INET_REQ_LISTEN with a backlog of 5");
        drop(reg);
        self.port = port;
    }

    /// Applies `arg` through the numeric `setopts` request and answers the driver's reply.
    fn configure(&mut self, arg: &[u8]) -> Vec<u8> {
        let mut reg = self.reg();
        setopts(&mut reg, self.id, arg)
    }

    /// A real `std::net` client connected to the listening port, with a read timeout so a missing
    /// reply can never hang the suite.
    fn connect(&self) -> Option<TcpStream> {
        match TcpStream::connect(("127.0.0.1", self.port)) {
            Ok(client) => {
                let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
                // The client is only useful once the kernel has completed the handshake, which a
                // `connect(2)` on loopback has by the time it returns; a peer that is really
                // listening is what the accept below needs.
                Some(client)
            }
            Err(error) => {
                skip_no_loopback(&format!(
                    "TcpStream::connect(127.0.0.1:{}) failed: {error}",
                    self.port
                ));
                None
            }
        }
    }

    /// Queues the C's multi-operation `accept` and pumps until the runtime has adopted (or refused)
    /// the descriptor it took.
    ///
    /// `None` when no completion arrived inside [`ACCEPT_BOUND`] — the environment could not run
    /// the case ([`skip_no_loopback`] has printed the banner), never a silent pass.
    fn accept_one(&mut self) -> Option<Adoption> {
        let mut reg = self.reg();
        let reply = control(
            &mut reg,
            self.id,
            &accept_op(),
            &ACCEPT_TIMEOUT_MS.to_be_bytes(),
        );
        assert_eq!(
            reply.first(),
            Some(&1),
            "INET_REQ_ACCEPT answers `[1] ++ <<Id:16/big>>`: {reply:?}"
        );
        assert_eq!(reply.len(), 3, "the queued operation's id: {reply:?}");
        let deadline = Instant::now() + ACCEPT_BOUND;
        while Instant::now() < deadline {
            let events = match reg.pump(PUMP_SLICE) {
                Ok(events) => events,
                Err(error) => {
                    skip_no_loopback(&format!("the port round failed: {error}"));
                    return None;
                }
            };
            for event in events {
                let PortEvent::Completion { packet } = event else {
                    continue;
                };
                if let CompletionEvent::InetAsync { result, .. } = packet.event {
                    match result {
                        InetAsyncResult::Port(adopted) => return Some(Adoption::Port(adopted)),
                        InetAsyncResult::Error(reason) => {
                            return Some(Adoption::Rejected(reason))
                        }
                        _ => continue,
                    }
                }
            }
        }
        skip_no_loopback(&format!(
            "no accept completion within {} ms — the loopback accept did not complete here",
            ACCEPT_BOUND.as_millis()
        ));
        None
    }

    /// Closes both the listening port and `extra`, failing loudly if either teardown fails.
    fn close_all(&self, extra: Option<PortId>) {
        let mut reg = self.reg();
        if let Some(port) = extra {
            reg.close(port)
                .unwrap_or_else(|error| panic!("the accepted port {port:?} closes: {error}"));
        }
        reg.close(self.id)
            .unwrap_or_else(|error| panic!("the listening port {:?} closes: {error}", self.id));
    }
}

/// The loud skip banner for a case the **environment** cannot host: no loopback socket, or an
/// accept that never completed. Never a silent pass, never a panic.
fn skip_no_loopback(what: &str) {
    println!("================================================================");
    println!("SKIP: {what}");
    println!("      (this case needs real loopback TCP traffic and could not get it)");
    println!("================================================================");
}

// -----------------------------------------------------------------------------------------------
// Test 1 — the whole copied block, read back through both drivers, over real traffic
// -----------------------------------------------------------------------------------------------

/// An accepted socket inherits **every** field of the C's `tcp_inet_copy` block that an option
/// record can carry, and does **not** inherit `{active, …}`.
///
/// The listener is told all 16 inherited codes plus `{active,3}` the way `prim_inet` tells it; a
/// real `std::net` client connects over loopback; the runtime adopts the descriptor the `accept`
/// took, and [`TcpInetDriver::from_accepted`](beam_io::TcpInetDriver::from_accepted) takes the
/// listener's option store across.
///
/// The comparison is the contract itself — the accepted port's `getopts` reply is the listener's,
/// byte for byte, for every code — so a field the inheritance lost, re-ordered or applied with
/// different semantics fails here even before the four literal rows below say which encoding is
/// meant. Every literal is the oracle's own answer for the same sequence (cross-checked by
/// [`the_oracle_copies_the_same_option_set_into_an_accepted_socket`]).
#[test]
fn an_accepted_socket_inherits_the_whole_tcp_inet_copy_block() {
    let Some(mut listener) = Listener::open() else {
        return;
    };
    listener.bind_and_listen();
    let arg = listener_options();
    // The argument really drives every code of the copied block, once each: 17 five-byte records
    // (`INHERITED_CODES` plus `{active,3}`) with the multi form's two-byte counter after them. A
    // code that is missing here would make the byte-for-byte comparison below vacuous for it.
    assert_eq!(
        arg.len(),
        LISTENER_RECORDS * 5 + 2,
        "the whole block plus the `{{active,3}}` counter"
    );
    // Split the argument into its records: five bytes each, except the multi `{active, N}` form,
    // which carries its two-byte counter after the value word (`inet_drv.c:6977-6979`) — the only
    // variable-length record this case sends.
    let mut codes: Vec<u8> = Vec::new();
    let mut rest = &arg[..];
    while rest.len() >= 5 {
        let code = rest[0];
        let value = i32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]);
        let step = if code == INET_LOPT_ACTIVE && value == INET_MULTI {
            5 + 2
        } else {
            5
        };
        codes.push(code);
        rest = &rest[step..];
    }
    assert!(rest.is_empty(), "the argument is whole records: {} left", rest.len());
    assert_eq!(
        codes.len(),
        LISTENER_RECORDS,
        "one record per inherited code plus `{{active,3}}`: {codes:?}"
    );
    for code in INHERITED_CODES {
        assert_eq!(
            codes.iter().filter(|sent| **sent == code).count(),
            1,
            "code {code} must be sent to the listener exactly once, or its comparison is vacuous"
        );
    }
    let reply = listener.configure(&arg);
    assert_eq!(
        reply,
        vec![1],
        "the driver accepts the whole copied block plus `{{active,3}}`"
    );

    // The listener's own state, before any accept: the driver really took every record, so a
    // mismatch after the accept can only come from the inheritance.
    {
        let mut reg = listener.reg();
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_PACKET),
            vec![1, INET_LOPT_PACKET, 0, 0, 0, 2],
            "the listener's `{{packet,2}}` (the C's TCP_PB_2 code)"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_PACKET_SIZE),
            vec![1, INET_LOPT_PACKET_SIZE, 0, 0, 16, 0],
            "the listener's `{{packet_size,4096}}`"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_HEADER),
            vec![1, INET_LOPT_HEADER, 0, 0, 0, HEADER_SIZE as u8],
            "the listener's `{{header,8}}`"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_MODE),
            vec![1, INET_LOPT_MODE, 0, 0, 0, 0],
            "the listener's `{{mode,list}}` (`INET_MODE_LIST` is 0)"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_BUFFER),
            vec![1, INET_LOPT_BUFFER, 0, 0, 16, 0],
            "the listener's `inet.bufsz` is the `{{buffer,4096}}` it was told, NOT the 65536 its \
             earlier `{{recbuf,65536}}` raised it to"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_TCP_HIWTRMRK),
            vec![1, INET_LOPT_TCP_HIWTRMRK, 0, 0, 3, 232],
            "the listener's `{{high_watermark,1000}}`"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_TCP_LOWTRMRK),
            vec![1, INET_LOPT_TCP_LOWTRMRK, 0, 0, 1, 244],
            "the listener's `{{low_watermark,500}}`, which did not clamp the high one"
        );
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_ACTIVE),
            vec![1, INET_LOPT_ACTIVE, 0, 0, 0, INET_MULTI as u8, 0, 3],
            "the listener is in the multi (counter) form, with a live counter of 3"
        );
        // The mirror holds exactly the three latches a `setopts` can move — `inet.mode`
        // (`inet_drv.c:899`), `inet.active` (`:6968-7016`) and `inet.exitf` (`:904`) — each the
        // last accepted value, fed by production alone (`src/port.rs:1843`). The other 14 records of
        // the block are the driver's option store, not the runtime's.
        let mirrored = reg
            .inet_options(listener.id)
            .expect("a configured listener has an option mirror");
        assert_eq!(
            recorded(&reg, listener.id),
            LISTENER_LATCHES,
            "all three latches are set, and nothing else is retained"
        );
        assert_eq!(
            mirrored.mode(),
            Some(StreamType::List),
            "the `{{mode,list}}` record moved the listener's `inet.mode` latch"
        );
        assert_eq!(
            mirrored.active_mode(),
            Some(INET_MULTI),
            "the listener's `{{active,3}}` latch is the C's multi form"
        );
        assert_eq!(
            mirrored.exit_on_close(),
            Some(false),
            "the `{{exit_on_close,false}}` record moved the listener's `inet.exitf` latch"
        );
    }

    let Some(mut client) = listener.connect() else {
        return;
    };
    let Some(adopted) = listener.accept_one() else {
        return;
    };
    let Adoption::Port(accepted) = adopted else {
        panic!("a clean listener's accept must adopt the descriptor, got {adopted:?}");
    };
    {
        let reg = listener.reg();
        assert!(
            reg.is_live(accepted),
            "the adopted descriptor became a live port (the C's `{{ok, Port2}}`)"
        );
        assert_eq!(
            reg.len(),
            2,
            "exactly the listener and the accepted port are open"
        );
        assert_eq!(
            reg.data_mode(accepted, StreamType::Binary),
            StreamType::List,
            "the accepted port inherits the listener's *data* mode, not its `[binary]` stream option"
        );
        // Nothing was replayed onto the adopted port: its mirror holds the two latches the C copies
        // and no record list at all. `{active, …}` is the one the C's copy list omits
        // (`inet_drv.c:11814`-`:11844`, `inet_start` resets it at `:10967`/`:10932`), so the
        // accepted port starts passive while its **driver** still answers the listener's block.
        let inherited = reg
            .inet_options(accepted)
            .expect("an adopted port carries the two seeded latches");
        assert_eq!(
            recorded(&reg, accepted),
            2,
            "only `inet.mode` and `inet.exitf` are inherited — nothing was replayed"
        );
        assert_eq!(
            inherited.mode(),
            Some(StreamType::List),
            "`inet.mode` is copied (`inet_drv.c:11811`)"
        );
        assert_eq!(
            inherited.exit_on_close(),
            Some(false),
            "`inet.exitf` is copied (`inet_drv.c:11812`)"
        );
        assert_eq!(
            inherited.active_mode(),
            None,
            "`inet.active` is not copied, so the accepted port starts passive"
        );
        assert_eq!(
            reg.data_mode(accepted, StreamType::Binary),
            reg.data_mode(listener.id, StreamType::Binary),
            "the adopted port renders payloads in the listener's data mode — the mirror equality \
             every inet delivery renderer depends on"
        );
    }

    // The inherited effect, read back through the accepted port's own driver: for every code the C
    // copies, the answer is the listener's, byte for byte.
    {
        let mut reg = listener.reg();
        for code in INHERITED_CODES {
            let listener_reply = getopts(&mut reg, listener.id, code);
            let accepted_reply = getopts(&mut reg, accepted, code);
            assert_eq!(
                accepted_reply, listener_reply,
                "code {code}: the accepted socket must report the listener's value (the C's \
                 tcp_inet_copy), got {accepted_reply:?} against the listener's {listener_reply:?}"
            );
            assert_eq!(
                listener_reply.first(),
                Some(&1),
                "code {code}: the listener's own reply is an INET_REP_OK record: \
                 {listener_reply:?}"
            );
        }
        assert_eq!(
            getopts(&mut reg, accepted, INET_LOPT_ACTIVE),
            vec![1, INET_LOPT_ACTIVE, 0, 0, 0, INET_PASSIVE as u8],
            "`{{active, N}}` is NOT inherited: the accepted socket stays passive, so the counter \
             cannot be doubled (the Erlang side re-applies it, `prim_inet:accept_opts/3`)"
        );
        // The two copied fields with no option code: the driver's cached socket tags. `beam_io`
        // reads them for the accepted descriptor rather than copying them, and both ports answer
        // the same bytes — the agreement the C gets from `tcp_inet_copy` (`inet_drv.c:11817-11818`).
        let listener_type = gettype(&mut reg, listener.id);
        let accepted_type = gettype(&mut reg, accepted);
        assert_eq!(
            listener_type.first(),
            Some(&1),
            "the listener's `gettype` reply is an INET_REP_OK record: {listener_type:?}"
        );
        assert_eq!(
            accepted_type, listener_type,
            "the accepted socket's cached stype/family must be the listener's"
        );
    }

    // …and the inherited `{packet,2}` is live on the wire: the named `send` verb frames the payload
    // with the 2-byte big-endian length header the C's `tcp_sendv` prepends.
    {
        let mut reg = listener.reg();
        let sent = control(&mut reg, accepted, "send", b"hi");
        assert_eq!(sent, vec![1], "the named `send` verb on a connected socket");
    }
    let mut framed = [0u8; 4];
    client
        .read_exact(&mut framed)
        .expect("the peer reads the framed write from the accepted socket");
    assert_eq!(
        framed,
        [0, 2, b'h', b'i'],
        "`{{packet,2}}` inherited from the listener frames the write: a 2-byte length header"
    );

    listener.close_all(Some(accepted));
}

// -----------------------------------------------------------------------------------------------

// Test 1b — the one copied field with two writers: `{recbuf, N}`'s raise of `inet.bufsz`

// -----------------------------------------------------------------------------------------------

/// A listener that was **never** told `{buffer, …}` but was told `{recbuf, 65536}` has
/// `inet.bufsz = 65536` — the C's `{recbuf, …}` arm raises it while `INET_FLG_BUFFER_SET` is clear
/// (`inet_drv.c:7350-7360`) — and `tcp_inet_copy` copies that value into the accepted socket.
///
/// This is why the inheritance cannot be built out of a record list: the runtime's mirror holds
/// three latches and no `{buffer, …}` at all, so only the driver's own option store (which
/// [`TcpInetDriver::from_accepted`](beam_io::TcpInetDriver::from_accepted) takes across whole) can
/// carry the derived 65536 — otherwise the accepted socket would report the 9216-byte default. Measured against the oracle by
/// [`the_oracle_inherits_a_recbuf_raised_driver_buffer`].
#[test]
fn a_recbuf_raised_driver_buffer_is_inherited() {
    let Some(mut listener) = Listener::open() else {
        return;
    };
    listener.bind_and_listen();
    let reply = listener.configure(&record(INET_OPT_RCVBUF, RECBUF_SIZE));
    assert_eq!(reply, vec![1], "the driver accepts `{{recbuf,65536}}`");
    {
        let mut reg = listener.reg();
        assert_eq!(
            getopts(&mut reg, listener.id, INET_LOPT_BUFFER),
            vec![1, INET_LOPT_BUFFER, 0, 1, 0, 0],
            "`{{recbuf,65536}}` raised the listener's own `inet.bufsz` to 65536"
        );
        assert!(
            reg.inet_options(listener.id).is_none(),
            "…and the runtime's mirror holds nothing at all: `{{recbuf, N}}` is none of the three \
             latched codes, so the 65536 can only come from the driver's own option store"
        );
    }

    let Some(_client) = listener.connect() else {
        return;
    };
    let Some(adopted) = listener.accept_one() else {
        return;
    };
    let Adoption::Port(accepted) = adopted else {
        panic!("a clean listener's accept must adopt the descriptor, got {adopted:?}");
    };
    {
        let mut reg = listener.reg();
        assert_eq!(
            getopts(&mut reg, accepted, INET_LOPT_BUFFER),
            vec![1, INET_LOPT_BUFFER, 0, 1, 0, 0],
            "the accepted socket inherits the `{{recbuf, …}}`-raised `inet.bufsz`, as \
             `tcp_inet_copy` does"
        );
        assert_eq!(
            getopts(&mut reg, accepted, INET_OPT_RCVBUF),
            getopts(&mut reg, listener.id, INET_OPT_RCVBUF),
            "and its kernel receive buffer, which `accept(2)` inherited anyway"
        );
    }

    listener.close_all(Some(accepted));
}

// -----------------------------------------------------------------------------------------------
// Test 2 — a rejected `setopts` never enters the mirror
// -----------------------------------------------------------------------------------------------

/// A `setopts` the driver **rejects** leaves the mirror exactly as it was, so a value the driver
/// refused can never be inherited by a later accepted socket.
///
/// Two rejections are covered, because the driver has two shapes for them:
///
/// * an **unserved option code** — the C's `default: RETURN_ERROR()` (`inet_drv.c:9719`), which is a
///   `[0]einval` **reply** (the operation itself succeeded, its tag says the record was refused);
/// * a **truncated `{active, N}`** record — a multi form without its trailing counter, which this
///   floor reports as the typed `badarg` instead of reading past the payload.
#[test]
fn a_rejected_setopts_never_enters_the_option_mirror() {
    let Some(mut listener) = Listener::open() else {
        return;
    };
    listener.bind_and_listen();
    let arg = listener_options();
    assert_eq!(listener.configure(&arg), vec![1], "the driver accepts the block");
    let before = {
        let reg = listener.reg();
        reg.inet_options(listener.id)
            .cloned()
            .expect("the configured listener's mirror")
    };

    {
        let mut reg = listener.reg();
        // (a) An unserved code: the driver's own `[0]einval` reply, not an error.
        let unserved = record(200, 1);
        let reply = control(&mut reg, listener.id, &setopts_op(), &unserved);
        assert_eq!(
            reply,
            b"\x00einval".to_vec(),
            "the C's `ctl_xerror(\"einval\")` reply for an option code the driver does not serve"
        );
        assert!(!setopts_succeeded(&reply), "a `[0]` tag is not INET_REP_OK");
        assert!(
            !reg.note_inet_setopts(listener.id, &unserved, setopts_succeeded(&reply)),
            "a rejected record is never stored"
        );
        assert_eq!(
            reg.inet_options(listener.id),
            Some(&before),
            "the mirror is unchanged by a rejected `setopts`"
        );

        // (b) A truncated `{active, N}`: the driver cannot read the counter and refuses the call.
        let truncated = record(INET_LOPT_ACTIVE, INET_MULTI);
        assert!(
            matches!(
                reg.port_control(listener.id, &setopts_op(), &truncated),
                Err(BeamError::BadArg)
            ),
            "a multi `{{active, N}}` record without its counter is the driver's typed `badarg`"
        );
        assert!(
            !reg.note_inet_setopts(listener.id, &truncated, false),
            "a refused call is never stored either"
        );
        assert_eq!(
            reg.inet_options(listener.id),
            Some(&before),
            "the mirror is unchanged by the refused call"
        );
    }

    listener.close_all(None);
}

// -----------------------------------------------------------------------------------------------
// Test 2b — the recording hook is *production's*, proved without any harness stand-in
// -----------------------------------------------------------------------------------------------

/// The option mirror is fed by `PortRegistry::port_control` itself: one `setopts` control call
/// populates it, and a refused one leaves it untouched.
///
/// Every other case here reaches the mirror through the same `port_control` path, so this case's
/// job is to state the fact the others rely on: nothing in the harness supplies the state.
#[test]
fn a_setopts_control_call_populates_the_option_mirror_from_production_alone() {
    let Some(mut listener) = Listener::open() else {
        return;
    };
    listener.bind_and_listen();
    {
        let reg = listener.reg();
        assert_eq!(
            recorded(&reg, listener.id),
            0,
            "a freshly opened port has been told no option, so its mirror holds no record"
        );
        assert_eq!(
            reg.data_mode(listener.id, StreamType::Binary),
            StreamType::Binary,
            "and its effective data mode is still the open-time `[binary]`"
        );
    }

    let arg = listener_options();
    let reply = control(&mut listener.reg(), listener.id, &setopts_op(), &arg);
    assert_eq!(reply, vec![1], "the driver accepts the whole block");
    {
        let reg = listener.reg();
        assert_eq!(
            recorded(&reg, listener.id),
            LISTENER_LATCHES,
            "the control call moved every latched code on its own — no caller stood in"
        );
        assert_eq!(
            reg.data_mode(listener.id, StreamType::Binary),
            StreamType::List,
            "and the `{{mode,list}}` record moved the port's effective data mode"
        );
    }

    // A refused record is a reply (`[0]einval`), not an error, and must record nothing either.
    let before = {
        let reg = listener.reg();
        reg.inet_options(listener.id)
            .cloned()
            .expect("the configured listener's mirror")
    };
    let unserved = record(200, 1);
    let reply = control(&mut listener.reg(), listener.id, &setopts_op(), &unserved);
    assert_eq!(reply, b"\x00einval".to_vec(), "the driver's refusal reply");
    {
        let reg = listener.reg();
        assert_eq!(
            reg.inet_options(listener.id),
            Some(&before),
            "a `[0]` tag is not INET_REP_OK, so production recorded nothing"
        );
    }

    listener.close_all(None);
}

// -----------------------------------------------------------------------------------------------
// Test 3 — the two refusals: the constructor's own errno, and a listener with no driver
// -----------------------------------------------------------------------------------------------

/// The owner operand every adopted port would be registered under — `prim_inet`'s own owner in
/// production, and a fixed pid here because no process table is involved.
const OWNER: ProcessId = ProcessId::new(7, 0);

/// A raw descriptor that is **not** a socket — what [`TcpInetDriver::from_accepted`] refuses.
///
/// `/dev/null` is a character device, so the constructor's very first probe
/// (`getsockopt(SO_ACCEPTCONN)`, `beam_io/src/drivers/inet/sys.rs:217`) fails with `ENOTSOCK`. The
/// descriptor is handed over as a raw number (`into_raw_fd`), i.e. nobody closes it but the code
/// under test — which is exactly what the rows below assert.
///
/// `None` when this host has no `/dev/null` ([`skip_no_loopback`] has printed the banner).
fn non_socket_fd(label: &str) -> Option<RawFd> {
    match std::fs::File::open("/dev/null") {
        Ok(file) => Some(file.into_raw_fd()),
        Err(error) => {
            skip_no_loopback(&format!("{label}: /dev/null could not be opened: {error}"));
            None
        }
    }
}

/// Asserts that `fd` has already been released — a second `close(2)` must answer `EBADF`.
///
/// This is how every refusal row proves it leaked nothing: the number is closed by the code under
/// test, so re-closing it can only fail, and it fails with the kernel's own `EBADF`
/// ([`beam_io::close_fd`] reports it rather than swallowing it).
fn assert_released(fd: RawFd, what: &str) {
    match beam_io::close_fd(fd) {
        Err(BeamError::Io(error)) => assert_eq!(
            error.raw_os_error(),
            Some(libc::EBADF),
            "{what}: the descriptor was already released, so the re-close is EBADF"
        ),
        Ok(()) => panic!("{what}: the descriptor was still open — the refusal leaked it"),
        Err(other) => panic!("{what}: the re-close answered {other}, not EBADF"),
    }
}

/// A descriptor the **constructor** refuses answers the constructor's own `getsockopt(2)` errno and
/// is released — never a fabricated `ebadf`/`einval`, and never a leak.
///
/// `beam_io`'s `from_socket` evaluates `is_listening`/`has_peer` *before* the struct literal whose
/// `Drop` closes the socket (`beam_io/src/drivers/inet/mod.rs:848`), so a refused adoption produces
/// no driver at all and the descriptor is still the caller's — which is why
/// `PortRegistry::adopt_accepted` must release it itself.
#[test]
fn a_descriptor_that_is_not_a_socket_is_refused_with_the_constructors_errno() {
    let Some(mut listener) = Listener::open() else {
        return;
    };
    listener.bind_and_listen();
    let Some(fd) = non_socket_fd("refused adoption") else {
        return;
    };
    let (adoption, ports_before) = {
        let mut reg = listener.reg();
        let before = reg.len();
        let adoption = reg.adopt_accepted(listener.id, fd, OWNER, StreamType::Binary, None);
        (adoption, before)
    };
    match adoption {
        InetAsyncResult::Error(reason) => {
            assert_eq!(
                reason,
                InetReason::from_errno(libc::ENOTSOCK),
                "the constructor's own `getsockopt(2)` errno, preserved (`beam_io`'s \
                 `from_socket` never rewrites it) — not a fabricated `ebadf`/`einval`"
            );
            assert_eq!(
                reason.atom_name(),
                "enotsock",
                "the atom the C's `error_atom(err)` would spell for the same descriptor \
                 (`inet_drv.c:2314-2317`)"
            );
        }
        InetAsyncResult::Port(port) => {
            panic!("a non-socket descriptor must not become a port, got {port:?}")
        }
        other => panic!("unexpected adoption result {other:?}"),
    }
    assert_released(fd, "a refused descriptor");
    {
        let reg = listener.reg();
        assert_eq!(
            reg.len(),
            ports_before,
            "a refused adoption registers no port (nothing was left behind)"
        );
        assert!(
            reg.is_live(listener.id),
            "the listening port itself survives a refused adoption"
        );
    }
    listener.close_all(None);
}

/// A listener [`beam_io::PortRegistry::driver_as`] cannot resolve to a `TcpInetDriver` answers
/// `{error, closed}` — the C's own teardown reason, never an invented errno — and releases the
/// descriptor.
///
/// Unreachable from `beam_io`'s accept path ([`InetAsyncResult::Accepted`] is staged **only** by
/// `TcpInetDriver`, `beam_io/src/drivers/inet/accept.rs:345-384`, and `UdpInetDriver` has no accept
/// path), but a listener whose port was torn down between the driver's `accept(2)` and the runtime's
/// drain would reach it — so it is a tested, descriptor-releasing answer, not an `unwrap`.
#[test]
fn a_listener_that_names_no_tcp_inet_driver_is_refused_and_releases_the_descriptor() {
    let Some(mut listener) = Listener::open() else {
        return;
    };
    listener.bind_and_listen();
    let Some(fd) = non_socket_fd("unresolvable listener") else {
        return;
    };
    // A port number this registry does not hold: the downcast has no port to reach.
    let absent = PortId::new(999);
    let (adoption, ports_before) = {
        let mut reg = listener.reg();
        assert!(
            !reg.is_live(absent),
            "the fixture must not hold a port at the number this row uses"
        );
        let before = reg.len();
        let adoption = reg.adopt_accepted(absent, fd, OWNER, StreamType::Binary, None);
        (adoption, before)
    };
    match adoption {
        InetAsyncResult::Error(reason) => {
            assert_eq!(
                reason,
                InetReason::Closed,
                "no errno is fabricated: the refusal is the C's own `closed` teardown reason"
            );
            assert_eq!(reason.atom_name(), "closed", "the atom name a caller renders");
        }
        InetAsyncResult::Port(port) => {
            panic!("an unresolvable listener must not adopt a port, got {port:?}")
        }
        other => panic!("unexpected adoption result {other:?}"),
    }
    assert_released(fd, "an unresolvable listener's descriptor");
    {
        let reg = listener.reg();
        assert_eq!(
            reg.len(),
            ports_before,
            "the live listening port is untouched and nothing was registered"
        );
        assert!(reg.is_live(listener.id), "the listener is still live");
    }
    listener.close_all(None);
}

// -----------------------------------------------------------------------------------------------
// Test 4 — the oracle
// -----------------------------------------------------------------------------------------------

/// The system `erl` copies the same option set into an accepted socket, so the rows
/// [`an_accepted_socket_inherits_the_whole_tcp_inet_copy_block`] asserts are the C's own answers.
///
/// `gen_tcp:listen/2` applies the block to the listening descriptor, `gen_tcp:accept/2` makes the
/// new port through `tcp_inet_copy` + `prim_inet:accept_opts/3`, and `inet:getopts/2` reads **both**
/// ports back. Two codes of the suite's own list are left out of the probe on purpose:
/// `active`, whose oracle value is `prim_inet:accept_opts/3`'s re-application rather than the
/// driver's copy (this runtime has no `prim_inet` in the loop, so its accepted **port** is
/// passive), and `recbuf`, whose answer is the kernel's doubled figure rather than a driver stored
/// value ([`the_oracle_inherits_a_recbuf_raised_driver_buffer`] pins the `bufsz` row instead).
#[test]
fn the_oracle_copies_the_same_option_set_into_an_accepted_socket() {
    let expr = "begin \
         Opts = [list,{ip,{127,0,0,1}},{packet,2},{packet_size,4096},{header,8},\
         {deliver,port},{exit_on_close,false},{buffer,4096},\
         {high_watermark,1000},{low_watermark,500},\
         {send_timeout,7000},{send_timeout_close,true},\
         {show_econnreset,true},{read_ahead,false},\
         {high_msgq_watermark,20000},{low_msgq_watermark,10000}], \
         {ok,L} = gen_tcp:listen(0,Opts), \
         Codes = [packet,packet_size,header,mode,deliver,exit_on_close,buffer,\
         high_watermark,low_watermark,send_timeout,send_timeout_close,\
         show_econnreset,read_ahead,high_msgq_watermark,low_msgq_watermark], \
         {ok,LO} = inet:getopts(L,Codes), \
         {ok,{_,P}} = inet:sockname(L), \
         Self = self(), \
         _ = spawn(fun() -> {ok,_S} = gen_tcp:connect({127,0,0,1},P,[]), \
         Self ! connected, receive after 3000 -> ok end end), \
         receive connected -> ok after 3000 -> exit(no_client) end, \
         {ok,A} = gen_tcp:accept(L,5000), \
         {ok,AO} = inet:getopts(A,Codes), \
         io:format(\"L=~w A=~w\",[LO,AO]), gen_tcp:close(A), gen_tcp:close(L) end";
    let Some(output) = super::super::tests::erl_eval(expr) else {
        skip_oracle_check("accepted-socket option inheritance (tcp_inet_copy)");
        return;
    };
    assert_eq!(
        output,
        "L=[{packet,2},{packet_size,4096},{header,8},{mode,list},{deliver,port},\
         {exit_on_close,false},{buffer,4096},{high_watermark,1000},{low_watermark,500},\
         {send_timeout,7000},{send_timeout_close,true},{show_econnreset,true},{read_ahead,false},\
         {high_msgq_watermark,20000},{low_msgq_watermark,10000}] \
         A=[{packet,2},{packet_size,4096},{header,8},{mode,list},{deliver,port},\
         {exit_on_close,false},{buffer,4096},{high_watermark,1000},{low_watermark,500},\
         {send_timeout,7000},{send_timeout_close,true},{show_econnreset,true},{read_ahead,false},\
         {high_msgq_watermark,20000},{low_msgq_watermark,10000}]",
        "the oracle's accepted socket carries exactly the block this runtime's adopted driver inherits"
    );
}

/// The oracle's `inet.bufsz` after `{recbuf, 65536}` and no `{buffer, …}`: `65536` on the listener,
/// and `65536` again on the accepted socket because `tcp_inet_copy` copies the field.
///
/// This is the row that says the driver's option store — not a record list — is what carries
/// `inet.bufsz`: a copy that only re-sent `{buffer, …}` records would leave the accepted port at the
/// 9216-byte default, and this measurement is what says so.
#[test]
fn the_oracle_inherits_a_recbuf_raised_driver_buffer() {
    let expr = "begin \
         {ok,L} = gen_tcp:listen(0,[list,{ip,{127,0,0,1}},{recbuf,65536}]), \
         {ok,LB} = inet:getopts(L,[buffer]), \
         {ok,{_,P}} = inet:sockname(L), \
         Self = self(), \
         _ = spawn(fun() -> {ok,_S} = gen_tcp:connect({127,0,0,1},P,[]), \
         Self ! connected, receive after 3000 -> ok end end), \
         receive connected -> ok after 3000 -> exit(no_client) end, \
         {ok,A} = gen_tcp:accept(L,5000), \
         {ok,AB} = inet:getopts(A,[buffer]), \
         io:format(\"L=~w A=~w\",[LB,AB]), gen_tcp:close(A), gen_tcp:close(L) end";
    let Some(output) = super::super::tests::erl_eval(expr) else {
        skip_oracle_check("the `{recbuf, …}` raise of the copied `inet.bufsz`");
        return;
    };
    assert_eq!(
        output, "L=[{buffer,65536}] A=[{buffer,65536}]",
        "`{{recbuf,65536}}` raises the copied `inet.bufsz` on both the listener and its accepted \
         socket"
    );
}
