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

//! [`RamFileDriver`] — the built-in in-memory file driver (`"ram_file_drv"`).
//!
//! The Rust counterpart of the legacy C driver
//! `erts/emulator/drivers/common/ram_file_drv.c`, the driver `lib/kernel/src/ram_file.erl`
//! talks to. A port opened on it holds a byte buffer entirely in memory (the C's `RamFile`),
//! so `ram_file:open/2` gives a "file" that never touches the OS.
//!
//! # Command protocol
//!
//! The driver owns **no** file descriptor and registers nothing with the poller: it is
//! purely *command driven*. Every operation arrives as packed bytes through
//! `erlang:port_command/2` — the C's `rfile_command`, registered in the driver entry's
//! `output` slot — and the reply is written back as bytes for the port owner (the C's
//! `driver_output2`/`driver_output_binary`). [`Driver::output`] therefore drains the port's
//! output queue, treats the **first byte as the command word** and the rest as its
//! operands, and appends the reply to the port's input queue.
//!
//! # Integer ↔ name correspondence
//!
//! Every command has an integer (the C `RAM_FILE_*` opcode, the wire form `ram_file.erl`
//! sends) *and* a name (this crate's string-verb form of [`Driver::control`], mirroring the
//! `ram_file.erl` function names). Both drive exactly the same core
//! ([`RamFileDriver::execute`]), and the decimal integer is accepted as a command name too,
//! so the two spellings of a command are directly comparable.
//!
//! | int | name(s) | argument | reply |
//! |---|---|---|---|
//! | 1 | `open` | `flags:i32be` + initial content | `NUMBER 0` |
//! | 2 | `read` | `len:i32be` | `DATA n` + bytes |
//! | 3 | `lseek`, `position` | `offset:i32be`, `whence:i32be` | `NUMBER pos` |
//! | 4 | `write` | the data bytes | `NUMBER len` |
//! | 9 | `fsync`, `sync` | — | `OK` (or `ERROR EBADF`) |
//! | 14 | `truncate` | — | `OK` (or `ERROR EACCES`) |
//! | 17 | `pread` | `offset:i32be`, `len:i32be` | `DATA n` + bytes |
//! | 18 | `pwrite` | `offset:i32be` + data | `NUMBER len` |
//! | 19 | `fdatasync`, `datasync` | — | `OK` (or `ERROR EBADF`) |
//! | 30 | `get`, `get_file` | — | `DATA end` + whole file |
//! | 37 | `size`, `get_size` | — | `NUMBER end` |
//! | 38 | `advise` | `off:i64be, len:i64be, advice:i32be` (ignored) | `OK` (or `ERROR EBADF`) |
//! | 39 | `allocate` | `off:i64be, len:i64be` (ignored) | `OK` (or `ERROR EBADF`) |
//!
//! `close` is not a driver opcode at all: `ram_file:close/1` is `erlang:port_close/1`, so on
//! the port path teardown is the registry's reserved
//! [`PortControl::CLOSE`](crate::PortControl::CLOSE) op plus [`Driver::stop`]. The `close`
//! *name* this driver accepts exists for a direct caller and is exactly what [`Driver::stop`]
//! runs (reset to the inert, unopened state); it replies `OK` with no operands.
//!
//! # Reply encoding (byte-exact)
//!
//! * `OK` ([`RAM_FILE_RESP_OK`]) — one byte, `[0]`.
//! * `NUMBER` ([`RAM_FILE_RESP_NUMBER`]) — `[3]` + a 4-byte **big-endian** `i32`.
//! * `DATA` ([`RAM_FILE_RESP_DATA`]) — `[2]` + a 4-byte big-endian length `n` + `n` bytes.
//! * `ERROR` ([`RAM_FILE_RESP_ERROR`]) — `[1]` + the **lowercased POSIX errno name** as ASCII
//!   (`[1] ++ "ebadf"`), produced through [`crate::error::errno_atom`].
//!
//! The only errnos this driver produces are `EBADF`, `EINVAL`, `ENOMEM` and `EACCES`.
//!
//! # The in-memory file
//!
//! The logical file is modelled as a [`Vec<u8>`] whose `len()` **is** the C's `end` (the
//! "end of valid data" position). The C's separate `size` capacity field is a pure
//! allocation detail — no reply exposes it — so it has no counterpart here. The access mode
//! ([`RamFileDriver::flags`]) and the current position ([`RamFileDriver::position`]) are the
//! C's `flags`/`cur`, both signed exactly as the C's `int`/`ErlDrvSSizeT` are.
//!
//! A write that starts **past** the logical end must leave the bytes in between as **zero**
//! (the C's `ram_file_expand` zeroes every freshly allocated byte, and a later `get`/`read`
//! can observe the gap), so the buffer is extended with `0` before the data is copied in.
//!
//! # Errors
//!
//! An **operation-level** failure (a wrong access mode, a negative offset, an allocation
//! failure) is *not* a Rust error: it is the `ERROR <errno>` reply the C emits, so the
//! caller observes the same bytes the oracle does. A **protocol** failure — an unknown
//! command word or an argument too short for its command — is the typed
//! [`crate::error::badarg`], never a silent no-op and never a fabricated reply.
//!
//! Nothing here needs `unsafe`: the driver holds no OS resource.

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
use crate::error;

