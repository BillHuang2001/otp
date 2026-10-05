//! The `mio`-backed event poller/reactor and its readiness newtypes.
//!
//! [`Poller`] wraps a portable [`mio::Poll`] (epoll on Linux, kqueue elsewhere) and
//! exposes a **mio-independent** registration API: callers register **raw file
//! descriptors** ([`RawFd`]) with an [`Interest`] mask and an opaque `u64` key, then
//! receive owned [`Event`]s from [`Poller::poll`]. This mirrors the legacy C poll loop's
//! one-shot readiness model (`erts/emulator/sys/common/erl_poll.c`, `erl_check_io.c`)
//! while letting a future `io_uring`/`/dev/poll` backend be swapped in behind the same
//! API.
//!
//! [`Token`] `0` is reserved for the poller's own asynchronous [`Waker`]; user
//! registrations start at token `1`. The `u64` "key" travels opaquely with every event so
//! the reactor ([`crate::port::PortRegistry::handle_events`]) can look the owning port up
//! by its port number without the poller knowing what a port is. The key of a waker event
//! is [`WAKER_KEY`].

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::time::Duration;

use beam_common::Result;

/// The key carried by the poller's own waker event.
///
/// It is [`u64::MAX`], chosen so it cannot collide with a port number used as a
/// registration key.
pub const WAKER_KEY: u64 = u64::MAX;

/// An opaque registration handle returned by [`Poller::register`].
///
/// Token `0` is reserved for the poller's internal [`Waker`]; user registrations are
/// assigned increasing tokens starting at `1`. A token is stable for the life of the
/// registration and is how a caller re-registers or de-registers a descriptor.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Token(usize);

impl Token {
    /// The raw index of this token.
    pub fn get(self) -> usize {
        self.0
    }
}

/// A readiness mask requested at registration time.
///
/// It is a tiny bitset: whether the caller wants to be notified when the descriptor is
/// readable and/or writable. Use the [`Interest::READABLE`], [`Interest::WRITABLE`] and
/// [`Interest::BOTH`] constants or the [`Interest::readable`]/[`Interest::writable`]
/// constructors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Interest {
    bits: u8,
}

impl Interest {
    /// Interested in readability only.
    pub const READABLE: Interest = Interest { bits: 0b01 };

    /// Interested in writability only.
    pub const WRITABLE: Interest = Interest { bits: 0b10 };

    /// Interested in both readiness directions.
    pub const BOTH: Interest = Interest { bits: 0b11 };

    /// An [`Interest`] in readability only (same as [`Interest::READABLE`]).
    pub const fn readable() -> Interest {
        Interest::READABLE
    }

    /// An [`Interest`] in writability only (same as [`Interest::WRITABLE`]).
    pub const fn writable() -> Interest {
        Interest::WRITABLE
    }

    /// Whether this mask includes readability.
    pub fn is_readable(self) -> bool {
        self.bits & 0b01 != 0
    }

    /// Whether this mask includes writability.
    pub fn is_writable(self) -> bool {
        self.bits & 0b10 != 0
    }
}

/// The readiness actually observed for a descriptor.
///
/// Unlike [`Interest`], a [`Ready`] value reports what the operating system observed in a
/// [`Poller::poll`] round: readability, writability, an error condition and the
/// read/write half-close (EOF) edges.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Ready {
    readable: bool,
    writable: bool,
    error: bool,
    read_closed: bool,
    write_closed: bool,
}

impl Ready {
    /// Whether the descriptor is readable.
    pub fn readable(self) -> bool {
        self.readable
    }

    /// Whether the descriptor is writable.
    pub fn writable(self) -> bool {
        self.writable
    }

    /// Whether the descriptor reported an error condition.
    pub fn is_error(self) -> bool {
        self.error
    }

    /// Whether the read half of the descriptor was closed by the peer (EOF for reads).
    pub fn is_read_closed(self) -> bool {
        self.read_closed
    }

    /// Whether the write half of the descriptor was closed by the peer.
    pub fn is_write_closed(self) -> bool {
        self.write_closed
    }
}

