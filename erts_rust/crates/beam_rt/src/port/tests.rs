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
//! The port provider's unit suite: [`crate::port`]'s `open_port/2` decoding, the
//! `port_info/1,2` vocabulary, the port-message pump and its driver interop.
//!
//! A separate file so the suite does not grow [`crate::port`] past its production size; the
//! sibling `driver/*_tests.rs` modules are split the same way. It is a child module, so
//! `use super::*;` reaches every private item of the provider.
use super::*;
use beam_io::signal::{Disposition, Signal};
use beam_io::DriverContext;
use beam_process::{ProcessState, SignalAtoms};

use crate::boot;
use crate::config::RuntimeConfig;

/// The process every port the fixture's hook opens claims as its owner.
const OWNER: ProcessId = ProcessId::new(7, 0);

/// A test fixture: the shared registry, the atom table both the provider and the test
/// resolve names through, the provider itself, and a term heap.
///
/// The atom table is interned **before** the provider takes its handle: the handle is
/// an `Rc` clone, so mutating the table afterwards would fork it (see the module docs).
struct Fixture {
    registry: Rc<RefCell<PortRegistry>>,
    atoms: Rc<AtomTable>,
    hook: RuntimePorts,
    heap: Heap,
}

/// The vocabulary the tests build `Name`/`Options` terms out of, and the item atoms.
///
/// The item atoms (`name`, `id`, `connected`, `os_pid`, `links`, `input`, `output`,
/// `monitors`) come from the boot seed; the option atoms do not, so the whole
/// `open_port/2` option whitelist is interned here too.
const TEST_ATOMS: &[&str] = &[
    "spawn",
    "spawn_executable",
    "fd",
    "not_a_driver",
    // The honored option tags.
    "cd",
    "env",
    "args",
    // The full option-atom whitelist and its rejected neighbours.
    "stream",
    "use_stdio",
    "stderr_to_stdout",
    "line",
    "nouse_stdio",
    "binary",
    "list",
    "in",
    "out",
    "eof",
    "hide",
    "exit_status",
    "overlapped_io",
    // The option-tuple whitelist and its rejected neighbours.
    "packet",
    "arg0",
    "parallelism",
    "busy_limits_port",
    "busy_limits_msgq",
    "nope",
];

fn fixture() -> Fixture {
    let mut atoms = AtomTable::new();
    boot::seed_core_atoms(&mut atoms);
    for name in TEST_ATOMS {
        atoms.intern(name);
    }
    let atoms = Rc::new(atoms);
    let registry = Rc::new(RefCell::new(PortRegistry::new().expect("a port registry")));
    let hook = RuntimePorts::new(Rc::clone(&registry), Rc::clone(&atoms), OWNER);
    Fixture {
        registry,
        atoms,
        hook,
        heap: Heap::new(512).expect("a test heap"),
    }
}

impl Fixture {
    /// The atom `name` as a term (it must be pre-interned: see [`Fixture`]).
    fn atom(&self, name: &str) -> Eterm {
        make_atom(self.atoms.lookup(name).expect("a pre-interned atom"))
    }

    /// `text` as an Erlang string on the fixture's heap.
    fn string(&mut self, text: &str) -> Eterm {
        charlist(&mut self.heap, text).expect("a string")
    }

    /// An Erlang list of `elements` on the fixture's heap.
    fn list(&mut self, elements: &[Eterm]) -> Eterm {
        let mut list = NIL;
        for element in elements.iter().rev() {
            list = heap_cons(&mut self.heap, *element, list);
        }
        list
    }

    /// The `{tag, value}` `Name` tuple `open_port/2` takes.
    fn name(&mut self, tag: &str, value: Eterm) -> Eterm {
        let tag = self.atom(tag);
        self.heap.tuple(&[tag, value]).expect("a name tuple")
    }

    /// The `{spawn, Cmd}` `Name` with `command` as the shell command line.
    fn spawn(&mut self, command: &str) -> Eterm {
        let command = self.string(command);
        self.name("spawn", command)
    }

    /// The `{args, [Arg…]}` option tuple of a `{spawn_executable, Path}` port.
    fn args_option(&mut self, args: &[&str]) -> Eterm {
        let terms: Vec<Eterm> = args.iter().map(|arg| self.string(arg)).collect();
        let list = self.list(&terms);
        let tag = self.atom("args");
        self.heap.tuple(&[tag, list]).expect("an args option")
    }        /// The value `port_info/1` answered for `name`.
    fn item(&mut self, port: PortId, name: &str) -> Eterm {
        let pairs = self
            .hook
            .port_info(port)
            .unwrap_or_else(|error| panic!("port_info({port:?}) failed: {error}"));
        let wanted = self.atom(name);
        pairs
            .iter()
            .find(|(key, _)| *key == wanted)
            .map(|(_, value)| *value)
            .unwrap_or_else(|| panic!("no {name:?} item in {pairs:?}"))
    }
}

/// A cons cell built directly on the fixture's heap (the `beam_alloc` interface, which
/// is what the production `charlist`/`pid_list` helpers use).
fn heap_cons(heap: &mut Heap, head: Eterm, tail: Eterm) -> Eterm {
    heap.cons(head, tail).expect("a cons cell")
}

/// `term` read as a byte string.
fn text_of(term: Eterm) -> String {
    terms::list_elements(term)
        .expect("a proper list")
        .into_iter()
        .map(|element| char::from(u8::try_from(signed_value(element)).expect("a byte")))
        .collect()
}

/// Asserts `result` is exactly `Err(BeamError::BadArg)`.
///
/// [`BeamError`] is not `PartialEq` (it carries an owned message and a live BEAM reason
/// term), so the check is a `matches!` rather than an `assert_eq!`.
fn assert_badarg<T: std::fmt::Debug>(result: Result<T>) {
    assert!(
        matches!(result, Err(BeamError::BadArg)),
        "expected badarg, got {result:?}"
    );
}

/// Drains the fixture's port poller until `port` produces stream data, returning it.
///
/// The child's output is *not* read by the test any more: the runtime's poller owns the
/// descriptors (the whole point of the `beam_io` adoption), so the bytes are recovered
/// through the same `pump` the runtime itself uses.
fn pump_data(registry: &mut PortRegistry, port: PortId) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        for event in registry.pump(Duration::from_millis(50)).expect("pump") {
            if let PortEvent::Data {
                port: source,
                bytes,
                ..
            } = event
            {
                if source == port {
                    return bytes;
                }
            }
        }
    }
    panic!("no stream data for {port:?}");
}

/// Drains the fixture's port poller until `port` reports its **exit**, returning every
/// data event it produced, in order (one entry per [`PortEvent::Data`]).
///
/// The child `port` names must be opened with the `exit_status` option: that notification
/// is what tells the test the stream is complete, rather than a fixed sleep. Each round is
/// one real [`PortRegistry::pump`] — the same read path the runtime's scheduler pass drives
/// — and a round that reports the exit is still drained for the bytes it read in the very
/// same round (a reaped child's last bytes and its status arrive together).
///
/// `port` must be the fixture's **only** live port: a round drains every live port, so an
/// event bearing another port would otherwise be discarded here — and for an `Exited` that
/// is unrecoverable, because [`PortRegistry::take_exit_status`] hands a status out exactly
/// once. Rather than silently starving the caller, a foreign event fails the helper loudly
/// (open one port per fixture at a time, or drive the two strictly one after the other).
fn pump_events_until_exit(
    registry: &mut PortRegistry,
    port: PortId,
    bound: Duration,
) -> Vec<Vec<u8>> {
    let deadline = Instant::now() + bound;
    let mut data = Vec::new();
    while Instant::now() < deadline {
        let mut exited = false;
        for event in registry.pump(Duration::from_millis(50)).expect("pump") {
            // A signal carries no port and no owner (it is node-global), so it is the one
            // event that says nothing about which port a round drained.
            let source = match &event {
                PortEvent::Data { port, .. }
                | PortEvent::Exited { port, .. }
                | PortEvent::Term { port, .. } => Some(*port),
                PortEvent::Signal { .. } => None,
            };
            if let Some(source) = source {
                assert_eq!(
                    source, port,
                    "another live port produced an event in the round for {port:?}; \
                     pump one port per fixture at a time"
                );
            }
            match event {
                PortEvent::Data { bytes, .. } => data.push(bytes),
                PortEvent::Exited { .. } => exited = true,
                _ => {}
            }
        }
        if exited {
            return data;
        }
    }
    panic!("{port:?} reported no exit within {bound:?}; got {data:?}");
}