/// The driver's registered name — the string `port_info(Port, name)` reports for a
/// `ram_file` port and the key [`crate::DriverRegistry`] would use.
pub const DRIVER_NAME: &str = "ram_file_drv";

/// The `open` command: set the access mode and load the initial content.
pub const RAM_FILE_OPEN: u8 = 1;
/// The `read` command: read `len` bytes at the current position.
pub const RAM_FILE_READ: u8 = 2;
/// The `lseek` command: move the current position.
pub const RAM_FILE_LSEEK: u8 = 3;
/// The `write` command: write the operand bytes at the current position.
pub const RAM_FILE_WRITE: u8 = 4;
/// The `fsync` command (a no-op on an in-memory file, but it still requires an open mode).
pub const RAM_FILE_FSYNC: u8 = 9;
/// The `truncate` command: drop everything after the current position.
pub const RAM_FILE_TRUNCATE: u8 = 14;
/// The `pread` command: read at an explicit offset, leaving the position untouched.
pub const RAM_FILE_PREAD: u8 = 17;
/// The `pwrite` command: write at an explicit offset, leaving the position untouched.
pub const RAM_FILE_PWRITE: u8 = 18;
/// The `fdatasync` command (the `fsync` counterpart).
pub const RAM_FILE_FDATASYNC: u8 = 19;
/// The `get` command: return a copy of the whole logical file.
pub const RAM_FILE_GET: u8 = 30;
/// The `size` command: return the logical file length.
pub const RAM_FILE_SIZE: u8 = 37;
/// The `advise` command: accepted and ignored (its operands are dropped, as in the C).
pub const RAM_FILE_ADVISE: u8 = 38;
/// The `allocate` command: accepted and ignored (its operands are dropped, as in the C).
pub const RAM_FILE_ALLOCATE: u8 = 39;

/// The `OK` response code (a bare `[0]` reply).
pub const RAM_FILE_RESP_OK: u8 = 0;
/// The `ERROR` response code: `[1]` + the lowercased errno name.
pub const RAM_FILE_RESP_ERROR: u8 = 1;
/// The `DATA` response code: `[2]` + a 4-byte big-endian length + that many bytes.
pub const RAM_FILE_RESP_DATA: u8 = 2;
/// The `NUMBER` response code: `[3]` + a 4-byte big-endian `i32`.
pub const RAM_FILE_RESP_NUMBER: u8 = 3;
/// The `INFO` response code — declared by the reference but never emitted by this driver.
pub const RAM_FILE_RESP_INFO: u8 = 4;

