//! The **distributed node** (contract A.5, Phase 1) at the runtime boundary.
//!
//! What these tests pin, and with what:
//!
//! * a configured `-sname`/`-name` really binds a listener and really publishes the
//!   node to `epmd` (`beam_dist::epmd::lookup_port` resolves the *same* port the
//!   node reports) — skipped **loudly** when no `epmd` daemon is reachable;
//! * a **real system `erl` peer** completes the `'N'` handshake against this
//!   runtime: the peer is started with `net_kernel:connect_node/1`, which is real
//!   OTP resolving this node through `epmd` and performing the v6 handshake as the
//!   connecting side, and the runtime's own link reaches
//!   `DistConnection::is_established()` with the peer's full `node@host` name,
//!   while the peer reports `true` — skipped **loudly** when the system toolchain
//!   is absent.
//!
//! The handshake only completes because the node advertises its **full**
//! `node@host` name on the wire: OTP's connecting side validates the name in the
//! server's `'N'`/challenge message against the node it dialled
//! (`dist_util.erl`'s `recv_challenge_new/2`), so a bare `-sname` advertised bare
//! is aborted mid-handshake (the proof on the protocol side is
//! `beam_dist/tests/live_accept.rs::a_bare_advertised_name_is_aborted_by_the_real_peer`).
//!
//! Phase 1 deliberately stops at `Established`: what a link carries afterwards
//! (`DOP_REG_SEND`/`DOP_SEND` into a mailbox) is Phase 2, so these tests assert the
//! handshake, the peer identity and the registration — never a delivered payload.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use beam_rt::{Runtime, RuntimeConfig};

/// How long a real `erl` peer is given to resolve and handshake with this node.
const PEER_TIMEOUT: Duration = Duration::from_secs(20);

/// A unique-enough tag for this test run's node/cookie names (the differential
/// corpus runs sequentially, but a developer may run the suite twice at once).
fn unique(tag: &str) -> String {
    format!("beamrt{}{}", tag, std::process::id())
}

