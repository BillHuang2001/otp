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

//! The per-port **inet option mirror** — the three latches a socket port's `setopts` calls leave
//! behind, and the two of them an **accepted** socket inherits from its listener.
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
//! The C keeps four such fields on the port's descriptor and reads them from wherever it needs
//! them. `beam_io`'s socket driver keeps them privately and exposes no accessor, so the runtime
//! observes the same records the driver applies and holds the three the runtime itself must
//! answer from.
//!
//! # What is mirrored
//!
//! Per port, keyed by **port number** (the [`PortExtras`](super::ops::PortExtras) key), exactly
//! three latches ([`PortInetOptions`]), each the **last** accepted record's value:
//!
//! | latch | code | the C field | the consumer |
//! |---|---|---|---|
//! | [`PortInetOptions::mode`] | [`INET_LOPT_MODE`] (24) | `inet.mode` (`inet_drv.c:899`) | [`PortRegistry::data_mode`], which every inet payload renderer resolves its stream through |
//! | [`PortInetOptions::active_mode`] | [`INET_LOPT_ACTIVE`] (22) | `inet.active` (`inet_drv.c:6968-7016`) | the `{tcp_closed, S}` gate in [`inet_active`](super::peer_closed) |
//! | [`PortInetOptions::exit_on_close`] | [`INET_LOPT_EXITONCLOSE`] (26) | `inet.exitf` (`inet_drv.c:904`) | the port-retirement gate in [`inet_exit_on_close`](super::peer_closed) |
//!
//! Nothing else about a `setopts` argument is (or needs to be) retained: the option store itself
//! is the driver's, and the mirror answers only what the driver cannot. Recording is one entry
//! point ([`PortRegistry::note_inet_setopts`]) called from
//! [`PortRegistry::port_control`](super::PortRegistry) once the driver has answered, so a rejected
//! `setopts` can never move a latch: only a reply tagged `INET_REP_OK` is applied.
//!
//! # `mode` is a seam, not a convenience
//!
//! [`PortInetOptions::mode`] is **not** [`PortMeta::stream`](super::PortMeta): `prim_inet:open/6`
//! always opens the driver port `[binary]` (`erts/preloaded/src/prim_inet.erl:99-137`), so
//! `PortMeta::stream` is [`StreamType::Binary`] for every inet port while the driver may well be
//! in `INET_MODE_LIST`. `PortMeta` is fixed at `open_port/2` and a post-open `{mode, …}` control op
//! never reaches `beam_io`, so [`PortRegistry::data_mode`] — this latch, with `PortMeta::stream`
//! as its fallback — is the only way a renderer can shape a `{Port, {data, …}}` or
//! `{inet_async, …}` payload the way the oracle does.
//!
//! # The accepted socket inherits the two copied latches
//!
//! In the C an `accept` builds the new port from the listening descriptor and copies its option
//! block field by field (`tcp_inet_copy`, `erts/emulator/drivers/common/inet_drv.c:11795`, copy
//! list `:11814`-`:11831`). The **option store** half of that copy belongs to `beam_io`:
//! [`TcpInetDriver::from_accepted`](beam_io::TcpInetDriver::from_accepted) builds the accepted
//! driver from the listener's own store, so the runtime retains no record for a replay and
//! replays none.
//!
//! What the driver cannot carry across — because these are the runtime's latches, not the option
//! store's fields — is the other half, and the C copies exactly two of them into the new
//! descriptor: `inet.mode` (`:11811`) and `inet.exitf` (`:11812`). So
//! [`PortRegistry::adopt_accepted`](super::PortRegistry) copies both through
//! [`PortRegistry::seed_inet_latches`] as it registers the new port. `inet.active` is **not**
//! copied: the C's copy list omits it (`:11814`-`:11844`) and `inet_start` resets it
//! (`:10967`/`:10932`), so the accepted port starts passive exactly as the oracle's does — and
//! `prim_inet:accept_opts/3` (`erts/preloaded/src/prim_inet.erl:502-517`) is what re-applies the
//! listener's active mode from the Erlang side.
//!
//! [`InetAsyncResult::Accepted`]: beam_io::InetAsyncResult::Accepted

