//! The host-seam bridges installed by [`RuntimeContext::call_registered`].
//!
//! The engine's [`ExecutionContext`](super::RuntimeContext) dispatch is plain Rust, so a few
//! `beam_bifs` BIF families cannot run themselves: they need to apply a fun ([`FunBridge`]),
//! reach the running process's dictionary ([`DictBridge`]), `dlopen` a NIF ([`NifBridge`]) or
//! read a loaded module's record definitions ([`RecordBridge`]). Each bridge borrows a
//! different slice of the runtime and therefore cannot co-exist with the others, so
//! `call_registered` installs exactly one per call. [`SharedSink`] is the shared, cloneable
//! `io:format` destination every context (outer and nested) writes through, and
//! [`PendingInterner`] is the deferred, predicted-index atom interner (see the parent
//! module docs for the full rationale).

use std::cell::RefCell;
use std::rc::Rc;

use beam_bifs::{
    AtomInterner, DictHook, FunCaller, LocalNode, OutputSink, RecordDefinitionView,
    RecordFieldView, RecordSource,
};
use beam_common::{BeamError, Result};
use beam_engine::ExecutionContext;
use beam_loader::Module;
use beam_process::{ProcessId, ProcessTable};
use beam_term::{Atom, AtomTable, Eterm};

use crate::codeserver::CodeServer;
use crate::error;
use crate::nif::{NifEntry, NifHostImpl};
use crate::runtime::Runtime;

use super::RuntimeContext;

/// A cloneable, shared `io:format` destination.
///
/// A [`RuntimeContext`] must be able to hand the **same** sink to a nested context when
/// a `fun`-taking BIF applies its fun (see [`FunBridge`]) while the outer call still
/// holds it — but a borrowed `&mut dyn OutputSink` cannot be duplicated. The sink
/// therefore lives behind an `Rc`-shared, interior-mutable cell: every context owns its
/// own [`SharedSink`] *value* wrapping a clone of the same `Rc`, and the (single-threaded)
/// writes are serialized by the `RefCell`, so there is no Rust aliasing — this crate is
/// `#![deny(unsafe_code)]`.
///
/// The shared cell is a `dyn SinkCell` rather than a `RefCell<dyn OutputSink>` on purpose:
/// it keeps `SharedSink<'a>` **covariant** in `'a`, so a nested context can reuse a sink
/// borrowed for a *longer* lifetime — exactly what [`FunBridge::call_fun`] does when it
/// reborrows the runtime for a short nested call.
#[derive(Clone)]
pub(crate) struct SharedSink<'a>(Rc<dyn SinkCell + 'a>);

/// The interior-mutable cell behind a [`SharedSink`].
///
/// A trait (not a generic struct) so the object type carries the sink's lifetime — and
/// nothing else — which is what makes [`SharedSink`] covariant in `'a` (a `RefCell` over a
/// lifetime-parameterized type would be invariant).
trait SinkCell {
    /// Writes `s` through the borrowed sink.
    fn write(&self, s: &str) -> Result<()>;
}

impl<'a> SinkCell for RefCell<&'a mut dyn OutputSink> {
    fn write(&self, s: &str) -> Result<()> {
        let mut sink = self.borrow_mut();
        sink.write_str(s)
    }
}

impl<'a> SharedSink<'a> {
    /// Wraps a borrowed sink into a shared handle.
    pub(super) fn new(sink: &'a mut dyn OutputSink) -> SharedSink<'a> {
        SharedSink(Rc::new(RefCell::new(sink)))
    }
}

impl OutputSink for SharedSink<'_> {
    fn write_str(&mut self, s: &str) -> Result<()> {
        self.0.write(s)
    }
}

