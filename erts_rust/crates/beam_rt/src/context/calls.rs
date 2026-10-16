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
use beam_alloc::Heap;
use beam_bifs::{BifContext, BifFn, TableRegistry};
use beam_common::{BeamError, Result};
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

/// How much free room a pooled BIF heap must still have for a call to use it as its allocation
/// frontier: less than `capacity() / BIF_HEAP_GROWTH_DIVISOR` free words is "nearly exhausted" and
/// [`RuntimeContext::take_bif_frontier`] retires the region instead.
///
/// The threshold is deliberately a *fraction of the capacity* rather than a word count, so the
/// same policy applies to the default region and to every region a growth step produced. It also
/// makes the check a single compare on the hot path — the pool is taken on **every** registered
/// call.
const BIF_HEAP_GROWTH_DIVISOR: usize = 16;

impl<'a> RuntimeContext<'a> {
    /// Takes the pooled BIF heap this call allocates on, **growing the frontier** when the region
    /// it hands back is (nearly) exhausted.
    ///
    /// # Why the pool needs a growth path
    ///
    /// A registered BIF's container result is moved onto the running process's own young heap and
    /// the pooled region is then rewound to this call's `mark` — but only when that rewind is both
    /// **safe** and **complete**, and the copy-out requires a scheduled process
    /// ([`Self::move_result_into_process_heap`] answers `None` without a `mailbox_table`). On the
    /// **table-less** route — the `-eval` evaluator and every single-pass nested run — there is no
    /// process heap to move a container onto, so nothing rewinds the pool: every container a BIF
    /// builds stays on the same [`beam_alloc::Heap`] for the run's whole life, the bump pointer
    /// marches to that region's fixed capacity, and the next allocation there is
    /// [`AllocKind::Heap`](beam_common::AllocKind) — which
    /// [`error::enforce_term_heap_limit`] renders as `error:system_limit`. A long in-process
    /// workload (the installed compiler is the load-bearing one) therefore used to stop for a
    /// reason unrelated to its live set.
    ///
    /// # Retirement, never relocation
    ///
    /// Growth is by **region replacement**, exactly as the collector's own growth is: the
    /// exhausted region goes back to the pool ([`Runtime::put_bif_heap`]) and a fresh, larger one
    /// becomes this call's frontier. A [`beam_alloc::Heap`] never moves its terms and is never
    /// freed here, so every word a caller still owns stays valid and at its own address — the
    /// retired region leaves only the *allocation* rotation. Nothing is retried either: the growth
    /// happens **before** the call's own work, so the BIF runs exactly once and no side effect can
    /// be repeated.
    ///
    /// # The cap
    ///
    /// The new frontier is the old capacity doubled ([`usize::saturating_mul`]), never below
    /// [`Runtime::bif_heap_words`], and capped by a configured `+hmax`
    /// ([`crate::config::RuntimeConfig::heap_options`]'s
    /// [`policy`](crate::config::HeapOptions::policy) `max_heap_words`; `0` is "no cap"). When the
    /// cap leaves no room (`computed <= capacity()`) the heap is returned **as it is**, so an
    /// explicit `+hmax` still yields today's `error:system_limit` — a configured limit is never
    /// widened.
    ///
    /// A **nested** call may be handed a region an outer call retired; the same check applies
    /// there and is safe (the region is never rewound below the words another caller owns: the
    /// callers' own `mark = used()` rule takes care of that).
    ///
    /// # Errors
    ///
    /// Propagates the fresh region's allocation failure — raised **before** the BIF has run, so no
    /// side effect is repeated and no result is fabricated.
    fn take_bif_frontier(&mut self) -> Result<Heap> {
        let heap = self.runtime.take_bif_heap()?;
        if heap.available() >= heap.capacity() / BIF_HEAP_GROWTH_DIVISOR {
            return Ok(heap);
        }
        let doubled = heap
            .capacity()
            .saturating_mul(2)
            .max(self.runtime.bif_heap_words);
        let max = self.runtime.config().heap_options().policy().max_heap_words;
        let grown = if max == 0 { doubled } else { doubled.min(max) };
        if grown <= heap.capacity() {
            // A configured `+hmax` forbids the growth: keep today's region and today's
            // `system_limit` for a call that really overflows it.
            return Ok(heap);
        }
        // Retire the exhausted region — never freed, never moved, so the words another caller owns
        // in it stay valid — and continue the run in a fresh, larger frontier.
        self.runtime.put_bif_heap(heap);
        Heap::new(grown)
    }

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
    /// the caller's sink through a [`SharedSink`] clone. A nested call made by a BIF that
    /// installed **no** tables itself (`lists:map/2`) borrows the free ETS cell and reaches
    /// the shared `ets:*` operations. A nested call made by one that **did** hold the cell
    /// (`ets:foldl/3`/`ets:foldr/3`) receives the outer call's registry **lent** through the
    /// fun-caller bridge ([`RuntimeContext::lent_tables`]) and installs it, so an `ets:*`
    /// inside the fold's own fun resolves exactly as at the top level.
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