/// [`pump_events_until_exit`] concatenated into the child's whole byte stream.
fn pump_until_exit(registry: &mut PortRegistry, port: PortId, bound: Duration) -> Vec<u8> {
    pump_events_until_exit(registry, port, bound).concat()
}
/// Runs `expr` under the host `erl` and returns its stdout, or `None` when the toolchain is
/// not usable (the caller then prints a loud skip banner — never a silent pass).
fn erl_eval(expr: &str) -> Option<String> {
    let output = std::process::Command::new("erl")
        .args(["-noshell", "-eval", expr, "-s", "init", "stop"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Prints the loud "this oracle cross-check was skipped" banner (see [`erl_eval`]).
fn skip_oracle_check(what: &str) {
    println!("================================================================");
    println!("SKIP: no usable `erl` on PATH — the oracle cross-check for {what} is skipped");
    println!("================================================================");
}
/// The `port_info/1,2` metadata of a port a test registers **directly** (bypassing
/// `open_port/2`), so a recorder driver can be driven without a real child.
fn test_meta(command: &str) -> PortMeta {
    PortMeta {
        command: command.to_string(),
        kind: PortName::Spawn,
        os_pid: None,
        stream: StreamType::List,
        notify_exit: false,
        monitors: Vec::new(),
        owner: OWNER,
        driver_terms: None,
        driver_atoms: None,
        driver_control_flags: None,
    }
}

/// A driver that records its lifecycle callbacks and can declare a timer.
///
/// The built-in drivers (`fd`/`efile`/`spawn`) give no observation point for the two
/// `beam_rt`-driven seams — the port-exit notification ([`Driver::port_exit`]) and the
/// poll deadline ([`Driver::next_timeout`] + [`Driver::timeout`]) — so they are pinned
/// against this recorder instead.
struct RecorderDriver {
    /// The registered name (required by the trait; unused by the tests).
    name: &'static str,
    /// The lifecycle callbacks in call order: `"exit:<Reason>"` from `port_exit` and
    /// `"stop"` from the teardown.
    log: Rc<RefCell<Vec<String>>>,
    /// The deadline `next_timeout` declares, when this recorder carries a timer.
    deadline: Option<Instant>,
    /// Where the timer records the instant it actually fired (absent without a timer).
    fired_at: Option<Rc<RefCell<Option<Instant>>>>,
}

impl RecorderDriver {
    /// A recorder with no timer, appending its callbacks to `log`.
    fn new(name: &'static str, log: Rc<RefCell<Vec<String>>>) -> RecorderDriver {
        RecorderDriver {
            name,
            log,
            deadline: None,
            fired_at: None,
        }
    }

    /// A recorder declaring a timer at `deadline`, recording when it fires.
    fn with_timer(deadline: Instant, fired_at: Rc<RefCell<Option<Instant>>>) -> RecorderDriver {
        RecorderDriver {
            name: "recorder",
            log: Rc::new(RefCell::new(Vec::new())),
            deadline: Some(deadline),
            fired_at: Some(fired_at),
        }
    }
}

impl Driver for RecorderDriver {
    fn name(&self) -> &str {
        self.name
    }

    fn start(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn output(&mut self, _ctx: &mut DriverContext<'_>) -> Result<usize> {
        Ok(0)
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        Ok(())
    }

    fn stop(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        self.log.borrow_mut().push("stop".to_string());
        Ok(())
    }

    fn port_exit(&mut self, _ctx: &mut DriverContext<'_>, reason: PortExitReason) -> Result<()> {
        self.log.borrow_mut().push(format!("exit:{reason:?}"));
        Ok(())
    }

    fn next_timeout(&self) -> Option<Instant> {
        // Declared until it has fired: the port re-arms its `TIMEOUT` task from this
        // after every interaction, so a still-declared deadline would fire every round.
        match &self.fired_at {
            Some(fired) if fired.borrow().is_some() => None,
            _ => self.deadline,
        }
    }

    fn timeout(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        if let Some(fired) = &self.fired_at {
            *fired.borrow_mut() = Some(Instant::now());
        }
        Ok(())
    }
}

#[test]
fn spawn_true_opens_a_real_registered_port() {
    let mut fx = fixture();
    let command = fx.string("true");
    let name = fx.name("spawn", command);

    let port = fx
        .hook
        .open_port(name, NIL)
        .expect("open_port({spawn, \"true\"}, [])");
    assert!(PortId::is_port(port.to_eterm()), "a real internal port");
    assert!(fx.registry.borrow().contains(port));

    // The child really started: `os_pid` is its process id, captured from the driver
    // before the port was registered.
    let os_pid = signed_value(fx.item(port, "os_pid"));
    assert!(os_pid > 0, "a spawned child has an os pid, got {os_pid}");

    // Every item atom resolves through the shared table (never a fabricated index), and
    // the pair list is exactly the oracle's `/1` roster followed by the `/2`-only
    // `monitors` — in the oracle's order.
    let pairs = fx.hook.port_info(port).expect("port_info");
    let keys: Vec<String> = pairs
        .iter()
        .map(|(key, _)| {
            fx.atoms
                .name(atom_val(*key))
                .expect("an item atom resolves")
                .to_string()
        })
        .collect();
    assert_eq!(
        keys,
        [
            "name", "links", "id", "connected", "input", "output", "os_pid", "monitors"
        ]
    );
    assert_eq!(text_of(fx.item(port, "name")), "true");
    // `id` is a bare small integer (the port number), **not** the port term.
    assert_eq!(
        fx.item(port, "id"),
        make_small(i64::try_from(port.number()).expect("an id fits"))
    );
    // `links` is the owner as a one-element list and `connected` is that same owner pid.
    let owner = OWNER.to_eterm();
    assert_eq!(
        terms::list_elements(fx.item(port, "links")).expect("links is a list"),
        vec![owner]
    );
    assert_eq!(fx.item(port, "connected"), owner);
    assert_eq!(fx.item(port, "input"), make_small(0), "a byte counter, 0 at open");
    assert_eq!(fx.item(port, "output"), make_small(0), "a byte counter, 0 at open");
    assert_eq!(fx.item(port, "monitors"), NIL, "nothing monitors a port yet");
}

#[test]
fn the_name_forms_and_their_options_are_accepted() {
    let mut fx = fixture();

    // `{spawn, Atom}` — an atom is a shell command line.
    let name = fx.name("spawn", fx.atom("not_a_driver"));
    let port = fx.hook.open_port(name, NIL).expect("{spawn, atom}");
    assert_eq!(text_of(fx.item(port, "name")), "not_a_driver");
    // `{spawn, [Prog, Arg…]}` is **flattened** (concatenation, no separator) into a shell
    // command line — the oracle has no argv form for `{spawn, …}`.
    let program = fx.string("/bin/sh");
    let dash_c = fx.string("-c");
    let command = fx.string("true");
    let argv = fx.list(&[program, dash_c, command]);
    let name = fx.name("spawn", argv);
    let port = fx.hook.open_port(name, NIL).expect("{spawn, argv}");
    assert_eq!(text_of(fx.item(port, "name")), "/bin/sh-ctrue");
    // `{spawn_executable, Path}` honouring `{args, […]}` and a `binary` stream.
    let path = fx.string("/bin/sh");
    let name = fx.name("spawn_executable", path);
    let dash_c = fx.string("-c");
    let true_cmd = fx.string("true");
    let args = fx.list(&[dash_c, true_cmd]);
    let args_pair = {
        let tag = fx.atom("args");
        fx.heap.tuple(&[tag, args]).expect("an args option")
    };
    let binary = fx.atom("binary");
    let options = fx.list(&[binary, args_pair]);
    let port = fx
        .hook
        .open_port(name, options)
        .expect("{spawn_executable, \"/bin/sh\"} with args");
    assert_eq!(text_of(fx.item(port, "name")), "/bin/sh");
    // `stream` is no longer a `port_info` item: the honored `binary` option is asserted
    // on the port's own metadata.
    let stream = fx
        .registry
        .borrow()
        .meta
        .get(&port.number())
        .expect("a live port")
        .stream;
    assert_eq!(stream, StreamType::Binary);

    // `{fd, N}` registers a descriptor port with no OS process. The descriptor is a
    // socketpair end the **test** owns and keeps open: `beam_io`'s driver adopts a
    // `{fd, N}` by putting it in non-blocking mode (`fcntl(F_SETFL, O_NONBLOCK)`), which
    // the sandbox refuses for an inherited descriptor (`EPERM`); a socketpair end always
    // accepts it, so the childless name form is exercised independently of the test
    // harness's fds.
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    let (local, _peer) = UnixStream::pair().expect("a socketpair");
    let fd = local.as_raw_fd();
    let name = fx.name("fd", make_small(i64::from(fd)));
    let port = fx.hook.open_port(name, NIL).expect("{fd, N}");
    // `os_pid` is `undefined` for a port without an OS process, while `connected` is
    // still the owner pid — both the oracle's answers.
    assert_eq!(fx.item(port, "os_pid"), fx.atom("undefined"));
    assert_eq!(fx.item(port, "connected"), OWNER.to_eterm());
}

#[test]
fn the_working_directory_option_is_applied() {
    let mut fx = fixture();
    let name = fx.spawn("pwd");
    let dir = fx.string("/");
    let cd = {
        let tag = fx.atom("cd");
        fx.heap.tuple(&[tag, dir]).expect("a cd option")
    };
    let options = fx.list(&[cd]);
    // The option is applied to the child (`pwd` runs in `/`); the port itself only
    // exposes that it started.
    let port = fx.hook.open_port(name, options).expect("{cd, \"/\"}");
    assert!(signed_value(fx.item(port, "os_pid")) > 0);
}

#[test]
fn invalid_names_and_options_are_badarg() {
    let mut fx = fixture();

    // A Name that is not a recognized `{tag, …}` tuple.
    assert_badarg(fx.hook.open_port(make_small(1), NIL));
    let command = fx.string("true");
    let unknown = fx.name("not_a_driver", command);
    assert_badarg(fx.hook.open_port(unknown, NIL));
    // A known tag whose `Cmd` the drivers cannot accept.
    let bad_command = fx.name("spawn", make_small(5));
    assert_badarg(fx.hook.open_port(bad_command, NIL));
    // A negative descriptor.
    let bad_fd = fx.name("fd", make_small(-1));
    assert_badarg(fx.hook.open_port(bad_fd, NIL));

    // An `Options` that is not a list at all.
    let spawn = fx.spawn("true");
    assert_badarg(fx.hook.open_port(spawn, make_small(5)));
    // A recognized option with a malformed value.
    let cd_pair = {
        let tag = fx.atom("cd");
        fx.heap.tuple(&[tag, make_small(5)]).expect("a cd option")
    };
    let options = fx.list(&[cd_pair]);
    assert_badarg(fx.hook.open_port(spawn, options));
    let bad_args = {
        let tag = fx.atom("args");
        let value = fx.atom("nope");
        fx.heap.tuple(&[tag, value]).expect("an args option")
    };
    let options = fx.list(&[bad_args]);
    assert_badarg(fx.hook.open_port(spawn, options));
    // A recognized option with the wrong arity.
    let cd_alone = {
        let tag = fx.atom("cd");
        fx.heap.tuple(&[tag]).expect("a cd option")
    };
    let options = fx.list(&[cd_alone]);
    assert_badarg(fx.hook.open_port(spawn, options));

    // Validation precedes allocation, so no rejected call consumed a port number: the
    // next successful `open_port` gets the very first id.
    let port = fx.hook.open_port(spawn, NIL).expect("a valid port");
    assert_eq!(port, PortId::new(0));
}

#[test]
fn whitelisted_option_atoms_and_tuples_are_accepted() {
    let mut fx = fixture();
    // The whole accepted option-atom whitelist, every no-op here except `binary`.
    let atoms = [
        "stream",
        "use_stdio",
        "stderr_to_stdout",
        "line",
        "nouse_stdio",
        "binary",
        "in",
        "out",
        "eof",
        "hide",
        "exit_status",
        "overlapped_io",
    ];
    let mut elements: Vec<Eterm> = Vec::new();
    for name in atoms {
        let atom = fx.atom(name);
        elements.push(atom);
    }
    // A whitelisted tuple option is accepted too.
    let packet = {
        let tag = fx.atom("packet");
        fx.heap.tuple(&[tag, make_small(4)]).expect("a packet option")
    };
    elements.push(packet);
    let options = fx.list(&elements);
    let name = fx.spawn("true");
    let port = fx
        .hook
        .open_port(name, options)
        .expect("whitelisted options are accepted");
    // Accepting an option never changes what the port is (the `binary` atom is the one
    // that *does* change the stream, exercised in the name-forms test).
    assert_eq!(text_of(fx.item(port, "name")), "true");
}

#[test]
fn options_outside_the_whitelist_are_badarg() {
    let mut fx = fixture();
    // `list` is not an option (a list stream is the default).
    let name = fx.spawn("true");
    let list = fx.atom("list");
    let options = fx.list(&[list]);
    assert_badarg(fx.hook.open_port(name, options));
    // An unknown option atom.
    let name = fx.spawn("true");
    let nope = fx.atom("nope");
    let options = fx.list(&[nope]);
    assert_badarg(fx.hook.open_port(name, options));
    // An unknown option tuple.
    let name = fx.spawn("true");
    let bad_tuple = {
        let tag = fx.atom("nope");
        fx.heap.tuple(&[tag, make_small(1)]).expect("a nope option")
    };
    let options = fx.list(&[bad_tuple]);
    assert_badarg(fx.hook.open_port(name, options));
    // A whitelisted tuple whose tag is fine but whose arity is wrong.
    let name = fx.spawn("true");
    let bad_arity = {
        let tag = fx.atom("packet");
        fx.heap.tuple(&[tag]).expect("a packet option")
    };
    let options = fx.list(&[bad_arity]);
    assert_badarg(fx.hook.open_port(name, options));
    // A non-atom, non-tuple option element.
    let name = fx.spawn("true");
    let options = fx.list(&[make_small(1)]);
    assert_badarg(fx.hook.open_port(name, options));
    // Validation precedes allocation: none of the rejected calls consumed a port number.
    let name = fx.spawn("true");
    let port = fx.hook.open_port(name, NIL).expect("a valid port");
    assert_eq!(port, PortId::new(0));
}

#[test]
fn close_port_releases_the_slot_and_reaps_a_running_child() {
    let mut fx = fixture();
    // `cat` blocks on the port's stdin pipe (which the runtime holds open), so the
    // child is certainly still running when the port is closed.
    let name = fx.spawn("cat");
    let port = fx.hook.open_port(name, NIL).expect("open_port({spawn, \"cat\"}, [])");
    assert!(fx.registry.borrow().contains(port));

    fx.hook
        .close_port(port)
        .expect("port_close/1 kills and reaps the child");
    assert!(!fx.registry.borrow().contains(port));
    // A closed port is *not live*: `port_info/1` reports no items (the oracle's
    // `undefined`), while a close of it stays `badarg` — never a silently accepted
    // close.
    assert_badarg(fx.hook.close_port(port));
    assert!(fx
        .hook
        .port_info(port)
        .expect("a closed port has no items")
        .is_empty());
}

#[test]
fn closing_a_port_that_already_exited_succeeds() {
    let mut fx = fixture();
    // `true` exits immediately; whether it has been reaped by the time it is closed
    // depends on scheduling, and both paths must succeed.
    let name = fx.spawn("true");
    let port = fx.hook.open_port(name, NIL).expect("open_port");
    fx.hook.close_port(port).expect("closing an exited child is fine");
    assert!(!fx.registry.borrow().contains(port));
}

#[test]
fn an_unallocated_port_is_badarg() {
    let mut fx = fixture();
    let never_allocated = PortId::new(999);
    // `port_info/1` answers the oracle's "no items", i.e. the atom `undefined` once
    // `beam_bifs`/the native floor render it — never `badarg`, and never a fabricated
    // pair (see [`PortRegistry::info`]).
    assert!(fx
        .hook
        .port_info(never_allocated)
        .expect("an unallocated port has no items, not a badarg")
        .is_empty());
    // Closing a port the runtime does not hold *is* `badarg`: the close cannot be
    // performed, and a silently accepted no-op would hide a leaked port.
    assert_badarg(fx.hook.close_port(never_allocated));
}

#[test]
fn write_command_really_writes_to_the_child_and_an_unknown_port_is_badarg() {
    let mut fx = fixture();
    // `cat` echoes exactly what it is given, so the write is observable through the
    // runtime's own poller (the same path that delivers `{Port, {data, …}}`).
    let name = fx.spawn("cat");
    let port = fx.hook.open_port(name, NIL).expect("open_port({spawn, \"cat\"}, [])");
    fx.registry
        .borrow_mut()
        .write_command(port, b"hello")
        .expect("port_command");
    let echo = pump_data(&mut fx.registry.borrow_mut(), port);
    assert_eq!(&echo, b"hello");

    // A port the registry does not hold is `badarg` (the oracle's reason for a closed
    // port), and so is a port id that was never allocated.
    assert_badarg(fx.registry.borrow_mut().write_command(PortId::new(9), b"x"));
    fx.hook.close_port(port).expect("port_close/1");
    assert_badarg(fx.registry.borrow_mut().write_command(port, b"x"));
}

/// `erlang:statistics(io)` reads the port subsystem's own byte counters: `output` is what
/// `port_command/2,3` handed to a port's stream and `input` is what a port delivered back
/// as `{Port, {data, …}}`. Both are bumped at the real read/write sites, so a byte count
/// can never be a fabricated constant.
#[test]
fn the_io_counters_track_the_bytes_written_to_and_read_from_a_port() {
    let mut fx = fixture();
    // The runtime keeps a clone of the registry's counters; grab it before borrowing the
    // registry mutably below.
    let counters = fx.registry.borrow().io_counters();
    assert_eq!(counters.bytes(), (0, 0), "a fresh runtime has moved no port bytes");

    let name = fx.spawn("cat");
    let port = fx
        .hook
        .open_port(name, NIL)
        .expect("open_port({spawn, \"cat\"}, [])");
    // `cat` echoes exactly what it is given, so the write and the echo are both
    // observable through the runtime's own poller.
    fx.registry
        .borrow_mut()
        .write_command(port, b"hello")
        .expect("port_command");
    assert_eq!(
        counters.bytes(),
        (0, 5),
        "the write is `output`; nothing has been read back yet"
    );
    let echo = pump_data(&mut fx.registry.borrow_mut(), port);
    assert_eq!(&echo, b"hello");
    assert_eq!(
        counters.bytes(),
        (5, 5),
        "the echo `cat` produced is `input`"
    );

    // A rejected write moves nothing: the counters are bumped only after the port is
    // resolved.
    assert_badarg(fx.registry.borrow_mut().write_command(PortId::new(9), b"x"));
    assert_eq!(counters.bytes(), (5, 5));
}

// --- the port READ path (contract A.4) ------------------------------------------------
//
// Two properties of the read path are pinned here, because both are observable and both
// are answerable by the oracle:
//
// * a `binary` stream has **no width limit** — the driver hands over up to 64 KiB of one
//   `read(2)`, and the oracle delivers a `binary()` of exactly that width, so the message
//   builder may not go through the engine's 64-byte-only on-heap binary writer;
// * the child's stderr is **inherited** from the emulator by default and merged into the
//   port's stream by `stderr_to_stdout` (`spawn_start`'s `stderrfd = opts->redir_stderr ?
//   ifd[1] : dup(2)`, `erts/emulator/sys/unix/sys_drivers.c:500`).

/// A `binary`-stream message is a `binary()` of **every** width.
///
/// `ERL_ONHEAP_BINARY_LIMIT` (64) is exactly where the engine's `make_heap_binary` stops,
/// so the widths below straddle it; a `list` stream is unchanged (a char list).
#[test]
fn a_binary_stream_message_is_a_binary_of_every_width() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let port = PortId::new(1);
    for width in [0usize, 1, 63, 64, 65, 200, 4096] {
        let bytes: Vec<u8> = (0..width).map(|i| (i % 251) as u8).collect();
        let message = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.port_data_message(port, &bytes, StreamType::Binary)
                .unwrap_or_else(|error| panic!("a {width}-byte binary-stream message: {error}"))
        };
        // `{Port, {data, Bin}}`.
        assert_eq!(terms::tuple_arity(message).expect("a port message tuple"), 2);
        let inner = terms::tuple_element(message, 1).expect("the `{data, …}` tuple");
        let payload = terms::tuple_element(inner, 1).expect("the data payload");
        assert_eq!(
            terms::binary_bytes(payload).expect("the payload is a binary"),
            &bytes[..],
            "a {width}-byte read is delivered as a {width}-byte binary"
        );
    }
    // The `data` atom really is the shared table's `data` (the printer and a `receive`
    // clause both resolve it).
    let data_atom = rt.code_server.atoms().lookup("data").expect("the data atom");
    let message = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.port_data_message(port, b"ab", StreamType::Binary)
            .expect("a message")
    };
    let inner = terms::tuple_element(message, 1).expect("the `{data, …}` tuple");
    assert_eq!(
        terms::tuple_element(inner, 0).expect("the tag"),
        make_atom(data_atom)
    );

    // The `list` stream is untouched: a char list, never a binary.
    let message = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.port_data_message(port, b"abc", StreamType::List)
            .expect("a list-stream message")
    };
    let inner = terms::tuple_element(message, 1).expect("the `{data, …}` tuple");
    assert_eq!(text_of(terms::tuple_element(inner, 1).expect("the payload")), "abc");
}

