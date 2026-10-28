// %CopyrightBegin%
//
// Copyright Ericsson AB 2025. All Rights Reserved.
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

//! `prim_file:sendfile/8` — the userspace sendfile.
//!
//! The oracle's `prim_file:sendfile/8` (`erts/preloaded/src/prim_file.erl:420-439`)
//! advises the descriptor and then asks the inet driver to move the bytes
//! (`prim_inet:sendfile/4` → `TCP_REQ_SENDFILE`, opcode 45). This runtime's
//! `beam_io` refuses opcode 45 (`beam_io/src/drivers/inet/mod.rs:2254-2257`,
//! `:2667-2668`), so the driver path would answer `{error,einval}` and a
//! `cowboy_static` reply would emit its headers and then nothing. This module is
//! the substitution: it reads the requested extent from the descriptor's own host
//! file and writes it through the **port command seam** (`port_command/2`'s own
//! path), so the bytes reach the socket and no opcode 45 is involved.
//!
//! # The oracle's answer matrix
//!
//! Measured on the host oracle (OTP 29.1.2 / erts-17.1.1) with a 26-byte file
//! `abcdefghijklmnopqrstuvwxyz`, a `[read,raw,binary]` descriptor and a loopback
//! TCP socket, `prim_file:sendfile(Fd, S, Off, Len, [], Headers, Trailers, [])`:
//!
//! | call | oracle | peer received |
//! |---|---|---|
//! | `Off=0, Len=26` | `{ok,26}` | `<<"abcdefghijklmnopqrstuvwxyz">>` |
//! | `Off=7, Len=7` | `{ok,7}` | `<<"hijklmn">>` |
//! | `Len=0` | `{ok,26}` | all 26 |
//! | `Off=20, Len=100` | `{ok,6}` | `<<"uvwxyz">>` |
//! | `Off=100` (past EOF) | `{ok,0}` | nothing |
//! | `Len=1 bsl 60` | `{ok,26}` | all 26 |
//! | `Off=1 bsl 60` | `{ok,0}` | nothing |
//! | `Len=(1 bsl 63)-1` | `{ok,26}` | all 26 |
//! | `Len=1 bsl 63`, `Off/Len` negative | `{error,einval}` | nothing |
//! | non-empty `Headers`/`Trailers` | `{error,enotsup}` | nothing |
//! | non-port socket | `{error,badarg}` | nothing |
//! | float or atom `Offset` | `{error,badarg}` | nothing |
//! | closed descriptor | `{error,badarg}` | nothing |
//! | socket owned by another process | `{error,not_owner}` | nothing |
//! | closed TCP socket | `{error,einval}` | nothing |
//! | live non-inet spawn port (`cat`) | `{error,einval}` | nothing |
//! | unconnected `{spawn_driver,"tcp_inet"}` | `{error,enotconn}` | nothing |
//!
//! `file:sendfile/5` maps our `{error,enotsup}` onto OTP's own Erlang
//! `sendfile_fallback` (`lib/kernel/src/file.erl:2772-2810`), which this runtime
//! supports — so the `{ok,26}` rows above are reachable through `file:sendfile`
//! too (`file:sendfile(Fd, SO, 0, 26, [{chunk_size,1024}])` ⇒ `{ok,26}` measured).
//!
//! # Check order (each answer is the measured row above)
//!
//! 1. non-nil `Headers`/`Trailers` ⇒ the **value** `{error,enotsup}`;
//! 2. a malformed descriptor ⇒ `{error,badarg}` (the module's documented
//!    divergence: the oracle raises `function_clause` through `get_fd_data/1`);
//! 3. `check_owner` ⇒ the `not_on_controlling_process` **raise** (as the oracle's
//!    `get_fd_data/1` does);
//! 4. a closed descriptor ⇒ `{error,badarg}` (`get_handle/1` answers a tuple,
//!    which is `not is_binary` in `prim_inet:sendfile/4`'s guard);
//! 5. `Socket`/`Offset`/`Bytes` shape ⇒ `{error,badarg}` (a float is `badarg`
//!    because the guard is `is_integer/1`, unlike `advise`'s `einval`);
//! 6. port liveness/ownership ⇒ a dead port is `{error,einval}` (the oracle's
//!    `port_info(P, connected)` on a dead port is `undefined`, `prim_inet`'s
//!    `_Other` clause), another owner is `{error,not_owner}`;
//! 7. range ⇒ `{error,einval}` outside `0 .. (1 bsl 63)-1`, `Len = 0` means "to
//!    the end of the file";
//! 8. the socket must be a **connected `tcp_inet`** port ⇒ otherwise
//!    `{error,einval}` (not a TCP port) / `{error,enotconn}` (a TCP port with no
//!    connection);
//! 9. the payload cap (below) ⇒ a typed `unsupported` gap;
//! 10. read the extent, write it through `crate::port::busy_wait::issue`.
//!
//! # Documented divergences and residuals
//!
//! * A live port that is **not** a `tcp_inet` one answers `{error,einval}` here.
//!   The oracle answers per driver — `einval` for a spawn port, `exbadport` for
//!   `{spawn_driver,"udp_inet"}` and `einval` for `ram_file_drv` — because the
//!   C's `tcp_inet_command` dispatches opcode 45 out of its own table; this
//!   runtime has one answer for "not a TCP socket".
//! * `BytesSent` is `payload.len()`. The command seam hands the whole payload to
//!   the driver or fails the call, so it cannot report a short write; the C's
//!   `sendfile(2)` may send fewer bytes than asked.
//! * A zero-byte extent (offset at or past EOF, or an empty file) answers
//!   `{ok,0}` **without** issuing a port command — there is nothing to write.
//! * **The bounded form.** One call's payload is held in memory and this runtime
//!   has no zero-copy path, so an extent larger than [`MAX_SENDFILE_BYTES`]
//!   answers a typed `error::unsupported` naming the cap — decided **before** a
//!   single byte is read or written, so a refused call never half-sends. The
//!   residual is that a legitimate `file:sendfile/5` of a larger file answers
//!   that gap instead of falling back: the fallback is only taken on
//!   `{error,enotsup}`, and claiming `enotsup` for a self-imposed cap would be a
//!   lie about the capability. Files of 16 MiB and below — the `cowboy_static`
//!   shapes measured, 26 bytes and `Range: bytes=7-13` style extents included —
//!   are byte-exact.

