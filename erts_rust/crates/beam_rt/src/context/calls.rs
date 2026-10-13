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

//! The registered-BIF call path: the memoized dispatch answer, the bridge selection and the
//! NIF short-circuit.
//!
//! # One context, three dispatch routes
//!
//! The engine routes `call_ext*` **and** `apply`/`apply_last` through
//! [`ExecutionContext::call_bif`] — unless the host can run the callee in-machine, which the
//! [`ExecutionContext::resolve_external_call`] seam reports — and `call_fun`/`call_fun2`
//! through the in-machine [`ExecutionContext::resolve_local_fun`] seam **first**, falling back
//! to the value-returning [`ExecutionContext::apply_fun`]. So [`RuntimeContext::call_bif`]
//! serves, in order:
//!
//! 1. the **runtime-native floor** ([`crate::native`]: `halt`, `display`,
//!    `io:put_chars`, `erlang:apply`, `init:get_argument`);
//! 2. the **`beam_bifs` registry** (the `bif.tab` surface);
//! 3. an **external module call**, loaded on demand from the code path through the
//!    [`CodeServer`](crate::CodeServer) and run with `self` as the context — the
//!    recursion that makes the runtime a real multi-module host.
//!
//! # The shared atom table
//!
//! A [`BifContext`] takes *one* [`AtomTable`](beam_term::AtomTable). The engine resolves
//! atoms — including the ones inside `Module` code — against the **loaded module's**
//! table (`beam_engine`'s `cmp(.., &module.atom_table)`). Because every module the
//! [`CodeServer`](crate::CodeServer) loads is interned into the runtime's **one shared**
//! table, handing the BIFs that same table is exactly right: an atom a BIF builds is an
//! index the engine can compare and the printer can name, for **every** loaded module.
//!
//! # Interning new atoms (the predicted-index commit)
//!
//! Some BIFs (`binary_to_term/1`, `list_to_atom/1`, …) must be able to add a **new** atom
//! to that same shared table, but the [`BifContext`] only borrows it *immutably* for index
//! resolution. Since this crate is `#![deny(unsafe_code)]`, the interner cannot hold a
//! second, mutable borrow of the one table. The [`AtomInterner`](beam_bifs::AtomInterner)
//! hook solves this without aliasing: [`PendingInterner`] records the names a BIF interns
//! during a call and **predicts** each index as `base + <names already recorded>` (interning
//! is append-only and the runtime is single-threaded, so the prediction is exact); after the
//! BIF returns — releasing the table borrow — [`RuntimeContext::call_registered`] commits the
//! recorded names into the shared table in the same order, so each lands at exactly the
//! predicted index. A name seen twice in one call is deduplicated and gets the same index.

use std::rc::Rc;

use beam_abi::nif::loader::NifCallError;
use beam_bifs::BifContext;
use beam_common::Result;
use beam_process::ProcessTable;
use beam_term::Eterm;

use crate::error;
use crate::nif::{NifHostImpl, RuntimeNifServices};
use crate::port::RuntimePorts;

use super::bridges::PendingInterner;
use super::bridges::{DictBridge, FunBridge, NifBridge, NodeIdentity, RecordBridge, SendBridge};
use super::dispatch::DispatchHooks;
use super::fun_identity::FunIdentityBridge;
use super::memory_hook::RuntimeMemory;
use super::module_info::ModuleInfoBridge;
use super::persistent_term::PersistentTermBridge;
use super::process_bridge::ProcessBridge;
use super::runtime_context::RuntimeContext;
use super::runtime_hooks::RuntimeHooksBridge;

impl<'a> RuntimeContext<'a> {
    /// Dispatches a `beam_bifs`-registered BIF over a [`BifContext`] built from the
    /// runtime's shared state and this context's output sink.
    ///
    /// The context is handed a [`PendingInterner`] so a BIF that builds a *new* atom
    /// (`binary_to_term/1`, `list_to_atom/1`, …) can intern it into the one shared table
    /// without aliasing it: the interner records the names, the BIF runs against the
    /// immutable table, and the names are committed (in order, at their predicted
    /// indices) once the BIF has returned and released the borrow — see the module docs.
    ///
    /// # Why every resource is a local
    ///
    /// The `fun`-taking `lists`/`maps` BIFs call back into the runtime through
    /// [`FunBridge`] (the [`FunCaller`] seam), which needs `&mut Runtime`. The
    /// [`BifContext`] therefore holds **no** borrow of the runtime: the registry, the
    /// atom table and the tables are cheap `Rc` clones (the atom handle dereferences to
    /// the shared table without borrowing the server), the heap comes from the BIF heap
    /// pool, and the output sink is a shared [`SharedSink`] handle cloned out of this
    /// context (see below). The only borrow of the runtime is the bridge's.
    ///
    /// A **nested** call (inside a fun a BIF applied) re-enters here through the bridge:
    /// it takes its own heap, shares the atom table through another `Rc` handle and shares
    /// the caller's sink through a [`SharedSink`] clone, and — because the outer call did
    /// **not** take the ETS cell (see [`Self::reentrancy_capable`]) — borrows the shared
    /// tables and reaches the outer tables' `ets:*` operations.
    ///
    /// # The process-dictionary seam
    ///
    /// The five process-dictionary BIFs (`erlang:put/2`, `get/1`, `erase/1`,
    /// `get_keys/0`, `get_keys/1`) need the **running process's** [`Dict`](beam_process::Dict)
    /// — a `&mut ProcessTable`, which for an `-eval` context is
    /// `&mut runtime.processes` and therefore cannot co-exist with [`FunBridge`]'s
    /// `&mut Runtime`. [`Self::uses_dict`] gates them: such a call installs a
    /// [`DictBridge`] (resolving the table the same way the mailbox/`self` seam does)
    /// **instead of** the fun-caller bridge. No dict BIF applies a fun argument, so nothing
    /// is lost (see [`DictBridge`]).