/// The `beam_bifs` fun-calling seam: runs a fun term for a `fun`-taking BIF.
///
/// The `fun`-taking `lists`/`maps` BIFs (`lists:map/2`, `lists:foldl/3`, `lists:sort/2`,
/// `maps:map/2`, …) are plain Rust functions and cannot execute BEAM code themselves, so
/// they call their fun through [`BifContext::call_fun`] — this is the concrete
/// [`FunCaller`] the runtime installs. It borrows the whole runtime (the caller's
/// `BifContext` holds only locals and cheap clones, so the runtime is free), builds a
/// nested [`RuntimeContext`] over it and delegates to [`ExecutionContext::apply_fun`]:
/// an `-eval` closure (`uniq` == [`crate::eval::EVAL_FUN_UNIQ`]) is applied through
/// [`crate::eval::apply_eval_closure`], and every other fun is a `.beam` fun applied
/// through `Machine::run_label` — exactly the semantics a direct `call_fun` opcode has.
///
/// It carries a clone of the caller's [`SharedSink`], so a nested `io:format` reaches the
/// caller's sink rather than stdout. It carries **no** [`TableRegistry`] of its own: the
/// outer call intentionally did not take the shared cell (see
/// [`RuntimeContext::reentrancy_capable`]), so the nested context's own
/// [`RuntimeContext::call_registered`] borrows it normally and `ets:*` inside the applied
/// fun reaches the runtime's tables.
pub(super) struct FunBridge<'a> {
    /// The runtime the fun runs on.
    pub(super) runtime: &'a mut Runtime,
    /// The caller's shared output sink (stdout when unset), cloned from the outer context.
    pub(super) output: Option<SharedSink<'a>>,
    /// The pid the outer call runs as, so a `receive` inside the applied fun resolves
    /// the *same* process's mailbox.
    pub(super) mailbox_pid: ProcessId,
    /// The outer call's externally supplied process table, if any (see
    /// [`RuntimeContext::for_process`]).
    pub(super) mailbox_table: Option<&'a mut ProcessTable>,
    /// The outer call's [`RuntimeContext::may_pump_scheduler`], inherited so the nested
    /// context can pump the scheduler exactly when the caller could.
    pub(super) may_pump_scheduler: bool,
    /// The outer call's executing-module name, inherited so a nested call keeps the same
    /// call-site identity (see [`RuntimeContext::current_module`]).
    pub(super) current_module: Option<String>,
}

impl FunCaller for FunBridge<'_> {
    fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        // A fresh nested context over the same runtime, sharing the caller's output sink
        // so a nested `io:format` reaches it. The ETS cell is left free by the outer call
        // (it is re-entrancy-capable), so the nested `call_registered` borrows it normally.
        // The mailbox identity (pid + table) is inherited, so a `receive` in the fun
        // scans the same process's mailbox as the outer call, and the scheduler-pump
        // permission is inherited with it.
        let mut ctx = RuntimeContext {
            runtime: self.runtime,
            output: self.output.clone(),
            mailbox_pid: self.mailbox_pid,
            mailbox_table: self.mailbox_table.as_deref_mut(),
            may_pump_scheduler: self.may_pump_scheduler,
            current_module: self.current_module.clone(),
        };
        ctx.apply_fun(fun, args)
    }
}

/// The `beam_bifs` process-dictionary seam: the running process's [`Dict`](beam_process::Dict).
///
/// The five process-dictionary BIFs (`erlang:put/2`, `get/1`, `erase/1`, `get_keys/0`,
/// `get_keys/1`) are plain Rust functions that cannot reach the calling process's dictionary
/// themselves, so they call it through the `BifContext` `dict_*` operations — this
/// is the concrete [`DictHook`] the runtime installs. It borrows the process table the
/// running process lives in (the runtime's own for a `-eval` context, the caller-supplied
/// one for a scheduler pass — see [`RuntimeContext::call_registered`]) and forwards each
/// operation to that process's [`Dict`](beam_process::Dict).
///
/// A pid the table does not hold is the typed capability gap [`crate::error::no_mailbox`],
/// never a panic — the same convention as the receive seam.
///
/// # Heap lifetime (the process-dictionary M1 decision)
///
/// The keys and values stored here are the argument [`Eterm`]s themselves. `beam_bifs`
/// builds **nothing** on its pooled BIF heap for a store, and Milestone-1's
/// [`beam_alloc::Heap`] pool and engine arena are pointer-stable and never reset (there is
/// no GC — see the module docs), so a stored term cannot dangle. The stronger
/// [`beam_process::Dict`] contract — every key/value *copied into the process's young heap*
/// — is **not** reachable from `beam_rt`: `Process::copy_into_heap` is `pub(crate)` to
/// `beam_process` (out of this crate's scope). Registering the term on the runtime's
/// pointer-stable arena is therefore the Milestone-1 decision; a future pass that wires the
/// collector up must make [`Dict`](beam_process::Dict) a genuine GC sub-rootset here.
pub(super) struct DictBridge<'a> {
    /// The table the running process lives in.
    pub(super) table: &'a mut ProcessTable,
    /// The running process's pid.
    pub(super) pid: ProcessId,
}

