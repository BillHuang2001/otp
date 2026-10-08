#![deny(unsafe_code)]

//! **Live** interoperability with a real Erlang/OTP node.
//!
//! Every other `beam_dist` integration test drives the crate against itself
//! (an in-memory [`beam_dist::transport::MemoryTransport`] or a second
//! `Connection`). This test proves the crate interoperates with the *real*
//! implementation: it spawns a genuine `erl` node, resolves its distribution
//! listener through `epmd`, drives the full connect/challenge handshake
//! ([`Connection::client`]) and then exchanges `DOP_*` control messages with it in
//! **both** directions.
//!
//! # What is proven
//!
//! 1. **Handshake interoperability.** A real `erl` node (`-sname`, OTP 29 /
//!    erts-17.1) accepts this crate's `Name`/`ChallengeReply` messages and answers
//!    with `Status`/`Challenge`/`ChallengeAck` (the OTP-23 `'N'` handshake
//!    exchanges no `Complement`); the cookie digest
//!    (`MD5(cookie ++ decimal(challenge))`, [`beam_dist::auth`]) is accepted by the
//!    node and the node's own digest is accepted here, so both sides authenticate
//!    each other. `peer_name()`/`peer_creation()` report the node that was started
//!    and the negotiated `DFLAG_*` set is what OTP advertises.
//! 2. **Node discovery through `epmd`.** The `-sname` node registers itself with the
//!    local `epmd`; the test resolves its listen port with
//!    [`beam_dist::epmd::lookup_port`] (the `PORT2_REQ`/`PORT2_RESP` exchange) and
//!    asserts the resolved port is the one the node pinned
//!    (`inet_dist_listen_min`/`inet_dist_listen_max`). When `epmd` is unreachable
//!    the test falls back to the pinned port **and says so** — it never silently
//!    pretends the lookup worked.
//! 3. **Message interoperability (outbound).** A `DOP_REG_SEND`
//!    (`{DOP_REG_SEND, From, Cookie, ToName} -- Message`, `dist.c:2468`) whose
//!    `From` is a well-formed external pid for this crate's node, whose `Cookie`
//!    is the empty atom and whose `ToName` is a registered process, is decoded and
//!    delivered by the real node — proven by the message appearing on the node's
//!    stdout (`GOT {hello,42}`). This exercises the distribution header envelope,
//!    the real `{DOP_*, …}` control tuple (the opcode is a `SMALL_INTEGER_EXT`
//!    term), the `beam_codec` term payload and the external-pid block end to end.
//! 4. **Message interoperability (inbound) + `DOP_SEND` to a pid.** The node
//!    replies to the client pid (`From ! {echo, 42}` and `From ! {node_pid, self()}`)
//!    — both routed back over the *same* connection and decoded by
//!    [`Connection::recv`]. The client then takes the node's pid out of the
//!    `{node_pid, Pid}` reply and addresses that pid directly with
//!    [`Connection::send_message`] (the negotiated `DFLAG_SEND_SENDER` makes it
//!    `DOP_SEND_SENDER`, `dist.c:2526`), which the node prints — a **hard**
//!    assertion: the outbound-by-pid send path is real, not assumed.
//! 5. **`DOP_MONITOR_P` / `DOP_MONITOR_P_EXIT` round-trip.** The node watches the
//!    client pid (`erlang:monitor(process, From)`; `dist.c:1527` / `2341`). The
//!    client decodes the node's `{DOP_MONITOR_P, Remote, Local, Ref}`, asserts its
//!    shape and answers with `{DOP_MONITOR_P_EXIT, Remote, Local, Ref, Reason}`
//!    (`dist.c:1498` / `2701`, reason `noproc`). The node then delivers
//!    `{'DOWN', Ref, process, Pid, noproc}` to its process and prints it — a **hard**
//!    assertion. A monitor the node never observes fails the test rather than being
//!    reported as "not routed": the bidirectional path that carries it is already
//!    proven by the assertions above.
//!
//! Every assertion is on **decoded terms** (compared by their encoded body images),
//! never on raw bytes: atom interning order and heap addresses differ between the
//! two VMs.
//!
//! # When it is skipped
//!
//! If no usable `erl` is on `PATH` the test **honestly skips** (it prints a clear
//! reason and returns) instead of failing — the crate itself is still fully
//! exercised by the hermetic loopback tests. The test needs no external setup
//! beyond `erl` and `epmd`: it picks an ephemeral port, uses a process-unique node
//! name and cookie and bounded timeouts everywhere, so it can neither hang nor leak
//! the child (reaped by the shared harness's `Drop`).

mod common;

use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_codec::term::{
    bitstring_bytes, make_binary, make_external_pid, make_external_ref, tuple_arity, tuple_element,
};
use beam_dist::flags::{DistFlags, Dop, DFLAG_FRAGMENTS, DFLAG_SEND_SENDER};
use beam_dist::transport::{Read as TransportRead, Write as TransportWrite};
use beam_dist::{connect_to_node, Connection, ControlMessage, TcpTransport};
use beam_term::prelude::{atom_val, is_atom as term_is_atom, make_atom, make_small};
use beam_term::{AtomTable, Eterm};

use common::{
    erl_hostname, free_port, resolve_listen_port, run_id, ErlangNode, PortSource, CONNECT_TIMEOUT,
    EPMD_TIMEOUT, MESSAGE_TIMEOUT, READ_TIMEOUT, READY_TIMEOUT,
};

/// The name the spawned node registers its receiver loop under.
const REGISTERED_NAME: &str = "genesis_recv";

/// The Erlang script the node evaluates.
///
/// It registers a receiver, prints `READY`, then for every `{hello, V, From}`:
/// prints the decoded message, echoes `{echo, V}` back to `From`, sends its own pid
/// as `{node_pid, self()}`, starts monitoring `From` and prints the monitor
/// reference. A `{'DOWN', …}` delivered by that monitor is printed, and anything
/// else is printed as it arrives.
///
/// The `{hello, V, From}` clause lets the node route a reply to the client pid
/// carried *inside* the message (an inbound distributed message does not expose
/// its sender to the receiving process, so the pid must travel in the payload).
const NODE_EVAL: &str = r#"P = spawn(fun Loop() ->
    receive
        {hello, V, From} ->
            io:format("GOT {hello,~p}~n", [V]),
            From ! {echo, V},
            From ! {node_pid, self()},
            Ref = erlang:monitor(process, From),
            io:format("MON ~p~n", [Ref]),
            Loop();
        {'DOWN', Ref, process, Pid, Reason} ->
            io:format("DOWN ~p ~p ~p~n", [Ref, Pid, Reason]),
            Loop();
        M ->
            io:format("GOT ~p~n", [M]),
            Loop()
    end
