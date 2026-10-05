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

use std::os::unix::io::RawFd;

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
use crate::poller::Token;

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
}

impl FdDriver {
    /// Creates a driver over a fresh OS pipe: bytes written to the write fd are read back
    /// from the read fd.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when `pipe(2)` fails.
    pub fn pipe() -> Result<FdDriver> {
        todo!("libc::pipe, then wrap both fds with name = \"fd\"")
    }

    /// Adopts an existing read fd and write fd (both are assumed non-blocking).
    pub fn connected(read_fd: RawFd, write_fd: RawFd) -> FdDriver {
        let _ = (read_fd, write_fd);
        todo!("adopt both fds under the name \"fd\"")
    }

    /// Adopts an existing read-only fd.
    pub fn read_only(fd: RawFd) -> FdDriver {
        let _ = fd;
        todo!("adopt the read fd under the name \"fd\"")
    }

    /// Adopts an existing write-only fd.
    pub fn write_only(fd: RawFd) -> FdDriver {
        let _ = fd;
        todo!("adopt the write fd under the name \"fd\"")
    }
}

impl Driver for FdDriver {
    fn name(&self) -> &str {
        todo!("&self.name")
    }

    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let _ = ctx;
        todo!("register the read fd READABLE (and the write fd WRITABLE if there is pending output)")
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let _ = ctx;
        todo!("libc::write from ctx.output() to the write fd; on EAGAIN leave the rest queued and arm WRITABLE")
    }

    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let _ = ctx;
        todo!("libc::read from the read fd into ctx.input(), EAGAIN-tolerant; EOF stops further reads")
    }

    fn ready_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let _ = ctx;
        todo!("resume draining the output queue after write readiness")
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let _ = ctx;
        todo!("deregister any registered fds, then close the owned fds")
    }
}

impl Drop for FdDriver {
    fn drop(&mut self) {
        todo!("close any still-owned fds (stop() already closed them on the normal path)")
    }
}
