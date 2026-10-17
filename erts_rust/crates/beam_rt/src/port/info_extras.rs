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

//! The **derived** `erlang:port_info/2` items — `queue_size`, `memory`, `monitored_by`,
//! `locking` and `parallelism` — as read-only accessors over the port table.
//!
//! They live in their own file (rather than inline in [`super`]) because each one is an
//! *accounting* question answered from state other items do not touch, and each carries the C
//! derivation that decides the number. Nothing here mutates a port.
//!
//! # `queue_size` — the port's un-consumed byte count
//!
//! The C answers `erts_port_ioq_size(prt)` (`erts/emulator/beam/erl_bif_info.c:3868-3874`), the
//! size of the port's `ErlPortIOQueue` (the driver-visible `io-queue` of `io.c:130-155`). That
//! queue is the driver's own staging area in **both** directions: bytes the driver has produced
//! but not yet handed to the owner (`driver_output`/`driver_enq` — delivered as
//! `{Port,{data,…}}` later), and bytes the owner wrote that the device has not consumed
//! (`driver_enq` on a short write, drained by `ready_output`).
//!
//! [`beam_io::Port`] holds exactly those two queues, one per direction: its **input** queue is
//! "bytes read from the device, waiting for Erlang" and its **output** queue is "bytes written
//! by Erlang, waiting for the device" (`beam_io/src/port.rs:107-110`). So the honest answer is
//! their sum, and it is **measured**: a fresh port answers `0` (the oracle's own answer right
//! after `open_port`), while a 1 MiB `port_command` to a child that never reads leaves
//! `output=1000000` with a `queue_size` of `934464` on the oracle (the 64 KiB pipe buffer was
//! written, the rest is still queued).
//!
//! # `memory` — the port's own accounted size
//!
//! The C's `am_memory` arm (`erl_bif_info.c:3838-3860`) sums the port's link nodes, its monitor
//! records, its `port_set_data/2` heap and its line buffer — and **deliberately excludes the
//! `Port` struct itself** ("the Port struct should not be included though"). Its two tree walks
//! are `link_size`/`monitor_size` callbacks that **assign** rather than accumulate
//! (`*((Uint *) vsz) = …`, `erl_bif_info.c:3648-3660`), so N links still report one link node
//! and N monitors one monitor half; that is reproduced verbatim here because the oracle is the
//! compatibility target (measured: two links ⇒ `48`, one and three watchers ⇒ `72`).
//!
//! The constants are the C struct sizes on a 64-bit build, each derived rather than guessed:
//!
//! * [`LINK_NODE_BYTES`] — `sizeof(ErtsLink)` = `sizeof(ErtsMonLnkNode)`
//!   (`erts/emulator/beam/erl_monitor_link.h:545-558`): its `node` union is `sizeof(ErtsNonMsgSignal)`
//!   = 32 (`next` + `specific` + `tag` + `mlenoffs`, `erl_proc_sig_queue.h:154-168`, the widest
//!   member), plus `other` 8, plus `offset` 2 + `key_offset` 2 + `flags` 4 = **48**. A bare port
//!   has exactly one link (its owner), so a bare port's `memory` is 48 — the oracle's literal
//!   answer for `{spawn_driver,"ram_file_drv"}` *and* `{spawn,"cat"}`.
//! * [`MONITOR_DATA_BYTES`] — `erts_monitor_size` of a non-extended process monitor
//!   (`erl_monitor_link.c:1205-1250`) is `sizeof(ErtsMonitorDataHeap) / refc` with `refc = 2`
//!   (the origin and target halves share one record): `ErtsMonitorData` = origin 48 + target 48
//!   + `ref` 8 + `refc` 8 = 112, plus `ref_heap[ERTS_MAX_INTERNAL_REF_SIZE]` = 4 words = 32
//!   ⇒ 144 / 2 = **72** (the oracle's measured increment for a watched port).
//! * [`PORT_DATA_HEAP_BYTES`] — `sizeof(ErtsPortDataHeap)` (`erl_bif_port.c:608-615`), 72, with
//!   `(hsize-1)` further words for the stored term (`erts_port_data_size`, `erl_bif_port.c:658-668`).
//!   Measured against the oracle: `port_set_data(P,{foo,1})` (a 3-word tuple) moves `memory`
//!   from 48 to 136 = 48 + 72 + 2·8.
//! * [`LINEBUF_BYTES`] / [`LINEBUF_INITIAL`] — a `{line, N}` port adds
//!   `sizeof(LineBuf) + linebuf->ovsiz`, where `ovsiz = min(bufsiz, LINEBUF_INITIAL)`
//!   (`allocate_linebuf`, `io.c:3155-3166`; `LINEBUF_INITIAL` is 100, `io.c:176`) and `bufsiz`
//!   is the `{line, N}` length or 512 for a bare `line` (`erl_bif_port.c:862-868`, `:985-986`).
//!   `sizeof(LineBuf)` = three `ErlDrvSizeT` + `char data[1]` = **32**. Measured:
//!   `{line,10}` ⇒ 48+32+10 = 90, `{line,200}` and `line` ⇒ 48+32+100 = 180.
//!
//! # `monitored_by` — the watchers
//!
//! The C's `am_monitored_by` arm walks the port's **target** monitors (`ERTS_P_LT_MONITORS`
//! plus the target half of `ERTS_P_MONITORS`, `erl_bif_info.c:3735-3765`) and reports the
//! watching entities, which for a port are process pids. This runtime's authoritative set is the
//! port table's incoming monitor list — the very one
//! [`Runtime::port_monitors`](crate::Runtime::port_monitors) reports and
//! `erlang:monitor(port, Port)` populates.
//!
//! # `locking` / `parallelism`
//!
//! `locking` is the C's port-vs-driver lock choice: `port_level` when the driver entry sets
//! `ERL_DRV_FLAG_USE_PORT_LOCKING`, else `driver_level` (`erl_bif_info.c:3875-3894`). The C's
//! `spawn`/`fd` drivers do set it (`erts/emulator/sys/unix/sys_drivers.c:251`), measured as
//! `port_level`; `ram_file_drv` does not, measured as `driver_level`. A loaded driver's entry
//! flags are read at `dlopen` time in `crate::port::driver`, but the bit itself is **not**
//! carried across the `beam_io` seam — [`beam_io::DriverFlags`](beam_io::DriverFlags) keeps only
//! `SOFT_BUSY` (`beam_io/src/abi/port.rs:554-561`) — so a `{spawn_driver, Name}` port reports
//! the `driver_level` default and would diverge for a driver that requests port locking.
//! `parallelism` is the C's `ERTS_PTS_FLG_PARALLELISM` bit (`erl_bif_info.c:3895-3905`): this
//! runtime never grants a port that flag, so it truthfully answers `false` (the oracle's own
//! answer for both a `spawn` and a driver port).
use beam_process::{PortId, ProcessId};

