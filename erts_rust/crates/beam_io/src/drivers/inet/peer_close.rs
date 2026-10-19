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

//! The C's **passive peer-close teardown**: the half of a latched close that closes the
//! descriptor.
//!
//! A passive socket's close path does not merely arm the reply ladders — it **tears the
//! descriptor down**, and that is observable through the descriptor-state requests. The three
//! discovery sites each run `tcp_desc_close` (`inet_drv.c:11932`-`:11939`):
//!
//! * `tcp_recv_closed` (`:12783`-`:12846`), passive arm: `if (desc->inet.exitf) tcp_desc_close(desc);
//!   else desc_close_read(INETP(desc));` (`:12824`-`:12827`);
//! * `tcp_recv_error` (`:12849`-`:12890`), passive arm: the same split (`:12872`-`:12875`);
//! * `tcp_send_or_shutdown_error` (`:13613`-`:13690`), passive arm: `tcp_desc_close(desc)`
//!   **unconditionally** (`:13670`), the `{exit_on_close, …}` option never consulted.
//!
//! `tcp_desc_close` is `tcp_clear_input` + `tcp_clear_output` + `erl_inet_close`, and
//! `erl_inet_close` (`:5098`-`:5117`) is the part that matters here:
//!
//! ```c
//! if ((desc->prebound == 0) && (desc->state & INET_F_OPEN)) {
//!     desc_close(desc);                 /* descriptor gone, state &= ~CONNECTED (:5078) */
//!     desc->state = INET_STATE_CLOSED;  /* :5106 */
//! } else if (desc->prebound && …) {
//!     sock_select(…, 0);                /* registration only — the fd is the caller's */
//! }
//! ```
//!
//! So five control replies change after such a close, which is why this half exists:
//!
//! | request | before | after |
//! |---|---|---|
//! | `GETSTATUS`(16) | `[1,0,0,0,5]` | `[1,0,0,0,0]` (the `0` of `INET_STATE_CLOSED`) |
//! | `GETFD`(14) | `[1] ++ fd_be32` | `[0]"einval"` |
//! | `PEER`(4) | `[1] ++ addr` | `[0]"enotconn"` |
//! | `NAME`(5) | `[1] ++ addr` | `[0]"ebadf"` |
//! | `OPEN`(1) | `[0]"exbadseq"` | `[1]` (and a fresh socket) |
//!
//! `GETTYPE`(15) is **not** affected: `inet.stype`/`inet.sfamily` are cached at `open`
//! (`:5214`-`:5216`) and no close clears them.
//!
//! The teardown is deliberately **not** [`TcpInetDriver::close_socket`], which also resets the
//! peer-close latch (and `read_eof`): the C keeps `desc->tcp_add_flags` across `tcp_desc_close`,
//! so `TCP_ADDF_DELAYED_CLOSE_SEND`/`_RECV`/`_ECONNRESET` survive it and are what make the next
//! `send` answer `closed` and the next armed `recv` answer `closed`/`econnreset` (`:13678`,
//! `:13683`, `:12279`-`:12288`). `close_socket` is the *port* teardown (the `close` verb,
//! `Driver::stop`, `Drop`), where starting over is right; here the connection's history must be
//! kept.
//!
//! **Out of scope:** the *active*-mode arm of the same three sites — the C's
//! `driver_exit(desc->inet.port, 0)` under `{exit_on_close,true}` (`:12831`,
//! `tcp_recv_error`'s `:12876`) — kills the port, which changes consumer-visible behaviour
//! (`{tcp_closed, Port}` plus a dead port) rather than only these replies. It stays as it is.
//! An `{active, …}` socket therefore never takes this teardown: see
//! [`TcpInetDriver::tears_down_on_close`].

use super::sys::close_fd;
use super::{CloseSource, InetState, TcpInetDriver};

impl TcpInetDriver {
    /// Whether the C would **tear the descriptor down** for a close discovered this way.
    ///
    /// The three discovery sites' passive arms, each read off the source:
    ///
    /// * a **write** discovery (`tcp_send_or_shutdown_error`, `inet_drv.c:13652`-`:13684`) runs
    ///   `tcp_desc_close` unconditionally once the socket is passive (`:13670`) — the
    ///   `{exit_on_close, …}` option plays no part there;
    /// * a **read** discovery (`tcp_recv_closed` `:12819`-`:12828`, `tcp_recv_error`
    ///   `:12867`-`:12876`) picks `tcp_desc_close` only under `desc->inet.exitf`
    ///   (`{exit_on_close,true}`, the default — `inet_start` sets `desc->inet.exitf = 1`,
    ///   `:10929`) and takes `desc_close_read` (`:5083`-`:5098`, which only calls
    ///   `sock_select(…, FD_READ | FD_CLOSE, 0)` and never touches `desc->state`) otherwise;
    /// * an **active** socket never reaches either arm — the C kills the port
    ///   (`driver_exit`, out of scope, see the module doc) or only drops read readiness.
    pub(super) fn tears_down_on_close(&self, source: CloseSource) -> bool {
        if self.options.active != 0 {
            return false;
        }
        match source {
            CloseSource::Write { .. } => true,
            CloseSource::Read { .. } => self.options.exit_on_close != 0,
        }
    }

    /// Closes the owned descriptor and marks the port closed — the `erl_inet_close` half of
    /// `tcp_desc_close` (`inet_drv.c:5098`-`:5117`), **without** the peer-close latch reset that
    /// [`TcpInetDriver::close_socket`] adds (see the module doc).
    ///
    /// The framing buffer goes with the descriptor (`tcp_clear_input`, `:11706`, which
    /// `tcp_desc_close` runs before the ownership test), so a partial `{packet, N}` frame
    /// buffered for the dead connection is dropped. A **prebound** descriptor is not this
    /// driver's to close and the C keeps both `desc->s` and `desc->state` for one (the `else if`
    /// arm above): nothing is closed and the state is kept, exactly as
    /// [`TcpInetDriver::close_socket`] does.
    ///
    /// Returns whether this driver owned the descriptor (i.e. whether it was really closed).
    /// The poller registration is **not** touched here — every caller de-registers first.
    pub(super) fn close_descriptor(&mut self) -> bool {
        self.pending_input.clear();
        if self.prebound {
            return false;
        }
        if let Some(fd) = self.socket.take() {
            close_fd(fd);
        }
        self.state = InetState::Closed;
        true
    }
}
