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

//! The `efile` driver: [`FdDriver`] plus the `efile` control family.
//!
//! `efile` is the emulator's file driver (`erts/emulator/drivers/unix/efile_drv.c`). Like
//! the C driver it is a thin byte-stream driver over a file descriptor, so this wrapper
//! **composes** an [`FdDriver`] (reusing all its registration, non-blocking read/write and
//! line/phase handling — nothing is duplicated) and adds the `efile`-specific control
//! commands a port sends through `port_control/3`.
//!
//! [`EfileDriver`] is built either over fds the caller already holds
//! ([`EfileDriver::read_only`], [`EfileDriver::write_only`], [`EfileDriver::connected`]) or
//! *unopened* ([`EfileDriver::unopened`], [`make_efile`]) — an unopened port is inert until
//! a `open` control command hands it a path to open, mirroring how a port is started before
//! it is told which file to serve.
//!
//! # Control commands
//!
//! Commands the *efile* family adds on top of [`FdDriver`]'s (`line`/`noline`/`text`/
//! `getopts`, which are delegated unchanged):
//!
//! | command | argument | effect / reply |
//! |---|---|---|
//! | `open` | `[<flags> ]<path>` | open the path (see *Open modes*) and attach it; no reply |
//! | `close` | — | deregister and close the attached read/write fd(s); no reply |
//! | `path` | — | reply the currently open path (empty when unopened) |
//! | `eof` | — | reply `true`/`false` for the latched end-of-file |
//! | `position` | optional offset | reply the resulting absolute position (see below) |
//! | `seek` | `<whence>[,<offset>]` | reposition and reply the resulting absolute position |
//! | `sync` | — | `fsync(2)` the attached fd; no reply |
//! | `truncate` | optional length | `ftruncate(2)` the attached fd; no reply |
//! | `binary` | — | read raw bytes and record [`FileDataMode::Binary`]; no reply |
//! | `list` | — | read raw bytes and record [`FileDataMode::List`]; no reply |
//!
//! `position` with no argument replies the current offset (`lseek(fd, 0, SEEK_CUR)`);
//! `position <N>` seeks to the absolute offset `N` (`SEEK_SET`, negative allowed). `seek`
//! takes a `whence` of `bof`/`cur`/`eof` and an optional signed decimal offset (default
//! `0`), mirroring the C's `EFILE_SEEK_{BOF,CUR,EOF}` (`prim_file_nif.h:50-54`). `truncate`
//! with no argument truncates **at the current position**, exactly as the C's `truncate/1`
//! does (`ftruncate(fd, lseek(fd, 0, SEEK_CUR))`, `unix_prim_file.c:693-704`); with an
//! argument it truncates to that absolute length. The position/sync/truncate verbs act on
//! the read fd when one is attached, else the write fd (for a read+write open the two are
//! `dup(2)`s of one file description, so either names the same offset).
//!
//! The `line`/`noline`/`text`/`getopts` verbs still delegate to the composed [`FdDriver`]
//! and control the **port framing** (line buffering). `binary`/`list` are the `file:`
//! *data representation* options: both read the file as a raw byte stream (the `file:`
//! module has no line framing), and only the mode recorded on the driver — exposed through
//! [`EfileDriver::data_mode`] — tells the runtime whether to render a chunk as a binary or
//! as a string. `binary` therefore both selects the raw framing phase and records
//! [`FileDataMode::Binary`].
//!
//! An unknown command, an unopened port given a file-position request, or an unparsable
//! argument is rejected with [`beam_common::BeamError::BadArg`].
//!
//! # Open modes
//!
//! The `open` argument is `[<flags> ]<path>`. The `<flags>` token is optional and comes
//! first, separated from the path by a space; it is recognised as a flag list only when
//! **every** comma-separated word is a known flag — otherwise the whole argument is the
//! path (which keeps the original `open <path>` read-only form working byte-for-byte, even
//! for a path containing spaces). A path whose first token *is* a flag list can be
//! disambiguated the usual way (`open ./read`).
//!
//! | flag word | effect |
//! |---|---|
//! | `read` | open for reading |
//! | `write` | open for writing (truncates when it is the only access flag, as `O_TRUNC`) |
//! | `append` | open for writing, positioned at end of file (`O_APPEND`; cancels `O_TRUNC`) |
//! | `exclusive` | fail with `EEXIST` if the path already exists (`O_EXCL`) |
//! | `sync` | synchronised writes (`O_SYNC`) |
//! | `binary` / `list` | select the [`FileDataMode`] (see above) |
//!
//! The mapping is the reference's `efile_translate_modelist`
//! (`erts/emulator/nifs/common/prim_file_nif.c:500-539`) plus `get_flags`
//! (`erts/emulator/nifs/unix/unix_prim_file.c:142-171`), reproduced exactly:
//! `read` alone ⇒ `O_RDONLY`; `write` alone ⇒ `O_TRUNC|O_WRONLY|O_CREAT`;
//! `read`+`write` ⇒ `O_RDWR|O_CREAT`; `append`/`exclusive` imply `write`; and with no access
//! flag at all the access defaults to `read`. `O_NONBLOCK|O_CLOEXEC` are always added —
//! the reactor cannot serve a blocking descriptor, and the driver must not leak its fd into
//! an `exec` — and the mode word is the C's `FILE_MODE` (`0666`). Opening a **directory** is
//! refused with `EISDIR`, exactly as the oracle's `open_file_is_dir` type check does
//! (`unix_prim_file.c:206-213`); `open(2)` alone would happily open one read-only.
//!
//! A failed `open` is reported as [`beam_common::BeamError::Io`] carrying the failing
//! `errno`; the `{error, <errno-atom>}` rendering (`enoent`, `eexist`, `eisdir`, …) is the
//! runtime's `prim_file` floor's job, not this driver's.
//!
//! # `read_ahead` and `delayed_write` (`prim_file` handoffs)
//!
//! These two `file:open/2` options are deliberately **not** implemented here: neither is an
//! `open(2)` flag, so a driver-level equivalent would be a fabrication.
//!
//! * The C's mode translator ignores them on purpose — *"Modes like 'raw', 'ram',
//!   'delayed_writes' etc are handled further up the chain"* (`prim_file_nif.c:521-524`) —
//!   and they appear nowhere in `efile_modes_t`.
//! * `read_ahead` is a **read cache** owned by `prim_file`: it sets `r_ahead_size` (default
//!   `64 bsl 10`) and hangs an Erlang-side `prim_buffer` off the file data
//!   (`erts/preloaded/src/prim_file.erl:516-521`), which `read/2` then refills and serves
//!   from, tracking the *user's* logical position separately from the real one
//!   (`prim_file.erl:487-497`). Reproducing it means owning that buffer and the
//!   logical-vs-real position bookkeeping — `prim_file` work, not a driver mode.
//! * `delayed_write` is a **write-buffering policy** owned by `file`: it batches successive
//!   `write/2` calls until `Size` bytes (`64 KiB` by default) or `Delay` ms have elapsed
//!   and only then hands them to the driver (`lib/kernel/src/file.erl:1302-1323`). It is a
//!   property of the Erlang-level writer, not of the underlying `open(2)`.
//!
//! Both are therefore recorded as [`beam_rt`]'s `prim_file` floor handoffs rather than
//! approximated here (see the crate `CONTEXT.md`).
//!
//! [`beam_rt`]: ../../../../beam_rt
//!
//! # Regular files
//!
//! [`EfileDriver::control`]'s `open` registers the fd with the `mio`-backed poller. Linux
//! `epoll` refuses to register **regular files** (`EPOLL_CTL_ADD` ⇒ `EPERM`), so a plain
//! file is served by the poller's `poll(2)`-based **fallback readiness source** instead
//! (see [`crate::poller`]): `open`/`read`/`close` (and writing through
//! [`EfileDriver::write_only`]/[`EfileDriver::connected`], or for a `write`/`append`
//! `open`) work on a regular file exactly as the reference runtime's `poll(2)`-based
//! readiness path makes them, and never fail with `EPERM`. Once a read reaches end-of-file
//! the composed [`FdDriver`] de-registers the fd (see its "Readiness model"), so the
//! always-ready regular file stops producing events.