/// A real child's 200-byte `binary`-stream read reaches the owner at full width, through
/// the same pump the runtime's scheduler pass drives — and the oracle agrees on the width.
#[test]
fn a_wide_binary_stream_read_is_delivered_at_full_width() {
    let mut fx = fixture();
    // `head -c 200 /dev/zero` writes exactly 200 NUL bytes — three times the 64-byte
    // on-heap binary limit — and exits.
    let name = fx.spawn("head -c 200 /dev/zero");
    let binary = fx.atom("binary");
    let exit_status = fx.atom("exit_status");
    let options = fx.list(&[binary, exit_status]);
    let port = fx.hook.open_port(name, options).expect("open_port binary");

    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let chunks = pump_events_until_exit(
        &mut fx.registry.borrow_mut(),
        port,
        Duration::from_secs(10),
    );
    let mut got: Vec<u8> = Vec::new();
    for bytes in &chunks {
        // Every chunk is built into the oracle's `{Port, {data, Bin}}` shape, and each one
        // is a binary of exactly its own width.
        let message = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.port_data_message(port, bytes, StreamType::Binary)
                .expect("a binary-stream message")
        };
        let inner = terms::tuple_element(message, 1).expect("the `{data, …}` tuple");
        let payload = terms::tuple_element(inner, 1).expect("the payload");
        assert_eq!(
            terms::binary_bytes(payload).expect("a binary payload"),
            &bytes[..]
        );
        got.extend_from_slice(bytes);
    }
    assert_eq!(got.len(), 200, "the child's 200 bytes are delivered in full");
    assert!(
        got.iter().all(|byte| *byte == 0),
        "`/dev/zero` wrote NUL bytes"
    );
    // Oracle cross-check: the *same* child under the real runtime must report the same
    // width (the oracle stores a binary this wide off-heap; this runtime builds it on-heap
    // — the observable term is identical).
    let expr = r#"begin P = open_port({spawn,"head -c 200 /dev/zero"},[binary]), F = fun F(N) -> receive {P,{data,D}} -> F(N + byte_size(D)) after 2000 -> N end end, io:format("~w",[F(0)]) end"#;
    match erl_eval(expr) {
        Some(oracle) => assert_eq!(
            oracle, "200",
            "the oracle reads the same 200 bytes from the same child"
        ),
        None => skip_oracle_check("the width of a binary-stream read"),
    }
}

