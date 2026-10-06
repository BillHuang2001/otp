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
//! Commands the *efile* family adds on top of [`FdDriver`]'s (`line`/`noline`/`binary`/
//! `text`/`getopts`, which are delegated unchanged):
//!
//! | command | argument | effect / reply |
//! |---|---|---|
//! | `open` | path bytes | open the path read-only non-blocking and register it; no reply |
//! | `close` | — | deregister and close the attached read fd; no reply |
//! | `path` | — | reply the currently open path (empty when unopened) |
//! | `eof` | — | reply `true`/`false` for the latched end-of-file |
//!
//! An unknown command is rejected with [`beam_common::BeamError::BadArg`].
//!
//! # Known limitation (the file-readiness seam)
//!
//! [`EfileDriver::control`]'s `open` registers the fd with the `mio`-backed poller. Linux
//! `epoll` refuses to register **regular files** (`EPOLL_CTL_ADD` ⇒ `EPERM`), so `open` on
//! a regular file currently fails with [`beam_common::BeamError::Io`]; it works for the
//! descriptors epoll *does* support — pipes, FIFOs, character devices and sockets — which
//! is what makes [`EfileDriver::read_only`] over a caller-supplied fd (and the FIFO test in
//! this module) usable today. A future backend that serves regular files (a dedicated
//! `efile` readiness source, or an `io_uring`/thread-pool path) would drop this limitation
//! without changing this driver's interface; the emulator solves the same problem with its
//! poll-thread/`erts_poll` machinery (`erts/emulator/sys/common/erl_poll.c`).

use std::os::unix::io::RawFd;

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
use crate::drivers::fd::FdDriver;
use crate::error;

/// The `efile` driver: an [`FdDriver`] plus the `efile` control family.
pub struct EfileDriver {
    /// The underlying byte-stream driver (registration, read/write, line/phase).
    inner: FdDriver,
    /// The path most recently opened through the `open` command, if any.
    path: Option<Vec<u8>>,
}

impl EfileDriver {
    /// An inert, unopened `efile` port: it owns no fds until a `open` control command
    /// attaches one.
    pub fn unopened() -> EfileDriver {
        EfileDriver {
            inner: FdDriver::inert(),
            path: None,
        }
    }

    /// An `efile` port reading from `fd` (which the driver then owns).
    pub fn read_only(fd: RawFd) -> EfileDriver {
        EfileDriver {
            inner: FdDriver::read_only(fd),
            path: None,
        }
    }

    /// An `efile` port writing to `fd` (which the driver then owns).
    pub fn write_only(fd: RawFd) -> EfileDriver {
        EfileDriver {
            inner: FdDriver::write_only(fd),
            path: None,
        }
    }

    /// An `efile` port reading from `read_fd` and writing to `write_fd` (both owned).
    pub fn connected(read_fd: RawFd, write_fd: RawFd) -> EfileDriver {
        EfileDriver {
            inner: FdDriver::connected(read_fd, write_fd),
            path: None,
        }
    }

    /// The path most recently opened through the `open` control command.
    pub fn path(&self) -> Option<&[u8]> {
        self.path.as_deref()
    }

    /// Whether the read fd has reported end-of-file.
    pub fn eof(&self) -> bool {
        self.inner.eof()
    }

    /// Opens `path` read-only, non-blocking and attaches it as the read fd.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when no path is given or a read fd is
    /// already attached, and [`beam_common::BeamError::Io`] when `open(2)` or the poller
    /// registration fails (the latter includes the regular-file `EPERM` case described in
    /// the module docs).
    fn open_path(&mut self, ctx: &mut DriverContext<'_>, path: &[u8]) -> Result<()> {
        if path.is_empty() || path.contains(&0) {
            return Err(error::badarg());
        }
        if self.inner.has_read_fd() {
            return Err(error::badarg());
        }
        let mut c_path = path.to_vec();
        c_path.push(0);
        // SAFETY: `c_path` is a NUL-terminated byte string owned by this call, so
        // `open(2)` only reads it; the flags/`mode` combination is valid and `open`
        // returns a fresh descriptor owned by this driver on success. `O_NONBLOCK`
        // matters for FIFOs, whose `open` would otherwise block until a writer appears.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr().cast(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(error::io(std::io::Error::last_os_error()));
        }
        match self.inner.adopt_read_fd(ctx, fd) {
            Ok(()) => {
                self.path = Some(path.to_vec());
                Ok(())
            }
            Err(err) => {
                // Registration failed, so ownership never transferred: close the fd here.
                // SAFETY: `fd` was returned by `open(2)` above and has not been stored.
                let _ = unsafe { libc::close(fd) };
                Err(err)
            }
        }
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
                self.inner.release_read_fd(ctx)?;
                self.path = None;
                Ok(Vec::new())
            }
            "path" => Ok(self.path.clone().unwrap_or_default()),
            "eof" => Ok(if self.inner.eof() {
                b"true".to_vec()
            } else {
                b"false".to_vec()
            }),
            // Everything else (`line`/`noline`/`binary`/`text`/`getopts`) is the shared
            // byte-stream family, handled by the composed driver.
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
}