use std::rc::Rc;

use beam_bifs::PortHook;
use beam_common::Result;
use beam_process::PortId;
use beam_term::{is_nil, make_small, Eterm};

use crate::error;

/// The cap on one call's payload, in bytes.
///
/// The command seam's `make_data` closure is `Fn() -> Result<Vec<u8>>` and may be
/// called twice across a busy wait, so the extent must be materialised in memory
/// and cloned; there is no zero-copy route to the driver in this runtime. 16 MiB
/// is several times the largest `cowboy_static` body measured and keeps one call's
/// transient at a bounded size. A larger extent is the typed `unsupported` gap
/// documented in the module docs — never a partial send.
const MAX_SENDFILE_BYTES: usize = 16 * 1024 * 1024;

/// The largest `Offset`/`Length` the oracle accepts (`(1 bsl 63) - 1`).
const SENDFILE_MAX: u64 = i64::MAX as u64;

/// `Offset`/`Bytes` classified for the range check.
///
/// `prim_inet:sendfile/4`'s guard is `is_integer/1` — so a bignum passes the guard
/// and is then range-checked, while a float or an atom is `badarg`.
enum Limit {
    /// An in-range magnitude.
    Value(u64),
    /// A negative or over-`SENDFILE_MAX` integer ⇒ `{error,einval}`.
    OutOfRange,
    /// Not an integer at all ⇒ `{error,badarg}`.
    NotInteger,
}

/// Classify one `Offset`/`Bytes` argument.
///
/// A bignum is `is_integer/1`-true, so it must reach the range check: a
/// non-negative one whose magnitude fits in a signed 64-bit word is in range
/// (`Len = 1 bsl 60` is measured `{ok,26}`), and anything wider or negative is
/// `{error,einval}` (`Len = 1 bsl 63` measured).
fn limit_arg(term: Eterm) -> Limit {
    match super::integer_arg(term) {
        super::IntegerArg::Value(value) if value < 0 => Limit::OutOfRange,
        super::IntegerArg::Value(value) => Limit::Value(value as u64),
        super::IntegerArg::Big => match beam_codec::term::big_parts(term) {
            Ok((true, _)) => Limit::OutOfRange,
            Ok((false, limbs)) => match limbs {
                [] => Limit::Value(0),
                [only] if *only <= SENDFILE_MAX => Limit::Value(*only),
                _ => Limit::OutOfRange,
            },
            Err(_) => Limit::NotInteger,
        },
        super::IntegerArg::NotInteger => Limit::NotInteger,
    }
}

/// The file length behind a live descriptor.
enum FileLength {
    /// The descriptor's host file is `n` bytes long.
    Length(u64),
    /// The descriptor is not open here any more (closed between the two reads).
    Missing,
    /// `metadata(2)` failed — an `einval`-class answer, never a silent zero.
    Failed,
}