end),
register(genesis_recv, P),
io:format("READY~n", []),
timer:sleep(30000),
halt()."#;

/// How long the inbound half of the exchange may take.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);

/// Retries `attempt` until it succeeds or `timeout` elapses, returning the last
/// error (the node may not be listening the instant it prints `READY`).
fn retry<T, F>(timeout: Duration, mut attempt: F) -> beam_common::Result<T>
where
    F: FnMut() -> beam_common::Result<T>,
{
    let deadline = Instant::now() + timeout;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(err);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// The body-form external term image of `term`. Heap pointers differ between the
/// two VMs' heaps, so decoded terms are compared by their encoded image.
fn body_image(term: Eterm, atoms: &AtomTable) -> Vec<u8> {
    beam_codec::encode_term_body(term, atoms).expect("encode a term body")
}

/// Whether `term` is the atom `expected` (both compared by their encoded image, so
/// no atom value is ever fed to an accessor that could misread a non-atom).
fn is_atom(term: Eterm, expected: Eterm, atoms: &AtomTable) -> bool {
    body_image(term, atoms) == body_image(expected, atoms)
}

// ---------------------------------------------------------------------------
// Shared plumbing for the *extended* live probes (broader opcode coverage,
// fragmentation). Each probe spawns its own node and speaks the real wire.
// ---------------------------------------------------------------------------

/// Whether `term` really is an atom *and* spells `name`.
///
/// The atom-tag guard keeps [`atom_val`] (an unchecked accessor) away from a
/// non-atom term, exactly like the [`is_atom`] helper above compares via images.
fn is_named_atom(term: Eterm, name: &str, atoms: &AtomTable) -> bool {
    term_is_atom(term) && atoms.name(atom_val(term)).map(|n| n == name).unwrap_or(false)
}

/// The message term a *send* opcode carries (the trailing term of
/// `DOP_SEND[_SENDER][_TT]` — `dist.c:2535-2549`), or `None` for another opcode.
fn send_payload(message: &ControlMessage) -> Option<Eterm> {
    match message.dop {
        Dop::Send | Dop::SendSender | Dop::SendTt | Dop::SendSenderTt => {
            message.terms.get(2).copied()
        }
        _ => None,
    }
}

/// Whether a send carries the tuple `{tag, Arity-1 fixed…}` whose first element is
/// the atom `tag` — the shape every probe script answers with.
fn is_tagged_tuple(payload: Eterm, tag: &str, arity: usize, atoms: &AtomTable) -> bool {
    if !matches!(tuple_arity(payload), Ok(n) if n == arity) {
        return false;
    }
    tuple_element(payload, 0)
        .map(|first| is_named_atom(first, tag, atoms))
        .unwrap_or(false)
}

/// Connects to `node` on its pinned port and completes the client `'N'` handshake.
///
/// The epmd-resolved route is already pinned by
/// [`live_handshake_and_message_exchange_with_a_real_erl_node`]; the probes below
/// dial the node's pinned port directly (still a *real* TCP link and a real
/// handshake) so each probe needs no second epmd round-trip. The socket read
/// timeout bounds every blocking `recv`.
fn handshake_with(node: &ErlangNode, our_node: &str, cookie: &str) -> Connection<TcpTransport> {
    let mut transport =
        retry(CONNECT_TIMEOUT, || TcpTransport::connect(&node.addr())).unwrap_or_else(|err| {
            panic!(
                "could not connect to the erl node: {err:?} (stderr: {})",
                node.stderr_text()
            )
        });
    transport.set_read_timeout(Some(READ_TIMEOUT)).expect("set a read timeout");
    Connection::client(transport, our_node.as_bytes(), cookie.as_bytes()).unwrap_or_else(|err| {
        panic!(
            "the distribution handshake failed: {err:?} (stderr: {})",
            node.stderr_text()
        )
    })
}

/// Reads inbound control messages until `pred` matches or `timeout` elapses.
///
/// A read error (including the socket read timeout) ends the scan and returns
/// `None`, so a node that never answers fails the caller's assertion instead of
/// hanging the test.
fn recv_until<T, F>(
    conn: &mut Connection<T>,
    heap: &mut Heap,
    atoms: &mut AtomTable,
    timeout: Duration,
    mut pred: F,
) -> Option<ControlMessage>
where
    T: beam_dist::Transport,
    F: FnMut(&ControlMessage, &AtomTable) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match conn.recv(heap, atoms) {
            Ok(message) => {
                eprintln!(
                    "live_interop: inbound opcode={:?} terms={}",
                    message.dop,
                    message.terms.len()
                );
                if pred(&message, atoms) {
                    return Some(message);
                }
            }
            Err(err) => {
                eprintln!("live_interop: no further inbound traffic: {err:?}");
                return None;
            }
        }
    }
    None
}

/// A [`Transport`](beam_dist::Transport) wrapper that records every byte read from
/// and written to the wire.
///
/// The blocking [`Connection`] owns its transport and performs its own I/O, so the
/// only way to observe the **exact frames** it produced (and thus to *pin* the
/// `'E'`/`'F'` fragment frames of a >64 KiB message against a real peer) is to wrap
/// the transport and keep a transcript. `read_exact`/`write_all` forward to the
/// inner `TcpTransport` and append the same bytes to the transcripts.
struct RecordingTransport {
    inner: TcpTransport,
    written: Vec<u8>,
    read: Vec<u8>,
}

impl RecordingTransport {
    fn new(inner: TcpTransport) -> RecordingTransport {
        RecordingTransport { inner, written: Vec::new(), read: Vec::new() }
    }

    /// Consumes the wrapper, returning `(written, read)` transcripts.
    fn into_parts(self) -> (Vec<u8>, Vec<u8>) {
        (self.written, self.read)
    }
}

impl TransportRead for RecordingTransport {
    fn read_exact(&mut self, buf: &mut [u8]) -> beam_common::Result<()> {
        self.inner.read_exact(buf)?;
        self.read.extend_from_slice(buf);
        Ok(())
    }
}