use std::collections::HashMap;

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
const ACTIVE_COUNTER: usize = 2;

// ---------------------------------------------------------------------------
// The option codes the mirror names
// ---------------------------------------------------------------------------
//
// `beam_io` keeps its option vocabulary in a private `opt` submodule and re-exports only part of
// it, so the codes this module needs are restated here with the C they come from. They are
// **hand-maintained wire constants** (`inet_drv.c:874-930`), not derived values. Only the three
// the latches decode and the ones the walker must know to step over a record are listed.

/// `INET_LOPT_ACTIVE` (22) — `{active, false|true|once|N}` (`inet_drv.c:890`).
const INET_LOPT_ACTIVE: u8 = 22;
/// `INET_LOPT_MODE` (24) — `{mode, list|binary}`, the C's `inet.mode` (`inet_drv.c:899`).
///
/// `beam_io` declares the same number privately (`drivers/inet/opt.rs:193`) but does not re-export
/// it, so the mirror restates it.
const INET_LOPT_MODE: u8 = 24;
/// `INET_LOPT_EXITONCLOSE` (26) — the C's `inet.exitf` (`inet_drv.c:904`).
const INET_LOPT_EXITONCLOSE: u8 = 26;

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
/// `INET_MODE_BINARY` (1) — the only `mode` value besides `INET_MODE_LIST` (`0`, `inet_drv.c:779`).
const INET_MODE_BINARY: i32 = 1;

/// One record of a `setopts` argument, as [`walk`] reports it: the option code and the big-endian
/// value word, in arrival order.
///
/// The record's **span** — [`record_len`]'s answer — is not part of the value: [`walk`] computes it
/// only to find where the next record starts (a `{raw, …}` or `{netns, …}` record carries a
/// variable payload), and the mirror keeps no bytes at all (see the module docs).
struct RecordBytes {
    /// The option code (the record's first byte).
    code: u8,
    /// The record's big-endian value word, as the C reads it.
    value: i32,
}

/// What a port has been told about its inet options: three latches, each the last accepted
/// record's value.
///
/// Keyed by port **number** in [`InetOptions`], and dropped with the rest of a port's extras when it
/// closes ([`PortExtras::forget_port`](super::ops::PortExtras)), so a recycled port number never
/// inherits a dead socket's options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PortInetOptions {
    /// The driver's current `{mode, list|binary}` — the C's `inet.mode`. `None` while no `mode`
    /// record has been seen (every `prim_inet` port sets one, but a port driven by hand may not).
    mode: Option<StreamType>,
    /// The driver's current `{active, …}` operand — the C's `inet.active`. See
    /// [`PortInetOptions::active_mode`].
    active: Option<i32>,
    /// The driver's current `{exit_on_close, …}` flag — the C's `inet.exitf`. See
    /// [`PortInetOptions::exit_on_close`].
    exit_on_close: Option<bool>,
}

impl PortInetOptions {
    /// The driver's current data mode, if a `{mode, …}` record has been seen.
    pub(crate) fn mode(&self) -> Option<StreamType> {
        self.mode
    }