use std::os::unix::io::RawFd;

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
use crate::drivers::fd::FdDriver;
use crate::error;

/// The `open(2)` mode word the reference uses (`unix_prim_file.c:55`, `FILE_MODE`).
///
/// It is masked by the process umask, so this creates files a `std::fs::File::create` would
/// also create under the usual `0022`.
const FILE_MODE: libc::mode_t = 0o666;

/// How a `file:`-style reader should render the bytes this driver delivers.
///
/// The `file:` module has no line framing, so both modes read the same raw byte stream; the
/// mode only tells the runtime whether to hand a chunk on as a binary
/// ([`FileDataMode::Binary`], the `binary` option) or as a string of code points
/// ([`FileDataMode::List`], the `list` option). It is the driver-level counterpart of
/// `file:open/2`'s `binary`/`list` mode words, which the C's translator likewise drops
/// before the `open(2)` call (`prim_file_nif.c:521-524`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FileDataMode {
    /// Deliver each chunk as a binary (the default).
    #[default]
    Binary,
    /// Deliver each chunk as a string (a list of code points).
    List,
}

/// The access mode an `open` flag set resolves to, after the reference's implications.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Access {
    /// `O_RDONLY`.
    Read,
    /// `O_TRUNC | O_WRONLY | O_CREAT` (minus `O_TRUNC` when `append` is also given).
    Write,
    /// `O_RDWR | O_CREAT`.
    ReadWrite,
}