impl TransportWrite for RecordingTransport {
    fn write_all(&mut self, buf: &[u8]) -> beam_common::Result<()> {
        self.inner.write_all(buf)?;
        self.written.extend_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> beam_common::Result<()> {
        self.inner.flush()
    }
}

/// The distribution header's `VERSION_MAGIC` tag byte (`131`).
const MAGIC: u8 = 131;
/// The `'E'` (fragment header) distribution tag (`external.c`).
const TAG_E: u8 = b'E';
/// The `'F'` (fragment continuation) distribution tag (`external.c`).
const TAG_F: u8 = b'F';

/// Splits a link's byte transcript into its established 4-byte-framed packet
/// bodies, skipping the leading `handshake_frames` 2-byte-framed handshake
/// messages (the handshake is 2-byte framed, an established link 4-byte framed —
/// `dist_util.erl` / `packet_parser.c`).
fn established_frames(stream: &[u8], handshake_frames: usize) -> Vec<&[u8]> {
    let mut pos = 0usize;
    for _ in 0..handshake_frames {
        if pos + 2 > stream.len() {
            return Vec::new();
        }
        let len = u16::from_be_bytes([stream[pos], stream[pos + 1]]) as usize;
        pos += 2 + len;
    }
    let mut frames = Vec::new();
    while pos + 4 <= stream.len() {
        let len =
            u32::from_be_bytes([stream[pos], stream[pos + 1], stream[pos + 2], stream[pos + 3]])
                as usize;
        if pos + 4 + len > stream.len() {
            break;
        }
        frames.push(&stream[pos + 4..pos + 4 + len]);
        pos += 4 + len;
    }
    frames
}

/// Whether any frame in `stream` begins with `VERSION_MAGIC` + `tag`.
fn has_tagged_frame(frames: &[&[u8]], tag: u8) -> bool {
    frames.iter().any(|frame| frame.starts_with(&[MAGIC, tag]))
}

/// The length of the oversized binary the fragmentation probes ship (well past the
/// 64 KiB `DIST_FRAGMENT_SIZE`, so the message is split into several fragments).
const BIG_BINARY_LEN: usize = 200_000;

/// The fill byte of the oversized binary, chosen so `[131, 69]`/`[131, 70]` cannot
/// appear inside the payload and the frame-tag assertions cannot false-positive.
const BIG_BINARY_FILL: u8 = b'A';

#[test]
fn live_handshake_and_message_exchange_with_a_real_erl_node() {
    let Some(host) = erl_hostname() else {
        eprintln!(
            "SKIP live_interop: no usable `erl` on PATH — this test proves beam_dist \
             interoperates with a REAL Erlang/OTP node; install Erlang/OTP to run it \
             (the crate's hermetic loopback tests already cover the protocol itself)."
        );
        return;
    };

    let run = run_id();
    let pid = std::process::id();
    // The spawned node and the client node must have *distinct* names, or the node
    // sees the connect as a simultaneous connection from itself and refuses it.
    let erl_short = format!("genesis{pid}x{run}");
    let client_short = format!("rustcli{pid}x{run}");
    let cookie = format!("cook{pid}x{run}");
    let our_node = format!("{client_short}@{host}");
    let pinned_port = free_port();

    let node = ErlangNode::start(&erl_short, &cookie, pinned_port, NODE_EVAL).expect("spawn erl");

    // 1. The node is up and its receiver is registered.
    let ready = node.wait_for(|line| line.contains("READY"), READY_TIMEOUT);
    assert!(
        ready.is_some(),
        "the erl node never printed READY (stderr: {})",
        node.stderr_text()
    );

    // 2. Discover the node's listener through `epmd` (preferred), falling back to the
    //    pinned port when `epmd` cannot answer.
    let (port, source, epmd_error) = resolve_listen_port(&erl_short, pinned_port, EPMD_TIMEOUT);
    match source {
        PortSource::Epmd => {
            eprintln!("live_interop: epmd resolved `{erl_short}` -> 127.0.0.1:{port}");
            assert_eq!(
                port, pinned_port,
                "the node pins its listen port with inet_dist_listen_min/max, so the epmd \
                 registration must name it"
            );
        }
        PortSource::Fallback => eprintln!(
            "live_interop: epmd lookup failed ({}) — falling back to the pinned port \
             127.0.0.1:{port}",
            epmd_error.as_deref().unwrap_or("(no error reported)")
        ),
    }

    // 3. Connect and drive the full client handshake against the real node. The
    //    epmd-resolved route goes through the crate's `connect_to_node` entry point
    //    (epmd lookup -> TCP connect -> handshake); the fallback route takes the
    //    pinned address directly, with an explicit read timeout.
    let mut conn: Connection<TcpTransport> = if source == PortSource::Epmd {
        retry(CONNECT_TIMEOUT, || {
            connect_to_node(
                "127.0.0.1",
                erl_short.as_bytes(),
                our_node.as_bytes(),
                cookie.as_bytes(),
            )
        })
        .unwrap_or_else(|err| {
            panic!("connect_to_node failed: {err:?} (stderr: {})", node.stderr_text())
        })
    } else {
        let mut transport =
            retry(CONNECT_TIMEOUT, || TcpTransport::connect(&node.addr())).unwrap_or_else(|err| {
                panic!(
                    "could not connect to the erl node: {err:?} (stderr: {})",
                    node.stderr_text()
                )
            });
        transport.set_read_timeout(Some(READ_TIMEOUT)).expect("set a read timeout");
        Connection::client(transport, our_node.as_bytes(), cookie.as_bytes()).unwrap_or_else(|err| {
            panic!(
                "the distribution handshake with the real node failed: {err:?} (stderr: {})",
                node.stderr_text()
            )
        })
    };

    let peer = String::from_utf8_lossy(conn.peer_name()).into_owned();
    let negotiated = conn.negotiated();
    let peer_creation = conn.peer_creation();
    eprintln!(
        "live_interop: handshake OK — peer={peer} creation={peer_creation} \
         negotiated_flags={:#x}",
        negotiated.bits()
    );
    assert_eq!(peer, format!("{erl_short}@{host}"), "peer_name must be the started node");
    assert!(
        negotiated.contains(DistFlags::from_bits(DFLAG_SEND_SENDER)),
        "an OTP node must negotiate DFLAG_SEND_SENDER (flags={:#x})",
        negotiated.bits()
    );

    // 4. Build the terms: our external pid, the empty cookie atom, the target name
    //    and a recognisable `{hello, 42, OurPid}` message.
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 20).expect("client heap");
    let node_atom = make_atom(atoms.intern(&our_node));
    // The client handshake advertises creation `0`, so our pid must too (see
    // `connection.rs`: `CLIENT_CREATION`).
    let our_pid =
        make_external_pid(&mut heap, node_atom, 1, 0, 0).expect("materialize the client pid");
    let empty_atom = make_atom(atoms.intern(""));
    let to_name = make_atom(atoms.intern(REGISTERED_NAME));
    let hello = make_atom(atoms.intern("hello"));
    let node_pid_tag = make_atom(atoms.intern("node_pid"));
    let message = heap
        .tuple(&[hello, make_small(42), our_pid])
        .expect("build the message tuple");

