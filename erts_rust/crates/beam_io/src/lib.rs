#![doc = "Poller/reactor (epoll/kqueue/io_uring), ports and built-in drivers."]

//! I/O subsystem: the poller/reactor, ports and built-in drivers.
//!
//! Implements the OS/driver seam called out in Part II §A (one of the two clean
//! rewrite seams) and the Stage-5 row of Part II §H. Provides the epoll/kqueue/
//! io_uring event loop, port programs, the spawn/fork driver and the built-in
//! drivers (`tcp_inet`, `efile`, `fd_driver`, …) behind the frozen driver C ABI.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_io() -> &'static str {
    "beam_io"
}