use super::{PortName, PortRegistry};

/// `sizeof(ErtsLink)` — one link node (see the module docs for the derivation).
const LINK_NODE_BYTES: usize = 48;

/// `erts_monitor_size` of a non-extended process monitor (see the module docs).
const MONITOR_DATA_BYTES: usize = 72;

/// `sizeof(ErtsPortDataHeap)` — the `port_set_data/2` record's header, before its term's words.
const PORT_DATA_HEAP_BYTES: usize = 72;

/// `sizeof(LineBuf)` — the C line buffer's header (three `ErlDrvSizeT` + the inline `data[1]`).
const LINEBUF_BYTES: usize = 32;

/// `LINEBUF_INITIAL` (`io.c:176`): the capped initial overflow size of a line buffer.
const LINEBUF_INITIAL: usize = 100;
impl PortRegistry {
    /// The port's `queue_size`: the bytes it is holding that neither the owner nor the device
    /// has consumed yet (see the module docs for why that is the C's `ioq`).
    ///
    /// A port the registry no longer holds has no queue, so it answers `0`; the `port_info`
    /// floor answers `undefined` for such a port before it ever asks.
    pub(super) fn info_queue_size(&self, port: PortId) -> usize {
        self.registry
            .get(port)
            .map(|held| held.input_len() + held.output_len())
            .unwrap_or(0)
    }