    /// The driver's current `{active, …}` mode value, if a record has been seen.
    ///
    /// [`INET_LOPT_ACTIVE`]'s operand **is** the C's `desc->inet.active`
    /// (`inet_set_opts`'s arm, `erts/emulator/drivers/common/inet_drv.c:6968-7016`), whose
    /// vocabulary is `INET_PASSIVE` 0 / `INET_ACTIVE` 1 / `INET_ONCE` 2 / `INET_MULTI` 3
    /// (`inet_drv.c:786-789`). The mirror only observes it: the driver owns the state machine.
    ///
    /// `None` means **no `{active, …}` record has ever been accepted** — the port's driver is
    /// still at the C's `inet_start` default, `INET_PASSIVE` (`inet_drv.c:10932`). The last record
    /// wins, exactly as the driver's own field does.
    ///
    /// One caveat the caller must carry: a **spent** grant is invisible here. The C clears
    /// `desc->active` back to `INET_PASSIVE` after an `{active, once}` delivery
    /// (`INET_CHECK_ACTIVE_TO_PASSIVE`) or when an `{active, N}` counter reaches zero — a
    /// transition that is the *driver's* state, which this mirror records only when a **new**
    /// `setopts` arrives.
    pub(crate) fn active_mode(&self) -> Option<i32> {
        self.active
    }

    /// The driver's current `{exit_on_close, …}` flag, if a record has been seen — the C's
    /// `desc->inet.exitf` ([`INET_LOPT_EXITONCLOSE`], `inet_drv.c:904`).
    ///
    /// `None` means no such record was accepted, i.e. the driver's own default, `true`
    /// (`exitf = 1`, `inet_drv.c:10929`). Both `{exit_on_close, true}` and the `0`/`1` operand the
    /// wire carries are covered: the record's value word is the flag itself.
    pub(crate) fn exit_on_close(&self) -> Option<bool> {
        self.exit_on_close
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

    /// Applies one **successful** `setopts` argument to port `number`'s latches, answering whether
    /// it was walked at all.
    ///
    /// Nothing is applied when
    ///
    /// * the argument is empty (a `setopts` of no options tells the mirror nothing), or
    /// * the argument cannot be walked into whole records ([`walk`]'s conservative rules): a
    ///   truncated or unrecognised variable-length record would make every following record's start
    ///   ambiguous, so the records cannot be attributed to codes at all. The port keeps whatever it
    ///   already had.
    ///
    /// A walked argument whose records carry none of the three latched codes stores nothing but
    /// still answers `true`: the argument *was* applied to the mirror, it simply names no latch
    /// (that is what lets `note_inet_setopts`'s caller learn the argument was accepted). A port told
    /// only non-latched options therefore has **no** mirror entry at all, so
    /// [`InetOptions::get`]'s `None` keeps meaning "nothing about this port is latched".
    fn record(&mut self, number: u64, arg: &[u8]) -> bool {
        if arg.is_empty() {
            return false;
        }
        let Some(records) = walk(arg) else {
            return false;
        };
        for record in &records {
            match record.code {
                INET_LOPT_MODE => {
                    if let Some(mode) = mode_of(record.value) {
                        self.ports.entry(number).or_default().mode = Some(mode);
                    }
                }
                INET_LOPT_ACTIVE => {
                    self.ports.entry(number).or_default().active = Some(record.value);
                }
                INET_LOPT_EXITONCLOSE => {
                    self.ports.entry(number).or_default().exit_on_close = Some(record.value != 0);
                }
                _ => {}
            }
        }
        true
    }

    /// `number`'s mirror, when at least one latch has been observed for it.
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

    /// Seeds `number`'s `{exit_on_close, …}` latch — the C's copied `inet.exitf`
    /// (`inet_drv.c:11812`), which an accepted port takes from its listener.
    ///
    /// A latch already observed is never overwritten, for the reason [`InetOptions::seed_mode`]
    /// gives.
    fn seed_exit_on_close(&mut self, number: u64, exit_on_close: bool) {
        let entry = self.ports.entry(number).or_default();
        if entry.exit_on_close.is_none() {
            entry.exit_on_close = Some(exit_on_close);
        }
    }
}

impl PortRegistry {
    /// Notes one `setopts` control answer for the port `id`, applying the argument to its latches
    /// when the driver accepted it.
    ///
    /// `ok` is the caller's reading of the reply's **leading tag** ([`setopts_succeeded`]): the C's
    /// `INET_REP_OK`. A rejected `setopts` (`[0]einval`, a `badarg`, a capability gap) is never
    /// applied, so a value the driver refused can never move a latch.
    ///
    /// Answers whether the argument was walked (see [`InetOptions::record`]).
    pub(crate) fn note_inet_setopts(&mut self, id: PortId, arg: &[u8], ok: bool) -> bool {
        if !ok {
            return false;
        }
        self.extras.inet.record(id.number(), arg)
    }