impl DictBridge<'_> {
    /// The running process, or the typed capability gap when the table does not hold it.
    fn process_mut(&mut self) -> Result<&mut beam_process::Process> {
        self.table
            .get_mut(self.pid)
            .ok_or_else(|| error::no_mailbox(self.pid))
    }
}

impl DictHook for DictBridge<'_> {
    fn put(&mut self, key: Eterm, value: Eterm) -> Result<Option<Eterm>> {
        Ok(self.process_mut()?.dict_mut().put(key, value))
    }

    fn get(&mut self, key: Eterm) -> Result<Option<Eterm>> {
        Ok(self.process_mut()?.dict().get(key))
    }

    fn erase(&mut self, key: Eterm) -> Result<Option<Eterm>> {
        Ok(self.process_mut()?.dict_mut().erase(key))
    }

    fn get_keys(&mut self) -> Result<Vec<Eterm>> {
        Ok(self.process_mut()?.dict().get_keys())
    }

    fn get_keys_for_value(&mut self, value: Eterm) -> Result<Vec<Eterm>> {
        Ok(self.process_mut()?.dict().get_keys_for_value(value))
    }
}

/// The `beam_bifs` NIF-loading seam: `dlopen`s a NIF library and registers it with the runtime.
///
/// `erlang:load_nif/2` is a plain Rust function that cannot open a shared object or bind its
/// entry points itself, so it routes every load through the [`NifLoader`](beam_bifs::NifLoader)
/// seam — this is the concrete provider the runtime installs. It needs `&mut Runtime` (to store
/// the loaded [`NifEntry`](crate::nif::NifEntry) in [`Runtime::nifs`](crate::runtime::Runtime)),
/// so [`RuntimeContext::call_registered`] installs it as its own mutually exclusive arm.
///
/// # The persistent host (why the load host is not the call host)
///
/// The host handed to [`beam_abi::nif::loader::load`] is **boxed and stored with the library**:
/// the loader erases the host borrow into a raw pointer, and the library's `unload` callback
/// (run from the library's `Drop`) later needs a host again. The box must therefore outlive the
/// library; [`crate::nif::NifEntry`] pairs them in the right drop order. A **call** to a NIF,
/// by contrast, builds its own fresh host ([`RuntimeContext::try_call_nif`]), so this long-lived
/// host is only ever the `unload` context.
///
/// The host's [`PendingInterner`] is a clone of this call's, so a name the C `load` callback
/// interns is committed (at its predicted index) together with any the BIF interns.
pub(super) struct NifBridge<'a> {
    /// The runtime whose NIF registry stores the loaded library.
    pub(super) runtime: &'a mut Runtime,
    /// A cheap handle to the shared atom table, cloned for each host.
    pub(super) atoms: Rc<AtomTable>,
    /// The in-flight deferred interner, shared with the BIF call.
    pub(super) interner: PendingInterner,
    /// `erlang:self()` as seen by the library's `load` callback.
    pub(super) self_pid: Eterm,
}