impl super::RuntimeContext<'_> {
    /// `prim_file:sendfile(Fd, Socket, Offset, Bytes, _ChunkSize, Headers, Trailers, _Flags)`.
    ///
    /// The user-space substitution for the driver's `TCP_REQ_SENDFILE` (opcode 45,
    /// which `beam_io` refuses): the extent is read from the descriptor's own host
    /// file with `pread(2)` (so the descriptor's cursor does not move, as the C's
    /// `sendfile(2)` does not move it) and written through the port command seam.
    /// The answer matrix and its oracle measurements are this module's docs.
    ///
    /// # Errors
    ///
    /// The `not_on_controlling_process` raise for another process's descriptor, the
    /// typed `unsupported` gap past [`MAX_SENDFILE_BYTES`] (and for a port table
    /// already in use), plus whatever the port command seam reports — never a
    /// silent success. The oracle's `{error, …}` tuples are **values**, returned as
    /// terms (an `{error,enotsup}` from `Headers`/`Trailers` is what makes
    /// `file:sendfile/5` take OTP's own `sendfile_fallback`).
    pub(crate) fn prim_sendfile(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let [fd, socket, offset, bytes, _chunk_size, headers, trailers, _flags] = args else {
            return Err(crate::native::prim::function_clause());
        };
        let (fd, socket, offset, bytes, headers, trailers) =
            (*fd, *socket, *offset, *bytes, *headers, *trailers);

        // 1. The oracle's only two clauses with a non-empty `Headers`/`Trailers` answer
        //    the *value* `{error,enotsup}` (`prim_file.erl:438-439`).
        if !is_nil(headers) || !is_nil(trailers) {
            return self.error_tuple("enotsup");
        }

        // 2. The descriptor must decode; a malformed one is this module's documented
        //    `{error,badarg}` (the oracle raises `function_clause`).
        let Some(descriptor) = self.decode_descriptor(fd) else {
            return self.error_tuple("badarg");
        };

        // 3. Another process's descriptor keeps the oracle's raise.
        self.check_owner(&descriptor)?;

        // 4. A closed descriptor: `get_handle/1` answers a tuple, so
        //    `prim_inet:sendfile/4`'s `is_binary(FileHandle)` guard answers `badarg`.
        if !self.runtime.open_files_mut().contains_key(&descriptor.id) {
            return self.error_tuple("badarg");
        }

        // 5. `Socket` must be a port and `Offset`/`Bytes` integers (`is_integer/1`,
        //    so a float is `badarg`).
        let Ok(socket_id) = PortId::from_eterm(socket) else {
            return self.error_tuple("badarg");
        };
        let offset_arg = limit_arg(offset);
        let bytes_arg = limit_arg(bytes);
        if matches!(offset_arg, Limit::NotInteger) || matches!(bytes_arg, Limit::NotInteger) {
            return self.error_tuple("badarg");
        }

        // 6. Port liveness and ownership: the `{connected, Pid}` pair is what
        //    `erlang:port_info(S, connected)` answers. A dead port has no pair
        //    (`prim_inet`'s `_Other` clause ⇒ `einval`); another owner is `not_owner`.
        let connected = self.sendfile_port_owner(socket_id)?;
        let Some(owner) = connected else {
            return self.error_tuple("einval");
        };
        if owner != self.pid().to_eterm() {
            return self.error_tuple("not_owner");
        }

        // 7. Range: `Len = 0` is "to the end of the file" (`sendfile_1` substitutes
        //    `(1 bsl 63) - 1`), and anything outside `0 .. (1 bsl 63)-1` is `einval`.
        let (Limit::Value(offset), Limit::Value(bytes)) = (offset_arg, bytes_arg) else {
            return self.error_tuple("einval");
        };

        // 8. Only a connected `tcp_inet` port can be sent a payload here. A live port
        //    that is not one is `einval`; a TCP port with no connection is `enotconn`
        //    (the oracle's `{spawn_driver,"tcp_inet"}` row).
        match self.sendfile_tcp_state(socket_id)? {
            None => return self.error_tuple("einval"),
            Some(state) if !state.is_connected() => return self.error_tuple("enotconn"),
            Some(_) => {}
        }

        // 9. The extent: clamp the request to the file's length, then apply the cap
        //    **before** reading or writing anything, so a refused call sends nothing.
        let file_len = match self.sendfile_file_length(descriptor.id) {
            FileLength::Length(len) => len,
            FileLength::Missing => return self.error_tuple("badarg"),
            FileLength::Failed => return self.error_tuple("einval"),
        };
        let remaining = file_len.saturating_sub(offset);
        let wanted = if bytes == 0 { remaining } else { bytes };
        let take_u64 = wanted.min(remaining);
        let take = usize::try_from(take_u64).unwrap_or(usize::MAX);
        if take > MAX_SENDFILE_BYTES {
            return Err(error::unsupported(
                "prim_file:sendfile/8: the requested extent exceeds MAX_SENDFILE_BYTES \
                 (16 MiB) in src/native/prim_file/sendfile.rs; nothing was sent",
            ));
        }

        // 10. A zero-byte extent answers `{ok,0}` with no port command at all.
        if take == 0 {
            return self.ok_tuple(make_small(0));
        }

        // The read happens **before** the write: `make_data` is `Fn()` and may be
        // called twice across a busy wait, and no `open_files_mut()` borrow may be
        // held across the `&mut *self.runtime` the seam takes.
        let read = {
            let files = self.runtime.open_files_mut();
            match files.get(&descriptor.id) {
                None => Err(None),
                Some(file) => super::pread_extent(file, offset, take).map_err(Some),
            }
        };
        let payload = match read {
            Ok(payload) => payload,
            Err(None) => return self.error_tuple("badarg"),
            Err(Some(failure)) => return self.error_tuple_for(&failure),
        };
        if payload.is_empty() {
            return self.ok_tuple(make_small(0));
        }
        let sent = payload.len();

        // 11. The port command seam, exactly as `port_command/2` drives it.
        let may_pump = self.may_pump_scheduler();
        let owner = self.pid();
        let written = crate::port::busy_wait::issue(
            &mut *self.runtime,
            may_pump,
            owner,
            socket_id,
            move || Ok(payload.clone()),
            beam_io::PortCommandOptions::EMPTY,
        )?;
        match written {
            crate::port::busy_wait::PortWrite::Written => {
                // The seam hands the whole payload to the driver or fails, so the count
                // is the payload's length; the cap above keeps the cast exact.
                self.ok_tuple(make_small(sent as i64))
            }
            crate::port::busy_wait::PortWrite::Parked => {
                // The C's `erts_suspend` + re-run: the outcome door turns this into
                // `BifOutcome::Suspend` and the engine does not apply the instruction,
                // so this value is never observed. It is a well-formed one regardless.
                self.request_suspend();
                self.atom("ok")
            }
            // `EMPTY` options carry no `nosuspend` flag, so a busy port parks instead.
            crate::port::busy_wait::PortWrite::Suspended => Err(error::unsupported(
                "prim_file:sendfile/8: a nosuspend port command cannot complete a sendfile",
            )),
        }
    }

    /// The `{connected, Pid}` pair of `socket`, or `None` when the port cannot answer
    /// one (a dead port).
    ///
    /// The pairs come from the **one** producer, [`PortHook::port_info`], exactly as
    /// `port_info/1,2` reads them; the atom lease is taken and dropped inside this
    /// call and only looked *up* through, never across an intern.
    fn sendfile_port_owner(&mut self, socket: PortId) -> Result<Option<Eterm>> {
        let pairs = {
            let mut hook = crate::port::RuntimePorts::new(
                Rc::clone(&self.runtime.ports),
                self.runtime.code_server.atoms_handle(),
                self.pid(),
            );
            hook.port_info(socket)?
        };
        let mut connected = None;
        for (item, value) in pairs {
            if self.atom_name(item).as_deref() == Some("connected") {
                connected = Some(value);
            }
        }
        Ok(connected)
    }

    /// The [`beam_io`] TCP state of `socket`, or `None` when it is not a live
    /// `tcp_inet` port.
    ///
    /// # Errors
    ///
    /// The typed capability gap when the port table is already borrowed.
    fn sendfile_tcp_state(
        &mut self,
        socket: PortId,
    ) -> Result<Option<beam_io::drivers::inet::InetState>> {
        let ports = Rc::clone(&self.runtime.ports);
        let held = match ports.try_borrow() {
            Ok(registry) => registry
                .driver_as::<beam_io::drivers::inet::TcpInetDriver>(socket)
                .map(|driver| driver.state()),
            Err(_) => return Err(error::unsupported("the port table is already in use")),
        };
        Ok(held)
    }

    /// How long the descriptor's host file is.
    fn sendfile_file_length(&mut self, id: u64) -> FileLength {
        let files = self.runtime.open_files_mut();
        match files.get(&id) {
            None => FileLength::Missing,
            Some(file) => match file.metadata() {
                Ok(metadata) => FileLength::Length(metadata.len()),
                Err(_) => FileLength::Failed,
            },
        }
    }
}
