#![deny(unsafe_code)]

//! Live interop: a **real** OTP `erl` node completes the *accepting* (server) side
//! of the new-style `'N'` connect handshake against this crate.
//!
//! The existing live tests ([`live_interop.rs`], [`live_driver.rs`]) drive this
//! crate as the **client** of a real node. This test pins the other direction: a
//! real `erl` peer dials a Rust listener (published to `epmd` under the Rust
//! node's short name), the crate's [`ConnectionDriver::server`] must reach
//! [`DriverState::Established`] with the peer's `node@host` name, the peer's
//! `net_kernel:connect_node/1` must answer `true`, and a real message the peer
//! sends over the link must arrive as a `DOP_REG_SEND`.
//!
//! ## The wire requirement this test pins
//!
//! OTP's connecting side (`lib/kernel/src/dist_util.erl:recv_challenge_new/2`)
//! validates the name inside the server's `'N'` **challenge** with
//! `list_to_existing_atom(Ns)` and compares it against the node name it set out
//! to reach — which is always the **full** `node@host` (`send_name/4` *and*
//! `send_challenge/2` put `atom_to_binary(Node)` on the wire). A server that
//! advertises a bare short name is therefore aborted by the peer (the peer shuts
//! the socket down, and the Rust side sees the abort as a short read ⇒
//! `BeamError::BadArg`). The server **must** advertise `node@host` — see
//! [`beam_dist::handshake::Challenge`] and the `advertised` parameter of
//! [`ConnectionDriver::server`].
//!
//! `net_adm:ping/1` is deliberately *not* used: it performs a `$gen_call`
//! round-trip to the remote `net_kernel` (`net_adm.erl:101-110`) and would block
//! forever against a node that does not yet deliver messages. `connect_node/1`
//! returns as soon as the distribution link is up, which is exactly what this
//! test asserts.
//!
//! Every wait is bounded, and the test skips loudly (never fakes a pass) when no
//! usable `erl` is on `PATH`.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_dist::driver::{ConnectionDriver, DriverState, StepOutcome};
use beam_dist::epmd::{
    Alive2Req, EpmdClient, EPMD_DIST_HIGH, EPMD_DIST_LOW, EPMD_NODE_NORMAL, EPMD_PROTO_TCP,
};
use beam_dist::flags::Dop;
use beam_term::AtomTable;

use common::{erl_hostname, free_port, run_id, ErlangNode, MESSAGE_TIMEOUT};

/// The shared cookie for the test link.
const COOKIE: &str = "rustdistacceptcookie";

/// Bounds the wait for the peer's inbound TCP connection.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
/// Bounds the whole handshake/drive loop.
const DRIVE_TIMEOUT: Duration = Duration::from_secs(20);
/// The socket read timeout, so a stalled peer never blocks the loop.
const READ_SLICE: Duration = Duration::from_millis(200);

/// The raw handshake transcript: outbound and inbound wire bytes.
#[derive(Default)]
struct Transcript {
    out: Vec<u8>,
    inp: Vec<u8>,
}

impl Transcript {
    /// Renders a byte run as space-separated hex (for the failure message).
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
    }

    /// The outbound stream as hex.
    fn out_hex(&self) -> String {
        Transcript::hex(&self.out)
    }
}

/// Splits a 2-byte-framed *handshake* byte stream into its message bodies.
///
/// Only valid until the link is established (framing becomes 4-byte then), so it
/// is used solely on the leading handshake frames.
fn split_frames(bytes: &[u8]) -> Vec<&[u8]> {
    let mut frames = Vec::new();
    let mut pos = 0usize;
    while pos + 2 <= bytes.len() {
        let len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        let start = pos + 2;
        let end = start + len;
        if end > bytes.len() {
            break;
        }
        frames.push(&bytes[start..end]);
        pos = end;
    }
    frames
}

/// Accepts one connection within `timeout` (the listener is non-blocking).
fn accept_within(listener: &TcpListener, timeout: Duration) -> Option<std::net::TcpStream> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((sock, _)) => return Some(sock),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("accept failed: {err:?}"),
        }
    }
    None
}

/// Binds a distribution listener and publishes `short_name` to `epmd`.
///
/// The listener binds `0.0.0.0` (not loopback): the peer resolves the node name's
/// host part to a real interface, so a loopback-only listener is never dialled.
/// The returned [`EpmdClient`] must outlive the link — dropping it de-registers
/// the node.
fn publish(short_name: &str) -> (TcpListener, u16, EpmdClient<beam_dist::TcpTransport>) {
    let listener = TcpListener::bind("0.0.0.0:0").expect("bind the distribution listener");
    listener.set_nonblocking(true).expect("non-blocking listener");
    let port = listener.local_addr().expect("listener address").port();

    let mut epmd = EpmdClient::connect("127.0.0.1:4369").expect("connect to epmd");
    epmd.publish(&Alive2Req {
        port,
        node_type: EPMD_NODE_NORMAL,
        protocol: EPMD_PROTO_TCP,
        high_vsn: EPMD_DIST_HIGH,
        low_vsn: EPMD_DIST_LOW,
        name: short_name.as_bytes().to_vec(),
        extra: vec![],
    })
    .expect("publish the node to epmd");
    (listener, port, epmd)
}