/// The textual flag words an `open` argument may carry, before the reference's implications.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OpenFlags {
    read: bool,
    write: bool,
    append: bool,
    exclusive: bool,
    sync: bool,
}

impl OpenFlags {
    /// The access mode, mirroring `efile_translate_modelist`
    /// (`erts/emulator/nifs/common/prim_file_nif.c:529-536`): `append`/`exclusive` are
    /// documented as "open for writing", and with neither `read` nor `write` given the
    /// access defaults to `read`.
    fn access(self) -> Access {
        let read = self.read;
        let write = self.write || self.append || self.exclusive;
        match (read, write) {
            (true, true) => Access::ReadWrite,
            (_, true) => Access::Write,
            // `read` is the documented default (see above).
            _ => Access::Read,
        }
    }

    /// The `open(2)` flag word, mirroring `get_flags`
    /// (`erts/emulator/nifs/unix/unix_prim_file.c:142-171`), plus the two flags this
    /// runtime always needs: `O_NONBLOCK` (the reactor is edge-triggered and cannot serve a
    /// blocking descriptor) and `O_CLOEXEC` (the driver must not leak its fd into a child
    /// `exec`, which the `spawn` driver starts).
    fn to_oflags(self) -> libc::c_int {
        let mut flags = match self.access() {
            Access::Read => libc::O_RDONLY,
            Access::Write => libc::O_TRUNC | libc::O_WRONLY | libc::O_CREAT,
            Access::ReadWrite => libc::O_RDWR | libc::O_CREAT,
        };
        if self.append {
            // `append` cancels the truncation `O_WRONLY` carries.
            flags &= !libc::O_TRUNC;
            flags |= libc::O_APPEND;
        }
        if self.exclusive {
            flags |= libc::O_EXCL;
        }
        if self.sync {
            flags |= libc::O_SYNC;
        }
        flags | libc::O_NONBLOCK | libc::O_CLOEXEC
    }
}

/// A parsed `open` control argument: the flag set, the optional data mode and the path.
struct OpenArg {
    /// The textual access flags, empty (`read`) when the argument was a bare path.
    flags: OpenFlags,
    /// The data mode one of the `binary`/`list` flag words selected, if either was given.
    data_mode: Option<FileDataMode>,
    /// The path to open.
    path: Vec<u8>,
}

/// The `efile` driver: an [`FdDriver`] plus the `efile` control family.
pub struct EfileDriver {
    /// The underlying byte-stream driver (registration, read/write, line/phase).
    inner: FdDriver,
    /// The path most recently opened through the `open` command, if any.
    path: Option<Vec<u8>>,
    /// The `file:` data representation reads are rendered in.
    data_mode: FileDataMode,
}
impl EfileDriver {
    /// An inert, unopened `efile` port: it owns no fds until a `open` control command
    /// attaches one.
    pub fn unopened() -> EfileDriver {
        EfileDriver {
            inner: FdDriver::inert(),
            path: None,
            data_mode: FileDataMode::default(),
        }
    }

    /// An `efile` port reading from `fd` (which the driver then owns).
    pub fn read_only(fd: RawFd) -> EfileDriver {
        EfileDriver {
            inner: FdDriver::read_only(fd),
            path: None,
            data_mode: FileDataMode::default(),
        }
    }

    /// An `efile` port writing to `fd` (which the driver then owns).
    pub fn write_only(fd: RawFd) -> EfileDriver {
        EfileDriver {
            inner: FdDriver::write_only(fd),
            path: None,
            data_mode: FileDataMode::default(),
        }
    }

    /// An `efile` port reading from `read_fd` and writing to `write_fd` (both owned).
    pub fn connected(read_fd: RawFd, write_fd: RawFd) -> EfileDriver {
        EfileDriver {
            inner: FdDriver::connected(read_fd, write_fd),
            path: None,
            data_mode: FileDataMode::default(),
        }
    }

    /// The path most recently opened through the `open` control command.
    pub fn path(&self) -> Option<&[u8]> {
        self.path.as_deref()
    }

    /// The `file:` data representation the port is configured to read in.
    ///
    /// Set by the `binary`/`list` control commands or the matching `open` flag words; the
    /// default is [`FileDataMode::Binary`].
    pub fn data_mode(&self) -> FileDataMode {
        self.data_mode
    }

    /// Whether the read fd has reported end-of-file.
    pub fn eof(&self) -> bool {
        self.inner.eof()
    }

    /// The descriptor the file-position/sync/truncate verbs act on, or `BadArg` when the
    /// port owns no fd (unopened, or already closed).
    fn attached_fd(&self) -> Result<RawFd> {
        self.inner.attached_fd().ok_or_else(error::badarg)
    }