/// `stderr_to_stdout` merges the child's stderr into the port's stream; without it the
/// child's stderr is inherited by the emulator and never reaches the port.
///
/// Both ports run the *same* child (the same executable, `{args, …}` and command line), so
/// the option list is the only difference — which is what makes the assertion
/// discriminating rather than incidental. Without the option the child's `err` line lands
/// on this test process's own stderr (the C's `dup(2)` default), which is why it is absent
/// from the port's stream.
///
/// The two ports are driven **strictly one after the other**, never concurrently: the
/// fixture owns one poller and [`PortRegistry::pump`] drains *every* live port in a round,
/// so pumping one port while a second is open would fold the second port's chunks — and its
/// one-shot `{Port, {exit_status, …}}` — into a round whose events the first port's helper
/// does not keep (`beam_io::PortRegistry::take_exit_status` hands a status out exactly
/// once).
#[test]
fn the_stderr_to_stdout_option_merges_the_child_stderr_into_the_port_stream() {
    let mut fx = fixture();
    let script = "echo out; echo err >&2";
    let name = {
        let path = fx.string("/bin/sh");
        fx.name("spawn_executable", path)
    };
    let args = fx.args_option(&["-c", script]);
    let binary = fx.atom("binary");
    let exit_status = fx.atom("exit_status");
    let merge = fx.atom("stderr_to_stdout");

    // The merged port first, driven to its exit before the plain port is opened.
    let merged_port = {
        let merged = fx.list(&[binary, merge, exit_status, args]);
        fx.hook
            .open_port(name, merged)
            .expect("open_port [binary, stderr_to_stdout, exit_status]")
    };
    let merged = pump_until_exit(
        &mut fx.registry.borrow_mut(),
        merged_port,
        Duration::from_secs(10),
    );
    assert_eq!(
        String::from_utf8_lossy(&merged),
        "out\nerr\n",
        "the child's stderr is port data, interleaved with stdout in write order"
    );

    // …then the *same* child **without** the option (the option list is the only difference).
    let plain_port = {
        let plain = fx.list(&[binary, exit_status, args]);
        fx.hook
            .open_port(name, plain)
            .expect("open_port [binary, exit_status]")
    };
    let plain = pump_until_exit(
        &mut fx.registry.borrow_mut(),
        plain_port,
        Duration::from_secs(10),
    );
    assert_eq!(
        String::from_utf8_lossy(&plain),
        "out\n",
        "without the option the child's stderr is inherited, never port data"
    );

    // Oracle cross-check: the merged byte sequence, as the oracle delivers it.
    let expr = r#"begin P = open_port({spawn_executable,"/bin/sh"},[binary,stderr_to_stdout,exit_status,{args,["-c","echo out; echo err >&2"]}]), F = fun F(A) -> receive {P,{data,D}} -> F(<<A/binary,D/binary>>); {P,{exit_status,_}} -> A after 3000 -> A end end, io:format("~w",[binary_to_list(F(<<>>))]) end"#;
    match erl_eval(expr) {
        Some(oracle) => assert_eq!(
            oracle,
            format!("[{}]", byte_list(&merged)),
            "the oracle merges the same bytes in the same order"
        ),
        None => skip_oracle_check("the `stderr_to_stdout` merge"),
    }
}

/// `bytes` rendered as the `~w` form of an Erlang byte list (`[1,2,3]`), so a whole
/// sequence can be compared against an oracle's `io:format("~w", …)` output.
fn byte_list(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| byte.to_string())
        .collect::<Vec<String>>()
        .join(",")
}
#[test]
fn a_fd_port_write_reaches_the_adopted_descriptor() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    let mut fx = fixture();
    // A descriptor the **test** owns and keeps open: `beam_io`'s driver adopts a
    // `{fd, N}` by putting it in non-blocking mode (`fcntl(F_SETFL, O_NONBLOCK)`), which
    // the sandbox refuses for an inherited descriptor (`EPERM`); a socketpair end always
    // accepts it. The adoption is **duplex** — the one descriptor is registered with
    // `Interest::BOTH` — so `port_command` writes out through it (the oracle's own
    // behaviour for a `{fd, N}` port).
    let (local, mut peer) = UnixStream::pair().expect("a socketpair");
    let fd = local.as_raw_fd();
    let name = fx.name("fd", make_small(i64::from(fd)));
    let port = fx.hook.open_port(name, NIL).expect("{fd, N}");

    // The write takes the normal driver write path and the bytes come out of the peer.
    fx.registry
        .borrow_mut()
        .write_command(port, b"ping")
        .expect("the write reaches the adopted descriptor");
    let mut buf = [0u8; 4];
    peer.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a read timeout");
    peer.read_exact(&mut buf)
        .expect("the peer reads what the port wrote");
    assert_eq!(&buf, b"ping");

    // A write never closes the port.
    assert!(fx.registry.borrow().is_live(port));
    fx.hook.close_port(port).expect("port_close/1");
}

#[test]
fn an_empty_port_name_is_badarg() {
    let mut fx = fixture();
    // The oracle rejects an empty command/path in its own validation of the `Name`
    // (`open_port({spawn,""},[])` and `open_port({spawn_executable,""},[])` are both
    // `error:badarg` against erts-17.1) — before any driver is reached, whose `exec` of
    // an empty program would report `enoent` instead.
    let empty = fx.string("");
    let name = fx.name("spawn", empty);
    assert_badarg(fx.hook.open_port(name, NIL));
    let empty = fx.string("");
    let name = fx.name("spawn_executable", empty);
    assert_badarg(fx.hook.open_port(name, NIL));
    // The 3-tuple form carries the same name.
    let empty = fx.string("");
    let options = fx.list(&[]);
    let tag = fx.atom("spawn_executable");
    let name = fx.heap.tuple(&[tag, empty, options]).expect("a name tuple");
    assert_badarg(fx.hook.open_port(name, NIL));

    // Validation precedes allocation: none of the rejected calls consumed a port number.
    let name = fx.spawn("true");
    let port = fx.hook.open_port(name, NIL).expect("a valid port");
    assert_eq!(port, PortId::new(0));
}

#[test]
fn closing_a_port_notifies_the_driver_of_the_exit_before_the_teardown() {
    let fx = fixture();
    let log = Rc::new(RefCell::new(Vec::new()));
    let driver = RecorderDriver::new("recorder", Rc::clone(&log));
    let port = fx
        .registry
        .borrow_mut()
        .register(Box::new(driver), test_meta("recorder"))
        .expect("a registered port");

    // The driver is told the port is exiting (`Closed`) **before** its teardown `stop`:
    // the two are distinct callbacks, and the order is what makes `port_exit` an
    // advisory notification rather than part of the teardown.
    fx.registry.borrow_mut().close(port).expect("close");
    assert_eq!(
        *log.borrow(),
        vec!["exit:Closed".to_string(), "stop".to_string()],
        "the port-exit notification precedes the teardown"
    );
    assert!(!fx.registry.borrow().contains(port));
    // A close the runtime has already performed is never silently accepted again.
    assert_badarg(fx.registry.borrow_mut().close(port));
}

#[test]
fn a_port_exit_notification_is_delivered_once_and_leaves_the_port_live() {
    let fx = fixture();
    let log = Rc::new(RefCell::new(Vec::new()));
    let driver = RecorderDriver::new("recorder", Rc::clone(&log));
    let port = fx
        .registry
        .borrow_mut()
        .register(Box::new(driver), test_meta("recorder"))
        .expect("a registered port");

    // An isolated notification reaches the driver **once** and does not tear the port
    // down: a repeat never calls `port_exit` again, and the port is still live for the
    // later `stop`.
    let mut borrowed = fx.registry.borrow_mut();
    let ports = &mut *borrowed;
    ports
        .registry
        .notify_port_exit(&mut ports.poller, port, PortExitReason::Closed)
        .expect("the first notification reaches the driver");
    assert_eq!(*log.borrow(), vec!["exit:Closed".to_string()]);
    ports
        .registry
        .notify_port_exit(&mut ports.poller, port, PortExitReason::OwnerExited)
        .expect("a repeat is a no-op");
    assert_eq!(
        *log.borrow(),
        vec!["exit:Closed".to_string()],
        "the port-exit notification is delivered at most once"
    );
    assert!(
        ports.contains(port),
        "the notification leaves the port live"
    );
    ports.close(port).expect("the teardown still runs");
    assert_eq!(
        *log.borrow(),
        vec!["exit:Closed".to_string(), "stop".to_string()]
    );
}