/// Runs `erl -noshell -eval EXPR` and returns its trimmed stdout.
fn erl_eval(expr: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", expr])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The short hostname Erlang itself derives — the host part of a `-sname` node
/// name, so the peer's view of this node is in the node's own realm.
fn erl_hostname() -> Option<String> {
    let host = erl_eval("io:format(\"~s\",[element(2,inet:gethostname())]), halt().")?;
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Whether a real `erl` can be executed at all (no toolchain ⇒ skip loudly).
fn erl_available() -> bool {
    erl_eval("halt(0).").is_some()
}

/// Distinguishes "epmd answered `noport`" (reachable) from "no daemon" — the peer
/// crate returns a typed error for both, so the probe is its own connect.
fn epmd_unreachable() -> bool {
    match beam_dist::epmd::connect(beam_dist::EPMD_DEFAULT_ADDR) {
        Ok(_) => false,
        Err(_) => true,
    }
}

/// Starts a `-sname` runtime with `-setcookie` through the **CLI parser**, so the
/// configuration is exactly what the command line produces.
fn node_runtime(name: &str, cookie: &str) -> Runtime {
    let args: Vec<String> = ["-noshell", "-sname", name, "-setcookie", cookie, "-eval", "ok."]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let config: RuntimeConfig = beam_rt::parse(&args).expect("the node switches parse");
    Runtime::new(config).expect("the runtime starts (the listener binds)")
}

/// A real `erl` peer is spawned, resolves this node through `epmd` and completes
/// the `'N'` handshake against the runtime's listener.
struct Peer {
    child: Child,
    /// The peer's stdout, completed when the reader thread sees EOF (the peer
    /// halts on its own) or when the peer is killed.
    stdout: Receiver<String>,
}

impl Peer {
    /// Spawns `erl -sname peer -setcookie cookie` asking `net_kernel` to connect to
    /// `target` (a full `node@host` name). The peer prints the `~p` of its
    /// `net_kernel:connect_node/1` result and then stays alive for two seconds
    /// before halting, so the link is not torn down before this side observed it.
    /// Drop kills it.
    fn connect(name: &str, cookie: &str, target: &str) -> std::io::Result<Peer> {
        let eval = format!(
            "T = net_kernel:connect_node('{target}'), \
             io:format(\"~p\",[T]), \
             timer:sleep(2000), \
             halt().",
            target = target
        );
        let mut child = Command::new("erl")
            .arg("-sname")
            .arg(name)
            .arg("-setcookie")
            .arg(cookie)
            .args(["-noshell", "-eval", &eval])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdout = child.stdout.take().expect("the peer's stdout is piped");
        // A detached reader: the peer's output is complete at EOF, which happens
        // when the peer halts by itself (or when Drop kills it).
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            let _ = tx.send(text);
        });
        Ok(Peer {
            child,
            stdout: rx,
        })
    }

    /// The peer's stdout, bounded by `timeout` (the peer halts on its own after
    /// two seconds, so this never waits longer than that unless `erl` hangs).
    fn stdout_within(&self, timeout: Duration) -> Option<String> {
        match self.stdout.recv_timeout(timeout) {
            Ok(text) => Some(text),
            Err(RecvTimeoutError::Timeout) => None,
            // The reader thread is gone without sending: the pipe is broken.
            Err(RecvTimeoutError::Disconnected) => Some(String::new()),
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `-sname NAME` makes the runtime a node: the name is `<NAME>@<host>`, the
/// listener is bound, and the node is published to `epmd` under the **short** name
/// (what a `PORT2_REQ` looks up).
#[test]
fn a_sname_node_binds_a_listener_and_registers_with_epmd() {
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: no usable system `erl` (cannot derive the short hostname)");
        return;
    };
    let name = unique("n");
    let cookie = unique("c");
    let mut runtime = node_runtime(&name, &cookie);

    // One pass is what publishes the node (registration happens on the first pump).
    runtime.run_one_slice().expect("a scheduler pass");

    let node = runtime.dist_node().expect("a configured node is a DistNode");
    assert_eq!(node.node_name(), format!("{name}@{host}"));
    assert!(
        node.is_short_name(),
        "a bare `-sname` value is a short name: {}",
        node.node_name()
    );
    assert_eq!(
        node.epmd_name(),
        name.as_bytes(),
        "epmd is told the short name"
    );
    assert!(node.port() > 0, "the listener bound a real port");

    if epmd_unreachable() {
        eprintln!("SKIP (registration): no `epmd` daemon on {}", beam_dist::EPMD_DEFAULT_ADDR);
        return;
    }
    assert!(
        node.is_registered(),
        "epmd accepted the registration: {:?}",
        node.registration_error()
    );
    assert_eq!(
        beam_dist::epmd::lookup_port(name.as_bytes()).expect("the node is registered"),
        node.port(),
        "epmd resolves the name to the very port the listener bound"
    );
    assert!(node.is_alive(), "a registered, listening node is alive");
}

/// A **real system `erl`** resolves this node through `epmd` and handshakes with
/// it: the runtime's link reaches `Established` and recorded the peer's name, and
/// the peer's own `net_kernel:connect_node/1` answers `true`.
#[test]
fn a_real_erl_peer_completes_the_handshake_against_the_node() {
    if !erl_available() {
        eprintln!("SKIP: no usable system `erl`");
        return;
    }
    if epmd_unreachable() {
        eprintln!("SKIP: a real peer resolves this node through epmd, and no `epmd` is reachable");
        return;
    }
    let Some(host) = erl_hostname() else {
        eprintln!("SKIP: cannot derive the short hostname");
        return;
    };
    let name = unique("h");
    let peer_name = unique("p");
    let cookie = unique("k");
    let mut runtime = node_runtime(&name, &cookie);
    runtime.run_one_slice().expect("a scheduler pass");
    let target = runtime
        .node_name()
        .expect("the node's name")
        .to_string();
    assert_eq!(target, format!("{name}@{host}"));

    let peer = Peer::connect(&peer_name, &cookie, &target).expect("the peer node starts");

    // The link must reach `Established` with the connecting peer's name while the
    // peer is still alive (it stays up for two seconds after connecting).
    let deadline = Instant::now() + PEER_TIMEOUT;
    let mut established = None;
    while Instant::now() < deadline {
        runtime.run_scheduler().expect("a scheduler pass");
        if let Some(link) = runtime
            .dist_node()
            .expect("the node")
            .connections()
            .iter()
            .find(|link| link.is_established())
        {
            established = Some(link.peer_name());
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let node = runtime.dist_node().expect("the node");
    assert!(
        node.is_registered(),
        "the node is published to epmd (a peer can resolve it): {:?}",
        node.registration_error()
    );
    let Some(established_name) = established else {
        panic!(
            "a real erl peer resolved {target} through epmd but the 'N' handshake did not \
             complete within {PEER_TIMEOUT:?} (links={}, last_link_error={:?})",
            node.connections().len(),
            node.last_link_error(),
        );
    };
    // The `'N'` message carries the peer's own **full** `node@host` name (OTP's
    // `send_name/4` puts `atom_to_binary(Node)` on the wire), not a bare short name:
    // the same rule this node now follows for its own advertised name.
    assert_eq!(
        established_name.as_deref(),
        Some(format!("{peer_name}@{host}").as_str()),
        "the connecting peer's node name is what the 'N' message carried"
    );

    // The peer's own verdict: a real OTP node considers the distribution link up.
    // It prints the `~p` of `net_kernel:connect_node/1` and halts (bounded).
    let printed = peer
        .stdout_within(PEER_TIMEOUT)
        .expect("the peer prints its connect result and exits");
    assert_eq!(
        printed.trim(),
        "true",
        "a real erl node connected to {target} (peer printed {printed:?}, \
         last_link_error={:?})",
        node.last_link_error(),
    );
}
