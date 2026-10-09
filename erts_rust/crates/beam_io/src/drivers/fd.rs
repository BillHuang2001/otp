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

//! [`FdDriver`] — the built-in fd/pipe byte-stream driver.
//!
//! The simplest possible driver: it owns one read fd and one write fd, registers the read
//! fd [`Interest::READABLE`] on `start`, drains the port's output queue to the write fd on
//! `output` (leaving the remainder queued and arming [`Interest::WRITABLE`] on `EAGAIN`)
//! and reads the read fd into the port's input queue on `ready_input`.
//!
//! The driver **owns** the fds it is given: [`FdDriver::pipe`] creates a fresh OS
//! `pipe(2)` and the [`FdDriver::connected`]/[`FdDriver::read_only`]/
//! [`FdDriver::write_only`] constructors adopt caller-supplied fds. In every case the
//! driver closes them on [`Driver::stop`] and on `Drop`; nothing else may close them.
//!
//! # Readiness model
//!
//! The poller's `mio` backend is **edge-triggered** (`mio` registers descriptors with
//! `EPOLLET`), so every callback must drain to `EAGAIN`: [`FdDriver::ready_input`] reads
//! until the fd is empty (or reports EOF) and the write path loops until the queue is
//! drained or the kernel buffer is full.
//!
//! A **regular file**, which `epoll` refuses, is served by the poller's `poll(2)`-based
//! fallback readiness source instead (see [`crate::poller`]) — and that source is
//! *level*-triggered. Because a read fd at end-of-file can never become ready again,
//! [`FdDriver::ready_input`] latches EOF and then **de-registers** the read fd: it stops the
//! level-triggered fallback from reporting an exhausted regular file forever. The fd itself
//! stays attached (and owned) until [`Driver::stop`], so a further post-EOF `ready_input`
//! still answers `Ok` (a no-op) rather than `badarg`.
//!
//! # Input phases and line buffering
//!
//! Input is delivered through a [`PhaseChange`] state machine (the ERTS port "phase
//! change", `ERTS_PORT_SFLG_BINARY_IO`). The **default is [`Phase::Binary`]**, in which
//! every chunk read from the fd is appended to the port input queue unchanged. Enabling
//! the text phase ([`FdDriver::set_line_mode`], or the `line`/`text` control commands)
//! frames the stream into lines first (`{line, N}`, see [`crate::drivers::line_buffer`]):
//! only complete lines reach the port input queue, a line longer than the maximum is
//! split, and a trailing partial line is released on EOF/`stop`.
//!
//! Output is **not** line-framed: in the emulator the line flag
//! (`ERTS_PORT_SFLG_LINEBUF_IO`) affects how a port *delivers* input to its owner, not how
//! it accepts output, so [`Driver::output`] stays a raw byte-stream drain.
//!
//! # Control handshake
//!
//! [`Driver::control`] accepts a UTF-8 command string plus raw argument bytes:
//!
//! | command | argument | effect |
//! |---|---|---|
//! | `line` | optional decimal max line length | enable line buffering (text phase) |
//! | `noline` | — | disable line buffering (binary phase) |
//! | `binary` | — | switch to the raw binary phase |
//! | `text` | — | switch to the line-framed text phase |
//! | `getopts` | — | reply `line,<N>` in the text phase, `binary` otherwise |
//!
//! An unknown command is rejected with [`beam_common::BeamError::BadArg`].
//!
//! All raw syscalls (`read`/`write`/`pipe`/`fcntl`/`close`) live in this module; every
//! `unsafe` block carries a `// SAFETY:` rationale.

use std::os::unix::io::RawFd;

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
use crate::drivers::binary_io::{Phase, PhaseChange};
use crate::error;
use crate::poller::{Interest, Token};

/// The read buffer size of a single [`FdDriver::ready_input`] syscall.
const READ_CHUNK: usize = 64 * 1024;

/// A byte-stream driver over a read fd and a write fd.
pub struct FdDriver {
    /// The registered driver name (`"fd"`).
    name: String,
    /// The fd to read from, or `None` for a write-only driver.
    read_fd: Option<RawFd>,
    /// The fd to write to, or `None` for a read-only driver.
    write_fd: Option<RawFd>,
    /// The poller token of the read-fd registration, once started.
    in_token: Option<Token>,
    /// The poller token of the write-fd registration, once armed.
    out_token: Option<Token>,
    /// Set once the read fd reported end-of-file, so no further reads are attempted.
    read_eof: bool,
    /// The input framing phase (defaults to raw binary pass-through).
    phase: PhaseChange,
}