    pub(super) fn call_registered(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
        hooks: DispatchHooks,
    ) -> Result<Eterm> {
        // Cheap handles that do NOT borrow the runtime: the runtime stays free for the
        // fun-caller bridge.
        let registry = Rc::clone(&self.runtime.registry);
        let atoms = self.runtime.code_server.atoms_handle();
        let tables_handle = Rc::clone(&self.runtime.tables);
        let ports_handle = Rc::clone(&self.runtime.ports);
        let mut heap = self.runtime.take_bif_heap()?;
        // Whether the pooled heap came to this call **clean**. The pool is shared: a heap is
        // popped here and pushed back by every other caller too (the native floor's
        // `mint_reference`/`reference_term`, trace, the ETS owner sweep), and those put it back
        // **without** resetting it — the heap a reference or a spawned pid was just built on is
        // still holding that live term when this call receives it. A rewind to the base would
        // recycle those words and hand a later caller the *same* address for a different term
        // (the `assert_ne!` in `native::process::tests::alias_…` is exactly that), so the
        // copy-out/rewind below is applied **only** when no other caller owns a word of it.
        // See [`Self::call_registered`]'s copy-out rule.
        let pool_clean = heap.used() == 0;
        // `self_pid` is the pid this context runs as: `erlang:self/0` reports it, and a
        // `receive` in a fun a BIF applies resolves its mailbox against it.
        let self_pid = self.mailbox_pid;
        // `base` is the shared table's length before the call: the index the first new
        // name gets, because nothing else interns while this single-threaded call runs.
        let mut pending = PendingInterner::new(atoms.len());
        // Whether this is a process-dictionary BIF, which needs the running process's
        // `Dict` (a `&mut ProcessTable`) rather than a fun-caller bridge — the two borrows
        // cannot co-exist (see the method docs).
        let uses_dict = hooks.dict;
        // Whether this is one of the eleven process/name/global-state BIFs, which need the
        // process-table/port-registry bridge (a `&mut ProcessTable` borrow, like the dict
        // arm — see [`ProcessBridge`]).
        let uses_process_hook = hooks.process_hook;
        // Whether this is `erlang:load_nif/2`, which needs the NIF-loading bridge (also a
        // `&mut Runtime` borrow, and therefore a third mutually exclusive arm).
        let uses_nif_loader = hooks.nif_loader;
        // Whether this is `records:get_definition/2`, which needs the record-source bridge
        // (a `&CodeServer` borrow, and therefore a fourth mutually exclusive arm).
        let uses_record_source = hooks.record_source;
        // Whether this is `erlang:send/2`/`erlang:'!'/2`, which need the message-delivery
        // bridge (a fifth arm: it borrows the whole `Runtime` like the fun-caller bridge, and
        // the two cannot co-exist — see [`SendBridge`]).
        let uses_send_hook = hooks.send_hook;
        // Whether this is `erlang:get_module_info/1,2`, which needs the module-info bridge
        // (a `&CodeServer` borrow, and therefore its own mutually exclusive arm).
        let uses_module_info = hooks.module_info;
        // Whether this is one of the seven `persistent_term:*` BIFs, which need the
        // node-global store bridge (a cheap `Rc` clone: no runtime borrow at all).
        let uses_persistent_term = hooks.persistent_term;
        // Whether this is one of the runtime-coupled BIF families (the timers, the loaded
        // export check, the link set) that only the running runtime can answer, and which
        // need the timer heap / reference space / process table (`&mut Runtime` plus a
        // `&mut ProcessTable`, like the dict arm — and therefore its own mutually exclusive
        // arm; see [`RuntimeHooksBridge`]).
        let uses_runtime_hooks = hooks.runtime_hooks;
        // The local-node identity: **this runtime's own node** — the configured
        // `node@host` atom when `-name`/`-sname` made it a distributed node, else the fixed
        // `nonode@nohost` ([`Runtime::local_node`] is the single source of truth for both).
        // It is read from the runtime *before* the shared atom table is borrowed by the
        // context, and it is always `Some` for a live runtime: the atom was interned into
        // that very table at construction, so the hook can never answer "unknown" for a node
        // this runtime *is*. Both fields are `Copy`, so this hook needs no runtime borrow at
        // all and every arm below can install it (see [`NodeIdentity`]).
        let mut local_node = NodeIdentity {
            node: Some(self.runtime.local_node()),
        };
        // The caller's module name is **not** cloned here: only the NIF-loading, `send` and
        // fun-caller arms consume it, and a registered BIF call may not pay an owned `String`
        // for them. The borrow below serves the one pre-destructure consumer (the fun-identity
        // snapshot); the destructure then moves the field out as `caller_module` for the arms.
        // The local-fun identity: `term_to_binary/1,2` (and `external_size/1,2`, which sizes
        // the same image) of a compiled module's `make_fun3` closure needs the `NEW_FUN_EXT`
        // fields the term does not carry (module atom, module MD5, `old_index`/`old_uniq`,
        // creator pid), which only the *loaded module* knows. Like `local_node` this hook
        // needs **no** runtime borrow: it shares the code server's **cached** loaded-module
        // snapshot (`CodeServer::fun_module_snapshot`, rebuilt only when the loaded-module
        // set changes) and scans it caller-first, so a call costs one `Rc` clone and one name
        // lookup rather than a walk of the module cache. It is still built **only** for the
        // four BIFs that consult this seam (see [`Self::uses_fun_identity`]); every other
        // registered call pays nothing at all.
        // For every other registered call it stays `None` and no hook is installed —
        // indistinguishable from the hook itself, because no other BIF ever calls
        // `local_fun_identity`. The `-eval` evaluator's own closures match no loaded module's
        // `FunT`, so they keep the codec's typed `unsupported_local_fun` (see
        // [`FunIdentityBridge`]).
        let mut fun_identity = if hooks.fun_identity {
            Some(FunIdentityBridge::new(
                &self.runtime.code_server,
                self.current_module.as_deref(),
                local_node.node,
            ))
        } else {
            None
        };
        // The node's memory accounting behind `erlang:memory/0,1`: an owned snapshot holding
        // the shared atom table plus the figures reachable only *before* the call (the
        // process table, the code server, the ETS registry) — it needs **no** `&mut Runtime`
        // borrow and is built before the ETS `RefCell` is borrowed for the call, which is why
        // its ETS figure is captured (see [`RuntimeMemory`]). It is built **only** for
        // `erlang:memory/0,1` (see [`Self::uses_memory_hook`]): its `code` figure stats every
        // loaded module's `.beam` (one `stat` syscall per module, ~23 after boot), so building
        // it on *every* registered BIF call would add ~10 us to each arithmetic operation. No
        // other BIF consults the [`MemoryHook`] seam.
        let mut memory = if hooks.memory_hook {
            Some(RuntimeMemory::new(
                Rc::clone(&atoms),
                self.mailbox_table
                    .as_deref()
                    .unwrap_or(&self.runtime.processes),
                &self.runtime.code_server,
                &tables_handle,
            ))
        } else {
            None
        };
        // Whether this call can **re-enter** the runtime by applying a fun argument (see
        // [`Self::reentrancy_capable`]). Hoisted out of the dispatch block below because the
        // result-rewind gate needs it too.
        let reentrant = Self::reentrancy_capable(args);
        // Whether the BIF's result may be **moved out of the pooled BIF heap onto the running
        // process's own young heap** and the pooled heap then **rewound** — the tail of this
        // method (see [`Self::move_result_into_process_heap`]).
        //
        // Six of the `DispatchHooks` arms ([`super::dispatch`]) each let a term the BIF built on
        // `ctx.heap()` **escape** into state that outlives the call, which is exactly what a
        // rewind would invalidate:
        //
        // * `dict` — the process dictionary ([`DictBridge`]);
        // * `process_hook` — the process/port registry ([`ProcessBridge`]);
        // * `nif_loader` — a loaded NIF library's private data ([`NifBridge`]);
        // * `send_hook` — a delivered message ([`SendBridge`]);
        // * `persistent_term` — the node-global store ([`PersistentTermBridge`]);
        // * `runtime_hooks` — the timer heap / reference space / link set
        //   ([`RuntimeHooksBridge`]).
        //
        // A **reentrant** call (a fun-applying BIF) is excluded outright: the nested run can push
        // an argument that lives on this very heap into any of those seams. The remaining arms —
        // `record_source`, `module_info` and the `fun_identity`/`memory_hook` build gates — are
        // pure reads of loader/runtime state, and the output sink is pure output, so none of them
        // retains a heap word.
        let safe_to_rewind = !hooks.dict
            && !hooks.process_hook
            && !hooks.nif_loader
            && !hooks.send_hook
            && !hooks.persistent_term
            && !hooks.runtime_hooks
            && !reentrant;
        let mut result = {
            let RuntimeContext {
                runtime,
                output,
                mailbox_pid,
                mailbox_table,
                may_pump_scheduler,
                current_module: caller_module,
                engines,
                gc_armed: _,
                dispatch_memo: _,
            } = self;
            // Re-entry happens only when the BIF *applies a fun argument* (the fun-taking
            // `lists:*`/`maps:*` BIFs): the applied fun can call back into the runtime and
            // reach `ets:*`. Borrowing the shared `TableRegistry` for the whole call would
            // leave its `RefCell` taken, so the nested `ets:*` could not borrow it. When an
            // argument is a fun, therefore, do **not** install the tables — the cell stays
            // free for the nested call. Nothing is lost: a BIF that takes a fun scans it
            // with `lists`/`maps` logic and does not touch ETS itself (no M1 BIF both needs
            // ETS *and* applies a fun argument). `reentrant` was computed above the block (the
            // result-rewind gate needs it there too).
            let mut tables = if reentrant {
                None
            } else {
                // `try_borrow_mut` (never `borrow_mut`): an unexpected outer borrow must
                // degrade to "no ETS", never panic.
                tables_handle.try_borrow_mut().ok()
            };
            // The port provider is a cheap handle shared by both branches: it holds the
            // shared `Rc` port registry and a clone of the shared atom-table handle, so
            // `erlang:open_port/2` really starts an OS process and `erlang:port_info/1,2`
            // answers from the runtime's own port table (see `crate::port`).
            let mut ports = RuntimePorts::new(Rc::clone(&ports_handle), Rc::clone(&atoms), self_pid);
            if uses_dict {
                // The running process's directory: resolve the table the same way the
                // mailbox/`self` seam does — the caller-supplied table (a scheduler pass)
                // when there is one, else the runtime's own process table (a `-eval`
                // context). This borrow of `runtime` is the *only* one here, so it does not
                // clash with anything (the fun-caller bridge is not built).
                let table: &mut ProcessTable = match mailbox_table {
                    Some(ref mut table) => &mut **table,
                    None => &mut runtime.processes,
                };
                let mut dict = DictBridge {
                    table,
                    pid: self_pid,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_dict_hook(&mut dict)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_process_hook {
                // The eleven process/name/global-state BIFs: the bridge borrows the process
                // table (the caller-supplied pass table when there is one, else the
                // runtime's own) *and* the runtime (for the `process_info` arena), exactly
                // like the dict arm, so it is its own mutually exclusive arm. None of them
                // applies a fun argument or touches the dictionary.
                let mut bridge = ProcessBridge::new(
                    runtime,
                    mailbox_table.as_deref_mut(),
                    Rc::clone(&ports_handle),
                    *engines,
                    self_pid,
                );
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_process_hook(&mut bridge)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_nif_loader {
                // `erlang:load_nif/2`: the NIF bridge needs `&mut Runtime` (to store the loaded
                // library in the runtime's NIF registry), so it is the only runtime borrow in
                // this arm — mutually exclusive with the dict/fun-caller arms. Its host shares
                // *this* call's `pending` interner, so a name the C `load` callback interns and
                // a name the BIF interns land in one ordered, exactly-predicted list.
                let mut bridge = NifBridge {
                    runtime,
                    atoms: Rc::clone(&atoms),
                    interner: pending.share(),
                    self_pid: self_pid.to_eterm(),
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_nif_loader(&mut bridge)
                    .with_current_module(caller_module.as_deref().unwrap_or(""))
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_record_source {
                // `records:get_definition/2` answers from the *loaded module's* `Recs` chunk,
                // which lives in the code server's module map — a `&CodeServer` borrow, so it
                // is its own mutually exclusive arm (see [`RecordBridge`]). The BIF applies no
                // fun argument and touches no dictionary, so nothing is lost.
                let mut source = RecordBridge {
                    code_server: &runtime.code_server,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_record_source(&mut source)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_send_hook {
                // `erlang:send/2`/`erlang:'!'/2` (`SendBridge`): the delivery bridge borrows the
                // whole `Runtime` (it delegates to the existing `RuntimeContext::send_message`
                // path, so nothing about delivery is re-implemented), making it mutually
                // exclusive with the fun-caller/dict/NIF/record arms. A `send` applies no fun
                // argument and touches no dictionary, so nothing is lost.
                let mut bridge = SendBridge {
                    runtime,
                    output: output.clone(),
                    mailbox_pid: *mailbox_pid,
                    mailbox_table: mailbox_table.as_deref_mut(),
                    may_pump_scheduler: *may_pump_scheduler,
                    current_module: caller_module.clone(),
                    engines: *engines,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_sender(&mut bridge)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_module_info {
                // `erlang:get_module_info/1,2`: answers from the loaded module's IR — a
                // `&CodeServer` borrow, its own mutually exclusive arm. It applies no fun
                // argument.
                let mut source = ModuleInfoBridge {
                    code_server: &runtime.code_server,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_module_info(&mut source)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_persistent_term {
                // The seven `persistent_term:*` BIFs: a cheap `Rc` clone of the
                // node-global store, plus a clone of the shared atom-table handle so the
                // store can compare keys/values **structurally** (`beam_term::eq`) — two
                // structurally equal boxed terms are distinct heap words. No runtime
                // borrow is held (it can coexist with everything).
                //
                // The store keeps its key and value as **raw `Eterm` words**, so a term a
                // *scheduled* process supplied lives on that process's own young heap — the
                // heap the collector relocates at the engine's heap-admission point
                // ([`crate::context::gc`]). A raw word kept past a collection would dangle,
                // so the two **storing** BIFs (`put/2`, `put_new/2`) copy their key and
                // value onto the pointer-stable arena first: exactly the escape-store
                // treatment every other runtime-level store already takes
                // ([`Runtime::try_stabilize_escaped`]). The five **reading** BIFs are
                // unaffected — every word they can hand back was stabilized when it was
                // stored (a key they receive is only compared, never stored).
                let stored_args = if Self::persistent_term_stores(module, function, arity) {
                    let mut stabilized = Vec::with_capacity(args.len());
                    for arg in args {
                        stabilized.push(runtime.try_stabilize_escaped(*arg)?);
                    }
                    Some(stabilized)
                } else {
                    None
                };
                let args = stored_args.as_deref().unwrap_or(args);
                let mut store = PersistentTermBridge::new(
                    Rc::clone(&runtime.persistent_terms),
                    Rc::clone(&atoms),
                );
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_persistent_term(&mut store)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else if uses_runtime_hooks {
                // The runtime-coupled BIF families (`RuntimeHooksBridge`): the timer heap and
                // reference space (`&mut Runtime`) plus the caller's process table
                // (`&mut ProcessTable`) — the same pair the dict arm resolves, so it is its
                // own mutually exclusive arm. None of these keys applies a fun argument or
                // touches the dictionary, so the fun-caller/dict seams are never needed for
                // one, and every key this bridge does not serve answers the typed capability
                // gap the defaulted `RuntimeHooks` seam reports (workspace `CONTEXT.md` §3).
                let mut bridge = RuntimeHooksBridge {
                    runtime,
                    table: mailbox_table.as_deref_mut(),
                    self_pid,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_runtime_hooks(&mut bridge)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            } else {
                // The bridge gets its own clone of the shared sink, so a `fun` a BIF applies
                // writes to the caller's sink (not stdout) — see `SharedSink`/`FunBridge`. It
                // inherits the caller's module name so a nested call still knows its call site.
                let mut bridge = FunBridge {
                    runtime,
                    output: output.clone(),
                    mailbox_pid: *mailbox_pid,
                    mailbox_table: mailbox_table.as_deref_mut(),
                    may_pump_scheduler: *may_pump_scheduler,
                    current_module: caller_module.clone(),
                    engines: *engines,
                };
                let mut context = BifContext::new(&mut heap, &atoms, self_pid)
                    .with_atom_interner(&mut pending)
                    .with_fun_caller(&mut bridge)
                    .with_port_hook(&mut ports)
                    .with_local_node(&mut local_node);
                if let Some(fun_identity) = fun_identity.as_mut() {
                    context = context.with_local_fun_identity(fun_identity);
                }
                if let Some(memory) = memory.as_mut() {
                    context = context.with_memory_hook(memory);
                }
                if let Some(sink) = output.as_mut() {
                    context = context.with_output(sink);
                }
                if let Some(tables) = tables.as_deref_mut() {
                    context = context.with_tables(tables);
                }
                registry.call(module, function, arity, &mut context, args)
            }
        };
        // The result is still on the pooled BIF heap. Left there it is a container in a region
        // the running process's copying collector does **not** scan, so the unscanned-root valve
        // ([`super::gc::root_is_unscanned`]) declines every collection while it is live in an x
        // register — and a churn loop that keeps such a result alive then grows peak memory with
        // its iteration count. Move it onto the running process's own young heap (a scanned
        // region) and rewind the pool, so the result is collectable and the heap returns to its
        // base.
        //
        // The rewind happens only when the move is both **safe** and **complete**:
        //
        // * `safe_to_rewind` — no seam can have retained a heap word (see its definition above);
        // * an **`Ok`** result — a raise may carry a live reason term
        //   ([`beam_common::Raise::live_reason`], the `{badkey,K}`/`{case_clause,V}` shapes) that
        //   points into this heap, so the error path never rewinds;
        // * a **container** result — an immediate is neither boxed nor a cons and a literal lives
        //   in the frozen literal area, so neither can reach a word on this heap; skipping the
        //   copy keeps the arithmetic/immediate hot path free (dispatch cost is measured —
        //   see `TRACE_AND_PERF.md`);
        // * a move that **succeeded** — no scheduled process, a nursery that cannot be grown, or
        //   a graph the copier refuses all leave the result where it is;
        // * a **clean** pooled heap — `heap` must not have been handed over with a live word below
        //   this call's own base (see `pool_clean` above): rewinding then would recycle a term
        //   another caller is still holding.
        let mut rewind = false;
        if safe_to_rewind && pool_clean {
            if let Ok(term) = result {
                if !Self::result_is_foreign_container(term) {
                    // Nothing reachable lives on this heap, so no byte of it was handed out for
                    // anything that survives the call: rewinding disturbs nothing.
                    rewind = true;
                } else if let Some(copied) = self.move_result_into_process_heap(term) {
                    result = Ok(copied);
                    rewind = true;
                }
            }
        }
        // The BIF (and any NIF a bridge ran) is done and every borrow released, so commit the
        // names interned during the call: each lands at the index the interner predicted,
        // keeping the BIF's/NIF's atom terms (and every printer/comparison that resolves them)
        // consistent.
        //
        // The emptiness guard is load-bearing for performance, not semantics: an empty commit
        // is a no-op, but `atoms_mut()` goes through `Rc::make_mut`, and the port-registry /
        // persistent-term / NIF handles plus this call's own `atoms` binding keep the shared
        // table's strong count at 2 or more -- so touching it would deep-clone the whole
        // `AtomTable` on *every* registered BIF call. Only a call that actually interns a name
        // (`binary_to_term`/`list_to_atom`/`binary_to_atom`/`load_nif`, all rare) pays the
        // commit, and then it still lands the names at their predicted indices.
        if !pending.is_empty() {
            pending.commit(self.runtime.code_server.atoms_mut());
        }
        if rewind {
            // Every word this call handed out of the pooled heap is now either unreachable (the
            // BIF's intermediate terms) or has a scanned copy in the process's young heap, so the
            // bump pointer can safely return to the heap's base. The pooled heap then serves the
            // next call from word 0 instead of marching toward the pool's capacity.
            heap.reset();
        }
        self.runtime.put_bif_heap(heap);
        // A registered BIF that ran its heap dry is the emulator's documented hard limit,
        // not a host-level allocation abort (see `error::enforce_term_heap_limit`).
        result.map_err(error::enforce_term_heap_limit)
    }

    /// Whether `term` is a **container the pooled BIF heap could own** — the mirror of the
    /// container test in [`super::gc::root_is_unscanned`].
    ///
    /// An **immediate** (a small integer, an atom, `NIL`, a local pid/port/reference) is neither
    /// boxed nor a cons, and a **literal** (a frozen code constant) lives in the literal area,
    /// never on the pooled heap — so only a boxed, non-literal term or a cons cell can reach a
    /// word this call handed out of the pool. Everything else may rewind the pool with no copy,
    /// which keeps the arithmetic/immediate hot path free.
    fn result_is_foreign_container(term: Eterm) -> bool {
        (beam_term::prelude::is_boxed(term) || beam_term::prelude::is_list(term))
            && !beam_term::literal::is_literal(term)
    }

    /// Moves a registered BIF's result `term` off the pooled BIF heap onto the running process's
    /// **own young heap**, returning the copy — or `None` when it cannot be re-homed, in which
    /// case the caller leaves both the result and the pool exactly as they were.
    ///
    /// # The destination is the process's SCANNED heap
    ///
    /// The target is [`Process::heap_mut`](beam_process::Process::heap_mut) — the very young heap
    /// the engine's own allocation seams bump ([`super::gc::alloc_words_rooted`]) — and a region
    /// the process's collector **scans**, so the moved result is kept alive by the root set the
    /// engine already publishes (it lands in `x(0)` on return) and is relocated by a later
    /// collection, instead of declining every one through the unscanned-root valve.
    ///
    /// # Growth is the NO-RELOCATION seam
    ///
    /// A full nursery is met by [`Process::grow_nursery`](beam_process::Process::grow_nursery) and
    /// **one** retry — deliberately **not** by
    /// [`ensure_capacity`](beam_process::Process::ensure_capacity), which collects first over
    /// `process.regs.gather_roots()`, the process's *published* root set. Inside a BIF call the
    /// machine's live x registers and Erlang stack are **not** published, so collecting here would
    /// free live terms. `grow_nursery` retires the space it replaces and moves no live term, which
    /// its own docs declare legal and correct for a host holding an incomplete or unpublished root
    /// set — exactly this situation.
    ///
    /// # The EMPTY source table is load-bearing
    ///
    /// The copy's source [`OffHeapTable`](beam_message::OffHeapTable) is empty because the BIF
    /// heap owns no off-heap handle. `beam_message`'s copier resolves a refc binary's payload
    /// through `ctx.src_off_heap().binary_by_data_ptr(val)`; with no owner it falls back to
    /// **flattening** the window into a self-contained on-heap bitstring — byte-faithful, nothing
    /// registered in the destination table, no refcount to lose. An off-heap **MSO** reference
    /// with no resolvable owner is the copier's typed `bad_term` instead, so this function returns
    /// `None` (the caller then does not rewind the pool) — never a dangling payload. The empty
    /// source table can therefore only ever produce a *complete* copy, and the destination
    /// (`Runtime::eval_off_heap`) grows by at most one entry per distinct node an external
    /// pid/port/reference names.
    fn move_result_into_process_heap(&mut self, term: Eterm) -> Option<Eterm> {
        // `ProcessId` is `Copy`, so read it before the destructure (match ergonomics, as the
        // dispatch block above does).
        let pid = self.mailbox_pid;
        let RuntimeContext {
            runtime,
            mailbox_table,
            ..
        } = self;
        // No caller-supplied table is the `-eval`/single-pass shape: leave today's behaviour byte
        // for byte (the result stays on the pooled heap, which is then not rewound).
        let table = mailbox_table.as_deref_mut()?;
        let process = table.get_mut(pid)?;
        let copied = {
            let src = beam_message::OffHeapTable::new();
            let mut ctx = beam_message::CopyContext::new(&src, &mut runtime.eval_off_heap);
            beam_message::copy_struct(term, process.heap_mut(), &mut ctx)
        };
        if let Ok(copied) = copied {
            return Some(copied);
        }
        // The nursery could not serve the copy: GROW (retiring the old space, moving nothing) and
        // retry once. `size_object` is the graph's flat word count and the floor of 256 keeps a
        // small result from landing exactly on the current space's edge.
        let hint = beam_term::size::size_object(term).max(256);
        process.grow_nursery(hint).ok()?;
        // A fresh `CopyContext` over the (now current) grown space, whose bump pointer is at its
        // base. Nothing the first attempt wrote needs unwinding: a non-clean destination takes
        // `copy_struct`'s preflight path, which refuses **before** writing any destination word.
        let src = beam_message::OffHeapTable::new();
        let mut ctx = beam_message::CopyContext::new(&src, &mut runtime.eval_off_heap);
        beam_message::copy_struct(term, process.heap_mut(), &mut ctx).ok()
    }

    /// Dispatches a **NIF** call without touching the runtime's ordinary dispatch.
    ///
    /// A perfect no-op on a miss (`Ok(None)`), so it is safe to try before *every* BIF: only a
    /// `(module, function, arity)` a loaded NIF library actually exports is intercepted, exactly
    /// as the BEAM's own NIF dispatch works (a NIF replaces the Erlang stub of the same name).
    ///
    /// The NIF runs against a **fresh** per-call [`NifHostImpl`] over a pooled BIF heap, with a
    /// fresh [`PendingInterner`] rooted at the shared table's current length. The term it
    /// returns lives on that pooled heap, which is pointer-stable and **not** rewound: a NIF
    /// result is **not** copied out — unlike a registered BIF's (see
    /// [`Self::move_result_into_process_heap`] and the rewind in [`Self::call_registered`]) — so
    /// it stays where it was built and keeps declining a collection through the unscanned-root
    /// valve while it is live. That is a deliberate Milestone-1 residual (see
    /// [`Self::open_gate_for_foreign_result`]).
    ///
    /// # Errors
    ///
    /// A NIF that records a pending exception (`enif_raise_exception`, `enif_make_badarg`)
    /// raises that reason as a live reason term ([`crate::error::nif_raised`]) — never swallowed
    /// and never replaced by a placeholder. A NIF that hands back a **non-term** (the ABI's
    /// `THE_NON_VALUE`, a bare header word) is that broken NIF's diagnostic, never a crash and
    /// never a fabricated value. An unexported `{name, arity}` (impossible here, the lookup
    /// already confirmed it) is the bare [`crate::error::undef`].
    pub(super) fn try_call_nif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Option<Eterm>> {
        if !self.runtime.nifs.borrow().lookup(module, function, arity) {
            return Ok(None);
        }
        let atoms = self.runtime.code_server.atoms_handle();
        let mut interner = PendingInterner::new(atoms.len());
        let heap = self.runtime.take_bif_heap()?;
        let self_pid = self.mailbox_pid.to_eterm();
        let schedulers = crate::info::schedulers_count(self.runtime) as i32;
        // Clone the registry `Rc` so the call itself does not borrow `self.runtime` — the borrow
        // below is spent on `services`.
        let nifs = Rc::clone(&self.runtime.nifs);
        // The runtime services the host calls back into, over the same process table the rest of
        // the context resolves against (the slice's own table when there is one, else the
        // runtime's). These are **disjoint** field borrows of `self.runtime`, which is what lets
        // them co-exist with each other and with `self.mailbox_table`.
        let processes: &ProcessTable = match &self.mailbox_table {
            Some(table) => &**table,
            None => &self.runtime.processes,
        };
        let mut services = RuntimeNifServices {
            refs: &mut self.runtime.refs,
            references: &mut self.runtime.references,
            processes,
            ports: &self.runtime.ports,
        };
        let mut host = NifHostImpl::new(
            heap,
            Rc::clone(&atoms),
            interner.share(),
            self_pid,
            schedulers,
            Some(&mut services),
        );
        // The registry is borrowed immutably only for the duration of the call, and released
        // before the interner is committed (which needs `&mut` on the code server).
        let outcome = {
            let reg = nifs.borrow();
            let mut env = beam_abi::NifEnv::new(&mut host);
            reg.call(module, function, arity, &mut env, args)
        };
        // Consuming the host releases its borrow of `services`, and dropping `services` releases
        // the runtime field borrows before the heap/interner work below reuses `self.runtime`.
        let heap = host.into_heap();
        drop(services);
        self.runtime.put_bif_heap(heap);
        // Same load-bearing emptiness guard as `call_registered`: `atoms_mut()` is
        // `Rc::make_mut`, and the live host/NIF-registry handles keep the shared table's
        // strong count above 1, so an unconditional commit would deep-clone the whole
        // `AtomTable` on every NIF call. Only commit when the NIF actually interned a name.
        if !interner.is_empty() {
            interner.commit(self.runtime.code_server.atoms_mut());
        }
        match outcome {
            // A NIF must hand back a **term** (or record an exception, which `NifLibrary::call`
            // turns into `Raised`). The ABI's `THE_NON_VALUE` (a bare header word) is not one: the
            // boxed-term predicates accept its primary tag, so letting it reach the engine would
            // dereference a null boxed pointer and abort. Report the broken NIF instead — a NIF
            // returning `enif_make_copy`'s "no value" word (or any other non-term) never crashes
            // the runtime and never becomes a fabricated value.
            Ok(term) if crate::nif::is_nif_value(term) => Ok(Some(term)),
            Ok(_) => Err(error::message("NIF returned a non-value term")),
            Err(NifCallError::Raised(reason)) => Err(error::nif_raised(reason)),
            // Defensive: `lookup` above already confirmed the export, so this cannot happen —
            // but a library raced out from under us would be `undef`, never a fabricated value.
            Err(NifCallError::UnknownFunction { .. }) => Err(error::undef()),
        }
    }
}

impl RuntimeContext<'_> {
    /// The **routing** half of [`ExecutionContext::call_bif`]: pick the route that serves
    /// `module:function/arity` and run it.
    ///
    /// Kept apart from the seam itself so the seam can inspect the **result** once, at the one
    /// point every route funnels through (see [`Self::open_gate_for_foreign_result`]).
    pub(super) fn call_bif_routed(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // A loaded NIF library wins over every other route: a NIF replaces the Erlang stub of
        // the same `{module, function, arity}`. On a miss this is a no-op (`Ok(None)`).
        if let Some(term) = self.try_call_nif(module, function, arity, args)? {
            return Ok(term);
        }
        if let Some(result) = self.try_native(module, function, arity, args) {
            // The floor builds its terms on the same pooled BIF heap, so an exhaustion
            // there is the same documented limit (see `error::enforce_term_heap_limit`).
            return result.map_err(error::enforce_term_heap_limit);
        }
        let dispatch = self.dispatch(module, function, arity);
        if dispatch.bif.is_some() {
            // A table-creating `ets:new/2` is observed here so the runtime's owner index can
            // release the table when its creator dies (see `crate::runtime::ets_owner`). Only
            // a `named_table` is reachable through the registry's *public* surface, so only it
            // is indexed: the handle is resolved by the name registration the call newly made
            // (a `before`/`after` `lookup_name` delta distinguishes a real registration from a
            // pre-existing name or an unnamed table that merely carries the same name).
            if module == "ets" && function == "new" && arity == 2 {
                let name = args
                    .first()
                    .copied()
                    .filter(|term| beam_term::is_atom(*term))
                    .map(beam_term::atom_val);
                let before = name.and_then(|atom| self.runtime.tables.borrow().lookup_name(atom));
                let result = self.call_registered(module, function, arity, args, dispatch.hooks);
                if let (Some(atom), Ok(_)) = (name, &result) {
                    let after = self.runtime.tables.borrow().lookup_name(atom);
                    if let Some(handle) = after.filter(|handle| Some(*handle) != before) {
                        // Indexing the table also takes ownership of its creation-time heir's
                        // gift term, which `beam_bifs` installed as a raw reference into the
                        // creating process's heap (see `runtime::ets_owner::own_heir_gift`).
                        self.runtime.adopt_created_table(self.mailbox_pid, handle, atom);
                    }
                }
                return result;
            }
            return self.call_registered(module, function, arity, args, dispatch.hooks);
        }
        if module == "erlang" {
            // The **effective** gap set: the curated list intersected with what the live
            // registry does not own, so a freshly-registered BIF can never be shadowed (see
            // `error::is_known_unimplemented_bif`).
            if error::is_known_unimplemented_bif(&self.runtime.registry, function, arity) {
                return Err(error::known_unimplemented_bif(function, arity));
            }
            // A name the release does not have raises the bare `error:undef` the oracle does
            // (never a fabricated value and never a capability gap — see `error::undef`).
            return Err(error::undef());
        }
        // The code server adds the system library directories (stdlib, kernel) the first
        // time a module has to be resolved, so an external call reaches the real stdlib
        // without an explicit `-pa`. A miss here is not fatal: `CodeServer::load` below
        // reports it, naming every directory searched.
        let _ = self.runtime.find_module_file(module);
        let loaded = self.runtime.code_server.load(module)?;
        self.run_export(&loaded, function, arity, args)
    }

    /// Opens the running process's **descent gate** ([`crate::context::gc_walk`]) when `term` is
    /// a container in a region its collector does not scan.
    ///
    /// A registered BIF, the runtime-native floor and a NIF all build their results on a pooled
    /// heap **outside** the process's own young/mature area, and the engine stores such a result
    /// straight into the process's reachable graph. That is the one way a foreign container
    /// enters the graph other than an engine arena fallback
    /// ([`Runtime::note_gc_fallback`](crate::runtime::gc)), so it is the event that must open the
    /// gate: the valve's always-run per-word phase still classifies the result itself while it
    /// is a root, but the process may nest it inside a container it builds on its *own* scanned
    /// heap — and that nested shape is exactly what the gated descent exists to find.
    ///
    /// This is deliberately **not** a cost on every BIF call: the gate is
    /// [`gc::root_is_unscanned`](crate::context::gc::root_is_unscanned), which rejects every
    /// immediate (small integers, atoms, `NIL`), every literal-area constant and every pointer
    /// into the process's own heap in a few instructions — so the ordinary result costs two
    /// branches and the `O(live objects)` descent stays off the allocator's path. A process with
    /// no table (`-eval`/single-pass) or none the table still holds cannot collect, so it needs
    /// no gate.
    pub(super) fn open_gate_for_foreign_result(&mut self, term: Eterm) {
        let pid = self.mailbox_pid;
        let foreign = match self
            .mailbox_table
            .as_deref()
            .and_then(|table| table.get(pid))
        {
            Some(process) => crate::context::gc::root_is_unscanned(process, term),
            None => false,
        };
        if foreign {
            self.runtime.note_foreign_reachable(pid);
        }
    }
}