    /// Opens a file and attaches its descriptor(s): the whole of the `open` command after
    /// argument parsing.
    ///
    /// The fd is registered with the poller; a regular file (which `epoll` refuses) is
    /// served by the poller's fallback readiness source, so this works for plain files too.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when no path is given, the argument cannot
    /// be parsed, or a descriptor is already attached; [`beam_common::BeamError::Io`] when
    /// `open(2)` fails, the path is a directory, `dup(2)` fails, or the poller registration
    /// is rejected.
    fn open_path(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<()> {
        if self.inner.has_read_fd() || self.inner.has_write_fd() {
            return Err(error::badarg());
        }
        let parsed = parse_open_arg(arg)?;
        if parsed.path.is_empty() || parsed.path.contains(&0) {
            return Err(error::badarg());
        }
        let mut c_path = parsed.path.clone();
        c_path.push(0);
        // SAFETY: `c_path` is a NUL-terminated byte string owned by this call, so
        // `open(2)` only reads it; the flags/`mode` combination is valid and `open`
        // returns a fresh descriptor owned by this driver on success. `O_NONBLOCK`
        // (always set, see `OpenFlags::to_oflags`) matters for FIFOs, whose `open` would
        // otherwise block until a writer appears.
        let fd = unsafe { libc::open(c_path.as_ptr().cast(), parsed.flags.to_oflags(), FILE_MODE) };
        if fd < 0 {
            return Err(error::io(std::io::Error::last_os_error()));
        }
        // The oracle refuses a directory (`EISDIR`) unless directory mode was asked for
        // (`open_file_is_dir`, `unix_prim_file.c:206-213`); `open(2)` on its own opens a
        // read-only directory happily, so without this check a directory port would
        // register and only fail later, on the first `read(2)`.
        if is_directory(fd) {
            // SAFETY: `fd` was returned by `open(2)` above and has not been stored.
            let _ = unsafe { libc::close(fd) };
            return Err(error::io(std::io::Error::from_raw_os_error(libc::EISDIR)));
        }
        match parsed.flags.access() {
            Access::Read => self.install(ctx, Some(fd), None, &parsed),
            Access::Write => self.install(ctx, None, Some(fd), &parsed),
            Access::ReadWrite => {
                // The byte-stream driver has one read slot and one write slot and closes
                // each exactly once, so a read+write open needs two descriptors: `dup(2)`
                // shares the file *description*, so the read and write offsets stay in
                // step exactly as a single fd's would.
                // SAFETY: `fd` is an open descriptor owned by this call and is not stored.
                let write_fd = unsafe { libc::dup(fd) };
                if write_fd < 0 {
                    let err = std::io::Error::last_os_error();
                    // SAFETY: `fd` was returned by `open(2)` above and has not been stored.
                    let _ = unsafe { libc::close(fd) };
                    return Err(error::io(err));
                }
                self.install(ctx, Some(fd), Some(write_fd), &parsed)
            }
        }
    }

    /// Attaches the freshly opened descriptor(s), rolling back everything already attached
    /// (and closing every fd that was never stored) if an attachment fails.
    fn install(
        &mut self,
        ctx: &mut DriverContext<'_>,
        read_fd: Option<RawFd>,
        write_fd: Option<RawFd>,
        parsed: &OpenArg,
    ) -> Result<()> {
        if let Some(fd) = read_fd {
            if let Err(err) = self.inner.adopt_read_fd(ctx, fd) {
                // Ownership never transferred, so these fds are this call's to close.
                // SAFETY: both were returned by `open(2)`/`dup(2)` above and are unstored.
                let _ = unsafe { libc::close(fd) };
                if let Some(write_fd) = write_fd {
                    let _ = unsafe { libc::close(write_fd) };
                }
                return Err(err);
            }
        }
        if let Some(fd) = write_fd {
            if let Err(err) = self.inner.adopt_write_fd(ctx, fd) {
                // Undo the read attachment, then release the fd that never transferred.
                let _ = self.inner.release_read_fd(ctx);
                // SAFETY: `fd` was returned by `dup(2)` above and was not stored.
                let _ = unsafe { libc::close(fd) };
                return Err(err);
            }
        }
        self.path = Some(parsed.path.clone());
        if let Some(mode) = parsed.data_mode {
            self.data_mode = mode;
            // `file:` has no line framing, so neither data mode keeps text framing.
            self.inner.set_binary_mode();
        }
        Ok(())
    }
}

impl Driver for EfileDriver {
    fn name(&self) -> &str {
        "efile"
    }

    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.inner.start(ctx)
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.inner.output(ctx)
    }

    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.inner.ready_input(ctx)
    }

    fn ready_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.inner.ready_output(ctx)
    }

    fn control(
        &mut self,
        ctx: &mut DriverContext<'_>,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        match command {
            "open" => {
                self.open_path(ctx, arg)?;
                Ok(Vec::new())
            }
            "close" => {
                // Both slots are released: a read+write open holds two descriptors, and a
                // write-only open holds one in the write slot. Each is closed exactly once.
                let read = self.inner.release_read_fd(ctx);
                let write = self.inner.release_write_fd(ctx);
                self.path = None;
                read.and(write)?;
                Ok(Vec::new())
            }
            "path" => Ok(self.path.clone().unwrap_or_default()),
            "eof" => Ok(if self.inner.eof() {
                b"true".to_vec()
            } else {
                b"false".to_vec()
            }),
            "position" => {
                let fd = self.attached_fd()?;
                // No argument: report the current offset. With one: seek to it absolutely.
                let (offset, whence) = if arg.is_empty() {
                    (0, libc::SEEK_CUR)
                } else {
                    (parse_offset(arg)?, libc::SEEK_SET)
                };
                Ok(decimal_position(seek(fd, offset, whence)?))
            }
            "seek" => {
                let fd = self.attached_fd()?;
                let (whence, offset) = parse_seek(arg)?;
                Ok(decimal_position(seek(fd, offset, whence)?))
            }
            "sync" => {
                let fd = self.attached_fd()?;
                // SAFETY: `fd` is an open descriptor owned by this driver; `fsync(2)` only
                // flushes its dirty state and never retains a pointer.
                let rc = unsafe { libc::fsync(fd) };
                if rc != 0 {
                    return Err(error::io(std::io::Error::last_os_error()));
                }
                Ok(Vec::new())
            }
            "truncate" => {
                let fd = self.attached_fd()?;
                // No argument: truncate at the current position (the C's `truncate/1`,
                // `unix_prim_file.c:693-704`). With one: truncate to that absolute length.
                let len = if arg.is_empty() {
                    seek(fd, 0, libc::SEEK_CUR)?
                } else {
                    parse_offset(arg)?
                };
                if len < 0 {
                    return Err(error::badarg());
                }
                // SAFETY: `fd` is an open descriptor owned by this driver; `ftruncate(2)`
                // only changes its length and never retains a pointer.
                let rc = unsafe { libc::ftruncate(fd, len) };
                if rc != 0 {
                    return Err(error::io(std::io::Error::last_os_error()));
                }
                Ok(Vec::new())
            }
            // The two `file:` data-representation options. Both read raw bytes (no line
            // framing exists in `file:`), so both select the binary phase; only the mode
            // recorded on the driver differs.
            "binary" => {
                self.data_mode = FileDataMode::Binary;
                self.inner.set_binary_mode();
                Ok(Vec::new())
            }
            "list" => {
                self.data_mode = FileDataMode::List;
                self.inner.set_binary_mode();
                Ok(Vec::new())
            }
            // Everything else (`line`/`noline`/`text`/`getopts`) is the shared byte-stream
            // family, handled by the composed driver.
            _ => self.inner.control(ctx, command, arg),
        }
    }

    fn timeout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.inner.timeout(ctx)
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.inner.stop(ctx)
    }
}

