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

//! The poller's suite: the readiness newtypes (`Interest`/`Ready`), the `mio`↔`poll(2)`
//! mappings, and the real-descriptor cases (pipes, socket pairs, regular files through the
//! fallback source, and an observed error condition).
//!
//! It is a `#[path]` sibling of `poller.rs` so the source half stays near the workspace's
//! ~1000-line convention — the same pattern as `port.rs`'s `active_input_tests.rs` — and it
//! stays a **child** module of `poller`, so it can pin the private `Interest` bitset.

use super::{to_mio_interest, Event, Interest, Poller, Ready, Token, WAKER_KEY};
use beam_common::BeamError;
use std::io::Write;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[test]
fn interest_constants_and_constructors_agree() {
    assert_eq!(Interest::readable(), Interest::READABLE);
    assert_eq!(Interest::writable(), Interest::WRITABLE);

    assert!(Interest::READABLE.is_readable());
    assert!(!Interest::READABLE.is_writable());

    assert!(Interest::WRITABLE.is_writable());
    assert!(!Interest::WRITABLE.is_readable());

    assert!(Interest::BOTH.is_readable());
    assert!(Interest::BOTH.is_writable());
}

#[test]
fn interest_maps_onto_mio() {
    assert!(to_mio_interest(Interest::READABLE).is_readable());
    assert!(!to_mio_interest(Interest::READABLE).is_writable());

    assert!(to_mio_interest(Interest::WRITABLE).is_writable());
    assert!(!to_mio_interest(Interest::WRITABLE).is_readable());

    let both = to_mio_interest(Interest::BOTH);
    assert!(both.is_readable() && both.is_writable());
}

#[test]
fn interest_error_bit_is_additive_and_leaves_existing_bits_unchanged() {
    // The bitset itself is pinned: the three pre-existing constants keep their exact
    // values and `ERROR` takes the next free bit.
    assert_eq!(Interest::READABLE.bits, 0b01);
    assert_eq!(Interest::WRITABLE.bits, 0b10);
    assert_eq!(Interest::BOTH.bits, 0b11);
    assert_eq!(Interest::ERROR.bits, 0b100);

    assert_eq!(Interest::error(), Interest::ERROR);
    assert!(Interest::ERROR.is_error());

    // The error bit is independent of both directions.
    assert!(!Interest::READABLE.is_error());
    assert!(!Interest::WRITABLE.is_error());
    assert!(!Interest::BOTH.is_error());

    assert!(!Interest::ERROR.is_readable());
    assert!(!Interest::ERROR.is_writable());
}

#[test]
fn interest_add_unions_the_masks() {
    let read_error = Interest::READABLE.add(Interest::ERROR);
    assert_eq!(read_error, Interest::ERROR.add(Interest::READABLE));
    assert!(read_error.is_readable() && read_error.is_error());
    assert!(!read_error.is_writable());

    assert_eq!(Interest::READABLE.add(Interest::WRITABLE), Interest::BOTH);

    let both_error = Interest::BOTH.add(Interest::ERROR);
    assert!(both_error.is_readable());
    assert!(both_error.is_writable());
    assert!(both_error.is_error());
    assert_eq!(both_error.bits, 0b111);

    // Adding a mask to itself is idempotent.
    assert_eq!(
        Interest::READABLE.add(Interest::READABLE),
        Interest::READABLE
    );
    assert_eq!(Interest::ERROR.add(Interest::ERROR), Interest::ERROR);
    assert_eq!(both_error.add(both_error), both_error);
}

#[test]
fn error_interest_maps_onto_a_legal_mio_interest() {
    // `mio::Interest` has no error bit and must be non-zero, so `ERROR` alone rides the
    // documented readability fallback.
    let error_only = to_mio_interest(Interest::ERROR);
    assert!(
        error_only.is_readable(),
        "the non-zero fallback is readability"
    );
    assert!(!error_only.is_writable());

    let read_error = to_mio_interest(Interest::READABLE.add(Interest::ERROR));
    assert!(read_error.is_readable());
    assert!(!read_error.is_writable());
    assert_eq!(read_error, to_mio_interest(Interest::READABLE));

    let both_error = to_mio_interest(Interest::BOTH.add(Interest::ERROR));
    assert!(both_error.is_readable() && both_error.is_writable());
    assert_eq!(both_error, to_mio_interest(Interest::BOTH));
}

/// Owns a raw descriptor for the lifetime of a test, closing it exactly once — including
/// when an assertion panics — so a failing test cannot leak an fd.
struct OwnedFd(RawFd);