impl beam_bifs::NifLoader for NifBridge<'_> {
    fn load_nif(
        &mut self,
        module: &str,
        path: &str,
        load_info: Eterm,
    ) -> core::result::Result<(), beam_bifs::NifLoadError> {
        // The oracle's rule since OTP 20: a second `load_nif` for a module that already has a
        // library is a `reload` (upgrading needs `reload`/`upgrade` callbacks the runtime does
        // not model). Checked *before* touching the filesystem, which is the oracle's order.
        if self.runtime.nifs.borrow().is_loaded(module) {
            return Err(beam_bifs::NifLoadError::Reload(
                "NIF library already loaded (reload disallowed since OTP 20).".to_string(),
            ));
        }
        // A dedicated, long-lived host heap: the library's `unload` callback runs against this
        // host long after this call returns, so it may not use a pooled BIF heap.
        let heap = beam_alloc::Heap::new(self.runtime.bif_heap_words).map_err(|error| {
            beam_bifs::NifLoadError::Load(format!("cannot allocate the NIF host heap: {error}"))
        })?;
        let mut host = Box::new(NifHostImpl::new(
            heap,
            Rc::clone(&self.atoms),
            self.interner.clone(),
            self.self_pid,
        ));
        let lib = beam_abi::nif::loader::load(path, module, host.as_mut(), load_info)
            .map_err(crate::nif::map_load_error)?;
        self.runtime
            .nifs
            .borrow_mut()
            .insert(module.to_string(), NifEntry::new(lib, host));
        Ok(())
    }
}

/// The `beam_bifs` record-source seam: the loaded module's `Recs`-chunk definitions.
///
/// `records:get_definition/2` asks the runtime's code server for a compiled module's
/// native-record definitions (C: the code server's record table, populated from a `.beam`'s
/// `Recs` chunk at load time — `erts_record_put`, `erl_record.c:173-178`). The definitions
/// live in the [`Module`]'s [`RecordDefinition`](beam_loader::RecordDefinition) list, whose
/// record/field names are atoms of the **module's own** atom table — not of this call's
/// `BifContext` table. This bridge therefore resolves the module by name in the code server
/// and **name-resolves** each definition through that table, so the BIF never has to know
/// the loader's layout (see `beam_bifs::RecordSource`).
///
/// A module that is not loaded, or that defines no such record, answers `Ok(None)` — the
/// BIF then raises the oracle's plain `badarg` (`records_get_definition_2`,
/// `erl_record.c:1342-1403`; the code server's lookup `erts_record_find_entry` returns
/// `NULL` and the BIF raises `badarg`). A field atom the module's own table cannot resolve
/// is a typed [`BeamError::TermNotFound`] — never a fabricated name.
///
/// It borrows only the [`CodeServer`] (a `&Runtime` read), so it cannot co-exist with
/// [`FunBridge`]'s `&mut Runtime` or the other bridges —
/// [`RuntimeContext::call_registered`] installs exactly one of them.
pub(super) struct RecordBridge<'a> {
    /// The code server holding the loaded modules.
    pub(super) code_server: &'a CodeServer,
}

impl RecordSource for RecordBridge<'_> {
    fn record_definition(
        &mut self,
        module: &str,
        name: &str,
    ) -> Result<Option<RecordDefinitionView>> {
        let Some(module) = self.code_server.module(module) else {
            // Not loaded (or not a module name at all): no definition exists, which the
            // BIF reports as the oracle's `badarg`.
            return Ok(None);
        };
        let Some(record) = module.record_by_name(name) else {
            return Ok(None);
        };
        let mut fields = Vec::with_capacity(record.fields.len());
        for field in &record.fields {
            // The field name is an atom of the module's own atom table (the loader interns
            // the module's `AtU8` names there — `beam_loader::lower::build_module`), so it
            // resolves by construction; a name it cannot resolve is a typed error.
            let name = module
                .atom_table
                .name(field.name)
                .ok_or(BeamError::TermNotFound)?
                .to_string();
            fields.push(RecordFieldView {
                name,
                default: field.default,
            });
        }
        Ok(Some(RecordDefinitionView {
            is_exported: record.is_exported,
            fields,
        }))
    }
}

