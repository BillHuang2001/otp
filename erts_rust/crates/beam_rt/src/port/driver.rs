//! The **driver C ABI host** (contract **A.4**, phase 1): load a real C `.so` driver and drive it.
//!
//! `beam_abi::driver` freezes the *shape* of the driver ABI — the `ErlDrvEntry` struct, the
//! `driver_*` / `erl_drv_*` function table ([`beam_abi::driver::api::ErlDrvApi`]) and the
//! `#[no_mangle]` host functions ([`beam_abi::driver::drv`]) — and owns the `dlopen` loader
//! ([`beam_abi::driver::loader`]). Those host functions cannot do their job alone:
//! `driver_output` must hand its bytes to *this* runtime, `erl_drv_output_term` must build its
//! term on *this* runtime's heap and intern its atoms in *this* runtime's table, and
//! `driver_connected` must answer with *this* runtime's pid. [`DriverPort`] is that host — the
//! concrete [`beam_abi::DriverHost`] behind the opaque `*mut ErlDrvPort_` a driver callback
//! receives.
//!
//! # The port handle is the environment pointer
//!
//! [`beam_abi::DriverEnv`] is the per-port environment; its `as_port` pointer **is** the
//! `ErlDrvPort` a driver callback gets, and `beam_abi`'s host functions recover it with
//! `DriverEnv::from_port`. `DriverEnv::new` erases the borrow of the host into a raw pointer (a
//! documented `mem::transmute` inside `beam_abi`), so the caller must keep the host alive **and in
//! place** for as long as the environment (and every port handle derived from it) is used. The
//! [`Driver::open_port`] accessor is the one place that invariant is stated for this crate: the
//! environment is a local of the caller's scope, and the host lives in a `Box` that never moves.
//!
//! # What phase 1 proves
//!
//! A real C driver `.so`, compiled against the system `erl_driver.h`, is `dlopen`ed, validated and
//! driven: the driver's `start` callback allocates its private state through the runtime's
//! `driver_alloc`, its `output` callback echoes through `driver_output`, and the bytes land in
//! [`DriverPort::outputs`]. The driver's `driver_*` imports are left **undefined** by the linker and
//! bind at `dlopen(RTLD_NOW)` out of the loading process's **dynamic symbol table**
//! (`build.rs`'s `-Wl,--export-dynamic`, anchored by
//! [`PortRegistry::new`](crate::port::PortRegistry::new)'s `black_box` reference to
//! [`beam_abi::driver::drv::driver_api_table`]). `tests/driver.rs` pins the mechanism with
//! `nm -D` / `readelf --dyn-syms`.
//!
//! # What is NOT wired yet (the typed gaps)
//!
//! * **`open_port({spawn_driver, Name}, …)` is not routed here.** [`crate::port::RuntimePorts`]
//!   still answers only `{spawn, Cmd}` / `{spawn_executable, Path}` / `{fd, N}`; a `spawn_driver`
//!   name is [`BeamError`](beam_common::BeamError)'s `BadArg`, never a silent no-op. Wiring it
//!   needs, beyond this host, the driver's **port lifecycle**: a `beam_io` `Driver` that owns the
//!   `ErlDrvEntry` callbacks (`output`/`control`/`timeout`/`ready_input`), turns captured
//!   `driver_output` bytes into the owning process's `{Port, {data, …}}` message, and runs
//!   `stop`/`finish` at close.
//! * **`select` / `set_timer` / `cancel_timer` / `read_timer` / `async` keep the `DriverHost`
//!   defaults** (`-1` / `None`), i.e. "unsupported": the runtime has no timer wheel or async pool
//!   wired into the host yet, so a driver that asks for one gets the C's own failure code rather
//!   than a fabricated success.
//! * **`send_term` is captured, not delivered.** The decoded term is recorded in
//!   [`DriverPort::sent`]; turning it into a port message to the owner needs the same port
//!   lifecycle as `spawn_driver` above.

use std::rc::Rc;

use libc::c_int;