/// One readiness event produced by [`Poller::poll`].
///
/// It pairs the opaque [`Token`] assigned at registration with the caller's `u64` key and
/// the observed [`Ready`] mask, so the reactor can find the owning port by key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event {
    token: Token,
    key: u64,
    ready: Ready,
}

impl Event {
    /// Builds an event from its parts (used by the poller and by the port registry's
    /// tests).
    pub fn new(token: Token, key: u64, ready: Ready) -> Event {
        Event { token, key, ready }
    }

    /// The registration token this event belongs to.
    pub fn token(self) -> Token {
        self.token
    }

    /// The caller key supplied at registration time (a port number for port events).
    pub fn key(self) -> u64 {
        self.key
    }

    /// The observed readiness.
    pub fn ready(self) -> Ready {
        self.ready
    }
}

/// The reactor: a `mio::Poll` plus the token→key registration bookkeeping.
///
/// Register descriptors with [`Poller::register`] (giving each an [`Interest`] and a `u64`
/// key), drain readiness with [`Poller::poll`] and hand the returned [`Event`]s to the
/// port registry. Token `0` is reserved for the poller's own [`Waker`], which it creates
/// once and hands out shared handles to via [`Poller::waker`]; user tokens start at `1`.
pub struct Poller {
    /// The underlying portable poll set.
    poll: mio::Poll,
    /// The reusable event buffer drained by [`Poller::poll`].
    events: mio::Events,
    /// The single wakeup handle for this poll set, shared with every [`Waker`] handed out.
    waker: Arc<mio::Waker>,
    /// Token index → the caller key it was registered with.
    registrations: HashMap<usize, u64>,
    /// The next token index to hand out (starts after the waker's token `0`).
    next_token: usize,
}

/// Translates this crate's [`Interest`] mask into `mio`'s.
///
/// `mio::Interest` is a non-zero bitset, so an empty mask (no readiness direction
/// requested) falls back to readability, which is always a legal registration.
fn to_mio_interest(interest: Interest) -> mio::Interest {
    match (interest.is_readable(), interest.is_writable()) {
        (true, true) => mio::Interest::READABLE.add(mio::Interest::WRITABLE),
        (false, true) => mio::Interest::WRITABLE,
        _ => mio::Interest::READABLE,
    }
}

/// Maps a `mio` event's readiness onto this crate's [`Ready`].
fn to_ready(event: &mio::event::Event) -> Ready {
    Ready {
        readable: event.is_readable(),
        writable: event.is_writable(),
        error: event.is_error(),
        read_closed: event.is_read_closed(),
        write_closed: event.is_write_closed(),
    }
}

impl Poller {
    /// Creates an empty poller.
    ///
    /// The poller's single wakeup handle is created here, under the reserved token `0`;
    /// [`Poller::waker`] hands out cheap shared handles to it.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` if the operating system refuses to create the poll set or
    /// its wakeup handle.
    pub fn new() -> Result<Poller> {
        let poll = mio::Poll::new().map_err(crate::error::io)?;
        let waker = mio::Waker::new(poll.registry(), mio::Token(0)).map_err(crate::error::io)?;
        Ok(Poller {
            poll,
            events: mio::Events::with_capacity(1024),
            waker: Arc::new(waker),
            registrations: HashMap::new(),
            next_token: 1,
        })
    }

    /// Registers `fd` with the poller, requesting `interest` and tagging it with `key`.
    ///
    /// Returns the [`Token`] assigned to the registration. Registering the same `fd`
    /// twice is an error (`BeamError::Io`, `AlreadyExists` underlying).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the operating system rejects the registration, or
    /// `BeamError::SystemLimit` when the token space is exhausted.
    pub fn register(&mut self, fd: RawFd, interest: Interest, key: u64) -> Result<Token> {
        let index = self.next_token;
        if index == usize::MAX {
            return Err(crate::error::system_limit());
        }
        // `SourceFd` borrows the fd only for the duration of the syscall; the local
        // binding gives it a place expression to borrow.
        let mut fd = fd;
        let mut source = mio::unix::SourceFd(&mut fd);
        self.poll
            .registry()
            .register(&mut source, mio::Token(index), to_mio_interest(interest))
            .map_err(crate::error::io)?;
        self.registrations.insert(index, key);
        self.next_token = index.saturating_add(1);
        Ok(Token(index))
    }

