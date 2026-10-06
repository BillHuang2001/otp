//! The runtime's port provider: the concrete [`PortHook`] behind `erlang:open_port/2`.
//!
//! `beam_bifs` owns the *port BIFs* (`open_port/2`, `port_close/1`, `port_info/1,2`) but
//! deliberately owns none of the port *machinery*: a port is an OS process plus a driver
//! plus an entry in the port table, which is runtime state (see
//! `../../beam_bifs/src/port.rs` and the `PortHook` seam in
//! `../../beam_bifs/src/context.rs`). This module is that provider — the counterpart of
//! the legacy C's `spawn` driver (`../../erts/emulator/beam/erl_bif_port.c` and
//! `../../erts/emulator/drivers/unix/spawn_driver.c`).
//!
//! # What is real, and what is not (yet)
//!
//! **Real:** `open_port({spawn, Cmd}, _)` runs `Cmd` through `/bin/sh -c` (or, for an
//! argv list, runs the program directly) exactly as the `spawn` driver does;
//! `{spawn_executable, Path}` runs `Path` directly with the `{args, …}` option; `{fd, N}`
//! registers a descriptor port; the port gets a real [`PortId`] out of [`PortTable`];
//! `port_close/1` kills and reaps the child and returns the id to the table;
//! `port_info/1,2` answers real items; and the child's **stdout is polled** — a
//! `receive {Port, {data, Data}}` reads the driver's stream back, delivering a `binary`
//! for a `binary`-stream port (a char list otherwise) through
//! [`RuntimeContext::poll_port_messages`].
//!
//! **Not yet:** the child's stdin/stderr *are* pipes and stdout is read (above), but only
//! **stdout** is polled and only **one** `Read::read` per readiness (a message per read,
//! rather than a stream coalesced per driver callback); `port_command/2` does not exist, so
//! nothing is ever written to the child's stdin; `connected` still only flips to `false` at
//! `port_close/1` rather than when the child exits by itself; and a stream read of more than
//! `ERL_ONHEAP_BINARY_LIMIT` (64) bytes on a `binary` port is a typed *unsupported* gap
//! (there is no off-heap ref-counted binary build path here yet). Wiring the poller into the
//! scheduler's wait set (rather than into the `-eval` `receive`) is the follow-up; this
//! module keeps the honest state rather than pretending an unread pipe is delivered data.
//!
//! The `exit_status` and `eof` options are *recognized and accepted but have no effect*:
//! their only observable behaviour is a `{Port, {exit_status, S}}` / `{Port, eof}` message,
//! which the poller does not yet synthesize.
//!
//! # `port_info` items (and the deliberate deviations)
//!
//! `port_info/1` answers `name`, `id`, `connected`, `os_pid`, `links`, `monitors` and
//! `stream`. Three of those differ from what erts-17.1 reports, and the difference is
//! deliberate (never a fabricated value, but not a bit-for-bit copy either):
//!
//! * `id` is the **port term** ([`PortId::to_eterm`]); the oracle reports a bare integer.
//! * `connected` is the boolean `true`/`false`; the oracle reports the *controlling
//!   process* (a pid) while a port is connected.
//! * `stream` reflects the `binary`/`list` option, which the oracle does not report as an
//!   item at all (it reports `input`/`output` byte counters instead).
//!
//! # The shared-registry shape (why the hook cannot borrow the runtime)
//!
//! [`crate::context::RuntimeContext::call_registered`] builds its `BifContext` from cheap
//! clones that do **not** borrow the [`Runtime`](crate::Runtime), because the installed
//! fun-caller bridge needs `&mut Runtime` at the same time. The port state therefore
//! travels the same way the ETS tables do: [`Runtime::ports`](crate::Runtime) is an
//! `Rc<RefCell<PortRegistry>>`, [`RuntimePorts`] holds a cheap clone of that `Rc` plus a
//! clone of the shared atom-table handle, and a nested borrow is reported as a typed
//! capability gap instead of panicking (the crate is `#![deny(unsafe_code)]` and never
//! panics on runtime data).
//!
//! # `unsafe`-free
//!
//! Everything here is safe Rust: `std::process::Command` starts the child, the
//! `beam_process` port table hands out ids, and terms are built through
//! `beam_term`/`beam_engine::terms`.

use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_bifs::PortHook;
use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_io::{Interest, Poller};
use beam_process::{PortId, PortTable, ProcessId};
use beam_term::{
    atom_val, is_atom, is_small, make_atom, make_small, signed_value, try_make_small, AtomTable,
    Eterm, NIL,
};

use crate::context::RuntimeContext;
use crate::error;

/// The size in term words of the arena a port's `port_info/1` result is built on.
///
/// The arena is **never reset** (see [`PortRegistry::heap`]): a `port_info/1` result is a
/// term, and a term is a pointer, so its cells must outlive the call that produced it.
const PORT_HEAP_WORDS: usize = 4_096;

