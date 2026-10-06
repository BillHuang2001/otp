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
use beam_codec::term::{make_external_pid, tuple_arity, tuple_element};
use beam_dist::flags::{DistFlags, Dop, DFLAG_SEND_SENDER};
use beam_dist::{connect_to_node, Connection, TcpTransport};
use beam_term::prelude::{make_atom, make_small};
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
