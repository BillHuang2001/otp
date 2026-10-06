//! The BIF execution context: a heap, the atom table, the calling process and the
//! optional runtime seams.
//!
//! A `BifFn` (see [`crate::registry`]) receives a [`BifContext`] and its argument
//! slice. The context carries exactly what a BIF may touch:
//!
//! * the destination [`beam_alloc::Heap`] (all BIF allocation goes through it —
//!   `beam_term::heap::Heap` is a test arena only, see `../beam_alloc/CONTEXT.md`);
//! * the [`AtomTable`] (atom construction never invents an index);
//! * the calling process's [`ProcessId`] — there is no ambient "current process"
//!   in the Rust core, so identity is injected here;
//! * six **optional** seams: an [`OutputSink`] (`io:format`), a [`SendHook`]
//!   (`send/2`), a [`TableRegistry`] (`ets:*`), a [`FunCaller`] (the
//!   `fun`-taking `lists`/`maps` BIFs), a [`PortHook`] (`open_port/2` and the
//!   port BIFs) and a [`DictHook`] (the `put/2`/`get/1`/`erase/1`/`get_keys/*`
//!   process-dictionary BIFs). `beam_engine` depends only on `beam_loader`, so
//!   `beam_rt` supplies the concrete implementations; an unset seam means
//!   `io:format` writes to stdout and the `send`/`ets`/`call_fun`/port/dict BIFs
//!   report `unsupported_bif`.

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_process::{PortId, ProcessId};
use beam_term::prelude::{make_atom, try_make_small};
use beam_term::{Atom, AtomTable, Eterm, NIL};

use crate::access;
use crate::ets::TableRegistry;

/// A destination for `io:format` output.
pub trait OutputSink {
    /// Writes `s` to the sink.
    ///
    /// # Errors
    ///
    /// A sink that fails (e.g. a broken stdout pipe) reports [`BeamError::Io`].
    fn write_str(&mut self, s: &str) -> Result<()>;
}

/// The default sink: the process's standard output.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdoutSink;

impl OutputSink for StdoutSink {
    fn write_str(&mut self, s: &str) -> Result<()> {
        use std::io::Write;
        let mut out = std::io::stdout();
        out.write_all(s.as_bytes()).map_err(BeamError::Io)
    }
}

/// A sink that discards everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl OutputSink for NullSink {
    fn write_str(&mut self, _s: &str) -> Result<()> {
        Ok(())
    }
}

/// An in-memory capture sink (tests and embedding).
#[derive(Debug, Default, Clone)]
pub struct BytesSink {
    buf: Vec<u8>,
}

impl BytesSink {
    /// An empty capture buffer.
    pub fn new() -> BytesSink {
        BytesSink { buf: Vec::new() }
    }

    /// The captured bytes.
    pub fn contents(&self) -> &[u8] {
        &self.buf
    }