/// The atom vocabulary `port_info/1,2` can emit.
///
/// [`PortRegistry::info`] resolves each of these through the runtime's **one shared**
/// [`AtomTable`] and reports [`BeamError::TermNotFound`] rather than inventing an index —
/// so the runtime's boot seed ([`crate::boot::seed_core_atoms`]) must contain them. This
/// is the single source of truth for that list, exactly like [`crate::info::INFO_ATOMS`]
/// for the introspection surface (`true`/`false`/`undefined`/`process` are already part of
/// [`crate::boot::CORE_ATOMS`], and interning is idempotent).
///
/// The names are resolved **through** the hook's [`Rc`] handle but never interned
/// *through* it: the handle is a clone of the shared table, so `Rc::make_mut` would clone
/// it and mint an index the runtime's own table does not have — the term would then not
/// resolve (see [`RuntimePorts::atoms`]).
pub const PORT_INFO_ATOMS: &[&str] = &[
    // The item names.
    "name",
    "id",
    "connected",
    "os_pid",
    "links",
    "monitors",
    "stream",
    // The `stream` values.
    "binary",
    "list",
    // The shape of a `monitors` entry.
    "process",
];

/// Which `Name` the `open_port/2` call used — the driver that backs the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortName {
    /// `{spawn, Cmd}`: run `Cmd` through `/bin/sh -c` (or, for an argv list, directly).
    Spawn,
    /// `{spawn_executable, Path}`: run `Path` directly with the `args` option.
    SpawnExecutable,
    /// `{fd, N}`: a descriptor the Erlang side already owns.
    Fd,
}

/// The Erlang-side representation of a port's stream: the `binary`/`list` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    /// `binary` — the driver would deliver `{Port, {data, Bin}}`.
    Binary,
    /// `list` — the driver would deliver `{Port, {data, Str}}` (the `erl` default).
    List,
}

impl StreamType {
    /// The atom `port_info(Port, stream)` answers with.
    pub fn atom_name(self) -> &'static str {
        match self {
            StreamType::Binary => "binary",
            StreamType::List => "list",
        }
    }
}

/// A live port: the spawned OS process plus the metadata `port_info/1,2` reports.
///
/// A port is a `!Send` value owned by the single-threaded runtime. The child handle is
/// kept so `port_close/1` can kill and reap it.
#[derive(Debug)]
pub struct LivePort {
    /// Which driver backs this port.
    name: PortName,
    /// The raw command line (a shell command, or an argv join) or path — the
    /// `port_info(Port, name)` value.
    command: String,
    /// The child process; absent for a `{fd, N}` port.
    child: Option<Child>,
    /// The child's read end of the port stream.
    ///
    /// Taken out of the child after the spawn (`child.stdout.take()`) so a `Read` can be
    /// performed on it directly; `None` for a `{fd, N}` port (which owns no stream of
    /// ours) and after the pipeline reaches EOF (dropping the handle). A message is
    /// delivered by reading this handle — see [`RuntimeContext::poll_port_messages`].
    stdout: Option<ChildStdout>,
    /// The OS process id; absent for a `{fd, N}` port.
    os_pid: Option<u32>,
    /// `port_info(Port, connected)`. Tracks the *port*, not the child: nothing polls the
    /// child yet, so an independently exited child does not flip this (see the module
    /// docs).
    connected: bool,
    /// The `binary`/`list` option.
    stream: StreamType,
    /// The processes linked to this port. Empty in Milestone-1: nothing links a port yet.
    links: Vec<ProcessId>,
    /// The processes monitoring this port. Empty in Milestone-1: nothing monitors a port
    /// yet, and `beam_term` has no reference term to identify a monitor by.
    monitors: Vec<ProcessId>,
}

impl LivePort {
    /// Kills and reaps the child, if there is one.
    ///
    /// A child that has already exited is reaped by `try_wait` and needs no kill — so the
    /// one case where `kill` legitimately fails is never mistaken for a genuine failure,
    /// and a genuine `kill`/`wait` failure is reported rather than swallowed.
    fn close(&mut self) -> Result<()> {
        // A `{fd, N}` port owns no process: the descriptor belongs to the Erlang side, so
        // closing the port must not touch it.
        if self.name == PortName::Fd {
            return Ok(());
        }
        // A `spawn`/`spawn_executable` port is always created with a child.
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        match child.try_wait().map_err(BeamError::Io)? {
            Some(_status) => Ok(()),
            None => {
                child.kill().map_err(BeamError::Io)?;
                child.wait().map_err(BeamError::Io)?;
                Ok(())
            }
        }
    }
}

/// The runtime's port subsystem state: the id table plus the live ports.
///
/// Shared as `Rc<RefCell<PortRegistry>>` (see the module docs). It also owns the arena a
/// `port_info/1` result is built on — see [`PortRegistry::heap`].
pub struct PortRegistry {
    /// The port-number table (`beam_process` owns the numbering).
    table: PortTable,
    /// The live ports, by port number.
    live: HashMap<u64, LivePort>,
    /// The arena a `port_info/1` result is built on.
    ///
    /// `port_info/1` hands its `{Item, Value}` pairs to `beam_bifs`, which copies them into
    /// the BIF's **result term** — and a list term is a *pointer*, so the cells must stay
    /// valid after the call returns. The arena is therefore runtime-lifetime and **never
    /// reset** (the same rule as [`crate::Runtime::eval_heap`]); a pathologically long
    /// sequence of `port_info/1` calls on long command names would exhaust it, and that
    /// exhaustion surfaces as the arena's typed allocation error rather than as a dangling
    /// term.
    heap: Heap,
}

