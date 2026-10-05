#![deny(unsafe_code)]

//! **Live** interoperability with a real Erlang/OTP node.
//!
//! Every other `beam_dist` integration test drives the crate against itself
//! (an in-memory [`beam_dist::transport::MemoryTransport`] or a second
//! `Connection`). This test proves the crate interoperates with the *real*
//! implementation: it spawns a genuine `erl` node, connects to its distribution
//! listener over loopback TCP, drives the full connect/challenge handshake
//! ([`Connection::client`]) and then exchanges `DOP_*` control messages with it.
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
//! 2. **Message interoperability (outbound).** A `DOP_REG_SEND`
//!    (`{DOP_REG_SEND, From, Cookie, ToName} -- Message`, `dist.c:2468`) whose
//!    `From` is a well-formed external pid for this crate's node, whose `Cookie`
//!    is the empty atom and whose `ToName` is a registered process, is decoded and
//!    delivered by the real node — proven by the message appearing on the node's
//!    stdout (`GOT {hello,42}`). This exercises the distribution header envelope,
//!    the real `{DOP_*, …}` control tuple (the opcode is a `SMALL_INTEGER_EXT`
//!    term), the `beam_codec` term payload and the external-pid block end to end.
//! 3. **Message interoperability (inbound, best effort).** When the node replies
//!    to the client pid (`From ! {echo, 42}`) the reply is routed back over the
//!    *same* connection and decoded by [`Connection::recv`]. This depends on the
//!    real node accepting a client node creation of `0` and routing to an
//!    inbound-only (never-listening) peer; if the node does not route the reply,
//!    the outbound assertion above remains the pass criterion and the limitation is
//!    reported on stderr rather than faked.
//!
//! # When it is skipped
//!
//! If no usable `erl` is on `PATH` the test **honestly skips** (it prints a clear
//! reason and returns) instead of failing — the crate itself is still fully
//! exercised by the hermetic loopback tests. The test needs no external setup
//! beyond `erl`: it picks an ephemeral port, uses a process-unique node name and
//! cookie, connects straight to the fixed distribution port (never `epmd`), and
//! uses bounded timeouts everywhere so it can neither hang nor leak the child.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_codec::term::make_external_pid;
use beam_dist::connection::{Connection, TcpTransport};
use beam_dist::flags::{DistFlags, Dop, DFLAG_SEND_SENDER};
use beam_term::prelude::{make_atom, make_small};
use beam_term::{AtomTable, Eterm};

/// The name the spawned node registers its receiver loop under.
const REGISTERED_NAME: &str = "genesis_recv";

/// The Erlang script the node evaluates: register a receiver, print `READY`, then
/// echo `{hello, V, From}` back to `From` while printing everything it receives.
///
/// The `{hello, V, From}` clause lets the node route a reply to the client pid
/// carried *inside* the message (an inbound distributed message does not expose
/// its sender to the receiving process, so the pid must travel in the payload).
const NODE_EVAL: &str = r#"P = spawn(fun Loop() ->
    receive
        {hello, V, From} ->
            From ! {echo, V},
            io:format("GOT {hello,~p}~n", [V]),
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

/// How long to wait for the node to print `READY`.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to keep retrying the TCP connect to the node's listener.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The socket read timeout for the established link (bounds every `recv`).
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for the node to print the received message.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// A process-wide counter, so a run's node name/cookie never collides with those
/// of a concurrent test binary.
static RUN: AtomicU32 = AtomicU32::new(0);

/// A spawned `erl` node whose stdout/stderr are read on background threads.
///
/// Killing and reaping the child (and joining the readers) happens in [`Drop`], so
/// a failing assertion never leaks the node.
struct ErlangNode {
    child: Child,
    lines: Receiver<String>,
    stdout_thread: Option<thread::JoinHandle<()>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
    port: u16,
}