    /// The captured bytes as a `String` (lossily decoded).
    pub fn as_str(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    /// Discards the captured bytes.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

impl OutputSink for BytesSink {
    fn write_str(&mut self, s: &str) -> Result<()> {
        self.buf.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// Delivers `term` to the process `to` (an internal pid).
///
/// The runtime supplies this over `beam_process`/`beam_message`; Milestone-1 has no
/// in-crate delivery, so the `send/2` and `"!"/2` BIFs raise `unsupported_bif`
/// when no hook is installed.
pub trait SendHook {
    /// Sends `term` to `to`.
    ///
    /// # Errors
    ///
    /// Reports a delivery failure as a [`BeamError`].
    fn send(&mut self, to: Eterm, term: Eterm) -> Result<()>;
}

/// Calls an Erlang fun from inside a BIF.
///
/// A [`BifFn`](crate::registry::BifFn) is a plain Rust function: it cannot execute
/// BEAM code itself. The `fun`-taking `lists`/`maps` BIFs (`lists:map/2`,
/// `lists:foldl/3`, `lists:sort/2`, `maps:map/2`, …) must nevertheless *call* an
/// Erlang fun, so the runtime supplies this hook over its evaluator's closure
/// table (`beam_rt` wires it to `beam_engine::ExecutionContext`'s apply-fun seam).
/// Milestone-1 has no in-crate evaluator, so those BIFs raise `unsupported_bif`
/// when no hook is installed — never a wrong result.
pub trait FunCaller {
    /// Calls `fun` with `args`, returning its result term.
    ///
    /// # Errors
    ///
    /// Propagates whatever the call raises: an argument mismatch
    /// (`badarg`/`badarity`), or a BEAM raise (`error`/`exit`/`throw`) raised by
    /// the fun body, as a [`BeamError`].
    fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm>;
}

/// Interns a new atom name into the runtime's *global* atom table.
///
/// The context only borrows the atom table immutably ([`BifContext::atoms`]),
/// because atom indices are global and owned by the runtime (`beam_rt`), not by a
/// BIF call. The `list_to_atom`/`binary_to_atom` BIFs must nonetheless be able to
/// add a *new* atom; the runtime supplies this hook over the real, mutable
/// `AtomTable`. With no hook installed those BIFs can still resolve an *existing*
/// atom ([`BifContext::intern_atom`] looks it up first) and otherwise report
/// [`BeamError::TermNotFound`] — never a fabricated index.
pub trait AtomInterner {
    /// Interns `name`, returning its [`Atom`] index (creating it if new).
    fn intern(&mut self, name: &str) -> Atom;
}

/// Creates, closes and inspects the runtime's ports.
///
/// A port *is* an OS process and a driver (`spawn`/`fd`/…) plus a port table
/// slot — the Stage-5 machinery that lives in `beam_io`/`beam_rt`, never in a BIF.
/// The `erlang:open_port/2`/`port_close/1`/`port_info/1,2` BIFs therefore route
/// every port operation through this hook; `beam_rt` supplies the concrete
/// implementation over `beam_io` and `beam_process`. Milestone-1 has no such
/// provider, so those BIFs raise `unsupported_bif` when no hook is installed —
/// never a fabricated port id and never a silently dropped close
/// (workspace `CONTEXT.md` §3).
///
/// The hook owns argument validation: it is the only side that knows which
/// `{spawn, …}`/`{spawn_executable, …}`/`{fd, …}` names and option lists its
/// driver set accepts, and it reports an invalid Name/Options as
/// [`BeamError::BadArg`].
pub trait PortHook {
    /// `open_port(Name, Options)` — creates a port and returns its id.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a Name/Options the installed drivers do not
    /// accept; a [`BeamError::Raise`] to propagate a failure to the caller.
    fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId>;

    /// `port_close(Port)` — closes `port`.
    ///
    /// # Errors
    ///
    /// Reports a close failure as a [`BeamError`].
    fn close_port(&mut self, port: PortId) -> Result<()>;

    /// The `{Item, Value}` pairs of `port_info(Port)`, in the runtime's order.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a port the runtime does not know.
    fn port_info(&mut self, port: PortId) -> Result<Vec<(Eterm, Eterm)>>;
}

/// Reads and writes the calling process's **dictionary**.
///
/// The process dictionary is a per-process key/value store owned by
/// [`beam_process::Dict`] and reached at the language level through the
/// `erlang:put/2`, `erlang:get/1`, `erlang:erase/1`, `erlang:get_keys/0` and
/// `erlang:get_keys/1` BIFs. The store is a genuine GC sub-rootset of *a*
/// process, so a BIF must never reach into it directly — `beam_rt` supplies this
/// hook over the calling process's real dictionary. Milestone-1 has no such
/// provider, so those BIFs raise `unsupported_bif` when no hook is installed
/// (never a silently fabricated value; workspace `CONTEXT.md` §3).
///
/// `None` from `put`/`get`/`erase` means **absent** and the BIF layer maps it to
/// the atom `undefined` (the C's `THE_NON_VALUE`/`am_undefined`). Keys and values
/// are stored by **raw `Eterm` word equality** — the [`beam_process::Dict`]
/// contract; the hook is responsible for copying a key/value into the process
/// heap before storing it.
pub trait DictHook {
    /// `put(Key, Value)` — stores `value` under `key`, returning the **replaced**
    /// value (`None` when the key was absent).
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn put(&mut self, key: Eterm, value: Eterm) -> Result<Option<Eterm>>;

    /// `get(Key)` — the value stored under `key`, or `None` when absent.
    ///
    /// # Errors
    ///
    /// Reports a lookup failure as a [`BeamError`].
    fn get(&mut self, key: Eterm) -> Result<Option<Eterm>>;

    /// `erase(Key)` — removes `key`, returning the value that was stored under it
    /// (`None` when absent).
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn erase(&mut self, key: Eterm) -> Result<Option<Eterm>>;

    /// `get_keys/0` — every key, in the store's order.
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn get_keys(&mut self) -> Result<Vec<Eterm>>;

    /// `get_keys(Value)` — every key whose value raw-equals `value`, in the
    /// store's order.
    ///
    /// # Errors
    ///
    /// Reports a store failure as a [`BeamError`].
    fn get_keys_for_value(&mut self, value: Eterm) -> Result<Vec<Eterm>>;
}

/// What a BIF may read and write during one call.
pub struct BifContext<'a> {
    heap: &'a mut Heap,
    atoms: &'a AtomTable,
    self_pid: ProcessId,
    output: Option<&'a mut dyn OutputSink>,
    sender: Option<&'a mut dyn SendHook>,
    tables: Option<&'a mut TableRegistry>,
    interner: Option<&'a mut dyn AtomInterner>,
    fun_caller: Option<&'a mut dyn FunCaller>,
    port: Option<&'a mut dyn PortHook>,
    dict: Option<&'a mut dyn DictHook>,
}

impl<'a> BifContext<'a> {
    /// A context over `heap`, `atoms` and the calling process `self_pid`, with no
    /// optional seams installed.
    pub fn new(heap: &'a mut Heap, atoms: &'a AtomTable, self_pid: ProcessId) -> Self {
        BifContext {
            heap,
            atoms,
            self_pid,
            output: None,
            sender: None,
            tables: None,
            interner: None,
            fun_caller: None,
            port: None,
            dict: None,
        }
    }

    /// Installs the `io:format` output sink.
    pub fn with_output(mut self, sink: &'a mut dyn OutputSink) -> Self {
        self.output = Some(sink);
        self
    }

    /// Installs the `send/2` delivery hook.
    pub fn with_sender(mut self, hook: &'a mut dyn SendHook) -> Self {
        self.sender = Some(hook);
        self
    }

    /// Installs the `ets:*` table registry.
    pub fn with_tables(mut self, reg: &'a mut TableRegistry) -> Self {
        self.tables = Some(reg);
        self
    }

    /// Installs the atom-interning hook used by `list_to_atom`/`binary_to_atom`.
    pub fn with_atom_interner(mut self, interner: &'a mut dyn AtomInterner) -> Self {
        self.interner = Some(interner);
        self
    }

    /// Installs the fun-calling hook used by the `fun`-taking `lists`/`maps` BIFs.
    pub fn with_fun_caller(mut self, caller: &'a mut dyn FunCaller) -> Self {
        self.fun_caller = Some(caller);
        self
    }

    /// Installs the port hook used by `open_port/2` and the port BIFs.
    pub fn with_port_hook(mut self, hook: &'a mut dyn PortHook) -> Self {
        self.port = Some(hook);
        self
    }

    /// Installs the process-dictionary hook used by the `put/2`/`get/1`/`erase/1`
    /// /`get_keys/*` BIFs.
    pub fn with_dict_hook(mut self, hook: &'a mut dyn DictHook) -> Self {
        self.dict = Some(hook);
        self
    }

    /// The destination heap.
    pub fn heap(&mut self) -> &mut Heap {
        &mut *self.heap
    }

    /// The atom table.
    pub fn atoms(&self) -> &AtomTable {
        self.atoms
    }

    /// The calling process's pid.
    pub fn self_pid(&self) -> ProcessId {
        self.self_pid
    }

    /// Writes `s` to the configured sink, or to stdout when none is installed.
    ///
    /// # Errors
    ///
    /// Propagates the sink's failure.
    pub fn write_output(&mut self, s: &str) -> Result<()> {
        match self.output.as_deref_mut() {
            Some(sink) => sink.write_str(s),
            None => {
                let mut sink = StdoutSink;
                sink.write_str(s)
            }
        }
    }

    /// The installed delivery hook, if any.
    ///
    /// The trait-object lifetime is spelled out (`dyn SendHook + 'a`) because a
    /// `&mut` reference is invariant over its referent: the hook handed back must
    /// stay valid for the context's own lifetime, not merely the accessor's borrow.
    pub fn sender(&mut self) -> Option<&mut (dyn SendHook + 'a)> {
        self.sender.as_deref_mut()
    }

    /// The installed table registry, if any.
    pub fn tables(&mut self) -> Option<&mut TableRegistry> {
        self.tables.as_deref_mut()
    }

    /// Calls `fun` with `args` through the installed [`FunCaller`].
    ///
    /// This is the seam the `fun`-taking `lists`/`maps` BIFs use to execute an
    /// Erlang fun (`lists:map/2`, `lists:foldl/3`, `maps:map/2`, …).
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no caller;
    /// otherwise propagates the callee's own failure.
    pub fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        match self.fun_caller.as_deref_mut() {
            Some(caller) => caller.call_fun(fun, args),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Creates a port through the installed [`PortHook`] — the seam behind
    /// `erlang:open_port/2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure (`badarg` for an unusable Name/Options).
    pub fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId> {
        match self.port.as_deref_mut() {
            Some(hook) => hook.open_port(name, options),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Closes `port` through the installed [`PortHook`] — the seam behind
    /// `erlang:port_close/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn close_port(&mut self, port: PortId) -> Result<()> {
        match self.port.as_deref_mut() {
            Some(hook) => hook.close_port(port),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The `{Item, Value}` pairs of `port` through the installed [`PortHook`] —
    /// the seam behind `erlang:port_info/1,2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn port_info(&mut self, port: PortId) -> Result<Vec<(Eterm, Eterm)>> {
        match self.port.as_deref_mut() {
            Some(hook) => hook.port_info(port),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Stores `value` under `key` in the process dictionary through the installed
    /// [`DictHook`], returning the **replaced** value (`None` when the key was
    /// absent) — the seam behind `erlang:put/2`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_put(&mut self, key: Eterm, value: Eterm) -> Result<Option<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.put(key, value),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// The value stored under `key` in the process dictionary through the
    /// installed [`DictHook`], or `None` when the key is absent — the seam behind
    /// `erlang:get/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_get(&mut self, key: Eterm) -> Result<Option<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.get(key),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Removes `key` from the process dictionary through the installed
    /// [`DictHook`], returning the value that was stored under it (`None` when the
    /// key was absent) — the seam behind `erlang:erase/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_erase(&mut self, key: Eterm) -> Result<Option<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.erase(key),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Every key of the process dictionary through the installed [`DictHook`] —
    /// the seam behind `erlang:get_keys/0`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_get_keys(&mut self) -> Result<Vec<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.get_keys(),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// Every key whose value raw-equals `value` through the installed
    /// [`DictHook`] — the seam behind `erlang:get_keys/1`.
    ///
    /// # Errors
    ///
    /// Raises `unsupported_bif` when the runtime has installed no hook; otherwise
    /// propagates the hook's own failure.
    pub fn dict_get_keys_for_value(&mut self, value: Eterm) -> Result<Vec<Eterm>> {
        match self.dict.as_deref_mut() {
            Some(hook) => hook.get_keys_for_value(value),
            None => Err(crate::error::unsupported_bif()),
        }
    }

    /// A 60-bit small integer.
    ///
    /// # Errors
    ///
    /// [`BeamError::SystemLimit`] when `v` does not fit a small integer (the
    /// rewrite has no `f64`-free bignum arithmetic in Milestone-1; real Erlang
    /// promotes to a bignum).
    pub fn int(&self, v: i64) -> Result<Eterm> {
        try_make_small(v).map_err(|_| BeamError::SystemLimit)
    }

    /// The atom `name`.
    ///
    /// # Errors
    ///
    /// [`BeamError::TermNotFound`] when `name` is not interned in the atom table.
    pub fn atom(&self, name: &str) -> Result<Eterm> {
        self.atoms
            .lookup(name)
            .map(make_atom)
            .ok_or(BeamError::TermNotFound)
    }

    /// The atom `name`, interning it through the [`AtomInterner`] hook when it is
    /// not already present.
    ///
    /// An already-interned name never needs the hook; a new name requires it.
    ///
    /// # Errors
    ///
    /// [`BeamError::TermNotFound`] when `name` is unknown and no interner is
    /// installed (the runtime has not wired one up).
    pub fn intern_atom(&mut self, name: &str) -> Result<Eterm> {
        if let Some(atom) = self.atoms.lookup(name) {
            return Ok(make_atom(atom));
        }
        match self.interner.as_deref_mut() {
            Some(interner) => Ok(make_atom(interner.intern(name))),
            None => Err(BeamError::TermNotFound),
        }
    }

    /// The atom `true` or `false`.
    pub fn boolean(&self, b: bool) -> Result<Eterm> {
        self.atom(if b { "true" } else { "false" })
    }

    /// The atom `ok`.
    pub fn ok(&self) -> Result<Eterm> {
        self.atom("ok")
    }

    /// A boxed float built on the context heap.
    ///
    /// # Errors
    ///
    /// Propagates the heap's allocation failure.
    pub fn make_float(&mut self, v: f64) -> Result<Eterm> {
        access::make_float(self.heap, v)
    }

    /// A tuple of `elems` built on the context heap.
    ///
    /// # Errors
    ///
    /// Propagates the heap's allocation failure.
    pub fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        self.heap.tuple(elems)
    }

    /// A cons cell `[car | cdr]` built on the context heap.
    ///
    /// # Errors
    ///
    /// Propagates the heap's allocation failure.
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm> {
        self.heap.cons(car, cdr)
    }

    /// A proper list of `elems` built on the context heap (`NIL` when empty).
    ///
    /// # Errors
    ///
    /// Propagates the heap's allocation failure.
    pub fn list(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        let mut acc = NIL;
        for &elem in elems.iter().rev() {
            acc = self.heap.cons(elem, acc)?;
        }
        Ok(acc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_term::{atom_val, is_list, make_small};

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok"]);
        atoms
    }

    #[test]
    fn writes_to_the_configured_sink() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut sink = BytesSink::new();
        {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            ctx.write_output("hello ").expect("captured");
            ctx.write_output("world").expect("captured");
        }
        assert_eq!(sink.contents(), b"hello world");
        assert_eq!(sink.as_str(), "hello world");
        sink.clear();
        assert!(sink.contents().is_empty());
    }

    #[test]
    fn null_sink_and_default_stdout_sink_discard_and_write() {
        let mut sink = NullSink;
        assert!(sink.write_str("dropped").is_ok());
        // `StdoutSink` writes to the process stdout; a fresh handle is enough to
        // exercise the code path without asserting on the terminal.
        let mut stdout = StdoutSink;
        assert!(stdout.write_str("").is_ok());
    }

    #[test]
    fn identity_and_atoms_accessors() {
        let mut heap = Heap::new(8).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(7, 3));
        assert_eq!(ctx.self_pid(), ProcessId::new(7, 3));
        assert!(ctx.atoms().lookup("ok").is_some());
        assert!(ctx.heap().available() > 0);
    }

    #[test]
    fn writers_build_the_expected_terms() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        assert_eq!(ctx.int(7).expect("a small int"), make_small(7));
        let ok = ctx.ok().expect("ok is seeded");
        assert_eq!(ctx.atoms().name(atom_val(ok)), Some("ok"));
        assert_eq!(
            ctx.atoms()
                .name(atom_val(ctx.boolean(true).expect("seeded"))),
            Some("true")
        );
        assert_eq!(
            ctx.atoms()
                .name(atom_val(ctx.boolean(false).expect("seeded"))),
            Some("false")
        );

        assert!(matches!(ctx.atom("nope"), Err(BeamError::TermNotFound)));
        assert!(matches!(
            ctx.int(beam_term::MAX_SMALL + 1),
            Err(BeamError::SystemLimit)
        ));
    }

    #[test]
    fn structural_writers_build_terms() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let tuple = ctx.tuple(&[make_small(1), make_small(2)]).expect("fits");
        assert!(crate::access::is_tuple(tuple));

        let list = ctx.list(&[make_small(1), make_small(2)]).expect("fits");
        assert!(is_list(list));
        assert_eq!(ctx.list(&[]).expect("empty is NIL"), NIL);
        assert!(is_list(ctx.cons(make_small(1), NIL).expect("fits")));

        let float = ctx.make_float(1.5).expect("fits");
        assert!(crate::access::is_float(float));
        assert_eq!(crate::access::float_value(float), 1.5);
    }

    /// A minimal `FunCaller` test double: it recognises a *sentinel* fun term (the
    /// small integer `1`) and answers with `Args ++ Marker` on its own heap.
    ///
    /// Milestone-1 cannot build a real `FUN_SUBTAG` term from safe Rust without an
    /// evaluator, so the tests identify "the fun" structurally — exactly what a
    /// `beam_rt` implementation does with a real closure reference.
    struct RecordingCaller {
        heap: Heap,
        calls: Vec<Vec<Eterm>>,
    }

    impl RecordingCaller {
        fn new() -> RecordingCaller {
            RecordingCaller {
                heap: Heap::new(64).expect("test heap"),
                calls: Vec::new(),
            }
        }
    }

    impl FunCaller for RecordingCaller {
        fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
            assert_eq!(fun, make_small(1), "only the sentinel fun is recognised");
            self.calls.push(args.to_vec());
            self.heap.tuple(args)
        }
    }

    #[test]
    fn call_fun_without_a_caller_is_a_typed_unsupported() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        match ctx.call_fun(make_small(1), &[make_small(2)]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected an unsupported_bif raise, got {other:?}"),
        }
    }

    #[test]
    fn call_fun_with_a_caller_delegates_with_the_arguments() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let fun = make_small(1);
        let mut caller = RecordingCaller::new();
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_fun_caller(&mut caller);
            let result = ctx
                .call_fun(fun, &[make_small(7), make_small(8)])
                .expect("the caller answers");
            assert!(crate::access::is_tuple(result));
            assert_eq!(crate::access::tuple_size(result), 2);
            assert_eq!(crate::access::tuple_element(result, 1), make_small(8));
        }
        assert_eq!(caller.calls, vec![vec![make_small(7), make_small(8)]]);
    }