/// The pump never waits past a driver-declared deadline.
///
/// Two declarations prove it, and the split is deliberate — only one of them can carry a
/// wall-clock upper bound:
///
/// * **A deadline in the future** (`+10 ms`, inside [`PORT_POLL_SLICE`]) is the clamp's
///   *lower* bound: the timer must not fire early, and it must fire exactly once. No upper
///   bound may be asserted here, because servicing this declaration requires the poll to
///   **sleep** the remaining ~10 ms, and an `epoll_wait` wake-up is only as prompt as the
///   OS scheduler: under parallel load the observed fire time reaches 60 ms while the
///   unclamped wait — the whole [`PORT_POLL_SLICE`] — is only 25 ms, so *any* wall-clock
///   bound on this measurement is either flaky or no longer discriminating (measured on
///   this host: 18–62 ms clamped vs ~25 ms unclamped).
/// * **A deadline already due** is the clamp's *upper* bound, and it is the sound one: the
///   clamp is `min(slice, deadline − now)` with a saturating subtraction, so a due deadline
///   waits **zero** and the poll does not sleep at all — the timer is serviced by the very
///   next task round, in microseconds (measured: ≤ 36 µs over 200 runs on a 40-way loaded
///   14-core host). Without the clamp that first poll blocks a whole [`PORT_POLL_SLICE`]
///   before any task can run, so the 5 ms bound below sits ~140× above the measured cost
///   and ~5× below the unclamped wait: it tolerates a scheduling hiccup, yet an unclamped
///   wait — a *sleeping* poll of a full slice (measured at 25.08 ms with the clamp removed)
///   — cannot pass it.
#[test]
fn the_pump_clamps_its_wait_to_a_driver_declared_deadline() {
    let fx = fixture();
    // --- a deadline in the future is a lower bound -------------------------------------
    let fired_at = Rc::new(RefCell::new(None::<Instant>));
    let declared = Instant::now() + Duration::from_millis(10);
    let driver = RecorderDriver::with_timer(declared, Rc::clone(&fired_at));
    fx.registry
        .borrow_mut()
        .register(Box::new(driver), test_meta("timer"))
        .expect("a registered port");

    // The declaration reaches the clamp: `beam_io`'s `next_deadline` is exactly the
    // driver's `next_timeout`, which the port's `TIMEOUT` task mirrors.
    assert_eq!(
        fx.registry.borrow().registry.next_deadline(),
        Some(declared),
        "the driver's declaration is the clamp's upper bound"
    );

    // A pump whose own timeout (60 ms) is far past the deadline still services the timer
    // **at** its deadline: the poll may not block past it, so the task round right after
    // the poll runs it.
    let started = Instant::now();
    let events = fx
        .registry
        .borrow_mut()
        .pump(Duration::from_millis(60))
        .expect("pump");
    assert!(events.is_empty(), "a timer is not a port message: {events:?}");

    let fired = fired_at.borrow().expect("the declared timer fired");
    let delay = fired.duration_since(started);
    assert!(
        delay >= Duration::from_millis(5),
        "the deadline is a lower bound — the timer must not fire early, fired after {delay:?}"
    );
    // The timer fired once and cleared its own declaration, so nothing is left armed.
    assert_eq!(*fired_at.borrow(), Some(fired), "the timer fired exactly once");
    assert_eq!(fx.registry.borrow().registry.next_deadline(), None);

    // --- a deadline already due is clamped to a zero wait -------------------------------
    let fired_due_at = Rc::new(RefCell::new(None::<Instant>));
    // Already due, so the clamp's `saturating_sub` yields `0` rather than a negative wait.
    let declared_due = Instant::now() - Duration::from_millis(1);
    let driver = RecorderDriver::with_timer(declared_due, Rc::clone(&fired_due_at));
    fx.registry
        .borrow_mut()
        .register(Box::new(driver), test_meta("timer-due"))
        .expect("a registered port");
    // The still-live first port declares nothing now, so this is the only declaration.
    assert_eq!(
        fx.registry.borrow().registry.next_deadline(),
        Some(declared_due),
        "the due deadline is the declaration the clamp sees"
    );
    let started_due = Instant::now();
    let events = fx
        .registry
        .borrow_mut()
        .pump(Duration::from_millis(60))
        .expect("pump");
    assert!(events.is_empty(), "a timer is not a port message: {events:?}");
    let fired_due = fired_due_at.borrow().expect("the due timer fired");
    let delay_due = fired_due.duration_since(started_due);
    assert!(
        delay_due < Duration::from_millis(5),
        "a due deadline is serviced without sleeping: the clamp yielded a zero wait, so the \
         poll returned at once (measured µs even under load) instead of the \
         {PORT_POLL_SLICE:?} slice an unclamped wait would spend, fired after {delay_due:?}"
    );
    assert_eq!(
        *fired_due_at.borrow(),
        Some(fired_due),
        "the due timer fired exactly once"
    );
    assert_eq!(fx.registry.borrow().registry.next_deadline(), None);
}
#[test]
fn a_fd_port_adopts_a_caller_owned_descriptor() {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    let mut fx = fixture();
    // A descriptor the **test** owns and keeps open for the whole test.
    // `beam_io`'s driver adopts a `{fd, N}` by putting it in non-blocking mode
    // (`fcntl(F_SETFL, O_NONBLOCK)`), which the sandbox refuses for an inherited
    // descriptor (fd 0 yields `EPERM`); a socketpair end always accepts it, so the
    // childless name form is exercised independently of the test harness's fds.
    let (mut local, _peer) = UnixStream::pair().expect("a socketpair");
    let fd = local.as_raw_fd();

    let name = fx.name("fd", make_small(i64::from(fd)));
    let port = fx
        .hook
        .open_port(name, NIL)
        .expect("{fd, N} adopts the descriptor");

    // A descriptor port has no OS process of its own, is connected to its owner, and
    // reports the `fd:<N>` rendering the reference implementation uses.
    assert_eq!(fx.item(port, "os_pid"), fx.atom("undefined"));
    assert_eq!(fx.item(port, "connected"), OWNER.to_eterm());
    assert_eq!(text_of(fx.item(port, "name")), format!("fd:{fd}"));
    assert!(fx.registry.borrow().contains(port));

    // Closing the port releases it but **never closes** the adopted descriptor: it
    // belongs to the Erlang side. The write below would fail with `EBADF` if the
    // driver had closed it.
    fx.hook.close_port(port).expect("port_close/1");
    assert!(!fx.registry.borrow().contains(port));
    local
        .write_all(b"x")
        .expect("the adopted descriptor is still open after port_close/1");
}

#[test]
fn port_info_on_an_unknown_or_closed_port_is_an_empty_pair_list() {
    let mut fx = fixture();
    // Never opened: no pairs (the oracle's `undefined` through `beam_bifs`, and the
    // native floor's own `undefined` for `erlang:port_info/1,2`).
    assert!(fx.hook.port_info(PortId::new(9)).expect("no badarg").is_empty());
    assert!(!fx.registry.borrow().is_live(PortId::new(9)));

    // Closed: same answer, and `port_close/1` on it is still `badarg` — the two are
    // deliberately different questions (reports may be empty, but a close the runtime
    // cannot perform is never silently accepted).
    let name = fx.spawn("true");
    let port = fx.hook.open_port(name, NIL).expect("open_port");
    assert!(fx.registry.borrow().is_live(port));
    assert_eq!(
        fx.hook.port_info(port).expect("a live port").len(),
        8,
        "seven `/1` items plus the `/2`-only `monitors`"
    );
    fx.hook.close_port(port).expect("port_close/1");
    assert!(!fx.registry.borrow().is_live(port));
    assert!(fx.hook.port_info(port).expect("no badarg").is_empty());
    assert_badarg(fx.hook.close_port(port));
}

#[test]
fn the_exit_status_option_is_remembered_on_the_port() {
    let mut fx = fixture();
    // Without the option the poller has nothing to report.
    let name = fx.spawn("true");
    let plain = fx.hook.open_port(name, NIL).expect("open_port");
    let exit_status = fx.atom("exit_status");
    let options = fx.list(&[exit_status]);
    let name = fx.spawn("true");
    let with_option = fx.hook.open_port(name, options).expect("open_port");
    let registry = fx.registry.borrow();
    assert!(
        !registry.meta.get(&plain.number()).expect("a port").notify_exit,
        "a port opened without exit_status must not report an exit message"
    );
    assert!(
        registry
            .meta
            .get(&with_option.number())
            .expect("a port")
            .notify_exit,
        "the exit_status option must be recorded"
    );
}

#[test]
fn the_pump_reports_a_reaped_childs_exit_status_once() {
    let mut fx = fixture();
    let exit_status = fx.atom("exit_status");
    let options = fx.list(&[exit_status]);
    let name = fx.spawn("sh -c \"exit 3\"");
    let port = fx.hook.open_port(name, options).expect("open_port");

    // The status is not tied to stream readiness: a child that wrote nothing still
    // reports it, and it is reported exactly once.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut status = None;
    let mut reports = 0;
    while Instant::now() < deadline {
        for event in fx
            .registry
            .borrow_mut()
            .pump(Duration::from_millis(50))
            .expect("pump")
        {
            if let PortEvent::Exited {
                port: source,
                status: reported,
                ..
            } = event
            {
                assert_eq!(source, port);
                status = Some(reported);
                reports += 1;
            }
        }
        if status.is_some() {
            break;
        }
    }
    assert_eq!(status, Some(3), "the reaped child's exit code");
    assert_eq!(reports, 1);

    // A second round must not report it again.
    let quiet = fx
        .registry
        .borrow_mut()
        .pump(Duration::from_millis(50))
        .expect("pump");
    assert!(
        quiet.is_empty(),
        "an exit status is handed out exactly once: {quiet:?}"
    );

    // A port opened *without* the option never reports one.
    let name = fx.spawn("true");
    let plain = fx.hook.open_port(name, NIL).expect("open_port");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let events = fx
            .registry
            .borrow_mut()
            .pump(Duration::from_millis(50))
            .expect("pump");
        for event in events {
            assert!(
                !matches!(event, PortEvent::Exited { port: p, .. } if p == plain),
                "a port without the exit_status option must not report one"
            );
        }
        if fx.registry.borrow().is_live(plain) {
            // The child has certainly exited by now; nothing was reported for it.
            break;
        }
    }
}

#[test]
fn the_port_info_atom_vocabulary_is_seeded() {
    let atoms = AtomTable::new();
    let mut seeded = atoms;
    boot::seed_core_atoms(&mut seeded);
    for name in PORT_INFO_ATOMS {
        assert!(seeded.lookup(name).is_some(), "{name} must be seeded");
    }
    // The provider resolves `true`/`false`/`undefined` too.
    for name in ["true", "false", "undefined"] {
        assert!(seeded.lookup(name).is_some(), "{name} must be seeded");
    }
}

#[test]
fn an_empty_port_registry_has_no_ports() {
    let registry = PortRegistry::new().expect("a port registry");
    assert!(registry.is_empty());
    assert!(!registry.contains(PortId::new(0)));
    assert_eq!(registry.len(), 0);
    assert!(registry.ids().is_empty());
}