use beam_abi::driver::loader::{self, DriverLibrary, DriverLoadError};
use beam_abi::driver::types::ErlDrvPort;
use beam_abi::{DriverEnv, DriverHost};
use beam_alloc::Heap;
use beam_bifs::AtomInterner;
use beam_process::ProcessId;
use beam_term::{make_atom, AtomTable, Eterm};

use crate::context::PendingInterner;

/// One loaded driver and the host its callbacks run against.
///
/// The field order is deliberate, mirroring `crate::nif`'s `NifEntry`: the loaded library drops
/// **before** the host, so the driver's `finish` callback (run from [`DriverLibrary`]'s `Drop`)
/// still sees a live host if it calls a `driver_*` function.
pub struct Driver {
    /// The `dlopen`ed library (its `Drop` runs `finish` then `dlclose`s).
    library: DriverLibrary,
    /// The host every callback of this driver draws its runtime services from.
    ///
    /// Boxed so the [`DriverEnv`] pointer handed to the driver stays valid: the host must not move
    /// while a callback holds a port handle derived from it.
    host: Box<DriverPort>,
}

impl Driver {
    /// Pairs `library` with the `host` its callbacks will run against.
    pub fn new(library: DriverLibrary, host: DriverPort) -> Driver {
        Driver {
            library,
            host: Box::new(host),
        }
    }

    /// The loaded library (the `ErlDrvEntry` plus its `start`/`output`/`control`/`stop` wrappers).
    pub fn library(&self) -> &DriverLibrary {
        &self.library
    }

    /// The host shared by every callback of this driver, for the driver's whole lifetime.
    pub fn host(&self) -> &DriverPort {
        &self.host
    }

    /// The host, mutably (the caller building the environment).
    pub fn host_mut(&mut self) -> &mut DriverPort {
        &mut self.host
    }

    /// The driver name the entry declares (`ErlDrvEntry::driver_name`).
    pub fn name(&self) -> String {
        self.library.name()
    }
}

/// A driver host for one port: the runtime services the `driver_*` / `erl_drv_*` functions need.
///
/// It owns a private term arena (the heap the term-data decoder builds its terms on), holds a
/// handle to the runtime's **one shared** [`AtomTable`] plus the deferred-commit [`PendingInterner`]
/// a *new* atom is recorded on, and answers the port queries with the owning [`ProcessId`].
///
/// See the module docs for the phase-1 scope: the output / term-data / pid services are
/// implemented, the select / timer / async services keep the seam's "unsupported" defaults.
pub struct DriverPort {
    /// The term arena the term-data decoder allocates on.
    ///
    /// Runtime-lifetime and **never reset**: an `erl_drv_output_term` result is a term — a pointer —
    /// and the driver may hold it after the callback returns (the same rule as
    /// [`crate::port::PortRegistry`]'s `port_info` arena).
    heap: Heap,
    /// A handle to the runtime's shared atom table, used to resolve (never to mint *through*) an
    /// existing atom.
    atoms: Rc<AtomTable>,
    /// The deferred-commit interner a **new** atom is recorded on (see
    /// [`DriverPort::commit_atoms`]).
    interner: PendingInterner,
    /// The process the port belongs to — what `driver_connected` / `driver_caller` report.
    owner: ProcessId,
    /// Every `driver_output` / `erl_drv_output` payload, in call order.
    outputs: Vec<Vec<u8>>,
    /// Every decoded term handed to `send_term` (`erl_drv_output_term` / `erl_drv_send_term`),
    /// with the destination (`None` = the port owner).
    sent: Vec<(Option<Eterm>, Eterm)>,
}

impl DriverPort {
    /// A host over `heap`, resolving/interning atoms against `atoms`, running as `owner`.
    pub fn new(heap: Heap, atoms: Rc<AtomTable>, owner: ProcessId) -> DriverPort {
        let interner = PendingInterner::new(atoms.len());
        DriverPort {
            heap,
            atoms,
            interner,
            owner,
            outputs: Vec::new(),
            sent: Vec::new(),
        }
    }

    /// The process the port belongs to.
    pub fn owner(&self) -> ProcessId {
        self.owner
    }

    /// Every captured `driver_output` payload, in call order.
    pub fn outputs(&self) -> &[Vec<u8>] {
        &self.outputs
    }