    /// # The implementation is already resolved
    ///
    /// `bif` is the [`BifFn`] the per-context dispatch memo resolved
    /// ([`Self::dispatch`] → `BifRegistry::lookup`), so this method invokes it **directly**
    /// through [`invoke_registered`] instead of re-entering `BifRegistry::call` — which would
    /// re-hash the two names on every call, the dominant cost of the prologue. The registry
    /// lookup stays the **single** source of truth for "is this key registered": it ran once to
    /// produce `bif`, and [`Self::call_bif_routed`] only reaches this method when it answered
    /// `Some`.

    pub(super) fn call_registered(
        &mut self,
        bif: BifFn,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
        hooks: DispatchHooks,
    ) -> Result<Eterm> {
        // Cheap handles that do NOT borrow the runtime: the runtime stays free for the
        // fun-caller bridge.
        let atoms = self.runtime.code_server.atoms_handle();
        let tables_handle = Rc::clone(&self.runtime.tables);
        let ports_handle = Rc::clone(&self.runtime.ports);
        // The allocation frontier for this call, grown when the pooled region is nearly exhausted
        // (see [`Self::take_bif_frontier`]). A freshly grown region starts empty, so `mark` read
        // *after* the helper is this call's true start offset either way.
        let mut heap = self.take_bif_frontier()?;
        // The offset at which **this** call starts on the pooled heap — the boundary between the
        // words another caller owns and the words this call hands out. The pool is shared: a heap
        // is popped here and pushed back by every other caller too (the native floor's
        // `mint_reference`/`reference_term`, trace, the ETS owner sweep), and those put it back
        // **without** resetting it — the heap a reference or a spawned pid was just built on is
        // still holding that live term when this call receives it. A rewind to the heap's base
        // would recycle those words and hand a later caller the *same* address for a different
        // term (the `assert_ne!` in `native::process::tests::alias_…` is exactly that), so the
        // tail rewinds to `mark` — [`Heap::reset_to`](beam_alloc::Heap::reset_to) — which frees
        // exactly `[mark..used)` (this call's own words) and keeps every word below it that
        // another owner still holds. `mark == 0` (a heap taken clean) makes that the plain
        // `reset()` it used to be. See [`Self::call_registered`]'s copy-out rule.
        let mark = heap.used();
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
        // creator pid), which only the *loaded module* knows — and `erlang:fun_info/1,2` needs
        // the very same derivation for a fun's `name`. Like `local_node` this hook
        // needs **no** runtime borrow: it shares the code server's **cached** loaded-module
        // snapshot (`CodeServer::fun_module_snapshot`, rebuilt only when the loaded-module
        // set changes) and scans it caller-first, so a call costs one `Rc` clone and one name
        // lookup rather than a walk of the module cache. It is still built **only** for the
        // BIFs that consult this seam (see [`Self::uses_fun_identity`]); every other
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
        // Whether this call must **hold the shared `TableRegistry` cell** for its whole
        // duration. This is deliberately a *separate* predicate from `reentrant` (which keeps
        // its own meaning for the `safe_to_rewind` gate below): a reentrant call no longer
        // implies "leave the cell free".
        //
        // A reentrant call (a fun-applying BIF such as `lists:map/2`) normally does **not** take
        // the cell, so the fun it applies can re-enter the runtime and reach `ets:*` (see the
        // comment at the borrow site). But a call that *itself* reads the `TableRegistry` seam
        // cannot run without it: with no cell installed the BIF's `ctx.tables()` is `None` and it
        // raises the typed `unsupported_bif`. The two `ets` fun-folds (`ets:foldl/3`,
        // `ets:foldr/3`) are exactly that case — they take the table out of the registry, collect
        // its objects, restore it, and only *then* apply the fun — so they must hold the cell
        // despite being reentrant. [`Self::tables_capable`] names the seam's consumers (from
        // `beam_bifs`) rather than blanket-enabling every reentrant call, which would break
        // `lists:map(fun(X) -> ets:lookup(T, X) end, L)` by blocking that nested call.
        let install_tables = !reentrant || Self::tables_capable(module, function, arity);
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
                lent_tables,
            } = self;
            // The ETS registry this call installs. Two sources, in precedence order:
            //
            // * a **lent** registry (`lent_tables`) — the one live `&mut TableRegistry` an outer
            //   `BifContext::call_fun` handed the fun-caller bridge for the duration of a nested
            //   call (`ets:foldl/3`/`ets:foldr/3`). The outer fold holds the shared `RefCell`
            //   for its whole duration, so the cell **must not be consulted at all** on this
            //   path; the lent `&mut` is exclusive by construction, so installing it cannot
            //   alias. It is installed for **every** nested call, not only a reentrant one, so it
            //   propagates through an intermediate that installs no cell of its own
            //   (`ets:foldl` fun → `lists:map` fun → `ets:lookup`).
            // * otherwise the shared cell, held exactly when this call needs it
            //   (`install_tables`, computed above): a plain call, or a reentrant call that itself
            //   reads the `TableRegistry` seam — the two `ets` fun-folds. A reentrant call that
            //   does **not** read the seam leaves the cell free, so the fun it applies can
            //   re-enter the runtime and reach `ets:*` itself (that is why
            //   `lists:map(fun(X) -> ets:lookup(T, X) end, L)` keeps working: the outer
            //   `lists:map/2` installs nothing).
            //
            // `try_borrow_mut` (never `borrow_mut`): an unexpected outer borrow must degrade to
            // "no ETS", never panic.
            let mut held_tables = if lent_tables.is_none() && install_tables {
                tables_handle.try_borrow_mut().ok()
            } else {
                None
            };
            // `held_tables` must outlive `tables` (it owns the cell borrow the latter reborrows).
            let mut tables: Option<&mut TableRegistry> = match lent_tables.as_deref_mut() {
                Some(lent) => Some(lent),
                None => held_tables.as_deref_mut(),
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
                invoke_registered(bif, arity, &mut context, args)
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
                invoke_registered(bif, arity, &mut context, args)
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
                invoke_registered(bif, arity, &mut context, args)
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
                invoke_registered(bif, arity, &mut context, args)
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
                invoke_registered(bif, arity, &mut context, args)
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
                invoke_registered(bif, arity, &mut context, args)
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
                invoke_registered(bif, arity, &mut context, args)
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
                invoke_registered(bif, arity, &mut context, args)
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
                invoke_registered(bif, arity, &mut context, args)
            }
        };
        // The result is still on the pooled BIF heap. Left there it is a container in a region
        // the running process's copying collector does **not** scan, so the unscanned-root valve
        // ([`super::gc::root_is_unscanned`]) declines every collection while it is live in an x
        // register — and a churn loop that keeps such a result alive then grows peak memory with
        // its iteration count. Move it onto the running process's own young heap (a scanned
        // region) and rewind the pool **to this call's `mark`**, so the result is collectable and
        // the words this call handed out are given back.
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
        //   a graph the copier refuses all leave the result where it is.
        //
        // A **partially used** pool is no obstacle: `mark` was taken before the call, so
        // `reset_to(mark)` frees exactly this call's words `[mark..used)` and leaves the ones a
        // previous caller still owns below `mark` untouched (see `mark` above). For a heap taken
        // clean (`mark == 0`) that is byte for byte the `reset()` it replaces.
        let mut rewind = false;
        if safe_to_rewind {
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
        // Two handles are dropped first, and they are exactly why the commit is cheap: the
        // table is shared through an `Rc`, so its strong count decides whether a write appends
        // in place or deep-clones it. This call's own `atoms` binding and the `RuntimeMemory`
        // snapshot built for `erlang:memory/0,1` (which stores `Rc::clone(&atoms)`) are two
        // such owners -- the BIF context, the port/persistent-term/NIF bridges built inside the
        // block above have already been dropped with it. Neither handle is read after the call
        // (the result is a term, and the rewind above touches only the heap), so dropping them
        // hands the commit the append-only fast path: `CodeServer::intern_pending` takes
        // `Rc::get_mut` and appends, instead of `Rc::make_mut` cloning the whole `AtomTable`
        // (thousands of atoms) on every interning call.
        //
        // The emptiness guard stays load-bearing for the same reason it always was: a call that
        // interned nothing must not borrow the table at all, so `binary_to_term`/`list_to_atom`
        // -- or, far more often, every arithmetic/immediate BIF -- keeps its no-copy fast path.
        drop(atoms);
        drop(memory);
        if !pending.is_empty() {
            self.runtime.code_server.intern_pending(&pending);
        }
        // Rewind to this call's mark and give the heap back to the pool. Every word this call
        // handed out of the pooled heap is now either unreachable (the BIF's intermediate terms)
        // or has a scanned copy in the process's young heap, so the bump pointer can safely
        // return to `mark` — the words below it belong to another caller and stay owned. The
        // pooled heap then serves the next call from `mark` instead of marching toward the pool's
        // capacity.
        //
        // `reset_to` is fallible, but the error arm is unreachable: `mark` was read from **this**
        // heap at the top of this call, the heap is a local held for the call's whole body, and
        // its offset only ever grows in between (`used` advances on every allocation and nothing
        // the call can reach truncates a heap — `Heap` exposes no rewind to the BIF/NIF/native
        // seams, and a nested call takes a *different* heap from the pool), so `mark <=
        // heap.used()` always holds. It is handled anyway, typed and with no `unwrap`: a refusal
        // leaves the heap untouched (its own contract), so the honest answer is to surface it —
        // that is a `BeamError::BadArg` out of the same `Result` this call already returns, never
        // a recycled live word and never a silently mis-sized pool.
        let rewound = if rewind { heap.reset_to(mark) } else { Ok(()) };
        self.runtime.put_bif_heap(heap);
        rewound?;
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
    /// # A FRESH, EMPTY destination off-heap table per attempt
    ///
    /// The copy's destination off-heap table is a **local** [`OffHeapTable`](beam_message::OffHeapTable)
    /// built fresh for each attempt — never [`Runtime::eval_off_heap`], the runtime-lifetime store
    /// that also receives entries from transport copies ([`Runtime::stabilize_term_with`]) and
    /// [`Runtime::adopt_escaped_binaries`]. `beam_message`'s copier takes its single-traversal fast
    /// path (`try_copy_fast`) only when the destination table is *fully empty*
    /// (`copy.rs`'s `is_fast_path_eligible`), so pointing it at the shared runtime store would force
    /// **every** copy-out onto the two-traversal `preflight_words` route (and, for a graph too big
    /// for the nursery, an extra `bounded_words` + acyclicity-`HashSet` sizing pass) from the moment
    /// any unrelated runtime event registered one entry there — a **permanent** poisoning. The
    /// retry gets its **own** fresh table too: a partial attempt may have left registrations in the
    /// first, and it must not re-poison the retry.
    ///
    /// On a **successful** copy the local table's off-heap references are **adopted** into
    /// [`Runtime::eval_off_heap`] — the runtime-lifetime owner the copied term resolves them
    /// through — and only then is the copy returned; a **refusal** adopts nothing (the local table
    /// is dropped, its `Drop` releasing the node references the abandoned attempt registered). The
    /// copied term resolves a refc binary by payload **data pointer**
    /// ([`binary_by_data_ptr`](beam_message::OffHeapTable::binary_by_data_ptr)) and a node by
    /// **identity**, never by destination slot index, so the adoption is transparent: each drained
    /// node entry is pushed once per counted reference, so the store's `push_node` dedup restores
    /// exactly the count the local table held.
    ///
    /// # The EMPTY source table is load-bearing (and is what makes the adoption sound)
    ///
    /// The copy's source [`OffHeapTable`](beam_message::OffHeapTable) is empty because the BIF
    /// heap owns no off-heap handle. `beam_message`'s copier resolves a refc binary's payload
    /// through `ctx.src_off_heap().binary_by_data_ptr(val)`; with no owner it falls back to
    /// **flattening** the window into a self-contained on-heap bitstring — byte-faithful, nothing
    /// registered in the destination table, no refcount to lose. An off-heap **MSO** reference
    /// with no resolvable owner is the copier's typed `bad_term` instead, so this function returns
    /// `None` (the caller then does not rewind the pool) — never a dangling payload. The empty
    /// source table can therefore only ever produce a *complete* copy, and it is also *why the
    /// adoption is sound*: no off-heap **MSO container** can be copied (its source slot never
    /// resolves — `clone_slot` is `None` ⇒ `bad_term`), so the local table's MSO list is provably
    /// empty on every success and `drain_handles` can never re-slot a container whose on-heap
    /// `val` word would then name the wrong slot.
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
        // One fresh destination table per attempt (see the docs above): a refused attempt's
        // partial state is never rolled into the retry, so both attempts hand the copier an
        // empty destination and take the single-traversal fast path.
        let needed = match copy_result_fresh(process.heap_mut(), &mut runtime.eval_off_heap, term) {
            Ok(copied) => return Some(copied),
            Err(error) if !beam_message::is_message_heap_exhaustion(&error) => {
                // A refusal that is **not** the destination running out of room — above all the
                // copier's malformed-graph `badarg` — is permanent: growing the destination
                // cannot fix it, so a retry would re-walk the whole graph, grow a whole space for
                // nothing and fail again with the very same reason. Leave both the result and the
                // pool exactly as they were.
                return None;
            }
            // The destination could not hold the graph. The copier's **own** preflight has
            // already counted the flat words it wanted and reports that count back in the
            // exhaustion error (`AllocFailure::requested`, the `beam_message::error` contract's
            // `words * size_of::<Eterm>()`), so the growth hint costs no second traversal of the
            // graph: `beam_term::size_object(term)` — the obvious alternative — walks it all over
            // again *and* builds a `HashSet` plus a `Vec` on the way.
            Err(error) => copy_shortfall_words(&error),
        };
        // GROW: the NO-RELOCATION seam ([`Process::grow_nursery`]) retires the space it replaces
        // and moves no live term, so the copy can simply be re-attempted from the fresh space's
        // base — `needed` is the copier's exact flat word count, so the retry fits. The floor of
        // 256 keeps a small result from landing exactly on the new space's edge.
        #[cfg(test)]
        notes::note_grow();
        process.grow_nursery(needed.max(256)).ok()?;
        copy_result_fresh(process.heap_mut(), &mut runtime.eval_off_heap, term).ok()
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
        {
            // The registry is normally **empty** — only `load_nif/2` ever fills it — so
            // `is_empty()` (a length compare) short-circuits *before* the `HashMap` SipHash of
            // the module name that `lookup` pays on this fast path.
            let nifs = self.runtime.nifs.borrow();
            if nifs.is_empty() || !nifs.lookup(module, function, arity) {
                return Ok(None);
            }
        }
        let atoms = self.runtime.code_server.atoms_handle();
        let mut interner = PendingInterner::new(atoms.len());
        // A NIF result is never copied off the pool (see the method docs), so this route grows the
        // frontier but never rewinds it: [`Self::take_bif_frontier`] is what keeps a long table-less
        // run from marching one region to its capacity.
        let heap = self.take_bif_frontier()?;
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
        // The host also owned the call's *second* table handle (`Rc::clone(&atoms)`), so
        // consuming it leaves this call's own `atoms` binding as the only transient owner --
        // dropped explicitly below, before the commit.
        let heap = host.into_heap();
        drop(services);
        self.runtime.put_bif_heap(heap);
        // Same load-bearing emptiness guard as `call_registered`, and the same reason to drop the
        // transient handle *before* committing: the table is an `Rc`, so a write from a shared
        // table deep-clones it. `atoms` is not read after this point (the outcome below is a term
        // built on the heap), so dropping it lets `CodeServer::intern_pending` take its
        // `Rc::get_mut` append-only fast path; a genuinely outstanding handle (a loaded NIF
        // library's stored host -- a *different* host from this per-call one -- or an open driver
        // port) still falls back to the correct copy-on-write, merely unoptimized. Only a NIF that
        // actually interned a name reaches any of this.
        drop(atoms);
        if !interner.is_empty() {
            self.runtime.code_server.intern_pending(&interner);
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
        // `Some` is exactly "the registry has this key" — the memo resolved it through
        // `BifRegistry::lookup`, which stays the single source of truth — and the resolved
        // `BifFn` is what `call_registered` invokes (no second registry lookup).
        if let Some(bif) = dispatch.bif {
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
                let result =
                    self.call_registered(bif, module, function, arity, args, dispatch.hooks);
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
            return self.call_registered(bif, module, function, arity, args, dispatch.hooks);
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
            crate::debug_trace::trace(format_args!("UNDEF erlang:{function}/{arity}"));
            return Err(error::undef());
        }
        // The code server adds the system library directories (stdlib, kernel) the first
        // time a module has to be resolved, so an external call reaches the real stdlib
        // without an explicit `-pa`. A miss here is not fatal: `CodeServer::load` below
        // reports it, naming every directory searched.
        let _ = self.runtime.find_module_file(module);
        let loaded = match self.runtime.code_server.load(module) {
            Ok(loaded) => loaded,
            Err(err) => {
                crate::debug_trace::trace(format_args!(
                    "UNDEF load {module}:{function}/{arity}: {err}"
                ));
                return Err(err);
            }
        };
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

/// Runs **one** copy-out attempt of `term` onto `heap` through a **fresh, empty** local destination
/// off-heap table, adopting that table's off-heap references into `store` on success.
///
/// The fresh table is the whole point: `beam_message`'s copier takes its single-traversal fast path
/// (`copy.rs`'s `try_copy_fast`) only for a destination whose three off-heap lists are empty, and
/// `store` ([`Runtime::eval_off_heap`]) is routinely non-empty because transport copies
/// ([`Runtime::stabilize_term_with`]) and nested runs
/// ([`Runtime::adopt_escaped_binaries`]) register entries there. Handing the copier a local table
/// keeps the copy-out on that fast path no matter what the shared store holds; the adoption below
/// then hands the references back to the runtime before the (now dead) local table drops them.
///
/// On **success** the local table's MSO handles, refc binaries and per-reference node entries move
/// into `store`; on **failure** nothing is adopted — the local table is dropped, its `Drop`
/// releasing the node references the abandoned attempt took. Because every attempt calls this with
/// its own new table, no partial state can be carried into a retry.
///
/// The SOURCE table is empty and stays empty: the pooled BIF heap owns no off-heap handle, so a
/// refc binary whose payload no source handle owns is **flattened** into an on-heap bitstring (the
/// copier's fallback) and an off-heap MSO container has no resolvable source slot at all
/// (`clone_slot` is `None` ⇒ the copier's `bad_term`). That is why the adopted MSO list is
/// provably empty on success — `push` can never re-slot a container whose on-heap `val` word would
/// then name the wrong slot. See [`RuntimeContext::move_result_into_process_heap`].
fn copy_result_fresh(
    heap: &mut Heap,
    store: &mut beam_message::OffHeapTable,
    term: Eterm,
) -> Result<Eterm> {
    let src = beam_message::OffHeapTable::new();
    let mut local = beam_message::OffHeapTable::new();
    #[cfg(test)]
    notes::note_attempt(&local);
    let outcome = {
        let mut ctx = beam_message::CopyContext::new(&src, &mut local);
        beam_message::copy_struct(term, heap, &mut ctx)
    };
    if outcome.is_ok() {
        // Adopt the local table's off-heap references into the runtime-lifetime store. The copied
        // term resolves them by payload **data pointer** (refc binaries) and by node **identity**,
        // never by destination slot index, so re-registering each drained entry there is
        // transparent. (`push` is a no-op on the empty MSO list success guarantees; the guard on
        // the outcome is what makes the node adoption release-correct on the refusal path.)
        for handle in local.drain_handles() {
            store.push(handle);
        }
        for binary in local.drain_binaries() {
            store.push_binary(binary);
        }
        for entry in local.drain_nodes() {
            // One reference per count, faithfully: the local table dedups by identity and counts
            // references, so replaying `push_node` once per counted reference restores exactly that
            // count in the store (which dedups the same way).
            for _ in 0..entry.refcount() {
                store.push_node(beam_message::NodeEntry::new(entry.node(), entry.creation()));
            }
        }
    }
    outcome
}

/// The flat word count a refused copy reported it could not fit.
///
/// [`beam_message`]'s **message-heap exhaustion** ([`beam_message::is_message_heap_exhaustion`])
/// carries the copier's own preflight shortfall: `AllocFailure::requested` is the flat term-word
/// count the copy wanted, in bytes (the `beam_message::error` module's documented
/// `words * size_of::<Eterm>()`), so recovering it here is free.
///
/// [`beam_term::size_object`], the obvious alternative, is a **second full traversal** of a graph
/// the copier has just walked — and one that additionally builds a `HashSet` and a `Vec` to defend
/// against a forged cycle. Paying for it on every refused copy is what this helper removes.
///
/// A refusal of any other class reports no room request at all; the returned `0` is the
/// conservative answer, because the caller's floor raises it to a legal growth hint.
fn copy_shortfall_words(error: &BeamError) -> usize {
    match error {
        BeamError::Alloc(failure) => failure.requested / core::mem::size_of::<Eterm>(),
        // Unreachable from [`RuntimeContext::move_result_into_process_heap`], which only reaches
        // this for `is_message_heap_exhaustion` (which implies the `Alloc` variant).
        _ => 0,
    }
}

/// Invokes a `beam_bifs` implementation the dispatch memo already **resolved**.
///
/// This is [`BifRegistry::call`](beam_bifs::BifRegistry::call) minus the registry lookup. The
/// memo holds the [`BifFn`] `BifRegistry::lookup` returned — the registry's own answer to "is this
/// key registered", so it can never become a second, drifting registry — and re-running that
/// lookup here would only re-hash the two names, which is the dominant cost of the dispatch
/// prologue ([`super::dispatch`]) and exactly what memoizing the implementation is for.
///
/// What is invoked and what it returns are **unchanged**: the arity check `BifRegistry::call`
/// performs is reproduced byte for byte (`args.len() != arity` is the same `error:badarity`
/// [`BeamError::BadArity`] the registry raises), and the implementation is called with the same
/// context and the same argument slice. Only the lookup in front of it is gone.
fn invoke_registered(
    bif: BifFn,
    arity: u32,
    context: &mut BifContext<'_>,
    args: &[Eterm],
) -> Result<Eterm> {
    if args.len() != arity as usize {
        return Err(BeamError::BadArity);
    }
    bif(context, args)
}
/// Test-only structural counters for the copy-out (`RuntimeContext::move_result_into_process_heap`).
///
/// **Thread-local**, so a test that drives the copy-out reads only the counts *it* produced and the
/// crate's parallel unit tests cannot perturb them (each `cargo test` case runs on its own thread).
///
/// * `copy_attempts` — one per [`beam_message::copy_struct`] call (a copy-out that exhausts the
///   nursery and retries charges two).
/// * `grow_retries` — one per `Process::grow_nursery` call.
/// * `empty_destination_attempts` — one per attempt whose destination table was **fully empty** as
///   handed to the copier, i.e. the copier's single-traversal fast path was eligible. Computed from
///   the table's **public** API (the same three-list test `is_fast_path_eligible` makes).
#[cfg(test)]
mod notes {
    use core::cell::Cell;

    thread_local! {
        static COPY_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
        static GROW_RETRIES: Cell<usize> = const { Cell::new(0) };
        static EMPTY_DESTINATION_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
    }

    /// Records one copy attempt against the destination table `destination`, classifying it as an
    /// "empty destination" attempt when all three of that table's lists are empty.
    pub(super) fn note_attempt(destination: &beam_message::OffHeapTable) {
        COPY_ATTEMPTS.with(|c| c.set(c.get() + 1));
        if destination.is_empty()
            && destination.binaries().is_empty()
            && destination.nodes().is_empty()
        {
            EMPTY_DESTINATION_ATTEMPTS.with(|c| c.set(c.get() + 1));
        }
    }

    /// Records one `grow_nursery` call.
    pub(super) fn note_grow() {
        GROW_RETRIES.with(|c| c.set(c.get() + 1));
    }

    /// Zeroes this thread's counters, so a test measures only its own work.
    pub(super) fn reset() {
        COPY_ATTEMPTS.with(|c| c.set(0));
        GROW_RETRIES.with(|c| c.set(0));
        EMPTY_DESTINATION_ATTEMPTS.with(|c| c.set(0));
    }

    /// This thread's `(copy_attempts, grow_retries, empty_destination_attempts)`.
    pub(super) fn snapshot() -> (usize, usize, usize) {
        (
            COPY_ATTEMPTS.with(Cell::get),
            GROW_RETRIES.with(Cell::get),
            EMPTY_DESTINATION_ATTEMPTS.with(Cell::get),
        )
    }
}

#[cfg(test)]
mod tests {
    //! Units for the copy-out's **retry policy** (see
    //! [`RuntimeContext::move_result_into_process_heap`]): the growth hint recovered from the
    //! copier's own refusal, and the gate that keeps a *permanent* refusal from growing the
    //! destination at all. The copy itself is exercised end to end by `tests/perf_call.rs`
    //! (path B copies a container result on every iteration) and the `src/context/tests.rs`
    //! registered-BIF suites.

    use beam_common::{AllocFailure, AllocKind};
    use beam_process::SignalAtoms;
    use beam_term::make_small;
    use beam_term::prelude::make_header;
    use super::*;

    /// The number of `Eterm`s a byte figure converts back to.
    const WORD: usize = core::mem::size_of::<Eterm>();

    /// A `runtime` + a caller-owned `ProcessTable` holding one default process, plus the pid —
    /// the `for_process` shape, i.e. the one where the copy-out (and its retry) is ON.
    fn process_table(rt: &mut crate::runtime::Runtime) -> (ProcessTable, beam_process::ProcessId) {
        let signals = SignalAtoms::from_table(rt.code_server_atoms_mut());
        let mut table = ProcessTable::new(signals);
        let pid = table.spawn_default().expect("a default process");
        (table, pid)
    }

    #[test]
    fn the_growth_hint_is_the_copiers_own_shortfall_in_words() {
        // `beam_message`'s exhaustion error carries `words * size_of::<Eterm>()` in
        // `AllocFailure::requested` (its documented contract), so dividing recovers exactly the
        // flat word count the refused copy wanted — with no second traversal of the graph.
        let exhaustion = BeamError::Alloc(AllocFailure::new(AllocKind::Message, 37 * WORD));
        assert_eq!(
            copy_shortfall_words(&exhaustion),
            37,
            "the copier's own word count is recovered verbatim"
        );
        // A refusal of any other class reports no room request: the conservative `0` (the
        // caller's `max(256)` floor turns it into a legal growth hint).
        assert_eq!(copy_shortfall_words(&BeamError::BadArg), 0);
        assert_eq!(copy_shortfall_words(&BeamError::BadArity), 0);
        assert_eq!(
            copy_shortfall_words(&BeamError::Alloc(AllocFailure::new(AllocKind::Heap, 9 * WORD))),
            9,
            "any `Alloc` refusal carries its own requested byte count"
        );
    }

    #[test]
    fn a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing() {
        let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
            .expect("the default runtime");
        let (mut table, pid) = process_table(&mut rt);
        let spaces_before = table
            .get(pid)
            .expect("the spawned process")
            .collector()
            .young_space_count();
        let used_before = table.get(pid).expect("the spawned process").heap().used();
        let copy = {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            // A bare header word is not a standing term at all (`make_header(0, 0)` is the C
            // ABI's `THE_NON_VALUE`): `copy_struct` answers the copier's typed malformed-graph
            // `badarg` — `BeamError::BadArg`, **not** a message-heap exhaustion. Growing the
            // destination cannot make it succeed, so the retry must not run: no space is
            // retired, no word is written, and the caller gets `None` (so `call_registered`
            // leaves both the result and the pooled BIF heap exactly as they were).
            ctx.move_result_into_process_heap(make_header(0, 0))
        };
        assert!(
            copy.is_none(),
            "a malformed graph cannot be re-homed; the caller must leave the result untouched"
        );
        let process = table.get(pid).expect("the spawned process");
        assert_eq!(
            process.collector().young_space_count(),
            spaces_before,
            "a permanent refusal must not retire a nursery space (the item-4 growth is per-call)"
        );
        assert_eq!(
            process.heap().used(),
            used_before,
            "a refused copy writes no destination word"
        );
    }

    #[test]
    fn an_exhaustion_refusal_grows_the_nursery_and_the_retry_copies() {
        // The positive control for the gate above: a **valid** container the nursery cannot hold
        // still grows the destination by the copier's own word count and is copied on the retry.
        // `lists:seq/2`'s 4000-odd-word result does not fit the default 233-word nursery.
        use beam_engine::ExecutionContext;

        let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
            .expect("the default runtime");
        let (mut table, pid) = process_table(&mut rt);
        let spaces_before = table
            .get(pid)
            .expect("the spawned process")
            .collector()
            .young_space_count();
        let copied = {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            notes::reset();
            ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(2000)])
                .expect("lists:seq(1, 2000)")
        };
        assert!(
            beam_term::prelude::is_list(copied) && !beam_term::literal::is_literal(copied),
            "the result is a real list built on the process's own heap"
        );
        assert!(
            table
                .get(pid)
                .expect("the spawned process")
                .collector()
                .young_space_count()
                > spaces_before,
            "an exhaustion refusal grows the destination (the retry then fits)"
        );
        // The successful retry's destination was **empty** as handed to the copier (fast path
        // eligible) — `lists:seq/2` carries no off-heap reference, and the retry builds its own
        // fresh table, so nothing from the refused attempt can have poisoned it.
        let (attempts, grows, empties) = notes::snapshot();
        assert_eq!(
            (attempts, grows),
            (2, 1),
            "one refused attempt, one growth, one retry"
        );
        assert_eq!(
            empties, attempts,
            "every attempt — including the retry — was offered a fully empty destination"
        );
    }

    /// The copy-out's destination off-heap table is **fresh and empty per attempt**, so
    /// `beam_message`'s copier takes its single-traversal fast path even when the runtime's
    /// escaped-payload store ([`crate::runtime::Runtime::eval_off_heap`]) is already populated by an
    /// unrelated nested run or transport copy.
    ///
    /// **This test fails on the pre-fix code**: with the shared `eval_off_heap` as the copier's
    /// destination, the node entry pushed below makes the table non-empty for *both* attempts, so
    /// `empty_destination_attempts` reads `0` instead of the `2` asserted here — exactly the
    /// permanent poisoning the fresh local table removes.
    #[test]
    fn the_copy_out_always_hands_the_copier_an_empty_destination_table() {
        let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
            .expect("the default runtime");
        let (mut table, pid) = process_table(&mut rt);
        // Model a runtime whose store a prior nested run / transport copy has already populated:
        // one node entry, exactly what a copy that passed an external pid would have registered.
        let node = {
            let atoms = rt.code_server_atoms_mut();
            beam_term::prelude::make_atom(atoms.intern("nonode@nohost"))
        };
        rt.eval_off_heap
            .push_node(beam_message::NodeEntry::new(node, 0));
        // A **foreign** container — a term built on a heap the process's collector does not scan —
        // that fits the default nursery by a wide margin (a 2-tuple is 3 words), held in a local so
        // it outlives both copies.
        let mut foreign = beam_alloc::Heap::new(16).expect("a local heap");
        let term = foreign
            .tuple(&[make_small(1), make_small(2)])
            .expect("the tuple fits");
        let used_before = table.get(pid).expect("the spawned process").heap().used();
        notes::reset();
        let (first, second) = {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            (
                ctx.move_result_into_process_heap(term),
                ctx.move_result_into_process_heap(term),
            )
        };
        assert!(
            first.is_some() && second.is_some(),
            "both copies fit the nursery"
        );
        let (attempts, grows, empties) = notes::snapshot();
        assert_eq!(
            empties, 2,
            "each copy was handed a fully empty destination table (the fast path was eligible)"
        );
        assert_eq!(attempts, 2, "one `copy_struct` call per copy-out");
        assert_eq!(grows, 0, "a 3-word tuple never has to grow the nursery");
        let copied_words = table.get(pid).expect("the spawned process").heap().used() - used_before;
        assert_eq!(
            copied_words, 6,
            "two 3-word tuples land on the process's own heap"
        );
    }

    /// A **refused** copy adopts nothing into the runtime-lifetime store: a malformed-graph refusal
    /// leaves [`crate::runtime::Runtime::eval_off_heap`] exactly as it was — its pre-existing entry
    /// intact, no entry added — because the destination table is local and is merely dropped.
    #[test]
    fn a_refused_copy_adopts_nothing_into_the_runtime_store() {
        let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
            .expect("the default runtime");
        let (mut table, pid) = process_table(&mut rt);
        // A pre-existing entry models a store a prior nested run populated: the refusal must not
        // disturb it, and must add nothing of its own.
        let node = {
            let atoms = rt.code_server_atoms_mut();
            beam_term::prelude::make_atom(atoms.intern("nonode@nohost"))
        };
        rt.eval_off_heap
            .push_node(beam_message::NodeEntry::new(node, 0));
        let nodes_before = rt.eval_off_heap.nodes().len();
        let binaries_before = rt.eval_off_heap.binaries().len();
        notes::reset();
        let copy = {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
            // The `make_header(0, 0)` malformed graph is refused **permanently** (see
            // `a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing`).
            ctx.move_result_into_process_heap(make_header(0, 0))
        };
        assert!(copy.is_none(), "the malformed graph is not re-homed");
        assert_eq!(
            rt.eval_off_heap.nodes().len(),
            nodes_before,
            "a refused copy adopts nothing (it also drops its own node references)"
        );
        assert_eq!(
            rt.eval_off_heap.binaries().len(),
            binaries_before,
            "a refused copy adopts no binary"
        );
        let (attempts, grows, _) = notes::snapshot();
        assert_eq!(attempts, 1, "the malformed graph is refused on the first attempt");
        assert_eq!(grows, 0, "a permanent refusal never grows the nursery");
    }
}