/// The [`crate::driver::DriverFactory`] of the built-in `"efile"` driver.
///
/// Builds an unopened [`EfileDriver`]; the port becomes usable once a `open` control
/// command attaches a path.
///
/// # Errors
///
/// Never fails today (the signature matches [`crate::driver::DriverFactory`]).
pub fn make_efile() -> Result<Box<dyn Driver>> {
    Ok(Box::new(EfileDriver::unopened()))
}

/// Parses an `open` argument (`[<flags> ]<path>`) into its flag set and path.
///
/// The leading `<flags>` token is recognised only when it is a comma-separated list in which
/// **every** word is a known flag (`read`/`write`/`append`/`exclusive`/`sync`/`binary`/
/// `list`); otherwise the whole argument is the path, which keeps the original read-only
/// `open <path>` form working for every path, spaces included.
///
/// # Errors
///
/// Returns [`beam_common::BeamError::BadArg`] when a recognised flag list is empty (e.g. a
/// leading or trailing comma) — a bare path is never an error here; the empty-path check is
/// the caller's.
fn parse_open_arg(arg: &[u8]) -> Result<OpenArg> {
    let bare = OpenArg {
        flags: OpenFlags::default(),
        data_mode: None,
        path: arg.to_vec(),
    };
    let Some(space) = arg.iter().position(|&b| b == b' ') else {
        return Ok(bare);
    };
    let (token, path) = arg.split_at(space);
    // `path` keeps the separating space; strip exactly one.
    let path = &path[1..];
    match parse_flag_token(token) {
        Ok((flags, data_mode)) => Ok(OpenArg {
            flags,
            data_mode,
            path: path.to_vec(),
        }),
        // Not a flag list: the whole argument was a path containing a space.
        Err(_) => Ok(bare),
    }
}