    /// Changes the [`Interest`] mask of a previously registered descriptor.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when `token` is unknown or the operating system rejects the
    /// re-registration.
    pub fn reregister(&mut self, fd: RawFd, token: Token, interest: Interest) -> Result<()> {
        let mut fd = fd;
        let mut source = mio::unix::SourceFd(&mut fd);
        self.poll
            .registry()
            .reregister(
                &mut source,
                mio::Token(token.get()),
                to_mio_interest(interest),
            )
            .map_err(crate::error::io)
    }

    /// Removes a registration, freeing its token.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when `token` is unknown or the operating system rejects the
    /// de-registration.
    pub fn deregister(&mut self, fd: RawFd, token: Token) -> Result<()> {
        let mut fd = fd;
        let mut source = mio::unix::SourceFd(&mut fd);
        self.poll
            .registry()
            .deregister(&mut source)
            .map_err(crate::error::io)?;
        self.registrations.remove(&token.get());
        Ok(())
    }

    /// Waits for readiness and returns the observed events.
    ///
    /// The events are returned **owned** so the caller can take `&mut Poller` again to
    /// re-register while iterating them. An empty result means the timeout elapsed with no
    /// readiness; `timeout = None` blocks until an event arrives (interruptible by
    /// [`Waker::wake`]).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the wait fails.
    pub fn poll(&mut self, timeout: Option<Duration>) -> Result<Vec<Event>> {
        self.poll
            .poll(&mut self.events, timeout)
            .map_err(crate::error::io)?;
        let registrations = &self.registrations;
        let events: Vec<Event> = self
            .events
            .iter()
            .map(|event| {
                let index = event.token().0;
                // Token `0` is the waker (and any token the map no longer knows is
                // reported as a waker key, so it can never be mistaken for a port).
                let key = registrations.get(&index).copied().unwrap_or(WAKER_KEY);
                Event::new(Token(index), key, to_ready(event))
            })
            .collect();
        Ok(events)
    }

    /// Returns a thread-safe [`Waker`] that unblocks a concurrent [`Poller::poll`].
    ///
    /// The poller owns a **single** wakeup handle (created in [`Poller::new`]); this returns
    /// a cheap cloneable handle to it. Call it as many times as needed, from any thread — the
    /// wakeup handle stays alive for as long as the [`Poller`] does.
    ///
    /// The woken event carries the poller's reserved token `0` and [`WAKER_KEY`] as its
    /// key.
    ///
    /// # Errors
    ///
    /// This call cannot fail; it returns `Result` for API symmetry with the other poller
    /// methods.
    pub fn waker(&self) -> Result<Waker> {
        Ok(Waker(Arc::clone(&self.waker)))
    }
    /// The caller key a token was registered with, if the token is still live.
    pub fn registration(&self, token: Token) -> Option<u64> {
        self.registrations.get(&token.get()).copied()
    }

    /// The number of live registrations (excluding the internal waker).
    pub fn len(&self) -> usize {
        self.registrations.len()
    }

    /// Whether the poller has no live registrations.
    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }
}

/// An asynchronous notification handle that unblocks a [`Poller::poll`].
///
/// It is a cheap [`Arc`]-backed handle to `mio`'s portable wakeup (eventfd/pipe), created
/// by (and owned by) the [`Poller`]; the woken event carries [`WAKER_KEY`]. Obtained from
/// [`Poller::waker`], it may be cloned and used any number of times from any thread;
/// `Waker::wake` is safe to call from any thread.
pub struct Waker(Arc<mio::Waker>);

impl Waker {
    /// Wakes the associated [`Poller`], interrupting a blocking [`Poller::poll`].
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the underlying wakeup fails.
    pub fn wake(&self) -> Result<()> {
        self.0.wake().map_err(crate::error::io)
    }
}

#[cfg(test)]
mod tests {
    use super::{to_mio_interest, Event, Interest, Poller, Ready, Token, WAKER_KEY};
    use beam_common::BeamError;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;

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
}
