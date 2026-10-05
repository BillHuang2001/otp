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
//! The poller's backend is **edge-triggered** (`mio` registers descriptors with
//! `EPOLLET`), so every callback must drain to `EAGAIN`: [`FdDriver::ready_input`] reads
//! until the fd is empty (or reports EOF) and the write path loops until the queue is
//! drained or the kernel buffer is full.
//!
//! All raw syscalls (`read`/`write`/`pipe`/`fcntl`/`close`) live in this module; every
//! `unsafe` block carries a `// SAFETY:` rationale.

use std::os::unix::io::RawFd;

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
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

    /// The shared constructor: the driver takes ownership of every fd it is handed.
    fn from_fds(read_fd: Option<RawFd>, write_fd: Option<RawFd>) -> FdDriver {
        FdDriver {
            name: "fd".to_string(),
            read_fd,
            write_fd,
            in_token: None,
            out_token: None,
            read_eof: false,
        }
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
            return Ok(());
        }
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // SAFETY: `fd` is an open, non-blocking descriptor owned by this driver and
            // `buf` is a writable array of `READ_CHUNK` bytes, so `read(2)` writes at
            // most `READ_CHUNK` bytes into it.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), READ_CHUNK) };
            if n > 0 {
                ctx.input().push(&buf[..n as usize]);
                // Edge-triggered readiness: keep reading until the fd is empty.
                continue;
            }
            if n == 0 {
                // End of file: nothing more will ever arrive on this fd.
                self.read_eof = true;
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

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
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
fn set_nonblocking(fd: RawFd) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::FdDriver;
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
}
