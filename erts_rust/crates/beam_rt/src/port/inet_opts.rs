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

//! The per-port **inet option mirror** — what an `inet` port has been told through
//! `INET_REQ_SETOPTS`, and what an **accepted** socket therefore inherits.
//!
//! # Why a mirror exists at all
//!
//! `prim_inet:open/6` opens the driver port with **one** option, `[binary]`
//! (`erts/preloaded/src/prim_inet.erl:99-137`), and applies every other socket option afterwards
//! through `setopts/2`, which is an `erlang:port_control/3` call carrying the C's
//! `[opt_code, val_be32]` records. So neither the port's `open_port/2` options nor
//! [`PortMeta::stream`](super::PortMeta) know what the *driver* has been told: a
//! `{mode, list}` socket is a `[binary]` port whose driver holds `INET_MODE_LIST`.
//!
//! The C's socket drivers do not have this problem: an `accept` creates the new port from the
//! listening socket's own descriptor (`tcp_inet_copy`, `erts/emulator/drivers/common/inet_drv.c:11834`)
//! and copies the option block field by field. This crate's drivers cannot create a port
//! ([`InetAsyncResult::Accepted`](beam_io::InetAsyncResult) hands the bare descriptor to the runtime,
//! which opens the port — [`PortRegistry::adopt_accepted`](super::PortRegistry)), so the inherited
//! block has to be **replayed** onto the new port through the driver's own `setopts` surface.
//! That is what this module records and replays.
//!
//! # What is mirrored
//!
//! Per port, keyed by **port number** (the [`PortExtras`](super::ops::PortExtras) key), the mirror
//! holds
//!
//! * every **successful** `setopts` argument verbatim and in arrival order
//!   ([`PortInetOptions::records`]) — the replay source, and
//! * the driver's **current** `{mode, list|binary}` ([`PortInetOptions::mode`], option code
//!   [`INET_LOPT_MODE`]), which is the port's *data* mode and **not** the same thing as
//!   [`PortMeta::stream`](super::PortMeta) (see [`PortRegistry::data_mode`]).
//!
//! Recording is one entry point ([`PortRegistry::note_inet_setopts`]) called from
//! [`PortRegistry::port_control`](super::PortRegistry) once the driver has answered, so a rejected
//! `setopts` can never be inherited: only a reply tagged `INET_REP_OK` is recorded.
//!
//! # Which records are replayed
//!
//! Replay is **not** "everything the listener was told": it is exactly the set the C's
//! `tcp_inet_copy` copies into the accepted descriptor (`inet_drv.c:11810-11827`), mapped onto the
//! option codes that set them — the packet type, the packet size, the header size, the mode, the
//! `deliver` flag, `exit_on_close`, the buffer size, the two TCP watermarks, the send-timeout pair,
//! the busy-message-queue pair and the three `tcp_add_flags` bits (`show_econnreset`,
//! `read_ahead`, `linger_zero`). [`is_inherited_record`] is that table.
//!
//! Two families are deliberately **not** replayed, each for a reason the C itself gives:
//!
//! * **`{active, …}`** — `tcp_inet_copy` does not copy `active`/`active_count` at all, and the
//!   Erlang side re-applies it: `prim_inet:accept_opts/3`
//!   (`erts/preloaded/src/prim_inet.erl:502-517`) reads `active` off the *listener* and
//!   `setopts`es it onto the accepted socket. Replaying it here as well would **double-arm a
//!   counter**, because the C's `INET_LOPT_ACTIVE` arm *adds* its operand to the descriptor's
//!   existing `active_count` (`inet_drv.c:6968-7000`): a `{active, 3}` listener would leave the
//!   accepted socket at 6 instead of the oracle's 3. So for `active` the Erlang `setopts` is
//!   authoritative and the mirror only *records* it.
//! * the **kernel-backed** options (`nodelay`, `keepalive`, `linger`, `reuseaddr`, `priority`,
//!   `recvbuf`/`sendbuf`, …) — the C does not copy them to the new descriptor either. They do
//!   not need replaying: `accept(2)` gives the new descriptor the listener's own `SO_*` state,
//!   and `prim_inet:accept_opts/3` re-applies six of them besides.
//!
//! `{line_delim, …}` is not replayed for the same reason the C does not copy `desc->delimiter`:
//! the accepted socket starts with the default delimiter.
//!
//! # The replay's failure rule
//!
//! A replay record the driver **rejects** (the C's `[0]einval`) cannot be ignored: half a
//! configuration is a socket whose `{packet, N}`/`{mode, …}` disagree with the listener's, and a
//! silent half-inheritance is exactly the class of bug this module exists to prevent. So
//! [`PortRegistry::inherit_inet_options`] stops at the first rejected record, the caller
//! ([`adopt_accepted`](super::PortRegistry)) **closes** the freshly adopted port (the descriptor is
//! owned by that port, so the close releases it — nothing leaks) and the accept operation completes
//! as `{error, einval}`, the errno the driver itself answers for a bad option value. The C cannot
//! fail here because it copies a struct; a *replay* can, and a typed `{error, einval}` from
//! `gen_tcp:accept/1` is the honest report.
//!
//! [`InetAsyncResult::Accepted`]: beam_io::InetAsyncResult::Accepted