impl ErlangNode {
    /// Starts a `-sname` node listening on `127.0.0.1:port` (no `epmd` needed: the
    /// test connects straight to that fixed distribution port).
    fn start(name: &str, cookie: &str, port: u16) -> std::io::Result<ErlangNode> {
        let mut child = Command::new("erl")
            .arg("-sname")
            .arg(name)
            .arg("-setcookie")
            .arg(cookie)
            .arg("-kernel")
            .arg("inet_dist_listen_min")
            .arg(port.to_string())
            .arg("inet_dist_listen_max")
            .arg(port.to_string())
            .arg("-noshell")
            .arg("-eval")
            .arg(NODE_EVAL)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr_pipe = child.stderr.take().expect("stderr was piped");

        let (tx, lines) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    // Stop if the receiver (the test) has gone away.
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_sink = Arc::clone(&stderr);
        let stderr_thread = thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                match line {
                    Ok(line) => {
                        if let Ok(mut buf) = stderr_sink.lock() {
                            buf.push_str(&line);
                            buf.push('\n');
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(ErlangNode {
            child,
            lines,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stderr,
            port,
        })
    }

    /// The loopback address of the node's distribution listener.
    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Waits (bounded by `timeout`) for a stdout line satisfying `pred`.
    ///
    /// Returns the matching line, or `None` if the timeout elapses first (or the
    /// node's stdout closes).
    fn wait_for<F: Fn(&str) -> bool>(&self, pred: F, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    if pred(&line) {
                        return Some(line);
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// The node's stderr collected so far (for failure diagnostics).
    fn stderr_text(&self) -> String {
        self.stderr.lock().map(|buf| buf.clone()).unwrap_or_default()
    }
}

impl Drop for ErlangNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Reports whether a usable `erl` exists and, if so, the short hostname Erlang
/// itself derives. That hostname is the host part of a `-sname` node name, so
/// deriving it here (rather than from the OS) guarantees the client node name is
/// in the node's own realm. `None` means "skip".
///
/// `inet:gethostname/0` returns `{ok, Host}` on OTP 29, hence the `element/2`.
fn erl_hostname() -> Option<String> {
    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[element(2,inet:gethostname())]), halt().",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let host = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Picks a free loopback TCP port by binding an ephemeral listener and dropping
/// it (a small TOCTOU window that the connect retry loop absorbs).
fn free_port() -> u16 {
    for _ in 0..16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
        let port = listener.local_addr().expect("read the ephemeral port").port();
        drop(listener);
        if port != 0 {
            return port;
        }
    }
    panic!("could not obtain a free loopback port");
}

/// Connects to `addr`, retrying until `timeout` elapses (the node may not be
/// listening the instant it prints `READY`).
fn connect_with_retry(addr: &str, timeout: Duration) -> beam_common::Result<TcpTransport> {
    let deadline = Instant::now() + timeout;
    loop {
        match TcpTransport::connect(addr) {
            Ok(transport) => return Ok(transport),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(err);
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// The body-form external term image of `term` (heap pointers differ between
/// heaps, so decoded terms are compared by their encoded image).
fn body_image(term: Eterm, atoms: &AtomTable) -> Vec<u8> {
    beam_codec::encode_term_body(term, atoms).expect("encode a term body")
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

    let run = RUN.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    // The spawned node and the client node must have *distinct* names, or the node
    // sees the connect as a simultaneous connection from itself and refuses it.
    let erl_short = format!("genesis{pid}x{run}");
    let client_short = format!("rustcli{pid}x{run}");
    let cookie = format!("cook{pid}x{run}");
    let our_node = format!("{client_short}@{host}");
    let port = free_port();

    let node = ErlangNode::start(&erl_short, &cookie, port).expect("spawn the erl node");

    // 1. The node is up and its receiver is registered.
    let ready = node.wait_for(|line| line.contains("READY"), READY_TIMEOUT);
    assert!(
        ready.is_some(),
        "the erl node never printed READY (stderr: {})",
        node.stderr_text()
    );

    // 2. Connect and drive the full client handshake against the real node.
    let mut transport = connect_with_retry(&node.addr(), CONNECT_TIMEOUT).unwrap_or_else(|err| {
        panic!("could not connect to the erl node: {err:?} (stderr: {})", node.stderr_text())
    });
    transport.set_read_timeout(Some(READ_TIMEOUT)).expect("set a read timeout");
    let mut conn = Connection::client(transport, our_node.as_bytes(), cookie.as_bytes())
        .unwrap_or_else(|err| {
            panic!(
                "the distribution handshake with the real node failed: {err:?} (stderr: {})",
                node.stderr_text()
            )
        });

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

    // 3. Build the terms: our external pid, the empty cookie atom, the target name
    //    and a recognisable `{hello, 42, OurPid}` message.
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 16).expect("client heap");
    let node_atom = make_atom(atoms.intern(&our_node));
    // The client handshake advertises creation `0`, so our pid must too (see
    // `connection.rs`: `CLIENT_CREATION`).
    let our_pid =
        make_external_pid(&mut heap, node_atom, 1, 0, 0).expect("materialize the client pid");
    let empty_atom = make_atom(atoms.intern(""));
    let to_name = make_atom(atoms.intern(REGISTERED_NAME));
    let hello = make_atom(atoms.intern("hello"));
    let message = heap
        .tuple(&[hello, make_small(42), our_pid])
        .expect("build the message tuple");

    // 4. `{DOP_REG_SEND, From, Cookie, ToName} -- Message` (`dist.c:2468`).
    conn.send_control(Dop::RegSend, &[our_pid, empty_atom, to_name, message], &atoms)
        .expect("send DOP_REG_SEND");

    // 5. The real node must decode and deliver the message (its stdout proves it).
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

    // 6. Stretch: the node echoes `{echo, 42}` back to our pid over the same
    //    connection. Best effort — a node that will not route to an inbound-only
    //    client node simply leaves the primary assertion above as the pass
    //    criterion.
    let expected = heap
        .tuple(&[make_atom(atoms.intern("echo")), make_small(42)])
        .expect("build the expected echo tuple");
    let expected_image = body_image(expected, &atoms);
    match conn.recv(&mut heap, &mut atoms) {
        Ok(message) => {
            eprintln!(
                "live_interop: inbound reply opcode={:?} terms={}",
                message.dop,
                message.terms.len()
            );
            assert!(
                message.terms.len() >= 3,
                "a send control carries From, To and the message: {:?}",
                message.terms
            );
            assert_eq!(
                body_image(message.terms[2], &atoms),
                expected_image,
                "the reply must carry the echoed {{echo,42}} term"
            );
            eprintln!("live_interop: bidirectional round-trip OK — decoded {{echo,42}} from the node");
        }
        Err(err) => {
            eprintln!(
                "live_interop: no inbound reply (bidirectional stretch not routed by the node): \
                 {err:?} — the outbound interop assertion above still passed"
            );
        }
    }

    // The node is killed and reaped by `Drop`.
}