impl PortRegistry {
    /// An empty registry with its term arena.
    ///
    /// # Errors
    ///
    /// Propagates the arena's allocation failure.
    pub fn new() -> Result<PortRegistry> {
        Ok(PortRegistry {
            table: PortTable::new(),
            live: HashMap::new(),
            heap: Heap::new(PORT_HEAP_WORDS)?,
        })
    }

    /// Allocates a fresh port id.
    ///
    /// # Errors
    ///
    /// [`BeamError::SystemLimit`] when the port number space is exhausted.
    pub fn alloc(&mut self) -> Result<PortId> {
        self.table.alloc()
    }

    /// Returns `id` to the port table without touching the live map.
    pub fn release(&mut self, id: PortId) {
        self.table.release(id);
    }

    /// Whether `id` is currently allocated.
    pub fn contains(&self, id: PortId) -> bool {
        self.table.contains(id)
    }

    /// The number of **live** ports — the value `erlang:system_info(port_count)` reports.
    pub fn len(&self) -> usize {
        self.live.len()
    }

    /// Whether no port is open.
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Records `live` as the port `id`.
    pub fn insert(&mut self, id: PortId, live: LivePort) {
        self.live.insert(id.number(), live);
    }

    /// Removes the port `id`, releasing its table slot, and returns it.
    ///
    /// The slot is released even when the caller then fails to reap the child: the caller
    /// asked to close the port, and a close that reports an error must not leave the port
    /// half-registered.
    pub fn remove(&mut self, id: PortId) -> Option<LivePort> {
        let live = self.live.remove(&id.number())?;
        self.table.release(id);
        Some(live)
    }

    /// The `{Item, Value}` pairs of `port` — the `port_info/1` answer.
    ///
    /// Every item atom is resolved through `atoms` (the runtime's one shared table), and
    /// an unknown port is [`BeamError::BadArg`] — never an empty list and never a
    /// fabricated pair.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port the runtime does not know;
    /// [`BeamError::TermNotFound`] for an item atom missing from the seed (a bug, not a
    /// runtime condition — the list is [`PORT_INFO_ATOMS`]); the arena's allocation failure
    /// when a value does not fit.
    fn info(&mut self, port: PortId, atoms: &AtomTable) -> Result<Vec<(Eterm, Eterm)>> {
        let Some(live) = self.live.get(&port.number()) else {
            return Err(BeamError::BadArg);
        };
        let item = |name: &str| -> Result<Eterm> {
            atoms
                .lookup(name)
                .map(make_atom)
                .ok_or(BeamError::TermNotFound)
        };
        // `self.live` and `self.heap` are disjoint fields, so the immutable borrow of the
        // port and the mutable borrow of the arena coexist.
        let name = charlist(&mut self.heap, &live.command)?;
        let links = pid_list(&mut self.heap, &live.links)?;
        let monitors = monitor_list(&mut self.heap, atoms, &live.monitors)?;
        let os_pid = match live.os_pid {
            Some(pid) => try_make_small(i64::from(pid)).map_err(|_| BeamError::SystemLimit)?,
            // `port_info(Port, os_pid)` is `undefined` for a port without an OS process —
            // the oracle's own answer.
            None => item("undefined")?,
        };
        let connected = item(if live.connected { "true" } else { "false" })?;
        let stream = item(live.stream.atom_name())?;
        Ok(vec![
            (item("name")?, name),
            (item("id")?, port.to_eterm()),
            (item("connected")?, connected),
            (item("os_pid")?, os_pid),
            (item("links")?, links),
            (item("monitors")?, monitors),
            (item("stream")?, stream),
        ])
    }
}

/// The validated `Name` of an `open_port/2` call.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NameSpec {
    /// Which driver backs the port.
    kind: PortName,
    /// The command line (or path) `port_info(Port, name)` reports.
    command: String,
    /// The argv vector to run directly, when the name carried one (`{spawn, […]}`).
    argv: Option<Vec<String>>,
    /// The inline option list of `{spawn_executable, Path, Opts}`, if given.
    inline_options: Option<Eterm>,
}

/// The validated option list of an `open_port/2` call.
///
/// `stream` is an `Option` so that overlaying the inline options of a
/// `{spawn_executable, Path, Opts}` name with the call's own `Options` can tell "no stream
/// asked for" from "`list` asked for".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PortOptions {
    /// The `binary`/`list` stream choice.
    stream: Option<StreamType>,
    /// The `{cd, Dir}` working directory.
    cd: Option<String>,
    /// The `{env, [{Name, Value}]}` environment additions.
    env: Vec<(String, String)>,
    /// The `{args, [String]}` arguments of a `spawn_executable` port.
    args: Vec<String>,
}