#[test]
fn ids_reports_live_ports_across_a_released_lower_slot() {
    let mut fx = fixture();
    let first = {
        let name = fx.spawn("true");
        fx.hook.open_port(name, NIL).expect("first port")
    };
    let second = {
        let name = fx.spawn("true");
        fx.hook.open_port(name, NIL).expect("second port")
    };
    assert_eq!(
        fx.registry.borrow().ids(),
        vec![first, second],
        "the live ports are reported in port-number order"
    );

    // Closing the *lower* port leaves a gap below the still-live higher one. The
    // metadata map is the source of truth, so `ids` still reports the higher port (a
    // scan-from-zero recovery would have stopped at the gap and missed it).
    fx.hook.close_port(first).expect("closing the first port");
    assert!(!fx.registry.borrow().contains(first));
    assert_eq!(fx.registry.borrow().ids(), vec![second]);
}

// -----------------------------------------------------------------------------------------
// The driver-emitted-term delivery handoff (`erl_drv_output_term`/`erl_drv_send_term`)
// -----------------------------------------------------------------------------------------

/// A `DriverPort` host plus the `port_info` metadata a `{spawn_driver, Name}` `open_port/2`
/// builds for it: the two halves of the driver-term handoff (see
/// [`RuntimePorts::open_port`]).
///
/// The host's [`driver::TermQueue`] is the **same** handle stored on `meta.driver_terms`, so
/// pushing through the host (what a driver callback does) is exactly what the port's drain
/// observes.
fn driver_term_host(atoms: &Rc<AtomTable>, owner: ProcessId) -> (driver::DriverPort, PortMeta) {
    let host = driver::DriverPort::new(
        Heap::new(64).expect("a driver heap"),
        Rc::clone(atoms),
        owner,
    );
    let queue = host.term_queue();
    let mut meta = test_meta("driver");
    meta.kind = PortName::SpawnDriver;
    meta.owner = owner;
    meta.driver_terms = Some(queue);
    meta.driver_atoms = Some(host.pending_atoms());
    (host, meta)
}

/// The same, registered in the fixture's registry (a port with no OS child, driven by a
/// [`RecorderDriver`]).
fn driver_term_fixture(
    fx: &Fixture,
    owner: ProcessId,
) -> (driver::DriverPort, PortId) {
    let (host, meta) = driver_term_host(&fx.atoms, owner);
    let port = fx
        .registry
        .borrow_mut()
        .register(
            Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
            meta,
        )
        .expect("a registered driver port");
    (host, port)
}

/// A term a C driver emitted is drained off the port's [`driver::TermQueue`] as a
/// [`PortEvent::Term`] — the decoded term itself, with the destination `erl_drv_output_term`
/// (`None` = the owner) or `erl_drv_send_term` (`Some(pid)`) named — and a destination this
/// node cannot resolve is dropped (the C's own "not delivered", `io.c:5664`).
#[test]
fn a_driver_emitted_term_is_drained_as_a_term_event() {
    use beam_abi::DriverHost;

    let fx = fixture();
    let (mut host, port) = driver_term_fixture(&fx, OWNER);

    // Nothing queued yet.
    assert!(fx.registry.borrow_mut().take_driver_terms().is_empty());

    let recipient = ProcessId::new(9, 0);
    // `erl_drv_output_term`: the port's connected process.
    assert_eq!(host.send_term(std::ptr::null_mut(), None, make_small(1)), 0);
    // `erl_drv_send_term`: an explicit destination.
    assert_eq!(
        host.send_term(std::ptr::null_mut(), Some(recipient.to_eterm()), make_small(2)),
        0
    );
    // A destination that is not a pid of this node: the C's refusal, never a misdelivery.
    assert_eq!(
        host.send_term(std::ptr::null_mut(), Some(make_small(3)), make_small(4)),
        0
    );

    let events = fx.registry.borrow_mut().take_driver_terms();
    assert_eq!(
        events.len(),
        2,
        "the unresolvable destination is dropped: {events:?}"
    );
    match &events[0] {
        PortEvent::Term {
            port: source,
            to,
            term,
            owner,
        } => {
            assert_eq!(*source, port);
            assert_eq!(*to, None, "`None` = the port owner");
            assert_eq!(*term, make_small(1));
            assert_eq!(*owner, OWNER);
        }
        other => panic!("expected a term event, got {other:?}"),
    }
    match &events[1] {
        PortEvent::Term { to, term, .. } => {
            assert_eq!(*to, Some(recipient));
            assert_eq!(*term, make_small(2));
        }
        other => panic!("expected a term event, got {other:?}"),
    }
    // Draining empties the queue: a second round reports nothing.
    assert!(fx.registry.borrow_mut().take_driver_terms().is_empty());
}

/// The port pump surfaces a driver-emitted term as its own event (it is one of the two
/// producers [`PortRegistry::pump`] harvests, next to `driver_output`'s bytes).
#[test]
fn the_pump_reports_a_driver_emitted_term() {
    use beam_abi::DriverHost;

    let fx = fixture();
    let (mut host, port) = driver_term_fixture(&fx, OWNER);
    assert_eq!(host.send_term(std::ptr::null_mut(), None, make_small(7)), 0);

    let events = fx
        .registry
        .borrow_mut()
        .pump(Duration::from_millis(1))
        .expect("pump");
    assert!(
        events.iter().any(|event| matches!(
            event,
            PortEvent::Term { port: p, term, .. } if *p == port && *term == make_small(7)
        )),
        "the pump must report the driver's term: {events:?}"
    );
    // Handed out exactly once.
    assert!(fx.registry.borrow_mut().take_driver_terms().is_empty());
}
/// A name a driver interned inside a callback lands in the **live** atom table at the port
/// boundary — the registry half of the deferred-commit rule.
///
/// A driver host cannot mint an atom inline (the live table is borrowed for the whole callback
/// and this crate is `#![deny(unsafe_code)]`), so it hands the driver a **predicted** index and
/// records the name on its [`driver::DriverAtoms`] handle. [`PortRegistry::commit_driver_atoms`]
/// is the only thing that makes that index name anything: without the call the name stays
/// unresolvable, which is what the first half pins. The second half pins the drift rule — a
/// name whose predicted index something else has already taken is *reported*, never interned at
/// a different index (which would make the driver's already-built term name a different atom).
#[test]
fn a_driver_interned_name_lands_in_the_live_atom_table_at_the_port_boundary() {
    use beam_abi::DriverHost;
    // The table is uniquely owned again by the time the commit runs (the host is dropped
    // below), so `Rc::make_mut` hands the commit the *live* table rather than a copy of it.
    let mut table = AtomTable::new();
    let existing = table.intern("existing");
    let mut atoms = Rc::new(table);
    let (mut host, meta) = driver_term_host(&atoms, OWNER);
    let pending = meta.driver_atoms.clone().expect("the pending handle");
    // A name the table already holds keeps its shared index and is never recorded: only a
    // *new* name has to land at a predicted one.
    assert_eq!(host.intern_atom("existing"), Some(make_atom(existing)));
    assert!(pending.is_empty());
    // A fresh name is predicted at the table's next index and names nothing yet.
    let fresh = host
        .intern_atom("driver_fresh")
        .expect("interning always answers");
    let predicted = atom_val(fresh);
    assert_eq!(predicted, beam_term::Atom::from_raw(atoms.len() as u32));
    assert_eq!(
        atoms.lookup("driver_fresh"),
        None,
        "unresolvable before the boundary"
    );
    // Hand the host away: nothing but this test holds the table now.
    drop(host);
    let registry = Rc::new(RefCell::new(PortRegistry::new().expect("a port registry")));
    let _port = registry
        .borrow_mut()
        .register(
            Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
            meta,
        )
        .expect("a registered driver port");
    // The boundary: the port layer lands the predicted name in the live table.
    let live = Rc::make_mut(&mut atoms);
    assert_eq!(registry.borrow_mut().commit_driver_atoms(live), Vec::new());
    assert_eq!(
        live.name(predicted),
        Some("driver_fresh"),
        "the commit must land the name at exactly its predicted index"
    );
    assert_eq!(live.lookup("driver_fresh"), Some(predicted));
    assert_eq!(
        live.lookup("existing"),
        Some(existing),
        "the known name is untouched"
    );
    // Landed once: nothing is left in the window, so a second boundary commits nothing.
    assert!(pending.is_empty());
    assert_eq!(registry.borrow_mut().commit_driver_atoms(live), Vec::new());

    // The drift half: something else interns between the driver's prediction and the boundary,
    // so the predicted index is taken by a different name.
    let mut atoms = Rc::new(AtomTable::new());
    let (mut host, meta) = driver_term_host(&atoms, OWNER);
    let pending = meta.driver_atoms.clone().expect("the pending handle");
    assert!(
        host.intern_atom("drifted").is_some(),
        "a new name is always predicted"
    );
    drop(host);
    // The interleaving intern: the table now ends past the prediction.
    Rc::make_mut(&mut atoms).intern("interleaved");
    let registry = Rc::new(RefCell::new(PortRegistry::new().expect("a port registry")));
    let port = registry
        .borrow_mut()
        .register(
            Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
            meta,
        )
        .expect("a registered driver port");
    let live = Rc::make_mut(&mut atoms);
    assert_eq!(
        registry.borrow_mut().commit_driver_atoms(live),
        vec![(port, "drifted".to_string())],
        "a drifted name is reported, never silently dropped"
    );
    assert_eq!(
        live.lookup("drifted"),
        None,
        "a drifted name is never interned at an index that names something else"
    );
    assert!(pending.is_empty(), "a dropped name leaves the window");
}