    /// The mirror recorded for `id`, when at least one latch has been observed for it.
    ///
    /// `None` means "this port was never told a latched inet option" — a non-inet port, one whose
    /// `setopts` calls all named non-latched options (or were all rejected), or one that was only
    /// *seeded* with nothing — never "the defaults". The defaults live at the call sites
    /// ([`PortRegistry::data_mode`]'s `fallback`, the C's `INET_PASSIVE` and `exitf = 1`).
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
    /// listener's mode (see [`InetOptions::seed_mode`] and
    /// [`PortRegistry::seed_inet_latches`]).
    pub(crate) fn seed_inet_mode(&mut self, id: PortId, mode: StreamType) {
        self.extras.inet.seed_mode(id.number(), mode);
    }

    /// Seeds the two latches the C copies into an accepted descriptor — the accepted port's half of
    /// `tcp_inet_copy` that the *driver* cannot carry across, because they are this runtime's
    /// mirror and not the driver's option store.
    ///
    /// Exactly the two fields the C copies and the driver cannot: `inet.mode` (`:11811`) and
    /// `inet.exitf` (`:11812`). `inet.active` is deliberately absent — the C's copy list omits it
    /// (`:11814`-`:11844`) and `inet_start` resets it, so the accepted port starts passive and
    /// `prim_inet:accept_opts/3` re-applies the listener's mode from the Erlang side. Both seeds are
    /// last-resort fallbacks ([`InetOptions::seed_mode`],
    /// [`InetOptions::seed_exit_on_close`]): a value the port has already observed wins.
    pub(crate) fn seed_inet_latches(&mut self, id: PortId, mode: StreamType, exit_on_close: bool) {
        self.seed_inet_mode(id, mode);
        self.extras.inet.seed_exit_on_close(id.number(), exit_on_close);
    }
}

/// Whether a control reply reports a **successful** `setopts` — the C's leading `INET_REP_OK` tag
/// (`inet_drv.c:870`), which is what makes a rejected record (`[0]einval`) distinguishable from an
/// accepted one: both are `Ok(bytes)` at the `port_control` seam, because a driver's *own*
/// rejection is a reply, not an error.
pub(crate) fn setopts_succeeded(reply: &[u8]) -> bool {
    reply.first() == Some(&INET_REP_OK)
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
/// [`InetOptions::record`] then applies **nothing** — a mis-split record list would attribute a
/// value to the wrong code, which is worse than keeping the previous latch. The C cannot hit this
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
/// consumed, so ignoring a tail costs it nothing. This walker's output decides what the **mirror**
/// believes, and an argument that is not a whole number of records did not come from the record
/// encoder (`prim_inet:encode_opt_val/1`, `erts/preloaded/src/prim_inet.erl:2398-2435`, which emits
/// whole records only): caller and driver disagree about the layout, and what the driver consumed is
/// not recoverable from what was left — one arm does not even keep the remaining length honest
/// (`len -= len;` in the IPv6 membership arm, `inet_drv.c:7780`). A subset that happens to parse
/// would therefore claim a configuration the listener need not have, so a truncated argument applies
/// **nothing**, exactly as an undeterminable record span does.
fn walk(arg: &[u8]) -> Option<Vec<RecordBytes>> {
    let mut rest = arg;
    let mut records = Vec::new();
    while rest.len() >= RECORD {
        let code = rest[0];
        let value = i32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]);
        let step = record_len(code, value, rest)?;
        if step > rest.len() {
            return None;
        }
        records.push(RecordBytes { code, value });
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