impl PortOptions {
    /// The effective stream type (`list` unless `binary` was asked for — the oracle's
    /// default).
    fn stream(&self) -> StreamType {
        self.stream.unwrap_or(StreamType::List)
    }

    /// Applies `other` on top of `self`: whatever `other` explicitly set wins, and the
    /// flags accumulate. This is how the call's `Options` overlay the inline options of a
    /// `{spawn_executable, Path, Opts}` name.
    fn overlay(&mut self, other: PortOptions) {
        if other.stream.is_some() {
            self.stream = other.stream;
        }
        if other.cd.is_some() {
            self.cd = other.cd;
        }
        if !other.args.is_empty() {
            self.args = other.args;
        }
        self.env.extend(other.env);
    }
}

/// The concrete [`PortHook`]: validates `open_port/2` arguments, really starts the OS
/// process, and answers the port BIFs from the runtime's [`PortRegistry`].
///
/// One is built per BIF call (see the module docs). It owns nothing but two cheap `Rc`
/// clones, so it never borrows the [`Runtime`](crate::Runtime).
pub struct RuntimePorts {
    /// The shared port state.
    registry: Rc<RefCell<PortRegistry>>,
    /// A handle to the runtime's **one shared** atom table, used to resolve (never to
    /// mint) the `port_info/1` item atoms.
    ///
    /// Interning *through* this handle would be wrong: it is an `Rc` clone of the shared
    /// table, so `Rc::make_mut` would clone it and mint an index the runtime's own table
    /// does not have, and the handle is dropped when the call ends. The names are instead
    /// part of the boot seed ([`PORT_INFO_ATOMS`]) and only ever looked up here.
    atoms: Rc<AtomTable>,
}

impl RuntimePorts {
    /// A provider over `registry` that resolves its item atoms in `atoms`.
    pub fn new(registry: Rc<RefCell<PortRegistry>>, atoms: Rc<AtomTable>) -> RuntimePorts {
        RuntimePorts { registry, atoms }
    }

