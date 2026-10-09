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

//! The `tcp_inet` **socket-option vocabulary** (`INET_REQ_SETOPTS`/`INET_REQ_GETOPTS`).
//!
//! The `setopts`/`getopts` control verbs speak the C's option wire verbatim, so every reply
//! byte can be cross-checked against the oracle:
//!
//! * `setopts` receives a sequence of **five-byte** records `[opt, val_be32]` — the C's
//!   `opt = *ptr++; ival = get_int32(ptr);` (`inet_set_opts`, `inet_drv.c:6915`-`:6918`, which
//!   walks the payload with `while (len >= 5)`, so a trailing partial record is ignored) — and
//!   answers the C's `ctl_reply_ok` `[1]`;
//! * `getopts` receives a sequence of **one-byte** option codes (the C's `while(len--)`, which
//!   is why an empty payload answers a bare `[1]`) and answers `[1]` followed by one
//!   `[opt, val_be32]` record per requested code, **in request order** (the C's
//!   `*ptr++ = opt; put_int32(ival, ptr);`, `inet_fill_opts`, `inet_drv.c:9141`-`:9751`).
//!
//! # The served options
//!
//! Four codes are served, each a direct `setsockopt(2)`/`getsockopt(2)` — the options whose
//! value is a plain `int`:
//!
//! | code | `gen_tcp` option | socket option | C constant |
//! |---|---|---|---|
//! | `1` ([`INET_OPT_KEEPALIVE`]) | `keepalive` | `SO_KEEPALIVE` | `inet_drv.c:875` |
//! | `6` ([`INET_OPT_SNDBUF`]) | `sndbuf` | `SO_SNDBUF` | `inet_drv.c:880` |
//! | `7` ([`INET_OPT_RCVBUF`]) | `recbuf` | `SO_RCVBUF` | `inet_drv.c:881` |
//! | `10` ([`TCP_OPT_NODELAY`]) | `nodelay` | `TCP_NODELAY` | `inet_drv.c:884` |
//!
//! The C reads these four from `SOL_SOCKET`, except `nodelay` which is `IPPROTO_TCP`
//! (`inet_drv.c:7286`-`:7321`, `:7497`-`:7504`; `:9429`-`:9450`).
//!
//! A code this floor does not serve is the C's `default: return -1`
//! (`inet_set_opts`, `inet_drv.c:7914`; `inet_fill_opts`, `:9719`), which the `inet_ctl` arms
//! turn into `ctl_error(EINVAL)` = `[0] ++ "einval"` (`:11062`, `:11128`) — the whole reply,
//! never a partial one, so a mixed request like `[keepalive, 200]` answers exactly that.
//!
//! # Faithfully mirrored quirks
//!
//! The oracle's observable behaviour for these two verbs has three quirks, each measured
//! against `erl` and reproduced here byte for byte:
//!
//! 1. **A configured buffer reports the kernel's value, not the requested one.** Linux doubles
//!    a `SO_RCVBUF`/`SO_SNDBUF` request, so `setopts` of 4096 reads back as 8192 (and the
//!    default `recbuf` of a fresh AF_INET stream socket reads back as 131072). Reporting the
//!    kernel's own `getsockopt(2)` value reproduces that exactly, with nothing hard-coded.
//! 2. **`setopts` answers `[1]` even when the kernel rejects the option.** The C leaves
//!    `propagate = 0` for this option family, so a failed `setsockopt` is deliberately not
//!    reported (`inet_set_opts`, `inet_drv.c:7898`, `:7923`-`:7942` — "not all errors can be
//!    propagated for BC reasons"). A caller that cares reads the value back with `getopts`.
//! 3. **Neither verb has an `IS_OPEN` guard.** The C dispatches `INET_REQ_SETOPTS`/
//!    `INET_REQ_GETOPTS` (`:11111`/`:11046`) without one, so on a port with no socket at all
//!    `setopts` answers `[1]` (the `setsockopt` on the invalid descriptor fails, and is not
//!    propagated) and `getopts` answers `[1]` with **no** record (the `getsockopt` fails and
//!    the C's `TRUNCATE_TO(0, ptr)` drops the entry, `:9739`) — but an unserved code is still
//!    `[0]einval`, because the option switch precedes the syscall.
//!
//! The one deliberate non-mirror is an **unrecoverable** `getsockopt(2)` failure on a live
//! socket (something the four served options cannot raise): the C silently drops that record
//! and answers a *shorter* reply, which would be a wrong result, so it surfaces as the typed
//! `BeamError::Io` instead.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`.

use std::os::unix::io::RawFd;

use beam_common::Result;