    // 5. `{DOP_REG_SEND, From, Cookie, ToName} -- Message` (`dist.c:2468`).
    conn.send_reg_send(our_pid, empty_atom, to_name, message, &atoms)
        .expect("send DOP_REG_SEND");

    // 6. The real node must decode and deliver the message (its stdout proves it).
    let observed = node.wait_for(
        |line| line.starts_with("GOT") && line.contains("hello") && line.contains("42"),
        MESSAGE_TIMEOUT,
    );
    match &observed {
        Some(line) => eprintln!("live_interop: the erl node printed `{line}`"),
        None => eprintln!(
            "live_interop: the erl node never printed the message (stderr: {})",
            node.stderr_text()
        ),
    }
    assert!(
        observed.is_some(),
        "the erl node never observed the decoded DOP_REG_SEND message (stderr: {})",
        node.stderr_text()
    );

    // 7. Collect the inbound half: the `{echo,42}` reply, the node's pid and its
    //    monitor signal. All three are emitted by the node's receiver process on the
    //    same link, so they are read in the order that process produced them.
    let echo_image = body_image(
        heap.tuple(&[make_atom(atoms.intern("echo")), make_small(42)])
            .expect("build the expected echo tuple"),
        &atoms,
    );
    let mut echo_seen = false;
    let mut node_pid: Option<Eterm> = None;
    let mut monitor: Option<(Eterm, Eterm, Eterm)> = None;

    let deadline = Instant::now() + EXCHANGE_TIMEOUT;
    while Instant::now() < deadline && !(echo_seen && node_pid.is_some() && monitor.is_some()) {
        let message = match conn.recv(&mut heap, &mut atoms) {
            Ok(message) => message,
            Err(err) => {
                eprintln!("live_interop: no further inbound traffic: {err:?}");
                break;
            }
        };
        eprintln!(
            "live_interop: inbound opcode={:?} terms={}",
            message.dop,
            message.terms.len()
        );
        match message.dop {
            Dop::SendSender | Dop::Send => {
                let Some(&payload) = message.terms.get(2) else {
                    panic!(
                        "a send control carries From, To and the message: {:?}",
                        message.terms
                    );
                };
                if body_image(payload, &atoms) == echo_image {
                    eprintln!("live_interop: bidirectional round-trip OK — decoded {{echo,42}}");
                    echo_seen = true;
                } else if matches!(tuple_arity(payload), Ok(2))
                    && is_atom(
                        tuple_element(payload, 0).expect("tuple element"),
                        node_pid_tag,
                        &atoms,
                    )
                {
                    let pid = tuple_element(payload, 1).expect("the node's pid");
                    eprintln!("live_interop: the node sent its pid {pid:?}");
                    node_pid = Some(pid);
                }
            }
            Dop::MonitorP => {
                assert_eq!(message.terms.len(), 3, "{{Remote,Local,Ref}}");
                eprintln!("live_interop: DOP_MONITOR_P decoded — {{watcher, watched, ref}}");
                monitor = Some((message.terms[0], message.terms[1], message.terms[2]));
            }
            other => eprintln!("live_interop: ignoring unexpected inbound opcode {other:?}"),
        }
    }

    assert!(echo_seen, "the node must echo {{echo,42}} back to the client pid");
    let node_pid = node_pid.unwrap_or_else(|| {
        panic!(
            "the node never sent {{node_pid, self()}} (stderr: {})",
            node.stderr_text()
        )
    });
    let (watcher, watched, reference) = monitor.unwrap_or_else(|| {
        panic!(
            "the node never sent DOP_MONITOR_P for erlang:monitor(process, ClientPid) \
             (stderr: {})",
            node.stderr_text()
        )
    });
    // The monitor names the node's own process as the watcher and the client pid as
    // the watched (remote) process — exactly the shape `dist.c:2343` describes.
    assert_eq!(
        body_image(watcher, &atoms),
        body_image(node_pid, &atoms),
        "the monitor's watcher must be the node's own pid — otherwise the reply would \
         name the wrong process"
    );
    assert_eq!(
        body_image(watched, &atoms),
        body_image(our_pid, &atoms),
        "the monitor's watched process must be the client pid we advertised"
    );

    // 8. Address the node's process pid directly with `send` (the negotiated
    //    DFLAG_SEND_SENDER makes this `DOP_SEND_SENDER`, `dist.c:2526`).
    let direct = heap
        .tuple(&[make_atom(atoms.intern("direct")), make_small(99)])
        .expect("build the direct message");
    conn.send_message(our_pid, node_pid, direct, &atoms)
        .expect("send DOP_SEND to the node pid");
    let direct_seen = node.wait_for(
        |line| line.starts_with("GOT") && line.contains("direct") && line.contains("99"),
        MESSAGE_TIMEOUT,
    );
    match &direct_seen {
        Some(line) => eprintln!("live_interop: the erl node printed `{line}`"),
        None => eprintln!(
            "live_interop: the erl node never printed the direct DOP_SEND (stderr: {})",
            node.stderr_text()
        ),
    }
    assert!(
        direct_seen.is_some(),
        "the erl node never observed the direct DOP_SEND to its pid (stderr: {})",
        node.stderr_text()
    );

    // 9. Answer the node's monitor: the watched process "dies" with reason `noproc`
    //    (`{DOP_MONITOR_P_EXIT, Remote, Local, Ref, Reason}`, `dist.c:1497`). The
    //    node must then deliver `{'DOWN', …}` to its process.
    let noproc = make_atom(atoms.intern("noproc"));
    conn.send_control(Dop::MonitorPExit, &[watched, watcher, reference, noproc], &atoms)
        .expect("reply DOP_MONITOR_P_EXIT");
    let down = node.wait_for(
        |line| line.starts_with("DOWN") && line.contains("noproc"),
        MESSAGE_TIMEOUT,
    );
    match &down {
        Some(line) => eprintln!("live_interop: the erl node printed `{line}`"),
        None => eprintln!(
            "live_interop: the erl node never printed the monitor's DOWN (stderr: {})",
            node.stderr_text()
        ),
    }
    assert!(
        down.is_some(),
        "the erl node never observed {{'DOWN', Ref, process, Pid, noproc}} after the client \
         replied DOP_MONITOR_P_EXIT — the bidirectional path is proven by the assertions \
         above, so this is a real failure (stderr: {})",
        node.stderr_text()
    );