    /// Every decoded term a `*_output_term` / `*_send_term` call delivered: `(destination, term)`.
    pub fn sent(&self) -> &[(Option<Eterm>, Eterm)] {
        &self.sent
    }

    /// The name of `term` when it is an atom the shared table **already** holds.
    ///
    /// A name recorded on the pending interner is not resolvable until
    /// [`DriverPort::commit_atoms`] lands it, so this answers `None` for it — the honest answer,
    /// never a guessed name.
    pub fn atom_name(&self, term: Eterm) -> Option<&str> {
        if !beam_term::is_atom(term) {
            return None;
        }
        self.atoms.name(beam_term::atom_val(term))
    }

    /// Interns the names recorded by [`DriverHost::intern_atom`] into `atoms`, in order.
    ///
    /// The same deferred-commit rule as the NIF host (`crate::nif`'s `NifHostImpl`): the runtime's
    /// atom table is held immutably while a driver callback runs, so a *new* name is given its
    /// predicted index straight away and only materialised **after** the borrow is released. The
    /// caller must pass the **same** table the host's `atoms` handle refers to.
    pub fn commit_atoms(&self, atoms: &mut AtomTable) {
        self.interner.commit(atoms);
    }

    /// Builds the one per-port environment for this driver.
    ///
    /// The returned [`DriverEnv`] must stay alive — and must not be moved — for as long as any
    /// `ErlDrvPort` derived from it is used (the `ErlDrvPort` **is** the environment's own address;
    /// see the module docs). One host is one port and the host is not shared, so exactly one
    /// environment is meaningful; nothing stops a second call, but the two would define the same
    /// port.
    pub fn environment(&mut self) -> DriverEnv {
        DriverEnv::new(self)
    }

    /// Takes the term arena back out (a host is consumed once its driver is unloaded).
    pub fn into_heap(self) -> Heap {
        self.heap
    }
}

impl DriverHost for DriverPort {
    /// The term arena the term-data decoder allocates on.
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// An existing atom keeps the shared table's index; a new one is recorded on the pending
    /// interner at its predicted index (committed later by [`DriverPort::commit_atoms`]).
    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        if let Some(atom) = self.atoms.lookup(name) {
            return Some(make_atom(atom));
        }
        Some(make_atom(self.interner.intern(name)))
    }

    /// Captures the port's output bytes (`driver_output` / `erl_drv_output`). Always succeeds: the
    /// bytes are the driver's own and a capture cannot fail.
    fn output(&mut self, _port: ErlDrvPort, data: &[u8]) -> c_int {
        self.outputs.push(data.to_vec());
        0
    }

    /// Captures the decoded term (`erl_drv_output_term` / `erl_drv_send_term`). Delivery to the
    /// owner's mailbox is the port lifecycle's job (a phase-1 gap, see the module docs).
    fn send_term(&mut self, _port: ErlDrvPort, to: Option<Eterm>, term: Eterm) -> c_int {
        self.sent.push((to, term));
        0
    }

    /// `driver_connected`: the port's owning process (this runtime's own pid term).
    fn connected_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        Some(self.owner.to_eterm())
    }

    /// `driver_caller`: the process that opened the port. One [`DriverPort`] is one port opened by
    /// one process, so this is the same pid [`DriverPort::connected_pid`] answers with.
    fn caller_pid(&mut self, _port: ErlDrvPort) -> Option<Eterm> {
        Some(self.owner.to_eterm())
    }
}