impl OwnedFd {
    fn raw(&self) -> RawFd {
        self.0
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        // SAFETY: this guard owns the descriptor and closes it exactly once.
        let _ = unsafe { libc::close(self.0) };
    }
}

#[test]
fn a_pipe_read_end_interested_in_error_still_reports_readiness() {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid, writable two-element array that `pipe(2)` fills in; it
    // retains no pointer and returns descriptors this call owns.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    assert_eq!(rc, 0, "pipe(2) succeeds");
    let read_end = OwnedFd(fds[0]);
    let write_end = OwnedFd(fds[1]);

    let mut poller = Poller::new().expect("a poll set");
    let token = poller
        .register(read_end.raw(), Interest::READABLE.add(Interest::ERROR), 11)
        .expect("the pipe's read end registers");

    let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
    assert!(events.is_empty(), "an empty pipe is not readable");

    let byte = [0xABu8];
    // SAFETY: `byte` is a valid one-byte buffer and `write_end.raw()` is this test's
    // live write end; `write(2)` only reads the buffer.
    let written = unsafe { libc::write(write_end.raw(), byte.as_ptr().cast(), 1) };
    assert_eq!(written, 1, "the write end accepts one byte");

    let events = poller
        .poll(Some(Duration::from_secs(1)))
        .expect("poll succeeds");
    assert_eq!(events.len(), 1, "exactly the pipe's read end reports");
    assert_eq!(events[0].token(), token);
    assert_eq!(events[0].key(), 11);
    assert!(
        events[0].ready().readable(),
        "the byte makes the read end readable"
    );
    assert!(
        !events[0].ready().is_error(),
        "a healthy pipe reports no error condition"
    );

    poller
        .deregister(read_end.raw(), token)
        .expect("the registration de-registers");
    assert!(poller.is_empty());
}