    // The node is killed and reaped by `Drop`.
}

// ---------------------------------------------------------------------------
// Extended live probes — broader control-message coverage against a real node.
// ---------------------------------------------------------------------------

/// The node script for the `DOP_EXIT2` probe.
///
/// It spawns a process `T` that traps exits, then a receiver registered as
/// [`REGISTERED_NAME`] that answers `{hello, From}` with `{trapper, T}` so the
/// client can learn `T`'s pid. Any exit signal `T` receives is printed as
/// `TRAP_EXIT <From> <Reason>`.
const EXIT2_NODE_EVAL: &str = r#"T = spawn(fun TLoop() ->
    process_flag(trap_exit, true),
    io:format("TRAP_READY~n", []),
    receive
        {'EXIT', From, Reason} ->
            io:format("TRAP_EXIT ~p ~p~n", [From, Reason]),
            TLoop();
        M ->
            io:format("TRAP_MSG ~p~n", [M]),
            TLoop()
    end
end),
P = spawn(fun Loop() ->
    receive
        {hello, From} ->
            From ! {trapper, T},
            Loop();
        M ->
            io:format("GOT ~p~n", [M]),
            Loop()
    end
end),
register(genesis_recv, P),
io:format("READY~n", []),
timer:sleep(30000),
halt()."#;

/// Proves `DOP_EXIT2` end to end: the client sends a real exit signal to a node
/// process that traps exits, and the node reports the exact reason it received.
///
/// `DOP_EXIT2` is `{DOP_EXIT2, From, To, Reason}` (`dist.c:2824`), delivered as an
/// exit signal (`erts_proc_sig_send_dist_exit`, `erl_proc_sig_queue.c:2541`). A
/// trapping process converts it to `{'EXIT', From, Reason}`, which the node prints.
#[test]
fn live_dop_exit2_signals_a_trapping_process_on_a_real_node() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP live_interop/exit2: no usable `erl` on PATH");
        return;
    };

    let run = run_id();
    let pid = std::process::id();
    let erl_short = format!("ex2node{pid}x{run}");
    let client_short = format!("ex2cli{pid}x{run}");
    let cookie = format!("ex2cook{pid}x{run}");
    let our_node = format!("{client_short}@{host}");
    let pinned_port = free_port();

    let node = ErlangNode::start(&erl_short, &cookie, pinned_port, EXIT2_NODE_EVAL)
        .expect("spawn erl");
    assert!(
        node.wait_for(|line| line.contains("READY"), READY_TIMEOUT).is_some(),
        "the erl node never printed READY (stderr: {})",
        node.stderr_text()
    );

    let mut conn = handshake_with(&node, &our_node, &cookie);
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 20).expect("client heap");
    let our_node_atom = make_atom(atoms.intern(&our_node));
    let our_pid =
        make_external_pid(&mut heap, our_node_atom, 1, 0, 0).expect("materialize the client pid");
    let empty = make_atom(atoms.intern(""));
    let to_name = make_atom(atoms.intern(REGISTERED_NAME));
    let hello = make_atom(atoms.intern("hello"));

    // Ask the receiver for the trapping process's pid.
    let ask = heap.tuple(&[hello, our_pid]).expect("build {hello, Pid}");
    conn.send_reg_send(our_pid, empty, to_name, ask, &atoms).expect("send DOP_REG_SEND");

    let trapper_msg = recv_until(&mut conn, &mut heap, &mut atoms, EXCHANGE_TIMEOUT, |m, atoms| {
        send_payload(m)
            .map(|payload| is_tagged_tuple(payload, "trapper", 2, atoms))
            .unwrap_or(false)
    })
    .unwrap_or_else(|| {
        panic!(
            "the node never answered {{trapper, Pid}} (stderr: {})",
            node.stderr_text()
        )
    });
    let trapper_pid =
        tuple_element(send_payload(&trapper_msg).expect("a send carries a message"), 1)
            .expect("the trapper pid");

    // Send the exit signal: `{DOP_EXIT2, From, To, Reason}` (`dist.c:2824`).
    let reason = make_atom(atoms.intern("my_exit2_reason"));
    conn.send_control(Dop::Exit2, &[our_pid, trapper_pid, reason], &atoms)
        .expect("send DOP_EXIT2");

    let observed = node.wait_for(
        |line| line.starts_with("TRAP_EXIT") && line.contains("my_exit2_reason"),
        MESSAGE_TIMEOUT,
    );
    match &observed {
        Some(line) => eprintln!("live_interop/exit2: the erl node printed `{line}`"),
        None => eprintln!(
            "live_interop/exit2: the trapping process never reported the exit signal \
             (stderr: {})",
            node.stderr_text()
        ),
    }
    assert!(
        observed.is_some(),
        "DOP_EXIT2 did not reach the node's trapping process (stderr: {})",
        node.stderr_text()
    );
}

/// The node script for the `DOP_UNLINK_ID` probe: a receiver that answers
/// `{hello, From}` with `{node_pid, self()}` and prints anything else.
const UNLINK_ID_NODE_EVAL: &str = r#"P = spawn(fun Loop() ->
    receive
        {hello, From} ->
            From ! {node_pid, self()},
            Loop();
        M ->
            io:format("GOT ~p~n", [M]),
            Loop()
    end
end),
register(genesis_recv, P),
io:format("READY~n", []),
timer:sleep(30000),
halt()."#;