/// The `beam_bifs` local-node-identity seam: this runtime's fixed, non-distributed identity.
///
/// `term_to_binary/1` of an **internal** pid/port id needs the local node's name atom and
/// creation to emit `NEW_PID_EXT` (`88`)/`V4_PORT_EXT` (`120`). Milestone 1 has no
/// distribution, so the identity is the fixed `nonode@nohost` with creation `0` — the same
/// identity `beam_bifs`' own [`BifContext::local_node`] fallback answers for the runtime's
/// node (see `crate::native`). Making it explicit here means the identity does not depend on
/// the fallback's atom being present.
///
/// Both fields are `Copy` (the atom is resolved once, from the shared table, before the
/// context borrows it), so the hook needs **no** runtime borrow and can be installed in
/// every [`RuntimeContext::call_registered`] arm.
pub(super) struct NodeIdentity {
    /// The node-name atom, resolved from the shared table at the start of the call.
    pub(super) node: Option<Atom>,
}

impl LocalNode for NodeIdentity {
    fn local_node(&mut self) -> Option<(Atom, u32)> {
        // Creation `0`: the value of an undistributed node (the C's `erts_creation` of a
        // node that never connected anywhere).
        self.node.map(|node| (node, 0))
    }
}

/// Interns atom names for the duration of one BIF call, predicting their indices.
///
/// A [`BifContext`] borrows the shared [`AtomTable`](beam_term::AtomTable) immutably for
/// index resolution while the interner must add names to it; this crate is
/// `#![deny(unsafe_code)]`, so the interner cannot hold a coexisting mutable borrow of the
/// same table. Interning is **append-only** and the runtime is **single-threaded**, so a
/// name added during one call always lands at `base + <names recorded so far>` — the
/// interner records the names and predicts that index; the caller commits them, in order,
/// after the call. A name seen twice in one call is deduplicated and returns the same index.
///
/// # Why the inner state is shared (`Rc<RefCell<…>>`)
///
/// One `erlang:load_nif/2` call has **two** new-atom writers: the BIF layer (through
/// [`BifContext`]'s [`AtomInterner`]) and the NIF host the load runs against
/// ([`NifHostImpl`](crate::nif::NifHostImpl)'s `enif_make_atom`). They may interleave, so two
/// independent predicted-index lists would hand out colliding indices. A cloneable handle over
/// one shared list makes both writers agree on `base + <names recorded>`, and the single
/// [`PendingInterner::commit`] by the caller lands every name at exactly its predicted index.
/// The handle is **not** shared across calls: each call gets a fresh one rooted at the table's
/// current length.
#[derive(Clone)]
pub(crate) struct PendingInterner {
    /// The shared base + recorded names (one allocation per call).
    inner: Rc<RefCell<PendingInternerInner>>,
}

/// The mutable half of a [`PendingInterner`].
struct PendingInternerInner {
    /// The shared table's length when the call began: the first new name's index.
    base: usize,
    /// Names interned during this call, in order (deduplicated).
    names: Vec<String>,
}

impl PendingInterner {
    /// A pending interner rooted at `base` (the current shared-table length).
    pub(crate) fn new(base: usize) -> PendingInterner {
        PendingInterner {
            inner: Rc::new(RefCell::new(PendingInternerInner {
                base,
                names: Vec::new(),
            })),
        }
    }

    /// Commits the recorded names into `atoms`, in intern order.
    ///
    /// Must be called once the BIF/host has returned and released its borrow of the table, so
    /// each name lands at exactly the index the interner predicted.
    pub(crate) fn commit(&self, atoms: &mut AtomTable) {
        for name in self.inner.borrow_mut().names.drain(..) {
            atoms.intern(&name);
        }
    }
}

impl AtomInterner for PendingInterner {
    fn intern(&mut self, name: &str) -> Atom {
        let mut inner = self.inner.borrow_mut();
        if let Some(offset) = inner.names.iter().position(|pending| pending == name) {
            // A duplicate within this call reuses the predicted index.
            return Atom::from_raw((inner.base + offset) as u32);
        }
        let index = inner.base + inner.names.len();
        inner.names.push(name.to_string());
        Atom::from_raw(index as u32)
    }
}

/// The `FunDescriptor` of the lambda `(uniq, index)` in `module`, if it defines one.
pub(super) fn descriptor_of(
    module: &Module,
    uniq: u64,
    index: u32,
) -> Option<beam_loader::FunDescriptor> {
    module
        .funs()
        .iter()
        .find(|descriptor| u64::from(descriptor.uniq) == uniq && descriptor.index == index)
        .copied()
}