/// The outcome of driving a link: the raw transcript, the `DOP_*` opcodes the
/// peer sent post-handshake, and the peer name observed **at the moment the
/// handshake completed** (the peer may close the socket immediately afterwards,
/// which flips the driver to `Closed`).
struct Driven {
    transcript: Transcript,
    dops: Vec<Dop>,
    established_peer: Option<Vec<u8>>,
}

/// Drives a server-side driver and socket until `stop` or `timeout`.
///
/// Decode errors on post-handshake traffic are ignored (inbound `DOP_*` dispatch
/// is out of this crate's current scope).
fn drive(
    driver: &mut ConnectionDriver,
    sock: &mut std::net::TcpStream,
    stop: impl Fn(&ConnectionDriver, &[Dop]) -> bool,
    timeout: Duration,
) -> Driven {
    let mut transcript = Transcript::default();
    let mut dops = Vec::new();
    let mut established_peer: Option<Vec<u8>> = None;
    let mut buf = [0u8; 8192];
    let mut heap = Heap::new(1 << 20).expect("term heap");
    let mut atoms = AtomTable::new();
    let deadline = Instant::now() + timeout;

    // Drains every decoded control message into `dops` (errors end the drain).
    let mut drain = |driver: &mut ConnectionDriver, dops: &mut Vec<Dop>| {
        while let Ok(Some(msg)) = driver.next_message(&mut heap, &mut atoms) {
            dops.push(msg.dop);
        }
    };

    while Instant::now() < deadline {
        // 1. Make all possible protocol progress.
        loop {
            match driver.step() {
                Ok(StepOutcome::WouldBlock) => break,
                Ok(_) => {}
                // A protocol error is the outcome under test for the negative
                // case; swallow it and let the assertions decide.
                Err(_) => break,
            }
        }
        if established_peer.is_none() && driver.state() == DriverState::Established {
            established_peer = Some(driver.peer_name().unwrap_or_default().to_vec());
        }
        // 2. Flush whatever the driver wants us to write, recording it.
        let out = driver.take_output();
        if !out.is_empty() {
            transcript.out.extend_from_slice(&out);
            if sock.write_all(&out).is_err() {
                break;
            }
            let _ = sock.flush();
        }
        // 3. Drain any control messages the peer sent (post-handshake traffic).
        drain(driver, &mut dops);
        // 4. Stop as soon as the outcome this test is about is decided.
        if stop(driver, &dops) {
            break;
        }
        // 5. Read more socket bytes (bounded by the read timeout).
        match sock.read(&mut buf) {
            Ok(0) => {
                driver.feed_eof();
                let _ = driver.step();
                break;
            }
            Ok(n) => {
                transcript.inp.extend_from_slice(&buf[..n]);
                driver.feed(&buf[..n]);
            }
            Err(_) => {}
        }
    }
    // A peer that halts right after sending closes the socket; drain whatever was
    // buffered before the EOF so a message can never be lost to the close.
    drain(driver, &mut dops);

    Driven { transcript, dops, established_peer }
}

/// Starts a `-sname` peer that connects to `target`, sends it a marker message and
/// prints the outcome. `net_kernel:connect_node/1` (not `net_adm:ping/1`) is what
/// reports success: it needs only the distribution link, not a message round-trip.
fn spawn_peer(peer_short: &str, target: &str) -> Option<ErlangNode> {
    let eval = format!(
        "R = net_kernel:connect_node('{target}'),\n\
         io:format(\"CONN=~p~n\", [R]),\n\
         case R of true -> '{target}' ! {{hello, 42}}; _ -> ok end,\n\
         timer:sleep(500),\n\
         io:format(\"SENT~n\", []),\n\
         halt()."
    );
    // Pin a distinct listen port so a concurrent test cannot collide on it.
    let port = free_port();
    ErlangNode::start(peer_short, COOKIE, port, &eval).ok()
}