/// Parses a comma-separated `open` flag token, or rejects it so the caller can fall back to
/// treating the argument as a path.
fn parse_flag_token(token: &[u8]) -> Result<(OpenFlags, Option<FileDataMode>)> {
    let text = std::str::from_utf8(token).map_err(|_| error::badarg())?;
    let mut flags = OpenFlags::default();
    let mut data_mode = None;
    for word in text.split(',') {
        match word {
            "read" => flags.read = true,
            "write" => flags.write = true,
            "append" => flags.append = true,
            "exclusive" => flags.exclusive = true,
            "sync" => flags.sync = true,
            "binary" => data_mode = Some(FileDataMode::Binary),
            "list" => data_mode = Some(FileDataMode::List),
            _ => return Err(error::badarg()),
        }
    }
    Ok((flags, data_mode))
}

/// Parses a signed decimal byte offset.
///
/// # Errors
///
/// Returns [`beam_common::BeamError::BadArg`] when the argument is not a valid `i64` (after
/// trimming surrounding whitespace).
fn parse_offset(arg: &[u8]) -> Result<i64> {
    let text = std::str::from_utf8(arg).map_err(|_| error::badarg())?;
    text.trim().parse::<i64>().map_err(|_| error::badarg())
}

/// Parses a `seek` argument: `<whence>[,<offset>]`, whence `bof`/`cur`/`eof`.
///
/// The vocabulary mirrors the C's `EFILE_SEEK_{BOF,CUR,EOF}` (`prim_file_nif.h:50-54`) and
/// the `{bof,N}`/`{cur,N}`/`{eof,N}` locations `file:position/2` takes. The offset defaults
/// to `0`.
///
/// # Errors
///
/// Returns [`beam_common::BeamError::BadArg`] for an unknown whence or a non-numeric offset.
fn parse_seek(arg: &[u8]) -> Result<(libc::c_int, i64)> {
    let text = std::str::from_utf8(arg).map_err(|_| error::badarg())?;
    let text = text.trim();
    let (whence, offset) = match text.split_once(',') {
        Some((whence, offset)) => (whence.trim(), parse_offset(offset.as_bytes())?),
        None => (text, 0),
    };
    let whence = match whence {
        "bof" => libc::SEEK_SET,
        "cur" => libc::SEEK_CUR,
        "eof" => libc::SEEK_END,
        _ => return Err(error::badarg()),
    };
    Ok((whence, offset))
}

/// Repositions `fd`, returning the resulting absolute offset.
///
/// # Errors
///
/// Returns [`beam_common::BeamError::Io`] when `lseek(2)` fails (e.g. `ESPIPE` on a FIFO).
fn seek(fd: RawFd, offset: i64, whence: libc::c_int) -> Result<i64> {
    // SAFETY: `fd` is an open descriptor owned by the caller; `lseek(2)` only repositions it
    // and never retains a pointer.
    let rc = unsafe { libc::lseek(fd, offset, whence) };
    if rc < 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    Ok(rc)
}

/// Renders an absolute file position as decimal ASCII (the reply of `position`/`seek`).
fn decimal_position(position: i64) -> Vec<u8> {
    position.to_string().into_bytes()
}