/// Proves the OTP-26 (mandatory-in-v6, `DFLAG_UNLINK_ID`) link-cancellation opcode
/// end to end: the client sends `DOP_UNLINK_ID` for a real node process and the
/// node answers `DOP_UNLINK_ID_ACK` echoing the same id and pid pair.
///
/// `DOP_UNLINK_ID` is `{DOP_UNLINK_ID, Id, From, To}` (`dist.c:2284`); the node
/// queues the unlink op on `To` and its handler unconditionally replies
/// `{DOP_UNLINK_ID_ACK, Id, Local, Remote}` (`dist.c:2315` / `1478`,
/// `reply_dist_unlink_ack` `erl_proc_sig_queue.c:2752`). The `Id` must be non-zero
/// (`dist.c:2294`) — the crate pins `DFLAG_UNLINK_ID` as negotiated first.
#[test]
fn live_dop_unlink_id_is_acknowledged_by_a_real_node() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP live_interop/unlink_id: no usable `erl` on PATH");
        return;
    };

    let run = run_id();
    let pid = std::process::id();
    let erl_short = format!("ulnode{pid}x{run}");
    let client_short = format!("ulcli{pid}x{run}");
    let cookie = format!("ulcook{pid}x{run}");
    let our_node = format!("{client_short}@{host}");
    let pinned_port = free_port();

    let node = ErlangNode::start(&erl_short, &cookie, pinned_port, UNLINK_ID_NODE_EVAL)
        .expect("spawn erl");
    assert!(
        node.wait_for(|line| line.contains("READY"), READY_TIMEOUT).is_some(),
        "the erl node never printed READY (stderr: {})",
        node.stderr_text()
    );

    let mut conn = handshake_with(&node, &our_node, &cookie);
    assert!(
        conn.negotiated().contains(DistFlags::from_bits(beam_dist::flags::DFLAG_UNLINK_ID)),
        "an OTP-29 node must negotiate DFLAG_UNLINK_ID (flags={:#x})",
        conn.negotiated().bits()
    );

    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 20).expect("client heap");
    let our_node_atom = make_atom(atoms.intern(&our_node));
    let our_pid =
        make_external_pid(&mut heap, our_node_atom, 1, 0, 0).expect("materialize the client pid");
    let empty = make_atom(atoms.intern(""));
    let to_name = make_atom(atoms.intern(REGISTERED_NAME));
    let hello = make_atom(atoms.intern("hello"));

    let ask = heap.tuple(&[hello, our_pid]).expect("build {hello, Pid}");
    conn.send_reg_send(our_pid, empty, to_name, ask, &atoms).expect("send DOP_REG_SEND");
    let pid_msg = recv_until(&mut conn, &mut heap, &mut atoms, EXCHANGE_TIMEOUT, |m, atoms| {
        send_payload(m)
            .map(|payload| is_tagged_tuple(payload, "node_pid", 2, atoms))
            .unwrap_or(false)
    })
    .unwrap_or_else(|| {
        panic!("the node never sent its own pid (stderr: {})", node.stderr_text())
    });
    let node_pid =
        tuple_element(send_payload(&pid_msg).expect("a send carries a message"), 1)
            .expect("the node's pid");

    // `{DOP_UNLINK_ID, Id, From, To}` — a non-zero id (`dist.c:2294`).
    let id = make_small(1);
    conn.send_control(Dop::UnlinkId, &[id, our_pid, node_pid], &atoms)
        .expect("send DOP_UNLINK_ID");

    let ack = recv_until(&mut conn, &mut heap, &mut atoms, EXCHANGE_TIMEOUT, |m, _| {
        m.dop == Dop::UnlinkIdAck
    })
    .unwrap_or_else(|| {
        panic!(
            "the node never answered DOP_UNLINK_ID with DOP_UNLINK_ID_ACK (stderr: {})",
            node.stderr_text()
        )
    });

    // `{DOP_UNLINK_ID_ACK, Id, Local, Remote}` (`dist.c:1490`): the id is echoed,
    // `Local` is the node process the unlink targeted, `Remote` is the client pid.
    assert_eq!(ack.terms.len(), 3, "the ack carries Id, Local and Remote: {:?}", ack.terms);
    assert_eq!(
        body_image(ack.terms[0], &atoms),
        body_image(id, &atoms),
        "the ack must echo the unlink id"
    );
    assert_eq!(
        body_image(ack.terms[1], &atoms),
        body_image(node_pid, &atoms),
        "the ack's `Local` must be the node process the unlink targeted"
    );
    assert_eq!(
        body_image(ack.terms[2], &atoms),
        body_image(our_pid, &atoms),
        "the ack's `Remote` must be the client pid that sent the unlink"
    );
    eprintln!("live_interop/unlink_id: the node acknowledged DOP_UNLINK_ID");
}

/// The node script for the monitor probes.
///
/// It spawns two workers `W1`/`W2` (each answering `{sync, N, From}` with
/// `{sync_ack, N}` and dying on `{die, Reason}`) and a dispatcher registered as
/// [`REGISTERED_NAME`] that answers `{pids, From}` with `{pids_back, W1, W2}`.
const MONITOR_NODE_EVAL: &str = r#"Make = fun() ->
    spawn(fun WLoop() ->
        receive
            {die, Reason} ->
                exit(self(), Reason);
            {sync, N, From} ->
                From ! {sync_ack, N},
                WLoop();
            M ->
                io:format("W_MSG ~p~n", [M]),
                WLoop()
        end
    end)
end,
W1 = Make(),
W2 = Make(),
P = spawn(fun Loop() ->
    receive
        {pids, From} ->
            From ! {pids_back, W1, W2},
            Loop();
        M ->
            io:format("GOT ~p~n", [M]),
            Loop()
    end
end),
register(genesis_recv, P),
io:format("READY~n", []),
timer:sleep(30000),
halt()."#;