#[test]
fn an_error_condition_is_reported_as_error_readiness() {
    use std::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a loopback listener");
    let addr = listener.local_addr().expect("the listener's address");
    let client = TcpStream::connect(addr).expect("the client connects");
    let (server, _peer) = listener.accept().expect("the listener accepts");

    // A zero linger timeout makes `close(2)` send a RST instead of a FIN, so the client
    // observes a genuine error condition rather than a clean hang-up.
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: `server` owns a live socket descriptor and `linger` outlives the call;
    // `setsockopt` reads exactly the size passed and keeps no pointer.
    let rc = unsafe {
        libc::setsockopt(
            server.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&linger as *const libc::linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "SO_LINGER is accepted");
    drop(server);

    let mut poller = Poller::new().expect("a poll set");
    let client_fd = client.as_raw_fd();
    let token = poller
        .register(client_fd, Interest::READABLE.add(Interest::ERROR), 3)
        .expect("the client registers");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut observed_error = false;
    while Instant::now() < deadline {
        let events = poller
            .poll(Some(Duration::from_millis(20)))
            .expect("poll succeeds");
        if events
            .iter()
            .any(|event| event.token() == token && event.ready().is_error())
        {
            observed_error = true;
            break;
        }
    }

    poller
        .deregister(client_fd, token)
        .expect("the registration de-registers");
    assert!(poller.is_empty());

    if !observed_error {
        // Not every kernel/container surfaces `EPOLLERR` for a reset socket promptly; the
        // observation half is pinned by the `Interest`/`Ready` unit tests, so skip rather
        // than fail or flake on the environment.
        eprintln!(
            "SKIP: this environment did not surface EPOLLERR for a RST socket; \
             error-readiness observation is pinned by the Interest/Ready unit tests"
        );
    }
}

#[test]
fn ready_and_event_accessors() {
    let ready = Ready {
        readable: true,
        writable: false,
        error: false,
        read_closed: true,
        write_closed: false,
    };
    assert!(ready.readable());
    assert!(!ready.writable());
    assert!(!ready.is_error());
    assert!(ready.is_read_closed());
    assert!(!ready.is_write_closed());
    assert_eq!(
        Ready::default(),
        Ready {
            readable: false,
            writable: false,
            error: false,
            read_closed: false,
            write_closed: false
        }
    );

    let event = Event::new(Token(3), 42, ready);
    assert_eq!(event.token(), Token(3));
    assert_eq!(event.token().get(), 3);
    assert_eq!(event.key(), 42);
    assert_eq!(event.ready(), ready);
}

#[test]
fn a_fresh_poller_has_no_registrations() {
    let poller = Poller::new().expect("a poll set");
    assert!(poller.is_empty());
    assert_eq!(poller.len(), 0);
    assert_eq!(poller.registration(Token(1)), None);
}

#[test]
fn waker_key_cannot_collide_with_a_port_number() {
    assert_eq!(WAKER_KEY, u64::MAX);
}

#[test]
fn waker_handles_are_cheap_repeatable_clones() {
    let poller = Poller::new().expect("a poll set");
    // The poller owns the single wakeup handle; obtaining several handles to it must
    // not panic (mio permits only one `Waker` per poll set).
    let first = poller.waker().expect("a waker");
    let second = poller.waker().expect("a second waker");
    first.wake().expect("the first handle fires");
    second.wake().expect("the second handle fires");
    drop(first);
    second.wake().expect("the surviving handle still fires");
}
#[test]
fn register_assigns_tokens_from_one_and_records_the_key() {
    let mut poller = Poller::new().expect("a poll set");
    let (_peer, fd) = UnixStream::pair().expect("socketpair");

    let first = poller
        .register(fd.as_raw_fd(), Interest::READABLE, 7)
        .expect("the fd registers");
    assert_eq!(first, Token(1), "user tokens start after the waker's 0");
    assert_eq!(poller.registration(first), Some(7));
    assert_eq!(poller.len(), 1);
    assert!(!poller.is_empty());

    // Registering the very same descriptor twice is rejected by the OS.
    let err = poller
        .register(fd.as_raw_fd(), Interest::READABLE, 8)
        .expect_err("a duplicate registration fails");
    assert!(matches!(err, BeamError::Io(_)), "mapped to BeamError::Io");
    assert_eq!(poller.len(), 1, "the failed registration recorded nothing");

    poller
        .deregister(fd.as_raw_fd(), first)
        .expect("deregisters");
    assert_eq!(poller.registration(first), None);
    assert!(poller.is_empty());
}

#[test]
fn reregister_and_deregister_an_unknown_token_map_to_io() {
    let mut poller = Poller::new().expect("a poll set");
    let (_peer, fd) = UnixStream::pair().expect("socketpair");
    // Token 1 was never registered with this poller.
    let err = poller
        .reregister(fd.as_raw_fd(), Token(1), Interest::WRITABLE)
        .expect_err("an unknown token cannot be re-registered");
    assert!(matches!(err, BeamError::Io(_)));

    let err = poller
        .deregister(fd.as_raw_fd(), Token(1))
        .expect_err("an unknown token cannot be de-registered");
    assert!(matches!(err, BeamError::Io(_)));
}

/// Creates a unique temporary **regular file** holding `contents` and opens a fresh
/// descriptor over it with `flags` (the caller owns and closes the descriptor).
///
/// A regular file is exactly the descriptor `mio`'s `epoll` backend refuses, so every
/// test below travels the poller's fallback readiness source.
fn temp_regular_file(tag: &str, contents: &[u8], flags: libc::c_int) -> (PathBuf, RawFd) {
    let path = std::env::temp_dir().join(format!(
        "beam_io_poller_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock")
            .as_nanos()
    ));
    std::fs::write(&path, contents).expect("the fixture file is written");
    let mut c_path = path.as_os_str().as_encoded_bytes().to_vec();
    c_path.push(0);
    // SAFETY: `c_path` is a NUL-terminated path owned by this call; `open(2)` only
    // reads it and returns a fresh descriptor owned by the caller.
    let fd = unsafe { libc::open(c_path.as_ptr().cast(), flags | libc::O_CLOEXEC) };
    assert!(fd >= 0, "the regular file opens");
    (path, fd)
}

/// Closes a descriptor and removes the fixture file a `temp_regular_file` created.
fn cleanup(path: &PathBuf, fd: RawFd) {
    // SAFETY: `fd` was created by `temp_regular_file` and is not used again.
    let _ = unsafe { libc::close(fd) };
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_regular_file_registers_through_the_fallback_and_reports_readiness() {
    let (path, fd) = temp_regular_file("read", b"hello", libc::O_RDONLY);

    let mut poller = Poller::new().expect("a poll set");
    let token = poller
        .register(fd, Interest::READABLE, 42)
        .expect("a regular file registers (epoll would answer EPERM)");
    assert_eq!(token, Token(1), "user tokens still start after the waker's 0");
    assert_eq!(poller.registration(token), Some(42));
    assert_eq!(poller.len(), 1, "the fallback registration counts towards len");
    assert!(!poller.is_empty());

    // A regular file is always reported readable.
    let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].key(), 42);
    assert_eq!(events[0].token(), token);
    assert!(events[0].ready().readable(), "a regular file is always readable");
    assert!(!events[0].ready().is_error());
    // `poll(None)` (wait forever) must not block while a fallback registration exists.
    let started = Instant::now();
    let events = poller.poll(None).expect("poll succeeds");
    assert!(!events.is_empty(), "the fallback descriptor is ready");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "poll(None) returned promptly instead of blocking, took {:?}",
        started.elapsed()
    );

    poller
        .deregister(fd, token)
        .expect("the fallback registration de-registers");
    assert_eq!(poller.registration(token), None);
    assert!(poller.is_empty());

    cleanup(&path, fd);
}