    /// Borrows the shared registry for one operation.
    ///
    /// A nested borrow (a port operation re-entered from inside a port operation) is
    /// impossible today — no port operation calls back into the runtime — but it is
    /// reported as a typed capability gap rather than panicking.
    fn registry_mut(&self) -> Result<RefMut<'_, PortRegistry>> {
        self.registry
            .try_borrow_mut()
            .map_err(|_| error::unsupported("the port table is already in use"))
    }

    /// The name of `term`, when `term` is an atom interned in the shared table.
    fn atom_name(&self, term: Eterm) -> Option<&str> {
        if !is_atom(term) {
            return None;
        }
        self.atoms.name(atom_val(term))
    }

    /// `term` as an Erlang string — a char list of bytes, a binary, or an atom's own name.
    ///
    /// Everything else is [`BeamError::BadArg`], and so is a char list that is not a byte
    /// string or whose bytes are not UTF-8 (a `String` cannot carry the latter, and
    /// silently mangling a command would be worse than rejecting it).
    fn string_value(&self, term: Eterm) -> Result<String> {
        if let Ok(bytes) = terms::binary_bytes(term) {
            return String::from_utf8(bytes.to_vec()).map_err(|_| BeamError::BadArg);
        }
        if let Ok(elements) = terms::list_elements(term) {
            let mut bytes = Vec::with_capacity(elements.len());
            for element in elements {
                if !is_small(element) {
                    return Err(BeamError::BadArg);
                }
                bytes.push(u8::try_from(signed_value(element)).map_err(|_| BeamError::BadArg)?);
            }
            return String::from_utf8(bytes).map_err(|_| BeamError::BadArg);
        }
        if let Some(name) = self.atom_name(term) {
            return Ok(name.to_string());
        }
        Err(BeamError::BadArg)
    }

    /// The `Name` of an `open_port/2` call: `{spawn, Cmd}`, `{spawn_executable, Path}`
    /// (optionally with an inline option list) or `{fd, N}`.
    ///
    /// Anything else — a bare atom, a tuple with another tag or arity, a `Cmd` that is not
    /// a string/atom/argv list, a negative or non-integer descriptor — is
    /// [`BeamError::BadArg`].
    fn name_spec(&self, name: Eterm) -> Result<NameSpec> {
        let arity = terms::tuple_arity(name)?;
        let tag = terms::tuple_element(name, 0)?;
        let tag = self.atom_name(tag).ok_or(BeamError::BadArg)?;
        match (tag, arity) {
            ("spawn", 2) => {
                let (command, argv) = self.command_spec(terms::tuple_element(name, 1)?)?;
                Ok(NameSpec {
                    kind: PortName::Spawn,
                    command,
                    argv,
                    inline_options: None,
                })
            }
            ("spawn_executable", 2) => Ok(NameSpec {
                kind: PortName::SpawnExecutable,
                command: self.string_value(terms::tuple_element(name, 1)?)?,
                argv: None,
                inline_options: None,
            }),
            ("spawn_executable", 3) => Ok(NameSpec {
                kind: PortName::SpawnExecutable,
                command: self.string_value(terms::tuple_element(name, 1)?)?,
                argv: None,
                inline_options: Some(terms::tuple_element(name, 2)?),
            }),
            ("fd", 2) => {
                let descriptor = terms::tuple_element(name, 1)?;
                // A descriptor is a non-negative integer; anything else the driver cannot
                // accept.
                if !is_small(descriptor) || signed_value(descriptor) < 0 {
                    return Err(BeamError::BadArg);
                }
                Ok(NameSpec {
                    kind: PortName::Fd,
                    command: format!("fd:{}", signed_value(descriptor)),
                    argv: None,
                    inline_options: None,
                })
            }
            _ => Err(BeamError::BadArg),
        }
    }

    /// The command of a `{spawn, Cmd}` name: a shell command line, or an argv vector.
    ///
    /// A char list / binary / atom is a command line the shell runs (`/bin/sh -c`). A list
    /// whose elements are themselves strings/atoms is an argv vector, run directly — the
    /// second form the real `spawn` driver accepts.
    fn command_spec(&self, value: Eterm) -> Result<(String, Option<Vec<String>>)> {
        if is_atom(value) || terms::binary_bytes(value).is_ok() {
            return Ok((self.string_value(value)?, None));
        }
        let elements = terms::list_elements(value)?;
        if elements.iter().all(|element| is_small(*element)) {
            // A string: the shell runs it.
            return Ok((self.string_value(value)?, None));
        }
        let mut argv = Vec::with_capacity(elements.len());
        for element in elements {
            argv.push(self.string_value(element)?);
        }
        if argv.is_empty() {
            return Err(BeamError::BadArg);
        }
        let display = argv.join(" ");
        Ok((display, Some(argv)))
    }

    /// The validated option list of an `open_port/2` call.
    ///
    /// `binary`/`list`, `{cd, Dir}`, `{env, [{Name, Value}]}` and `{args, [String]}` are
    /// honored. `exit_status` and `eof` are **recognized and accepted but have no effect**
    /// (see the module docs), and any other option — an unknown atom, or an unknown tuple —
    /// is tolerated: the real `erl` validates a longer list than Milestone-1 needs, and
    /// rejecting a driver option that simply has no effect here would be worse than
    /// ignoring it.
    ///
    /// A non-list `Options`, a non-atom/non-tuple option element, a recognized option with
    /// a malformed value (`{cd, 5}`, `{args, foo}`) or the wrong arity is
    /// [`BeamError::BadArg`] — those the drivers clearly cannot accept.
    fn port_options(&self, options: Eterm) -> Result<PortOptions> {
        let mut parsed = PortOptions::default();
        for element in terms::list_elements(options)? {
            if is_atom(element) {
                match self.atom_name(element) {
                    Some("binary") => parsed.stream = Some(StreamType::Binary),
                    Some("list") => parsed.stream = Some(StreamType::List),
                    // Recognized, accepted, and without effect until the runtime polls the
                    // port's streams (see the module docs).
                    Some("exit_status") | Some("eof") => {}
                    // Any other option atom is tolerated.
                    _ => {}
                }
                continue;
            }
            let arity = terms::tuple_arity(element)?;
            let tag = terms::tuple_element(element, 0)?;
            match (self.atom_name(tag), arity) {
                (Some("cd"), 2) => {
                    parsed.cd = Some(self.string_value(terms::tuple_element(element, 1)?)?);
                }
                (Some("env"), 2) => {
                    parsed.env = self.env_list(terms::tuple_element(element, 1)?)?;
                }
                (Some("args"), 2) => {
                    parsed.args = self.arg_list(terms::tuple_element(element, 1)?)?;
                }
                // A recognized option with the wrong arity is not a usable option.
                (Some("cd" | "env" | "args"), _) => return Err(BeamError::BadArg),
                // An unknown tuple option is tolerated.
                _ => {}
            }
        }
        Ok(parsed)
    }

    /// The `{env, [{Name, Value}]}` list: each name/value is a string or an atom.
    fn env_list(&self, term: Eterm) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for entry in terms::list_elements(term)? {
            if terms::tuple_arity(entry)? != 2 {
                return Err(BeamError::BadArg);
            }
            let name = self.string_value(terms::tuple_element(entry, 0)?)?;
            let value = self.string_value(terms::tuple_element(entry, 1)?)?;
            out.push((name, value));
        }
        Ok(out)
    }

    /// The `{args, [String]}` list: each element is a string or an atom.
    fn arg_list(&self, term: Eterm) -> Result<Vec<String>> {
        terms::list_elements(term)?
            .into_iter()
            .map(|element| self.string_value(element))
            .collect()
    }

    /// Starts the OS process `spec`/`options` describe.
    ///
    /// A `{fd, N}` port has no process at all. Everything else runs `/bin/sh -c` for a
    /// shell command line, or the argv program directly, with `{cd, Dir}` / `{env, …}`
    /// applied. The child's three standard streams are pipes: nothing reads them yet (see
    /// the module docs), but they keep the child from inheriting the emulator's own
    /// captured output.
    ///
    /// # Errors
    ///
    /// A diagnostic naming the command when the process cannot be started; the argv form
    /// with an empty vector is [`BeamError::BadArg`].
    fn spawn(&self, spec: &NameSpec, options: &PortOptions) -> Result<LivePort> {
        let live = |child: Option<Child>, os_pid: Option<u32>| {
            // Take the read end out of the child so it can be polled/read directly (a
            // `{fd, N}` port has neither).
            let mut child = child;
            let stdout = child.as_mut().and_then(|child| child.stdout.take());
            LivePort {
                name: spec.kind,
                command: spec.command.clone(),
                child,
                stdout,
                os_pid,
                connected: true,
                stream: options.stream(),
                links: Vec::new(),
                monitors: Vec::new(),
            }
        };
        if spec.kind == PortName::Fd {
            return Ok(live(None, None));
        }
        // `{spawn_executable, Path}` builds its argv from the `args` option; `{spawn,
        // [Prog, Arg…]}` already carries one; a shell command line carries none.
        let argv = match spec.kind {
            PortName::SpawnExecutable => {
                let mut argv = Vec::with_capacity(1 + options.args.len());
                argv.push(spec.command.clone());
                argv.extend(options.args.iter().cloned());
                Some(argv)
            }
            _ => spec.argv.clone(),
        };
        let mut command = match &argv {
            Some(argv) => {
                let program = argv.first().ok_or(BeamError::BadArg)?;
                let mut command = Command::new(program);
                command.args(&argv[1..]);
                command
            }
            None => {
                let mut command = Command::new("/bin/sh");
                command.arg("-c").arg(&spec.command);
                command
            }
        };
        if let Some(dir) = &options.cd {
            command.current_dir(dir);
        }
        for (key, value) in &options.env {
            command.env(key, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().map_err(|source| {
            error::message(format!(
                "open_port: cannot start {:?}: {source}",
                spec.command
            ))
        })?;
        let os_pid = child.id();
        Ok(live(Some(child), Some(os_pid)))
    }
}

impl PortHook for RuntimePorts {
    fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId> {
        // Validation first: a Name/Options the drivers cannot accept is `badarg` and must
        // not consume a port number.
        let spec = self.name_spec(name)?;
        // The call's own `Options` overlay the inline options of a
        // `{spawn_executable, Path, Opts}` name.
        let mut resolved = match spec.inline_options {
            Some(inline) => self.port_options(inline)?,
            None => PortOptions::default(),
        };
        resolved.overlay(self.port_options(options)?);

        // The id is allocated first so that a start failure cannot leak a slot, and it is
        // released again if the start fails. The registry stays borrowed across the
        // `spawn`: starting a process cannot re-enter a port operation, and a nested borrow
        // is reported as a typed error rather than panicking.
        let mut registry = self.registry_mut()?;
        let id = registry.alloc()?;
        match self.spawn(&spec, &resolved) {
            Ok(live) => {
                registry.insert(id, live);
                Ok(id)
            }
            Err(error) => {
                registry.release(id);
                Err(error)
            }
        }
    }

    fn close_port(&mut self, port: PortId) -> Result<()> {
        let mut registry = self.registry_mut()?;
        // An unknown port is `badarg` — never a silently dropped close.
        if !registry.contains(port) {
            return Err(BeamError::BadArg);
        }
        // `contains` established the port is registered, so it has a live entry; the
        // second lookup keeps the invariant checked rather than assumed.
        let Some(mut live) = registry.remove(port) else {
            return Err(BeamError::BadArg);
        };
        live.connected = false;
        live.close()
    }

    fn port_info(&mut self, port: PortId) -> Result<Vec<(Eterm, Eterm)>> {
        let atoms = Rc::clone(&self.atoms);
        let mut registry = self.registry_mut()?;
        registry.info(port, &atoms)
    }
}

impl RuntimeContext<'_> {
    /// Reads any pending port-stream output as `{Port, {data, Data}}` messages.
    ///
    /// This is the runtime's port **poller** (the legacy C's `erts_poll`/driver
    /// `ready_input` callback equivalent): it registers every live port that still has a
    /// readable stream with a [`beam_io::Poller`] and waits up to `timeout` for readiness.
    /// A readable descriptor is read **once** (a readable poll guarantees the read does not
    /// block) and its bytes become one `{Port, {data, Data}}` message — an Erlang **binary**
    /// for a `binary`-stream port, else a char list. It returns as soon as the first message
    /// has been produced, `Ok(vec![])` once `timeout` elapses, and immediately when no live
    /// port has a readable stream (an absent stream is never slept on). A read that returns
    /// `0` is EOF: the handle is dropped so a later poll cannot spin on the closed
    /// descriptor.
    ///
    /// The `data` atom is interned in the runtime's **one shared** table, and the terms are
    /// built on the runtime's pointer-stable arena, so the messages are nameable by the
    /// printer and comparable by the evaluator's `receive` clauses.
    ///
    /// # Errors
    ///
    /// The poller's own failures ([`BeamError::Io`] when the poll/register syscall or a
    /// read fails); a nested borrow of the port table is a typed capability gap rather than
    /// a panic; the arena's allocation failure when a term does not fit.
    pub(crate) fn poll_port_messages(&mut self, timeout: Duration) -> Result<Vec<Eterm>> {
        // The shared port state is cloned out so the *message* — which needs `&mut self` —
        // is never built while the registry is borrowed (the same snapshot rule `native.rs`
        // follows).
        let registry = Rc::clone(&self.runtime.ports);
        // Which live ports still have a readable stream: (port number, raw fd, stream).
        let registrations: Vec<(u64, RawFd, StreamType)> = {
            let registry = registry
                .try_borrow()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry
                .live
                .iter()
                .filter_map(|(number, live)| {
                    live.stdout
                        .as_ref()
                        .map(|stdout| (*number, stdout.as_raw_fd(), live.stream))
                })
                .collect()
        };
        // No readable stream at all ⇒ nothing to wait for: return at once, never sleep.
        if registrations.is_empty() {
            return Ok(Vec::new());
        }
        let mut poller = Poller::new()?;
        // The token indexes the poller's registration so an EOF'd descriptor can be
        // de-registered before its handle (and therefore its fd) is dropped.
        let mut registered: Vec<(u64, RawFd, StreamType, beam_io::Token)> =
            Vec::with_capacity(registrations.len());
        for (number, fd, stream) in registrations {
            let token = poller.register(fd, Interest::READABLE, number)?;
            registered.push((number, fd, stream, token));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let events = poller.poll(Some(remaining))?;
            // An empty round means the timeout elapsed with no readiness: the wait is over.
            if events.is_empty() {
                return Ok(Vec::new());
            }
            for event in events {
                // A half-close (HUP) or an error condition also means "read now": a `read`
                // then returns the remaining bytes and finally `0` at EOF.
                let ready = event.ready();
                if !(ready.readable() || ready.is_read_closed() || ready.is_error()) {
                    continue;
                }
                let number = event.key();
                let Some((fd, stream, token)) = registered
                    .iter()
                    .find(|(registered, _, _, _)| *registered == number)
                    .map(|(_, fd, stream, token)| (*fd, *stream, *token))
                else {
                    continue;
                };
                // One `Read::read` after a readable poll does not block. The bytes are
                // snapshotted into owned data and the registry borrow is dropped before the
                // term is built (see the snapshot rule above).
                let message: Option<(Vec<u8>, StreamType)> = {
                    let mut registry = registry
                        .try_borrow_mut()
                        .map_err(|_| error::unsupported("the port table is already in use"))?;
                    let Some(live) = registry.live.get_mut(&number) else {
                        continue;
                    };
                    // Move the handle out so the EOF path can leave it dropped (closing the
                    // descriptor) without aliasing the borrow.
                    let mut handle = live.stdout.take();
                    let read = match handle.as_mut() {
                        Some(stdout) => {
                            let mut buf = [0u8; 4096];
                            let count = stdout.read(&mut buf).map_err(BeamError::Io)?;
                            if count == 0 {
                                // EOF: de-register while the descriptor is still open, so a
                                // later poll cannot spin on the closed fd; `handle` then
                                // drops here and closes it.
                                poller.deregister(fd, token)?;
                                None
                            } else {
                                Some((buf[..count].to_vec(), stream))
                            }
                        }
                        None => None,
                    };
                    // Put the live handle back unless the stream reached EOF.
                    if read.is_some() {
                        live.stdout = handle;
                    }
                    read
                };
                let Some((bytes, stream)) = message else {
                    continue;
                };
                return Ok(vec![self.port_data_message(PortId::new(number), &bytes, stream)?]);
            }
        }
    }

    /// The `{Port, {data, Data}}` message of one port-stream read.
    ///
    /// `Data` is an Erlang **binary** for a `binary` stream and a char list otherwise (the
    /// `list` default, `erl`'s own). The `data` atom is interned into the runtime's one
    /// shared table, so the message is nameable by the printer and matchable by a `receive`
    /// clause.
    fn port_data_message(
        &mut self,
        port: PortId,
        bytes: &[u8],
        stream: StreamType,
    ) -> Result<Eterm> {
        let data = make_atom(self.runtime.code_server.atoms_mut().intern("data"));
        let payload = match stream {
            StreamType::Binary => terms::make_heap_binary(self, bytes)?,
            StreamType::List => {
                let mut list = NIL;
                for byte in bytes.iter().rev() {
                    list = terms::make_cons(self, make_small(i64::from(*byte)), list)?;
                }
                list
            }
        };
        let inner = terms::make_tuple(self, &[data, payload])?;
        terms::make_tuple(self, &[port.to_eterm(), inner])
    }
}