/// The `open` mode bit: reading is permitted.
pub const RAM_FILE_MODE_READ: i32 = 1;
/// The `open` mode bit: writing is permitted.
pub const RAM_FILE_MODE_WRITE: i32 = 2;
/// Both `open` mode bits (`READ | WRITE`).
pub const RAM_FILE_MODE_READ_WRITE: i32 = 3;

/// The `lseek` whence: the offset is absolute.
pub const RAM_FILE_SEEK_SET: i32 = 0;
/// The `lseek` whence: the offset is relative to the current position.
pub const RAM_FILE_SEEK_CUR: i32 = 1;
/// The `lseek` whence: the offset is relative to the end of the file.
pub const RAM_FILE_SEEK_END: i32 = 2;

/// The in-memory file driver (`"ram_file_drv"`).
///
/// See the [module documentation](self) for the command protocol and the reply encoding.
/// The driver is inert until an [`RAM_FILE_OPEN`] command gives it an access mode: an
/// unopened driver answers `ERROR EBADF` to every operation that needs one, `NUMBER 0` to
/// `size`, and `DATA 0` to `get` — exactly like a fresh C `RamFile`.
#[derive(Default)]
pub struct RamFileDriver {
    /// The access mode (the C `RamFile::flags`); `0` means "no access mode set yet".
    flags: i32,
    /// The logical file: `buf.len()` **is** the C's `end`.
    buf: Vec<u8>,
    /// The current position (the C `RamFile::cur`); never negative.
    cur: i64,
}

impl RamFileDriver {
    /// A fresh, inert driver: no access mode, an empty file and the position at `0`.
    pub fn new() -> RamFileDriver {
        RamFileDriver::default()
    }

    /// The access mode (`READ`/`WRITE` bits), `0` before the first `open`.
    pub fn flags(&self) -> i32 {
        self.flags
    }

    /// The current position (`cur`).
    pub fn position(&self) -> i64 {
        self.cur
    }

    /// The logical file length (`end`) — what the `size` command reports.
    pub fn size(&self) -> i64 {
        self.buf.len() as i64
    }

    /// Whether the driver has a non-zero access mode.
    ///
    /// This is the reference's own "is it open?" test — the C gates `lseek`, `fsync`,
    /// `fdatasync`, `advise` and `allocate` on `flags == 0`. Note that an `open` carrying
    /// mode `0` therefore leaves the driver behaving as if unopened (the only operations it
    /// still serves are `size` and `get`), which is precisely what the C does.
    pub fn is_open(&self) -> bool {
        self.flags != 0
    }

    /// Returns the driver to its inert, unopened state: no access mode, an empty file and
    /// the position at `0`.
    ///
    /// The `close` verb and [`Driver::stop`] both run this.
    pub fn reset(&mut self) {
        self.flags = 0;
        self.buf = Vec::new();
        self.cur = 0;
    }

    /// Runs one byte-level driver command: `command` is the command word (the first byte of
    /// a `port_command` payload) and `arg` the operand bytes that followed it.
    ///
    /// This is the single shared core behind both transports — the [`Driver::output`]
    /// `port_command` path and the [`Driver::control`] named-verb path (which funnels its
    /// verb into the matching integer). It returns the reply bytes to be handed back to the
    /// port owner, exactly as the C's `rfile_command` writes them.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::badarg`] for an unknown command word, and for an argument too
    /// short for the command that reads it. Operation-level failures (a wrong access mode, a
    /// negative offset, an allocation failure) are reported as `ERROR <errno>` **replies**,
    /// not as Rust errors.
    pub fn execute(&mut self, command: u8, arg: &[u8]) -> Result<Vec<u8>> {
        match command {
            RAM_FILE_OPEN => self.open_command(arg),
            RAM_FILE_READ => self.read_command(arg),
            RAM_FILE_LSEEK => self.lseek_command(arg),
            RAM_FILE_WRITE => Ok(self.write_at(self.cur, arg, true)),
            RAM_FILE_FSYNC | RAM_FILE_FDATASYNC | RAM_FILE_ADVISE | RAM_FILE_ALLOCATE => {
                Ok(self.require_open())
            }
            RAM_FILE_TRUNCATE => Ok(self.truncate_command()),
            RAM_FILE_PREAD => self.pread_command(arg),
            RAM_FILE_PWRITE => self.pwrite_command(arg),
            RAM_FILE_GET => Ok(data_reply(&self.buf)),
            RAM_FILE_SIZE => Ok(number_reply(self.buf.len() as i64)),
            _ => Err(error::badarg()),
        }
    }