    /// A minimal `PortHook` test double: it allocates `PortId::new(11)` for every
    /// open and answers a fixed pair list from `port_info`.
    struct StubPorts;

    impl PortHook for StubPorts {
        fn open_port(&mut self, _name: Eterm, _options: Eterm) -> Result<PortId> {
            Ok(PortId::new(11))
        }

        fn close_port(&mut self, _port: PortId) -> Result<()> {
            Ok(())
        }

        fn port_info(&mut self, _port: PortId) -> Result<Vec<(Eterm, Eterm)>> {
            Ok(vec![(make_small(1), make_small(2))])
        }
    }

    #[test]
    fn port_bifs_without_a_hook_are_a_typed_unsupported() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let port = PortId::new(3);

        for result in [
            ctx.open_port(make_small(1), NIL).map(|_| ()),
            ctx.close_port(port),
            ctx.port_info(port).map(|_| ()),
        ] {
            match result {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                    assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
                }
                other => panic!("expected an unsupported_bif raise, got {other:?}"),
            }
        }
    }

    #[test]
    fn port_bifs_with_a_hook_delegate() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut hook = StubPorts;
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_port_hook(&mut hook);

        assert_eq!(ctx.open_port(make_small(1), NIL).expect("the hook answers"), PortId::new(11));
        ctx.close_port(PortId::new(11)).expect("the hook closes");
        assert_eq!(
            ctx.port_info(PortId::new(11)).expect("the hook answers"),
            vec![(make_small(1), make_small(2))]
        );
    }

    /// A minimal `DictHook` test double over an in-memory pair list: it stores
    /// `(Key, Value)` pairs by raw `Eterm` equality (the `beam_process::Dict`
    /// contract).
    #[derive(Default)]
    struct StubDict {
        entries: Vec<(Eterm, Eterm)>,
    }

    impl DictHook for StubDict {
        fn put(&mut self, key: Eterm, value: Eterm) -> Result<Option<Eterm>> {
            match self.entries.iter().position(|&(stored, _)| stored == key) {
                Some(index) => Ok(Some(core::mem::replace(&mut self.entries[index].1, value))),
                None => {
                    self.entries.push((key, value));
                    Ok(None)
                }
            }
        }

        fn get(&mut self, key: Eterm) -> Result<Option<Eterm>> {
            Ok(self
                .entries
                .iter()
                .find(|&&(stored, _)| stored == key)
                .map(|&(_, value)| value))
        }

        fn erase(&mut self, key: Eterm) -> Result<Option<Eterm>> {
            match self.entries.iter().position(|&(stored, _)| stored == key) {
                Some(index) => Ok(Some(self.entries.remove(index).1)),
                None => Ok(None),
            }
        }

        fn get_keys(&mut self) -> Result<Vec<Eterm>> {
            Ok(self.entries.iter().map(|&(key, _)| key).collect())
        }

        fn get_keys_for_value(&mut self, value: Eterm) -> Result<Vec<Eterm>> {
            Ok(self
                .entries
                .iter()
                .filter(|&&(_, stored)| stored == value)
                .map(|&(key, _)| key)
                .collect())
        }
    }

    #[test]
    fn dict_accessors_without_a_hook_are_a_typed_unsupported() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        for result in [
            ctx.dict_put(make_small(1), make_small(2)).map(|_| ()),
            ctx.dict_get(make_small(1)).map(|_| ()),
            ctx.dict_erase(make_small(1)).map(|_| ()),
            ctx.dict_get_keys().map(|_| ()),
            ctx.dict_get_keys_for_value(make_small(2)).map(|_| ()),
        ] {
            match result {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                    assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
                }
                other => panic!("expected an unsupported_bif raise, got {other:?}"),
            }
        }
    }

    #[test]
    fn dict_accessors_with_a_hook_delegate() {
        let mut heap = Heap::new(16).expect("test heap");
        let atoms = seeded_atoms();
        let mut hook = StubDict::default();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_dict_hook(&mut hook);

        assert_eq!(ctx.dict_put(make_small(1), make_small(10)).expect("the hook answers"), None);
        assert_eq!(
            ctx.dict_put(make_small(1), make_small(11)).expect("the hook answers"),
            Some(make_small(10))
        );
        assert_eq!(ctx.dict_get(make_small(1)).expect("the hook answers"), Some(make_small(11)));
        assert_eq!(ctx.dict_get(make_small(2)).expect("the hook answers"), None);
        assert_eq!(ctx.dict_get_keys().expect("the hook answers"), vec![make_small(1)]);

        ctx.dict_put(make_small(2), make_small(11)).expect("the hook stores");
        assert_eq!(
            ctx.dict_get_keys_for_value(make_small(11)).expect("the hook answers"),
            vec![make_small(1), make_small(2)]
        );
        assert_eq!(ctx.dict_erase(make_small(1)).expect("the hook answers"), Some(make_small(11)));
        assert_eq!(ctx.dict_erase(make_small(1)).expect("the hook answers"), None);
    }
}
