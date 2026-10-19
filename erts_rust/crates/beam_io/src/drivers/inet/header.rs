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
//! The **`{header, N}` split** of one delivered payload — the C's `desc->hsz`
//! (`erts/emulator/drivers/common/inet_drv.c:6931`).
//!
//! `{header, N}` is *not* the `{packet, N}` framing header: that one is stripped before any of this
//! (`packet_get_body`, `:4285`, applied by [`super::frame`]), and it is never reported. `{header,
//! N}` is the socket option that makes the driver hand the first `N` bytes of every delivered
//! **payload** to the owner separately, so an application-level header needs no second binary —
//! the C's `[H1,…,HSz | Data]`.
//!
//! # The three-way rule, and why it is one helper
//!
//! The C writes the same `if` four times, once per term builder: `tcp_message` (`:3979`-`:4008`,
//! active mode), `inet_async_data` (`:3162`-`:3210`, the passive `TCP_REQ_RECV` reply),
//! `inet_port_data` (`:2753`-`:2765`, the `{deliver, port}` / `driver_output` route) and the
//! datagram pair (`packet_binary_message`, `:4106`, and `inet_async_binary_data`, `:3870`). It is
//!
//! ```c
//! if ((desc->mode == INET_MODE_LIST) || (hsz > len)) {
//!     LOAD_STRING(buf, len);              /* [H1,H2,...Hn] — one FLAT list, no binary tail */
//! } else {
//!     LOAD_BUF2BINARY(buf+hsz, len-hsz);  /* <<Data>> (sz may be 0 => <<>>) */
//!     if (hsz > 0) LOAD_STRING_CONS(buf, hsz);   /* [H1,..,HSz | <<Data>>] */
//! }
//! ```
//!
//! [`split_payload`] is that rule, and every payload producer of this driver that reports through
//! a term builder goes through it, so the `hsz > len` branch cannot be lost on one path and kept on
//! another. The **mode** half of the condition is deliberately absent: `[list]` mode renders the
//! header and the body as plain ints whatever the split is, which is exactly
//! [`ReadMessage::flattened`] — the runtime's choice, not the driver's.
//!
//! # The shapes
//!
//! `hsz` is the `{header, N}` option (an `i32` here, an `unsigned int` in the C), so a **negative**
//! option is the huge unsigned that always takes the flat-list branch. In `[binary]` mode:
//!
//! | payload `len` | `hsz` | C term | returned [`ReadMessage`] |
//! |---|---|---|---|
//! | `0` | `0` | `<<>>` (`LOAD_BUF2BINARY(buf, 0)`) | `header = []`, `body = Some([])` |
//! | `0` | `> 0` | `[]` (the flat-list branch) | `header = []`, `body = None` |
//! | `0 < len` | `0` | `<<payload>>` (no cons) | `header = []`, `body = Some(payload)` |
//! | `0 < hsz < len` | | `[H1..Hh \| <<rest>>]` | `header = payload[..hsz]`, `body = Some(payload[hsz..])` |
//! | `hsz == len > 0` | | `[H1..Hh \| <<>>]` | `header = payload`, `body = Some([])` |
//! | `hsz > len > 0` | | `[H1..Hn]` (the flat-list branch) | `header = payload`, `body = None` |
//!
//! [`ReadMessage::body`] `None` is the C's `buf == NULL`, i.e. "no binary tail at all" — which is
//! what makes the `hsz > len` row render as the C's proper list of the whole payload instead of
//! `[H1..Hn|<<>>]`.
use crate::port_task::ReadMessage;

/// Splits one delivered payload by the `{header, N}` option — the C's `desc->hsz` rule (see the
/// module docs for the four term-builder sites and the shape table).
///
/// `hsz` is the raw option value (`SocketOptions::header`), cast to the C's `unsigned int` here so
/// a negative option takes the flat-list branch exactly as the C's does.
pub(super) fn split_payload(hsz: i32, payload: Vec<u8>) -> ReadMessage {
    // `desc->hsz` is an `unsigned int` (`:6931`), so a negative `{header, N}` is the huge unsigned
    // that makes `hsz > len` true.
    let hsz = hsz as u32 as usize;
    if hsz > payload.len() {
        // The flat-list branch: the whole payload is the (already complete) header, with **no**
        // binary tail — `LOAD_STRING(buf, len)`.
        return ReadMessage {
            header: payload,
            body: None,
        };
    }
    // The binary branch: `<<payload[hsz..]>>` as the tail, consed onto `payload[..hsz]` (an empty
    // header leaves the binary bare, which is the `hsz == 0` shape).
    let mut header = payload;
    let body = header.split_off(hsz);
    ReadMessage {
        header,
        body: Some(body),
    }
}