/// A driver-emitted term is delivered into its recipient's mailbox by
/// [`Runtime::deliver_port_messages`]: the term itself (never a `{Port, Term}` wrapper), to the
/// port's owner for `erl_drv_output_term` and to the named pid for `erl_drv_send_term`.
///
/// The term was built on the **driver host's** arena, so it is copied onto the runtime's
/// pointer-stable heap before delivery — the mailbox payload is a fresh copy, not the driver's
/// own words (see [`RuntimeContext::stabilize_driver_term`]).
#[test]
fn a_driver_emitted_term_is_delivered_into_the_recipients_mailbox() {
    use beam_abi::DriverHost;

    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
    let owner = table.spawn_default().expect("the port's owner");
    let other = table.spawn_default().expect("an explicit destination");

    // A two-element tuple built on the *host's* arena, so the delivered copy proves the term
    // was stabilised off that heap rather than shared. A fresh `AtomTable` is fine: nothing in
    // this term is an atom.
    let atoms = Rc::new(AtomTable::new());
    let (mut host, meta) = driver_term_host(&atoms, owner);
    let mut scratch = Heap::new(16).expect("a scratch heap");
    let queued = scratch
        .tuple(&[make_small(1), make_small(2)])
        .expect("a small tuple");
    assert_eq!(host.send_term(std::ptr::null_mut(), None, queued), 0);
    assert_eq!(
        host.send_term(std::ptr::null_mut(), Some(other.to_eterm()), queued),
        0
    );

    let port = rt
        .ports
        .borrow_mut()
        .register(
            Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
            meta,
        )
        .expect("a registered driver port");

    let delivered = rt
        .deliver_port_messages(&mut table, Duration::from_millis(1))
        .expect("deliver");
    assert_eq!(delivered, 2, "both terms are delivered");

    // The owner got the `erl_drv_output_term` term; the explicit destination got its own.
    let in_owner = table
        .get(owner)
        .expect("the owner")
        .mailbox()
        .peek()
        .expect("a message")
        .payload();
    assert_eq!(
        terms::tuple_arity(in_owner).expect("the term itself is the message"),
        2,
        "in_owner={in_owner:?}"
    );
    assert_eq!(signed_value(terms::tuple_element(in_owner, 0).unwrap()), 1);
    assert_eq!(signed_value(terms::tuple_element(in_owner, 1).unwrap()), 2);
    assert_ne!(
        in_owner.raw(),
        queued.raw(),
        "the delivered term is a copy off the driver's arena"
    );

    let in_other = table
        .get(other)
        .expect("the explicit destination")
        .mailbox()
        .peek()
        .expect("a message")
        .payload();
    assert_eq!(terms::tuple_arity(in_other).unwrap(), 2, "{in_other:?}");

    // The `from` of both messages is the port itself (what send-tracing reports).
    assert_eq!(
        table.get(owner).unwrap().mailbox().peek().unwrap().from(),
        port.to_eterm()
    );
}

/// An unresolvable `erl_drv_send_term` destination is dropped at the drain, so
/// [`Runtime::deliver_port_messages`] delivers nothing for it — the C's own "not delivered".
#[test]
fn a_driver_term_with_an_unresolvable_destination_is_not_delivered() {
    use beam_abi::DriverHost;

    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
    let owner = table.spawn_default().expect("the port's owner");

    let atoms = Rc::new(AtomTable::new());
    let (mut host, meta) = driver_term_host(&atoms, owner);
    assert_eq!(
        host.send_term(std::ptr::null_mut(), Some(make_small(3)), make_small(4)),
        0
    );
    let _ = rt
        .ports
        .borrow_mut()
        .register(
            Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
            meta,
        )
        .expect("a registered driver port");

    let delivered = rt
        .deliver_port_messages(&mut table, Duration::from_millis(1))
        .expect("deliver");
    assert_eq!(delivered, 0, "the unresolvable destination is dropped");
    assert!(
        table.get(owner).expect("the owner").mailbox().is_empty(),
        "nothing reaches the owner"
    );
}

/// A term addressed to a process that is **not** the port's owner and not the evaluating
/// process is delivered to the named process, not to the owner.
#[test]
fn an_explicit_destination_overrides_the_owner() {
    use beam_abi::DriverHost;

    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
    let owner = table.spawn_default().expect("the port's owner");
    let other = table.spawn_default().expect("the destination");

    let atoms = Rc::new(AtomTable::new());
    let (mut host, meta) = driver_term_host(&atoms, owner);
    assert_eq!(
        host.send_term(std::ptr::null_mut(), Some(other.to_eterm()), make_small(42)),
        0
    );
    let _ = rt
        .ports
        .borrow_mut()
        .register(
            Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
            meta,
        )
        .expect("a registered driver port");

    let delivered = rt
        .deliver_port_messages(&mut table, Duration::from_millis(1))
        .expect("deliver");
    assert_eq!(delivered, 1);
    assert_eq!(
        table.get(other).unwrap().mailbox().peek().unwrap().payload(),
        make_small(42)
    );
    assert!(
        table.get(owner).unwrap().mailbox().is_empty(),
        "the owner receives nothing for an explicit destination"
    );
}

/// The `-eval` receive path returns a driver term addressed to the runtime's own process and
/// skips one addressed elsewhere (it can only observe its own mailbox).
#[test]
fn the_eval_receive_path_returns_only_its_own_driver_terms() {
    use beam_abi::DriverHost;

    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let me = rt.self_pid();
    let elsewhere = ProcessId::new(1234, 0);

    let atoms = Rc::new(AtomTable::new());
    let (mut host, meta) = driver_term_host(&atoms, me);
    // Addressed to the runtime's own process (the port owner).
    assert_eq!(host.send_term(std::ptr::null_mut(), None, make_small(11)), 0);
    // Addressed explicitly to the same process.
    assert_eq!(
        host.send_term(std::ptr::null_mut(), Some(me.to_eterm()), make_small(12)),
        0
    );
    // Addressed to another process: not observable here.
    assert_eq!(
        host.send_term(std::ptr::null_mut(), Some(elsewhere.to_eterm()), make_small(13)),
        0
    );
    let _ = rt
        .ports
        .borrow_mut()
        .register(
            Box::new(RecorderDriver::new("recorder", Rc::new(RefCell::new(Vec::new())))),
            meta,
        )
        .expect("a registered driver port");

    let messages = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.poll_port_messages(Duration::from_millis(1))
            .expect("poll")
    };
    let values: Vec<i64> = messages.iter().map(|term| signed_value(*term)).collect();
    assert_eq!(
        values,
        vec![11, 12],
        "only the terms addressed to the evaluating process are returned"
    );
}

/// [`RuntimeContext::stabilize_driver_term`] copies a heap term onto the runtime's
/// pointer-stable arena: the copy is at a different address and is structurally the same.
#[test]
fn stabilize_driver_term_copies_a_heap_term() {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let mut src = Heap::new(16).expect("a source heap");
    let term = src
        .tuple(&[make_small(5), make_small(6)])
        .expect("a small tuple");

    let stable = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.stabilize_driver_term(term).expect("stabilize")
    };
    assert_ne!(stable.raw(), term.raw(), "the term is copied off its source heap");
    assert_eq!(terms::tuple_arity(stable).unwrap(), 2);
    assert_eq!(signed_value(terms::tuple_element(stable, 0).unwrap()), 5);
    assert_eq!(signed_value(terms::tuple_element(stable, 1).unwrap()), 6);

    // An immediate is shared, not copied.
    let stable_immediate = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.stabilize_driver_term(make_small(9)).expect("stabilize")
    };
    assert_eq!(stable_immediate, make_small(9));
}

// --- `os:set_signal/2`'s delivery half (contract A.8) --------------------------------
//
// [`watch`] owns the mechanism (the one facility, the self-pipe, the dispositions) and
// [`PortRegistry::pump`] turns a caught signal into a [`PortEvent::Signal`]; the tests below
// prove the *delivery* [`Runtime::deliver_port_messages`] performs — the `{notify, Name}`
// 2-tuple, addressed by name to the `erl_signal_server` process with `system` as the sender
// (`signal_notify_requested`, `erts/emulator/sys/unix/sys.c:589-604`).
//
// Two properties of the subject shape every test here:
//
// * a disposition is **process-global**, so these tests serialize with the other
//   disposition-mutating tests of this crate's lib test binary on [`watch::test_guard`];
// * the signal is sent **from outside** with `kill(1)` (how an operator sends it), so it
//   arrives asynchronously — hence the bounded, poll-driven waits (each round is a real
//   [`PORT_POLL_SLICE`] poll of the runtime's own poller) rather than a single pump or a
//   fixed sleep.

/// How long a test waits for a signal it has sent to arrive and be delivered.
///
/// Generous on purpose, and only ever spent on *failure*: a passing test returns as soon as
/// the delivery lands, and a signal is normally caught within the first slice.
const SIGNAL_DELIVERY_BOUND: Duration = Duration::from_secs(5);

/// A runtime and a [`ProcessTable`] sharing the **runtime's** atom table.
///
/// The sharing is load-bearing: [`Runtime::deliver_port_messages`] resolves
/// `erl_signal_server` in the *runtime's* table and then asks `table.whereis` for the
/// process registered under that atom — a table built from an unrelated `AtomTable` would
/// hold the name under a different index and never find the server.
fn runtime_with_table() -> (Runtime, ProcessTable) {
    let mut rt = Runtime::new(RuntimeConfig::default()).expect("the default runtime");
    let table = ProcessTable::new(SignalAtoms::from_table(rt.code_server_atoms_mut()));
    (rt, table)
}

/// Registers `pid` as `erl_signal_server`, interning the name into the runtime's shared
/// table (so the runtime's own `lookup` in [`Runtime::deliver_port_messages`] finds it).
fn register_signal_server(rt: &mut Runtime, table: &mut ProcessTable, pid: ProcessId) {
    let name = rt.code_server_atoms_mut().intern("erl_signal_server");
    table
        .register(name, pid)
        .expect("`erl_signal_server` is a free name");
}

/// Drives the port half until it has delivered `wanted` messages, returning the total
/// delivered — bounded by [`SIGNAL_DELIVERY_BOUND`].
///
/// A signal's arrival is asynchronous, so the first round may legitimately observe nothing;
/// each round polls for up to [`PORT_POLL_SLICE`], making this a bounded wait on the
/// runtime's own poller rather than a race against a fixed sleep.
fn deliver_until(rt: &mut Runtime, table: &mut ProcessTable, wanted: usize) -> usize {
    let deadline = Instant::now() + SIGNAL_DELIVERY_BOUND;
    let mut delivered = 0;
    while delivered < wanted && Instant::now() < deadline {
        delivered += rt
            .deliver_port_messages(table, PORT_POLL_SLICE)
            .expect("the port poll must not fail");
    }
    delivered
}