/// Loads the driver shared object for `name`, trying each of `candidates` in order.
///
/// The candidate spellings are the caller's (typically the driver name as written in
/// `open_port({spawn_driver, Name}, …)`, then the explicit `<Name>.so`); the `dlopen` loader appends
/// the platform's shared-object suffix when a candidate does not already carry it
/// (`beam_abi::driver::loader::load`), so `["t", "t.so"]` resolves `<dir>/t.so` on the first and the
/// second attempt alike, and `["/abs/t"]` resolves `/abs/t.so`.
///
/// A failure is the loader's own typed [`DriverLoadError`] (its [`DriverLoadError::reason_atom`] is
/// the oracle's `{error, Reason}` atom) — never a fabricated success and never a panic. The **last**
/// candidate's error is the one reported, since it is the one that names the path actually meant.
pub fn load_driver(
    candidates: &[String],
    name: &str,
) -> std::result::Result<DriverLibrary, DriverLoadError> {
    let mut last: Option<DriverLoadError> = None;
    for candidate in candidates {
        match loader::load(candidate, name) {
            Ok(library) => return Ok(library),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        DriverLoadError::NoDriverInit(format!("no candidate paths to load driver '{name}' from"))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{atom_val, is_atom, Atom};

    /// A host over `atoms`, running as pid 1, with a small arena.
    fn host(atoms: &Rc<AtomTable>) -> DriverPort {
        DriverPort::new(
            Heap::new(256).expect("a test heap"),
            Rc::clone(atoms),
            ProcessId::new(1, 0),
        )
    }

    #[test]
    fn a_known_atom_keeps_the_shared_index_and_a_new_one_is_predicted() {
        let mut table = AtomTable::new();
        let known = table.intern("known");
        let atoms = Rc::new(table);
        let mut host = host(&atoms);

        // An existing atom keeps the shared table's index.
        assert_eq!(host.intern_atom("known"), Some(make_atom(known)));

        // A new name is predicted at `base + 0` (nothing else interning in between) and is not
        // resolvable until the caller commits it.
        let fresh = host.intern_atom("fresh").expect("interning always answers");
        assert!(is_atom(fresh));
        assert_eq!(atom_val(fresh), Atom::from_raw(atoms.len() as u32));
        assert_eq!(atoms.name(atom_val(fresh)), None);

        // A second call for the same name reuses the predicted index.
        assert_eq!(host.intern_atom("fresh"), Some(fresh));

        // Committing lands the name at exactly the predicted index.
        let mut committed = (*atoms).clone();
        host.commit_atoms(&mut committed);
        assert_eq!(committed.name(atom_val(fresh)), Some("fresh"));
        // ...and the handle's own table still does not see it: it is the caller's job to commit
        // into the *live* table (see `DriverPort::commit_atoms`).
        assert_eq!(atoms.name(atom_val(fresh)), None);
    }

    #[test]
    fn the_host_captures_output_and_answers_the_port_queries() {
        let atoms = Rc::new(AtomTable::new());
        let mut host = host(&atoms);
        assert_eq!(host.output(std::ptr::null_mut(), b"hi"), 0);
        assert_eq!(host.outputs(), &[b"hi".to_vec()]);
        assert!(host.sent().is_empty());

        // The owner answers both `driver_*` pid queries with this runtime's pid term.
        let owner = host.owner().to_eterm();
        assert_eq!(host.connected_pid(std::ptr::null_mut()), Some(owner));
        assert_eq!(host.caller_pid(std::ptr::null_mut()), Some(owner));

        // The select/timer/async services are the "unsupported" defaults in phase 1.
        assert_eq!(host.set_timer(std::ptr::null_mut(), 100), -1);
        assert_eq!(host.cancel_timer(std::ptr::null_mut()), -1);
        assert_eq!(host.read_timer(std::ptr::null_mut()), None);
        assert_eq!(
            host.r#async(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                None,
                std::ptr::null_mut(),
                None
            ),
            -1
        );
    }

    #[test]
    fn load_driver_reports_the_last_candidates_error() {
        let error = load_driver(&["/nonexistent/beam-rt-driver-test".to_string()], "t")
            .expect_err("a nonexistent object cannot load");
        assert_eq!(error.reason_atom(), "unspecified_error");
        assert!(error.text().contains("beam-rt-driver-test"), "{error:?}");
    }

    /// An empty candidate list is still a typed failure, never a panic.
    #[test]
    fn load_driver_rejects_an_empty_candidate_list() {
        let error = load_driver(&[], "t").expect_err("nothing to load");
        assert_eq!(error.reason_atom(), "no_driver_init");
    }

    #[test]
    fn the_host_hands_its_heap_back() {
        let atoms = Rc::new(AtomTable::new());
        let host = host(&atoms);
        assert!(host.into_heap().available() > 0, "the arena survives the host");
    }
}