    /// The port's `memory`: its link nodes, its monitor records, its `port_set_data/2` term and
    /// its line buffer — everything the C's `am_memory` arm counts, and nothing more (the
    /// `Port` struct itself is excluded there too).
    ///
    /// The **one** divergence is the stored data term's own words: the C copies it onto a
    /// private heap and counts `sizeof(ErtsPortDataHeap) + (hsize-1)·8`, while this runtime keeps
    /// the term on its pointer-stable store (see [`super::ops::PortExtras`]). The header size is
    /// counted here, the term's words are not — so `port_set_data(P,{foo,1})` reports 120 on this
    /// runtime where the oracle reports 136. Sizing the term faithfully would need the C's
    /// `copy_struct` word count, which `beam_message::copy::size_of_copy` does provide, but its
    /// on-heap binary shape is not the oracle's (`port_set_data(P,<<"abc">>)` is 136 on the
    /// oracle, i.e. a 3-word record, where our copier's on-heap bitstring is 8 words), so a
    /// derived number would be wrong for exactly the terms it looked precise about.
    pub(super) fn info_memory(&self, port: PortId) -> usize {
        let Some(meta) = self.meta.get(&port.number()) else {
            return 0;
        };
        // The link tree: the owner alone until `port_connect/2` adds one (see
        // [`super::ops::PortExtras::links`]). The C's walk **assigns** per entry, so a port with
        // any link reports one link node however many it has.
        let links = self
            .extra_links(port.number())
            .map_or(1, <[_]>::len);
        let watchers = self.port_table().monitors(port).len();
        let mut size = 0;
        if links > 0 {
            size = LINK_NODE_BYTES;
        }
        if !meta.monitors.is_empty() || watchers > 0 {
            size = MONITOR_DATA_BYTES;
        }
        // `erts_port_data_size`: the record's header, plus nothing for the term itself (see the
        // method docs). `port_set_data/2` stores the atom `undefined` as its default, and the C
        // counts an immediate as 0.
        //
        // [`PortRegistry::port_data`] is `badarg` for a port the registry no longer holds, which
        // is also a port with no stored term — and the `port_info` floor answers `undefined` for
        // such a port before this is ever reached — so the "no data term" reading is the exact
        // one, not a swallowed error.
        let data = self.port_data(port).unwrap_or(None);
        if meta_data_is_heap(data) {
            size += PORT_DATA_HEAP_BYTES;
        }
        if let Some(max_len) = meta.line {
            // `allocate_linebuf(bufsiz)`: `ovsiz = min(bufsiz, LINEBUF_INITIAL)`. `meta.line`
            // already holds the C's *resolved* `bufsiz` — `{line, N}` ⇒ `N` and a bare `line`
            // ⇒ 512, the parent module's `LINE_DEFAULT_MAX_LEN` (`erl_bif_port.c:985-986`) —
            // so no "bare option" sentinel has to be decoded here.
            size += LINEBUF_BYTES + max_len.min(LINEBUF_INITIAL);
        }
        size
    }

    /// The pids watching the port — the `monitored_by` item.
    ///
    /// The source is the port table's incoming monitor list, the same authority
    /// [`crate::Runtime::port_monitors`] reports: `erlang:monitor(port, Port)` records a
    /// [`PortMonitor`](beam_process::PortMonitor) there and a dead watcher is swept from it.
    pub(super) fn info_monitored_by(&self, port: PortId) -> Vec<ProcessId> {
        self.port_table()
            .monitors(port)
            .iter()
            .map(|monitor| monitor.watcher)
            .collect()
    }

    /// The `locking` item's value atom: `port_level` when the port has its own lock,
    /// `driver_level` when all ports of its driver share one. See the module docs.
    pub(super) fn info_locking(&self, port: PortId) -> &'static str {
        match self.meta.get(&port.number()).map(|meta| meta.kind) {
            // The C's `spawn`/`fd` drivers set `ERL_DRV_FLAG_USE_PORT_LOCKING`.
            Some(PortName::Spawn | PortName::SpawnExecutable | PortName::Fd) => "port_level",
            // A driver port: the entry's flags decide, and `beam_io` keeps only `SOFT_BUSY`, so
            // the honest default is "the driver did not ask for a per-port lock".
            _ => "driver_level",
        }
    }
}

/// Whether a `port_set_data/2` term occupies a C data heap.
///
/// The C keeps an **immediate** in the field itself and reports its size as `0`
/// (`erts_port_data_size`, `erl_bif_port.c:658-668`: `if ((data & 0x3) != 0) return 0`), and the
/// field's initial value — `undefined`, which `erts_init_port_data` stores
/// (`erl_bif_port.c:642`) — is one. Every atom is an immediate, so `undefined` needs no
/// special case: the `is_immediate` test *is* the C's `(data & 0x3) != 0`.
fn meta_data_is_heap(data: Option<beam_term::Eterm>) -> bool {
    match data {
        Some(term) => !beam_term::is_immediate(term),
        None => false,
    }
}