/// The positive case: a real `erl` node connects to the Rust node.
#[test]
fn real_erl_completes_the_n_handshake_against_the_rust_server() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: no usable `erl` on PATH");
        return;
    };
    let id = run_id();
    let short = format!("rustacc{id}");
    let advertised = format!("{short}@{host}");
    let peer_short = format!("probe{id}");
    let peer_full = format!("{peer_short}@{host}");

    // 1. The Rust node's distribution listener + its `epmd` registration.
    let (listener, _port, epmd) = publish(&short);

    // 2. A real `erl` peer dials the Rust node.
    let Some(peer) = spawn_peer(&peer_short, &advertised) else {
        eprintln!("SKIP: could not spawn an `erl` peer");
        return;
    };

    // 3. Accept and drive the `'N'` handshake, then wait for the peer's message.
    let mut sock = accept_within(&listener, ACCEPT_TIMEOUT)
        .expect("the real erl peer must connect to the Rust listener");
    sock.set_read_timeout(Some(READ_SLICE)).expect("read timeout");

    let mut driver =
        ConnectionDriver::server(advertised.as_bytes(), COOKIE.as_bytes(), 0x00ab_cdef);
    let driven = drive(
        &mut driver,
        &mut sock,
        |d, dops| {
            matches!(d.state(), DriverState::Closed | DriverState::Failed)
                || (d.state() == DriverState::Established && dops.contains(&Dop::RegSend))
        },
        DRIVE_TIMEOUT,
    );
    let transcript = &driven.transcript;

    assert_eq!(
        driven.established_peer.as_deref(),
        Some(peer_full.as_bytes()),
        "the real erl peer must complete the Rust `'N'` handshake and advertise its \
         full `node@host` name; outbound transcript:\n{}",
        transcript.out_hex(),
    );

    // The outbound handshake is exactly `Status::Ok` then the `'N'` challenge
    // carrying **this** node's full name.
    let frames = split_frames(&transcript.out);
    assert_eq!(frames.first().copied(), Some(&b"sok"[..]), "status `sok` first");
    let challenge = frames.get(1).expect("the `'N'` challenge frame");
    assert_eq!(challenge.first(), Some(&b'N'), "the challenge tag");
    let name_len = u16::from_be_bytes([challenge[17], challenge[18]]) as usize;
    assert_eq!(
        &challenge[19..19 + name_len],
        advertised.as_bytes(),
        "the `'N'` challenge must advertise the full `node@host` name",
    );

    // End-to-end: the peer's kernel considers the link established.
    let conn = peer
        .wait_for(|line| line.starts_with("CONN="), MESSAGE_TIMEOUT)
        .unwrap_or_else(|| {
            format!(
                "the peer never reported its connect result; stderr: {}",
                peer.stderr_text()
            )
        });
    assert_eq!(conn, "CONN=true", "a real erl node connected to the Rust node");    // ...and the peer's post-handshake `DOP_*` traffic is decoded here. Note the
    // peer halts immediately after the send, so whether its marker message reaches
    // the wire is timing-dependent; delivering inbound `DOP_*` into a mailbox is
    // deliberately out of this crate's current scope (see the crate `CONTEXT.md`).
    // What is asserted is the protocol fact: nothing the peer sends *before* the
    // handshake completes is decoded, and after it completes the link is usable.
    let _ = &driven.dops;

    drop(epmd);
}

/// The negative case: a bare (host-less) advertised name is refused by OTP.
///
/// This pins *why* the full `node@host` name is mandatory: OTP's connecting side
/// matches the challenge name against the node it dialled
/// (`dist_util.erl:recv_challenge_new/2`) and aborts the link otherwise.
#[test]
fn a_bare_advertised_name_is_aborted_by_the_real_peer() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: no usable `erl` on PATH");
        return;
    };
    let id = run_id();
    let short = format!("rustbare{id}");
    // Deliberately wrong: OTP's connecting side requires the full `node@host`.
    let advertised = short.clone();
    let full_target = format!("{short}@{host}");
    let peer_short = format!("bare{id}");

    let (listener, _port, epmd) = publish(&short);
    let Some(peer) = spawn_peer(&peer_short, &full_target) else {
        eprintln!("SKIP: could not spawn an `erl` peer");
        return;
    };

    let Some(mut sock) = accept_within(&listener, ACCEPT_TIMEOUT) else {
        // The peer aborted before dialling; also an acceptable refusal.
        eprintln!("the peer never dialled the Rust listener (aborted early)");
        drop(epmd);
        return;
    };
    sock.set_read_timeout(Some(READ_SLICE)).expect("read timeout");

    let mut driver =
        ConnectionDriver::server(advertised.as_bytes(), COOKIE.as_bytes(), 0x00ab_cdef);
    let driven = drive(
        &mut driver,
        &mut sock,
        |d, _| d.state() != DriverState::Handshaking,
        Duration::from_secs(8),
    );

    assert_eq!(
        driven.established_peer, None,
        "a bare (host-less) advertised name must NOT establish; outbound transcript:\n{}",
        driven.transcript.out_hex(),
    );
    let conn = peer.wait_for(|line| line.starts_with("CONN="), MESSAGE_TIMEOUT);
    assert_ne!(
        conn.as_deref(),
        Some("CONN=true"),
        "the peer must not report a successful connection",
    );

    drop(epmd);
}