impl FdDriver {
    /// Creates a driver over a fresh OS pipe: bytes written to the write fd are read back
    /// from the read fd.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when `pipe(2)` fails.
    pub fn pipe() -> Result<FdDriver> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` is a valid, writable 2-element array; on success `pipe(2)` fills
        // both slots with fresh descriptors that this driver then owns.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(error::io(std::io::Error::last_os_error()));
        }
        Ok(FdDriver::from_fds(Some(fds[0]), Some(fds[1])))
    }

    /// Adopts an existing read fd and write fd (both are assumed non-blocking).
    pub fn connected(read_fd: RawFd, write_fd: RawFd) -> FdDriver {
        FdDriver::from_fds(Some(read_fd), Some(write_fd))
    }

    /// Adopts an existing read-only fd.
    pub fn read_only(fd: RawFd) -> FdDriver {
        FdDriver::from_fds(Some(fd), None)
    }

    /// Adopts an existing write-only fd.
    pub fn write_only(fd: RawFd) -> FdDriver {
        FdDriver::from_fds(None, Some(fd))
    }

    /// A driver owning no fds, used by the [`crate::drivers::EfileDriver`] unopened state
    /// (and by tests exercising the inert paths).
    pub(crate) fn inert() -> FdDriver {
        FdDriver::from_fds(None, None)
    }

    /// The shared constructor: the driver takes ownership of every fd it is handed.
    fn from_fds(read_fd: Option<RawFd>, write_fd: Option<RawFd>) -> FdDriver {
        FdDriver {
            name: "fd".to_string(),
            read_fd,
            write_fd,
            in_token: None,
            out_token: None,
            read_eof: false,
            phase: PhaseChange::new(),
        }
    }

    /// Enables line-buffered (text-phase) input delivery with `max_len` as the maximum
    /// line length in bytes (`0` = no length limit).
    ///
    /// Builder form of [`FdDriver::set_line_mode`]; the default behaviour of a freshly
    /// constructed driver is raw byte pass-through and is unchanged.
    pub fn with_line_mode(mut self, max_len: usize) -> FdDriver {
        self.set_line_mode(max_len);
        self
    }

    /// Enables line-buffered (text-phase) input delivery with `max_len` as the maximum
    /// line length in bytes (`0` = no length limit).
    ///
    /// Only complete lines reach the port input queue; a line longer than `max_len` is
    /// split and any trailing partial line is released on EOF/`stop`.
    pub fn set_line_mode(&mut self, max_len: usize) {
        self.phase.set_max_line_len(max_len);
        self.phase.to_text();
    }

    /// Disables line buffering, restoring raw byte pass-through (the binary phase).
    pub fn set_binary_mode(&mut self) {
        self.phase.to_binary();
    }

    /// Whether line-buffered (text-phase) input delivery is active.
    pub fn is_line_buffered(&self) -> bool {
        self.phase.current().is_text()
    }

    /// The input framing phase.
    pub fn phase(&self) -> Phase {
        self.phase.current()
    }

    /// Sets the input framing phase.
    pub fn set_phase(&mut self, phase: Phase) {
        self.phase.set(phase);
    }

    /// The maximum line length used in the text/line phase (`0` = unlimited).
    pub fn max_line_len(&self) -> usize {
        self.phase.max_line_len()
    }

    /// Sets the maximum line length (`0` = unlimited); the phase is not changed.
    pub fn set_max_line_len(&mut self, max_len: usize) {
        self.phase.set_max_line_len(max_len);
    }

    /// The number of input bytes held back from the port input queue by line framing.
    pub fn pending_input(&self) -> usize {
        self.phase.pending()
    }

    /// The reply bytes of the `getopts` control command.
    ///
    /// The documented format is `line,<N>` in the text/line phase (`<N>` being the
    /// decimal maximum line length) and `binary` in the raw binary phase.
    fn options_reply(&self) -> Vec<u8> {
        match self.phase.current() {
            Phase::Text => format!("line,{}", self.phase.max_line_len()).into_bytes(),
            Phase::Binary => Phase::Binary.as_str().as_bytes().to_vec(),
        }
    }

    /// Whether a read fd is currently attached.
    pub(crate) fn has_read_fd(&self) -> bool {
        self.read_fd.is_some()
    }

    /// Whether a write fd is currently attached.
    pub(crate) fn has_write_fd(&self) -> bool {
        self.write_fd.is_some()
    }

    /// The descriptor the file-position/sync/truncate operations act on: the read fd when one
    /// is attached, else the write fd (so a write-only file is still reachable).
    pub(crate) fn attached_fd(&self) -> Option<RawFd> {
        self.read_fd.or(self.write_fd)
    }

    /// Whether the read fd has reported end-of-file (after which [`Driver::ready_input`]
    /// is a no-op).
    pub fn eof(&self) -> bool {
        self.read_eof
    }

    /// Attaches and registers a read fd opened after [`Driver::start`] (the `efile`
    /// `open` path).
    ///
    /// The fd is put into non-blocking mode and registered [`Interest::READABLE`] under
    /// this port's key.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when a read fd is already attached, or
    /// whatever `fcntl`/registration returns.
    pub(crate) fn adopt_read_fd(
        &mut self,
        ctx: &mut DriverContext<'_>,
        fd: RawFd,
    ) -> Result<()> {
        if self.read_fd.is_some() {
            return Err(error::badarg());
        }
        set_nonblocking(fd)?;
        self.in_token = Some(ctx.register(fd, Interest::READABLE)?);
        self.read_fd = Some(fd);
        self.read_eof = false;
        Ok(())
    }

    /// Deregisters and closes the attached read fd, if any (the `efile` `close` path).
    ///
    /// A deregistration failure is reported but the fd is closed either way, and an fd that
    /// was already de-registered (a read fd that reached end-of-file, see
    /// [`FdDriver::disarm_input`]) is still closed.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the deregistration.
    pub(crate) fn release_read_fd(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.read_eof = false;
        let token = self.in_token.take();
        let fd = self.read_fd.take();
        let mut result = Ok(());
        if let (Some(token), Some(fd)) = (token, fd) {
            result = ctx.deregister(fd, token);
        }
        if let Some(fd) = fd {
            // SAFETY: `fd` is owned by this driver and has just been removed from the
            // struct, so it is closed exactly once.
            let _ = unsafe { libc::close(fd) };
        }
        result
    }

    /// Attaches and registers a **write** fd opened after [`Driver::start`] (the `efile`
    /// write-mode `open` path).
    ///
    /// The fd is put into non-blocking mode. Write readiness is armed only when the port's
    /// output queue already holds bytes to flush; otherwise [`FdDriver::output`] writes
    /// synchronously and [`FdDriver::write_pending`] arms readiness on demand — exactly what
    /// [`Driver::start`] does for a write fd the driver was built with.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when a write fd is already attached, or
    /// whatever `fcntl`/registration returns.
    pub(crate) fn adopt_write_fd(&mut self, ctx: &mut DriverContext<'_>, fd: RawFd) -> Result<()> {
        if self.write_fd.is_some() {
            return Err(error::badarg());
        }
        set_nonblocking(fd)?;
        if !ctx.output().is_empty() {
            self.out_token = Some(ctx.register(fd, Interest::WRITABLE)?);
        }
        self.write_fd = Some(fd);
        Ok(())
    }

    /// Deregisters and closes the attached write fd, if any (the `efile` `close` path).
    ///
    /// A deregistration failure is reported but the fd is closed either way, and a write fd
    /// whose readiness was never armed (nothing ever stalled) is still closed.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the deregistration.
    pub(crate) fn release_write_fd(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let token = self.out_token.take();
        let fd = self.write_fd.take();
        let mut result = Ok(());
        if let (Some(token), Some(fd)) = (token, fd) {
            result = ctx.deregister(fd, token);
        }
        if let Some(fd) = fd {
            // SAFETY: `fd` is owned by this driver and has just been removed from the
            // struct, so it is closed exactly once.
            let _ = unsafe { libc::close(fd) };
        }
        result
    }

    /// Drops the read-fd registration, so no further read-readiness events arrive for it.
    ///
    /// Only the poll registration is removed: the fd stays attached (and owned) — that is
    /// what [`FdDriver::ready_input`] needs after it latches end-of-file, since a further
    /// post-EOF call must still answer `Ok` rather than `badarg`.
    ///
    /// Used when the read fd reaches end-of-file. The `mio` registrations are
    /// edge-triggered and would stop on their own, but a descriptor served by the poller's
    /// regular-file fallback is *level*-triggered and would report readiness forever; a
    /// read fd with no data left can never become ready again, so removing the
    /// registration is correct for both sources.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the deregistration.
    fn disarm_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if let Some(token) = self.in_token.take() {
            if let Some(fd) = self.read_fd {
                ctx.deregister(fd, token)?;
            }
        }
        Ok(())
    }

    /// Arms write readiness for `fd` — registering it the first time, re-registering it
    /// after that — so [`Driver::ready_output`] runs once the kernel buffer drains.
    fn arm_output(&mut self, ctx: &mut DriverContext<'_>, fd: RawFd) -> Result<()> {
        match self.out_token {
            Some(token) => ctx.reregister(fd, token, Interest::WRITABLE),
            None => {
                self.out_token = Some(ctx.register(fd, Interest::WRITABLE)?);
                Ok(())
            }
        }
    }

    /// Drops the write registration, so no further writable events arrive.
    fn disarm_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if let Some(token) = self.out_token.take() {
            if let Some(fd) = self.write_fd {
                ctx.deregister(fd, token)?;
            }
        }
        Ok(())
    }

    /// Drains the port's output queue to the write fd, returning the number of bytes
    /// written.
    ///
    /// A short write (`EAGAIN`) leaves the remainder queued and arms write readiness;
    /// a fully drained queue drops the write registration again.
    fn write_pending(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let fd = self.write_fd.ok_or_else(error::badarg)?;
        let mut written = 0usize;
        let mut stalled = false;
        loop {
            let front = ctx.output().front();
            if front.is_empty() {
                break;
            }
            let len = front.len();
            // SAFETY: `fd` is an open, non-blocking descriptor owned by this driver and
            // `front` points to `len` initialized bytes — `write(2)` reads at most that
            // many and never retains the pointer.
            let n = unsafe { libc::write(fd, front.as_ptr().cast(), len) };
            if n > 0 {
                let n = n as usize;
                ctx.output().consume(n);
                written += n;
                continue;
            }
            if n == 0 {
                // No progress: treat it like a full buffer and let readiness re-drive us
                // instead of spinning here.
                stalled = true;
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // A signal interrupted the write; retry.
                Some(libc::EINTR) => continue,
                // The kernel buffer is full (`EWOULDBLOCK` shares `EAGAIN`'s value on
                // every platform this crate targets).
                Some(libc::EAGAIN) => {
                    stalled = true;
                    break;
                }
                _ => return Err(error::io(err)),
            }
        }

        if ctx.output().is_empty() {
            self.disarm_output(ctx)?;
        } else if stalled {
            self.arm_output(ctx, fd)?;
        }
        Ok(written)
    }

    /// Hands `data`, freshly read from the fd, up through the current input phase:
    /// binary mode appends it to the port input queue unchanged, text mode frames it into
    /// complete lines (holding back a trailing partial line).
    fn deliver_input(&mut self, ctx: &mut DriverContext<'_>, data: &[u8]) {
        match self.phase.current() {
            Phase::Binary => ctx.input().push(data),
            Phase::Text => {
                for frame in self.phase.handle(data) {
                    ctx.input().push(&frame);
                }
            }
        }
    }

    /// Releases any input held back by text-phase framing (a trailing partial line) into
    /// the port input queue. A no-op in the binary phase.
    fn flush_input(&mut self, ctx: &mut DriverContext<'_>) {
        for frame in self.phase.flush() {
            ctx.input().push(&frame);
        }
    }

    /// Closes every fd still owned by the driver.
    ///
    /// Each field is `take()`n first, so this is idempotent: `stop` and `Drop` can never
    /// close the same descriptor twice.
    fn close_fds(&mut self) {
        for fd in [self.read_fd.take(), self.write_fd.take()]
            .into_iter()
            .flatten()
        {
            // SAFETY: `fd` was created by `pipe(2)` or explicitly adopted, is owned by
            // this driver, and has just been removed from the struct, so it is closed
            // exactly once.
            let _ = unsafe { libc::close(fd) };
        }
    }
}

impl Driver for FdDriver {
    fn name(&self) -> &str {
        &self.name
    }

    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if let Some(fd) = self.read_fd {
            set_nonblocking(fd)?;
            self.in_token = Some(ctx.register(fd, Interest::READABLE)?);
        }
        if let Some(fd) = self.write_fd {
            set_nonblocking(fd)?;
            // Only arm the write fd when there is already something to flush; otherwise
            // `output` writes synchronously and write interest is never needed.
            if !ctx.output().is_empty() {
                self.out_token = Some(ctx.register(fd, Interest::WRITABLE)?);
            }
        }
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.write_pending(ctx)
    }

    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let fd = self.read_fd.ok_or_else(error::badarg)?;
        if self.read_eof {
            // End of file was already observed: further read readiness is a no-op (the
            // poller may still report the fd, e.g. on a lingering `EPOLLHUP`).
            return Ok(());
        }
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // SAFETY: `fd` is an open, non-blocking descriptor owned by this driver and
            // `buf` is a writable array of `READ_CHUNK` bytes, so `read(2)` writes at
            // most `READ_CHUNK` bytes into it.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), READ_CHUNK) };
            if n > 0 {
                self.deliver_input(ctx, &buf[..n as usize]);
                // Edge-triggered readiness: keep reading until the fd is empty.
                continue;
            }
            if n == 0 {
                // End of file: release any trailing partial line, latch EOF so no further
                // reads are attempted on this fd, and drop the read registration — the fd
                // can never become readable again, and a level-triggered fallback source
                // would otherwise keep reporting it.
                self.flush_input(ctx);
                self.read_eof = true;
                self.disarm_input(ctx)?;
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Ok(()),
                _ => return Err(error::io(err)),
            }
        }
    }

    fn ready_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        // Write readiness means the kernel buffer drained: resume at the queue front.
        self.write_pending(ctx)
    }

    fn control(
        &mut self,
        _ctx: &mut DriverContext<'_>,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        match command {
            "line" => {
                if arg.is_empty() {
                    // Keep the currently configured limit.
                    self.set_line_mode(self.phase.max_line_len());
                } else {
                    let max_len = parse_line_len(arg)?;
                    self.set_line_mode(max_len);
                }
                Ok(Vec::new())
            }
            "noline" | "binary" => {
                self.set_binary_mode();
                Ok(Vec::new())
            }
            "text" => {
                self.phase.to_text();
                Ok(Vec::new())
            }
            "getopts" => Ok(self.options_reply()),
            _ => Err(error::badarg()),
        }
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // Release anything still held by text-phase framing before the fds go away.
        self.flush_input(ctx);
        let in_result = match (self.in_token.take(), self.read_fd) {
            (Some(token), Some(fd)) => ctx.deregister(fd, token),
            _ => Ok(()),
        };
        let out_result = match (self.out_token.take(), self.write_fd) {
            (Some(token), Some(fd)) => ctx.deregister(fd, token),
            _ => Ok(()),
        };
        // Release the descriptors whatever the deregistration outcome: they are taken
        // here, so `Drop` will not touch them again.
        self.close_fds();
        in_result.and(out_result)
    }
}

impl Drop for FdDriver {
    fn drop(&mut self) {
        // `stop` already released the fds on the normal teardown path; this covers a
        // driver dropped without ever being stopped.
        self.close_fds();
    }
}

/// Puts `fd` into non-blocking mode so no read or write can stall the reactor.
///
/// Shared with the sibling [`crate::drivers::spawn`] driver, which puts the two standard
/// streams of the child it starts into non-blocking mode for the same reason.
pub(crate) fn set_nonblocking(fd: RawFd) -> Result<()> {
    // SAFETY: `fd` is a descriptor this driver owns; `F_GETFL` reads the status flags
    // and takes no extra argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    // SAFETY: the same owned descriptor; setting the current status flags plus
    // `O_NONBLOCK` is a valid `F_SETFL` update that neither closes nor replaces the fd.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Parses the `line` command's optional maximum-length argument.
///
/// The argument is a decimal byte count, optionally prefixed with `line,` so that the
/// [`FdDriver::options_reply`] of `getopts` (`line,<N>`) can be fed straight back in.
///
/// # Errors
///
/// Returns [`beam_common::BeamError::BadArg`] when the argument is not valid UTF-8 or not
/// a decimal number.
fn parse_line_len(arg: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(arg).map_err(|_| error::badarg())?;
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix("line,").unwrap_or(trimmed).trim();
    digits.parse::<usize>().map_err(|_| error::badarg())
}

#[cfg(test)]
mod tests {
    use super::FdDriver;
    use crate::drivers::Phase;
    use crate::poller::Poller;
    use crate::port::{PortRegistry, PortState};
    use beam_common::BeamError;
    use beam_process::PortId;
    use std::io::{Read, Write};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    fn poller() -> Poller {
        Poller::new().expect("a poll set")
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
    fn pipe_round_trip_through_the_poller() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("the port starts");
        assert_eq!(registry.get(id).expect("live").state(), PortState::Open);
        assert_eq!(registry.get(id).expect("live").driver_name(), "fd");

        // `output` writes to the pipe synchronously, so the bytes are already waiting on
        // the read end when the poller reports read readiness.
        assert_eq!(
            registry.output(&mut poller, id, b"hello").expect("writes"),
            5
        );
        assert_eq!(
            pump_until_input(&mut poller, &mut registry, id, 5),
            b"hello"
        );

        registry.close(&mut poller, id).expect("closes");
        assert!(registry.get(id).is_none());
    }

    #[test]
    fn a_write_only_driver_writes_but_cannot_read() {
        let (peer, write_end) = UnixStream::pair().expect("socketpair");
        // SAFETY: `write_end.as_raw_fd()` names an open descriptor of a live stream.
        let fd = unsafe { libc::dup(write_end.as_raw_fd()) };
        assert!(fd >= 0, "dup(2) succeeded");

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(FdDriver::write_only(fd)))
            .expect("the port starts");

        // The write fd is registered only implicitly, so `output` writes directly.
        assert_eq!(registry.output(&mut poller, id, b"xy").expect("writes"), 2);
        let mut received = [0u8; 4];
        let mut reader = &peer;
        assert_eq!(reader.read(&mut received).expect("reads"), 2);
        assert_eq!(&received[..2], b"xy");

        // With no read fd there is nothing to serve read readiness from.
        assert!(matches!(
            registry.get_mut(id).expect("live").ready_input(&mut poller),
            Err(BeamError::BadArg)
        ));
        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn a_read_only_driver_reports_eof_and_stops_reading() {
        let (peer, read_end) = UnixStream::pair().expect("socketpair");
        // SAFETY: `read_end.as_raw_fd()` names an open descriptor of a live stream.
        let fd = unsafe { libc::dup(read_end.as_raw_fd()) };
        assert!(fd >= 0, "dup(2) succeeded");

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(FdDriver::read_only(fd)))
            .expect("the port starts");

        let mut writer = &peer;
        writer.write_all(b"bye").expect("writes");
        assert_eq!(pump_until_input(&mut poller, &mut registry, id, 3), b"bye");

        // Closing the peer turns the read fd into EOF; the driver latches it and never
        // appends anything again.
        drop(peer);
        let events = poller
            .poll(Some(Duration::from_millis(200)))
            .expect("poll succeeds");
        registry
            .handle_events(&mut poller, &events)
            .expect("events dispatch");
        let port = registry.get_mut(id).expect("live");
        let mut out = [0u8; 8];
        assert_eq!(port.read_input(&mut out), 0, "EOF yields no bytes");
        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn an_inert_driver_reports_bad_arg_for_both_directions() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(FdDriver::from_fds(None, None)))
            .expect("an inert port still starts");
        assert!(matches!(
            registry.output(&mut poller, id, b"x"),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            registry.get_mut(id).expect("live").ready_input(&mut poller),
            Err(BeamError::BadArg)
        ));
        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn stop_deregisters_the_read_fd() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("the port starts");
        assert_eq!(poller.len(), 1, "start registers the read fd");

        registry.close(&mut poller, id).expect("closes");
        assert!(poller.is_empty(), "stop deregisters the read fd");
    }

    #[test]
    fn dropping_a_driver_closes_the_fds_it_owns() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a valid, writable 2-element array for `pipe(2)`.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "pipe(2) succeeded");

        // Move both ends well above the range every other test allocates from, so the
        // EBADF checks below cannot race with a concurrent test reusing the number.
        // SAFETY: both arguments name open descriptors owned by this test.
        let read_fd = unsafe { libc::fcntl(fds[0], libc::F_DUPFD, 1000) };
        // SAFETY: as above.
        let write_fd = unsafe { libc::fcntl(fds[1], libc::F_DUPFD, 1000) };
        assert!(read_fd >= 1000 && write_fd >= 1000, "F_DUPFD succeeded");
        for fd in fds {
            // SAFETY: the originals are owned by this test and are no longer needed.
            let _ = unsafe { libc::close(fd) };
        }

        drop(FdDriver::connected(read_fd, write_fd));

        for fd in [read_fd, write_fd] {
            // SAFETY: `F_GETFD` only inspects the descriptor; it cannot have been reused
            // because it lives far above the range the process allocates from.
            let res = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            let err = std::io::Error::last_os_error();
            assert_eq!(res, -1, "fd {fd} was closed by Drop");
            assert_eq!(err.raw_os_error(), Some(libc::EBADF), "fd {fd} is gone");
        }
    }

    #[test]
    fn stop_then_drop_does_not_double_close() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a valid, writable 2-element array for `pipe(2)`.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "pipe(2) succeeded");
        // SAFETY: as above.
        let read_fd = unsafe { libc::fcntl(fds[0], libc::F_DUPFD, 2000) };
        // SAFETY: as above.
        let write_fd = unsafe { libc::fcntl(fds[1], libc::F_DUPFD, 2000) };
        assert!(read_fd >= 2000 && write_fd >= 2000, "F_DUPFD succeeded");
        for fd in fds {
            // SAFETY: the originals are owned by this test.
            let _ = unsafe { libc::close(fd) };
        }

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(
                &mut poller,
                Box::new(FdDriver::connected(read_fd, write_fd)),
            )
            .expect("the port starts");
        // `close` runs the driver's `stop` and then drops it, exercising the
        // `stop` + `Drop` overlap.
        registry.close(&mut poller, id).expect("closes");
        assert!(poller.is_empty());

        for fd in [read_fd, write_fd] {
            // SAFETY: `F_GETFD` only inspects the descriptor.
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) },
                -1,
                "fd {fd} is closed"
            );
        }
    }

    #[test]
    fn phase_and_line_mode_accessors_track_the_configuration() {
        let mut driver = FdDriver::inert();
        // A fresh driver is in the raw binary phase, so nothing is framed.
        assert_eq!(driver.phase(), Phase::Binary);
        assert!(!driver.is_line_buffered());
        assert_eq!(driver.pending_input(), 0);

        driver.set_line_mode(8);
        assert_eq!(driver.phase(), Phase::Text);
        assert!(driver.is_line_buffered());
        assert_eq!(driver.max_line_len(), 8);

        // Changing the limit does not change the phase.
        driver.set_max_line_len(4);
        assert_eq!(driver.max_line_len(), 4);
        assert_eq!(driver.phase(), Phase::Text);

        driver.set_phase(Phase::Binary);
        assert_eq!(driver.phase(), Phase::Binary);
        assert!(!driver.is_line_buffered());
        driver.set_phase(Phase::Text);
        assert_eq!(driver.phase(), Phase::Text);

        driver.set_binary_mode();
        assert_eq!(driver.phase(), Phase::Binary);

        // The builder form is equivalent to `set_line_mode`.
        let built = FdDriver::pipe().expect("a pipe").with_line_mode(16);
        assert!(built.is_line_buffered());
        assert_eq!(built.max_line_len(), 16);
    }

    #[test]
    fn the_control_family_toggles_line_and_phase() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(FdDriver::inert()))
            .expect("the port starts");

        let opts = |registry: &mut PortRegistry, poller: &mut Poller| -> Vec<u8> {
            registry
                .get_mut(id)
                .expect("live")
                .control(poller, "getopts", b"")
                .expect("getopts")
        };
        let ctl = |registry: &mut PortRegistry,
                   poller: &mut Poller,
                   cmd: &str,
                   arg: &[u8]|
         -> beam_common::Result<Vec<u8>> {
            registry
                .get_mut(id)
                .expect("live")
                .control(poller, cmd, arg)
        };

        // Default: raw binary phase.
        assert_eq!(opts(&mut registry, &mut poller), b"binary");

        // `line` with no argument keeps the default limit.
        assert!(ctl(&mut registry, &mut poller, "line", b"").expect("line").is_empty());
        assert_eq!(opts(&mut registry, &mut poller), b"line,1024");

        // `line,<N>` (the getopts reply shape) round-trips.
        ctl(&mut registry, &mut poller, "line", b"line,512").expect("line,512");
        assert_eq!(opts(&mut registry, &mut poller), b"line,512");
        ctl(&mut registry, &mut poller, "line", b"32").expect("line 32");
        assert_eq!(opts(&mut registry, &mut poller), b"line,32");

        // `text` keeps the limit but switches phase; `noline`/`binary` reset to binary.
        ctl(&mut registry, &mut poller, "text", b"").expect("text");
        assert_eq!(opts(&mut registry, &mut poller), b"line,32");
        ctl(&mut registry, &mut poller, "noline", b"").expect("noline");
        assert_eq!(opts(&mut registry, &mut poller), b"binary");
        ctl(&mut registry, &mut poller, "text", b"").expect("text");
        ctl(&mut registry, &mut poller, "binary", b"").expect("binary");
        assert_eq!(opts(&mut registry, &mut poller), b"binary");

        // A malformed length argument and an unknown command are both rejected without
        // changing the options.
        assert!(matches!(
            ctl(&mut registry, &mut poller, "line", b"not-a-number"),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            ctl(&mut registry, &mut poller, "bogus", b""),
            Err(BeamError::BadArg)
        ));
        assert_eq!(opts(&mut registry, &mut poller), b"binary");

        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn line_mode_delivers_only_complete_lines() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(
                &mut poller,
                Box::new(FdDriver::pipe().expect("a pipe").with_line_mode(1024)),
            )
            .expect("the port starts");

        // "one\ntw": only the complete first line may cross the port boundary.
        assert_eq!(
            registry
                .output(&mut poller, id, b"one\ntw")
                .expect("writes"),
            6
        );
        let events = poller
            .poll(Some(Duration::from_millis(200)))
            .expect("poll succeeds");
        registry
            .handle_events(&mut poller, &events)
            .expect("events dispatch");
        assert_eq!(
            registry.get_mut(id).expect("live").drain_input(),
            b"one",
            "the partial second line is held back"
        );
        assert!(registry
            .get_mut(id)
            .expect("live")
            .drain_input()
            .is_empty());

        // Completing the second line releases it.
        assert_eq!(
            registry
                .output(&mut poller, id, b"o\n")
                .expect("writes"),
            2
        );
        assert_eq!(
            pump_until_input(&mut poller, &mut registry, id, 3),
            b"two"
        );

        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn turning_line_mode_off_restores_raw_pass_through() {
        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(FdDriver::pipe().expect("a pipe")))
            .expect("the port starts");

        // Enable line mode, then disable it through control: input must pass through
        // unframed again.
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "line", b"")
            .expect("line");
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "binary", b"")
            .expect("binary");

        assert_eq!(
            registry
                .output(&mut poller, id, b"a\nb")
                .expect("writes"),
            3
        );
        assert_eq!(
            pump_until_input(&mut poller, &mut registry, id, 3),
            b"a\nb",
            "the raw bytes arrive unmodified"
        );

        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn line_mode_flushes_a_partial_line_at_eof() {
        let (peer, read_end) = UnixStream::pair().expect("socketpair");
        // SAFETY: `read_end.as_raw_fd()` names an open descriptor of a live stream.
        let fd = unsafe { libc::dup(read_end.as_raw_fd()) };
        assert!(fd >= 0, "dup(2) succeeded");

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(
                &mut poller,
                Box::new(FdDriver::read_only(fd).with_line_mode(1024)),
            )
            .expect("the port starts");

        let mut writer = &peer;
        writer
            .write_all(b"complete\npartial")
            .expect("writes");

        // The complete line is delivered; the trailing partial one is still buffered.
        assert_eq!(
            pump_until_input(&mut poller, &mut registry, id, 8),
            b"complete"
        );

        // Closing the peer delivers EOF, which flushes the partial line.
        drop(peer);
        assert_eq!(
            pump_until_input(&mut poller, &mut registry, id, 7),
            b"partial",
            "EOF flushes the trailing partial line"
        );

        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn eof_after_the_peer_closes_delivers_pending_bytes_then_stays_quiet() {
        let (peer, read_end) = UnixStream::pair().expect("socketpair");
        // SAFETY: `read_end.as_raw_fd()` names an open descriptor of a live stream.
        let fd = unsafe { libc::dup(read_end.as_raw_fd()) };
        assert!(fd >= 0, "dup(2) succeeded");

        let mut poller = poller();
        let mut registry = PortRegistry::new();
        let id = registry
            .open(&mut poller, Box::new(FdDriver::read_only(fd)))
            .expect("the port starts");

        // Write and then close the peer: the bytes and the EOF become ready together.
        let mut writer = &peer;
        writer.write_all(b"tail").expect("writes");
        drop(peer);

        assert_eq!(
            pump_until_input(&mut poller, &mut registry, id, 4),
            b"tail",
            "bytes written before the close are delivered"
        );

        // A further read after the latched EOF must neither error nor add bytes.
        let before = registry.get(id).expect("live").input_len();
        registry
            .get_mut(id)
            .expect("live")
            .ready_input(&mut poller)
            .expect("a post-EOF ready_input is not an error");
        assert_eq!(
            registry.get(id).expect("live").input_len(),
            before,
            "nothing new arrives after EOF"
        );
        assert!(registry
            .get_mut(id)
            .expect("live")
            .drain_input()
            .is_empty());

        registry.close(&mut poller, id).expect("closes");
    }
}