use super::sys::{get_socket_int_option, set_socket_int_option};
use super::wire::{ctl_ok, ctl_xerror};

/// The `keepalive` option code (`INET_OPT_KEEPALIVE`, `inet_drv.c:875`) — `SO_KEEPALIVE`.
pub const INET_OPT_KEEPALIVE: u8 = 1;

/// The `sndbuf` option code (`INET_OPT_SNDBUF`, `inet_drv.c:880`) — `SO_SNDBUF`.
pub const INET_OPT_SNDBUF: u8 = 6;

/// The `recbuf` option code (`INET_OPT_RCVBUF`, `inet_drv.c:881`) — `SO_RCVBUF`.
pub const INET_OPT_RCVBUF: u8 = 7;

/// The `nodelay` option code (`TCP_OPT_NODELAY`, `inet_drv.c:884`) — `TCP_NODELAY`.
pub const TCP_OPT_NODELAY: u8 = 10;

/// The bytes of one `[opt, val_be32]` record, the C's `while (len >= 5)` unit.
const RECORD: usize = 5;

/// The `(level, name)` pair a served option code selects, or `None` for a code this floor
/// does not serve (the C's `default:` arm becoming `[0]einval`).
fn resolve(code: u8) -> Option<(libc::c_int, libc::c_int)> {
    match code {
        INET_OPT_KEEPALIVE => Some((libc::SOL_SOCKET, libc::SO_KEEPALIVE)),
        INET_OPT_SNDBUF => Some((libc::SOL_SOCKET, libc::SO_SNDBUF)),
        INET_OPT_RCVBUF => Some((libc::SOL_SOCKET, libc::SO_RCVBUF)),
        TCP_OPT_NODELAY => Some((libc::IPPROTO_TCP, libc::TCP_NODELAY)),
        _ => None,
    }
}

/// The `setopts` verb: applies a sequence of `[opt, val_be32]` records.
///
/// `fd` is the port's socket, or `None` while the port has none (the C's invalid descriptor):
/// the option codes are still validated — an unserved one answers `[0]einval` exactly as the
/// oracle does — but no syscall is attempted, and the reply is the C's `[1]`.
///
/// # Errors
///
/// Never for a served option: a rejected `setsockopt(2)` is not propagated, mirroring the C's
/// `propagate = 0` (see the module docs).
pub(super) fn set_options(fd: Option<RawFd>, arg: &[u8]) -> Result<Vec<u8>> {
    // `chunks_exact(RECORD)` is the C's `while (len >= 5)`: a trailing partial record is the
    // remainder and is ignored (`inet_set_opts`, `inet_drv.c:6908`).
    for record in arg.chunks_exact(RECORD) {
        let code = record[0];
        let value = i32::from_be_bytes([record[1], record[2], record[3], record[4]]);
        let Some((level, name)) = resolve(code) else {
            return Ok(ctl_xerror("einval"));
        };
        if let Some(fd) = fd {
            // Deliberately discarded, not swallowed by accident: the C leaves `propagate = 0`
            // for this option family, so a failed `setsockopt` still answers `ok`
            // (`inet_drv.c:7923`-`:7942`). `getopts` is how a caller learns the real value.
            let _ = set_socket_int_option(fd, level, name, value);
        }
    }
    Ok(ctl_ok())
}

/// The `getopts` verb: reads one `[opt, val_be32]` record per requested option **code**, in
/// request order.
///
/// `fd` is the port's socket, or `None` while the port has none. With no socket the C's
/// `getsockopt` fails and its entry is dropped, so every served code contributes nothing and
/// the reply is the bare `[1]`; an unserved code is still `[0]einval`.
///
/// # Errors
///
/// `BeamError::Io` when a served option's `getsockopt(2)` fails on a live socket — the
/// deliberate non-mirror of the C's silent truncation (see the module docs).
pub(super) fn get_options(fd: Option<RawFd>, arg: &[u8]) -> Result<Vec<u8>> {
    let mut reply = ctl_ok();
    for &code in arg {
        let Some((level, name)) = resolve(code) else {
            return Ok(ctl_xerror("einval"));
        };
        let Some(fd) = fd else {
            // The C's `getsockopt` on the invalid descriptor fails, so `TRUNCATE_TO(0, ptr)`
            // drops this entry (`inet_drv.c:9739`) and the reply stays the bare `[1]`.
            continue;
        };
        let value = get_socket_int_option(fd, level, name)?;
        // The C's `*ptr++ = opt; put_int32(ival, ptr);` (`inet_drv.c:9743`-`:9746`).
        reply.push(code);
        reply.extend_from_slice(&value.to_be_bytes());
    }
    Ok(reply)
}