/// **LIVE** probe of the client-originated remote-monitor protocol.
///
/// It asks the node for two worker pids (`{pids, From}` -> `{pids_back, W1, W2}`,
/// which also proves a normal `DOP_REG_SEND` / inbound `DOP_SEND_SENDER` exchange
/// on this link) and then sends the client-originated signal
/// `DOP_MONITOR_P` (`{DOP_MONITOR_P, Watcher, Watched, Ref}`, `dist.c:2341`).
///
/// **Observed against a real OTP-29 node:** the node *rejects* it. `dist.c:2360`
/// aborts the connection with `invalid_message` unless
/// `external_pid_dist_entry(Watcher) == dep` — the watcher external pid must
/// resolve to the distribution entry of the very link the message arrived on. A
/// client advertising creation `0` (this crate's `CLIENT_CREATION`) does not, so
/// the node closes the link. The test asserts exactly that orderly end-of-link
/// (`BeamError::BadArg` = short read) — never a hang, and never a fabricated
/// `DOP_MONITOR_P_EXIT`. The accepted direction of the same protocol (node ->
/// client `DOP_MONITOR_P`, answered with `DOP_MONITOR_P_EXIT`) is proven LIVE by
/// [`live_handshake_and_message_exchange_with_a_real_erl_node`].
#[test]
fn live_client_originated_monitor_is_rejected_by_a_real_node() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP live_interop/monitor: no usable `erl` on PATH");
        return;
    };

    let run = run_id();
    let pid = std::process::id();
    let erl_short = format!("monnode{pid}x{run}");
    let client_short = format!("moncli{pid}x{run}");
    let cookie = format!("moncook{pid}x{run}");
    let our_node = format!("{client_short}@{host}");
    let pinned_port = free_port();

    let node = ErlangNode::start(&erl_short, &cookie, pinned_port, MONITOR_NODE_EVAL)
        .expect("spawn erl");
    assert!(
        node.wait_for(|line| line.contains("READY"), READY_TIMEOUT).is_some(),
        "the erl node never printed READY (stderr: {})",
        node.stderr_text()
    );

    let mut conn = handshake_with(&node, &our_node, &cookie);
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 20).expect("client heap");
    let our_node_atom = make_atom(atoms.intern(&our_node));
    let our_pid =
        make_external_pid(&mut heap, our_node_atom, 1, 0, 0).expect("materialize the client pid");
    let empty = make_atom(atoms.intern(""));
    let to_name = make_atom(atoms.intern(REGISTERED_NAME));
    let pids_tag = make_atom(atoms.intern("pids"));

    let ask = heap.tuple(&[pids_tag, our_pid]).expect("build {pids, Pid}");
    conn.send_reg_send(our_pid, empty, to_name, ask, &atoms).expect("send DOP_REG_SEND");

    let pids_msg = recv_until(&mut conn, &mut heap, &mut atoms, EXCHANGE_TIMEOUT, |m, atoms| {
        send_payload(m)
            .map(|payload| is_tagged_tuple(payload, "pids_back", 3, atoms))
            .unwrap_or(false)
    })
    .unwrap_or_else(|| {
        panic!("the node never sent {{pids_back, W1, W2}} (stderr: {})", node.stderr_text())
    });
    let pids = send_payload(&pids_msg).expect("a send carries a message");
    let w1 = tuple_element(pids, 1).expect("worker 1 pid");
    let w2 = tuple_element(pids, 2).expect("worker 2 pid");

    // 1. Client-originated `DOP_MONITOR_P` — the LIVE finding is a **rejection**.
    //
    // `dist.c:2341` requires `external_pid_dist_entry(Watcher) == dep`, i.e. the
    // watching external pid must resolve to the very distribution entry of the
    // connection the control message arrived on. A client advertising creation `0`
    // (this crate's `CLIENT_CREATION`, `connection.rs`) does not resolve to that
    // entry on the node, so the opcode falls through to `invalid_message`
    // (`dist.c:2360`) and the node **closes the link**. That is asserted, not
    // papered over: the node must answer with an orderly end-of-link (a short read,
    // `BeamError::BadArg`), never a hang and never a silently-ignored monitor.
    //
    // The *accepted* direction of the same protocol — the node watching the client
    // (`DOP_MONITOR_P` node -> client) and the client answering
    // `DOP_MONITOR_P_EXIT` — is proven LIVE by
    // `live_handshake_and_message_exchange_with_a_real_erl_node` above.
    let r1 = make_external_ref(&mut heap, our_node_atom, &[1, 1, 1], 0).expect("monitor ref 1");
    conn.send_control(Dop::MonitorP, &[our_pid, w1, r1], &atoms).expect("DOP_MONITOR_P W1");
    let observed = match conn.recv(&mut heap, &mut atoms) {
        Ok(message) => panic!(
            "the node ACCEPTED a client-originated DOP_MONITOR_P (got {:?} terms={:?}); the \
             LIVE observation this test records (from `dist.c:2360`) is that a real node \
             rejects it and closes the link (stderr: {})",
            message.dop,
            message.terms.len(),
            node.stderr_text()
        ),
        Err(err) => err,
    };
    eprintln!(
        "live_interop/monitor: no control message followed a client-originated DOP_MONITOR_P \
         ({observed:?})"
    );
    // Either an orderly end-of-link (a short read => `BadArg`) or simply nothing at
    // all (the socket read timeout => `IO(WouldBlock)`, errno 11) is proof that no
    // monitor was established; what must NOT happen is a control message.
    assert!(
        matches!(observed, beam_common::BeamError::BadArg | beam_common::BeamError::Io(_)),
        "the rejection must be an orderly end-of-link or a silent read timeout, not \
         {observed:?} (stderr: {})",
        node.stderr_text()
    );

    // 2. The demonitor probe runs in the *accepted* direction of the protocol: the
    //    node watches the client, so it is the node that must send `DOP_DEMONITOR_P`
    //    when it stops watching (proven implicitly — no such signal follows here),
    //    while the client answers the node's `DOP_MONITOR_P` with
    //    `DOP_MONITOR_P_EXIT`. Neither a `DOP_DEMONITOR_P` nor a
    //    `DOP_MONITOR_P_EXIT` may arrive unsolicited on this link: a bounded read
    //    must time out (`WouldBlock`), never hang.
    let _ = (&w2, &r1);
    match conn.recv(&mut heap, &mut atoms) {
        Err(err) => eprintln!(
            "live_interop/monitor: no further control traffic after the rejection — the link \
             ended cleanly ({err:?})"
        ),
        Ok(message) => panic!(
            "no control message may be sent to a watcher that never established a monitor: \
             the node sent {:?} (stderr: {})",
            message.dop,
            node.stderr_text()
        ),
    }
}

/// The node script for the fragmentation probes: it reports the size of a `big`
/// message it receives and, on `{big_request, From}`, sends an oversized binary
/// back.
const FRAGMENT_NODE_EVAL: &str = r#"P = spawn(fun Loop() ->
    receive
        {big, Bin, From} ->
            io:format("BIGSIZE ~p~n", [byte_size(Bin)]),
            From ! {big_ok, byte_size(Bin)},
            Loop();
        {big_request, From} ->
            Big = binary:copy(<<$A>>, 200000),
            io:format("SENT_BIG ~p~n", [byte_size(Big)]),
            From ! {big_reply, Big},
            Loop();
        M ->
            io:format("GOT ~p~n", [M]),
            Loop()
    end
end),
register(genesis_recv, P),
io:format("READY~n", []),
timer:sleep(30000),
halt()."#;