use std::collections::HashMap;

use beam_common::{BeamError, Result};
use beam_process::PortId;

use crate::port::{PortRegistry, StreamType};

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`) — the offset the Erlang
/// side adds to every `inet` control request and both `*_inet_ctl` entries subtract again.
const INET_CONTROL_MAGIC: u64 = 0x03f1_a300;

/// `INET_REQ_SETOPTS` (7) — the request the mirror listens for (`inet_drv.c:812`).
const INET_REQ_SETOPTS: u64 = 7;

/// The **named** verb the same request reaches a driver as (the runtime forwards either the decimal
/// `c_uint` string or the driver's own verb; `beam_io` decodes both onto one arm).
const SETOPTS_VERB: &str = "setopts";

/// `INET_REP_OK` — the leading tag of a control reply that succeeded (`inet_drv.c:870`,
/// `beam_io/src/drivers/inet/wire.rs:44`).
const INET_REP_OK: u8 = 1;

/// The bytes of one `[opt, val_be32]` record — the C's `while (len >= 5)` unit
/// (`inet_set_opts`, `inet_drv.c:6908`).
const RECORD: usize = 5;

/// The extra bytes an `{active, N}` record carries after `[opt, val_be32]` (the big-endian 16-bit
/// counter, `Sint16`, `inet_drv.c:1256`).
///
/// This walker only needs the length — `{active, N}` records are recorded but never replayed — but
/// it still has to step over them correctly.
const ACTIVE_COUNTER: usize = 2;

// ---------------------------------------------------------------------------
// The option codes the mirror names
// ---------------------------------------------------------------------------
//
// `beam_io` keeps its option vocabulary in a private `opt` submodule and re-exports only part of
// it, so the codes this module needs are restated here with the C they come from. They are
// **hand-maintained wire constants** (`inet_drv.c:874-930`), not derived values.

/// `INET_LOPT_BUFFER` (20) — the driver's own staging buffer (`inet_drv.c:882`).
const INET_LOPT_BUFFER: u8 = 20;
/// `INET_LOPT_HEADER` (21) — the packet header size (`inet_drv.c:886`).
const INET_LOPT_HEADER: u8 = 21;
/// `INET_LOPT_ACTIVE` (22) — `{active, false|true|once|N}` (`inet_drv.c:890`). Recorded, never
/// replayed (see the module docs).
const INET_LOPT_ACTIVE: u8 = 22;
/// `INET_LOPT_PACKET` (23) — the `{packet, …}` type, the C's `inet.htype` (`inet_drv.c:898`).
const INET_LOPT_PACKET: u8 = 23;
/// `INET_LOPT_MODE` (24) — `{mode, list|binary}`, the C's `inet.mode` (`inet_drv.c:899`).
///
/// `beam_io` declares the same number privately (`drivers/inet/opt.rs:193`) but does not re-export
/// it, so the mirror restates it.
const INET_LOPT_MODE: u8 = 24;
/// `INET_LOPT_DELIVER` (25) — the `{deliver, port|term}` flag (`inet_drv.c:902`).
const INET_LOPT_DELIVER: u8 = 25;
/// `INET_LOPT_EXITONCLOSE` (26) — the C's `inet.exitf` (`inet_drv.c:904`).
const INET_LOPT_EXITONCLOSE: u8 = 26;
/// `INET_LOPT_TCP_HIWTRMRK` (27) — the stream high watermark (`inet_drv.c:906`).
const INET_LOPT_TCP_HIWTRMRK: u8 = 27;
/// `INET_LOPT_TCP_LOWTRMRK` (28) — the stream low watermark (`inet_drv.c:907`).
const INET_LOPT_TCP_LOWTRMRK: u8 = 28;
/// `INET_LOPT_TCP_SEND_TIMEOUT` (30) — `{send_timeout, …}` (`inet_drv.c:910`).
const INET_LOPT_TCP_SEND_TIMEOUT: u8 = 30;
/// `INET_LOPT_PACKET_SIZE` (32) — `{packet_size, …}`, the C's `inet.psize` (`inet_drv.c:918`).
const INET_LOPT_PACKET_SIZE: u8 = 32;
/// `INET_LOPT_TCP_SEND_TIMEOUT_CLOSE` (35) — `{send_timeout_close, …}` (`inet_drv.c:922`).
const INET_LOPT_TCP_SEND_TIMEOUT_CLOSE: u8 = 35;
/// `INET_LOPT_MSGQ_HIWTRMRK` (36) — the busy-message-queue high watermark (`inet_drv.c:924`).
const INET_LOPT_MSGQ_HIWTRMRK: u8 = 36;
/// `INET_LOPT_MSGQ_LOWTRMRK` (37) — the busy-message-queue low watermark (`inet_drv.c:925`).
const INET_LOPT_MSGQ_LOWTRMRK: u8 = 37;
/// `INET_LOPT_TCP_SHOW_ECONNRESET` (39) — `tcp_add_flags` bit 1 (`inet_drv.c:930`).
const INET_LOPT_TCP_SHOW_ECONNRESET: u8 = 39;
/// `INET_LOPT_TCP_READ_AHEAD` (49) — the `TCP_ADDF_NO_READ_AHEAD` bit's option (`inet_drv.c:930`).
const INET_LOPT_TCP_READ_AHEAD: u8 = 49;

// The **variable-length** codes: the five-byte record plus a payload. Their lengths are the C's own
// (`inet_set_opts`), and the walker must step over them exactly or every following record would be
// mis-parsed — so each constant below spells out the payload it carries.

/// `INET_OPT_LINGER` (3) — `[opt, onoff_be32, l_linger_be32]`, four payload bytes
/// (`inet_drv.c:877`; the arm at `:7363-7369`).
const INET_OPT_LINGER: u8 = 3;
/// `UDP_OPT_MULTICAST_IF` (11) — `[opt, addr_be32]`, a **whole** record (see [`record_len`])
/// (`inet_drv.c:885`).
const INET_OPT_MULTICAST_IF: u8 = 11;
/// `UDP_OPT_ADD_MEMBERSHIP` (14) — `[opt, domain_be32, payload…]`, the payload's own span being
/// domain-dependent (`inet_drv.c:888`; the arm at `:7619-7642`).
const INET_OPT_ADD_MEMBERSHIP: u8 = 14;
/// `UDP_OPT_DROP_MEMBERSHIP` (15) — as [`INET_OPT_ADD_MEMBERSHIP`] (`inet_drv.c:889`; the arm at
/// `:7644-7667`).
const INET_OPT_DROP_MEMBERSHIP: u8 = 15;
/// `INET_OPT_RAW` (34) — `[opt, proto_be32, type_be32, arg_sz_be32, arg bytes]`
/// (`inet_drv.c:909`; the arm at `:7825-7845`).
const INET_OPT_RAW: u8 = 34;
/// `INET_LOPT_NETNS` (38) — `[opt, len_be32, path bytes]` (`inet_drv.c:913`; the arm at
/// `:7152-7168`).
const INET_LOPT_NETNS: u8 = 38;
/// `INET_OPT_BIND_TO_DEVICE` (42) — `[opt, len_be32, device name bytes]` (`inet_drv.c:917`; the
/// arm at `:7848-7862`).
const INET_OPT_BIND_TO_DEVICE: u8 = 42;

/// `INET_AF_INET` (1) and `INET_AF_INET6` (2) — the two membership domains, which a membership
/// record's value word names and which decide that record's payload span
/// (`inet_drv.c:757-758`, see [`record_len`]).
const INET_AF_INET: i32 = 1;
/// See [`INET_AF_INET`].
const INET_AF_INET6: i32 = 2;

/// `INET_MULTI` (3) — the `{active, N}` value whose record carries the trailing counter
/// (`inet_drv.c:780`).
const INET_MULTI: i32 = 3;
/// `INET_MODE_BINARY` (1) — the only `mode` value besides [`INET_MODE_LIST`] (`inet_drv.c:779`).
const INET_MODE_BINARY: i32 = 1;

/// One `[code, val_be32]` record of a `setopts` argument.
struct RecordBytes<'a> {
    /// The option code (the record's first byte).
    code: u8,
    /// The record's big-endian value word, as the C reads it.
    value: i32,
    /// The record's **whole** byte span, `RECORD` bytes or more (the variable-length codes), which
    /// is what the mirror stores and what the replay sends again.
    bytes: &'a [u8],
}



/// What a port has been told about its inet options.
///
/// Keyed by port **number** in [`InetOptions`], and dropped with the rest of a port's extras when it
/// closes ([`PortExtras::forget_port`](super::ops::PortExtras)), so a recycled port number never
/// inherits a dead socket's options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PortInetOptions {
    /// The driver's current `{mode, list|binary}` — the C's `inet.mode`. `None` while no `mode`
    /// record has been seen (every `prim_inet` port sets one, but a port driven by hand may not).
    mode: Option<StreamType>,
    /// Every **successful** `setopts` argument's records, verbatim and in arrival order.
    records: Vec<Vec<u8>>,
}

impl PortInetOptions {
    /// The driver's current data mode, if a `{mode, …}` record has been seen.
    pub(crate) fn mode(&self) -> Option<StreamType> {
        self.mode
    }

    /// The recorded `setopts` records, in arrival order (the replay source).
    pub(crate) fn records(&self) -> &[Vec<u8>] {
        &self.records
    }
}

/// The per-port inet option mirrors of one [`PortRegistry`].
///
/// A side table rather than a field of [`PortMeta`](super::PortMeta) for the same reason
/// [`PortExtras`](super::ops::PortExtras) is one: the metadata is built once by `open_port/2`'s
/// arms (three of them), while the mirror is filled later and only for `inet` ports. It is held
/// **inside** `PortExtras` so a port has exactly one "not `open_port/2`'s state" lifecycle.
#[derive(Debug, Default)]
pub(crate) struct InetOptions {
    /// The mirrors, keyed by port number.
    ports: HashMap<u64, PortInetOptions>,
}

impl InetOptions {
    /// Drops `number`'s mirror — called from
    /// [`PortExtras::forget_port`](super::ops::PortExtras), the single place a port's
    /// post-`open_port/2` state leaves the runtime.
    pub(crate) fn forget_port(&mut self, number: u64) {
        self.ports.remove(&number);
    }

    /// Records one **successful** `setopts` argument for port `number`, answering whether it was
    /// stored.
    ///
    /// Nothing is stored when
    ///
    /// * the argument is empty (a `setopts` of no options inherits nothing), or
    /// * the argument cannot be walked into whole records ([`walk`]'s conservative rules): a
    ///   truncated or unrecognised variable-length record would make every following record's start
    ///   ambiguous, and a leftover tail shorter than one record means the argument is not a whole
    ///   number of records at all. Storing a mis-split record list would corrupt the replay.
    ///   Dropping the whole argument is the only safe answer — the port keeps whatever it already
    ///   had.
    fn record(&mut self, number: u64, arg: &[u8]) -> bool {
        if arg.is_empty() {
            return false;
        }
        let Some(records) = walk(arg) else {
            return false;
        };
        let entry = self.ports.entry(number).or_default();
        for record in &records {
            if record.code == INET_LOPT_MODE {
                if let Some(mode) = mode_of(record.value) {
                    entry.mode = Some(mode);
                }
            }
            entry.records.push(record.bytes.to_vec());
        }
        true
    }

    /// `number`'s mirror, when anything has been recorded for it.
    fn get(&self, number: u64) -> Option<&PortInetOptions> {
        self.ports.get(&number)
    }

    /// Seeds `number`'s data mode without inventing a record — what an **accepted** port inherits
    /// from its listener when the listener's own mode came from `open_port/2`'s stream option
    /// rather than from a `{mode, …}` record.
    ///
    /// A mode already observed is never overwritten: the seeded value is the fallback the listener's
    /// own [`PortRegistry::data_mode`] resolution produced.
    fn seed_mode(&mut self, number: u64, mode: StreamType) {
        let entry = self.ports.entry(number).or_default();
        if entry.mode.is_none() {
            entry.mode = Some(mode);
        }
    }
}

impl PortRegistry {
    /// Notes one `setopts` control answer for the port `id`, recording the argument when the driver
    /// accepted it.
    ///
    /// `ok` is the caller's reading of the reply's **leading tag** ([`setopts_succeeded`]): the C's
    /// `INET_REP_OK`. A rejected `setopts` (`[0]einval`, a `badarg`, a capability gap) is never
    /// recorded, so a value the driver refused can never be replayed onto an accepted socket.
    ///
    /// Answers whether the argument was stored (see [`InetOptions::record`]).
    pub(crate) fn note_inet_setopts(&mut self, id: PortId, arg: &[u8], ok: bool) -> bool {
        if !ok {
            return false;
        }
        self.extras.inet.record(id.number(), arg)
    }

    /// The mirror recorded for `id`, when it has one.
    ///
    /// `None` means "this port was never told any inet option" — a non-inet port, or one whose
    /// `setopts` calls were all rejected — never "the defaults".
    pub(crate) fn inet_options(&self, id: PortId) -> Option<&PortInetOptions> {
        self.extras.inet.get(id.number())
    }

    /// The port's current **data mode**: the driver's `{mode, …}` when the mirror knows it, else
    /// `fallback`.
    ///
    /// ⚠️ This is **not** [`PortMeta::stream`](super::PortMeta): `prim_inet:open/6` always opens the
    /// driver port `[binary]` (`erts/preloaded/src/prim_inet.erl:99-137`), so `PortMeta::stream` is
    /// [`StreamType::Binary`] for every inet port while the driver may well be in `INET_MODE_LIST`.
    /// A `{mode, list}` socket's `{Port, {data, …}}`/`{inet_async, …}` payloads are char lists on
    /// the oracle, so any renderer that shapes an inet payload must resolve its stream through here,
    /// with `PortMeta::stream` as the `fallback`.
    pub(crate) fn data_mode(&self, id: PortId, fallback: StreamType) -> StreamType {
        self.inet_options(id)
            .and_then(PortInetOptions::mode)
            .unwrap_or(fallback)
    }

    /// Seeds the data mode of a **newly created** port — the accepted port inheriting its
    /// listener's mode before any record is replayed (see [`InetOptions::seed_mode`]).
    pub(crate) fn seed_inet_mode(&mut self, id: PortId, mode: StreamType) {
        self.extras.inet.seed_mode(id.number(), mode);
    }

    /// Replays `from`'s inherited option block onto `to` — the runtime half of the C's
    /// `tcp_inet_copy`, which the driver cannot do because it cannot create a port.
    ///
    /// Only [`is_inherited_record`]'s codes are sent, in the listener's own arrival order, through
    /// the ordinary **named** `setopts` verb of [`PortRegistry::port_control`] — so each replayed
    /// record is re-recorded by [`PortRegistry::note_inet_setopts`], the same path the original
    /// call took (one entry point, not two).
    ///
    /// A port with no records replays nothing and answers `Ok(())`: an accept from a listening
    /// socket that was never configured inherits nothing, which is the C's own result.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] at the first record the driver **rejects** (the C's `[0]einval` reply)
    /// or the first `port_control` failure. The caller must then discard the port: a partially
    /// configured socket is worse than no socket (see the module docs' failure rule).
    pub(crate) fn inherit_inet_options(&mut self, from: PortId, to: PortId) -> Result<()> {
        let Some(source) = self.inet_options(from) else {
            return Ok(());
        };
        // Snapshot before the replays: `port_control` borrows this registry mutably (and re-enters
        // `note_inet_setopts` on the **destination**'s entry).
        let records: Vec<Vec<u8>> = source.records().to_vec();
        for record in &records {
            if !is_inherited_record(record) {
                continue;
            }
            let reply = self.port_control(to, SETOPTS_VERB, record)?;
            if !setopts_succeeded(&reply) {
                return Err(BeamError::BadArg);
            }
        }
        Ok(())
    }
}

/// Whether a control reply reports a **successful** `setopts` — the C's leading `INET_REP_OK` tag
/// (`inet_drv.c:870`), which is what makes a rejected record (`[0]einval`) distinguishable from an
/// accepted one: both are `Ok(bytes)` at the `port_control` seam, because a driver's *own*
/// rejection is a reply, not an error.
pub(crate) fn setopts_succeeded(reply: &[u8]) -> bool {
    reply.first() == Some(&INET_REP_OK)
}

/// Whether `record` is part of the option block the C copies into an accepted descriptor
/// (`tcp_inet_copy`, `erts/emulator/drivers/common/inet_drv.c:11810-11827`).
///
/// `record` is one whole record (`[code, val_be32, …]`); the decision is the code's:
///
/// | code | option | the C field it is |
/// |---|---|---|
/// | 20 | `{buffer, N}` | `bufsz` (`:11820`) |
/// | 21 | `{header, N}` | `hsz` (`:11819`) |
/// | 23 | `{packet, N}` | `htype` (`:11815`) |
/// | 24 | `{mode, …}` | `mode` (`:11811`) |
/// | 25 | `{deliver, …}` | `deliver` (`:11813`) |
/// | 26 | `{exit_on_close, …}` | `exitf` (`:11812`) |
/// | 27/28 | the TCP watermarks | `high`/`low` (`:11821-11822`) |
/// | 30 | `{send_timeout, …}` | `send_timeout` (`:11823`) |
/// | 32 | `{packet_size, N}` | `psize` (`:11816`) |
/// | 35 | `{send_timeout_close, …}` | `send_timeout_close` (`:11824`) |
/// | 36/37 | the busy-message-queue pair | copied through `erl_drv_busy_msgq_limits` (`:11850-11854`) |
/// | 39 | `{show_econnreset, …}` | `TCP_ADDF_SHOW_ECONNRESET` (`:11825`) |
/// | 49 | `{read_ahead, …}` | `TCP_ADDF_NO_READ_AHEAD` (`:11827`) |
///
/// Everything else is **not** inherited, and the module docs say why for each family.
fn is_inherited_record(record: &[u8]) -> bool {
    let Some(&code) = record.first() else {
        return false;
    };
    matches!(
        code,
        INET_LOPT_BUFFER
            | INET_LOPT_HEADER
            | INET_LOPT_PACKET
            | INET_LOPT_MODE
            | INET_LOPT_DELIVER
            | INET_LOPT_EXITONCLOSE
            | INET_LOPT_TCP_HIWTRMRK
            | INET_LOPT_TCP_LOWTRMRK
            | INET_LOPT_TCP_SEND_TIMEOUT
            | INET_LOPT_PACKET_SIZE
            | INET_LOPT_TCP_SEND_TIMEOUT_CLOSE
            | INET_LOPT_MSGQ_HIWTRMRK
            | INET_LOPT_MSGQ_LOWTRMRK
            | INET_LOPT_TCP_SHOW_ECONNRESET
            | INET_LOPT_TCP_READ_AHEAD
    )
}

/// The [`StreamType`] a `{mode, N}` record's value names, or `None` for a value the driver would
/// have refused (so the mirror keeps whatever it had).
fn mode_of(value: i32) -> Option<StreamType> {
    match value {
        INET_MODE_BINARY => Some(StreamType::Binary),
        0 => Some(StreamType::List),
        _ => None,
    }
}

/// Whether `op` is an `INET_REQ_SETOPTS` control call — the decimal `c_uint` string
/// `erlang:port_control/3` forwards (`INET_REQ_SETOPTS + ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`,
/// `inet_drv.c:805-847`) or the driver's own `setopts` verb.
///
/// Anything else (an `open`, a `recv`, a driver's private verb) is not the mirror's business, so the
/// `port_control` hook parses a number exactly once for the requests that can be one.
pub(crate) fn is_setopts_control(op: &str) -> bool {
    if op == SETOPTS_VERB {
        return true;
    }
    op.parse::<u64>()
        .is_ok_and(|code| code == INET_REQ_SETOPTS + INET_CONTROL_MAGIC)
}

/// Walks one `setopts` argument into whole records, or `None` when a record boundary cannot be
/// determined.
///
/// The walk is the C's (`inet_set_opts`, `inet_drv.c:6908`): while at least [`RECORD`] bytes remain,
/// take the code and the big-endian value word, then step over the record's **whole** length —
/// [`RECORD`] for every fixed-length code and the code-specific payload for the variable-length ones
/// ([`record_len`]).
///
/// The conservative rule: a record whose length is not determinable from the bytes actually present
/// (a negative or oversized length word, a truncated payload) makes the walk answer `None`, and
/// [`InetOptions::record`] then stores **nothing** — a mis-split record list would be replayed as
/// garbage onto an accepted socket, which is worse than inheriting nothing. The C cannot hit this
/// from `prim_inet` (it sends whole records), but a driver-level caller can, and a truncated
/// argument is a value the driver did not fully apply either.
///
/// # The tail rule
///
/// The C's loop is `while (len >= 5)` (`inet_drv.c:6908`): when fewer than five bytes remain it is
/// left (`:7943`) and the call still **succeeds** (its final `return 0`, `:7987`), so a trailing
/// partial record is silently ignored by the driver. The walk is stricter — a non-empty tail after
/// the loop answers `None`.
///
/// The difference is what the answer is *for*. The C has already applied the whole records it
/// consumed, so ignoring a tail costs it nothing. This walker's output is instead **replayed** onto
/// another socket, and an argument that is not a whole number of records did not come from the
/// record encoder (`prim_inet:encode_opt_val/1`, `erts/preloaded/src/prim_inet.erl:2398-2435`,
/// which emits whole records only): caller and driver disagree about the layout, and what the driver
/// consumed is not recoverable from what was left — one arm does not even keep the remaining length
/// honest (`len -= len;` in the IPv6 membership arm, `inet_drv.c:7780`). Replaying the subset that
/// happens to parse would therefore claim a configuration the listener need not have, so a
/// truncated argument records **nothing**, exactly as an undeterminable record span does.
fn walk(arg: &[u8]) -> Option<Vec<RecordBytes<'_>>> {
    let mut rest = arg;
    let mut records = Vec::new();
    while rest.len() >= RECORD {
        let code = rest[0];
        let value = i32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]);
        let step = record_len(code, value, rest)?;
        if step > rest.len() {
            return None;
        }
        records.push(RecordBytes {
            code,
            value,
            bytes: &rest[..step],
        });
        rest = &rest[step..];
    }
    if rest.is_empty() {
        Some(records)
    } else {
        None
    }
}

/// The whole length of the record at the head of `rest`, or `None` when it cannot be determined
/// from the bytes present.
///
/// `rest` starts at the record's own `[opt, val_be32]`, so every span below is [`RECORD`] plus the
/// payload, and each is the C's own — read from the arm the code selects inside `inet_set_opts`
/// (the loop has already consumed the five-byte head, so the arm's `len` is what follows it):
///
/// * `linger` (3) — five plus the four-byte `l_linger` (`inet_drv.c:7363-7369`: `if (len < 4)
///   return -1`, then `ptr += 4`).
/// * `add_membership` (14) / `drop_membership` (15) — five plus a **domain-dependent** payload
///   (`:7668-7801`): twelve for `INET_AF_INET` (ifindex, multiaddr, local address — three
///   `get_int32`s, `:7703-7711`), twenty for `INET_AF_INET6` (ifindex plus the sixteen-byte group
///   address, `:7778-7779`). Any other domain is a `-1` from the driver (`:7639-7641`,
///   `:7664-7666`), so its span is undeterminable and the walk answers `None`; `prim_inet`'s
///   encoder agrees with both spans (`enc_value_2(membership, …)`,
///   `erts/preloaded/src/prim_inet.erl:2182-2212`).
/// * `{active, N}` (22) when the value is `INET_MULTI` (3) — five plus the two-byte counter
///   (`:6977-6979`).
/// * `raw` (34) — five plus eight plus the operand's own length word (`:7830-7844`): the arm
///   reads `proto = ival`, then `type`, then `arg_sz` — so the **second** extra word is the span,
///   not the first.
/// * `{netns, …}` (38) and `bind_to_device` (42) — five plus the length word's value (`:7157-7167`,
///   `:7853-7861`).
///
/// `multicast_if` (11) is **not** one of them: it is a whole [`RECORD`], because the C's arm only
/// re-reads the value it already has (`ival`, the address, `:7589-7617`) and steps over nothing, and
/// `prim_inet` encodes that address as exactly the four value bytes (`enc_value_2(mif, …)`,
/// `prim_inet.erl:2178-2181`).
///
/// Every other code occupies exactly [`RECORD`] bytes. A length word that is negative or does not
/// fit the bytes present answers `None` (see [`walk`]).
fn record_len(code: u8, value: i32, rest: &[u8]) -> Option<usize> {
    match code {
        INET_OPT_LINGER => Some(RECORD + 4),
        // A whole record: the arm reads no payload at all.
        INET_OPT_MULTICAST_IF => Some(RECORD),
        INET_OPT_ADD_MEMBERSHIP | INET_OPT_DROP_MEMBERSHIP => match value {
            INET_AF_INET => Some(RECORD + 12),
            INET_AF_INET6 => Some(RECORD + 20),
            _ => None,
        },
        INET_LOPT_ACTIVE if value == INET_MULTI => Some(RECORD + ACTIVE_COUNTER),
        INET_LOPT_NETNS | INET_OPT_BIND_TO_DEVICE => {
            let payload = usize::try_from(value).ok()?;
            RECORD.checked_add(payload)
        }
        INET_OPT_RAW => {
            // `[opt, proto, type, arg_sz, arg…]`: `type` and `arg_sz` are the two extra words, and
            // the C's `if (len < 8)` (`:7830`) is the same requirement as needing both present.
            let length = rest.get(RECORD + 4..RECORD + 8)?;
            let operand = u32::from_be_bytes([length[0], length[1], length[2], length[3]]);
            RECORD.checked_add(8)?.checked_add(usize::try_from(operand).ok()?)
        }
        _ => Some(RECORD),
    }
}

#[cfg(test)]
#[path = "inet_opts_tests.rs"]
mod tests;