/// `text` as an Erlang string (a char list of bytes), built on `heap`.
///
/// # Errors
///
/// Propagates the arena's allocation failure.
fn charlist(heap: &mut Heap, text: &str) -> Result<Eterm> {
    let mut list = NIL;
    for byte in text.bytes().rev() {
        list = heap.cons(make_small(i64::from(byte)), list)?;
    }
    Ok(list)
}

/// `pids` as the Erlang list `[Pid]` — the `port_info(Port, links)` shape.
///
/// # Errors
///
/// Propagates the arena's allocation failure.
fn pid_list(heap: &mut Heap, pids: &[ProcessId]) -> Result<Eterm> {
    let mut list = NIL;
    for pid in pids.iter().rev() {
        list = heap.cons(pid.to_eterm(), list)?;
    }
    Ok(list)
}

/// `pids` as the Erlang list `[{process, Pid}]` — the oracle's `port_info(Port, monitors)`
/// shape.
///
/// # Errors
///
/// [`BeamError::TermNotFound`] when the `process` atom is missing from the seed (a bug);
/// the arena's allocation failure.
fn monitor_list(heap: &mut Heap, atoms: &AtomTable, pids: &[ProcessId]) -> Result<Eterm> {
    let process = atoms
        .lookup("process")
        .map(make_atom)
        .ok_or(BeamError::TermNotFound)?;
    let mut list = NIL;
    for pid in pids.iter().rev() {
        let entry = heap.tuple(&[process, pid.to_eterm()])?;
        list = heap.cons(entry, list)?;
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot;

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
    const TEST_ATOMS: &[&str] = &[
        "spawn",
        "spawn_executable",
        "fd",
        "not_a_driver",
        "cd",
        "env",
        "args",
        "packet",
        "hide",
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
        let hook = RuntimePorts::new(Rc::clone(&registry), Rc::clone(&atoms));
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
                list = self.heap.cons(*element, list).expect("a cons cell");
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
        /// The value `port_info/1` answered for `name`.
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

        // The child really started: `os_pid` is its process id.
        let os_pid = signed_value(fx.item(port, "os_pid"));
        assert!(os_pid > 0, "a spawned child has an os pid, got {os_pid}");

        // Every item atom resolves through the shared table (never a fabricated index).
        let pairs = fx.hook.port_info(port).expect("port_info/1");
        assert_eq!(pairs.len(), 7);
        for (key, _) in &pairs {
            assert!(fx.atoms.name(atom_val(*key)).is_some(), "{key:?}");
        }
        assert_eq!(text_of(fx.item(port, "name")), "true");
        assert_eq!(fx.item(port, "id"), port.to_eterm());
        assert_eq!(fx.item(port, "connected"), fx.atom("true"));
        assert_eq!(fx.item(port, "links"), NIL, "nothing links a port yet");
        assert_eq!(fx.item(port, "monitors"), NIL, "nothing monitors a port yet");
        assert_eq!(
            fx.item(port, "stream"),
            fx.atom("list"),
            "`list` is the default stream type"
        );
    }

    #[test]
    fn the_name_forms_and_their_options_are_accepted() {
        let mut fx = fixture();

        // `{spawn, Atom}` — an atom is a shell command line.
        let name = fx.name("spawn", fx.atom("not_a_driver"));
        let port = fx.hook.open_port(name, NIL).expect("{spawn, atom}");
        assert_eq!(text_of(fx.item(port, "name")), "not_a_driver");
        // `{spawn, [Prog, Arg…]}` — an argv vector, run directly (`/bin/sh` is the one
        // executable the runtime may assume: the `spawn` driver's shell command form needs
        // it too).
        let program = fx.string("/bin/sh");
        let dash_c = fx.string("-c");
        let command = fx.string("true");
        let argv = fx.list(&[program, dash_c, command]);
        let name = fx.name("spawn", argv);
        let port = fx.hook.open_port(name, NIL).expect("{spawn, argv}");
        assert_eq!(text_of(fx.item(port, "name")), "/bin/sh -c true");
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
        assert_eq!(fx.item(port, "stream"), fx.atom("binary"));

        // `{fd, N}` registers a descriptor port with no OS process.
        let name = fx.name("fd", make_small(2));
        let port = fx.hook.open_port(name, NIL).expect("{fd, 2}");
        // `os_pid` is `undefined` for a port without an OS process — the oracle's answer.
        assert_eq!(fx.item(port, "os_pid"), fx.atom("undefined"));
        assert_eq!(fx.item(port, "connected"), fx.atom("true"));
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
    fn unknown_option_atoms_and_tuples_are_tolerated() {
        let mut fx = fixture();
        let name = fx.spawn("true");
        let unknown_atom = fx.atom("hide");
        let unknown_tuple = {
            let tag = fx.atom("packet");
            fx.heap.tuple(&[tag, make_small(4)]).expect("a packet option")
        };
        let options = fx.list(&[unknown_atom, unknown_tuple]);
        let port = fx
            .hook
            .open_port(name, options)
            .expect("unknown options are tolerated, not rejected");
        // Tolerating an option never changes what the port is.
        assert_eq!(text_of(fx.item(port, "name")), "true");
        assert_eq!(fx.item(port, "stream"), fx.atom("list"));
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
        // A closed port is unknown: `port_info`/`port_close` are `badarg`, never a
        // silently accepted close.
        assert_badarg(fx.hook.close_port(port));
        assert_badarg(fx.hook.port_info(port));
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
        assert_badarg(fx.hook.port_info(never_allocated));
        assert_badarg(fx.hook.close_port(never_allocated));
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
        assert!(registry.live.is_empty());
        assert!(!registry.contains(PortId::new(0)));
    }
}