/// Whether `fd` names a directory (the C's `open_file_is_dir`, `unix_prim_file.c:126-140`).
///
/// A failed `fstat` is treated as "not a directory", exactly as the C assumes.
fn is_directory(fd: RawFd) -> bool {
    // SAFETY: `libc::stat` is a plain C struct with no invalid bit patterns, so zeroing it is
    // sound, and `fstat(2)` fills it for a live descriptor.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is an open descriptor owned by the caller and `&mut stat` is a valid
    // writable `libc::stat` for the duration of the call.
    let rc = unsafe { libc::fstat(fd, &mut stat) };
    rc == 0 && (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR
}

#[cfg(test)]
mod tests {
    use super::{make_efile, EfileDriver};
    use crate::poller::Poller;
    use crate::port::PortRegistry;
    use beam_common::BeamError;
    use beam_process::PortId;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    fn poller() -> Poller {
        Poller::new().expect("a poll set")
    }

    /// A unique temporary path under the OS temp directory.
    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "beam_io_efile_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ))
    }

    /// Polls and dispatches until `id` has buffered at least `want` input bytes, returning
    /// everything the port has received so far.
    fn pump_until_input(
        poller: &mut Poller,
        registry: &mut PortRegistry,
        id: PortId,
        want: usize,
    ) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut input = Vec::new();
        while input.len() < want {
            assert!(
                Instant::now() < deadline,
                "ready_input never delivered bytes"
            );
            let events = poller
                .poll(Some(Duration::from_millis(200)))
                .expect("poll succeeds");
            registry
                .handle_events(poller, &events)
                .expect("events dispatch");
            input.extend(
                registry
                    .get_mut(id)
                    .expect("the port is live")
                    .drain_input(),
            );
        }
        input
    }

    #[test]
    fn make_efile_builds_an_unopened_efile_driver() {
        let driver = make_efile().expect("a driver");
        assert_eq!(driver.name(), "efile");
    }

    #[test]
    fn an_unopened_efile_reports_no_path_and_no_eof() {
        let driver = EfileDriver::unopened();
        assert!(driver.path().is_none());
        assert!(!driver.eof());
    }

    #[test]
    fn a_read_only_efile_reads_bytes_and_answers_getopts() {
        let (peer, read_end) = UnixStream::pair().expect("socketpair");
        // SAFETY: `read_end.as_raw_fd()` names an open descriptor of a live stream.
        let fd = unsafe { libc::dup(read_end.as_raw_fd()) };
        assert!(fd >= 0, "dup(2) succeeded");

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(EfileDriver::read_only(fd)))
            .expect("the port starts");
        assert_eq!(registry.get(id).expect("live").driver_name(), "efile");
        assert_eq!(poller.len(), 1, "start registers the read fd");

        // The default options are the raw binary phase.
        assert_eq!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "getopts", b"")
                .expect("getopts"),
            b"binary"
        );

        let mut writer = &peer;
        writer.write_all(b"hello\n").expect("writes");
        assert_eq!(
            pump_until_input(&mut poller, &mut registry, id, 6),
            b"hello\n"
        );

        // The `line`/`text`/`binary` family is inherited from the composed `FdDriver`.
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "line", b"")
            .expect("line");
        assert_eq!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "getopts", b"")
                .expect("getopts"),
            b"line,1024"
        );
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "line", b"8")
            .expect("line with a limit");
        assert_eq!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "getopts", b"")
                .expect("getopts"),
            b"line,8"
        );

        // An unknown command is rejected by the composed driver.
        assert!(matches!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "bogus", b""),
            Err(BeamError::BadArg)
        ));

        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn an_unopened_efile_is_inert_and_rejects_bad_open_arguments() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(EfileDriver::unopened()))
            .expect("an unopened port still starts");
        assert!(
            poller.is_empty(),
            "an unopened efile registers no fds on start"
        );
        assert_eq!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "eof", b"")
                .expect("eof"),
            b"false"
        );
        assert_eq!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "path", b"")
                .expect("path"),
            b""
        );

        // An empty path and a path that cannot be opened are both errors.
        assert!(matches!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "open", b""),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "open", b"/no/such/beam_io/file"),
            Err(BeamError::Io(_))
        ));

        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn open_of_a_fifo_reads_its_bytes() {
        // A FIFO is epoll-registerable, unlike a regular file (see the module docs).
        let path = std::env::temp_dir().join(format!(
            "beam_io_efile_fifo_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        let mut c_path = path.as_os_str().as_encoded_bytes().to_vec();
        c_path.push(0);
        // SAFETY: `c_path` is a NUL-terminated path owned by this call; `mkfifo` only
        // reads it and creates a new special file.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr().cast(), 0o600) };
        assert_eq!(rc, 0, "mkfifo({:?}) succeeded", path);

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(EfileDriver::unopened()))
            .expect("the port starts");
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "open", path.as_os_str().as_encoded_bytes())
            .expect("opens the fifo");
        assert_eq!(poller.len(), 1, "open registers the read fd");

        // Open the write end (also non-blocking, so it never blocks the test) and send
        // bytes through the FIFO.
        // SAFETY: `c_path` is the same NUL-terminated path; `open` only reads it and
        // returns a fresh descriptor owned by this test.
        let write_fd = unsafe {
            libc::open(
                c_path.as_ptr().cast(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        assert!(write_fd >= 0, "the fifo write end opens");
        let payload = b"pong\n";
        // SAFETY: `write_fd` is an open descriptor and `payload` is a valid byte slice.
        let written = unsafe { libc::write(write_fd, payload.as_ptr().cast(), payload.len()) };
        assert_eq!(written, payload.len() as isize, "the fifo accepts the bytes");

        assert_eq!(
            pump_until_input(&mut poller, &mut registry, id, payload.len()),
            payload
        );

        // Closing the write end turns the read fd into EOF. A FIFO reports that as an
        // `EPOLLHUP` (neither readable nor writable in `mio`'s `Ready`), which the reactor
        // dispatches only through a readiness bit — so drive the read path directly and
        // assert the EOF is latched and no further bytes appear.
        // SAFETY: `write_fd` was created by this test and is not used again.
        let _ = unsafe { libc::close(write_fd) };
        registry
            .get_mut(id)
            .expect("live")
            .ready_input(&mut poller)
            .expect("reading to EOF is not an error");
        assert_eq!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "eof", b"")
                .expect("eof"),
            b"true",
            "the driver latched end-of-file"
        );
        assert!(
            registry.get_mut(id).expect("live").drain_input().is_empty(),
            "EOF yields no further bytes"
        );

        registry.close(&mut poller, id).expect("closes");
        // SAFETY: `c_path` is the path this test created; unlinking it only removes the
        // directory entry.
        let _ = unsafe { libc::unlink(c_path.as_ptr().cast()) };
    }

    #[test]
    fn close_releases_the_open_fifo() {
        let path = std::env::temp_dir().join(format!(
            "beam_io_efile_close_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        let mut c_path = path.as_os_str().as_encoded_bytes().to_vec();
        c_path.push(0);
        // SAFETY: `c_path` is a NUL-terminated path owned by this call.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr().cast(), 0o600) };
        assert_eq!(rc, 0, "mkfifo succeeded");

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(EfileDriver::unopened()))
            .expect("the port starts");
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "open", path.as_os_str().as_encoded_bytes())
            .expect("opens the fifo");
        assert_eq!(poller.len(), 1);

        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "close", b"")
            .expect("closes the read fd");
        assert!(poller.is_empty(), "close deregisters the read fd");

        registry.close(&mut poller, id).expect("closes the port");
        // SAFETY: `c_path` is the path this test created.
        let _ = unsafe { libc::unlink(c_path.as_ptr().cast()) };
    }

    #[test]
    fn open_of_a_regular_file_reads_it_to_eof_through_the_fallback_source() {
        // A regular file is not `epoll`-registerable (`EPOLL_CTL_ADD` ⇒ `EPERM`); the
        // poller's `poll(2)` fallback readiness source serves it, so `open`/`read`/`close`
        // work on a plain file exactly as the reference runtime's `poll(2)`-based path makes
        // them.
        let path = temp_path("regular_read");
        let payload = b"the quick brown fox\njumps over the lazy dog\n";
        std::fs::write(&path, payload).expect("the fixture file is written");

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(EfileDriver::unopened()))
            .expect("the port starts");
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "open", path.as_os_str().as_encoded_bytes())
            .expect("a regular file opens through the fallback readiness source");
        assert_eq!(
            poller.len(),
            1,
            "the regular file is registered with the poller"
        );
        assert_eq!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "path", b"")
                .expect("path"),
            path.as_os_str().as_encoded_bytes(),
            "the driver remembers the opened path"
        );

        // Pump the reactor until the whole file has arrived. The driver reads the regular
        // file to EOF inside a single `ready_input` round and latches end-of-file.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut input = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "the reactor never read the file");
            let events = poller
                .poll(Some(Duration::from_millis(200)))
                .expect("poll succeeds");
            registry
                .handle_events(&mut poller, &events)
                .expect("events dispatch");
            input.extend(registry.get_mut(id).expect("live").drain_input());
            let eof = registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "eof", b"")
                .expect("eof");
            if eof == b"true" {
                break;
            }
        }
        assert_eq!(input, payload.to_vec(), "the whole file content is delivered");
        assert_eq!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "eof", b"")
                .expect("eof"),
            b"true",
            "end-of-file is latched"
        );
        assert!(
            poller.is_empty(),
            "reaching EOF de-registers the regular file, so it stops producing events"
        );

        // `close` releases the attached fd; the port is then closed normally.
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "close", b"")
            .expect("closes the file");
        assert!(poller.is_empty());
        registry.close(&mut poller, id).expect("closes the port");
        assert!(!registry.contains(id));

        std::fs::remove_file(&path).expect("the fixture file is removed");
    }

    #[test]
    fn a_write_only_efile_writes_a_regular_file() {
        let path = temp_path("regular_write");
        let mut c_path = path.as_os_str().as_encoded_bytes().to_vec();
        c_path.push(0);
        // SAFETY: `c_path` is a NUL-terminated path owned by this call; `open(2)` only
        // reads it and returns a fresh descriptor that `EfileDriver::write_only` then owns.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr().cast(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NONBLOCK | libc::O_CLOEXEC,
                0o600,
            )
        };
        assert!(fd >= 0, "the output file opens");

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(EfileDriver::write_only(fd)))
            .expect("the port starts");
        assert!(
            poller.is_empty(),
            "a write-only port arms no readiness registration up front"
        );

        // Writing to a regular file never blocks or stalls, so the whole payload drains
        // synchronously and nothing stays queued.
        let payload = b"written to a regular file\n";
        assert_eq!(
            registry
                .output(&mut poller, id, payload)
                .expect("the driver writes to the regular file"),
            payload.len(),
            "the whole payload is written"
        );
        assert_eq!(
            registry.get(id).expect("live").output_len(),
            0,
            "nothing stalls in the output queue"
        );

        registry.close(&mut poller, id).expect("closes the port");
        assert_eq!(
            std::fs::read(&path).expect("the written file is readable"),
            payload.to_vec(),
            "the bytes reached the regular file"
        );
        std::fs::remove_file(&path).expect("the fixture file is removed");
    }
}