/// Waits (bounded) until the signal self-pipe is readable — that is, until the trampoline
/// has really **caught** a signal — without consuming it (see `watch::watcher::pending`).
fn wait_until_caught() -> bool {
    let deadline = Instant::now() + SIGNAL_DELIVERY_BOUND;
    while Instant::now() < deadline {
        if watch::watcher::pending() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

/// The `{notify, Name}` payload of the first message `pid` received.
fn notify_payload(table: &ProcessTable, pid: ProcessId) -> Eterm {
    table
        .get(pid)
        .expect("the signal server is live")
        .mailbox()
        .peek()
        .expect("the signal server received a message")
        .payload()
}

/// A handled signal reaches the process registered as `erl_signal_server` as the 2-tuple
/// `{notify, Sigusr1}`, sent by the emulator itself (`system`) — and **wakes** that process,
/// which was blocked (`os:set_signal/2`'s delivery half, end to end).
#[test]
fn a_handled_signal_is_delivered_to_the_registered_signal_server() {
    let _guard = watch::test_guard();
    let (mut rt, mut table) = runtime_with_table();
    let server = table.spawn_default().expect("the signal server process");
    register_signal_server(&mut rt, &mut table, server);
    // Blocked, like a process sitting in `receive {notify, _} -> … end`: the notification
    // has to wake it through `beam_process`'s own resume-on-send path.
    table
        .get_mut(server)
        .expect("the server")
        .set_state(ProcessState::Waiting);

    let before = watch::watcher::current(Signal::SigUsr1);
    watch::install(Signal::SigUsr1, Disposition::Handle).expect("install handle on sigusr1");
    watch::watcher::send_to_self(Signal::SigUsr1);
    let delivered = deliver_until(&mut rt, &mut table, 1);
    watch::install(Signal::SigUsr1, before).expect("restore the prior sigusr1 disposition");

    assert_eq!(
        delivered, 1,
        "a handled signal must reach the registered `erl_signal_server` within \
         {SIGNAL_DELIVERY_BOUND:?}"
    );
    assert!(
        table.woken().contains(&server),
        "the blocked server must be woken by the delivery: {:?}",
        table.woken()
    );
    assert_eq!(
        table.get(server).expect("the server").state(),
        ProcessState::Runnable,
        "the wake puts the server back on the run queue"
    );

    let message = notify_payload(&table, server);
    assert_eq!(
        terms::tuple_arity(message).expect("the notification is a tuple"),
        2,
        "message={message:?}"
    );
    let tag = terms::tuple_element(message, 0).expect("element 0");
    assert_eq!(
        rt.atom_name_of(tag),
        Some("notify"),
        "element 0 is the `notify` atom"
    );
    let name = terms::tuple_element(message, 1).expect("element 1");
    assert_eq!(
        rt.atom_name_of(name),
        Some("sigusr1"),
        "element 1 names the signal with its own atom"
    );
    // The sender is the emulator (`am_system`), exactly as `signal_notify_requested` sends it
    // — not a pid, and not the port-less `undefined`.
    let from = table
        .get(server)
        .expect("the server")
        .mailbox()
        .peek()
        .expect("a message")
        .from();
    assert_eq!(rt.atom_name_of(from), Some("system"), "sender is `system`");
}

/// With **no** process registered as `erl_signal_server`, a caught signal is dropped
/// silently: no message, no error, and [`Runtime::deliver_port_messages`] reports `Ok(0)` —
/// the C's own behaviour (nothing is sent when the name has no live process).
///
/// The test first proves the signal really *was* caught (the self-pipe is readable), so the
/// `Ok(0)` is a **drop**, not a signal that never arrived.
#[test]
fn a_handled_signal_with_no_signal_server_is_dropped_silently() {
    let _guard = watch::test_guard();
    let (mut rt, mut table) = runtime_with_table();
    // A live process that is deliberately *not* the signal server.
    let bystander = table.spawn_default().expect("a bystander process");

    let before = watch::watcher::current(Signal::SigUsr1);
    watch::install(Signal::SigUsr1, Disposition::Handle).expect("install handle on sigusr1");
    watch::watcher::send_to_self(Signal::SigUsr1);
    assert!(
        wait_until_caught(),
        "the signal was never caught, so the assertion below would be vacuous"
    );

    let delivered = rt
        .deliver_port_messages(&mut table, PORT_POLL_SLICE)
        .expect("a caught signal with no recipient is not an error");
    watch::install(Signal::SigUsr1, before).expect("restore the prior sigusr1 disposition");

    assert_eq!(delivered, 0, "no `erl_signal_server` ⇒ nothing is delivered");
    assert!(
        table
            .get(bystander)
            .expect("the bystander")
            .mailbox()
            .is_empty(),
        "a notification must never reach an arbitrary process"
    );
    assert!(table.woken().is_empty(), "nothing was woken");
}

/// The watch survives its first drain: a **second** handled signal is delivered again, to
/// the same recipient, and wakes it again.
#[test]
fn a_second_handled_signal_is_delivered_again() {
    let _guard = watch::test_guard();
    let (mut rt, mut table) = runtime_with_table();
    let server = table.spawn_default().expect("the signal server process");
    register_signal_server(&mut rt, &mut table, server);

    let before = watch::watcher::current(Signal::SigUsr1);
    watch::install(Signal::SigUsr1, Disposition::Handle).expect("install handle on sigusr1");

    table
        .get_mut(server)
        .expect("the server")
        .set_state(ProcessState::Waiting);
    watch::watcher::send_to_self(Signal::SigUsr1);
    assert_eq!(
        deliver_until(&mut rt, &mut table, 1),
        1,
        "the first signal is delivered"
    );
    assert!(
        table.drain_woken().contains(&server),
        "the first delivery wakes the server"
    );

    // The registration is *not* consumed by the first round: re-block the server, send a
    // second signal, and the same watch reports it.
    table
        .get_mut(server)
        .expect("the server")
        .set_state(ProcessState::Waiting);
    watch::watcher::send_to_self(Signal::SigUsr1);
    let second = deliver_until(&mut rt, &mut table, 1);
    watch::install(Signal::SigUsr1, before).expect("restore the prior sigusr1 disposition");

    assert_eq!(second, 1, "the second signal is delivered too");
    assert!(
        table.drain_woken().contains(&server),
        "and it wakes the server again"
    );
    let mailbox = table.get(server).expect("the server").mailbox();
    assert_eq!(mailbox.len(), 2, "one message per signal");
    for message in mailbox.iter() {
        let payload = message.payload();
        assert_eq!(terms::tuple_arity(payload).expect("a tuple"), 2);
        assert_eq!(
            rt.atom_name_of(terms::tuple_element(payload, 0).expect("element 0")),
            Some("notify")
        );
        assert_eq!(
            rt.atom_name_of(terms::tuple_element(payload, 1).expect("element 1")),
            Some("sigusr1")
        );
        assert_eq!(rt.atom_name_of(message.from()), Some("system"));
    }
}

/// `ignore` and `default` produce no notification and leave the registry with **nothing to
/// poll**: no trampoline is installed, so the self-pipe can never become readable and the
/// runtime must not keep sleeping on it.
///
/// Only `ignore` is actually *delivered* — the kernel discards it. `default` is never
/// exercised with a real signal: `sigusr1`'s default action terminates the process, so a test
/// that sent one would kill the whole test binary.
#[test]
fn ignore_and_default_leave_nothing_to_poll_and_notify_nobody() {
    let _guard = watch::test_guard();
    let (mut rt, mut table) = runtime_with_table();
    let server = table.spawn_default().expect("the signal server process");
    register_signal_server(&mut rt, &mut table, server);

    let before = watch::watcher::current(Signal::SigUsr1);
    for disposition in [Disposition::Ignore, Disposition::Default] {
        watch::install(Signal::SigUsr1, disposition).expect("the OS accepts the disposition");
        assert!(
            !watch::watcher::pending(),
            "{disposition:?} installs no trampoline, so nothing can be caught"
        );
        assert!(
            rt.ports.borrow().has_nothing_to_poll(),
            "{disposition:?} must leave the runtime nothing to poll"
        );
    }

    // `ignore` really is exercised against the kernel: the signal is sent and discarded.
    watch::install(Signal::SigUsr1, Disposition::Ignore).expect("install ignore on sigusr1");
    watch::watcher::send_to_self(Signal::SigUsr1);
    // Give the OS time to act on it, then show the runtime saw nothing at all.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !watch::watcher::pending(),
        "an ignored signal never reaches the trampoline"
    );
    let delivered = rt
        .deliver_port_messages(&mut table, PORT_POLL_SLICE)
        .expect("deliver");
    watch::install(Signal::SigUsr1, before).expect("restore the prior sigusr1 disposition");

    assert_eq!(delivered, 0, "an ignored signal notifies nobody");
    assert!(
        table.get(server).expect("the server").mailbox().is_empty(),
        "the signal server receives nothing"
    );
    assert!(rt.ports.borrow().has_nothing_to_poll());
}

/// The regression guard for the "don't sleep" shortcut: with **zero** live ports but a
/// registered signal watch, [`PortRegistry::has_nothing_to_poll`] is false, so the pump
/// still registers the self-pipe and **waits** on it for its slice instead of returning at
/// once (the old port-only predicate would have let a caught signal sit undelivered).
#[test]
fn a_zero_port_runtime_still_polls_a_registered_signal_watch() {
    let _guard = watch::test_guard();
    let (mut rt, mut table) = runtime_with_table();
    // No port is opened at all: the *ports* predicate is empty …
    assert!(rt.ports.borrow().is_empty(), "no port is open");
    assert!(
        !rt.ports.borrow().watch.is_registered(),
        "nothing is registered before the first round"
    );

    let before = watch::watcher::current(Signal::SigUsr1);
    watch::install(Signal::SigUsr1, Disposition::Handle).expect("install handle on sigusr1");

    // … yet a `handle` disposition means there is still something to wait on, so the pump
    // must not take its early return.
    assert!(
        !rt.ports.borrow().has_nothing_to_poll(),
        "a `handle` disposition keeps the poller awake with zero ports"
    );

    let started = Instant::now();
    let delivered = rt
        .deliver_port_messages(&mut table, PORT_POLL_SLICE)
        .expect("deliver");
    let elapsed = started.elapsed();
    watch::install(Signal::SigUsr1, before).expect("restore the prior sigusr1 disposition");

    assert_eq!(delivered, 0, "no signal has been sent in this test");
    assert!(
        rt.ports.borrow().watch.is_registered(),
        "the round registered the self-pipe with the runtime's poller"
    );
    // The wait is real and bounded by the slice: the poll blocks for it when nothing is
    // ready, where a port-only shortcut returned immediately (every other path in this test
    // is unchanged whether the round waits or not, which is why the timing *is* the guard).
    assert!(
        elapsed >= Duration::from_millis(5),
        "the zero-port pump must really poll the self-pipe, not return at once \
         (it returned after {elapsed:?})"
    );
}