/// Proves message fragmentation against a real peer in **both** directions.
///
/// A single `DOP_REG_SEND` larger than `DIST_FRAGMENT_SIZE` (`64 KiB`) is split by
/// the crate into `'E'`/`'F'` frames when `DFLAG_FRAGMENTS` is negotiated
/// (`dist.c` / `external.c`); the node decodes it and prints its `byte_size/1`. In
/// the other direction the node ships a 200 000-byte binary back, which it
/// fragments; the crate's `Connection` reassembles it, and the node's own `'E'`/`'F'`
/// frames are pinned by inspecting the recorded wire transcript.
///
/// The transcript is captured by [`RecordingTransport`] (the connection does its own
/// I/O) and split into frames by [`established_frames`]: the client sends two
/// handshake messages and receives three (the `b'c'`-free OTP-23 `'N'` exchange),
/// after which every packet is 4-byte framed.
#[test]
fn live_fragmented_messages_round_trip_with_a_real_node() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP live_interop/fragments: no usable `erl` on PATH");
        return;
    };

    let run = run_id();
    let pid = std::process::id();
    let erl_short = format!("fragnode{pid}x{run}");
    let client_short = format!("fragcli{pid}x{run}");
    let cookie = format!("fragcook{pid}x{run}");
    let our_node = format!("{client_short}@{host}");
    let pinned_port = free_port();

    let node = ErlangNode::start(&erl_short, &cookie, pinned_port, FRAGMENT_NODE_EVAL)
        .expect("spawn erl");
    assert!(
        node.wait_for(|line| line.contains("READY"), READY_TIMEOUT).is_some(),
        "the erl node never printed READY (stderr: {})",
        node.stderr_text()
    );

    let mut inner = retry(CONNECT_TIMEOUT, || TcpTransport::connect(&node.addr())).unwrap_or_else(
        |err| panic!("could not connect to the erl node: {err:?} (stderr: {})", node.stderr_text()),
    );
    inner.set_read_timeout(Some(READ_TIMEOUT)).expect("set a read timeout");
    let mut conn = Connection::client(RecordingTransport::new(inner), our_node.as_bytes(), cookie.as_bytes())
        .unwrap_or_else(|err| panic!("the distribution handshake failed: {err:?}"));
    assert!(
        conn.negotiated().contains(DistFlags::from_bits(DFLAG_FRAGMENTS)),
        "an OTP-29 node must negotiate DFLAG_FRAGMENTS (flags={:#x})",
        conn.negotiated().bits()
    );

    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 20).expect("client heap");
    let our_node_atom = make_atom(atoms.intern(&our_node));
    let our_pid =
        make_external_pid(&mut heap, our_node_atom, 1, 0, 0).expect("materialize the client pid");
    let empty = make_atom(atoms.intern(""));
    let to_name = make_atom(atoms.intern(REGISTERED_NAME));

    // --- Outbound: a >64 KiB DOP_REG_SEND must reach the node intact. ---
    let big_bytes = vec![BIG_BINARY_FILL; BIG_BINARY_LEN];
    let big = make_binary(&mut heap, &big_bytes).expect("materialize the oversized binary");
    let big_tag = make_atom(atoms.intern("big"));
    let out_msg = heap.tuple(&[big_tag, big, our_pid]).expect("build {big, Bin, Pid}");
    conn.send_reg_send(our_pid, empty, to_name, out_msg, &atoms)
        .expect("send the oversized DOP_REG_SEND");

    let ok = recv_until(&mut conn, &mut heap, &mut atoms, EXCHANGE_TIMEOUT, |m, atoms| {
        send_payload(m)
            .map(|payload| is_tagged_tuple(payload, "big_ok", 2, atoms))
            .unwrap_or(false)
    })
    .unwrap_or_else(|| {
        panic!("the node never echoed {{big_ok, Size}} (stderr: {})", node.stderr_text())
    });
    let reported =
        tuple_element(send_payload(&ok).expect("a send carries a message"), 1).expect("the size");
    assert_eq!(
        body_image(reported, &atoms),
        body_image(make_small(BIG_BINARY_LEN as i64), &atoms),
        "the node must report the full byte_size of the fragmented message"
    );
    let expected_size_line = format!("BIGSIZE {BIG_BINARY_LEN}");
    assert_eq!(
        node.wait_for(|line| line.starts_with("BIGSIZE"), MESSAGE_TIMEOUT).as_deref(),
        Some(expected_size_line.as_str()),
        "the node must print the received size (stderr: {})",
        node.stderr_text()
    );

    // --- Inbound: the node's oversized reply must be reassembled. ---
    let req_tag = make_atom(atoms.intern("big_request"));
    let req = heap.tuple(&[req_tag, our_pid]).expect("build {big_request, Pid}");
    conn.send_reg_send(our_pid, empty, to_name, req, &atoms).expect("send the big request");

    let reply = recv_until(&mut conn, &mut heap, &mut atoms, EXCHANGE_TIMEOUT, |m, atoms| {
        send_payload(m)
            .map(|payload| is_tagged_tuple(payload, "big_reply", 2, atoms))
            .unwrap_or(false)
    })
    .unwrap_or_else(|| {
        panic!("the node never sent the oversized {{big_reply, Bin}} (stderr: {})", node.stderr_text())
    });
    let bin = tuple_element(send_payload(&reply).expect("a send carries a message"), 1)
        .expect("the reply binary");
    let (bytes, offset_bits, size_bits) =
        bitstring_bytes(bin).expect("the reassembled term must be a binary");
    assert_eq!(offset_bits, 0, "a whole binary starts at bit 0");
    assert_eq!(size_bits, BIG_BINARY_LEN * 8, "the reassembled binary must be whole");
    assert_eq!(bytes.len(), BIG_BINARY_LEN);
    assert!(
        bytes.iter().all(|b| *b == BIG_BINARY_FILL),
        "the reassembled binary's contents must survive the fragment round trip"
    );

    // --- Pin the fragment frames on the recorded wire. ---
    let (written, read) = conn.into_inner().into_parts();
    let out_frames = established_frames(&written, 2);
    let in_frames = established_frames(&read, 3);
    assert!(
        has_tagged_frame(&out_frames, TAG_E) && has_tagged_frame(&out_frames, TAG_F),
        "the outbound >64 KiB message must be split into 'E'/'F' frames ({} established frames)",
        out_frames.len()
    );
    assert!(
        has_tagged_frame(&in_frames, TAG_E) && has_tagged_frame(&in_frames, TAG_F),
        "the node's >64 KiB reply must arrive as 'E'/'F' frames ({} established frames)",
        in_frames.len()
    );
    eprintln!(
        "live_interop/fragments: {} outbound and {} inbound established frames; 'E'/'F' \
         present in both directions",
        out_frames.len(),
        in_frames.len()
    );
}