    /// `open`: take the access mode from the first four operand bytes and reset the file to
    /// the remaining bytes, replying `NUMBER 0` (the C's unused result slot).
    fn open_command(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        let flags = read_i32(arg)?;
        let content = &arg[4..];
        let mut buf = Vec::new();
        if buf.try_reserve(content.len()).is_err() {
            return Ok(error_reply(libc::ENOMEM));
        }
        buf.extend_from_slice(content);
        // The C assigns `f->flags` *before* re-initialising the buffer, so a failed
        // allocation still leaves the new mode in place.
        self.flags = flags;
        self.buf = buf;
        self.cur = 0;
        Ok(number_reply(0))
    }

    /// `read`: read at the current position and advance it (the C's `location == NULL` form).
    fn read_command(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        let len = read_i32(arg)? as i64;
        Ok(self.read_at(self.cur, len, true))
    }

    /// `pread`: read at an explicit offset and leave the position untouched.
    ///
    /// The offset is validated **before** the access mode, matching the C's `rfile_command`
    /// (which rejects a negative offset itself and only then calls `ram_file_read`).
    fn pread_command(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        let offset = read_i32(arg)? as i64;
        if offset < 0 {
            return Ok(error_reply(libc::EINVAL));
        }
        let len = read_i32(&arg[4..])? as i64;
        Ok(self.read_at(offset, len, false))
    }

    /// `pwrite`: write at an explicit offset and leave the position untouched.
    ///
    /// As for [`RamFileDriver::pread_command`], the offset is validated before the mode.
    fn pwrite_command(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        let offset = read_i32(arg)? as i64;
        if offset < 0 {
            return Ok(error_reply(libc::EINVAL));
        }
        Ok(self.write_at(offset, &arg[4..], false))
    }

    /// `lseek`: reposition the file, honouring the seek mode.
    fn lseek_command(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        let offset = read_i32(arg)? as i64;
        let whence = read_i32(&arg[4..])?;
        if self.flags == 0 {
            return Ok(error_reply(libc::EBADF));
        }
        let pos = match whence {
            RAM_FILE_SEEK_SET => offset,
            RAM_FILE_SEEK_CUR => self.cur + offset,
            RAM_FILE_SEEK_END => self.buf.len() as i64 + offset,
            _ => return Ok(error_reply(libc::EINVAL)),
        };
        if pos < 0 {
            return Ok(error_reply(libc::EINVAL));
        }
        self.cur = pos;
        Ok(number_reply(pos))
    }

    /// `truncate`: the logical file ends at the current position.
    ///
    /// Shrinking drops the tail; truncating past the end extends the file with zeros, which
    /// is what the C's unwritten (`end = cur`) region reads back as — see the module docs.
    fn truncate_command(&mut self) -> Vec<u8> {
        if self.flags & RAM_FILE_MODE_WRITE == 0 {
            return error_reply(libc::EACCES);
        }
        let cur = self.cur.max(0) as usize;
        self.buf.resize(cur, 0);
        ok_reply()
    }

    /// The `flags == 0` gate shared by `fsync`/`fdatasync`/`advise`/`allocate`.
    fn require_open(&self) -> Vec<u8> {
        if self.flags == 0 {
            error_reply(libc::EBADF)
        } else {
            ok_reply()
        }
    }