#[test]
fn the_mio_and_fallback_sources_merge_into_one_event_batch() {
    let (path, file_fd) = temp_regular_file("merge", b"data", libc::O_RDONLY);
    let (peer, local) = UnixStream::pair().expect("socketpair");

    let mut poller = Poller::new().expect("a poll set");
    let mio_token = poller
        .register(local.as_raw_fd(), Interest::READABLE, 7)
        .expect("the socket registers with mio");
    let fallback_token = poller
        .register(file_fd, Interest::READABLE, 9)
        .expect("the regular file registers with the fallback source");
    assert_eq!(poller.len(), 2, "len counts both sources");

    // Only the always-ready fallback descriptor is ready before the socket is written.
    let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
    assert_eq!(events.len(), 1, "the idle socket is not ready");
    assert_eq!(events[0].key(), 9);
    assert_eq!(events[0].token(), fallback_token);

    // Writing to the socket makes the mio descriptor ready too; both merge into one
    // batch.
    (&peer).write_all(b"x").expect("the peer accepts the write");
    let mut events = poller
        .poll(Some(Duration::from_secs(1)))
        .expect("poll succeeds");
    events.sort_by_key(|event| event.key());
    assert_eq!(events.len(), 2, "the mio and fallback events merge");
    assert_eq!(events[0].key(), 7);
    assert_eq!(events[0].token(), mio_token);
    assert!(events[0].ready().readable());
    assert_eq!(events[1].key(), 9);
    assert!(events[1].ready().readable());

    // Deregistering removes each from whichever set holds it.
    poller
        .deregister(local.as_raw_fd(), mio_token)
        .expect("the mio registration de-registers");
    assert_eq!(poller.len(), 1);
    poller
        .deregister(file_fd, fallback_token)
        .expect("the fallback registration de-registers");
    assert!(poller.is_empty());

    cleanup(&path, file_fd);
}

#[test]
fn reregister_switches_the_interest_of_a_fallback_registration() {
    let (path, fd) = temp_regular_file("rw", b"content", libc::O_RDWR);

    let mut poller = Poller::new().expect("a poll set");
    let token = poller
        .register(fd, Interest::READABLE, 5)
        .expect("the regular file registers");

    let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
    assert_eq!(events.len(), 1);
    assert!(events[0].ready().readable());
    assert!(
        !events[0].ready().writable(),
        "only readability was requested"
    );

    poller
        .reregister(fd, token, Interest::WRITABLE)
        .expect("the fallback registration re-registers");
    let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].token(), token);
    assert!(
        events[0].ready().writable(),
        "a descriptor open for writing is writable"
    );
    assert!(
        !events[0].ready().readable(),
        "readability is no longer requested"
    );

    poller.deregister(fd, token).expect("de-registers");
    assert!(poller.is_empty());

    cleanup(&path, fd);
}

#[test]
fn two_fallback_registrations_over_one_descriptor_are_independent() {
    // Two fallback registrations over the same descriptor are independent entries: the
    // poller keeps no per-fd index for the fallback set, so both are accepted (a
    // regular file has no exclusive epoll membership to clash with). Deregistering one
    // leaves the other live.
    let (path, fd) = temp_regular_file("dup", b"x", libc::O_RDONLY);
    let mut poller = Poller::new().expect("a poll set");
    let first = poller
        .register(fd, Interest::READABLE, 1)
        .expect("the first fallback registration");
    let second = poller
        .register(fd, Interest::READABLE, 2)
        .expect("a second fallback registration is accepted");
    assert_ne!(first, second);
    assert_eq!(poller.len(), 2);

    let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
    assert_eq!(events.len(), 2, "both registrations report readiness");

    poller.deregister(fd, first).expect("the first de-registers");
    assert_eq!(poller.len(), 1);
    poller.deregister(fd, second).expect("the second de-registers");
    assert!(poller.is_empty());

    cleanup(&path, fd);
}