    /// The C `ram_file_read`: read `len` bytes at `pos`, replying `DATA`.
    ///
    /// Reading at or past the logical end yields an empty `DATA` reply, and the amount is
    /// clamped to what is left, so a read can never run past the end. Only the
    /// non-located (`read`) form moves the position, and only by the amount actually read.
    fn read_at(&mut self, pos: i64, len: i64, advance: bool) -> Vec<u8> {
        if self.flags & RAM_FILE_MODE_READ == 0 {
            return error_reply(libc::EBADF);
        }
        if pos < 0 || len < 0 {
            return error_reply(libc::EINVAL);
        }
        let end = self.buf.len() as i64;
        let count = if pos < end { len.min(end - pos) } else { 0 };
        let start = pos as usize;
        let reply = data_reply(&self.buf[start..start + count as usize]);
        if advance {
            self.cur = pos + count;
        }
        reply
    }

    /// The C `ram_file_write`: write `data` at `pos`, replying `NUMBER <len>`.
    ///
    /// A write that starts past the logical end zero-fills the gap before the data, and the
    /// logical end grows to the furthest byte written. Only the non-located (`write`) form
    /// moves the position.
    fn write_at(&mut self, pos: i64, data: &[u8], advance: bool) -> Vec<u8> {
        if self.flags & RAM_FILE_MODE_WRITE == 0 {
            return error_reply(libc::EBADF);
        }
        let len = data.len() as i64;
        if pos < 0 || pos + len < 0 {
            return error_reply(libc::EINVAL);
        }
        let new_end = pos + len as i64;
        if new_end > self.buf.len() as i64 {
            let extra = (new_end - self.buf.len() as i64) as usize;
            if self.buf.try_reserve(extra).is_err() {
                return error_reply(libc::ENOMEM);
            }
            // The C zeroes every freshly allocated byte, so the gap this write may span is
            // observable as zeros through `get`/`read`.
            self.buf.resize(new_end as usize, 0);
        }
        let start = pos as usize;
        self.buf[start..start + data.len()].copy_from_slice(data);
        if advance {
            self.cur = new_end;
        }
        number_reply(len)
    }
}

impl Driver for RamFileDriver {
    fn name(&self) -> &str {
        DRIVER_NAME
    }

    /// A no-op: the driver owns no descriptor and registers nothing with the poller.
    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    /// The `port_command` transport: run the command word at the head of the output queue
    /// and push its reply onto the input queue.
    ///
    /// The whole queued payload is consumed — the driver reports exactly its length, as the
    /// consume-what-you-report contract (see [`Driver::output`]) requires — and the reply is
    /// left for the port to deliver to the owner as `{Port, {data, Reply}}`.
    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let queued = ctx.output().front();
        if queued.is_empty() {
            return Ok(0);
        }
        let command = queued[0];
        let arg = queued[1..].to_vec();
        let consumed = queued.len();
        ctx.output().consume(consumed);
        let reply = self.execute(command, &arg)?;
        ctx.input().push(&reply);
        Ok(consumed)
    }

    /// A no-op: the driver registers no descriptor, so read readiness is never meaningful.
    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    /// `false` — the C's `ram_file_driver_entry` has `control == NULL`
    /// (`erts/emulator/drivers/common/ram_file_drv.c:116`), so `erlang:port_control/3` on a
    /// `ram_file_drv` port is the oracle's `badarg` and never reaches the driver.
    ///
    /// The opcode core stays reachable through [`Driver::output`] (this driver's actual transport,
    /// the C's `rfile_command`), and [`Driver::control`] keeps the named-verb form as a crate-only
    /// test/driver aid — it is simply not on the `port_control` path, exactly as in the C.
    fn has_control(&self) -> bool {
        false
    }

    /// The named-verb transport: map `command` to its opcode and run the shared core.
    ///
    /// Accepts both the C-style opcode names (`open`, `read`, `lseek`, `write`, `fsync`,
    /// `truncate`, `pread`, `pwrite`, `fdatasync`, `get`, `size`, `advise`, `allocate`) and
    /// the `ram_file.erl` function names (`position`, `sync`, `datasync`, `get_file`,
    /// `get_size`), plus the **decimal opcode itself** (e.g. `"38"`), so the integer↔name
    /// correspondence can be driven either way. `close` returns the driver to its inert
    /// state and replies `OK`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::badarg`] for an unknown verb.
    fn control(
        &mut self,
        _ctx: &mut DriverContext<'_>,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        match command {
            "open" => self.execute(RAM_FILE_OPEN, arg),
            "read" => self.execute(RAM_FILE_READ, arg),
            "lseek" | "position" => self.execute(RAM_FILE_LSEEK, arg),
            "write" => self.execute(RAM_FILE_WRITE, arg),
            "fsync" | "sync" => self.execute(RAM_FILE_FSYNC, arg),
            "truncate" => self.execute(RAM_FILE_TRUNCATE, arg),
            "pread" => self.execute(RAM_FILE_PREAD, arg),
            "pwrite" => self.execute(RAM_FILE_PWRITE, arg),
            "fdatasync" | "datasync" => self.execute(RAM_FILE_FDATASYNC, arg),
            "get" | "get_file" => self.execute(RAM_FILE_GET, arg),
            "size" | "get_size" => self.execute(RAM_FILE_SIZE, arg),
            "advise" => self.execute(RAM_FILE_ADVISE, arg),
            "allocate" => self.execute(RAM_FILE_ALLOCATE, arg),
            "close" => {
                self.reset();
                Ok(ok_reply())
            }
            other => match other.parse::<u8>() {
                Ok(opcode) => self.execute(opcode, arg),
                Err(_) => Err(error::badarg()),
            },
        }
    }

    /// Releases the buffer, returning the driver to its inert state.
    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        self.reset();
        Ok(())
    }
}

/// Creates a fresh [`RamFileDriver`] boxed as a [`Driver`] — the factory shape
/// [`crate::DriverRegistry::register`] takes.
///
/// # Errors
///
/// Never fails today (the driver allocates nothing at construction); the fallible signature
/// is the [`DriverFactory`](crate::DriverFactory) contract.
pub fn make_ram_file() -> Result<Box<dyn Driver>> {
    Ok(Box::new(RamFileDriver::new()))
}

/// The `OK` reply: the bare response code.
fn ok_reply() -> Vec<u8> {
    vec![RAM_FILE_RESP_OK]
}

/// The `NUMBER` reply: the response code plus the value truncated to an `i32`, big-endian.
fn number_reply(value: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.push(RAM_FILE_RESP_NUMBER);
    out.extend_from_slice(&(value as i32).to_be_bytes());
    out
}

/// The `DATA` reply: the response code plus the length (big-endian `i32`) plus the bytes.
fn data_reply(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + data.len());
    out.push(RAM_FILE_RESP_DATA);
    out.extend_from_slice(&(data.len() as i32).to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// The `ERROR` reply: the response code plus the lowercased POSIX errno name.
///
/// The name comes from [`crate::error::errno_atom`], the crate's single errno→atom table —
/// the same string the C's `erl_errno_id` lowercased. An errno that table does not name
/// falls back to the reference's own generic spelling rather than inventing an atom.
fn error_reply(errno: i32) -> Vec<u8> {
    let mut out = vec![RAM_FILE_RESP_ERROR];
    match error::errno_atom(errno) {
        Some(name) => out.extend_from_slice(name.as_bytes()),
        None => out.extend_from_slice(format!("errno_{errno}").as_bytes()),
    }
    out
}

/// Reads a big-endian `i32` from the first four bytes of `bytes`.
///
/// # Errors
///
/// Returns [`crate::error::badarg`] when fewer than four operand bytes are present.
fn read_i32(bytes: &[u8]) -> Result<i32> {
    if bytes.len() < 4 {
        return Err(error::badarg());
    }
    Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
#[path = "ram_file_tests.rs"]
mod tests;
